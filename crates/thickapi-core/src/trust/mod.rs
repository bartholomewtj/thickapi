use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;

static TRUST_MUTEX: Mutex<()> = Mutex::new(());

fn lock_trust() -> std::sync::MutexGuard<'static, ()> {
    TRUST_MUTEX.lock().unwrap_or_else(|e| e.into_inner())
}

/// Replace `path` with `bytes` via a sibling temp file and a rename, so a crash or a failed
/// write never leaves the user's config truncated, and a reader sees the old file or the new
/// one, never half of either (R13). The temp is removed on failure.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "file".into());
    let tmp = path.with_file_name(format!(".{name}.{}.thickapi.tmp", std::process::id()));
    let result = (|| {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&tmp, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

/// Keys of the Claude `projects` entries thickAPI created (it never owns an entry that was
/// there before it seeded). Only these are pruned, and only once their cwd is gone (R13).
fn claude_seeded_ledger(home: &Path) -> PathBuf {
    home.join(".thickapi").join("claude-trust-seeded.json")
}

fn read_ledger(path: &Path) -> Vec<String> {
    fs::read_to_string(path)
        .ok()
        .and_then(|t| serde_json::from_str::<Vec<String>>(&t).ok())
        .unwrap_or_default()
}

fn write_ledger(path: &Path, keys: &[String]) {
    if let Ok(text) = serde_json::to_string_pretty(keys) {
        if let Err(err) = write_atomic(path, format!("{text}\n").as_bytes()) {
            tracing::warn!(path = %path.display(), error = %err, "trust ledger not written");
        }
    }
}

pub fn claude_trust_key(cwd: &str) -> String {
    cwd.replace('\\', "/")
}

pub fn claude_config_json_paths(home: &Path, claude_config_dir: Option<&str>) -> Vec<PathBuf> {
    let config_dir = claude_config_dir
        .filter(|s| !s.trim().is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".claude"));
    let nested = config_dir.join(".claude.json");
    let legacy = home.join(".claude.json");
    if nested == legacy {
        vec![nested]
    } else {
        vec![nested, legacy]
    }
}

#[derive(Debug, Clone)]
pub struct SeedResult {
    pub ok: bool,
    pub updated: Vec<PathBuf>,
    pub key: String,
    pub message: Option<String>,
}

fn read_optional(path: &Path) -> Result<Option<String>, String> {
    match fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.to_string()),
    }
}

/// Merge the trust flag for `key` into `text` and drop the `prune` keys. `Ok(None)` when
/// nothing changes.
fn merge_claude_projects(
    text: Option<&str>,
    key: &str,
    prune: &[String],
) -> Result<Option<(String, bool)>, String> {
    let raw: serde_json::Value = match text {
        Some(text) => serde_json::from_str(text).map_err(|_| "skip".to_string())?,
        None => serde_json::json!({}),
    };
    let mut root = match raw {
        serde_json::Value::Object(map) => map,
        _ => return Err("skip".to_string()),
    };
    let projects = root
        .entry("projects".to_string())
        .or_insert_with(|| serde_json::json!({}));
    let projects_obj = projects.as_object_mut().ok_or_else(|| "skip".to_string())?;
    let mut changed = false;
    for gone in prune {
        if gone != key && projects_obj.remove(gone).is_some() {
            changed = true;
        }
    }
    let created = !projects_obj.contains_key(key);
    let existing = projects_obj
        .entry(key.to_string())
        .or_insert_with(|| serde_json::json!({}));
    if existing
        .get("hasTrustDialogAccepted")
        .and_then(|v| v.as_bool())
        != Some(true)
    {
        if let Some(obj) = existing.as_object_mut() {
            obj.insert(
                "hasTrustDialogAccepted".to_string(),
                serde_json::Value::Bool(true),
            );
            changed = true;
        }
    }
    if !changed {
        return Ok(None);
    }
    let text = serde_json::to_string_pretty(&serde_json::Value::Object(root))
        .map_err(|e| e.to_string())?;
    Ok(Some((format!("{}\n", text), created)))
}

/// Attempts at a read-merge-write before giving up on a file another process keeps changing.
const MERGE_ATTEMPTS: usize = 3;

fn seed_one_claude_file(
    path: &Path,
    key: &str,
    prune: &[String],
) -> Result<SeedOneOutcome, String> {
    seed_one_claude_file_with(path, key, prune, &mut || {})
}

/// Read, merge, then re-read just before the write: when another Claude Code session wrote
/// the file in between, merge again on top of its version instead of overwriting it (R13).
/// `before_commit` runs between the merge and the re-read (tests use it to race a writer).
fn seed_one_claude_file_with(
    path: &Path,
    key: &str,
    prune: &[String],
    before_commit: &mut dyn FnMut(),
) -> Result<SeedOneOutcome, String> {
    let mut current = read_optional(path)?;
    for _ in 0..MERGE_ATTEMPTS {
        let Some((next, created)) = merge_claude_projects(current.as_deref(), key, prune)? else {
            return Ok(SeedOneOutcome::Unchanged);
        };
        before_commit();
        let latest = read_optional(path)?;
        if latest != current {
            current = latest;
            continue;
        }
        write_atomic(path, next.as_bytes()).map_err(|e| e.to_string())?;
        return Ok(SeedOneOutcome::Updated { created });
    }
    Err(format!("{} kept changing while seeding", path.display()))
}

enum SeedOneOutcome {
    Updated { created: bool },
    Unchanged,
}

pub fn seed_claude_trust(cwd: &Path, home: &Path, claude_config_dir: Option<&str>) -> SeedResult {
    let key = claude_trust_key(&cwd.to_string_lossy());
    if key.is_empty() {
        return SeedResult {
            ok: false,
            updated: vec![],
            key,
            message: Some("empty cwd".into()),
        };
    }
    let paths = claude_config_json_paths(home, claude_config_dir);
    let ledger_path = claude_seeded_ledger(home);
    let mut ledger = read_ledger(&ledger_path);
    // Our own entries whose cwd is gone (a removed draftr worktree): prune them.
    let prune: Vec<String> = ledger
        .iter()
        .filter(|k| k.as_str() != key && !Path::new(k.as_str()).exists())
        .cloned()
        .collect();
    let mut updated = Vec::new();
    let mut created_any = false;
    let mut any_file = false;
    let mut failures = Vec::new();
    for path in &paths {
        if !path.exists() {
            continue;
        }
        any_file = true;
        match seed_one_claude_file(path, &key, &prune) {
            Ok(SeedOneOutcome::Updated { created }) => {
                created_any |= created;
                updated.push(path.clone());
            }
            Ok(SeedOneOutcome::Unchanged) => {}
            // A file that is not a JSON object is not ours to rewrite.
            Err(msg) if msg == "skip" => {}
            Err(msg) => failures.push(msg),
        }
    }
    let ledger_before = ledger.len();
    ledger.retain(|k| !prune.contains(k));
    if created_any && !ledger.contains(&key) {
        ledger.push(key.clone());
    }
    if ledger.len() != ledger_before || created_any {
        write_ledger(&ledger_path, &ledger);
    }
    if !any_file {
        let Some(first) = paths.first() else {
            return SeedResult {
                ok: false,
                updated: vec![],
                key,
                message: Some("empty cwd".into()),
            };
        };
        match seed_one_claude_file(first, &key, &[]) {
            Ok(SeedOneOutcome::Updated { .. }) => {
                return SeedResult {
                    ok: true,
                    updated: vec![first.clone()],
                    key,
                    message: None,
                };
            }
            _ => {
                return SeedResult {
                    ok: false,
                    updated: vec![],
                    key,
                    message: Some(format!("could not write {}", first.display())),
                };
            }
        }
    }
    if !failures.is_empty() && updated.is_empty() {
        return SeedResult {
            ok: false,
            updated,
            key,
            message: Some(failures.join("; ")),
        };
    }
    SeedResult {
        ok: true,
        updated,
        key,
        message: (!failures.is_empty()).then(|| failures.join("; ")),
    }
}

pub fn seed_claude_trust_locked(cwd: &Path) -> SeedResult {
    let _guard = lock_trust();
    let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
    let config_dir = std::env::var("CLAUDE_CONFIG_DIR").ok();
    seed_claude_trust(cwd, &home, config_dir.as_deref())
}

pub fn grok_trust_key(cwd: &str, existing_keys: &[String], platform: &str) -> String {
    let back = existing_keys.iter().any(|k| k.contains('\\'));
    let fwd = existing_keys.iter().any(|k| k.contains('/'));
    let sep = if back && !fwd {
        '\\'
    } else if fwd && !back {
        '/'
    } else if platform == "win32" {
        '\\'
    } else {
        '/'
    };
    if sep == '\\' {
        cwd.replace('/', "\\")
    } else {
        cwd.replace('\\', "/")
    }
}

fn grok_path_equal(a: &str, b: &str) -> bool {
    a.replace('\\', "/") == b.replace('\\', "/")
}

pub fn git_checkout_root(cwd: &Path) -> Option<PathBuf> {
    let output = Command::new("git")
        .args([
            "-C",
            &cwd.to_string_lossy(),
            "rev-parse",
            "--git-common-dir",
        ])
        .output();
    match output {
        Ok(o) if o.status.success() => {
            let raw = String::from_utf8_lossy(&o.stdout).trim().to_string();
            if raw.is_empty() {
                return None;
            }
            let common_dir = if Path::new(&raw).is_absolute() {
                PathBuf::from(raw)
            } else {
                cwd.join(raw)
            };
            if common_dir.file_name().and_then(|n| n.to_str()) != Some(".git") {
                return None;
            }
            common_dir.parent().map(PathBuf::from)
        }
        _ => None,
    }
}

pub fn grok_trust_paths(cwd: &Path) -> Vec<PathBuf> {
    if cwd.as_os_str().is_empty() {
        return vec![];
    }
    let mut paths = vec![cwd.to_path_buf()];
    if let Some(root) = git_checkout_root(cwd) {
        if !grok_path_equal(&root.to_string_lossy(), &cwd.to_string_lossy()) {
            paths.push(root);
        }
    }
    paths
}

pub fn seed_grok_trust(cwd: &Path, home: &Path) -> SeedResult {
    let platform = if cfg!(windows) { "win32" } else { "linux" };
    let paths = grok_trust_paths(cwd);
    if paths.is_empty() {
        return SeedResult {
            ok: false,
            updated: vec![],
            key: String::new(),
            message: Some("empty cwd".into()),
        };
    }
    let mut updated = Vec::new();
    let mut key = cwd.to_string_lossy().to_string();
    for path in paths {
        let result = seed_grok_trust_path(&path, home, platform);
        if !result.ok {
            return result;
        }
        if path == cwd {
            key = result.key.clone();
        }
        for f in result.updated {
            if !updated.contains(&f) {
                updated.push(f);
            }
        }
    }
    SeedResult {
        ok: true,
        updated,
        key,
        message: None,
    }
}

pub fn seed_grok_trust_locked(cwd: &Path) -> SeedResult {
    let _guard = lock_trust();
    let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
    seed_grok_trust(cwd, &home)
}

fn seed_grok_trust_path(cwd: &Path, home: &Path, platform: &str) -> SeedResult {
    let cwd_str = cwd.to_string_lossy().to_string();
    if cwd_str.is_empty() {
        return SeedResult {
            ok: false,
            updated: vec![],
            key: cwd_str,
            message: Some("empty cwd".into()),
        };
    }
    let path = home.join(".grok").join("trusted_folders.toml");
    let decided_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    let existing_text = if path.exists() {
        match fs::read_to_string(&path) {
            Ok(t) => t,
            Err(_) => {
                return SeedResult {
                    ok: false,
                    updated: vec![],
                    key: cwd_str,
                    message: Some(format!("could not read {}", path.display())),
                };
            }
        }
    } else {
        String::new()
    };

    if !existing_text.trim().is_empty() && existing_text.trim().starts_with('{') {
        return SeedResult {
            ok: false,
            updated: vec![],
            key: cwd_str,
            message: Some(format!("not grok trusted_folders.toml: {}", path.display())),
        };
    }

    let lines: Vec<String> = if existing_text.is_empty() {
        vec![]
    } else {
        existing_text.lines().map(String::from).collect()
    };

    let mut keys = Vec::new();
    let mut i = 0usize;
    while i < lines.len() {
        let Some(line) = lines.get(i) else {
            break;
        };
        if let Some(m) = parse_grok_folder_header(line) {
            keys.push(m);
            i += 1;
            while i < lines.len() {
                let Some(body) = lines.get(i) else {
                    break;
                };
                if is_grok_header(body) || parse_grok_folder_header(body).is_some() {
                    break;
                }
                i += 1;
            }
            continue;
        }
        i += 1;
    }

    let key = grok_trust_key(&cwd_str, &keys, platform);
    let section = format!(
        "[folders.'{}']\ntrusted = true\ndecided_at = {}\n",
        key.replace('\'', "\\'"),
        decided_at
    );

    i = 0;
    while i < lines.len() {
        let Some(header_line) = lines.get(i) else {
            break;
        };
        if let Some(existing_key) = parse_grok_folder_header(header_line) {
            let start = i;
            i += 1;
            while i < lines.len() {
                let Some(body) = lines.get(i) else {
                    break;
                };
                if is_grok_header(body) || parse_grok_folder_header(body).is_some() {
                    break;
                }
                i += 1;
            }
            if !grok_path_equal(&existing_key, &key) {
                continue;
            }
            let body_lines = lines.get(start + 1..i).unwrap_or(&[]);
            if grok_section_trusted(body_lines) {
                return SeedResult {
                    ok: true,
                    updated: vec![],
                    key: existing_key,
                    message: None,
                };
            }
            let mut rewritten = body_lines.to_vec();
            let mut has_trusted = false;
            let mut has_decided = false;
            for bl in &mut rewritten {
                if bl.trim_start().starts_with("trusted") {
                    has_trusted = true;
                    *bl = bl
                        .split('=')
                        .next()
                        .map(|p| format!("{}true", p.trim_end()))
                        .unwrap_or_else(|| "trusted = true".to_string());
                }
                if bl.trim_start().starts_with("decided_at") {
                    has_decided = true;
                    *bl = format!("decided_at = {}", decided_at);
                }
            }
            if !has_trusted {
                rewritten.push("trusted = true".to_string());
            }
            if !has_decided {
                rewritten.push(format!("decided_at = {}", decided_at));
            }
            let mut next = lines.get(..start + 1).unwrap_or(&[]).to_vec();
            next.extend(rewritten);
            next.extend(lines.get(i..).unwrap_or(&[]).iter().cloned());
            while next.last().map(|l| l.is_empty()).unwrap_or(false) {
                next.pop();
            }
            if let Err(err) = write_atomic(&path, format!("{}\n", next.join("\n")).as_bytes()) {
                return SeedResult {
                    ok: false,
                    updated: vec![],
                    key,
                    message: Some(format!("could not write {}: {err}", path.display())),
                };
            }
            return SeedResult {
                ok: true,
                updated: vec![path.clone()],
                key,
                message: None,
            };
        }
        i += 1;
    }

    let prefix = if existing_text.is_empty() || existing_text.ends_with('\n') {
        existing_text.clone()
    } else {
        format!("{}\n", existing_text)
    };
    let gap = if prefix.is_empty() || prefix.ends_with("\n\n") {
        String::new()
    } else {
        "\n".to_string()
    };
    if let Err(err) = write_atomic(&path, format!("{}{}{}", prefix, gap, section).as_bytes()) {
        return SeedResult {
            ok: false,
            updated: vec![],
            key,
            message: Some(format!("could not write {}: {err}", path.display())),
        };
    }
    SeedResult {
        ok: true,
        updated: vec![path.clone()],
        key,
        message: None,
    }
}

fn parse_grok_folder_header(line: &str) -> Option<String> {
    let trimmed = line.trim();
    if !trimmed.starts_with("[folders.'") {
        return None;
    }
    let rest = trimmed.trim_start_matches("[folders.'");
    let end = rest.find("']")?;
    Some(rest[..end].replace("\\'", "'"))
}

fn is_grok_header(line: &str) -> bool {
    let trimmed = line.trim();
    trimmed.starts_with('[') && trimmed.ends_with(']') && !trimmed.starts_with("[folders.'")
}

fn grok_section_trusted(body_lines: &[String]) -> bool {
    for line in body_lines {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("trusted") {
            if rest.contains('=') {
                let val = rest.split('=').nth(1).unwrap_or("").trim();
                return val.eq_ignore_ascii_case("true");
            }
        }
    }
    false
}

fn agy_path_equal(a: &str, b: &str) -> bool {
    a.replace('\\', "/") == b.replace('\\', "/")
}

fn agy_slash_style(existing: &[String]) -> Option<char> {
    let back = existing.iter().any(|k| k.contains('\\'));
    let fwd = existing.iter().any(|k| k.contains('/'));
    if back && !fwd {
        Some('\\')
    } else if fwd && !back {
        Some('/')
    } else {
        None
    }
}

pub fn agy_trust_key(cwd: &str, existing: &[String]) -> String {
    match agy_slash_style(existing) {
        Some('\\') => cwd.replace('/', "\\"),
        Some('/') => cwd.replace('\\', "/"),
        _ => cwd.to_string(),
    }
}

pub fn seed_agy_trust(cwd: &Path, home: &Path) -> SeedResult {
    let cwd_str = cwd.to_string_lossy().to_string();
    if cwd_str.is_empty() {
        return SeedResult {
            ok: false,
            updated: vec![],
            key: cwd_str,
            message: Some("empty cwd".into()),
        };
    }
    let path = home
        .join(".gemini")
        .join("antigravity-cli")
        .join("settings.json");
    if !path.exists() {
        let key = agy_trust_key(&cwd_str, &[]);
        let body = serde_json::json!({ "trustedWorkspaces": [key] });
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).ok();
        }
        let text = serde_json::to_string_pretty(&body).map_err(|e| e.to_string());
        match text {
            Ok(text) => {
                if write_atomic(&path, format!("{}\n", text).as_bytes()).is_err() {
                    return SeedResult {
                        ok: false,
                        updated: vec![],
                        key,
                        message: Some(format!("could not write {}", path.display())),
                    };
                }
                return SeedResult {
                    ok: true,
                    updated: vec![path],
                    key,
                    message: None,
                };
            }
            Err(msg) => {
                return SeedResult {
                    ok: false,
                    updated: vec![],
                    key,
                    message: Some(msg),
                };
            }
        }
    }

    let text = match fs::read_to_string(&path) {
        Ok(t) => t,
        Err(_) => {
            return SeedResult {
                ok: false,
                updated: vec![],
                key: cwd_str,
                message: Some(format!("could not read {}", path.display())),
            };
        }
    };
    let raw: serde_json::Value = match serde_json::from_str(&text) {
        Ok(v) => v,
        Err(_) => {
            return SeedResult {
                ok: false,
                updated: vec![],
                key: cwd_str,
                message: Some(format!("not json: {}", path.display())),
            };
        }
    };
    let mut root = match raw.as_object() {
        Some(m) => m.clone(),
        None => {
            return SeedResult {
                ok: false,
                updated: vec![],
                key: cwd_str,
                message: Some(format!("not a json object: {}", path.display())),
            };
        }
    };
    let current = root.get("trustedWorkspaces");
    if current.is_some_and(|v| !v.is_array()) {
        return SeedResult {
            ok: false,
            updated: vec![],
            key: cwd_str,
            message: Some(format!(
                "trustedWorkspaces is not an array: {}",
                path.display()
            )),
        };
    }
    let list = current
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let strings: Vec<String> = list
        .iter()
        .filter_map(|v| v.as_str().map(String::from))
        .collect();
    let key = agy_trust_key(&cwd_str, &strings);
    for s in &strings {
        if agy_path_equal(s, &key) {
            return SeedResult {
                ok: true,
                updated: vec![],
                key: s.clone(),
                message: None,
            };
        }
    }
    let mut next = list;
    next.push(serde_json::Value::String(key.clone()));
    root.insert(
        "trustedWorkspaces".to_string(),
        serde_json::Value::Array(next),
    );
    let text =
        serde_json::to_string_pretty(&serde_json::Value::Object(root)).map_err(|e| e.to_string());
    match text {
        Ok(text) => {
            if write_atomic(&path, format!("{}\n", text).as_bytes()).is_err() {
                return SeedResult {
                    ok: false,
                    updated: vec![],
                    key,
                    message: Some(format!("could not write {}", path.display())),
                };
            }
            SeedResult {
                ok: true,
                updated: vec![path],
                key,
                message: None,
            }
        }
        Err(msg) => SeedResult {
            ok: false,
            updated: vec![],
            key,
            message: Some(msg),
        },
    }
}

pub fn seed_agy_trust_locked(cwd: &Path) -> SeedResult {
    let _guard = lock_trust();
    let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
    seed_agy_trust(cwd, &home)
}

pub fn seed_backend_trust(backend: crate::types::BackendId, cwd: &Path) -> SeedResult {
    match backend {
        crate::types::BackendId::ClaudeCode => seed_claude_trust_locked(cwd),
        crate::types::BackendId::Grok => seed_grok_trust_locked(cwd),
        crate::types::BackendId::Agy => seed_agy_trust_locked(cwd),
        _ => SeedResult {
            ok: true,
            updated: vec![],
            key: String::new(),
            message: None,
        },
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    /// R13: another writer between our read and our write is merged, not lost.
    #[test]
    fn concurrent_writer_between_read_and_write_is_kept() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join(".claude.json");
        fs::write(&path, r#"{"numStartups":1,"projects":{}}"#).unwrap();
        let mut raced = false;
        let outcome = seed_one_claude_file_with(&path, "C:/wt", &[], &mut || {
            if !raced {
                raced = true;
                fs::write(&path, r#"{"numStartups":2,"other":true,"projects":{}}"#).unwrap();
            }
        })
        .expect("seed");
        assert!(matches!(outcome, SeedOneOutcome::Updated { created: true }));
        let v: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(v["numStartups"], 2);
        assert_eq!(v["other"], true);
        assert_eq!(v["projects"]["C:/wt"]["hasTrustDialogAccepted"], true);
    }

    /// R13: a write that fails leaves the original file intact.
    #[test]
    fn failed_atomic_write_leaves_original_intact() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join(".claude.json");
        fs::write(&path, "original").unwrap();
        // Occupy the temp name with a directory so the temp cannot be opened.
        let tmp = temp
            .path()
            .join(format!("..claude.json.{}.thickapi.tmp", std::process::id()));
        fs::create_dir(&tmp).unwrap();
        assert!(write_atomic(&path, b"new").is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), "original");
    }

    #[test]
    fn atomic_write_replaces_and_leaves_no_temp() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("f.json");
        fs::write(&path, "old").unwrap();
        write_atomic(&path, b"new").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "new");
        assert_eq!(fs::read_dir(temp.path()).unwrap().count(), 1);
    }

    /// R13: entries thickAPI created are pruned once their cwd is gone; entries it did not
    /// create, and entries whose cwd still exists, are kept.
    #[test]
    fn prunes_only_own_entries_for_missing_cwds() {
        let home = tempfile::tempdir().expect("home");
        let live_wt = tempfile::tempdir().expect("live wt");
        let gone_wt = home.path().join("gone-wt");
        fs::create_dir(&gone_wt).unwrap();
        let claude_json = home.path().join(".claude").join(".claude.json");
        fs::create_dir_all(claude_json.parent().unwrap()).unwrap();
        fs::write(
            &claude_json,
            r#"{"projects":{"C:/users/someone/elsewhere":{"hasTrustDialogAccepted":true}}}"#,
        )
        .unwrap();

        let r = seed_claude_trust(&gone_wt, home.path(), None);
        assert!(r.ok, "{:?}", r.message);
        let r = seed_claude_trust(live_wt.path(), home.path(), None);
        assert!(r.ok);
        let gone_key = claude_trust_key(&gone_wt.to_string_lossy());
        let live_key = claude_trust_key(&live_wt.path().to_string_lossy());
        assert_eq!(
            read_ledger(&claude_seeded_ledger(home.path())),
            vec![gone_key.clone(), live_key.clone()]
        );

        fs::remove_dir(&gone_wt).unwrap();
        let third = tempfile::tempdir().expect("third wt");
        let r = seed_claude_trust(third.path(), home.path(), None);
        assert!(r.ok);
        let v: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&claude_json).unwrap()).unwrap();
        let projects = v["projects"].as_object().unwrap();
        assert!(
            !projects.contains_key(&gone_key),
            "own entry for a gone cwd stays"
        );
        assert!(projects.contains_key(&live_key));
        assert!(
            projects.contains_key("C:/users/someone/elsewhere"),
            "foreign entry pruned"
        );
        let ledger = read_ledger(&claude_seeded_ledger(home.path()));
        assert!(!ledger.contains(&gone_key));
    }
}
