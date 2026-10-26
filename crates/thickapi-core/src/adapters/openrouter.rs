//! OpenRouter combo-tail adapter (`or/*`).
//!
//! Spawns the `pi` CLI with an isolated `PI_CODING_AGENT_DIR`. Pool name is
//! `openrouter`. The catalogue/sqlite id stays `BackendId::Pi` so yaml
//! `backend: pi` and stored session bindings stay compatible.

use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};

use crate::config::models::ModelEntry;
use crate::env::child_env;
use crate::types::{BackendId, TurnSpec};

use super::{
    mcp::{DenyPlacement, McpPlacement},
    resolve_cli, Account, BackendAdapter, LearnedTools, ParsedLine, SessionBinding, SpawnPlan,
    StdinContent, TurnFailureClass,
};

pub const MEASURED: &[(&str, &str, &str)] = &[
    ("-p", "1.0.0", "2026-03-01"),
    ("--model", "1.0.0", "2026-03-01"),
    ("--output-format", "1.0.0", "2026-03-01"),
];

pub struct OpenRouterAdapter;

fn resolve_program() -> Result<PathBuf> {
    if let Some(test_child) = super::test_child_override() {
        return Ok(test_child);
    }
    resolve_cli("pi").ok_or_else(|| anyhow!("pi CLI not found on PATH"))
}

pub fn pi_agent_dir(data_dir: &Path, session_id: &str) -> PathBuf {
    data_dir
        .join("pi-agents")
        .join(crate::engine::path_component(session_id))
}

pub fn inject_openrouter_env(
    mut env: std::collections::HashMap<String, String>,
    account: &Account,
) -> Result<std::collections::HashMap<String, String>> {
    if let Some(env_key) = &account.env_key {
        let value = std::env::var(env_key)
            .with_context(|| format!("missing env var {} for OpenRouter account", env_key))?;
        env.insert(env_key.clone(), value);
    }
    Ok(env)
}

pub fn build_argv(resolved: &ModelEntry) -> Vec<String> {
    vec![
        "-p".to_string(),
        "--output-format".to_string(),
        "streaming-messages-json".to_string(),
        "--model".to_string(),
        resolved.cli_model.clone(),
    ]
}

impl BackendAdapter for OpenRouterAdapter {
    fn id(&self) -> BackendId {
        BackendId::Pi
    }

    fn build_spawn(
        &self,
        spec: &TurnSpec,
        resolved: &ModelEntry,
        session: &SessionBinding,
        data_dir: &Path,
    ) -> Result<SpawnPlan> {
        let program = resolve_program()?;
        let cwd = dunce::canonicalize(&spec.cwd)
            .with_context(|| format!("canonicalize cwd {}", spec.cwd.display()))?;
        let agent_dir = pi_agent_dir(data_dir, &session.session_id);
        std::fs::create_dir_all(&agent_dir)
            .with_context(|| format!("create pi agent dir {}", agent_dir.display()))?;

        let mut env = child_env(spec.allow_api);
        env.insert(
            "PI_CODING_AGENT_DIR".to_string(),
            agent_dir.to_string_lossy().to_string(),
        );

        let mut cleanup = Vec::new();
        if let Some(item) = McpPlacement::apply(BackendId::Pi, &cwd, spec)? {
            cleanup.push(item);
        }
        if let Some(item) = DenyPlacement::apply(BackendId::Pi, &cwd, spec)? {
            cleanup.push(item);
        }
        Ok(SpawnPlan {
            program,
            args: build_argv(resolved),
            env,
            cwd: cwd.clone(),
            stdin: StdinContent::Prompt(spec.prompt.clone()),
            prompt_file: None,
            trust_cwd: cwd,
            backend: BackendId::Pi,
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
        if value.get("type").and_then(|v| v.as_str()) == Some("init") {
            out.push(ParsedLine::SessionInit(value.clone()));
        }
        if let Some(text) = self.assistant_text(&value) {
            out.push(ParsedLine::AssistantDelta(text));
        }
        if let Some(usage) = value.get("usage") {
            out.push(ParsedLine::Usage {
                input: usage
                    .get("input_tokens")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0),
                output: usage
                    .get("output_tokens")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0),
                cache_read: 0,
                cache_write: 0,
                reasoning: usage
                    .get("reasoning_tokens")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0),
                cost: value
                    .get("cost_usd")
                    .and_then(|v| v.as_f64())
                    .or_else(|| usage.get("cost_usd").and_then(|v| v.as_f64()))
                    .unwrap_or(0.0),
            });
        }
        out
    }

    fn assistant_text(&self, event: &serde_json::Value) -> Option<String> {
        if event.get("type").and_then(|v| v.as_str()) == Some("assistant_message") {
            return event
                .get("content")
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
        _last_events: &[serde_json::Value],
    ) -> TurnFailureClass {
        let lower = stderr.to_ascii_lowercase();
        if lower.contains("429") && !lower.contains("retry-after") && !lower.contains("retry after")
        {
            return TurnFailureClass::SuspectedBlocked;
        }
        if lower.contains("rate limit") {
            TurnFailureClass::RateLimited
        } else if lower.contains("subscription") {
            TurnFailureClass::SubscriptionBlocked
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

    #[test]
    fn golden_argv_uses_model_flag() {
        let resolved = crate::adapters::test_model_entry(
            "or/anthropic/claude-sonnet-4-6",
            "anthropic/claude-sonnet-4-6",
            BackendId::Pi,
            200_000,
        );
        let argv = build_argv(&resolved);
        assert_eq!(argv[0], "-p");
        assert!(argv.contains(&"anthropic/claude-sonnet-4-6".to_string()));
    }

    #[test]
    fn inject_openrouter_env_sets_key() {
        unsafe {
            std::env::set_var("THICKAPI_TEST_OR_KEY", "test-key");
        }
        let account = Account {
            id: "or-main".into(),
            backend: BackendId::Pi,
            auth: crate::config::accounts::AuthKind::Env,
            env_key: Some("THICKAPI_TEST_OR_KEY".into()),
        };
        let env = inject_openrouter_env(child_env(false), &account).expect("inject");
        assert_eq!(
            env.get("THICKAPI_TEST_OR_KEY"),
            Some(&"test-key".to_string())
        );
        unsafe {
            std::env::remove_var("THICKAPI_TEST_OR_KEY");
        }
    }

    #[test]
    fn classify_headerless_429_as_suspected_blocked() {
        let adapter = OpenRouterAdapter;
        assert_eq!(
            adapter.classify_failure(1, "HTTP 429 Too Many Requests", &[]),
            TurnFailureClass::SuspectedBlocked
        );
        assert_eq!(
            adapter.classify_failure(1, "HTTP 429 Too Many Requests Retry-After: 60", &[]),
            TurnFailureClass::Generic
        );
    }

    #[test]
    #[ignore]
    fn live_measured_flags_in_help() {
        if std::env::var("THICKAPI_LIVE").as_deref() != Ok("1") {
            return;
        }
        let output = std::process::Command::new("pi")
            .arg("--help")
            .output()
            .expect("pi --help");
        let help = String::from_utf8_lossy(&output.stdout);
        for (flag, _, _) in MEASURED {
            assert!(help.contains(flag), "missing flag {} in pi --help", flag);
        }
    }
}
