//! Thin API — OpenAI-compatible model surface (SPEC §19).
//!
//! The route in `thickapi-api` parses an OpenAI chat request, resolves the catalogue row and
//! its transport, and hands a [`ThinRequest`] to a [`ModelAdapter`]. Adapters stream
//! [`ThinEvent`]s; the route turns them into `chat.completion.chunk`s or one `chat.completion`.

pub mod anthropic_blocks;
pub mod capture;
pub mod claude_bridge;
pub mod claude_tools;
pub mod dialect;
pub mod empty_stop;
pub mod error;
pub mod fixture_set;
pub mod mcp_proposal;
pub mod model_check;
pub mod oauth;
pub mod openai;
pub mod private_dir;
pub mod runtime;

pub use error::ThinError;

use serde_json::Value;
use std::future::Future;
use std::pin::Pin;

/// Compile a regex written in the source; a bad pattern is a programming error.
pub(crate) fn static_regex(pattern: &'static str) -> regex::Regex {
    match regex::Regex::new(pattern) {
        Ok(re) => re,
        Err(err) => panic!("invalid static regex {pattern:?}: {err}"),
    }
}

/// Insert `key` on a JSON object. No-op if `obj` is not an object (avoids `Index` panic).
pub(crate) fn json_insert(obj: &mut Value, key: &str, val: Value) {
    if let Some(map) = obj.as_object_mut() {
        map.insert(key.to_string(), val);
    }
}

use tokio::sync::mpsc;

use crate::config::models::ModelEntry;
use crate::types::Effort;
use openai::{ChatRequest, FinishReason, Usage};

/// One resolved thin request: the parsed body plus the catalogue row it maps to.
#[derive(Debug, Clone)]
pub struct ThinRequest {
    pub request: ChatRequest,
    pub entry: ModelEntry,
    /// Pi `/thinking` after §19.5 normalisation (`None` = Absent = client did not ask).
    pub effort: Option<Effort>,
    pub request_id: String,
    /// Serving path for `Started` / `x-thickapi-path` (SPEC §19.15 R4). Default `oauth`.
    pub path: &'static str,
    pub fallback: Option<&'static str>,
    /// This request's Cursor cwd (`x-thickapi-cwd`). `None` keeps the adapter's configured cwd.
    /// Never written back onto the adapter (SPEC §19.8, #205).
    pub cursor_cwd: Option<String>,
}

impl ThinRequest {
    /// Clone with the path the inner adapter should emit on `Started`.
    pub fn with_serving(&self, path: &'static str, fallback: Option<&'static str>) -> Self {
        let mut r = self.clone();
        r.path = path;
        r.fallback = fallback;
        r
    }

    /// The same request with one more user record. The §19.16 `required` retry uses it.
    pub fn with_user_record(&self, text: &str) -> Self {
        let mut r = self.clone();
        r.request.messages.push(openai::ChatMessage {
            role: "user".into(),
            content: Some(openai::Content::Text(text.to_string())),
            name: None,
            tool_calls: None,
            tool_call_id: None,
            reasoning_content: None,
            reasoning_signature: None,
            reasoning_details: None,
            tool_is_error: false,
        });
        r
    }
}

/// Adapter → route events. Text and reasoning stream live; tool calls arrive complete.
#[derive(Debug, Clone)]
pub enum ThinEvent {
    /// Vendor accepted the request; the route may now commit to a 200 and start streaming.
    Started {
        model: String,
        /// `oauth` | `bridge` (SPEC §19.15 R4).
        path: &'static str,
        /// `oauth->bridge` | `bridge->oauth` when this response is a fallback replay.
        fallback: Option<&'static str>,
        /// The vendor parameters actually sent, `k=v` comma-joined in send order, for the access
        /// line's `wire_params` (SPEC §19.15 R5); `None` when the adapter sent none or does not
        /// report them.
        wire_params: Option<String>,
    },
    TextDelta(String),
    ReasoningDelta {
        text: String,
        signature: Option<String>,
        details: Option<Vec<crate::thin::openai::ReasoningDetail>>,
    },
    ToolCall {
        id: String,
        name: String,
        arguments: String,
    },
    Usage(Usage),
    /// Advisory utilisation from Claude's `rate_limit_event` (SPEC §19.4).
    RateLimitAdvisory {
        utilization: f64,
        resets_at: Option<String>,
    },
    Finish(FinishReason),
    Error(ThinError),
}

pub type EventSender = mpsc::Sender<ThinEvent>;
pub type AdapterFuture<'a> = Pin<Box<dyn Future<Output = Result<(), ThinError>> + Send + 'a>>;

/// One transport for one vendor. `run` streams events until `Finish` or `Error`; the returned
/// error (if any) is only used when nothing has been sent yet — after `Started` the adapter
/// must report failures as `ThinEvent::Error`.
pub trait ModelAdapter: Send + Sync {
    fn vendor(&self) -> &'static str;
    fn run<'a>(&'a self, req: &'a ThinRequest, tx: EventSender) -> AdapterFuture<'a>;
}
