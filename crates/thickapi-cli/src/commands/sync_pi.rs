//! `thickapi sync-pi` (SPEC §19.9–§19.10): write Pi's `providers.subs` as a plain
//! `openai-completions` provider on `/v1`, keyed with a `chat:write`-only token, with the thin
//! catalogue from `GET /v1/models?format=pi`.
//!
//! The thick `(thickapi)` lane retired at T5: a leftover `providers.thick` block (or a pre-T1
//! `providers.subs` still marked `api: thickapi`) is removed and its `pi-sync` key revoked, so
//! no `turn:write` credential stays in Pi's `models.json`.
//!
//! Refuses to write unless `/ready` answers; warns when `install-service` is not registered.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use thickapi_core::config::keys::{
    find_key_by_token, generate_token, has_scope, hash_token, load_keys, save_keys, KeyEntry,
    PI_SCOPES, SCOPE_TURN_WRITE,
};
use thickapi_core::config::router::{is_unspecified_host, split_listen};
use thickapi_core::thin::private_dir::write_private_atomic;

pub const THIN_KEY_LABEL: &str = "pi-thin";
/// The retired thick lane's key label; `sync-pi` revokes it when it finds one.
pub const THICK_KEY_LABEL: &str = "pi-sync";
/// Mirrors install_service::TASK_NAME.
const SERVICE_TASK_NAME: &str = "thickapi-serve";

#[derive(Debug)]
pub struct SyncOutcome {
    pub models_path: PathBuf,
    pub thin_models: usize,
    /// A retired `providers.thick` block was found and removed.
    pub thick_removed: bool,
}

/// Base URL Pi (and this sync) call: `router.yaml`'s `listen`, rewritten to loopback when it is
/// a wildcard bind (`0.0.0.0`, `::`) — Pi on Windows cannot connect to `0.0.0.0` (SPEC §1.6,
/// §19.10; D3 deliverable 5). `config::load_config` already refused a wildcard bind with
/// `lan: false`, so reaching here means `lan: true` allowed it.
pub fn listen_base_url(listen: &str) -> String {
    let (host, port) = split_listen(listen);
    if is_unspecified_host(host) {
        format!("http://127.0.0.1:{port}")
    } else {
        format!("http://{listen}")
    }
}

pub fn run(config_dir: &Path, key: Option<String>) -> Result<()> {
    run_in(config_dir, key, &pi_models_path()?, &pi_sidecar_path()?)
}

pub fn run_in(
    config_dir: &Path,
    key: Option<String>,
    models_path: &Path,
    sidecar_path: &Path,
) -> Result<()> {
    let config = thickapi_core::load_config(config_dir)?;
    let base = listen_base_url(&config.router.listen);

    // Pi loads `models.json` once, at startup, and keeps that `apiKey` for the life of the
    // process. Minting on every sync revokes the key the running process still sends, and the
    // next turn is 401. Reuse the token the file already holds when the daemon would still
    // accept it. Mint only when that token is missing or would be rejected. A minted key sits
    // beside the old one until models.json holds it: a failed sync must not leave Pi with a
    // key that `keys.yaml` no longer knows.
    let explicit_key = key.is_some();
    let (thin_key, minted) = match key {
        Some(k) => (k, false),
        None => match reusable_pi_key(config_dir, models_path)? {
            Some(existing) => (existing, false),
            None => (mint_key(config_dir, THIN_KEY_LABEL, PI_SCOPES)?, true),
        },
    };
    let synced = sync_with_key(config_dir, &base, &thin_key, models_path, sidecar_path);
    if minted {
        // A tidy-up failure must not hide the sync's own result.
        if let Err(e) = settle_minted_key(config_dir, models_path, &thin_key) {
            eprintln!("[warn] keys.yaml still holds a stale `{THIN_KEY_LABEL}` key: {e:#}");
        }
    }
    let (outcome, thin_models) = synced?;
    if minted {
        println!("minted a new pi-thin key; restart Pi so it loads models.json");
    } else if !explicit_key {
        println!("kept the existing pi-thin key");
    }

    println!(
        "synced providers.subs ({} thin model(s), openai-completions on {}/v1) to {}",
        outcome.thin_models,
        base,
        outcome.models_path.display()
    );
    if outcome.thick_removed {
        let revoked = revoke_key(config_dir, THICK_KEY_LABEL)?;
        println!(
            "removed the retired providers.thick block (T5); revoked {} `{}` turn:write key(s)",
            revoked, THICK_KEY_LABEL
        );
    }
    if thin_models.is_empty() {
        println!(
            "[warn] no thin-capable rows: set `transport: oauth_direct|cli_bridge` on models.yaml rows (SPEC §19.6)"
        );
    }
    if !service_installed() {
        println!("[warn] thickapi-serve scheduled task not found; run `thickapi install-service` so Pi can rely on the daemon");
    }
    Ok(())
}

/// Everything that needs the daemon and the key. The keyless sidecar goes first so the
/// step that hands Pi the key is the last one that can fail.
fn sync_with_key(
    config_dir: &Path,
    base: &str,
    thin_key: &str,
    models_path: &Path,
    sidecar_path: &Path,
) -> Result<(SyncOutcome, Vec<Value>)> {
    if !probe_ready(base, thin_key)? {
        bail!(
            "GET {}/ready did not answer with 200; start the daemon (`thickapi serve`) and retry — models.json and keys.yaml left unchanged",
            base
        );
    }
    let thin_models = fetch_models(base, thin_key, "pi")?;
    let subs_block = build_subs_block(base, thin_key, &thin_models);
    write_sidecar(sidecar_path, config_dir, base)?;
    let outcome = merge_blocks(models_path, &subs_block)?;
    Ok((outcome, thin_models))
}

/// `providers.subs.apiKey` when the file exists and the field is a non-empty string.
/// An unparseable `models.json` is an error: rewriting it would drop other providers.
fn pi_key_in_models(models_path: &Path) -> Result<Option<String>> {
    if !models_path.exists() {
        return Ok(None);
    }
    let doc = read_doc(models_path)?;
    Ok(doc
        .pointer("/providers/subs/apiKey")
        .and_then(Value::as_str)
        .filter(|token| !token.is_empty())
        .map(str::to_string))
}

/// The `pi-thin` token Pi already has on disk, when `keys.yaml` would still accept it.
///
/// Returns `None` when the file has no token, the daemon would reject it, the row is a
/// different label, or it carries `turn:write` (that credential must not stay in `models.json`).
fn reusable_pi_key(config_dir: &Path, models_path: &Path) -> Result<Option<String>> {
    let Some(token) = pi_key_in_models(models_path)? else {
        return Ok(None);
    };
    let keys = load_keys(&config_dir.join("keys.yaml"))?;
    let Some(entry) = find_key_by_token(&keys, &token) else {
        return Ok(None);
    };
    if entry.id != THIN_KEY_LABEL || entry.scopes.iter().any(|s| s == SCOPE_TURN_WRITE) {
        return Ok(None);
    }
    if !PI_SCOPES.iter().all(|scope| has_scope(entry, scope)) {
        return Ok(None);
    }
    Ok(Some(token))
}

/// Append a new key under label (`keys create`, and `sync-pi` before it retires the old one).
pub fn mint_key(config_dir: &Path, label: &str, scopes: &[&str]) -> Result<String> {
    fs::create_dir_all(config_dir)
        .with_context(|| format!("create config dir {}", config_dir.display()))?;
    let keys_path = config_dir.join("keys.yaml");
    let mut config = load_keys(&keys_path)?;
    let token = generate_token();
    // The daemon picks a new key up when keys.yaml mtime changes (LiveKeys): no restart.
    config.keys.push(KeyEntry {
        id: label.into(),
        hash: hash_token(&token),
        scopes: scopes.iter().map(|s| (*s).to_string()).collect(),
        created: chrono::Utc::now().to_rfc3339(),
    });
    save_keys(&keys_path, &config)?;
    Ok(token)
}

/// After a sync attempt, leave exactly the key Pi holds. What models.json contains decides,
/// not whether the sync returned `Ok`: a write can land and still report an error (the
/// post-rename ACL check), and the key Pi was given must never be the one retired.
fn settle_minted_key(config_dir: &Path, models_path: &Path, minted: &str) -> Result<()> {
    let pi_holds_it = fs::read_to_string(models_path).is_ok_and(|text| text.contains(minted));
    if pi_holds_it {
        retire_other_keys(config_dir, THIN_KEY_LABEL, minted)
    } else {
        retire_key(config_dir, minted)
    }
}

/// Drop one key by its plaintext (a minted key whose sync failed).
fn retire_key(config_dir: &Path, token: &str) -> Result<()> {
    let hash = hash_token(token);
    rewrite_keys(config_dir, |k| k.hash != hash)
}

/// A label names one live key: once `keep` is in models.json, drop its predecessors.
fn retire_other_keys(config_dir: &Path, label: &str, keep: &str) -> Result<()> {
    let hash = hash_token(keep);
    rewrite_keys(config_dir, |k| k.id != label || k.hash == hash)
}

fn rewrite_keys(config_dir: &Path, keep: impl Fn(&KeyEntry) -> bool) -> Result<()> {
    let keys_path = config_dir.join("keys.yaml");
    let mut config = load_keys(&keys_path)?;
    let before = config.keys.len();
    config.keys.retain(|k| keep(k));
    if config.keys.len() != before {
        save_keys(&keys_path, &config)?;
    }
    Ok(())
}

/// Remove every key under label; returns how many went. No-op when `keys.yaml` is absent.
pub fn revoke_key(config_dir: &Path, label: &str) -> Result<usize> {
    let keys_path = config_dir.join("keys.yaml");
    if !keys_path.exists() {
        return Ok(0);
    }
    let mut config = load_keys(&keys_path)?;
    let before = config.keys.len();
    config.keys.retain(|k| k.id != label);
    let removed = before - config.keys.len();
    if removed > 0 {
        save_keys(&keys_path, &config)?;
    }
    Ok(removed)
}

fn probe_ready(base_url: &str, api_key: &str) -> Result<bool> {
    let url = format!("{}/ready", base_url.trim_end_matches('/'));
    match ureq::get(&url)
        .set("Authorization", &format!("Bearer {}", api_key))
        .call()
    {
        Ok(response) => Ok(response.status() == 200),
        Err(ureq::Error::Status(_, _)) => Ok(false),
        Err(e) => bail!("GET /ready failed: {}", e),
    }
}

fn fetch_models(base_url: &str, api_key: &str, format: &str) -> Result<Vec<Value>> {
    let url = format!(
        "{}/v1/models?format={}",
        base_url.trim_end_matches('/'),
        format
    );
    let response = ureq::get(&url)
        .set("Authorization", &format!("Bearer {}", api_key))
        .call()
        .map_err(|e| anyhow::anyhow!("GET /v1/models?format={} failed: {}", format, e))?;
    let body = response.into_string()?;
    let models: Vec<Value> = serde_json::from_str(&body)?;
    Ok(models)
}

/// `providers.subs` (SPEC §19.10): plain OpenAI-compatible provider on `/v1`.
pub fn build_subs_block(base_url: &str, api_key: &str, models: &[Value]) -> Value {
    json!({
        "api": "openai-completions",
        "baseUrl": format!("{}/v1", base_url.trim_end_matches('/')),
        "apiKey": api_key,
        "compat": {
            "supportsDeveloperRole": true,
            "supportsReasoningEffort": true
        },
        "models": models,
    })
}

/// A missing file is an empty document; an unreadable or unparseable one is an error —
/// rewriting it would silently drop every other provider and its key.
fn read_doc(path: &Path) -> Result<Value> {
    if !path.exists() {
        return Ok(json!({}));
    }
    let text = fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let doc: Value = serde_json::from_str(text.trim_start_matches('\u{feff}')).with_context(|| {
        format!(
            "{} is not valid JSON; refusing to rewrite it (fix or move it, then rerun `thickapi sync-pi`)",
            path.display()
        )
    })?;
    if !doc.is_object() {
        bail!(
            "{} is not a JSON object; refusing to rewrite it",
            path.display()
        );
    }
    Ok(doc)
}

/// Replace `providers.subs`, drop a retired `providers.thick`, leave every other provider alone.
pub fn merge_blocks(path: &Path, subs_block: &Value) -> Result<SyncOutcome> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut doc = read_doc(path)?;
    if path.exists() {
        let backup = path.with_extension("json.bak");
        let previous = fs::read(path).with_context(|| format!("read {}", path.display()))?;
        write_private_atomic(&backup, &previous)
            .with_context(|| format!("write {}", backup.display()))?;
    }
    if !doc.get("providers").is_some_and(Value::is_object) {
        if let Some(root) = doc.as_object_mut() {
            root.insert("providers".into(), json!({}));
        }
    }
    let Some(providers) = doc
        .as_object_mut()
        .and_then(|root| root.get_mut("providers").and_then(Value::as_object_mut))
    else {
        anyhow::bail!("models.json providers is not an object");
    };
    // A pre-T1 `subs` still on the thick wire counts as the retired block too.
    let old_subs_was_thick = providers
        .get("subs")
        .and_then(|s| s.get("api"))
        .and_then(Value::as_str)
        == Some("thickapi");
    providers.insert("subs".into(), subs_block.clone());
    let thick_removed = providers.remove("thick").is_some() || old_subs_was_thick;
    write_private_atomic(path, serde_json::to_string_pretty(&doc)?.as_bytes())
        .with_context(|| format!("write {}", path.display()))?;
    Ok(SyncOutcome {
        models_path: path.to_path_buf(),
        thin_models: subs_block
            .get("models")
            .and_then(Value::as_array)
            .map_or(0, Vec::len),
        thick_removed,
    })
}

/// `~/.pi/agent/thickapi-subs.json`: where the Pi extension finds the daemon binary to
/// auto-start on ECONNREFUSED, and which config dir it serves.
fn write_sidecar(path: &Path, config_dir: &Path, base_url: &str) -> Result<()> {
    let exe = std::env::current_exe().context("resolve thickapi executable")?;
    let doc = sidecar_json(&exe, config_dir, base_url);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    write_private_atomic(path, serde_json::to_string_pretty(&doc)?.as_bytes())
        .with_context(|| format!("write sidecar {}", path.display()))
}

pub fn sidecar_json(exe: &Path, config_dir: &Path, base_url: &str) -> Value {
    let mut doc = json!({
        "exe": exe,
        "configDir": config_dir,
        "baseUrl": base_url,
        "writtenBy": "thickapi sync-pi",
    });
    if path_has_target_dir(exe) {
        if let Some(map) = doc.as_object_mut() {
            map.insert("allow_debug".into(), json!(true));
        }
    }
    doc
}

pub fn path_has_target_dir(path: &Path) -> bool {
    path.components().any(|c| c.as_os_str() == "target")
}

fn service_installed() -> bool {
    #[cfg(windows)]
    {
        std::process::Command::new("schtasks")
            .args(["/Query", "/TN", SERVICE_TASK_NAME])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }
    #[cfg(not(windows))]
    {
        true
    }
}

pub fn pi_models_path() -> Result<PathBuf> {
    Ok(pi_agent_dir()?.join("models.json"))
}

pub fn pi_sidecar_path() -> Result<PathBuf> {
    Ok(pi_agent_dir()?.join("thickapi-subs.json"))
}

fn pi_agent_dir() -> Result<PathBuf> {
    if let Ok(p) = std::env::var("THICKAPI_PI_AGENT") {
        if !p.is_empty() {
            return Ok(PathBuf::from(p));
        }
    }
    let home = dirs::home_dir().context("home dir")?;
    Ok(home.join(".pi").join("agent"))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use thickapi_core::config::keys::{load_keys, DEFAULT_SCOPES};

    #[test]
    fn mint_key_uses_requested_scopes() {
        let temp = tempfile::tempdir().expect("tempdir");
        let key = mint_key(temp.path(), THIN_KEY_LABEL, PI_SCOPES).expect("mint key");
        assert!(key.starts_with("ta_"));
        let keys = load_keys(&temp.path().join("keys.yaml")).expect("load keys");
        let entry = keys
            .keys
            .iter()
            .find(|k| k.id == THIN_KEY_LABEL)
            .expect("entry");
        assert_eq!(
            entry.scopes,
            vec!["chat:write".to_string(), "models:read".to_string()]
        );
        assert!(!entry.scopes.iter().any(|s| s == "turn:write"));
        assert!(!entry.created.is_empty(), "mint_key must stamp `created`");
    }

    /// D3 deliverable 5: a wildcard bind rewrites to loopback (Pi on Windows cannot dial
    /// `0.0.0.0`); a loopback or named-host `listen` passes through unchanged.
    #[test]
    fn listen_base_url_rewrites_wildcard_binds_to_loopback() {
        assert_eq!(listen_base_url("0.0.0.0:18787"), "http://127.0.0.1:18787");
        assert_eq!(listen_base_url("[::]:18787"), "http://127.0.0.1:18787");
        assert_eq!(listen_base_url("127.0.0.1:8787"), "http://127.0.0.1:8787");
        assert_eq!(listen_base_url("localhost:8787"), "http://localhost:8787");
        assert_eq!(listen_base_url("[::1]:8787"), "http://[::1]:8787");
    }

    #[test]
    fn retiring_keeps_one_key_per_label_and_mint_key_appends() {
        let temp = tempfile::tempdir().expect("tempdir");
        mint_key(temp.path(), "pi", DEFAULT_SCOPES).unwrap();
        mint_key(temp.path(), "pi", DEFAULT_SCOPES).unwrap();
        let keys = load_keys(&temp.path().join("keys.yaml")).unwrap();
        assert_eq!(keys.keys.iter().filter(|k| k.id == "pi").count(), 2);

        mint_key(temp.path(), THIN_KEY_LABEL, PI_SCOPES).unwrap();
        let fresh = mint_key(temp.path(), THIN_KEY_LABEL, PI_SCOPES).unwrap();
        retire_other_keys(temp.path(), THIN_KEY_LABEL, &fresh).unwrap();
        let keys = load_keys(&temp.path().join("keys.yaml")).unwrap();
        let thin: Vec<_> = keys
            .keys
            .iter()
            .filter(|k| k.id == THIN_KEY_LABEL)
            .collect();
        assert_eq!(thin.len(), 1, "one live key per sync-pi label");
        assert_eq!(thin[0].hash, hash_token(&fresh));
        assert_eq!(
            keys.keys.iter().filter(|k| k.id == "pi").count(),
            2,
            "other labels untouched"
        );
    }

    /// No daemon: the key Pi already holds must still be the key `keys.yaml` knows.
    #[test]
    fn failed_sync_leaves_keys_and_models_untouched() {
        let temp = tempfile::tempdir().expect("tempdir");
        let config_dir = temp.path().join("config");
        fs::create_dir_all(&config_dir).unwrap();
        let examples = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../configs");
        for name in ["accounts", "models", "combos"] {
            fs::copy(
                examples.join(format!("{name}.yaml.example")),
                config_dir.join(format!("{name}.yaml")),
            )
            .unwrap();
        }
        // Bind then drop: a port nothing is listening on.
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let router = fs::read_to_string(examples.join("router.yaml.example"))
            .unwrap()
            .replace("127.0.0.1:8787", &format!("127.0.0.1:{port}"));
        fs::write(config_dir.join("router.yaml"), router).unwrap();

        let held = mint_key(&config_dir, THIN_KEY_LABEL, PI_SCOPES).unwrap();
        let keys_before = fs::read(config_dir.join("keys.yaml")).unwrap();
        let models = temp.path().join("models.json");
        fs::write(&models, r#"{"providers":{"subs":{"apiKey":"held"}}}"#).unwrap();

        let err = run_in(
            &config_dir,
            None,
            &models,
            &temp.path().join("sidecar.json"),
        );
        assert!(err.is_err());
        assert_eq!(fs::read(config_dir.join("keys.yaml")).unwrap(), keys_before);
        let keys = load_keys(&config_dir.join("keys.yaml")).unwrap();
        assert!(keys.keys.iter().any(|k| k.hash == hash_token(&held)));
        assert!(fs::read_to_string(&models).unwrap().contains("held"));
    }

    /// The key a running Pi already loaded must survive a sync that cannot reach the daemon.
    /// Minting here would revoke it the moment the daemon re-read `keys.yaml`.
    #[test]
    fn a_live_pi_key_is_not_minted_when_the_daemon_is_down() {
        let temp = tempfile::tempdir().expect("tempdir");
        let config_dir = temp.path().join("config");
        fs::create_dir_all(&config_dir).unwrap();
        let examples = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../configs");
        for name in ["accounts", "models", "combos"] {
            fs::copy(
                examples.join(format!("{name}.yaml.example")),
                config_dir.join(format!("{name}.yaml")),
            )
            .unwrap();
        }
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let router = fs::read_to_string(examples.join("router.yaml.example"))
            .unwrap()
            .replace("127.0.0.1:8787", &format!("127.0.0.1:{port}"));
        fs::write(config_dir.join("router.yaml"), router).unwrap();

        let held = mint_key(&config_dir, THIN_KEY_LABEL, PI_SCOPES).unwrap();
        let models = temp.path().join("models.json");
        fs::write(
            &models,
            format!(r#"{{"providers":{{"subs":{{"apiKey":"{held}"}}}}}}"#),
        )
        .unwrap();
        let before = fs::read(config_dir.join("keys.yaml")).unwrap();
        let mtime = fs::metadata(config_dir.join("keys.yaml"))
            .unwrap()
            .modified()
            .unwrap();

        let err = run_in(
            &config_dir,
            None,
            &models,
            &temp.path().join("sidecar.json"),
        );
        assert!(err.is_err(), "no daemon, sync must refuse");
        assert_eq!(
            fs::metadata(config_dir.join("keys.yaml"))
                .unwrap()
                .modified()
                .unwrap(),
            mtime,
            "reuse must not write keys.yaml"
        );
        assert_eq!(fs::read(config_dir.join("keys.yaml")).unwrap(), before);
        assert!(fs::read_to_string(&models).unwrap().contains(&held));
    }

    #[test]
    fn reusable_pi_key_accepts_only_a_live_pi_thin_token() {
        let temp = tempfile::tempdir().unwrap();
        let models = temp.path().join("models.json");

        assert!(reusable_pi_key(temp.path(), &models).unwrap().is_none());

        let held = mint_key(temp.path(), THIN_KEY_LABEL, PI_SCOPES).unwrap();
        fs::write(
            &models,
            format!(r#"{{"providers":{{"subs":{{"apiKey":"{held}"}}}}}}"#),
        )
        .unwrap();
        assert_eq!(
            reusable_pi_key(temp.path(), &models).unwrap().as_deref(),
            Some(held.as_str())
        );

        fs::write(&models, r#"{"providers":{"subs":{"apiKey":"ta_missing"}}}"#).unwrap();
        assert!(reusable_pi_key(temp.path(), &models).unwrap().is_none());

        let turn = mint_key(temp.path(), THIN_KEY_LABEL, &["turn:write", "chat:write"]).unwrap();
        fs::write(
            &models,
            format!(r#"{{"providers":{{"subs":{{"apiKey":"{turn}"}}}}}}"#),
        )
        .unwrap();
        assert!(
            reusable_pi_key(temp.path(), &models).unwrap().is_none(),
            "a turn:write credential must not stay in models.json"
        );

        let other = mint_key(temp.path(), "scripts", PI_SCOPES).unwrap();
        fs::write(
            &models,
            format!(r#"{{"providers":{{"subs":{{"apiKey":"{other}"}}}}}}"#),
        )
        .unwrap();
        assert!(reusable_pi_key(temp.path(), &models).unwrap().is_none());

        fs::write(&models, r#"{"providers":{"subs":{"apiKey":""}}}"#).unwrap();
        assert!(reusable_pi_key(temp.path(), &models).unwrap().is_none());
    }

    /// The sync reported an error but models.json already carries the new key: that key stays.
    #[test]
    fn a_key_that_reached_models_json_is_never_retired() {
        let temp = tempfile::tempdir().unwrap();
        let models = temp.path().join("models.json");
        let old = mint_key(temp.path(), THIN_KEY_LABEL, PI_SCOPES).unwrap();
        let new = mint_key(temp.path(), THIN_KEY_LABEL, PI_SCOPES).unwrap();
        let hashes = |dir: &Path| -> Vec<String> {
            let keys = load_keys(&dir.join("keys.yaml")).unwrap();
            keys.keys.into_iter().map(|k| k.hash).collect()
        };

        fs::write(
            &models,
            format!(r#"{{"providers":{{"subs":{{"apiKey":"{new}"}}}}}}"#),
        )
        .unwrap();
        settle_minted_key(temp.path(), &models, &new).unwrap();
        assert_eq!(hashes(temp.path()), vec![hash_token(&new)]);

        // And the mirror image: models.json never got it, so the minted key goes, the old stays.
        let temp = tempfile::tempdir().unwrap();
        let models = temp.path().join("models.json");
        let old2 = mint_key(temp.path(), THIN_KEY_LABEL, PI_SCOPES).unwrap();
        let new2 = mint_key(temp.path(), THIN_KEY_LABEL, PI_SCOPES).unwrap();
        fs::write(
            &models,
            format!(r#"{{"providers":{{"subs":{{"apiKey":"{old2}"}}}}}}"#),
        )
        .unwrap();
        settle_minted_key(temp.path(), &models, &new2).unwrap();
        assert_eq!(hashes(temp.path()), vec![hash_token(&old2)]);
        let _ = old;
    }

    #[test]
    fn merge_refuses_an_unparseable_models_json() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("models.json");
        let broken = r#"{"providers":{"openrouter":{"apiKey":"sk-or"},}}"#;
        fs::write(&path, broken).unwrap();
        let subs = build_subs_block("http://127.0.0.1:18787", "ta_thin", &[]);
        let err = merge_blocks(&path, &subs).expect_err("must refuse");
        assert!(
            format!("{err:#}").contains("refusing to rewrite"),
            "{err:#}"
        );
        assert_eq!(fs::read_to_string(&path).unwrap(), broken);

        fs::write(&path, "[]").unwrap();
        assert!(merge_blocks(&path, &subs).is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), "[]");
    }

    #[test]
    fn merge_backs_up_the_previous_models_json_and_accepts_a_bom() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("models.json");
        let previous = "\u{feff}{\"providers\":{\"openrouter\":{\"apiKey\":\"sk-or\"}}}";
        fs::write(&path, previous).unwrap();
        let subs = build_subs_block("http://127.0.0.1:18787", "ta_thin", &[]);
        merge_blocks(&path, &subs).unwrap();
        assert_eq!(
            fs::read_to_string(temp.path().join("models.json.bak")).unwrap(),
            previous
        );
        let doc: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(doc["providers"]["openrouter"]["apiKey"], "sk-or");
    }

    #[test]
    fn subs_block_is_openai_completions_on_v1() {
        let block = build_subs_block(
            "http://127.0.0.1:18787",
            "ta_x",
            &[json!({"id":"claude/sonnet"})],
        );
        assert_eq!(block["api"], "openai-completions");
        assert_eq!(block["baseUrl"], "http://127.0.0.1:18787/v1");
        assert_eq!(block["models"][0]["id"], "claude/sonnet");
    }

    #[test]
    fn merge_replaces_subs_removes_retired_thick_and_keeps_others() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("models.json");
        fs::write(
            &path,
            r#"{"providers":{"openrouter":{"api":"openai-completions","apiKey":"sk-or"},"subs":{"api":"openai-completions","apiKey":"ta_prev","baseUrl":"http://127.0.0.1:18787/v1"},"thick":{"api":"thickapi","apiKey":"ta_old","baseUrl":"http://127.0.0.1:18787","models":[{"id":"subs/grok/grok-4.7"}]}}}"#,
        )
        .unwrap();
        let subs = build_subs_block("http://127.0.0.1:18787", "ta_thin", &[]);
        let out = merge_blocks(&path, &subs).unwrap();
        assert!(out.thick_removed);
        let doc: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(doc["providers"]["openrouter"]["apiKey"], "sk-or");
        assert_eq!(doc["providers"]["subs"]["api"], "openai-completions");
        assert_eq!(doc["providers"]["subs"]["apiKey"], "ta_thin");
        assert!(doc["providers"].get("thick").is_none(), "thick block gone");
        assert!(!fs::read_to_string(&path).unwrap().contains("ta_old"));

        // Second run: nothing retired left to report.
        let out = merge_blocks(&path, &subs).unwrap();
        assert!(!out.thick_removed);
    }

    #[test]
    fn merge_treats_pre_t1_thick_subs_as_retired() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("models.json");
        fs::write(
            &path,
            r#"{"providers":{"subs":{"api":"thickapi","apiKey":"ta_old","baseUrl":"http://127.0.0.1:18787"}}}"#,
        )
        .unwrap();
        let subs = build_subs_block("http://127.0.0.1:18787", "ta_thin", &[]);
        let out = merge_blocks(&path, &subs).unwrap();
        assert!(out.thick_removed);
        let doc: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(doc["providers"]["subs"]["api"], "openai-completions");
    }

    #[test]
    fn revoke_key_drops_only_the_label() {
        let temp = tempfile::tempdir().unwrap();
        assert_eq!(revoke_key(temp.path(), THICK_KEY_LABEL).unwrap(), 0);
        mint_key(temp.path(), THICK_KEY_LABEL, DEFAULT_SCOPES).unwrap();
        mint_key(temp.path(), THICK_KEY_LABEL, DEFAULT_SCOPES).unwrap();
        mint_key(temp.path(), THIN_KEY_LABEL, PI_SCOPES).unwrap();
        assert_eq!(revoke_key(temp.path(), THICK_KEY_LABEL).unwrap(), 2);
        let keys = load_keys(&temp.path().join("keys.yaml")).unwrap();
        assert_eq!(keys.keys.len(), 1);
        assert_eq!(keys.keys[0].id, THIN_KEY_LABEL);
    }

    #[test]
    fn sidecar_sets_allow_debug_only_for_target_exes() {
        let release = sidecar_json(
            Path::new(r"C:\tools\thickapi.exe"),
            Path::new(r"C:\Users\me\.thickapi"),
            "http://127.0.0.1:18787",
        );
        assert!(release.get("allow_debug").is_none());
        let debug = sidecar_json(
            Path::new(r"C:\repo\target\debug\thickapi.exe"),
            Path::new(r"C:\Users\me\.thickapi"),
            "http://127.0.0.1:18787",
        );
        assert_eq!(debug["allow_debug"], true);
        assert!(path_has_target_dir(Path::new(
            r"C:\work\thickapi\target\debug\thickapi.exe"
        )));
    }
}
