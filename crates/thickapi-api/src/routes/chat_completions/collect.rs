use std::time::Duration;

use axum::{
    http::{HeaderMap, HeaderValue},
    response::{IntoResponse, Response},
    Json,
};
use thickapi_core::thin::openai::{
    self, ChatCompletion, CompletionChoice, CompletionMessage, FinishReason, FunctionCall,
    ToolCallOut, Usage,
};
use thickapi_core::thin::{ThinError, ThinEvent};
use tokio::sync::mpsc;

use super::access_log::{AccessFields, RequestLogGuard};
use super::{route_idle_error, Ctx};

// ---- non-stream ----

/// One `thickapi_api::access` line for this request, then the same response or error the body
/// below produces (SPEC A2 deliverable 3). `log`'s `Drop` covers a client that disconnects
/// while this future is still awaiting the adapter. The body runs as an inline block, not a
/// second named function, so it does not add a second `Result<_, ThinError>` return site next
/// to this one for clippy's `result_large_err` to flag (`ThinError` is already over its
/// threshold everywhere in this crate; the brief adds no new `#[allow]` for it).
pub(super) async fn collect_response(
    ctx: Ctx,
    headers: HeaderMap,
    early: Vec<ThinEvent>,
    mut rx: mpsc::Receiver<ThinEvent>,
    idle: Option<Duration>,
    mut log: RequestLogGuard,
) -> Result<Response, ThinError> {
    let outcome: Result<(Response, FinishReason, Usage), ThinError> = async {
        let mut text = String::new();
        let mut reasoning = String::new();
        let mut signature: Option<String> = None;
        let mut details: Vec<thickapi_core::thin::openai::ReasoningDetail> = Vec::new();
        let mut calls: Vec<ToolCallOut> = Vec::new();
        let mut usage = Usage::default();
        let mut finish: Option<FinishReason> = None;
        let mut headers = headers;

        let mut apply = |ev: ThinEvent, headers: &mut HeaderMap| -> Result<bool, ThinError> {
            match ev {
                ThinEvent::Started { .. } => {}
                ThinEvent::TextDelta(t) => text.push_str(&t),
                ThinEvent::ReasoningDelta {
                    text: t,
                    signature: s,
                    details: d,
                } => {
                    reasoning.push_str(&t);
                    if s.is_some() {
                        signature = s;
                    }
                    if let Some(extra) = d {
                        details.extend(extra);
                    }
                }
                ThinEvent::ToolCall {
                    id,
                    name,
                    arguments,
                } => {
                    if ctx.parallel || calls.is_empty() {
                        thickapi_core::thin::dialect::note_tools(&ctx.id, &[name.as_str()]);
                        calls.push(ToolCallOut {
                            index: Some(calls.len()),
                            id: openai::route_call_id(id),
                            kind: "function".into(),
                            function: FunctionCall { name, arguments },
                        });
                    }
                }
                ThinEvent::Usage(u) => usage = u,
                ThinEvent::RateLimitAdvisory {
                    utilization,
                    resets_at,
                } => {
                    let value = match resets_at {
                        Some(r) => format!("{utilization:.3};reset={r}"),
                        None => format!("{utilization:.3}"),
                    };
                    if let Ok(v) = HeaderValue::from_str(&value) {
                        headers.insert("x-thickapi-rate-limit", v);
                    }
                }
                ThinEvent::Finish(f) => {
                    finish = Some(f);
                    return Ok(true);
                }
                ThinEvent::Error(err) => return Err(err),
            }
            Ok(false)
        };

        let mut done = false;
        for ev in early {
            if apply(ev, &mut headers)? {
                done = true;
            }
        }
        while !done {
            let next = match idle {
                Some(bound) => match tokio::time::timeout(bound, rx.recv()).await {
                    Ok(ev) => ev,
                    Err(_) => {
                        return Err(route_idle_error(bound));
                    }
                },
                None => rx.recv().await,
            };
            match next {
                Some(ev) => done = apply(ev, &mut headers)?,
                None => break,
            }
        }

        // The adapter closed without `Finish`: a truncated answer, never a whole one (#121).
        // Same error the stream path sends.
        let Some(finish) = finish else {
            return Err(ThinError::vendor_down(
                "thickapi",
                "stream ended without finish",
            ));
        };
        let usage_out = usage.clone();
        let completion = ChatCompletion {
            id: ctx.id,
            object: "chat.completion",
            created: ctx.created,
            model: ctx.model,
            choices: vec![CompletionChoice {
                index: 0,
                message: CompletionMessage {
                    role: "assistant",
                    content: if text.is_empty() && !calls.is_empty() {
                        None
                    } else {
                        Some(text)
                    },
                    reasoning_content: (!reasoning.is_empty()).then_some(reasoning),
                    reasoning_signature: signature,
                    reasoning_details: (!details.is_empty()).then_some(details),
                    tool_calls: (!calls.is_empty()).then_some(calls),
                },
                finish_reason: finish,
            }],
            usage,
        };
        let mut response = Json(completion).into_response();
        response.headers_mut().extend(headers);
        Ok((response, finish, usage_out))
    }
    .await;
    match outcome {
        Ok((response, finish, usage)) => {
            log.finish(AccessFields::success(finish, &usage));
            Ok(response)
        }
        Err(err) => {
            log.finish(AccessFields::error(&err, false));
            Err(err)
        }
    }
}
