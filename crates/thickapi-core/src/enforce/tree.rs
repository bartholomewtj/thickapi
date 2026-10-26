use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};

use crate::error::TurnError;
use crate::types::{EnforcePolicy, PermissionBreach};

use super::patterns::permitted;
use super::EnforceOutcome;

pub const TREE_SNAPSHOT_CAP: usize = 2 * 1024 * 1024;
pub const TREE_WALK_MAX_FILES: usize = 10_000;
pub const TREE_WALK_MAX_DEPTH: usize = 64;

#[derive(Debug, Clone)]
pub struct TreeSnapshot {
    pub root: PathBuf,
    pub files: BTreeMap<String, FileEntry>,
    pub cap_exceeded: bool,
}

#[derive(Debug, Clone)]
pub struct FileEntry {
    pub hash: String,
    pub backup: Option<PathBuf>,
}

fn invalid(msg: impl Into<String>) -> anyhow::Error {
    TurnError::InvalidSpec(msg.into()).into()
}

pub fn tree_snapshot(cwd: &Path, backup_dir: &Path) -> Result<TreeSnapshot> {
    fs::create_dir_all(backup_dir)
        .with_context(|| format!("create tree-snapshot backup dir {}", backup_dir.display()))?;
    let mut files = BTreeMap::new();
    let mut total_bytes = 0usize;

    for entry in walk_files(
        cwd,
        TREE_WALK_MAX_FILES,
        TREE_WALK_MAX_DEPTH,
        /*refuse_on_cap*/ true,
    )? {
        let rel = entry
            .strip_prefix(cwd)
            .context("strip prefix")?
            .to_string_lossy()
            .replace('\\', "/");
        if rel.is_empty() {
            continue;
        }
        let meta = fs::symlink_metadata(&entry).map_err(|e| invalid(format!("stat {rel}: {e}")))?;
        if !meta.is_file() {
            continue;
        }
        let size = meta.len() as usize;
        if total_bytes.saturating_add(size) > TREE_SNAPSHOT_CAP {
            return Err(invalid(format!(
                "tree snapshot exceeds {TREE_SNAPSHOT_CAP} byte cap; turn refused"
            )));
        }
        let content = fs::read(&entry).map_err(|e| invalid(format!("read {rel}: {e}")))?;
        let hash = format!("{:x}", Sha256::digest(&content));
        let dest = backup_dir.join(&rel);
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("create backup parent {}", parent.display()))?;
        }
        fs::copy(&entry, &dest).with_context(|| format!("backup {rel} to {}", dest.display()))?;
        total_bytes += size;
        files.insert(
            rel,
            FileEntry {
                hash,
                backup: Some(dest),
            },
        );
    }

    Ok(TreeSnapshot {
        root: cwd.to_path_buf(),
        files,
        cap_exceeded: false,
    })
}

fn skip_walk_dir(name: Option<&OsStr>) -> bool {
    matches!(name.and_then(|n| n.to_str()), Some(".git") | Some("target"))
}

/// Walk `root` without following symlinks or junctions. `.git` and `target` are skipped.
/// Snapshot walks refuse past the file/depth caps; the after-walk passes `usize::MAX`
/// for both so new files past those limits can still be rolled back.
fn walk_files(
    root: &Path,
    max_files: usize,
    max_depth: usize,
    refuse_on_cap: bool,
) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    if !root.exists() {
        return Ok(out);
    }
    let root_meta =
        fs::symlink_metadata(root).map_err(|e| invalid(format!("stat {}: {e}", root.display())))?;
    if root_meta.file_type().is_symlink() {
        return Err(invalid(format!(
            "cwd is a symlink or junction: {}",
            root.display()
        )));
    }
    if !root_meta.is_dir() {
        return Err(invalid(format!(
            "cwd is not a directory: {}",
            root.display()
        )));
    }

    let mut stack = vec![(root.to_path_buf(), 0usize)];
    while let Some((dir, depth)) = stack.pop() {
        if depth > max_depth {
            if refuse_on_cap {
                return Err(invalid(format!(
                    "tree walk exceeds depth cap of {max_depth}"
                )));
            }
            continue;
        }
        let entries = fs::read_dir(&dir)
            .map_err(|e| invalid(format!("cannot walk {}: {e}", dir.display())))?;
        for entry in entries {
            let entry =
                entry.map_err(|e| invalid(format!("cannot walk {}: {e}", dir.display())))?;
            let path = entry.path();
            let meta = fs::symlink_metadata(&path)
                .map_err(|e| invalid(format!("stat {}: {e}", path.display())))?;
            if meta.file_type().is_symlink() {
                continue;
            }
            if meta.is_dir() {
                if skip_walk_dir(path.file_name()) {
                    continue;
                }
                let next = depth + 1;
                if next > max_depth {
                    if refuse_on_cap {
                        return Err(invalid(format!(
                            "tree walk exceeds depth cap of {max_depth}"
                        )));
                    }
                    continue;
                }
                stack.push((path, next));
            } else if meta.is_file() {
                if out.len() >= max_files {
                    if refuse_on_cap {
                        return Err(invalid(format!(
                            "tree walk exceeds file cap of {max_files}"
                        )));
                    }
                    continue;
                }
                out.push(path);
            }
        }
    }
    out.sort();
    Ok(out)
}

fn current_hashes(cwd: &Path) -> Result<BTreeMap<String, String>> {
    let mut map = BTreeMap::new();
    // No file/depth cap: new files the agent created past the snapshot limits must
    // still be visible so they can be rolled back.
    for entry in walk_files(cwd, usize::MAX, usize::MAX, false)? {
        let rel = entry
            .strip_prefix(cwd)?
            .to_string_lossy()
            .replace('\\', "/");
        let content = fs::read(&entry).map_err(|e| invalid(format!("read {rel}: {e}")))?;
        map.insert(rel, format!("{:x}", Sha256::digest(&content)));
    }
    Ok(map)
}

pub fn tree_enforce(
    cwd: &Path,
    policy: &EnforcePolicy,
    data_dir: &Path,
    before: &TreeSnapshot,
) -> Result<EnforceOutcome> {
    if before.cap_exceeded {
        return Err(invalid(
            "tree snapshot exceeded cap; turn should not have started",
        ));
    }

    let after = current_hashes(cwd)?;
    let mut touched = Vec::new();
    for (path, entry) in &before.files {
        let after_hash = after.get(path);
        if after_hash != Some(&entry.hash) {
            touched.push(path.clone());
        }
    }
    for path in after.keys() {
        if !before.files.contains_key(path) {
            touched.push(path.clone());
        }
    }
    touched.sort();
    touched.dedup();

    let breaches: Vec<PermissionBreach> = touched
        .iter()
        .filter(|p| !permitted(p, policy, data_dir))
        .map(|p| PermissionBreach {
            path: p.clone(),
            message: "modified outside permitted scope".to_string(),
        })
        .collect();

    let mut rolled_back = Vec::new();
    if !breaches.is_empty() {
        for breach in &breaches {
            let path = cwd.join(&breach.path);
            if let Some(entry) = before.files.get(&breach.path) {
                let Some(backup) = &entry.backup else {
                    // A path present in the snapshot must never be deleted. A missing backup
                    // is a snapshot bug (cap used to skip copies); refuse rather than degrade.
                    return Err(invalid(format!(
                        "{} has no snapshot backup; turn should not have started",
                        breach.path
                    )));
                };
                if let Some(parent) = path.parent() {
                    fs::create_dir_all(parent)?;
                }
                fs::copy(backup, &path)?;
                rolled_back.push(format!("{} — rolled back", breach.path));
            } else if path.exists() {
                fs::remove_file(&path)?;
                rolled_back.push(format!("{} — deleted", breach.path));
            }
        }
    }

    Ok(EnforceOutcome {
        touched_paths: touched,
        breaches,
        rolled_back,
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    #[test]
    fn walk_refuses_past_the_file_cap() {
        let temp = tempfile::tempdir().expect("tempdir");
        let cwd = temp.path();
        fs::write(cwd.join("a.txt"), "a").expect("a");
        fs::write(cwd.join("b.txt"), "b").expect("b");
        fs::write(cwd.join("c.txt"), "c").expect("c");
        let err = walk_files(cwd, 2, 64, true).expect_err("cap");
        let msg = err.to_string();
        assert!(
            msg.contains("file cap") || msg.contains("invalid_spec"),
            "{msg}"
        );
    }
}
