//! Direct Anthropic Messages API with the Claude Code OAuth token (SPEC §19.8.1). Best effort:
//! the request is shaped to pass as Claude Code (identity block, betas, user-agent); when
//! Anthropic rejects it the failure is classified as a **block** and the caller falls back to
//! the bridge. Nothing is streamed to the client before HTTP 200, so replay is clean. The
//! request's life, the probe's included, is [`VendorSession::run`]'s (SPEC §19.8).
//!
//! Risk stated once: using this token outside Claude Code is a Consumer ToS violation per
//! Anthropic's Feb 2026 policy and carries account-action risk. `doctor` prints that line.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use serde_json::{json, Value};

use super::creds::{self, Token};
use super::sse::SseEvent;
use super::state::{redact, VendorState};
use super::vendor::{
    self, AdvisoryHeaders, CliBacked, CliToken, CliVersion, MapperCore, Outcome, Rejected, Sse,
    SseMapper, StreamBudgets, VendorParts, VendorRequest, VendorSession, WireMapper, Wording,
};
use crate::config::models::{ModelEntry, Transport};
use crate::config::router::{ClaudeThinkingBudgets, ThinConfig};
use crate::thin::anthropic_blocks::{BlockMachine, Strictness};
use crate::thin::capture::Tap;
use crate::thin::error::DirectBlock;
use crate::thin::openai::{
    ChatMessage, ChatRequest, Content, ContentPart, FinishReason, ReasoningDetail, Usage,
};
use crate::thin::{EventSender, ThinError, ThinEvent, ThinRequest};
use crate::thinking::{profile, Effort, VendorThinking};
use crate::types::BackendId;

pub const VENDOR: &str = "claude";
/// The identity line Anthropic validates on non-Haiku models (one of three accepted variants).
pub const IDENTITY: &str = "You are Claude Code, Anthropic's official CLI for Claude.";
pub const ANTHROPIC_VERSION: &str = "2023-06-01";
pub const BETA_BASE: &str = "claude-code-20250219,oauth-2025-04-20";
pub const BETA_THINKING: &str = "interleaved-thinking-2025-05-14";
pub const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";
pub const TOOL_PREFIX: &str = "mcp__pi__";
const DEFAULT_MAX_TOKENS: u64 = 16_384;
pub(crate) const THINKING_HEADROOM: u64 = 1_024;

#[derive(Debug)]
pub enum OauthFailure {
    /// Anthropic refused the token outside Claude Code; fall back to the bridge and mark blocked.
    Blocked(String),
    /// A non-Haiku 400 that is not a known request-validation error (SPEC §19.8.1). Replay the
    /// bridge once; two consecutive suspected blocks become a real block.
    SuspectedBlock(String),
    Error(ThinError),
}

/// The runner returns a [`ThinError`]; a §19.8.1 verdict rides on it as a [`DirectBlock`] tag
/// and comes back here as the variant `ClaudeAdapter` replays on the bridge.
impl From<ThinError> for OauthFailure {
    fn from(e: ThinError) -> Self {
        match e.direct_block() {
            Some(DirectBlock::Blocked) => Self::Blocked(e.message),
            Some(DirectBlock::Suspected) => Self::SuspectedBlock(e.message),
            None => Self::Error(e),
        }
    }
}

impl From<OauthFailure> for ThinError {
    fn from(f: OauthFailure) -> Self {
        let (reason, tag) = match f {
            OauthFailure::Blocked(reason) => (reason, DirectBlock::Blocked),
            OauthFailure::SuspectedBlock(reason) => (reason, DirectBlock::Suspected),
            OauthFailure::Error(e) => return e,
        };
        ThinError::vendor_drift(VENDOR, reason).with_direct_block(tag)
    }
}

pub struct ClaudeOauth {
    base_url: String,
    budgets: ClaudeThinkingBudgets,
    stream_budgets: StreamBudgets,
    /// The credentials file, and the `claude --version` cache the `user-agent` follows (#124).
    cli: CliBacked,
    /// Client, `thin.oauth_in_flight` permits (requests and probe), tap, reply timeout (#125).
    parts: VendorParts,
}

#[derive(Default)]
pub struct BuiltRequest {
    pub body: Value,
    pub thinking_enabled: bool,
    /// Prefixed wire name → client tool name (identity map when unprefixed).
    pub tool_names: BTreeMap<String, String>,
}

impl ClaudeOauth {
    /// The default credentials file and `thin:` block; `cli_version` in the `user-agent`.
    pub fn new(
        budgets: ClaudeThinkingBudgets,
        cli_version: Option<String>,
        base_url: Option<String>,
    ) -> Self {
        let unknown = CliVersion::with("claude", Box::new(|| None), Box::new(|_| {}));
        Self::configured(
            budgets,
            base_url,
            None,
            (unknown, cli_version),
            &ThinConfig::default(),
        )
    }

    /// `router.yaml`'s credentials path and `thin:` block, and the bridge's `claude --version`
    /// cache with the version read at start until it is known. The direct path is only handed
    /// a token the adapter judged unexpired (an expired one goes to the bridge, whose run
    /// refreshes the file, SPEC §19.8), so nothing here runs the CLI.
    pub fn configured(
        budgets: ClaudeThinkingBudgets,
        base_url: Option<String>,
        credentials_path: Option<PathBuf>,
        (version, cli_version): (Arc<CliVersion>, Option<String>),
        thin: &ThinConfig,
    ) -> Self {
        let read = move || match &credentials_path {
            Some(p) => creds::claude_token_from(p),
            None => creds::claude_token(),
        };
        let token = CliToken::new(VENDOR, "claude", "claude login").cli_owns_store(false);
        let fallback = cli_version.unwrap_or_else(|| "2.1.273".to_string());
        Self {
            base_url: base_url.unwrap_or_else(|| DEFAULT_BASE_URL.to_string()),
            budgets,
            stream_budgets: StreamBudgets::from_config(&thin.vendors.claude.budgets),
            cli: CliBacked::new(token, read, version, None, fallback),
            parts: VendorParts::new(thin),
        }
    }

    /// Quiet-event budgets finer than `router.yaml`'s seconds (tests).
    pub fn with_stream_budgets(mut self, budgets: StreamBudgets) -> Self {
        self.stream_budgets = budgets;
        self
    }

    /// `thin.reply_timeout_seconds` other than the `thin:` block's (tests).
    pub fn with_reply_timeout(mut self, limit: std::time::Duration) -> Self {
        self.parts.set_reply_timeout(limit);
        self
    }

    /// Record the wire exchange (`thickapi capture`); `serve` never sets one.
    pub fn set_tap(&mut self, tap: Tap) {
        self.parts.set_tap(tap);
    }

    /// The stored token and the CLI version the direct path sends.
    pub fn cli(&self) -> &CliBacked {
        &self.cli
    }

    /// Thinking budget per SPEC §19.5. `None` = omit the field; `Some(0)` = disabled.
    pub fn thinking_budget(&self, effort: Option<Effort>) -> Option<u64> {
        let probe = crate::config::models::ModelEntry::bridge_probe("oauth");
        for action in profile(BackendId::ClaudeCode).thin_apply(effort, &probe) {
            match action {
                VendorThinking::Omit => return None,
                VendorThinking::Disabled => return Some(0),
                VendorThinking::Budget { level } => return Some(self.budget_tokens(level)),
                _ => {}
            }
        }
        None
    }

    fn budget_tokens(&self, level: Effort) -> u64 {
        match level {
            Effort::Low => self.budgets.low as u64,
            Effort::Medium => self.budgets.medium as u64,
            Effort::High => self.budgets.high as u64,
            Effort::XHigh => self.budgets.xhigh as u64,
            Effort::Max => self.budgets.xhigh as u64 * 2,
            Effort::Off | Effort::Minimal => 0,
        }
    }

    pub fn build(&self, req: &ThinRequest, prefix_tools: bool) -> Result<BuiltRequest, ThinError> {
        let chat = &req.request;
        let mut budget = self.thinking_budget(req.effort);
        if force_thinking_off(chat, req.effort, &req.entry) {
            budget = Some(0);
        }
        let thinking_enabled = budget.is_some_and(|b| b > 0);

        let mut system: Vec<Value> = vec![json!({"type": "text", "text": IDENTITY})];
        let pi_system = collect_system(chat);
        if !pi_system.is_empty() {
            system.push(
                json!({"type": "text", "text": pi_system, "cache_control": {"type": "ephemeral"}}),
            );
        }

        let mut tool_names = BTreeMap::new();
        let mut tools: Vec<Value> = Vec::new();
        for (i, tool) in chat.tools.iter().enumerate() {
            if tool.kind != "function" {
                return Err(ThinError::invalid_request(
                    "tools[].type must be \"function\"",
                    Some(&format!("tools[{i}].type")),
                ));
            }
            let wire = if prefix_tools {
                format!("{TOOL_PREFIX}{}", tool.function.name)
            } else {
                tool.function.name.clone()
            };
            tool_names.insert(wire.clone(), tool.function.name.clone());
            tools.push(json!({
                "name": wire,
                "description": tool.function.description.clone().unwrap_or_default(),
                "input_schema": tool.function.parameters.clone().unwrap_or_else(|| json!({"type": "object", "properties": {}})),
            }));
        }
        let client_to_wire: BTreeMap<&str, &str> = tool_names
            .iter()
            .map(|(w, c)| (c.as_str(), w.as_str()))
            .collect();

        let messages = build_messages(chat, thinking_enabled, &client_to_wire, prefix_tools)?;

        let requested_max = chat
            .max_output_tokens()
            .or_else(|| (req.entry.max_tokens > 0).then_some(req.entry.max_tokens));
        let mut max_tokens = requested_max.unwrap_or(DEFAULT_MAX_TOKENS);
        if let Some(b) = budget.filter(|b| *b > 0) {
            max_tokens = max_tokens.max(b + THINKING_HEADROOM);
        }

        let mut body = json!({
            "model": req.entry.wire_model(),
            "max_tokens": max_tokens,
            "system": system,
            "messages": messages,
            "stream": true,
        });
        let Some(obj) = body.as_object_mut() else {
            panic!("json! object");
        };
        if !tools.is_empty() {
            obj.insert("tools".into(), Value::Array(tools));
            if let Some(choice) = tool_choice(chat)? {
                obj.insert("tool_choice".into(), choice);
            }
        }
        match budget {
            None => {}
            Some(0) => {
                obj.insert("thinking".into(), json!({"type": "disabled"}));
            }
            Some(b) => {
                obj.insert(
                    "thinking".into(),
                    json!({"type": "enabled", "budget_tokens": b}),
                );
            }
        }
        if !thinking_enabled {
            if let Some(t) = chat.temperature {
                obj.insert("temperature".into(), json!(t));
            }
            if let Some(p) = chat.top_p {
                obj.insert("top_p".into(), json!(p));
            }
        }
        if let Some(stop) = &chat.stop {
            let seqs: Vec<String> = match stop {
                Value::String(s) => vec![s.clone()],
                Value::Array(a) => a
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect(),
                _ => vec![],
            };
            if !seqs.is_empty() {
                obj.insert("stop_sequences".into(), json!(seqs));
            }
        }
        Ok(BuiltRequest {
            body,
            thinking_enabled,
            tool_names,
        })
    }

    fn session<'a>(&'a self, state: &'a VendorState) -> VendorSession<'a> {
        self.parts.session(VENDOR, &self.cli, state)
    }

    /// A `thin.oauth_in_flight` permit to hold across the §19.16 same-path retry.
    pub async fn permit(&self) -> Result<tokio::sync::SemaphorePermit<'_>, ThinError> {
        self.parts.permit(VENDOR).await
    }

    /// `req` as one direct request. `prefixed` names the tools `mcp__pi__*`; `may_prefix` lets
    /// a tool-naming 400 ask for that retry.
    fn request(
        &self,
        req: &ThinRequest,
        prefixed: bool,
        may_prefix: bool,
    ) -> Result<ClaudeDirectRequest, ThinError> {
        Ok(ClaudeDirectRequest {
            url: format!("{}/v1/messages", self.base_url.trim_end_matches('/')),
            built: self.build(req, prefixed)?,
            budgets: self.stream_budgets,
            serving: (req.path, req.fallback),
            may_prefix: may_prefix && !req.request.tools.is_empty(),
            wants_prefix: AtomicBool::new(false),
            finished: AtomicBool::new(false),
        })
    }

    /// Direct-path probe (SPEC §19.15 R2): identity block, one `read` tool (doctor handshake
    /// schema), thinking enabled at `Effort::Low`, `max_tokens >= Low + THINKING_HEADROOM`,
    /// prompt `OK` — sent through the same runner as a request, under an `in_flight` permit,
    /// and classified and recorded by the same hooks. The answer goes to a sink that stops at
    /// `Started`: the Messages API answered, so the path is open (`mark_ok`); the rest of the
    /// body is not read. Bounded by `thin.reply_timeout_seconds` (#125): a stalled probe must
    /// not hold the probe loop or its permit.
    pub async fn probe(
        &self,
        token: &Token,
        model: &str,
        state: &VendorState,
    ) -> Result<(), OauthFailure> {
        let permit = self.permit().await?;
        let req = probe_request(model)?;
        // No tool-naming retry: the probe classifies its first answer.
        let direct = self.request(&req, false, false)?;
        let session = self.session(state).with_token(token).holding(&permit);
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        let send = async move {
            let sent = session.run(&direct, &req, &tx).await;
            drop(tx);
            sent
        };
        // Dropping the sink after the first event ends the stream: `drive` sees the client go.
        let exchange = async move {
            let (sent, first) = tokio::join!(send, async move { rx.recv().await });
            sent.map(|()| first)
        };
        let limit = self.parts.reply_timeout();
        match vendor::bounded(VENDOR, "direct-path probe", limit, exchange).await? {
            Some(ThinEvent::Started { .. }) => {
                mark_success(state, model, 200);
                Ok(())
            }
            _ => Err(ThinError::vendor_drift(VENDOR, "probe ended before message_start").into()),
        }
    }

    /// Anthropic's model list (`GET /v1/models`) with the same token and identity headers, for
    /// `thickapi models check` (SPEC §10.3): the ids the subscription can currently name.
    pub async fn list_models(&self, token: &Token) -> Result<Vec<String>, ThinError> {
        let url = format!(
            "{}/v1/models?limit=100",
            self.base_url.trim_end_matches('/')
        );
        let state = VendorState::new(); // a list is not a request outcome
        let session = self.session(&state);
        let rb = claude_headers(session.http().get(url), token, false, &self.cli)
            .header("accept", "application/json");
        let doc: Value = session
            .fetch_json("GET /v1/models", rb, |status, text| {
                ThinError::vendor_down(VENDOR, format!("GET /v1/models {status}: {}", redact(text)))
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
                "/v1/models answered without a data[] list",
            ));
        }
        Ok(ids)
    }

    /// Run one request and record its outcome. `Started` is sent only after HTTP 200.
    pub async fn run(
        &self,
        req: &ThinRequest,
        tx: &EventSender,
        token: &Token,
        state: &VendorState,
    ) -> Result<(), OauthFailure> {
        if self.run_attempt(req, tx, token, state, None).await? {
            mark_success(state, req.entry.wire_model(), 200);
        }
        Ok(())
    }

    /// [`Self::run`] without recording a success: returns whether the answer finished, so a
    /// caller that may still drop it (the §19.16 guard) records it only once it is kept.
    /// Failures are recorded by the runner (#126). `permit` is one the caller holds across
    /// attempts; without it the runner takes one per send.
    pub async fn run_attempt(
        &self,
        req: &ThinRequest,
        tx: &EventSender,
        token: &Token,
        state: &VendorState,
        permit: Option<&tokio::sync::SemaphorePermit<'_>>,
    ) -> Result<bool, OauthFailure> {
        let mut prefixed = false;
        loop {
            let direct = self.request(req, prefixed, !prefixed)?;
            let mut session = self.session(state).with_token(token);
            if let Some(permit) = permit {
                session = session.holding(permit);
            }
            match session.run(&direct, req, tx).await {
                Ok(()) => return Ok(direct.finished.load(Ordering::Relaxed)),
                // Tool naming rejection: retry once with the CLI's mcp__pi__ prefix.
                Err(_) if direct.wants_prefix.load(Ordering::Relaxed) => prefixed = true,
                Err(e) => return Err(e.into()),
            }
        }
    }
}

/// The Claude Code request headers, shared by a request, the probe and `list_models`.
fn claude_headers(
    builder: reqwest::RequestBuilder,
    token: &Token,
    thinking: bool,
    cli: &CliBacked,
) -> reqwest::RequestBuilder {
    let beta = if thinking {
        format!("{BETA_BASE},{BETA_THINKING}")
    } else {
        BETA_BASE.to_string()
    };
    builder
        .header("authorization", format!("Bearer {}", token.access_token))
        .header("anthropic-version", ANTHROPIC_VERSION)
        .header("anthropic-beta", beta)
        .header(
            "user-agent",
            format!("claude-cli/{} (external, cli)", cli.client_version_now()),
        )
        .header("x-app", "cli")
        .header("accept", "text/event-stream")
        .header("content-type", "application/json")
}

fn probe_request(model: &str) -> Result<ThinRequest, ThinError> {
    let request: ChatRequest = serde_json::from_value(json!({
        "model": model,
        "messages": [{"role": "user", "content": "OK"}],
        "tools": [{"type": "function", "function": {"name": "read", "description": "Read a file.",
            "parameters": {"type": "object", "properties": {"path": {"type": "string"}}, "required": ["path"]}}}],
        "max_tokens": 1
    }))
    .map_err(|e| ThinError::server(e.to_string()))?;
    Ok(ThinRequest {
        request,
        entry: probe_entry(model),
        effort: Some(Effort::Low),
        request_id: "probe".into(),
        path: "oauth",
        fallback: None,
        cursor_cwd: None,
    })
}

/// One direct Messages request as [`VendorSession::run`] sees it (SPEC §19.8, §19.8.1).
pub struct ClaudeDirectRequest {
    url: String,
    built: BuiltRequest,
    budgets: StreamBudgets,
    /// `Started`'s path and R4 fallback tag (SPEC §19.15 R4).
    serving: (&'static str, Option<&'static str>),
    may_prefix: bool,
    /// `classify` saw a tool-naming 400: the caller retries prefixed; nothing is recorded.
    wants_prefix: AtomicBool,
    /// Ended on a `Finish`; the caller records it once the §19.16 guard keeps the answer.
    finished: AtomicBool,
}

impl ClaudeDirectRequest {
    /// Fixture replay only (SPEC §19.11): the decoder `run` uses, tool names unprefixed.
    pub fn offline() -> Self {
        Self {
            url: String::new(),
            built: BuiltRequest::default(),
            budgets: StreamBudgets::OFFLINE,
            serving: ("oauth", None),
            may_prefix: false,
            wants_prefix: AtomicBool::new(false),
            finished: AtomicBool::new(false),
        }
    }
}

impl VendorRequest for ClaudeDirectRequest {
    type Wire = Sse<StreamMapper>;
    const WORDING: Wording = Wording {
        label: "claude oauth",
        host: "api.anthropic.com",
        login_hint: "claude login",
        headers_sent: REQUEST_HEADER_NAMES,
    };

    async fn build(
        &self,
        session: &VendorSession<'_>,
        _req: &ThinRequest,
        token: &Token,
    ) -> Result<reqwest::RequestBuilder, ThinError> {
        let post = session.http().post(&self.url);
        Ok(
            claude_headers(post, token, self.built.thinking_enabled, session.cli())
                .json(&self.built.body),
        )
    }

    /// The model is the one `message_start` names, not the one asked for.
    fn wire(&self, _model: &str, budgets: StreamBudgets) -> Self::Wire {
        let (path, fallback) = self.serving;
        let core = MapperCore::new(VENDOR, "", budgets).with_serving(path, fallback);
        Sse::new(StreamMapper::new(&self.built.tool_names, core))
    }

    fn wire_params(&self, _req: &ThinRequest) -> Option<String> {
        let t = self.built.body.get("thinking")?;
        match t.get("type").and_then(Value::as_str)? {
            "enabled" => {
                let b = t.get("budget_tokens").and_then(Value::as_u64)?;
                Some(format!("thinking_budget={b}"))
            }
            other => Some(format!("thinking={other}")),
        }
    }

    fn budgets(&self, _req: &ThinRequest, _cfg: &ThinConfig) -> StreamBudgets {
        self.budgets
    }

    /// The §19.8.1 rule table ([`classify`]); a tool-naming 400 on an unprefixed request asks
    /// for the prefixed retry first.
    fn classify(&self, rejected: &Rejected<'_>) -> ThinError {
        let (err_type, message) = parse_error(rejected.body);
        let naming = message.contains("tools") && message.contains("name");
        if self.may_prefix && rejected.status == 400 && naming {
            self.wants_prefix.store(true, Ordering::Relaxed);
            return ThinError::invalid_request(redact(&message), None).with_vendor(VENDOR);
        }
        let wait = vendor::retry_after(rejected.headers);
        let (token, model) = (rejected.token, rejected.model);
        classify(rejected.status, err_type, &message, wait, token, model).into()
    }

    /// `anthropic-ratelimit-unified-*` (Claude Code's `rate_limit_event`).
    fn advisory(&self, headers: &reqwest::header::HeaderMap) -> Option<ThinEvent> {
        let unified = AdvisoryHeaders::Utilization {
            utilization: &[
                "anthropic-ratelimit-unified-5h-utilization",
                "anthropic-ratelimit-unified-utilization",
            ],
            reset: &[
                "anthropic-ratelimit-unified-5h-reset",
                "anthropic-ratelimit-unified-reset",
            ],
        };
        vendor::rate_limit_advisory(headers, &unified)
    }

    /// A rejection is recorded under its classification (`blocked` / `suspected_block` / the
    /// code); the block itself is `ClaudeAdapter`'s to mark, with its cooldown and count. A
    /// stream records its failure here and its finish through the caller, once the §19.16
    /// guard keeps the answer (#126) — where a Haiku finish never clears the block (#122).
    fn on_outcome(&self, state: &VendorState, outcome: Outcome<'_, Self::Wire>) {
        match outcome {
            Outcome::Rejected { .. } if self.wants_prefix.load(Ordering::Relaxed) => {}
            Outcome::Rejected {
                status,
                error,
                body,
            } => {
                let classification = match error.direct_block() {
                    Some(DirectBlock::Blocked) => "blocked",
                    Some(DirectBlock::Suspected) => "suspected_block",
                    None => error.code,
                };
                let wait = error.retry_after_seconds();
                state.mark_rejection_retry(status, classification, body, wait);
            }
            Outcome::Streamed { wire, .. } => {
                let core = wire.core();
                self.finished.store(core.finished(), Ordering::Relaxed);
                vendor::record_outcome_with(state, core, || {});
            }
        }
    }
}

fn collect_system(chat: &ChatRequest) -> String {
    let mut out = String::new();
    for m in &chat.messages {
        if m.role == "system" || m.role == "developer" {
            if let Some(c) = &m.content {
                if !out.is_empty() {
                    out.push_str("\n\n");
                }
                out.push_str(&c.text());
            }
        }
    }
    out
}

fn image_source(url: &str, param: &str) -> Result<Value, ThinError> {
    if let Some(rest) = url.strip_prefix("data:") {
        let (meta, payload) = rest
            .split_once(',')
            .ok_or_else(|| ThinError::invalid_request("malformed data: URL", Some(param)))?;
        let mime = meta.split(';').next().unwrap_or_default();
        return Ok(json!({"type": "base64", "media_type": mime, "data": payload}));
    }
    Ok(json!({"type": "url", "url": url}))
}

fn content_to_blocks(
    content: &Content,
    param: &str,
    allow_images: bool,
) -> Result<Vec<Value>, ThinError> {
    match content {
        Content::Text(t) => Ok(if t.is_empty() {
            vec![]
        } else {
            vec![json!({"type": "text", "text": t})]
        }),
        Content::Parts(parts) => {
            let mut out = Vec::new();
            for (i, part) in parts.iter().enumerate() {
                match part {
                    ContentPart::Text { text } if !text.is_empty() => {
                        out.push(json!({"type": "text", "text": text}))
                    }
                    ContentPart::Text { .. } => {}
                    ContentPart::ImageUrl { image_url } => {
                        if !allow_images {
                            return Err(ThinError::invalid_request(
                                "image parts are not accepted in this position",
                                Some(&format!("{param}[{i}]")),
                            ));
                        }
                        out.push(json!({"type": "image", "source": image_source(&image_url.url, &format!("{param}[{i}]"))?}));
                    }
                    ContentPart::Unknown => {
                        return Err(ThinError::invalid_request(
                            "unsupported content part type",
                            Some(&format!("{param}[{i}].type")),
                        ))
                    }
                }
            }
            Ok(out)
        }
    }
}

fn probe_entry(model: &str) -> ModelEntry {
    ModelEntry {
        id: format!("probe/{model}"),
        context_window: 200_000,
        thinking: true,
        transport: Transport::OauthDirect,
        vendor_model: Some(model.to_string()),
        ..ModelEntry::bridge_probe(model)
    }
}

/// Last assistant has `tool_calls` and no recoverable signature while thinking would be enabled.
pub fn force_thinking_off(
    chat: &ChatRequest,
    effort: Option<Effort>,
    entry: &crate::config::models::ModelEntry,
) -> bool {
    let would_enable = profile(BackendId::ClaudeCode)
        .thin_apply(effort, entry)
        .iter()
        .any(|a| matches!(a, VendorThinking::Budget { .. }));
    if !would_enable {
        return false;
    }
    let Some(last) = chat.messages.iter().rev().find(|m| m.role == "assistant") else {
        return false;
    };
    last.tool_calls.as_ref().is_some_and(|c| !c.is_empty())
        && !last.has_recoverable_thinking_signature()
}

fn replay_thinking_blocks(m: &ChatMessage) -> Vec<Value> {
    let mut blocks = Vec::new();
    if let Some(details) = &m.reasoning_details {
        for d in details {
            match d {
                ReasoningDetail::Text { text, signature } => {
                    if let Some(sig) = signature.as_ref().filter(|s| !s.is_empty()) {
                        blocks.push(json!({
                            "type": "thinking",
                            "thinking": text,
                            "signature": sig
                        }));
                    }
                }
                ReasoningDetail::Encrypted { data } if !data.is_empty() => {
                    blocks.push(json!({"type": "redacted_thinking", "data": data}));
                }
                _ => {}
            }
        }
        if !blocks.is_empty() {
            return blocks;
        }
    }
    if let (Some(thinking), Some(sig)) = (&m.reasoning_content, &m.reasoning_signature) {
        if !thinking.is_empty() && !sig.is_empty() {
            blocks.push(json!({
                "type": "thinking",
                "thinking": thinking,
                "signature": sig
            }));
        }
    }
    blocks
}

fn build_messages(
    chat: &ChatRequest,
    thinking_enabled: bool,
    client_to_wire: &BTreeMap<&str, &str>,
    prefix_tools: bool,
) -> Result<Vec<Value>, ThinError> {
    let mut out: Vec<Value> = Vec::new();
    let push = |out: &mut Vec<Value>, role: &str, blocks: Vec<Value>| {
        if blocks.is_empty() {
            return;
        }
        if let Some(last) = out.last_mut() {
            if last["role"] == role {
                if let Some(arr) = last["content"].as_array_mut() {
                    arr.extend(blocks);
                    return;
                }
            }
        }
        out.push(json!({"role": role, "content": blocks}));
    };
    for (i, m) in chat.messages.iter().enumerate() {
        let param = format!("messages[{i}]");
        match m.role.as_str() {
            "system" | "developer" => {}
            "user" => {
                let content = m.content.as_ref().ok_or_else(|| {
                    ThinError::invalid_request(
                        "user message needs content",
                        Some(&format!("{param}.content")),
                    )
                })?;
                push(
                    &mut out,
                    "user",
                    content_to_blocks(content, &format!("{param}.content"), true)?,
                );
            }
            "assistant" => {
                let mut blocks = Vec::new();
                if thinking_enabled {
                    blocks.extend(replay_thinking_blocks(m));
                }
                if let Some(c) = &m.content {
                    blocks.extend(content_to_blocks(c, &format!("{param}.content"), false)?);
                }
                for (j, call) in m.tool_calls.iter().flatten().enumerate() {
                    let input: Value = serde_json::from_str(&call.function.arguments)
                        .unwrap_or_else(|_| json!({}));
                    let input = if input.is_object() { input } else { json!({}) };
                    let name = client_to_wire
                        .get(call.function.name.as_str())
                        .map(|s| s.to_string())
                        .unwrap_or_else(|| {
                            if prefix_tools {
                                format!("{TOOL_PREFIX}{}", call.function.name)
                            } else {
                                call.function.name.clone()
                            }
                        });
                    if call.id.is_empty() {
                        return Err(ThinError::invalid_request(
                            "tool_calls[].id must be nonempty",
                            Some(&format!("{param}.tool_calls[{j}].id")),
                        ));
                    }
                    blocks.push(
                        json!({"type": "tool_use", "id": call.id, "name": name, "input": input}),
                    );
                }
                push(&mut out, "assistant", blocks);
            }
            "tool" => {
                let id = m
                    .tool_call_id
                    .as_deref()
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| {
                        ThinError::invalid_request(
                            "tool message needs tool_call_id",
                            Some(&format!("{param}.tool_call_id")),
                        )
                    })?;
                let content = match &m.content {
                    Some(c) => content_to_blocks(c, &format!("{param}.content"), true)?,
                    None => vec![],
                };
                let content_value = if content.is_empty() {
                    json!("")
                } else if content.len() == 1
                    && content
                        .first()
                        .and_then(|c| c.get("type"))
                        .and_then(Value::as_str)
                        == Some("text")
                {
                    content
                        .first()
                        .and_then(|c| c.get("text"))
                        .cloned()
                        .unwrap_or(json!(""))
                } else {
                    Value::Array(content)
                };
                push(
                    &mut out,
                    "user",
                    vec![
                        json!({"type": "tool_result", "tool_use_id": id, "content": content_value}),
                    ],
                );
            }
            other => {
                return Err(ThinError::invalid_request(
                    format!("unsupported message role: {other}"),
                    Some(&format!("{param}.role")),
                ))
            }
        }
    }
    Ok(out)
}

fn tool_choice(chat: &ChatRequest) -> Result<Option<Value>, ThinError> {
    let disable_parallel = !chat.parallel_tool_calls();
    let choice = match &chat.tool_choice {
        None => None,
        Some(Value::String(s)) => match s.as_str() {
            "auto" => None,
            "none" => Some(json!({"type": "none"})),
            "required" => Some(json!({"type": "any"})),
            other => {
                return Err(ThinError::invalid_request(
                    format!("unsupported tool_choice: {other}"),
                    Some("tool_choice"),
                ))
            }
        },
        Some(Value::Object(o)) => {
            let name = o
                .get("function")
                .and_then(|f| f.get("name"))
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    ThinError::invalid_request(
                        "tool_choice.function.name required",
                        Some("tool_choice"),
                    )
                })?;
            Some(json!({"type": "tool", "name": name}))
        }
        Some(_) => {
            return Err(ThinError::invalid_request(
                "malformed tool_choice",
                Some("tool_choice"),
            ))
        }
    };
    Ok(match (choice, disable_parallel) {
        (None, false) => None,
        (None, true) => Some(json!({"type": "auto", "disable_parallel_tool_use": true})),
        (Some(mut c), true) => {
            if c.get("type").and_then(Value::as_str) != Some("none") {
                crate::thin::json_insert(&mut c, "disable_parallel_tool_use", json!(true));
            }
            Some(c)
        }
        (Some(c), false) => Some(c),
    })
}

fn parse_error(text: &str) -> (String, String) {
    let v: Value = serde_json::from_str(text).unwrap_or(Value::Null);
    let err = v.get("error").cloned().unwrap_or(Value::Null);
    let kind = err
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let message = err
        .get("message")
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| text.chars().take(300).collect());
    (kind, message)
}

/// Haiku has never been blocked (SPEC §19.15); a Haiku row may stay on the direct path
/// while non-Haiku traffic is bridged.
pub fn model_is_haiku(model: &str) -> bool {
    model.to_ascii_lowercase().contains("haiku")
}

/// A direct-path success. Haiku rides the direct path while non-Haiku is blocked, so its
/// success must not clear that block (audit R2, #122).
pub(super) fn mark_success(state: &VendorState, wire_model: &str, status: u16) {
    if model_is_haiku(wire_model) {
        state.mark_ok_haiku(status);
    } else {
        state.mark_ok(status);
    }
}

/// Named ToS/third-party wordings. First match in the rule table; any HTTP status.
fn is_named_block(message: &str) -> bool {
    if message.contains("only authorized for use with Claude Code") {
        return true;
    }
    message.contains("Third-party apps") && message.contains("extra usage")
}

/// Request-validation patterns (SPEC §19.8.1). Ordered, more specific than a bare `model`
/// substring so a reworded block that happens to mention "model" is not a hard 400.
fn is_request_validation(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    [
        "messages.",
        "max_tokens",
        "tools.",
        "thinking.",
        "system.",
        "tool_use_id",
        "model:",
        "invalid model",
        "unknown model",
        "prompt is too long",
    ]
    .iter()
    .any(|p| lower.contains(p))
}

/// Ordered classification (SPEC §19.8.1 / §19.15 C9–C10). First match wins.
///
/// 1. Haiku never returns `Blocked` / `SuspectedBlock` (model-class-aware).
/// 2. Named block wordings → `Blocked`.
/// 3. 400 validation → `invalid_request`.
/// 4. Other non-Haiku 400 → `SuspectedBlock`.
/// 5. 401 `authentication_error` on an unexpired token → `Blocked`.
/// 6. 429 → rate-limit / quota. A lone first-request 429 is **not** a block (C9);
///    named wording in (2) already caught a disguised ToS 429.
fn classify(
    status: u16,
    err_type: String,
    message: &str,
    retry_after: Option<u64>,
    token: &Token,
    model: &str,
) -> OauthFailure {
    let shown = redact(message);
    let cli = format!("claude oauth {status} {err_type}");
    if model_is_haiku(model) {
        if status == 400 {
            return OauthFailure::Error(
                ThinError::invalid_request(shown, None).with_vendor(VENDOR),
            );
        }
        return OauthFailure::Error(status_error(
            status,
            &cli,
            &shown,
            message,
            retry_after,
            model,
            !token.expired(),
        ));
    }
    if is_named_block(message) {
        let reason = if message.contains("Third-party apps") {
            format!("{cli}: classed as third-party app: {shown}")
        } else {
            format!("{cli}: {shown}")
        };
        return OauthFailure::Blocked(reason);
    }
    if status == 400 {
        if is_request_validation(message) {
            return OauthFailure::Error(
                ThinError::invalid_request(shown, None).with_vendor(VENDOR),
            );
        }
        return OauthFailure::SuspectedBlock(format!("{cli}: suspected block: {shown}"));
    }
    if status == 401 && err_type == "authentication_error" && !token.expired() {
        return OauthFailure::Blocked(format!("{cli}: unexpired token rejected: {shown}"));
    }
    OauthFailure::Error(status_error(
        status,
        &cli,
        &shown,
        message,
        retry_after,
        model,
        !token.expired(),
    ))
}

/// Header names on the Messages request, named in a 403-on-unexpired-token drift error.
const REQUEST_HEADER_NAMES: &str =
    "authorization, anthropic-version, anthropic-beta, user-agent, x-app";

/// The shared table (audit R8). `classify` has already sorted every Claude 400 (validation,
/// block, suspected block), so one that lands here stays the server error it always was. A
/// `403` is never quota: on an unexpired token it is drift, otherwise a permission error.
fn status_error(
    status: u16,
    cli: &str,
    shown: &str,
    message: &str,
    retry_after: Option<u64>,
    model: &str,
    token_unexpired: bool,
) -> ThinError {
    if status == 400 {
        return ThinError::server(format!("{cli}: {shown}"));
    }
    vendor::status_error(&vendor::StatusFault {
        vendor: VENDOR,
        status,
        prefix: cli,
        shown,
        raw: message,
        retry_after,
        model,
        token_unexpired,
        login_hint: "claude login",
        headers_sent: REQUEST_HEADER_NAMES,
    })
}

pub struct StreamMapper {
    core: MapperCore,
    tool_names: BTreeMap<String, String>,
    blocks: BlockMachine,
    /// `message_delta.stop_reason`: once set the answer is whole, whatever follows.
    finish: Option<FinishReason>,
    input: u64,
    output: u64,
    cache_read: u64,
    cache_write: u64,
}

impl StreamMapper {
    fn new(tool_names: &BTreeMap<String, String>, core: MapperCore) -> Self {
        Self {
            core,
            tool_names: tool_names.clone(),
            blocks: BlockMachine::new(),
            finish: None,
            input: 0,
            output: 0,
            cache_read: 0,
            cache_write: 0,
        }
    }

    /// Only `message_start` opens a turn. Anything else arriving first is not the Messages
    /// API, and `emit` would otherwise open the turn itself, with no model, and answer 200.
    fn never_started(&mut self, what: &str) -> bool {
        if self.core.started {
            return false;
        }
        self.core.fail(ThinError::vendor_drift(
            VENDOR,
            format!("{what} without message_start"),
        ));
        true
    }

    /// Usage, then `Finish`: the end of a good turn.
    fn complete(&mut self) {
        if self.never_started("stream ended") {
            return;
        }
        if let Some(usage) = self.usage() {
            self.core.emit(ThinEvent::Usage(usage));
        }
        let Some(finish) = self.finish else {
            let vendor = self.core.vendor();
            self.core.fail(ThinError::vendor_down(
                vendor,
                format!("{vendor} closed the stream before the answer ended"),
            ));
            return;
        };
        self.core.finish(ThinEvent::Finish(finish));
    }

    fn usage(&self) -> Option<Usage> {
        self.core
            .started
            .then(|| Usage::from_parts(self.input + self.cache_write, self.output, self.cache_read))
    }

    fn apply_usage(&mut self, usage: Option<&Value>) {
        let Some(u) = usage.and_then(Value::as_object) else {
            return;
        };
        let num = |k: &str| u.get(k).and_then(Value::as_u64);
        if let Some(v) = num("input_tokens") {
            self.input = v;
        }
        if let Some(v) = num("output_tokens") {
            self.output = v;
        }
        if let Some(v) = num("cache_read_input_tokens") {
            self.cache_read = v;
        }
        if let Some(v) = num("cache_creation_input_tokens") {
            self.cache_write = v;
        }
    }

    fn accept(&mut self, event: &SseEvent) {
        if self.core.done {
            return;
        }
        let data: Value = match serde_json::from_str(&event.data) {
            Ok(v) => v,
            Err(_) => return,
        };
        let kind = event
            .event
            .clone()
            .or_else(|| data.get("type").and_then(Value::as_str).map(str::to_string))
            .unwrap_or_default();
        let needs_a_message = matches!(
            kind.as_str(),
            "content_block_start"
                | "content_block_delta"
                | "content_block_stop"
                | "message_delta"
                | "message_stop"
        );
        if needs_a_message && self.never_started(&kind) {
            return;
        }
        match kind.as_str() {
            "message_start" => {
                let message = data.get("message").cloned().unwrap_or(Value::Null);
                self.apply_usage(message.get("usage"));
                let model = message
                    .get("model")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                self.core.set_model(model);
                self.core.ensure_started();
            }
            "content_block_start" => self.on_block_start(&data),
            "content_block_delta" => self.on_block_delta(&data),
            "content_block_stop" => self.on_block_stop(&data),
            "message_delta" => {
                self.apply_usage(data.get("usage"));
                if let Some(reason) = data
                    .get("delta")
                    .and_then(|d| d.get("stop_reason"))
                    .and_then(Value::as_str)
                {
                    self.finish = Some(match reason {
                        "tool_use" => FinishReason::ToolCalls,
                        "max_tokens" => FinishReason::Length,
                        _ => FinishReason::Stop,
                    });
                }
            }
            // The wire's own end marker: whole even if `message_delta` never said why.
            "message_stop" => {
                self.finish.get_or_insert(FinishReason::Stop);
                self.complete()
            }
            "error" => {
                let (kind, message) = parse_error(&event.data);
                let shown = redact(&message);
                let err = if kind == "overloaded_error" {
                    ThinError::vendor_down(VENDOR, format!("claude oauth stream: {shown}"))
                } else if kind == "rate_limit_error" {
                    vendor::limit_error(
                        VENDOR,
                        format!("claude oauth stream: {shown}"),
                        &message,
                        None,
                    )
                } else {
                    ThinError::vendor_down(VENDOR, format!("claude oauth stream {kind}: {shown}"))
                };
                self.core.fail(err);
            }
            _ => {}
        }
    }

    fn on_block_start(&mut self, data: &Value) {
        let index = data.get("index").and_then(Value::as_u64).unwrap_or(0);
        let block = data.get("content_block").cloned().unwrap_or(Value::Null);
        let names = &self.tool_names;
        let Ok(out) =
            self.blocks.start(
                index,
                &block,
                |wire| {
                    Some(names.get(wire).cloned().unwrap_or_else(|| {
                        wire.strip_prefix(TOOL_PREFIX).unwrap_or(wire).to_string()
                    }))
                },
                Strictness::Lenient,
            )
        else {
            return;
        };
        for event in out.events {
            self.core.emit(event);
        }
    }

    fn on_block_delta(&mut self, data: &Value) {
        let index = data.get("index").and_then(Value::as_u64).unwrap_or(0);
        let delta = data.get("delta").cloned().unwrap_or(Value::Null);
        let core = &mut self.core;
        let Ok(out) = self
            .blocks
            .delta(index, &delta, Strictness::Lenient, &mut |dst, piece| {
                core.accumulate(dst, piece)
            })
        else {
            return;
        };
        for event in out.events {
            core.emit(event);
        }
    }

    fn on_block_stop(&mut self, data: &Value) {
        let index = data.get("index").and_then(Value::as_u64).unwrap_or(0);
        let Ok(out) = self.blocks.stop(index, Strictness::Lenient) else {
            return;
        };
        for event in out.events {
            self.core.emit(event);
        }
    }
}

impl SseMapper for StreamMapper {
    fn core(&mut self) -> &mut MapperCore {
        &mut self.core
    }

    fn on_event(&mut self, event: &SseEvent) {
        self.accept(event);
    }

    /// No `message_stop`. With a `stop_reason` the answer is whole; without one it is
    /// truncated and the shared end-of-body rule fails it (#121). Before `message_start` it
    /// is not the Messages API at all.
    fn on_end(&mut self) {
        if self.finish.is_some() || !self.core.started {
            self.complete();
        }
    }

    fn on_interrupt(&mut self, err: ThinError) {
        if self.finish.is_some() {
            self.complete();
        } else {
            self.core.fail(err);
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::config::models::{ModelEntry, Transport};
    use crate::config::router::ClaudeThinkingBudgets;
    use crate::thin::anthropic_blocks::Block;
    use crate::thinking::Effort;

    fn entry() -> ModelEntry {
        let mut e = ModelEntry::bridge_probe("claude-sonnet-4-6");
        e.transport = Transport::OauthDirect;
        e.vendor_model = Some("claude-sonnet-4-6".into());
        e
    }

    fn thin(body: Value, effort: Option<&str>) -> ThinRequest {
        ThinRequest {
            request: serde_json::from_value(body).unwrap(),
            entry: entry(),
            effort: crate::thinking::normalise_effort(effort).unwrap(),
            request_id: "t".into(),
            path: "oauth",
            fallback: None,
            cursor_cwd: None,
        }
    }

    fn oauth() -> ClaudeOauth {
        ClaudeOauth::new(
            ClaudeThinkingBudgets::default(),
            Some("2.1.273".into()),
            None,
        )
    }

    #[test]
    fn a_403_is_never_quota() {
        // Audit R8 / #128: a permission or scope 403 used to read to Pi as "quota exceeded".
        for model in ["claude-fable-5-1", "claude-haiku-4-5"] {
            let e = classify(
                403,
                "permission_error".into(),
                "not allowed",
                None,
                &token(false),
                model,
            );
            let OauthFailure::Error(e) = e else {
                panic!("{model}: 403 is an error, not a block");
            };
            assert_eq!((e.status, e.code), (502, "vendor_drift"), "{model}");
            assert!(e.message.contains("anthropic-beta"), "{}", e.message);
            let e = classify(
                403,
                "permission_error".into(),
                "not allowed",
                None,
                &token(true),
                model,
            );
            let OauthFailure::Error(e) = e else {
                panic!("{model}: 403 is an error, not a block");
            };
            assert_eq!((e.status, e.code), (403, "permission_error"), "{model}");
        }
        let e = classify(
            402,
            "billing_error".into(),
            "payment required",
            None,
            &token(false),
            "claude-fable-5-1",
        );
        assert!(
            matches!(e, OauthFailure::Error(ref e) if e.code == "insufficient_quota"),
            "402 stays quota"
        );
    }

    #[test]
    fn body_starts_with_identity_then_cached_pi_system() {
        let req = thin(
            json!({"model": "m", "messages": [
                {"role": "system", "content": "You are Pi."},
                {"role": "user", "content": "hi"}
            ], "max_tokens": 100, "temperature": 0.2}),
            None,
        );
        let built = oauth().build(&req, false).unwrap();
        let b = &built.body;
        assert_eq!(b["system"][0]["text"], IDENTITY);
        assert_eq!(b["system"][1]["text"], "You are Pi.");
        assert_eq!(b["system"][1]["cache_control"]["type"], "ephemeral");
        assert_eq!(b["messages"][0]["role"], "user");
        assert_eq!(b["messages"][0]["content"][0]["text"], "hi");
        assert_eq!(b["max_tokens"], 100);
        assert_eq!(b["temperature"], 0.2);
        assert!(b.get("thinking").is_none());
        assert_eq!(b["stream"], true);
    }

    #[test]
    fn thinking_ladder_and_headroom() {
        let o = oauth();
        assert_eq!(o.thinking_budget(None), None);
        assert_eq!(o.thinking_budget(Some(Effort::Off)), Some(0));
        assert_eq!(o.thinking_budget(Some(Effort::Low)), Some(4_000));
        assert_eq!(o.thinking_budget(Some(Effort::High)), Some(32_000));
        assert_eq!(o.thinking_budget(Some(Effort::XHigh)), Some(64_000));
        let req = thin(
            json!({"model": "m", "messages": [{"role": "user", "content": "hi"}], "max_tokens": 100, "temperature": 0.2}),
            Some("medium"),
        );
        let built = o.build(&req, false).unwrap();
        assert_eq!(built.body["thinking"]["budget_tokens"], 16_000);
        assert_eq!(built.body["max_tokens"], 16_000 + THINKING_HEADROOM);
        assert!(
            built.body.get("temperature").is_none(),
            "temperature dropped with thinking"
        );
        assert!(built.thinking_enabled);
        let req = thin(
            json!({"model": "m", "messages": [{"role": "user", "content": "hi"}]}),
            Some("off"),
        );
        assert_eq!(
            o.build(&req, false).unwrap().body["thinking"]["type"],
            "disabled"
        );
    }

    #[test]
    fn tools_tool_results_and_history_map_natively() {
        let req = thin(
            json!({"model": "m", "messages": [
                {"role": "user", "content": "read a"},
                {"role": "assistant", "content": null, "tool_calls": [
                    {"id": "toolu_1", "type": "function", "function": {"name": "read", "arguments": "{\"path\":\"a\"}"}}]},
                {"role": "tool", "tool_call_id": "toolu_1", "content": "contents"},
                {"role": "tool", "tool_call_id": "toolu_1", "content": "more"},
                {"role": "user", "content": "thanks"}
            ], "tools": [{"type": "function", "function": {"name": "read", "parameters": {"type": "object"}}}],
               "parallel_tool_calls": false}),
            None,
        );
        let built = oauth().build(&req, false).unwrap();
        let m = &built.body["messages"];
        assert_eq!(m[1]["content"][0]["type"], "tool_use");
        assert_eq!(m[1]["content"][0]["name"], "read");
        assert_eq!(m[1]["content"][0]["input"]["path"], "a");
        assert_eq!(m[2]["role"], "user");
        assert_eq!(m[2]["content"][0]["type"], "tool_result");
        assert_eq!(
            m[2]["content"][1]["type"], "tool_result",
            "consecutive tool results merge"
        );
        assert_eq!(
            m[2]["content"][2]["text"], "thanks",
            "following user text merges too"
        );
        assert_eq!(built.body["tools"][0]["input_schema"]["type"], "object");
        assert_eq!(built.body["tool_choice"]["disable_parallel_tool_use"], true);
        let prefixed = oauth().build(&req, true).unwrap();
        assert_eq!(prefixed.body["tools"][0]["name"], "mcp__pi__read");
        assert_eq!(
            prefixed.body["messages"][1]["content"][0]["name"],
            "mcp__pi__read"
        );
        assert_eq!(prefixed.tool_names["mcp__pi__read"], "read");
    }

    #[test]
    fn thinking_replay_requires_signature_and_thinking_on() {
        let body = json!({"model": "m", "messages": [
            {"role": "user", "content": "q"},
            {"role": "assistant", "content": "a", "reasoning_content": "hmm", "reasoning_signature": "sig"},
            {"role": "user", "content": "q2"}
        ]});
        let with = oauth()
            .build(&thin(body.clone(), Some("low")), false)
            .unwrap();
        assert_eq!(with.body["messages"][1]["content"][0]["type"], "thinking");
        assert_eq!(with.body["messages"][1]["content"][0]["signature"], "sig");
        let without = oauth().build(&thin(body, None), false).unwrap();
        assert_eq!(without.body["messages"][1]["content"][0]["type"], "text");
    }

    #[test]
    fn reasoning_details_replay_before_text_and_tool_use() {
        let body = json!({"model": "m", "messages": [
            {"role": "user", "content": "q"},
            {"role": "assistant", "content": "a",
             "tool_calls": [{"id":"call_1","type":"function","function":{"name":"read","arguments":"{}"}}],
             "reasoning_details": [
                {"type": "reasoning.text", "text": "plan", "signature": "sig-1"},
                {"type": "reasoning.encrypted", "data": "blob"}
             ]
            },
            {"role": "tool", "tool_call_id": "call_1", "content": "ok"}
        ], "tools": [{"type": "function", "function": {"name": "read", "parameters": {"type": "object"}}}]});
        let built = oauth().build(&thin(body, Some("low")), false).unwrap();
        let blocks = built.body["messages"][1]["content"].as_array().unwrap();
        assert_eq!(blocks[0]["type"], "thinking");
        assert_eq!(blocks[0]["thinking"], "plan");
        assert_eq!(blocks[0]["signature"], "sig-1");
        assert_eq!(blocks[1]["type"], "redacted_thinking");
        assert_eq!(blocks[1]["data"], "blob");
        assert_eq!(blocks[2]["type"], "text");
        assert_eq!(blocks[3]["type"], "tool_use");
        assert_eq!(built.body["thinking"]["type"], "enabled");
    }

    #[test]
    fn tool_chain_without_signature_disables_thinking() {
        let body = json!({"model": "m", "messages": [
            {"role": "user", "content": "q"},
            {"role": "assistant", "content": null,
             "tool_calls": [{"id":"call_1","type":"function","function":{"name":"read","arguments":"{}"}}]
            },
            {"role": "tool", "tool_call_id": "call_1", "content": "ok"}
        ], "tools": [{"type": "function", "function": {"name": "read", "parameters": {"type": "object"}}}]});
        let built = oauth().build(&thin(body, Some("high")), false).unwrap();
        assert_eq!(built.body["thinking"]["type"], "disabled");
        assert!(!force_thinking_off(
            &thin(
                json!({"model":"m","messages":[{"role":"user","content":"q"}]}),
                Some("high")
            )
            .request,
            Some(Effort::High),
            &entry()
        ));
    }

    fn token(expired: bool) -> Token {
        Token {
            access_token: "sk-ant-oat01-x".into(),
            expires_at: Some(if expired { 1 } else { u64::MAX / 2 }),
            source: Default::default(),
            note: None,
        }
    }

    #[test]
    fn block_detection_rules() {
        let b = classify(400, "invalid_request_error".into(), "This credential is only authorized for use with Claude Code and cannot be used for other API requests.", None, &token(false), "claude-sonnet-4-6");
        assert!(matches!(b, OauthFailure::Blocked(_)));
        let b = classify(400, "invalid_request_error".into(), "Third-party apps now draw from your extra usage, not your plan limits. Add more at claude.ai/settings/usage and keep going.", None, &token(false), "claude-sonnet-4-6");
        assert!(matches!(b, OauthFailure::Blocked(ref r) if r.contains("third-party")));
        let b = classify(
            400,
            "invalid_request_error".into(),
            "Error",
            None,
            &token(false),
            "claude-sonnet-4-6",
        );
        assert!(
            matches!(b, OauthFailure::SuspectedBlock(_)),
            "novel non-Haiku 400 is a suspected block"
        );
        let e = classify(
            400,
            "invalid_request_error".into(),
            "Error",
            None,
            &token(false),
            "claude-haiku-4-5",
        );
        assert!(
            matches!(e, OauthFailure::Error(ref e) if e.status == 400),
            "bare Error on Haiku is a plain 400"
        );
        let e = classify(
            400,
            "invalid_request_error".into(),
            "messages.1.content: thinking block required",
            None,
            &token(false),
            "claude-sonnet-4-6",
        );
        assert!(
            matches!(e, OauthFailure::Error(ref e) if e.status == 400),
            "known validation pattern stays 400"
        );
        let b = classify(
            401,
            "authentication_error".into(),
            "invalid x-api-key",
            None,
            &token(false),
            "m",
        );
        assert!(matches!(b, OauthFailure::Blocked(_)));
        let e = classify(
            401,
            "authentication_error".into(),
            "expired",
            None,
            &token(true),
            "m",
        );
        assert!(matches!(e, OauthFailure::Error(ref e) if e.code == "vendor_login_required"));
        let b = classify(
            429,
            "rate_limit_error".into(),
            "x",
            Some(30),
            &token(false),
            "claude-sonnet-4-6",
        );
        match b {
            OauthFailure::Error(e) => {
                assert_eq!(
                    e.status, 429,
                    "first 429 is not a block without corroboration"
                );
                assert_eq!(e.code, "rate_limit_error");
            }
            other => panic!("expected rate_limited, got {other:?}"),
        }
        let b = classify(
            429,
            "rate_limit_error".into(),
            "Third-party apps now draw from your extra usage, not your plan limits.",
            Some(30),
            &token(false),
            "claude-sonnet-4-6",
        );
        assert!(
            matches!(b, OauthFailure::Blocked(_)),
            "named ToS wording corroborates a 429 as a block"
        );
        let e = classify(
            400,
            "invalid_request_error".into(),
            "this model is not allowed for API use outside the CLI",
            None,
            &token(false),
            "claude-sonnet-4-6",
        );
        assert!(
            matches!(e, OauthFailure::SuspectedBlock(_)),
            "bare 'model' substring is not a validation 400"
        );
        let e = classify(
            429,
            "rate_limit_error".into(),
            "rate limited",
            Some(10),
            &token(false),
            "claude-haiku-4-5",
        );
        assert!(
            matches!(e, OauthFailure::Error(ref e) if e.status == 429),
            "Haiku 429 never marks the direct path blocked"
        );
        let e = classify(
            400,
            "invalid_request_error".into(),
            "only authorized for use with Claude Code",
            None,
            &token(false),
            "claude-haiku-4-5",
        );
        assert!(
            matches!(e, OauthFailure::Error(ref e) if e.status == 400),
            "Haiku never returns Blocked"
        );
        let e = classify(
            429,
            "rate_limit_error".into(),
            "You have exceeded your usage limit",
            Some(30),
            &token(false),
            "m",
        );
        match e {
            OauthFailure::Error(e) => {
                assert_eq!(e.status, 402);
                assert_eq!(e.code, "insufficient_quota");
                assert_eq!(e.retry_after_seconds(), Some(30));
                assert!(e.no_retry());
                assert!(e.message.starts_with("quota exceeded"), "{}", e.message);
            }
            _ => panic!(),
        }
        let e = classify(
            529,
            "overloaded_error".into(),
            "Overloaded",
            None,
            &token(false),
            "m",
        );
        assert!(matches!(e, OauthFailure::Error(ref e) if e.status == 503));
        let e = classify(
            400,
            "invalid_request_error".into(),
            "prompt is too long: 250000 tokens > 200000 maximum",
            None,
            &token(false),
            "m",
        );
        assert!(
            matches!(e, OauthFailure::Error(ref e) if e.message.contains("prompt is too long"))
        );
    }

    fn feed(m: &mut StreamMapper, event: &str, data: Value) {
        m.accept(&SseEvent {
            event: Some(event.into()),
            data: data.to_string(),
        });
    }

    #[test]
    fn tool_json_over_the_cap_is_drift_and_does_not_grow() {
        let names = BTreeMap::new();
        let mut m = StreamMapper::new(&names, MapperCore::new(VENDOR, "", StreamBudgets::OFFLINE));
        feed(&mut m, "message_start", json!({"message": {"model": "m"}}));
        m.blocks.insert(
            1,
            Block::ToolUse {
                id: "call_1".into(),
                name: "read".into(),
                wire_name: "read".into(),
                json: "x".repeat(vendor::MAX_SSE_FRAME_BYTES - 10),
            },
        );
        let before = match m.blocks.get(1) {
            Some(Block::ToolUse { json, .. }) => json.len(),
            other => panic!("{other:?}"),
        };
        feed(
            &mut m,
            "content_block_delta",
            json!({"index":1,"delta":{"type":"input_json_delta","partial_json":"01234567890"}}),
        );
        let after = match m.blocks.get(1) {
            Some(Block::ToolUse { json, .. }) => json.len(),
            other => panic!("{other:?}"),
        };
        assert_eq!(after, before);
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

    fn kinds(m: &StreamMapper) -> Vec<&'static str> {
        m.core
            .pending
            .iter()
            .map(|e| match e {
                ThinEvent::Started { .. } => "started",
                ThinEvent::TextDelta(_) => "text",
                ThinEvent::Usage(_) => "usage",
                ThinEvent::Finish(_) => "finish",
                ThinEvent::Error(_) => "error",
                _ => "other",
            })
            .collect()
    }

    /// `Started` carries the model Anthropic served and the fallback tag the route reports
    /// as `x-thickapi-fallback` (SPEC §19.15 R4).
    #[test]
    fn started_names_the_served_model_and_the_fallback() {
        let names = BTreeMap::new();
        let core = MapperCore::new(VENDOR, "", StreamBudgets::OFFLINE)
            .with_serving("oauth", Some("bridge->oauth"));
        let mut m = StreamMapper::new(&names, core);
        feed(
            &mut m,
            "message_start",
            json!({"message": {"model": "claude-fable-5-1", "usage": {"input_tokens": 3}}}),
        );
        match m.core.pending.front() {
            Some(ThinEvent::Started {
                model,
                path,
                fallback,
                ..
            }) => assert_eq!(
                (model.as_str(), *path, *fallback),
                ("claude-fable-5-1", "oauth", Some("bridge->oauth"))
            ),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_lost_connection_is_an_error_unless_the_stop_reason_had_arrived() {
        let names = BTreeMap::new();
        let lost = || ThinError::vendor_down(VENDOR, "stream interrupted: reset");
        let start = json!({"message": {"model": "m", "usage": {"input_tokens": 3}}});
        let text = json!({"index": 0, "delta": {"type": "text_delta", "text": "half"}});
        let block = json!({"index": 0, "content_block": {"type": "text", "text": ""}});

        let mut cut =
            StreamMapper::new(&names, MapperCore::new(VENDOR, "", StreamBudgets::OFFLINE));
        feed(&mut cut, "message_start", start.clone());
        feed(&mut cut, "content_block_start", block.clone());
        feed(&mut cut, "content_block_delta", text.clone());
        SseMapper::on_interrupt(&mut cut, lost());
        assert_eq!(kinds(&cut), ["started", "text", "error"]);

        let mut whole =
            StreamMapper::new(&names, MapperCore::new(VENDOR, "", StreamBudgets::OFFLINE));
        feed(&mut whole, "message_start", start);
        feed(&mut whole, "content_block_start", block);
        feed(&mut whole, "content_block_delta", text);
        feed(
            &mut whole,
            "message_delta",
            json!({"delta": {"stop_reason": "end_turn"}, "usage": {"output_tokens": 1}}),
        );
        SseMapper::on_interrupt(&mut whole, lost());
        assert_eq!(kinds(&whole), ["started", "text", "usage", "finish"]);
    }

    /// Before `message_start` the fallback in `oauth/mod.rs` needs an `Err`, which `drive`
    /// makes of a failure queued ahead of `Started`.
    #[test]
    fn a_body_that_never_starts_is_drift_and_nothing_follows_a_stream_error() {
        let names = BTreeMap::new();
        let mut empty =
            StreamMapper::new(&names, MapperCore::new(VENDOR, "", StreamBudgets::OFFLINE));
        SseMapper::on_end(&mut empty);
        assert_eq!(kinds(&empty), ["error"]);
        assert!(!empty.core.started);

        // Content or an ending with no `message_start` must not open the turn by itself.
        let block = json!({"index": 0, "content_block": {"type": "text", "text": ""}});
        let stop = json!({"delta": {"stop_reason": "end_turn"}});
        for (event, data) in [
            ("message_stop", json!({})),
            ("content_block_start", block),
            ("message_delta", stop),
        ] {
            let core = MapperCore::new(VENDOR, "", StreamBudgets::OFFLINE);
            let mut m = StreamMapper::new(&names, core);
            feed(&mut m, event, data);
            assert_eq!(kinds(&m), ["error"], "{event}");
            assert!(!m.core.started, "{event}");
        }

        let mut m = StreamMapper::new(&names, MapperCore::new(VENDOR, "", StreamBudgets::OFFLINE));
        feed(&mut m, "message_start", json!({"message": {"model": "m"}}));
        feed(
            &mut m,
            "error",
            json!({"type": "error", "error": {"type": "overloaded_error", "message": "busy"}}),
        );
        feed(&mut m, "message_stop", json!({}));
        assert_eq!(kinds(&m), ["started", "error"]);
    }

    #[test]
    fn mid_stream_rate_limit_goes_through_limit_error() {
        let names = BTreeMap::new();
        let mut exhaust =
            StreamMapper::new(&names, MapperCore::new(VENDOR, "", StreamBudgets::OFFLINE));
        feed(
            &mut exhaust,
            "message_start",
            json!({"message": {"model": "m"}}),
        );
        feed(
            &mut exhaust,
            "error",
            json!({"type": "error", "error": {"type": "rate_limit_error", "message": "You have exceeded your usage limit"}}),
        );
        let err = exhaust.core.pending.iter().find_map(|e| match e {
            ThinEvent::Error(err) => Some(err),
            _ => None,
        });
        match err {
            Some(e) => {
                assert_eq!(e.status, 402, "{}", e.message);
                assert!(e.no_retry());
            }
            None => panic!("{:?}", kinds(&exhaust)),
        }

        let mut throttle =
            StreamMapper::new(&names, MapperCore::new(VENDOR, "", StreamBudgets::OFFLINE));
        feed(
            &mut throttle,
            "message_start",
            json!({"message": {"model": "m"}}),
        );
        feed(
            &mut throttle,
            "error",
            json!({"type": "error", "error": {"type": "rate_limit_error", "message": "slow down"}}),
        );
        let err = throttle.core.pending.iter().find_map(|e| match e {
            ThinEvent::Error(err) => Some(err),
            _ => None,
        });
        match err {
            Some(e) => {
                assert_eq!(e.status, 429, "{}", e.message);
                assert!(!e.no_retry());
            }
            None => panic!("{:?}", kinds(&throttle)),
        }
    }

    #[test]
    fn stream_mapper_two_tool_uses_and_usage() {
        let names: BTreeMap<String, String> = [("read".to_string(), "read".to_string())].into();
        let core = MapperCore::new(VENDOR, "", StreamBudgets::OFFLINE);
        let mut m = StreamMapper::new(&names, core);
        let frames = [
            (
                "message_start",
                json!({"type":"message_start","message":{"model":"claude-sonnet-4-6","usage":{"input_tokens":10,"cache_read_input_tokens":500,"cache_creation_input_tokens":20,"output_tokens":1}}}),
            ),
            (
                "content_block_start",
                json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
            ),
            (
                "content_block_delta",
                json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Reading."}}),
            ),
            (
                "content_block_stop",
                json!({"type":"content_block_stop","index":0}),
            ),
            (
                "content_block_start",
                json!({"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_1","name":"read","input":{}}}),
            ),
            (
                "content_block_delta",
                json!({"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"path\":"}}),
            ),
            (
                "content_block_delta",
                json!({"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"\"a\"}"}}),
            ),
            (
                "content_block_stop",
                json!({"type":"content_block_stop","index":1}),
            ),
            (
                "content_block_start",
                json!({"type":"content_block_start","index":2,"content_block":{"type":"tool_use","id":"toolu_2","name":"read","input":{}}}),
            ),
            (
                "content_block_delta",
                json!({"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"{\"path\":\"b\"}"}}),
            ),
            (
                "content_block_stop",
                json!({"type":"content_block_stop","index":2}),
            ),
            (
                "message_delta",
                json!({"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":42}}),
            ),
            ("message_stop", json!({"type":"message_stop"})),
        ];
        for (ev, data) in frames {
            m.accept(&SseEvent {
                event: Some(ev.into()),
                data: data.to_string(),
            });
        }
        assert!(m.core.started && m.core.done);
        assert_eq!(m.finish, Some(FinishReason::ToolCalls));
        let calls: Vec<_> = m
            .core
            .pending
            .iter()
            .filter(|e| matches!(e, ThinEvent::ToolCall { .. }))
            .collect();
        assert_eq!(calls.len(), 2);
        let u = m.usage().unwrap();
        assert_eq!(u.prompt_tokens, 530);
        assert_eq!(u.completion_tokens, 42);
        assert_eq!(u.prompt_tokens_details.unwrap().cached_tokens, 500);
    }

    /// #121: after `message_start`, a body that closes with no `stop_reason` and no
    /// `message_stop` is truncated. A `stop_reason` without `message_stop` is whole.
    #[test]
    fn a_clean_close_without_a_stop_reason_is_an_error_not_stop() {
        let names = BTreeMap::new();
        let start = json!({"message": {"model": "m", "usage": {"input_tokens": 3}}});
        let block = json!({"index": 0, "content_block": {"type": "text", "text": ""}});
        let text = json!({"index": 0, "delta": {"type": "text_delta", "text": "half"}});

        let mut cut =
            StreamMapper::new(&names, MapperCore::new(VENDOR, "", StreamBudgets::OFFLINE));
        feed(&mut cut, "message_start", start.clone());
        feed(&mut cut, "content_block_start", block.clone());
        feed(&mut cut, "content_block_delta", text.clone());
        SseMapper::on_end(&mut cut);
        assert_eq!(
            kinds(&cut),
            ["started", "text"],
            "left open for the shared rule"
        );
        let frames = [
            b"event: message_start
data: {\"type\":\"message_start\",\"message\":{\"model\":\"m\",\"usage\":{\"input_tokens\":3}}}

".to_vec(),
            b"event: content_block_start
data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}

".to_vec(),
            b"event: content_block_delta
data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"half\"}}

".to_vec(),
        ];
        let events = ClaudeDirectRequest::offline().replay("", &frames);
        assert!(
            !events.iter().any(|e| matches!(e, ThinEvent::Finish(_))),
            "{events:?}"
        );
        assert!(
            matches!(events.last(), Some(ThinEvent::Error(e)) if e.code == "vendor_down"),
            "{events:?}"
        );

        let mut whole =
            StreamMapper::new(&names, MapperCore::new(VENDOR, "", StreamBudgets::OFFLINE));
        feed(&mut whole, "message_start", start);
        feed(&mut whole, "content_block_start", block);
        feed(&mut whole, "content_block_delta", text);
        feed(
            &mut whole,
            "message_delta",
            json!({"delta": {"stop_reason": "end_turn"}, "usage": {"output_tokens": 1}}),
        );
        SseMapper::on_end(&mut whole);
        assert_eq!(kinds(&whole), ["started", "text", "usage", "finish"]);
    }
}
