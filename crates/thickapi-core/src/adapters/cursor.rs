use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};

use crate::config::models::ModelEntry;
use crate::env::child_env;
use crate::types::{BackendId, TurnSpec};

use super::{
    mcp::{DenyPlacement, McpPlacement},
    resolve_cli, BackendAdapter, LearnedTools, ParsedLine, SessionBinding, SpawnPlan, StdinContent,
    TurnFailureClass,
};

pub const MEASURED: &[(&str, &str, &str)] = &[
    ("-p", "2026.09.10-fd3934a", "2026-09-16"),
    ("--output-format", "2026.09.10-fd3934a", "2026-09-16"),
    ("--model", "2026.09.10-fd3934a", "2026-09-16"),
    ("--workspace", "2026.09.10-fd3934a", "2026-09-16"),
    ("--force", "2026.09.10-fd3934a", "2026-09-16"),
    ("--trust", "2026.09.10-fd3934a", "2026-09-16"),
    ("--approve-mcps", "2026.09.10-fd3934a", "2026-09-16"),
    ("--resume", "2026.09.10-fd3934a", "2026-09-16"),
];

pub struct CursorAdapter;

fn resolve_program() -> Result<(PathBuf, Vec<String>)> {
    if let Some(test_child) = super::test_child_override() {
        return Ok((test_child, vec![]));
    }
    let cli =
        resolve_cli("cursor-agent").ok_or_else(|| anyhow!("cursor-agent CLI not found on PATH"))?;
    if let Some((node, index)) = unwrap_cursor_node(&cli) {
        return Ok((node, vec![index]));
    }
    Ok((cli, vec![]))
}

/// `cursor-agent.cmd` launches powershell; `CREATE_NO_WINDOW` + piped stdout
/// then hangs with no lines. Prefer sibling `node.exe` + `index.js`.
pub fn unwrap_cursor_node(cli: &Path) -> Option<(PathBuf, String)> {
    let dir = cli.parent()?;
    let node = dir.join("node.exe");
    let index = dir.join("index.js");
    if node.is_file() && index.is_file() {
        return Some((node, index.to_string_lossy().into_owned()));
    }
    let versions_dir = dir.join("versions");
    if versions_dir.is_dir() {
        if let Ok(entries) = std::fs::read_dir(&versions_dir) {
            let mut vdirs: Vec<PathBuf> = entries
                .filter_map(|e| e.ok())
                .map(|e| e.path())
                .filter(|p| p.is_dir())
                .collect();
            vdirs.sort();
            while let Some(vdir) = vdirs.pop() {
                let vnode = vdir.join("node.exe");
                let vindex = vdir.join("index.js");
                if vnode.is_file() && vindex.is_file() {
                    return Some((vnode, vindex.to_string_lossy().into_owned()));
                }
            }
        }
    }
    None
}

pub fn build_argv(spec: &TurnSpec, resolved: &ModelEntry, session: &SessionBinding) -> Vec<String> {
    let mut args = vec![
        "-p".to_string(),
        "--output-format".to_string(),
        "stream-json".to_string(),
        "--workspace".to_string(),
        spec.cwd.to_string_lossy().to_string(),
        "--trust".to_string(),
        "--force".to_string(),
        "--approve-mcps".to_string(),
        "--model".to_string(),
        resolved.cli_model.clone(),
    ];

    if let Some(cli_session) = &session.cli_session {
        args.push("--resume".to_string());
        args.push(cli_session.clone());
    }

    args.push(spec.prompt.clone());
    args
}

impl CursorAdapter {
    fn tool_calls(&self, event: &serde_json::Value) -> Vec<ParsedLine> {
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

impl BackendAdapter for CursorAdapter {
    fn id(&self) -> BackendId {
        BackendId::Cursor
    }

    fn build_spawn(
        &self,
        spec: &TurnSpec,
        resolved: &ModelEntry,
        session: &SessionBinding,
        _data_dir: &std::path::Path,
    ) -> Result<SpawnPlan> {
        let (program, mut args) = resolve_program()?;
        let cwd = dunce::canonicalize(&spec.cwd)
            .with_context(|| format!("canonicalize cwd {}", spec.cwd.display()))?;
        let mut spec = spec.clone();
        spec.cwd = cwd.clone();
        args.extend(build_argv(&spec, resolved, session));
        let mut env = child_env(spec.allow_api);
        super::mcp::inject_server_env(&mut env, &spec)?;
        let mut cleanup = Vec::new();
        if let Some(item) = McpPlacement::apply(BackendId::Cursor, &cwd, &spec)? {
            cleanup.push(item);
        }
        if let Some(item) = DenyPlacement::apply(BackendId::Cursor, &cwd, &spec)? {
            cleanup.push(item);
        }
        Ok(SpawnPlan {
            program,
            args,
            env,
            cwd: cwd.clone(),
            stdin: StdinContent::None,
            prompt_file: None,
            trust_cwd: cwd,
            backend: BackendId::Cursor,
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
        if ty == Some("system") && value.get("subtype").and_then(|v| v.as_str()) == Some("init")
            || ty == Some("init")
        {
            out.push(ParsedLine::SessionInit(value.clone()));
        }
        if let Some(text) = self.assistant_text(&value) {
            out.push(ParsedLine::AssistantDelta(text));
        }
        for call in self.tool_calls(&value) {
            out.push(call);
        }
        if ty == Some("result") {
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
                    cache_read: usage
                        .get("cache_read_input_tokens")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0),
                    cache_write: usage
                        .get("cache_creation_input_tokens")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0),
                    reasoning: 0,
                    cost: value
                        .get("total_cost_usd")
                        .and_then(|v| v.as_f64())
                        .unwrap_or(0.0),
                });
            }
        }
        out
    }

    fn assistant_text(&self, event: &serde_json::Value) -> Option<String> {
        if event.get("type").and_then(|v| v.as_str()) == Some("assistant") {
            if let Some(arr) = event
                .pointer("/message/content")
                .or_else(|| event.get("content"))
                .and_then(|c| c.as_array())
            {
                let text: String = arr
                    .iter()
                    .filter_map(|b| {
                        if b.get("type").and_then(|t| t.as_str()) == Some("text") {
                            b.get("text").and_then(|t| t.as_str())
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
        if event.get("type").and_then(|v| v.as_str()) == Some("result") {
            return event
                .get("result")
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
        if lower.contains("rate limit") {
            TurnFailureClass::RateLimited
        } else if lower.contains("authentication") || lower.contains("not logged in") {
            TurnFailureClass::AuthRequired
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
            .or_else(|| event.get("chatId"))
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
    fn golden_argv_print_mode_no_mcp_flag() {
        let spec = TurnSpec {
            model: "subs/cursor/auto".into(),
            prompt: "hi".into(),
            system_prompt: None,
            system_prompt_file: None,
            cwd: PathBuf::from("C:\\wt"),
            session_id: "s1".into(),
            thinking: Effort::Medium,
            tools: vec![],
            deny_writes: vec![],
            enforce_policy: None,
            mcp: crate::types::McpMode::On,
            mcp_config: Some(PathBuf::from("mcp.json")),
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
            "subs/cursor/auto",
            "auto",
            BackendId::Cursor,
            128_000,
        );
        let session = SessionBinding {
            session_id: "s1".into(),
            account_id: "cursor-main".into(),
            backend: BackendId::Cursor,
            cli_session: None,
            cwd: PathBuf::from("C:\\wt"),
        };
        let argv = build_argv(&spec, &resolved, &session);
        assert_eq!(argv[0], "-p");
        assert!(argv.contains(&"stream-json".to_string()));
        assert!(argv.contains(&"--approve-mcps".to_string()));
        assert!(argv.contains(&"auto".to_string()));
        assert_eq!(argv.last().map(String::as_str), Some("hi"));
        assert!(!argv.contains(&"--mcp-config".to_string()));
    }

    #[test]
    fn off_vs_high_argv_identical() {
        let resolved = crate::adapters::test_model_entry(
            "subs/cursor/auto",
            "auto",
            BackendId::Cursor,
            128_000,
        );
        let session = SessionBinding {
            session_id: "s1".into(),
            account_id: "cursor-main".into(),
            backend: BackendId::Cursor,
            cli_session: None,
            cwd: PathBuf::from("C:\\wt"),
        };
        let mut off = spec_print();
        off.thinking = Effort::Off;
        let mut high = spec_print();
        high.thinking = Effort::High;
        assert_eq!(
            build_argv(&off, &resolved, &session),
            build_argv(&high, &resolved, &session)
        );
    }

    fn spec_print() -> TurnSpec {
        TurnSpec {
            model: "subs/cursor/auto".into(),
            prompt: "hi".into(),
            system_prompt: None,
            system_prompt_file: None,
            cwd: PathBuf::from("C:\\wt"),
            session_id: "s1".into(),
            thinking: Effort::Medium,
            tools: vec![],
            deny_writes: vec![],
            enforce_policy: None,
            mcp: crate::types::McpMode::On,
            mcp_config: Some(PathBuf::from("mcp.json")),
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
    #[ignore]
    fn unwrap_cursor_node_finds_sibling_node() {
        if std::env::var("THICKAPI_LIVE").as_deref() != Ok("1") {
            return;
        }
        let cli = resolve_cli("cursor-agent").expect("cursor-agent on PATH");
        let (node, index) = unwrap_cursor_node(&cli).expect("sibling node.exe + index.js");
        assert!(node.ends_with("node.exe"), "{}", node.display());
        assert!(index.ends_with("index.js"), "{index}");
        assert_eq!(node.parent(), Path::new(&index).parent());
    }

    #[test]
    #[ignore]
    fn live_measured_flags_in_help() {
        if std::env::var("THICKAPI_LIVE").as_deref() != Ok("1") {
            return;
        }
        let output = std::process::Command::new("cursor-agent")
            .arg("--help")
            .output()
            .expect("cursor-agent --help");
        let help = String::from_utf8_lossy(&output.stdout);
        for (flag, _, _) in MEASURED {
            assert!(
                help.contains(flag),
                "missing flag {} in cursor-agent --help",
                flag
            );
        }
    }
}
