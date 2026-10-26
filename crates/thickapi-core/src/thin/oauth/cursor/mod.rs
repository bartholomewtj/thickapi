//! Cursor OAuth-direct adapter (SPEC §19.8, T4).
//!
//! `cursor-agent` talks to `agentn.global.api5.cursor.sh/agent.v1.AgentService/Run`: a paced,
//! bidirectional Connect-over-HTTP/2 stream of protobuf frames. Wire shape from the MIT
//! `pleaseai/shunt` (`src/adapters/cursor/agent.rs`) and `1jehuang/jcode` projects, which
//! captured it from the real CLI. What this adapter relies on:
//!
//! - Connect framing `[flags u8][len u32 BE][payload]`; flag `0x01` = gzip payload, `0x02` =
//!   end-of-stream trailer (JSON `{}` on success, `{"error":{code,message}}` otherwise).
//! - Request frames: `AgentClientMessage{run_request}` (frame 0), an environment block as
//!   `exec_client_message.request_context_result` (frame 1), then small marker frames, paced.
//!   A `f7:''` heartbeat every 5 s keeps the stream open while the answer streams.
//!   Half-closing early, or ignoring `ExecServerMessage.request_context_args` (`f2.f10`),
//!   yields Connect `internal: No exec result` (2026.09.18 requires a matching
//!   `exec_client_message.request_context_result` with id / exec_id echoed). So does a
//!   `stream_close {id}` that reaches the server before that result: the close follows the
//!   result, as `cursor-agent` writes it, never on a timer (H2, #186). A `No exec result`
//!   before any output gets one fresh Run; a second is a retryable `503 vendor_down`.
//! - Response: answer text at `f1.f1.f1` (every leaf in the frame), reasoning at `f1.f4.f1`,
//!   a bridged tool call at `f2.f11` (`McpArgs{name=1,args=2,tool_call_id=3,tool_name=5}`);
//!   `f2.f10` is the request-context exec (answered, not a tool) and emits `Started` so
//!   `/v1/chat/completions` does not withhold HTTP 200 for the whole Connect hold.
//!   `f2.f36` is the MCP-state exec (measured 2026-09-22, Grok 4.7): the server asks for
//!   the `thickapi` server's tools before the model can call one, and the adapter answers
//!   with the frame-0 declarations, status `connected`. `f2.f19` is a `SpanContext` trace
//!   carried on every exec message; it is not a tool.
//! - `f4` is `kv_server_message`: the server parks conversation blobs on the client
//!   (`set_blob_args`) and may ask for them back (`get_blob_args`); each carries an id and
//!   is acknowledged on the duplex (`kv_client_message`), or the server never calls the
//!   model again inside the Run (measured 2026-09-22 after an MCP-state result).
//!   Cursor's own built-in tools arrive under other `f2.*` fields carrying a `tool_<uuid>`
//!   id. Read (`f7`), grep (`f5`), ls (`f8`), write (`f3`) and shell (`f2` / `f14`) are
//!   handed to Pi as a tool the request declared (`read` / `grep` / `find` / `ls` / `write`
//!   / `bash`, else a quoted `bash` fallback; [`route_builtin`], G2) and the turn ends, the
//!   same way an MCP `f2.f11` does. A shell command that is only a channel probe
//!   (`python -c "print('ok')"`, `echo ok`) is refused on the duplex and the Run goes on;
//!   the third probe in one Run ends the turn. A builder brief uses these instead of MCP and
//!   does not recover from a duplex denial (draftr `3ae4656e`, `4f2a1494`). A read of a
//!   file that does not exist (resolved as Pi resolves it, under the request cwd) is the
//!   exception (G1, #175): it is answered on the duplex with `ReadResult.file_not_found`,
//!   the CLI's own ENOENT answer, and the Run goes on — Cursor's edit built-in pre-reads
//!   its target, then sends `f3 write_args`, which becomes Pi `write`. A built-in no
//!   declared tool runs, and every other documented builtin, is denied on the duplex so the
//!   stream can continue; no undeclared tool name is ever emitted. An unknown `f2`
//!   field with a `tool_` id is 502 `vendor_drift`, `x-should-retry: false`.
//!   Connect `not_found` is `404 model_not_found` naming the
//!   `requested_model` id and params that went on the wire. A `{}` trailer with no output
//!   is `503 vendor_down`, not `stop`. A turn that ends with reasoning and no answer
//!   text is `502 vendor_drift`, not `stop`. So is a turn whose answer text carries a
//!   rendered built-in marker (`⚡ read(1)`, #114) and no tool call: the model wrote its
//!   call out instead of making it; `/ready` records that as `last_row_rejection`
//!   `rendered_builtin`. A `<tool_call>` block written as answer text, when it parses to
//!   one declared tool and a JSON object, is that tool call (#215). A Cursor native name
//!   in the id (`Read`, `StrReplace`, `Write`, `Shell`, `Grep`, `Glob`) with flat child
//!   elements is the declared tool that runs it, and a block naming a tool the request did
//!   not declare is skipped so a later declared block in the same answer still counts.
//!   Composer also writes a bare `<tool_call>` as lines (`Read` / `path` / `<file>`) and
//!   ends the turn without `</tool_call>`; that form is the same call. A `find` exec
//!   whose arguments use `glob` instead of `pattern` is rewritten. The markup is not
//!   streamed. An unparseable block is `502 vendor_drift` `code: text_tool_call`, held
//!   for one fresh Run only when no prose was streamed, and not recorded as
//!   `rendered_builtin`. The live driver opens one fresh Run
//!   before a reasoning-only end or that unparseable block is sent; a second such end is final. Conversation-state `f4` envelopes do not count
//!   toward the unknown-top-level drift counter. Measured-healthy nested preamble
//!   frames (`f1.13`, `f1.7` tool-id echo, `f1.8`, `f1.25`, `f1.5`, `f1.2`,
//!   `f1.27` tool_requests_listed, `f1.3` tool_call_completed, and
//!   timing `f8`) do not count toward the nested or (for lone `f8`) top-level
//!   five-frame budget and are not ThinEvents.
//! - The wire carries one user message, so the OpenAI history is flattened into one prompt
//!   (`<system>`/`<user>`/`<assistant>`/`<tool_call>`/`<tool_result>` blocks) and Pi's tools
//!   are declared natively as `McpTools`. A tool call ends the turn; the next request re-runs
//!   with the result in the transcript. No usage on the wire → the adapter estimates
//!   `ThinEvent::Usage` (prompt bytes/4 + 750 per image; completion from streamed text).
//! - HTTP 464 (empty, `awselb/2.0`) means the ALB refused HTTP/1.1 or the client version is
//!   below Cursor's floor: reported as `vendor_drift`, not retried.
//!
//! The session JWT in `auth.json` lives ~60 days; `cursor-agent status` is run when < 1 day is
//! left so the CLI refreshes under its own lock (the daemon never writes the store).

mod connect;
mod mapper;
mod pb;
mod prompt;
mod request;

use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use bytes::Bytes;

use crate::config::router::ThinConfig;
use crate::config::ConfigBundle;
use crate::thin::capture::Tap;
use crate::thin::{AdapterFuture, EventSender, ModelAdapter, ThinError, ThinRequest};
use crate::thinking::{profile, VendorThinking, CURSOR_EFFORT_SUFFIXES};
use crate::types::BackendId;

use super::creds::{self, Token};
use super::state::{redact, VendorState};
use super::vendor::{
    self, CliToken, Outcome, Rejected, StreamBudgets, VendorParts, VendorRequest, VendorSession,
    Wording,
};

use mapper::CursorWire;
use prompt::{estimate_prompt_tokens, repeated_tool_call};

pub use connect::{connect_frame, Frame, FrameDecoder};
pub use mapper::{
    describe_exec_payload, dump_connect_exec, extract_answer_text, extract_builtin_exec,
    extract_reasoning_text, extract_tool_call, extract_tool_calls, extract_unbridged_builtin_call,
    AgentMapper, BuiltinExec,
};
pub use pb::{decode_protobuf_value, encode_protobuf_value, field_ld, field_str, field_varint};
pub use prompt::{decode_data_url, render_prompt, render_prompt_with_fence, SelectedImage};
pub use request::{
    build_run_frames, builtin_read_call_id, builtin_read_path, declared_tools_in_body,
    deny_builtin_frames, extract_kv_server_message, extract_mcp_state_exec,
    extract_request_context_exec, grep_error_frame, heartbeat_frame, kv_get_result_frame,
    kv_set_result_frame, mcp_state_result_frame, read_file_not_found_frame, read_miss_frame,
    read_rejected_frame, read_success_frame, read_target_missing, read_target_text,
    request_context_result_frame, request_frame_pace, resolve_like_pi, route_builtin, BuiltinRoute,
    DeclaredTools, KvKind, KvServerMessage, McpStateExec, McpTool, ReadMissShape,
    RequestContextExec, RunInput, WireModel, AGENT_MODE, EDIT_PRE_READ_MAX_BYTES, READ_MISS_REASON,
};

pub const VENDOR: &str = "cursor";
/// `RequestContext.env.shell` when `thin.vendors.cursor.shell` is unset. The client's shell tool
/// is Pi's `bash` (Git bash on Windows), not the host OS default (SPEC §19.8, G4).
pub const DEFAULT_SHELL: &str = "bash";
/// `/ready` `last_row_rejection.code` when the row wrote a built-in tool call as answer
/// text instead of making it (#114).
pub const RENDERED_BUILTIN: &str = "rendered_builtin";
/// `/ready` `last_row_rejection.code` and the error `code` when the history shows the same tool
/// call answered the same way `builtin_loop_limit` times in a row (G3, #183).
pub const BUILTIN_LOOP: &str = "builtin_loop";
/// `thin.vendors.cursor.builtin_loop_limit` when unset.
pub const DEFAULT_BUILTIN_LOOP_LIMIT: u32 = 3;

pub const DEFAULT_BASE_URL: &str = "https://agentn.global.api5.cursor.sh";
pub const AGENT_PATH: &str = "/agent.v1.AgentService/Run";
/// `cursor-agent --version` on the machine this was measured on (2026-09-17).
const FALLBACK_CLI_VERSION: &str = "2026.09.15-d2fe57e";
/// `McpToolDefinition.provider_identifier` for Pi's tools.
const PROVIDER_ID: &str = "thickapi";
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(5);
/// Generation can take a while to start; reasoning-only also uses this budget (H3: a 4 s
/// pause after the first thinking delta used to truncate the turn as `stop`).
/// Counted as quiet *ThinEvent* time — Connect frames that map to no event do not reset it
/// (2026-09-20 wrap-up stall: keepalive reset `body.next()` and Pi waited ~300s).
const FIRST_BYTE_TIMEOUT: Duration = Duration::from_secs(90);
/// After an `interaction_update` heartbeat (`f1.13`) and before any answer text, how long
/// the last ThinEvent may sit. cursor-agent resets its stall timer on that frame; xhigh
/// Grok often emits only heartbeats for longer than [`FIRST_BYTE_TIMEOUT`] before the first
/// thinking delta. Empty `f7` keepalives do not widen this — those are the 2026-09-20 stall.
/// Measured from the last ThinEvent, not extended by further heartbeats.
const HEARTBEAT_ONLY_TIMEOUT: Duration = Duration::from_secs(600);
/// After a text-only answer with no tools in the request the server holds the stream
/// open; quiet for this long ends the turn.
const IDLE_TIMEOUT: Duration = Duration::from_secs(4);
/// After text, when the client offered tools, this is only a safety net (H1, #185).
/// A text answer ends on `f1.14 turn_ended`; a tool call ends `tool_calls`. Spending
/// this after answer text is `vendor_down` (`cursor went quiet mid-answer`), not
/// `stop`. Above the longest gap on the F1 `quiet` set; 300 s is the floor.
const IDLE_AFTER_TEXT_WITH_TOOLS: Duration = Duration::from_secs(300);
/// Header names on the Connect request, named in a 403-on-unexpired-token drift error.
const REQUEST_HEADER_NAMES: &str = "authorization, connect-accept-encoding, \
     connect-protocol-version, content-type, user-agent, x-cursor-client-type, \
     x-cursor-client-version, x-ghost-mode, x-request-id, x-original-request-id";
/// `ExecServerMessage` oneof fields that are Cursor's own tools (not MCP `f11`).
/// Documented fields are denied on the duplex; an unknown field carrying a tool-call id
/// (`tool_`, `toolu_`, `call-`) is 502, no retry.
const BUILTIN_EXEC_FIELDS: &[u64] = &[
    2, 3, 4, 5, 6, 7, 8, 9, 12, 13, 14, 16, 17, 18, 20, 21, 22, 23, 24, 25, 26, 27, 28, 29, 30, 31,
    37,
];
/// `ExecClientMessage` / `ExecServerMessage` ids.
const EXEC_CLIENT_ID: u64 = 1;
const EXEC_CLIENT_EXEC_ID: u64 = 15;
/// `ExecServerMessage.grep_args` / `ExecClientMessage.grep_result`.
const EXEC_GREP: u64 = 5;
/// `ExecServerMessage.read_args` / `ExecClientMessage.read_result`.
const EXEC_READ: u64 = 7;
/// `ExecServerMessage.request_context_args` / `ExecClientMessage.request_context_result`.
const EXEC_REQUEST_CONTEXT: u64 = 10;
/// `ExecServerMessage.mcp_state_exec_args` / `ExecClientMessage.mcp_state_exec_result`
/// (`cursor-agent` 2026.09.18 proto). Cursor loads MCP tool schemas lazily for some models
/// (Grok 4.7): the model's first move is a state request for the `thickapi` server, and
/// the adapter answers it with the tools already declared on frame 0. Unanswered, the
/// server holds the stream with keepalives until the budget ends the turn (#114).
const EXEC_MCP_STATE: u64 = 36;
/// `AgentServerMessage.kv_server_message` / `AgentClientMessage.kv_client_message`
/// (`cursor-agent` 2026.09.18 proto): `KvServerMessage {id=1, get_blob_args=2, set_blob_args=3,
/// span_context=4}` answered by `KvClientMessage {id=1, get_blob_result=2, set_blob_result=3}`.
/// The server parks conversation blobs on the client and awaits each ack before it calls the
/// model again inside the same Run (measured 2026-09-22: after an MCP-state result the model
/// never resumed until these were answered).
const AGENT_SERVER_KV: u64 = 4;
const AGENT_CLIENT_KV: u64 = 3;
const KV_ID: u64 = 1;
const KV_GET_BLOB: u64 = 2;
const KV_SET_BLOB: u64 = 3;
/// Blob bytes one Run keeps for `get_blob_args`; beyond it a set is acknowledged with an error.
const KV_BLOB_BUDGET: usize = 256 * 1024 * 1024;
/// `AgentClientMessage.exec_client_control_message`.
const AGENT_CLIENT_CONTROL: u64 = 5;
/// `ExecClientControlMessage.stream_close` / `throw`.
const CONTROL_STREAM_CLOSE: u64 = 1;
const CONTROL_THROW: u64 = 2;
/// Five decoded frames that produce no ThinEvent before any output → drift.
const UNRECOGNISED_FRAME_LIMIT: u8 = 5;
/// Let the CLI refresh when this little of the session token is left.
const REFRESH_WINDOW_SECONDS: u64 = 24 * 3600;
const MAX_FRAME_BYTES: usize = 64 * 1024 * 1024;

pub const FLAG_GZIP: u8 = 0x01;
pub const FLAG_END: u8 = 0x02;

/// Cursor's three-way quiet budget (SPEC §19.8). `0` in yaml is off (`Duration::MAX`).
#[derive(Debug, Clone, Copy)]
pub struct CursorBudgets {
    pub first_output: Duration,
    pub idle: Duration,
    pub idle_after_text_with_tools: Duration,
}

impl CursorBudgets {
    pub const DEFAULT: Self = Self {
        first_output: FIRST_BYTE_TIMEOUT,
        idle: IDLE_TIMEOUT,
        idle_after_text_with_tools: IDLE_AFTER_TEXT_WITH_TOOLS,
    };

    /// `thin.vendors.cursor.budgets`.
    pub fn from_config(budgets: &crate::config::router::CursorBudgetsCfg) -> Self {
        Self {
            first_output: StreamBudgets::seconds(budgets.first_output_seconds),
            idle: StreamBudgets::seconds(budgets.idle_seconds),
            idle_after_text_with_tools: StreamBudgets::seconds(
                budgets.idle_after_text_with_tools_seconds,
            ),
        }
    }
}

/// Decides whether a built-in read's target is missing (`true` = answer it in protocol).
pub type ReadMissCheck = Arc<dyn Fn(&str) -> bool + Send + Sync>;

/// The text of a built-in read's target for the edit pre-read (#248); `None` sends the read
/// to Pi as before. Live: [`read_target_text`] under the request cwd.
pub type ReadText = Arc<dyn Fn(&str) -> Option<String> + Send + Sync>;

/// How a built-in `f7 read_args` whose target is missing is answered (G1, #175): on the
/// duplex, in `shape`, and the Run continues ([`route_builtin`]'s
/// [`BuiltinRoute::ReadMissing`]), instead of handing the read to Pi. Every Run has one:
/// [`ReadMiss::under_cwd`] with [`ReadMissShape::FileNotFound`] unless `thickapi capture`
/// picked another shape ([`CursorAdapter::set_read_miss_shape`]) or fixture replay swapped
/// the check. `router.yaml` has no key for it.
#[derive(Clone)]
pub struct ReadMiss {
    pub shape: ReadMissShape,
    pub is_missing: ReadMissCheck,
}

impl ReadMiss {
    /// Live: the path resolved against the request cwd, as Pi would resolve it.
    pub fn under_cwd(shape: ReadMissShape, cwd: &str) -> Self {
        let cwd = cwd.to_string();
        Self {
            shape,
            is_missing: Arc::new(move |path| read_target_missing(&cwd, path)),
        }
    }

    /// Fixture replay (SPEC §19.11): a captured read's target as the capture host saw it. A
    /// capture redacts its checkout to `<REPO>` and the home directory to `<HOME>`, padded
    /// with `_` to keep protobuf lengths; those resolve under `repo_root` (where `thickapi
    /// capture` ran) and this machine's home, anything else as Pi resolves it under
    /// `repo_root`. Shape `FileNotFound`, as `serve` answers.
    pub fn for_fixtures(repo_root: &std::path::Path) -> Self {
        let root = repo_root.to_path_buf();
        Self {
            shape: ReadMissShape::FileNotFound,
            is_missing: Arc::new(move |path| {
                let home = || {
                    std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" })
                        .map(std::path::PathBuf::from)
                };
                let redacted = [("<REPO>", Some(root.clone())), ("<HOME>", home())]
                    .into_iter()
                    .find_map(|(marker, base)| Some((path.strip_prefix(marker)?, base)));
                match redacted {
                    Some((rest, Some(base))) => {
                        let rest = rest.trim_start_matches('_');
                        let rest = rest.trim_start_matches(['\\', '/']).replace('\\', "/");
                        !base.join(rest).exists()
                    }
                    Some((_, None)) => true,
                    None => read_target_missing(&root.to_string_lossy(), path),
                }
            }),
        }
    }
}

impl std::fmt::Debug for ReadMiss {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReadMiss")
            .field("shape", &self.shape)
            .finish_non_exhaustive()
    }
}

pub struct CursorAdapter {
    base_url: String,
    cwd: String,
    /// `thin.vendors.cursor.shell`: what `RequestContext.env.shell` advertises.
    shell: String,
    /// `thin.vendors.cursor.builtin_loop_limit` (0 = off).
    builtin_loop_limit: usize,
    /// How a built-in read of a missing file is answered on the duplex (G1, #175):
    /// `FileNotFound` everywhere but `thickapi capture`'s F1 `create`. Not a `router.yaml` key.
    read_miss_shape: ReadMissShape,
    pub cli: vendor::CliBacked,
    /// Serialises `cursor-agent --list-models` runs against a concurrent CLI refresh.
    list_models_lock: tokio::sync::Mutex<()>,
    pub state: Arc<VendorState>,
    /// HTTP/2 client, `thin.oauth_in_flight` permits, tap, `thin.reply_timeout_seconds`.
    parts: VendorParts,
    budgets: CursorBudgets,
    /// Bound on `cursor-agent --list-models` (`thin.vendors.cursor.budgets.list_models_seconds`).
    list_models_timeout: Duration,
}

impl CursorAdapter {
    /// Record the wire exchange (`thickapi capture`); `serve` never sets one.
    pub fn set_tap(&mut self, tap: Tap) {
        self.parts.set_tap(tap);
    }

    /// Capture-only (F1, #181), set the way the tap is: the shape a built-in read of a
    /// missing file is answered in. `serve` never calls this and keeps `FileNotFound` (G1).
    pub fn set_read_miss_shape(&mut self, shape: ReadMissShape) {
        self.read_miss_shape = shape;
    }

    /// `FileNotFound` everywhere but `thickapi capture --scenario create`.
    pub fn read_miss_shape(&self) -> ReadMissShape {
        self.read_miss_shape
    }

    pub fn new(cfg: &ConfigBundle, state: Arc<VendorState>) -> Self {
        let thin = &cfg.router.thin;
        let cursor = &thin.vendors.cursor;
        let base_url = cursor
            .base_url
            .clone()
            .filter(|u| Self::base_url_allowed(u))
            .unwrap_or_else(|| DEFAULT_BASE_URL.to_string());
        let mut builder = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(15))
            .http2_keep_alive_interval(Duration::from_secs(15));
        if base_url.starts_with("http://") {
            // Plaintext (loopback stand-ins) cannot negotiate h2 via ALPN; the duplex stream
            // needs HTTP/2, so speak it with prior knowledge.
            builder = builder.http2_prior_knowledge();
        }
        let http = match builder.build() {
            Ok(http) => http,
            Err(err) => panic!("reqwest client: {err}"),
        };
        let cwd = cursor.cwd.clone().unwrap_or_else(|| {
            std::env::current_dir()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_else(|_| ".".to_string())
        });
        let shell = cursor
            .shell
            .clone()
            .unwrap_or_else(|| DEFAULT_SHELL.to_string());
        let credentials_path = cursor.credentials_path.clone();
        let read_path = credentials_path.clone();
        Self {
            base_url,
            cwd,
            shell,
            builtin_loop_limit: cursor
                .builtin_loop_limit
                .unwrap_or(DEFAULT_BUILTIN_LOOP_LIMIT) as usize,
            read_miss_shape: ReadMissShape::FileNotFound,
            // The session JWT lives ~60 days; `cursor-agent status` is a cheap authenticated
            // call the CLI refreshes under its own lock when < 24h remain.
            cli: vendor::CliBacked::new(
                CliToken::new(VENDOR, "cursor-agent", "cursor-agent login")
                    .refresh_with(&["status"], Duration::from_secs(20))
                    .window(Duration::from_secs(REFRESH_WINDOW_SECONDS))
                    .cli_owns_store(credentials_path.is_none()),
                move || match &read_path {
                    Some(p) => creds::cursor_token_from(p),
                    None => creds::cursor_token(),
                },
                vendor::CliVersion::new("cursor-agent"),
                cursor.client_version.clone(),
                FALLBACK_CLI_VERSION,
            )
            .with_client_version_prefix("cli-"),
            list_models_lock: tokio::sync::Mutex::new(()),
            state,
            parts: VendorParts::with_client(thin, http),
            budgets: CursorBudgets::from_config(&cursor.budgets),
            list_models_timeout: StreamBudgets::seconds(cursor.budgets.list_models_seconds),
        }
    }

    /// The subscription bearer only ever goes to a Cursor host over https, or to loopback (tests).
    fn base_url_allowed(url: &str) -> bool {
        let Ok(parsed) = reqwest::Url::parse(url) else {
            tracing::warn!(url, "thin.vendors.cursor.base_url ignored: not a URL");
            return false;
        };
        let host = parsed.host_str().unwrap_or("");
        let loopback = host == "127.0.0.1" || host == "localhost" || host == "[::1]";
        let ok = loopback
            || (parsed.scheme() == "https"
                && (host == "cursor.sh" || host.ends_with(".cursor.sh")));
        if !ok {
            tracing::warn!(
                url,
                "thin.vendors.cursor.base_url ignored: not an https cursor.sh host"
            );
        }
        ok
    }

    /// SPEC §19.5 Cursor column. `vendor_model` is the id `cursor-agent --list-models` prints
    /// (`claude-fable-5-1-thinking-high`, `grok-4.7-high`, `gpt-5.6-sol-xhigh-fast`, `auto`);
    /// the agent service wants the **base** model id plus `ModelParameter {id, value}` pairs,
    /// as the CLI's own `modelParameters` config shows. The alias is decomposed: a trailing
    /// `-fast` → `fast=true`; an effort suffix (`-low|-medium|-high|-xhigh|-max|-minimal|-none`)
    /// → an effort parameter, replaced by the requested level when one is given (`off`/`minimal`
    /// keep the row's default); a trailing `-thinking` → `thinking=true`. A leading
    /// `cursor-` after those suffixes is the `--list-models` variant prefix: strip it so
    /// the wire name is the parameterized base (`cursor-grok-4.7-high` → `grok-4.7` +
    /// `effort=high` + `fast=false`; measured 2026-09-21, `cursor-agent 2026.09.18-9a7762b`
    /// Connect frame 0 `requested_model.model_id`). Keeping `cursor-grok-4.7` is Connect
    /// `not_found`. `auto` is `default`. Ids without an effort suffix (`auto`) ignore the level.
    ///
    /// Grok 4.7 (AvailableModels, 2026-09-22, same CLI): the parameter id is
    /// `reasoning_effort`, not `effort`, and every variant also sends `context`.
    /// `--list-models` prints `grok-4.7-high` once. That legacy slug is shared by a
    /// `context=256k` variant and a `context=500k` max-mode variant; `variants.find`
    /// returns the first, which is 256k. `500k` is `increasesModelCost`. `wire_model`
    /// (no context argument) stays on 256k. `subs/cursor/grok-4.7` asks for 500k
    /// because its catalogue `context_window` is 500000; the caller passes that into
    /// `wire_model_with_context`. That tier is `is_max_mode`, so the encoder sets
    /// `RequestedModel.max_mode`. #147 dropped a separate `-xhigh` row; sending
    /// `context=500k` without `max_mode` is Connect `not_found`.
    pub fn wire_model(vendor_model: &str, effort: Option<&str>) -> WireModel {
        Self::wire_model_with_context(vendor_model, effort, None)
    }

    /// Same decomposition as `wire_model`. `context` replaces Grok 4.7's `context`
    /// parameter (`256k` or `500k`). `None` keeps `256k`.
    pub fn wire_model_with_context(
        vendor_model: &str,
        effort: Option<&str>,
        context: Option<&str>,
    ) -> WireModel {
        let mut name = vendor_model;
        let fast = match name.strip_suffix("-fast") {
            Some(b) => {
                name = b;
                true
            }
            None => false,
        };
        let mut level: Option<String> = None;
        for suffix in CURSOR_EFFORT_SUFFIXES {
            if let Some(stem) = name.strip_suffix(suffix) {
                name = stem;
                level = Some(suffix[1..].to_string());
                break;
            }
        }
        let thinking = match name.strip_suffix("-thinking") {
            Some(b) => {
                name = b;
                true
            }
            None => false,
        };
        let name = if name == "auto" {
            "default".to_string()
        } else {
            name.strip_prefix("cursor-").unwrap_or(name).to_string()
        };
        if level.is_some() {
            if let Some(l @ ("low" | "medium" | "high" | "xhigh" | "max")) = effort {
                level = Some(l.to_string())
            }
        }
        let mut params: Vec<(String, String)> = Vec::new();
        if thinking {
            params.push(("thinking".into(), "true".into()));
        }
        // Order matches AvailableModels `parameterValues` (context, then effort, then fast).
        let grok_47 = name == "grok-4.7";
        if grok_47 {
            params.push(("context".into(), context.unwrap_or("256k").to_string()));
        }
        if let Some(level) = level {
            let key = if grok_47 {
                "reasoning_effort"
            } else {
                "effort"
            };
            params.push((key.into(), level));
        }
        params.push(("fast".into(), fast.to_string()));
        let max_mode = params.iter().any(|(k, v)| k == "context" && v == "500k");
        WireModel {
            name,
            params,
            max_mode,
        }
    }

    /// Cursor's catalogue as the CLI prints it (`cursor-agent --list-models`: `<id> - <name>`
    /// lines), for `thickapi models check` (SPEC §10.3).
    pub async fn list_models(&self) -> Result<Vec<String>, ThinError> {
        let Some(exe) = crate::adapters::resolve_cli("cursor-agent") else {
            return Err(ThinError::vendor_down(
                VENDOR,
                "cursor-agent not found on PATH",
            ));
        };
        let _guard = self.list_models_lock.lock().await;
        let run = tokio::process::Command::new(exe)
            .arg("--list-models")
            .stdin(std::process::Stdio::null())
            .kill_on_drop(true)
            .output();
        let limit = self.list_models_timeout;
        let out = match tokio::time::timeout(limit, run).await {
            Ok(Ok(out)) => out,
            Ok(Err(e)) => {
                return Err(ThinError::vendor_down(
                    VENDOR,
                    format!("cursor-agent --list-models failed to start: {e}"),
                ))
            }
            Err(_) => {
                return Err(ThinError::vendor_down(
                    VENDOR,
                    format!(
                        "cursor-agent --list-models timed out ({}s)",
                        limit.as_secs()
                    ),
                ))
            }
        };
        let text = String::from_utf8_lossy(&out.stdout);
        let ids: Vec<String> = text
            .lines()
            .filter_map(|line| {
                let (id, _name) = line.split_once(" - ")?;
                let id = id.trim();
                (!id.is_empty() && !id.contains(' ')).then(|| id.to_string())
            })
            .collect();
        if ids.is_empty() {
            return Err(ThinError::vendor_drift(
                VENDOR,
                format!(
                    "cursor-agent --list-models printed no `<id> - <name>` lines (exit {}): {}",
                    out.status,
                    redact(&String::from_utf8_lossy(&out.stderr))
                ),
            ));
        }
        Ok(ids)
    }

    /// One thin request: up to two Runs through [`VendorSession::run`], on one token and one
    /// permit. The first reasoning-only end, the first unparseable `<tool_call` that
    /// streamed no prose (#215), or the first handshake `internal: No exec result` before
    /// any output (H2, #186), is held; one fresh Run (new conversation id) follows, and
    /// its end is the one the client sees — never `stop`. A Run that already streamed
    /// answer text is never retried (#127).
    async fn run_turn(&self, req: &ThinRequest, tx: &EventSender) -> Result<(), ThinError> {
        self.refuse_builtin_loop(req)?;
        let token = self.cli.fresh_token().await?;
        let permit = self.parts.permit(VENDOR).await?;
        let session = self.parts.session(VENDOR, &self.cli, &self.state);
        let session = session.with_token(&token).holding(&permit);
        {
            let first = CursorRequest::live(self, req, true);
            let result = session.run(&first, req, tx).await;
            let Some(held) = first.held.get().copied() else {
                return result;
            };
            if tx.is_closed() {
                return result;
            }
            match held {
                HeldEnd::ReasoningOnly => {
                    tracing::info!(request_id = %req.request_id, "cursor ended with reasoning and no answer text; retrying the Run once")
                }
                HeldEnd::NoExecResult => {
                    tracing::warn!(request_id = %req.request_id, "cursor internal: No exec result before any output; retrying the Run once")
                }
                HeldEnd::TextToolCall => {
                    tracing::info!(request_id = %req.request_id, "cursor wrote a <tool_call> and no declared call was recovered; retrying the Run once")
                }
            }
        }
        session
            .run(&CursorRequest::live(self, req, false), req, tx)
            .await
    }

    /// G3: the same tool call answered the same way `builtin_loop_limit` times in a row is a
    /// seat that will not move on. Refused before a token, permit or Run: `502 builtin_loop`,
    /// recorded as the row's `last_row_rejection`, never the vendor's `last_rejection`.
    fn refuse_builtin_loop(&self, req: &ThinRequest) -> Result<(), ThinError> {
        let Some(call) = repeated_tool_call(&req.request.messages, self.builtin_loop_limit) else {
            return Ok(());
        };
        let effort = VendorThinking::wire_param(
            &profile(BackendId::Cursor).thin_apply(req.effort, &req.entry),
        );
        let wire = CursorAdapter::wire_model_with_context(
            req.entry.wire_model(),
            effort,
            (req.entry.context_window == 500_000).then_some("500k"),
        );
        self.state
            .mark_row_rejection(&wire.describe(), 502, BUILTIN_LOOP);
        Err(ThinError::builtin_loop(
            VENDOR,
            format!(
                "cursor is looping: `{}` was called with the same arguments ({}) and got the same result {} times in a row; refused before opening a Run",
                call.name, call.first_arg, self.builtin_loop_limit
            ),
        ))
    }

    /// A row the vendor answered `not_found` for — HTTP 404 (checked before any Connect
    /// frame) or a Connect `not_found` trailer (`terminal_error` below): both mark the row
    /// unavailable so `/ready` `rows_unavailable` reports it (#147), same as the per-row
    /// rejection they already recorded.
    fn mark_row_not_found(&self, row_id: &str, wire_sent: &str, status: u16) {
        self.mark_row_out(row_id, wire_sent, status, "model_not_found");
    }

    /// A per-row fact (`not_found`, or a usage limit on one model, #249): `/ready`
    /// `last_row_rejection` and `rows_unavailable`, never vendor `last_rejection`, so the
    /// account's other rows stay available.
    fn mark_row_out(&self, row_id: &str, wire_sent: &str, status: u16, code: &str) {
        self.state.mark_row_rejection(wire_sent, status, code);
        self.state.mark_row_unavailable(row_id);
    }

    fn record_stream_outcome(
        &self,
        row_id: &str,
        wire: &CursorWire<'_>,
        result: &Result<(), ThinError>,
    ) {
        if let Some(err) = wire.terminal_error() {
            if err.code == "model_not_found" {
                self.mark_row_not_found(row_id, wire.wire_sent(), err.status);
            } else if wire.row_limited() {
                self.mark_row_out(row_id, wire.wire_sent(), err.status, err.code);
            } else if wire.rendered_builtin().is_some() {
                // Recorded above as the row fact; the vendor stays as it was.
            } else {
                self.state
                    .mark_rejection(err.status, err.code, &err.message);
            }
            return;
        }
        if wire.finished_ok() {
            self.state.mark_ok(200);
        } else if let Err(err) = result {
            self.state
                .mark_rejection(err.status, err.code, &err.message);
        }
    }
}

impl ModelAdapter for CursorAdapter {
    fn vendor(&self) -> &'static str {
        VENDOR
    }

    fn run<'a>(&'a self, req: &'a ThinRequest, tx: EventSender) -> AdapterFuture<'a> {
        Box::pin(async move { self.run_turn(req, &tx).await })
    }
}

/// One Cursor Run as [`VendorSession::run`] sees it (SPEC §19.8): a paced Connect request
/// whose body is also where exec / KV / MCP-state results go back
/// ([`VendorRequest::duplex`]), the Connect decoder, the HTTP status table, and the outcome
/// rules that keep a row's `not_found` or rendered built-in off the vendor's `last_rejection`.
/// The default is the offline one: fixture replay, nothing sent or recorded.
#[derive(Default)]
pub struct CursorRequest<'a> {
    adapter: Option<&'a CursorAdapter>,
    /// Hold a reasoning-only end for one fresh Run (the first Run only).
    defer: bool,
    row_id: String,
    /// The `requested_model` name and params on frame 0, named by `not_found` (C5).
    wire: String,
    /// The params on frame 0, `k=v` comma-joined, for the access line.
    params: String,
    has_tools: bool,
    /// Thin request id, logged when the after-text safety net fires (H1).
    request_id: String,
    /// The tools declared on frame 0, replayed on `mcp_state_exec_args`.
    tools: Vec<McpTool>,
    prompt_tokens: u64,
    /// The opening frames as one blob, for `thickapi capture`.
    blob: Option<Vec<u8>>,
    body_tx: Option<vendor::ReplySender>,
    body: Mutex<Option<reqwest::Body>>,
    /// Paces the opening frames, then heartbeats; dropping the Run stops it and half-closes
    /// the body.
    _pacer: Option<TurnGuard>,
    /// Hold a handshake `No exec result` for one fresh Run: the first live Run, and not
    /// under `thickapi capture` (it retries with a fresh recording of its own).
    hold_no_exec: bool,
    /// How the Run ended when it was held for a fresh one.
    held: OnceLock<HeldEnd>,
    /// How a built-in read of a missing file is answered in protocol (G1). `None` is the
    /// wire's default: `FileNotFound`, the path resolved under the wire's cwd.
    read_miss: Option<ReadMiss>,
    /// Cwd advertised on this Run only. Empty on the offline default, which has no request.
    cwd: String,
}

/// `x-thickapi-cwd` for one chat completion (SPEC §19.8, #205).
///
/// Missing or empty keeps the configured cwd (`Ok(None)`). A relative path, or a path that
/// does not exist, is `400 invalid_spec` — no fall back to the config cwd.
pub fn cursor_cwd_override(raw: Option<&str>) -> Result<Option<String>, ThinError> {
    let Some(raw) = raw else {
        return Ok(None);
    };
    let raw = raw.trim();
    if raw.is_empty() {
        return Ok(None);
    }
    let path = std::path::Path::new(raw);
    if !path.is_absolute() || !path.exists() {
        return Err(ThinError::invalid_spec(
            "x-thickapi-cwd must be one absolute path that exists",
        ));
    }
    Ok(Some(raw.to_string()))
}

/// The cwd this request advertises: the header override, else the client's configured cwd.
fn request_cwd<'b>(adapter: &'b CursorAdapter, req: &'b ThinRequest) -> &'b str {
    req.cursor_cwd.as_deref().unwrap_or(adapter.cwd.as_str())
}

impl<'a> CursorRequest<'a> {
    /// Built once the permit is held: the flattened transcript, the native tool catalogue
    /// and the model meta (`wire`, what Connect `not_found` names) as frames. Frame 0 and 1
    /// need settle time before the markers, then a heartbeat every `HEARTBEAT_INTERVAL`
    /// until the Run is dropped.
    fn live(adapter: &'a CursorAdapter, req: &ThinRequest, defer: bool) -> Self {
        let (prompt, images) = render_prompt(&req.request);
        let prompt_tokens = estimate_prompt_tokens(&prompt, images.len());
        let effort = VendorThinking::wire_param(
            &profile(BackendId::Cursor).thin_apply(req.effort, &req.entry),
        );
        let context = (req.entry.context_window == 500_000).then_some("500k");
        let wire = CursorAdapter::wire_model_with_context(req.entry.wire_model(), effort, context);
        let tools: Vec<McpTool> = req
            .request
            .tools
            .iter()
            .map(|t| McpTool {
                name: t.function.name.clone(),
                description: t.function.description.clone().unwrap_or_default(),
                input_schema: t
                    .function
                    .parameters
                    .clone()
                    .unwrap_or_else(|| serde_json::json!({"type": "object", "properties": {}})),
            })
            .collect();
        let cwd = request_cwd(adapter, req).to_string();
        let frames = build_run_frames(&RunInput {
            prompt: &prompt,
            model: &wire.name,
            params: &wire.params,
            cwd: &cwd,
            shell: &adapter.shell,
            mode: AGENT_MODE,
            images: &images,
            tools: &tools,
        });
        let (body_tx, body_rx) = tokio::sync::mpsc::channel::<Result<Bytes, std::io::Error>>(8);
        let (stop_tx, mut stop_rx) = tokio::sync::oneshot::channel::<()>();
        let blob = adapter.parts.tapping().then(|| frames.concat());
        let pacer = body_tx.clone();
        let sender = tokio::spawn(async move {
            for (idx, frame) in frames.into_iter().enumerate() {
                if pacer.send(Ok(frame)).await.is_err() {
                    return;
                }
                let pace = request_frame_pace(idx);
                if pace.is_zero() {
                    continue;
                }
                tokio::select! {
                    _ = &mut stop_rx => return,
                    _ = tokio::time::sleep(pace) => {}
                }
            }
            let start = tokio::time::Instant::now() + HEARTBEAT_INTERVAL;
            let mut ticker = tokio::time::interval_at(start, HEARTBEAT_INTERVAL);
            loop {
                tokio::select! {
                    _ = &mut stop_rx => return,
                    _ = ticker.tick() => {
                        if pacer.send(Ok(heartbeat_frame())).await.is_err() {
                            return;
                        }
                    }
                }
            }
        });
        let body = reqwest::Body::wrap_stream(futures_util::stream::unfold(
            body_rx,
            |mut rx| async move { rx.recv().await.map(|item| (item, rx)) },
        ));
        Self {
            adapter: Some(adapter),
            defer,
            row_id: req.entry.id.clone(),
            wire: wire.describe(),
            params: wire.params_line(),
            has_tools: !req.request.tools.is_empty(),
            request_id: req.request_id.clone(),
            tools,
            prompt_tokens,
            blob,
            body_tx: Some(body_tx),
            body: Mutex::new(Some(body)),
            _pacer: Some(TurnGuard {
                _stop: stop_tx,
                _sender: sender,
            }),
            hold_no_exec: defer && !adapter.parts.tapping(),
            held: OnceLock::new(),
            read_miss: Some(ReadMiss::under_cwd(adapter.read_miss_shape, &cwd)),
            cwd,
        }
    }

    /// Fixture replay only (SPEC §19.11): the Connect decoder `run` uses. A body that ends
    /// without a Connect terminal is finished as `Stop` when output arrived, as the live idle
    /// end does.
    pub fn offline() -> Self {
        Self::default()
    }

    /// Replay with this existence check (and shape) for built-in reads, in place of the
    /// default (the path resolved under the process cwd): the reads `miss.is_missing` names
    /// are answered in protocol, the rest go by the declared table. F1's `create*` sets
    /// resolve their redacted capture paths this way.
    pub fn answering_missing_reads(mut self, miss: ReadMiss) -> Self {
        self.read_miss = Some(miss);
        self
    }

    /// Replay as if frame 0 had declared these tools (G2): a built-in becomes one of them or
    /// is denied, as on the live Run. The offline default declares none.
    pub fn declaring(mut self, declared: &DeclaredTools) -> Self {
        self.has_tools = !declared.names().is_empty();
        self.tools = declared
            .names()
            .iter()
            .map(|name| McpTool {
                name: name.clone(),
                description: String::new(),
                input_schema: serde_json::json!({ "type": "object", "properties": {} }),
            })
            .collect();
        self
    }
}

impl<'a> VendorRequest for CursorRequest<'a> {
    type Wire = CursorWire<'a>;
    const WORDING: Wording = Wording {
        label: "cursor",
        host: "cursor agent service",
        login_hint: "cursor-agent login",
        headers_sent: REQUEST_HEADER_NAMES,
    };

    async fn build(
        &self,
        session: &VendorSession<'_>,
        _req: &ThinRequest,
        token: &Token,
    ) -> Result<reqwest::RequestBuilder, ThinError> {
        let body = self.body.lock().unwrap_or_else(|p| p.into_inner()).take();
        let (Some(adapter), Some(body)) = (self.adapter, body) else {
            return Err(ThinError::server("cursor: this Run has no request to send"));
        };
        let request_id = uuid::Uuid::new_v4().to_string();
        let url = format!("{}{AGENT_PATH}", adapter.base_url.trim_end_matches('/'));
        Ok(session
            .http()
            .post(url)
            .header("authorization", format!("Bearer {}", token.access_token))
            .header("connect-accept-encoding", "gzip,br")
            .header("connect-protocol-version", "1")
            .header("content-type", "application/connect+proto")
            .header("user-agent", "connect-es/1.6.1")
            .header("x-cursor-client-type", "cli")
            .header("x-cursor-client-version", session.cli().client_version())
            .header("x-ghost-mode", "true")
            .header("x-request-id", &request_id)
            .header("x-original-request-id", &request_id)
            .body(body))
    }

    fn wire_params(&self, _req: &ThinRequest) -> Option<String> {
        (!self.params.is_empty()).then(|| self.params.clone())
    }

    /// Cursor's two-field view of `thin.vendors.cursor.budgets`, carried to [`Self::wire`] and
    /// not read there: the decoder applies the three-way rule from [`CursorBudgets`].
    fn budgets(&self, _req: &ThinRequest, cfg: &ThinConfig) -> StreamBudgets {
        let own = &cfg.vendors.cursor.budgets;
        StreamBudgets {
            first_event: StreamBudgets::seconds(own.first_output_seconds),
            idle: StreamBudgets::seconds(own.idle_seconds),
        }
    }

    /// Cursor's three-way quiet budget (`thin.vendors.cursor.budgets`: `first_output_seconds`,
    /// `idle_seconds`, `idle_after_text_with_tools_seconds`) lives in the decoder, which knows
    /// what has arrived. `f1.14 turn_ended` finishes a text answer; a tool call finishes
    /// `tool_calls`. Spending the after-text budget when tools were offered is `vendor_down`
    /// (`cursor went quiet mid-answer`), not `stop`. `idle_seconds` still ends a text-only
    /// answer that has not seen `turn_ended`.
    fn wire(&self, model: &str, _budgets: StreamBudgets) -> Self::Wire {
        let cwd = if !self.cwd.is_empty() {
            self.cwd.as_str()
        } else {
            self.adapter.map_or("", |a| a.cwd.as_str())
        };
        let mut wire = CursorWire::new(
            model,
            Some(&self.wire),
            cwd,
            self.has_tools,
            self.prompt_tokens,
            self.adapter.map_or(CursorBudgets::DEFAULT, |a| a.budgets),
            self.tools.clone(),
        );
        wire.set_request_id(&self.request_id);
        if let Some(miss) = &self.read_miss {
            wire = wire.with_read_miss(miss.clone());
        }
        if let Some(a) = self.adapter {
            wire.set_shell(&a.shell);
        }
        if self.defer {
            wire.defer_reasoning_only_end();
        }
        if self.hold_no_exec {
            wire.hold_no_exec_result();
        }
        wire
    }

    /// HTTP 404 is the row's `not_found`, naming the `requested_model` sent; the rest is
    /// Cursor's status table (464: the ALB refused the client).
    fn classify(&self, rejected: &Rejected<'_>) -> ThinError {
        if rejected.status == 404 {
            return ThinError::model_not_found(&self.wire)
                .with_vendor(VENDOR)
                .with_hint("thickapi doctor");
        }
        let server = rejected.headers.get("server").and_then(|v| v.to_str().ok());
        let version = self.adapter.map(|a| a.cli.client_version());
        classify_http(
            rejected.status,
            rejected.body,
            server.unwrap_or(""),
            vendor::retry_after(rejected.headers),
            version.as_deref().unwrap_or(""),
            !rejected.token.expired(),
        )
    }

    fn duplex(&self) -> Option<vendor::ReplySender> {
        self.body_tx.clone()
    }

    fn captured_body(&self) -> Option<Vec<u8>> {
        self.blob.clone()
    }

    /// Per-row facts stay off the vendor: a 404 or Connect `not_found` is
    /// `last_row_rejection` plus `rows_unavailable`, a rendered built-in is
    /// `last_row_rejection` whether held or final. A held reasoning-only end records nothing:
    /// the fresh Run answers for it. Success is Cursor's own finish, not the core's.
    fn on_outcome(&self, state: &VendorState, outcome: Outcome<'_, Self::Wire>) {
        let Some(adapter) = self.adapter else {
            return;
        };
        match outcome {
            Outcome::Rejected { status: 404, .. } => {
                adapter.mark_row_not_found(&self.row_id, &self.wire, 404);
            }
            Outcome::Rejected {
                status,
                error,
                body,
            } => state.mark_rejection_retry(status, error.code, body, error.retry_after_seconds()),
            Outcome::Streamed { wire, result } => {
                let held = if wire.reasoning_only_deferred() {
                    Some(HeldEnd::ReasoningOnly)
                } else if wire.no_exec_result_held() {
                    Some(HeldEnd::NoExecResult)
                } else if wire.text_tool_call_deferred() {
                    Some(HeldEnd::TextToolCall)
                } else {
                    None
                };
                if let Some(end) = held {
                    let _ = self.held.set(end);
                }
                if wire.rendered_builtin().is_some() {
                    state.mark_row_rejection(wire.wire_sent(), 502, RENDERED_BUILTIN);
                }
                if held.is_none() {
                    adapter.record_stream_outcome(&self.row_id, wire, result);
                }
            }
        }
    }
}

/// Why a first Run was held for one fresh Run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HeldEnd {
    /// Reasoning and no answer text.
    ReasoningOnly,
    /// Connect `internal: No exec result` before any output (H2, #186).
    NoExecResult,
    /// An unparseable `<tool_call` and no prose `TextDelta` (#215).
    TextToolCall,
}

/// Keeps the paced request stream alive for the Run; dropping it stops the sender and
/// half-closes the request body.
struct TurnGuard {
    _stop: tokio::sync::oneshot::Sender<()>,
    _sender: tokio::task::JoinHandle<()>,
}

/// Map a non-2xx HTTP status (before any Connect frame) to a thin error.
fn classify_http(
    status: u16,
    body: &str,
    server: &str,
    retry_after: Option<u64>,
    client_version: &str,
    token_unexpired: bool,
) -> ThinError {
    let excerpt: String = redact(body).chars().take(200).collect();
    match status {
        401 => ThinError::vendor_login_required(
            VENDOR,
            format!("cursor rejected the session token (401): {excerpt}"),
            "cursor-agent login",
        ),
        402 | 403 | 429 => vendor::status_error(&vendor::StatusFault {
            vendor: VENDOR,
            status,
            prefix: &format!("cursor {status}"),
            shown: &excerpt,
            raw: body,
            retry_after,
            model: "",
            token_unexpired,
            login_hint: "cursor-agent login",
            headers_sent: REQUEST_HEADER_NAMES,
        }),
        464 => ThinError::vendor_drift(
            VENDOR,
            format!(
                "cursor agent service returned 464 (server {server:?}): the ALB refused the \
                 request before Cursor saw it — HTTP/2 was not negotiated or client version \
                 {client_version} is below the floor; set router.yaml thin.vendors.cursor.client_version \
                 to the current `cursor-agent --version` or disable the rows"
            ),
        ),
        s if (500..600).contains(&s) => {
            ThinError::vendor_down(VENDOR, format!("cursor {s}: {excerpt}"))
        }
        s => ThinError::vendor_drift(VENDOR, format!("cursor {s}: {excerpt}")),
    }
}

/// Map a Connect end-of-stream error (`{"error":{code,message}}`) to a thin error.
/// `wire` is the `requested_model` name and params that went on the Run (C5).
fn classify_connect(code: &str, message: &str, wire: Option<&str>) -> ThinError {
    let raw = message;
    let message = redact(message);
    match code {
        "unauthenticated" => ThinError::vendor_login_required(
            VENDOR,
            format!("cursor: {message}"),
            "cursor-agent login",
        ),
        "permission_denied" => {
            ThinError::permission(format!("cursor: {message}")).with_vendor(VENDOR)
        }
        "resource_exhausted" => {
            vendor::limit_error(VENDOR, format!("cursor: {message}"), raw, None)
        }
        "not_found" => {
            let sent = wire.filter(|s| !s.is_empty()).unwrap_or("(unknown)");
            let mut err = ThinError::model_not_found(sent)
                .with_vendor(VENDOR)
                .with_hint("thickapi doctor");
            err.message = format!("cursor not_found: requested_model {sent} (vendor: {message})");
            err
        }
        "context_length_exceeded" | "invalid_argument" => {
            ThinError::invalid_request(format!("cursor {code}: {message}"), None)
                .with_vendor(VENDOR)
        }
        "unavailable" | "deadline_exceeded" | "internal" | "unknown" => {
            ThinError::vendor_down(VENDOR, format!("cursor {code}: {message}"))
        }
        other => ThinError::vendor_drift(VENDOR, format!("cursor {other}: {message}")),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests;
