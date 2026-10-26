use std::path::PathBuf;

use serde::{Deserialize, Serialize};

pub use crate::thinking::Effort;

pub type TurnId = String;
pub type SessionId = String;

pub fn default_thinking() -> Effort {
    Effort::Medium
}

pub fn default_timeout() -> u64 {
    5400
}

pub fn default_enforce_mode() -> EnforceMode {
    EnforceMode::GitFingerprint
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum McpMode {
    Off,
    #[default]
    Auto,
    On,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub struct TurnSpec {
    pub model: String,
    pub prompt: String,
    #[serde(default)]
    pub system_prompt: Option<String>,
    pub system_prompt_file: Option<PathBuf>,
    pub cwd: PathBuf,
    pub session_id: SessionId,
    #[serde(default = "default_thinking")]
    pub thinking: Effort,
    #[serde(default)]
    pub tools: Vec<String>,
    #[serde(default)]
    pub deny_writes: Vec<String>,
    #[serde(default)]
    pub enforce_policy: Option<EnforcePolicy>,
    #[serde(default)]
    pub mcp: McpMode,
    #[serde(default)]
    pub mcp_config: Option<PathBuf>,
    pub setting_sources: Option<String>,
    #[serde(default)]
    pub allow_api: bool,
    pub combo: Option<String>,
    #[serde(default)]
    pub allow_paid: bool,
    #[serde(default = "default_timeout")]
    pub timeout_seconds: u64,
    pub client: Option<String>,
    pub turn_id: Option<TurnId>,
    #[serde(default)]
    pub metadata: serde_json::Map<String, serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub struct EnforcePolicy {
    pub writes: Option<Vec<String>>,
    #[serde(default)]
    pub protected_files: Vec<String>,
    pub out_of_scope: Option<Vec<String>>,
    #[serde(default)]
    pub always_writable: Vec<String>,
    #[serde(default = "default_enforce_mode")]
    pub mode: EnforceMode,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EnforceMode {
    GitFingerprint,
    TreeCopy,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TurnEvent {
    TurnStart {
        turn_id: TurnId,
        session_id: SessionId,
        backend: BackendId,
        account_id: String,
        model: String,
    },
    Queued {
        position: u32,
    },
    AssistantDelta {
        text: String,
    },
    ToolCall {
        tool: String,
        tool_call_id: String,
        args: serde_json::Value,
        label: Option<String>,
        ok: Option<bool>,
        result_snippet: Option<String>,
        duration_ms: Option<u64>,
        started_at: String,
        ended_at: Option<String>,
    },
    Usage {
        input: u64,
        output: u64,
        cache_read: u64,
        cache_write: u64,
        reasoning: u64,
        cost: f64,
        cost_components: serde_json::Value,
    },
    TurnFallback {
        from_model: String,
        to_model: String,
        reason: String,
    },
    TurnEnd {
        state: TurnState,
        returncode: i32,
        text: String,
        retry_after_seconds: Option<u64>,
        touched_paths: Vec<String>,
        breaches: Vec<PermissionBreach>,
    },
    Error {
        code: String,
        message: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TurnResult {
    pub turn_id: TurnId,
    pub session_id: SessionId,
    pub state: TurnState,
    pub returncode: i32,
    pub text: String,
    pub tokens: u64,
    pub cost: f64,
    pub context_tokens: u64,
    pub usage: UsageBreakdown,
    pub context_window: u64,
    pub backend: BackendId,
    pub account_id: String,
    pub resolved_model: String,
    /// §3.1 code when the engine, not the child, ended the turn. Replay uses this
    /// field; it does not re-parse `text`. Absent on older `result.json` files.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnState {
    Success,
    Failed,
    Cancelled,
    Timeout,
    RateLimited,
    SubscriptionBlocked,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BackendId {
    ClaudeCode,
    Grok,
    Agy,
    Cursor,
    Codex,
    Pi,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub struct UsageBreakdown {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub reasoning: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct PermissionBreach {
    pub path: String,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredEvent {
    pub seq: u64,
    pub event: TurnEvent,
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    /// A misspelt safety field must fail the request, not run the turn unenforced.
    #[test]
    fn misspelt_enforcement_fields_are_rejected() {
        let base = r#""model":"m","prompt":"p","cwd":".","session_id":"s""#;
        assert!(serde_json::from_str::<TurnSpec>(&format!("{{{base}}}")).is_ok());
        for typo in [
            r#""enforcePolicy":{"writes":[]}"#,
            r#""enforce_policy":{"protected_file":["a"]}"#,
            r#""deny_write":["src"]"#,
        ] {
            let err =
                serde_json::from_str::<TurnSpec>(&format!("{{{base},{typo}}}")).expect_err(typo);
            assert!(err.to_string().contains("unknown field"), "{err}");
        }
    }

    #[test]
    fn turn_spec_roundtrip() {
        let spec = TurnSpec {
            model: "subs/claude/sonnet".into(),
            prompt: "hello".into(),
            system_prompt: None,
            system_prompt_file: None,
            cwd: PathBuf::from("/tmp"),
            session_id: "sess-1".into(),
            thinking: Effort::Medium,
            tools: vec![],
            deny_writes: vec![],
            enforce_policy: None,
            mcp: McpMode::Auto,
            mcp_config: None,
            setting_sources: None,
            allow_api: false,
            combo: None,
            allow_paid: false,
            timeout_seconds: 5400,
            client: Some("cli".into()),
            turn_id: None,
            metadata: serde_json::Map::new(),
        };
        let json = serde_json::to_string(&spec).expect("serialize");
        let back: TurnSpec = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back.model, "subs/claude/sonnet");
        assert_eq!(back.prompt, "hello");
        assert_eq!(back.thinking, Effort::Medium);
    }

    #[test]
    fn turn_event_variants() {
        let events: Vec<TurnEvent> = vec![
            TurnEvent::TurnStart {
                turn_id: "t1".into(),
                session_id: "s1".into(),
                backend: BackendId::ClaudeCode,
                account_id: "claude-main".into(),
                model: "subs/claude/sonnet".into(),
            },
            TurnEvent::Queued { position: 1 },
            TurnEvent::AssistantDelta { text: "hi".into() },
            TurnEvent::ToolCall {
                tool: "edit".into(),
                tool_call_id: "tc1".into(),
                args: serde_json::json!({}),
                label: None,
                ok: Some(true),
                result_snippet: None,
                duration_ms: Some(100),
                started_at: "2026-01-01T00:00:00Z".into(),
                ended_at: None,
            },
            TurnEvent::Usage {
                input: 1,
                output: 2,
                cache_read: 0,
                cache_write: 0,
                reasoning: 0,
                cost: 0.0,
                cost_components: serde_json::json!({}),
            },
            TurnEvent::TurnFallback {
                from_model: "a".into(),
                to_model: "b".into(),
                reason: "rate limited".into(),
            },
            TurnEvent::TurnEnd {
                state: TurnState::Success,
                returncode: 0,
                text: "done".into(),
                retry_after_seconds: None,
                touched_paths: vec![],
                breaches: vec![],
            },
            TurnEvent::Error {
                code: "spawn_failed".into(),
                message: "not found".into(),
            },
        ];
        for event in events {
            let json = serde_json::to_string(&event).expect("serialize");
            let _: TurnEvent = serde_json::from_str(&json).expect("deserialize");
        }
    }

    #[test]
    fn turn_result_roundtrip() {
        let result = TurnResult {
            turn_id: "t1".into(),
            session_id: "s1".into(),
            state: TurnState::Success,
            returncode: 0,
            text: "ok".into(),
            tokens: 100,
            cost: 0.0,
            context_tokens: 50,
            usage: UsageBreakdown::default(),
            context_window: 200_000,
            backend: BackendId::ClaudeCode,
            account_id: "claude-main".into(),
            resolved_model: "claude-sonnet-4-6".into(),
            error_code: None,
        };
        let json = serde_json::to_string(&result).expect("serialize");
        let back: TurnResult = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back.turn_id, "t1");
        assert_eq!(back.state, TurnState::Success);
        assert_eq!(back.error_code, None);
        let old = r#"{"turn_id":"t1","session_id":"s1","state":"success","returncode":0,"text":"ok","tokens":0,"cost":0.0,"context_tokens":0,"usage":{"input":0,"output":0,"cache_read":0,"cache_write":0,"reasoning":0},"context_window":0,"backend":"claude_code","account_id":"","resolved_model":""}"#;
        let old_back: TurnResult = serde_json::from_str(old).expect("old result.json");
        assert_eq!(old_back.error_code, None);
    }

    #[test]
    fn backend_id_serde() {
        let json = serde_json::to_string(&BackendId::ClaudeCode).expect("serialize");
        assert_eq!(json, "\"claude_code\"");
        let back: BackendId = serde_json::from_str("\"grok\"").expect("deserialize");
        assert_eq!(back, BackendId::Grok);
    }
}
