//! Argv, environment allowlist and process launch for the Claude bridge (SPEC §19.7).
//! Argv is the thirteen reference flags from `claude-args.ts`, verbatim.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use serde_json::json;
use tokio::process::{Child, Command};

use crate::engine::job::JobObject;
use crate::thinking::{profile, Effort, VendorThinking};
use crate::types::BackendId;

/// Same settings blob upstream pins: hooks and auto-memory off, and the token reminder off
/// (Claude Code 2.1.233 added a changing reminder that breaks prefix caching).
pub const SETTINGS: &str =
    r#"{"disableAllHooks":true,"autoMemoryEnabled":false,"totalTokensReminder":"off"}"#;
pub const EMPTY_MCP: &str = r#"{"mcpServers":{}}"#;

/// Exit code we hand `TerminateJobObject` on a tool handoff. Accepted together with
/// a clean 0 when `tool_use_stop` is already set (the child exited first).
pub const KNOWN_EXIT: u32 = 0x7A11;
/// `taskkill /F` fallback closes the root as 1 (upstream's Windows acceptance).
pub const TASKKILL_EXIT: i32 = 1;

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

pub struct McpPaths<'a> {
    pub catalog: &'a Path,
    pub ready: &'a Path,
    pub violation: &'a Path,
}

/// `--mcp-config` JSON pointing Claude at `thickapi mcp-proposal` by absolute path.
pub fn mcp_config_json(thickapi_exe: &Path, paths: Option<&McpPaths<'_>>) -> String {
    match paths {
        None => EMPTY_MCP.to_string(),
        Some(p) => json!({
            "mcpServers": {
                "pi": {
                    "command": thickapi_exe,
                    "args": [
                        "mcp-proposal",
                        "--catalog", p.catalog,
                        "--ready", p.ready,
                        "--violation", p.violation,
                    ],
                    "env": {},
                }
            }
        })
        .to_string(),
    }
}

/// Effort ladder per SPEC §19.5 (thin apply, Claude bridge). `None` → no `--effort`;
/// Off/Minimal → no `--effort` (thinking is off through `CLAUDE_CODE_DISABLE_THINKING`). Every other
/// level is passed by name: Claude Code 2.1.308 takes `low|medium|high|xhigh|max`, and Fable
/// 5.1, Opus 5.5 and Sonnet 5.5 each answer at `xhigh` and `max` (measured 2026-10-25). The
/// frozen thick lane still folds xhigh/max to `high`.
pub fn effort_flag(level: Option<Effort>) -> Option<&'static str> {
    match level? {
        Effort::Off | Effort::Minimal => None,
        Effort::Low => Some("low"),
        Effort::Medium => Some("medium"),
        Effort::High => Some("high"),
        Effort::XHigh => Some("xhigh"),
        Effort::Max => Some("max"),
    }
}

/// What the spawn applies, for the access line's `wire_params` (SPEC §19.15 R5).
pub fn wire_params(level: Option<Effort>) -> Option<String> {
    if thinking_off(level) {
        return Some("thinking=disabled".into());
    }
    effort_flag(level).map(|e| format!("effort={e}"))
}

pub fn thinking_off(level: Option<Effort>) -> bool {
    let Some(effort) = level else {
        return false;
    };
    profile(BackendId::ClaudeCode)
        .thick_apply(effort)
        .iter()
        .any(|a| {
            matches!(
                a,
                VendorThinking::Env {
                    name: "MAX_THINKING_TOKENS",
                    ..
                }
            )
        })
}

pub fn base_args() -> Vec<String> {
    [
        "-p",
        "--setting-sources",
        "",
        "--settings",
        SETTINGS,
        "--disable-slash-commands",
        "--strict-mcp-config",
        "--permission-mode",
        "dontAsk",
        "--no-chrome",
        "--no-session-persistence",
        "--prompt-suggestions",
        "false",
    ]
    .into_iter()
    .map(String::from)
    .collect()
}

pub fn claude_args(
    mcp_config: &str,
    model: &str,
    effort: Option<Effort>,
    system_prompt_path: &Path,
) -> Vec<String> {
    let mut args = base_args();
    args.extend(["--mcp-config", mcp_config, "--tools", "", "--model", model].map(String::from));
    if let Some(effort) = effort_flag(effort) {
        args.extend(["--effort", effort].map(String::from));
    }
    // Claude Code 2.1.308 streams thinking blocks with empty text in `-p` mode unless the
    // display is `summarized`, so Pi saw no Claude reasoning at any level; the
    // `showThinkingSummaries` setting is not honoured under `--setting-sources ""`, the flag
    // (hidden from `--help`) is (measured 2026-10-25). Thinking off has nothing to show.
    if !thinking_off(effort) {
        args.extend(["--thinking-display", "summarized"].map(String::from));
    }
    args.extend(
        [
            "--input-format",
            "stream-json",
            "--output-format",
            "stream-json",
            "--include-partial-messages",
            "--verbose",
            "--system-prompt-file",
        ]
        .map(String::from),
    );
    args.push(system_prompt_path.to_string_lossy().into_owned());
    args
}

/// Names copied from the daemon's environment. Anything else (notably `ANTHROPIC_*` and other
/// vendors' API keys) is dropped: an inherited key flips `apiKeySource` and fails init, or
/// silently bills the API.
pub const ENV_ALLOWLIST: &[&str] = &[
    "USERPROFILE",
    "HOMEDRIVE",
    "HOMEPATH",
    "HOME",
    "PATH",
    "PATHEXT",
    "SystemRoot",
    "SYSTEMROOT",
    "windir",
    "ComSpec",
    "COMSPEC",
    "TEMP",
    "TMP",
    "TMPDIR",
    "LANG",
    "LC_ALL",
    "LC_CTYPE",
    "USER",
    "USERNAME",
    "LOGNAME",
    "SHELL",
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "NO_PROXY",
    "http_proxy",
    "https_proxy",
    "no_proxy",
    "CLAUDE_CONFIG_DIR",
    "APPDATA",
    "LOCALAPPDATA",
    "PROGRAMDATA",
];

pub fn claude_env(
    max_output_tokens: Option<u64>,
    thinking_off: bool,
    catalog_path: Option<&Path>,
) -> BTreeMap<String, String> {
    let mut env = BTreeMap::new();
    for name in ENV_ALLOWLIST {
        if let Ok(value) = std::env::var(name) {
            env.insert((*name).to_string(), value);
        }
    }
    for (name, value) in std::env::vars() {
        if name.starts_with("LC_") {
            env.entry(name).or_insert(value);
        }
    }
    env.insert("CLAUDE_CODE_DISABLE_AUTO_MEMORY".into(), "1".into());
    env.insert(
        "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC".into(),
        "1".into(),
    );
    if let Some(max) = max_output_tokens {
        env.insert("CLAUDE_CODE_MAX_OUTPUT_TOKENS".into(), max.to_string());
    }
    // `MAX_THINKING_TOKENS=0` no longer turns thinking off (Opus 5.5 still thinks under it on
    // Claude Code 2.1.308); this switch does: no thinking block at all (measured 2026-10-25).
    if thinking_off {
        env.insert("CLAUDE_CODE_DISABLE_THINKING".into(), "1".into());
    }
    if let Some(path) = catalog_path {
        env.insert(
            "PI_CLAUDE_TOOL_CATALOG".into(),
            path.to_string_lossy().into_owned(),
        );
    }
    env
}

/// The daemon binary that will be spawned as the MCP proposal server.
pub fn thickapi_exe() -> std::io::Result<PathBuf> {
    let exe = std::env::current_exe()?;
    Ok(dunce::canonicalize(&exe).unwrap_or(exe))
}

/// Resolve `claude.exe` (never the `.cmd` shim) or the configured override.
pub fn resolve_claude(override_path: Option<&Path>) -> Option<PathBuf> {
    if let Some(path) = override_path {
        return path.is_file().then(|| path.to_path_buf());
    }
    let resolved = crate::adapters::resolve_cli("claude")?;
    #[cfg(windows)]
    {
        let ext = resolved
            .extension()
            .map(|e| e.to_string_lossy().to_ascii_lowercase())
            .unwrap_or_default();
        if ext == "cmd" || ext == "bat" {
            // The shim could not be resolved to its target; refuse rather than spawn cmd.exe.
            return None;
        }
    }
    Some(resolved)
}

pub struct Spawned {
    pub child: Child,
    pub job: JobObject,
    pub pid: u32,
}

/// Launch Claude with `cwd` = the private directory, piped stdio, hidden window, in a job.
pub fn spawn(
    program: &Path,
    cwd: &Path,
    args: &[String],
    env: &BTreeMap<String, String>,
) -> std::io::Result<Spawned> {
    let job = JobObject::new()?;
    let mut cmd = Command::new(program);
    cmd.args(args)
        .env_clear()
        .envs(env)
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(windows)]
    {
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    let child = cmd.spawn()?;
    let pid = child
        .id()
        .ok_or_else(|| std::io::Error::other("child exited before pid was read"))?;
    #[cfg(windows)]
    {
        let handle = child
            .raw_handle()
            .ok_or_else(|| std::io::Error::other("child has no process handle"))?;
        // SAFETY: the tokio child is alive and owns handle for the duration of this call.
        unsafe { job.assign_handle(handle as _)? };
    }
    #[cfg(not(windows))]
    {
        let _ = &job;
    }
    Ok(Spawned { child, job, pid })
}

/// Kill the child tree. Returns `true` when the job object did it (exit code == `KNOWN_EXIT`),
/// `false` when the taskkill/SIGKILL fallback was used.
pub fn terminate(spawned: &mut Spawned) -> bool {
    if spawned.job.terminate(KNOWN_EXIT).is_ok() {
        return true;
    }
    crate::process::kill_process_tree(spawned.pid);
    let _ = spawned.child.start_kill();
    false
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    #[test]
    fn argv_has_all_reference_flags_in_order() {
        let args = claude_args(
            EMPTY_MCP,
            "claude-sonnet-4-6",
            Some(Effort::Medium),
            Path::new("C:/p/system-prompt.txt"),
        );
        let expected_prefix = [
            "-p",
            "--setting-sources",
            "",
            "--settings",
            SETTINGS,
            "--disable-slash-commands",
            "--strict-mcp-config",
            "--permission-mode",
            "dontAsk",
            "--no-chrome",
            "--no-session-persistence",
            "--prompt-suggestions",
            "false",
            "--mcp-config",
            EMPTY_MCP,
            "--tools",
            "",
            "--model",
            "claude-sonnet-4-6",
            "--effort",
            "medium",
            "--thinking-display",
            "summarized",
            "--input-format",
            "stream-json",
            "--output-format",
            "stream-json",
            "--include-partial-messages",
            "--verbose",
            "--system-prompt-file",
        ];
        assert_eq!(&args[..expected_prefix.len()], &expected_prefix[..]);
        assert_eq!(args.len(), expected_prefix.len() + 1);
        assert!(args.last().unwrap().ends_with("system-prompt.txt"));
    }

    #[test]
    fn effort_ladder_per_spec() {
        assert_eq!(effort_flag(None), None);
        assert_eq!(effort_flag(Some(Effort::Off)), None);
        assert_eq!(effort_flag(Some(Effort::Minimal)), None);
        assert_eq!(effort_flag(Some(Effort::Low)), Some("low"));
        assert_eq!(effort_flag(Some(Effort::XHigh)), Some("xhigh"));
        assert_eq!(effort_flag(Some(Effort::Max)), Some("max"));
        assert_eq!(wire_params(None), None);
        assert_eq!(
            wire_params(Some(Effort::Minimal)).as_deref(),
            Some("thinking=disabled")
        );
        assert_eq!(
            wire_params(Some(Effort::Max)).as_deref(),
            Some("effort=max")
        );
        assert!(thinking_off(Some(Effort::Off)));
        assert!(!thinking_off(Some(Effort::Low)));
        let args = claude_args(EMPTY_MCP, "m", None, Path::new("x"));
        assert!(!args.iter().any(|a| a == "--effort"));
        assert!(args
            .windows(2)
            .any(|w| w == ["--thinking-display", "summarized"]));
        let off = claude_args(EMPTY_MCP, "m", Some(Effort::Off), Path::new("x"));
        assert!(!off.iter().any(|a| a == "--thinking-display"));
    }

    #[test]
    fn env_is_allowlisted_and_pinned() {
        // SAFETY: test-local; the env lock pattern from env.rs is not needed for a set/remove of a unique key.
        unsafe {
            std::env::set_var("ANTHROPIC_API_KEY", "leak");
            std::env::set_var("THICKAPI_BRIDGE_TEST_JUNK", "leak");
        }
        let env = claude_env(Some(4096), true, Some(Path::new("C:/p/tools.json")));
        unsafe {
            std::env::remove_var("ANTHROPIC_API_KEY");
            std::env::remove_var("THICKAPI_BRIDGE_TEST_JUNK");
        }
        assert!(!env.contains_key("ANTHROPIC_API_KEY"));
        assert!(!env.contains_key("THICKAPI_BRIDGE_TEST_JUNK"));
        assert!(env.contains_key("PATH"));
        assert_eq!(env["CLAUDE_CODE_DISABLE_AUTO_MEMORY"], "1");
        assert_eq!(env["CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC"], "1");
        assert_eq!(env["CLAUDE_CODE_MAX_OUTPUT_TOKENS"], "4096");
        assert_eq!(env["CLAUDE_CODE_DISABLE_THINKING"], "1");
        assert!(!env.contains_key("MAX_THINKING_TOKENS"));
        assert!(env["PI_CLAUDE_TOOL_CATALOG"].ends_with("tools.json"));
        let env = claude_env(None, false, None);
        assert!(!env.contains_key("CLAUDE_CODE_DISABLE_THINKING"));
        assert!(!env.contains_key("CLAUDE_CODE_MAX_OUTPUT_TOKENS"));
    }

    #[test]
    fn mcp_config_points_at_thickapi_mcp_proposal() {
        let paths = McpPaths {
            catalog: Path::new("C:/p/tools.json"),
            ready: Path::new("C:/p/mcp-ready"),
            violation: Path::new("C:/p/mcp-execution-attempt"),
        };
        let text = mcp_config_json(Path::new("C:/bin/thickapi.exe"), Some(&paths));
        let v: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v["mcpServers"]["pi"]["command"], "C:/bin/thickapi.exe");
        assert_eq!(v["mcpServers"]["pi"]["args"][0], "mcp-proposal");
        assert_eq!(v["mcpServers"]["pi"]["args"][2], "C:/p/tools.json");
        assert_eq!(mcp_config_json(Path::new("x"), None), EMPTY_MCP);
    }
}
