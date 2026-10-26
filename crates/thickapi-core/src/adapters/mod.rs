pub mod agy;
pub mod claude;
mod cli_resolve;
pub mod codex;
pub mod cursor;
pub mod grok;
pub mod mcp;
pub mod openrouter;

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::config::accounts::AuthKind;
use crate::config::models::ModelEntry;
use crate::types::{BackendId, TurnSpec};

pub use agy::AgyAdapter;
pub use claude::ClaudeAdapter;
pub use cli_resolve::resolve_cli;
pub use cursor::CursorAdapter;
pub use grok::GrokAdapter;
pub use mcp::{DenyPlacement, McpPlacement, RestoreOnDrop, SpawnCleanup};
pub use openrouter::{inject_openrouter_env, OpenRouterAdapter};

/// Honour `THICKAPI_TEST_CHILD` only in debug/test builds (SPEC: never in release).
pub fn test_child_override() -> Option<PathBuf> {
    test_child_override_from(
        cfg!(debug_assertions),
        std::env::var_os("THICKAPI_TEST_CHILD"),
    )
}

fn test_child_override_from(
    debug_assertions: bool,
    value: Option<std::ffi::OsString>,
) -> Option<PathBuf> {
    if debug_assertions {
        value.map(PathBuf::from)
    } else {
        None
    }
}

#[derive(Debug, Clone)]
pub struct Account {
    pub id: String,
    pub backend: BackendId,
    pub auth: AuthKind,
    pub env_key: Option<String>,
}

#[derive(Debug, Clone)]
pub struct SessionBinding {
    pub session_id: String,
    pub account_id: String,
    pub backend: BackendId,
    pub cli_session: Option<String>,
    pub cwd: PathBuf,
}

#[derive(Debug, Clone)]
pub enum StdinContent {
    None,
    Prompt(String),
}

#[derive(Debug, Clone)]
pub struct SpawnPlan {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub env: HashMap<String, String>,
    pub cwd: PathBuf,
    pub stdin: StdinContent,
    pub prompt_file: Option<PathBuf>,
    pub trust_cwd: PathBuf,
    pub backend: BackendId,
    pub cleanup: Vec<SpawnCleanup>,
}

#[derive(Debug, Clone)]
pub enum ParsedLine {
    Raw(serde_json::Value),
    AssistantDelta(String),
    ToolCall {
        tool_call_id: String,
        tool: String,
        args: serde_json::Value,
    },
    Usage {
        input: u64,
        output: u64,
        cache_read: u64,
        cache_write: u64,
        reasoning: u64,
        cost: f64,
    },
    SessionInit(serde_json::Value),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnFailureClass {
    Generic,
    RateLimited,
    SubscriptionBlocked,
    AuthRequired,
    SuspectedBlocked,
    /// `--resume` of a CLI session the vendor no longer has. The engine clears `cli_session`
    /// and retries once without `--resume`.
    UnknownSession,
}

/// A CLI `--resume` of a session it no longer has. Matched on stderr or a JSON error event.
pub fn looks_like_unknown_session(stderr: &str, last_events: &[serde_json::Value]) -> bool {
    fn hit(text: &str) -> bool {
        let hay = text.to_ascii_lowercase();
        [
            "no conversation found",
            "session not found",
            "conversation not found",
            "unknown session",
            "no session found",
            "unable to resume",
            "cannot resume",
            "failed to resume",
            "invalid session id",
            "session does not exist",
            "conversation does not exist",
        ]
        .iter()
        .any(|n| hay.contains(n))
    }
    if hit(stderr) {
        return true;
    }
    last_events.iter().any(|ev| {
        ev.as_str().is_some_and(hit)
            || ev
                .pointer("/error/message")
                .and_then(|v| v.as_str())
                .is_some_and(hit)
            || ev.get("result").and_then(|v| v.as_str()).is_some_and(hit)
            || ev.get("error").and_then(|v| v.as_str()).is_some_and(hit)
            || ev.get("message").and_then(|v| v.as_str()).is_some_and(hit)
    })
}

#[derive(Debug, Clone)]
pub struct LearnedTools {
    pub tools: Vec<String>,
}

pub trait BackendAdapter: Send + Sync {
    fn id(&self) -> BackendId;
    fn build_spawn(
        &self,
        spec: &TurnSpec,
        resolved: &ModelEntry,
        session: &SessionBinding,
        data_dir: &Path,
    ) -> Result<SpawnPlan>;
    fn parse_line(&self, line: &str) -> Vec<ParsedLine>;
    fn assistant_text(&self, event: &serde_json::Value) -> Option<String>;
    fn classify_failure(
        &self,
        returncode: i32,
        stderr: &str,
        last_events: &[serde_json::Value],
    ) -> TurnFailureClass;
    fn observe_init(&self, event: &serde_json::Value) -> Option<LearnedTools>;
    fn session_followed(&self, event: &serde_json::Value) -> Option<String>;
}

pub fn for_backend(id: BackendId) -> &'static dyn BackendAdapter {
    match id {
        BackendId::ClaudeCode => &ClaudeAdapter,
        BackendId::Grok => &GrokAdapter,
        BackendId::Agy => &AgyAdapter,
        BackendId::Cursor => &CursorAdapter,
        BackendId::Codex => &codex::CodexThinOnly,
        BackendId::Pi => &OpenRouterAdapter,
    }
}

#[cfg(test)]
pub(crate) fn test_model_entry(
    id: &str,
    cli_model: &str,
    backend: BackendId,
    context_window: u64,
) -> ModelEntry {
    use crate::config::models::{default_input, Transport};
    ModelEntry {
        id: id.into(),
        backend,
        cli_model: cli_model.into(),
        context_window,
        max_tokens: 0,
        thinking: false,
        cost: 0.0,
        name: String::new(),
        provider: None,
        transport: Transport::default(),
        vendor_model: None,
        enabled: true,
        input: default_input(),
    }
}

/// Directory prefix → `/**`; globs pass through.
pub fn path_deny_rule(pattern: &str) -> String {
    let p = pattern.trim();
    if p.is_empty() {
        return p.to_string();
    }
    let normalized = p.replace('\\', "/");
    if normalized.contains('*') || normalized.contains('?') || normalized.contains('[') {
        return p.to_string();
    }
    if normalized.ends_with('/') {
        return format!("{}**", p.trim_end_matches(['/', '\\']));
    }
    if std::path::Path::new(p).is_dir() {
        let sep = if p.ends_with('\\') || p.ends_with('/') {
            ""
        } else {
            "/"
        };
        return format!("{}{}**", p, sep);
    }
    p.to_string()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    #[test]
    fn test_child_override_is_debug_only() {
        let value = Some(std::ffi::OsString::from("/tmp/fake-cli"));
        assert!(test_child_override_from(false, value.clone()).is_none());
        assert_eq!(
            test_child_override_from(true, value),
            Some(PathBuf::from("/tmp/fake-cli"))
        );
        assert!(test_child_override_from(true, None).is_none());
    }
}
