//! What every OAuth-direct adapter does the same way (SPEC §19.8).
//!
//! A vendor file keeps what is its own: the request it builds, the wire it decodes into
//! [`ThinEvent`]s, and its status table. Everything that makes a turn *survive* lives here
//! once, so a fix reaches every vendor:
//! - [`drive`] — the response loop: quiet-event budgets, client disconnect, reply frames
//!   for a duplex wire, and the pre-/post-`Started` error rule.
//! - [`Sse`] — SSE framing with a frame cap, for the three vendors that speak it.
//! - [`MapperCore`] — the `Started` / `pending` / `done` bookkeeping every mapper repeats.
//! - [`CliToken`] — single-flight token refresh through the vendor's own CLI.
//! - [`retry_after`] and [`limit_error`] — one reading of "slow down" versus "you are out".
//! - [`VendorSession::run`] — one request's life, start to end, for a vendor that implements
//!   [`VendorRequest`]; [`VendorSession::fetch_json`] for its non-streaming exchanges.
//! - [`usage_from`] and [`rate_limit_advisory`] — one reading each of usage and rate-limit
//!   headers, keyed per vendor.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use bytes::Bytes;
use futures_util::StreamExt;
use serde_json::Value;

use crate::config::router::{OauthBudgets, ThinConfig};
use crate::thin::capture::{self, Tap};
use crate::thin::openai::{PromptTokensDetails, Usage};
use crate::thin::{EventSender, ThinError, ThinEvent, ThinRequest};

use super::creds::{CredError, Token};
use super::sse::{SseEvent, SseParser};
use super::state::{redact, rejection_excerpt, VendorState};

/// Replies a duplex wire (Cursor) writes back while the response streams.
pub type ReplySender = tokio::sync::mpsc::Sender<Result<Bytes, std::io::Error>>;

/// How long a stream may go without producing a [`ThinEvent`]. Bytes that map to no event
/// (SSE comments, pings, Connect heartbeats) do not count: a vendor that keeps a dead turn
/// warm would otherwise hold the client for as long as it likes (2026-09-20, Cursor, 308 s).
#[derive(Debug, Clone, Copy)]
pub struct StreamBudgets {
    pub first_event: Duration,
    pub idle: Duration,
}

impl StreamBudgets {
    /// Fixture replay has no clock; the budgets are carried and never read.
    pub const OFFLINE: Self = Self {
        first_event: Duration::MAX,
        idle: Duration::MAX,
    };

    /// `0` in `router.yaml` turns a budget off rather than making it instant.
    pub fn seconds(s: u64) -> Duration {
        match s {
            0 => Duration::MAX,
            s => Duration::from_secs(s),
        }
    }

    /// A vendor's `thin.vendors.<id>.budgets` (`first_event_seconds`, `idle_seconds`).
    pub fn from_config(budgets: &OauthBudgets) -> Self {
        Self {
            first_event: Self::seconds(budgets.first_event_seconds),
            idle: Self::seconds(budgets.idle_seconds),
        }
    }

    /// The idle budget raised to at least `floor`: a wire that delivers a piece of the answer
    /// whole (Grok's tool calls) is silent for as long as generating it takes, and that
    /// silence is not the vendor going away. The first-event budget is left alone: the
    /// floor is for silence mid-answer (#119), and the route's `start_deadline_seconds`
    /// bounds the wait for the first event anyway (audit R9, #129).
    pub fn with_idle_floor(self, floor: Duration) -> Self {
        Self {
            first_event: self.first_event,
            idle: self.idle.max(floor),
        }
    }
}

/// The bound on a non-streaming vendor exchange: the Claude probe, a `list_models` call,
/// agy's `loadCodeAssist`, and the body read of a rejected response
/// (`thin.reply_timeout_seconds`, SPEC §19.8; audit R5, #125). The four clients set only
/// `connect_timeout`, so without this one stalled connection held its caller — and, for the
/// probe, an `in_flight` permit — for good.
pub fn reply_timeout(thin: &ThinConfig) -> Duration {
    StreamBudgets::seconds(thin.reply_timeout_seconds)
}

/// Run one non-streaming exchange under `limit`; past it the answer is `503 vendor_down`
/// naming `what`.
pub async fn bounded<T>(
    vendor: &'static str,
    what: &str,
    limit: Duration,
    fut: impl std::future::Future<Output = Result<T, ThinError>>,
) -> Result<T, ThinError> {
    match tokio::time::timeout(limit, fut).await {
        Ok(result) => result,
        Err(_) => Err(ThinError::vendor_down(
            vendor,
            format!("{what} timed out ({}s)", limit.as_secs()),
        )),
    }
}

/// The body of a rejected response, read under `limit`. A body that stalls reads as empty:
/// the status alone still classifies the rejection.
pub async fn error_body(response: reqwest::Response, limit: Duration) -> String {
    match tokio::time::timeout(limit, response.text()).await {
        Ok(Ok(text)) => text,
        Ok(Err(_)) => String::new(),
        Err(_) => {
            tracing::warn!(
                seconds = limit.as_secs(),
                "vendor error body timed out; classifying on the status alone"
            );
            String::new()
        }
    }
}

/// `http.execute` until it returns or the client drops the event channel (SPEC §19.7 Lifecycle).
pub async fn execute_open(
    http: &reqwest::Client,
    request: reqwest::Request,
    tx: &EventSender,
    vendor: &'static str,
    map_err: impl FnOnce(reqwest::Error) -> ThinError,
) -> Result<reqwest::Response, ThinError> {
    tokio::select! {
        _ = tx.closed() => Err(ThinError::vendor_down(vendor, "client disconnected")),
        result = http.execute(request) => result.map_err(map_err),
    }
}

/// The words a vendor's errors carry. They are part of what a client sees, so each vendor
/// keeps its own and the runner only fills them in.
#[derive(Debug, Clone, Copy)]
pub struct Wording {
    /// Leads a status message (`grok 403: …`, `antigravity 429: …`), the rejection log line
    /// and a request-build failure.
    pub label: &'static str,
    /// Named in `<host> unreachable: …`.
    pub host: &'static str,
    pub login_hint: &'static str,
    /// Request header names, named in the 403-on-unexpired-token drift message.
    pub headers_sent: &'static str,
}

/// A non-2xx answer, as [`VendorRequest::classify`] sees it.
pub struct Rejected<'a> {
    pub vendor: &'static str,
    pub status: u16,
    pub headers: &'a reqwest::header::HeaderMap,
    /// Read under `thin.reply_timeout_seconds`; empty when it stalled or broke.
    pub body: &'a str,
    pub token: &'a Token,
    /// The wire model asked for.
    pub model: &'a str,
}

/// How a request ended, for [`VendorRequest::on_outcome`].
pub enum Outcome<'a, W> {
    /// The vendor refused the request before streaming anything.
    Rejected {
        status: u16,
        error: &'a ThinError,
        body: &'a str,
    },
    /// The stream ran. The wire's core says whether it finished, failed or was abandoned;
    /// `result` is what [`drive`] returned (an `Err` only before `Started`).
    Streamed {
        wire: &'a mut W,
        result: &'a Result<(), ThinError>,
    },
}

/// What a vendor file supplies to [`VendorSession::run`] (SPEC §19.8): its request, its wire
/// mapper, its budgets, its status overrides and its rate-limit advisory. Everything else about
/// a request's life — the order of token, permit, send, rejection, stream and outcome — is the
/// runner's, once. Two vendors need more than that, and say so through hooks with no-op
/// defaults: Cursor's duplex reply channel ([`duplex`](Self::duplex)) and its per-row
/// rejections, Claude direct's block classification and its §19.16-aware success
/// ([`classify`](Self::classify), [`on_outcome`](Self::on_outcome)).
pub trait VendorRequest: Sync {
    /// The response decoder; an SSE vendor's is [`Sse`] around its [`SseMapper`].
    type Wire: WireMapper;
    const WORDING: Wording;

    /// The request, headers and body. Runs after the permit is held, so work the request needs
    /// first (agy's project discovery) happens here, under the same cap.
    fn build(
        &self,
        session: &VendorSession<'_>,
        req: &ThinRequest,
        token: &Token,
    ) -> impl std::future::Future<Output = Result<reqwest::RequestBuilder, ThinError>> + Send;

    /// A fresh decoder for one response. `model` is the wire model asked for.
    fn wire(&self, model: &str, budgets: StreamBudgets) -> Self::Wire;

    /// The vendor parameters this request sends, `k=v` comma-joined in send order, for the
    /// access line's `wire_params` (SPEC §19.15 R5). `None` (logged `-`) when none are sent.
    /// Never prompt text or tool arguments (§19.9).
    fn wire_params(&self, _req: &ThinRequest) -> Option<String> {
        None
    }

    /// The quiet-event budgets for this request (SPEC §19.8 **Stream budgets**), from the
    /// vendor's own `thin.vendors.<id>.budgets`.
    fn budgets(&self, req: &ThinRequest, cfg: &ThinConfig) -> StreamBudgets;

    /// A non-2xx status → the error the route answers with. The default reads the OpenAI/xAI
    /// error envelope, the vendor's `Retry-After`, and the shared status table.
    fn classify(&self, rejected: &Rejected<'_>) -> ThinError {
        classify_status(
            rejected.vendor,
            &Self::WORDING,
            rejected.status,
            &error_message(rejected.body),
            retry_after(rejected.headers),
            rejected.model,
            !rejected.token.expired(),
        )
    }

    /// A rate-limit advisory from the 200's headers, sent right after `Started`.
    fn advisory(&self, _headers: &reqwest::header::HeaderMap) -> Option<ThinEvent> {
        None
    }

    /// The request-body sender of a duplex wire (Cursor: exec, KV and MCP-state results are
    /// written back on the request it is still streaming); `None` for a plain response.
    fn duplex(&self) -> Option<ReplySender> {
        None
    }

    /// A streamed request body the capture keeps as one blob (Cursor's opening frames); the
    /// tap otherwise records the body the request carries.
    fn captured_body(&self) -> Option<Vec<u8>> {
        None
    }

    /// Record how the request ended in `/ready` (#126), once: a rejection with its status and
    /// wait; a stream by [`record_outcome`] — ok only on a `Finish`, nothing for a client that
    /// left.
    fn on_outcome(&self, state: &VendorState, outcome: Outcome<'_, Self::Wire>) {
        match outcome {
            Outcome::Rejected {
                status,
                error,
                body,
            } => state.mark_rejection_retry(status, error.code, body, error.retry_after_seconds()),
            Outcome::Streamed { wire, .. } => record_outcome(state, wire.core()),
        }
    }

    /// Fixture replay (SPEC §19.11): captured body chunks through this vendor's own decoder
    /// and the end-of-body rule `run` uses. A failure shows up as a trailing `Error`.
    fn replay(&self, model: &str, frames: &[Vec<u8>]) -> Vec<ThinEvent> {
        drive_offline(frames, &mut self.wire(model, StreamBudgets::OFFLINE))
    }
}

/// What an HTTP vendor owns besides its CLI and its `/ready` state: the client, the
/// `thin.oauth_in_flight` permits, the capture tap and the `thin:` block its budgets and reply
/// timeout come from. [`VendorParts::session`] lends them, with the CLI and state, to one call.
pub struct VendorParts {
    http: reqwest::Client,
    permits: tokio::sync::Semaphore,
    tap: Tap,
    cfg: ThinConfig,
}

impl VendorParts {
    pub fn new(cfg: &ThinConfig) -> Self {
        let http = match reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(15))
            .build()
        {
            Ok(http) => http,
            Err(err) => panic!("reqwest client: {err}"),
        };
        Self::with_client(cfg, http)
    }

    /// A vendor whose transport needs its own client (Cursor: HTTP/2 keepalive, prior
    /// knowledge on a loopback stand-in).
    pub fn with_client(cfg: &ThinConfig, http: reqwest::Client) -> Self {
        Self {
            http,
            permits: tokio::sync::Semaphore::new(cfg.oauth_in_flight.max(1) as usize),
            tap: None,
            cfg: cfg.clone(),
        }
    }

    /// Record the wire exchange (`thickapi capture`); `serve` never sets one.
    pub fn set_tap(&mut self, tap: Tap) {
        self.tap = tap;
    }

    /// A capture tap is set (`thickapi capture`).
    pub fn tapping(&self) -> bool {
        self.tap.is_some()
    }

    /// `thin.reply_timeout_seconds` (#125).
    pub fn reply_timeout(&self) -> Duration {
        reply_timeout(&self.cfg)
    }

    /// Override `thin.reply_timeout_seconds` (tests; `Duration::MAX` is off).
    pub fn set_reply_timeout(&mut self, limit: Duration) {
        self.cfg.reply_timeout_seconds = match limit {
            Duration::MAX => 0,
            limit => limit.as_secs().max(1),
        };
    }

    /// A `thin.oauth_in_flight` permit for a caller that keeps it across several runs
    /// (Claude's §19.16 same-path retry, the R2 probe); see [`VendorSession::holding`].
    pub async fn permit(
        &self,
        vendor: &'static str,
    ) -> Result<tokio::sync::SemaphorePermit<'_>, ThinError> {
        self.permits
            .acquire()
            .await
            .map_err(|_| ThinError::server(format!("{vendor} semaphore closed")))
    }

    pub fn session<'a>(
        &'a self,
        vendor: &'static str,
        cli: &'a CliBacked,
        state: &'a VendorState,
    ) -> VendorSession<'a> {
        VendorSession {
            vendor,
            cli,
            state,
            http: &self.http,
            permits: &self.permits,
            tap: &self.tap,
            cfg: &self.cfg,
            token: None,
            held: None,
        }
    }
}

/// One vendor's shared parts, borrowed for one call.
pub struct VendorSession<'a> {
    vendor: &'static str,
    cli: &'a CliBacked,
    state: &'a VendorState,
    http: &'a reqwest::Client,
    permits: &'a tokio::sync::Semaphore,
    tap: &'a Tap,
    cfg: &'a ThinConfig,
    /// A token the caller already read and routed on (Claude: an expired one goes to the
    /// bridge, so the runner must send the one that was judged).
    token: Option<&'a Token>,
    /// A permit the caller already holds from these parts; the runner takes no second one.
    held: Option<&'a tokio::sync::SemaphorePermit<'a>>,
}

impl<'a> VendorSession<'a> {
    pub fn cli(&self) -> &'a CliBacked {
        self.cli
    }

    pub fn state(&self) -> &'a VendorState {
        self.state
    }

    pub fn http(&self) -> &'a reqwest::Client {
        self.http
    }

    fn reply_timeout(&self) -> Duration {
        reply_timeout(self.cfg)
    }

    /// Send `token` instead of reading a fresh one.
    pub fn with_token(mut self, token: &'a Token) -> Self {
        self.token = Some(token);
        self
    }

    /// Run under `permit`, which the caller took from [`VendorParts::permit`] and keeps across
    /// runs, instead of taking one per run.
    pub fn holding(mut self, permit: &'a tokio::sync::SemaphorePermit<'a>) -> Self {
        self.held = Some(permit);
        self
    }

    /// One streamed request, start to end, in this order: fresh token → in-flight permit →
    /// build → tap → send (abandoned if the client leaves) → on a non-2xx, the body read under
    /// `reply_timeout_seconds`, classified and recorded → else the advisory, the decoder and
    /// [`drive`] (with the duplex reply channel, if the vendor has one) → the outcome recorded
    /// once, after `drive` (#126). The permit is held to the end of the stream.
    pub async fn run<V: VendorRequest>(
        &self,
        v: &V,
        req: &ThinRequest,
        tx: &EventSender,
    ) -> Result<(), ThinError> {
        let vendor = self.vendor;
        let label = V::WORDING.label;
        let token = match self.token {
            Some(token) => token.clone(),
            None => self.cli.fresh_token().await?,
        };
        let _permit = match self.held {
            Some(_) => None,
            None => Some(
                self.permits
                    .acquire()
                    .await
                    .map_err(|_| ThinError::server(format!("{vendor} semaphore closed")))?,
            ),
        };
        let request = v
            .build(self, req, &token)
            .await?
            .build()
            .map_err(|e| ThinError::server(format!("build {label} request: {e}")))?;
        tracing::info!(request_id = %req.request_id, model = %req.entry.id, path = "oauth", "{label} thin request");
        capture::tap_request(self.tap, &request, v.captured_body().as_deref());
        let host = V::WORDING.host;
        let response = execute_open(self.http, request, tx, vendor, |e| {
            ThinError::vendor_down(vendor, format!("{host} unreachable: {e}"))
        })
        .await?;
        capture::tap_response(self.tap, &response);

        let status = response.status().as_u16();
        if !response.status().is_success() {
            let headers = response.headers().clone();
            let body = error_body(response, self.reply_timeout()).await;
            let err = v.classify(&Rejected {
                vendor,
                status,
                headers: &headers,
                body: &body,
                token: &token,
                model: req.entry.wire_model(),
            });
            v.on_outcome(
                self.state,
                Outcome::Rejected {
                    status,
                    error: &err,
                    body: &body,
                },
            );
            tracing::warn!(status, excerpt = %rejection_excerpt(&body), "{label} rejected");
            return Err(err);
        }
        let mut wire = v.wire(req.entry.wire_model(), v.budgets(req, self.cfg));
        wire.core().advisory = v.advisory(response.headers());
        wire.core().set_wire_params(v.wire_params(req));
        let duplex = v.duplex();
        let result = drive(response, tx, &mut wire, self.tap, duplex.as_ref()).await;
        v.on_outcome(
            self.state,
            Outcome::Streamed {
                wire: &mut wire,
                result: &result,
            },
        );
        result
    }

    /// One non-streaming exchange (`list_models`, agy's `loadCodeAssist`), bounded end to end —
    /// body included — by `thin.reply_timeout_seconds` (#125). A body that fails to read is an
    /// error, never an empty answer; a non-2xx status goes to `rejected` with its body.
    pub async fn fetch_json<T: serde::de::DeserializeOwned>(
        &self,
        what: &str,
        rb: reqwest::RequestBuilder,
        rejected: impl FnOnce(u16, &str) -> ThinError + Send,
    ) -> Result<T, ThinError> {
        let vendor = self.vendor;
        bounded(vendor, what, self.reply_timeout(), async {
            let response = rb
                .send()
                .await
                .map_err(|e| ThinError::vendor_down(vendor, format!("{what} unreachable: {e}")))?;
            let status = response.status();
            let text = response.text().await.map_err(|e| {
                ThinError::vendor_down(vendor, format!("{what} body read failed: {e}"))
            })?;
            if !status.is_success() {
                return Err(rejected(status.as_u16(), &text));
            }
            serde_json::from_str(&text)
                .map_err(|e| ThinError::vendor_drift(vendor, format!("{what} not JSON: {e}")))
        })
        .await
    }
}

/// The bookkeeping half of a mapper. Errors travel in `pending` like any other event, in
/// order; [`drive`] decides whether one is returned (before `Started`) or streamed (after).
#[derive(Debug)]
pub struct MapperCore {
    vendor: &'static str,
    model: String,
    path: &'static str,
    fallback: Option<&'static str>,
    /// `Started`'s `wire_params`.
    wire_params: Option<String>,
    /// Sent right after `Started` (rate-limit headers are known before the body is).
    advisory: Option<ThinEvent>,
    budgets: StreamBudgets,
    pub pending: VecDeque<ThinEvent>,
    pub started: bool,
    pub done: bool,
    /// Set by [`MapperCore::working`], taken by `drive`.
    working: bool,
    /// How the turn ended, for [`record_outcome`]: a `Finish`, or the first failure.
    finished: bool,
    failure: Option<ThinError>,
}

impl MapperCore {
    pub fn new(vendor: &'static str, model: impl Into<String>, budgets: StreamBudgets) -> Self {
        Self {
            vendor,
            model: model.into(),
            path: "oauth",
            fallback: None,
            wire_params: None,
            advisory: None,
            budgets,
            pending: VecDeque::new(),
            started: false,
            done: false,
            working: false,
            finished: false,
            failure: None,
        }
    }

    /// The answer grew without making an event: tool arguments and a thinking signature
    /// stream in pieces and surface whole. That is not silence, so it resets the quiet
    /// clock as an event does. An empty piece grew nothing and counts for nothing: a vendor
    /// stuck sending those is as quiet as one sending keepalives.
    pub fn working(&mut self, piece: &str) {
        self.working |= !piece.is_empty();
    }

    pub fn with_advisory(mut self, advisory: Option<ThinEvent>) -> Self {
        self.advisory = advisory;
        self
    }

    /// What `Started` reports as the serving path (SPEC §19.15 R4): Claude's direct path can
    /// itself be the fallback from a bridge fault.
    pub fn with_serving(mut self, path: &'static str, fallback: Option<&'static str>) -> Self {
        self.path = path;
        self.fallback = fallback;
        self
    }

    /// The vendor parameters this request sent, for `Started` (see [`VendorRequest::wire_params`]).
    pub fn set_wire_params(&mut self, params: Option<String>) {
        self.wire_params = params;
    }

    /// For a wire that names the model it actually served (Claude's `message_start`).
    pub fn set_model(&mut self, model: impl Into<String>) {
        self.model = model.into();
    }

    pub fn model(&self) -> &str {
        &self.model
    }

    pub fn vendor(&self) -> &'static str {
        self.vendor
    }

    pub fn ensure_started(&mut self) {
        if self.started || self.done {
            return;
        }
        self.started = true;
        self.pending.push_back(ThinEvent::Started {
            model: self.model.clone(),
            path: self.path,
            fallback: self.fallback,
            wire_params: self.wire_params.clone(),
        });
        if let Some(advisory) = self.advisory.take() {
            self.pending.push_back(advisory);
        }
    }

    /// An event the client sees; implies `Started`. Dropped once the turn is over, so a
    /// mapper cannot send content — or a `Finish` — after its own failure.
    pub fn emit(&mut self, event: ThinEvent) {
        if self.done {
            return;
        }
        self.ensure_started();
        self.pending.push_back(event);
    }

    /// The turn ended on a `Finish` (not a failure, not abandoned).
    pub fn finished(&self) -> bool {
        self.finished
    }

    /// The last event of a good turn.
    pub fn finish(&mut self, event: ThinEvent) {
        if self.done {
            return;
        }
        self.emit(event);
        self.done = true;
        self.finished = true;
    }

    /// The last event of a bad one. The first ending wins.
    pub fn fail(&mut self, err: ThinError) {
        if !self.done {
            self.failure = Some(err.clone());
            self.pending.push_back(ThinEvent::Error(err));
            self.done = true;
        }
    }

    /// Append `piece` unless it would grow past the SSE frame cap. Fails the turn as drift
    /// when it would; returns false so the caller can stop.
    pub fn accumulate(&mut self, dst: &mut String, piece: &str) -> bool {
        if dst.len().saturating_add(piece.len()) > MAX_SSE_FRAME_BYTES {
            let vendor = self.vendor;
            self.fail(ThinError::vendor_drift(
                vendor,
                format!("{vendor} streamed over {MAX_SSE_FRAME_BYTES} bytes"),
            ));
            return false;
        }
        dst.push_str(piece);
        self.working(piece);
        true
    }

    pub fn budget(&self) -> Duration {
        if self.started {
            self.budgets.idle
        } else {
            self.budgets.first_event
        }
    }

    /// The default reading of a spent quiet budget: the vendor went away.
    pub fn fail_quiet(&mut self, waited: Duration) {
        let what = if self.started {
            "went quiet mid-answer"
        } else {
            "sent nothing"
        };
        self.fail(ThinError::vendor_down(
            self.vendor,
            format!("{} {what} for {}s", self.vendor, waited.as_secs()),
        ));
    }
}

/// One vendor's wire → [`ThinEvent`]s, as [`drive`] sees it.
pub trait WireMapper: Send {
    fn core(&mut self) -> &mut MapperCore;
    /// Decode one body chunk. A duplex wire pushes what it must write back into `replies`;
    /// everyone else leaves it alone.
    fn on_chunk(&mut self, chunk: &[u8], replies: &mut Vec<Bytes>);
    /// The body closed cleanly. A wire with its own terminator that never saw it fails here.
    fn on_body_end(&mut self);
    /// The transport broke. What was already complete may still be delivered; the rest is
    /// an error — never a `Finish`, which would pass a truncated answer off as whole.
    fn on_interrupt(&mut self, err: ThinError) {
        self.core().fail(err);
    }
    /// `waited` went by with no event. Cursor finishes a text answer on `f1.14 turn_ended`
    /// (and a tool call as `tool_calls`); with tools offered, a spent after-text budget is
    /// `vendor_down`, not `stop`. `idle_seconds` still ends a text-only answer that has not
    /// seen `turn_ended`. Returning without ending the turn grants a fresh budget.
    fn on_quiet(&mut self, waited: Duration) {
        self.core().fail_quiet(waited);
    }
    /// Cursor's budget depends on what it has seen so far, not only on `Started`.
    fn budget(&mut self) -> Duration {
        self.core().budget()
    }
    /// A reply the vendor never reads must not park the turn (the request-body channel is
    /// small and shared with the heartbeat task).
    fn reply_timeout(&self) -> Duration {
        Duration::from_secs(15)
    }
}

/// Run one response to its end. Returns `Err` only for a failure before `Started`, so the
/// route can still answer with a real status (and Claude's fallback can still replay on the
/// bridge); after it, failures are `ThinEvent::Error`. A client that went away ends the turn
/// quietly — dropping the body closes the upstream.
pub async fn drive(
    response: reqwest::Response,
    tx: &EventSender,
    mapper: &mut dyn WireMapper,
    tap: &Tap,
    replies: Option<&ReplySender>,
) -> Result<(), ThinError> {
    let mut body = response.bytes_stream();
    let mut outbound = Vec::new();
    // Same clock as `timeout` below. `std::time::Instant` ignores `start_paused`, so a
    // stall on the Windows runner could spend the quiet budget between two chunks that
    // the test had spaced well inside it.
    let mut quiet_since = tokio::time::Instant::now();
    let mut started = false;
    loop {
        let budget = mapper.budget();
        let remaining = budget.saturating_sub(quiet_since.elapsed());
        let pending_before = mapper.core().pending.len();
        let next = tokio::select! {
            _ = tx.closed() => return Ok(()),
            next = tokio::time::timeout(remaining, body.next()) => next,
        };
        match next {
            Ok(Some(Ok(chunk))) => {
                capture::tap_frame(tap, &chunk);
                mapper.on_chunk(&chunk, &mut outbound);
            }
            Ok(Some(Err(e))) => {
                let vendor = mapper.core().vendor();
                mapper.on_interrupt(ThinError::vendor_down(
                    vendor,
                    format!("stream interrupted: {e}"),
                ));
            }
            Ok(None) => end_of_body(mapper),
            Err(_) => {
                mapper.on_quiet(budget);
                quiet_since = tokio::time::Instant::now();
            }
        }

        for reply in outbound.drain(..) {
            let Some(replies) = replies else { break };
            let sent = tokio::select! {
                _ = tx.closed() => return Ok(()),
                sent = tokio::time::timeout(mapper.reply_timeout(), replies.send(Ok(reply))) => sent,
            };
            match sent {
                Ok(Ok(())) => {}
                Ok(Err(_)) => return Ok(()),
                Err(_) => {
                    let vendor = mapper.core().vendor();
                    mapper.core().fail(ThinError::vendor_down(
                        vendor,
                        format!("{vendor} stopped reading the request stream"),
                    ));
                    break;
                }
            }
        }

        // Only a started answer can be growing; the first-event budget is not extended.
        let working = std::mem::take(&mut mapper.core().working) && mapper.core().started;
        let eventful = working || mapper.core().pending.len() > pending_before;
        while let Some(event) = mapper.core().pending.pop_front() {
            match event {
                ThinEvent::Error(err) if !started => return Err(err),
                event => {
                    started |= matches!(event, ThinEvent::Started { .. });
                    if tx.send(event).await.is_err() {
                        return Ok(());
                    }
                }
            }
        }
        if mapper.core().done {
            return Ok(());
        }
        // After the flush: time spent waiting on a slow client is not the vendor's silence.
        if eventful {
            quiet_since = tokio::time::Instant::now();
        }
    }
}

/// Record how a streamed turn ended in the vendor's `/ready` state (SPEC §19.8): ok only on
/// a `Finish`, a rejection on a failure — in-stream 429, overload, drift, truncation. The
/// response headers say nothing about that, so the 200 alone is never recorded. A turn the
/// client abandoned has no outcome and records nothing. Cursor records its own through
/// [`VendorRequest::on_outcome`] (held retry, per-row facts).
pub fn record_outcome(state: &VendorState, core: &MapperCore) {
    record_outcome_with(state, core, || state.mark_ok(200));
}

/// [`record_outcome`] with the vendor's own success rule (Claude: Haiku keeps a block).
pub fn record_outcome_with(state: &VendorState, core: &MapperCore, on_ok: impl FnOnce()) {
    if let Some(err) = &core.failure {
        state.mark_rejection(err.status, err.code, &err.message);
    } else if core.finished {
        on_ok();
    }
}

/// The body closed cleanly. The mapper finishes only on its wire's end-of-answer marker
/// (a finish reason, `message_stop`, `finishReason`); a body that closes without one is a
/// truncated answer, never a `stop` (SPEC §19.4). One rule for every vendor, live and replay.
fn end_of_body(mapper: &mut dyn WireMapper) {
    mapper.on_body_end();
    let vendor = mapper.core().vendor();
    mapper.core().fail(ThinError::vendor_down(
        vendor,
        format!("{vendor} closed the stream before the answer ended"),
    ));
}

/// [`drive`] without a clock or a peer, for fixture replay (SPEC §19.11): the same mapper
/// calls in the same order. Reply frames are decoded and dropped; a failure before `Started`
/// shows up as a trailing `ThinEvent::Error` rather than an `Err`. A vendor's fixture replay
/// is [`VendorRequest::replay`]; this is for a hand-built wire.
pub fn drive_offline(frames: &[Vec<u8>], mapper: &mut dyn WireMapper) -> Vec<ThinEvent> {
    let mut out = Vec::new();
    let mut replies = Vec::new();
    for chunk in frames {
        mapper.on_chunk(chunk, &mut replies);
        out.extend(mapper.core().pending.drain(..));
        if mapper.core().done {
            return out;
        }
    }
    end_of_body(mapper);
    out.extend(mapper.core().pending.drain(..));
    out
}

/// What an SSE vendor implements; [`Sse`] supplies the framing.
pub trait SseMapper: Send {
    fn core(&mut self) -> &mut MapperCore;
    fn on_event(&mut self, event: &SseEvent);
    /// The stream ended without the wire's own terminator (`[DONE]`, `message_stop`).
    /// Finish here only when the answer's end-of-answer marker was already seen; otherwise
    /// leave the turn open and the shared rule fails it as truncated.
    fn on_end(&mut self);
    /// See [`WireMapper::on_interrupt`]. A mapper that already holds a complete answer —
    /// everything but the terminator — overrides this to deliver it.
    fn on_interrupt(&mut self, err: ThinError) {
        self.core().fail(err);
    }
}

/// No vendor sends a frame this large; one that does is drift, not an answer to buffer.
pub const MAX_SSE_FRAME_BYTES: usize = 8 * 1024 * 1024;

/// [`WireMapper`] for an SSE wire.
pub struct Sse<M> {
    pub mapper: M,
    parser: SseParser,
}

impl<M: SseMapper> Sse<M> {
    pub fn new(mapper: M) -> Self {
        Self {
            mapper,
            parser: SseParser::default(),
        }
    }

    fn flush_partial_frame(&mut self) {
        if let Some(event) = self.parser.finish() {
            self.mapper.on_event(&event);
        }
    }
}

impl<M: SseMapper> WireMapper for Sse<M> {
    fn core(&mut self) -> &mut MapperCore {
        self.mapper.core()
    }

    fn on_chunk(&mut self, chunk: &[u8], _replies: &mut Vec<Bytes>) {
        for event in self.parser.push(chunk) {
            self.mapper.on_event(&event);
            if self.mapper.core().done {
                return;
            }
        }
        if self.parser.buffered() > MAX_SSE_FRAME_BYTES {
            let vendor = self.mapper.core().vendor();
            self.mapper.core().fail(ThinError::vendor_drift(
                vendor,
                format!("{vendor} sent an SSE frame over {MAX_SSE_FRAME_BYTES} bytes"),
            ));
        }
    }

    fn on_body_end(&mut self) {
        self.flush_partial_frame();
        if !self.mapper.core().done {
            self.mapper.on_end();
        }
    }

    fn on_interrupt(&mut self, err: ThinError) {
        self.flush_partial_frame();
        if !self.mapper.core().done {
            self.mapper.on_interrupt(err);
        }
    }
}

/// The vendor's own CLI keeps its token fresh; the daemon only asks it to (SPEC §19.8).
pub struct CliToken {
    vendor: &'static str,
    cli: &'static str,
    login_hint: &'static str,
    /// A cheap authenticated command that makes the CLI refresh under its own lock.
    refresh_args: &'static [&'static str],
    refresh_timeout: Duration,
    /// Refresh when less than this is left.
    window: Duration,
    /// `false` for a `router.yaml` credentials override: not the CLI's file, so nothing the
    /// CLI runs can refresh it.
    cli_owns_store: bool,
    last_attempt: tokio::sync::Mutex<Option<Instant>>,
    /// How many times this process decided to `run_cli` (tests pin the single-flight).
    attempts: AtomicUsize,
}

/// Do not run the CLI again this soon after running it: a logged-out user's queued requests
/// would otherwise each wait out a full CLI run before their 401.
const REFRESH_BACKOFF: Duration = Duration::from_secs(60);

impl CliToken {
    pub fn new(vendor: &'static str, cli: &'static str, login_hint: &'static str) -> Self {
        Self {
            vendor,
            cli,
            login_hint,
            refresh_args: &[],
            refresh_timeout: Duration::from_secs(20),
            window: Duration::from_secs(300),
            cli_owns_store: true,
            last_attempt: tokio::sync::Mutex::new(None),
            attempts: AtomicUsize::new(0),
        }
    }

    pub fn refresh_with(mut self, args: &'static [&'static str], timeout: Duration) -> Self {
        self.refresh_args = args;
        self.refresh_timeout = timeout;
        self
    }

    pub fn window(mut self, window: Duration) -> Self {
        self.window = window;
        self
    }

    pub fn cli_owns_store(mut self, owns: bool) -> Self {
        self.cli_owns_store = owns;
        self
    }

    fn login_required(&self, message: String) -> ThinError {
        ThinError::vendor_login_required(self.vendor, message, self.login_hint)
    }

    /// A store that records no expiry cannot tell us to refresh; treat it as fresh rather
    /// than run the CLI on every request.
    fn is_fresh(&self, token: &Token) -> bool {
        token.expires_at.is_none() || token.seconds_left() > self.window.as_secs()
    }

    /// The stored token, refreshed through the CLI first when it is inside the window.
    /// Healthy tokens never touch the lock; those that wait on it re-read before running
    /// anything, so concurrent callers share one CLI run.
    pub async fn fresh(
        &self,
        read: impl Fn() -> Result<Token, CredError>,
    ) -> Result<Token, ThinError> {
        let read =
            || read().map_err(|e| self.login_required(format!("{} credentials: {e}", self.vendor)));
        let token = read()?;
        if self.is_fresh(&token) {
            return Ok(token);
        }
        if !self.cli_owns_store {
            return self.usable(token);
        }
        let mut last_attempt = self.last_attempt.lock().await;
        let token = read()?;
        if self.is_fresh(&token) {
            return Ok(token);
        }
        if last_attempt.is_none_or(|at| at.elapsed() >= REFRESH_BACKOFF) {
            *last_attempt = Some(Instant::now());
            self.attempts.fetch_add(1, Ordering::SeqCst);
            self.run_cli().await;
        }
        drop(last_attempt);
        self.usable(read()?)
    }

    /// Inside the window but not yet expired is still a working token.
    fn usable(&self, token: Token) -> Result<Token, ThinError> {
        if !token.expired() {
            return Ok(token);
        }
        let vendor = self.vendor;
        Err(self.login_required(if self.cli_owns_store {
            let command = std::iter::once(self.cli)
                .chain(self.refresh_args.iter().copied())
                .collect::<Vec<_>>()
                .join(" ");
            format!("{vendor} token expired and `{command}` did not refresh it")
        } else {
            format!(
                "{vendor} token in {} expired; that file is a `router.yaml` override, not the \
                 CLI's store, so nothing refreshes it",
                token.source.display()
            )
        }))
    }

    async fn run_cli(&self) {
        let Some(exe) = crate::adapters::resolve_cli(self.cli) else {
            return;
        };
        let run = tokio::process::Command::new(exe)
            .args(self.refresh_args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .status();
        let cli = self.cli;
        match tokio::time::timeout(self.refresh_timeout, run).await {
            Ok(Ok(status)) => tracing::info!(%status, cli, "CLI run to refresh its token"),
            Ok(Err(e)) => tracing::warn!(error = %e, cli, "token refresh CLI failed to start"),
            Err(_) => tracing::warn!(cli, "token refresh CLI timed out"),
        }
    }
}

/// `<cli> --version` → the first word that starts with a digit (`grok 1.0.30 (04b7…)
/// [stable]` → `1.0.30`). Bounded and detached from the daemon's stdin: a CLI that waits on
/// input must not take a runtime worker with it. `None` when the CLI is missing or slow.
pub fn probe_cli_version(cli: &str) -> Option<String> {
    off_worker(|| {
        let exe = crate::adapters::resolve_cli(cli)?;
        probe_exe_version(&exe, &|_| {})
    })
}

/// Run blocking work without parking a tokio worker: `block_in_place` on a multi-thread
/// runtime, a scoped thread on a current-thread one (where `block_in_place` panics), inline
/// outside a runtime.
fn off_worker<T: Send>(work: impl FnOnce() -> T + Send) -> T {
    match tokio::runtime::Handle::try_current() {
        Ok(handle) if handle.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread => {
            tokio::task::block_in_place(work)
        }
        Ok(_) => std::thread::scope(|s| match s.spawn(work).join() {
            Ok(out) => out,
            Err(panic) => std::panic::resume_unwind(panic),
        }),
        Err(_) => work(),
    }
}

/// How long a CLI gets to print its version.
const VERSION_PATIENCE: Duration = Duration::from_secs(10);
/// A CLI that was missing is looked for again this often.
const MISSING_RECHECK: Duration = Duration::from_secs(60);

type Resolve = Box<dyn Fn() -> Option<std::path::PathBuf> + Send + Sync>;
type Configure = Box<dyn Fn(&mut std::process::Command) + Send + Sync>;

#[derive(Debug, Clone)]
struct Probed {
    version: Option<String>,
    exe: Option<std::path::PathBuf>,
    mtime: Option<std::time::SystemTime>,
    at: Instant,
}

/// A vendor CLI's `--version`, kept off the request path and fresh across CLI auto-updates
/// (audit R4, #124). [`CliVersion::current`] never waits: `/ready` reads it, and a wedged CLI
/// must not hang `/ready` or every Pi turn behind it. It starts a background probe when none
/// has run, when the exe's mtime changed since the last one (an auto-update replaced it), or
/// when the CLI was missing a minute ago. [`CliVersion::wait`] is for callers that need the
/// answer (a request header, `doctor`, `capture`): it probes once, bounded by
/// [`VERSION_PATIENCE`], and afterwards behaves like `current`.
pub struct CliVersion {
    cli: &'static str,
    resolve: Resolve,
    configure: Configure,
    known: std::sync::Mutex<Option<Probed>>,
    probing: std::sync::Mutex<()>,
    refreshing: std::sync::atomic::AtomicBool,
}

impl std::fmt::Debug for CliVersion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CliVersion")
            .field("cli", &self.cli)
            .field("known", &self.snapshot())
            .finish()
    }
}

impl CliVersion {
    /// `cli` resolved on `PATH`, run with the daemon's environment.
    pub fn new(cli: &'static str) -> std::sync::Arc<Self> {
        Self::with(
            cli,
            Box::new(move || crate::adapters::resolve_cli(cli)),
            Box::new(|_| {}),
        )
    }

    /// A custom resolver and command setup (the Claude bridge pins its exe and scrubs the
    /// environment).
    pub fn with(cli: &'static str, resolve: Resolve, configure: Configure) -> std::sync::Arc<Self> {
        std::sync::Arc::new(Self {
            cli,
            resolve,
            configure,
            known: std::sync::Mutex::new(None),
            probing: std::sync::Mutex::new(()),
            refreshing: std::sync::atomic::AtomicBool::new(false),
        })
    }

    fn snapshot(&self) -> Option<Probed> {
        self.known.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }

    fn stale(probed: &Probed) -> bool {
        match &probed.exe {
            Some(exe) => mtime(exe) != probed.mtime,
            None => probed.at.elapsed() >= MISSING_RECHECK,
        }
    }

    /// The last known version, never waiting. `None` until the first probe has finished.
    pub fn current(self: &std::sync::Arc<Self>) -> Option<String> {
        match self.snapshot() {
            Some(probed) => {
                if Self::stale(&probed) {
                    self.refresh_in_background();
                }
                probed.version
            }
            None => {
                self.refresh_in_background();
                None
            }
        }
    }

    /// The version, probing now (bounded) if no probe has finished yet.
    pub fn wait(self: &std::sync::Arc<Self>) -> Option<String> {
        if self.snapshot().is_some() {
            return self.current();
        }
        off_worker(|| {
            let _probe = self.probing.lock().unwrap_or_else(|p| p.into_inner());
            if let Some(probed) = self.snapshot() {
                return probed.version;
            }
            self.probe_now()
        })
    }

    /// Probe on a background thread unless one is already running.
    pub fn refresh_in_background(self: &std::sync::Arc<Self>) {
        use std::sync::atomic::Ordering;
        if self.refreshing.swap(true, Ordering::AcqRel) {
            return;
        }
        let this = std::sync::Arc::clone(self);
        let spawned = std::thread::Builder::new()
            .name(format!("{}-version", self.cli))
            .spawn(move || {
                {
                    let _probe = this.probing.lock().unwrap_or_else(|p| p.into_inner());
                    let fresh = this.snapshot().is_some_and(|p| !Self::stale(&p));
                    if !fresh {
                        this.probe_now();
                    }
                }
                this.refreshing.store(false, Ordering::Release);
            });
        if spawned.is_err() {
            self.refreshing.store(false, Ordering::Release);
        }
    }

    /// Caller holds `probing`.
    fn probe_now(&self) -> Option<String> {
        let exe = (self.resolve)();
        let before = exe.as_deref().and_then(mtime);
        let version = exe
            .as_deref()
            .and_then(|exe| probe_exe_version(exe, &self.configure));
        let previous = self.snapshot().and_then(|p| p.version);
        if previous.is_some() && previous != version {
            tracing::info!(cli = self.cli, from = ?previous, to = ?version, "CLI version changed");
        }
        *self.known.lock().unwrap_or_else(|p| p.into_inner()) = Some(Probed {
            version: version.clone(),
            exe,
            mtime: before,
            at: Instant::now(),
        });
        version
    }
}

/// One CLI-backed vendor's token + version state (SPEC §19.8): grok, agy, Cursor each hold one.
pub struct CliBacked {
    token: CliToken,
    read_token: Box<dyn Fn() -> Result<Token, CredError> + Send + Sync>,
    version: std::sync::Arc<CliVersion>,
    client_version_override: Option<String>,
    /// Sent until the CLI's own version is known (Claude direct: the version read at start).
    fallback_client_version: std::borrow::Cow<'static, str>,
    client_version_prefix: &'static str,
}

impl CliBacked {
    pub fn new(
        token: CliToken,
        read_token: impl Fn() -> Result<Token, CredError> + Send + Sync + 'static,
        version: std::sync::Arc<CliVersion>,
        client_version_override: Option<String>,
        fallback_client_version: impl Into<std::borrow::Cow<'static, str>>,
    ) -> Self {
        Self {
            token,
            read_token: Box::new(read_token),
            version,
            client_version_override,
            fallback_client_version: fallback_client_version.into(),
            client_version_prefix: "",
        }
    }

    /// Cursor sends `cli-<version>`; grok/agy send the bare version. Never applied to an override.
    pub fn with_client_version_prefix(mut self, prefix: &'static str) -> Self {
        self.client_version_prefix = prefix;
        self
    }

    pub fn token(&self) -> Result<Token, CredError> {
        (self.read_token)()
    }

    pub async fn fresh_token(&self) -> Result<Token, ThinError> {
        self.token.fresh(|| self.token()).await
    }

    pub fn cli_version(&self) -> Option<String> {
        self.version.wait()
    }

    /// Never waits on the CLI (`/ready`, #124).
    pub fn cli_version_now(&self) -> Option<String> {
        self.version.current()
    }

    pub fn client_version(&self) -> String {
        self.format_client_version(self.cli_version())
    }

    pub fn client_version_now(&self) -> String {
        self.format_client_version(self.cli_version_now())
    }

    fn format_client_version(&self, cli: Option<String>) -> String {
        if let Some(v) = &self.client_version_override {
            return v.clone();
        }
        let v = cli.unwrap_or_else(|| self.fallback_client_version.to_string());
        if self.client_version_prefix.is_empty() || v.starts_with(self.client_version_prefix) {
            v
        } else {
            format!("{}{v}", self.client_version_prefix)
        }
    }

    pub fn snapshot(&self) -> CliSnapshot {
        CliSnapshot {
            token: self.token(),
            cli_version: self.cli_version_now(),
            client_version: self.client_version_now(),
        }
    }
}

/// [`CliBacked::snapshot`]'s fields, read straight into `/ready`.
pub struct CliSnapshot {
    pub token: Result<Token, CredError>,
    pub cli_version: Option<String>,
    pub client_version: String,
}

fn mtime(path: &std::path::Path) -> Option<std::time::SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

fn probe_exe_version(
    exe: &std::path::Path,
    configure: &(dyn Fn(&mut std::process::Command) + Send + Sync),
) -> Option<String> {
    let mut command = std::process::Command::new(exe);
    configure(&mut command);
    let mut child = command
        .arg("--version")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;
    let started = Instant::now();
    while child.try_wait().ok()?.is_none() {
        if started.elapsed() > VERSION_PATIENCE {
            let _ = child.kill();
            let _ = child.wait();
            return None;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    // A version banner fits the pipe buffer, so reading after exit cannot deadlock.
    let mut bytes = Vec::new();
    std::io::Read::read_to_end(&mut child.stdout.take()?, &mut bytes).ok()?;
    String::from_utf8_lossy(&bytes)
        .split_whitespace()
        .find(|w| w.chars().next().is_some_and(|c| c.is_ascii_digit()))
        .map(|w| {
            w.trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '.')
                .to_string()
        })
}

/// `Retry-After` as seconds: the integer form, or an HTTP-date measured from now. A date
/// already past is `None`, like no header.
pub fn retry_after(headers: &reqwest::header::HeaderMap) -> Option<u64> {
    let value = headers.get("retry-after")?.to_str().ok()?.trim();
    if let Ok(seconds) = value.parse::<u64>() {
        return Some(seconds);
    }
    // A date already past (clock skew) names no wait; the caller's default beats "now".
    let at = httpdate::parse_http_date(value).ok()?;
    at.duration_since(std::time::SystemTime::now())
        .ok()
        .map(|d| d.as_secs())
}

/// A vendor that names a wait this short is throttling.
const THROTTLE_HORIZON_SECONDS: u64 = 3600;
const DEFAULT_RETRY_SECONDS: u64 = 60;
/// The text is about a rate: always a throttle.
const RATE_WORDS: [&str; 6] = [
    "per minute",
    "perminute",
    "per second",
    "persecond",
    "rate limit",
    "too many requests",
];
/// The text is about the plan: exhaustion, even beside a short wait (a gateway's stock
/// `Retry-After: 30` on a monthly cap must not set Pi looping).
const PLAN_WORDS: [&str; 7] = [
    "billing",
    "payment",
    "credit",
    "upgrade",
    "plan limit",
    "usage limit",
    "monthly",
];
/// Google says "quota" and `RESOURCE_EXHAUSTED` for a per-minute throttle too, so these only
/// mean exhaustion when the vendor names no wait at all.
const VAGUE_WORDS: [&str; 2] = ["quota", "exhausted"];
/// A throttle's phrase, but a plan cap says it too ("usage limit reached, try again at 4pm"),
/// so it only counts once the text is not about the plan.
const RETRY_WORDS: [&str; 1] = ["try again"];

/// One reading of a vendor's 429 (SPEC §19.4). "Slow down" and "you are out" share a status
/// and much of a vocabulary. In order: wording about a rate is a throttle; wording about the
/// plan is exhaustion; a bare "try again" is a throttle; otherwise the wait decides — an hour or less is a throttle, longer is
/// exhaustion, and with no wait named the vague words tip it to exhaustion.
/// A throttle is `429`, retryable, with the vendor's own wait. Exhaustion is `402` when the
/// text names quota/usage/payment, else a non-retryable `429`.
pub fn limit_error(
    vendor: &str,
    message: impl Into<String>,
    vendor_text: &str,
    wait: Option<u64>,
) -> ThinError {
    let lower = vendor_text.to_ascii_lowercase();
    let says = |words: &[&str]| words.iter().any(|w| lower.contains(w));
    let exhausted = if says(&RATE_WORDS) {
        false
    } else if says(&PLAN_WORDS) {
        true
    } else if says(&RETRY_WORDS) {
        false
    } else {
        match wait {
            Some(seconds) => seconds > THROTTLE_HORIZON_SECONDS,
            None => says(&VAGUE_WORDS),
        }
    };
    ThinError::vendor_limit(
        vendor,
        message,
        wait.unwrap_or(DEFAULT_RETRY_SECONDS),
        vendor_text,
        exhausted,
    )
}

/// What a vendor's non-2xx status needs to become a [`ThinError`] (SPEC §19.4, audit R8).
pub struct StatusFault<'a> {
    pub vendor: &'static str,
    pub status: u16,
    /// Leads the message, e.g. `grok 403` or `claude oauth 403 permission_error`.
    pub prefix: &'a str,
    /// The vendor's text, already redacted.
    pub shown: &'a str,
    /// The vendor's raw text, for the 429 throttle-or-exhaustion reading.
    pub raw: &'a str,
    pub retry_after: Option<u64>,
    pub model: &'a str,
    /// The token we sent has not expired. A 403 on it is the vendor refusing our request
    /// shape (drift), not an account or quota state.
    pub token_unexpired: bool,
    pub login_hint: &'a str,
    /// Request header names, named in the 403-on-unexpired-token drift message.
    pub headers_sent: &'a str,
}

/// The one status table every OAuth-direct vendor shares (audit R8). A vendor that needs a
/// different answer for a status matches it first and falls through here for the rest.
/// `402` alone is quota: a `403` is never `insufficient_quota`.
pub fn status_error(f: &StatusFault<'_>) -> ThinError {
    let StatusFault {
        vendor,
        status,
        prefix,
        shown,
        ..
    } = *f;
    match status {
        400 => ThinError::invalid_request(shown, None).with_vendor(vendor),
        401 => ThinError::vendor_login_required(vendor, format!("{prefix}: {shown}"), f.login_hint),
        402 => ThinError::insufficient_quota(vendor, format!("{prefix}: {shown}")),
        403 if f.token_unexpired => ThinError::vendor_drift(
            vendor,
            format!(
                "{prefix}: 403 on unexpired token (headers sent: {}): {shown}",
                f.headers_sent
            ),
        ),
        403 => ThinError::permission(format!("{prefix}: {shown}"))
            .with_vendor(vendor)
            .with_hint(f.login_hint),
        404 => ThinError::model_not_found(f.model).with_vendor(vendor),
        429 => limit_error(vendor, format!("{prefix}: {shown}"), f.raw, f.retry_after),
        s if s >= 500 => ThinError::vendor_down(vendor, format!("{prefix}: {shown}")),
        _ => ThinError::server(format!("{prefix}: {shown}")),
    }
}

/// A rejection's status read through the shared table, worded as the vendor words it:
/// `<label> <status>: <redacted message>`. Used for a non-2xx answer and for an error the
/// vendor sends in the stream, so both read the same way.
pub fn classify_status(
    vendor: &'static str,
    wording: &Wording,
    status: u16,
    message: &str,
    retry_after: Option<u64>,
    model: &str,
    token_unexpired: bool,
) -> ThinError {
    let shown = redact(message);
    let prefix = format!("{} {status}", wording.label);
    status_error(&StatusFault {
        vendor,
        status,
        prefix: &prefix,
        shown: &shown,
        raw: message,
        retry_after,
        model,
        token_unexpired,
        login_hint: wording.login_hint,
        headers_sent: wording.headers_sent,
    })
}

/// An error body's message: xAI's `{"code","error"}`, the OpenAI envelope
/// `{"error":{"message"}}`, a bare `{"message"}`, else the first 300 characters.
pub fn error_message(text: &str) -> String {
    let parsed: Option<Value> = serde_json::from_str(text).ok();
    parsed
        .as_ref()
        .and_then(|v| {
            v.get("error")
                .and_then(|e| {
                    e.as_str()
                        .map(str::to_string)
                        .or_else(|| e.get("message").and_then(Value::as_str).map(str::to_string))
                })
                .or_else(|| v.get("message").and_then(Value::as_str).map(str::to_string))
        })
        .unwrap_or_else(|| text.chars().take(300).collect())
}

/// Where a vendor's usage object keeps its counts.
#[derive(Debug, Clone, Copy)]
pub struct UsageKeys {
    pub prompt: &'static str,
    /// Summed: agy counts thoughts apart from the answer.
    pub completion: &'static [&'static str],
    /// Absent → prompt + completion.
    pub total: &'static str,
    /// A JSON pointer into the usage object.
    pub cached: &'static str,
}

/// A vendor's usage object → [`Usage`]; a missing count is 0.
pub fn usage_from(u: &Value, keys: &UsageKeys) -> Usage {
    let n = |k: &str| u.get(k).and_then(Value::as_u64).unwrap_or(0);
    let prompt = n(keys.prompt);
    let completion = keys.completion.iter().map(|k| n(k)).sum::<u64>();
    Usage {
        prompt_tokens: prompt,
        completion_tokens: completion,
        total_tokens: u
            .get(keys.total)
            .and_then(Value::as_u64)
            .unwrap_or(prompt + completion),
        prompt_tokens_details: Some(PromptTokensDetails {
            cached_tokens: u.pointer(keys.cached).and_then(Value::as_u64).unwrap_or(0),
        }),
    }
}

/// Where a vendor's response headers say how much of its rate limit is used.
#[derive(Debug, Clone, Copy)]
pub enum AdvisoryHeaders {
    /// A limit and what remains of it: used = (limit − remaining) / limit (grok's
    /// `x-ratelimit-{limit,remaining}-tokens`).
    Remaining {
        limit: &'static str,
        remaining: &'static str,
    },
    /// A utilisation in `0..=1` and a unix reset time, each the first of its names present
    /// (Claude's `anthropic-ratelimit-unified-*`).
    Utilization {
        utilization: &'static [&'static str],
        reset: &'static [&'static str],
    },
}

/// Rate-limit response headers → a `RateLimitAdvisory`, or `None` when they are absent or
/// make no sense.
pub fn rate_limit_advisory(
    headers: &reqwest::header::HeaderMap,
    source: &AdvisoryHeaders,
) -> Option<ThinEvent> {
    let get = |k: &str| headers.get(k).and_then(|v| v.to_str().ok());
    match *source {
        AdvisoryHeaders::Remaining { limit, remaining } => {
            let num = |k: &str| get(k).and_then(|v| v.parse::<f64>().ok());
            let limit = num(limit)?;
            let remaining = num(remaining)?;
            if limit <= 0.0 {
                return None;
            }
            Some(ThinEvent::RateLimitAdvisory {
                utilization: ((limit - remaining) / limit).clamp(0.0, 1.0),
                resets_at: None,
            })
        }
        AdvisoryHeaders::Utilization { utilization, reset } => {
            let utilization = utilization
                .iter()
                .find_map(|k| get(k))
                .and_then(|v| v.parse::<f64>().ok())
                .filter(|u| (0.0..=1.0).contains(u))?;
            let resets_at = reset
                .iter()
                .find_map(|k| get(k))
                .and_then(|v| v.parse::<u64>().ok())
                .map(crate::thin::claude_bridge::protocol::iso8601);
            Some(ThinEvent::RateLimitAdvisory {
                utilization,
                resets_at,
            })
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    use super::super::creds::now_unix;
    use super::*;

    fn fault(status: u16, token_unexpired: bool) -> ThinError {
        status_error(&StatusFault {
            vendor: "grok",
            status,
            prefix: &format!("grok {status}"),
            shown: "no",
            raw: "no",
            retry_after: None,
            model: "grok-4.7",
            token_unexpired,
            login_hint: "grok login",
            headers_sent: "authorization",
        })
    }

    #[test]
    fn status_table_403_is_never_quota() {
        let e = fault(403, true);
        assert_eq!((e.status, e.code), (502, "vendor_drift"));
        assert!(
            e.message.contains("headers sent: authorization"),
            "{}",
            e.message
        );
        let e = fault(403, false);
        assert_eq!((e.status, e.code), (403, "permission_error"));
        assert_eq!(e.hint(), Some("grok login"));
        assert_eq!(fault(402, true).code, "insufficient_quota");
        assert_eq!(fault(401, true).code, "vendor_login_required");
        assert_eq!(fault(404, true).code, "model_not_found");
        assert_eq!(fault(429, true).status, 429);
        assert_eq!(fault(503, true).code, "vendor_down");
        assert_eq!(fault(418, true).code, "server_error");
    }

    fn budgets() -> StreamBudgets {
        StreamBudgets {
            first_event: Duration::from_secs(120),
            idle: Duration::from_secs(180),
        }
    }

    #[test]
    fn a_short_wait_is_a_throttle_even_when_google_says_quota() {
        let err = limit_error("agy", "agy 429", "Quota exceeded for model", Some(7));
        assert_eq!((err.status, err.code), (429, "rate_limit_error"));
        assert!(!err.no_retry());
        assert_eq!(err.retry_after_seconds(), Some(7));
        assert!(
            !err.message.starts_with("quota exceeded"),
            "{}",
            err.message
        );
    }

    #[test]
    fn wording_about_a_rate_is_a_throttle_with_or_without_a_wait() {
        let text = "Quota exceeded for quota metric 'requests' and limit 'PerMinute'";
        assert!(!limit_error("agy", "m", text, None).no_retry());
        assert!(!limit_error("agy", "m", text, Some(7200)).no_retry());
        // A plan cap that also says when to come back is still the plan.
        let cap = "You've reached your usage limit. Please try again at 4pm.";
        assert!(limit_error("claude", "m", cap, None).no_retry());
        assert!(!limit_error("x", "m", "Busy, try again shortly", None).no_retry());
        let bare = limit_error("grok", "m", "Rate limit exceeded, please try again", None);
        assert_eq!((bare.status, bare.no_retry()), (429, false));
        assert_eq!(bare.retry_after_seconds(), Some(DEFAULT_RETRY_SECONDS));
    }

    #[test]
    fn wording_about_the_plan_is_exhaustion_even_beside_a_short_wait() {
        let err = limit_error("x", "m", "Monthly plan limit reached; upgrade", Some(30));
        assert!(err.no_retry());
        let err = limit_error("grok", "m", "You have run out of credits", None);
        assert_eq!((err.status, err.no_retry()), (429, true));
    }

    #[test]
    fn otherwise_the_wait_decides_and_an_hour_is_still_a_throttle() {
        let reached = "Individual quota reached. Resets in 65h32m";
        let err = limit_error("agy", "m", reached, Some(65 * 3600));
        assert_eq!((err.status, err.code), (402, "insufficient_quota"));
        assert!(!limit_error("agy", "m", reached, Some(THROTTLE_HORIZON_SECONDS)).no_retry());
        assert!(
            limit_error("agy", "m", reached, None).no_retry(),
            "vague words, no wait"
        );
        assert!(!limit_error("agy", "m", "slow down", None).no_retry());
    }

    #[test]
    fn probe_cli_version_missing_cli_returns_none_quickly() {
        let started = Instant::now();
        assert_eq!(probe_cli_version("thickapi-no-such-cli"), None);
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "{:?}",
            started.elapsed()
        );
    }

    #[test]
    fn cli_version_current_never_waits_on_a_slow_probe() {
        let slow = CliVersion::with(
            "stub",
            Box::new(|| {
                std::thread::sleep(Duration::from_millis(800));
                None
            }),
            Box::new(|_| {}),
        );
        let started = Instant::now();
        assert_eq!(slow.current(), None, "nothing known yet");
        assert_eq!(slow.current(), None);
        assert!(
            started.elapsed() < Duration::from_millis(200),
            "current() waited on the probe: {:?}",
            started.elapsed()
        );
        // `wait` joins the running probe instead of starting a second one.
        assert_eq!(slow.wait(), None);
        assert!(started.elapsed() >= Duration::from_millis(700));
        assert!(slow.snapshot().is_some(), "the probe result is kept");
    }

    #[test]
    fn cli_version_reprobes_when_the_exe_changes() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("fake.exe");
        std::fs::write(&exe, b"v1").unwrap();
        let calls = std::sync::Arc::new(AtomicUsize::new(0));
        let counted = calls.clone();
        let path = exe.clone();
        let version = CliVersion::with(
            "stub",
            Box::new(move || {
                counted.fetch_add(1, Ordering::SeqCst);
                Some(path.clone())
            }),
            Box::new(|_| {}),
        );
        // Not a runnable program: the probe answers `None`, but it ran.
        assert_eq!(version.wait(), None);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        let _ = version.current();
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "unchanged exe: no re-probe"
        );
        let later = std::time::SystemTime::now() + Duration::from_secs(5);
        std::fs::File::options()
            .write(true)
            .open(&exe)
            .unwrap()
            .set_modified(later)
            .unwrap();
        let _ = version.current();
        let deadline = Instant::now() + Duration::from_secs(5);
        while calls.load(Ordering::SeqCst) < 2 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(calls.load(Ordering::SeqCst), 2, "a new mtime re-probes");
    }

    #[test]
    fn retry_after_reads_both_forms() {
        let mut headers = reqwest::header::HeaderMap::new();
        assert_eq!(retry_after(&headers), None);
        headers.insert("retry-after", "12".parse().unwrap());
        assert_eq!(retry_after(&headers), Some(12));
        let at = std::time::SystemTime::now() + Duration::from_secs(90);
        headers.insert("retry-after", httpdate::fmt_http_date(at).parse().unwrap());
        assert!((88..=90).contains(&retry_after(&headers).unwrap()));
        headers.insert(
            "retry-after",
            "Thu, 01 Jan 1970 00:00:00 GMT".parse().unwrap(),
        );
        assert_eq!(retry_after(&headers), None);
    }

    #[test]
    fn a_zero_budget_is_off_not_instant() {
        let b = StreamBudgets::from_config(&OauthBudgets {
            first_event_seconds: 0,
            idle_seconds: 5,
        });
        assert_eq!(
            (b.first_event, b.idle),
            (Duration::MAX, Duration::from_secs(5))
        );
    }

    fn kinds(core: &MapperCore) -> Vec<String> {
        core.pending
            .iter()
            .map(|e| {
                let debug = format!("{e:?}");
                debug
                    .split(['(', ' ', '{'])
                    .next()
                    .unwrap_or("")
                    .to_string()
            })
            .collect()
    }

    #[test]
    fn an_idle_floor_raises_only_idle_and_never_lowers_it() {
        let base = budgets();
        let floored = base.with_idle_floor(Duration::from_secs(600));
        assert_eq!(
            (floored.first_event, floored.idle),
            (base.first_event, Duration::from_secs(600))
        );
        let kept = base.with_idle_floor(Duration::from_secs(150));
        assert_eq!(
            (kept.first_event, kept.idle),
            (base.first_event, Duration::from_secs(180))
        );
        let off = base.with_idle_floor(StreamBudgets::seconds(0));
        assert_eq!(
            (off.first_event, off.idle),
            (base.first_event, Duration::MAX)
        );
    }

    #[test]
    fn core_orders_started_advisory_events_and_lets_nothing_follow_the_end() {
        let advisory = ThinEvent::RateLimitAdvisory {
            utilization: 0.5,
            resets_at: None,
        };
        let mut core = MapperCore::new("claude", "asked-for", budgets())
            .with_advisory(Some(advisory))
            .with_serving("oauth", Some("bridge->oauth"));
        assert_eq!(core.budget(), Duration::from_secs(120));
        core.set_model("served");
        core.emit(ThinEvent::TextDelta("hi".into()));
        assert_eq!(core.budget(), Duration::from_secs(180));
        core.fail_quiet(Duration::from_secs(25));
        core.fail(ThinError::server("a second failure is ignored"));
        core.emit(ThinEvent::TextDelta(
            "nor may content follow a failure".into(),
        ));
        assert_eq!(
            kinds(&core),
            ["Started", "RateLimitAdvisory", "TextDelta", "Error"]
        );
        match core.pending.front() {
            Some(ThinEvent::Started {
                model, fallback, ..
            }) => {
                assert_eq!(
                    (model.as_str(), *fallback),
                    ("served", Some("bridge->oauth"))
                );
            }
            other => panic!("{other:?}"),
        }
        match core.pending.back() {
            Some(ThinEvent::Error(e)) => assert!(e.message.contains("mid-answer for 25s")),
            other => panic!("{other:?}"),
        }
    }

    /// A scripted wire: each chunk is a command, so `drive`'s own rules can be pinned.
    struct Script {
        core: MapperCore,
        quiet_is_the_end: bool,
        quiets: usize,
    }

    impl WireMapper for Script {
        fn core(&mut self) -> &mut MapperCore {
            &mut self.core
        }
        fn on_chunk(&mut self, chunk: &[u8], replies: &mut Vec<Bytes>) {
            match chunk {
                b"text" => self.core.emit(ThinEvent::TextDelta("t".into())),
                b"reply" => replies.push(Bytes::from_static(b"pong")),
                b"args" => self.core.working("{"),
                b"empty" => self.core.working(""),
                b"ping" => {}
                b"start+fail" => {
                    self.core.ensure_started();
                    self.core.fail(ThinError::server("after start"));
                }
                b"fail" => self
                    .core
                    .fail(ThinError::vendor_drift("script", "before start")),
                _ => {}
            }
        }
        fn on_body_end(&mut self) {}
        fn reply_timeout(&self) -> Duration {
            Duration::from_millis(200)
        }
        fn on_quiet(&mut self, waited: Duration) {
            self.quiets += 1;
            if self.quiet_is_the_end {
                self.core.fail_quiet(waited);
            }
        }
    }

    fn script(first_event_ms: u64, quiet_is_the_end: bool) -> Script {
        let budgets = StreamBudgets {
            first_event: Duration::from_millis(first_event_ms),
            idle: Duration::from_millis(first_event_ms),
        };
        Script {
            core: MapperCore::new("script", "m", budgets),
            quiet_is_the_end,
            quiets: 0,
        }
    }

    struct DummySse {
        core: MapperCore,
    }

    impl SseMapper for DummySse {
        fn core(&mut self) -> &mut MapperCore {
            &mut self.core
        }
        fn on_event(&mut self, _event: &SseEvent) {}
        fn on_end(&mut self) {}
    }

    #[test]
    fn an_sse_frame_over_the_cap_is_drift() {
        let mut sse = Sse::new(DummySse {
            core: MapperCore::new("script", "m", budgets()),
        });
        let huge = vec![b'x'; MAX_SSE_FRAME_BYTES + 1];
        sse.on_chunk(&huge, &mut Vec::new());
        match sse.core().pending.back() {
            Some(ThinEvent::Error(e)) => {
                assert_eq!(e.code, "vendor_drift");
                assert!(
                    e.message.contains(&MAX_SSE_FRAME_BYTES.to_string()),
                    "{}",
                    e.message
                );
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn accumulating_past_the_sse_cap_is_drift_and_does_not_grow() {
        let mut core = MapperCore::new("script", "m", budgets());
        let mut dst = "x".repeat(MAX_SSE_FRAME_BYTES - 10);
        let before = dst.len();
        assert!(!core.accumulate(&mut dst, "01234567890"));
        assert_eq!(dst.len(), before);
        match core.pending.back() {
            Some(ThinEvent::Error(e)) => {
                assert_eq!(e.code, "vendor_drift");
                assert!(
                    e.message.contains(&MAX_SSE_FRAME_BYTES.to_string()),
                    "{}",
                    e.message
                );
            }
            other => panic!("{other:?}"),
        }
    }

    /// In-memory body with the same pacing as [`response`]. No socket, so a
    /// `start_paused` test can advance the quiet clock without the runtime skipping a
    /// real read.
    fn paused_response(chunks: Vec<Result<&'static str, ()>>, hold: bool) -> reqwest::Response {
        let body = async_stream::stream! {
            for chunk in chunks {
                match chunk {
                    Ok(text) => yield Ok::<_, std::io::Error>(Bytes::from(text)),
                    Err(()) => {
                        yield Err(std::io::Error::other("reset"));
                        return;
                    }
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            if hold {
                std::future::pending::<()>().await;
            }
        };
        let http = axum::http::Response::builder()
            .status(200)
            .body(reqwest::Body::wrap_stream(body))
            .expect("scripted response");
        reqwest::Response::from(http)
    }

    /// A loopback body: `Ok` chunks are sent in order, `Err` breaks the transport, and
    /// `hold` keeps the connection open afterwards instead of ending it.
    async fn response(chunks: Vec<Result<&'static str, ()>>, hold: bool) -> reqwest::Response {
        reqwest::get(serve(200, &[], chunks, hold).await)
            .await
            .unwrap()
    }

    /// A loopback server that answers its one request with `status`, `headers` and the body
    /// [`response`] describes. Returns its URL.
    async fn serve(
        status: u16,
        headers: &'static [(&'static str, &'static str)],
        chunks: Vec<Result<&'static str, ()>>,
        hold: bool,
    ) -> String {
        let body = async_stream::stream! {
            for chunk in chunks {
                match chunk {
                    Ok(text) => yield Ok::<_, std::io::Error>(Bytes::from(text)),
                    Err(()) => {
                        yield Err(std::io::Error::other("reset"));
                        return;
                    }
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            if hold {
                std::future::pending::<()>().await;
            }
        };
        // A handler must be callable more than once; this one is only ever called once.
        let body = std::sync::Arc::new(std::sync::Mutex::new(Some(axum::body::Body::from_stream(
            body,
        ))));
        let app = axum::Router::new().fallback(move || {
            let body = body.clone();
            async move {
                let mut response =
                    axum::response::Response::new(body.lock().unwrap().take().unwrap());
                *response.status_mut() = axum::http::StatusCode::from_u16(status).unwrap();
                for (name, value) in headers {
                    response.headers_mut().insert(
                        axum::http::HeaderName::from_static(name),
                        axum::http::HeaderValue::from_static(value),
                    );
                }
                response
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("http://{addr}/")
    }

    async fn collect(mut rx: tokio::sync::mpsc::Receiver<ThinEvent>) -> Vec<String> {
        let mut seen = Vec::new();
        while let Some(event) = rx.recv().await {
            let debug = format!("{event:?}");
            seen.push(
                debug
                    .split(['(', ' ', '{'])
                    .next()
                    .unwrap_or("")
                    .to_string(),
            );
        }
        seen
    }

    #[tokio::test]
    async fn a_failure_before_started_is_returned_and_after_it_is_streamed() {
        let (tx, rx) = tokio::sync::mpsc::channel(8);
        let mut m = script(5_000, true);
        let err = drive(
            response(vec![Ok("fail")], true).await,
            &tx,
            &mut m,
            &None,
            None,
        )
        .await
        .expect_err("nothing was sent, so the route can still answer with a status");
        assert_eq!(err.code, "vendor_drift");
        drop(tx);
        assert!(collect(rx).await.is_empty());

        let (tx, rx) = tokio::sync::mpsc::channel(8);
        let mut m = script(5_000, true);
        drive(
            response(vec![Ok("start+fail")], true).await,
            &tx,
            &mut m,
            &None,
            None,
        )
        .await
        .expect("Started went out first");
        drop(tx);
        assert_eq!(collect(rx).await, ["Started", "Error"]);
    }

    #[tokio::test]
    async fn a_quiet_handler_that_does_not_end_the_turn_gets_a_fresh_budget() {
        let (tx, _rx) = tokio::sync::mpsc::channel(8);
        let mut m = script(30, false);
        let run = drive(response(vec![], true).await, &tx, &mut m, &None, None);
        assert!(tokio::time::timeout(Duration::from_millis(200), run)
            .await
            .is_err());
        // Roughly one per 30 ms; a spin would be in the millions.
        assert!((2..=10).contains(&m.quiets), "{} quiet calls", m.quiets);
    }

    #[tokio::test]
    async fn a_slow_client_is_not_mistaken_for_a_quiet_vendor() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(1);
        let mut m = script(150, true);
        let chunks = vec![Ok("text"), Ok("text"), Ok("text"), Ok("text")];
        let reader = tokio::spawn(async move {
            let mut seen = 0;
            while let Some(event) = rx.recv().await {
                // Slower than the whole quiet budget, per event.
                tokio::time::sleep(Duration::from_millis(200)).await;
                assert!(!matches!(event, ThinEvent::Error(_)), "{event:?}");
                seen += 1;
                if seen == 5 {
                    break;
                }
            }
            seen
        });
        let run = drive(response(chunks, true).await, &tx, &mut m, &None, None);
        let _ = tokio::time::timeout(Duration::from_secs(3), run).await;
        assert_eq!(
            reader.await.unwrap(),
            5,
            "Started and four deltas, no error between"
        );
    }

    /// A long tool call streams its arguments with no event; that is work, not silence.
    /// The same stream of keepalives, or of empty pieces, is silence.
    ///
    /// Paused time, and an in-memory body: a loopback socket plus `std::time` made this
    /// fail when the Windows runner stalled longer than the 150 ms budget between chunks.
    #[tokio::test(start_paused = true)]
    async fn silent_work_resets_the_quiet_clock_and_a_keepalive_does_not() {
        for (chunk, survives) in [("args", true), ("ping", false), ("empty", false)] {
            let (tx, rx) = tokio::sync::mpsc::channel(8);
            let mut m = script(150, true);
            // 60 chunks at 5 ms apart. Argument bytes reset the clock each chunk, so they
            // stay inside the 150 ms budget. Pings and empty pieces do not, so the budget
            // ends the turn before the second text.
            let mut chunks = vec![Ok("text")];
            chunks.extend(std::iter::repeat_n(Ok(chunk), 60));
            chunks.push(Ok("text"));
            let run = drive(paused_response(chunks, true), &tx, &mut m, &None, None);
            let _ = tokio::time::timeout(Duration::from_secs(5), run).await;
            drop(tx);
            let seen = collect(rx).await;
            let deltas = seen.iter().filter(|k| *k == "TextDelta").count();
            if survives {
                assert_eq!(deltas, 2, "{chunk}: {seen:?}");
            } else {
                assert_eq!(seen, ["Started", "TextDelta", "Error"], "{chunk}");
            }
        }
    }

    #[tokio::test]
    async fn a_broken_transport_is_an_error_never_a_finish() {
        let (tx, rx) = tokio::sync::mpsc::channel(8);
        let mut m = script(5_000, true);
        drive(
            response(vec![Ok("text"), Err(())], false).await,
            &tx,
            &mut m,
            &None,
            None,
        )
        .await
        .unwrap();
        drop(tx);
        assert_eq!(collect(rx).await, ["Started", "TextDelta", "Error"]);
    }

    #[tokio::test]
    async fn a_client_that_leaves_ends_the_turn() {
        let (tx, rx) = tokio::sync::mpsc::channel(8);
        let mut m = script(60_000, true);
        let run = drive(
            response(vec![Ok("text")], true).await,
            &tx,
            &mut m,
            &None,
            None,
        );
        tokio::pin!(run);
        assert!(tokio::time::timeout(Duration::from_millis(100), &mut run)
            .await
            .is_err());
        drop(rx);
        tokio::time::timeout(Duration::from_secs(2), run)
            .await
            .expect("drive must notice the closed channel")
            .unwrap();
    }

    #[tokio::test]
    async fn a_reply_reaches_the_request_stream() {
        let (tx, _rx) = tokio::sync::mpsc::channel(8);
        let (reply_tx, mut reply_rx) = tokio::sync::mpsc::channel(1);
        let mut m = script(5_000, true);
        let run = drive(
            response(vec![Ok("reply")], true).await,
            &tx,
            &mut m,
            &None,
            Some(&reply_tx),
        );
        tokio::pin!(run);
        let got = tokio::select! {
            _ = &mut run => panic!("drive ended early"),
            got = reply_rx.recv() => got,
        };
        assert_eq!(got.unwrap().unwrap(), Bytes::from_static(b"pong"));
    }

    /// Nobody drains the request stream: the second reply cannot be queued, and the turn
    /// ends with an error instead of parking the read loop with no clock running.
    #[tokio::test]
    async fn a_reply_the_vendor_never_reads_fails_the_turn() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        let (reply_tx, _reply_rx) = tokio::sync::mpsc::channel(1);
        let mut m = script(60_000, true);
        let run = drive(
            response(vec![Ok("text"), Ok("reply"), Ok("reply")], true).await,
            &tx,
            &mut m,
            &None,
            Some(&reply_tx),
        );
        tokio::time::timeout(Duration::from_secs(5), run)
            .await
            .expect("a stuck reply must not hold the turn")
            .expect("Started went out first");
        drop(tx);
        let mut last = None;
        while let Some(event) = rx.recv().await {
            assert!(!matches!(event, ThinEvent::Finish(_)), "{event:?}");
            last = Some(event);
        }
        match last {
            Some(ThinEvent::Error(e)) => {
                assert!(
                    e.message.contains("stopped reading the request stream"),
                    "{}",
                    e.message
                );
            }
            other => panic!("{other:?}"),
        }
    }

    /// A stand-in SSE wire for the runner: `text`, `stop` (the end-of-answer marker) and an
    /// in-stream `429`.
    struct Lines {
        core: MapperCore,
    }

    impl SseMapper for Lines {
        fn core(&mut self) -> &mut MapperCore {
            &mut self.core
        }
        fn on_event(&mut self, event: &SseEvent) {
            match event.data.trim() {
                "text" => self.core.emit(ThinEvent::TextDelta("t".into())),
                "stop" => self
                    .core
                    .finish(ThinEvent::Finish(crate::thin::openai::FinishReason::Stop)),
                "429" => self.core.fail(limit_error(
                    "script",
                    "script 429: slow down",
                    "rate limit",
                    Some(5),
                )),
                _ => {}
            }
        }
        fn on_end(&mut self) {}
    }

    struct Stub {
        url: String,
    }

    impl VendorRequest for Stub {
        type Wire = Sse<Lines>;
        const WORDING: Wording = Wording {
            label: "script",
            host: "stand-in",
            login_hint: "script login",
            headers_sent: "authorization",
        };

        async fn build(
            &self,
            session: &VendorSession<'_>,
            _req: &ThinRequest,
            _token: &Token,
        ) -> Result<reqwest::RequestBuilder, ThinError> {
            Ok(session.http().post(&self.url))
        }

        fn wire(&self, model: &str, budgets: StreamBudgets) -> Self::Wire {
            Sse::new(Lines {
                core: MapperCore::new("script", model, budgets),
            })
        }

        fn budgets(&self, _req: &ThinRequest, cfg: &ThinConfig) -> StreamBudgets {
            StreamBudgets::from_config(&cfg.vendors.agy.budgets)
        }
    }

    /// What an adapter lends a [`VendorSession`].
    struct Parts {
        cli: CliBacked,
        state: VendorState,
        parts: VendorParts,
        http: reqwest::Client,
    }

    impl Parts {
        fn new(reply_timeout_seconds: u64) -> Self {
            Self {
                cli: CliBacked::new(
                    cli_token(),
                    || Ok(token(None)),
                    CliVersion::new("thickapi-no-such-cli"),
                    Some("1.0".into()),
                    "1.0",
                ),
                state: VendorState::new(),
                parts: VendorParts::new(&ThinConfig {
                    reply_timeout_seconds,
                    ..Default::default()
                }),
                http: reqwest::Client::new(),
            }
        }

        fn session(&self) -> VendorSession<'_> {
            self.parts.session("script", &self.cli, &self.state)
        }
    }

    fn thin_request() -> ThinRequest {
        use crate::config::models::{default_input, ModelEntry, Transport};
        ThinRequest {
            request: serde_json::from_value(serde_json::json!({
                "model": "subs/script/m",
                "messages": [{"role": "user", "content": "hi"}]
            }))
            .unwrap(),
            entry: ModelEntry {
                id: "subs/script/m".into(),
                backend: crate::types::BackendId::Grok,
                cli_model: "m".into(),
                context_window: 0,
                max_tokens: 0,
                thinking: false,
                cost: 0.0,
                name: String::new(),
                provider: None,
                transport: Transport::OauthDirect,
                vendor_model: None,
                enabled: true,
                input: default_input(),
            },
            effort: None,
            request_id: "chatcmpl-test".into(),
            path: "oauth",
            fallback: None,
            cursor_cwd: None,
        }
    }

    /// #126 through the runner: a 200 whose stream carries a 429 is a rejection; a stream the
    /// client leaves records nothing, however far it got; only a `Finish` is a success.
    #[tokio::test]
    async fn run_records_an_in_stream_429_as_a_rejection_and_ok_only_after_finish() {
        let parts = Parts::new(5);
        let req = thin_request();

        let url = serve(
            200,
            &[],
            vec![Ok("data: text\n\n"), Ok("data: 429\n\n")],
            false,
        )
        .await;
        let (tx, rx) = tokio::sync::mpsc::channel(8);
        parts
            .session()
            .run(&Stub { url }, &req, &tx)
            .await
            .expect("Started went out first, so the 429 is streamed");
        drop(tx);
        assert_eq!(collect(rx).await, ["Started", "TextDelta", "Error"]);
        assert_eq!(parts.state.successes(), 0);
        assert_eq!(parts.state.last_status(), Some(429));
        assert!(parts.state.last_rejection_excerpt().is_some());

        let url = serve(200, &[], vec![Ok("data: text\n\n")], true).await;
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        let stub = Stub { url };
        let session = parts.session();
        let run = session.run(&stub, &req, &tx);
        tokio::pin!(run);
        for _ in 0..2 {
            tokio::select! {
                _ = &mut run => panic!("the held stream ended early"),
                event = rx.recv() => assert!(!matches!(event, Some(ThinEvent::Error(_)))),
            }
        }
        assert_eq!(parts.state.successes(), 0, "no ok before a Finish");
        drop(rx);
        tokio::time::timeout(Duration::from_secs(2), run)
            .await
            .expect("the runner notices the client left")
            .unwrap();
        assert_eq!(
            (parts.state.successes(), parts.state.last_status()),
            (0, Some(429)),
            "an abandoned turn records nothing"
        );

        let url = serve(
            200,
            &[],
            vec![Ok("data: text\n\n"), Ok("data: stop\n\n")],
            false,
        )
        .await;
        let (tx, rx) = tokio::sync::mpsc::channel(8);
        parts.session().run(&Stub { url }, &req, &tx).await.unwrap();
        drop(tx);
        assert_eq!(collect(rx).await, ["Started", "TextDelta", "Finish"]);
        assert_eq!(
            (parts.state.successes(), parts.state.last_status()),
            (1, Some(200))
        );
    }

    /// #121 through the runner: a clean close after content, with no end-of-answer marker, is
    /// a truncated answer and a recorded failure — never `stop`.
    #[tokio::test]
    async fn run_fails_a_body_that_closes_before_its_end_marker() {
        let parts = Parts::new(5);
        let url = serve(200, &[], vec![Ok("data: text\n\n")], false).await;
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        parts
            .session()
            .run(&Stub { url }, &thin_request(), &tx)
            .await
            .unwrap();
        drop(tx);
        let mut last = None;
        while let Some(event) = rx.recv().await {
            assert!(!matches!(event, ThinEvent::Finish(_)), "{event:?}");
            last = Some(event);
        }
        match last {
            Some(ThinEvent::Error(e)) => {
                assert_eq!(e.code, "vendor_down");
                assert!(
                    e.message
                        .contains("script closed the stream before the answer ended"),
                    "{}",
                    e.message
                );
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(parts.state.successes(), 0);
        assert!(parts.state.last_rejection_excerpt().is_some());
    }

    /// A rejected request: the body is read, the vendor's `Retry-After` kept, the status mapped
    /// by `classify` and recorded — and nothing reaches the client, so the route answers it.
    #[tokio::test]
    async fn run_classifies_and_records_a_rejection_before_started() {
        let parts = Parts::new(5);
        let url = serve(
            429,
            &[("retry-after", "7")],
            vec![Ok(r#"{"error":{"message":"Rate limit exceeded"}}"#)],
            false,
        )
        .await;
        let (tx, rx) = tokio::sync::mpsc::channel(8);
        let err = parts
            .session()
            .run(&Stub { url }, &thin_request(), &tx)
            .await
            .expect_err("nothing was streamed");
        assert_eq!((err.status, err.retry_after_seconds()), (429, Some(7)));
        assert!(
            err.message.contains("Rate limit exceeded"),
            "{}",
            err.message
        );
        drop(tx);
        assert!(collect(rx).await.is_empty());
        assert_eq!(parts.state.last_status(), Some(429));
        assert_eq!(parts.state.successes(), 0);
    }

    /// #125: the body of a non-streaming exchange is inside the reply timeout, not after it.
    #[tokio::test]
    async fn fetch_json_times_out_a_stalled_body_at_the_reply_timeout() {
        let parts = Parts::new(1);
        let url = serve(200, &[], vec![Ok("{\"data\":")], true).await;
        let started = Instant::now();
        let err = parts
            .session()
            .fetch_json::<Value>("GET /v1/models", parts.http.get(url), |_, _| {
                panic!("a 200 is not a rejection")
            })
            .await
            .expect_err("stalled");
        assert_eq!(err.code, "vendor_down");
        assert!(
            err.message.contains("GET /v1/models timed out (1s)"),
            "{}",
            err.message
        );
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    /// A body that breaks is an error. `unwrap_or_default()` once read it as an empty model
    /// list; no caller of `fetch_json` can see a default.
    #[tokio::test]
    async fn fetch_json_fails_a_broken_body_instead_of_reading_it_as_empty() {
        let parts = Parts::new(5);
        let url = serve(200, &[], vec![Ok("{\"data\":["), Err(())], false).await;
        let got = parts
            .session()
            .fetch_json::<Value>("GET /v1/models", parts.http.get(url), |_, _| {
                panic!("a 200 is not a rejection")
            })
            .await;
        let err = got.expect_err("a broken body is an error, never a default");
        assert_eq!(err.code, "vendor_down");
        assert!(err.message.contains("body read failed"), "{}", err.message);

        // A rejected status is handed its body; a stand-in 500 names it.
        let url = serve(500, &[], vec![Ok("down")], false).await;
        let err = parts
            .session()
            .fetch_json::<Value>("GET /v1/models", parts.http.get(url), |status, text| {
                ThinError::vendor_down("script", format!("{status}: {text}"))
            })
            .await
            .expect_err("rejected");
        assert_eq!(err.message, "500: down");
    }

    /// No vendor reads a broken body as an empty answer: grok's model list and agy's
    /// `loadCodeAssist` both go through `fetch_json` (`unwrap_or_default()` on `.text()` once
    /// turned the first into an empty list).
    #[tokio::test]
    async fn no_vendor_reads_a_broken_body_as_an_empty_answer() {
        use super::super::{antigravity::AgyAdapter, grok::GrokAdapter};
        use crate::config::router::{AgyCfg, GrokCfg, Vendors};
        let broken = || serve(200, &[], vec![Ok("{\"data\":["), Err(())], false);

        let tmp = tempfile::tempdir().unwrap();
        let creds = tmp.path().join("auth.json");
        std::fs::write(
            &creds,
            serde_json::json!({ "https://auth.x.ai::client": {
                "key": "eyJ.unit.grok", "auth_mode": "oidc", "refresh_token": "r",
                "expires_at": "2999-01-01T00:00:00Z" } })
            .to_string(),
        )
        .unwrap();
        let grok = GrokAdapter::new(
            &ThinConfig {
                vendors: Vendors {
                    grok: GrokCfg {
                        base_url: Some(broken().await),
                        credentials_path: Some(creds),
                        client_version: Some("1.0.30".into()),
                        ..Default::default()
                    },
                    ..Default::default()
                },
                ..Default::default()
            },
            Arc::default(),
        );
        let err = grok.list_models().await.expect_err("never an empty list");
        assert_eq!(err.code, "vendor_down");
        assert!(err.message.contains("body read failed"), "{}", err.message);

        let agy = AgyAdapter::new(
            &ThinConfig {
                vendors: Vendors {
                    agy: AgyCfg {
                        base_url: Some(broken().await),
                        client_version: Some("1.2.4".into()),
                        ..Default::default()
                    },
                    ..Default::default()
                },
                ..Default::default()
            },
            Arc::default(),
        );
        let err = agy
            .build(&agy.session(), &thin_request(), &token(None))
            .await
            .expect_err("never a default project");
        assert_eq!(err.code, "vendor_down");
        assert!(
            err.message.contains("loadCodeAssist body read failed"),
            "{}",
            err.message
        );
    }

    #[test]
    fn usage_reads_each_vendors_keys() {
        let grok = UsageKeys {
            prompt: "prompt_tokens",
            completion: &["completion_tokens"],
            total: "total_tokens",
            cached: "/prompt_tokens_details/cached_tokens",
        };
        let u = usage_from(
            &serde_json::json!({"prompt_tokens": 7, "completion_tokens": 3,
                "prompt_tokens_details": {"cached_tokens": 5}}),
            &grok,
        );
        assert_eq!(
            (u.prompt_tokens, u.completion_tokens, u.total_tokens),
            (7, 3, 10),
            "total falls back to prompt + completion"
        );
        assert_eq!(u.prompt_tokens_details.unwrap().cached_tokens, 5);
        let agy = UsageKeys {
            prompt: "promptTokenCount",
            completion: &["candidatesTokenCount", "thoughtsTokenCount"],
            total: "totalTokenCount",
            cached: "/cachedContentTokenCount",
        };
        let u = usage_from(
            &serde_json::json!({"promptTokenCount": 10, "candidatesTokenCount": 2,
                "thoughtsTokenCount": 4, "totalTokenCount": 99}),
            &agy,
        );
        assert_eq!(
            (u.prompt_tokens, u.completion_tokens, u.total_tokens),
            (10, 6, 99)
        );
        assert_eq!(u.prompt_tokens_details.unwrap().cached_tokens, 0);
    }

    fn advisory(
        headers: &[(&'static str, &'static str)],
        source: &AdvisoryHeaders,
    ) -> Option<(f64, Option<String>)> {
        let mut map = reqwest::header::HeaderMap::new();
        for (k, v) in headers {
            map.insert(*k, v.parse().unwrap());
        }
        match rate_limit_advisory(&map, source)? {
            ThinEvent::RateLimitAdvisory {
                utilization,
                resets_at,
            } => Some((utilization, resets_at)),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn rate_limit_headers_read_as_remaining_or_as_utilization() {
        let grok = AdvisoryHeaders::Remaining {
            limit: "x-ratelimit-limit-tokens",
            remaining: "x-ratelimit-remaining-tokens",
        };
        assert_eq!(
            advisory(
                &[
                    ("x-ratelimit-limit-tokens", "200"),
                    ("x-ratelimit-remaining-tokens", "50")
                ],
                &grok
            ),
            Some((0.75, None))
        );
        assert_eq!(
            advisory(
                &[
                    ("x-ratelimit-limit-tokens", "0"),
                    ("x-ratelimit-remaining-tokens", "0")
                ],
                &grok
            ),
            None
        );
        assert_eq!(advisory(&[], &grok), None);

        let claude = AdvisoryHeaders::Utilization {
            utilization: &[
                "anthropic-ratelimit-unified-5h-utilization",
                "anthropic-ratelimit-unified-utilization",
            ],
            reset: &[
                "anthropic-ratelimit-unified-5h-reset",
                "anthropic-ratelimit-unified-reset",
            ],
        };
        let (used, reset) = advisory(
            &[
                ("anthropic-ratelimit-unified-utilization", "0.4"),
                ("anthropic-ratelimit-unified-reset", "0"),
            ],
            &claude,
        )
        .unwrap();
        assert_eq!(used, 0.4);
        assert_eq!(reset.as_deref(), Some("1970-01-01T00:00:00Z"));
        assert_eq!(
            advisory(
                &[("anthropic-ratelimit-unified-utilization", "1.5")],
                &claude
            ),
            None,
            "a utilisation outside 0..=1 is not believed"
        );
    }

    fn token(expires_at: Option<u64>) -> Token {
        Token {
            access_token: "t".into(),
            expires_at,
            source: "mem".into(),
            note: None,
        }
    }

    fn cli_token() -> CliToken {
        CliToken::new("grok", "thickapi-no-such-cli", "grok login")
            .refresh_with(&["models"], Duration::from_secs(1))
    }

    #[tokio::test]
    async fn a_token_without_an_expiry_is_never_refreshed() {
        let reads = AtomicUsize::new(0);
        let cli = cli_token();
        cli.fresh(|| {
            reads.fetch_add(1, Ordering::SeqCst);
            Ok(token(None))
        })
        .await
        .expect("usable");
        assert_eq!(reads.load(Ordering::SeqCst), 1, "no refresh path taken");
        assert!(cli.last_attempt.lock().await.is_none());
    }

    #[tokio::test]
    async fn an_expired_token_names_what_could_have_refreshed_it() {
        let expired = now_unix().saturating_sub(10);
        let cli = cli_token();
        let err = cli
            .fresh(|| Ok(token(Some(expired))))
            .await
            .expect_err("expired");
        assert_eq!(err.status, 401);
        assert!(
            err.message.contains("`thickapi-no-such-cli models`"),
            "{}",
            err.message
        );

        let override_file = cli_token().cli_owns_store(false);
        let err = override_file
            .fresh(|| Ok(token(Some(expired))))
            .await
            .expect_err("expired");
        assert!(
            err.message.contains("`router.yaml` override"),
            "{}",
            err.message
        );
        assert!(
            override_file.last_attempt.lock().await.is_none(),
            "no CLI run for it"
        );
    }

    #[tokio::test]
    async fn a_refresh_is_attempted_once_per_backoff_and_a_valid_token_stays_usable() {
        let closing = now_unix() + 10;
        let cli = cli_token();
        cli.fresh(|| Ok(token(Some(closing))))
            .await
            .expect("inside the window, still valid");
        let first = (*cli.last_attempt.lock().await).expect("the CLI was tried");
        cli.fresh(|| Ok(token(Some(closing))))
            .await
            .expect("still valid during back-off");
        assert_eq!(*cli.last_attempt.lock().await, Some(first), "no second run");
    }

    #[tokio::test]
    async fn waiters_reread_instead_of_refreshing_again() {
        let (soon, later) = (now_unix() + 10, now_unix() + 7200);
        let cli = cli_token();
        // Inside the window on the first read; by the re-read under the lock a peer refreshed.
        let reads = AtomicUsize::new(0);
        let got = cli
            .fresh(|| {
                let n = reads.fetch_add(1, Ordering::SeqCst);
                Ok(token(Some(if n == 0 { soon } else { later })))
            })
            .await
            .expect("fresh");
        assert_eq!(got.expires_at, Some(later));
        assert!(
            cli.last_attempt.lock().await.is_none(),
            "no CLI run was needed"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
    async fn concurrent_waiters_share_one_cli_refresh() {
        let cli = Arc::new(cli_token());
        let mut handles = Vec::new();
        for _ in 0..8 {
            let cli = cli.clone();
            handles.push(tokio::spawn(async move {
                cli.fresh(|| Ok(token(Some(now_unix() + 10)))).await
            }));
        }
        for handle in handles {
            handle.await.expect("join").expect("usable");
        }
        assert_eq!(cli.attempts.load(Ordering::SeqCst), 1);
        assert!(cli.last_attempt.lock().await.is_some());
    }

    fn cli_backed(override_version: Option<&str>) -> CliBacked {
        CliBacked::new(
            cli_token(),
            || Ok(token(Some(now_unix() + 10))),
            CliVersion::new("thickapi-no-such-cli"),
            override_version.map(str::to_string),
            "9.9.9",
        )
    }

    #[test]
    fn client_version_prefers_override_then_probe_then_fallback() {
        let backed = cli_backed(None).with_client_version_prefix("cli-");
        assert_eq!(backed.cli_version_now(), None, "no probe has landed yet");
        // No override: the fallback is used and gets the prefix an override never would.
        assert_eq!(backed.client_version_now(), "cli-9.9.9");
        let overridden = cli_backed(Some("pinned")).with_client_version_prefix("cli-");
        assert_eq!(overridden.client_version(), "pinned");
        assert_eq!(overridden.client_version_now(), "pinned");
    }

    #[tokio::test]
    async fn fresh_token_after_token_reuses_the_single_flight_path() {
        let backed = cli_backed(None);
        let direct = backed.token().expect("read");
        let fresh = backed.fresh_token().await.expect("fresh");
        assert_eq!(direct.access_token, fresh.access_token);
    }
}
