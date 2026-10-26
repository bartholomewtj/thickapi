use std::convert::Infallible;
use std::time::Duration;

// Tokio's clock, so a `start_paused` test drives the quiet clock too (std `Instant`
// ignores a paused runtime). Identical to std time outside tests.
use tokio::time::Instant;

use axum::{
    body::Body,
    http::{header, HeaderMap, StatusCode},
    response::Response,
};
use futures_util::{stream, StreamExt};
use thickapi_core::thin::openai::{
    self, ChatChunk, ChunkChoice, Delta, FinishReason, FunctionCall, ToolCallOut, Usage,
};
use thickapi_core::thin::{ThinError, ThinEvent};
use tokio::sync::mpsc;

use super::access_log::{AccessFields, RequestLogGuard};
use super::{route_idle_error, AbortOnDrop, Ctx};

// ---- stream ----

pub(super) struct StreamBounds {
    pub(super) keepalive: Option<Duration>,
    pub(super) idle: Option<Duration>,
}

/// Everything the stream body only needs to *hold* for its lifetime, not read: the adapter's
/// `JoinHandle` (aborted on drop), the in-flight gauge, and the one access log line the request
/// owes (SPEC A2 deliverable 3). One field instead of three keeps `stream_response` under
/// clippy's `too_many_arguments` without an `#[allow]`.
pub(super) struct StreamGuards {
    pub(super) runner: AbortOnDrop,
    pub(super) inflight: Option<crate::ThinInFlightGuard>,
    pub(super) log: RequestLogGuard,
}

pub(super) fn stream_response(
    ctx: Ctx,
    headers: HeaderMap,
    early: Vec<ThinEvent>,
    rx: mpsc::Receiver<ThinEvent>,
    bounds: StreamBounds,
    guards: StreamGuards,
) -> Response {
    let state = StreamState {
        ctx,
        early: early.into_iter().collect(),
        rx,
        sent_role: false,
        tool_index: 0,
        first_call_seen: false,
        usage: None,
        finished: false,
        done: false,
        keepalive: bounds.keepalive,
        idle: bounds.idle,
        quiet_since: Instant::now(),
        _runner: guards.runner,
        _inflight: guards.inflight,
        log: guards.log,
    };
    let body = stream::unfold(state, |mut s| async move {
        if s.done {
            return None;
        }
        if let Some(ev) = s.early.pop_front() {
            let frames = s.render(ev);
            return Some((frames, s));
        }
        let remaining_idle = s
            .idle
            .map(|bound| bound.saturating_sub(s.quiet_since.elapsed()));
        if remaining_idle == Some(Duration::ZERO) {
            s.done = true;
            return Some((
                match s.idle {
                    Some(idle) => s.render(ThinEvent::Error(route_idle_error(idle))),
                    None => String::new(),
                },
                s,
            ));
        }
        let wait = match (s.keepalive, remaining_idle) {
            (Some(ka), Some(rem)) => Some(ka.min(rem)),
            (Some(ka), None) => Some(ka),
            (None, Some(rem)) => Some(rem),
            (None, None) => None,
        };
        let ev = match wait {
            None => s.rx.recv().await,
            Some(d) => match tokio::time::timeout(d, s.rx.recv()).await {
                Ok(ev) => ev,
                Err(_) => {
                    if remaining_idle.is_some_and(|rem| rem <= d) {
                        s.done = true;
                        return Some((
                            match s.idle {
                                Some(idle) => s.render(ThinEvent::Error(route_idle_error(idle))),
                                None => String::new(),
                            },
                            s,
                        ));
                    }
                    return Some((KEEPALIVE.to_string(), s));
                }
            },
        };
        let frames = match ev {
            // Adapter vanished mid-stream: name it rather than dangling.
            None if !s.finished => s.render(ThinEvent::Error(ThinError::vendor_down(
                "thickapi",
                "stream ended without finish",
            ))),
            None => return None,
            Some(ev) => {
                s.quiet_since = Instant::now();
                s.render(ev)
            }
        };
        Some((frames, s))
    });
    let body = Body::from_stream(body.map(Ok::<String, Infallible>));
    let mut response = Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .header("x-accel-buffering", "no")
        .body(body)
        .unwrap_or_else(|err| {
            tracing::error!(%err, "failed to build SSE response");
            Response::new(Body::empty())
        });
    response.headers_mut().extend(headers);
    response
}
struct StreamState {
    ctx: Ctx,
    early: std::collections::VecDeque<ThinEvent>,
    rx: mpsc::Receiver<ThinEvent>,
    sent_role: bool,
    tool_index: usize,
    first_call_seen: bool,
    usage: Option<Usage>,
    finished: bool,
    done: bool,
    keepalive: Option<Duration>,
    idle: Option<Duration>,
    quiet_since: Instant,
    _runner: AbortOnDrop,
    _inflight: Option<crate::ThinInFlightGuard>,
    log: RequestLogGuard,
}

impl StreamState {
    fn chunk(&self, delta: Delta, finish: Option<FinishReason>) -> ChatChunk {
        ChatChunk {
            id: self.ctx.id.clone(),
            object: "chat.completion.chunk",
            created: self.ctx.created,
            model: self.ctx.model.clone(),
            choices: vec![ChunkChoice {
                index: 0,
                delta,
                finish_reason: finish,
            }],
            usage: None,
        }
    }

    fn role_prefix(&mut self) -> String {
        if self.sent_role {
            return String::new();
        }
        self.sent_role = true;
        sse_json(&self.chunk(
            Delta {
                role: Some("assistant"),
                content: Some(String::new()),
                ..Default::default()
            },
            None,
        ))
    }

    fn render(&mut self, ev: ThinEvent) -> String {
        match ev {
            ThinEvent::Started { .. } | ThinEvent::RateLimitAdvisory { .. } => String::new(),
            ThinEvent::TextDelta(t) => {
                let mut out = self.role_prefix();
                out.push_str(&sse_json(&self.chunk(
                    Delta {
                        content: Some(t),
                        ..Default::default()
                    },
                    None,
                )));
                out
            }
            ThinEvent::ReasoningDelta {
                text,
                signature,
                details,
            } => {
                let mut out = self.role_prefix();
                out.push_str(&sse_json(&self.chunk(
                    Delta {
                        reasoning_content: Some(text),
                        reasoning_signature: signature,
                        reasoning_details: details,
                        ..Default::default()
                    },
                    None,
                )));
                out
            }
            ThinEvent::ToolCall {
                id,
                name,
                arguments,
            } => {
                if !self.ctx.parallel && self.first_call_seen {
                    return String::new();
                }
                self.first_call_seen = true;
                thickapi_core::thin::dialect::note_tools(&self.ctx.id, &[name.as_str()]);
                let mut out = self.role_prefix();
                // Vendor ids (toolu_…) are replaced by OpenAI-shaped ids (SPEC §19.4); clients replay ours.
                // An adapter that already minted a route-shaped id (agy keeps thought signatures
                // under it) keeps it.
                let call = ToolCallOut {
                    index: Some(self.tool_index),
                    id: openai::route_call_id(id),
                    kind: "function".into(),
                    function: FunctionCall { name, arguments },
                };
                self.tool_index += 1;
                out.push_str(&sse_json(&self.chunk(
                    Delta {
                        tool_calls: Some(vec![call]),
                        ..Default::default()
                    },
                    None,
                )));
                out
            }
            ThinEvent::Usage(u) => {
                self.usage = Some(u);
                String::new()
            }
            ThinEvent::Finish(reason) => {
                self.finished = true;
                self.done = true;
                let mut out = self.role_prefix();
                out.push_str(&sse_json(&self.chunk(Delta::default(), Some(reason))));
                if self.ctx.include_usage {
                    let usage_chunk = ChatChunk {
                        id: self.ctx.id.clone(),
                        object: "chat.completion.chunk",
                        created: self.ctx.created,
                        model: self.ctx.model.clone(),
                        choices: vec![],
                        usage: Some(self.usage.clone().unwrap_or_default()),
                    };
                    out.push_str(&sse_json(&usage_chunk));
                }
                out.push_str("data: [DONE]\n\n");
                self.log.finish(AccessFields::success(
                    reason,
                    &self.usage.clone().unwrap_or_default(),
                ));
                out
            }
            // The error frame is the last thing sent, and `[DONE]` is not: `[DONE]` tells an
            // OpenAI client the stream completed, and no `finish_reason` went out (SPEC §19.4).
            ThinEvent::Error(err) => {
                self.finished = true;
                self.done = true;
                let err = err.with_request_id(self.ctx.id.clone());
                self.log.finish(AccessFields::error(&err, true));
                sse_json(&err.stream_body())
            }
        }
    }
}

/// An SSE comment: bytes on the wire that no client parses as data.
pub(super) const KEEPALIVE: &str = ": keepalive\n\n";
pub(super) fn sse_json<T: serde::Serialize>(v: &T) -> String {
    format!(
        "data: {}\n\n",
        serde_json::to_string(v).unwrap_or_else(|_| "{}".into())
    )
}
