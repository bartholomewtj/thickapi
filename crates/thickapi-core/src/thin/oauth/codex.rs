//! Codex OAuth-direct adapter (SPEC §19.8 `codex.rs`).
//!
//! The codex CLI's ChatGPT login (`~/.codex/auth.json`) is a bearer for the Responses API at
//! `https://chatgpt.com/backend-api/codex/responses`. This adapter turns the thin OpenAI chat
//! request into a Responses request and the Responses SSE stream back into thin events.
//! Measured live 2026-10-24 on `codex-cli 0.164.0`, ChatGPT Plus, `gpt-6.1-sol`, `gpt-6-astra`,
//! `gpt-6-luna`:
//! - Headers: `authorization`, `chatgpt-account-id` (the access token's
//!   `https://api.openai.com/auth.chatgpt_account_id` claim), `OpenAI-Beta:
//!   responses=experimental`, `originator: codex_cli_rs`, `session_id`.
//! - Body: `store: false`, `stream: true`, `instructions`, `input` items, `tools` as flat
//!   `{type: function, name, description, parameters}` (no `strict`: the client's schema goes
//!   as sent), `reasoning: {effort, summary: auto}`, `include: [reasoning.encrypted_content]`.
//!   `max_output_tokens` is accepted; `temperature`, `top_p` and `stop` are `400 Unsupported
//!   parameter`, so they are dropped.
//! - `reasoning.effort`: `low|medium|high|xhigh|max` on all three; `none` only on Luna
//!   ([`crate::thinking::codex_accepts_none`]); `minimal` and `ultra` are `400`. Absent is the
//!   server's `medium`.
//! - Stream: `response.output_text.delta`, `response.reasoning_summary_text.delta`, a
//!   `function_call` item whole in `response.output_item.done` (its `call_id` is the id the
//!   next turn's `function_call_output` must name), and the usage in `response.completed`.
//! - Reasoning items carry `encrypted_content`; replaying them is optional (a tool turn
//!   without them still answers), so the adapter keeps them per call id and replays them when
//!   the client sends the call back, like agy's thought signatures.
//! - Token life ten days; the CLI refreshes it. `codex login status` runs when less than a day
//!   is left; the daemon never writes `auth.json`.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};

use crate::config::router::ThinConfig;
use crate::thin::capture::Tap;
use crate::thin::openai::{ChatMessage, Content, ContentPart, FinishReason};
use crate::thin::{AdapterFuture, EventSender, ModelAdapter, ThinError, ThinEvent, ThinRequest};
use crate::thinking::{profile, Effort, VendorThinking};
use crate::types::BackendId;

use super::creds::{self, Token};
use super::sse::SseEvent;
use super::state::{redact, VendorState};
use super::vendor::{
    self, CliToken, MapperCore, Rejected, Sse, SseMapper, StreamBudgets, UsageKeys, VendorParts,
    VendorRequest, VendorSession, Wording,
};

pub const VENDOR: &str = "codex";
pub const DEFAULT_BASE_URL: &str = "https://chatgpt.com/backend-api";
/// What the CLI sends as `originator`; the backend expects one on every request.
const ORIGINATOR: &str = "codex_cli_rs";
const FALLBACK_CLI_VERSION: &str = "0.164.0";
/// Ask the CLI to refresh when less than this is left of the ten-day token.
const REFRESH_WINDOW: Duration = Duration::from_secs(24 * 3600);
const REFRESH_TIMEOUT: Duration = Duration::from_secs(60);
const REASONING_CACHE_CAP: usize = 2048;
const INSTRUCTIONS_FALLBACK: &str = "You are a helpful assistant.";
/// The smallest `max_output_tokens` the backend takes.
const MIN_OUTPUT_TOKENS: u64 = 16;

/// Reasoning items one response produced, keyed by each `call_id` it issued, so the next turn
/// can replay them before the calls (bounded FIFO).
#[derive(Default)]
pub struct ReasoningCache {
    map: HashMap<String, Vec<Value>>,
    order: VecDeque<String>,
}

impl ReasoningCache {
    pub fn insert(&mut self, call_id: String, items: Vec<Value>) {
        if self.map.insert(call_id.clone(), items).is_none() {
            self.order.push_back(call_id);
            while self.order.len() > REASONING_CACHE_CAP {
                if let Some(old) = self.order.pop_front() {
                    self.map.remove(&old);
                }
            }
        }
    }

    pub fn get(&self, call_id: &str) -> Option<&Vec<Value>> {
        self.map.get(call_id)
    }
}

pub struct CodexAdapter {
    base_url: String,
    pub cli: vendor::CliBacked,
    reasoning: Arc<Mutex<ReasoningCache>>,
    pub state: Arc<VendorState>,
    parts: VendorParts,
}

impl CodexAdapter {
    /// Record the wire exchange (`thickapi capture`); `serve` never sets one.
    pub fn set_tap(&mut self, tap: Tap) {
        self.parts.set_tap(tap);
    }

    pub fn new(thin: &ThinConfig, state: Arc<VendorState>) -> Self {
        let codex = &thin.vendors.codex;
        let credentials_path = codex.credentials_path.clone();
        let read_path = credentials_path.clone();
        Self {
            base_url: codex
                .base_url
                .clone()
                .unwrap_or_else(|| DEFAULT_BASE_URL.to_string()),
            cli: vendor::CliBacked::new(
                CliToken::new(VENDOR, "codex", "codex login")
                    .refresh_with(&["login", "status"], REFRESH_TIMEOUT)
                    .window(REFRESH_WINDOW)
                    .cli_owns_store(credentials_path.is_none()),
                move || match &read_path {
                    Some(p) => creds::codex_token_from(p),
                    None => creds::codex_token(),
                },
                vendor::CliVersion::new("codex"),
                codex.client_version.clone(),
                FALLBACK_CLI_VERSION,
            ),
            reasoning: Arc::new(Mutex::new(ReasoningCache::default())),
            state,
            parts: VendorParts::new(thin),
        }
    }

    /// SPEC §19.5 thin apply, Codex: `None` = omit `reasoning` (the server's `medium`).
    pub fn map_effort(
        effort: Option<Effort>,
        entry: &crate::config::models::ModelEntry,
    ) -> Option<&'static str> {
        VendorThinking::wire_param(&profile(BackendId::Codex).thin_apply(effort, entry))
    }

    fn url(&self) -> String {
        let base = self.base_url.trim_end_matches('/');
        if base.ends_with("/codex") {
            format!("{base}/responses")
        } else {
            format!("{base}/codex/responses")
        }
    }

    /// The outbound Responses body. Pure so tests can pin the shape.
    pub fn build_body(req: &ThinRequest, cache: &ReasoningCache) -> Value {
        let chat = &req.request;
        let mut instructions: Vec<String> = Vec::new();
        let mut input: Vec<Value> = Vec::new();
        for m in &chat.messages {
            match m.role.as_str() {
                "system" | "developer" => {
                    let text = m.content.as_ref().map(Content::text).unwrap_or_default();
                    if !text.is_empty() {
                        instructions.push(text);
                    }
                }
                "assistant" => assistant_items(m, cache, &mut input),
                "tool" => {
                    let call_id = m.tool_call_id.clone().unwrap_or_default();
                    input.push(json!({
                        "type": "function_call_output",
                        "call_id": call_id,
                        "output": tool_output(m.content.as_ref()),
                    }));
                }
                _ => {
                    let content = user_content(m.content.as_ref());
                    if !content.is_empty() {
                        input.push(json!({ "role": "user", "content": content }));
                    }
                }
            }
        }
        let instructions = if instructions.is_empty() {
            INSTRUCTIONS_FALLBACK.to_string()
        } else {
            instructions.join("\n\n")
        };
        let mut body = json!({
            "model": req.entry.wire_model(),
            "instructions": instructions,
            "input": input,
            "store": false,
            "stream": true,
            "include": ["reasoning.encrypted_content"],
            "parallel_tool_calls": chat.parallel_tool_calls.unwrap_or(true),
            "prompt_cache_key": prompt_cache_key(req.entry.wire_model(), chat),
        });
        if !chat.tools.is_empty() {
            let tools: Vec<Value> = chat
                .tools
                .iter()
                .map(|t| {
                    let mut d = json!({ "type": "function", "name": t.function.name });
                    if let Some(desc) = &t.function.description {
                        crate::thin::json_insert(&mut d, "description", json!(desc));
                    }
                    let params = t
                        .function
                        .parameters
                        .clone()
                        .unwrap_or_else(|| json!({ "type": "object", "properties": {} }));
                    crate::thin::json_insert(&mut d, "parameters", params);
                    d
                })
                .collect();
            crate::thin::json_insert(&mut body, "tools", json!(tools));
            crate::thin::json_insert(
                &mut body,
                "tool_choice",
                tool_choice(chat.tool_choice.as_ref()),
            );
        }
        if let Some(effort) = Self::map_effort(req.effort, &req.entry) {
            crate::thin::json_insert(
                &mut body,
                "reasoning",
                json!({ "effort": effort, "summary": "auto" }),
            );
        }
        if let Some(max) = chat.max_output_tokens() {
            // `400 integer below minimum value` under 16 (measured 2026-10-25).
            let max = max.max(MIN_OUTPUT_TOKENS);
            crate::thin::json_insert(&mut body, "max_output_tokens", json!(max));
        }
        body
    }

    /// The plan's model ids for `thickapi models check` (SPEC §10.3): `GET /codex/models`,
    /// the list the CLI caches in `~/.codex/models_cache.json`, without `visibility: hide` rows.
    pub async fn list_models(&self) -> Result<Vec<String>, ThinError> {
        let token = self.cli.fresh_token().await?;
        let version = self.cli.client_version();
        let url = format!(
            "{}/codex/models?client_version={version}",
            self.base_url.trim_end_matches('/')
        );
        let session = self.session();
        let mut rb = session
            .http()
            .get(&url)
            .header("accept", "application/json");
        for (k, v) in headers(&token, &version) {
            rb = rb.header(k, v);
        }
        let doc: Value = session
            .fetch_json(&format!("GET {url}"), rb, |status, text| {
                ThinError::vendor_down(VENDOR, format!("GET {url} {status}: {}", redact(text)))
            })
            .await?;
        let ids: Vec<String> = doc
            .get("models")
            .and_then(Value::as_array)
            .map(|rows| {
                rows.iter()
                    .filter(|m| m.get("visibility").and_then(Value::as_str) != Some("hide"))
                    .filter_map(|m| m.get("slug").and_then(Value::as_str).map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        if ids.is_empty() {
            return Err(ThinError::vendor_drift(
                VENDOR,
                format!("{url} answered without a models[] list"),
            ));
        }
        Ok(ids)
    }

    pub fn session(&self) -> VendorSession<'_> {
        self.parts.session(VENDOR, &self.cli, &self.state)
    }
}

/// `codex_cli_rs/<version> (<os>; <arch>)`, `originator` and the account header.
fn headers(token: &Token, version: &str) -> Vec<(&'static str, String)> {
    let mut out = vec![
        ("authorization", format!("Bearer {}", token.access_token)),
        ("originator", ORIGINATOR.to_string()),
        (
            "user-agent",
            format!(
                "{ORIGINATOR}/{version} ({}; {})",
                std::env::consts::OS,
                std::env::consts::ARCH
            ),
        ),
    ];
    if let Some(account) = creds::codex_account_id(&token.access_token) {
        out.push(("chatgpt-account-id", account));
    }
    out
}

/// One cache key per conversation: the model, the instructions and the first user record do
/// not change from turn to turn, so the backend can route a session's turns to its cache.
fn prompt_cache_key(model: &str, chat: &crate::thin::openai::ChatRequest) -> String {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    model.hash(&mut h);
    for m in &chat.messages {
        if matches!(m.role.as_str(), "system" | "developer") {
            m.content.as_ref().map(Content::text).hash(&mut h);
        }
    }
    if let Some(first) = chat.messages.iter().find(|m| m.role == "user") {
        first.content.as_ref().map(Content::text).hash(&mut h);
    }
    format!("thickapi-{:016x}", h.finish())
}

/// Chat `tool_choice` → Responses: the strings pass; `{type: function, function: {name}}`
/// becomes the flat `{type: function, name}`.
fn tool_choice(choice: Option<&Value>) -> Value {
    match choice {
        None => json!("auto"),
        Some(Value::String(s)) => json!(s),
        Some(v) => match v.pointer("/function/name").and_then(Value::as_str) {
            Some(name) => json!({ "type": "function", "name": name }),
            None => json!("auto"),
        },
    }
}

fn user_content(content: Option<&Content>) -> Vec<Value> {
    match content {
        None => Vec::new(),
        Some(Content::Text(t)) if t.is_empty() => Vec::new(),
        Some(Content::Text(t)) => vec![json!({ "type": "input_text", "text": t })],
        Some(Content::Parts(parts)) => parts
            .iter()
            .filter_map(|p| match p {
                ContentPart::Text { text } => Some(json!({ "type": "input_text", "text": text })),
                ContentPart::ImageUrl { image_url } => {
                    Some(json!({ "type": "input_image", "image_url": image_url.url }))
                }
                ContentPart::Unknown => None,
            })
            .collect(),
    }
}

/// A tool result: plain text, or the content list when it carries an image.
fn tool_output(content: Option<&Content>) -> Value {
    match content {
        Some(Content::Parts(parts))
            if parts
                .iter()
                .any(|p| matches!(p, ContentPart::ImageUrl { .. })) =>
        {
            json!(user_content(content))
        }
        Some(c) => json!(c.text()),
        None => json!(""),
    }
}

/// An assistant turn → its text as an `output_text` message, then its calls, each preceded
/// (once per response) by the reasoning items the call came with.
fn assistant_items(m: &ChatMessage, cache: &ReasoningCache, input: &mut Vec<Value>) {
    let text = m.content.as_ref().map(Content::text).unwrap_or_default();
    let calls = m.tool_calls.as_deref().unwrap_or_default();
    if let Some(first) = calls.first() {
        if let Some(items) = cache.get(&first.id) {
            input.extend(items.iter().cloned());
        }
    }
    if !text.is_empty() {
        input.push(json!({
            "type": "message",
            "role": "assistant",
            "content": [{ "type": "output_text", "text": text }],
        }));
    }
    for c in calls {
        input.push(json!({
            "type": "function_call",
            "call_id": c.id,
            "name": c.function.name,
            "arguments": c.function.arguments,
        }));
    }
}

const WORDING: Wording = Wording {
    label: "codex",
    host: "chatgpt.com",
    login_hint: "codex login",
    headers_sent: "authorization, chatgpt-account-id, originator, user-agent, OpenAI-Beta",
};

impl VendorRequest for CodexAdapter {
    type Wire = Sse<ResponsesMapper>;
    const WORDING: Wording = WORDING;

    async fn build(
        &self,
        session: &VendorSession<'_>,
        req: &ThinRequest,
        token: &Token,
    ) -> Result<reqwest::RequestBuilder, ThinError> {
        let version = session.cli().client_version();
        let body = {
            let cache = self
                .reasoning
                .lock()
                .map_err(|_| ThinError::server("codex reasoning cache poisoned"))?;
            Self::build_body(req, &cache)
        };
        let mut rb = session
            .http()
            .post(self.url())
            .header("OpenAI-Beta", "responses=experimental")
            .header("session_id", req.request_id.as_str())
            .header("accept", "text/event-stream")
            .header("content-type", "application/json");
        for (k, v) in headers(token, &version) {
            rb = rb.header(k, v);
        }
        Ok(rb.json(&body))
    }

    fn wire(&self, model: &str, budgets: StreamBudgets) -> Self::Wire {
        Sse::new(ResponsesMapper::new(model, self.reasoning.clone(), budgets))
    }

    fn wire_params(&self, req: &ThinRequest) -> Option<String> {
        Self::map_effort(req.effort, &req.entry).map(|e| format!("reasoning_effort={e}"))
    }

    fn budgets(&self, _req: &ThinRequest, cfg: &ThinConfig) -> StreamBudgets {
        let own = &cfg.vendors.codex.budgets;
        StreamBudgets::from_config(&crate::config::router::OauthBudgets {
            first_event_seconds: own.first_event_seconds,
            idle_seconds: own.idle_seconds,
        })
    }

    /// The usage-limit body names its own wait (`resets_in_seconds`) when no `Retry-After`.
    fn classify(&self, rejected: &Rejected<'_>) -> ThinError {
        let parsed: Option<Value> = serde_json::from_str(rejected.body).ok();
        let wait = vendor::retry_after(rejected.headers).or_else(|| {
            parsed
                .as_ref()
                .and_then(|v| v.pointer("/error/resets_in_seconds"))
                .and_then(Value::as_u64)
        });
        vendor::classify_status(
            VENDOR,
            &WORDING,
            rejected.status,
            &codex_error_message(rejected.body),
            wait,
            rejected.model,
            !rejected.token.expired(),
        )
    }
}

impl ModelAdapter for CodexAdapter {
    fn vendor(&self) -> &'static str {
        VENDOR
    }

    fn run<'a>(&'a self, req: &'a ThinRequest, tx: EventSender) -> AdapterFuture<'a> {
        Box::pin(async move { self.session().run(self, req, &tx).await })
    }
}

/// `{"detail": "..."}` (a rejected parameter) or the OpenAI `{"error": {message}}` envelope.
fn codex_error_message(text: &str) -> String {
    let parsed: Option<Value> = serde_json::from_str(text).ok();
    parsed
        .as_ref()
        .and_then(|v| v.get("detail").and_then(Value::as_str).map(str::to_string))
        .unwrap_or_else(|| vendor::error_message(text))
}

/// A mid-stream `error` event or a `response.failed` body → the shared status table.
fn stream_error(err: &Value, model: &str) -> ThinError {
    let code = err.get("code").and_then(Value::as_str).unwrap_or("");
    let kind = err.get("type").and_then(Value::as_str).unwrap_or("");
    let message = err
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or(code)
        .to_string();
    let blob = format!("{code} {kind} {message}").to_ascii_lowercase();
    let status = if blob.contains("usage_limit") || blob.contains("rate_limit") {
        429
    } else if blob.contains("insufficient_quota") {
        402
    } else if blob.contains("context_length")
        || blob.contains("invalid_request")
        || blob.contains("invalid_prompt")
    {
        400
    } else if blob.contains("auth") {
        401
    } else {
        503
    };
    let wait = err.get("resets_in_seconds").and_then(Value::as_u64);
    vendor::classify_status(VENDOR, &WORDING, status, &message, wait, model, true)
}

/// Responses SSE → thin events. Text and reasoning summaries stream live; a function call is
/// emitted when its item is done (`call_id` is the client-facing id); `response.completed`
/// carries the usage and ends the turn.
pub struct ResponsesMapper {
    pub core: MapperCore,
    cache: Arc<Mutex<ReasoningCache>>,
    /// Reasoning items of this response, cached under each of its call ids at the end.
    reasoning: Vec<Value>,
    call_ids: Vec<String>,
    /// A summary part already streamed: the next one starts on a blank line.
    summary_seen: bool,
    completed: bool,
}

impl ResponsesMapper {
    pub fn new(model: &str, cache: Arc<Mutex<ReasoningCache>>, budgets: StreamBudgets) -> Self {
        Self {
            core: MapperCore::new(VENDOR, model, budgets),
            cache,
            reasoning: Vec::new(),
            call_ids: Vec::new(),
            summary_seen: false,
            completed: false,
        }
    }

    pub fn accept(&mut self, data: &str) {
        let data = data.trim();
        if data.is_empty() || self.core.done || data == "[DONE]" {
            return;
        }
        let event: Value = match serde_json::from_str(data) {
            Ok(v) => v,
            Err(e) => {
                self.core.fail(ThinError::vendor_drift(
                    VENDOR,
                    format!("unparseable stream event: {e}: {}", redact(data)),
                ));
                return;
            }
        };
        let kind = event.get("type").and_then(Value::as_str).unwrap_or("");
        match kind {
            "error" => {
                let err = event.get("error").unwrap_or(&event).clone();
                self.core.fail(stream_error(&err, self.core.model()));
            }
            "response.failed" => {
                let err = event
                    .pointer("/response/error")
                    .cloned()
                    .unwrap_or_else(|| json!({ "message": "response.failed" }));
                self.core.fail(stream_error(&err, self.core.model()));
            }
            "response.output_text.delta" => {
                if let Some(text) = event.get("delta").and_then(Value::as_str) {
                    if !text.is_empty() {
                        self.core.emit(ThinEvent::TextDelta(text.to_string()));
                    }
                }
            }
            "response.reasoning_summary_part.added" => {
                if self.summary_seen {
                    self.core.emit(ThinEvent::ReasoningDelta {
                        text: "\n\n".into(),
                        signature: None,
                        details: None,
                    });
                }
            }
            "response.reasoning_summary_text.delta" => {
                if let Some(text) = event.get("delta").and_then(Value::as_str) {
                    if !text.is_empty() {
                        self.summary_seen = true;
                        self.core.emit(ThinEvent::ReasoningDelta {
                            text: text.to_string(),
                            signature: None,
                            details: None,
                        });
                    }
                }
            }
            "response.output_item.done" => self.item_done(event.get("item")),
            "response.completed" | "response.incomplete" => {
                self.complete(event.get("response"));
            }
            _ => self.core.ensure_started(),
        }
    }

    fn item_done(&mut self, item: Option<&Value>) {
        let Some(item) = item else { return };
        match item.get("type").and_then(Value::as_str) {
            Some("function_call") => {
                let id = item
                    .get("call_id")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let name = item
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                if name.is_empty() {
                    return;
                }
                let arguments = match item.get("arguments").and_then(Value::as_str) {
                    Some(a) if !a.trim().is_empty() => a.to_string(),
                    _ => "{}".to_string(),
                };
                self.call_ids.push(id.clone());
                self.core.emit(ThinEvent::ToolCall {
                    id,
                    name,
                    arguments,
                });
            }
            Some("reasoning") => {
                // Only what a replay needs; `status` and the like are output-only.
                let mut keep = json!({ "type": "reasoning" });
                for key in ["id", "summary", "encrypted_content"] {
                    if let Some(v) = item.get(key) {
                        crate::thin::json_insert(&mut keep, key, v.clone());
                    }
                }
                self.reasoning.push(keep);
            }
            _ => self.core.ensure_started(),
        }
    }

    fn complete(&mut self, response: Option<&Value>) {
        self.core.ensure_started();
        self.completed = true;
        if !self.reasoning.is_empty() && !self.call_ids.is_empty() {
            if let Ok(mut cache) = self.cache.lock() {
                for id in &self.call_ids {
                    cache.insert(id.clone(), self.reasoning.clone());
                }
            }
        }
        if let Some(u) = response
            .and_then(|r| r.get("usage"))
            .filter(|u| !u.is_null())
        {
            self.core
                .emit(ThinEvent::Usage(vendor::usage_from(u, &USAGE_KEYS)));
        }
        let incomplete = response
            .and_then(|r| r.pointer("/incomplete_details/reason"))
            .and_then(Value::as_str);
        let reason = match incomplete {
            Some("max_output_tokens") => FinishReason::Length,
            Some("content_filter") => FinishReason::ContentFilter,
            _ if !self.call_ids.is_empty() => FinishReason::ToolCalls,
            _ => FinishReason::Stop,
        };
        self.core.finish(ThinEvent::Finish(reason));
    }
}

impl SseMapper for ResponsesMapper {
    fn core(&mut self) -> &mut MapperCore {
        &mut self.core
    }

    fn on_event(&mut self, event: &SseEvent) {
        self.accept(&event.data);
    }

    /// No `response.completed`: the answer may be cut; the shared rule fails it as truncated.
    fn on_end(&mut self) {}
}

const USAGE_KEYS: UsageKeys = UsageKeys {
    prompt: "input_tokens",
    completion: &["output_tokens"],
    total: "total_tokens",
    cached: "/input_tokens_details/cached_tokens",
};

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::config::models::{default_input, ModelEntry, Transport};
    use crate::thin::openai::ChatRequest;

    fn entry(wire: &str) -> ModelEntry {
        ModelEntry {
            id: "subs/codex/gpt-sol".into(),
            backend: BackendId::Codex,
            cli_model: wire.into(),
            context_window: 500_000,
            max_tokens: 128_000,
            thinking: true,
            cost: 0.0,
            name: "GPT Sol".into(),
            provider: None,
            transport: Transport::OauthDirect,
            vendor_model: None,
            enabled: true,
            input: default_input(),
        }
    }

    fn req(body: Value, effort: Option<&str>, wire: &str) -> ThinRequest {
        let request: ChatRequest = serde_json::from_value(body).unwrap();
        ThinRequest {
            request,
            entry: entry(wire),
            effort: crate::thinking::normalise_effort(effort).unwrap(),
            request_id: "chatcmpl-test".into(),
            path: "oauth",
            fallback: None,
            cursor_cwd: None,
        }
    }

    fn events(m: &mut ResponsesMapper) -> Vec<ThinEvent> {
        m.core.pending.drain(..).collect()
    }

    #[test]
    fn effort_ladder_matches_the_measured_column() {
        let sol = entry("gpt-6.1-sol");
        let luna = entry("gpt-6-luna");
        let e = |x: Option<Effort>, m: &ModelEntry| CodexAdapter::map_effort(x, m);
        assert_eq!(e(None, &sol), None);
        assert_eq!(e(Some(Effort::Off), &sol), Some("low"));
        assert_eq!(e(Some(Effort::Off), &luna), Some("none"));
        assert_eq!(e(Some(Effort::Minimal), &luna), Some("low"));
        assert_eq!(e(Some(Effort::Medium), &sol), Some("medium"));
        assert_eq!(e(Some(Effort::XHigh), &sol), Some("xhigh"));
        assert_eq!(e(Some(Effort::Max), &sol), Some("max"));
    }

    #[test]
    fn body_translates_messages_tools_choice_and_effort() {
        let body = json!({
            "model": "subs/codex/gpt-sol",
            "messages": [
                {"role": "system", "content": "Be brief."},
                {"role": "user", "content": [
                    {"type": "text", "text": "Read a.txt"},
                    {"type": "image_url", "image_url": {"url": "data:image/png;base64,AA"}}
                ]},
                {"role": "assistant", "content": "Reading.", "tool_calls": [
                    {"id": "call_1", "type": "function", "function": {"name": "read", "arguments": "{\"path\":\"a.txt\"}"}}
                ]},
                {"role": "tool", "tool_call_id": "call_1", "content": "hello"}
            ],
            "tools": [{"type": "function", "function": {"name": "read", "description": "Read",
                "parameters": {"type": "object", "properties": {"n": {"anyOf": [{"type": "string"}, {"type": "number"}]}}}}}],
            "tool_choice": {"type": "function", "function": {"name": "read"}},
            "temperature": 0.2,
            "max_tokens": 100
        });
        let mut cache = ReasoningCache::default();
        cache.insert(
            "call_1".into(),
            vec![
                json!({"type": "reasoning", "id": "rs_1", "summary": [], "encrypted_content": "E"}),
            ],
        );
        let out = CodexAdapter::build_body(&req(body, Some("high"), "gpt-6.1-sol"), &cache);
        assert_eq!(out["model"], "gpt-6.1-sol");
        assert_eq!(out["instructions"], "Be brief.");
        assert_eq!(out["store"], false);
        assert_eq!(
            out["reasoning"],
            json!({"effort": "high", "summary": "auto"})
        );
        assert_eq!(out["max_output_tokens"], 100);
        assert!(out.get("temperature").is_none());
        let input = out["input"].as_array().unwrap();
        assert_eq!(input[0]["role"], "user");
        assert_eq!(input[0]["content"][1]["type"], "input_image");
        assert_eq!(input[1]["type"], "reasoning");
        assert_eq!(input[1]["encrypted_content"], "E");
        assert_eq!(input[2]["content"][0]["type"], "output_text");
        assert_eq!(
            input[3],
            json!({"type": "function_call", "call_id": "call_1", "name": "read", "arguments": "{\"path\":\"a.txt\"}"})
        );
        assert_eq!(
            input[4],
            json!({"type": "function_call_output", "call_id": "call_1", "output": "hello"})
        );
        assert_eq!(out["tools"][0]["name"], "read");
        assert!(out["tools"][0].get("strict").is_none());
        assert_eq!(
            out["tools"][0]["parameters"]["properties"]["n"]["anyOf"][1]["type"],
            "number"
        );
        assert_eq!(
            out["tool_choice"],
            json!({"type": "function", "name": "read"})
        );
    }

    #[test]
    fn absent_effort_sends_no_reasoning_block() {
        let body = json!({"model": "m", "messages": [{"role": "user", "content": "hi"}]});
        let out =
            CodexAdapter::build_body(&req(body, None, "gpt-6-astra"), &ReasoningCache::default());
        assert!(out.get("reasoning").is_none());
        assert_eq!(out["instructions"], INSTRUCTIONS_FALLBACK);
        assert!(out.get("tools").is_none());
        let tiny =
            json!({"model": "m", "max_tokens": 8, "messages": [{"role": "user", "content": "hi"}]});
        let out =
            CodexAdapter::build_body(&req(tiny, None, "gpt-6-astra"), &ReasoningCache::default());
        assert_eq!(out["max_output_tokens"], 16);
    }

    #[test]
    fn mapper_streams_summary_text_call_usage_and_caches_reasoning() {
        let cache = Arc::new(Mutex::new(ReasoningCache::default()));
        let mut m = ResponsesMapper::new("gpt-6.1-sol", cache.clone(), StreamBudgets::OFFLINE);
        m.accept(r#"{"type":"response.created","response":{"id":"resp_1"}}"#);
        m.accept(r#"{"type":"response.reasoning_summary_part.added","summary_index":0}"#);
        m.accept(r#"{"type":"response.reasoning_summary_text.delta","delta":"Plan"}"#);
        m.accept(r#"{"type":"response.reasoning_summary_part.added","summary_index":1}"#);
        m.accept(r#"{"type":"response.reasoning_summary_text.delta","delta":"More"}"#);
        m.accept(r#"{"type":"response.output_item.done","item":{"type":"reasoning","id":"rs_1","status":"completed","summary":[],"encrypted_content":"ENC"}}"#);
        m.accept(r#"{"type":"response.output_text.delta","delta":"Reading."}"#);
        m.accept(r#"{"type":"response.output_item.done","item":{"type":"function_call","id":"fc_1","call_id":"call_9","name":"read","arguments":"{\"path\":\"a\"}","status":"completed"}}"#);
        m.accept(r#"{"type":"response.completed","response":{"status":"completed","usage":{"input_tokens":19,"input_tokens_details":{"cached_tokens":4},"output_tokens":6,"output_tokens_details":{"reasoning_tokens":2},"total_tokens":25}}}"#);
        let ev = events(&mut m);
        assert!(matches!(&ev[0], ThinEvent::Started { .. }));
        let reasoning: String = ev
            .iter()
            .filter_map(|e| match e {
                ThinEvent::ReasoningDelta { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(reasoning, "Plan\n\nMore");
        assert!(ev
            .iter()
            .any(|e| matches!(e, ThinEvent::TextDelta(t) if t == "Reading.")));
        assert!(ev.iter().any(|e| matches!(e, ThinEvent::ToolCall { id, name, .. } if id == "call_9" && name == "read")));
        let usage = ev.iter().find_map(|e| match e {
            ThinEvent::Usage(u) => Some(u.clone()),
            _ => None,
        });
        let usage = usage.unwrap();
        assert_eq!(
            (
                usage.prompt_tokens,
                usage.completion_tokens,
                usage.total_tokens
            ),
            (19, 6, 25)
        );
        assert_eq!(usage.prompt_tokens_details.unwrap().cached_tokens, 4);
        assert!(matches!(
            ev.last(),
            Some(ThinEvent::Finish(FinishReason::ToolCalls))
        ));
        let cached = cache.lock().unwrap().get("call_9").cloned().unwrap();
        assert_eq!(
            cached,
            vec![
                json!({"type": "reasoning", "id": "rs_1", "summary": [], "encrypted_content": "ENC"})
            ]
        );
    }

    #[test]
    fn incomplete_is_length_and_stream_errors_are_classified() {
        let cache = Arc::new(Mutex::new(ReasoningCache::default()));
        let mut m = ResponsesMapper::new("gpt-6.1-sol", cache.clone(), StreamBudgets::OFFLINE);
        m.accept(r#"{"type":"response.output_text.delta","delta":"half"}"#);
        m.accept(r#"{"type":"response.incomplete","response":{"status":"incomplete","incomplete_details":{"reason":"max_output_tokens"}}}"#);
        assert!(matches!(
            events(&mut m).last(),
            Some(ThinEvent::Finish(FinishReason::Length))
        ));

        let mut m = ResponsesMapper::new("gpt-6.1-sol", cache.clone(), StreamBudgets::OFFLINE);
        m.accept(r#"{"type":"response.failed","response":{"status":"failed","error":{"code":"usage_limit_reached","message":"The usage limit has been reached","resets_in_seconds":120}}}"#);
        // The plan's usage limit is a quota (the shared table's `usage limit` rule, as for
        // Cursor), with the reset the body names.
        match events(&mut m).last() {
            Some(ThinEvent::Error(e)) => {
                assert_eq!((e.status, e.code), (402, "insufficient_quota"), "{e:?}");
                assert_eq!(e.retry_after_seconds(), Some(120));
            }
            other => panic!("{other:?}"),
        }

        let mut m = ResponsesMapper::new("gpt-6.1-sol", cache, StreamBudgets::OFFLINE);
        m.accept(r#"{"type":"error","code":"context_length_exceeded","message":"Your input exceeds the context window"}"#);
        match events(&mut m).last() {
            Some(ThinEvent::Error(e)) => assert_eq!(e.status, 400, "{e:?}"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn rejected_detail_body_reads_its_message() {
        assert_eq!(
            codex_error_message(r#"{"detail":"Unsupported parameter: temperature"}"#),
            "Unsupported parameter: temperature"
        );
        assert_eq!(
            codex_error_message(r#"{"error":{"message":"bad","code":"model_not_found"}}"#),
            "bad"
        );
    }
}
