//! Antigravity (`agy`) OAuth-direct adapter (SPEC §19.8, T3).
//!
//! `agy 1.2.4` talks to Google's Code Assist API, `POST
//! https://daily-cloudcode-pa.googleapis.com/v1internal:streamGenerateContent?alt=sse`, with a
//! Google OAuth bearer it keeps in the **Windows Credential Manager** (`gemini:antigravity`),
//! not in `~/.gemini/oauth_creds.json` (that file is gemini-cli's and was stale). Measured live
//! 2026-09-17:
//! - Envelope `{project, model, request: <Gemini GenerateContentRequest>, requestType: "agent",
//!   userAgent: "antigravity", requestId}`; without the `Client-Metadata`/`User-Agent` client
//!   headers `loadCodeAssist` answers `UNSUPPORTED_CLIENT` and `fetchAvailableModels` 403.
//! - `project` comes from `loadCodeAssist.cloudaicompanionProject` (`aicode-consumers` on the
//!   consumer tier); discovered once per process.
//! - Model ids are the Antigravity catalogue's (`gemini-3.8-flash-tiered`, `gemini-3.1-pro-low`,
//!   `claude-sonnet-4-6`, …). `agy models` shows effort-suffixed aliases; the backend wants the
//!   base id plus `generationConfig.thinkingConfig.thinkingLevel` (`low|medium|high`; `minimal`
//!   → 400, `thinkingBudget: 0` accepted for off).
//! - SSE `data: {"response": {candidates, usageMetadata, …}}`; thoughts stream as parts with
//!   `thought: true`; a `functionCall` part arrives whole with an `id` and a **`thoughtSignature`
//!   that must be replayed** on the next turn (400 otherwise). Unknown signatures are replaced
//!   by the documented `skip_thought_signature_validator` sentinel.
//! - Tool parameter schemas are a proto subset: `$schema`, `const` and array `type`s are
//!   rejected; `additionalProperties` and `default` are tolerated.
//! - Token life 1 h. `agy models` (≈5 s) performs the CLI's silent refresh; `fresh_token` runs it
//!   when < 5 min remain. The daemon never writes the credential.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Map, Value};

use crate::config::router::ThinConfig;
use crate::thin::capture::Tap;
use crate::thin::openai::{self, ChatMessage, Content, ContentPart, FinishReason, Usage};
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

pub const VENDOR: &str = "agy";
/// The host `agy 1.2.4` itself calls (`cli.log`); the community proxies use the prod host.
pub const DEFAULT_BASE_URL: &str = "https://daily-cloudcode-pa.googleapis.com";
const API_CLIENT: &str = "google-cloud-sdk vscode_cloudshelleditor/0.1";
/// Gemini's documented escape hatch when a function call's signature is not available.
pub const SKIP_SIGNATURE: &str = "skip_thought_signature_validator";
const FALLBACK_CLI_VERSION: &str = "1.2.4";
const REFRESH_WINDOW: Duration = Duration::from_secs(300);
/// `agy models` takes about 5 s; this is for a CLI that hangs.
const REFRESH_TIMEOUT: Duration = Duration::from_secs(60);
const SIGNATURE_CACHE_CAP: usize = 4096;
const LOGIN_HINT: &str = "run `agy` once and sign in";

/// What we remember about a function call we handed to the client, keyed by the `call_…` id the
/// client sees, so the next turn can replay Gemini's own id and thought signature.
#[derive(Debug, Clone, Default)]
pub struct CallMeta {
    pub name: String,
    pub gemini_id: Option<String>,
    pub signature: Option<String>,
}

/// Bounded id → [`CallMeta`] map (FIFO eviction). Shared by the adapter and its stream mappers.
#[derive(Default)]
pub struct SignatureCache {
    map: HashMap<String, CallMeta>,
    order: VecDeque<String>,
}

impl SignatureCache {
    pub fn insert(&mut self, id: String, meta: CallMeta) {
        if self.map.insert(id.clone(), meta).is_none() {
            self.order.push_back(id);
            while self.order.len() > SIGNATURE_CACHE_CAP {
                if let Some(old) = self.order.pop_front() {
                    self.map.remove(&old);
                }
            }
        }
    }

    pub fn get(&self, id: &str) -> Option<&CallMeta> {
        self.map.get(id)
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }
}

pub struct AgyAdapter {
    base_url: String,
    configured_project: Option<String>,
    pub cli: vendor::CliBacked,
    discovered_project: tokio::sync::Mutex<Option<String>>,
    signatures: Arc<Mutex<SignatureCache>>,
    pub state: Arc<VendorState>,
    /// Client, permits, tap and the `thin:` block (budgets, `reply_timeout_seconds`, #125).
    parts: VendorParts,
}

impl AgyAdapter {
    /// Record the wire exchange (`thickapi capture`); `serve` never sets one.
    pub fn set_tap(&mut self, tap: Tap) {
        self.parts.set_tap(tap);
    }

    /// Only `router.yaml`'s `thin:` block (`thin.vendors.agy` for this vendor's own keys) is
    /// read; fixture replay passes the defaults.
    pub fn new(thin: &ThinConfig, state: Arc<VendorState>) -> Self {
        let agy = &thin.vendors.agy;
        let credentials_path = agy.credentials_path.clone();
        let read_path = credentials_path.clone();
        Self {
            base_url: agy
                .base_url
                .clone()
                .unwrap_or_else(|| DEFAULT_BASE_URL.to_string()),
            configured_project: agy.project.clone(),
            // `agy models` performs the CLI's silent refresh; the daemon never writes the
            // credential.
            cli: vendor::CliBacked::new(
                CliToken::new(VENDOR, "agy", LOGIN_HINT)
                    .refresh_with(&["models"], REFRESH_TIMEOUT)
                    .window(REFRESH_WINDOW)
                    .cli_owns_store(credentials_path.is_none()),
                move || match &read_path {
                    Some(p) => creds::antigravity_token_from(p),
                    None => creds::antigravity_token(),
                },
                vendor::CliVersion::new("agy"),
                agy.client_version.clone(),
                FALLBACK_CLI_VERSION,
            ),
            discovered_project: tokio::sync::Mutex::new(None),
            signatures: Arc::new(Mutex::new(SignatureCache::default())),
            state,
            parts: VendorParts::new(thin),
        }
    }

    /// `antigravity/cli/<version> (aidev_client; os_type=<os>; arch=<arch>;
    /// auth_method=consumer)` — the shape `agy 1.4.7` sends. The backend gates models on it:
    /// under the older `antigravity/<version> <os>/<arch>` Gemini 4 Argon is `404` and absent
    /// from `fetchAvailableModels` (measured 2026-10-24); the CLI's `cl=<build>` is not needed.
    pub fn user_agent(&self) -> String {
        let os = match std::env::consts::OS {
            "windows" => "windows",
            "macos" => "darwin",
            other => other,
        };
        let arch = match std::env::consts::ARCH {
            "x86_64" => "amd64",
            "aarch64" => "arm64",
            other => other,
        };
        format!(
            "antigravity/cli/{} (aidev_client; os_type={os}; arch={arch}; auth_method=consumer)",
            self.cli.client_version()
        )
    }

    pub fn client_metadata() -> String {
        let platform = match std::env::consts::OS {
            "windows" => "WINDOWS",
            "macos" => "MACOS",
            "linux" => "LINUX",
            _ => "PLATFORM_UNSPECIFIED",
        };
        json!({ "ideType": "ANTIGRAVITY", "platform": platform, "pluginType": "GEMINI" })
            .to_string()
    }

    /// SPEC §19.5 thin apply, Antigravity. Absent → vendor default with thoughts; Off/minimal → budget 0.
    pub fn thinking_config(effort: Option<Effort>) -> Value {
        let probe = crate::config::models::ModelEntry {
            id: "subs/agy/probe".into(),
            backend: BackendId::Agy,
            cli_model: "gemini-3.8-flash".into(),
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
        let actions = profile(BackendId::Agy).thin_apply(effort, &probe);
        if actions
            .iter()
            .any(|a| matches!(a, VendorThinking::BudgetZero))
        {
            return json!({ "thinkingBudget": 0 });
        }
        if actions.iter().any(|a| matches!(a, VendorThinking::Omit)) {
            return json!({ "includeThoughts": true });
        }
        if let Some(level) = VendorThinking::config_level(&actions) {
            return json!({ "includeThoughts": true, "thinkingLevel": level });
        }
        json!({ "includeThoughts": true })
    }

    /// `(thinkingConfig, model)` for a row. A catalogue id ending in `-low|-medium|-high`
    /// (`gemini-4-argon-medium`, `claude-opus-5-5-medium`) is one backend model per level and
    /// the bare stem is `404`, so the requested level replaces the suffix and travels in
    /// `thinkingConfig` too, as `agy 1.4.7` sends it; with no level asked, the row's own suffix
    /// is the level; thinking off keeps the row's id with `thinkingBudget: 0`. A tiered id
    /// (`gemini-3.8-flash-tiered`) takes its level in `thinkingConfig` only (measured
    /// 2026-10-24).
    pub fn wire_thinking(row_model: &str, effort: Option<Effort>) -> (Value, String) {
        let mut thinking = Self::thinking_config(effort);
        // A row that cannot turn thinking off gets its lowest level instead of budget 0.
        if thinking.get("thinkingBudget").is_some()
            && !crate::thinking::agy_can_disable_thinking(row_model)
        {
            thinking = json!({ "includeThoughts": true, "thinkingLevel": "low" });
        }
        let stem = crate::thinking::strip_agy_effort_suffix(row_model);
        let Some(row_level) = row_model
            .strip_prefix(stem)
            .and_then(|s| s.strip_prefix('-'))
        else {
            return (thinking, row_model.to_string());
        };
        if let Some(level) = thinking.get("thinkingLevel").and_then(Value::as_str) {
            let model = format!("{stem}-{level}");
            return (thinking, model);
        }
        if thinking.get("thinkingBudget").is_none() {
            crate::thin::json_insert(&mut thinking, "thinkingLevel", json!(row_level));
        }
        (thinking, row_model.to_string())
    }

    /// The outbound Code Assist envelope. Pure so tests can pin the shape.
    pub fn build_request(req: &ThinRequest, project: &str, cache: &SignatureCache) -> Value {
        let chat = &req.request;
        let mut system_parts: Vec<Value> = Vec::new();
        let mut contents: Vec<Value> = Vec::new();
        // tool_call id → function name, for `functionResponse.name`.
        let mut names: HashMap<String, String> = HashMap::new();

        for m in &chat.messages {
            match m.role.as_str() {
                "system" | "developer" => {
                    let text = m.content.as_ref().map(Content::text).unwrap_or_default();
                    if !text.is_empty() {
                        system_parts.push(json!({ "text": text }));
                    }
                }
                "assistant" => {
                    let mut parts: Vec<Value> = Vec::new();
                    let text = m.content.as_ref().map(Content::text).unwrap_or_default();
                    if !text.is_empty() {
                        parts.push(json!({ "text": text }));
                    }
                    for call in m.tool_calls.iter().flatten() {
                        names.insert(call.id.clone(), call.function.name.clone());
                        let args: Value = serde_json::from_str(&call.function.arguments)
                            .ok()
                            .filter(Value::is_object)
                            .unwrap_or_else(|| json!({}));
                        let meta = cache.get(&call.id);
                        let gemini_id = meta
                            .and_then(|m| m.gemini_id.clone())
                            .unwrap_or_else(|| call.id.clone());
                        let signature = meta
                            .and_then(|m| m.signature.clone())
                            .unwrap_or_else(|| SKIP_SIGNATURE.to_string());
                        parts.push(json!({
                            "functionCall": { "name": call.function.name, "args": args, "id": gemini_id },
                            "thoughtSignature": signature,
                        }));
                    }
                    if !parts.is_empty() {
                        push_content(&mut contents, "model", parts);
                    }
                }
                "tool" => {
                    let id = m.tool_call_id.clone().unwrap_or_default();
                    let name = names
                        .get(&id)
                        .cloned()
                        .or_else(|| m.name.clone())
                        .unwrap_or_else(|| "tool".to_string());
                    let raw = m.content.as_ref().map(Content::text).unwrap_or_default();
                    let result: Value = serde_json::from_str(&raw)
                        .ok()
                        .filter(|v: &Value| v.is_object() || v.is_array())
                        .unwrap_or(Value::String(raw));
                    let gemini_id = cache
                        .get(&id)
                        .and_then(|m| m.gemini_id.clone())
                        .unwrap_or(id);
                    let part = json!({
                        "functionResponse": { "name": name, "id": gemini_id, "response": { "result": result } }
                    });
                    push_content(&mut contents, "user", vec![part]);
                }
                _ => {
                    let parts = user_parts(m);
                    if !parts.is_empty() {
                        push_content(&mut contents, "user", parts);
                    }
                }
            }
        }

        let mut request = json!({ "contents": contents });
        if !system_parts.is_empty() {
            crate::thin::json_insert(
                &mut request,
                "systemInstruction",
                json!({ "parts": system_parts }),
            );
        }
        let (thinking, model) = Self::wire_thinking(req.entry.wire_model(), req.effort);
        if !chat.tools.is_empty() {
            // Claude behind Antigravity: Google turns a proto `parameters` into JSON Schema that
            // Anthropic rejects whenever it holds an `anyOf` (`input_schema: JSON schema is
            // invalid`), while `parametersJsonSchema` reaches Anthropic as sent and Anthropic
            // validates it itself (measured 2026-10-24 on `claude-opus-5-5-low`).
            let json_schema = model.starts_with("claude-");
            let decls: Vec<Value> = chat
                .tools
                .iter()
                .map(|t| {
                    let mut d = json!({ "name": t.function.name });
                    if let Some(desc) = &t.function.description {
                        crate::thin::json_insert(&mut d, "description", json!(desc));
                    }
                    match t.function.parameters.as_ref() {
                        Some(schema) if json_schema => {
                            crate::thin::json_insert(
                                &mut d,
                                "parametersJsonSchema",
                                schema.clone(),
                            );
                        }
                        Some(schema) => {
                            if let Some(params) = sanitize_parameters(schema) {
                                crate::thin::json_insert(&mut d, "parameters", params);
                            }
                        }
                        None => {}
                    }
                    d
                })
                .collect();
            crate::thin::json_insert(
                &mut request,
                "tools",
                json!([{ "functionDeclarations": decls }]),
            );
            if let Some(cfg) = tool_config(chat.tool_choice.as_ref()) {
                crate::thin::json_insert(&mut request, "toolConfig", cfg);
            }
        }
        let mut generation = json!({ "thinkingConfig": thinking });
        if let Some(max) = chat.max_output_tokens() {
            crate::thin::json_insert(&mut generation, "maxOutputTokens", json!(max));
        }
        if let Some(t) = chat.temperature {
            crate::thin::json_insert(&mut generation, "temperature", json!(t));
        }
        if let Some(p) = chat.top_p {
            crate::thin::json_insert(&mut generation, "topP", json!(p));
        }
        match &chat.stop {
            Some(Value::String(s)) => {
                crate::thin::json_insert(&mut generation, "stopSequences", json!([s]))
            }
            Some(Value::Array(a)) if !a.is_empty() => {
                crate::thin::json_insert(&mut generation, "stopSequences", json!(a))
            }
            _ => {}
        }
        crate::thin::json_insert(&mut request, "generationConfig", generation);

        json!({
            "project": project,
            "model": model,
            "request": request,
            "requestType": "agent",
            "userAgent": "antigravity",
            "requestId": format!("agent-{}", req.request_id),
        })
    }

    fn headers(&self, token: &str) -> Vec<(&'static str, String)> {
        vec![
            ("authorization", format!("Bearer {token}")),
            ("content-type", "application/json".into()),
            ("user-agent", self.user_agent()),
            ("x-goog-api-client", API_CLIENT.into()),
            ("client-metadata", Self::client_metadata()),
        ]
    }

    fn url(&self, method: &str) -> String {
        format!(
            "{}/v1internal:{method}",
            self.base_url.trim_end_matches('/')
        )
    }

    /// `loadCodeAssist` → `cloudaicompanionProject`, once per process (config override wins).
    async fn project(&self, session: &VendorSession<'_>, token: &str) -> Result<String, ThinError> {
        if let Some(p) = &self.configured_project {
            return Ok(p.clone());
        }
        let mut slot = self.discovered_project.lock().await;
        if let Some(p) = slot.as_ref() {
            return Ok(p.clone());
        }
        let body = json!({ "metadata": {
            "ideType": "ANTIGRAVITY", "platform": "PLATFORM_UNSPECIFIED", "pluginType": "GEMINI" } });
        let mut rb = session.http().post(self.url("loadCodeAssist"));
        for (k, v) in self.headers(token) {
            rb = rb.header(k, v);
        }
        let state = session.state();
        let doc: Value = session
            .fetch_json("loadCodeAssist", rb.json(&body), |status, text| {
                let err = classify_body(status, text, None, "loadCodeAssist");
                state.mark_rejection_retry(status, err.code, text, err.retry_after_seconds());
                err
            })
            .await?;
        let project = doc
            .get("cloudaicompanionProject")
            .and_then(|v| {
                v.as_str()
                    .map(str::to_string)
                    .or_else(|| v.get("id").and_then(Value::as_str).map(str::to_string))
            })
            .ok_or_else(|| {
                ThinError::vendor_drift(
                    VENDOR,
                    format!(
                        "loadCodeAssist has no cloudaicompanionProject: {}",
                        redact(&doc.to_string())
                    ),
                )
            })?;
        tracing::info!(project = %project, "antigravity project discovered");
        *slot = Some(project.clone());
        Ok(project)
    }

    /// The Antigravity catalogue as the CLI prints it (`agy models`: `<id>\t<name>` lines), for
    /// `thickapi models check` (SPEC §10.3). The ids are the effort-suffixed CLI aliases; the
    /// check compares by family and version, so `gemini-3.8-flash-tiered` matches
    /// `gemini-3.8-flash-high`.
    pub async fn list_models(&self) -> Result<Vec<String>, ThinError> {
        let Some(exe) = crate::adapters::resolve_cli("agy") else {
            return Err(ThinError::vendor_down(VENDOR, "agy not found on PATH"));
        };
        let run = tokio::process::Command::new(exe)
            .arg("models")
            .stdin(std::process::Stdio::null())
            .kill_on_drop(true)
            .output();
        let out = match tokio::time::timeout(std::time::Duration::from_secs(90), run).await {
            Ok(Ok(out)) => out,
            Ok(Err(e)) => {
                return Err(ThinError::vendor_down(
                    VENDOR,
                    format!("agy models failed to start: {e}"),
                ))
            }
            Err(_) => return Err(ThinError::vendor_down(VENDOR, "agy models timed out (90s)")),
        };
        let text = String::from_utf8_lossy(&out.stdout);
        let ids: Vec<String> = text
            .lines()
            .filter_map(|line| {
                let id = line.split(['\t', ' ']).next()?.trim();
                (!id.is_empty() && id.chars().next().is_some_and(|c| c.is_ascii_alphanumeric()))
                    .then(|| id.to_string())
            })
            .collect();
        if ids.is_empty() {
            return Err(ThinError::vendor_drift(
                VENDOR,
                format!(
                    "agy models printed no `<id>\\t<name>` lines (exit {}): {}",
                    out.status,
                    redact(&String::from_utf8_lossy(&out.stderr))
                ),
            ));
        }
        Ok(ids)
    }

    pub fn session(&self) -> VendorSession<'_> {
        self.parts.session(VENDOR, &self.cli, &self.state)
    }
}

const WORDING: Wording = Wording {
    label: "antigravity",
    host: "cloudcode-pa",
    login_hint: LOGIN_HINT,
    headers_sent: REQUEST_HEADER_NAMES,
};

/// The Code Assist envelope (after project discovery, under the permit), the Gemini decoder
/// and Google's own status reading (`UNSUPPORTED_CLIENT`, `RetryInfo`).
impl VendorRequest for AgyAdapter {
    type Wire = Sse<GeminiMapper>;
    const WORDING: Wording = WORDING;

    async fn build(
        &self,
        session: &VendorSession<'_>,
        req: &ThinRequest,
        token: &Token,
    ) -> Result<reqwest::RequestBuilder, ThinError> {
        let project = self.project(session, &token.access_token).await?;
        let body = {
            let cache = self.signatures.lock().unwrap_or_else(|p| p.into_inner());
            Self::build_request(req, &project, &cache)
        };
        let mut rb = session
            .http()
            .post(format!("{}?alt=sse", self.url("streamGenerateContent")))
            .header("accept", "text/event-stream");
        for (k, v) in self.headers(&token.access_token) {
            rb = rb.header(k, v);
        }
        Ok(rb.json(&body))
    }

    fn wire(&self, model: &str, budgets: StreamBudgets) -> Self::Wire {
        Sse::new(GeminiMapper::new(model, self.signatures.clone(), budgets))
    }

    fn wire_params(&self, req: &ThinRequest) -> Option<String> {
        let row = req.entry.wire_model();
        let (cfg, model) = Self::wire_thinking(row, req.effort);
        let thinking = if let Some(b) = cfg.get("thinkingBudget").and_then(Value::as_u64) {
            format!("thinking_budget={b}")
        } else {
            format!(
                "thinking_level={}",
                cfg.get("thinkingLevel").and_then(Value::as_str)?
            )
        };
        // An effort-suffixed row sends a different id per level; name the one sent.
        if model != row {
            return Some(format!("{thinking},model={model}"));
        }
        Some(thinking)
    }

    fn budgets(&self, _req: &ThinRequest, cfg: &ThinConfig) -> StreamBudgets {
        StreamBudgets::from_config(&cfg.vendors.agy.budgets)
    }

    fn classify(&self, rejected: &Rejected<'_>) -> ThinError {
        classify_body(
            rejected.status,
            rejected.body,
            vendor::retry_after(rejected.headers).or_else(|| retry_delay_from(rejected.body)),
            rejected.model,
        )
    }
}

impl ModelAdapter for AgyAdapter {
    fn vendor(&self) -> &'static str {
        VENDOR
    }

    fn run<'a>(&'a self, req: &'a ThinRequest, tx: EventSender) -> AdapterFuture<'a> {
        Box::pin(async move { self.session().run(self, req, &tx).await })
    }
}

/// Append parts to `contents`, merging into the previous content when the role repeats (Gemini
/// wants one `user` content holding every parallel `functionResponse`).
fn push_content(contents: &mut Vec<Value>, role: &str, parts: Vec<Value>) {
    if let Some(last) = contents.last_mut() {
        if last["role"] == role {
            if let Some(existing) = last["parts"].as_array_mut() {
                existing.extend(parts);
                return;
            }
        }
    }
    contents.push(json!({ "role": role, "parts": parts }));
}

/// User message content → Gemini parts. Data-URL images become `inlineData`; remote URLs are
/// not fetchable by Code Assist, so they degrade to a text note.
fn user_parts(m: &ChatMessage) -> Vec<Value> {
    match &m.content {
        None => vec![],
        Some(Content::Text(t)) => {
            if t.is_empty() {
                vec![]
            } else {
                vec![json!({ "text": t })]
            }
        }
        Some(Content::Parts(parts)) => parts
            .iter()
            .filter_map(|p| match p {
                ContentPart::Text { text } if !text.is_empty() => Some(json!({ "text": text })),
                ContentPart::Text { .. } => None,
                ContentPart::ImageUrl { image_url } => Some(inline_image(&image_url.url)),
                ContentPart::Unknown => None,
            })
            .collect(),
    }
}

fn inline_image(url: &str) -> Value {
    if let Some(rest) = url.strip_prefix("data:") {
        if let Some((meta, data)) = rest.split_once(',') {
            let mime = meta.split(';').next().unwrap_or("image/png");
            if meta.contains(";base64") {
                return json!({ "inlineData": { "mimeType": mime, "data": data } });
            }
        }
    }
    json!({ "text": format!("[image: {url}]") })
}

fn tool_config(choice: Option<&Value>) -> Option<Value> {
    let choice = choice?;
    let mode = |m: &str| json!({ "functionCallingConfig": { "mode": m } });
    match choice {
        Value::String(s) => match s.as_str() {
            "none" => Some(mode("NONE")),
            "required" => Some(mode("ANY")),
            "auto" => Some(mode("AUTO")),
            _ => None,
        },
        Value::Object(o) => {
            let name = o
                .get("function")
                .and_then(|f| f.get("name"))
                .and_then(Value::as_str)?;
            Some(
                json!({ "functionCallingConfig": { "mode": "ANY", "allowedFunctionNames": [name] } }),
            )
        }
        _ => None,
    }
}

/// The `Type` names the proto parses (`google.ai.generativelanguage`'s enum). Measured
/// 2026-10-22: `float` is `400 Invalid value … (TYPE_ENUM)`, and a bare number is read as the
/// enum's index — `5` is `ARRAY`, which then demands `items`.
fn is_schema_type(name: &str) -> bool {
    matches!(
        name,
        "string" | "number" | "integer" | "boolean" | "array" | "object" | "null"
    )
}

/// JSON Schema → the proto subset Code Assist parses: an accepted keyword is copied, every
/// other key is dropped, because the wire rejects an unknown name outright (`400 Unknown name
/// "…"`). The accepted set is measured, not guessed — 2026-10-22, `agy 1.4.4`,
/// `gemini-3.8-flash`, through `subs/agy/gemini-3.8-flash`: accepted `type`, `format`, `title`,
/// `description`, `nullable`, `default`, `enum`, `example`, `items`, `properties`, `required`,
/// `anyOf`, `allOf`, `not`, `minimum`, `maximum`, `minLength`, `maxLength`, `pattern`,
/// `minItems`, `maxItems`, `minProperties`, `maxProperties`, `propertyOrdering`; rejected
/// `$ref`, `deprecated`, `readOnly`, `writeOnly`, `multipleOf`, `uniqueItems`, `if`/`then`/`else`,
/// `exclusiveMinimum`, `exclusiveMaximum`, plus `$schema`, `$id`, `$comment`, `examples`,
/// `additionalProperties`, `$defs`, `definitions`, `strict`.
///
/// Rewrites: `const` → `enum`, `type: [T, "null"]` → `type: T, nullable: true`, other type
/// arrays → `anyOf`, `oneOf` → `anyOf`, and an **integer** `exclusiveMinimum`/`exclusiveMaximum`
/// → the exact inclusive `minimum`/`maximum` (a non-integer exclusive bound has no inclusive
/// equivalent, so it is dropped rather than widened). `allOf`/`not` subschemas are sanitized
/// too, so a rejected name nested there cannot reach the wire.
///
/// **Structural rules the wire enforces** (measured the same day, one probe each, because Google
/// reports only the first it hits): a `required` name must be one of the `properties` beside it
/// (`required[1]: property is not defined`, top level and nested); `type: "array"` must carry
/// `items` (`items: missing field`); an `enum` value must match the declared scalar `type`
/// (`Invalid value … (TYPE_STRING)`) though `null` is allowed. Not enforced: `propertyOrdering`
/// names, a `required` with no `properties` beside it, `required: []`, dotted property names,
/// `minLength > maxLength`, duplicate `required` names. `{}` and `{"type":"object"}` are
/// accepted as a property or as `items`, so an empty object schema is **kept** here — dropping
/// it is what left `required` (and an array's `items`) naming something absent. Only a
/// declaration's own `parameters` omits an empty object ([`sanitize_parameters`]), which is
/// where "Gemini prefers the field absent" applies.
pub fn sanitize_schema(schema: &Value) -> Value {
    let Value::Object(obj) = schema else {
        return schema.clone();
    };
    let mut out = Map::new();
    for (k, v) in obj {
        match k.as_str() {
            "const" => {
                out.insert("enum".into(), json!([v]));
                if !obj.contains_key("type") {
                    out.insert(
                        "type".into(),
                        json!(match v {
                            Value::Number(_) => "number",
                            Value::Bool(_) => "boolean",
                            _ => "string",
                        }),
                    );
                }
            }
            "type" => match v {
                Value::Array(types) => {
                    let non_null: Vec<&Value> = types.iter().filter(|t| *t != "null").collect();
                    if non_null.len() < types.len() {
                        out.insert("nullable".into(), json!(true));
                    }
                    let known: Vec<&Value> = non_null
                        .iter()
                        .copied()
                        .filter(|t| t.as_str().is_some_and(is_schema_type))
                        .collect();
                    match known.as_slice() {
                        // Every name was unparseable: the schema says nothing rather than
                        // something the wire cannot read.
                        [] if !non_null.is_empty() => {}
                        [] => {
                            out.insert("type".into(), json!("string"));
                        }
                        [one] => {
                            out.insert("type".into(), (*one).clone());
                        }
                        many => {
                            out.insert(
                                "anyOf".into(),
                                json!(many
                                    .iter()
                                    .map(|t| json!({ "type": t }))
                                    .collect::<Vec<_>>()),
                            );
                        }
                    }
                }
                _ if v.as_str().is_some_and(is_schema_type) => {
                    out.insert("type".into(), v.clone());
                }
                // A name the enum does not parse fails the whole request, so it is dropped and
                // the rest of the schema stands.
                _ => {}
            },
            "properties" => {
                let props: Map<String, Value> = v
                    .as_object()
                    .map(|p| {
                        p.iter()
                            .map(|(name, s)| (name.clone(), sanitize_schema(s)))
                            .collect()
                    })
                    .unwrap_or_default();
                // An empty map is left off rather than sent as `properties: {}`: the accepted
                // form of an object schema with nothing in it is `{"type":"object"}`.
                if !props.is_empty() {
                    out.insert("properties".into(), Value::Object(props));
                }
            }
            "items" => {
                out.insert("items".into(), sanitize_schema(v));
            }
            "anyOf" | "oneOf" => {
                let variants: Vec<Value> = v
                    .as_array()
                    .map(|a| a.iter().map(sanitize_schema).collect())
                    .unwrap_or_default();
                if !variants.is_empty() {
                    out.insert("anyOf".into(), json!(variants));
                }
            }
            "allOf" => {
                let variants: Vec<Value> = v
                    .as_array()
                    .map(|a| a.iter().map(sanitize_schema).collect())
                    .unwrap_or_default();
                if !variants.is_empty() {
                    out.insert("allOf".into(), json!(variants));
                }
            }
            "not" => {
                out.insert("not".into(), sanitize_schema(v));
            }
            "title" | "description" | "format" | "default" | "example" | "enum" | "nullable"
            | "pattern" | "minLength" | "maxLength" | "minimum" | "maximum" | "minItems"
            | "maxItems" | "minProperties" | "maxProperties" | "propertyOrdering" => {
                out.insert(k.clone(), v.clone());
            }
            // Both are rebuilt after the loop: `required` from the properties that survived,
            // `exclusiveMinimum`/`exclusiveMaximum` from the object's own inclusive bound (a
            // later `minimum`/`maximum` key must not overwrite the translated one).
            "required" | "exclusiveMinimum" | "exclusiveMaximum" => {}
            _ => {}
        }
    }
    if out.get("type").and_then(Value::as_str) == Some("integer") {
        for (exclusive, inclusive, step) in [
            ("exclusiveMinimum", "minimum", 1_i64),
            ("exclusiveMaximum", "maximum", -1_i64),
        ] {
            let Some(bound) = obj.get(exclusive).and_then(Value::as_i64) else {
                continue;
            };
            let bound = bound + step;
            let tighter = match out.get(inclusive).and_then(Value::as_i64) {
                Some(inclusive_bound) => {
                    if step > 0 {
                        bound > inclusive_bound
                    } else {
                        bound < inclusive_bound
                    }
                }
                None => true,
            };
            if tighter {
                out.insert(inclusive.into(), json!(bound));
            }
        }
    }
    // `enum` is a `repeated string` in the proto: a number or a boolean entry fails the whole
    // request (`Invalid value … (TYPE_STRING)`) whatever the declared type, while `"1"` is
    // accepted for an integer, and the parser ignores a `null` entry. A scalar is therefore
    // carried over as its text, and a value the proto cannot carry at all is dropped; a list
    // cut to nothing becomes no `enum`.
    let declared_enum = out.get("enum").and_then(Value::as_array).cloned();
    if let Some(entries) = declared_enum {
        let kept: Vec<Value> = entries
            .iter()
            .filter_map(|entry| match entry {
                Value::String(_) => Some(entry.clone()),
                Value::Number(number) => Some(Value::String(number.to_string())),
                Value::Bool(flag) => Some(Value::String(flag.to_string())),
                _ => None,
            })
            .collect();
        if kept.is_empty() {
            out.remove("enum");
        } else {
            out.insert("enum".into(), json!(kept));
        }
    }
    // An array with no `items` fails the whole request (`items: missing field`, measured), and
    // `items` must itself be an object schema — `null` and the tuple form are not. An array of
    // anything is `items: {}`, which the wire accepts.
    if out.get("type").and_then(Value::as_str) == Some("array")
        && !out.get("items").is_some_and(Value::is_object)
    {
        out.insert("items".into(), json!({}));
    }
    // A `required` name the wire cannot find rejects the whole request, so the list is cut to
    // the properties that survived. With no `properties` beside it the wire does not check the
    // list (measured), so the client's own list is kept as written.
    match out.get("properties").and_then(Value::as_object) {
        Some(properties) => {
            let kept: Vec<Value> = obj
                .get("required")
                .and_then(Value::as_array)
                .map(|names| {
                    names
                        .iter()
                        .filter(|n| n.as_str().is_some_and(|n| properties.contains_key(n)))
                        .cloned()
                        .collect()
                })
                .unwrap_or_default();
            if !kept.is_empty() {
                out.insert("required".into(), json!(kept));
            }
        }
        None => {
            if let Some(required) = obj.get("required") {
                out.insert("required".into(), required.clone());
            }
        }
    }
    Value::Object(out)
}

/// The `parameters` field of one `functionDeclarations` entry. `None` leaves the field absent,
/// which is the one place Gemini prefers that to an empty object schema — a nested property or
/// an `items` keeps the same schema (measured 2026-10-22).
pub fn sanitize_parameters(schema: &Value) -> Option<Value> {
    let cleaned = sanitize_schema(schema);
    let empty = cleaned.get("type").and_then(Value::as_str) == Some("object")
        && cleaned
            .get("properties")
            .and_then(Value::as_object)
            .is_none_or(|p| p.is_empty())
        && cleaned.get("anyOf").is_none();
    (!empty).then_some(cleaned)
}

/// Google error body → (message, google.rpc status name).
fn parse_google_error(text: &str) -> (String, Option<String>) {
    let parsed: Option<Value> = serde_json::from_str(text).ok();
    let status = parsed.as_ref().and_then(|v| {
        v.get("error")
            .and_then(|e| e.get("status"))
            .and_then(Value::as_str)
            .map(str::to_string)
    });
    let message = parsed
        .as_ref()
        .and_then(|v| {
            v.get("error").and_then(|e| {
                e.get("message")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .or_else(|| e.as_str().map(str::to_string))
            })
        })
        .unwrap_or_else(|| text.chars().take(300).collect());
    (message, status)
}

/// `details[].retryDelay: "12s"` (google.rpc.RetryInfo) → seconds.
fn retry_delay_from(text: &str) -> Option<u64> {
    let v: Value = serde_json::from_str(text).ok()?;
    v.pointer("/error/details")?
        .as_array()?
        .iter()
        .find_map(|d| d.get("retryDelay").and_then(Value::as_str))
        .and_then(|s| s.trim_end_matches('s').parse::<f64>().ok())
        .map(|f| f.ceil().max(1.0) as u64)
}

const REQUEST_HEADER_NAMES: &str =
    "authorization, content-type, user-agent, x-goog-api-client, client-metadata";

fn classify_body(status: u16, text: &str, retry_after: Option<u64>, model: &str) -> ThinError {
    let (message, google_status) = parse_google_error(text);
    classify(
        status,
        &message,
        retry_after,
        model,
        google_status.as_deref(),
    )
}

fn classify(
    status: u16,
    message: &str,
    retry_after: Option<u64>,
    model: &str,
    google_status: Option<&str>,
) -> ThinError {
    if let Some(google @ ("UNSUPPORTED_CLIENT" | "FAILED_PRECONDITION")) = google_status {
        return ThinError::vendor_drift(
            VENDOR,
            format!(
                "{} {status} {google}: client headers sent ({REQUEST_HEADER_NAMES}): {}",
                WORDING.label,
                redact(message)
            ),
        );
    }
    // Google names its own drift (`UNSUPPORTED_CLIENT` above); a bare 403 from Code Assist is
    // a project or entitlement refusal, so it stays a permission error.
    vendor::classify_status(VENDOR, &WORDING, status, message, retry_after, model, false)
}

/// Code Assist SSE (`{"response": GenerateContentResponse}`) → thin events. Text and thoughts
/// stream; a `functionCall` part is complete on arrival, so it is emitted at once under a fresh
/// `call_…` id and its Gemini id + `thoughtSignature` are remembered for the next turn.
pub struct GeminiMapper {
    pub core: MapperCore,
    model: String,
    signatures: Arc<Mutex<SignatureCache>>,
    calls: usize,
    finish: Option<FinishReason>,
    usage: Option<Usage>,
}

impl GeminiMapper {
    pub fn new(
        model: &str,
        signatures: Arc<Mutex<SignatureCache>>,
        budgets: StreamBudgets,
    ) -> Self {
        Self {
            core: MapperCore::new(VENDOR, model, budgets),
            model: model.to_string(),
            signatures,
            calls: 0,
            finish: None,
            usage: None,
        }
    }

    pub fn accept(&mut self, data: &str) {
        let data = data.trim();
        if data.is_empty() || self.core.done {
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
            let message = err
                .get("message")
                .and_then(Value::as_str)
                .map(str::to_string)
                .unwrap_or_else(|| err.to_string());
            let status = err.get("code").and_then(Value::as_u64).unwrap_or(500) as u16;
            let google_status = err.get("status").and_then(Value::as_str);
            let err = if self.core.started {
                ThinError::vendor_down(
                    VENDOR,
                    format!("antigravity stream error: {}", redact(&message)),
                )
            } else {
                // The in-stream body carries the same `RetryInfo` a rejected request does.
                let wait = retry_delay_from(data);
                classify(status, &message, wait, &self.model, google_status)
            };
            self.core.fail(err);
            return;
        }
        let response = chunk.get("response").unwrap_or(&chunk);
        self.core.ensure_started();
        if let Some(u) = response.get("usageMetadata").filter(|u| u.is_object()) {
            self.usage = Some(vendor::usage_from(u, &USAGE_KEYS));
        }
        let Some(candidate) = response
            .get("candidates")
            .and_then(Value::as_array)
            .and_then(|c| c.first())
        else {
            return;
        };
        if let Some(parts) = candidate
            .pointer("/content/parts")
            .and_then(Value::as_array)
        {
            for part in parts {
                if let Some(fc) = part.get("functionCall") {
                    let name = fc
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    if name.is_empty() {
                        continue;
                    }
                    let args = fc.get("args").cloned().unwrap_or_else(|| json!({}));
                    let id = openai::new_call_id();
                    let meta = CallMeta {
                        name: name.clone(),
                        gemini_id: fc.get("id").and_then(Value::as_str).map(str::to_string),
                        signature: part
                            .get("thoughtSignature")
                            .and_then(Value::as_str)
                            .map(str::to_string),
                    };
                    self.signatures
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .insert(id.clone(), meta);
                    self.calls += 1;
                    self.core.emit(ThinEvent::ToolCall {
                        id,
                        name,
                        arguments: args.to_string(),
                    });
                    continue;
                }
                let Some(text) = part.get("text").and_then(Value::as_str) else {
                    continue;
                };
                if text.is_empty() {
                    continue;
                }
                if part.get("thought").and_then(Value::as_bool) == Some(true) {
                    self.core.emit(ThinEvent::ReasoningDelta {
                        text: text.to_string(),
                        signature: None,
                        details: None,
                    });
                } else {
                    self.core.emit(ThinEvent::TextDelta(text.to_string()));
                }
            }
        }
        if let Some(reason) = candidate.get("finishReason").and_then(Value::as_str) {
            match reason {
                "MAX_TOKENS" => {
                    self.finish = Some(FinishReason::Length);
                    self.finish();
                }
                "SAFETY" | "RECITATION" => {
                    self.finish = Some(FinishReason::ContentFilter);
                    self.finish();
                }
                "MALFORMED_FUNCTION_CALL" | "UNEXPECTED_TOOL_CALL" | "OTHER" => {
                    self.core.fail(
                        ThinError::vendor_down(
                            VENDOR,
                            format!("antigravity finishReason {reason}"),
                        )
                        .with_retry_after(2),
                    );
                }
                other => {
                    if !matches!(other, "STOP" | "FINISH_REASON_UNSPECIFIED") {
                        tracing::warn!(reason = other, "antigravity finishReason");
                    }
                    self.finish = Some(if self.calls > 0 {
                        FinishReason::ToolCalls
                    } else {
                        FinishReason::Stop
                    });
                    self.finish();
                }
            }
        }
    }

    pub fn finish(&mut self) {
        if self.core.done {
            return;
        }
        if let Some(u) = self.usage.take() {
            self.core.emit(ThinEvent::Usage(u));
        }
        let reason = self.finish.take().unwrap_or(if self.calls > 0 {
            FinishReason::ToolCalls
        } else {
            FinishReason::Stop
        });
        self.core.finish(ThinEvent::Finish(reason));
    }
}

impl SseMapper for GeminiMapper {
    fn core(&mut self) -> &mut MapperCore {
        &mut self.core
    }

    fn on_event(&mut self, event: &SseEvent) {
        self.accept(&event.data);
    }

    /// A `finishReason` ends the turn in `accept`. A body that closes cleanly without one is
    /// a truncated answer: left open, so the shared end-of-body rule fails it (#121).
    fn on_end(&mut self) {}
}

const USAGE_KEYS: UsageKeys = UsageKeys {
    prompt: "promptTokenCount",
    completion: &["candidatesTokenCount", "thoughtsTokenCount"],
    total: "totalTokenCount",
    cached: "/cachedContentTokenCount",
};

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::config::models::{default_input, ModelEntry, Transport};
    use crate::thin::openai::ChatRequest;
    use crate::thinking::Effort;
    use crate::types::BackendId;

    fn budgets() -> StreamBudgets {
        StreamBudgets::OFFLINE
    }

    fn offline() -> AgyAdapter {
        AgyAdapter::new(&ThinConfig::default(), Arc::default())
    }

    fn failure(m: &GeminiMapper) -> ThinError {
        match m.core.pending.back() {
            Some(ThinEvent::Error(e)) => e.clone(),
            other => panic!("expected a failure, got {other:?}"),
        }
    }

    fn entry() -> ModelEntry {
        ModelEntry {
            id: "subs/agy/gemini-3.8-flash".into(),
            backend: BackendId::Agy,
            cli_model: "gemini-3.8-flash-high".into(),
            context_window: 1_000_000,
            max_tokens: 65_536,
            thinking: true,
            cost: 0.0,
            name: "Gemini 3.8 Flash".into(),
            provider: None,
            transport: Transport::OauthDirect,
            vendor_model: Some("gemini-3.8-flash-tiered".into()),
            enabled: true,
            input: default_input(),
        }
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
    fn envelope_translates_messages_tools_and_thinking() {
        let mut cache = SignatureCache::default();
        cache.insert(
            "call_known".into(),
            CallMeta {
                name: "get_weather".into(),
                gemini_id: Some("call_164112".into()),
                signature: Some("SIG".into()),
            },
        );
        let r = req(
            json!({
                "model": "subs/agy/gemini-3.8-flash",
                "messages": [
                    {"role": "developer", "content": "be terse"},
                    {"role": "user", "content": [{"type":"text","text":"weather?"}]},
                    {"role": "assistant", "content": null, "tool_calls": [
                        {"id":"call_known","type":"function","function":{"name":"get_weather","arguments":"{\"city\":\"Brisbane\"}"}},
                        {"id":"call_lost","type":"function","function":{"name":"get_weather","arguments":"{\"city\":\"Perth\"}"}}
                    ]},
                    {"role": "tool", "tool_call_id": "call_known", "content": "{\"temp\":27}"},
                    {"role": "tool", "tool_call_id": "call_lost", "content": "hot"}
                ],
                "tools": [{"type":"function","function":{"name":"get_weather","description":"w","parameters":{
                    "$schema":"x","type":"object","properties":{"city":{"type":"string"},"mode":{"const":"c"},"n":{"type":["number","null"]}},"required":["city"],"additionalProperties":false}}}],
                "tool_choice": "required",
                "max_completion_tokens": 300,
                "temperature": 0.1,
                "stop": "END"
            }),
            Some("max"),
        );
        let body = AgyAdapter::build_request(&r, "aicode-consumers", &cache);
        assert_eq!(body["project"], "aicode-consumers");
        assert_eq!(body["model"], "gemini-3.8-flash-tiered");
        assert_eq!(body["requestType"], "agent");
        assert_eq!(body["userAgent"], "antigravity");
        assert_eq!(body["requestId"], "agent-chatcmpl-test");
        let rq = &body["request"];
        assert_eq!(rq["systemInstruction"]["parts"][0]["text"], "be terse");
        let contents = rq["contents"].as_array().unwrap();
        assert_eq!(contents.len(), 3, "{contents:?}");
        assert_eq!(contents[0]["role"], "user");
        assert_eq!(contents[0]["parts"][0]["text"], "weather?");
        assert_eq!(contents[1]["role"], "model");
        let calls = contents[1]["parts"].as_array().unwrap();
        assert_eq!(calls[0]["functionCall"]["id"], "call_164112");
        assert_eq!(calls[0]["functionCall"]["args"]["city"], "Brisbane");
        assert_eq!(calls[0]["thoughtSignature"], "SIG");
        assert_eq!(calls[1]["functionCall"]["id"], "call_lost");
        assert_eq!(calls[1]["thoughtSignature"], SKIP_SIGNATURE);
        // both tool results merged into one user content
        assert_eq!(contents[2]["role"], "user");
        let responses = contents[2]["parts"].as_array().unwrap();
        assert_eq!(responses.len(), 2);
        assert_eq!(responses[0]["functionResponse"]["name"], "get_weather");
        assert_eq!(responses[0]["functionResponse"]["id"], "call_164112");
        assert_eq!(
            responses[0]["functionResponse"]["response"]["result"]["temp"],
            27
        );
        assert_eq!(
            responses[1]["functionResponse"]["response"]["result"],
            "hot"
        );
        let decl = &rq["tools"][0]["functionDeclarations"][0];
        assert_eq!(decl["name"], "get_weather");
        let params = &decl["parameters"];
        assert!(params.get("$schema").is_none());
        assert!(params.get("additionalProperties").is_none());
        assert_eq!(params["properties"]["mode"]["enum"][0], "c");
        assert_eq!(params["properties"]["mode"]["type"], "string");
        assert_eq!(params["properties"]["n"]["type"], "number");
        assert_eq!(params["properties"]["n"]["nullable"], true);
        assert_eq!(rq["toolConfig"]["functionCallingConfig"]["mode"], "ANY");
        let gen = &rq["generationConfig"];
        assert_eq!(gen["maxOutputTokens"], 300);
        assert_eq!(gen["temperature"], 0.1);
        assert_eq!(gen["stopSequences"][0], "END");
        assert_eq!(gen["thinkingConfig"]["thinkingLevel"], "high");
        assert_eq!(gen["thinkingConfig"]["includeThoughts"], true);
    }

    #[test]
    fn thinking_config_matches_measured_column() {
        assert_eq!(
            AgyAdapter::thinking_config(None),
            json!({ "includeThoughts": true })
        );
        assert_eq!(
            AgyAdapter::thinking_config(Some(Effort::Off)),
            json!({ "thinkingBudget": 0 })
        );
        assert_eq!(
            AgyAdapter::thinking_config(Some(Effort::Minimal)),
            json!({ "thinkingBudget": 0 })
        );
        assert_eq!(
            AgyAdapter::thinking_config(Some(Effort::Medium))["thinkingLevel"],
            "medium"
        );
        assert_eq!(
            AgyAdapter::thinking_config(Some(Effort::XHigh))["thinkingLevel"],
            "high"
        );
    }

    #[test]
    fn suffixed_row_sends_the_level_in_the_id_and_tiered_row_does_not() {
        let w = AgyAdapter::wire_thinking;
        // Tiered: one id, level in thinkingConfig only.
        let (cfg, model) = w("gemini-3.8-flash-tiered", Some(Effort::Low));
        assert_eq!(model, "gemini-3.8-flash-tiered");
        assert_eq!(cfg["thinkingLevel"], "low");
        let (cfg, model) = w("gemini-3.8-flash-tiered", None);
        assert_eq!(model, "gemini-3.8-flash-tiered");
        assert!(cfg.get("thinkingLevel").is_none());
        // Suffixed: the requested level replaces the suffix and travels in both places.
        let (cfg, model) = w("gemini-4-argon-medium", Some(Effort::High));
        assert_eq!(model, "gemini-4-argon-high");
        assert_eq!(cfg["thinkingLevel"], "high");
        let (_, model) = w("claude-opus-5-5-medium", Some(Effort::Max));
        assert_eq!(model, "claude-opus-5-5-high");
        let (_, model) = w("claude-opus-5-5-medium", Some(Effort::Low));
        assert_eq!(model, "claude-opus-5-5-low");
        // No level asked: the row's own suffix is the level.
        let (cfg, model) = w("gemini-4-argon-medium", None);
        assert_eq!(model, "gemini-4-argon-medium");
        assert_eq!(
            cfg,
            json!({ "includeThoughts": true, "thinkingLevel": "medium" })
        );
        // Off: the row's id, budget 0, no level; Claude cannot turn thinking off, so it gets
        // its lowest level.
        let (cfg, model) = w("gemini-4-argon-medium", Some(Effort::Off));
        assert_eq!(model, "gemini-4-argon-medium");
        assert_eq!(cfg, json!({ "thinkingBudget": 0 }));
        let (cfg, model) = w("claude-opus-5-5-medium", Some(Effort::Off));
        assert_eq!(model, "claude-opus-5-5-low");
        assert_eq!(
            cfg,
            json!({ "includeThoughts": true, "thinkingLevel": "low" })
        );
    }

    #[test]
    fn claude_row_sends_the_client_schema_as_parameters_json_schema() {
        let schema = json!({
            "type": "object",
            "properties": { "id": { "anyOf": [{ "type": "string" }, { "type": "number" }] } },
            "additionalProperties": false
        });
        let body = json!({
            "model": "subs/agy/opus",
            "messages": [{ "role": "user", "content": "hi" }],
            "tools": [{ "type": "function", "function": { "name": "t", "parameters": schema } }]
        });
        let cache = SignatureCache::default();
        let mut opus = req(body.clone(), None);
        opus.entry.vendor_model = Some("claude-opus-5-5-medium".into());
        let sent = AgyAdapter::build_request(&opus, "p", &cache);
        let decl = &sent["request"]["tools"][0]["functionDeclarations"][0];
        assert_eq!(decl["parametersJsonSchema"], schema);
        assert!(decl.get("parameters").is_none());
        // Gemini rows keep the proto `parameters`, cut to the measured subset.
        let sent = AgyAdapter::build_request(&req(body, None), "p", &cache);
        let decl = &sent["request"]["tools"][0]["functionDeclarations"][0];
        assert!(decl.get("parametersJsonSchema").is_none());
        assert!(decl["parameters"].get("additionalProperties").is_none());
    }

    #[test]
    fn user_agent_is_the_1_4_7_cli_shape() {
        let ua = offline().user_agent();
        assert!(ua.starts_with("antigravity/cli/"), "{ua}");
        assert!(ua.contains(" (aidev_client; os_type="), "{ua}");
        assert!(ua.ends_with("; auth_method=consumer)"), "{ua}");
    }

    #[test]
    fn schema_is_cut_to_the_measured_proto_subset() {
        // Measured 2026-10-22 (`agy 1.4.4`, `gemini-3.8-flash`): an unknown name in a
        // `functionDeclarations` parameters object is `400 Unknown name "…"`, so every key
        // outside the accepted set must be gone before the body is built — including one
        // nested in `allOf`/`not`.
        let rejected = [
            "$schema",
            "$id",
            "$comment",
            "$ref",
            "examples",
            "additionalProperties",
            "$defs",
            "definitions",
            "strict",
            "deprecated",
            "readOnly",
            "writeOnly",
            "multipleOf",
            "uniqueItems",
            "if",
            "then",
            "else",
            "exclusiveMinimum",
            "exclusiveMaximum",
        ];
        let s = sanitize_schema(&json!({
            "$schema": "x",
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "a": {"type":"string","$ref":"#/properties/a","deprecated":true,"readOnly":true,
                      "writeOnly":true,"$comment":"c","examples":["e"],"pattern":"^a$","minLength":1,
                      "maxLength":3,"format":"date-time","title":"t","example":"e","default":"d",
                      "description":"d"},
                "b": {"type":"array","items":{"type":"string"},"minItems":1,"maxItems":3,"uniqueItems":true},
                "c": {"type":"number","maximum":9,"multipleOf":2,"exclusiveMinimum":3},
                "d": {"type":"integer","maximum":60000,"exclusiveMinimum":0},
                "e": {"type":"integer","minimum":5,"exclusiveMinimum":0},
                "f": {"type":"integer","maximum":5,"exclusiveMaximum":10},
                "g": {"allOf":[{"type":"integer","exclusiveMinimum":0,"exclusiveMaximum":10}]},
                "h": {"not":{"type":"string","exclusiveMinimum":0}},
                "i": {"type":"object","properties":{"z":{"type":"string"}},"if":{"type":"string"},
                      "then":{"type":"number"},"else":{"type":"number"},"$defs":{"d":{"type":"string"}}}
            },
            "required": ["a"],
            "propertyOrdering": ["a", "b"],
            "strict": true
        }));
        let text = s.to_string();
        for key in rejected {
            assert!(
                !text.contains(&format!("\"{key}\"")),
                "{key} survived: {text}"
            );
        }
        let props = &s["properties"];
        assert_eq!(props["a"]["pattern"], "^a$");
        assert_eq!(props["a"]["minLength"], 1);
        assert_eq!(props["a"]["maxLength"], 3);
        assert_eq!(props["a"]["format"], "date-time");
        assert_eq!(props["a"]["example"], "e");
        assert_eq!(props["a"]["default"], "d");
        assert_eq!(props["b"]["minItems"], 1);
        assert_eq!(props["b"]["maxItems"], 3);
        // An integer exclusive bound becomes the exact inclusive one; `c` is a number, so its
        // bound has no inclusive equivalent and is dropped rather than widened.
        assert_eq!(props["d"]["minimum"], 1);
        assert_eq!(props["d"]["maximum"], 60000);
        assert_eq!(
            props["e"]["minimum"], 5,
            "the tighter of the two bounds wins"
        );
        assert_eq!(props["f"]["maximum"], 5);
        assert_eq!(props["g"]["allOf"][0]["minimum"], 1);
        assert_eq!(props["g"]["allOf"][0]["maximum"], 9);
        assert_eq!(props["h"]["not"]["type"], "string");
        assert!(
            props["c"].get("minimum").is_none(),
            "a non-integer exclusive bound is dropped, not widened: {props}"
        );
        assert_eq!(props["c"]["maximum"], 9);
        assert!(props["i"].get("propertyOrdering").is_none());
        assert_eq!(s["propertyOrdering"], json!(["a", "b"]));
        assert!(s.get("additionalProperties").is_none());
    }

    #[test]
    fn an_array_gets_items_and_a_type_is_readable() {
        // `type: "array"` with no `items` is `items: missing field`; `items` must be an object
        // schema (`null` and the tuple form are not). Both measured 2026-10-22.
        let s = sanitize_schema(&json!({
            "type":"object",
            "properties":{
                "bare":{"type":"array"},
                "null_items":{"type":"array","items":null},
                "tuple_items":{"type":"array","items":[{"type":"string"}]},
                "typed":{"type":"array","items":{"type":"string"}}
            }
        }));
        let props = &s["properties"];
        assert_eq!(props["bare"]["items"], json!({}));
        assert_eq!(props["null_items"]["items"], json!({}));
        assert_eq!(props["tuple_items"]["items"], json!({}));
        assert_eq!(props["typed"]["items"], json!({"type":"string"}));
        // A name the `Type` enum cannot parse fails the whole request, so it is dropped and the
        // rest of the schema stands. A bare number is read as the enum's index, not as a name.
        assert_eq!(sanitize_schema(&json!({"type":"float"})), json!({}));
        assert_eq!(sanitize_schema(&json!({"type":5})), json!({}));
        assert_eq!(
            sanitize_schema(&json!({"type":["string","float"]})),
            json!({"type":"string"})
        );
        assert_eq!(
            sanitize_schema(&json!({"type":["float","decimal"]})),
            json!({})
        );
        assert_eq!(
            sanitize_schema(&json!({"type":["number","null"]})),
            json!({"nullable":true,"type":"number"})
        );
        // `type: ["null"]` keeps the long-standing permissive rewrite.
        assert_eq!(
            sanitize_schema(&json!({"type":["null"]})),
            json!({"nullable":true,"type":"string"})
        );
    }

    #[test]
    fn enum_entries_are_carried_as_text() {
        // `enum` is a `repeated string` (measured 2026-10-22): a number or a boolean entry is
        // `Invalid value … (TYPE_STRING)` whatever the declared type, `"1"` is accepted for an
        // integer, and the parser ignores a `null` entry. A scalar becomes its text.
        assert_eq!(
            sanitize_schema(&json!({"type":"string","enum":["a",1,true,null]})),
            json!({"type":"string","enum":["a","1","true"]})
        );
        assert_eq!(
            sanitize_schema(&json!({"type":"integer","enum":[1,2]})),
            json!({"type":"integer","enum":["1","2"]})
        );
        assert_eq!(
            sanitize_schema(&json!({"type":"number","enum":[1.5]})),
            json!({"type":"number","enum":["1.5"]})
        );
        // Nothing the proto can carry is no `enum` at all, rather than an empty or invalid one.
        assert_eq!(
            sanitize_schema(&json!({"enum":[null, {"a":1}, [1]]})),
            json!({})
        );
        // `const` builds its enum on the same path, so it is stringified too.
        assert_eq!(
            sanitize_schema(&json!({"const":5})),
            json!({"type":"number","enum":["5"]})
        );
        assert_eq!(
            sanitize_schema(&json!({"const":"c"})),
            json!({"type":"string","enum":["c"]})
        );
        assert_eq!(
            sanitize_schema(&json!({"const":false})),
            json!({"type":"boolean","enum":["false"]})
        );
    }

    #[test]
    fn empty_object_schema_is_omitted_as_parameters_and_kept_nested() {
        // A nested `{"type":"object"}` (or `{}`) is accepted by the wire, and dropping it was
        // itself the bug: `required` and an array's `items` were left naming something absent
        // (`required[1]: property is not defined`, `items: missing field`, measured 2026-10-22).
        let s = sanitize_schema(&json!({
            "type": "object",
            "properties": {
                "answers": {"type":"object"},
                "list": {"type":"array","items":{"type":"object"}},
                "empty": {"type":"object","properties":{}}
            },
            "required": ["answers", "list", "empty"]
        }));
        assert_eq!(s["properties"]["answers"], json!({"type":"object"}));
        assert_eq!(s["properties"]["list"]["items"], json!({"type":"object"}));
        assert_eq!(s["properties"]["empty"], json!({"type":"object"}));
        assert_eq!(s["required"], json!(["answers", "list", "empty"]));
        // Only the declaration's own `parameters` omits it.
        assert!(sanitize_parameters(&json!({"type":"object","properties":{}})).is_none());
        assert!(sanitize_parameters(&json!({"type":"object"})).is_none());
        assert!(sanitize_parameters(
            &json!({"type":"object","properties":{"a":{"type":"string"}}})
        )
        .is_some());
        let s = sanitize_schema(
            &json!({"type":"object","properties":{"a":{"oneOf":[{"type":"string"},{"type":"number"}]}}}),
        );
        assert_eq!(s["properties"]["a"]["anyOf"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn required_is_cut_to_the_properties_that_survived() {
        // `t3_pending_request_respond` declares `required: [requestId, answers]` with `answers`
        // a free-form object; the wire answered `required[1]: property is not defined`.
        let t3 = json!({
            "type": "object",
            "properties": {
                "threadId": {"anyOf":[{"type":"string"},{"type":"null"}]},
                "requestId": {"type":"string"},
                "answers": {"type":"object"}
            },
            "required": ["requestId", "answers"],
            "additionalProperties": true
        });
        let s = sanitize_schema(&t3);
        assert_eq!(s["required"], json!(["requestId", "answers"]));
        assert!(s.get("additionalProperties").is_none());
        // A name the client never declared beside `properties` cannot ride along, and a list cut
        // to nothing is dropped rather than sent empty.
        let s = sanitize_schema(&json!({
            "type":"object","properties":{"a":{"type":"string"}},"required":["a","gone","gone2"]
        }));
        assert_eq!(s["required"], json!(["a"]));
        let s = sanitize_schema(&json!({
            "type":"object","properties":{"a":{"type":"string"}},"required":["gone"]
        }));
        assert!(s.get("required").is_none(), "{s}");
        // Without `properties` the wire does not check the list, so it is kept as written.
        let s = sanitize_schema(&json!({"type":"object","required":["gone"]}));
        assert_eq!(s["required"], json!(["gone"]));
        // The same cut happens in a nested object and inside an `anyOf` variant.
        let s = sanitize_schema(&json!({
            "type":"object",
            "properties":{"outer":{"type":"object","properties":{"a":{"type":"string"}},"required":["a","gone"]}}
        }));
        assert_eq!(s["properties"]["outer"]["required"], json!(["a"]));
    }

    #[test]
    fn mapper_streams_thought_text_call_usage_and_remembers_signature() {
        let cache = Arc::new(Mutex::new(SignatureCache::default()));
        let mut m = GeminiMapper::new("gemini-3.8-flash-tiered", cache.clone(), budgets());
        m.accept(r#"{"response":{"candidates":[{"content":{"role":"model","parts":[{"thought":true,"text":"Need the tool."}]}}],"usageMetadata":{"promptTokenCount":58,"totalTokenCount":58}}}"#);
        m.accept(r#"{"response":{"candidates":[{"content":{"role":"model","parts":[{"text":"Checking"}]}}]}}"#);
        m.accept(r#"{"response":{"candidates":[{"content":{"role":"model","parts":[{"thoughtSignature":"SIG1","functionCall":{"name":"get_weather","args":{"city":"Brisbane"},"id":"call_164112"}}]}}],"usageMetadata":{"promptTokenCount":58,"candidatesTokenCount":17,"thoughtsTokenCount":5,"totalTokenCount":80}}}"#);
        m.accept(r#"{"response":{"candidates":[{"content":{"role":"model","parts":[{"text":""}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":58,"candidatesTokenCount":17,"thoughtsTokenCount":5,"totalTokenCount":80}}}"#);
        assert!(m.core.done);
        let mut call_id = String::new();
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
                } => {
                    call_id = id.clone();
                    format!("call:{name}:{arguments}")
                }
                ThinEvent::Usage(u) => format!(
                    "usage:{}:{}:{}",
                    u.prompt_tokens, u.completion_tokens, u.total_tokens
                ),
                ThinEvent::Finish(f) => format!("finish:{f:?}"),
                other => format!("{other:?}"),
            })
            .collect();
        assert_eq!(
            kinds,
            vec![
                "started",
                "reason:Need the tool.",
                "text:Checking",
                "call:get_weather:{\"city\":\"Brisbane\"}",
                "usage:58:22:80",
                "finish:ToolCalls",
            ]
        );
        assert!(
            call_id.starts_with("call_") && call_id.len() == 29,
            "{call_id}"
        );
        let cache = cache.lock().unwrap();
        let meta = cache.get(&call_id).unwrap();
        assert_eq!(meta.gemini_id.as_deref(), Some("call_164112"));
        assert_eq!(meta.signature.as_deref(), Some("SIG1"));
        assert_eq!(meta.name, "get_weather");
    }

    #[test]
    fn mapper_error_before_start_is_classified() {
        let cache = Arc::new(Mutex::new(SignatureCache::default()));
        let mut m = GeminiMapper::new("x", cache, budgets());
        m.accept(
            r#"{"error":{"code":429,"message":"Quota exceeded","status":"RESOURCE_EXHAUSTED"}}"#,
        );
        let err = failure(&m);
        assert_eq!(err.status, 402);
        assert_eq!(err.code, "insufficient_quota");
        assert!(err.no_retry());
        assert!(err.message.starts_with("quota exceeded"), "{}", err.message);
        assert!(!m.core.started);

        // The same status with a short `RetryInfo` is a throttle: retryable, the vendor's wait.
        let cache = Arc::new(Mutex::new(SignatureCache::default()));
        let mut m = GeminiMapper::new("x", cache, budgets());
        m.accept(
            r#"{"error":{"code":429,"message":"Quota exceeded for quota metric 'requests' and limit 'PerMinute'","status":"RESOURCE_EXHAUSTED","details":[{"@type":"type.googleapis.com/google.rpc.RetryInfo","retryDelay":"7s"}]}}"#,
        );
        let err = failure(&m);
        assert_eq!(
            (err.status, err.no_retry(), err.retry_after_seconds()),
            (429, false, Some(7))
        );
        assert!(
            !err.message.starts_with("quota exceeded"),
            "{}",
            err.message
        );
    }

    #[test]
    fn classify_and_retry_delay() {
        let text = r#"{"error":{"code":429,"message":"Resource exhausted","status":"RESOURCE_EXHAUSTED","details":[{"@type":"type.googleapis.com/google.rpc.RetryInfo","retryDelay":"12.5s"}]}}"#;
        assert_eq!(retry_delay_from(text), Some(13));
        assert_eq!(parse_google_error(text).0, "Resource exhausted");
        let e = classify(
            404,
            "Requested entity was not found.",
            None,
            "gemini-3.8-flash-low",
            None,
        );
        assert_eq!(e.status, 404);
        assert_eq!(e.code, "model_not_found");
        let e = classify(401, "invalid token", None, "m", None);
        assert_eq!(e.code, "vendor_login_required");
        let e = classify(403, "The caller does not have permission", None, "m", None);
        assert_eq!(e.status, 403);
        let e = classify(
            403,
            "client not supported",
            None,
            "m",
            Some("UNSUPPORTED_CLIENT"),
        );
        assert_eq!(e.status, 502);
        assert_eq!(e.code, "vendor_drift");
        assert!(e.message.contains("client-metadata"), "{}", e.message);
        let e = classify(400, "precondition", None, "m", Some("FAILED_PRECONDITION"));
        assert_eq!(e.code, "vendor_drift");
    }

    #[test]
    fn finish_reason_malformed_is_vendor_down_safety_is_content_filter() {
        let cache = Arc::new(Mutex::new(SignatureCache::default()));
        let mut m = GeminiMapper::new("x", cache.clone(), budgets());
        m.accept(r#"{"response":{"candidates":[{"content":{"role":"model","parts":[{"text":"no"}]},"finishReason":"MALFORMED_FUNCTION_CALL"}]}}"#);
        let err = failure(&m);
        assert_eq!(err.status, 503);
        assert_eq!(err.retry_after_seconds(), Some(2));
        assert!(!m
            .core
            .pending
            .iter()
            .any(|e| matches!(e, ThinEvent::Finish(FinishReason::Stop))));

        let mut m = GeminiMapper::new("x", cache, budgets());
        m.accept(r#"{"response":{"candidates":[{"content":{"role":"model","parts":[{"text":"no"}]},"finishReason":"SAFETY"}]}}"#);
        assert!(m
            .core
            .pending
            .iter()
            .any(|e| matches!(e, ThinEvent::Finish(FinishReason::ContentFilter))));
    }

    #[test]
    fn signature_cache_evicts_fifo() {
        let mut c = SignatureCache::default();
        for i in 0..(SIGNATURE_CACHE_CAP + 10) {
            c.insert(format!("call_{i}"), CallMeta::default());
        }
        assert_eq!(c.len(), SIGNATURE_CACHE_CAP);
        assert!(c.get("call_0").is_none());
        assert!(c
            .get(&format!("call_{}", SIGNATURE_CACHE_CAP + 9))
            .is_some());
    }

    #[test]
    fn data_url_image_becomes_inline_data() {
        let p = inline_image("data:image/png;base64,AAAA");
        assert_eq!(p["inlineData"]["mimeType"], "image/png");
        assert_eq!(p["inlineData"]["data"], "AAAA");
        assert!(inline_image("https://x/y.png")["text"].is_string());
    }

    /// #121: text with no `finishReason`, then a clean close, is a truncated answer.
    #[test]
    fn a_clean_close_without_a_finish_reason_is_an_error_not_stop() {
        let cut = b"data: {\"response\":{\"candidates\":[{\"content\":{\"role\":\"model\",\"parts\":[{\"text\":\"half\"}]}}]}}

";
        let events = offline().replay("x", &[cut.to_vec()]);
        assert!(
            !events.iter().any(|e| matches!(e, ThinEvent::Finish(_))),
            "{events:?}"
        );
        assert!(
            matches!(events.last(), Some(ThinEvent::Error(e)) if e.code == "vendor_down"),
            "{events:?}"
        );

        let whole = b"data: {\"response\":{\"candidates\":[{\"content\":{\"role\":\"model\",\"parts\":[{\"text\":\"x\"}]},\"finishReason\":\"STOP\"}]}}

";
        let events = offline().replay("x", &[whole.to_vec()]);
        assert!(matches!(
            events.last(),
            Some(ThinEvent::Finish(FinishReason::Stop))
        ));
    }
}
