//! One `thickapi_api::access` line per thin request end, plus a `warn` for every error after
//! `Started` (SPEC §19.4, §19.9, §19.15 R5; brief `requests/archive/a2-access-log.md`).
//!
//! No prompt text, tool arguments, tool results, system prompts, or raw token counts beyond the
//! usage totals already returned to the client (SPEC §19.9, §19.15 R5).

use std::time::Instant;

use thickapi_core::thin::openai::{FinishReason, Usage};
use thickapi_core::thin::ThinError;

/// Fixed facts about a thin request, known once the catalogue row and adapter are resolved
/// (SPEC A2 deliverable 2). Built once in `start::handle_inner`, carried into `stream_response` /
/// `collect_response` on the post-`Started` path, or logged directly for a request that never
/// reaches `Started` (the start-deadline 503).
pub(super) struct RequestLog {
    pub(super) request_id: String,
    pub(super) key: String,
    pub(super) model: String,
    pub(super) wire_model: String,
    /// Vendor parameters the adapter sent (`k=v` comma-joined), `None` logs `-` (SPEC §19.15 R5).
    pub(super) wire_params: Option<String>,
    pub(super) path: &'static str,
    pub(super) fallback: Option<&'static str>,
    pub(super) stream: bool,
    pub(super) t0: Instant,
    /// Time to `Started`, or `None` when it never arrived (a pre-`Started` end).
    pub(super) ttfe_ms: Option<u64>,
}

/// `t_started.map(...)` reused at every `RequestLog` construction site (post-`Started` and the
/// pre-`Started` start-deadline alike), kept as a free function so no site needs its own
/// `saturating_duration_since` line.
pub(super) fn ttfe_ms(t0: Instant, t_started: Option<Instant>) -> Option<u64> {
    t_started.map(|t| t.saturating_duration_since(t0).as_millis() as u64)
}

/// What one request end line reports beyond the fixed `RequestLog` facts.
pub(super) struct AccessFields {
    status: Option<u16>,
    stream_error: bool,
    finish_reason: Option<&'static str>,
    prompt_tokens: Option<u64>,
    completion_tokens: Option<u64>,
    cached_tokens: Option<u64>,
    code: Option<&'static str>,
    error_message: Option<String>,
    disconnect: bool,
}

impl AccessFields {
    pub(super) fn success(finish: FinishReason, usage: &Usage) -> Self {
        Self {
            status: Some(200),
            stream_error: false,
            finish_reason: Some(finish_reason_str(finish)),
            prompt_tokens: Some(usage.prompt_tokens),
            completion_tokens: Some(usage.completion_tokens),
            cached_tokens: Some(
                usage
                    .prompt_tokens_details
                    .as_ref()
                    .map(|d| d.cached_tokens)
                    .unwrap_or(0),
            ),
            code: None,
            error_message: None,
            disconnect: false,
        }
    }

    /// `mid_stream`: the error arrived as an SSE frame after the 200 headers were already
    /// sent, so the status line stays 200; `code` (and the `warn` line below) is what marks it
    /// as a failure (SPEC: "status ... or 200 + stream_error for a mid-stream frame").
    pub(super) fn error(err: &ThinError, mid_stream: bool) -> Self {
        Self {
            status: Some(if mid_stream { 200 } else { err.status }),
            stream_error: mid_stream,
            finish_reason: None,
            prompt_tokens: None,
            completion_tokens: None,
            cached_tokens: None,
            code: Some(err.code),
            error_message: Some(err.message.clone()),
            disconnect: false,
        }
    }

    fn disconnect(stream: bool) -> Self {
        Self {
            status: stream.then_some(200),
            stream_error: false,
            finish_reason: None,
            prompt_tokens: None,
            completion_tokens: None,
            cached_tokens: None,
            code: None,
            error_message: None,
            disconnect: true,
        }
    }
}

fn finish_reason_str(f: FinishReason) -> &'static str {
    match f {
        FinishReason::Stop => "stop",
        FinishReason::ToolCalls => "tool_calls",
        FinishReason::Length => "length",
        FinishReason::ContentFilter => "content_filter",
    }
}

/// Renders `None` as the literal text `null`, not an empty field, so a rolled log line reads
/// the same whether a human or a script (`Select-String`) is looking at it.
struct OrNull<T>(Option<T>);

impl<T: std::fmt::Display> std::fmt::Display for OrNull<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.0 {
            Some(v) => write!(f, "{v}"),
            None => write!(f, "null"),
        }
    }
}

/// Owns the one `thickapi_api::access` line this request owes the log. `finish(..)` emits it
/// on a normal end (clean finish, error frame, route idle, start-deadline); dropped without a
/// `finish` call — a client that walked away mid-stream or mid-collect — it emits the same line
/// with `disconnect=true` instead, so a test can fail on zero lines as readily as on two.
pub(super) struct RequestLogGuard(Option<RequestLog>);

impl RequestLogGuard {
    pub(super) fn new(log: RequestLog) -> Self {
        Self(Some(log))
    }

    pub(super) fn finish(&mut self, fields: AccessFields) {
        if let Some(log) = self.0.take() {
            emit(&log, &fields);
        }
    }
}

impl Drop for RequestLogGuard {
    fn drop(&mut self) {
        if let Some(log) = self.0.take() {
            let fields = AccessFields::disconnect(log.stream);
            emit(&log, &fields);
        }
    }
}

fn emit_access_line(log: &RequestLog, fields: &AccessFields) {
    let duration_ms = log.t0.elapsed().as_millis() as u64;
    let status = match (fields.status, fields.stream_error) {
        (Some(s), true) => format!("{s} stream_error"),
        (Some(s), false) => s.to_string(),
        (None, _) => "null".to_string(),
    };
    tracing::info!(
        target: "thickapi_api::access",
        request_id = %log.request_id,
        key = %log.key,
        model = %log.model,
        wire_model = %log.wire_model,
        wire_params = %log.wire_params.as_deref().unwrap_or("-"),
        path = %log.path,
        fallback = %OrNull(log.fallback),
        status = %status,
        ttfe_ms = %OrNull(log.ttfe_ms),
        duration_ms,
        finish_reason = %OrNull(fields.finish_reason),
        prompt_tokens = %OrNull(fields.prompt_tokens),
        completion_tokens = %OrNull(fields.completion_tokens),
        cached_tokens = %OrNull(fields.cached_tokens),
        code = %OrNull(fields.code),
        disconnect = fields.disconnect,
        dialect_error = %thickapi_core::thin::dialect::take_error(&log.request_id).as_deref().unwrap_or("-"),
        tools_out = %thickapi_core::thin::dialect::take_tools(&log.request_id).as_deref().unwrap_or("-"),
        spawn_ms = %OrNull(thickapi_core::thin::dialect::take_spawn_ms(&log.request_id)),
        "thin request end"
    );
}

/// Every `ThinEvent::Error` reaching this — the only caller is `RequestLogGuard`, which is
/// built after `Started` and used by `stream_response` / `collect_response` alone — arrived
/// after `Started` (SPEC A2 deliverable 4). A pre-`Started` end (start-deadline) logs the
/// access line directly through `emit_pre_started` below and never calls this.
fn emit_post_started_warn(log: &RequestLog, fields: &AccessFields) {
    if let Some(code) = fields.code {
        tracing::warn!(
            target: "thickapi_api",
            request_id = %log.request_id,
            model = %log.model,
            wire_model = %log.wire_model,
            path = %log.path,
            code = %code,
            message = %fields.error_message.as_deref().unwrap_or(""),
            "thin request error after Started"
        );
    }
}

fn emit(log: &RequestLog, fields: &AccessFields) {
    emit_access_line(log, fields);
    emit_post_started_warn(log, fields);
}

/// A request that never reached `Started` (the start-deadline 503): the access line still owed,
/// but never the post-`Started` `warn` — the adapter's own pre-`Started` `warn` lines are not
/// doubled (SPEC A2 deliverables 3, 4).
pub(super) fn emit_pre_started(log: &RequestLog, err: &ThinError) {
    emit_access_line(log, &AccessFields::error(err, false));
}

#[cfg(test)]
pub(super) fn test_guard() -> RequestLogGuard {
    RequestLogGuard::new(RequestLog {
        request_id: "chatcmpl-test".into(),
        key: "test".into(),
        model: "m".into(),
        wire_model: "m".into(),
        wire_params: None,
        path: "oauth",
        fallback: None,
        stream: true,
        t0: Instant::now(),
        ttfe_ms: Some(0),
    })
}
