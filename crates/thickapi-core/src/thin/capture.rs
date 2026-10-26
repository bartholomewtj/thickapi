//! Wire tap for the thin adapters (SPEC §19.11). A [`FrameTap`] is optional on every adapter
//! (`None` in `serve`); `thickapi capture` sets one (`thickapi-cli/src/capture/`) that records
//! the outbound request, the response head and every body chunk as it arrived, redacted.
//!
//! This module keeps what the adapters and the replay tests share: the tap, its record types,
//! the scenario readers, and the id-free event summary a set's `events.jsonl` holds. Set
//! layout, the newest-set rule and the leak table are [`crate::thin::fixture_set`].

use std::path::Path;
use std::sync::Arc;

use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::thin::fixture_set::{EVENTS_FILE, FRAMES_FILE, REQUEST_FILE};
use crate::thin::openai::FinishReason;
use crate::thin::ThinEvent;

/// Receives one request/response exchange as the adapter drives it.
pub trait FrameTap: Send + Sync {
    fn request(&self, record: RequestRecord);
    fn response(&self, status: u16, headers: Vec<(String, String)>);
    fn frame(&self, chunk: &[u8]);
}

pub type Tap = Option<Arc<dyn FrameTap>>;

/// Capture-only adapter switches `thickapi capture` sets beside the tap (F1, #181). `serve`
/// builds its runtime with the default (every switch off), and no `router.yaml` key reaches
/// them.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CaptureSwitches {
    /// Cursor: answer a built-in read of a missing file on the duplex in this shape instead
    /// of `serve`'s `FileNotFound` (G1, #175). `None` keeps the default.
    pub cursor_read_miss_shape: Option<crate::thin::oauth::cursor::ReadMissShape>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct RequestRecord {
    pub method: String,
    pub url: String,
    pub headers: Vec<(String, String)>,
    /// JSON body when it was one (redacted); `None` for streaming or binary bodies.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<Value>,
    /// Binary body (Cursor's Connect frames) as base64.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body_b64: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ResponseRecord {
    pub status: u16,
    pub headers: Vec<(String, String)>,
}

/// One body chunk as delivered by the transport. Text when it was UTF-8, else base64.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FrameRecord {
    pub seq: usize,
    pub ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub b64: Option<String>,
}

impl FrameRecord {
    pub fn bytes(&self) -> Vec<u8> {
        if let Some(t) = &self.text {
            return t.as_bytes().to_vec();
        }
        self.b64
            .as_deref()
            .and_then(|b| base64::engine::general_purpose::STANDARD.decode(b).ok())
            .unwrap_or_default()
    }
}

fn header_pairs(headers: &reqwest::header::HeaderMap) -> Vec<(String, String)> {
    let pair = |(k, v): (&reqwest::header::HeaderName, &reqwest::header::HeaderValue)| {
        (k.to_string(), v.to_str().unwrap_or("<binary>").to_string())
    };
    headers.iter().map(pair).collect()
}

/// Record a built `reqwest::Request`. JSON bodies are stored as JSON; streaming bodies (Cursor)
/// pass their bytes through `body_b64` instead.
pub fn tap_request(tap: &Tap, request: &reqwest::Request, body_b64: Option<&[u8]>) {
    let Some(tap) = tap else { return };
    let body = request
        .body()
        .and_then(|b| b.as_bytes())
        .and_then(|bytes| serde_json::from_slice::<Value>(bytes).ok());
    tap.request(RequestRecord {
        method: request.method().to_string(),
        url: request.url().to_string(),
        headers: header_pairs(request.headers()),
        body,
        body_b64: body_b64.map(|b| base64::engine::general_purpose::STANDARD.encode(b)),
    });
}

pub fn tap_response(tap: &Tap, response: &reqwest::Response) {
    if let Some(tap) = tap {
        tap.response(response.status().as_u16(), header_pairs(response.headers()));
    }
}

pub fn tap_frame(tap: &Tap, chunk: &[u8]) {
    if let Some(tap) = tap {
        tap.frame(chunk);
    }
}

/// Raw body chunks of a scenario directory, in arrival order.
pub fn load_frames(scenario_dir: &Path) -> std::io::Result<Vec<Vec<u8>>> {
    let text = std::fs::read_to_string(scenario_dir.join(FRAMES_FILE))?;
    let frames = text.lines().filter_map(|l| serde_json::from_str(l).ok());
    Ok(frames.map(|f: FrameRecord| f.bytes()).collect())
}

/// The request a scenario recorded (`request.json`).
pub fn load_request(scenario_dir: &Path) -> std::io::Result<RequestRecord> {
    let text = std::fs::read_to_string(scenario_dir.join(REQUEST_FILE))?;
    serde_json::from_str(&text).map_err(std::io::Error::other)
}

impl RequestRecord {
    /// The binary body (Cursor's Connect frames), when one was recorded.
    pub fn body_bytes(&self) -> Option<Vec<u8>> {
        let b64 = self.body_b64.as_deref()?;
        base64::engine::general_purpose::STANDARD.decode(b64).ok()
    }
}

/// Event summaries recorded at capture time (`events.jsonl`).
pub fn load_events(scenario_dir: &Path) -> std::io::Result<Vec<Value>> {
    let text = std::fs::read_to_string(scenario_dir.join(EVENTS_FILE))?;
    Ok(text
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect())
}

/// Order-preserving, id-free view of an event stream: consecutive text/reasoning deltas are
/// coalesced, tool-call ids dropped (they are minted per request), errors reduced to their code.
pub fn summarize(events: &[ThinEvent]) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::new();
    let mut text = String::new();
    let mut reasoning = String::new();
    let flush = |out: &mut Vec<Value>, text: &mut String, reasoning: &mut String| {
        if !reasoning.is_empty() {
            out.push(json!({"type": "reasoning", "text": std::mem::take(reasoning)}));
        }
        if !text.is_empty() {
            out.push(json!({"type": "text", "text": std::mem::take(text)}));
        }
    };
    for ev in events {
        match ev {
            ThinEvent::TextDelta(t) => {
                if !reasoning.is_empty() {
                    out.push(json!({"type": "reasoning", "text": std::mem::take(&mut reasoning)}));
                }
                text.push_str(t);
            }
            ThinEvent::ReasoningDelta { text: t, .. } => {
                if !text.is_empty() {
                    out.push(json!({"type": "text", "text": std::mem::take(&mut text)}));
                }
                reasoning.push_str(t);
            }
            other => {
                flush(&mut out, &mut text, &mut reasoning);
                out.push(match other {
                    ThinEvent::Started { model, .. } => json!({"type": "started", "model": model}),
                    ThinEvent::ToolCall {
                        name, arguments, ..
                    } => {
                        let args: Value = serde_json::from_str(arguments)
                            .unwrap_or(Value::String(arguments.clone()));
                        json!({"type": "tool_call", "name": name, "arguments": args})
                    }
                    ThinEvent::Usage(u) => json!({
                        "type": "usage",
                        "prompt_tokens": u.prompt_tokens,
                        "completion_tokens": u.completion_tokens,
                    }),
                    ThinEvent::RateLimitAdvisory { .. } => json!({"type": "rate_limit"}),
                    ThinEvent::Finish(reason) => json!({
                        "type": "finish",
                        "reason": match reason {
                            FinishReason::Stop => "stop",
                            FinishReason::ToolCalls => "tool_calls",
                            FinishReason::Length => "length",
                            FinishReason::ContentFilter => "content_filter",
                        }
                    }),
                    ThinEvent::Error(e) => {
                        json!({"type": "error", "status": e.status, "code": e.code})
                    }
                    ThinEvent::TextDelta(_) | ThinEvent::ReasoningDelta { .. } => unreachable!(),
                });
            }
        }
    }
    flush(&mut out, &mut text, &mut reasoning);
    out
}
