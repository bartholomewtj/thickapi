//! Grok OAuth-direct adapter (SPEC §19.8, T2).
//!
//! xAI's `POST /v1/chat/completions` is already OpenAI-shaped and accepts the grok CLI's OIDC
//! bearer (`~/.grok/auth.json`, scope `api:access grok-cli:access`), so this adapter is a
//! near pass-through: forward the request with the CLI's client headers, map the SSE chunks
//! to [`ThinEvent`]s. Measured live 2026-09-17 on `grok 1.0.30`, rechecked 2026-09-22 on
//! `grok 1.0.40` with `model=grok-4.7` at `api.x.ai`:
//! - `reasoning_effort` accepted: `minimal | low | medium | high | xhigh`; `none` and `max` → 400.
//! - `reasoning_content` streams as deltas; tool calls arrive whole in one chunk with
//!   `id: call-<uuid>-<n>`; usage arrives in a final `choices: []` chunk when
//!   `stream_options.include_usage` is set.
//! - Errors: `{"code":"invalid-argument","error":"..."}` (xAI shape) or the OpenAI envelope.
//! - Rate limit: `x-ratelimit-{limit,remaining}-{tokens,requests}` response headers.
//! - Token life ~6 h. `grok models` (≈300 ms, exit 0) leaves a valid token alone; `fresh_token`
//!   runs it when < 5 min remain so the CLI refreshes under its own lock. Confirmed at the
//!   first live expiry, not before.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};

use crate::config::router::{OauthBudgets, ThinConfig};
use crate::thin::capture::Tap;
use crate::thin::openai::{Content, ContentPart, FinishReason, Usage};
use crate::thin::{AdapterFuture, EventSender, ModelAdapter, ThinError, ThinEvent, ThinRequest};
use crate::thinking::{profile, Effort, VendorThinking};
use crate::types::BackendId;

use super::creds::{self, Token};
use super::sse::SseEvent;
use super::state::{redact, VendorState};
use super::vendor::{
    self, AdvisoryHeaders, CliToken, MapperCore, Sse, SseMapper, StreamBudgets, UsageKeys,
    VendorParts, VendorRequest, VendorSession, Wording,
};

pub const VENDOR: &str = "grok";
pub const DEFAULT_BASE_URL: &str = "https://api.x.ai";
/// The grok CLI's own catalogue (`~/.grok/models_cache.json` `origin`): the models the
/// subscription offers, not the paid API list `api.x.ai/v1/models` returns.
pub const SUBSCRIPTION_MODELS_URL: &str = "https://cli-chat-proxy.grok.com/v1/models";
/// What the CLI sends as `x-grok-client-identifier` (its `GROK_CLIENT_NAME` default).
const CLIENT_IDENTIFIER: &str = "grok-cli";
const FALLBACK_CLI_VERSION: &str = "1.0.30";
/// Let the CLI refresh when this little of the token is left (it rotates the refresh token).
const REFRESH_WINDOW: Duration = Duration::from_secs(300);
/// `grok models` answers in ≈300 ms; this is for a CLI that hangs.
const REFRESH_TIMEOUT: Duration = Duration::from_secs(20);
/// Tool calls are slotted by the vendor's `index`; one this high is drift, not a call.
const MAX_TOOL_CALL_INDEX: usize = 256;

pub struct GrokAdapter {
    base_url: String,
    pub cli: vendor::CliBacked,
    pub state: Arc<VendorState>,
    /// Client, permits, tap and the `thin:` block (budgets, `reply_timeout_seconds`, #125).
    parts: VendorParts,
}

impl GrokAdapter {
    /// Record the wire exchange (`thickapi capture`); `serve` never sets one.
    pub fn set_tap(&mut self, tap: Tap) {
        self.parts.set_tap(tap);
    }

    /// Only `router.yaml`'s `thin:` block (`thin.vendors.grok` for this vendor's own keys) is
    /// read; fixture replay passes the defaults.
    pub fn new(thin: &ThinConfig, state: Arc<VendorState>) -> Self {
        let grok = &thin.vendors.grok;
        let credentials_path = grok.credentials_path.clone();
        let read_path = credentials_path.clone();
        Self {
            base_url: grok
                .base_url
                .clone()
                .unwrap_or_else(|| DEFAULT_BASE_URL.to_string()),
            // `grok models` leaves a valid token alone and refreshes a stale one under the
            // CLI's own `auth.json.lock`; the daemon never touches the refresh chain.
            cli: vendor::CliBacked::new(
                CliToken::new(VENDOR, "grok", "grok login")
                    .refresh_with(&["models"], REFRESH_TIMEOUT)
                    .window(REFRESH_WINDOW)
                    .cli_owns_store(credentials_path.is_none()),
                move || match &read_path {
                    Some(p) => creds::grok_token_from(p),
                    None => creds::grok_token(),
                },
                vendor::CliVersion::new("grok"),
                grok.client_version.clone(),
                FALLBACK_CLI_VERSION,
            ),
            state,
            parts: VendorParts::new(thin),
        }
    }

    /// SPEC §19.5 thin apply, Grok. `None` = omit the field. Off/minimal → `minimal` (never `none`).
    pub fn map_effort(effort: Option<Effort>) -> Option<&'static str> {
        let probe = crate::config::models::ModelEntry {
            id: "subs/grok/probe".into(),
            backend: BackendId::Grok,
            cli_model: "grok-4.7".into(),
            context_window: 0,
            max_tokens: 0,
            thinking: true,
            cost: 0.0,
            name: String::new(),
            provider: None,
            transport: crate::config::models::Transport::OauthDirect,
            vendor_model: None,
            enabled: true,
            input: crate::config::models::default_input(),
        };
        VendorThinking::config_level(&profile(BackendId::Grok).thin_apply(effort, &probe))
    }

    /// The outbound xAI body. Pure so tests can pin the shape.
    pub fn build_body(req: &ThinRequest) -> Value {
        let chat = &req.request;
        let mut body = json!({
            "model": req.entry.wire_model(),
            "messages": chat.messages.iter().map(message_to_value).collect::<Vec<_>>(),
            "stream": true,
            "stream_options": { "include_usage": true },
        });
        if !chat.tools.is_empty() {
            crate::thin::json_insert(&mut body, "tools", json!(chat.tools));
            if let Some(choice) = &chat.tool_choice {
                crate::thin::json_insert(&mut body, "tool_choice", choice.clone());
            }
            if let Some(parallel) = chat.parallel_tool_calls {
                crate::thin::json_insert(&mut body, "parallel_tool_calls", json!(parallel));
            }
        }
        if let Some(effort) = Self::map_effort(req.effort) {
            crate::thin::json_insert(&mut body, "reasoning_effort", json!(effort));
        }
        if let Some(max) = chat.max_output_tokens() {
            crate::thin::json_insert(&mut body, "max_tokens", json!(max));
        }
        if let Some(t) = chat.temperature {
            crate::thin::json_insert(&mut body, "temperature", json!(t));
        }
        if let Some(p) = chat.top_p {
            crate::thin::json_insert(&mut body, "top_p", json!(p));
        }
        if let Some(stop) = &chat.stop {
            crate::thin::json_insert(&mut body, "stop", stop.clone());
        }
        body
    }

    /// The subscription's model ids for `thickapi models check` (SPEC §10.3). Reads the CLI
    /// proxy's `/v1/models` with the stored token; a `grok_base_url` override (tests) is used
    /// instead so the stand-in answers.
    pub async fn list_models(&self) -> Result<Vec<String>, ThinError> {
        let token = self.cli.fresh_token().await?;
        let url = if self.base_url == DEFAULT_BASE_URL {
            SUBSCRIPTION_MODELS_URL.to_string()
        } else {
            format!("{}/v1/models", self.base_url.trim_end_matches('/'))
        };
        let session = self.session();
        let rb = session
            .http()
            .get(&url)
            .header("authorization", format!("Bearer {}", token.access_token))
            .header("x-grok-client-identifier", CLIENT_IDENTIFIER)
            .header("x-grok-client-version", self.cli.client_version())
            .header(
                "user-agent",
                format!("grok-cli/{}", self.cli.client_version()),
            )
            .header("accept", "application/json");
        let doc: Value = session
            .fetch_json(&format!("GET {url}"), rb, |status, text| {
                ThinError::vendor_down(VENDOR, format!("GET {url} {status}: {}", redact(text)))
            })
            .await?;
        let ids: Vec<String> = doc
            .get("data")
            .and_then(Value::as_array)
            .map(|rows| {
                rows.iter()
                    .filter_map(|m| m.get("id").and_then(Value::as_str).map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        if ids.is_empty() {
            return Err(ThinError::vendor_drift(
                VENDOR,
                format!("{url} answered without a data[] list"),
            ));
        }
        Ok(ids)
    }

    pub fn session(&self) -> VendorSession<'_> {
        self.parts.session(VENDOR, &self.cli, &self.state)
    }
}

const WORDING: Wording = Wording {
    label: "grok",
    host: "api.x.ai",
    login_hint: "grok login",
    headers_sent: REQUEST_HEADER_NAMES,
};

/// The request, decoder, tool-call budget and advisory; the status table is the shared one
/// over xAI's `{"code","error"}` or OpenAI-envelope bodies (the trait default).
impl VendorRequest for GrokAdapter {
    type Wire = Sse<ChunkMapper>;
    const WORDING: Wording = WORDING;

    async fn build(
        &self,
        session: &VendorSession<'_>,
        req: &ThinRequest,
        token: &Token,
    ) -> Result<reqwest::RequestBuilder, ThinError> {
        let client_version = session.cli().client_version();
        Ok(session
            .http()
            .post(format!(
                "{}/v1/chat/completions",
                self.base_url.trim_end_matches('/')
            ))
            .header("authorization", format!("Bearer {}", token.access_token))
            .header("x-grok-client-identifier", CLIENT_IDENTIFIER)
            .header("x-grok-client-version", &client_version)
            .header("user-agent", format!("grok-cli/{client_version}"))
            .header("accept", "text/event-stream")
            .header("content-type", "application/json")
            .json(&Self::build_body(req)))
    }

    fn wire(&self, model: &str, budgets: StreamBudgets) -> Self::Wire {
        Sse::new(ChunkMapper::new(model, budgets))
    }

    /// A tool call is delivered whole (SPEC §19.8 `grok.rs`): while one is being generated the
    /// wire is silent, and that silence may run past `budgets.idle_seconds`; such a request runs
    /// on `budgets.tool_call_floor_seconds` instead.
    fn wire_params(&self, req: &ThinRequest) -> Option<String> {
        Self::map_effort(req.effort).map(|e| format!("reasoning_effort={e}"))
    }

    fn budgets(&self, req: &ThinRequest, cfg: &ThinConfig) -> StreamBudgets {
        let own = &cfg.vendors.grok.budgets;
        let budgets = StreamBudgets::from_config(&OauthBudgets {
            first_event_seconds: own.first_event_seconds,
            idle_seconds: own.idle_seconds,
        });
        if req.request.tools.is_empty() {
            budgets
        } else {
            budgets.with_idle_floor(StreamBudgets::seconds(own.tool_call_floor_seconds))
        }
    }

    /// `x-ratelimit-remaining-tokens` / `x-ratelimit-limit-tokens` → utilisation advisory.
    fn advisory(&self, headers: &reqwest::header::HeaderMap) -> Option<ThinEvent> {
        vendor::rate_limit_advisory(
            headers,
            &AdvisoryHeaders::Remaining {
                limit: "x-ratelimit-limit-tokens",
                remaining: "x-ratelimit-remaining-tokens",
            },
        )
    }
}

impl ModelAdapter for GrokAdapter {
    fn vendor(&self) -> &'static str {
        VENDOR
    }

    fn run<'a>(&'a self, req: &'a ThinRequest, tx: EventSender) -> AdapterFuture<'a> {
        Box::pin(async move { self.session().run(self, req, &tx).await })
    }
}

/// OpenAI chat message (our lenient parse) → xAI wire message.
fn message_to_value(m: &crate::thin::openai::ChatMessage) -> Value {
    // `developer` is OpenAI-only; xAI knows `system`.
    let role = if m.role == "developer" {
        "system"
    } else {
        m.role.as_str()
    };
    let mut out = json!({ "role": role });
    match &m.content {
        None => {
            if m.tool_calls.is_none() {
                crate::thin::json_insert(&mut out, "content", json!(""));
            } else {
                crate::thin::json_insert(&mut out, "content", Value::Null);
            }
        }
        Some(Content::Text(t)) => crate::thin::json_insert(&mut out, "content", json!(t)),
        Some(Content::Parts(parts)) => {
            let parts: Vec<Value> = parts
                .iter()
                .filter_map(|p| match p {
                    ContentPart::Text { text } => Some(json!({ "type": "text", "text": text })),
                    ContentPart::ImageUrl { image_url } => Some(json!({
                        "type": "image_url",
                        "image_url": { "url": image_url.url, "detail": image_url.detail.clone().unwrap_or_else(|| "auto".into()) },
                    })),
                    ContentPart::Unknown => None,
                })
                .collect();
            crate::thin::json_insert(&mut out, "content", json!(parts));
        }
    }
    if let Some(name) = &m.name {
        crate::thin::json_insert(&mut out, "name", json!(name));
    }
    if let Some(calls) = &m.tool_calls {
        crate::thin::json_insert(
            &mut out,
            "tool_calls",
            json!(calls
                .iter()
                .map(|c| json!({
                    "id": c.id,
                    "type": "function",
                    "function": { "name": c.function.name, "arguments": c.function.arguments },
                }))
                .collect::<Vec<_>>()),
        );
    }
    if let Some(id) = &m.tool_call_id {
        crate::thin::json_insert(&mut out, "tool_call_id", json!(id));
    }
    // Pi replays `reasoning_content`; xAI ignores unknown assistant fields, so pass it through.
    if let Some(r) = &m.reasoning_content {
        if role == "assistant" {
            crate::thin::json_insert(&mut out, "reasoning_content", json!(r));
        }
    }
    out
}

const REQUEST_HEADER_NAMES: &str =
    "authorization, x-grok-client-identifier, x-grok-client-version, user-agent";

/// The shared table in grok's words, for a rejected request and a mid-stream error alike.
fn classify(
    status: u16,
    message: &str,
    retry_after: Option<u64>,
    model: &str,
    token_unexpired: bool,
) -> ThinError {
    vendor::classify_status(
        VENDOR,
        &WORDING,
        status,
        message,
        retry_after,
        model,
        token_unexpired,
    )
}

/// Mid-stream `{"error":…}` is the same table as HTTP, not a blanket `vendor_down`.
fn classify_stream_error(err: &Value, model: &str) -> ThinError {
    let message = err
        .as_str()
        .map(str::to_string)
        .or_else(|| {
            err.get("message")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .unwrap_or_else(|| err.to_string());
    let kind = err.get("type").and_then(Value::as_str).unwrap_or("");
    let code = err
        .get("code")
        .and_then(|c| {
            c.as_str()
                .map(str::to_string)
                .or_else(|| c.as_u64().map(|n| n.to_string()))
        })
        .unwrap_or_default();
    let retry = err
        .get("retry_after")
        .and_then(Value::as_u64)
        .or_else(|| err.get("retry_after_seconds").and_then(Value::as_u64));
    match stream_error_status(kind, &code, &message) {
        Some(status) => classify(status, &message, retry, model, true),
        None => ThinError::server(format!("grok stream error: {}", redact(&message)))
            .with_vendor(VENDOR),
    }
}

fn stream_error_status(kind: &str, code: &str, message: &str) -> Option<u16> {
    let kind_l = kind.to_ascii_lowercase();
    let code_l = code.to_ascii_lowercase();
    let msg_l = message.to_ascii_lowercase();
    let blob = format!("{kind_l} {code_l} {msg_l}");
    if kind_l == "rate_limit_error"
        || kind_l == "tokens"
        || code_l.contains("rate_limit")
        || code_l == "429"
        || blob.contains("rate limit")
        || blob.contains("too many requests")
        || blob.contains("usage limit")
        || blob.contains("quota")
        || blob.contains("credit")
    {
        return Some(429);
    }
    if kind_l == "authentication_error"
        || kind_l == "invalid_api_key"
        || code_l == "401"
        || blob.contains("unauthor")
        || blob.contains("unauthenticated")
        || blob.contains("invalid api key")
        || blob.contains("expired token")
    {
        return Some(401);
    }
    if kind_l == "permission_error" || code_l == "403" {
        return Some(403);
    }
    if kind_l == "insufficient_quota" || code_l == "402" {
        return Some(402);
    }
    if kind_l == "not_found" || kind_l == "not_found_error" || code_l == "404" {
        return Some(404);
    }
    if kind_l == "invalid_request_error" || code_l == "400" || code_l == "invalid-argument" {
        return Some(400);
    }
    if kind_l == "overloaded_error"
        || kind_l == "server_error"
        || code_l == "500"
        || code_l == "502"
        || code_l == "503"
        || blob.contains("overloaded")
    {
        return Some(503);
    }
    None
}

#[derive(Default)]
struct PendingCall {
    id: String,
    name: String,
    arguments: String,
}

/// `chat.completion.chunk` stream → thin events. Text/reasoning stream live; tool calls are
/// accumulated by `index` and emitted whole when the choice finishes (SPEC §19.4).
pub struct ChunkMapper {
    pub core: MapperCore,
    /// By the vendor's `index`: deltas for one call arrive interleaved with another's.
    calls: BTreeMap<usize, PendingCall>,
    finish: Option<FinishReason>,
    usage: Option<Usage>,
}

impl ChunkMapper {
    pub fn new(model: &str, budgets: StreamBudgets) -> Self {
        Self {
            core: MapperCore::new(VENDOR, model, budgets),
            calls: BTreeMap::new(),
            finish: None,
            usage: None,
        }
    }

    pub fn accept(&mut self, data: &str) {
        let data = data.trim();
        if data.is_empty() || self.core.done {
            return;
        }
        if data == "[DONE]" {
            self.finish();
            return;
        }
        let chunk: Value = match serde_json::from_str(data) {
            Ok(v) => v,
            Err(e) => {
                self.core.fail(ThinError::vendor_drift(
                    VENDOR,
                    format!("unparseable stream chunk: {e}: {}", redact(data)),
                ));
                return;
            }
        };
        if let Some(err) = chunk.get("error") {
            self.core
                .fail(classify_stream_error(err, self.core.model()));
            return;
        }
        self.core.ensure_started();
        if let Some(u) = chunk.get("usage").filter(|u| !u.is_null()) {
            self.usage = Some(vendor::usage_from(u, &USAGE_KEYS));
        }
        let Some(choice) = chunk
            .get("choices")
            .and_then(Value::as_array)
            .and_then(|c| c.first())
        else {
            return;
        };
        if let Some(delta) = choice.get("delta") {
            if let Some(text) = delta.get("content").and_then(Value::as_str) {
                if !text.is_empty() {
                    self.core.emit(ThinEvent::TextDelta(text.to_string()));
                }
            }
            if let Some(text) = delta.get("reasoning_content").and_then(Value::as_str) {
                if !text.is_empty() {
                    self.core.emit(ThinEvent::ReasoningDelta {
                        text: text.to_string(),
                        signature: None,
                        details: None,
                    });
                }
            }
            if let Some(calls) = delta.get("tool_calls").and_then(Value::as_array) {
                for call in calls {
                    let index = call.get("index").and_then(Value::as_u64).unwrap_or(0);
                    let Some(index) = usize::try_from(index)
                        .ok()
                        .filter(|i| *i < MAX_TOOL_CALL_INDEX)
                    else {
                        self.core.fail(ThinError::vendor_drift(
                            VENDOR,
                            format!("grok tool call index {index} (limit {MAX_TOOL_CALL_INDEX})"),
                        ));
                        return;
                    };
                    let slot = self.calls.entry(index).or_default();
                    if let Some(id) = call.get("id").and_then(Value::as_str) {
                        if !id.is_empty() {
                            slot.id = id.to_string();
                        }
                    }
                    if let Some(f) = call.get("function") {
                        if let Some(name) = f.get("name").and_then(Value::as_str) {
                            if !name.is_empty() {
                                slot.name = name.to_string();
                            }
                        }
                        if let Some(args) = f.get("arguments").and_then(Value::as_str) {
                            if !self.core.accumulate(&mut slot.arguments, args) {
                                return;
                            }
                        }
                    }
                }
            }
        }
        if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
            self.finish = Some(match reason {
                "tool_calls" | "function_call" => FinishReason::ToolCalls,
                "length" => FinishReason::Length,
                _ => {
                    if self.calls.is_empty() {
                        FinishReason::Stop
                    } else {
                        FinishReason::ToolCalls
                    }
                }
            });
        }
    }

    /// End of stream: flush tool calls, usage, finish.
    pub fn finish(&mut self) {
        if self.core.done {
            return;
        }
        self.core.ensure_started();
        for (index, call) in std::mem::take(&mut self.calls) {
            if call.name.is_empty() {
                continue;
            }
            let arguments = if call.arguments.trim().is_empty() {
                "{}".to_string()
            } else {
                call.arguments
            };
            let id = if call.id.is_empty() {
                format!("call-{index}")
            } else {
                call.id
            };
            self.core.emit(ThinEvent::ToolCall {
                id,
                name: call.name,
                arguments,
            });
        }
        if let Some(u) = self.usage.take() {
            self.core.emit(ThinEvent::Usage(u));
        }
        let reason = self.finish.take().unwrap_or(FinishReason::Stop);
        self.core.finish(ThinEvent::Finish(reason));
    }
}

impl SseMapper for ChunkMapper {
    fn core(&mut self) -> &mut MapperCore {
        &mut self.core
    }

    fn on_event(&mut self, event: &SseEvent) {
        self.accept(&event.data);
    }

    /// `[DONE]` lost but `finish_reason` seen: the answer is whole. Neither: truncated.
    fn on_end(&mut self) {
        if self.finish.is_some() {
            self.finish();
        }
    }

    /// `finish_reason` means the answer is whole; only `[DONE]` and perhaps usage were lost.
    fn on_interrupt(&mut self, err: ThinError) {
        if self.finish.is_some() {
            self.finish();
        } else {
            self.core.fail(err);
        }
    }
}

const USAGE_KEYS: UsageKeys = UsageKeys {
    prompt: "prompt_tokens",
    completion: &["completion_tokens"],
    total: "total_tokens",
    cached: "/prompt_tokens_details/cached_tokens",
};

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::config::models::{default_input, ModelEntry, Transport};
    use crate::thin::openai::ChatRequest;
    use crate::thinking::Effort;
    use crate::types::BackendId;

    fn entry() -> ModelEntry {
        ModelEntry {
            id: "subs/grok/grok-4.7".into(),
            backend: BackendId::Grok,
            cli_model: "grok-4.7".into(),
            context_window: 131_072,
            max_tokens: 32_000,
            thinking: true,
            cost: 0.0,
            name: "Grok 4.7".into(),
            provider: None,
            transport: Transport::OauthDirect,
            vendor_model: None,
            enabled: true,
            input: default_input(),
        }
    }

    fn offline() -> GrokAdapter {
        GrokAdapter::new(&ThinConfig::default(), Arc::default())
    }

    fn mapper() -> ChunkMapper {
        ChunkMapper::new("grok-4.7", StreamBudgets::OFFLINE)
    }

    fn req(body: Value, effort: Option<&str>) -> ThinRequest {
        let request: ChatRequest = serde_json::from_value(body).unwrap();
        ThinRequest {
            request,
            entry: entry(),
            effort: crate::thinking::normalise_effort(effort).unwrap(),
            request_id: "chatcmpl-test".into(),
            path: "oauth",
            fallback: None,
            cursor_cwd: None,
        }
    }

    #[test]
    fn body_is_openai_passthrough_with_effort_map() {
        let r = req(
            json!({
                "model": "subs/grok/grok-4.7",
                "messages": [
                    {"role": "developer", "content": "be terse"},
                    {"role": "user", "content": [{"type":"text","text":"hi"}]},
                    {"role": "assistant", "content": null, "tool_calls": [{"id":"call_x","type":"function","function":{"name":"f","arguments":"{}"}}]},
                    {"role": "tool", "tool_call_id": "call_x", "content": "ok"}
                ],
                "tools": [{"type":"function","function":{"name":"f","parameters":{"type":"object"}}}],
                "max_completion_tokens": 100,
                "temperature": 0.2
            }),
            Some("max"),
        );
        let body = GrokAdapter::build_body(&r);
        assert_eq!(body["model"], "grok-4.7");
        assert_eq!(body["stream"], true);
        assert_eq!(body["stream_options"]["include_usage"], true);
        assert_eq!(body["reasoning_effort"], "xhigh");
        assert_eq!(body["max_tokens"], 100);
        assert_eq!(body["temperature"], 0.2);
        assert_eq!(body["messages"][0]["role"], "system");
        assert_eq!(body["messages"][1]["content"][0]["text"], "hi");
        assert!(body["messages"][2]["content"].is_null());
        assert_eq!(body["messages"][2]["tool_calls"][0]["id"], "call_x");
        assert_eq!(body["messages"][3]["tool_call_id"], "call_x");
        assert_eq!(body["tools"][0]["function"]["name"], "f");
    }

    #[test]
    fn effort_map_matches_spec_column() {
        assert_eq!(GrokAdapter::map_effort(None), None);
        assert_eq!(GrokAdapter::map_effort(Some(Effort::Off)), Some("minimal"));
        assert_eq!(
            GrokAdapter::map_effort(Some(Effort::Medium)),
            Some("medium")
        );
        assert_eq!(GrokAdapter::map_effort(Some(Effort::XHigh)), Some("xhigh"));
        assert_eq!(GrokAdapter::map_effort(Some(Effort::Max)), Some("xhigh"));
    }

    #[test]
    fn mapper_streams_text_reasoning_tool_calls_usage_finish() {
        let mut m = mapper();
        m.accept(
            r#"{"choices":[{"index":0,"delta":{"reasoning_content":"think","role":"assistant"}}]}"#,
        );
        m.accept(r#"{"choices":[{"index":0,"delta":{"content":"Hel"}}]}"#);
        m.accept(r#"{"choices":[{"index":0,"delta":{"content":"lo"}}]}"#);
        m.accept(r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"id":"call-abc-0","function":{"name":"get_weather","arguments":"{\"city\":"},"index":0,"type":"function"}]}}]}"#);
        m.accept(r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"function":{"arguments":"\"Brisbane\"}"},"index":0}]}}]}"#);
        m.accept(r#"{"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#);
        m.accept(r#"{"choices":[],"usage":{"prompt_tokens":723,"completion_tokens":3,"total_tokens":826,"prompt_tokens_details":{"cached_tokens":640}}}"#);
        m.accept("[DONE]");
        assert!(m.core.done);
        let kinds: Vec<String> = m
            .core
            .pending
            .iter()
            .map(|e| match e {
                ThinEvent::Started { .. } => "started".into(),
                ThinEvent::ReasoningDelta { text, .. } => format!("reason:{text}"),
                ThinEvent::TextDelta(t) => format!("text:{t}"),
                ThinEvent::ToolCall {
                    id,
                    name,
                    arguments,
                } => format!("call:{id}:{name}:{arguments}"),
                ThinEvent::Usage(u) => format!(
                    "usage:{}:{}:{}",
                    u.prompt_tokens,
                    u.completion_tokens,
                    u.prompt_tokens_details.as_ref().unwrap().cached_tokens
                ),
                ThinEvent::Finish(f) => format!("finish:{f:?}"),
                other => format!("{other:?}"),
            })
            .collect();
        assert_eq!(
            kinds,
            vec![
                "started",
                "reason:think",
                "text:Hel",
                "text:lo",
                "call:call-abc-0:get_weather:{\"city\":\"Brisbane\"}",
                "usage:723:3:640",
                "finish:ToolCalls",
            ]
        );
    }

    #[test]
    fn mapper_reports_error_chunk_before_start() {
        let mut m = mapper();
        m.accept(r#"{"code":"invalid-argument","error":"This model does not support reasoning_effort value none."}"#);
        assert!(matches!(m.core.pending.back(), Some(ThinEvent::Error(_))));
        assert!(!m.core.started && m.core.done);
    }

    #[test]
    fn tool_arguments_over_the_cap_are_drift_and_do_not_grow() {
        let mut m = mapper();
        m.calls.insert(
            0,
            PendingCall {
                id: "c".into(),
                name: "f".into(),
                arguments: "x".repeat(vendor::MAX_SSE_FRAME_BYTES - 10),
            },
        );
        let before = m.calls.get(&0).unwrap().arguments.len();
        m.accept(
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"01234567890"}}]}}]}"#,
        );
        assert_eq!(m.calls.get(&0).unwrap().arguments.len(), before);
        match m.core.pending.back() {
            Some(ThinEvent::Error(e)) => {
                assert_eq!(e.code, "vendor_drift");
                assert!(
                    e.message.contains(&vendor::MAX_SSE_FRAME_BYTES.to_string()),
                    "{}",
                    e.message
                );
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn error_message_accepts_xai_and_openai_shapes() {
        assert_eq!(
            vendor::error_message(r#"{"code":"invalid-argument","error":"bad"}"#),
            "bad"
        );
        assert_eq!(
            vendor::error_message(r#"{"error":{"message":"nope","type":"x"}}"#),
            "nope"
        );
        assert_eq!(vendor::error_message("plain text"), "plain text");
    }

    #[test]
    fn stream_without_done_still_finishes() {
        let mut m = mapper();
        m.accept(r#"{"choices":[{"index":0,"delta":{"content":"x"},"finish_reason":"stop"}]}"#);
        assert!(!m.core.done);
        m.finish();
        assert!(matches!(
            m.core.pending.back(),
            Some(ThinEvent::Finish(FinishReason::Stop))
        ));
    }

    /// "limit" is in every throttle message; it must not make one terminal.
    #[test]
    fn a_throttle_is_retryable_and_keeps_the_vendor_wait() {
        let e = classify(
            429,
            "Rate limit exceeded, please try again",
            Some(10),
            "grok-4.7",
            true,
        );
        assert_eq!(
            (e.status, e.no_retry(), e.retry_after_seconds()),
            (429, false, Some(10))
        );
        assert!(!e.message.starts_with("quota exceeded"), "{}", e.message);
        let e = classify(429, "You have run out of credits", None, "grok-4.7", true);
        assert!(e.no_retry());
    }

    #[test]
    fn stream_errors_classify_instead_of_vendor_down() {
        let mut throttle = mapper();
        throttle.accept(
            r#"{"error":{"type":"rate_limit_error","message":"Rate limit exceeded, please try again"}}"#,
        );
        match throttle.core.pending.back() {
            Some(ThinEvent::Error(e)) => {
                assert_eq!(e.status, 429, "{}", e.message);
                assert!(!e.no_retry());
                assert_eq!(e.code, "rate_limit_error");
            }
            other => panic!("{other:?}"),
        }

        let mut exhaust = mapper();
        exhaust.accept(r#"{"error":{"message":"You have exceeded your usage limit"}}"#);
        match exhaust.core.pending.back() {
            Some(ThinEvent::Error(e)) => {
                assert_ne!(e.code, "vendor_down", "{}", e.message);
                assert!(e.no_retry(), "{}", e.message);
            }
            other => panic!("{other:?}"),
        }

        let mut auth = mapper();
        auth.accept(r#"{"error":{"type":"authentication_error","message":"expired token"}}"#);
        match auth.core.pending.back() {
            Some(ThinEvent::Error(e)) => {
                assert_eq!(e.status, 401, "{}", e.message);
                assert_eq!(e.code, "vendor_login_required");
            }
            other => panic!("{other:?}"),
        }

        let mut unknown = mapper();
        unknown.accept(r#"{"error":{"message":"internal mapper fault"}}"#);
        match unknown.core.pending.back() {
            Some(ThinEvent::Error(e)) => {
                assert_eq!(e.code, "server_error", "{}", e.message);
                assert_eq!(e.status, 500);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_lost_terminator_delivers_a_whole_answer_but_not_a_cut_one() {
        let lost = || ThinError::vendor_down(VENDOR, "stream interrupted: reset");
        let mut whole = mapper();
        whole.accept(r#"{"choices":[{"index":0,"delta":{"content":"x"},"finish_reason":"stop"}]}"#);
        SseMapper::on_interrupt(&mut whole, lost());
        assert!(matches!(
            whole.core.pending.back(),
            Some(ThinEvent::Finish(FinishReason::Stop))
        ));

        let mut cut = mapper();
        cut.accept(r#"{"choices":[{"index":0,"delta":{"content":"half an ans"}}]}"#);
        SseMapper::on_interrupt(&mut cut, lost());
        assert!(matches!(cut.core.pending.back(), Some(ThinEvent::Error(_))));
        assert!(!cut
            .core
            .pending
            .iter()
            .any(|e| matches!(e, ThinEvent::Finish(_))));
    }

    #[test]
    fn a_wild_tool_call_index_is_drift_not_an_allocation() {
        let mut m = mapper();
        m.accept(r#"{"choices":[{"delta":{"tool_calls":[{"index":4000000000,"id":"c","function":{"name":"f"}}]}}]}"#);
        match m.core.pending.back() {
            Some(ThinEvent::Error(e)) => assert_eq!(e.code, "vendor_drift"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn classify_403_on_unexpired_token_is_vendor_drift() {
        let e = classify(403, "forbidden", None, "grok-4.7", true);
        assert_eq!(e.status, 502);
        assert_eq!(e.code, "vendor_drift");
        assert!(e.message.contains("headers sent:"), "{}", e.message);
        let e = classify(403, "forbidden", None, "grok-4.7", false);
        assert_eq!(e.status, 403);
        let e = classify(402, "payment required", None, "grok-4.7", true);
        assert_eq!(e.status, 402);
        assert_eq!(e.code, "insufficient_quota");
    }

    fn ends(events: &[ThinEvent]) -> (bool, Option<&ThinError>) {
        let finished = events.iter().any(|e| matches!(e, ThinEvent::Finish(_)));
        let error = events.iter().find_map(|e| match e {
            ThinEvent::Error(err) => Some(err),
            _ => None,
        });
        (finished, error)
    }

    /// #121: a body that closes cleanly after text, with no `finish_reason` and no `[DONE]`,
    /// is a truncated answer — an error, never `stop`.
    #[test]
    fn a_clean_close_without_an_end_marker_is_an_error_not_stop() {
        let cut = b"data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"half\"}}]}

";
        let events = offline().replay("grok-4.7", &[cut.to_vec()]);
        let (finished, error) = ends(&events);
        assert!(!finished, "{events:?}");
        let error = error.expect("truncation is an error");
        assert_eq!(error.code, "vendor_down");
        assert!(
            error.message.contains("before the answer ended"),
            "{}",
            error.message
        );

        // `finish_reason` seen, `[DONE]` lost: the answer is whole.
        let whole = b"data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"x\"},\"finish_reason\":\"stop\"}]}

";
        let events = offline().replay("grok-4.7", &[whole.to_vec()]);
        let (finished, error) = ends(&events);
        assert!(finished && error.is_none(), "{events:?}");
    }

    /// #126: a 200 whose stream then carries a rate-limit error is not a success. The
    /// outcome is recorded from how the stream ended, never from the headers.
    #[test]
    fn an_in_stream_error_after_a_200_is_recorded_as_a_rejection() {
        use super::super::state::VendorState;
        let frames = [
            b"data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"x\"}}]}

".to_vec(),
            b"data: {\"error\":{\"type\":\"rate_limit_error\",\"code\":\"429\",\"message\":\"slow down\"}}

".to_vec(),
        ];
        let state = VendorState::new();
        let mut wire = Sse::new(mapper());
        let events = vendor::drive_offline(&frames, &mut wire);
        vendor::record_outcome(&state, wire.mapper.core());
        assert!(
            matches!(events.last(), Some(ThinEvent::Error(_))),
            "{events:?}"
        );
        assert_eq!(state.successes(), 0);
        assert_ne!(state.last_status(), Some(200));
        assert!(state.last_rejection_excerpt().is_some());

        let whole = [b"data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"x\"},\"finish_reason\":\"stop\"}]}

data: [DONE]

".to_vec()];
        let mut wire = Sse::new(mapper());
        vendor::drive_offline(&whole, &mut wire);
        vendor::record_outcome(&state, wire.mapper.core());
        assert_eq!(state.successes(), 1);
        assert_eq!(state.last_status(), Some(200));

        // A turn that neither finished nor failed (the client left) records nothing.
        let mut open = Sse::new(mapper());
        open.mapper
            .accept(r#"{"choices":[{"index":0,"delta":{"content":"x"}}]}"#);
        vendor::record_outcome(&state, open.mapper.core());
        assert_eq!(state.successes(), 1);
    }
}
