use std::path::Path;

use anyhow::{Context, Result};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct KeysConfig {
    #[serde(default)]
    pub keys: Vec<KeyEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub struct KeyEntry {
    pub id: String,
    pub hash: String,
    pub scopes: Vec<String>,
    /// RFC3339 mint time (`keys list`, D3 deliverable 2). `#[serde(default)]` so a `keys.yaml`
    /// written before this field existed still loads; `list` prints it blank for those rows.
    #[serde(default)]
    pub created: String,
}

pub const SCOPE_TURN_WRITE: &str = "turn:write";
pub const SCOPE_MODELS_READ: &str = "models:read";
/// Thin `/v1/chat/completions` scope (SPEC §19.9). `turn:write` does not imply it.
pub const SCOPE_CHAT_WRITE: &str = "chat:write";

/// Scopes `thickapi keys create` mints when `--scope` is absent (D3): a thin key, the shape
/// Pi and most callers want. `--scope turn:write` (repeatable) for a thick / script key.
pub const DEFAULT_SCOPES: &[&str] = &[SCOPE_CHAT_WRITE, SCOPE_MODELS_READ];
/// Scopes `sync-pi` mints for Pi's `models.json` (SPEC §19.9): never a turn credential.
pub const PI_SCOPES: &[&str] = &[SCOPE_CHAT_WRITE, SCOPE_MODELS_READ];

pub fn hash_token(token: &str) -> String {
    let digest = Sha256::digest(token.as_bytes());
    format!("sha256:{}", hex::encode(digest))
}

pub fn generate_token() -> String {
    let mut bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut bytes);
    format!("ta_{}", hex::encode(bytes))
}

pub fn verify_token(token: &str, expected_hash: &str) -> bool {
    hash_token(token) == expected_hash
}

pub fn find_key_by_token<'a>(config: &'a KeysConfig, token: &str) -> Option<&'a KeyEntry> {
    let hashed = hash_token(token);
    config.keys.iter().find(|k| k.hash == hashed)
}

/// `chat:write` satisfies `models:read` (SPEC §19.6); no other implication exists.
pub fn has_scope(entry: &KeyEntry, scope: &str) -> bool {
    entry.scopes.iter().any(|s| s == scope)
        || (scope == SCOPE_MODELS_READ && entry.scopes.iter().any(|s| s == SCOPE_CHAT_WRITE))
}

pub fn load_keys(path: &Path) -> Result<KeysConfig> {
    if !path.exists() {
        return Ok(KeysConfig::default());
    }
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("read keys config {}", path.display()))?;
    serde_yaml::from_str(&text).with_context(|| format!("parse keys config {}", path.display()))
}

/// Key lookup that tracks `keys.yaml` by mtime.
///
/// Construction (or the first lookup) loads the file. Later lookups `stat` it: an unchanged
/// mtime answers from cache, so an unknown token does not hammer the disk. A changed mtime
/// re-reads, so `sync-pi` mint and a revocation both take effect without a restart. There is
/// no always-valid start-up snapshot — lookup uses only the current file contents.
#[derive(Debug)]
pub struct LiveKeys {
    path: std::path::PathBuf,
    cache: std::sync::RwLock<ReloadCache>,
}

#[derive(Debug)]
struct ReloadCache {
    keys: KeysConfig,
    mtime: Option<std::time::SystemTime>,
}

fn file_mtime(path: &Path) -> Option<std::time::SystemTime> {
    std::fs::metadata(path).ok().and_then(|m| m.modified().ok())
}

impl LiveKeys {
    pub fn new(path: std::path::PathBuf) -> Self {
        let keys = match load_keys(&path) {
            Ok(keys) => keys,
            Err(err) => {
                tracing::warn!(path = %path.display(), error = %err, "keys.yaml load failed");
                KeysConfig::default()
            }
        };
        Self::from_snapshot(path, keys)
    }

    /// Seed from an already-loaded bundle (serve start / tests) and still re-read on mtime change.
    pub fn from_snapshot(path: std::path::PathBuf, keys: KeysConfig) -> Self {
        let mtime = file_mtime(&path);
        Self {
            path,
            cache: std::sync::RwLock::new(ReloadCache { keys, mtime }),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Look `token` up in the current `keys.yaml` (reloaded when mtime changes).
    pub fn find(&self, token: &str) -> Option<KeyEntry> {
        let disk_mtime = file_mtime(&self.path);
        {
            let cache = self.cache.read().unwrap_or_else(|e| e.into_inner());
            if cache.mtime == disk_mtime {
                return find_key_by_token(&cache.keys, token).cloned();
            }
        }
        let mut cache = self.cache.write().unwrap_or_else(|e| e.into_inner());
        let disk_mtime = file_mtime(&self.path);
        if cache.mtime == disk_mtime {
            return find_key_by_token(&cache.keys, token).cloned();
        }
        match load_keys(&self.path) {
            Ok(keys) => {
                let hit = find_key_by_token(&keys, token).cloned();
                cache.keys = keys;
                cache.mtime = file_mtime(&self.path);
                hit
            }
            Err(err) => {
                tracing::warn!(path = %self.path.display(), error = %err, "keys.yaml re-read failed");
                cache.keys = KeysConfig::default();
                cache.mtime = disk_mtime;
                None
            }
        }
    }
}

pub fn save_keys(path: &Path, config: &KeysConfig) -> Result<()> {
    let text = serde_yaml::to_string(config)?;
    crate::thin::private_dir::write_private_atomic(path, text.as_bytes())
        .with_context(|| format!("write keys config {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    #[test]
    fn chat_write_implies_models_read_only() {
        let chat = KeyEntry {
            id: "pi".into(),
            hash: String::new(),
            scopes: vec![SCOPE_CHAT_WRITE.into()],
            ..Default::default()
        };
        assert!(has_scope(&chat, SCOPE_MODELS_READ));
        assert!(has_scope(&chat, SCOPE_CHAT_WRITE));
        assert!(!has_scope(&chat, SCOPE_TURN_WRITE));
        let turn = KeyEntry {
            id: "draftr".into(),
            hash: String::new(),
            scopes: vec![SCOPE_TURN_WRITE.into()],
            ..Default::default()
        };
        assert!(!has_scope(&turn, SCOPE_CHAT_WRITE));
        assert!(!has_scope(&turn, SCOPE_MODELS_READ));
    }

    #[test]
    fn key_hash_verify() {
        let token = "ta_test_token_value";
        let hash = hash_token(token);
        assert!(hash.starts_with("sha256:"));
        assert!(verify_token(token, &hash));
        assert!(!verify_token("ta_wrong", &hash));
    }

    #[test]
    fn live_keys_picks_up_keys_minted_after_start() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("keys.yaml");
        let live = LiveKeys::new(path.clone());
        let token = generate_token();

        assert!(live.find(&token).is_none());

        let minted = KeysConfig {
            keys: vec![KeyEntry {
                id: "pi-thin".into(),
                hash: hash_token(&token),
                scopes: vec![SCOPE_CHAT_WRITE.into()],
                ..Default::default()
            }],
        };
        std::fs::write(&path, serde_yaml::to_string(&minted).unwrap()).unwrap();

        let hit = live
            .find(&token)
            .expect("mtime change finds the minted key");
        assert_eq!(hit.id, "pi-thin");
        assert!(live.find(&token).is_some());
    }

    #[test]
    fn live_keys_drops_a_key_removed_from_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("keys.yaml");
        let token = generate_token();
        let present = KeysConfig {
            keys: vec![KeyEntry {
                id: "keep".into(),
                hash: hash_token(&token),
                scopes: vec![SCOPE_CHAT_WRITE.into()],
                ..Default::default()
            }],
        };
        std::fs::write(&path, serde_yaml::to_string(&present).unwrap()).unwrap();
        let live = LiveKeys::new(path.clone());
        assert_eq!(live.find(&token).expect("present at start").id, "keep");

        // Two quick writes can share an mtime on coarse-clock filesystems (windows CI), so
        // put the second write's mtime explicitly 2 s past the first (no sleep).
        let first_mtime = std::fs::metadata(&path).unwrap().modified().unwrap();
        std::fs::write(&path, "keys: []\n").unwrap();
        let file = std::fs::File::options().write(true).open(&path).unwrap();
        file.set_modified(first_mtime + std::time::Duration::from_secs(2))
            .unwrap();
        drop(file);
        assert!(
            live.find(&token).is_none(),
            "a key deleted from keys.yaml must not stay valid"
        );
    }

    #[test]
    fn live_keys_does_not_reread_when_mtime_is_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("keys.yaml");
        let token = generate_token();
        let present = KeysConfig {
            keys: vec![KeyEntry {
                id: "keep".into(),
                hash: hash_token(&token),
                scopes: vec![SCOPE_CHAT_WRITE.into()],
                ..Default::default()
            }],
        };
        std::fs::write(&path, serde_yaml::to_string(&present).unwrap()).unwrap();
        let live = LiveKeys::new(path.clone());
        assert!(live.find(&token).is_some());

        let orig_mtime = std::fs::metadata(&path).unwrap().modified().unwrap();
        std::fs::write(&path, "keys: []\n").unwrap();
        let file = std::fs::File::options().write(true).open(&path).unwrap();
        let times = std::fs::FileTimes::new().set_modified(orig_mtime);
        file.set_times(times).unwrap();
        drop(file);

        assert!(
            live.find(&token).is_some(),
            "unchanged mtime must not re-read (stat is enough)"
        );
    }
}
