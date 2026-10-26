use std::collections::BTreeMap;
use std::io::Read;
use std::path::Path;
use std::process::Command;

use anyhow::{anyhow, Context, Result};
use sha2::{Digest, Sha256};

use crate::error::TurnError;
use crate::types::{EnforcePolicy, PermissionBreach};

use super::patterns::{changed_paths, normalize_path, permitted, HEAD_KEY};
use super::EnforceOutcome;

fn invalid(msg: impl Into<String>) -> anyhow::Error {
    TurnError::InvalidSpec(msg.into()).into()
}

fn git(args: &[&str], cwd: &Path) -> Result<String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .map_err(|e| invalid(format!("git {}: {e}", args.join(" "))))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(invalid(format!(
            "git {} failed: {}",
            args.join(" "),
            stderr.trim()
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

fn git_head(cwd: &Path) -> Result<String> {
    let output = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(cwd)
        .output()
        .map_err(|e| invalid(format!("git rev-parse HEAD: {e}")))?;
    if output.status.success() {
        return Ok(String::from_utf8_lossy(&output.stdout).trim().to_string());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    let lower = stderr.to_ascii_lowercase();
    if lower.contains("unknown revision")
        || lower.contains("needed a single revision")
        || lower.contains("ambiguous argument")
        || lower.contains("bad revision")
    {
        return Ok(String::new());
    }
    Err(invalid(format!("git rev-parse HEAD: {}", stderr.trim())))
}

fn git_z_paths(args: &[&str], cwd: &Path) -> Result<Vec<String>> {
    let raw = git(args, cwd)?;
    Ok(raw
        .split('\0')
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .collect())
}

fn content_fingerprint(cwd: &Path, rel: &str, kind: &str) -> Result<String> {
    let path = cwd.join(rel);
    let meta = match fs_symlink_metadata(&path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            // `git diff --name-only` lists working-tree deletions. Untracked/ignored
            // names git just printed must still be on disk.
            if kind == "h" {
                return Ok("deleted".into());
            }
            return Err(invalid(format!("stat {rel}: {e}")));
        }
        Err(e) => return Err(invalid(format!("stat {rel}: {e}"))),
    };
    if meta.file_type().is_symlink() {
        return Ok("symlink".into());
    }
    if meta.is_dir() {
        return Ok("dir".into());
    }
    let mut file = std::fs::File::open(&path).map_err(|e| invalid(format!("read {rel}: {e}")))?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 65536];
    loop {
        let n = file
            .read(&mut buf)
            .map_err(|e| invalid(format!("read {rel}: {e}")))?;
        if n == 0 {
            break;
        }
        let Some(chunk) = buf.get(..n) else {
            break;
        };
        hasher.update(chunk);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn fs_symlink_metadata(path: &Path) -> std::io::Result<std::fs::Metadata> {
    std::fs::symlink_metadata(path)
}

fn insert_fingerprints(
    fingerprints: &mut BTreeMap<String, String>,
    cwd: &Path,
    rel: &str,
    kind: &str,
) -> Result<()> {
    let rel = normalize_path(rel);
    if rel.is_empty() {
        return Ok(());
    }
    let hash = content_fingerprint(cwd, &rel, kind)?;
    fingerprints.insert(rel, format!("{kind}:{hash}"));
    Ok(())
}

pub fn git_snapshot(cwd: &Path) -> Result<BTreeMap<String, String>> {
    let mut fingerprints = BTreeMap::new();
    let head = git_head(cwd)?;
    fingerprints.insert(HEAD_KEY.to_string(), head.clone());

    if !head.is_empty() {
        for path in git_z_paths(&["diff", "-z", "--name-only", "HEAD"], cwd)? {
            insert_fingerprints(&mut fingerprints, cwd, &path, "h")?;
        }
    }

    for path in git_z_paths(&["ls-files", "-z", "--others", "--exclude-standard"], cwd)? {
        insert_fingerprints(&mut fingerprints, cwd, &path, "u")?;
    }

    for path in git_z_paths(
        &[
            "ls-files",
            "-z",
            "--others",
            "--ignored",
            "--exclude-standard",
            "--",
            "specs",
            "requests",
            "app_docs",
        ],
        cwd,
    )? {
        insert_fingerprints(&mut fingerprints, cwd, &path, "i")?;
    }

    Ok(fingerprints)
}

fn head(cwd: &Path) -> String {
    Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(cwd)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default()
}

fn uncommit(cwd: &Path, head_before: &str) -> Result<Option<String>, PermissionBreach> {
    let head_now = head(cwd);
    if head_before.is_empty() || head_now.is_empty() || head_before == head_now {
        return Ok(None);
    }

    let ancestor = Command::new("git")
        .args(["merge-base", "--is-ancestor", head_before, "HEAD"])
        .current_dir(cwd)
        .status();
    if ancestor.map(|s| !s.success()).unwrap_or(true) {
        return Err(PermissionBreach {
            path: HEAD_KEY.to_string(),
            message: format!(
                "HEAD moved from {} to {} during the turn and the old HEAD is not an ancestor of the new one — the agent rewrote or switched history",
                head_before.chars().take(7).collect::<String>(),
                head_now.chars().take(7).collect::<String>()
            ),
        });
    }

    let count = Command::new("git")
        .args(["rev-list", "--count", &format!("{head_before}..HEAD")])
        .current_dir(cwd)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|| "?".into());
    let reset = Command::new("git")
        .args(["reset", "-q", "--mixed", head_before])
        .current_dir(cwd)
        .output();
    if reset.as_ref().map(|o| !o.status.success()).unwrap_or(true) {
        let stderr = reset
            .as_ref()
            .map(|o| String::from_utf8_lossy(&o.stderr).to_string())
            .unwrap_or_default();
        return Err(PermissionBreach {
            path: HEAD_KEY.to_string(),
            message: format!(
                "the agent made {} commit(s) during the turn and they could not be undone: {}",
                count,
                stderr
                    .chars()
                    .rev()
                    .take(300)
                    .collect::<String>()
                    .chars()
                    .rev()
                    .collect::<String>()
            ),
        });
    }
    Ok(Some(format!(
        "undid {} commit(s) the agent made; the changes stay in the working tree",
        count
    )))
}

fn is_untracked_fingerprint(value: &str) -> bool {
    value.starts_with("u:") || value.starts_with("i:")
}

fn roll_back(
    cwd: &Path,
    path: &str,
    before: &BTreeMap<String, String>,
    after: &BTreeMap<String, String>,
) -> String {
    if before.contains_key(path) {
        if !after.contains_key(path) {
            return "REVERTED-BY-AGENT (uncommitted work lost, cannot restore)".to_string();
        }
        return "left as-is (was already modified)".to_string();
    }
    if after.get(path).is_some_and(|v| is_untracked_fingerprint(v)) {
        let file = cwd.join(path);
        if std::fs::remove_file(&file).is_ok() {
            return "deleted".to_string();
        }
        return "could not delete".to_string();
    }
    let result = Command::new("git")
        .args(["checkout", "--", path])
        .current_dir(cwd)
        .output();
    if result.map(|o| o.status.success()).unwrap_or(false) {
        "rolled back".to_string()
    } else {
        "could not roll back".to_string()
    }
}

pub fn git_enforce(
    cwd: &Path,
    policy: &EnforcePolicy,
    data_dir: &Path,
    before: &BTreeMap<String, String>,
) -> Result<EnforceOutcome> {
    let head_before = before.get(HEAD_KEY).cloned().unwrap_or_default();
    let mut notes = Vec::new();
    match uncommit(cwd, &head_before) {
        Ok(Some(msg)) => notes.push(msg),
        Ok(None) => {}
        Err(breach) => {
            return Ok(EnforceOutcome {
                touched_paths: vec![],
                breaches: vec![breach],
                rolled_back: vec![],
            });
        }
    }

    let after = git_snapshot(cwd)?;
    let touched = changed_paths(before, &after);
    let breaches: Vec<PermissionBreach> = touched
        .iter()
        .filter(|p| !permitted(p, policy, data_dir))
        .map(|p| PermissionBreach {
            path: p.clone(),
            message: "modified outside permitted scope".to_string(),
        })
        .collect();

    if breaches.is_empty() {
        return Ok(EnforceOutcome {
            touched_paths: touched,
            breaches: vec![],
            rolled_back: notes,
        });
    }

    let mut rolled_back = notes;
    for breach in &breaches {
        let outcome = roll_back(cwd, &breach.path, before, &after);
        rolled_back.push(format!("{} — {}", breach.path, outcome));
    }

    Ok(EnforceOutcome {
        touched_paths: touched,
        breaches,
        rolled_back,
    })
}

pub fn is_git_repo(cwd: &Path) -> bool {
    Command::new("git")
        .args(["rev-parse", "--git-dir"])
        .current_dir(cwd)
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

pub fn init_test_repo(cwd: &Path) -> Result<()> {
    Command::new("git")
        .args(["init"])
        .current_dir(cwd)
        .status()
        .context("git init")?;
    Command::new("git")
        .args(["config", "user.email", "test@thickapi.local"])
        .current_dir(cwd)
        .status()?;
    Command::new("git")
        .args(["config", "user.name", "thickapi test"])
        .current_dir(cwd)
        .status()?;
    Ok(())
}

pub fn git_add_commit(cwd: &Path, message: &str) -> Result<()> {
    Command::new("git")
        .args(["add", "-A"])
        .current_dir(cwd)
        .status()?;
    let status = Command::new("git")
        .args(["commit", "-m", message])
        .current_dir(cwd)
        .status()?;
    if !status.success() {
        return Err(anyhow!("git commit failed"));
    }
    Ok(())
}
