use std::path::PathBuf;

use anyhow::{anyhow, Context, Result};

use crate::config::models::ModelEntry;
use crate::env::child_env;
use crate::thinking::{apply_thick, profile};
use crate::types::{BackendId, TurnSpec};

use super::{
    mcp::{DenyPlacement, McpPlacement},
    resolve_cli, BackendAdapter, LearnedTools, ParsedLine, SessionBinding, SpawnPlan, StdinContent,
    TurnFailureClass,
};

pub const MEASURED: &[(&str, &str, &str)] = &[
    ("--prompt-file", "1.0.13", "2026-03-01"),
    ("--verbatim", "1.0.13", "2026-03-01"),
    ("--output-format", "1.0.13", "2026-03-01"),
    ("--permission-mode", "1.0.13", "2026-03-01"),
    ("--model", "1.0.13", "2026-03-01"),
    ("--reasoning-effort", "1.0.13", "2026-03-01"),
    ("--deny", "1.0.13", "2026-03-01"),
    // Flag read from `grok --help` on 1.0.34. Live resume is #64; unknown-session retry
    // clears a stale `cli_session` without waiting for that.
    ("--resume", "1.0.34", "2026-09-20"),
];

pub struct GrokAdapter;

fn resolve_program() -> Result<PathBuf> {
    if let Some(test_child) = super::test_child_override() {
        return Ok(test_child);
    }
    resolve_cli("grok").ok_or_else(|| anyhow!("grok CLI not found on PATH"))
}

pub fn build_argv(
    spec: &TurnSpec,
    resolved: &ModelEntry,
    session: &SessionBinding,
    prompt_file: &std::path::Path,
) -> Vec<String> {
    let mut args = vec![
        "--prompt-file".to_string(),
        prompt_file.to_string_lossy().to_string(),
        "--verbatim".to_string(),
        "--output-format".to_string(),
        "streaming-messages-json".to_string(),
        "--permission-mode".to_string(),
        "bypassPermissions".to_string(),
        "--model".to_string(),
        resolved.cli_model.clone(),
    ];
    apply_thick(
        &mut args,
        &mut std::collections::HashMap::new(),
        &profile(BackendId::Grok).thick_apply(spec.thinking),
    );

    // The second turn of a session used to start a fresh grok conversation: the binding was
    // ignored (C23). The first turn lets grok mint its own session id, which `session_followed`
    // reads off the stream and the engine stores; later turns resume it. No `--session-id` on
    // the first turn: grok refuses an id that already exists, so a first turn that failed
    // after grok had created the session would lock the thickAPI session out for good.
    if let Some(cli_session) = &session.cli_session {
        args.push("--resume".to_string());
        args.push(cli_session.clone());
    }

    DenyPlacement::apply_argv(BackendId::Grok, spec, &mut args);

    args
}

impl BackendAdapter for GrokAdapter {
    fn id(&self) -> BackendId {
        BackendId::Grok
    }

    fn build_spawn(
        &self,
        spec: &TurnSpec,
        resolved: &ModelEntry,
        session: &SessionBinding,
        _data_dir: &std::path::Path,
    ) -> Result<SpawnPlan> {
        let program = resolve_program()?;
        let cwd = dunce::canonicalize(&spec.cwd)
            .with_context(|| format!("canonicalize cwd {}", spec.cwd.display()))?;
        let prompt_file =
            std::env::temp_dir().join(format!("thickapi-grok-{}.txt", uuid::Uuid::new_v4()));
        std::fs::write(&prompt_file, &spec.prompt)
            .with_context(|| format!("write prompt file {}", prompt_file.display()))?;
        let args = build_argv(spec, resolved, session, &prompt_file);
        let mut env = child_env(spec.allow_api);
        super::mcp::inject_server_env(&mut env, spec)?;
        let mut cleanup = Vec::new();
        if let Some(item) = McpPlacement::apply(BackendId::Grok, &cwd, spec)? {
            cleanup.push(item);
        }
        if let Some(item) = DenyPlacement::apply(BackendId::Grok, &cwd, spec)? {
            cleanup.push(item);
        }
        Ok(SpawnPlan {
            program,
            args,
            env,
            cwd: cwd.clone(),
            stdin: StdinContent::None,
            prompt_file: Some(prompt_file),
            trust_cwd: cwd,
            backend: BackendId::Grok,
            cleanup,
        })
    }

    fn parse_line(&self, line: &str) -> Vec<ParsedLine> {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            return vec![];
        }
        let Ok(value) = serde_json::from_str::<serde_json::Value>(trimmed) else {
            return vec![];
        };
        let mut out = vec![ParsedLine::Raw(value.clone())];
        let ty = value.get("type").and_then(|v| v.as_str());
        // Live grok 1.0.34+ mints the session on `system`/`init` (Claude-shaped). The
        // committed fixture is still a top-level `type: init`.
        if ty == Some("init")
            || (ty == Some("system")
                && value.get("subtype").and_then(|v| v.as_str()) == Some("init"))
        {
            out.push(ParsedLine::SessionInit(value.clone()));
        }
        if let Some(text) = self.assistant_text(&value) {
            out.push(ParsedLine::AssistantDelta(text));
        }
        if let Some(usage) = value.get("usage") {
            let cost = value
                .get("total_cost_usd")
                .or_else(|| value.get("cost_usd"))
                .and_then(|v| v.as_f64())
                .unwrap_or(0.0);
            out.push(ParsedLine::Usage {
                input: usage
                    .get("input_tokens")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0),
                output: usage
                    .get("output_tokens")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0),
                cache_read: usage
                    .get("cache_read_input_tokens")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0),
                cache_write: usage
                    .get("cache_creation_input_tokens")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0),
                reasoning: usage
                    .get("reasoning_tokens")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0),
                cost,
            });
        }
        out
    }

    fn assistant_text(&self, event: &serde_json::Value) -> Option<String> {
        let ty = event.get("type").and_then(|v| v.as_str());
        // Fixture / older CLI: a string body on `assistant_message`.
        if ty == Some("assistant_message") {
            return event
                .get("content")
                .and_then(|v| v.as_str())
                .map(String::from);
        }
        // Live grok `streaming-messages-json` (1.0.34+): Claude-shaped assistant
        // records. Thinking / tool_use blocks are not text. Without this arm a
        // successful turn stores empty `TurnResult.text` (#64 side-port).
        if ty == Some("assistant") {
            if let Some(arr) = event
                .pointer("/message/content")
                .or_else(|| event.get("content"))
                .and_then(|c| c.as_array())
            {
                let text: String = arr
                    .iter()
                    .filter_map(|block| {
                        if block.get("type").and_then(|t| t.as_str()) == Some("text") {
                            block.get("text").and_then(|t| t.as_str())
                        } else {
                            None
                        }
                    })
                    .collect();
                if !text.is_empty() {
                    return Some(text);
                }
            }
            if let Some(s) = event.get("content").and_then(|v| v.as_str()) {
                return Some(s.to_string());
            }
        }
        if ty == Some("result") {
            return event
                .get("result")
                .and_then(|v| v.as_str())
                .map(String::from);
        }
        if event.get("role").and_then(|v| v.as_str()) == Some("assistant") {
            return event
                .get("content")
                .and_then(|v| v.as_str())
                .map(String::from);
        }
        None
    }

    fn classify_failure(
        &self,
        _returncode: i32,
        stderr: &str,
        last_events: &[serde_json::Value],
    ) -> TurnFailureClass {
        let lower = stderr.to_ascii_lowercase();
        if lower.contains("rate limit") {
            TurnFailureClass::RateLimited
        } else if lower.contains("subscription") {
            TurnFailureClass::SubscriptionBlocked
        } else if super::looks_like_unknown_session(stderr, last_events) {
            TurnFailureClass::UnknownSession
        } else {
            TurnFailureClass::Generic
        }
    }

    fn observe_init(&self, event: &serde_json::Value) -> Option<LearnedTools> {
        let tools = event
            .get("tools")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|t| {
                        t.as_str()
                            .or_else(|| t.get("name").and_then(|n| n.as_str()))
                            .map(String::from)
                    })
                    .collect()
            })
            .filter(|v: &Vec<String>| !v.is_empty());
        tools.map(|tools| LearnedTools { tools })
    }

    fn session_followed(&self, event: &serde_json::Value) -> Option<String> {
        event
            .get("session_id")
            .and_then(|v| v.as_str())
            .map(String::from)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::types::Effort;
    use std::path::PathBuf;

    fn binding(cli_session: Option<&str>) -> SessionBinding {
        SessionBinding {
            session_id: "s1".into(),
            account_id: "grok-main".into(),
            backend: BackendId::Grok,
            cli_session: cli_session.map(String::from),
            cwd: PathBuf::from("."),
        }
    }

    #[test]
    fn a_stored_cli_session_is_resumed_and_a_first_turn_names_none() {
        let spec: TurnSpec = serde_json::from_value(serde_json::json!({
            "model": "subs/grok/grok-4.7",
            "prompt": "hi",
            "cwd": ".",
            "session_id": "s1"
        }))
        .expect("spec");
        let resolved = crate::adapters::test_model_entry(
            "subs/grok/grok-4.7",
            "grok-4.7",
            BackendId::Grok,
            131_072,
        );
        let prompt_file = PathBuf::from("/tmp/prompt.txt");

        let first = build_argv(&spec, &resolved, &binding(None), &prompt_file);
        assert!(!first.contains(&"--resume".to_string()));
        assert!(!first.contains(&"--session-id".to_string()));

        let id = "0b6f3c1e-8a3d-4c55-9d52-5a1f0e6f7a10";
        let second = build_argv(&spec, &resolved, &binding(Some(id)), &prompt_file);
        let at = second
            .iter()
            .position(|a| a == "--resume")
            .expect("--resume");
        assert_eq!(second.get(at + 1).map(String::as_str), Some(id));
    }

    #[test]
    fn unknown_session_stderr_is_classified() {
        assert_eq!(
            GrokAdapter.classify_failure(1, "No conversation found with session ID: abc", &[]),
            TurnFailureClass::UnknownSession
        );
        assert_eq!(
            GrokAdapter.classify_failure(1, "rate limit exceeded", &[]),
            TurnFailureClass::RateLimited
        );
    }

    #[test]
    fn golden_argv_uses_prompt_file() {
        let spec = TurnSpec {
            model: "subs/grok/grok-4.7".into(),
            prompt: "hi".into(),
            system_prompt: None,
            system_prompt_file: None,
            cwd: PathBuf::from("."),
            session_id: "s1".into(),
            thinking: Effort::Medium,
            tools: vec![],
            deny_writes: vec!["lib/".into()],
            enforce_policy: None,
            mcp: crate::types::McpMode::Off,
            mcp_config: None,
            setting_sources: None,
            allow_api: false,
            combo: None,
            allow_paid: false,
            timeout_seconds: 60,
            client: None,
            turn_id: None,
            metadata: serde_json::Map::new(),
        };
        let resolved = crate::adapters::test_model_entry(
            "subs/grok/grok-4.7",
            "grok-4.7",
            BackendId::Grok,
            131_072,
        );
        let prompt_file = PathBuf::from("/tmp/prompt.txt");
        let argv = build_argv(&spec, &resolved, &binding(None), &prompt_file);
        assert_eq!(argv[0], "--prompt-file");
        assert!(argv.contains(&"streaming-messages-json".to_string()));
        assert!(argv.iter().any(|a| a.starts_with("Edit(")));
        assert!(!argv.contains(&"--mcp-config".to_string()));
    }

    #[test]
    fn off_and_minimal_use_minimal_never_none() {
        let prompt_file = PathBuf::from("/tmp/prompt.txt");
        let resolved = crate::adapters::test_model_entry(
            "subs/grok/grok-4.7",
            "grok-4.7",
            BackendId::Grok,
            131_072,
        );
        for level in [Effort::Off, Effort::Minimal] {
            let spec = TurnSpec {
                model: "subs/grok/grok-4.7".into(),
                prompt: "hi".into(),
                system_prompt: None,
                system_prompt_file: None,
                cwd: PathBuf::from("."),
                session_id: "s1".into(),
                thinking: level,
                tools: vec![],
                deny_writes: vec![],
                enforce_policy: None,
                mcp: crate::types::McpMode::Off,
                mcp_config: None,
                setting_sources: None,
                allow_api: false,
                combo: None,
                allow_paid: false,
                timeout_seconds: 60,
                client: None,
                turn_id: None,
                metadata: serde_json::Map::new(),
            };
            let argv = build_argv(&spec, &resolved, &binding(None), &prompt_file);
            let at = argv
                .iter()
                .position(|a| a == "--reasoning-effort")
                .expect("flag");
            assert_eq!(argv[at + 1], "minimal", "{level:?}");
            assert!(!argv.iter().any(|a| a == "none"), "{level:?}");
        }
    }

    #[test]
    #[ignore]
    fn live_measured_flags_in_help() {
        if std::env::var("THICKAPI_LIVE").as_deref() != Ok("1") {
            return;
        }
        let output = std::process::Command::new("grok")
            .arg("--help")
            .output()
            .expect("grok --help");
        let help = String::from_utf8_lossy(&output.stdout);
        for (flag, _, _) in MEASURED {
            assert!(help.contains(flag), "missing flag {} in grok --help", flag);
        }
    }
}
