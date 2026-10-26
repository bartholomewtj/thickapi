use std::fs::{self, File};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};

use crate::adapters::SpawnPlan;
use crate::types::TurnResult;

/// One directory name for a client-owned id (`turn_id`, `Idempotency-Key`, `session_id`).
/// Ids are opaque (SPEC §4), so nothing is rejected: an id that is already a plain file name
/// is used as-is, anything else — separators, `..`, a drive, a device name — becomes a hash.
pub fn path_component(id: &str) -> String {
    const RESERVED: [&str; 4] = ["con", "prn", "aux", "nul"];
    let stem = id
        .split('.')
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    let device = RESERVED.contains(&stem.as_str())
        || (stem.len() == 4
            && (stem.starts_with("com") || stem.starts_with("lpt"))
            && stem.ends_with(|c: char| c.is_ascii_digit()));
    let plain = (1..=128).contains(&id.len())
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
        && !id.starts_with('.')
        && !id.ends_with('.')
        && !device;
    if plain {
        return id.to_string();
    }
    let digest = format!("{:x}", Sha256::digest(id.as_bytes()));
    format!("h-{}", digest.get(..32).unwrap_or(&digest))
}

pub fn turn_artifact_dir(data_dir: &Path, turn_id: &str) -> PathBuf {
    data_dir.join("turns").join(path_component(turn_id))
}

pub fn ensure_turn_dir(data_dir: &Path, turn_id: &str) -> Result<PathBuf> {
    let dir = turn_artifact_dir(data_dir, turn_id);
    fs::create_dir_all(&dir).with_context(|| format!("create artifact dir {}", dir.display()))?;
    Ok(dir)
}

pub fn write_spawn_json(dir: &Path, plan: &SpawnPlan) -> Result<()> {
    let payload = serde_json::json!({
        "program": plan.program,
        "args": plan.args,
        "cwd": plan.cwd,
        "backend": plan.backend,
    });
    let path = dir.join("spawn.json");
    fs::write(&path, serde_json::to_string_pretty(&payload)?)
        .with_context(|| format!("write {}", path.display()))?;
    Ok(())
}

pub fn open_stdout_log(dir: &Path) -> Result<File> {
    let path = dir.join("stdout.jsonl");
    File::create(&path).with_context(|| format!("create {}", path.display()))
}

pub fn open_stderr_log(dir: &Path) -> Result<File> {
    let path = dir.join("stderr.log");
    File::create(&path).with_context(|| format!("create {}", path.display()))
}

pub fn write_result_json(dir: &Path, result: &TurnResult) -> Result<()> {
    let path = dir.join("result.json");
    fs::write(&path, serde_json::to_string_pretty(result)?)
        .with_context(|| format!("write {}", path.display()))?;
    Ok(())
}

pub fn prompt_sha256(prompt: &str) -> String {
    format!("{:x}", Sha256::digest(prompt.as_bytes()))
}

pub fn stored_prompt_hash(dir: &Path) -> Option<String> {
    fs::read_to_string(dir.join("prompt.sha256"))
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// First persist wins. Returns the hash now on disk.
pub fn persist_prompt_hash(dir: &Path, prompt: &str) -> Result<String> {
    use std::io::Write;
    let hash = prompt_sha256(prompt);
    let path = dir.join("prompt.sha256");
    match fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
    {
        Ok(mut file) => {
            file.write_all(hash.as_bytes())
                .with_context(|| format!("write {}", path.display()))?;
            Ok(hash)
        }
        Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
            Ok(stored_prompt_hash(dir).unwrap_or(hash))
        }
        Err(err) => Err(err).with_context(|| format!("create {}", path.display())),
    }
}

/// `None` when this turn has no stored prompt (legacy artifacts). `Some(true)` is a
/// re-`POST` with a different prompt.
pub fn turn_prompt_conflicts(data_dir: &Path, turn_id: &str, prompt: &str) -> Option<bool> {
    let stored = stored_prompt_hash(&turn_artifact_dir(data_dir, turn_id))?;
    Some(stored != prompt_sha256(prompt))
}

pub fn remember_turn_prompt(data_dir: &Path, turn_id: &str, prompt: &str) -> Result<String> {
    let dir = ensure_turn_dir(data_dir, turn_id)?;
    persist_prompt_hash(&dir, prompt)
}

pub fn append_event_jsonl(dir: &Path, seq: u64, event: &crate::types::TurnEvent) -> Result<()> {
    use std::io::Write;
    let path = dir.join("events.jsonl");
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("open {}", path.display()))?;
    let line = serde_json::json!({ "seq": seq, "event": event });
    writeln!(file, "{}", serde_json::to_string(&line)?)?;
    Ok(())
}

/// Rewrite the full turn event log (used on turn_end after buffering assistant deltas in memory).
pub fn write_events_jsonl(dir: &Path, events: &[crate::types::StoredEvent]) -> Result<()> {
    use std::io::Write;
    let path = dir.join("events.jsonl");
    let mut file = File::create(&path).with_context(|| format!("create {}", path.display()))?;
    for stored in events {
        let line = serde_json::json!({ "seq": stored.seq, "event": &stored.event });
        writeln!(file, "{}", serde_json::to_string(&line)?)?;
    }
    Ok(())
}

pub fn read_events_jsonl(dir: &Path) -> Result<Vec<crate::types::StoredEvent>> {
    let path = dir.join("events.jsonl");
    if !path.exists() {
        return Ok(vec![]);
    }
    let content = fs::read_to_string(&path)?;
    let mut events = Vec::new();
    for line in content.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let stored: crate::types::StoredEvent = serde_json::from_str(line)?;
        events.push(stored);
    }
    Ok(events)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    #[test]
    fn plain_ids_name_their_own_directory() {
        for id in [
            "sess-1",
            "3f2b8c1e-0d4a-4c7e-9a51-7d1c2b3a4e5f",
            "run_7.retry",
        ] {
            assert_eq!(path_component(id), id);
        }
    }

    #[test]
    fn an_id_can_never_leave_its_parent_directory() {
        let data = Path::new("data");
        for id in [
            "..",
            "../../../Users/me/Desktop",
            r"..\..\Windows",
            r"C:\Windows\Temp",
            "/etc/passwd",
            "a/b",
            "draftr:run:seat",
            ".hidden",
            "trailing.",
            "NUL",
            "com1.txt",
            "",
            &"x".repeat(200),
        ] {
            let name = path_component(id);
            assert!(
                name.starts_with("h-") && name.len() == 34,
                "{id:?} -> {name}"
            );
            let dir = turn_artifact_dir(data, id);
            assert_eq!(dir.parent(), Some(data.join("turns").as_path()), "{id:?}");
        }
        assert_ne!(path_component("a/b"), path_component(r"a\b"));
    }

    #[test]
    fn first_prompt_hash_wins_and_a_different_prompt_conflicts() {
        let dir = tempfile::tempdir().expect("temp");
        let turn_id = "t-hash";
        let data = dir.path();
        remember_turn_prompt(data, turn_id, "hello").expect("first");
        remember_turn_prompt(data, turn_id, "goodbye").expect("second is a no-op");
        assert_eq!(turn_prompt_conflicts(data, turn_id, "hello"), Some(false));
        assert_eq!(turn_prompt_conflicts(data, turn_id, "goodbye"), Some(true));
        assert_eq!(turn_prompt_conflicts(data, "never-seen", "hello"), None);
    }
}
