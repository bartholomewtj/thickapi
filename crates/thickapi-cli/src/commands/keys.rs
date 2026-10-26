use std::path::Path;

use anyhow::Result;
use thickapi_core::config::keys::{load_keys, DEFAULT_SCOPES};

/// Mint a key. `scopes` empty means the default thin shape (`chat:write`, `models:read`, D3
/// deliverable 1); `sync-pi`'s own mint stays on `PI_SCOPES` and is untouched by this default.
pub fn create_key(config_dir: &Path, label: String, scopes: &[&str]) -> Result<String> {
    let scopes: &[&str] = if scopes.is_empty() {
        DEFAULT_SCOPES
    } else {
        scopes
    };
    super::sync_pi::mint_key(config_dir, &label, scopes)
}

pub fn create(config_dir: &Path, label: String, scopes: &[&str]) -> Result<()> {
    let token = create_key(config_dir, label, scopes)?;
    println!("Created API key (store this plaintext securely; it will not be shown again):");
    println!("{}", token);
    Ok(())
}

/// `keys list`: id, scopes, created — never the hash (D3 deliverable 2).
pub fn list(config_dir: &Path) -> Result<()> {
    let keys_path = config_dir.join("keys.yaml");
    let config = load_keys(&keys_path)?;
    if config.keys.is_empty() {
        println!("no keys in {}", keys_path.display());
        return Ok(());
    }
    println!("{:<20} {:<36} created", "id", "scopes");
    for key in &config.keys {
        let created = if key.created.is_empty() {
            "unknown"
        } else {
            key.created.as_str()
        };
        println!("{:<20} {:<36} {}", key.id, key.scopes.join(","), created);
    }
    Ok(())
}

/// `keys revoke <id>`: drop every row with this id via `save_keys` (`write_private_atomic`);
/// `LiveKeys` picks the change up on the next request by `keys.yaml` mtime (SPEC §10.5), no
/// restart (D3 deliverable 2).
pub fn revoke(config_dir: &Path, id: &str) -> Result<()> {
    let removed = super::sync_pi::revoke_key(config_dir, id)?;
    if removed == 0 {
        println!("no key with id `{id}` in keys.yaml");
    } else {
        println!("revoked {removed} key(s) with id `{id}`");
    }
    Ok(())
}
