use std::time::Duration;

use bytes::Bytes;
use serde_json::Value;

use super::connect::connect_frame;
use super::pb::{
    encode_protobuf_value, field_ld, field_str, field_varint, iter_fields, read_varint,
};
use super::prompt::SelectedImage;
use super::{
    AGENT_CLIENT_CONTROL, AGENT_CLIENT_KV, AGENT_SERVER_KV, CONTROL_STREAM_CLOSE, CONTROL_THROW,
    EXEC_CLIENT_EXEC_ID, EXEC_CLIENT_ID, EXEC_GREP, EXEC_MCP_STATE, EXEC_READ,
    EXEC_REQUEST_CONTEXT, KV_GET_BLOB, KV_ID, KV_SET_BLOB, PROVIDER_ID,
};

/// `McpStateServer.status` for a server whose tools are ready (the CLI's own word).
const MCP_STATE_CONNECTED: &str = "connected";

/// Sleep after sending request frame `idx` before the next frame.
///
/// Frames 0 (run_request) and 1 (env as `exec_client_message.request_context_result`)
/// go back-to-back; the KV markers follow at 2.3 s, then every 0.4 s (SPEC §19.8).
/// Sleeping 1.5 s after frame 0 before env lets Cursor start without workspace context
/// and answer Connect `internal: No exec result` (draftr `1f2839e7`). On `cursor-agent`
/// 2026.09.18 the server also sends `f2.f10 request_context_args` and times out unless a
/// matching exec result is written on the duplex body.
///
/// H2 (#186): a paced `stream_close {id: 0}` used to go out at 1.5 s. The ask is exec 0
/// too, so a close that crossed an ask in flight closed the exec before its result and the
/// server trailered `No exec result` (all 15 live deaths, 20–29 Sep, had the ask land
/// 1.507–1.723 s in). The close now follows each answer ([`stream_close_frame`]); the KV
/// markers keep their old times (2.3 s, 2.7 s, …).
pub fn request_frame_pace(idx: usize) -> Duration {
    match idx {
        0 => Duration::ZERO,
        1 => Duration::from_millis(2300),
        _ => Duration::from_millis(400),
    }
}

/// `UserMessage.mode` (`AgentMode`): AGENT=1 (tools allowed), ASK=2, PLAN=3.
pub const AGENT_MODE: u64 = 1;

/// A Pi tool declared to Cursor as a native MCP tool.
#[derive(Debug, Clone)]
pub struct McpTool {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

/// The agent service's model selection: base id plus `ModelParameter {id, value}` pairs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WireModel {
    pub name: String,
    pub params: Vec<(String, String)>,
    /// `RequestedModel.max_mode`. True for the Grok 4.7 `context=500k` variant
    /// (`is_max_mode`). Omitting it is Connect `not_found`.
    pub max_mode: bool,
}

impl WireModel {
    /// The params as the access line's `wire_params`: `k=v` comma-joined in send order.
    pub fn params_line(&self) -> String {
        let mut out = self
            .params
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join(",");
        if self.max_mode {
            if !out.is_empty() {
                out.push(',');
            }
            out.push_str("max_mode=true");
        }
        out
    }

    /// What Connect `not_found` must name: the base id, max mode, and every param sent.
    pub fn describe(&self) -> String {
        let mut out = self.name.clone();
        if self.max_mode {
            out.push_str(" max_mode=true");
        }
        for (k, v) in &self.params {
            out.push(' ');
            out.push_str(k);
            out.push('=');
            out.push_str(v);
        }
        out
    }
}

pub struct RunInput<'a> {
    pub prompt: &'a str,
    pub model: &'a str,
    /// `ModelParameter` pairs (`thinking`, `effort`, `fast`) in the order the CLI sends them.
    pub params: &'a [(String, String)],
    pub cwd: &'a str,
    /// `RequestContext.env.shell`: the client's shell tool (default `bash`, SPEC §19.8).
    pub shell: &'a str,
    pub mode: u64,
    pub images: &'a [SelectedImage],
    pub tools: &'a [McpTool],
}

/// `McpToolDefinition { name=1, description=2, input_schema=3 (Value), provider_identifier=4,
/// tool_name=5 }`.
fn encode_mcp_tool(tool: &McpTool) -> Vec<u8> {
    let mut out = field_str(1, &tool.name);
    out.extend(field_str(2, &tool.description));
    out.extend(field_ld(3, &encode_protobuf_value(&tool.input_schema)));
    out.extend(field_str(4, PROVIDER_ID));
    out.extend(field_str(5, &tool.name));
    out
}

/// `McpTools { tools = 1 repeated }`; empty encodes to zero bytes, matching the text-only
/// placeholder `f4:''` exactly (the field is required even when empty).
pub(super) fn encode_mcp_tools(tools: &[McpTool]) -> Vec<u8> {
    let mut out = Vec::new();
    for tool in tools {
        out.extend(field_ld(1, &encode_mcp_tool(tool)));
    }
    out
}

/// `{f1: name, f2: max_mode, f3: {f1: id, f2: value}...}` — one `ModelParameter` per pair
/// (`fast` always). Field 2 is set only for the 500k max-mode variant.
pub(super) fn encode_model_meta(name: &str, params: &[(String, String)]) -> Vec<u8> {
    let mut out = field_str(1, name);
    if params.iter().any(|(k, v)| k == "context" && v == "500k") {
        out.extend(field_varint(2, 1));
    }
    for (id, value) in params {
        let mut kv = field_str(1, id);
        kv.extend(field_str(2, value));
        out.extend(field_ld(3, &kv));
    }
    out
}

/// `SelectedImage { uuid=2, path=3, mime_type=7, data=8 }`.
fn encode_selected_image(image: &SelectedImage) -> Vec<u8> {
    let mut out = field_str(2, &image.uuid);
    out.extend(field_str(3, &image.path));
    out.extend(field_str(7, &image.mime_type));
    out.extend(field_ld(8, &image.data));
    out
}

/// The ordered request frames for one turn.
pub fn build_run_frames(input: &RunInput<'_>) -> Vec<Bytes> {
    let conv = uuid::Uuid::new_v4().to_string();
    let msg = uuid::Uuid::new_v4().to_string();

    // frame 0: AgentClientMessage.run_request (f1) = AgentRunRequest
    //   f2 ConversationAction { f1 { f1 UserMessage { f1 text, f2 id, f3 selected_context, f4 mode } } }
    let mut user = field_str(1, input.prompt);
    user.extend(field_str(2, &msg));
    if input.images.is_empty() {
        user.extend(field_str(3, ""));
    } else {
        let mut ctx = Vec::new();
        for image in input.images {
            ctx.extend(field_ld(1, &encode_selected_image(image)));
        }
        user.extend(field_ld(3, &ctx));
    }
    user.extend(field_varint(4, input.mode));
    let action = field_ld(2, &field_ld(1, &field_ld(1, &user)));

    let mut req = field_str(1, ""); // conversation_state: none (stateless)
    req.extend(action);
    req.extend(field_ld(4, &encode_mcp_tools(input.tools)));
    req.extend(field_str(5, &conv));
    req.extend(field_ld(9, &encode_model_meta(input.model, input.params)));
    // f12 = 1 makes the server reject with "workspace context exclusion is not allowed".
    req.extend(field_varint(12, 0));
    req.extend(field_ld(14, &field_str(1, "default")));
    req.extend(field_ld(14, &encode_model_meta(input.model, input.params)));
    req.extend(field_str(16, &conv));
    if !input.images.is_empty() {
        req.extend(field_varint(19, 1)); // client_supports_inline_images
    }
    let frame0 = connect_frame(&field_ld(1, &req), 0);

    // frame 1: proactive env as ExecClientMessage.request_context_result (id=0).
    // 2026.09.15 accepted this as workspace context; 2026.09.18 still wants it, then
    // also sends f2.f10 which `stream` answers with a matching exec result.
    let frame1 = request_context_result_frame(input.cwd, input.shell, 0, None);

    // No paced `f5{f1:''}` (`stream_close {id: 0}`) here: it raced the server's exec-0 ask
    // (H2, #186). `cursor-agent` closes an exec only after its result.
    let mut frames = vec![frame0, frame1];
    frames.push(connect_frame(&field_ld(3, &field_str(3, "")), 0)); // f3{f3:''}
    for n in 1..=8u64 {
        let mut m = field_varint(1, n);
        m.extend(field_str(3, ""));
        frames.push(connect_frame(&field_ld(3, &m), 0)); // f3{f1:N, f3:''}
    }
    frames
}

pub fn heartbeat_frame() -> Bytes {
    connect_frame(&field_ld(7, &[]), 0)
}

/// `RequestContextEnv` as the CLI encodes it (os, cwd, shell, tz, sandbox flags).
fn encode_request_context_env(cwd: &str, shell: &str) -> Vec<u8> {
    let os = if cfg!(windows) {
        "windows"
    } else if cfg!(target_os = "macos") {
        "darwin"
    } else {
        "linux"
    };
    let mut env = field_str(1, os);
    env.extend(field_str(2, cwd));
    env.extend(field_str(3, shell));
    env.extend(field_str(10, "UTC"));
    env.extend(field_str(11, cwd));
    env.extend(field_varint(14, 1));
    env.extend(field_varint(16, 1));
    env.extend(field_varint(19, 0));
    env.extend(field_varint(20, 0));
    env.extend(field_str(21, cwd));
    env.extend(field_varint(22, 0));
    env
}

/// `RequestContext { env=4 }` — enough for the agent service to proceed without a real workspace.
fn encode_request_context(cwd: &str, shell: &str) -> Vec<u8> {
    field_ld(4, &encode_request_context_env(cwd, shell))
}

/// `RequestContextResult.success { request_context }` — oneof field 1 wrapping
/// `RequestContextSuccess.request_context` (also field 1).
fn encode_request_context_success(cwd: &str, shell: &str) -> Vec<u8> {
    field_ld(1, &field_ld(1, &encode_request_context(cwd, shell)))
}

/// Duplex body frame: `AgentClientMessage.exec_client_message` with `request_context_result`.
/// Echoes `id` / `exec_id` from the server's `request_context_args` so the CLI's executor
/// does not throw `No exec result`.
pub fn request_context_result_frame(
    cwd: &str,
    shell: &str,
    id: u64,
    exec_id: Option<&str>,
) -> Bytes {
    let mut msg = field_varint(EXEC_CLIENT_ID, id);
    if let Some(exec_id) = exec_id.filter(|s| !s.is_empty()) {
        msg.extend(field_str(EXEC_CLIENT_EXEC_ID, exec_id));
    }
    msg.extend(field_ld(
        EXEC_REQUEST_CONTEXT,
        &encode_request_context_success(cwd, shell),
    ));
    connect_frame(&field_ld(2, &msg), 0)
}

/// `ExecServerMessage.mcp_state_exec_args` (`f2.f36`): `McpStateExecArgs { server_identifiers
/// = 1 repeated, kick_only = 2 }`. Measured 2026-09-22 on `cursor-agent` 2026.09.18 with Grok
/// 4.7: the model asks for the `thickapi` server's state before calling any of its tools.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpStateExec {
    pub id: u64,
    pub exec_id: Option<String>,
    pub server_identifiers: Vec<String>,
    pub kick_only: bool,
}

/// `f2.f36` state ask (no `tool_` id). Empty when the frame is not an MCP-state exec.
pub fn extract_mcp_state_exec(payload: &[u8]) -> Option<McpStateExec> {
    for esm in iter_fields(payload) {
        if esm.field != 2 || esm.wire != 2 {
            continue;
        }
        let mut id = 0u64;
        let mut exec_id = None;
        let mut args = None;
        for field in iter_fields(esm.data) {
            match (field.field, field.wire) {
                (EXEC_CLIENT_ID, 0) => id = read_varint_value(field.data).unwrap_or(0),
                (EXEC_CLIENT_EXEC_ID, 2) => {
                    exec_id = std::str::from_utf8(field.data)
                        .ok()
                        .filter(|s| !s.is_empty())
                        .map(str::to_string);
                }
                (EXEC_MCP_STATE, 2) => args = Some(field.data),
                _ => {}
            }
        }
        let Some(args) = args else {
            continue;
        };
        let mut server_identifiers = Vec::new();
        let mut kick_only = false;
        for field in iter_fields(args) {
            match (field.field, field.wire) {
                (1, 2) => {
                    if let Ok(s) = std::str::from_utf8(field.data) {
                        server_identifiers.push(s.to_string());
                    }
                }
                (2, 0) => kick_only = read_varint_value(field.data).unwrap_or(0) != 0,
                _ => {}
            }
        }
        return Some(McpStateExec {
            id,
            exec_id,
            server_identifiers,
            kick_only,
        });
    }
    None
}

/// Duplex body frame answering `mcp_state_exec_args`: `ExecClientMessage.mcp_state_exec_result`
/// = `McpStateExecResult.success { McpStateSuccess.servers = [McpStateServer { server_name = 1,
/// server_identifier = 2, tools = 5 repeated McpToolDefinition, status = 7 }] }` — the one
/// `thickapi` server, its tools exactly as declared on frame 0, `connected`. Echoes `id` /
/// `exec_id` like the request-context result.
pub fn mcp_state_result_frame(id: u64, exec_id: Option<&str>, tools: &[McpTool]) -> Bytes {
    let mut server = field_str(1, PROVIDER_ID);
    server.extend(field_str(2, PROVIDER_ID));
    for tool in tools {
        server.extend(field_ld(5, &encode_mcp_tool(tool)));
    }
    server.extend(field_str(7, MCP_STATE_CONNECTED));
    let success = field_ld(1, &server);
    let result = field_ld(1, &success);
    let mut msg = field_varint(EXEC_CLIENT_ID, id);
    if let Some(exec_id) = exec_id.filter(|s| !s.is_empty()) {
        msg.extend(field_str(EXEC_CLIENT_EXEC_ID, exec_id));
    }
    msg.extend(field_ld(EXEC_MCP_STATE, &result));
    connect_frame(&field_ld(2, &msg), 0)
}

/// One `AgentServerMessage.kv_server_message` (`f4`): a blob the server parks on the client
/// (`set_blob_args {blob_id=1, blob_data=2}`) or asks back (`get_blob_args {blob_id=1}`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KvServerMessage {
    pub id: u64,
    pub kind: KvKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KvKind {
    Set { blob_id: Vec<u8>, data: Vec<u8> },
    Get { blob_id: Vec<u8> },
}

/// `f4` KV ask. Empty when the frame carries no KV message (or an unknown KV oneof).
pub fn extract_kv_server_message(payload: &[u8]) -> Option<KvServerMessage> {
    for kv in iter_fields(payload) {
        if kv.field != AGENT_SERVER_KV || kv.wire != 2 {
            continue;
        }
        let mut id = 0u64;
        let mut kind = None;
        for field in iter_fields(kv.data) {
            match (field.field, field.wire) {
                (KV_ID, 0) => id = read_varint_value(field.data).unwrap_or(0),
                (KV_SET_BLOB, 2) => {
                    let mut blob_id = Vec::new();
                    let mut data = Vec::new();
                    for f in iter_fields(field.data) {
                        match (f.field, f.wire) {
                            (1, 2) => blob_id = f.data.to_vec(),
                            (2, 2) => data = f.data.to_vec(),
                            _ => {}
                        }
                    }
                    kind = Some(KvKind::Set { blob_id, data });
                }
                (KV_GET_BLOB, 2) => {
                    let blob_id = iter_fields(field.data)
                        .find(|f| f.field == 1 && f.wire == 2)
                        .map(|f| f.data.to_vec())
                        .unwrap_or_default();
                    kind = Some(KvKind::Get { blob_id });
                }
                _ => {}
            }
        }
        if let Some(kind) = kind {
            return Some(KvServerMessage { id, kind });
        }
    }
    None
}

fn kv_error(message: &str) -> Vec<u8> {
    field_str(1, message)
}

/// Duplex body frame: `AgentClientMessage.kv_client_message {id, set_blob_result {error?}}`.
pub fn kv_set_result_frame(id: u64, error: Option<&str>) -> Bytes {
    let mut msg = field_varint(KV_ID, id);
    let result = match error {
        Some(e) => field_ld(1, &kv_error(e)),
        None => Vec::new(),
    };
    msg.extend(field_ld(KV_SET_BLOB, &result));
    connect_frame(&field_ld(AGENT_CLIENT_KV, &msg), 0)
}

/// Duplex body frame: `AgentClientMessage.kv_client_message {id, get_blob_result {blob_data | error}}`.
pub fn kv_get_result_frame(id: u64, data: Option<&[u8]>) -> Bytes {
    let mut msg = field_varint(KV_ID, id);
    let result = match data {
        Some(d) => field_ld(1, d),
        None => field_ld(2, &kv_error("blob not held by this client")),
    };
    msg.extend(field_ld(KV_GET_BLOB, &result));
    connect_frame(&field_ld(AGENT_CLIENT_KV, &msg), 0)
}

/// Ids from `AgentServerMessage.exec_server_message` (`f2`) when the oneof is `request_context_args` (`f10`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestContextExec {
    pub id: u64,
    pub exec_id: Option<String>,
}

fn read_varint_value(buf: &[u8]) -> Option<u64> {
    read_varint(buf).map(|(v, _)| v)
}

/// `f2.f10` handshake (no `tool_` id). Empty when the frame is not a request-context exec.
pub fn extract_request_context_exec(payload: &[u8]) -> Option<RequestContextExec> {
    for esm in iter_fields(payload) {
        if esm.field != 2 || esm.wire != 2 {
            continue;
        }
        let mut id = 0u64;
        let mut exec_id = None;
        let mut is_request_context = false;
        for field in iter_fields(esm.data) {
            match (field.field, field.wire) {
                (EXEC_CLIENT_ID, 0) => {
                    if let Some(v) = read_varint_value(field.data) {
                        id = v;
                    }
                }
                (EXEC_CLIENT_EXEC_ID, 2) => {
                    exec_id = std::str::from_utf8(field.data).ok().map(str::to_string);
                }
                (EXEC_REQUEST_CONTEXT, 2) => is_request_context = true,
                _ => {}
            }
        }
        if is_request_context {
            return Some(RequestContextExec { id, exec_id });
        }
    }
    None
}

const DENY_REASON_PREFIX: &str =
    "thin surface does not execute Cursor builtins; use declared MCP tools";

/// How many declared names a denial lists. Frame 0 has no shorter cap; 32 is the bound.
const DENY_NAMES_CAP: usize = 32;

const DENY_REASON_TAIL: &str =
    "AskQuestion, GetDynamicTools, and CallDynamicTool do not run. Call a declared tool by name.";

/// Denial text for a builtin no declared tool runs. Lists up to [`DENY_NAMES_CAP`]
/// names from frame 0, then the fixed tail.
fn deny_reason(names: &[String]) -> String {
    let list = match names.get(..names.len().min(DENY_NAMES_CAP)) {
        Some(shown) if !shown.is_empty() => shown.join(", "),
        _ => "(none)".to_string(),
    };
    format!("{DENY_REASON_PREFIX}: {list}. {DENY_REASON_TAIL}")
}

fn read_args_path(args: &[u8]) -> String {
    for field in iter_fields(args) {
        if field.field == 1 && field.wire == 2 {
            return String::from_utf8_lossy(field.data).into_owned();
        }
    }
    String::new()
}

fn exec_client_prefix(id: u64, exec_id: Option<&str>) -> Vec<u8> {
    let mut msg = field_varint(EXEC_CLIENT_ID, id);
    if let Some(exec_id) = exec_id.filter(|s| !s.is_empty()) {
        msg.extend(field_str(EXEC_CLIENT_EXEC_ID, exec_id));
    }
    msg
}

/// `ReadResult.rejected` (`f3`) with `ReadRejected { path=1, reason=2 }`.
pub fn read_rejected_frame(id: u64, exec_id: Option<&str>, path: &str, reason: &str) -> Bytes {
    let mut rejected = field_str(1, path);
    rejected.extend(field_str(2, reason));
    let mut msg = exec_client_prefix(id, exec_id);
    msg.extend(field_ld(EXEC_READ, &field_ld(3, &rejected)));
    connect_frame(&field_ld(2, &msg), 0)
}

/// `ReadResult.file_not_found` (`f4`) with `ReadFileNotFound { path=1 }`: what
/// `cursor-agent`'s own read executor answers on `ENOENT` (2026.09.28 bundle descriptors;
/// `rejected` is its user-denial shape).
pub fn read_file_not_found_frame(id: u64, exec_id: Option<&str>, path: &str) -> Bytes {
    let mut msg = exec_client_prefix(id, exec_id);
    msg.extend(field_ld(EXEC_READ, &field_ld(4, &field_str(1, path))));
    connect_frame(&field_ld(2, &msg), 0)
}

/// Which `ReadResult` answers a built-in read of a missing file on the duplex (G1, #175).
/// `serve` always answers [`ReadMissShape::FileNotFound`]; only `thickapi capture`'s F1
/// `create` scenario picks [`ReadMissShape::Rejected`], to record what that shape does.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ReadMissShape {
    /// `ReadResult.rejected { path, reason: "file not found" }` (`f3`). Measured F1 (#181):
    /// Cursor's edit ends with an empty `EditResult` and the model falls back to shell.
    Rejected,
    /// `ReadResult.file_not_found { path }` (`f4`), the CLI's own answer on ENOENT. Measured
    /// F1: Cursor's edit goes on to `f3 write_args` for the same path.
    #[default]
    FileNotFound,
}

/// `ReadRejected.reason` for a read of a missing file ([`ReadMissShape::Rejected`]).
pub const READ_MISS_REASON: &str = "file not found";

/// The duplex answer to a read of a missing file, in `shape`.
pub fn read_miss_frame(shape: ReadMissShape, id: u64, exec_id: Option<&str>, path: &str) -> Bytes {
    match shape {
        ReadMissShape::Rejected => read_rejected_frame(id, exec_id, path, READ_MISS_REASON),
        ReadMissShape::FileNotFound => read_file_not_found_frame(id, exec_id, path),
    }
}

/// `ReadResult.success` (`f1`) with `ReadSuccess { path=1, content=2, total_lines=3,
/// file_size=4 }` (`truncated=6` left false): what `cursor-agent`'s own read executor answers for a text
/// file (2026.10.01 bundle). Sent only for the pre-read of Cursor's edit built-in (#248), whole,
/// never truncated.
pub fn read_success_frame(id: u64, exec_id: Option<&str>, path: &str, content: &str) -> Bytes {
    let mut success = field_str(1, path);
    success.extend(field_str(2, content));
    success.extend(field_varint(3, content.lines().count() as u64));
    success.extend(field_varint(4, content.len() as u64));
    let mut msg = exec_client_prefix(id, exec_id);
    msg.extend(field_ld(EXEC_READ, &field_ld(1, &success)));
    connect_frame(&field_ld(2, &msg), 0)
}

/// `ReadArgs.path` of a built-in read, when it names one.
pub fn builtin_read_path(args: &[u8]) -> Option<String> {
    proto_str(args, 1).filter(|s| !s.is_empty())
}

/// `ReadArgs.tool_call_id` of a built-in read: the id of the model's tool call that asked
/// for it. For the pre-read of Cursor's edit built-in it is the edit's own call id (#248).
pub fn builtin_read_call_id(args: &[u8]) -> Option<String> {
    proto_str(args, 2).filter(|s| !s.is_empty())
}

/// The largest file the edit pre-read answers in protocol (#248). Above it, or for a file
/// that is not UTF-8, the read goes to Pi as before.
pub const EDIT_PRE_READ_MAX_BYTES: u64 = 1024 * 1024;

/// The text of a built-in read's target, resolved under `cwd` as Pi resolves it, when it is
/// a UTF-8 file of at most [`EDIT_PRE_READ_MAX_BYTES`].
pub fn read_target_text(cwd: &str, path: &str) -> Option<String> {
    let target = resolve_like_pi(cwd, path);
    let meta = std::fs::metadata(&target).ok()?;
    if !meta.is_file() || meta.len() > EDIT_PRE_READ_MAX_BYTES {
        return None;
    }
    String::from_utf8(std::fs::read(&target).ok()?).ok()
}

/// A built-in read's target does not exist: `path` resolved against the request `cwd` the
/// way Pi's `read` resolves it ([`resolve_like_pi`]).
pub fn read_target_missing(cwd: &str, path: &str) -> bool {
    !resolve_like_pi(cwd, path).exists()
}

/// `path` as Pi's tools resolve it against `cwd` (`resolveToCwd` in pi-coding-agent's
/// `utils/paths.js`): Unicode spaces become plain spaces, a leading `@` is dropped, on
/// Windows a Git Bash / MSYS / Cygwin / WSL drive path (`/c/x`, `/mnt/c/x`,
/// `/cygdrive/c/x`) becomes `C:\x`, `~` is the home directory, and an absolute path
/// (`C:\x` or `C:/x` on Windows) stands alone; anything else is under `cwd`.
pub fn resolve_like_pi(cwd: &str, path: &str) -> std::path::PathBuf {
    use std::path::{Path, PathBuf};
    let spaced: String = path
        .chars()
        .map(|c| match c {
            '\u{00A0}' | '\u{2000}'..='\u{200A}' | '\u{202F}' | '\u{205F}' | '\u{3000}' => ' ',
            c => c,
        })
        .collect();
    let mut path = spaced.strip_prefix('@').unwrap_or(&spaced).to_string();
    if cfg!(windows) {
        path = windows_shell_path(&path).unwrap_or(path);
    }
    let home = || std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" });
    if path == "~" {
        if let Some(home) = home() {
            return PathBuf::from(home);
        }
    }
    let tilde_rest = path
        .strip_prefix("~/")
        .or_else(|| path.strip_prefix("~\\").filter(|_| cfg!(windows)));
    if let (Some(rest), Some(home)) = (tilde_rest, home()) {
        return Path::new(&home).join(rest);
    }
    let target = Path::new(&path);
    if target.is_absolute() {
        target.to_path_buf()
    } else {
        Path::new(cwd).join(target)
    }
}

/// Pi's `normalizeWindowsShellPath`: `/c/x`, `/mnt/c/x` or `/cygdrive/c/x` → `C:\x`.
fn windows_shell_path(path: &str) -> Option<String> {
    if !path.starts_with('/') || path.starts_with("//") || path.contains('\\') {
        return None;
    }
    let rest = &path[1..];
    let rest = rest
        .strip_prefix("mnt/")
        .or_else(|| rest.strip_prefix("cygdrive/"))
        .unwrap_or(rest);
    let mut chars = rest.chars();
    let drive = chars.next().filter(char::is_ascii_alphabetic)?;
    let tail = chars.as_str();
    let suffix = match tail.strip_prefix('/') {
        Some(suffix) => suffix.replace('/', "\\"),
        None if tail.is_empty() => String::new(),
        None => return None,
    };
    Some(format!("{}:\\{suffix}", drive.to_ascii_uppercase()))
}

/// `GrepResult.error` (`f2`) with `GrepError { error=1 }`.
pub fn grep_error_frame(id: u64, exec_id: Option<&str>, error: &str) -> Bytes {
    let mut msg = exec_client_prefix(id, exec_id);
    msg.extend(field_ld(EXEC_GREP, &field_ld(2, &field_str(1, error))));
    connect_frame(&field_ld(2, &msg), 0)
}

/// `AgentClientMessage.exec_client_control_message` throw, then stream_close.
fn exec_throw_frames(id: u64, error: &str) -> Vec<Bytes> {
    let mut throw = field_varint(1, id);
    throw.extend(field_str(2, error));
    let throw_frame = connect_frame(
        &field_ld(AGENT_CLIENT_CONTROL, &field_ld(CONTROL_THROW, &throw)),
        0,
    );
    vec![throw_frame, stream_close_frame(id)]
}

/// `AgentClientMessage.exec_client_control_message.stream_close {id}`: the end of exec `id`'s
/// client stream. `cursor-agent` writes it after the exec's last result, never before
/// (`for await (… of results) write(r); write(streamClose{id})`, 2026.09.28 bundle). Sent
/// before the result, it ends the exec with none: `internal: No exec result` (H2, #186).
pub fn stream_close_frame(id: u64) -> Bytes {
    connect_frame(
        &field_ld(
            AGENT_CLIENT_CONTROL,
            &field_ld(CONTROL_STREAM_CLOSE, &field_varint(1, id)),
        ),
        0,
    )
}

/// `ExecServerMessage.write_args` (`WriteArgs { path=1, file_text=2 }`).
const EXEC_WRITE: u64 = 3;
/// `ExecServerMessage.ls_args` (`LsArgs { path=1 }`).
const EXEC_LS: u64 = 8;
/// `ExecServerMessage.shell_args` / `shell_stream_args`, both `ShellArgs { command=1 }`.
const EXEC_SHELL: u64 = 2;
const EXEC_SHELL_STREAM: u64 = 14;

/// The tool names a request declared on frame 0 (`McpTools`): the only names a Cursor
/// built-in may be handed to Pi as (SPEC §19.8, G2 #182).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DeclaredTools(Vec<String>);

impl DeclaredTools {
    pub fn new<I, S>(names: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self(names.into_iter().map(Into::into).collect())
    }

    /// The names of the tools frame 0 declares.
    pub fn from_tools(tools: &[McpTool]) -> Self {
        Self::new(tools.iter().map(|t| t.name.clone()))
    }

    pub fn contains(&self, name: &str) -> bool {
        self.0.iter().any(|n| n == name)
    }

    pub fn names(&self) -> &[String] {
        &self.0
    }
}

/// What to do with one Cursor built-in exec ([`route_builtin`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BuiltinRoute {
    /// Hand it to Pi as this declared tool: the turn ends as that tool call and the next
    /// request carries the result.
    ToPi { name: String, arguments: String },
    /// A read (`f7`) of a file that does not exist (G1, #175): answer it on the duplex
    /// with `ReadResult.file_not_found { path }` ([`read_miss_frame`]) and let the Run go on.
    /// When the read is the pre-read of Cursor's edit built-in, the write exec follows.
    ReadMissing { path: String },
    /// The pre-read of Cursor's edit built-in, of a file that exists (#248): answer it on
    /// the duplex with `ReadResult.success` and the file's text ([`read_success_frame`]) and
    /// let the Run go on to the edit's write exec. Handed to Pi, the edit never gets past
    /// it: the turn ends, Cursor starts the edit over and pre-reads again.
    EditPreRead { path: String, content: String },
    /// No declared tool runs it (or its args name nothing): answer it on the duplex
    /// ([`deny_builtin_frames`]) and let the Run go on, so the model can fall back to a
    /// declared MCP tool.
    Deny,
    /// A shell command that only checks the channel (`python -c "print('ok')"`, `echo ok`).
    /// Answered on the duplex ([`probe_refuse_frames`]) so Pi never runs it and never
    /// returns `ok`. The Run continues and the model can send the real command.
    NoopProbe { command: String },
}

/// The one decision for a Cursor built-in (SPEC §19.8, G2 #182, G1 #175). A read whose
/// target `read_missing` says is absent is [`BuiltinRoute::ReadMissing`], whatever the
/// request declared (Pi's ENOENT as tool-result text only makes Cursor read again, 55 times
/// in the 29 Sep smoke). Otherwise the first option in the table below that the request
/// declared, else [`BuiltinRoute::Deny`]. Never a tool name the request did not declare,
/// and nothing is run here.
///
/// | exec | 1st | fallback |
/// |---|---|---|
/// | `f7` read | `read` | `bash` `cat -- '<path>'` |
/// | `f5` grep with a pattern | `grep` | `bash` `grep -rnE [-i] [--include='<glob>'] -- '<pattern>' '<path>'` |
/// | `f5` glob only | `find` | `bash` `find '<path>' -name '<glob>'` |
/// | `f8` ls | `ls` | `bash` `ls -la -- '<path>'` |
/// | `f3` write | `write` | — |
/// | `f2` / `f14` shell | `bash` | — |
///
/// Pi's `bash` is Git bash on Windows, so the fallbacks are POSIX and single-quote every
/// argument; paths go through as Cursor sent them. `read_missing` is the existence check
/// (live: [`read_target_missing`] under the request cwd); it is asked only about reads.
pub fn route_builtin(
    field: u64,
    args: &[u8],
    declared: &DeclaredTools,
    read_missing: &dyn Fn(&str) -> bool,
) -> BuiltinRoute {
    if field == EXEC_READ {
        if let Some(path) = builtin_read_path(args).filter(|p| read_missing(p)) {
            return BuiltinRoute::ReadMissing { path };
        }
    }
    if field == EXEC_SHELL || field == EXEC_SHELL_STREAM {
        if let Some(command) = proto_str(args, 1).filter(|command| is_noop_probe(command)) {
            return BuiltinRoute::NoopProbe { command };
        }
    }
    builtin_options(field, args)
        .into_iter()
        .find(|(name, _)| declared.contains(name))
        .map_or(BuiltinRoute::Deny, |(name, value)| BuiltinRoute::ToPi {
            name: name.to_string(),
            arguments: value.to_string(),
        })
}

/// Every Pi tool call that would run this built-in, in preference order. Empty when the
/// field is not one Pi can run, or the args do not name a path / pattern / command.
fn builtin_options(field: u64, args: &[u8]) -> Vec<(&'static str, Value)> {
    let text = |n| proto_str(args, n).filter(|s| !s.is_empty());
    let bash = |command: String| ("bash", serde_json::json!({ "command": command }));
    match field {
        EXEC_READ => {
            let Some(path) = text(1) else {
                return Vec::new();
            };
            let cat = format!("cat -- {}", sh_quote(&path));
            vec![("read", serde_json::json!({ "path": path })), bash(cat)]
        }
        EXEC_GREP => {
            let path = text(2);
            let glob = text(3);
            let dir = sh_quote(path.as_deref().unwrap_or("."));
            let mut obj = serde_json::Map::new();
            if let Some(path) = &path {
                obj.insert("path".into(), serde_json::json!(path));
            }
            if let Some(limit) = proto_varint(args, 10).filter(|n| *n > 0) {
                obj.insert("limit".into(), serde_json::json!(limit));
            }
            match (text(1), glob) {
                (Some(pattern), glob) => {
                    let ignore_case = proto_varint(args, 8) == Some(1);
                    let mut cmd = String::from("grep -rnE");
                    if ignore_case {
                        cmd.push_str(" -i");
                    }
                    if let Some(glob) = &glob {
                        cmd.push_str(&grep_include_flags(glob));
                    }
                    cmd.push_str(&format!(" -- {} {dir}", sh_quote(&pattern)));
                    obj.insert("pattern".into(), serde_json::json!(pattern));
                    if let Some(glob) = glob {
                        obj.insert("glob".into(), serde_json::json!(glob));
                    }
                    if ignore_case {
                        obj.insert("ignoreCase".into(), Value::Bool(true));
                    }
                    vec![("grep", Value::Object(obj)), bash(cmd)]
                }
                // No pattern, only a glob (measured 2026-09-22, Grok 4.7 looking for a
                // file with `output_mode: files_with_matches`): a file search, Pi's `find`.
                (None, Some(glob)) => {
                    let name = name_glob(&glob);
                    let test = if name.contains('/') {
                        format!("-path {}", sh_quote(&format!("*/{name}")))
                    } else {
                        format!("-name {}", sh_quote(name))
                    };
                    obj.insert("pattern".into(), serde_json::json!(glob));
                    vec![
                        ("find", Value::Object(obj)),
                        bash(format!("find {dir} {test}")),
                    ]
                }
                (None, None) => Vec::new(),
            }
        }
        EXEC_LS => {
            let path = text(1).unwrap_or_else(|| ".".to_string());
            let ls = format!("ls -la -- {}", sh_quote(&path));
            vec![("ls", serde_json::json!({ "path": path })), bash(ls)]
        }
        EXEC_WRITE => {
            let Some(path) = text(1) else {
                return Vec::new();
            };
            let content = proto_str(args, 2).unwrap_or_default();
            vec![(
                "write",
                serde_json::json!({ "path": path, "content": content }),
            )]
        }
        EXEC_SHELL | EXEC_SHELL_STREAM => text(1).map(bash).into_iter().collect(),
        _ => Vec::new(),
    }
}

/// The whole shell command is a channel probe, not the task. Matched on the trimmed
/// command only, so a script that mentions `print('ok')` still runs.
pub fn is_noop_probe(command: &str) -> bool {
    let command = command.trim().trim_end_matches('\r');
    if command.eq_ignore_ascii_case("echo ok")
        || command.eq_ignore_ascii_case("echo 'ok'")
        || command.eq_ignore_ascii_case("echo \"ok\"")
    {
        return true;
    }
    const PROBES: &[&str] = &[
        "python -c \"print('ok')\"",
        "python -c 'print(\"ok\")'",
        "python3 -c \"print('ok')\"",
        "python3 -c 'print(\"ok\")'",
    ];
    PROBES.contains(&command)
}

/// Duplex refusal for [`BuiltinRoute::NoopProbe`]: throw, then `stream_close`, with a reason
/// that names the command and tells the model to send the real one.
pub fn probe_refuse_frames(id: u64, command: &str) -> Vec<Bytes> {
    exec_throw_frames(
        id,
        &format!(
            "Refused. `{command}` does not read or change files. Call the tool that does the \
             task. Do not send this probe again."
        ),
    )
}

/// `grep --include` flags for one glob. `*.{rs,json}` (each suffix `[A-Za-z0-9]+`)
/// becomes one `--include='*.suffix'` per suffix. Any other glob is a single
/// `--include` through [`name_glob`].
fn grep_include_flags(glob: &str) -> String {
    if let Some(suffixes) = brace_suffixes(glob) {
        let mut out = String::new();
        for suffix in suffixes {
            out.push_str(" --include=");
            out.push_str(&sh_quote(&format!("*.{suffix}")));
        }
        return out;
    }
    format!(" --include={}", sh_quote(name_glob(glob)))
}

/// Suffixes of `*.{a,b,c}` when every suffix is `[A-Za-z0-9]+`. `None` otherwise.
fn brace_suffixes(glob: &str) -> Option<Vec<&str>> {
    let inner = glob.strip_prefix("*.{")?.strip_suffix('}')?;
    if inner.is_empty() || inner.contains('{') || inner.contains('}') {
        return None;
    }
    let suffixes: Vec<&str> = inner.split(',').collect();
    if suffixes.is_empty() || suffixes.iter().any(|s| !suffix_token(s)) {
        return None;
    }
    Some(suffixes)
}

fn suffix_token(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_alphanumeric())
}

/// A glob as `find -name` / `grep --include` read it: they match the file name, so a
/// leading `**/` (any depth, what the model means) is dropped.
fn name_glob(glob: &str) -> &str {
    let mut out = glob;
    while let Some(rest) = out.strip_prefix("**/") {
        out = rest;
    }
    out
}

/// One POSIX shell word: single-quoted, an embedded `'` closed, escaped and reopened.
pub(super) fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// The tool names a Cursor request body declares: frame 0's `run_request` (`f1`)
/// `McpTools` (`f4`) `tools[].name`. Empty when the body does not open with a run request
/// (fixture replay reads the captured request with it).
pub fn declared_tools_in_body(body: &[u8]) -> DeclaredTools {
    let Some(len) = body
        .get(1..5)
        .and_then(|b| <[u8; 4]>::try_from(b).ok())
        .map(|b| u32::from_be_bytes(b) as usize)
    else {
        return DeclaredTools::default();
    };
    let Some(frame0) = body.get(5..5 + len) else {
        return DeclaredTools::default();
    };
    let mut names = Vec::new();
    for run in iter_fields(frame0).filter(|f| f.field == 1 && f.wire == 2) {
        for tools in iter_fields(run.data).filter(|f| f.field == 4 && f.wire == 2) {
            for tool in iter_fields(tools.data).filter(|f| f.field == 1 && f.wire == 2) {
                if let Some(name) = proto_str(tool.data, 1) {
                    names.push(name);
                }
            }
        }
    }
    DeclaredTools(names)
}

fn proto_str(buf: &[u8], n: u64) -> Option<String> {
    for field in iter_fields(buf) {
        if field.field == n && field.wire == 2 {
            return std::str::from_utf8(field.data).ok().map(str::to_string);
        }
    }
    None
}

fn proto_varint(buf: &[u8], n: u64) -> Option<u64> {
    for field in iter_fields(buf) {
        if field.field == n && field.wire == 0 {
            return read_varint(field.data).map(|(v, _)| v);
        }
    }
    None
}

/// Duplex denial for a documented Cursor builtin no declared tool runs. Read and
/// grep get a typed result; other fields throw so the server does not wait on a
/// missing oneof.
pub fn deny_builtin_frames(
    id: u64,
    exec_id: Option<&str>,
    field: u64,
    args: &[u8],
    declared: &[String],
) -> Vec<Bytes> {
    let reason = deny_reason(declared);
    match field {
        EXEC_READ => vec![read_rejected_frame(
            id,
            exec_id,
            &read_args_path(args),
            &reason,
        )],
        EXEC_GREP => vec![grep_error_frame(id, exec_id, &reason)],
        _ => exec_throw_frames(id, &reason),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn grep_command(glob: &str) -> String {
        let args = field_str(1, "pat")
            .into_iter()
            .chain(field_str(3, glob))
            .collect::<Vec<u8>>();
        let opts = builtin_options(EXEC_GREP, &args);
        let bash = opts.iter().find(|(name, _)| *name == "bash");
        let Some((_, value)) = bash else {
            panic!("grep builtin has no bash fallback");
        };
        value["command"]
            .as_str()
            .map(str::to_string)
            .unwrap_or_default()
    }

    #[test]
    fn noop_probe_is_not_handed_to_pi() {
        assert!(is_noop_probe("python -c \"print('ok')\""));
        assert!(is_noop_probe("  echo ok\n"));
        assert!(!is_noop_probe(
            "python -c \"import pathlib; print(pathlib.Path('a').read_text())\""
        ));
        let args = field_str(1, "python -c \"print('ok')\"");
        let declared = DeclaredTools::new(["bash"]);
        assert_eq!(
            route_builtin(EXEC_SHELL_STREAM, &args, &declared, &|_| false),
            BuiltinRoute::NoopProbe {
                command: "python -c \"print('ok')\"".into(),
            }
        );
        let real = field_str(1, "python -c \"print(1+1)\"");
        assert!(matches!(
            route_builtin(EXEC_SHELL, &real, &declared, &|_| false),
            BuiltinRoute::ToPi { name, .. } if name == "bash"
        ));
    }

    #[test]
    fn grep_include_flags_expands_brace_suffixes() {
        let cmd = grep_command("*.{rs,json,md,ts}");
        assert_eq!(
            cmd,
            "grep -rnE --include='*.rs' --include='*.json' --include='*.md' --include='*.ts' -- 'pat' '.'"
        );
        assert!(!cmd.contains('{'), "{cmd}");
    }

    #[test]
    fn grep_include_flags_keeps_plain_glob() {
        assert_eq!(
            grep_command("*.rs"),
            "grep -rnE --include='*.rs' -- 'pat' '.'"
        );
    }

    #[test]
    fn deny_reason_lists_declared_names_and_tail() {
        let reason = deny_reason(&["read".into(), "bash".into()]);
        assert!(reason.starts_with(DENY_REASON_PREFIX), "{reason}");
        assert!(reason.contains("read, bash"), "{reason}");
        assert!(reason.ends_with(DENY_REASON_TAIL), "{reason}");
        assert_eq!(
            deny_reason(&[]),
            format!("{DENY_REASON_PREFIX}: (none). {DENY_REASON_TAIL}")
        );
    }
}
