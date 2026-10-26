//! OAuth-direct adapters (SPEC §19.8): vendor HTTP APIs driven with the CLI's own token.

pub mod antigravity;
pub mod claude_oauth;
pub mod codex;
pub mod creds;
pub mod cursor;
pub mod grok;
pub mod sse;
pub mod state;
pub mod vendor;
#[cfg(windows)]
pub mod wincred;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crate::config::models::Transport;
use crate::config::router::{ClaudeOauthMode, ClaudeProbeConfig};
use crate::config::ConfigBundle;
use crate::thin::claude_bridge::{is_bridge_fault, ClaudeBridge};
use crate::thin::empty_stop;
use crate::thin::{AdapterFuture, EventSender, ModelAdapter, ThinError, ThinRequest};
use claude_oauth::{ClaudeOauth, OauthFailure};
use creds::now_unix;
use state::{redact_log, ProbeResult, VendorState};

pub use vendor::execute_open;

/// Longest the probe loop sleeps between schedule checks.
const PROBE_TICK: Duration = Duration::from_secs(60);

/// The Claude adapter: OAuth-direct first (per `claude_oauth.mode`), bridge as the fallback.
pub struct ClaudeAdapter {
    pub mode: ClaudeOauthMode,
    cooldown_seconds: u64,
    probe: ClaudeProbeConfig,
    /// Bridge → OAuth on a pre-`Started` bridge fault (SPEC §19.15 R3).
    cross_fallback: bool,
    oauth: ClaudeOauth,
    bridge: Arc<ClaudeBridge>,
    pub state: Arc<VendorState>,
    /// Real requests currently on the direct path; the probe stands aside while any exist.
    direct_in_flight: AtomicUsize,
}

impl ClaudeAdapter {
    pub fn new(cfg: &ConfigBundle, bridge: Arc<ClaudeBridge>, state: Arc<VendorState>) -> Self {
        let oauth_cfg = &cfg.router.claude_oauth;
        let oauth = ClaudeOauth::configured(
            oauth_cfg.budgets.clone(),
            oauth_cfg.base_url.clone(),
            oauth_cfg.credentials_path.clone(),
            (bridge.version_source(), bridge.claude_version()),
            &cfg.router.thin,
        );
        let probe_owned = oauth_cfg.mode == ClaudeOauthMode::Prefer && oauth_cfg.probe.enabled;
        state.set_probe_owned(probe_owned);
        Self {
            mode: oauth_cfg.mode,
            cooldown_seconds: oauth_cfg.retry_after_hours.saturating_mul(3600),
            probe: oauth_cfg.probe.clone(),
            cross_fallback: oauth_cfg.cross_fallback,
            oauth,
            bridge,
            state,
            direct_in_flight: AtomicUsize::new(0),
        }
    }

    /// `true` when `serve` should run the background probe (SPEC §19.15 R2): `prefer` with
    /// `probe.enabled`. `only` and `off` are unchanged.
    pub fn probe_active(&self) -> bool {
        self.state.probe_owned()
    }

    pub fn probe_config(&self) -> &ClaudeProbeConfig {
        &self.probe
    }

    /// One probe of the direct path, sent through the same runner as a request
    /// ([`ClaudeOauth::probe`]: `in_flight` permit, `reply_timeout_seconds`, `classify`). Owns
    /// the state transitions: 200 → `mark_ok`, a §19.8.1 block → `mark_blocked` with a fresh
    /// cooldown, anything else → unchanged and retried after `probe.retry_minutes`. Skipped
    /// when a real request is already on the direct path.
    pub async fn probe_once(&self) -> ProbeResult {
        let now = now_unix();
        let retry_at = Some(now + self.probe.retry_minutes.max(1) * 60);
        if self.direct_in_flight.load(Ordering::SeqCst) > 0 {
            tracing::debug!("claude oauth probe skipped: a request is on the direct path");
            self.state.record_probe(ProbeResult::Skipped, None);
            return ProbeResult::Skipped;
        }
        let token = match self.token() {
            Ok(t) if !t.expired() => t,
            Ok(_) => {
                tracing::info!(
                    "claude oauth probe deferred: token expired (a bridge run refreshes it)"
                );
                let r = ProbeResult::Error("token_expired".into());
                self.state.record_probe(r.clone(), retry_at);
                return r;
            }
            Err(e) => {
                tracing::info!(error = %e, "claude oauth probe deferred: no token");
                let r = ProbeResult::Error("no_token".into());
                self.state.record_probe(r.clone(), retry_at);
                return r;
            }
        };
        let outcome = self
            .oauth
            .probe(&token, &self.probe.model, &self.state)
            .await;
        let result = match outcome {
            Ok(()) => {
                tracing::info!(model = %self.probe.model, "claude oauth probe ok: direct path open");
                (ProbeResult::Ok, None)
            }
            Err(OauthFailure::Blocked(reason)) => {
                tracing::warn!(model = %self.probe.model, reason = %redact_log(&reason), "claude oauth probe blocked");
                self.state.mark_blocked(reason, self.cooldown_seconds);
                let next = self.state.blocked().map(|b| b.next_probe);
                (ProbeResult::Blocked, next)
            }
            Err(OauthFailure::SuspectedBlock(reason)) => {
                let n = self.state.note_suspected_block();
                tracing::warn!(model = %self.probe.model, reason = %redact_log(&reason), count = n, "claude oauth probe suspected block");
                if n >= 2 {
                    self.state.mark_blocked(reason, self.cooldown_seconds);
                    let next = self.state.blocked().map(|b| b.next_probe);
                    (ProbeResult::Blocked, next)
                } else {
                    (ProbeResult::Error("suspected_block".into()), retry_at)
                }
            }
            Err(OauthFailure::Error(e)) => {
                tracing::warn!(model = %self.probe.model, code = e.code, status = e.status, message = %redact_log(&e.message), "claude oauth probe error; state unchanged");
                (ProbeResult::Error(e.code.to_string()), retry_at)
            }
        };
        self.state.record_probe(result.0.clone(), result.1);
        result.0
    }

    /// The `serve` task (R2): probe on start when not blocked (`probe.on_start`), at
    /// `next_probe` while blocked, and at the loop's own retry time after an error.
    pub async fn probe_loop(self: Arc<Self>) {
        if let Some(b) = self.state.blocked() {
            tracing::info!(reason = %redact_log(&b.reason), next_probe = %iso(b.next_probe), "claude oauth starts blocked (persisted); bridge until the probe clears it");
            self.state.schedule_probe(b.next_probe);
        } else if self.probe.on_start {
            self.state.schedule_probe(now_unix());
        }
        loop {
            let now = now_unix();
            let due = match self.state.probe_next_at() {
                Some(at) => Some(at),
                None => self.state.blocked().map(|b| b.next_probe),
            };
            match due {
                Some(at) if at <= now => {
                    self.probe_once().await;
                }
                Some(at) => {
                    tokio::time::sleep(Duration::from_secs(at - now).min(PROBE_TICK)).await;
                }
                None => tokio::time::sleep(PROBE_TICK).await,
            }
        }
    }

    pub fn bridge(&self) -> &Arc<ClaudeBridge> {
        &self.bridge
    }

    pub fn oauth(&self) -> &ClaudeOauth {
        &self.oauth
    }

    /// Record the OAuth-direct wire exchange (`thickapi capture claude`). The bridge has its
    /// own tap, set before it was shared.
    pub fn set_tap(&mut self, tap: crate::thin::capture::Tap) {
        self.oauth.set_tap(tap);
    }

    /// The CLI's stored token (config override or the default Claude credentials file).
    pub fn token(&self) -> Result<creds::Token, creds::CredError> {
        self.oauth.cli().token()
    }

    /// Anthropic's live model ids for `thickapi models check` (SPEC §10.3); needs a live token.
    pub async fn list_models(&self) -> Result<Vec<String>, ThinError> {
        let token = self.token().map_err(|e| {
            ThinError::vendor_login_required(
                claude_oauth::VENDOR,
                format!("claude oauth: {e}"),
                "claude login",
            )
        })?;
        if token.expired() {
            return Err(ThinError::vendor_login_required(
                claude_oauth::VENDOR,
                "claude oauth token expired (a `claude -p` run or `claude login` refreshes it)",
                "claude login",
            ));
        }
        self.oauth.list_models(&token).await
    }

    /// Which path a request for `model` would take right now (`oauth` | `bridge`), with reason.
    /// Direct blocked **and** bridge drifted is the documented double fault (R3): still
    /// `bridge`, reason `direct blocked; bridge drift: <assertion>`.
    ///
    /// [`planned_path`] is the non-Haiku / default row. After a non-Haiku block, Haiku may
    /// still stay on oauth (SPEC §19.8.1 / §19.15) — use [`planned_path_for`].
    pub fn planned_path(&self) -> (&'static str, String) {
        self.planned_path_for("")
    }

    /// Like [`planned_path`], but honour the Haiku exception when `wire_model` is Haiku.
    pub fn planned_path_for(&self, wire_model: &str) -> (&'static str, String) {
        match self.mode {
            ClaudeOauthMode::Off => ("bridge", "claude_oauth.mode = off".into()),
            _ => match self.token() {
                Err(e) => ("bridge", format!("no usable token: {e}")),
                Ok(t) if t.expired() => (
                    "bridge",
                    "token expired; the bridge run refreshes it".into(),
                ),
                Ok(_) => {
                    if let Some(b) = self.state.blocked() {
                        if !self.state.should_try_direct() {
                            if claude_oauth::model_is_haiku(wire_model) {
                                return (
                                    "oauth",
                                    "Haiku stays on the direct path while non-Haiku is blocked"
                                        .into(),
                                );
                            }
                            if let Some(d) = self.bridge.state().drift() {
                                return (
                                    "bridge",
                                    format!("direct blocked; bridge drift: {}", d.reason),
                                );
                            }
                            let probe_at = self.state.probe_next_at().unwrap_or(b.next_probe);
                            return (
                                "bridge",
                                format!("blocked: {} (probe at {})", b.reason, iso(probe_at)),
                            );
                        }
                        return (
                            "oauth",
                            "cooldown passed; next request probes the direct path".into(),
                        );
                    }
                    ("oauth", "token present, not blocked".into())
                }
            },
        }
    }

    /// Choose the path (SPEC §19.15) and hand the request to it: the direct path through
    /// [`ClaudeOauth`]'s `VendorSession` run, the bridge through its own run (§19.7).
    async fn route(&self, req: &ThinRequest, tx: &EventSender) -> Result<(), ThinError> {
        if self.mode == ClaudeOauthMode::Off {
            return self
                .run_bridge_first(req, tx, "claude_oauth.mode = off")
                .await;
        }
        // A row pinned to `cli_bridge` goes to the bridge first; under `prefer` it may still
        // fall to the direct path on a bridge fault (R3), like any other bridge-first request.
        if req.entry.transport == Transport::CliBridge {
            return self
                .run_bridge_first(req, tx, "row transport = cli_bridge")
                .await;
        }
        let token = match self.token() {
            Ok(t) => t,
            Err(e) => {
                if self.mode == ClaudeOauthMode::Only {
                    return Err(ThinError::vendor_login_required(
                        claude_oauth::VENDOR,
                        format!("claude oauth: {e}"),
                        "claude login",
                    )
                    .with_path("oauth", None, None));
                }
                return self
                    .run_bridge_first(req, tx, &format!("no usable token: {e}"))
                    .await;
            }
        };
        if token.expired() {
            // The bridge's claude.exe run refreshes .credentials.json as a side effect.
            if self.mode == ClaudeOauthMode::Only {
                return Err(ThinError::vendor_login_required(
                    claude_oauth::VENDOR,
                    "claude oauth token expired (a `claude -p` run or `claude login` refreshes it)",
                    "claude login",
                )
                .with_path("oauth", None, Some("token_expired")));
            }
            tracing::info!(
                request_id = %req.request_id,
                expires_at = token.expires_at,
                "claude oauth token expired; handing off to the bridge"
            );
            let result = self
                .run_bridge_first(req, tx, "token expired; the bridge run refreshes it")
                .await;
            match self.token() {
                Ok(t) => tracing::info!(
                    request_id = %req.request_id,
                    expires_at = t.expires_at,
                    "claude oauth token after bridge run"
                ),
                Err(e) => tracing::info!(
                    request_id = %req.request_id,
                    error = %e,
                    "claude oauth token unread after bridge run"
                ),
            }
            return result;
        }
        if self.mode == ClaudeOauthMode::Prefer && !self.state.should_try_direct() {
            if !claude_oauth::model_is_haiku(req.entry.wire_model()) {
                return self.run_bridge_first(req, tx, "direct path blocked").await;
            }
            tracing::info!(
                request_id = %req.request_id,
                model = %req.entry.wire_model(),
                "claude haiku stays on the direct path while non-Haiku is blocked"
            );
        }

        tracing::info!(request_id = %req.request_id, model = %req.entry.id, path = "oauth", "claude thin request");
        let outcome = self
            .run_direct(&req.with_serving("oauth", None), tx, &token)
            .await?;
        match outcome {
            Ok(()) => Ok(()),
            Err(OauthFailure::Error(e)) => Err(e.with_path("oauth", None, None)),
            Err(OauthFailure::Blocked(reason)) => {
                self.replay_on_bridge(req, tx, reason, true).await
            }
            Err(OauthFailure::SuspectedBlock(reason)) => {
                let n = self.state.note_suspected_block();
                tracing::warn!(request_id = %req.request_id, reason = %redact_log(&reason), count = n, "claude oauth suspected block");
                self.replay_on_bridge(req, tx, reason, n >= 2).await
            }
        }
    }

    /// One direct-path run under the in-flight cap, counted for the probe's stand-aside check.
    /// The outer `Err` is the closed-semaphore case only.
    async fn run_direct(
        &self,
        req: &ThinRequest,
        tx: &EventSender,
        token: &creds::Token,
    ) -> Result<Result<(), OauthFailure>, ThinError> {
        let permit = self.oauth.permit().await?;
        let _guard = InFlight::enter(&self.direct_in_flight);
        // §19.16: an empty tool stop reruns the direct path once under the same permit.
        let permit = &permit;
        let (outcome, started, kept) = empty_stop::run_guarded(
            req,
            tx,
            || None,
            |_, attempt_req, inner| async move {
                self.oauth
                    .run_attempt(&attempt_req, &inner, token, &self.state, Some(permit))
                    .await
            },
        )
        .await;
        // A success counts only for an answer the guard kept (#126, §19.16).
        if kept && matches!(outcome, Ok(true)) {
            claude_oauth::mark_success(&self.state, req.entry.wire_model(), 200);
        }
        let outcome = outcome.map(|_| ());
        if !started {
            return Ok(outcome);
        }
        // The retry failed after the first attempt's `Started` reached the route: report it
        // on the channel. A block here must not replay on the bridge (no path change).
        let err = match outcome {
            Ok(()) => return Ok(Ok(())),
            Err(OauthFailure::Error(e)) => e,
            Err(OauthFailure::Blocked(reason)) => {
                self.state
                    .mark_blocked(reason.clone(), self.cooldown_seconds);
                ThinError::vendor_down(
                    claude_oauth::VENDOR,
                    format!("claude oauth blocked: {reason}"),
                )
            }
            Err(OauthFailure::SuspectedBlock(reason)) => {
                self.state.note_suspected_block();
                ThinError::vendor_down(claude_oauth::VENDOR, format!("claude oauth: {reason}"))
            }
        };
        let _ = tx.send(crate::thin::ThinEvent::Error(err)).await;
        Ok(Ok(()))
    }

    /// One bridge run under the §19.16 guard: the spawn slot is held across the same-path
    /// retry, and a retry failure after `Started` goes on the channel, so the caller's
    /// bridge → direct crossing (R3) only ever sees a failure from before `Started`.
    async fn run_bridge_guarded(
        &self,
        req: &ThinRequest,
        tx: &EventSender,
    ) -> Result<(), ThinError> {
        let _slot = self.bridge.acquire_slot(tx).await?;
        let bridge = &self.bridge;
        let (result, started, _) = empty_stop::run_guarded(
            req,
            tx,
            || bridge.claude_version().map(|v| format!("claude {v}")),
            |n, attempt_req, inner| async move {
                bridge
                    .run_bridge_attempt(&attempt_req, &inner, Some(n))
                    .await
            },
        )
        .await;
        match result {
            Err(err) if started => {
                let _ = tx.send(crate::thin::ThinEvent::Error(err)).await;
                Ok(())
            }
            other => other,
        }
    }

    /// The bridge as the chosen path. On a bridge fault before `Started` (R3) — under `prefer`
    /// with `cross_fallback`, a live token and a direct state that is not blocked — the direct
    /// path runs once. A request is served by at most two attempts, one per path; on a double
    /// failure the bridge's error is returned and both are logged. Direct blocked and bridge
    /// drifted is the documented double fault: the bridge's error with the combined reason.
    async fn run_bridge_first(
        &self,
        req: &ThinRequest,
        tx: &EventSender,
        why: &str,
    ) -> Result<(), ThinError> {
        tracing::info!(request_id = %req.request_id, model = %req.entry.id, path = "bridge", reason = why, "claude thin request");
        self.state.mark_bridge_used();
        let reason = path_reason_from(why);
        let bridge_req = req.with_serving("bridge", None);
        let err = match self.run_bridge_guarded(&bridge_req, tx).await {
            Ok(()) => return Ok(()),
            Err(e) => e,
        };
        if !is_bridge_fault(&err) || self.mode != ClaudeOauthMode::Prefer {
            return Err(err.with_path("bridge", None, reason));
        }
        // Re-read the state now, not at routing time: a probe may have reopened the direct
        // path while the bridge was failing.
        if let Some(b) = self.state.blocked() {
            tracing::warn!(request_id = %req.request_id, direct = %redact_log(&b.reason), bridge = %redact_log(&err.message), "claude double fault: direct blocked and bridge drift");
            return Err(double_fault(err, &b.reason));
        }
        if !self.cross_fallback {
            tracing::warn!(request_id = %req.request_id, bridge = %redact_log(&err.message), "claude bridge fault; cross_fallback off");
            return Err(err.with_path("bridge", None, reason));
        }
        let token = match self.token() {
            Ok(t) if !t.expired() => t,
            _ => {
                tracing::warn!(request_id = %req.request_id, bridge = %redact_log(&err.message), "claude bridge fault; no live token for the direct path");
                return Err(err.with_path("bridge", None, reason));
            }
        };
        tracing::warn!(request_id = %req.request_id, model = %req.entry.id, path = "oauth", fallback = "bridge->oauth", bridge = %redact_log(&err.message), "claude bridge fault before Started; trying the direct path");
        let cross = req.with_serving("oauth", Some("bridge->oauth"));
        match self.run_direct(&cross, tx, &token).await? {
            Ok(()) => Ok(()),
            Err(OauthFailure::Error(e)) => {
                tracing::warn!(request_id = %req.request_id, bridge = %redact_log(&err.message), direct = %redact_log(&e.message), "claude double failure: both paths failed; returning the bridge's error");
                Err(err.with_path("bridge", Some("bridge->oauth"), reason))
            }
            Err(OauthFailure::Blocked(reason)) => {
                self.state
                    .mark_blocked(reason.clone(), self.cooldown_seconds);
                tracing::warn!(request_id = %req.request_id, direct = %redact_log(&reason), bridge = %redact_log(&err.message), "claude double fault: direct blocked and bridge drift");
                Err(double_fault(err, &reason))
            }
            Err(OauthFailure::SuspectedBlock(reason)) => {
                let n = self.state.note_suspected_block();
                if n >= 2 {
                    self.state
                        .mark_blocked(reason.clone(), self.cooldown_seconds);
                    tracing::warn!(request_id = %req.request_id, direct = %redact_log(&reason), bridge = %redact_log(&err.message), "claude double fault: direct blocked and bridge drift");
                    Err(double_fault(err, &reason))
                } else {
                    tracing::warn!(request_id = %req.request_id, bridge = %redact_log(&err.message), direct = %redact_log(&reason), "claude double failure: both paths failed; returning the bridge's error");
                    Err(err.with_path("bridge", Some("bridge->oauth"), None))
                }
            }
        }
    }

    /// Replay a direct-path block (or suspected block) on the bridge. `mark` is a confirmed
    /// block: persist it and, on a bridge fault, return the combined double-fault error.
    async fn replay_on_bridge(
        &self,
        req: &ThinRequest,
        tx: &EventSender,
        reason: String,
        mark: bool,
    ) -> Result<(), ThinError> {
        if mark {
            tracing::warn!(request_id = %req.request_id, reason = %redact_log(&reason), "claude oauth blocked");
            self.state
                .mark_blocked(reason.clone(), self.cooldown_seconds);
        }
        if self.mode == ClaudeOauthMode::Only {
            return Err(ThinError::vendor_drift(
                claude_oauth::VENDOR,
                format!("claude oauth blocked (mode = only, no fallback): {reason}"),
            )
            .with_path("oauth", None, Some("blocked")));
        }
        tracing::info!(request_id = %req.request_id, model = %req.entry.id, path = "bridge", fallback = "oauth->bridge", "claude thin request replayed");
        let replay = req.with_serving("bridge", Some("oauth->bridge"));
        match self.run_bridge_guarded(&replay, tx).await {
            Ok(()) => Ok(()),
            Err(err) if is_bridge_fault(&err) && (mark || self.state.is_blocked()) => {
                tracing::warn!(request_id = %req.request_id, direct = %redact_log(&reason), bridge = %redact_log(&err.message), "claude double fault: direct blocked and bridge drift");
                Err(double_fault(err, &reason))
            }
            Err(err) => Err(err.with_path(
                "bridge",
                Some("oauth->bridge"),
                if mark { Some("blocked") } else { None },
            )),
        }
    }
}

/// The bridge's error carrying both reasons (SPEC §19.15 R3 double fault).
fn double_fault(mut bridge_err: ThinError, blocked_reason: &str) -> ThinError {
    bridge_err.message = format!(
        "direct blocked: {blocked_reason}; bridge drift: {}",
        bridge_err.message
    );
    bridge_err.with_path("bridge", Some("oauth->bridge"), Some("blocked"))
}

fn path_reason_from(why: &str) -> Option<&'static str> {
    let w = why.to_ascii_lowercase();
    if w.contains("token expired") {
        Some("token_expired")
    } else if w.contains("direct blocked") || w.starts_with("blocked:") {
        Some("blocked")
    } else if w.contains("mode = off") {
        Some("mode")
    } else if w.contains("cli_bridge") {
        Some("row")
    } else {
        None
    }
}

fn iso(unix: u64) -> String {
    chrono::DateTime::<chrono::Utc>::from_timestamp(unix as i64, 0)
        .map(|t| t.format("%Y-%m-%dT%H:%M:%SZ").to_string())
        .unwrap_or_else(|| unix.to_string())
}

/// Counts a real request on the direct path for the probe's stand-aside check.
struct InFlight<'a>(&'a AtomicUsize);

impl<'a> InFlight<'a> {
    fn enter(counter: &'a AtomicUsize) -> Self {
        counter.fetch_add(1, Ordering::SeqCst);
        Self(counter)
    }
}

impl Drop for InFlight<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

impl ModelAdapter for ClaudeAdapter {
    fn vendor(&self) -> &'static str {
        claude_oauth::VENDOR
    }

    fn run<'a>(&'a self, req: &'a ThinRequest, tx: EventSender) -> AdapterFuture<'a> {
        // Post-`Started` failures are already delivered as `ThinEvent::Error` by the inner
        // adapters; anything returned here happened before the route committed to a 200.
        Box::pin(async move { self.route(req, &tx).await })
    }
}
