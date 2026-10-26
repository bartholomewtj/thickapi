//! Child MCP for W3: `TurnSpec.mcp_config` is a Claude-shaped JSON file.
//!
//! Claude 2.1.x takes `--mcp-config <file>`. Grok 1.0.30 and agy 1.2.4 do not.
//! Measured 16 Sep 2026:
//! - Grok merges `{cwd}/.grok/config.toml` `[mcp_servers.*]` over user
//!   `~/.grok/config.toml` (same name overrides command/env).
//! - Agy only loads `~/.gemini/config/mcp_config.json` (no project file, no
//!   `GEMINI_*` config-dir env). Overlay that file for the turn and restore.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};

use crate::types::{BackendId, McpMode, TurnSpec};

#[derive(Debug, Clone)]
pub struct SpawnCleanup {
    pub path: PathBuf,
    pub previous: Option<Vec<u8>>,
    pub created_dir: Option<PathBuf>,
    pub lock_path: Option<PathBuf>,
}

pub struct RestoreOnDrop {
    pub items: Vec<SpawnCleanup>,
}

impl Drop for RestoreOnDrop {
    fn drop(&mut self) {
        restore_all(&self.items);
    }
}

pub fn restore_all(items: &[SpawnCleanup]) {
    for item in items.iter().rev() {
        match &item.previous {
            Some(bytes) => {
                let _ = fs::write(&item.path, bytes);
            }
            None => {
                let _ = fs::remove_file(&item.path);
            }
        }
        if let Some(dir) = &item.created_dir {
            let _ = fs::remove_dir(dir);
        }
        if let Some(lock) = &item.lock_path {
            let _ = fs::remove_file(lock);
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct McpServer {
    pub command: Option<String>,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub url: Option<String>,
    pub headers: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Default)]
pub struct McpConfigFile {
    pub servers: Vec<(String, McpServer)>,
}

pub fn mcp_config_path(spec: &TurnSpec) -> Option<&Path> {
    if spec.mcp == McpMode::Off {
        return None;
    }
    spec.mcp_config.as_deref()
}

pub fn load_mcp_config(path: &Path) -> Result<McpConfigFile> {
    let text =
        fs::read_to_string(path).with_context(|| format!("read mcp_config {}", path.display()))?;
    parse_mcp_config(&text).with_context(|| format!("parse mcp_config {}", path.display()))
}

pub fn parse_mcp_config(text: &str) -> Result<McpConfigFile> {
    let value: serde_json::Value = serde_json::from_str(text).context("mcp_config JSON")?;
    let servers = value
        .get("mcpServers")
        .and_then(|v| v.as_object())
        .ok_or_else(|| anyhow!("mcp_config missing mcpServers object"))?;
    let mut out = McpConfigFile::default();
    for (name, spec) in servers {
        let obj = spec
            .as_object()
            .ok_or_else(|| anyhow!("mcpServers.{name} must be an object"))?;
        let mut server = McpServer::default();
        if let Some(command) = obj.get("command").and_then(|v| v.as_str()) {
            server.command = Some(command.to_string());
        }
        if let Some(args) = obj.get("args").and_then(|v| v.as_array()) {
            server.args = args
                .iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect();
        }
        if let Some(env) = obj.get("env").and_then(|v| v.as_object()) {
            for (k, v) in env {
                if let Some(val) = v.as_str() {
                    server.env.insert(k.clone(), val.to_string());
                }
            }
        }
        let url = obj
            .get("url")
            .or_else(|| obj.get("serverUrl"))
            .and_then(|v| v.as_str());
        if let Some(url) = url {
            server.url = Some(url.to_string());
        }
        if let Some(headers) = obj.get("headers").and_then(|v| v.as_object()) {
            for (k, v) in headers {
                if let Some(val) = v.as_str() {
                    server.headers.insert(k.clone(), val.to_string());
                }
            }
        }
        out.servers.push((name.clone(), server));
    }
    Ok(out)
}

pub fn inject_server_env(
    env: &mut std::collections::HashMap<String, String>,
    spec: &TurnSpec,
) -> Result<()> {
    let Some(path) = mcp_config_path(spec) else {
        return Ok(());
    };
    let cfg = load_mcp_config(path)?;
    for (_, server) in cfg.servers {
        for (k, v) in server.env {
            env.insert(k, v);
        }
    }
    Ok(())
}

/// Where this backend places child MCP for the turn (SPEC §6).
pub struct McpPlacement;

impl McpPlacement {
    /// File overlay. Claude argv MCP is [`Self::apply_argv`].
    pub fn apply(backend: BackendId, cwd: &Path, spec: &TurnSpec) -> Result<Option<SpawnCleanup>> {
        match backend {
            BackendId::ClaudeCode | BackendId::Codex | BackendId::Pi => Ok(None),
            BackendId::Grok => apply_grok_overlay(cwd, spec),
            BackendId::Agy => {
                let home = dirs::home_dir().ok_or_else(|| anyhow!("home directory not found"))?;
                apply_agy_overlay(&home, spec)
            }
            BackendId::Cursor => apply_cursor_overlay(cwd, spec),
        }
    }

    /// Claude: `--mcp-config <file>`. Other backends: no argv.
    pub fn apply_argv(backend: BackendId, spec: &TurnSpec, args: &mut Vec<String>) {
        match backend {
            BackendId::ClaudeCode => {
                if spec.mcp != McpMode::Off {
                    if let Some(cfg) = &spec.mcp_config {
                        args.push("--mcp-config".to_string());
                        args.push(cfg.to_string_lossy().to_string());
                    }
                }
            }
            BackendId::Grok
            | BackendId::Agy
            | BackendId::Cursor
            | BackendId::Codex
            | BackendId::Pi => {}
        }
    }
}

/// Where this backend places write-deny for the turn (SPEC §6).
pub struct DenyPlacement;

impl DenyPlacement {
    /// File overlay. Claude/Grok argv deny is [`Self::apply_argv`].
    pub fn apply(backend: BackendId, cwd: &Path, spec: &TurnSpec) -> Result<Option<SpawnCleanup>> {
        match backend {
            BackendId::Cursor => apply_cli_json(cwd, spec),
            BackendId::ClaudeCode
            | BackendId::Grok
            | BackendId::Agy
            | BackendId::Codex
            | BackendId::Pi => Ok(None),
        }
    }

    /// Claude: `--disallowedTools`. Grok: `--deny Edit(…)`. Other backends: no argv.
    pub fn apply_argv(backend: BackendId, spec: &TurnSpec, args: &mut Vec<String>) {
        match backend {
            BackendId::ClaudeCode => {
                for pattern in &spec.deny_writes {
                    let rule = super::path_deny_rule(pattern);
                    args.push("--deny".to_string());
                    args.push(format!("Edit({})", rule));
                }
                if !spec.tools.is_empty() {
                    args.push("--disallowedTools".to_string());
                    args.push(spec.tools.join(","));
                } else if !spec.deny_writes.is_empty() {
                    args.push("--disallowedTools".to_string());
                    args.push(String::new());
                }
            }
            BackendId::Grok => {
                for pattern in &spec.deny_writes {
                    let rule = super::path_deny_rule(pattern);
                    args.push("--deny".to_string());
                    args.push(format!("Edit({})", rule));
                }
            }
            BackendId::Agy | BackendId::Cursor | BackendId::Codex | BackendId::Pi => {}
        }
    }
}

pub fn grok_servers_toml(cfg: &McpConfigFile) -> Result<String> {
    let mut mcp_servers = toml::map::Map::new();
    for (name, server) in &cfg.servers {
        mcp_servers.insert(name.clone(), grok_server_value(server));
    }
    let mut root = toml::map::Map::new();
    root.insert("mcp_servers".into(), toml::Value::Table(mcp_servers));
    toml::to_string(&toml::Value::Table(root)).context("serialize grok mcp toml")
}

fn grok_server_value(server: &McpServer) -> toml::Value {
    let mut map = toml::map::Map::new();
    if let Some(url) = &server.url {
        map.insert("url".into(), toml::Value::String(url.clone()));
        if !server.headers.is_empty() {
            let mut headers = toml::map::Map::new();
            for (k, v) in &server.headers {
                headers.insert(k.clone(), toml::Value::String(v.clone()));
            }
            map.insert("headers".into(), toml::Value::Table(headers));
        }
    } else if let Some(command) = &server.command {
        map.insert("command".into(), toml::Value::String(command.clone()));
        if !server.args.is_empty() {
            map.insert(
                "args".into(),
                toml::Value::Array(
                    server
                        .args
                        .iter()
                        .map(|a| toml::Value::String(a.clone()))
                        .collect(),
                ),
            );
        }
    }
    if !server.env.is_empty() {
        let mut env = toml::map::Map::new();
        for (k, v) in &server.env {
            env.insert(k.clone(), toml::Value::String(v.clone()));
        }
        map.insert("env".into(), toml::Value::Table(env));
    }
    toml::Value::Table(map)
}

pub fn apply_grok_overlay(cwd: &Path, spec: &TurnSpec) -> Result<Option<SpawnCleanup>> {
    let Some(path) = mcp_config_path(spec) else {
        return Ok(None);
    };
    let cfg = load_mcp_config(path)?;
    if cfg.servers.is_empty() {
        return Ok(None);
    }
    let grok_dir = cwd.join(".grok");
    let config_path = grok_dir.join("config.toml");
    let created_dir = if grok_dir.exists() {
        None
    } else {
        fs::create_dir_all(&grok_dir).with_context(|| format!("create {}", grok_dir.display()))?;
        Some(grok_dir.clone())
    };
    let previous = if config_path.exists() {
        Some(fs::read(&config_path).with_context(|| format!("read {}", config_path.display()))?)
    } else {
        None
    };
    let merged = merge_grok_toml(previous.as_deref(), &cfg)?;
    if let Err(err) = fs::write(&config_path, merged.as_bytes()) {
        if let Some(bytes) = &previous {
            let _ = fs::write(&config_path, bytes);
        } else {
            let _ = fs::remove_file(&config_path);
            if let Some(dir) = &created_dir {
                let _ = fs::remove_dir(dir);
            }
        }
        return Err(err).with_context(|| format!("write {}", config_path.display()));
    }
    Ok(Some(SpawnCleanup {
        path: config_path,
        previous,
        created_dir,
        lock_path: None,
    }))
}

fn merge_grok_toml(previous: Option<&[u8]>, cfg: &McpConfigFile) -> Result<String> {
    let mut root = match previous {
        Some(bytes) if !bytes.is_empty() => {
            let text = std::str::from_utf8(bytes).context("existing .grok/config.toml utf-8")?;
            text.parse::<toml::Value>()
                .context("parse existing .grok/config.toml")?
        }
        _ => toml::Value::Table(toml::map::Map::new()),
    };
    let table = root
        .as_table_mut()
        .ok_or_else(|| anyhow!(".grok/config.toml root must be a table"))?;
    let servers = table
        .entry("mcp_servers".to_string())
        .or_insert_with(|| toml::Value::Table(toml::map::Map::new()));
    let servers = servers
        .as_table_mut()
        .ok_or_else(|| anyhow!("mcp_servers must be a table"))?;
    for (name, server) in &cfg.servers {
        servers.insert(name.clone(), grok_server_value(server));
    }
    toml::to_string(&root).context("serialize merged grok toml")
}

pub fn apply_agy_overlay(home: &Path, spec: &TurnSpec) -> Result<Option<SpawnCleanup>> {
    let Some(path) = mcp_config_path(spec) else {
        return Ok(None);
    };
    let cfg = load_mcp_config(path)?;
    if cfg.servers.is_empty() {
        return Ok(None);
    }
    let dir = home.join(".gemini").join("config");
    let config_path = dir.join("mcp_config.json");
    let lock_path = dir.join("mcp_config.json.thickapi-lock");
    let created_dir = if dir.exists() {
        None
    } else {
        fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
        Some(dir.clone())
    };
    acquire_lock(&lock_path)?;
    let previous = if config_path.exists() {
        Some(fs::read(&config_path).with_context(|| format!("read {}", config_path.display()))?)
    } else {
        None
    };
    let merged = merge_agy_json(previous.as_deref(), &cfg)?;
    if let Err(err) = fs::write(&config_path, merged.as_bytes()) {
        if let Some(bytes) = &previous {
            let _ = fs::write(&config_path, bytes);
        } else {
            let _ = fs::remove_file(&config_path);
        }
        let _ = fs::remove_file(&lock_path);
        if let Some(dir) = &created_dir {
            let _ = fs::remove_dir(dir);
        }
        return Err(err).with_context(|| format!("write {}", config_path.display()));
    }
    Ok(Some(SpawnCleanup {
        path: config_path,
        previous,
        created_dir,
        lock_path: Some(lock_path),
    }))
}

fn acquire_lock(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).ok();
    }
    for _ in 0..100 {
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
        {
            Ok(_) => return Ok(()),
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
                thread::sleep(Duration::from_millis(50));
            }
            Err(err) => return Err(err).with_context(|| format!("create lock {}", path.display())),
        }
    }
    // Stale lock from a killed turn: take it.
    let _ = fs::remove_file(path);
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .with_context(|| format!("steal lock {}", path.display()))?;
    Ok(())
}

pub fn apply_cursor_overlay(cwd: &Path, spec: &TurnSpec) -> Result<Option<SpawnCleanup>> {
    let Some(path) = mcp_config_path(spec) else {
        return Ok(None);
    };
    let cfg = load_mcp_config(path)?;
    if cfg.servers.is_empty() {
        return Ok(None);
    }
    let cursor_dir = cwd.join(".cursor");
    let config_path = cursor_dir.join("mcp.json");
    let created_dir = if cursor_dir.exists() {
        None
    } else {
        fs::create_dir_all(&cursor_dir)
            .with_context(|| format!("create {}", cursor_dir.display()))?;
        Some(cursor_dir.clone())
    };
    let previous = if config_path.exists() {
        Some(fs::read(&config_path).with_context(|| format!("read {}", config_path.display()))?)
    } else {
        None
    };
    let merged = merge_cursor_json(previous.as_deref(), &cfg)?;
    if let Err(err) = fs::write(&config_path, merged.as_bytes()) {
        if let Some(bytes) = &previous {
            let _ = fs::write(&config_path, bytes);
        } else {
            let _ = fs::remove_file(&config_path);
            if let Some(dir) = &created_dir {
                let _ = fs::remove_dir(dir);
            }
        }
        return Err(err).with_context(|| format!("write {}", config_path.display()));
    }
    Ok(Some(SpawnCleanup {
        path: config_path,
        previous,
        created_dir,
        lock_path: None,
    }))
}

fn merge_cursor_json(previous: Option<&[u8]>, cfg: &McpConfigFile) -> Result<String> {
    let mut root = match previous {
        Some(bytes) if !bytes.is_empty() => {
            serde_json::from_slice::<serde_json::Value>(bytes).context("parse .cursor/mcp.json")?
        }
        _ => serde_json::json!({ "mcpServers": {} }),
    };
    let servers = root
        .as_object_mut()
        .ok_or_else(|| anyhow!(".cursor/mcp.json root must be an object"))?
        .entry("mcpServers")
        .or_insert_with(|| serde_json::json!({}));
    let servers = servers
        .as_object_mut()
        .ok_or_else(|| anyhow!("mcpServers must be an object"))?;
    for (name, server) in &cfg.servers {
        servers.insert(name.clone(), cursor_server_value(server));
    }
    serde_json::to_string_pretty(&root).context("serialize .cursor/mcp.json")
}

fn cursor_server_value(server: &McpServer) -> serde_json::Value {
    let mut map = serde_json::Map::new();
    if let Some(url) = &server.url {
        map.insert("url".to_string(), serde_json::Value::String(url.clone()));
        if !server.headers.is_empty() {
            let mut headers = serde_json::Map::new();
            for (k, v) in &server.headers {
                headers.insert(k.clone(), serde_json::Value::String(v.clone()));
            }
            map.insert("headers".to_string(), serde_json::Value::Object(headers));
        }
    } else if let Some(command) = &server.command {
        map.insert(
            "command".to_string(),
            serde_json::Value::String(command.clone()),
        );
        if !server.args.is_empty() {
            map.insert(
                "args".to_string(),
                serde_json::Value::Array(
                    server
                        .args
                        .iter()
                        .map(|a| serde_json::Value::String(a.clone()))
                        .collect(),
                ),
            );
        }
    }
    if !server.env.is_empty() {
        let mut env = serde_json::Map::new();
        for (k, v) in &server.env {
            env.insert(k.clone(), serde_json::Value::String(v.clone()));
        }
        map.insert("env".to_string(), serde_json::Value::Object(env));
    }
    serde_json::Value::Object(map)
}

fn merge_agy_json(previous: Option<&[u8]>, cfg: &McpConfigFile) -> Result<String> {
    let mut root = match previous {
        Some(bytes) if !bytes.is_empty() => serde_json::from_slice::<serde_json::Value>(bytes)
            .context("parse agy mcp_config.json")?,
        _ => serde_json::json!({ "mcpServers": {} }),
    };
    let servers = root
        .as_object_mut()
        .ok_or_else(|| anyhow!("agy mcp_config.json root must be an object"))?
        .entry("mcpServers")
        .or_insert_with(|| serde_json::json!({}));
    let servers = servers
        .as_object_mut()
        .ok_or_else(|| anyhow!("mcpServers must be an object"))?;
    for (name, server) in &cfg.servers {
        servers.insert(name.clone(), agy_server_value(server));
    }
    serde_json::to_string_pretty(&root).context("serialize agy mcp_config.json")
}

fn agy_server_value(server: &McpServer) -> serde_json::Value {
    let mut map = serde_json::Map::new();
    if let Some(url) = &server.url {
        map.insert(
            "serverUrl".to_string(),
            serde_json::Value::String(url.clone()),
        );
        if !server.headers.is_empty() {
            let mut headers = serde_json::Map::new();
            for (k, v) in &server.headers {
                headers.insert(k.clone(), serde_json::Value::String(v.clone()));
            }
            map.insert("headers".to_string(), serde_json::Value::Object(headers));
        }
    } else if let Some(command) = &server.command {
        map.insert(
            "command".to_string(),
            serde_json::Value::String(command.clone()),
        );
        if !server.args.is_empty() {
            map.insert(
                "args".to_string(),
                serde_json::Value::Array(
                    server
                        .args
                        .iter()
                        .map(|a| serde_json::Value::String(a.clone()))
                        .collect(),
                ),
            );
        }
        map.insert("disabled".to_string(), serde_json::Value::Bool(false));
    }
    if !server.env.is_empty() {
        let mut env = serde_json::Map::new();
        for (k, v) in &server.env {
            env.insert(k.clone(), serde_json::Value::String(v.clone()));
        }
        map.insert("env".to_string(), serde_json::Value::Object(env));
    }
    serde_json::Value::Object(map)
}

fn apply_cli_json(cwd: &std::path::Path, spec: &TurnSpec) -> Result<Option<SpawnCleanup>> {
    if spec.deny_writes.is_empty() {
        return Ok(None);
    }
    let cursor_dir = cwd.join(".cursor");
    let path = cursor_dir.join("cli.json");
    let created_dir = if cursor_dir.exists() {
        None
    } else {
        std::fs::create_dir_all(&cursor_dir)
            .with_context(|| format!("create {}", cursor_dir.display()))?;
        Some(cursor_dir.clone())
    };
    let previous = if path.exists() {
        Some(std::fs::read(&path).with_context(|| format!("read {}", path.display()))?)
    } else {
        None
    };
    let deny: Vec<String> = spec
        .deny_writes
        .iter()
        .map(|p| format!("Write({})", super::path_deny_rule(p)))
        .collect();
    let mut root = match &previous {
        Some(bytes) if !bytes.is_empty() => serde_json::from_slice::<serde_json::Value>(bytes)
            .unwrap_or_else(|_| serde_json::json!({})),
        _ => serde_json::json!({}),
    };
    let obj = root
        .as_object_mut()
        .ok_or_else(|| anyhow!(".cursor/cli.json root must be an object"))?;
    let permissions = obj
        .entry("permissions")
        .or_insert_with(|| serde_json::json!({ "allow": [], "deny": [] }));
    let permissions = permissions
        .as_object_mut()
        .ok_or_else(|| anyhow!("permissions must be an object"))?;
    let deny_val = permissions
        .entry("deny")
        .or_insert_with(|| serde_json::json!([]));
    let arr = deny_val
        .as_array_mut()
        .ok_or_else(|| anyhow!("permissions.deny must be an array"))?;
    for rule in deny {
        if !arr.iter().any(|v| v.as_str() == Some(rule.as_str())) {
            arr.push(serde_json::Value::String(rule));
        }
    }
    permissions
        .entry("allow")
        .or_insert_with(|| serde_json::json!([]));
    let body = serde_json::to_string_pretty(&root).context("serialize .cursor/cli.json")?;
    if let Err(err) = std::fs::write(&path, body.as_bytes()) {
        if let Some(bytes) = &previous {
            let _ = std::fs::write(&path, bytes);
        } else {
            let _ = std::fs::remove_file(&path);
            if let Some(dir) = &created_dir {
                let _ = std::fs::remove_dir(dir);
            }
        }
        return Err(err).with_context(|| format!("write {}", path.display()));
    }
    Ok(Some(SpawnCleanup {
        path,
        previous,
        created_dir,
        lock_path: None,
    }))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::types::Effort;
    use std::collections::HashMap;
    use tempfile::tempdir;

    fn spec_with_mcp(path: PathBuf, mode: McpMode) -> TurnSpec {
        TurnSpec {
            model: "subs/grok/grok-4.7".into(),
            prompt: "hi".into(),
            system_prompt: None,
            system_prompt_file: None,
            cwd: PathBuf::from("."),
            session_id: "s1".into(),
            thinking: Effort::Medium,
            tools: vec![],
            deny_writes: vec![],
            enforce_policy: None,
            mcp: mode,
            mcp_config: Some(path),
            setting_sources: None,
            allow_api: false,
            combo: None,
            allow_paid: false,
            timeout_seconds: 60,
            client: None,
            turn_id: None,
            metadata: serde_json::Map::new(),
        }
    }

    fn write_mcp(dir: &Path, body: &str) -> PathBuf {
        let path = dir.join("mcp.json");
        fs::write(&path, body).expect("write mcp.json");
        path
    }

    #[test]
    fn parse_stdio_and_http_servers() {
        let cfg = parse_mcp_config(
            r#"{
              "mcpServers": {
                "whistlr": {
                  "command": "bun",
                  "args": ["mcp"],
                  "env": { "WHISTLR_PEER": "style" }
                },
                "firelight": {
                  "url": "https://example.test/mcp",
                  "headers": { "Authorization": "Bearer x" }
                }
              }
            }"#,
        )
        .expect("parse");
        assert_eq!(cfg.servers.len(), 2);
        let whistlr = cfg
            .servers
            .iter()
            .find(|(name, _)| name == "whistlr")
            .map(|(_, s)| s)
            .expect("whistlr");
        assert_eq!(
            whistlr.env.get("WHISTLR_PEER").map(String::as_str),
            Some("style")
        );
        let firelight = cfg
            .servers
            .iter()
            .find(|(name, _)| name == "firelight")
            .map(|(_, s)| s)
            .expect("firelight");
        assert_eq!(firelight.url.as_deref(), Some("https://example.test/mcp"));
    }

    #[test]
    fn grok_toml_has_mcp_servers_and_env() {
        let cfg = parse_mcp_config(
            r#"{"mcpServers":{"whistlr":{"command":"bun","args":["mcp"],"env":{"WHISTLR_PEER":"style"}}}}"#,
        )
        .unwrap();
        let toml = grok_servers_toml(&cfg).unwrap();
        assert!(toml.contains("[mcp_servers.whistlr]"));
        assert!(toml.contains("command = ") && toml.contains("bun"));
        assert!(toml.contains("WHISTLR_PEER"));
        assert!(toml.contains("style"));
    }

    #[test]
    fn grok_overlay_writes_and_restore_deletes() {
        let tmp = tempdir().unwrap();
        let mcp = write_mcp(
            tmp.path(),
            r#"{"mcpServers":{"whistlr":{"command":"bun","args":["mcp"],"env":{"WHISTLR_PEER":"p1"}}}}"#,
        );
        let spec = spec_with_mcp(mcp, McpMode::On);
        let cwd = tmp.path().join("work");
        fs::create_dir(&cwd).unwrap();
        let item = apply_grok_overlay(&cwd, &spec).unwrap().expect("overlay");
        let written = fs::read_to_string(&item.path).unwrap();
        assert!(written.contains("[mcp_servers.whistlr]"));
        assert!(item.previous.is_none());
        restore_all(&[item]);
        assert!(!cwd.join(".grok").join("config.toml").exists());
    }

    #[test]
    fn grok_overlay_merges_existing_and_restores() {
        let tmp = tempdir().unwrap();
        let mcp = write_mcp(
            tmp.path(),
            r#"{"mcpServers":{"whistlr":{"command":"cmd","env":{"WHISTLR_PEER":"probe"}}}}"#,
        );
        let cwd = tmp.path().join("work");
        let grok = cwd.join(".grok");
        fs::create_dir_all(&grok).unwrap();
        let existing = "[ui]\nscreen_mode = \"minimal\"\n[mcp_servers.firelight]\nurl = \"https://keep.test\"\n";
        fs::write(grok.join("config.toml"), existing).unwrap();
        let spec = spec_with_mcp(mcp, McpMode::Auto);
        let item = apply_grok_overlay(&cwd, &spec).unwrap().expect("overlay");
        let written = fs::read_to_string(grok.join("config.toml")).unwrap();
        assert!(written.contains("screen_mode"));
        assert!(written.contains("keep.test"));
        assert!(written.contains("[mcp_servers.whistlr]"));
        restore_all(&[item]);
        assert_eq!(
            fs::read_to_string(grok.join("config.toml")).unwrap(),
            existing
        );
    }

    #[test]
    fn grok_off_skips_overlay() {
        let tmp = tempdir().unwrap();
        let mcp = write_mcp(
            tmp.path(),
            r#"{"mcpServers":{"whistlr":{"command":"bun"}}}"#,
        );
        let spec = spec_with_mcp(mcp, McpMode::Off);
        assert!(apply_grok_overlay(tmp.path(), &spec).unwrap().is_none());
    }

    #[test]
    fn agy_overlay_maps_url_to_server_url_and_restores() {
        let tmp = tempdir().unwrap();
        let mcp = write_mcp(
            tmp.path(),
            r#"{"mcpServers":{"firelight":{"url":"https://example.test/mcp"}}}"#,
        );
        let spec = spec_with_mcp(mcp, McpMode::On);
        let home = tmp.path().join("home");
        let item = apply_agy_overlay(&home, &spec).unwrap().expect("overlay");
        let written: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&item.path).unwrap()).unwrap();
        assert_eq!(
            written["mcpServers"]["firelight"]["serverUrl"],
            "https://example.test/mcp"
        );
        restore_all(&[item]);
        assert!(!home
            .join(".gemini")
            .join("config")
            .join("mcp_config.json")
            .exists());
    }

    #[test]
    fn cursor_overlay_writes_project_mcp_json_and_restores() {
        let tmp = tempdir().unwrap();
        let mcp = write_mcp(
            tmp.path(),
            r#"{"mcpServers":{"whistlr":{"command":"bun","args":["mcp"],"env":{"WHISTLR_PEER":"p1"}}}}"#,
        );
        let spec = spec_with_mcp(mcp, McpMode::On);
        let cwd = tmp.path().join("work");
        fs::create_dir(&cwd).unwrap();
        let item = apply_cursor_overlay(&cwd, &spec).unwrap().expect("overlay");
        let written: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&item.path).unwrap()).unwrap();
        assert_eq!(written["mcpServers"]["whistlr"]["command"], "bun");
        assert_eq!(
            written["mcpServers"]["whistlr"]["env"]["WHISTLR_PEER"],
            "p1"
        );
        restore_all(&[item]);
        assert!(!cwd.join(".cursor").join("mcp.json").exists());
    }

    #[test]
    fn inject_env_copies_server_env() {
        let tmp = tempdir().unwrap();
        let mcp = write_mcp(
            tmp.path(),
            r#"{"mcpServers":{"whistlr":{"command":"bun","env":{"WHISTLR_PEER":"style"}}}}"#,
        );
        let spec = spec_with_mcp(mcp, McpMode::On);
        let mut env = HashMap::new();
        inject_server_env(&mut env, &spec).unwrap();
        assert_eq!(env.get("WHISTLR_PEER").map(String::as_str), Some("style"));
    }

    #[test]
    fn mcp_placement_dispatches_grok_overlay_and_skips_pi() {
        let tmp = tempdir().unwrap();
        let mcp = write_mcp(
            tmp.path(),
            r#"{"mcpServers":{"whistlr":{"command":"bun","args":["mcp"]}}}"#,
        );
        let spec = spec_with_mcp(mcp, McpMode::On);
        let cwd = tmp.path().join("work");
        fs::create_dir(&cwd).unwrap();
        let item = McpPlacement::apply(BackendId::Grok, &cwd, &spec)
            .unwrap()
            .expect("overlay");
        let written = fs::read_to_string(&item.path).unwrap();
        assert!(written.contains("[mcp_servers.whistlr]"));
        restore_all(&[item]);
        assert!(McpPlacement::apply(BackendId::Pi, &cwd, &spec)
            .unwrap()
            .is_none());
        assert!(McpPlacement::apply(BackendId::ClaudeCode, &cwd, &spec)
            .unwrap()
            .is_none());
    }

    #[test]
    fn deny_placement_cursor_cli_json_and_agy_none() {
        let tmp = tempdir().unwrap();
        let cwd = tmp.path().join("work");
        fs::create_dir(&cwd).unwrap();
        let mut spec = spec_with_mcp(cwd.join("unused.json"), McpMode::Off);
        spec.deny_writes = vec!["src/**".into()];
        spec.mcp_config = None;
        let item = DenyPlacement::apply(BackendId::Cursor, &cwd, &spec)
            .unwrap()
            .expect("cli.json");
        let written: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&item.path).unwrap()).unwrap();
        assert!(written["permissions"]["deny"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v.as_str() == Some("Write(src/**)")));
        restore_all(&[item]);
        assert!(!cwd.join(".cursor").join("cli.json").exists());
        assert!(DenyPlacement::apply(BackendId::Agy, &cwd, &spec)
            .unwrap()
            .is_none());
    }

    #[test]
    fn mcp_placement_argv_claude_only() {
        let spec = spec_with_mcp(PathBuf::from("mcp.json"), McpMode::On);
        let mut claude = Vec::new();
        McpPlacement::apply_argv(BackendId::ClaudeCode, &spec, &mut claude);
        assert_eq!(
            claude,
            vec!["--mcp-config".to_string(), "mcp.json".to_string()]
        );
        let mut grok = Vec::new();
        McpPlacement::apply_argv(BackendId::Grok, &spec, &mut grok);
        assert!(grok.is_empty());
    }

    #[test]
    fn deny_placement_argv_claude_disallowed_and_grok_edit() {
        let mut spec = spec_with_mcp(PathBuf::from("mcp.json"), McpMode::Off);
        spec.tools = vec!["Bash".into()];
        spec.deny_writes = vec!["src/**".into()];
        let mut claude = Vec::new();
        DenyPlacement::apply_argv(BackendId::ClaudeCode, &spec, &mut claude);
        assert_eq!(
            claude,
            vec![
                "--deny".to_string(),
                "Edit(src/**)".to_string(),
                "--disallowedTools".to_string(),
                "Bash".to_string(),
            ]
        );
        let mut grok = Vec::new();
        DenyPlacement::apply_argv(BackendId::Grok, &spec, &mut grok);
        assert_eq!(grok, vec!["--deny".to_string(), "Edit(src/**)".to_string()]);
        let mut agy = Vec::new();
        DenyPlacement::apply_argv(BackendId::Agy, &spec, &mut agy);
        assert!(agy.is_empty());
    }
}
