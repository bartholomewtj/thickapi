//! Claude stream-json → thin events (port of `stream-events.ts`).
//!
//! Text and thinking deltas are published live. Tool-use blocks are buffered and published by
//! the caller only after the process is dead and the private state is verified (SPEC §19.7).
//! Termination happens **only** on `message_delta.stop_reason == "tool_use"` with every block
//! closed. Usage comes from `message_start` + `message_delta`, never from `result`.

use std::collections::BTreeMap;
use std::collections::BTreeSet;

use serde_json::Value;

use super::protocol::{
    client_safe_error, parse_rate_limit_notice, terminal_result_detail, validate_init,
    InitExpectation, ProtocolError, RateLimitNotice,
};
use crate::thin::anthropic_blocks::{
    BlockKind, BlockMachine, DeltaError, StartError, StopError, Strictness,
};
use crate::thin::openai::{FinishReason, Usage};
use crate::thin::ThinEvent;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapturedToolCall {
    pub id: String,
    /// Client tool name, or the Claude name of a dialect row (transport prefix already resolved).
    pub name: String,
    /// The name Claude proposed, prefix included (`mcp__pi__Edit`). Empty on a replay that has
    /// no wire name; publish then treats `name` as the wire name too.
    pub wire_name: String,
    /// JSON-encoded arguments object, as Claude sent them.
    pub arguments: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminationCause {
    None,
    ToolHandoff,
    CallerAbort,
}

#[derive(Debug, Default, Clone, Copy)]
struct RawUsage {
    input: u64,
    output: u64,
    cache_read: u64,
    cache_write: u64,
}

/// Why [`Mapper::accept_line`] refused a line (before the tool handoff; after it both skip).
#[derive(Debug)]
pub enum LineError {
    InvalidJson(serde_json::Error),
    Protocol(ProtocolError),
}

/// What the caller must do after `accept` returns.
#[derive(Debug, Default)]
pub struct Signals {
    /// Terminate the child now: `stop_reason == tool_use` and every block is closed.
    pub terminate_for_tool_use: bool,
}

pub struct Mapper<'a> {
    expected_tools: &'a BTreeSet<String>,
    transport_to_client: &'a BTreeMap<String, String>,
    /// `mcp__pi__<alias>` → client name or dialect row. Empty means every resolved name is a
    /// client name (dialect off, and `replay`).
    transport_target: BTreeMap<String, crate::thin::dialect::CatalogTarget>,
    private_paths: Vec<String>,
    parallel_tool_calls: bool,
    extra_tools: Vec<String>,
    nonempty_inventories: Vec<String>,

    initialized: bool,
    pub response_model: Option<String>,
    message_started: bool,
    message_stopped: bool,
    result_received: bool,
    stop_reason: Option<String>,
    blocks: BlockMachine,
    tool_calls: Vec<CapturedToolCall>,
    usage: RawUsage,
    usage_seen: bool,
    assistant_diagnostic: Option<String>,
    rate_limit_failure: Option<String>,
    rate_limit_seen: BTreeSet<String>,
    /// Set once a `result` record ended the turn successfully.
    successful_finish: Option<FinishReason>,
    /// Set once the message stop was a tool handoff.
    tool_use_stop: bool,
    /// Unknown record / event / block types skipped this turn (C8).
    skipped_records: u64,
    skipped_events: u64,
    skipped_blocks: u64,
    /// Tool calls dropped because `parallel_tool_calls` was false (SPEC §19.3).
    skipped_parallel: u64,
    /// Events for the caller to forward (drained after each `accept`).
    pending: Vec<ThinEvent>,
    pub(crate) path: &'static str,
    pub(crate) fallback: Option<&'static str>,
    /// The effort the child was spawned with, for the access line's `wire_params`.
    pub(crate) wire_params: Option<String>,
    /// For the warn line when a dangling text/thinking block is closed at a stop (#145).
    request_id: String,
    cli_version: Option<String>,
    /// Active dialect rows. Empty when dialect is off, which leaves publish unchanged.
    dialect: Vec<crate::thin::dialect::ActiveRow>,
    /// Text already forwarded to the route. A dialect retry must not send it again.
    forwarded_text: String,
}

impl<'a> Mapper<'a> {
    pub fn new(
        expected_tools: &'a BTreeSet<String>,
        transport_to_client: &'a BTreeMap<String, String>,
        private_paths: Vec<String>,
        parallel_tool_calls: bool,
    ) -> Self {
        Self {
            expected_tools,
            transport_to_client,
            transport_target: BTreeMap::new(),
            private_paths,
            parallel_tool_calls,
            extra_tools: Vec::new(),
            nonempty_inventories: Vec::new(),
            initialized: false,
            response_model: None,
            message_started: false,
            message_stopped: false,
            result_received: false,
            stop_reason: None,
            blocks: BlockMachine::new(),
            tool_calls: Vec::new(),
            usage: RawUsage::default(),
            usage_seen: false,
            assistant_diagnostic: None,
            rate_limit_failure: None,
            rate_limit_seen: BTreeSet::new(),
            successful_finish: None,
            tool_use_stop: false,
            skipped_records: 0,
            skipped_events: 0,
            skipped_blocks: 0,
            skipped_parallel: 0,
            pending: Vec::new(),
            path: "bridge",
            fallback: None,
            wire_params: None,
            request_id: String::new(),
            cli_version: None,
            dialect: Vec::new(),
            forwarded_text: String::new(),
        }
    }

    pub fn with_dialect(mut self, rows: Vec<crate::thin::dialect::ActiveRow>) -> Self {
        self.dialect = rows;
        self
    }

    /// Resolve a transport name to a dialect row when the catalog says so. A name absent from
    /// the map stays a client name, which is what `replay` and dialect off need.
    pub fn with_targets(
        mut self,
        targets: &BTreeMap<String, crate::thin::dialect::CatalogTarget>,
    ) -> Self {
        self.transport_target = targets.clone();
        self
    }

    pub fn dialect(&self) -> &[crate::thin::dialect::ActiveRow] {
        &self.dialect
    }

    /// The request id [`with_log_context`] set. `None` on a replay, which has no access line.
    pub fn request_id(&self) -> Option<&str> {
        if self.request_id.is_empty() {
            None
        } else {
            Some(&self.request_id)
        }
    }

    /// Text this attempt already sent to the route.
    pub fn forwarded_text(&self) -> String {
        self.forwarded_text.clone()
    }

    /// Name the request and the installed CLI in the mapper's own log lines.
    pub fn with_log_context(mut self, request_id: &str, cli_version: Option<String>) -> Self {
        self.request_id = request_id.to_string();
        self.cli_version = cli_version;
        self
    }

    pub fn with_tolerate(
        mut self,
        extra_tools: Vec<String>,
        nonempty_inventories: Vec<String>,
    ) -> Self {
        self.extra_tools = extra_tools;
        self.nonempty_inventories = nonempty_inventories;
        self
    }

    pub fn drain(&mut self) -> Vec<ThinEvent> {
        let pending = std::mem::take(&mut self.pending);
        for event in &pending {
            if let ThinEvent::TextDelta(text) = event {
                self.forwarded_text.push_str(text);
            }
        }
        pending
    }

    pub fn is_initialized(&self) -> bool {
        self.initialized
    }

    pub fn tool_use_stop(&self) -> bool {
        self.tool_use_stop
    }

    pub fn skipped_records(&self) -> u64 {
        self.skipped_records
    }

    /// Count a line the caller skipped before it reached [`Mapper::accept`] (a record torn by
    /// the tool-handoff kill, R3).
    pub fn note_skipped_record(&mut self) {
        self.skipped_records += 1;
    }

    pub fn skipped_events(&self) -> u64 {
        self.skipped_events
    }

    pub fn skipped_blocks(&self) -> u64 {
        self.skipped_blocks
    }

    pub fn successful_finish(&self) -> Option<FinishReason> {
        self.successful_finish
    }

    pub fn rate_limit_failure(&self) -> Option<&str> {
        self.rate_limit_failure.as_deref()
    }

    pub fn has_open_blocks(&self) -> bool {
        self.blocks.has_open()
    }

    /// Captured tool calls, honouring `parallel_tool_calls: false` (first only).
    pub fn tool_calls(&self) -> Vec<CapturedToolCall> {
        if self.parallel_tool_calls || self.tool_calls.len() <= 1 {
            self.tool_calls.clone()
        } else {
            self.tool_calls.iter().take(1).cloned().collect()
        }
    }

    /// Calls [`Self::tool_calls`] would drop. Counted once, when the turn is published.
    pub fn note_parallel_drops(&mut self) -> u64 {
        if self.parallel_tool_calls {
            return 0;
        }
        let dropped = self.tool_calls.len().saturating_sub(1) as u64;
        self.skipped_parallel = self.skipped_parallel.saturating_add(dropped);
        dropped
    }

    pub fn skipped_parallel(&self) -> u64 {
        self.skipped_parallel
    }

    /// OpenAI usage from `message_start` + `message_delta` (prompt = input + cache read + cache write).
    pub fn usage(&self) -> Option<Usage> {
        self.usage_seen.then(|| {
            Usage::from_parts(
                self.usage.input + self.usage.cache_write,
                self.usage.output,
                self.usage.cache_read,
            )
        })
    }

    pub fn accept(
        &mut self,
        value: &Value,
        cause: TerminationCause,
    ) -> Result<Signals, ProtocolError> {
        let record = value
            .as_object()
            .ok_or_else(|| ProtocolError::new("protocol_shape", "Invalid Claude event record"))?;
        let kind = record.get("type").and_then(Value::as_str);
        let subtype = record.get("subtype").and_then(Value::as_str);
        if kind == Some("system") && subtype == Some("init") {
            if self.initialized {
                return Err(ProtocolError::new(
                    "protocol_init",
                    "Claude emitted duplicate initialization",
                ));
            }
            let expect = InitExpectation {
                tools: self.expected_tools,
                mcp_pi: !self.expected_tools.is_empty(),
                private_paths: &self.private_paths,
                extra_tools: &self.extra_tools,
                nonempty_inventories: &self.nonempty_inventories,
            };
            let model = validate_init(value, &expect)?;
            self.response_model = Some(model.clone());
            self.initialized = true;
            self.pending.push(ThinEvent::Started {
                model,
                path: self.path,
                fallback: self.fallback,
                wire_params: self.wire_params.clone(),
            });
            return Ok(Signals::default());
        }
        if !self.initialized {
            // Claude Code 2.1.283 announces `system/commands_changed` before `system/init`.
            // A non-init system record is status, not output: skip it and keep waiting for init.
            if kind == Some("system") {
                self.skipped_records += 1;
                tracing::debug!(
                    subtype = subtype.unwrap_or("<missing>"),
                    "skipped Claude system record before init"
                );
                return Ok(Signals::default());
            }
            return Err(ProtocolError::new(
                "protocol_order",
                format!(
                    "Claude emitted a record before initialization (type {} subtype {})",
                    kind.unwrap_or("<missing>"),
                    subtype.unwrap_or("-")
                ),
            ));
        }
        if self.result_received && cause != TerminationCause::ToolHandoff {
            return Err(ProtocolError::new(
                "protocol_order",
                "Claude emitted a record after its result",
            ));
        }
        let outcome = match kind {
            Some("stream_event") => {
                let event = record
                    .get("event")
                    .and_then(Value::as_object)
                    .ok_or_else(|| {
                        ProtocolError::new(
                            "protocol_shape",
                            "Claude stream_event did not contain an event",
                        )
                    })?;
                self.accept_stream_event(event)
            }
            Some("result") => self.accept_result(value, cause).map(|_| Signals::default()),
            Some("rate_limit_event") => {
                if let Some(info) = record.get("rate_limit_info") {
                    self.accept_rate_limit(info);
                }
                Ok(Signals::default())
            }
            Some("assistant") => {
                self.accept_assistant(record);
                Ok(Signals::default())
            }
            // Completed user echoes and non-init system status records are redundant with the
            // partial stream events.
            Some("user") | Some("system") => Ok(Signals::default()),
            other => {
                // C8: unknown record type is vocabulary, not an isolation failure.
                self.skipped_records += 1;
                tracing::debug!(
                    kind = other.unwrap_or("<missing>"),
                    "skipped unknown Claude record type"
                );
                Ok(Signals::default())
            }
        };
        // C7: after a tool handoff, drain-and-tap — an abort/unknown record must not
        // drop captured tool_calls.
        match outcome {
            Err(_) if self.tool_use_stop && cause == TerminationCause::ToolHandoff => {
                self.skipped_records += 1;
                Ok(Signals::default())
            }
            other => other,
        }
    }

    /// One stream-json line, as the live loop and `replay` both read it (SPEC §19.7
    /// **Capture**). A blank line is nothing. After the tool handoff (audit R3) the captured
    /// calls are already complete: a line the kill tore in half is skipped and counted, and a
    /// protocol error is skipped, so neither drops `tool_calls`.
    pub fn accept_line(
        &mut self,
        line: &str,
        cause: TerminationCause,
    ) -> Result<Signals, LineError> {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            return Ok(Signals::default());
        }
        let handoff = cause == TerminationCause::ToolHandoff;
        let value: Value = match serde_json::from_str(trimmed) {
            Ok(v) => v,
            Err(e) if handoff => {
                self.note_skipped_record();
                tracing::debug!(error = %e, "skipping torn Claude record after tool handoff");
                return Ok(Signals::default());
            }
            Err(e) => return Err(LineError::InvalidJson(e)),
        };
        match self.accept(&value, cause) {
            Err(err) if handoff => {
                tracing::debug!(
                    code = err.code,
                    "skipping protocol error after tool handoff"
                );
                Ok(Signals::default())
            }
            other => other.map_err(LineError::Protocol),
        }
    }

    fn accept_stream_event(
        &mut self,
        event: &serde_json::Map<String, Value>,
    ) -> Result<Signals, ProtocolError> {
        let kind = event
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("<missing>");
        if kind == "message_start" {
            if self.message_started {
                return Err(ProtocolError::new(
                    "protocol_message",
                    "Claude emitted duplicate message_start",
                ));
            }
            let message = object(event.get("message"), "message_start.message")?;
            if let Some(model) = message.get("model").and_then(Value::as_str) {
                self.response_model = Some(model.to_string());
            }
            self.apply_usage(message.get("usage"));
            self.message_started = true;
            return Ok(Signals::default());
        }
        if self.message_stopped {
            return Err(ProtocolError::new(
                "protocol_order",
                format!("{kind} arrived after message_stop"),
            ));
        }
        if kind == "ping" {
            return Ok(Signals::default());
        }
        if !self.message_started {
            return Err(ProtocolError::new(
                "protocol_order",
                format!("{kind} arrived before message_start"),
            ));
        }
        match kind {
            "content_block_start" => self.start_block(event)?,
            "content_block_delta" => self.delta_block(event)?,
            "content_block_stop" => self.end_block(event)?,
            "message_delta" => {
                let delta = object(event.get("delta"), "message_delta.delta")?;
                if let Some(reason) = delta.get("stop_reason") {
                    if !reason.is_null() {
                        self.stop_reason = Some(stop_reason(reason)?);
                    }
                }
                self.apply_usage(event.get("usage"));
                if self.stop_reason.as_deref() == Some("tool_use") {
                    if self.has_open_blocks() {
                        return Err(ProtocolError::new(
                            "protocol_blocks",
                            "Claude signalled tool_use with unclosed content blocks",
                        ));
                    }
                    if self.tool_calls.is_empty() {
                        return Err(ProtocolError::new(
                            "protocol_tool",
                            "Claude reported tool use without a tool call",
                        ));
                    }
                    self.tool_use_stop = true;
                    return Ok(Signals {
                        terminate_for_tool_use: true,
                    });
                }
            }
            "message_stop" => {
                self.close_dangling_blocks(
                    "message_stop",
                    "Claude stopped with unclosed content blocks",
                )?;
                self.message_stopped = true;
            }
            other => {
                self.skipped_events += 1;
                tracing::debug!(kind = other, "skipped unknown Claude stream event");
            }
        }
        Ok(Signals::default())
    }

    fn start_block(&mut self, event: &serde_json::Map<String, Value>) -> Result<(), ProtocolError> {
        let index = index(event.get("index"))?;
        let source = object(
            event.get("content_block"),
            "content_block_start.content_block",
        )?;
        let value = Value::Object(source.clone());
        let names = self.transport_to_client;
        let targets = &self.transport_target;
        match self.blocks.start(
            index,
            &value,
            |wire| resolve_transport(wire, names, targets),
            Strictness::Strict,
        ) {
            Ok(out) => {
                if out.skipped {
                    self.skipped_blocks += 1;
                }
                self.pending.extend(out.events);
                Ok(())
            }
            Err(StartError::DuplicateIndex(i)) => Err(ProtocolError::new(
                "protocol_blocks",
                format!("Duplicate content block index {i}"),
            )),
            Err(StartError::UnknownTool(qualified)) => Err(ProtocolError::new(
                "tool_unknown",
                format!("Claude proposed an unknown tool: {qualified}"),
            )),
            Err(StartError::MissingToolId) => Err(ProtocolError::new(
                "tool_id",
                "Claude emitted a tool without an ID",
            )),
            Err(StartError::EmptyRedacted) => Err(ProtocolError::new(
                "protocol_block",
                "Claude emitted redacted thinking without opaque data",
            )),
        }
    }

    fn delta_block(&mut self, event: &serde_json::Map<String, Value>) -> Result<(), ProtocolError> {
        let index = index(event.get("index"))?;
        let delta = object(event.get("delta"), "content_block_delta.delta")?;
        let value = Value::Object(delta.clone());
        match self
            .blocks
            .delta(index, &value, Strictness::Strict, &mut |dst, piece| {
                dst.push_str(piece);
                true
            }) {
            Ok(out) => {
                if out.skipped {
                    self.skipped_events += 1;
                }
                self.pending.extend(out.events);
                Ok(())
            }
            Err(DeltaError::UnknownIndex(i)) => Err(ProtocolError::new(
                "protocol_blocks",
                format!("Delta for unknown content block {i}"),
            )),
            Err(DeltaError::Mismatch { delta_type }) => Err(mismatch(&delta_type)()),
        }
    }

    fn end_block(&mut self, event: &serde_json::Map<String, Value>) -> Result<(), ProtocolError> {
        let index = index(event.get("index"))?;
        self.close_block(index)
    }

    /// At `message_stop` / `result` (#145): a text or thinking block the CLI never closed is
    /// closed here — its content was already streamed, and a thinking block's signature is
    /// published as on a real `content_block_stop` — counted in `skipped_blocks`, and logged
    /// at `warn`. An open `tool_use` block may hold incomplete arguments: that stays
    /// `protocol_blocks`. The `stop_reason == "tool_use"` handoff does not come here.
    fn close_dangling_blocks(
        &mut self,
        at: &'static str,
        message: &'static str,
    ) -> Result<(), ProtocolError> {
        let open = self.blocks.open_indices();
        if open.is_empty() {
            return Ok(());
        }
        let named = open
            .iter()
            .map(|(i, k)| format!("{}@{i}", k.as_str()))
            .collect::<Vec<_>>()
            .join(",");
        if open.iter().any(|(_, k)| *k == BlockKind::ToolUse) {
            return Err(ProtocolError::new(
                "protocol_blocks",
                format!("{message} ({named})"),
            ));
        }
        for (index, _) in &open {
            self.close_block(*index)?;
            self.skipped_blocks += 1;
        }
        tracing::warn!(
            request_id = %self.request_id,
            cli_version = self.cli_version.as_deref().unwrap_or("unknown"),
            at,
            closed = %named,
            "claude bridge closed content blocks left open at the stop (#145)"
        );
        Ok(())
    }

    fn close_block(&mut self, index: u64) -> Result<(), ProtocolError> {
        match self.blocks.stop(index, Strictness::Strict) {
            Ok(out) => {
                self.pending.extend(out.events);
                if let Some(tool) = out.tool {
                    self.tool_calls.push(CapturedToolCall {
                        id: tool.id,
                        wire_name: tool.wire_name,
                        name: tool.name,
                        arguments: tool.arguments,
                    });
                }
                Ok(())
            }
            Err(StopError::UnknownIndex(i)) => Err(ProtocolError::new(
                "protocol_blocks",
                format!("Stop for unknown content block {i}"),
            )),
            Err(StopError::InvalidToolJson { name }) => Err(ProtocolError::new(
                "tool_arguments",
                format!("Claude emitted invalid arguments for tool {name}"),
            )),
            Err(StopError::NonObjectToolJson { name }) => Err(ProtocolError::new(
                "tool_arguments",
                format!("Claude emitted non-object arguments for tool {name}"),
            )),
        }
    }

    fn accept_result(
        &mut self,
        value: &Value,
        cause: TerminationCause,
    ) -> Result<(), ProtocolError> {
        let Some(record) = value.as_object() else {
            return Err(ProtocolError::new(
                "protocol_result",
                "Claude result was not an object",
            ));
        };
        self.close_dangling_blocks(
            "result",
            "Claude result arrived with unclosed content blocks",
        )?;
        let is_error = record
            .get("is_error")
            .and_then(Value::as_bool)
            .ok_or_else(|| {
                ProtocolError::new(
                    "protocol_result",
                    "Claude result omitted a boolean is_error field",
                )
            })?;
        if let Some(result) = record.get("result") {
            if !result.is_null() && !result.is_string() {
                return Err(ProtocolError::new(
                    "protocol_result",
                    "Claude result contained a non-string result field",
                ));
            }
        }
        self.result_received = true;
        // Usage is deliberately not read from `result` (SPEC §19.4).
        if is_error {
            if self.is_expected_tool_termination(record, cause) {
                return Ok(());
            }
            let status = record
                .get("api_error_status")
                .and_then(Value::as_i64)
                .map(|s| format!(" ({s})"))
                .unwrap_or_default();
            let detail = terminal_result_detail(
                value,
                self.assistant_diagnostic.as_deref(),
                self.rate_limit_failure.as_deref(),
            );
            let detail = client_safe_error(&detail, &self.private_paths);
            return Err(ProtocolError::new(
                "claude_error",
                format!("Claude Code request failed{status}: {detail}"),
            ));
        }
        if self.stop_reason.is_none() {
            if let Some(reason) = record.get("stop_reason").filter(|r| !r.is_null()) {
                self.stop_reason = Some(stop_reason(reason)?);
            }
        }
        if self.stop_reason.as_deref() == Some("tool_use") {
            return Err(ProtocolError::new(
                "protocol_result",
                "Claude returned success after a tool-use stop",
            ));
        }
        if !self.message_started {
            if let Some(text) = record
                .get("result")
                .and_then(Value::as_str)
                .filter(|t| !t.is_empty())
            {
                self.pending.push(ThinEvent::TextDelta(text.to_string()));
            }
        }
        self.successful_finish = Some(if self.stop_reason.as_deref() == Some("max_tokens") {
            FinishReason::Length
        } else {
            FinishReason::Stop
        });
        Ok(())
    }

    fn is_expected_tool_termination(
        &self,
        record: &serde_json::Map<String, Value>,
        cause: TerminationCause,
    ) -> bool {
        cause == TerminationCause::ToolHandoff
            && self.tool_use_stop
            && record.get("subtype").and_then(Value::as_str) == Some("error_during_execution")
            && record.get("api_error_status").is_none_or(Value::is_null)
            && record.get("stop_reason").and_then(Value::as_str) == Some("tool_use")
            && record.get("terminal_reason").and_then(Value::as_str) == Some("aborted_streaming")
            && record.get("result").is_none_or(Value::is_null)
    }

    fn accept_assistant(&mut self, record: &serde_json::Map<String, Value>) {
        if self.assistant_diagnostic.is_some() {
            return;
        }
        let error = record
            .get("error")
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or_default();
        let text = match record.get("message").and_then(|m| m.get("content")) {
            Some(Value::String(s)) => s.trim().to_string(),
            Some(Value::Array(blocks)) => blocks
                .iter()
                .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
                .filter_map(|b| b.get("text").and_then(Value::as_str))
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>()
                .join(" "),
            _ => String::new(),
        };
        let diagnostic = [error, text.as_str()]
            .into_iter()
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join(": ");
        if !diagnostic.is_empty() {
            self.assistant_diagnostic = Some(diagnostic);
        }
    }

    fn accept_rate_limit(&mut self, info: &Value) {
        let Some(notice) = parse_rate_limit_notice(info) else {
            return;
        };
        let key = format!("{notice:?}");
        if self.rate_limit_seen.insert(key) {
            if let Some(utilization) = notice.utilization {
                self.pending.push(ThinEvent::RateLimitAdvisory {
                    utilization,
                    resets_at: notice.resets_at.map(super::protocol::iso8601),
                });
            }
        }
        if notice.rejected {
            self.rate_limit_failure = Some(rate_limit_failure_text(&notice));
        }
    }

    fn apply_usage(&mut self, value: Option<&Value>) {
        let Some(usage) = value.and_then(Value::as_object) else {
            return;
        };
        let num = |k: &str| usage.get(k).and_then(Value::as_u64);
        if let Some(v) = num("input_tokens") {
            self.usage.input = v;
            self.usage_seen = true;
        }
        if let Some(v) = num("output_tokens") {
            self.usage.output = v;
            self.usage_seen = true;
        }
        if let Some(v) = num("cache_read_input_tokens") {
            self.usage.cache_read = v;
        }
        if let Some(v) = num("cache_creation_input_tokens") {
            self.usage.cache_write = v;
        }
    }
}

pub fn rate_limit_failure_text(notice: &RateLimitNotice) -> String {
    let reset = notice
        .resets_at
        .map(|t| format!("; resets at {}", super::protocol::iso8601(t)))
        .unwrap_or_default();
    let reason = notice
        .overage_disabled_reason
        .as_deref()
        .map(|r| format!("; {r}"))
        .unwrap_or_default();
    format!(
        "Claude rate limit rejected ({}){reason}{reset}",
        notice.rate_limit_type
    )
}

fn mismatch(delta_type: &str) -> impl Fn() -> ProtocolError + '_ {
    move || {
        ProtocolError::new(
            "protocol_delta",
            format!("Delta {delta_type} did not match its content block"),
        )
    }
}

fn stop_reason(value: &Value) -> Result<String, ProtocolError> {
    match value.as_str() {
        Some(s) if !s.is_empty() => Ok(s.to_string()),
        _ => Err(ProtocolError::new(
            "protocol_stop",
            format!("Invalid Claude stop reason: {value}"),
        )),
    }
}

fn index(value: Option<&Value>) -> Result<u64, ProtocolError> {
    value.and_then(Value::as_u64).ok_or_else(|| {
        ProtocolError::new(
            "protocol_index",
            format!("Invalid Claude content index: {value:?}"),
        )
    })
}

fn object<'v>(
    value: Option<&'v Value>,
    field: &str,
) -> Result<&'v serde_json::Map<String, Value>, ProtocolError> {
    value
        .and_then(Value::as_object)
        .ok_or_else(|| ProtocolError::new("protocol_shape", format!("{field} must be an object")))
}

/// A dialect target resolves to the Claude name, so `to_pi` can find its row. Anything else
/// resolves to the client name. An empty target map is the client map, which is dialect off.
fn resolve_transport(
    wire: &str,
    names: &BTreeMap<String, String>,
    targets: &BTreeMap<String, crate::thin::dialect::CatalogTarget>,
) -> Option<String> {
    match targets.get(wire) {
        Some(crate::thin::dialect::CatalogTarget::Dialect { row, .. }) => Some((*row).to_string()),
        Some(crate::thin::dialect::CatalogTarget::Client(name)) => Some(name.clone()),
        None => names.get(wire).cloned(),
    }
}

/// `true` when any string value in any tool call's arguments names the private directory.
/// The arguments are parsed, so a JSON-escaped Windows path (`C:\\Users\\…`) is compared as
/// the path it decodes to; slashes are normalised and the match ignores case (R10). Text that
/// does not parse as JSON is checked as one string.
pub fn contains_private_path(calls: &[CapturedToolCall], private_dir: &str) -> bool {
    let needle = normalise_path(private_dir);
    if needle.is_empty() {
        return false;
    }
    calls
        .iter()
        .any(|c| match serde_json::from_str::<Value>(&c.arguments) {
            Ok(value) => value_names_path(&value, &needle),
            Err(_) => normalise_path(&c.arguments).contains(&needle),
        })
}

/// Forward slashes, no repeated separators, lower case.
fn normalise_path(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars().flat_map(char::to_lowercase) {
        let c = if c == '\\' { '/' } else { c };
        if c == '/' && out.ends_with('/') {
            continue;
        }
        out.push(c);
    }
    out.trim_end_matches('/').to_string()
}

fn value_names_path(value: &Value, needle: &str) -> bool {
    match value {
        Value::String(s) => normalise_path(s).contains(needle),
        Value::Array(items) => items.iter().any(|v| value_names_path(v, needle)),
        Value::Object(map) => map.values().any(|v| value_names_path(v, needle)),
        _ => false,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use serde_json::json;

    fn fixture(name: &str) -> Vec<Value> {
        let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../fixtures/thin/claude-bridge");
        let text = std::fs::read_to_string(root.join(name)).expect("fixture");
        text.lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| serde_json::from_str(l).expect("json"))
            .collect()
    }

    fn maps() -> (BTreeSet<String>, BTreeMap<String, String>) {
        let tools: BTreeSet<String> = ["mcp__pi__read", "mcp__pi__bash"]
            .into_iter()
            .map(String::from)
            .collect();
        let names: BTreeMap<String, String> =
            [("mcp__pi__read", "read"), ("mcp__pi__bash", "bash")]
                .into_iter()
                .map(|(a, b)| (a.to_string(), b.to_string()))
                .collect();
        (tools, names)
    }

    #[test]
    fn text_turn_streams_deltas_and_finishes_on_result() {
        let (tools, names) = maps();
        let mut mapper = Mapper::new(&tools, &names, vec![], true);
        let mut text = String::new();
        let mut started = false;
        for record in fixture("text-turn.jsonl") {
            let signals = mapper
                .accept(&record, TerminationCause::None)
                .expect("accept");
            assert!(!signals.terminate_for_tool_use);
            for ev in mapper.drain() {
                match ev {
                    ThinEvent::Started { model, .. } => {
                        started = true;
                        assert_eq!(model, "claude-sonnet-4-6");
                    }
                    ThinEvent::TextDelta(t) => text.push_str(&t),
                    ThinEvent::ReasoningDelta { .. } => {}
                    other => panic!("unexpected {other:?}"),
                }
            }
        }
        assert!(started);
        assert_eq!(text, "Hello from the bridge.");
        assert_eq!(mapper.successful_finish(), Some(FinishReason::Stop));
        let usage = mapper.usage().expect("usage");
        // input 12 + cache_write 100 + cache_read 2000 = prompt; output from message_delta.
        assert_eq!(usage.prompt_tokens, 2112);
        assert_eq!(usage.completion_tokens, 9);
        assert_eq!(usage.prompt_tokens_details.unwrap().cached_tokens, 2000);
    }

    #[test]
    fn two_tool_use_blocks_terminate_on_message_delta_and_yield_two_calls() {
        let (tools, names) = maps();
        let mut mapper = Mapper::new(&tools, &names, vec![], true);
        let mut terminate_at = None;
        let records = fixture("two-tool-turn.jsonl");
        for (i, record) in records.iter().enumerate() {
            let cause = if terminate_at.is_some() {
                TerminationCause::ToolHandoff
            } else {
                TerminationCause::None
            };
            let signals = mapper.accept(record, cause).expect("accept");
            if signals.terminate_for_tool_use {
                assert!(terminate_at.is_none());
                terminate_at = Some(i);
            }
        }
        let at = terminate_at.expect("terminate signal");
        assert_eq!(records[at]["event"]["type"], "message_delta");
        assert!(mapper.tool_use_stop());
        assert!(!mapper.has_open_blocks());
        let calls = mapper.tool_calls();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].name, "read");
        assert_eq!(calls[0].id, "toolu_01");
        assert_eq!(
            serde_json::from_str::<Value>(&calls[0].arguments).unwrap()["path"],
            "a.txt"
        );
        assert_eq!(calls[1].name, "bash");
        assert_eq!(
            serde_json::from_str::<Value>(&calls[1].arguments).unwrap()["command"],
            "ls"
        );
        // The handoff result record after termination is accepted silently.
        assert!(mapper.successful_finish().is_none());
        assert!(mapper.usage().is_some());
    }

    #[test]
    fn parallel_false_keeps_first_call_only() {
        let (tools, names) = maps();
        let mut mapper = Mapper::new(&tools, &names, vec![], false);
        for record in fixture("two-tool-turn.jsonl") {
            mapper
                .accept(&record, TerminationCause::ToolHandoff)
                .expect("accept");
        }
        assert_eq!(mapper.tool_calls().len(), 1);
        assert_eq!(mapper.note_parallel_drops(), 1);
        assert_eq!(mapper.skipped_parallel(), 1);
    }

    #[test]
    fn unknown_tool_is_protocol_error() {
        let (tools, names) = maps();
        let mut mapper = Mapper::new(&tools, &names, vec![], true);
        let records = fixture("text-turn.jsonl");
        mapper.accept(&records[0], TerminationCause::None).unwrap();
        mapper.accept(&records[1], TerminationCause::None).unwrap();
        let bad = json!({"type": "stream_event", "event": {"type": "content_block_start", "index": 0,
            "content_block": {"type": "tool_use", "id": "t", "name": "Bash", "input": {}}}});
        let err = mapper.accept(&bad, TerminationCause::None).unwrap_err();
        assert_eq!(err.code, "tool_unknown");
    }

    #[test]
    fn record_before_init_is_rejected() {
        let (tools, names) = maps();
        let mut mapper = Mapper::new(&tools, &names, vec![], true);
        let err = mapper
            .accept(
                &json!({"type": "stream_event", "event": {"type": "ping"}}),
                TerminationCause::None,
            )
            .unwrap_err();
        assert_eq!(err.code, "protocol_order");
    }

    #[test]
    fn system_status_before_init_is_skipped() {
        // Claude Code 2.1.283: `system/commands_changed` arrives before `system/init`.
        let (tools, names) = maps();
        let mut mapper = Mapper::new(&tools, &names, vec![], true);
        mapper
            .accept(
                &json!({"type": "system", "subtype": "commands_changed", "commands": []}),
                TerminationCause::None,
            )
            .expect("status record before init");
        assert_eq!(mapper.skipped_records(), 1);
        let records = fixture("text-turn.jsonl");
        for record in &records {
            mapper
                .accept(record, TerminationCause::None)
                .expect("accept");
        }
        assert!(mapper
            .drain()
            .iter()
            .any(|e| matches!(e, ThinEvent::Started { .. })));
    }

    #[test]
    fn tool_use_with_open_block_fails() {
        let (tools, names) = maps();
        let mut mapper = Mapper::new(&tools, &names, vec![], true);
        let records = fixture("two-tool-turn.jsonl");
        // init, message_start, block start, delta, (skip stop) -> message_delta tool_use
        mapper.accept(&records[0], TerminationCause::None).unwrap();
        mapper.accept(&records[1], TerminationCause::None).unwrap();
        mapper.accept(&records[2], TerminationCause::None).unwrap();
        mapper.accept(&records[3], TerminationCause::None).unwrap();
        let delta = records
            .iter()
            .find(|r| r["event"]["type"] == "message_delta")
            .unwrap();
        // The open block is text: at a tool_use stop any open block still fails (#145 keeps it).
        let err = mapper.accept(delta, TerminationCause::None).unwrap_err();
        assert_eq!(err.code, "protocol_blocks");
    }

    /// Feed `records` until the first error; return the streamed text, the reasoning
    /// signatures, and the error.
    fn run_records(
        mapper: &mut Mapper<'_>,
        records: &[Value],
    ) -> (String, Vec<String>, Option<ProtocolError>) {
        let mut text = String::new();
        let mut signatures = Vec::new();
        for record in records {
            if let Err(e) = mapper.accept(record, TerminationCause::None) {
                return (text, signatures, Some(e));
            }
            for ev in mapper.drain() {
                match ev {
                    ThinEvent::TextDelta(t) => text.push_str(&t),
                    ThinEvent::ReasoningDelta {
                        signature: Some(s), ..
                    } => signatures.push(s),
                    _ => {}
                }
            }
        }
        (text, signatures, None)
    }

    fn event(event: Value) -> Value {
        json!({"type": "stream_event", "event": event})
    }

    #[test]
    fn open_text_block_at_message_stop_is_closed_and_counted() {
        // #145: Claude Code 2.1.283 ended a 32-minute turn without the last content_block_stop.
        let (tools, names) = maps();
        let mut mapper = Mapper::new(&tools, &names, vec![], true);
        let (text, _, err) = run_records(&mut mapper, &fixture("text-turn-open-block.jsonl"));
        assert!(err.is_none(), "{err:?}");
        assert_eq!(text, "Hello from the bridge.");
        assert_eq!(mapper.successful_finish(), Some(FinishReason::Stop));
        assert_eq!(mapper.skipped_blocks(), 1);
        assert!(!mapper.has_open_blocks());
        assert!(mapper.usage().is_some());
    }

    #[test]
    fn open_thinking_block_at_message_stop_publishes_its_signature() {
        let (tools, names) = maps();
        let mut mapper = Mapper::new(&tools, &names, vec![], true);
        let records = fixture("text-turn.jsonl");
        let mut turn = vec![records[0].clone(), records[1].clone()];
        turn.extend([
            event(
                json!({"type": "content_block_start", "index": 0, "content_block": {"type": "thinking", "thinking": "", "signature": ""}}),
            ),
            event(
                json!({"type": "content_block_delta", "index": 0, "delta": {"type": "thinking_delta", "thinking": "plan"}}),
            ),
            event(
                json!({"type": "content_block_delta", "index": 0, "delta": {"type": "signature_delta", "signature": "sig-1"}}),
            ),
            event(json!({"type": "content_block_stop", "index": 0})),
            event(
                json!({"type": "content_block_start", "index": 1, "content_block": {"type": "thinking", "thinking": "", "signature": ""}}),
            ),
            event(
                json!({"type": "content_block_delta", "index": 1, "delta": {"type": "signature_delta", "signature": "sig-2"}}),
            ),
            event(
                json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}, "usage": {"output_tokens": 3}}),
            ),
            event(json!({"type": "message_stop"})),
            json!({"type": "result", "subtype": "success", "is_error": false, "result": "", "stop_reason": "end_turn"}),
        ]);
        let (_, signatures, err) = run_records(&mut mapper, &turn);
        assert!(err.is_none(), "{err:?}");
        assert_eq!(signatures, vec!["sig-1".to_string(), "sig-2".to_string()]);
        assert_eq!(mapper.skipped_blocks(), 1);
        assert_eq!(mapper.successful_finish(), Some(FinishReason::Stop));
    }

    #[test]
    fn open_text_block_at_result_without_message_stop_is_closed() {
        let (tools, names) = maps();
        let mut mapper = Mapper::new(&tools, &names, vec![], true);
        let turn: Vec<Value> = fixture("text-turn-open-block.jsonl")
            .into_iter()
            .filter(|r| r["event"]["type"] != "message_stop")
            .collect();
        let (text, _, err) = run_records(&mut mapper, &turn);
        assert!(err.is_none(), "{err:?}");
        assert_eq!(text, "Hello from the bridge.");
        assert_eq!(mapper.skipped_blocks(), 1);
        assert_eq!(mapper.successful_finish(), Some(FinishReason::Stop));
    }

    /// init, message_start, text block closed, then a `read` tool_use block left open.
    fn open_tool_turn() -> Vec<Value> {
        fixture("two-tool-turn.jsonl")
            .into_iter()
            .take_while(|r| {
                !(r["event"]["type"] == "content_block_stop" && r["event"]["index"] == 1)
            })
            .collect()
    }

    #[test]
    fn open_tool_use_block_at_message_stop_is_still_protocol_blocks() {
        let (tools, names) = maps();
        let mut mapper = Mapper::new(&tools, &names, vec![], true);
        let mut turn = open_tool_turn();
        turn.extend([
            event(
                json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}, "usage": {"output_tokens": 3}}),
            ),
            event(json!({"type": "message_stop"})),
        ]);
        let (_, _, err) = run_records(&mut mapper, &turn);
        let err = err.expect("open tool_use at message_stop");
        assert_eq!(err.code, "protocol_blocks");
        assert!(
            err.message.contains("unclosed content blocks"),
            "{}",
            err.message
        );
        assert!(err.message.contains("tool_use@1"), "{}", err.message);
        assert_eq!(mapper.skipped_blocks(), 0);
    }

    #[test]
    fn open_tool_use_block_at_tool_use_stop_is_still_protocol_blocks() {
        let (tools, names) = maps();
        let mut mapper = Mapper::new(&tools, &names, vec![], true);
        let mut turn = open_tool_turn();
        turn.push(event(
            json!({"type": "message_delta", "delta": {"stop_reason": "tool_use"}, "usage": {"output_tokens": 3}}),
        ));
        let (_, _, err) = run_records(&mut mapper, &turn);
        let err = err.expect("open tool_use at stop_reason=tool_use");
        assert_eq!(err.code, "protocol_blocks");
        assert!(!mapper.tool_use_stop());
    }

    #[test]
    fn open_text_beside_open_tool_use_at_result_is_still_protocol_blocks() {
        let (tools, names) = maps();
        let mut mapper = Mapper::new(&tools, &names, vec![], true);
        let mut turn: Vec<Value> = open_tool_turn()
            .into_iter()
            .filter(|r| r["event"]["type"] != "content_block_stop")
            .collect();
        turn.push(json!({"type": "result", "subtype": "success", "is_error": false, "result": "", "stop_reason": "end_turn"}));
        let (_, _, err) = run_records(&mut mapper, &turn);
        let err = err.expect("open tool_use at result");
        assert_eq!(err.code, "protocol_blocks");
        assert!(err.message.contains("text@0,tool_use@1"), "{}", err.message);
        assert!(mapper.successful_finish().is_none());
    }

    #[test]
    fn error_result_names_detail() {
        let (tools, names) = maps();
        let mut mapper = Mapper::new(&tools, &names, vec![], true);
        let records = fixture("text-turn.jsonl");
        mapper.accept(&records[0], TerminationCause::None).unwrap();
        let err = mapper
            .accept(
                &json!({"type": "result", "is_error": true, "api_error_status": 529, "result": "Overloaded"}),
                TerminationCause::None,
            )
            .unwrap_err();
        assert_eq!(err.code, "claude_error");
        assert_eq!(err.message, "Claude Code request failed (529): Overloaded");
    }

    #[test]
    fn error_result_is_redacted_and_bounded() {
        // R14: a CLI error with a token, the private dir (JSON-escaped too) and a huge dump.
        let (tools, names) = maps();
        let private = r"C:\Users\x\.thickapi\tmp\req-1".to_string();
        let mut mapper = Mapper::new(&tools, &names, vec![private], true);
        let records = fixture("text-turn.jsonl");
        mapper.accept(&records[0], TerminationCause::None).unwrap();
        let result = format!(
            r"auth sk-ant-oat01-SECRETSECRET failed; read C:\Users\x\.thickapi\tmp\req-1\prompt.txt and C:\\Users\\x\\.thickapi\\tmp\\req-1\\catalog.json; {}",
            "x".repeat(10_000)
        );
        let err = mapper
            .accept(
                &json!({"type": "result", "is_error": true, "api_error_status": 500, "result": result}),
                TerminationCause::None,
            )
            .unwrap_err();
        assert_eq!(err.code, "claude_error");
        assert!(!err.message.contains("SECRETSECRET"), "{}", err.message);
        assert!(!err.message.contains(".thickapi"), "{}", err.message);
        assert!(err.message.contains("<PRIVATE>"));
        assert!(err.message.chars().count() <= 600, "{}", err.message.len());
    }

    #[test]
    fn rate_limit_event_yields_advisory_once() {
        let (tools, names) = maps();
        let mut mapper = Mapper::new(&tools, &names, vec![], true);
        let records = fixture("text-turn.jsonl");
        mapper.accept(&records[0], TerminationCause::None).unwrap();
        mapper.drain();
        let ev = json!({"type": "rate_limit_event", "rate_limit_info": {
            "status": "allowed_warning", "rateLimitType": "five_hour", "utilization": 0.5, "resetsAt": 1_800_000_000}});
        mapper.accept(&ev, TerminationCause::None).unwrap();
        mapper.accept(&ev, TerminationCause::None).unwrap();
        let events = mapper.drain();
        assert_eq!(events.len(), 1);
        assert!(
            matches!(events[0], ThinEvent::RateLimitAdvisory { utilization, .. } if (utilization - 0.5).abs() < 1e-9)
        );
    }

    #[test]
    fn private_path_detection_handles_slashes() {
        let calls = vec![CapturedToolCall {
            id: "1".into(),
            name: "read".into(),
            wire_name: String::new(),
            arguments: r#"{"path":"C:/Users/x/.thickapi/tmp/req/system-prompt.txt"}"#.into(),
        }];
        assert!(contains_private_path(
            &calls,
            r"C:\Users\x\.thickapi\tmp\req"
        ));
        assert!(!contains_private_path(
            &calls,
            r"C:\Users\x\.thickapi\tmp\other"
        ));
    }

    #[test]
    fn private_path_detection_handles_escaped_backslashes_case_and_nesting() {
        let dir = r"C:\Users\x\.thickapi\tmp\req";
        let call = |arguments: &str| {
            vec![CapturedToolCall {
                id: "1".into(),
                name: "read".into(),
                wire_name: String::new(),
                arguments: arguments.into(),
            }]
        };
        // JSON-escaped Windows path, as the model writes it.
        assert!(contains_private_path(
            &call(r#"{"path":"C:\\Users\\x\\.thickapi\\tmp\\req\\system-prompt.txt"}"#),
            dir
        ));
        // Mixed case.
        assert!(contains_private_path(
            &call(r#"{"path":"c:\\users\\X\\.THICKAPI\\tmp\\REQ\\catalog.json"}"#),
            dir
        ));
        // Nested inside an object and an array (bash command, edit lists).
        assert!(contains_private_path(
            &call(r#"{"edits":[{"file":{"p":"C:\\Users\\x\\.thickapi\\tmp\\req\\a"}}],"n":1}"#),
            dir
        ));
        assert!(contains_private_path(
            &call(r#"{"command":"type C:\\Users\\x\\.thickapi\\tmp\\req\\prompt.txt"}"#),
            dir
        ));
        // A sibling directory is not the private one.
        assert!(!contains_private_path(
            &call(r#"{"path":"C:\\Users\\x\\.thickapi\\tmp\\other\\a"}"#),
            dir
        ));
        // Unparseable arguments are checked as text.
        assert!(contains_private_path(
            &call(r"not json C:\Users\x\.thickapi\tmp\req\a"),
            dir
        ));
    }

    #[test]
    fn unknown_record_event_and_block_are_skipped_not_502() {
        let (tools, names) = maps();
        let mut mapper = Mapper::new(&tools, &names, vec![], true);
        let records = fixture("text-turn.jsonl");
        mapper.accept(&records[0], TerminationCause::None).unwrap();
        mapper
            .accept(
                &json!({"type": "status_update", "status": "thinking"}),
                TerminationCause::None,
            )
            .unwrap();
        assert_eq!(mapper.skipped_records(), 1);
        mapper.accept(&records[1], TerminationCause::None).unwrap();
        mapper
            .accept(
                &json!({"type": "stream_event", "event": {"type": "keepalive"}}),
                TerminationCause::None,
            )
            .unwrap();
        assert_eq!(mapper.skipped_events(), 1);
        mapper
            .accept(
                &json!({
                    "type": "stream_event",
                    "event": {
                        "type": "content_block_start",
                        "index": 9,
                        "content_block": {"type": "search_result", "title": "x"}
                    }
                }),
                TerminationCause::None,
            )
            .unwrap();
        assert_eq!(mapper.skipped_blocks(), 1);
        for record in records.iter().skip(2) {
            mapper
                .accept(record, TerminationCause::None)
                .expect("text turn continues after skipped vocabulary");
        }
        assert_eq!(mapper.successful_finish(), Some(FinishReason::Stop));
    }

    #[test]
    fn unexpected_abort_after_tool_handoff_keeps_tool_calls() {
        let (tools, names) = maps();
        let mut mapper = Mapper::new(&tools, &names, vec![], true);
        let records = fixture("two-tool-turn.jsonl");
        let mut cause = TerminationCause::None;
        for record in &records {
            let signals = mapper.accept(record, cause).expect("accept");
            if signals.terminate_for_tool_use {
                cause = TerminationCause::ToolHandoff;
            }
        }
        assert!(mapper.tool_use_stop());
        let before = mapper.tool_calls().len();
        assert_eq!(before, 2);
        mapper
            .accept(
                &json!({
                    "type": "result",
                    "is_error": true,
                    "subtype": "error_during_execution",
                    "api_error_status": 1,
                    "result": "aborted",
                    "stop_reason": "error"
                }),
                TerminationCause::ToolHandoff,
            )
            .expect("post-handoff abort is drained");
        mapper
            .accept(
                &json!({"type": "weird_abort", "reason": "killed"}),
                TerminationCause::ToolHandoff,
            )
            .expect("unknown post-handoff record is drained");
        assert_eq!(mapper.tool_calls().len(), before);
        assert!(mapper.skipped_records() >= 1);
    }
}
