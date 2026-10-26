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
    ("-p", "2.1.32", "2026-03-01"),
    ("--output-format", "2.1.32", "2026-03-01"),
    ("--verbose", "2.1.32", "2026-03-01"),
    ("--model", "2.1.32", "2026-03-01"),
    ("--effort", "2.1.32", "2026-03-01"),
    ("--permission-mode", "2.1.32", "2026-03-01"),
    ("--disallowedTools", "2.1.32", "2026-03-01"),
    ("--session-id", "2.1.32", "2026-03-01"),
    ("--resume", "2.1.32", "2026-03-01"),
    ("--deny", "2.1.32", "2026-03-01"),
];

pub struct ClaudeAdapter;

fn resolve_program() -> Result<PathBuf> {
    if let Some(test_child) = super::test_child_override() {
        return Ok(test_child);
    }
    resolve_cli("claude").ok_or_else(|| anyhow!("claude CLI not found on PATH"))
}

pub fn build_argv(spec: &TurnSpec, resolved: &ModelEntry, session: &SessionBinding) -> Vec<String> {
    let mut args = vec![
        "-p".to_string(),
        "--output-format".to_string(),
        "stream-json".to_string(),
        "--model".to_string(),
        resolved.cli_model.clone(),
    ];
    apply_thick(
        &mut args,
        &mut std::collections::HashMap::new(),
        &profile(BackendId::ClaudeCode).thick_apply(spec.thinking),
    );
    args.extend([
        "--permission-mode".to_string(),
        "bypassPermissions".to_string(),
    ]);

    args.insert(3, "--verbose".to_string());

    if let Some(file) = &spec.system_prompt_file {
        args.push("--system-prompt-file".to_string());
        args.push(file.to_string_lossy().to_string());
    } else if let Some(prompt) = &spec.system_prompt {
        let tmp =
            std::env::temp_dir().join(format!("thickapi-system-{}.txt", uuid::Uuid::new_v4()));
        if std::fs::write(&tmp, prompt).is_ok() {
            args.push("--system-prompt-file".to_string());
            args.push(tmp.to_string_lossy().to_string());
        }
    }

    McpPlacement::apply_argv(BackendId::ClaudeCode, spec, &mut args);

    if let Some(sources) = &spec.setting_sources {
        args.push("--settings".to_string());
        args.push(sources.clone());
    }

    if let Some(cli_session) = &session.cli_session {
        args.push("--resume".to_string());
        args.push(cli_session.clone());
    } else {
        args.push("--session-id".to_string());
        args.push(
            uuid::Uuid::new_v5(
                &crate::constants::THICKAPI_CLAUDE_NS,
                session.session_id.as_bytes(),
            )
            .to_string(),
        );
    }

    DenyPlacement::apply_argv(BackendId::ClaudeCode, spec, &mut args);

    args
}

impl ClaudeAdapter {
    fn tool_calls(&self, event: &serde_json::Value) -> Vec<ParsedLine> {
        if event.get("type").and_then(|v| v.as_str()) != Some("assistant") {
            return vec![];
        }
        let content = event
            .pointer("/message/content")
            .or_else(|| event.get("content"));
        let Some(arr) = content.and_then(|c| c.as_array()) else {
            return vec![];
        };
        let mut out = Vec::new();
        for block in arr {
            if block.get("type").and_then(|v| v.as_str()) != Some("tool_use") {
                continue;
            }
            let id = block
                .get("id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            if id.is_empty() {
                continue;
            }
            let tool = block
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("tool")
                .to_string();
            let args = block
                .get("input")
                .cloned()
                .unwrap_or_else(|| serde_json::json!({}));
            out.push(ParsedLine::ToolCall {
                tool_call_id: id,
                tool,
                args,
            });
        }
        out
    }
}

impl BackendAdapter for ClaudeAdapter {
    fn id(&self) -> BackendId {
        BackendId::ClaudeCode
    }

    fn build_spawn(
        &self,
        spec: &TurnSpec,
        resolved: &ModelEntry,
        session: &SessionBinding,
        _data_dir: &std::path::Path,
    ) -> Result<SpawnPlan> {
        let program = resolve_program()?;
        let args = build_argv(spec, resolved, session);
        let cwd = dunce::canonicalize(&spec.cwd)
            .with_context(|| format!("canonicalize cwd {}", spec.cwd.display()))?;
        let mut env = child_env(spec.allow_api);
        apply_thick(
            &mut Vec::new(),
            &mut env,
            &profile(BackendId::ClaudeCode).thick_apply(spec.thinking),
        );
        super::mcp::inject_server_env(&mut env, spec)?;
        let mut cleanup = Vec::new();
        if let Some(item) = McpPlacement::apply(BackendId::ClaudeCode, &cwd, spec)? {
            cleanup.push(item);
        }
        if let Some(item) = DenyPlacement::apply(BackendId::ClaudeCode, &cwd, spec)? {
            cleanup.push(item);
        }
        Ok(SpawnPlan {
            program,
            args,
            env,
            cwd: cwd.clone(),
            stdin: StdinContent::Prompt(spec.prompt.clone()),
            prompt_file: None,
            trust_cwd: cwd,
            backend: BackendId::ClaudeCode,
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
        if value.get("type").and_then(|v| v.as_str()) == Some("system")
            && value.get("subtype").and_then(|v| v.as_str()) == Some("init")
        {
            out.push(ParsedLine::SessionInit(value.clone()));
        }
        if let Some(text) = self.assistant_text(&value) {
            out.push(ParsedLine::AssistantDelta(text));
        }
        for call in self.tool_calls(&value) {
            out.push(call);
        }
        if value.get("type").and_then(|v| v.as_str()) == Some("result") {
            let usage = value.get("usage");
            let cost = value
                .get("total_cost_usd")
                .and_then(|v| v.as_f64())
                .unwrap_or(0.0);
            out.push(ParsedLine::Usage {
                input: usage
                    .and_then(|u| u.get("input_tokens"))
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0),
                output: usage
                    .and_then(|u| u.get("output_tokens"))
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0),
                cache_read: usage
                    .and_then(|u| u.get("cache_read_input_tokens"))
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0),
                cache_write: usage
                    .and_then(|u| u.get("cache_creation_input_tokens"))
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0),
                reasoning: 0,
                cost,
            });
        }
        out
    }

    fn assistant_text(&self, event: &serde_json::Value) -> Option<String> {
        if event.get("type").and_then(|v| v.as_str()) != Some("assistant") {
            return None;
        }
        let content = event
            .pointer("/message/content")
            .or_else(|| event.get("content"))?;
        if let Some(arr) = content.as_array() {
            let mut text = String::new();
            for block in arr {
                if block.get("type").and_then(|v| v.as_str()) == Some("text") {
                    if let Some(t) = block.get("text").and_then(|v| v.as_str()) {
                        text.push_str(t);
                    }
                }
            }
            if !text.is_empty() {
                return Some(text);
            }
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
        } else if lower.contains("subscription") || lower.contains("billing") {
            TurnFailureClass::SubscriptionBlocked
        } else if super::looks_like_unknown_session(stderr, last_events) {
            TurnFailureClass::UnknownSession
        } else if lower.contains("auth") || lower.contains("login") {
            TurnFailureClass::AuthRequired
        } else {
            TurnFailureClass::Generic
        }
    }

    fn observe_init(&self, event: &serde_json::Value) -> Option<LearnedTools> {
        let tools = event
            .pointer("/tools")
            .or_else(|| event.pointer("/message/tools"))
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|t| t.get("name").and_then(|n| n.as_str()).map(String::from))
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
            .or_else(|| {
                event
                    .pointer("/message/session_id")
                    .and_then(|v| v.as_str())
                    .map(String::from)
            })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::thinking::{apply_thick, profile, Effort};
    use std::path::PathBuf;

    fn sample_spec() -> TurnSpec {
        TurnSpec {
            model: "subs/claude/sonnet".into(),
            prompt: "hi".into(),
            system_prompt: None,
            system_prompt_file: None,
            cwd: PathBuf::from("."),
            session_id: "sess-1".into(),
            thinking: Effort::Medium,
            tools: vec!["Bash".into()],
            deny_writes: vec!["src/".into()],
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
        }
    }

    fn sample_session() -> SessionBinding {
        SessionBinding {
            session_id: "sess-1".into(),
            account_id: "claude-main".into(),
            backend: BackendId::ClaudeCode,
            cli_session: None,
            cwd: PathBuf::from("."),
        }
    }

    #[test]
    fn test_the_deny_flag_stays_last_in_argv() {
        let spec = sample_spec();
        let resolved = crate::adapters::test_model_entry(
            "subs/claude/sonnet",
            "claude-sonnet-4-6",
            BackendId::ClaudeCode,
            200_000,
        );
        let argv = build_argv(&spec, &resolved, &sample_session());
        assert_eq!(argv.last().map(String::as_str), Some("Bash"));
        assert_eq!(argv[argv.len() - 2], "--disallowedTools");
    }

    #[test]
    fn golden_argv_includes_core_flags() {
        let spec = sample_spec();
        let resolved = crate::adapters::test_model_entry(
            "subs/claude/sonnet",
            "claude-sonnet-4-6",
            BackendId::ClaudeCode,
            200_000,
        );
        let argv = build_argv(&spec, &resolved, &sample_session());
        assert!(argv.contains(&"-p".to_string()));
        assert!(argv.contains(&"stream-json".to_string()));
        assert!(argv.contains(&"bypassPermissions".to_string()));
        assert!(argv.contains(&"--session-id".to_string()));
    }

    #[test]
    fn off_and_minimal_drop_effort_flag_and_pin_env() {
        let resolved = crate::adapters::test_model_entry(
            "subs/claude/sonnet",
            "claude-sonnet-4-6",
            BackendId::ClaudeCode,
            200_000,
        );
        for level in [Effort::Off, Effort::Minimal] {
            let mut spec = sample_spec();
            spec.thinking = level;
            let argv = build_argv(&spec, &resolved, &sample_session());
            assert!(!argv.contains(&"--effort".to_string()), "{level:?}");
            let mut env = std::collections::HashMap::new();
            apply_thick(
                &mut Vec::new(),
                &mut env,
                &profile(BackendId::ClaudeCode).thick_apply(level),
            );
            assert_eq!(
                env.get("MAX_THINKING_TOKENS").map(String::as_str),
                Some("0"),
                "{level:?}"
            );
        }
    }

    #[test]
    fn omitted_medium_sets_effort_medium_without_thinking_env() {
        let spec = sample_spec();
        let resolved = crate::adapters::test_model_entry(
            "subs/claude/sonnet",
            "claude-sonnet-4-6",
            BackendId::ClaudeCode,
            200_000,
        );
        let argv = build_argv(&spec, &resolved, &sample_session());
        let effort_at = argv.iter().position(|a| a == "--effort").expect("flag");
        assert_eq!(argv[effort_at + 1], "medium");
        let mut env = std::collections::HashMap::new();
        apply_thick(
            &mut Vec::new(),
            &mut env,
            &profile(BackendId::ClaudeCode).thick_apply(Effort::Medium),
        );
        assert!(!env.contains_key("MAX_THINKING_TOKENS"));
    }

    #[test]
    #[ignore]
    fn live_measured_flags_in_help() {
        if std::env::var("THICKAPI_LIVE").as_deref() != Ok("1") {
            return;
        }
        let output = std::process::Command::new("claude")
            .arg("--help")
            .output()
            .expect("claude --help");
        let help = String::from_utf8_lossy(&output.stdout);
        for (flag, _, _) in MEASURED {
            assert!(
                help.contains(flag),
                "missing flag {} in claude --help",
                flag
            );
        }
    }
}
