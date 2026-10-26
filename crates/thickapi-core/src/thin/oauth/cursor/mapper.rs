use std::time::Duration;

use bytes::Bytes;
use serde_json::Value;

use crate::thin::openai::{self, FinishReason, Usage};
use crate::thin::{ThinError, ThinEvent};

use super::super::state::redact_log;
use super::super::vendor::{MapperCore, StreamBudgets, WireMapper};
use super::connect::{gunzip, parse_connect_error, ConnectError, Frame, FrameDecoder};
use super::pb::{decode_protobuf_value, iter_fields, read_varint};
use super::request::{
    builtin_read_call_id, builtin_read_path, deny_builtin_frames, extract_kv_server_message,
    extract_mcp_state_exec, extract_request_context_exec, kv_get_result_frame, kv_set_result_frame,
    mcp_state_result_frame, read_miss_frame, read_success_frame, read_target_text,
    request_context_result_frame, route_builtin, BuiltinRoute, DeclaredTools, KvKind, McpTool,
    ReadMissShape,
};
use super::{
    classify_connect, CursorBudgets, ReadMiss, ReadMissCheck, ReadText, BUILTIN_EXEC_FIELDS,
    DEFAULT_SHELL, EXEC_CLIENT_EXEC_ID, EXEC_CLIENT_ID, EXEC_READ, EXEC_REQUEST_CONTEXT, FLAG_END,
    FLAG_GZIP, HEARTBEAT_ONLY_TIMEOUT, UNRECOGNISED_FRAME_LIMIT, VENDOR,
};

/// `f1.f<inner>.f1` string leaves in a frame, concatenated (H3: every leaf, not the first).
fn extract_nested_text(payload: &[u8], inner: u64) -> Option<String> {
    let mut out = String::new();
    for f1 in iter_fields(payload) {
        if f1.field != 1 || f1.wire != 2 {
            continue;
        }
        for mid in iter_fields(f1.data) {
            if mid.field != inner || mid.wire != 2 {
                continue;
            }
            for leaf in iter_fields(mid.data) {
                if leaf.field == 1 && leaf.wire == 2 {
                    if let Ok(text) = std::str::from_utf8(leaf.data) {
                        if !text.is_empty() {
                            out.push_str(text);
                        }
                    }
                }
            }
        }
    }
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

pub fn extract_answer_text(payload: &[u8]) -> Option<String> {
    extract_nested_text(payload, 1)
}

pub fn extract_reasoning_text(payload: &[u8]) -> Option<String> {
    extract_nested_text(payload, 4)
}

/// Bridged MCP tool calls: `f2 (ExecServerMessage) . f11 (McpArgs)` → `(name, arguments JSON,
/// wire id)`. Every `f11` in the frame (issue #21 capped at one).
pub fn extract_tool_calls(payload: &[u8]) -> Vec<(String, String, Option<String>)> {
    let mut out = Vec::new();
    for esm in iter_fields(payload) {
        if esm.field != 2 || esm.wire != 2 {
            continue;
        }
        for args in iter_fields(esm.data) {
            if args.field == 11 && args.wire == 2 {
                if let Some(call) = decode_mcp_args(args.data) {
                    out.push(call);
                }
            }
        }
    }
    out
}

pub fn extract_tool_call(payload: &[u8]) -> Option<(String, String, Option<String>)> {
    extract_tool_calls(payload).into_iter().next()
}

/// A call through one of Cursor's own tools (any `f2.*` other than 11 carrying a `tool_<…>`
/// id). Returns the field number. Read, grep, ls, write and shell become a declared Pi tool
/// call ([`route_builtin`]). Other documented fields, and those no declared tool runs, are
/// skipped (duplex denial); unknown fields fail the turn.
pub fn extract_unbridged_builtin_call(payload: &[u8]) -> Option<u64> {
    for esm in iter_fields(payload) {
        if esm.field != 2 || esm.wire != 2 {
            continue;
        }
        for args in iter_fields(esm.data) {
            if args.field == 11 || args.field == EXEC_REQUEST_CONTEXT || args.wire != 2 {
                continue;
            }
            if contains_tool_id(args.data, 0) {
                return Some(args.field);
            }
        }
    }
    None
}

/// Documented Cursor builtin on `ExecServerMessage` (id / exec_id / oneof + args).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuiltinExec {
    pub id: u64,
    pub exec_id: Option<String>,
    pub field: u64,
    pub args: Vec<u8>,
}

/// `f2` envelope with a documented builtin oneof carrying a `tool_` id (not MCP, not handshake).
pub fn extract_builtin_exec(payload: &[u8]) -> Option<BuiltinExec> {
    for esm in iter_fields(payload) {
        if esm.field != 2 || esm.wire != 2 {
            continue;
        }
        let mut id = 0u64;
        let mut exec_id = None;
        let mut found = None;
        for field in iter_fields(esm.data) {
            match (field.field, field.wire) {
                (EXEC_CLIENT_ID, 0) => {
                    if let Some((v, _)) = read_varint(field.data) {
                        id = v;
                    }
                }
                (EXEC_CLIENT_EXEC_ID, 2) => {
                    exec_id = std::str::from_utf8(field.data).ok().map(str::to_string);
                }
                (n, 2)
                    if found.is_none()
                        && BUILTIN_EXEC_FIELDS.contains(&n)
                        && contains_tool_id(field.data, 0) =>
                {
                    found = Some((n, field.data.to_vec()));
                }
                _ => {}
            }
        }
        if let Some((field, args)) = found {
            return Some(BuiltinExec {
                id,
                exec_id,
                field,
                args,
            });
        }
    }
    None
}

/// A tool-call id as the models on Cursor mint them: Cursor's own `tool_<uuid>` (MCP),
/// Claude's `toolu_<id>` (Fable built-in read, measured 2026-09-22) and Grok's
/// `call-<uuid>-<n>` + newline + `fc_<id>` (Grok 4.7 built-in grep, same day). Only `tool_`
/// used to count, so a built-in exec from either model went unanswered and the turn
/// stalled (#114). Exactly those three prefixes: a looser `tool*` / `call*` would turn any
/// string on an unknown `f2.*` field (a span name, a hook context) into no-retry drift.
fn is_tool_call_id(data: &[u8]) -> bool {
    data.starts_with(b"tool_") || data.starts_with(b"toolu_") || data.starts_with(b"call-")
}

fn contains_tool_id(buf: &[u8], depth: usize) -> bool {
    for field in iter_fields(buf) {
        if field.wire != 2 {
            continue;
        }
        if is_tool_call_id(field.data) {
            return true;
        }
        if depth < 8 && contains_tool_id(field.data, depth + 1) {
            return true;
        }
    }
    false
}

fn preview_bytes(data: &[u8]) -> String {
    const MAX: usize = 80;
    if let Ok(s) = std::str::from_utf8(data) {
        if s.chars().all(|c| !c.is_control() || c == '\n' || c == '\t') {
            let trimmed: String = s.chars().take(MAX).collect();
            return if s.chars().count() > MAX {
                format!("{trimmed}…")
            } else {
                trimmed
            };
        }
    }
    let n = data.len().min(24);
    format!(
        "<{} bytes {}>",
        data.len(),
        hex_preview(data.get(..n).unwrap_or(&[]))
    )
}

fn hex_preview(data: &[u8]) -> String {
    data.iter()
        .map(|b| format!("{b:02x}"))
        .collect::<Vec<_>>()
        .join(" ")
}

fn preview_field(wire: u8, data: &[u8]) -> String {
    match wire {
        0 => match read_varint(data) {
            Some((v, _)) => format!("varint {v}"),
            None => "varint ?".into(),
        },
        2 => preview_bytes(data),
        other => format!("wire {other} {} bytes", data.len()),
    }
}

/// Nested field tree of one `AgentServerMessage` payload (capture dumps, SPEC C6).
pub fn describe_exec_payload(payload: &[u8]) -> String {
    let mut lines = Vec::new();
    for top in iter_fields(payload) {
        lines.push(format!(
            "f{} {} {}",
            top.field,
            preview_field(top.wire, top.data),
            if top.wire == 2 {
                format!("({} bytes)", top.data.len())
            } else {
                String::new()
            }
        ));
        if top.wire != 2 {
            continue;
        }
        for mid in iter_fields(top.data) {
            lines.push(format!(
                "  f{}.f{} {}",
                top.field,
                mid.field,
                preview_field(mid.wire, mid.data)
            ));
            if mid.wire != 2 {
                continue;
            }
            for leaf in iter_fields(mid.data) {
                lines.push(format!(
                    "    f{}.f{}.f{} {}",
                    top.field,
                    mid.field,
                    leaf.field,
                    preview_field(leaf.wire, leaf.data)
                ));
            }
        }
    }
    lines.join("\n")
}

/// Decode Connect body chunks and describe every exec-shaped payload (field 7 capture).
pub fn dump_connect_exec(chunks: &[Vec<u8>]) -> String {
    let mut decoder = FrameDecoder::default();
    let mut out = String::new();
    for (i, chunk) in chunks.iter().enumerate() {
        let frames = match decoder.push(chunk) {
            Ok(frames) => frames,
            Err(e) => {
                out.push_str(&format!("chunk {i}: decode error {e}\n"));
                continue;
            }
        };
        for (j, frame) in frames.into_iter().enumerate() {
            if frame.flags & FLAG_END != 0 {
                let trailer = if frame.flags & FLAG_GZIP != 0 {
                    gunzip(&frame.payload).unwrap_or(frame.payload)
                } else {
                    frame.payload
                };
                out.push_str(&format!(
                    "chunk {i} frame {j}: END {}\n",
                    String::from_utf8_lossy(&trailer)
                ));
                continue;
            }
            let payload = if frame.flags & FLAG_GZIP != 0 {
                match gunzip(&frame.payload) {
                    Ok(p) => p,
                    Err(e) => {
                        out.push_str(&format!("chunk {i} frame {j}: gzip {e}\n"));
                        continue;
                    }
                }
            } else {
                frame.payload
            };
            let builtin = extract_unbridged_builtin_call(&payload);
            out.push_str(&format!(
                "chunk {i} frame {j}: {} bytes builtin={builtin:?}\n{}\n\n",
                payload.len(),
                describe_exec_payload(&payload)
            ));
        }
    }
    out
}

/// `McpArgs { name=1, args=2 map<string,Value>, tool_call_id=3, tool_name=5 }`.
fn decode_mcp_args(buf: &[u8]) -> Option<(String, String, Option<String>)> {
    let mut name = None;
    let mut tool_name = None;
    let mut call_id = None;
    let mut args = serde_json::Map::new();
    for field in iter_fields(buf) {
        if field.wire != 2 {
            continue;
        }
        match field.field {
            1 => name = std::str::from_utf8(field.data).ok().map(str::to_string),
            5 => tool_name = std::str::from_utf8(field.data).ok().map(str::to_string),
            3 => {
                call_id = std::str::from_utf8(field.data)
                    .ok()
                    .filter(|s| !s.is_empty() && s.len() <= 512)
                    .map(str::to_string)
            }
            2 => {
                let mut key = None;
                let mut value = None;
                for entry in iter_fields(field.data) {
                    match (entry.field, entry.wire) {
                        (1, 2) => key = std::str::from_utf8(entry.data).ok().map(str::to_string),
                        (2, 2) => value = Some(entry.data),
                        _ => {}
                    }
                }
                if let (Some(k), Some(v)) = (key, value) {
                    args.insert(k, decode_protobuf_value(v, 0));
                }
            }
            _ => {}
        }
    }
    let name = tool_name.or(name)?;
    Some((name, Value::Object(args).to_string(), call_id))
}

/// Connect frames → [`ThinEvent`]s. `Started` fires on the first `f2.f10 request_context_args`
/// (or on first text / reasoning / tool if that arrives first). A tool call or idle/terminal
/// ends the turn.
pub struct AgentMapper {
    /// Its budgets are never read: Cursor's depend on what has arrived (`idle_budget`).
    pub core: MapperCore,
    /// `requested_model` name and params that went on the Run, for Connect `not_found` (C5).
    /// `None` on offline replay (the catalogue id is not what went on the wire).
    pub wire_sent: Option<String>,
    pub got_output: bool,
    /// Prose actually emitted as `TextDelta`. A held `<tool_call` does not set this, so a
    /// fresh Run can still arm (#127, #215).
    pub got_text: bool,
    pub got_tool_call: bool,
    /// An `interaction_update` heartbeat (`f1.13`) arrived. Before answer text, the quiet
    /// budget is [`HEARTBEAT_ONLY_TIMEOUT`] instead of `first_output`.
    seen_heartbeat: bool,
    pub last_error: Option<ThinError>,
    /// Set only by [`AgentMapper::finish`]: a `MapperCore::fail` is not a successful answer.
    answered: bool,
    budgets: CursorBudgets,
    unknown_top_level: u8,
    nested_silent: u8,
    unknown_top_paths: Vec<String>,
    nested_paths: Vec<String>,
    prompt_tokens: u64,
    completion_bytes: usize,
    /// First live Run only. A reasoning-only end is still drift, not `stop`, but the
    /// error event is held so the driver can open one fresh Run before the client sees it.
    /// Never once answer text has streamed (#127).
    defer_reasoning_only: bool,
    reasoning_only_deferred: bool,
    /// First live Run only (not `thickapi capture`, which retries with a fresh recording).
    /// `internal: No exec result` before any output is held so the driver can open one
    /// fresh Run before the client sees it (H2, #186).
    hold_no_exec_result: bool,
    no_exec_result_held: bool,
    /// Tail of the answer text so far, enough to see a rendered built-in marker
    /// (`⚡ read(1)`) that arrived split across deltas. Not the `<tool_call` hold.
    text_tail: String,
    /// The first rendered built-in marker seen in answer text (#114). Such a turn is not
    /// an answer: the model wrote its tool call as text instead of making it.
    rendered_builtin: Option<String>,
    /// Thin `request_id` for the recovery log. Empty on a unit-test mapper.
    request_id: String,
    /// A trailing prefix of `<tool_call` not yet known to be the tag. Not emitted.
    tag_prefix: String,
    /// Answer text that might be an invented-availability claim. Not emitted until the
    /// turn ends and the phrase does not match (#seat tool loop).
    availability_hold: String,
    /// Prose already streamed before [`Self::availability_hold`]. A claim can start
    /// after that prose; the hold alone is not the whole answer.
    streamed_answer: String,
    /// From the first byte of `<tool_call` through the end of the answer. Not emitted.
    /// Capped at [`TEXT_TOOL_CALL_CAP`].
    tool_call_hold: String,
    /// Further answer bytes are dropped: the hold is at the cap, or a complete call
    /// already parsed inside it.
    hold_drop_rest: bool,
    /// First live Run only. An unparseable `<tool_call` with no prose `TextDelta` is held
    /// for one fresh Run, the same arm as [`Self::defer_reasoning_only_end`] (#215).
    text_tool_call_deferred: bool,
    /// The tools frame 0 declared: the only names a built-in may become (G2, #182).
    declared: DeclaredTools,
    /// Whether a built-in read's target is missing (G1, #175). A standalone mapper has no
    /// disk and says no; [`CursorWire`] resolves under the request cwd.
    read_missing: ReadMissCheck,
    /// Call ids Cursor announced as its edit built-in (`edit_tool_call`, #248): a read
    /// carrying one is that edit's pre-read.
    edit_calls: std::collections::HashSet<String>,
    /// The text an edit pre-read of an existing file is answered with (#248). A standalone
    /// mapper has no disk and says `None`; [`CursorWire`] reads under the request cwd.
    read_text: ReadText,
    /// The Run ended on a usage limit that is true of this model only (#249): the row is
    /// unavailable, the vendor is not.
    row_limited: bool,
}

impl AgentMapper {
    pub fn new(model: String) -> Self {
        Self::with_budgets(model, CursorBudgets::DEFAULT)
    }

    pub fn with_budgets(model: String, budgets: CursorBudgets) -> Self {
        Self {
            core: MapperCore::new(VENDOR, model, StreamBudgets::OFFLINE),
            wire_sent: None,
            got_output: false,
            got_text: false,
            got_tool_call: false,
            seen_heartbeat: false,
            last_error: None,
            answered: false,
            row_limited: false,
            budgets,
            unknown_top_level: 0,
            nested_silent: 0,
            unknown_top_paths: Vec::new(),
            nested_paths: Vec::new(),
            prompt_tokens: 0,
            completion_bytes: 0,
            defer_reasoning_only: false,
            reasoning_only_deferred: false,
            hold_no_exec_result: false,
            no_exec_result_held: false,
            text_tail: String::new(),
            rendered_builtin: None,
            request_id: String::new(),
            tag_prefix: String::new(),
            availability_hold: String::new(),
            streamed_answer: String::new(),
            tool_call_hold: String::new(),
            hold_drop_rest: false,
            text_tool_call_deferred: false,
            declared: DeclaredTools::default(),
            read_missing: std::sync::Arc::new(|_| false),
            edit_calls: std::collections::HashSet::new(),
            read_text: std::sync::Arc::new(|_| None),
        }
    }

    /// The tools frame 0 declared ([`route_builtin`] maps a built-in only onto these).
    pub fn declaring(mut self, declared: DeclaredTools) -> Self {
        self.declared = declared;
        self
    }

    pub fn declared(&self) -> &DeclaredTools {
        &self.declared
    }

    /// The existence check a built-in read goes through ([`route_builtin`], G1).
    pub(crate) fn reading_through(mut self, read_missing: ReadMissCheck) -> Self {
        self.read_missing = read_missing;
        self
    }

    /// Where an edit pre-read of an existing file gets its text (#248).
    pub(crate) fn reading_text(mut self, read_text: ReadText) -> Self {
        self.read_text = read_text;
        self
    }

    /// The one decision for this built-in exec: the pre-read of Cursor's edit built-in on a
    /// file that exists is [`BuiltinRoute::EditPreRead`] (#248); everything else is
    /// [`route_builtin`].
    pub fn route(&self, builtin: &BuiltinExec) -> BuiltinRoute {
        if let Some(route) = self.edit_pre_read(builtin) {
            return route;
        }
        route_builtin(
            builtin.field,
            &builtin.args,
            &self.declared,
            &*self.read_missing,
        )
    }

    /// The rendered built-in marker (`⚡ read(1)`) found in this Run's answer text, if any.
    /// A read whose `tool_call_id` Cursor announced as an edit, of a file that exists and
    /// whose text `read_text` returns. A missing file stays G1's
    /// [`BuiltinRoute::ReadMissing`]; a read the model asked for itself carries a
    /// `read_tool_call` id and still goes to Pi.
    fn edit_pre_read(&self, builtin: &BuiltinExec) -> Option<BuiltinRoute> {
        if builtin.field != EXEC_READ {
            return None;
        }
        let call_id = builtin_read_call_id(&builtin.args)?;
        if !self.edit_calls.contains(&call_id) {
            return None;
        }
        let path = builtin_read_path(&builtin.args)?;
        if (self.read_missing)(&path) {
            return None;
        }
        let content = (self.read_text)(&path)?;
        Some(BuiltinRoute::EditPreRead { path, content })
    }

    pub fn rendered_builtin(&self) -> Option<&str> {
        self.rendered_builtin.as_deref()
    }

    /// Hold a reasoning-only drift off the client event stream. The classification does
    /// not change: this is not `Finish(Stop)`.
    pub(crate) fn defer_reasoning_only_end(&mut self) {
        self.defer_reasoning_only = true;
    }

    pub(crate) fn reasoning_only_deferred(&self) -> bool {
        self.reasoning_only_deferred
    }

    pub(crate) fn row_limited(&self) -> bool {
        self.row_limited
    }

    pub(crate) fn set_request_id(&mut self, request_id: &str) {
        self.request_id = request_id.to_string();
    }

    /// An unparseable `<tool_call` was held off the client for one fresh Run (#215).
    pub(crate) fn text_tool_call_deferred(&self) -> bool {
        self.text_tool_call_deferred
    }

    /// A `<tool_call` tag is buffered and has not been emitted.
    fn holding_text_call(&self) -> bool {
        !self.tool_call_hold.is_empty()
    }

    /// Hold a handshake `No exec result` (before any output) off the client event stream.
    pub(crate) fn hold_no_exec_result(&mut self) {
        self.hold_no_exec_result = true;
    }

    pub(crate) fn no_exec_result_held(&self) -> bool {
        self.no_exec_result_held
    }

    /// How long to wait for the next response byte given what we have already seen.
    pub fn idle_budget(&self, has_tools: bool) -> Duration {
        if self.got_tool_call {
            // Turn already finishing; this is only used if finish was deferred.
            self.budgets.idle
        } else if self.got_text && has_tools {
            self.budgets.idle_after_text_with_tools
        } else if self.got_text {
            self.budgets.idle
        } else if self.seen_heartbeat && self.budgets.first_output != Duration::MAX {
            self.budgets.first_output.max(HEARTBEAT_ONLY_TIMEOUT)
        } else {
            self.budgets.first_output
        }
    }

    /// Stream went quiet: tool calls win over a truncated `stop`.
    pub fn idle_end(&mut self) {
        self.end_turn(true);
    }

    /// Body or Connect trailer ended.
    pub fn stream_closed(&mut self) {
        self.end_turn(false);
    }

    fn end_turn(&mut self, quiet: bool) {
        // A dangling prefix was not the tag. Release it as prose before classifying.
        self.release_tag_prefix();
        if self.got_tool_call {
            // A real exec already finished `tool_calls`. Discard the text hold.
            self.tool_call_hold.clear();
            self.finish(FinishReason::ToolCalls);
        } else if let Some((name, arguments)) = self.recover_text_call() {
            self.emit_recovered(name, arguments);
        } else if self.tool_call_hold.contains(TOOL_CALL_OPEN) {
            // Not `/ready` `rendered_builtin`, even if a `⚡` marker was also streamed.
            // The hold never reaches the client; log its shape so the next miss is
            // diagnosable. Cap it: a hold can be up to the 1 MiB ceiling.
            self.rendered_builtin = None;
            tracing::info!(
                request_id = %self.request_id,
                hold = %self
                    .tool_call_hold
                    .chars()
                    .take(400)
                    .collect::<String>()
                    .replace('\n', "\\n"),
                "cursor text tool call not recovered"
            );
            self.hold_or_fail(
                ThinError::text_tool_call(
                    VENDOR,
                    "cursor wrote a <tool_call> block as answer text and no declared call \
                     was recovered",
                ),
                true,
            );
        } else if self.invented_availability() {
            self.rendered_builtin = None;
            self.availability_hold.clear();
            self.hold_or_fail(
                ThinError::text_tool_call(
                    VENDOR,
                    "cursor claimed a tool is not available and no declared call was recovered",
                ),
                true,
            );
        } else if self.got_text && self.rendered_builtin.is_none() {
            self.release_availability_hold();
            self.finish(FinishReason::Stop);
        } else if self.got_output || self.rendered_builtin.is_some() {
            // Reasoning only, or answer text that is a tool call written out instead of
            // made (#114). Neither is `stop`: the client would take it as the answer.
            let err = match &self.rendered_builtin {
                Some(marker) => ThinError::vendor_drift(
                    VENDOR,
                    format!(
                        "cursor rendered a built-in tool call as answer text ({marker}) and \
                         made no tool call"
                    ),
                ),
                None => ThinError::vendor_drift(
                    VENDOR,
                    "cursor ended with reasoning and no answer text",
                ),
            };
            // Held only while no answer text has reached the client: a fresh Run after
            // streamed text would splice two answers into one message (#127). A rendered
            // built-in is answer text, so it is final on the first Run.
            self.hold_or_fail(err, false);
        } else if quiet {
            self.fail(ThinError::vendor_down(
                VENDOR,
                format!(
                    "cursor sent nothing for {}s",
                    self.idle_budget(true).as_secs()
                ),
            ));
        } else {
            self.fail(ThinError::vendor_down(
                VENDOR,
                "cursor closed the stream before sending any output",
            ));
        }
    }

    /// Record the drift without a `Finish`. With the fresh-Run arm set and no prose
    /// emitted, hold it off the client the way a reasoning-only end is held.
    fn hold_or_fail(&mut self, err: ThinError, text_tool_call: bool) {
        if self.defer_reasoning_only && !self.got_text {
            if !self.core.done {
                if text_tool_call {
                    self.text_tool_call_deferred = true;
                } else {
                    self.reasoning_only_deferred = true;
                }
                self.last_error = Some(err);
                self.core.done = true;
            }
        } else {
            self.fail(err);
        }
    }

    fn recover_text_call(&self) -> Option<(String, String)> {
        if self.declared.names().is_empty() || self.tool_call_hold.is_empty() {
            return None;
        }
        parse_text_tool_call(&self.tool_call_hold, &self.declared)
    }

    /// Cursor's field names onto the declared tool's. A call already in the declared
    /// shape is unchanged.
    fn declared_arguments(&self, name: &str, arguments: String) -> (String, String) {
        native_declared_call(name, &arguments)
            .filter(|(pi_name, _)| self.declared.contains(pi_name))
            .map(|(pi_name, args)| (pi_name.to_string(), args))
            .unwrap_or_else(|| (name.to_string(), arguments))
    }

    fn emit_recovered(&mut self, name: String, arguments: String) {
        let (name, arguments) = self.declared_arguments(&name, arguments);
        tracing::info!(
            request_id = %self.request_id,
            tool = %name,
            "cursor text tool call recovered"
        );
        self.got_tool_call = true;
        self.tool_call_hold.clear();
        self.core.emit(ThinEvent::ToolCall {
            id: openai::new_call_id(),
            name,
            arguments,
        });
        self.finish(FinishReason::ToolCalls);
    }

    /// Answer bytes. With tools declared, prose stops at `<tool_call`; the tag and
    /// everything after it is held until the turn ends.
    fn feed_answer_text(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        // Bytes are still arriving, even when none of them are emitted.
        self.core.working(text);
        if self.declared.names().is_empty() {
            self.emit_prose(text);
            return;
        }
        self.note_answer_text(text);
        if self.hold_drop_rest || !self.tool_call_hold.is_empty() {
            self.push_hold(text);
            return;
        }
        if !self.availability_hold.is_empty() {
            self.push_availability(text);
            return;
        }
        let mut pending = std::mem::take(&mut self.tag_prefix);
        pending.push_str(text);
        if let Some(at) = pending.find(TOOL_CALL_OPEN) {
            if tool_call_boundary_ok(&pending, at) {
                if let Some(prose) = pending.get(..at) {
                    self.emit_prose(prose);
                }
                if let Some(rest) = pending.get(at..) {
                    self.push_hold(rest);
                }
                return;
            }
        }
        let keep = tool_call_prefix_len(&pending);
        let emit_at = pending.len().saturating_sub(keep);
        if let Some(prose) = pending.get(..emit_at) {
            self.hold_or_emit_prose(prose);
        }
        if let Some(prefix) = pending.get(emit_at..) {
            self.tag_prefix = prefix.to_string();
        }
    }

    /// With tools declared, a leading availability claim stays off the stream until the
    /// turn ends. Prose before the claim, or a claim that does not match, is emitted.
    fn hold_or_emit_prose(&mut self, text: &str) {
        if text.is_empty() || !self.availability_hold.is_empty() {
            self.push_availability(text);
            return;
        }
        if self.got_text || self.rendered_builtin.is_some() || !availability_might_match(text) {
            self.emit_prose(text);
            return;
        }
        self.push_availability(text);
    }

    fn emit_prose(&mut self, text: &str) {
        if text.is_empty() || self.core.done {
            return;
        }
        if !self.declared.names().is_empty() {
            let mut combined = String::with_capacity(self.streamed_answer.len() + text.len());
            combined.push_str(&self.streamed_answer);
            combined.push_str(text);
            if let Some(end) = rant_cut_end(&combined) {
                let already = self.streamed_answer.len();
                if end > already {
                    let take = floor_char_boundary(text, end - already);
                    if let Some(piece) = text.get(..take) {
                        self.emit_prose_now(piece);
                    }
                }
                self.fail(ThinError::text_tool_call(
                    VENDOR,
                    "cursor claimed a tool is not available and no declared call was recovered",
                ));
                return;
            }
        }
        self.emit_prose_now(text);
    }

    fn emit_prose_now(&mut self, text: &str) {
        if text.is_empty() || self.core.done {
            return;
        }
        self.got_text = true;
        self.completion_bytes += text.len();
        self.note_answer_text(text);
        self.streamed_answer.push_str(text);
        self.core.emit(ThinEvent::TextDelta(text.to_string()));
    }

    /// Tools were offered, nothing was recovered, and the held (or streamed) answer
    /// claims a tool is not available. The phrase itself was never emitted.
    fn invented_availability(&self) -> bool {
        if self.declared.names().is_empty() || self.got_tool_call {
            return false;
        }
        let mut text = self.streamed_answer.clone();
        text.push_str(&self.availability_hold);
        availability_claim(&text)
    }

    fn push_availability(&mut self, text: &str) {
        const CAP: usize = 512;
        let room = CAP.saturating_sub(self.availability_hold.len());
        if room == 0 || text.is_empty() {
            return;
        }
        let take = floor_char_boundary(text, room.min(text.len()));
        if let Some(piece) = text.get(..take) {
            self.availability_hold.push_str(piece);
        }
    }

    /// A recap that is not an availability claim is ordinary answer text.
    fn release_availability_hold(&mut self) {
        if self.availability_hold.is_empty() {
            return;
        }
        let held = std::mem::take(&mut self.availability_hold);
        self.emit_prose(&held);
    }

    /// End of the answer: a held prefix that never became `<tool_call` is prose.
    fn release_tag_prefix(&mut self) {
        if !self.tool_call_hold.is_empty() || self.tag_prefix.is_empty() {
            return;
        }
        let prefix = std::mem::take(&mut self.tag_prefix);
        self.emit_prose(&prefix);
    }

    fn push_hold(&mut self, text: &str) {
        if text.is_empty() || self.hold_drop_rest {
            return;
        }
        let room = TEXT_TOOL_CALL_CAP.saturating_sub(self.tool_call_hold.len());
        if room == 0 {
            self.hold_drop_rest = true;
            return;
        }
        let (piece, truncated) = if text.len() <= room {
            (text, false)
        } else {
            let take = floor_char_boundary(text, room);
            match text.get(..take) {
                Some(piece) => (piece, true),
                None => {
                    self.hold_drop_rest = true;
                    return;
                }
            }
        };
        self.tool_call_hold.push_str(piece);
        if truncated {
            self.hold_drop_rest = true;
        }
        if self.tool_call_hold.contains("</tool_call>")
            && parse_text_tool_call(&self.tool_call_hold, &self.declared).is_some()
        {
            self.hold_drop_rest = true;
        }
    }

    fn ensure_started(&mut self) {
        self.core.ensure_started();
    }

    pub fn fail(&mut self, err: ThinError) {
        if !self.core.done {
            self.last_error = Some(err.clone());
        }
        self.core.fail(err);
    }

    pub fn finish(&mut self, reason: FinishReason) {
        if self.core.done {
            return;
        }
        let usage = Usage::from_parts(self.prompt_tokens, (self.completion_bytes as u64) / 4, 0);
        self.core.emit(ThinEvent::Usage(usage));
        self.core.finish(ThinEvent::Finish(reason));
        self.answered = true;
    }

    pub fn answered(&self) -> bool {
        self.answered
    }

    pub fn accept(&mut self, frame: Frame) {
        let _ = self.accept_eventful(frame);
    }

    /// Decode one Connect frame. Returns true when at least one [`ThinEvent`] was queued:
    /// keepalives and a repeat handshake ask queue none.
    pub fn accept_eventful(&mut self, frame: Frame) -> bool {
        let before = self.core.pending.len();
        if let Some(payload) = self.open(frame) {
            self.accept_payload(&payload);
        }
        self.core.pending.len() > before
    }

    /// The protobuf inside a data frame, decompressed once. `None` when the frame was the
    /// Connect terminal (handled here) or could not be opened (the turn has failed).
    fn open(&mut self, frame: Frame) -> Option<Vec<u8>> {
        if self.core.done {
            return None;
        }
        if frame.flags & FLAG_END != 0 {
            // Cursor gzips the trailer too (`0x03`): read raw, its error was lost and the
            // usage limit read as a bare close (#249).
            let trailer = if frame.flags & FLAG_GZIP != 0 {
                match gunzip(&frame.payload) {
                    Ok(p) => p,
                    Err(e) => {
                        self.fail(ThinError::vendor_drift(
                            VENDOR,
                            format!("cursor trailer gzip: {e}"),
                        ));
                        return None;
                    }
                }
            } else {
                frame.payload
            };
            match parse_connect_error(&trailer) {
                Some(err) => self.connect_error(&err),
                None => self.stream_closed(),
            }
            return None;
        }
        if frame.flags & FLAG_GZIP == 0 {
            return Some(frame.payload);
        }
        match gunzip(&frame.payload) {
            Ok(payload) => Some(payload),
            Err(e) => {
                self.fail(ThinError::vendor_drift(VENDOR, format!("cursor gzip: {e}")));
                None
            }
        }
    }

    /// A Connect error trailer. `internal: No exec result` before any text, reasoning or tool
    /// call is the handshake death (P4, #186): retryable, and on the first live Run held for
    /// one fresh Run instead of reaching the client (the driver warns when it retries). After
    /// output it is today's error after `Started`.
    fn connect_error(&mut self, connect: &ConnectError) {
        let (code, message) = (connect.code.as_str(), connect.message.as_str());
        let err = classify_connect(code, message, self.wire_sent.as_deref());
        self.row_limited = connect.row_limit && err.code == "insufficient_quota";
        let handshake = code == "internal" && message.trim() == NO_EXEC_RESULT && !self.got_output;
        if handshake && self.hold_no_exec_result {
            tracing::debug!(
                "cursor internal: No exec result before any output; held for a fresh Run"
            );
            self.no_exec_result_held = true;
            self.last_error = Some(err.with_retry());
            self.core.done = true;
            return;
        }
        tracing::warn!(%code, message = %redact_log(message), "cursor connect error");
        self.fail(if handshake { err.with_retry() } else { err });
    }

    fn accept_payload(&mut self, payload: &[u8]) {
        self.accept_exec_payload(payload, None);
    }

    /// `routed`: the decision the duplex side already made for this frame's built-in, so
    /// both sides act on one call of [`route_builtin`] (`None`: decide here).
    fn accept_exec_payload(&mut self, payload: &[u8], routed: Option<BuiltinRoute>) {
        if self.core.done {
            return;
        }
        if let Some(call_id) = edit_call_id(payload) {
            self.edit_calls.insert(call_id);
        }
        if extract_request_context_exec(payload).is_some()
            || extract_mcp_state_exec(payload).is_some()
        {
            self.ensure_started();
        }
        let calls = extract_tool_calls(payload);
        if !calls.is_empty() {
            self.ensure_started();
            self.got_output = true;
            self.got_tool_call = true;
            for (name, arguments, _wire_id) in calls {
                let (name, arguments) = self.declared_arguments(&name, arguments);
                self.core.emit(ThinEvent::ToolCall {
                    id: openai::new_call_id(),
                    name,
                    arguments,
                });
            }
            self.finish(FinishReason::ToolCalls);
            return;
        }
        if let Some(builtin) = extract_builtin_exec(payload) {
            match routed.unwrap_or_else(|| self.route(&builtin)) {
                BuiltinRoute::ToPi { name, arguments } => {
                    self.ensure_started();
                    self.got_output = true;
                    self.got_tool_call = true;
                    self.core.emit(ThinEvent::ToolCall {
                        id: openai::new_call_id(),
                        name,
                        arguments,
                    });
                    self.finish(FinishReason::ToolCalls);
                    return;
                }
                // Answered on the duplex (`ReadResult.file_not_found`): no tool call, and the
                // Run goes on to what Cursor sends next (the edit's write exec).
                BuiltinRoute::ReadMissing { .. } => {
                    self.ensure_started();
                    return;
                }
                // Answered on the duplex (`ReadResult.success`, #248): the Run goes on to
                // the edit's write exec.
                BuiltinRoute::EditPreRead { .. } => {
                    self.ensure_started();
                    return;
                }
                BuiltinRoute::Deny | BuiltinRoute::NoopProbe { .. } => {}
            }
        }
        if let Some(field) = extract_unbridged_builtin_call(payload) {
            if BUILTIN_EXEC_FIELDS.contains(&field) {
                // Denied: answered on the duplex by CursorWire (`deny_builtin_frames`). Skipping
                // lets a later MCP `f2.f11` in this stream still become a tool_call.
                self.ensure_started();
            } else {
                self.fail(
                    ThinError::vendor_drift(
                        VENDOR,
                        format!(
                            "cursor tool-call id on ExecServerMessage field {field} (not f2.f11 MCP \
                             and not a documented built-in); wire moved"
                        ),
                    )
                    .without_retry(),
                );
            }
            return;
        }
        if let Some(text) = extract_reasoning_text(payload) {
            self.ensure_started();
            self.got_output = true;
            self.completion_bytes += text.len();
            self.core.emit(ThinEvent::ReasoningDelta {
                text,
                signature: None,
                details: None,
            });
        }
        if let Some(text) = extract_answer_text(payload) {
            self.ensure_started();
            self.got_output = true;
            self.feed_answer_text(&text);
        }
        if is_interaction_heartbeat(payload) {
            self.seen_heartbeat = true;
        }
        if !self.got_output && !self.core.done {
            self.note_silent(payload);
        }
        // H1 (#185): `interaction_update` field 14 is `turn_ended`. Text answers finish
        // here (`stop`, drift when the text is a rendered built-in, or a recovered
        // `<tool_call`). A pure held tag never set `got_text`, so the tag itself also
        // finishes the turn. A `turn_ended` before any output does not. A tool call has
        // already finished `tool_calls` above.
        let answer_started = self.got_text
            || self.holding_text_call()
            || !self.tag_prefix.is_empty()
            || !self.availability_hold.is_empty();
        if !self.core.done && answer_started && is_turn_ended(payload) {
            self.end_turn(false);
        }
    }

    /// Keep a short tail of the answer and remember the first rendered built-in marker.
    fn note_answer_text(&mut self, text: &str) {
        if self.rendered_builtin.is_some() {
            return;
        }
        self.text_tail.push_str(text);
        if self.text_tail.len() > TEXT_TAIL_BYTES * 2 {
            let keep_from = self
                .text_tail
                .char_indices()
                .map(|(i, _)| i)
                .find(|&i| self.text_tail.len() - i <= TEXT_TAIL_BYTES)
                .unwrap_or(0);
            self.text_tail.drain(..keep_from);
        }
        self.rendered_builtin = rendered_builtin_marker(&self.text_tail);
    }

    fn note_silent(&mut self, payload: &[u8]) {
        if payload.is_empty() || is_keepalive(payload) {
            return;
        }
        if extract_request_context_exec(payload).is_some()
            || extract_mcp_state_exec(payload).is_some()
        {
            return;
        }
        let top: Vec<u64> = iter_fields(payload).map(|f| f.field).collect();
        if top.is_empty() {
            return;
        }
        // Healthy 2026.09.18 runs send several conversation-state `f4` envelopes
        // before the first token. They are not unknown-top-level drift.
        if is_conversation_state(&top) {
            return;
        }
        // Lone timing-metrics `f8` (64-bit doubles). Same skip as conversation-state
        // `f4`: a metrics-only frame must not eat the unknown-top-level budget.
        if is_timing_metrics(&top) {
            return;
        }
        let paths = nested_field_paths(payload);
        // Fable tool turns send these nested silent frames before MCP `f2.f11` with
        // no reasoning text. They are not ThinEvents; do not execute `f1.7` / `f1.2`.
        if is_measured_healthy_preamble(&paths) {
            return;
        }
        let nested = top.iter().any(|&f| f == 1 || f == 2);
        // Two counters: unknown nested vs unknown top-level. Adding them together
        // false-trips at 5. Each keeps its own path list so a trip names only what
        // fed it.
        let count = if nested {
            self.nested_silent = self.nested_silent.saturating_add(1);
            for path in paths {
                if !self.nested_paths.contains(&path) {
                    self.nested_paths.push(path);
                }
            }
            self.nested_silent
        } else {
            self.unknown_top_level = self.unknown_top_level.saturating_add(1);
            for path in paths {
                if !self.unknown_top_paths.contains(&path) {
                    self.unknown_top_paths.push(path);
                }
            }
            self.unknown_top_level
        };
        if count >= UNRECOGNISED_FRAME_LIMIT {
            let seen = if nested {
                self.nested_paths.join(", ")
            } else {
                self.unknown_top_paths.join(", ")
            };
            let kind = if nested {
                "nested field"
            } else {
                "unrecognised top-level field"
            };
            self.fail(ThinError::vendor_drift(
                VENDOR,
                format!(
                    "cursor sent {count} frames with no recognised event ({kind}; seen paths: \
                     {seen}) before any output"
                ),
            ));
        }
    }
}

/// Cursor's Connect `internal` message when an exec ended with no result (P4, #186).
const NO_EXEC_RESULT: &str = "No exec result";

/// How much answer text is kept to spot a marker split across deltas.
const TEXT_TAIL_BYTES: usize = 96;

/// The tag that starts a text tool call. A proper prefix of this, split across deltas,
/// is held and not emitted. The 96-byte `⚡` tail is a different buffer.
const TOOL_CALL_OPEN: &str = "<tool_call";

/// Phrases Cursor has used when it narrates a tool failure instead of calling one.
/// Measured 2026-10-02 (`tool not available`, `is not available in this session`) and
/// 2026-10-15 (`isn't available`, `unavailable`, `still down`, `dropped out`, `tools failed`
/// on `subs/cursor/grok-4.7`).
const AVAILABILITY_PHRASES: &[&str] = &[
    "tool not available",
    "is not available in this session",
    "isn't available",
    "aren't available",
    "unavailable",
    "still down",
    "tools failed",
    "dropped out",
];

/// Three claims in one answer is the retry rant. Cut there so the rest is not streamed.
const AVAILABILITY_RANT_CLAIMS: usize = 3;

/// Invented availability: tools were offered, no call was recovered, and the answer
/// claims one. Case-insensitive. A recap that matches no phrase is a normal stop.
fn availability_claim(text: &str) -> bool {
    !claim_ends(text).is_empty()
}

/// Byte index just after the third claim, in `text`'s own coordinates. ASCII phrases keep
/// the same indexes after `to_ascii_lowercase`.
fn rant_cut_end(text: &str) -> Option<usize> {
    let mut ends = claim_ends(text);
    ends.sort_unstable();
    ends.dedup();
    ends.get(AVAILABILITY_RANT_CLAIMS - 1).copied()
}

fn claim_ends(text: &str) -> Vec<usize> {
    let lower = text.to_ascii_lowercase();
    let mut ends = Vec::new();
    for phrase in AVAILABILITY_PHRASES {
        let mut from = 0;
        while let Some(at) = lower[from..].find(phrase) {
            let end = from + at + phrase.len();
            ends.push(end);
            from = end;
        }
    }
    ends
}

/// True when `text` already contains an availability phrase, or its leading trimmed
/// text is a prefix of one. A short fragment that matches neither is a recap and may
/// be streamed.
fn availability_might_match(text: &str) -> bool {
    if availability_claim(text) {
        return true;
    }
    let lower = text.to_ascii_lowercase();
    let trimmed = lower.trim_start();
    !trimmed.is_empty()
        && AVAILABILITY_PHRASES
            .iter()
            .any(|phrase| phrase.starts_with(trimmed))
}

/// Cap on the `<tool_call` hold. Over the cap with no complete call is drift; a call
/// that already parsed inside the cap is kept and the rest is dropped.
const TEXT_TOOL_CALL_CAP: usize = 1024 * 1024;

/// Cursor's own rendering of a built-in tool call, as the model writes it when it narrates
/// instead of calling (#114): `⚡ <name>(<n>)`, ANSI colour around it or not. Returns the
/// marker without the colour codes.
pub fn rendered_builtin_marker(text: &str) -> Option<String> {
    for (start, ch) in text.char_indices() {
        if ch != '⚡' {
            continue;
        }
        let rest = text
            .get(start + ch.len_utf8()..)
            .unwrap_or("")
            .trim_start_matches([' ', '\u{fe0f}']);
        let name_len = rest
            .bytes()
            .take_while(|b| b.is_ascii_alphanumeric() || *b == b'_')
            .count();
        let (Some(name), Some(after_name)) = (rest.get(..name_len), rest.get(name_len..)) else {
            continue;
        };
        if name.is_empty() {
            continue;
        }
        let Some(args) = after_name.strip_prefix('(') else {
            continue;
        };
        let digits = args.bytes().take_while(u8::is_ascii_digit).count();
        let (Some(n), Some(close)) = (args.get(..digits), args.get(digits..)) else {
            continue;
        };
        if n.is_empty() || !close.starts_with(')') {
            continue;
        }
        return Some(format!("⚡ {name}({n})"));
    }
    None
}

fn floor_char_boundary(text: &str, mut index: usize) -> usize {
    if index >= text.len() {
        return text.len();
    }
    while index > 0 && !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}

/// `at` is the byte index of [`TOOL_CALL_OPEN`] in `text`. The next character must not
/// continue the name (`<tool_callfoo` is not the tag).
fn tool_call_boundary_ok(text: &str, at: usize) -> bool {
    match text
        .get(at + TOOL_CALL_OPEN.len()..)
        .and_then(|rest| rest.chars().next())
    {
        None => true,
        Some(c) => !c.is_ascii_alphanumeric() && c != '_',
    }
}

/// Longest suffix of `text` that is a proper prefix of [`TOOL_CALL_OPEN`].
fn tool_call_prefix_len(text: &str) -> usize {
    let bytes = text.as_bytes();
    let tag = TOOL_CALL_OPEN.as_bytes();
    let max = tag.len().saturating_sub(1).min(bytes.len());
    for n in (1..=max).rev() {
        let start = bytes.len() - n;
        if let (Some(suffix), Some(prefix)) = (bytes.get(start..), tag.get(..n)) {
            if suffix == prefix {
                return n;
            }
        }
    }
    0
}

/// The first `<tool_call`…`</tool_call>` in `hold` that is one declared tool and a
/// JSON object. The model's id is ignored. A block that names a tool the request did
/// not declare (`GetDynamicTools`, `StrReplace`) is skipped, and the next block is
/// tried: one native-dialect block must not bury a declared call later in the same
/// answer. A closed block whose arguments are not an object ends the scan.
///
/// Composer also writes the call as lines and ends the turn without `</tool_call>`
/// (measured 2026-10-11 and 2026-10-12, `subs/cursor/composer-2.5`):
///
/// ```text
/// <tool_call>
/// Read
/// path
/// src/out.txt
/// ```
///
/// Turn end closes that form. A later line-block whose name is not declared
/// (`GetDynamicTools`) is skipped the same way. A body that is not this dialect
/// stays `text_tool_call`.
fn parse_text_tool_call(hold: &str, declared: &DeclaredTools) -> Option<(String, String)> {
    let mut rest = hold;
    while let Some((open_inside, body, after)) = split_first_tool_call(rest) {
        if let Some(call) = call_in_block(open_inside, body, declared) {
            return Some(call);
        }
        if xml_attr(open_inside, "name").is_none() && xml_attr(open_inside, "id").is_none() {
            if let Some(call) = parse_line_calls(body, declared) {
                return Some(call);
            }
        }
        // An undeclared name is Cursor's own dialect, not a failed call. Keep looking.
        if block_name(open_inside, body).is_some_and(|name| !declared.contains(name)) {
            rest = after;
            continue;
        }
        return None;
    }
    parse_unclosed_line_call(hold, declared)
}

/// Tool names Composer's line dialect has used, plus the Pi names a seat declares.
/// A line that is not in this list is not a call.
const LINE_TOOL_NAMES: &[&str] = &[
    "Read",
    "StrReplace",
    "Write",
    "Shell",
    "Grep",
    "Glob",
    "GetDynamicTools",
    "CallDynamicTool",
    "read",
    "write",
    "edit",
    "bash",
    "grep",
    "find",
    "ls",
    "todo",
];

/// Argument names of that dialect. One line, then the value on the next line.
const LINE_KEYS: &[&str] = &[
    "path",
    "old_string",
    "new_string",
    "oldText",
    "newText",
    "contents",
    "content",
    "command",
    "pattern",
    "glob",
    "glob_pattern",
    "target_directory",
    "namespace",
    "toolName",
];

fn is_line_tool_name(line: &str) -> bool {
    LINE_TOOL_NAMES.contains(&line)
}

fn is_line_key(line: &str) -> bool {
    LINE_KEYS.contains(&line)
}

/// A bare `<tool_call>` (no `name` or `id`) that the turn ended without closing.
fn parse_unclosed_line_call(hold: &str, declared: &DeclaredTools) -> Option<(String, String)> {
    let start = hold.find(TOOL_CALL_OPEN)?;
    if !tool_call_boundary_ok(hold, start) {
        return None;
    }
    let after_open = hold.get(start + TOOL_CALL_OPEN.len()..)?;
    let gt = after_open.find('>')?;
    let open_inside = after_open.get(..gt)?;
    if !open_inside.trim().is_empty() {
        return None;
    }
    if after_open.contains("</tool_call>") {
        return None;
    }
    let body = after_open.get(gt + 1..)?;
    parse_line_calls(body, declared)
}

/// Line blocks in `body`. The first one that maps onto a declared tool wins.
/// An unknown line before any recovered call is not this dialect.
fn parse_line_calls(body: &str, declared: &DeclaredTools) -> Option<(String, String)> {
    let lines: Vec<&str> = body
        .split('\n')
        .map(|line| line.trim().trim_end_matches('\r'))
        .collect();
    let at = |i: usize| lines.get(i).copied();
    let mut i = 0;
    while at(i).is_some_and(|line| line.is_empty()) {
        i += 1;
    }
    if !at(i).is_some_and(is_line_tool_name) {
        return None;
    }
    while at(i).is_some() {
        while at(i).is_some_and(|line| line.is_empty()) {
            i += 1;
        }
        let Some(name) = at(i) else {
            break;
        };
        if !is_line_tool_name(name) {
            return None;
        }
        i += 1;
        let mut map = serde_json::Map::new();
        let mut bad = false;
        while let Some(line) = at(i) {
            if line.is_empty() {
                break;
            }
            if is_line_tool_name(line) && !map.is_empty() {
                break;
            }
            if !is_line_key(line) {
                bad = true;
                break;
            }
            let key = line;
            i += 1;
            let Some(value) = at(i).filter(|value| !value.is_empty()) else {
                bad = true;
                break;
            };
            if map.contains_key(key) {
                bad = true;
                break;
            }
            map.insert(key.to_string(), Value::String(value.to_string()));
            i += 1;
        }
        if bad {
            return None;
        }
        if let Some(call) = as_declared_call(
            name,
            &serde_json::to_string(&Value::Object(map)).ok()?,
            declared,
        ) {
            return Some(call);
        }
    }
    None
}

/// One closed block's declared call, if its arguments are a JSON object.
fn call_in_block(
    open_inside: &str,
    body: &str,
    declared: &DeclaredTools,
) -> Option<(String, String)> {
    let name_attr = xml_attr(open_inside, "name").filter(|name| !name.is_empty());
    if let Some(name) = name_attr {
        if let Some(arguments) = parse_transcript_echo(body) {
            return as_declared_call(name, &arguments, declared);
        }
    }
    if let Some((name, arguments)) = parse_dynamic_wrapper(body, name_attr) {
        return as_declared_call(&name, &arguments, declared);
    }
    if let Some(name) = name_attr {
        if let Some(arguments) = json_object_text(body.trim()) {
            return as_declared_call(name, &arguments, declared);
        }
    }
    // Cursor's own dialect: the tool name is the id (`StrReplace`), and the arguments
    // are flat child elements rather than a `<toolName>` / `<arguments>` wrapper.
    if name_attr.is_none() {
        if let Some(name) = xml_attr(open_inside, "id").filter(|name| !name.is_empty()) {
            if let Some(arguments) = flat_argument_object(body) {
                return as_declared_call(name, &arguments, declared);
            }
        }
    }
    None
}

/// A declared name as written, or a Cursor native name rewritten onto the declared
/// tool that runs it (`StrReplace` → `edit`). An undeclared name is not a call.
fn as_declared_call(
    name: &str,
    arguments: &str,
    declared: &DeclaredTools,
) -> Option<(String, String)> {
    if let Some(call) = declared_call(name, arguments.to_string(), declared) {
        return Some(call);
    }
    let (pi_name, arguments) = native_declared_call(name, arguments)?;
    declared_call(pi_name, arguments, declared)
}

/// Cursor's native tool name onto the Pi tool the request declared, with that tool's
/// argument names. Only the pairs measured in a Cursor text call. A name or a field
/// that is not in the table is not guessed.
fn native_declared_call(name: &str, arguments: &str) -> Option<(&'static str, String)> {
    let value: Value = serde_json::from_str(arguments).ok()?;
    let obj = value.as_object()?;
    let take = |key: &str| obj.get(key).and_then(Value::as_str).map(str::to_string);
    match name {
        "Read" => {
            take("path").map(|path| ("read", serde_json::json!({ "path": path }).to_string()))
        }
        "StrReplace" => {
            let path = take("path")?;
            let old = take("old_string").or_else(|| take("oldText"))?;
            let new = take("new_string").or_else(|| take("newText"))?;
            Some((
                "edit",
                serde_json::json!({
                    "path": path,
                    "edits": [{ "oldText": old, "newText": new }]
                })
                .to_string(),
            ))
        }
        // A real exec that used Cursor's field names (`old_string`) instead of the
        // declared schema's (`oldText`). Same rewrite, no native name involved.
        _ if obj.contains_key("old_string") || obj.contains_key("new_string") => {
            if name != "edit" {
                return None;
            }
            let path = take("path")?;
            let old = take("old_string").or_else(|| take("oldText"))?;
            let new = take("new_string").or_else(|| take("newText"))?;
            Some((
                "edit",
                serde_json::json!({
                    "path": path,
                    "edits": [{ "oldText": old, "newText": new }]
                })
                .to_string(),
            ))
        }
        "Write" => Some((
            "write",
            serde_json::json!({
                "path": take("path")?,
                "content": take("contents").or_else(|| take("content"))?
            })
            .to_string(),
        )),
        "Shell" => take("command").map(|command| {
            (
                "bash",
                serde_json::json!({ "command": command }).to_string(),
            )
        }),
        "Grep" => {
            let pattern = take("pattern")?;
            let mut out = serde_json::Map::new();
            out.insert("pattern".into(), Value::String(pattern));
            if let Some(path) = take("path") {
                out.insert("path".into(), Value::String(path));
            }
            if let Some(glob) = take("glob") {
                out.insert("glob".into(), Value::String(glob));
            }
            Some(("grep", Value::Object(out).to_string()))
        }
        "Glob" => {
            let pattern = take("glob_pattern")
                .or_else(|| take("pattern"))
                .or_else(|| take("glob"))?;
            let mut out = serde_json::Map::new();
            out.insert("pattern".into(), Value::String(pattern));
            if let Some(path) = take("target_directory").or_else(|| take("path")) {
                out.insert("path".into(), Value::String(path));
            }
            Some(("find", Value::Object(out).to_string()))
        }
        // Composer calls the declared `find` with Cursor's `glob` (measured
        // 2026-10-12). Pi's schema requires `pattern`. A call that already has
        // `pattern` is left as the model wrote it.
        "find" if !obj.contains_key("pattern") => {
            let pattern = take("glob_pattern").or_else(|| take("glob"))?;
            let mut out = serde_json::Map::new();
            out.insert("pattern".into(), Value::String(pattern));
            if let Some(path) = take("path").or_else(|| take("target_directory")) {
                out.insert("path".into(), Value::String(path));
            }
            Some(("find", Value::Object(out).to_string()))
        }
        _ => None,
    }
}

/// The name a block claims, whether or not the request declared it.
fn block_name<'a>(open_inside: &'a str, body: &'a str) -> Option<&'a str> {
    if let Some(name) = xml_attr(open_inside, "name").filter(|name| !name.is_empty()) {
        return Some(name);
    }
    if let Some(name) = tool_name_element(body) {
        return Some(name);
    }
    xml_attr(open_inside, "id").filter(|name| !name.is_empty())
}

/// `<toolName>` text when the body is a dynamic wrapper. Not a call by itself.
fn tool_name_element(body: &str) -> Option<&str> {
    let elems = top_level_elements(body.trim())?;
    elems
        .iter()
        .find(|(name, _)| *name == "toolName")
        .map(|(_, inner)| inner.trim())
        .filter(|text| !text.is_empty() && !text.contains('<'))
}

fn declared_call(
    name: &str,
    arguments: String,
    declared: &DeclaredTools,
) -> Option<(String, String)> {
    declared
        .contains(name)
        .then(|| (name.to_string(), arguments))
}

/// `(inside of the opening tag, body, text after the close)`. The first
/// `</tool_call>` closes it. A tag that does not close is not a block.
fn split_first_tool_call(hold: &str) -> Option<(&str, &str, &str)> {
    let start = hold.find(TOOL_CALL_OPEN)?;
    let after_open = hold.get(start + TOOL_CALL_OPEN.len()..)?;
    if !tool_call_boundary_ok(hold, start) {
        return None;
    }
    let gt = after_open.find('>')?;
    let open_inside = after_open.get(..gt)?;
    let body_and_rest = after_open.get(gt + 1..)?;
    let close_at = body_and_rest.find("</tool_call>")?;
    let body = body_and_rest.get(..close_at)?;
    let after = body_and_rest.get(close_at + "</tool_call>".len()..)?;
    Some((open_inside, body, after))
}

fn xml_attr<'a>(tag: &'a str, key: &str) -> Option<&'a str> {
    let mut rest = tag;
    while let Some(at) = rest.find(key) {
        let before_ok = at == 0
            || rest
                .get(..at)
                .and_then(|head| head.chars().next_back())
                .is_some_and(|c| c.is_ascii_whitespace());
        let after_key = rest.get(at + key.len()..)?;
        if before_ok {
            let after_key = after_key.trim_start();
            if let Some(after_eq) = after_key.strip_prefix('=') {
                let after_eq = after_eq.trim_start();
                let quote = after_eq.chars().next()?;
                if quote == '"' || quote == '\'' {
                    let value = after_eq.get(quote.len_utf8()..)?;
                    let end = value.find(quote)?;
                    return value.get(..end);
                }
            }
        }
        rest = rest.get(at + key.len()..)?;
    }
    None
}

/// `<<<TR-` + 8 hex, a JSON object, `>>>TR-` + the same hex. The fence is the model's.
fn parse_transcript_echo(body: &str) -> Option<String> {
    let rest = body.trim().strip_prefix("<<<TR-")?;
    let hex = rest.get(..8)?;
    if hex.len() != 8 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let after_hex = rest.get(8..)?;
    let close = format!(">>>TR-{hex}");
    let at = after_hex.find(&close)?;
    let json_part = after_hex.get(..at)?.trim();
    let trailing = after_hex.get(at + close.len()..)?.trim();
    if !trailing.is_empty() {
        return None;
    }
    json_object_text(json_part)
}

/// `<toolName>` when the opening tag has no `name`. `<namespace>` and `<mcpDetails>`
/// are ignored. `<arguments>` is a JSON object, or only flat child elements.
fn parse_dynamic_wrapper(body: &str, name_attr: Option<&str>) -> Option<(String, String)> {
    let elems = top_level_elements(body.trim())?;
    if elems.is_empty()
        || !elems
            .iter()
            .any(|(name, _)| *name == "toolName" || *name == "arguments")
    {
        return None;
    }
    let mut tool_name = None;
    let mut arguments = None;
    for (name, inner) in elems {
        match name {
            "namespace" | "mcpDetails" => {}
            "toolName" => {
                if tool_name.is_some() {
                    return None;
                }
                let text = inner.trim();
                if text.is_empty() || text.contains('<') {
                    return None;
                }
                tool_name = Some(text.to_string());
            }
            "arguments" => {
                if arguments.is_some() {
                    return None;
                }
                arguments = Some(parse_arguments_body(inner)?);
            }
            _ => {}
        }
    }
    let name = match name_attr {
        Some(name) => name.to_string(),
        None => tool_name?,
    };
    Some((name, arguments?))
}

fn parse_arguments_body(inner: &str) -> Option<String> {
    let trimmed = inner.trim();
    if trimmed.starts_with('{') {
        return json_object_text(trimmed);
    }
    flat_argument_object(trimmed)
}

/// Flat child elements become string fields. Nested markup, a duplicate name, or
/// no children is not an object: do not guess fields.
fn flat_argument_object(inner: &str) -> Option<String> {
    let children = top_level_elements(inner.trim())?;
    if children.is_empty() {
        return None;
    }
    let mut map = serde_json::Map::new();
    for (name, inner) in children {
        if inner.contains('<') || map.contains_key(name) {
            return None;
        }
        map.insert(name.to_string(), Value::String(inner.trim().to_string()));
    }
    serde_json::to_string(&Value::Object(map)).ok()
}

/// The whole string is one JSON object, including `{}`. The text is kept as written.
fn json_object_text(text: &str) -> Option<String> {
    let value: Value = serde_json::from_str(text).ok()?;
    value.as_object()?;
    Some(text.to_string())
}

/// Top-level elements of `input`. Stray text, or a tag that does not close, is `None`.
fn top_level_elements(input: &str) -> Option<Vec<(&str, &str)>> {
    let mut rest = input.trim_start();
    let mut out = Vec::new();
    while !rest.is_empty() {
        if !rest.starts_with('<')
            || rest.starts_with("</")
            || rest.starts_with("<?")
            || rest.starts_with("<!")
        {
            return None;
        }
        let after_lt = rest.get(1..)?;
        let name_len = after_lt
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-' || *c == ':')
            .map(char::len_utf8)
            .sum::<usize>();
        if name_len == 0 {
            return None;
        }
        let name = after_lt.get(..name_len)?;
        if !name.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_') {
            return None;
        }
        let after_name = after_lt.get(name_len..)?;
        let gt = after_name.find('>')?;
        let head = after_name.get(..gt)?;
        let after_gt = after_name.get(gt + 1..)?;
        if head.ends_with('/') {
            out.push((name, ""));
            rest = after_gt.trim_start();
            continue;
        }
        let close = format!("</{name}>");
        let close_at = after_gt.find(&close)?;
        let inner = after_gt.get(..close_at)?;
        out.push((name, inner));
        rest = after_gt.get(close_at + close.len()..)?.trim_start();
    }
    Some(out)
}

/// `AgentServerMessage.interaction_update` (`f1`) field 14: the answer is done (H1).
/// The call id of an `InteractionUpdate.tool_call_started` (`f1.2`) or `partial_tool_call`
/// (`f1.7`) whose `ToolCall` is `edit_tool_call` (`f12`): Cursor's edit built-in. Its
/// pre-read (`f2.f7`) carries the same id in `ReadArgs.tool_call_id` (measured in
/// `fixtures/cursor/2026.09.28-64d2043/create-grok-4.7`, frames 38-43; #248).
fn edit_call_id(payload: &[u8]) -> Option<String> {
    const TOOL_CALL_STARTED: u64 = 2;
    const PARTIAL_TOOL_CALL: u64 = 7;
    const EDIT_TOOL_CALL: u64 = 12;
    let update = iter_fields(payload).find(|f| f.field == 1 && f.wire == 2)?;
    for started in iter_fields(update.data) {
        if !matches!(started.field, TOOL_CALL_STARTED | PARTIAL_TOOL_CALL) || started.wire != 2 {
            continue;
        }
        let mut call_id = None;
        let mut is_edit = false;
        for field in iter_fields(started.data) {
            match (field.field, field.wire) {
                (1, 2) => call_id = std::str::from_utf8(field.data).ok(),
                (2, 2) => is_edit = iter_fields(field.data).any(|f| f.field == EDIT_TOOL_CALL),
                _ => {}
            }
        }
        if let Some(id) = call_id.filter(|id| is_edit && !id.is_empty()) {
            return Some(id.to_string());
        }
    }
    None
}

fn is_turn_ended(payload: &[u8]) -> bool {
    iter_fields(payload).any(|top| {
        top.field == 1 && top.wire == 2 && iter_fields(top.data).any(|update| update.field == 14)
    })
}

/// `AgentServerMessage.interaction_update` field 13 (`heartbeat`). cursor-agent treats this
/// as the server still being in the turn. A frame that also carries text or another update
/// is not this: only a lone `f1.13`.
fn is_interaction_heartbeat(payload: &[u8]) -> bool {
    let top: Vec<_> = iter_fields(payload).collect();
    let [only] = top.as_slice() else {
        return false;
    };
    if only.field != 1 || only.wire != 2 {
        return false;
    }
    let inner: Vec<_> = iter_fields(only.data).collect();
    matches!(inner.as_slice(), [beat] if beat.field == 13 && beat.wire == 2)
}

/// Empty `f7` is the Connect heartbeat; it must not count toward nested drift.
fn is_keepalive(payload: &[u8]) -> bool {
    let fields: Vec<_> = iter_fields(payload).collect();
    matches!(fields.as_slice(), [f] if f.field == 7 && f.data.is_empty())
}

/// Top-level conversation-state envelopes (`f4` only). Healthy 2026.09.18 sets send four
/// before the first token; they must not eat the unknown-top-level budget.
fn is_conversation_state(top: &[u64]) -> bool {
    !top.is_empty() && top.iter().all(|&f| f == 4)
}

/// Top-level timing-metrics envelopes (`f8` only). Healthy runs send 64-bit doubles
/// here, often riding along with `f1`; a lone metrics frame is not unknown-top-level.
fn is_timing_metrics(top: &[u64]) -> bool {
    !top.is_empty() && top.iter().all(|&f| f == 8)
}

/// Prefixes of nested paths measured on healthy Fable tool turns.
/// `1.7` matches `1.7`, `1.7.1`, `1.7.2` (tool_uuid announcement, not `f2.f7`).
/// `1.27` is `tool_requests_listed {call_count}` and `1.3` is `tool_call_completed`
/// `{call_id, tool_call, model_call_id}` (F1, CLI 2026.09.28, #187): progress
/// bookkeeping around a tool step, not answer text. The call the client runs
/// stays the exec (`f2.f36`, `f2.f11`, or a built-in). A dot boundary keeps
/// `1.2` from swallowing `1.27`.
const MEASURED_HEALTHY_NESTED_PREFIXES: &[&str] = &[
    "1.13", "1.7", "1.8", "1.25", "1.5", "1.2", "1.27", "1.3", "8",
];

fn path_has_healthy_prefix(path: &str, prefix: &str) -> bool {
    path == prefix
        || path
            .strip_prefix(prefix)
            .is_some_and(|rest| rest.starts_with('.'))
}

/// Every nested path in the frame is a measured-healthy preamble field. A mix with
/// an unknown path (`1.13` + `1.9.1`) must still count toward nested drift.
fn is_measured_healthy_preamble(paths: &[String]) -> bool {
    !paths.is_empty()
        && paths.iter().all(|path| {
            MEASURED_HEALTHY_NESTED_PREFIXES
                .iter()
                .any(|prefix| path_has_healthy_prefix(path, prefix))
        })
}

/// Nested protobuf paths up to three levels, for a drift message that names a moved field.
fn nested_field_paths(payload: &[u8]) -> Vec<String> {
    let mut out = Vec::new();
    for a in iter_fields(payload) {
        let mut found_child = false;
        if a.wire == 2 {
            for b in iter_fields(a.data) {
                found_child = true;
                let mut found_gchild = false;
                if b.wire == 2 {
                    for c in iter_fields(b.data) {
                        found_gchild = true;
                        out.push(format!("{}.{}.{}", a.field, b.field, c.field));
                    }
                }
                if !found_gchild {
                    out.push(format!("{}.{}", a.field, b.field));
                }
            }
        }
        if !found_child {
            out.push(a.field.to_string());
        }
    }
    out
}

/// [`AgentMapper`] behind Connect framing, plus what this wire needs written back: the
/// `request_context` result Cursor waits for, and a typed denial for a documented builtin
/// no declared tool runs.
pub struct CursorWire<'a> {
    mapper: AgentMapper,
    decoder: FrameDecoder,
    /// Owned so a per-request cwd can outlive the `&str` it was copied from (#205).
    cwd: String,
    shell: &'a str,
    has_tools: bool,
    /// Thin `request_id`, for the safety-net warning (H1). Empty on fixture replay.
    request_id: String,
    /// The tools declared on frame 0, replayed when Cursor asks for the server's state.
    tools: Vec<McpTool>,
    /// Conversation blobs the server parked here (`kv_server_message.set_blob_args`), for
    /// `get_blob_args`. One Run's worth; bounded by `KV_BLOB_BUDGET`.
    blobs: std::collections::HashMap<Vec<u8>, Vec<u8>>,
    blob_bytes: usize,
    /// The `ReadResult` a built-in read of a missing file is answered with (G1:
    /// `FileNotFound`; `thickapi capture --scenario create` records `Rejected`).
    read_miss_shape: ReadMissShape,
    /// No-op shell probes refused inside this Run. The third one ends the turn.
    probe_refusals: u8,
}

impl<'a> CursorWire<'a> {
    /// The shell `request_context_result` advertises (default [`DEFAULT_SHELL`]).
    pub(super) fn set_shell(&mut self, shell: &'a str) {
        self.shell = shell;
    }

    pub(super) fn set_request_id(&mut self, request_id: &str) {
        self.request_id = request_id.to_string();
        self.mapper.set_request_id(request_id);
    }

    /// Answer missing reads through this check and in this shape, in place of the default
    /// (`FileNotFound`, the path resolved under the wire's cwd).
    pub(super) fn with_read_miss(mut self, read_miss: ReadMiss) -> Self {
        self.read_miss_shape = read_miss.shape;
        self.mapper = self.mapper.reading_through(read_miss.is_missing);
        self
    }

    pub(super) fn new(
        model: &str,
        wire_sent: Option<&str>,
        cwd: &str,
        has_tools: bool,
        prompt_tokens: u64,
        budgets: CursorBudgets,
        tools: Vec<McpTool>,
    ) -> Self {
        let read_miss = ReadMiss::under_cwd(ReadMissShape::FileNotFound, cwd);
        let text_cwd = cwd.to_string();
        let mut mapper = AgentMapper::with_budgets(model.to_string(), budgets)
            .declaring(DeclaredTools::from_tools(&tools))
            .reading_through(read_miss.is_missing)
            .reading_text(std::sync::Arc::new(move |path| {
                read_target_text(&text_cwd, path)
            }));
        mapper.wire_sent = wire_sent.filter(|s| !s.is_empty()).map(str::to_string);
        mapper.prompt_tokens = prompt_tokens;
        Self {
            mapper,
            decoder: FrameDecoder::default(),
            cwd: cwd.to_string(),
            shell: DEFAULT_SHELL,
            has_tools,
            request_id: String::new(),
            tools,
            blobs: std::collections::HashMap::new(),
            blob_bytes: 0,
            read_miss_shape: read_miss.shape,
            probe_refusals: 0,
        }
    }

    /// Acknowledge a parked blob (or hand one back). The server awaits every ack before it
    /// calls the model again inside the Run.
    fn answer_kv(&mut self, id: u64, kind: KvKind, replies: &mut Vec<Bytes>) {
        match kind {
            KvKind::Set { blob_id, data } => {
                let over = self.blob_bytes.saturating_add(data.len()) > super::KV_BLOB_BUDGET;
                if over {
                    tracing::warn!(
                        id,
                        bytes = data.len(),
                        held = self.blob_bytes,
                        "cursor kv set_blob over budget; acknowledged with an error"
                    );
                    replies.push(kv_set_result_frame(id, Some("blob budget exceeded")));
                    return;
                }
                tracing::debug!(id, bytes = data.len(), "cursor kv set_blob; acknowledged");
                self.blob_bytes += data.len();
                if let Some(old) = self.blobs.insert(blob_id, data) {
                    self.blob_bytes = self.blob_bytes.saturating_sub(old.len());
                }
                replies.push(kv_set_result_frame(id, None));
            }
            KvKind::Get { blob_id } => {
                let data = self.blobs.get(&blob_id);
                tracing::info!(id, held = data.is_some(), "cursor kv get_blob; answered");
                replies.push(kv_get_result_frame(id, data.map(Vec::as_slice)));
            }
        }
    }

    pub(super) fn terminal_error(&self) -> Option<&ThinError> {
        self.mapper.last_error.as_ref()
    }

    /// The terminal error is a usage limit on this row only (#249).
    pub(super) fn row_limited(&self) -> bool {
        self.mapper.row_limited()
    }

    pub(super) fn wire_sent(&self) -> &str {
        self.mapper.wire_sent.as_deref().unwrap_or("")
    }

    pub(super) fn finished_ok(&self) -> bool {
        self.mapper.answered()
    }

    pub(super) fn defer_reasoning_only_end(&mut self) {
        self.mapper.defer_reasoning_only_end();
    }

    pub(super) fn reasoning_only_deferred(&self) -> bool {
        self.mapper.reasoning_only_deferred()
    }

    pub(super) fn text_tool_call_deferred(&self) -> bool {
        self.mapper.text_tool_call_deferred()
    }

    pub(super) fn hold_no_exec_result(&mut self) {
        self.mapper.hold_no_exec_result();
    }

    pub(super) fn no_exec_result_held(&self) -> bool {
        self.mapper.no_exec_result_held()
    }

    pub(super) fn rendered_builtin(&self) -> Option<&str> {
        self.mapper.rendered_builtin()
    }

    fn drift(&mut self, e: impl std::fmt::Display) {
        self.mapper.fail(ThinError::vendor_drift(
            VENDOR,
            format!("cursor frame: {e}"),
        ));
    }
}

impl WireMapper for CursorWire<'_> {
    fn core(&mut self) -> &mut MapperCore {
        &mut self.mapper.core
    }

    fn on_chunk(&mut self, chunk: &[u8], replies: &mut Vec<Bytes>) {
        let frames = match self.decoder.push(chunk) {
            Ok(frames) => frames,
            Err(e) => return self.drift(e),
        };
        for frame in frames {
            let Some(payload) = self.mapper.open(frame) else {
                continue;
            };
            tracing::debug!(
                bytes = payload.len(),
                paths = ?nested_field_paths(&payload),
                "cursor frame"
            );
            if iter_fields(&payload).any(|f| f.field != 1 || f.data.len() > 120) {
                tracing::debug!(body = %describe_exec_payload(&payload), "cursor frame body");
            }
            // Cursor may repeat `f2.f10`; every ask gets a result, as the only wire ever
            // run live did. Silence here is `internal: No exec result`. The exec's
            // `stream_close` follows its result, as `cursor-agent` writes them; a close
            // before the result is the same death (H2, #186).
            if let Some(exec) = extract_request_context_exec(&payload) {
                tracing::info!(
                    exec_id = exec.id,
                    "cursor request_context_args; sending exec result, then stream_close"
                );
                replies.push(request_context_result_frame(
                    &self.cwd,
                    self.shell,
                    exec.id,
                    exec.exec_id.as_deref(),
                ));
                replies.push(super::request::stream_close_frame(exec.id));
            }
            // Lazy tool loading (#114): the model asks for the `thickapi` server's tools
            // before it can call one. Answer with what frame 0 declared.
            if let Some(exec) = extract_mcp_state_exec(&payload) {
                tracing::info!(
                    exec_id = exec.id,
                    servers = ?exec.server_identifiers,
                    kick_only = exec.kick_only,
                    tools = self.tools.len(),
                    "cursor mcp_state_exec_args; sending the declared tools"
                );
                replies.push(mcp_state_result_frame(
                    exec.id,
                    exec.exec_id.as_deref(),
                    &self.tools,
                ));
            }
            if let Some(kv) = extract_kv_server_message(&payload) {
                self.answer_kv(kv.id, kv.kind, replies);
            }
            let routed = extract_builtin_exec(&payload).map(|builtin| {
                let route = self.mapper.route(&builtin);
                match &route {
                    BuiltinRoute::ToPi { name, .. } => tracing::info!(
                        field = builtin.field,
                        exec_id = builtin.id,
                        tool = %name,
                        "cursor builtin exec; handing to Pi as a declared tool"
                    ),
                    BuiltinRoute::ReadMissing { path } => {
                        tracing::info!(
                            field = builtin.field,
                            exec_id = builtin.id,
                            path = %redact_log(path),
                            shape = ?self.read_miss_shape,
                            "cursor builtin read of a missing file; answered in protocol"
                        );
                        replies.push(read_miss_frame(
                            self.read_miss_shape,
                            builtin.id,
                            builtin.exec_id.as_deref(),
                            path,
                        ));
                    }
                    BuiltinRoute::EditPreRead { path, content } => {
                        tracing::info!(
                            field = builtin.field,
                            exec_id = builtin.id,
                            path = %redact_log(path),
                            bytes = content.len(),
                            "cursor builtin edit pre-read of an existing file; answered in protocol"
                        );
                        replies.push(read_success_frame(
                            builtin.id,
                            builtin.exec_id.as_deref(),
                            path,
                            content,
                        ));
                    }
                    BuiltinRoute::Deny => {
                        tracing::info!(
                            field = builtin.field,
                            exec_id = builtin.id,
                            request_id = %self.request_id,
                            "cursor builtin denied"
                        );
                        replies.extend(deny_builtin_frames(
                            builtin.id,
                            builtin.exec_id.as_deref(),
                            builtin.field,
                            &builtin.args,
                            self.mapper.declared().names(),
                        ));
                    }
                    BuiltinRoute::NoopProbe { command } => {
                        self.probe_refusals = self.probe_refusals.saturating_add(1);
                        tracing::info!(
                            field = builtin.field,
                            exec_id = builtin.id,
                            request_id = %self.request_id,
                            refusals = self.probe_refusals,
                            "cursor noop shell probe refused"
                        );
                        replies.extend(super::request::probe_refuse_frames(builtin.id, command));
                        if self.probe_refusals >= 3 {
                            self.mapper.fail(
                                ThinError::vendor_drift(
                                    VENDOR,
                                    format!(
                                        "cursor refused a no-op shell probe (`{command}`) 3 times; \
                                         call the tool that does the work"
                                    ),
                                )
                                .without_retry(),
                            );
                        }
                    }
                }
                route
            });
            self.mapper.accept_exec_payload(&payload, routed);
        }
    }

    /// No Connect terminal: finish from what arrived. Quiet with tools offered is not
    /// this path — that safety net fails the turn (H1).
    fn on_body_end(&mut self) {
        if let Err(e) = self.decoder.finish() {
            self.drift(e);
        } else {
            self.mapper.stream_closed();
        }
    }

    /// Text-only: `idle_seconds` still ends the answer as `stop` when `turn_ended` has
    /// not arrived. With tools offered, the after-text budget is a safety net: spending
    /// it is `vendor_down`, not `stop` (H1, #185). A held `<tool_call` is not that
    /// silence: a complete call finishes `tool_calls`, and an incomplete one is
    /// `text_tool_call` drift (#215).
    fn on_quiet(&mut self, waited: Duration) {
        self.mapper.release_tag_prefix();
        if self.mapper.holding_text_call() {
            self.mapper.idle_end();
            return;
        }
        if self.has_tools
            && self.mapper.got_text
            && !self.mapper.got_tool_call
            && self.mapper.rendered_builtin().is_none()
        {
            tracing::warn!(
                request_id = %self.request_id,
                gap_s = waited.as_secs(),
                "cursor went quiet mid-answer"
            );
            self.mapper.fail(ThinError::vendor_down(
                VENDOR,
                format!("cursor went quiet mid-answer for {}s", waited.as_secs()),
            ));
            return;
        }
        self.mapper.idle_end();
    }

    fn on_interrupt(&mut self, err: ThinError) {
        self.mapper.fail(err);
    }

    fn budget(&mut self) -> Duration {
        self.mapper.idle_budget(self.has_tools)
    }
}
