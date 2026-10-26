use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use super::legacy::{self, LegacyKey};
use crate::types::BackendId;

/// `router.yaml`. Every struct under it refuses a key it does not know (audit D8); keys from
/// before the `thin.vendors` regroup are rewritten first ([`RouterConfig::from_yaml`]).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub struct RouterConfig {
    pub listen: String,
    pub data_dir: PathBuf,
    #[serde(default)]
    pub lan: bool,
    #[serde(default = "default_timeout_seconds")]
    pub default_timeout_seconds: u64,
    #[serde(default = "default_queue_timeout")]
    pub queue_timeout_seconds: u64,
    #[serde(default = "default_max_queue_depth")]
    pub max_queue_depth: u32,
    #[serde(default = "default_max_paid")]
    pub max_paid_usd_per_day: f64,
    #[serde(default = "default_drain_seconds")]
    pub drain_seconds: u64,
    #[serde(default)]
    pub refuse_paths: Vec<PathBuf>,
    /// Idle stall per pool (`claude`, `grok`, `agy`, `cursor`, `openrouter`), in seconds. The
    /// engine kills a child that has written nothing to stdout or stderr for this long. Unset
    /// pools take the per-backend default in `engine::default_idle_stall_seconds` (SPEC §5).
    #[serde(default)]
    pub idle_stall_seconds: BTreeMap<String, u64>,
    /// Allowed CORS origins for the thin routes (SPEC §19.4). Independent of `lan`.
    #[serde(default)]
    pub cors_origins: Vec<String>,
    /// Claude OAuth-direct policy (SPEC §19.8.1).
    #[serde(default)]
    pub claude_oauth: ClaudeOauthConfig,
    /// Thin-surface operational knobs (SPEC §19.9).
    #[serde(default)]
    pub thin: ThinConfig,
}

impl Default for RouterConfig {
    fn default() -> Self {
        Self {
            listen: "127.0.0.1:8787".into(),
            data_dir: PathBuf::from("~/.thickapi"),
            lan: false,
            default_timeout_seconds: default_timeout_seconds(),
            queue_timeout_seconds: default_queue_timeout(),
            max_queue_depth: default_max_queue_depth(),
            max_paid_usd_per_day: default_max_paid(),
            drain_seconds: default_drain_seconds(),
            refuse_paths: vec![],
            idle_stall_seconds: BTreeMap::new(),
            cors_origins: vec![],
            claude_oauth: ClaudeOauthConfig::default(),
            thin: ThinConfig::default(),
        }
    }
}

impl RouterConfig {
    /// Parse `router.yaml`, accepting the flat pre-C5 `thin:` keys and the H9-deleted
    /// `measured_versions` (SPEC §10.1): they are rewritten to the current shape and returned,
    /// one [`LegacyKey`] each, for the caller to [`LegacyKey::warn`] about. Any other unknown
    /// key is an error naming it.
    pub fn from_yaml(text: &str) -> anyhow::Result<(Self, Vec<LegacyKey>)> {
        let mut doc: serde_yaml::Value = serde_yaml::from_str(text)?;
        let found = legacy::migrate(&mut doc).map_err(anyhow::Error::msg)?;
        // Without a rewrite, parse the text itself: its errors carry line numbers.
        let config = if found.is_empty() {
            serde_yaml::from_str(text)?
        } else {
            serde_yaml::from_value(doc)?
        };
        Ok((config, found))
    }

    /// `lan: false` must not bind a non-loopback host (SPEC §1.6 LAN row, §19.10; D3 deliverable
    /// 5): `listen`'s host has to be `127.0.0.1`, `localhost` or `::1` unless `lan: true`. Called
    /// from `config::load_config` (`serve`, `doctor`, `sync-pi`, `turn`, … — every entry point
    /// that loads `router.yaml`), so a `0.0.0.0` / `::` bind without `lan: true` fails at load,
    /// before `serve` ever binds the socket.
    pub fn validate_lan(&self) -> Result<(), String> {
        if self.lan {
            return Ok(());
        }
        let (host, _port) = split_listen(&self.listen);
        if is_loopback_host(host) {
            Ok(())
        } else {
            Err(format!(
                "router.yaml: listen host '{host}' is not loopback (127.0.0.1, localhost, ::1); \
                 set lan: true to bind it, or change listen (listen={}, lan={})",
                self.listen, self.lan
            ))
        }
    }
}

/// Split a `host:port` listen string into `(host, port)`, stripping IPv6 brackets. No colon
/// (malformed config) returns the whole string as host with an empty port.
pub fn split_listen(listen: &str) -> (&str, &str) {
    match listen.rsplit_once(':') {
        Some((host, port)) => (host.trim_start_matches('[').trim_end_matches(']'), port),
        None => (listen, ""),
    }
}

/// SPEC §1.6 LAN row: the loopback hosts a `lan: false` bind may use.
pub fn is_loopback_host(host: &str) -> bool {
    matches!(host, "127.0.0.1" | "localhost" | "::1")
}

/// SPEC §19.10 `sync-pi`: a wildcard bind Pi (Windows) cannot connect to, rewritten to loopback.
pub fn is_unspecified_host(host: &str) -> bool {
    matches!(host, "0.0.0.0" | "::")
}

/// `claude_oauth.mode`: `off` (default) | `prefer` | `only` (SPEC §19.8.1, §19.15).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ClaudeOauthMode {
    /// Try OAuth-direct; fall back to the CLI bridge when Anthropic rejects the token.
    Prefer,
    /// Never use the token directly; every Claude row goes through the bridge. The default
    /// since 2026-09-28: Anthropic bills a direct OAuth request as a third-party app (extra
    /// usage), so Claude rows draw on the plan only through the bridge (#137).
    #[default]
    Off,
    /// Fail instead of falling back (for measuring).
    Only,
}

/// Thinking budgets (tokens) for the OAuth-direct Claude adapter (SPEC §19.5).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub struct ClaudeThinkingBudgets {
    #[serde(default = "default_budget_low")]
    pub low: u32,
    #[serde(default = "default_budget_medium")]
    pub medium: u32,
    #[serde(default = "default_budget_high")]
    pub high: u32,
    #[serde(default = "default_budget_xhigh")]
    pub xhigh: u32,
}

impl Default for ClaudeThinkingBudgets {
    fn default() -> Self {
        Self {
            low: default_budget_low(),
            medium: default_budget_medium(),
            high: default_budget_high(),
            xhigh: default_budget_xhigh(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub struct ClaudeOauthConfig {
    #[serde(default)]
    pub mode: ClaudeOauthMode,
    /// Cooldown before probing OAuth-direct again after a block (hours).
    #[serde(default = "default_retry_after_hours")]
    pub retry_after_hours: u64,
    #[serde(default)]
    pub budgets: ClaudeThinkingBudgets,
    /// Model for the `doctor` bridge handshake. Haiku has never been blocked so far.
    #[serde(default = "default_probe_model")]
    pub probe_model: String,
    /// Background direct-path probe (SPEC §19.15 R2).
    #[serde(default)]
    pub probe: ClaudeProbeConfig,
    /// Bridge → OAuth cross-fallback (SPEC §19.15 R3): a bridge fault before `Started` under
    /// `prefer` runs the direct path once when the token is live and the state is not blocked.
    #[serde(default = "default_true")]
    pub cross_fallback: bool,
    /// Anthropic API base (default `https://api.anthropic.com`); tests and `capture` point it at a stand-in.
    #[serde(default)]
    pub base_url: Option<String>,
    /// Credentials file (default `$CLAUDE_CONFIG_DIR/.credentials.json` or `~/.claude/.credentials.json`).
    #[serde(default)]
    pub credentials_path: Option<PathBuf>,
}

impl Default for ClaudeOauthConfig {
    fn default() -> Self {
        Self {
            mode: ClaudeOauthMode::default(),
            retry_after_hours: default_retry_after_hours(),
            budgets: ClaudeThinkingBudgets::default(),
            probe_model: default_probe_model(),
            probe: ClaudeProbeConfig::default(),
            cross_fallback: true,
            base_url: None,
            credentials_path: None,
        }
    }
}

/// `claude_oauth.probe` (SPEC §19.15 R2): a `serve` task owns the direct-path state; user
/// requests never act as the probe while it is enabled.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub struct ClaudeProbeConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Non-Haiku, or the probe cannot see the block that matters.
    #[serde(default = "default_probe_direct_model")]
    pub model: String,
    /// Probe on `serve` start when the persisted state is not blocked.
    #[serde(default = "default_true")]
    pub on_start: bool,
    /// Retry after a probe that neither succeeded nor classified as a block (minutes).
    #[serde(default = "default_probe_retry_minutes")]
    pub retry_minutes: u64,
}

impl Default for ClaudeProbeConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            model: default_probe_direct_model(),
            on_start: true,
            retry_minutes: default_probe_retry_minutes(),
        }
    }
}

/// `thin:` — what every thin request shares, plus one block per vendor (`thin.vendors.<id>`,
/// SPEC §10.1, §19.8). A vendor's knobs live only in its own block, so adding a vendor adds a
/// block and touches nothing else (R22 C5). Keys from before the regroup are rewritten at load
/// by [`super::legacy`], with one `warn` each.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "snake_case", deny_unknown_fields)]
pub struct ThinConfig {
    /// Write per-request trace artifacts under `tmp/<req>/trace/` (off by default).
    pub trace: bool,
    /// Maximum retained trace directories.
    pub trace_keep: u32,
    /// Daily rolling logs (`thickapi.<date>.log`). Default `~/.thickapi/logs`.
    pub log_dir: std::path::PathBuf,
    /// Delete rolling logs older than this many days (`reap`, SPEC §19.15 R5).
    pub log_keep_days: u64,
    /// In-flight cap per OAuth-direct vendor.
    pub oauth_in_flight: u32,
    /// The route's own limit on the wait for `Started`, whatever the adapter does: no request
    /// may sit without an HTTP status for longer. [`ThinConfig::validate`] refuses a value
    /// below any vendor's first-event budget or the bridge's MCP ready wait; token refresh and
    /// bridge spawn come on top of those. `0` = off.
    pub start_deadline_seconds: u64,
    /// Bound on a non-streaming vendor exchange: the Claude probe, `list_models`, agy's
    /// `loadCodeAssist`, and reading the body of a rejected response (SPEC §19.8). `0` = off.
    pub reply_timeout_seconds: u64,
    /// SSE comment sent when a stream has been silent this long, so a proxy or client read
    /// timeout does not kill a turn that is only thinking. `0` = off.
    pub keepalive_seconds: u64,
    /// Largest `/v1/chat/completions` body, in MiB. axum's 2 MiB default is smaller than one
    /// pasted screenshot once it is base64 inside a transcript. `0` = off (no limit).
    pub max_body_mib: u64,
    /// One block per vendor.
    pub vendors: Vendors,
}

impl Default for ThinConfig {
    fn default() -> Self {
        Self {
            trace: false,
            trace_keep: default_trace_keep(),
            log_dir: default_log_dir(),
            log_keep_days: default_log_keep_days(),
            oauth_in_flight: default_oauth_in_flight(),
            start_deadline_seconds: default_start_deadline(),
            reply_timeout_seconds: default_reply_timeout(),
            keepalive_seconds: default_keepalive(),
            max_body_mib: default_max_body_mib(),
            vendors: Vendors::default(),
        }
    }
}

impl ThinConfig {
    /// Budgets that contradict each other are a load error, not a silent override (audit R9,
    /// #129): a first-event budget above `start_deadline_seconds` would never fire, because
    /// the route drops the request first. `0` on either side means off and is not compared.
    pub fn validate(&self) -> Result<(), String> {
        if self.vendors.cursor.builtin_loop_limit == Some(1) {
            return Err(
                "thin.vendors.cursor.builtin_loop_limit must be 0 (off) or at least 2: a limit                  of 1 would refuse every tool turn"
                    .to_string(),
            );
        }
        if self.start_deadline_seconds == 0 {
            return Ok(());
        }
        let dialect = &self.vendors.claude.bridge.dialect;
        if dialect != crate::thin::dialect::MODE_CLAUDE_CODE
            && dialect != crate::thin::dialect::MODE_OFF
        {
            return Err(format!(
                "thin.vendors.claude.bridge.dialect must be claude_code or off, not {dialect}"
            ));
        }
        for (name, seconds) in self.vendors.first_event_budgets() {
            if seconds > self.start_deadline_seconds {
                return Err(format!(
                    "thin.start_deadline_seconds ({}) is below thin.{name} ({seconds}): the route \
                     would drop the request before that budget could fire",
                    self.start_deadline_seconds
                ));
            }
        }
        Ok(())
    }

    /// After `Started`, if the serving vendor's own idle budget is off, the route still ends a
    /// silent turn after [`DEFAULT_OAUTH_IDLE_SECONDS`]. Keep-alive comments do not reset this
    /// clock. Cursor counts as off when either of its after-output budgets is.
    pub fn route_idle(&self, backend: BackendId) -> Option<std::time::Duration> {
        let v = &self.vendors;
        let off = match backend {
            BackendId::ClaudeCode => v.claude.budgets.idle_seconds == 0,
            BackendId::Grok => v.grok.budgets.idle_seconds == 0,
            BackendId::Agy => v.agy.budgets.idle_seconds == 0,
            BackendId::Codex => v.codex.budgets.idle_seconds == 0,
            BackendId::Cursor => {
                v.cursor.budgets.idle_seconds == 0
                    || v.cursor.budgets.idle_after_text_with_tools_seconds == 0
            }
            BackendId::Pi => false,
        };
        off.then_some(std::time::Duration::from_secs(DEFAULT_OAUTH_IDLE_SECONDS))
    }
}

/// `thin.vendors`: one block per thin vendor.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "snake_case", deny_unknown_fields)]
pub struct Vendors {
    pub claude: ClaudeCfg,
    pub grok: GrokCfg,
    pub agy: AgyCfg,
    pub cursor: CursorCfg,
    pub codex: CodexCfg,
}

impl Vendors {
    /// Every budget that runs before a request's first event, named by its path under `thin.`,
    /// in seconds (`0` = off). [`ThinConfig::validate`] holds each to `start_deadline_seconds`.
    pub fn first_event_budgets(&self) -> [(&'static str, u64); 6] {
        [
            (
                "vendors.claude.budgets.first_event_seconds",
                self.claude.budgets.first_event_seconds,
            ),
            (
                "vendors.grok.budgets.first_event_seconds",
                self.grok.budgets.first_event_seconds,
            ),
            (
                "vendors.agy.budgets.first_event_seconds",
                self.agy.budgets.first_event_seconds,
            ),
            (
                "vendors.cursor.budgets.first_output_seconds",
                self.cursor.budgets.first_output_seconds,
            ),
            (
                "vendors.codex.budgets.first_event_seconds",
                self.codex.budgets.first_event_seconds,
            ),
            (
                "vendors.claude.bridge.mcp_ready_timeout_ms (as seconds)",
                self.claude.bridge.mcp_ready_timeout_ms.div_ceil(1000),
            ),
        ]
    }
}

/// An OAuth-direct stream's quiet-event budgets (SPEC §19.8 **Stream budgets**): the longest
/// quiet spell before the first event, and between events after it. Quiet means no
/// `ThinEvent` — vendor keepalives do not count. `0` turns a budget off; the route still bounds
/// a started turn with [`DEFAULT_OAUTH_IDLE_SECONDS`], because SSE keep-alive would otherwise
/// hold the client forever (SPEC §19.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "snake_case", deny_unknown_fields)]
pub struct OauthBudgets {
    pub first_event_seconds: u64,
    pub idle_seconds: u64,
}

impl Default for OauthBudgets {
    fn default() -> Self {
        Self {
            first_event_seconds: default_oauth_first_event(),
            idle_seconds: default_oauth_idle(),
        }
    }
}

/// `thin.vendors.claude`: the CLI bridge, and the direct path's stream budgets. The direct
/// path's policy, token file and base URL are `claude_oauth` (SPEC §19.8.1).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "snake_case", deny_unknown_fields)]
pub struct ClaudeCfg {
    /// Explicit `claude.exe` for the bridge (default: resolved from `PATH`); tests point it at
    /// a stand-in.
    pub path: Option<std::path::PathBuf>,
    pub bridge: ClaudeBridgeCfg,
    pub budgets: OauthBudgets,
}

/// `thin.vendors.claude.bridge` (SPEC §10.1, §19.2, §19.7).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "snake_case", deny_unknown_fields)]
pub struct ClaudeBridgeCfg {
    /// Cap on concurrent bridge spawns (`claude.exe` + `mcp-proposal`).
    pub max_concurrent: u32,
    /// Total per-request timeout (seconds).
    pub timeout_seconds: u64,
    /// Idle stall for the bridge child (seconds).
    pub idle_seconds: u64,
    /// How long to wait for the proposal MCP server's ready marker (milliseconds).
    pub mcp_ready_timeout_ms: u64,
    /// How long to keep draining stdout after a tool-use handoff kill, and how long any
    /// terminate path waits for the child to exit before giving up on it (seconds; at least
    /// 1, like the other bridge timeouts).
    pub handoff_drain_seconds: u64,
    /// Warn-and-continue lists for Claude init assertions (`protocol.rs` only).
    pub tolerate: BridgeTolerate,
    /// `claude_code` (default) shows Pi tools under Claude Code names. `off` is today's
    /// pass-through, byte for byte (SPEC §19.7 Dialect).
    pub dialect: String,
}

impl Default for ClaudeBridgeCfg {
    fn default() -> Self {
        Self {
            max_concurrent: default_bridge_max_concurrent(),
            timeout_seconds: default_bridge_timeout(),
            idle_seconds: default_bridge_idle(),
            mcp_ready_timeout_ms: default_mcp_ready_ms(),
            handoff_drain_seconds: default_handoff_drain(),
            tolerate: BridgeTolerate::default(),
            dialect: crate::thin::dialect::MODE_CLAUDE_CODE.to_string(),
        }
    }
}

/// Warn-and-continue lists for Claude init assertions in `protocol.rs` only (SPEC §19.7 Init validation).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "snake_case", deny_unknown_fields)]
pub struct BridgeTolerate {
    pub extra_tools: Vec<String>,
    pub nonempty_inventories: Vec<String>,
}

/// `thin.vendors.grok`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "snake_case", deny_unknown_fields)]
pub struct GrokCfg {
    /// xAI API base (default `https://api.x.ai`); tests point it at a stand-in.
    pub base_url: Option<String>,
    /// Credentials file (default `~/.grok/auth.json`).
    pub credentials_path: Option<std::path::PathBuf>,
    /// Override for `x-grok-client-version` (defaults to `grok --version`).
    pub client_version: Option<String>,
    pub budgets: GrokBudgets,
}

/// Grok's budgets: [`OauthBudgets`] plus the tool-call floor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "snake_case", deny_unknown_fields)]
pub struct GrokBudgets {
    pub first_event_seconds: u64,
    pub idle_seconds: u64,
    /// The quiet-event floor on a request that offers tools (SPEC §19.8 `grok.rs`). xAI streams
    /// text and reasoning token by token but delivers a tool call whole, so a long `write` is
    /// silent from the end of the reasoning until the call lands (measured 2026-09-22,
    /// grok-4.7). Raises `idle_seconds` — not the first-event budget — to at least this on such
    /// a request; `0` = off.
    pub tool_call_floor_seconds: u64,
}

impl Default for GrokBudgets {
    fn default() -> Self {
        Self {
            first_event_seconds: default_oauth_first_event(),
            idle_seconds: default_oauth_idle(),
            tool_call_floor_seconds: default_grok_tool_call(),
        }
    }
}

/// `thin.vendors.agy` (Antigravity).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "snake_case", deny_unknown_fields)]
pub struct AgyCfg {
    /// Code Assist API base (default the host `agy` itself calls,
    /// `https://daily-cloudcode-pa.googleapis.com`); tests point it at a stand-in.
    pub base_url: Option<String>,
    /// Credentials file. Default: the `gemini:antigravity` Windows credential, else
    /// `~/.gemini/antigravity-cli/antigravity-oauth-token`.
    pub credentials_path: Option<std::path::PathBuf>,
    /// Override for the `antigravity/<version>` user agent (defaults to `agy --version`).
    pub client_version: Option<String>,
    /// Code Assist project id; default discovered once via `loadCodeAssist`.
    pub project: Option<String>,
    pub budgets: OauthBudgets,
}

/// `thin.vendors.codex` (ChatGPT-plan Codex, the Responses API at
/// `chatgpt.com/backend-api/codex`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "snake_case", deny_unknown_fields)]
pub struct CodexCfg {
    /// ChatGPT backend base (default `https://chatgpt.com/backend-api`); tests point it at a
    /// stand-in.
    pub base_url: Option<String>,
    /// Credentials file (default `~/.codex/auth.json`, or `$CODEX_HOME/auth.json`).
    pub credentials_path: Option<std::path::PathBuf>,
    /// Override for the `codex_cli_rs/<version>` user agent (defaults to `codex --version`).
    pub client_version: Option<String>,
    pub budgets: CodexBudgets,
}

/// Codex's budgets: [`OauthBudgets`] with a longer idle, the codex CLI's own stream idle
/// timeout (300 s). A high-effort turn reasons in silence before its summary streams.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "snake_case", deny_unknown_fields)]
pub struct CodexBudgets {
    pub first_event_seconds: u64,
    pub idle_seconds: u64,
}

impl Default for CodexBudgets {
    fn default() -> Self {
        Self {
            first_event_seconds: default_oauth_first_event(),
            idle_seconds: 300,
        }
    }
}

/// `thin.vendors.cursor`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "snake_case", deny_unknown_fields)]
pub struct CursorCfg {
    /// Agent service base (default `https://agentn.global.api5.cursor.sh`); tests point it at
    /// a stand-in. Must be https unless it is a loopback host.
    pub base_url: Option<String>,
    /// Credentials file (default `%APPDATA%\Cursor\auth.json` / `~/.cursor/auth.json`).
    pub credentials_path: Option<std::path::PathBuf>,
    /// Override for `x-cursor-client-version` (defaults to `cli-<cursor-agent --version>`).
    pub client_version: Option<String>,
    /// Working directory advertised to the agent service (default the daemon's cwd). The thin
    /// surface never executes Cursor's built-in tools against it — they are denied on the
    /// duplex; Pi tools stay on MCP `f2.f11`.
    pub cwd: Option<String>,
    /// `RequestContext.env.shell` advertised to the agent service (default `bash`). It names
    /// the client's shell tool — Pi's `bash`, Git bash on Windows — not the host OS default;
    /// set it only for a client whose shell tool is something else (SPEC §19.8, §10.1).
    pub shell: Option<String>,
    /// G3 loop guard: a request whose history ends in this many identical tool calls with
    /// identical results is refused `502 builtin_loop` before a Run opens (default 3; `0` =
    /// off; SPEC §19.8, §10.1).
    pub builtin_loop_limit: Option<u32>,
    pub budgets: CursorBudgetsCfg,
}

/// Cursor's own quiet-event budgets (SPEC §19.8), a three-way rule the decoder applies. Not
/// the OAuth `idle_seconds`: Cursor holds the stream open after an answer, so a 180 s idle
/// would stall Pi. `0` = off.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "snake_case", deny_unknown_fields)]
pub struct CursorBudgetsCfg {
    /// Quiet before any output → `vendor_down`.
    pub first_output_seconds: u64,
    /// Quiet after output on a request that offered no tools.
    pub idle_seconds: u64,
    /// Quiet after text on a request that offered tools. Safety net only (H1, #185):
    /// the turn ends on `f1.14 turn_ended` or a tool call. Spending this after text is
    /// `vendor_down` (`cursor went quiet mid-answer`), not `stop`. Default 300. `0` = off.
    pub idle_after_text_with_tools_seconds: u64,
    /// `cursor-agent --list-models` (`thickapi models check`).
    pub list_models_seconds: u64,
}

impl Default for CursorBudgetsCfg {
    fn default() -> Self {
        Self {
            first_output_seconds: default_cursor_first_output(),
            idle_seconds: default_cursor_idle(),
            idle_after_text_with_tools_seconds: default_cursor_idle_after_text_with_tools(),
            list_models_seconds: default_cursor_list_models(),
        }
    }
}

fn default_timeout_seconds() -> u64 {
    5400
}

fn default_queue_timeout() -> u64 {
    300
}

fn default_max_queue_depth() -> u32 {
    16
}

fn default_max_paid() -> f64 {
    5.0
}

fn default_drain_seconds() -> u64 {
    120
}

fn default_retry_after_hours() -> u64 {
    6
}

fn default_probe_model() -> String {
    "claude-haiku-4-5".to_string()
}

/// The newest non-Haiku row (`models check` flags this too when Anthropic ships a successor).
fn default_probe_direct_model() -> String {
    "claude-fable-5-1".to_string()
}

fn default_probe_retry_minutes() -> u64 {
    15
}

fn default_true() -> bool {
    true
}

fn default_budget_low() -> u32 {
    4_000
}

fn default_budget_medium() -> u32 {
    16_000
}

fn default_budget_high() -> u32 {
    32_000
}

fn default_budget_xhigh() -> u32 {
    64_000
}

fn default_trace_keep() -> u32 {
    20
}

fn default_log_dir() -> std::path::PathBuf {
    std::path::PathBuf::from("~/.thickapi/logs")
}

fn default_log_keep_days() -> u64 {
    14
}

fn default_oauth_in_flight() -> u32 {
    4
}

fn default_oauth_first_event() -> u64 {
    120
}

/// Default OAuth quiet-event budget (seconds). Also the route-level idle used when the
/// serving vendor's own idle budget is 0 (SPEC §19.4).
pub const DEFAULT_OAUTH_IDLE_SECONDS: u64 = 180;

fn default_oauth_idle() -> u64 {
    DEFAULT_OAUTH_IDLE_SECONDS
}

fn default_start_deadline() -> u64 {
    300
}

fn default_reply_timeout() -> u64 {
    30
}

fn default_keepalive() -> u64 {
    15
}

fn default_max_body_mib() -> u64 {
    32
}

fn default_bridge_max_concurrent() -> u32 {
    2
}

fn default_bridge_timeout() -> u64 {
    1800
}

fn default_bridge_idle() -> u64 {
    300
}

fn default_mcp_ready_ms() -> u64 {
    5_000
}

fn default_handoff_drain() -> u64 {
    10
}

fn default_cursor_first_output() -> u64 {
    90
}

fn default_cursor_idle() -> u64 {
    4
}

/// Above the longest gap measured on the F1 `quiet` set; 300 s is the floor (H1, #185).
fn default_cursor_idle_after_text_with_tools() -> u64 {
    300
}

fn default_cursor_list_models() -> u64 {
    60
}

/// Grok 4.7 generates a 32k-token tool call in well under this; a vendor that is truly
/// gone is still bounded.
fn default_grok_tool_call() -> u64 {
    900
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    /// D3 deliverable 5: `lan: false` refuses a non-loopback bind; `lan: true` allows it.
    #[test]
    fn lan_false_refuses_a_wildcard_bind() {
        let router = RouterConfig {
            listen: "0.0.0.0:18787".into(),
            lan: false,
            ..Default::default()
        };
        let err = router.validate_lan().unwrap_err();
        assert!(err.contains("listen"), "{err}");
        assert!(err.contains("lan"), "{err}");

        let lan_on = RouterConfig {
            lan: true,
            ..router
        };
        assert!(lan_on.validate_lan().is_ok());
    }

    #[test]
    fn lan_false_allows_loopback_hosts() {
        for listen in ["127.0.0.1:18787", "localhost:18787", "[::1]:18787"] {
            let router = RouterConfig {
                listen: listen.into(),
                lan: false,
                ..Default::default()
            };
            assert!(router.validate_lan().is_ok(), "{listen}");
        }
    }

    #[test]
    fn split_listen_strips_ipv6_brackets_and_finds_the_port() {
        assert_eq!(split_listen("127.0.0.1:8787"), ("127.0.0.1", "8787"));
        assert_eq!(split_listen("[::1]:8787"), ("::1", "8787"));
        assert_eq!(split_listen("[::]:18787"), ("::", "18787"));
        assert!(is_unspecified_host("0.0.0.0"));
        assert!(is_unspecified_host("::"));
        assert!(!is_unspecified_host("127.0.0.1"));
    }

    fn fixture(name: &str) -> String {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../fixtures/thin")
            .join(name);
        std::fs::read_to_string(path).expect("fixture")
    }

    fn keys(found: &[LegacyKey]) -> Vec<&str> {
        found.iter().map(|k| k.key.as_str()).collect()
    }

    /// C5 (a): the pre-regroup example, every flat key live, loads to the same config as the
    /// same values in the `thin.vendors` shape, with one legacy warning per key.
    #[test]
    fn the_legacy_example_loads_to_the_same_config_as_the_new_shape() {
        let (legacy, found) = RouterConfig::from_yaml(&fixture("router-legacy.yaml")).unwrap();
        let (vendors, none) = RouterConfig::from_yaml(&fixture("router-vendors.yaml")).unwrap();
        assert!(none.is_empty(), "{none:?}");
        assert_eq!(legacy, vendors);
        let mut expected: Vec<String> = legacy::THIN_ALIASES
            .iter()
            .map(|(k, _)| format!("thin.{k}"))
            .collect();
        expected.insert(0, "measured_versions".into());
        assert_eq!(keys(&found), expected);
        assert!(found
            .iter()
            .all(|k| k.action != legacy::LegacyAction::Shadowed));
        assert!(vendors.thin.validate().is_ok());
        // Non-default values, so the equality above proves each key landed.
        let v = &vendors.thin.vendors;
        assert_eq!(v.cursor.budgets.first_output_seconds, 80);
        assert_eq!(v.grok.budgets.tool_call_floor_seconds, 800);
        assert_eq!(v.agy.budgets.idle_seconds, 170);
        assert_eq!(v.claude.bridge.max_concurrent, 3);
        assert_eq!(v.claude.bridge.tolerate.extra_tools, vec!["Foo"]);
    }

    /// G4: `thin.vendors.cursor.shell` is a known key (default unset = `bash`); a typo is
    /// still a load error.
    #[test]
    fn cursor_shell_is_a_known_key() {
        let head = "listen: \"127.0.0.1:18787\"
data_dir: \"~/.thickapi\"
thin:
  vendors:
    cursor:
";
        let (cfg, found) = RouterConfig::from_yaml(&format!(
            "{head}      shell: powershell
"
        ))
        .unwrap();
        assert!(found.is_empty(), "{found:?}");
        assert_eq!(cfg.thin.vendors.cursor.shell.as_deref(), Some("powershell"));
        let (cfg, _) = RouterConfig::from_yaml(head.trim_end_matches(
            "    cursor:
",
        ))
        .unwrap();
        assert_eq!(cfg.thin.vendors.cursor.shell, None);
        assert!(RouterConfig::from_yaml(&format!(
            "{head}      shel: x
"
        ))
        .is_err());
    }

    /// G3: `thin.vendors.cursor.builtin_loop_limit` is a known key (default unset = 3); a typo
    /// is still a load error.
    #[test]
    fn cursor_builtin_loop_limit_is_a_known_key() {
        let head = "listen: \"127.0.0.1:18787\"
data_dir: \"~/.thickapi\"
thin:
  vendors:
    cursor:
";
        let (cfg, found) = RouterConfig::from_yaml(&format!(
            "{head}      builtin_loop_limit: 0
"
        ))
        .unwrap();
        assert!(found.is_empty(), "{found:?}");
        assert_eq!(cfg.thin.vendors.cursor.builtin_loop_limit, Some(0));
        let (cfg, _) = RouterConfig::from_yaml(head.trim_end_matches(
            "    cursor:
",
        ))
        .unwrap();
        assert_eq!(cfg.thin.vendors.cursor.builtin_loop_limit, None);
        assert!(RouterConfig::from_yaml(&format!(
            "{head}      builtin_loop_limt: 2
"
        ))
        .is_err());
    }

    /// G3: `builtin_loop_limit: 1` is a load error, not a silent 2.
    #[test]
    fn cursor_builtin_loop_limit_of_one_is_refused() {
        let yaml = |n: u32| {
            format!(
                "listen: \"127.0.0.1:18787\"
data_dir: \"~/.thickapi\"
thin:
  vendors:
    cursor:
      builtin_loop_limit: {n}
"
            )
        };
        let (cfg, _) = RouterConfig::from_yaml(&yaml(1)).unwrap();
        let err = cfg.thin.validate().unwrap_err();
        assert!(err.contains("must be 0 (off) or at least 2"), "{err}");
        for ok in [0, 2, 3] {
            let (cfg, _) = RouterConfig::from_yaml(&yaml(ok)).unwrap();
            assert!(cfg.thin.validate().is_ok(), "{ok}");
        }
    }

    /// C5 (b): the live file's shape — `thin.cursor_cwd` and the H9-deleted `measured_versions`
    /// — loads, with exactly those two warnings.
    #[test]
    fn the_live_file_shape_loads_with_two_warnings() {
        let text = "listen: \"127.0.0.1:18787\"\ndata_dir: \"~/.thickapi\"\nlan: false\nrefuse_paths: []\nmeasured_versions:\n  claude: \"2.1.273\"\n  grok: \"1.0.30\"\nthin:\n  cursor_cwd: \"C:/work/thickapi\"\n";
        let (cfg, found) = RouterConfig::from_yaml(text).unwrap();
        assert_eq!(keys(&found), vec!["measured_versions", "thin.cursor_cwd"]);
        assert_eq!(found[0].action, legacy::LegacyAction::Removed);
        assert_eq!(found[1].new_key, "thin.vendors.cursor.cwd");
        assert_eq!(
            cfg.thin.vendors.cursor.cwd.as_deref(),
            Some("C:/work/thickapi")
        );
        assert!(cfg.validate_lan().is_ok());
        assert!(cfg.thin.validate().is_ok());
    }

    /// C5 (c), audit D8: a key that is neither current nor legacy is an error naming it, at
    /// the top level, under `thin`, and under a vendor — with or without a legacy key beside it.
    #[test]
    fn an_unknown_key_is_refused_by_name() {
        let base = "listen: \"127.0.0.1:8787\"\ndata_dir: \"~/.thickapi\"\n";
        for (extra, key) in [
            ("lsiten: x\n", "lsiten"),
            ("thin:\n  keepalive_secs: 5\n", "keepalive_secs"),
            (
                "thin:\n  vendors:\n    grok:\n      tool_call_seconds: 5\n",
                "tool_call_seconds",
            ),
            (
                "thin:\n  cursor_cwd: a\n  vendors:\n    cursor:\n      budgets:\n        idle: 3\n",
                "idle",
            ),
            (
                "thin:\n  bridge:\n    tolerate: {}\n    max_concurrent: 2\n",
                "bridge",
            ),
            ("claude_oauth:\n  probe:\n    enable: false\n", "enable"),
        ] {
            let err = RouterConfig::from_yaml(&format!("{base}{extra}")).unwrap_err();
            let err = format!("{err:#}");
            assert!(err.contains(&format!("`{key}`")), "{extra}: {err}");
        }
    }

    /// C5 (d), #129: the load-time check reads every vendor's first-event budget.
    #[test]
    fn start_deadline_below_a_first_event_budget_is_refused() {
        assert!(ThinConfig::default().validate().is_ok());
        let (cfg, _) = RouterConfig::from_yaml(
            "listen: \"127.0.0.1:8787\"\ndata_dir: \"~/.thickapi\"\nthin:\n  start_deadline_seconds: 60\n  vendors:\n    claude: {budgets: {first_event_seconds: 30}}\n    grok: {budgets: {first_event_seconds: 30}}\n    agy: {budgets: {first_event_seconds: 30}}\n    cursor:\n      budgets:\n        first_output_seconds: 90\n",
        )
        .unwrap();
        let err = cfg.thin.validate().unwrap_err();
        assert!(
            err.contains("thin.vendors.cursor.budgets.first_output_seconds (90)"),
            "{err}"
        );

        let mut low = ThinConfig {
            start_deadline_seconds: 60,
            ..Default::default()
        };
        let err = low.validate().unwrap_err();
        assert!(
            err.contains("vendors.claude.budgets.first_event_seconds"),
            "{err}"
        );
        low.vendors.claude.budgets.first_event_seconds = 60;
        low.vendors.agy.budgets.first_event_seconds = 60;
        low.vendors.grok.budgets.first_event_seconds = 61;
        assert!(low
            .validate()
            .unwrap_err()
            .contains("vendors.grok.budgets.first_event_seconds"));
        low.vendors.grok.budgets.first_event_seconds = 0;
        low.vendors.cursor.budgets.first_output_seconds = 0;
        low.vendors.codex.budgets.first_event_seconds = 0;
        assert!(low.validate().is_ok(), "a budget of 0 is off");
        low.vendors.claude.bridge.mcp_ready_timeout_ms = 60_001;
        assert!(low.validate().unwrap_err().contains("mcp_ready_timeout_ms"));

        let mut off = ThinConfig {
            start_deadline_seconds: 0,
            ..Default::default()
        };
        off.vendors.claude.budgets.first_event_seconds = 10_000;
        assert!(off.validate().is_ok(), "a deadline of 0 is off");
    }

    /// The shipped example is the new shape, and so is its commented `thin:` block once
    /// uncommented.
    #[test]
    fn the_example_and_its_commented_thin_block_are_the_new_shape() {
        let path =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../configs/router.yaml.example");
        let text = std::fs::read_to_string(path).unwrap();
        let (cfg, found) = RouterConfig::from_yaml(&text).unwrap();
        assert!(found.is_empty(), "{found:?}");
        assert_eq!(cfg.thin, ThinConfig::default());

        let mut in_thin = false;
        let uncommented: String = text
            .lines()
            .map(|line| {
                if line.starts_with("# thin:") {
                    in_thin = true;
                }
                match line.strip_prefix("# ") {
                    Some(rest) if in_thin => format!("{rest}\n"),
                    _ => format!("{line}\n"),
                }
            })
            .collect();
        let (cfg, found) = RouterConfig::from_yaml(&uncommented).unwrap();
        assert!(found.is_empty(), "{found:?}");
        assert!(cfg.thin.validate().is_ok());
        let (d, v) = (ThinConfig::default().vendors, cfg.thin.vendors);
        assert_eq!(v.claude.budgets, d.claude.budgets);
        assert_eq!(v.grok.budgets, d.grok.budgets);
        assert_eq!(v.agy.budgets, d.agy.budgets);
        assert_eq!(v.cursor.budgets, d.cursor.budgets);
        assert_eq!(v.claude.bridge, d.claude.bridge);
    }

    #[test]
    fn claude_oauth_defaults_to_off() {
        let cfg: RouterConfig =
            serde_yaml::from_str("listen: \"127.0.0.1:8787\"\ndata_dir: \"~/.thickapi\"\n")
                .expect("parse");
        assert_eq!(cfg.claude_oauth.mode, ClaudeOauthMode::Off);
        assert_eq!(cfg.claude_oauth.retry_after_hours, 6);
        assert_eq!(cfg.claude_oauth.budgets.medium, 16_000);
        assert!(cfg.claude_oauth.probe.enabled);
        assert!(cfg.claude_oauth.probe.on_start);
        assert_eq!(cfg.claude_oauth.probe.model, "claude-fable-5-1");
        assert_eq!(cfg.claude_oauth.probe.retry_minutes, 15);
        assert!(cfg.claude_oauth.cross_fallback);
        let t = &cfg.thin;
        assert!(t.vendors.claude.path.is_none());
        assert_eq!(t.log_dir, PathBuf::from("~/.thickapi/logs"));
        assert_eq!(t.log_keep_days, 14);
        assert_eq!(t.trace_keep, 20);
        assert_eq!(t.oauth_in_flight, 4);
        assert_eq!(t.start_deadline_seconds, 300);
        assert_eq!(t.reply_timeout_seconds, 30);
        assert_eq!(t.keepalive_seconds, 15);
        assert_eq!(t.max_body_mib, 32);
        let b = &t.vendors.claude.bridge;
        assert_eq!(
            (b.max_concurrent, b.timeout_seconds, b.idle_seconds),
            (2, 1800, 300)
        );
        assert_eq!(
            (b.mcp_ready_timeout_ms, b.handoff_drain_seconds),
            (5_000, 10)
        );
        assert_eq!(b.dialect, "claude_code");
        let oauth = [
            t.vendors.claude.budgets,
            t.vendors.agy.budgets,
            OauthBudgets {
                first_event_seconds: t.vendors.grok.budgets.first_event_seconds,
                idle_seconds: t.vendors.grok.budgets.idle_seconds,
            },
        ];
        for b in oauth {
            assert_eq!(
                (b.first_event_seconds, b.idle_seconds),
                (120, DEFAULT_OAUTH_IDLE_SECONDS)
            );
        }
        assert_eq!(t.vendors.grok.budgets.tool_call_floor_seconds, 900);
        let c = &t.vendors.cursor.budgets;
        assert_eq!(
            (
                c.first_output_seconds,
                c.idle_seconds,
                c.idle_after_text_with_tools_seconds,
                c.list_models_seconds
            ),
            (90, 4, 300, 60)
        );
        assert!(cfg.cors_origins.is_empty());
    }

    #[test]
    fn claude_cross_fallback_parses() {
        let cfg: RouterConfig = serde_yaml::from_str(
            "listen: \"127.0.0.1:8787\"\ndata_dir: \"~/.thickapi\"\nclaude_oauth:\n  cross_fallback: false\nthin:\n  vendors:\n    claude:\n      path: \"C:/tools/claude.exe\"\n",
        )
        .expect("parse");
        assert!(!cfg.claude_oauth.cross_fallback);
        assert_eq!(
            cfg.thin.vendors.claude.path.as_deref(),
            Some(std::path::Path::new("C:/tools/claude.exe"))
        );
    }

    #[test]
    fn claude_probe_parses() {
        let cfg: RouterConfig = serde_yaml::from_str(
            "listen: \"127.0.0.1:8787\"\ndata_dir: \"~/.thickapi\"\nclaude_oauth:\n  probe:\n    enabled: false\n    model: claude-opus-4-1\n",
        )
        .expect("parse");
        assert!(!cfg.claude_oauth.probe.enabled);
        assert_eq!(cfg.claude_oauth.probe.model, "claude-opus-4-1");
        assert!(cfg.claude_oauth.probe.on_start);
    }

    #[test]
    fn claude_oauth_mode_parses() {
        let cfg: RouterConfig = serde_yaml::from_str(
            "listen: \"127.0.0.1:8787\"\ndata_dir: \"~/.thickapi\"\nclaude_oauth:\n  mode: only\n  retry_after_hours: 2\n",
        )
        .expect("parse");
        assert_eq!(cfg.claude_oauth.mode, ClaudeOauthMode::Only);
        assert_eq!(cfg.claude_oauth.retry_after_hours, 2);
    }

    #[test]
    fn bridge_dialect_defaults_to_claude_code_and_off_parses() {
        let off = RouterConfig::from_yaml(
            "listen: \"127.0.0.1:8787\"\ndata_dir: \"~/.thickapi\"\nthin:\n  vendors:\n    claude:\n      bridge:\n        dialect: off\n",
        )
        .unwrap()
        .0;
        assert_eq!(off.thin.vendors.claude.bridge.dialect, "off");
        assert!(off.thin.validate().is_ok());
        let parsed = RouterConfig::from_yaml(
            "listen: \"127.0.0.1:8787\"\ndata_dir: \"~/.thickapi\"\nthin:\n  vendors:\n    claude:\n      bridge:\n        dialect: claude\n",
        )
        .unwrap()
        .0;
        let err = parsed.thin.validate().unwrap_err();
        assert!(err.contains("dialect"), "{err}");
    }

    #[test]
    fn bridge_tolerate_parses_nested() {
        let cfg: RouterConfig = serde_yaml::from_str(
            "listen: \"127.0.0.1:8787\"\ndata_dir: \"~/.thickapi\"\nthin:\n  vendors:\n    claude:\n      bridge:\n        tolerate:\n          extra_tools: [Foo]\n          nonempty_inventories: [skills]\n",
        )
        .expect("parse");
        let tolerate = &cfg.thin.vendors.claude.bridge.tolerate;
        assert_eq!(tolerate.extra_tools, vec!["Foo"]);
        assert_eq!(tolerate.nonempty_inventories, vec!["skills"]);
    }

    #[test]
    fn route_idle_follows_the_serving_vendors_own_idle() {
        let mut thin = ThinConfig::default();
        let all = [
            BackendId::ClaudeCode,
            BackendId::Grok,
            BackendId::Agy,
            BackendId::Cursor,
            BackendId::Pi,
        ];
        assert!(all.iter().all(|b| thin.route_idle(*b).is_none()));
        let bound = Some(std::time::Duration::from_secs(DEFAULT_OAUTH_IDLE_SECONDS));
        thin.vendors.grok.budgets.idle_seconds = 0;
        assert_eq!(thin.route_idle(BackendId::Grok), bound);
        assert!(thin.route_idle(BackendId::Agy).is_none());
        thin.vendors
            .cursor
            .budgets
            .idle_after_text_with_tools_seconds = 0;
        assert_eq!(thin.route_idle(BackendId::Cursor), bound);
        thin.vendors.claude.budgets.idle_seconds = 0;
        assert_eq!(thin.route_idle(BackendId::ClaudeCode), bound);
        assert!(thin.route_idle(BackendId::Pi).is_none());
    }
}
