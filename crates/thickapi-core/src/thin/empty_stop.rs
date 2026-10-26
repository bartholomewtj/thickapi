//! SPEC §19.16: an empty Claude stop while `tools` were offered. One guard for both Claude
//! paths (R23): it sits on the channel between a path's run and the route, so neither mapper
//! learns a second policy.
//!
//! The guard forwards `Started` at once (the route commits its 200 headers but sends no chunk
//! for it) and holds back what could still turn out to be an empty answer: whitespace-only
//! text or reasoning, usage, and the finish. The first non-whitespace text or reasoning, or a
//! tool call, flushes what it held and from then on everything passes through. A `stop` with
//! nothing forwarded is an empty tool stop: the attempt, its usage included, is dropped and
//! the same path runs once more. A second empty tool stop is `502 empty_completion`.
//!
//! `tool_choice: required` (and a named choice) is the hold-all verdict: text and reasoning are
//! held too, until a tool call arrives. A stop with none drops the attempt and retries once
//! with a user record appended. A second miss is the same 502, with its own message.

use std::future::Future;

use tokio::sync::mpsc;

use crate::thin::openai::{FinishReason, ToolChoice};
use crate::thin::{EventSender, ThinError, ThinEvent, ThinRequest};

/// Appended to the request the `required` retry sees.
const MUST_CALL: &str = "You must call one of the available tools now.";

/// Capacity of the channel between an attempt and the guard.
const ATTEMPT_CHANNEL: usize = 64;

/// Two attempts at most: the first and one same-path retry.
const ATTEMPTS: u32 = 2;

/// How one attempt ended, as far as the guard could see.
enum Verdict {
    /// Something was forwarded, or the attempt ended without an empty tool stop.
    Passed,
    /// `stop` with no text, reasoning or tool call; nothing but `Started` went out.
    EmptyToolStop {
        path: &'static str,
        model: String,
        output_tokens: Option<u64>,
    },
    /// The route stopped reading.
    ClientGone,
}

/// State shared by both attempts.
struct Shared {
    tools_offered: bool,
    /// `required` or named: hold text and reasoning until a tool call (SPEC §19.16).
    hold_all: bool,
    /// `Started` went to the route; a retry's own `Started` is dropped.
    started_sent: bool,
}

/// Run `attempt` under the §19.16 guard. `attempt(n, req, tx)` is one run on one path (`n`
/// is 0 for the first, 1 for the retry) that sends its events to `tx`. `req` is owned so the
/// attempt can outlive this call. The `required` retry receives the request with a user
/// record appended. The caller keeps its concurrency slot across both runs. `cli` names the
/// installed CLI version for the error message (asked only when that error is sent). Returns
/// the result of the last attempt that ran, whether a `Started` reached the route — when it
/// did, the caller must report a failure of that attempt on the channel, never by trying
/// another path (§19.15 R3) — and whether that attempt was kept: `false` when the guard
/// dropped it as an empty tool stop (so it is not a success, #126) or the client went away.
pub async fn run_guarded<T, F, Fut>(
    req: &ThinRequest,
    tx: &EventSender,
    cli: impl FnOnce() -> Option<String>,
    mut attempt: F,
) -> (T, bool, bool)
where
    F: FnMut(u32, ThinRequest, EventSender) -> Fut,
    Fut: Future<Output = T>,
{
    let choice = ToolChoice::parse(req.request.tool_choice.as_ref()).unwrap_or(ToolChoice::Auto);
    let mut shared = Shared {
        tools_offered: !req.request.tools.is_empty(),
        hold_all: choice.forces_tool(),
        started_sent: false,
    };
    let mut cli = Some(cli);
    let mut current = req.clone();
    let mut n = 0;
    loop {
        let (inner_tx, inner_rx) = mpsc::channel(ATTEMPT_CHANNEL);
        let (result, verdict) = tokio::join!(
            attempt(n, current.clone(), inner_tx),
            forward(inner_rx, tx, &mut shared)
        );
        let Verdict::EmptyToolStop {
            path,
            model,
            output_tokens,
        } = verdict
        else {
            let kept = matches!(verdict, Verdict::Passed);
            return (result, shared.started_sent, kept);
        };
        tracing::warn!(
            request_id = %req.request_id,
            model = %req.entry.id,
            wire_model = %model,
            path,
            finish_reason = "stop",
            output_tokens,
            attempt = n + 1,
            "claude empty stop while tools were offered; attempt dropped"
        );
        n += 1;
        if n >= ATTEMPTS {
            let _ = tx
                .send(ThinEvent::Error(empty_completion(
                    &model,
                    cli.take().and_then(|f| f()).as_deref(),
                    shared.hold_all,
                )))
                .await;
            return (result, shared.started_sent, false);
        }
        if shared.hold_all {
            current = current.with_user_record(MUST_CALL);
        }
    }
}

/// `502 empty_completion` (SPEC §19.16): not drift, no retry. `hold_all` is the
/// `tool_choice: required` miss, which has its own message.
fn empty_completion(model: &str, cli: Option<&str>, hold_all: bool) -> ThinError {
    let message = if hold_all {
        "tool_choice required: no tool call after retry".to_string()
    } else {
        let who = match cli {
            Some(cli) => format!("{model} ({cli})"),
            None => model.to_string(),
        };
        format!("{who}: empty stop while tools were offered")
    };
    ThinError::empty_completion(crate::thin::claude_bridge::VENDOR, message)
}

/// Move one attempt's events to the route under the guard's rules.
async fn forward(
    mut rx: mpsc::Receiver<ThinEvent>,
    tx: &EventSender,
    shared: &mut Shared,
) -> Verdict {
    let mut held: Vec<ThinEvent> = Vec::new();
    let mut committed = false;
    let mut path: &'static str = "oauth";
    let mut model = String::new();
    let mut output_tokens = None;
    let mut empty = false;
    while let Some(event) = rx.recv().await {
        if empty {
            // The attempt is being dropped; drain whatever follows the finish.
            continue;
        }
        let out: Vec<ThinEvent> = match event {
            ThinEvent::Started {
                model: m,
                path: p,
                fallback,
                wire_params,
            } => {
                path = p;
                model = m.clone();
                if shared.started_sent {
                    Vec::new()
                } else {
                    shared.started_sent = true;
                    vec![ThinEvent::Started {
                        model: m,
                        path: p,
                        fallback,
                        wire_params,
                    }]
                }
            }
            ThinEvent::TextDelta(text) if !committed => {
                // Hold-all keeps even real text: a tool call flushes it, a stop drops it.
                if shared.hold_all || text.trim().is_empty() {
                    held.push(ThinEvent::TextDelta(text));
                    Vec::new()
                } else {
                    committed = true;
                    flush(&mut held, ThinEvent::TextDelta(text))
                }
            }
            ThinEvent::ReasoningDelta {
                text,
                signature,
                details,
            } if !committed => {
                let event = ThinEvent::ReasoningDelta {
                    text: text.clone(),
                    signature,
                    details,
                };
                if shared.hold_all || text.trim().is_empty() {
                    held.push(event);
                    Vec::new()
                } else {
                    committed = true;
                    flush(&mut held, event)
                }
            }
            ThinEvent::ToolCall { .. } if !committed => {
                committed = true;
                flush(&mut held, event)
            }
            ThinEvent::Usage(usage) if !committed => {
                output_tokens = Some(usage.completion_tokens);
                held.push(ThinEvent::Usage(usage));
                Vec::new()
            }
            ThinEvent::Finish(FinishReason::Stop) if !committed && shared.tools_offered => {
                empty = true;
                held.clear();
                Vec::new()
            }
            ThinEvent::Finish(_) | ThinEvent::Error(_) if !committed => {
                committed = true;
                flush(&mut held, event)
            }
            other => vec![other],
        };
        for event in out {
            if tx.send(event).await.is_err() {
                return Verdict::ClientGone;
            }
        }
    }
    if empty {
        return Verdict::EmptyToolStop {
            path,
            model,
            output_tokens,
        };
    }
    // Ended without a finish (the path reports that itself): pass on what was held.
    for event in held {
        if tx.send(event).await.is_err() {
            return Verdict::ClientGone;
        }
    }
    Verdict::Passed
}

fn flush(held: &mut Vec<ThinEvent>, next: ThinEvent) -> Vec<ThinEvent> {
    let mut out = std::mem::take(held);
    out.push(next);
    out
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::thin::openai::Usage;

    fn request(tools: bool) -> ThinRequest {
        let tools = if tools {
            r#","tools":[{"type":"function","function":{"name":"read_file","parameters":{"type":"object"}}}]"#
        } else {
            ""
        };
        let body = format!(
            r#"{{"model":"subs/claude/fable","messages":[{{"role":"user","content":"hi"}}]{tools}}}"#
        );
        ThinRequest {
            request: serde_json::from_str(&body).unwrap(),
            entry: crate::config::models::ModelEntry::bridge_probe("claude-fable-5-1"),
            effort: None,
            request_id: "req-1".into(),
            path: "oauth",
            fallback: None,
            cursor_cwd: None,
        }
    }

    fn started() -> ThinEvent {
        ThinEvent::Started {
            model: "claude-fable-5-1".into(),
            path: "oauth",
            fallback: None,
            wire_params: None,
        }
    }

    fn usage() -> ThinEvent {
        ThinEvent::Usage(Usage::from_parts(10, 3, 0))
    }

    /// Run the guard over scripted attempts; returns what the route saw and the attempts run.
    async fn run(tools: bool, scripts: Vec<Vec<ThinEvent>>) -> (Vec<ThinEvent>, u32) {
        let req = request(tools);
        let (tx, mut rx) = mpsc::channel(256);
        let mut runs = 0;
        run_guarded(
            &req,
            &tx,
            || Some("claude 2.1.283".into()),
            |n, _attempt_req, inner| {
                runs += 1;
                let script = scripts
                    .get(n as usize)
                    .cloned()
                    .expect("no script for this attempt");
                async move {
                    for event in script {
                        inner.send(event).await.unwrap();
                    }
                }
            },
        )
        .await;
        drop(tx);
        let mut seen = Vec::new();
        while let Some(event) = rx.recv().await {
            seen.push(event);
        }
        (seen, runs)
    }

    fn empty_stop() -> Vec<ThinEvent> {
        vec![started(), usage(), ThinEvent::Finish(FinishReason::Stop)]
    }

    fn kinds(events: &[ThinEvent]) -> Vec<&'static str> {
        events
            .iter()
            .map(|e| match e {
                ThinEvent::Started { .. } => "started",
                ThinEvent::TextDelta(_) => "text",
                ThinEvent::ReasoningDelta { .. } => "reasoning",
                ThinEvent::ToolCall { .. } => "tool_call",
                ThinEvent::Usage(_) => "usage",
                ThinEvent::RateLimitAdvisory { .. } => "advisory",
                ThinEvent::Finish(FinishReason::Stop) => "stop",
                ThinEvent::Finish(FinishReason::ToolCalls) => "tool_calls",
                ThinEvent::Finish(FinishReason::Length) => "length",
                ThinEvent::Finish(FinishReason::ContentFilter) => "content_filter",
                ThinEvent::Error(_) => "error",
            })
            .collect()
    }

    #[tokio::test]
    async fn empty_stop_then_tool_call_shows_only_the_retry() {
        let retry = vec![
            started(),
            ThinEvent::ToolCall {
                id: "call_1".into(),
                name: "read_file".into(),
                arguments: "{}".into(),
            },
            usage(),
            ThinEvent::Finish(FinishReason::ToolCalls),
        ];
        let (seen, runs) = run(true, vec![empty_stop(), retry]).await;
        assert_eq!(runs, 2);
        assert_eq!(
            kinds(&seen),
            ["started", "tool_call", "usage", "tool_calls"],
            "one Started, the dropped attempt's usage gone"
        );
    }

    #[tokio::test]
    async fn two_empty_stops_are_empty_completion() {
        let (seen, runs) = run(true, vec![empty_stop(), empty_stop()]).await;
        assert_eq!(runs, 2);
        assert_eq!(kinds(&seen), ["started", "error"]);
        let ThinEvent::Error(err) = &seen[1] else {
            unreachable!()
        };
        assert_eq!(err.status, 502);
        assert_eq!(err.kind, "server_error");
        assert_eq!(err.code, "empty_completion");
        assert!(err.no_retry());
        assert_eq!(
            err.message,
            "claude-fable-5-1 (claude 2.1.283): empty stop while tools were offered"
        );
        assert_eq!(err.hint(), Some("name only tools present on this request"));
    }

    #[tokio::test]
    async fn text_and_stop_is_a_normal_stop() {
        let script = vec![
            started(),
            ThinEvent::TextDelta("No.".into()),
            usage(),
            ThinEvent::Finish(FinishReason::Stop),
        ];
        let (seen, runs) = run(true, vec![script]).await;
        assert_eq!(runs, 1);
        assert_eq!(kinds(&seen), ["started", "text", "usage", "stop"]);
    }

    #[tokio::test]
    async fn whitespace_text_is_empty() {
        let script = vec![
            started(),
            ThinEvent::TextDelta(" \n ".into()),
            usage(),
            ThinEvent::Finish(FinishReason::Stop),
        ];
        let (seen, runs) = run(true, vec![script.clone(), script]).await;
        assert_eq!(runs, 2);
        assert_eq!(kinds(&seen), ["started", "error"]);
    }

    #[tokio::test]
    async fn reasoning_only_is_not_empty() {
        let script = vec![
            started(),
            ThinEvent::ReasoningDelta {
                text: "thinking".into(),
                signature: None,
                details: None,
            },
            usage(),
            ThinEvent::Finish(FinishReason::Stop),
        ];
        let (seen, runs) = run(true, vec![script]).await;
        assert_eq!(runs, 1);
        assert_eq!(kinds(&seen), ["started", "reasoning", "usage", "stop"]);
    }

    #[tokio::test]
    async fn max_tokens_is_length_not_retried() {
        let script = vec![started(), usage(), ThinEvent::Finish(FinishReason::Length)];
        let (seen, runs) = run(true, vec![script]).await;
        assert_eq!(runs, 1);
        assert_eq!(kinds(&seen), ["started", "usage", "length"]);
    }

    #[tokio::test]
    async fn no_tools_empty_stop_is_unchanged() {
        let (seen, runs) = run(false, vec![empty_stop()]).await;
        assert_eq!(runs, 1);
        assert_eq!(kinds(&seen), ["started", "usage", "stop"]);
    }

    fn request_choice(choice: &str) -> ThinRequest {
        let mut req = request(true);
        req.request.tool_choice = Some(serde_json::json!(choice));
        req
    }

    /// What the route saw, how many attempts ran, and the last attempt's message count.
    async fn run_choice(
        choice: &str,
        scripts: Vec<Vec<ThinEvent>>,
    ) -> (Vec<ThinEvent>, u32, usize) {
        let req = request_choice(choice);
        let (tx, mut rx) = mpsc::channel(256);
        let mut runs = 0;
        let mut last_messages = 0;
        run_guarded(
            &req,
            &tx,
            || Some("claude 2.1.283".into()),
            |n, attempt_req, inner| {
                runs += 1;
                last_messages = attempt_req.request.messages.len();
                let script = scripts
                    .get(n as usize)
                    .cloned()
                    .expect("no script for this attempt");
                async move {
                    for event in script {
                        inner.send(event).await.unwrap();
                    }
                }
            },
        )
        .await;
        drop(tx);
        let mut seen = Vec::new();
        while let Some(event) = rx.recv().await {
            seen.push(event);
        }
        (seen, runs, last_messages)
    }

    #[tokio::test]
    async fn required_holds_text_until_a_tool_call_flushes_it() {
        let script = vec![
            started(),
            ThinEvent::TextDelta("looking".into()),
            ThinEvent::ReasoningDelta {
                text: "where".into(),
                signature: None,
                details: None,
            },
            usage(),
            ThinEvent::ToolCall {
                id: "call_1".into(),
                name: "read_file".into(),
                arguments: "{}".into(),
            },
            ThinEvent::Finish(FinishReason::ToolCalls),
        ];
        let (seen, runs, messages) = run_choice("required", vec![script]).await;
        assert_eq!(runs, 1);
        assert_eq!(messages, 1, "a tool call is not a retry");
        assert_eq!(
            kinds(&seen),
            [
                "started",
                "text",
                "reasoning",
                "usage",
                "tool_call",
                "tool_calls"
            ],
            "held events flush in order on the tool call"
        );
    }

    #[tokio::test]
    async fn required_text_stop_retries_once_and_drops_the_text() {
        let text_stop = vec![
            started(),
            ThinEvent::TextDelta("No.".into()),
            usage(),
            ThinEvent::Finish(FinishReason::Stop),
        ];
        let retry = vec![
            started(),
            ThinEvent::ToolCall {
                id: "call_1".into(),
                name: "read_file".into(),
                arguments: "{}".into(),
            },
            usage(),
            ThinEvent::Finish(FinishReason::ToolCalls),
        ];
        let (seen, runs, messages) = run_choice("required", vec![text_stop, retry]).await;
        assert_eq!(runs, 2);
        assert_eq!(messages, 2, "the retry sees the appended user record");
        assert_eq!(
            kinds(&seen),
            ["started", "tool_call", "usage", "tool_calls"],
            "the first attempt's text is not streamed"
        );
    }

    #[tokio::test]
    async fn required_two_text_stops_are_empty_completion() {
        let text_stop = vec![
            started(),
            ThinEvent::TextDelta("No.".into()),
            usage(),
            ThinEvent::Finish(FinishReason::Stop),
        ];
        let (seen, runs, _) = run_choice("required", vec![text_stop.clone(), text_stop]).await;
        assert_eq!(runs, 2);
        assert_eq!(kinds(&seen), ["started", "error"]);
        let ThinEvent::Error(err) = &seen[1] else {
            unreachable!()
        };
        assert_eq!(err.status, 502);
        assert_eq!(err.kind, "server_error");
        assert_eq!(err.code, "empty_completion");
        assert!(err.no_retry());
        assert_eq!(
            err.message,
            "tool_choice required: no tool call after retry"
        );
    }

    #[tokio::test]
    async fn an_error_passes_through_without_retry() {
        let script = vec![
            started(),
            ThinEvent::Error(ThinError::vendor_down("claude", "idle")),
        ];
        let (seen, runs) = run(true, vec![script]).await;
        assert_eq!(runs, 1);
        assert_eq!(kinds(&seen), ["started", "error"]);
    }
}
