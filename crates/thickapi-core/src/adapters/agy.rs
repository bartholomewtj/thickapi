use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};

use crate::config::models::ModelEntry;
use crate::env::child_env;
use crate::thinking::{apply_thick, profile, strip_agy_effort_suffix};
use crate::types::{BackendId, TurnSpec};

use super::{
    mcp::{DenyPlacement, McpPlacement},
    resolve_cli, BackendAdapter, LearnedTools, ParsedLine, SessionBinding, SpawnPlan, StdinContent,
    TurnFailureClass,
};

pub const MEASURED: &[(&str, &str, &str)] = &[
    ("-p", "1.1.22", "2026-03-01"),
    ("--output-format", "1.1.22", "2026-03-01"),
    ("--model", "1.1.22", "2026-03-01"),
    ("--effort", "1.1.22", "2026-03-01"),
    ("--print-timeout", "1.1.22", "2026-03-01"),
    ("--disable-slash-commands", "1.1.22", "2026-03-01"),
    ("--add-dir", "1.1.22", "2026-03-01"),
    ("--conversation", "1.1.22", "2026-03-01"),
    ("--dangerously-skip-permissions", "1.1.22", "2026-03-01"),
];

/// The engine's agy idle stall in the `--print-timeout` form agy wants. One number, so the
/// window agy is told it has and the window the engine actually allows cannot drift (B12).
fn print_timeout() -> String {
    format!("{}m", crate::engine::AGY_IDLE_STALL_SECONDS / 60)
}

pub struct AgyAdapter;

fn is_within(path: &Path, root: &Path) -> bool {
    let path = dunce::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let root = dunce::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    path.starts_with(root)
}

fn compose_first_prompt(system_prompt: Option<&str>, prompt: &str) -> String {
    if let Some(sys) = system_prompt.filter(|s| !s.is_empty()) {
        format!(
            "# SYSTEM INSTRUCTIONS\n\n{}\n\n---\n\n# TASK\n\n{}",
            sys, prompt
        )
    } else {
        prompt.to_string()
    }
}

fn resolve_program() -> Result<PathBuf> {
    if let Some(test_child) = super::test_child_override() {
        return Ok(test_child);
    }
    resolve_cli("agy").ok_or_else(|| anyhow!("agy CLI not found on PATH"))
}

pub fn build_argv(
    spec: &TurnSpec,
    resolved: &ModelEntry,
    session: &SessionBinding,
    prompt_file: &Path,
    cwd: &Path,
) -> Vec<String> {
    let mut args = vec![
        "-p".to_string(),
        format!("@{}", prompt_file.to_string_lossy()),
        "--output-format".to_string(),
        "stream-json".to_string(),
        "--model".to_string(),
        strip_agy_effort_suffix(&resolved.cli_model).to_string(),
    ];
    apply_thick(
        &mut args,
        &mut std::collections::HashMap::new(),
        &profile(BackendId::Agy).thick_apply(spec.thinking),
    );
    args.push("--print-timeout".to_string());
    args.push(print_timeout());
    args.push("--disable-slash-commands".to_string());

    if !is_within(prompt_file, cwd) {
        if let Some(parent) = prompt_file.parent() {
            args.push("--add-dir".to_string());
            args.push(parent.to_string_lossy().to_string());
        }
    }

    if let Some(conversation) = &session.cli_session {
        args.push("--conversation".to_string());
        args.push(conversation.clone());
    }

    if std::env::var("AGY_ASK_PERMISSIONS").as_deref() != Ok("1") {
        args.push("--dangerously-skip-permissions".to_string());
    }

    args
}

impl BackendAdapter for AgyAdapter {
    fn id(&self) -> BackendId {
        BackendId::Agy
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

        let prompt_text = if session.cli_session.is_some() {
            spec.prompt.clone()
        } else {
            compose_first_prompt(spec.system_prompt.as_deref(), &spec.prompt)
        };

        let prompt_file =
            std::env::temp_dir().join(format!("thickapi-agy-{}.md", uuid::Uuid::new_v4()));
        std::fs::write(&prompt_file, &prompt_text)
            .with_context(|| format!("write prompt file {}", prompt_file.display()))?;

        let args = build_argv(spec, resolved, session, &prompt_file, &cwd);
        let mut env = child_env(spec.allow_api);
        super::mcp::inject_server_env(&mut env, spec)?;
        let mut cleanup = Vec::new();
        if let Some(item) = McpPlacement::apply(BackendId::Agy, &cwd, spec)? {
            cleanup.push(item);
        }
        if let Some(item) = DenyPlacement::apply(BackendId::Agy, &cwd, spec)? {
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
            backend: BackendId::Agy,
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
        if value.get("event").and_then(|v| v.as_str()) == Some("init") {
            out.push(ParsedLine::SessionInit(value.clone()));
        }
        if let Some(text) = self.assistant_text(&value) {
            out.push(ParsedLine::AssistantDelta(text));
        }
        if value.get("event").and_then(|v| v.as_str()) == Some("step_update") {
            let usage = value.get("step_update").and_then(|s| s.get("usage"));
            if let Some(usage) = usage {
                let cached = usage
                    .get("cache_read_tokens")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0);
                let input = usage
                    .get("input_tokens")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0);
                let output = usage
                    .get("output_tokens")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0)
                    + usage
                        .get("thinking_tokens")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0);
                out.push(ParsedLine::Usage {
                    input: input.saturating_sub(cached),
                    output,
                    cache_read: cached,
                    cache_write: 0,
                    reasoning: usage
                        .get("thinking_tokens")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0),
                    cost: 0.0,
                });
            }
        }
        out
    }

    fn assistant_text(&self, event: &serde_json::Value) -> Option<String> {
        if event.get("event").and_then(|v| v.as_str()) != Some("result") {
            return None;
        }
        event
            .get("result")
            .and_then(|r| r.get("response"))
            .and_then(|v| v.as_str())
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .map(String::from)
    }

    fn classify_failure(
        &self,
        _returncode: i32,
        stderr: &str,
        last_events: &[serde_json::Value],
    ) -> TurnFailureClass {
        let mut haystack = stderr.to_ascii_lowercase();
        if let Some(last) = last_events.last() {
            if let Some(result) = last.get("result") {
                for key in ["status", "error", "response"] {
                    if let Some(v) = result.get(key).and_then(|v| v.as_str()) {
                        haystack.push(' ');
                        haystack.push_str(&v.to_ascii_lowercase());
                    }
                }
            }
        }
        if haystack.contains("rate limit")
            || haystack.contains("429")
            || haystack.contains("resource_exhausted")
            || haystack.contains("quota")
            || haystack.contains("too many requests")
            || haystack.contains("timeout waiting for response")
        {
            TurnFailureClass::RateLimited
        } else if haystack.contains("subscription") {
            TurnFailureClass::SubscriptionBlocked
        } else {
            TurnFailureClass::Generic
        }
    }

    fn observe_init(&self, _event: &serde_json::Value) -> Option<LearnedTools> {
        None
    }

    fn session_followed(&self, event: &serde_json::Value) -> Option<String> {
        event
            .get("conversation_id")
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

    #[test]
    fn golden_argv_no_deny_flags() {
        let spec = TurnSpec {
            model: "subs/agy/gemini-3.6-flash-high".into(),
            prompt: "hi".into(),
            system_prompt: Some("sys".into()),
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
            "subs/agy/gemini-3.6-flash-high",
            "gemini-3.6-flash-high",
            BackendId::Agy,
            1_000_000,
        );
        let session = SessionBinding {
            session_id: "s1".into(),
            account_id: "agy-main".into(),
            backend: BackendId::Agy,
            cli_session: None,
            cwd: PathBuf::from("."),
        };
        let prompt_file = PathBuf::from("/tmp/prompt.md");
        let argv = build_argv(&spec, &resolved, &session, &prompt_file, Path::new("."));
        assert!(argv.iter().any(|a| a.starts_with("@")));
        assert!(!argv.iter().any(|a| a == "--deny"));
        let at = argv
            .iter()
            .position(|a| a == "--print-timeout")
            .expect("print timeout");
        // The flag agy is given and the engine's agy idle stall are the same window (B12).
        assert_eq!(argv[at + 1], "240m");
        assert_eq!(crate::engine::AGY_IDLE_STALL_SECONDS, 240 * 60);
        assert_eq!(
            crate::engine::default_idle_stall_seconds(BackendId::Agy),
            crate::engine::AGY_IDLE_STALL_SECONDS
        );
        assert!(!argv.contains(&"--mcp-config".to_string()));
    }

    fn agy_spec(thinking: Effort) -> TurnSpec {
        TurnSpec {
            model: "subs/agy/gemini-3.8-flash".into(),
            prompt: "hi".into(),
            system_prompt: None,
            system_prompt_file: None,
            cwd: PathBuf::from("."),
            session_id: "s1".into(),
            thinking,
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
        }
    }

    #[test]
    fn stale_high_suffix_strips_and_off_omits_effort() {
        let resolved = crate::adapters::test_model_entry(
            "subs/agy/gemini-3.8-flash",
            "gemini-3.8-flash-high",
            BackendId::Agy,
            1_000_000,
        );
        let session = SessionBinding {
            session_id: "s1".into(),
            account_id: "agy-main".into(),
            backend: BackendId::Agy,
            cli_session: None,
            cwd: PathBuf::from("."),
        };
        let prompt_file = PathBuf::from("/tmp/prompt.md");
        let argv = build_argv(
            &agy_spec(Effort::Off),
            &resolved,
            &session,
            &prompt_file,
            Path::new("."),
        );
        let model_at = argv.iter().position(|a| a == "--model").expect("model");
        assert_eq!(argv[model_at + 1], "gemini-3.8-flash");
        assert!(!argv.contains(&"--effort".to_string()));

        let argv = build_argv(
            &agy_spec(Effort::Medium),
            &resolved,
            &session,
            &prompt_file,
            Path::new("."),
        );
        let model_at = argv.iter().position(|a| a == "--model").expect("model");
        assert_eq!(argv[model_at + 1], "gemini-3.8-flash");
        let effort_at = argv.iter().position(|a| a == "--effort").expect("effort");
        assert_eq!(argv[effort_at + 1], "medium");
    }

    #[test]
    #[ignore]
    fn live_measured_flags_in_help() {
        if std::env::var("THICKAPI_LIVE").as_deref() != Ok("1") {
            return;
        }
        let output = std::process::Command::new("agy")
            .arg("--help")
            .output()
            .expect("agy --help");
        let help = String::from_utf8_lossy(&output.stdout);
        for (flag, _, _) in MEASURED {
            assert!(help.contains(flag), "missing flag {} in agy --help", flag);
        }
    }
}
