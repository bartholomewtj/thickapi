//! Per-vendor OAuth path state for `/ready`, `doctor` and the Claude block → bridge fallback
//! (SPEC §19.8.1). In memory; the Claude state is additionally mirrored to
//! `~/.thickapi/thin/claude_oauth_state.json` (§19.15 R1) so a `serve` restart remembers a
//! block, and owned by the background probe when one is enabled (§19.15 R2).

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::creds::now_unix;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Blocked {
    pub reason: String,
    pub since: u64,
    pub next_probe: u64,
}

/// Outcome of the last background probe (R2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProbeResult {
    Ok,
    Blocked,
    /// A non-block failure (`code` is the thin error code, e.g. `vendor_down`).
    Error(String),
    /// A real direct-path request was already in flight; it served as the probe.
    Skipped,
}

impl ProbeResult {
    fn label(&self) -> String {
        match self {
            ProbeResult::Ok => "ok".into(),
            ProbeResult::Blocked => "blocked".into(),
            ProbeResult::Error(code) => format!("error({code})"),
            ProbeResult::Skipped => "skipped".into(),
        }
    }
}

#[derive(Debug, Default, Clone)]
struct Probe {
    last_at: Option<u64>,
    last_result: Option<ProbeResult>,
    next_at: Option<u64>,
}

/// On-disk mirror (R1). Classification string only: no bodies, tokens or prompt text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Persisted {
    blocked: Option<Blocked>,
    last_path: Option<String>,
    cli_version: Option<String>,
    updated_at: u64,
}

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rejection {
    pub classification: String,
    pub excerpt: String,
    /// HTTP status of the rejection.
    #[serde(default)]
    pub status: u16,
    /// Unix seconds when it was recorded.
    #[serde(default)]
    pub at: u64,
    /// The vendor's own wait (`Retry-After` or a body hint), seconds from `at`.
    #[serde(default)]
    pub retry_after: Option<u64>,
    /// Event order against [`VendorState::mark_ok`]: a success after this rejection
    /// supersedes it for `/ready` `available`.
    #[serde(skip)]
    seq: u64,
}

/// `/ready` → `thin.vendors.<vendor>.{available, retry_in_seconds, reason, rows_unavailable}`
/// (#110): one yes/no a client can read before it spawns a seat, instead of parsing each
/// vendor's own shape.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Availability {
    pub available: bool,
    pub retry_in_seconds: Option<u64>,
    pub reason: Option<String>,
    pub rows_unavailable: Vec<String>,
}

impl Default for Availability {
    fn default() -> Self {
        Self {
            available: true,
            retry_in_seconds: None,
            reason: None,
            rows_unavailable: Vec::new(),
        }
    }
}

impl Availability {
    /// Mark the vendor unavailable. With several causes the longest known wait wins, and a
    /// cause with no known end (`None`) wins over any wait.
    pub fn block(&mut self, reason: impl Into<String>, retry_in_seconds: Option<u64>) {
        if self.available {
            self.retry_in_seconds = retry_in_seconds;
            self.reason = Some(reason.into());
        } else {
            match (self.retry_in_seconds, retry_in_seconds) {
                (Some(old), Some(new)) if new > old => {
                    self.retry_in_seconds = Some(new);
                    self.reason = Some(reason.into());
                }
                (Some(_), None) => {
                    self.retry_in_seconds = None;
                    self.reason = Some(reason.into());
                }
                _ => {}
            }
        }
        self.available = false;
    }

    /// Write the four fields into a vendor's `/ready` object.
    pub fn attach(&self, vendor: &mut Value) {
        crate::thin::json_insert(vendor, "available", json!(self.available));
        crate::thin::json_insert(vendor, "retry_in_seconds", json!(self.retry_in_seconds));
        crate::thin::json_insert(
            vendor,
            "reason",
            json!(self.reason.as_deref().map(redact_log)),
        );
        crate::thin::json_insert(vendor, "rows_unavailable", json!(self.rows_unavailable));
    }

    /// Read the four fields back from a vendor's `/ready` object (the API folds account
    /// health on top of the runtime's view).
    pub fn from_json(vendor: &Value) -> Self {
        Self {
            available: vendor
                .get("available")
                .and_then(Value::as_bool)
                .unwrap_or(true),
            retry_in_seconds: vendor.get("retry_in_seconds").and_then(Value::as_u64),
            reason: vendor
                .get("reason")
                .and_then(Value::as_str)
                .map(str::to_string),
            rows_unavailable: vendor
                .get("rows_unavailable")
                .and_then(Value::as_array)
                .map(|rows| {
                    rows.iter()
                        .filter_map(|r| r.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default(),
        }
    }
}

/// How long a thin rejection keeps a vendor `available: false` when the vendor named no
/// wait of its own (#110). A success recorded after the rejection clears it sooner.
pub fn rejection_window(code: &str) -> Option<u64> {
    match code {
        "rate_limit_error" => Some(60),
        "insufficient_quota" | "subscription_blocked" => Some(15 * 60),
        "vendor_login_required" => Some(15 * 60),
        "vendor_down" | "vendor_drift" | "overloaded_error" => Some(120),
        _ => None,
    }
}

/// A row id that answered `not_found` stays in `rows_unavailable` this long (#110).
const ROW_UNAVAILABLE_SECONDS: u64 = 60 * 60;

/// A Connect error that is true of one catalogue row, not the vendor (SPEC §19.8 C4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RowRejection {
    pub wire: String,
    pub classification: String,
    pub status: u16,
}

#[derive(Debug, Default, Clone)]
struct Inner {
    blocked: Option<Blocked>,
    last_status: Option<u16>,
    /// Last vendor rejection (classification and excerpt).
    last_rejection: Option<Rejection>,
    /// Last per-row Connect fact that must not paint the whole vendor (Cursor `not_found`).
    last_row: Option<RowRejection>,
    /// Catalogue rows the vendor answered `not_found` for, with when (`rows_unavailable`).
    rows_unavailable: std::collections::BTreeMap<String, u64>,
    /// Event counter ordering rejections against successes.
    seq: u64,
    /// `seq` of the last success.
    ok_seq: u64,
    successes: u64,
    /// Which path served the last Claude request (`oauth` | `bridge`).
    last_path: Option<&'static str>,
    /// `true` when a background probe owns the block → ok transition (R2). The cooldown
    /// alone then never opens the direct path to user traffic.
    probe_owned: bool,
    probe: Probe,
    consecutive_suspected: u32,
    /// Last successful write (or load) of the mirror file.
    persisted: bool,
    /// What the file last held, to skip identical rewrites.
    last_written: Option<Persisted>,
}

#[derive(Debug, Default)]
pub struct VendorState {
    inner: Mutex<Inner>,
    persist_path: Option<PathBuf>,
    cli_version: Option<String>,
    /// Serialises file writes (probe task and requests share one temp name). Never held
    /// together with `inner`.
    io: Mutex<()>,
}

impl VendorState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Mirror the state to `path` (R1). Reads the file once; a missing, malformed or
    /// `next_probe`-in-the-past file means "not blocked". Never creates the file on load.
    pub fn with_persistence(path: PathBuf, cli_version: Option<String>) -> Self {
        let state = Self {
            inner: Mutex::default(),
            persist_path: Some(path.clone()),
            cli_version,
            io: Mutex::new(()),
        };
        if let Some(p) = load(&path) {
            let mut inner = state.lock();
            inner.blocked = p.blocked.clone().filter(|b| now_unix() < b.next_probe);
            inner.last_path = match p.last_path.as_deref() {
                Some("oauth") => Some("oauth"),
                Some("bridge") => Some("bridge"),
                _ => None,
            };
            inner.persisted = true;
            inner.last_written = Some(p);
        }
        state
    }

    pub fn persist_path(&self) -> Option<&Path> {
        self.persist_path.as_deref()
    }

    /// Last background probe timestamps/result for `doctor` (SPEC §19.15 R5).
    pub fn probe_view(&self) -> (Option<u64>, Option<String>, Option<u64>) {
        let inner = self.lock();
        (
            inner.probe.last_at,
            inner.probe.last_result.as_ref().map(ProbeResult::label),
            inner.probe.next_at,
        )
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|p| p.into_inner())
    }

    pub fn is_blocked(&self) -> bool {
        self.blocked().is_some()
    }

    pub fn blocked(&self) -> Option<Blocked> {
        self.lock().blocked.clone()
    }

    /// Hand the block → ok transition to the background probe (R2).
    pub fn set_probe_owned(&self, owned: bool) {
        self.lock().probe_owned = owned;
    }

    pub fn probe_owned(&self) -> bool {
        self.lock().probe_owned
    }

    /// `true` when the direct path should be attempted now. Probe-owned: never while blocked.
    /// Otherwise (T1 behaviour): not blocked, or the cooldown has passed and this request is
    /// the probe.
    pub fn should_try_direct(&self) -> bool {
        let inner = self.lock();
        match &inner.blocked {
            None => true,
            Some(_) if inner.probe_owned => false,
            Some(b) => now_unix() >= b.next_probe,
        }
    }

    pub fn mark_blocked(&self, reason: impl Into<String>, cooldown_seconds: u64) {
        let now = now_unix();
        let reason = reason.into();
        let write = {
            let mut inner = self.lock();
            let next_probe = now + cooldown_seconds;
            inner.blocked = Some(Blocked {
                reason,
                since: inner.blocked.as_ref().map(|b| b.since).unwrap_or(now),
                next_probe,
            });
            inner.consecutive_suspected = 0;
            inner.last_path = Some("bridge");
            if inner.probe_owned {
                inner.probe.next_at = Some(next_probe);
            }
            self.pending_write(&mut inner)
        };
        self.write(write);
    }

    pub fn mark_ok(&self, status: u16) {
        let write = {
            let mut inner = self.lock();
            inner.blocked = None;
            inner.consecutive_suspected = 0;
            inner.last_status = Some(status);
            inner.successes += 1;
            inner.last_path = Some("oauth");
            if inner.probe_owned {
                inner.probe.next_at = None;
            }
            inner.seq += 1;
            inner.ok_seq = inner.seq;
            self.pending_write(&mut inner)
        };
        self.write(write);
    }

    /// A 200 from a Haiku model. While non-Haiku is blocked, Haiku stays on the direct path
    /// (SPEC §19.8.1); its success says nothing about the block, so it must not clear it —
    /// only the probe or a non-Haiku success reopens the path (§19.15 R2, audit R2).
    pub fn mark_ok_haiku(&self, status: u16) {
        let blocked = {
            let mut inner = self.lock();
            if inner.blocked.is_some() {
                inner.last_status = Some(status);
                inner.successes += 1;
                inner.seq += 1;
                inner.ok_seq = inner.seq;
                true
            } else {
                false
            }
        };
        if !blocked {
            self.mark_ok(status);
        }
    }

    /// Count a suspected-block 400. Returns the new consecutive count (SPEC §19.8.1).
    pub fn note_suspected_block(&self) -> u32 {
        let mut inner = self.lock();
        inner.consecutive_suspected = inner.consecutive_suspected.saturating_add(1);
        inner.consecutive_suspected
    }

    pub fn consecutive_suspected(&self) -> u32 {
        self.lock().consecutive_suspected
    }

    pub fn mark_rejection(&self, status: u16, classification: &str, body: &str) {
        self.mark_rejection_retry(status, classification, body, None);
    }

    /// [`Self::mark_rejection`] with the vendor's own wait, so `/ready` can say when the
    /// vendor is worth trying again (#110).
    pub fn mark_rejection_retry(
        &self,
        status: u16,
        classification: &str,
        body: &str,
        retry_after: Option<u64>,
    ) {
        let mut inner = self.lock();
        inner.last_status = Some(status);
        inner.seq += 1;
        let seq = inner.seq;
        inner.last_rejection = Some(Rejection {
            classification: classification.to_string(),
            excerpt: rejection_excerpt(body),
            status,
            at: now_unix(),
            retry_after,
            seq,
        });
    }

    /// A catalogue row the vendor does not serve (`not_found`): `/ready` lists it in
    /// `rows_unavailable` so a client can skip that row and keep the vendor (#110).
    pub fn mark_row_unavailable(&self, row: &str) {
        self.lock()
            .rows_unavailable
            .insert(row.to_string(), now_unix());
    }

    /// The vendor's `/ready` availability from its own record: a rejection that no later
    /// success superseded and whose window (the vendor's wait, else [`rejection_window`])
    /// has not run out. `ignore` lists classifications that do not stop the vendor (Claude:
    /// a direct-path block or 401 falls to the bridge).
    pub fn availability(&self, now: u64, ignore: &[&str]) -> Availability {
        let inner = self.lock();
        let mut out = Availability {
            rows_unavailable: inner
                .rows_unavailable
                .iter()
                .filter(|(_, at)| now < at.saturating_add(ROW_UNAVAILABLE_SECONDS))
                .map(|(row, _)| row.clone())
                .collect(),
            ..Availability::default()
        };
        let Some(r) = &inner.last_rejection else {
            return out;
        };
        let code = r.classification.as_str();
        if r.seq <= inner.ok_seq || ignore.contains(&code) {
            return out;
        }
        let Some(window) = r.retry_after.or_else(|| rejection_window(code)) else {
            return out;
        };
        let until = r.at.saturating_add(window);
        if now < until {
            // A login needs a person, not a wait: no retry hint.
            let retry = (code != "vendor_login_required").then_some(until - now);
            out.block(format!("{code} ({})", r.status), retry);
        }
        out
    }

    /// Record a per-row Connect error without overwriting vendor `last_rejection`.
    pub fn mark_row_rejection(&self, wire: &str, status: u16, classification: &str) {
        let mut inner = self.lock();
        inner.last_row = Some(RowRejection {
            wire: wire.to_string(),
            classification: classification.to_string(),
            status,
        });
    }

    pub fn last_rejection_excerpt(&self) -> Option<String> {
        self.lock()
            .last_rejection
            .as_ref()
            .map(|r| r.excerpt.clone())
    }

    pub fn mark_bridge_used(&self) {
        let write = {
            let mut inner = self.lock();
            inner.last_path = Some("bridge");
            self.pending_write(&mut inner)
        };
        self.write(write);
    }

    /// Record a probe outcome and when the loop should run again (R2). `None` = nothing
    /// scheduled; the loop re-arms from `blocked.next_probe` if a block appears meanwhile.
    pub fn record_probe(&self, result: ProbeResult, next_at: Option<u64>) {
        let mut inner = self.lock();
        inner.probe.last_at = Some(now_unix());
        inner.probe.last_result = Some(result);
        inner.probe.next_at = next_at;
    }

    /// Schedule the next probe (start-up, or after a skip).
    pub fn schedule_probe(&self, at: u64) {
        self.lock().probe.next_at = Some(at);
    }

    pub fn probe_next_at(&self) -> Option<u64> {
        self.lock().probe.next_at
    }

    pub fn successes(&self) -> u64 {
        self.lock().successes
    }

    pub fn last_status(&self) -> Option<u16> {
        self.lock().last_status
    }

    pub fn last_path(&self) -> Option<&'static str> {
        self.lock().last_path
    }

    /// `/ready` fragment: `ok | blocked(reason, since, next probe) | off` plus last status;
    /// `persisted` when a mirror file is configured and `probe` when the probe owns the state.
    pub fn snapshot(&self, enabled: bool) -> Value {
        let inner = self.lock();
        let status = if !enabled {
            json!("off")
        } else if let Some(b) = &inner.blocked {
            json!({
                "blocked": {
                    "reason": redact_log(&b.reason),
                    "since": iso(b.since),
                    "next_probe": iso(b.next_probe),
                }
            })
        } else {
            json!("ok")
        };
        let mut out = json!({
            "status": status,
            "last_status": inner.last_status,
            "last_rejection": inner.last_rejection.as_ref().map(|r| &r.classification),
            "last_path": inner.last_path,
            "successes": inner.successes,
        });
        if let Some(row) = &inner.last_row {
            crate::thin::json_insert(
                &mut out,
                "last_row_rejection",
                json!({
                    "wire": row.wire,
                    "code": row.classification,
                    "status": row.status,
                }),
            );
        }
        if self.persist_path.is_some() {
            crate::thin::json_insert(&mut out, "persisted", json!(inner.persisted));
        }
        if inner.probe_owned {
            crate::thin::json_insert(
                &mut out,
                "probe",
                json!({
                    "last_at": inner.probe.last_at.map(iso),
                    "last_result": inner.probe.last_result.as_ref().map(ProbeResult::label),
                    "next_at": inner.probe.next_at.map(iso),
                }),
            );
        }
        out
    }

    /// The mirror record to write, or `None` when persistence is off or nothing changed.
    fn pending_write(&self, inner: &mut Inner) -> Option<Persisted> {
        self.persist_path.as_ref()?;
        let record = Persisted {
            blocked: inner.blocked.clone(),
            last_path: inner.last_path.map(str::to_string),
            cli_version: self.cli_version.clone(),
            updated_at: now_unix(),
        };
        let same = inner.last_written.as_ref().is_some_and(|w| {
            w.blocked == record.blocked
                && w.last_path == record.last_path
                && w.cli_version == record.cli_version
        });
        if same {
            return None;
        }
        inner.last_written = Some(record.clone());
        Some(record)
    }

    fn write(&self, record: Option<Persisted>) {
        let (Some(path), Some(record)) = (&self.persist_path, record) else {
            return;
        };
        let _io = self.io.lock().unwrap_or_else(|p| p.into_inner());
        match store(path, &record) {
            Ok(()) => self.lock().persisted = true,
            Err(e) => {
                self.lock().persisted = false;
                tracing::warn!(path = %path.display(), error = %e, "claude oauth state not persisted");
            }
        }
    }
}

/// The bridge's last pre-`Started` fault (SPEC §19.15 R3): the classification and message of
/// the error and when the bridge first drifted. In memory only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BridgeDrift {
    pub reason: String,
    pub since: u64,
}

/// Soft stream-vocabulary skips (SPEC §19.15 C8): unknown record / event / block types
/// that were not isolation failures. Process-lifetime; `/ready` surfaces the totals.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct BridgeSkipped {
    pub records: u64,
    pub events: u64,
    pub blocks: u64,
    /// Tool calls dropped for `parallel_tool_calls: false` (SPEC §19.3).
    pub parallel: u64,
}

/// `bridge_state: ok | drift(reason, since)` (R3). Set by a bridge failure before `Started`
/// (init validation miss, `claude.exe` unresolved, MCP ready timeout, spawn failure); cleared by
/// the next successful bridge run, including the `doctor` handshake. Skip counters survive `mark_ok`.
#[derive(Debug, Default)]
pub struct BridgeState {
    drift: Mutex<Option<BridgeDrift>>,
    skipped: Mutex<BridgeSkipped>,
}

impl BridgeState {
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Option<BridgeDrift>> {
        self.drift.lock().unwrap_or_else(|p| p.into_inner())
    }

    pub fn drift(&self) -> Option<BridgeDrift> {
        self.lock().clone()
    }

    pub fn is_drifted(&self) -> bool {
        self.lock().is_some()
    }

    /// Record a pre-`Started` fault. `since` is kept from the first fault of the run.
    pub fn mark_drift(&self, reason: impl Into<String>) {
        let mut guard = self.lock();
        let since = guard.as_ref().map(|d| d.since).unwrap_or_else(now_unix);
        *guard = Some(BridgeDrift {
            reason: reason.into(),
            since,
        });
    }

    pub fn mark_ok(&self) {
        *self.lock() = None;
    }

    pub fn skipped(&self) -> BridgeSkipped {
        *self.skipped.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Add mapper skip counts from one turn (C8).
    pub fn add_skipped(&self, records: u64, events: u64, blocks: u64, parallel: u64) {
        let mut g = self.skipped.lock().unwrap_or_else(|p| p.into_inner());
        g.records = g.records.saturating_add(records);
        g.events = g.events.saturating_add(events);
        g.blocks = g.blocks.saturating_add(blocks);
        g.parallel = g.parallel.saturating_add(parallel);
    }

    /// `/ready` fragment: `{status: ok, skipped}` or `{drift: {reason, since}, skipped}`.
    pub fn snapshot(&self) -> Value {
        let skipped = self.skipped();
        let skips = json!({
            "records": skipped.records,
            "events": skipped.events,
            "blocks": skipped.blocks,
            "parallel": skipped.parallel,
        });
        match &*self.lock() {
            None => json!({ "status": "ok", "skipped": skips }),
            Some(d) => json!({
                "drift": {
                    "reason": redact_log(&d.reason),
                    "since": iso(d.since),
                },
                "skipped": skips
            }),
        }
    }
}

fn load(path: &Path) -> Option<Persisted> {
    let text = std::fs::read_to_string(path).ok()?;
    match serde_json::from_str::<Persisted>(&text) {
        Ok(p) => Some(p),
        Err(e) => {
            tracing::warn!(path = %path.display(), error = %e, "claude oauth state file malformed; treating as not blocked");
            None
        }
    }
}

/// Atomic rename write; the temp file is made private before it takes the final name.
fn store(path: &Path, record: &Persisted) -> std::io::Result<()> {
    crate::thin::private_dir::write_private_atomic(path, &serde_json::to_vec_pretty(record)?)
}

fn iso(unix: u64) -> String {
    chrono::DateTime::<chrono::Utc>::from_timestamp(unix as i64, 0)
        .map(|t| t.format("%Y-%m-%dT%H:%M:%SZ").to_string())
        .unwrap_or_else(|| unix.to_string())
}

/// Client path: tokens, bearer, refresh and emails only — model ids and overflow wording stay.
pub fn redact(text: &str) -> String {
    apply_token_redactions(text).chars().take(600).collect()
}

/// Logs and `/ready` excerpts: also wipe 20+ character runs so request ids do not leak.
pub fn redact_log(text: &str) -> String {
    static RE_RUN20: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re_run20 = RE_RUN20.get_or_init(|| crate::thin::static_regex(r"[A-Za-z0-9._/-]{20,}"));
    re_run20
        .replace_all(&apply_token_redactions(text), "<redacted>")
        .chars()
        .take(600)
        .collect()
}

fn apply_token_redactions(text: &str) -> String {
    static RE_SK: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re_sk = RE_SK.get_or_init(|| crate::thin::static_regex(r"sk-ant-[A-Za-z0-9_-]+"));

    static RE_YA: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re_ya = RE_YA.get_or_init(|| crate::thin::static_regex(r"ya29\.[A-Za-z0-9_-]+"));

    static RE_BEARER: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re_bearer = RE_BEARER.get_or_init(|| crate::thin::static_regex(r#"(?i)Bearer\s+[^\s"']+"#));

    static RE_REFRESH: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re_refresh = RE_REFRESH.get_or_init(|| crate::thin::static_regex(r"1//[0-9A-Za-z_-]+"));

    static RE_EMAIL: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re_email = RE_EMAIL.get_or_init(|| {
        crate::thin::static_regex(
            r"[A-Za-z0-9._%+-]+@[A-Za-z0-9-]+(?:\.[A-Za-z0-9-]+)*\.[A-Za-z]{2,}",
        )
    });

    let mut out = text.to_string();
    out = re_sk.replace_all(&out, "<redacted>").into_owned();
    out = re_ya.replace_all(&out, "<redacted>").into_owned();
    out = re_bearer.replace_all(&out, "<redacted>").into_owned();
    out = re_refresh.replace_all(&out, "<redacted>").into_owned();
    out = re_email.replace_all(&out, "<redacted>").into_owned();
    out
}

pub fn rejection_excerpt(text: &str) -> String {
    redact_log(text).chars().take(120).collect()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    #[test]
    fn block_then_cooldown_then_probe() {
        let state = VendorState::new();
        assert!(state.should_try_direct());
        state.mark_blocked("only authorized for use with Claude Code", 3600);
        assert!(!state.should_try_direct());
        assert!(state.is_blocked());
        let snap = state.snapshot(true);
        assert_eq!(
            snap["status"]["blocked"]["reason"],
            "only authorized for use with Claude Code"
        );
        assert!(snap.get("persisted").is_none());
        assert!(snap.get("probe").is_none());
        state.mark_blocked("again", 0);
        assert!(
            state.should_try_direct(),
            "cooldown of zero probes immediately"
        );
        state.mark_ok(200);
        assert!(!state.is_blocked());
        assert_eq!(state.snapshot(true)["status"], "ok");
        assert_eq!(state.snapshot(false)["status"], "off");
    }

    #[test]
    fn probe_owned_never_opens_direct_on_cooldown_alone() {
        let state = VendorState::new();
        state.set_probe_owned(true);
        state.mark_blocked("third-party", 0);
        assert!(
            !state.should_try_direct(),
            "with a probe owning the state, an elapsed cooldown is not enough"
        );
        assert!(state.probe_next_at().is_some(), "block schedules the probe");
        state.record_probe(
            ProbeResult::Error("vendor_down".into()),
            Some(now_unix() + 900),
        );
        let snap = state.snapshot(true);
        assert_eq!(snap["probe"]["last_result"], "error(vendor_down)");
        assert!(snap["probe"]["next_at"].is_string());
        state.mark_ok(200);
        assert!(state.should_try_direct());
        assert!(state.probe_next_at().is_none(), "ok clears the schedule");
    }

    #[test]
    fn availability_follows_the_last_rejection_until_its_window_or_a_success() {
        let state = VendorState::new();
        let now = now_unix();
        assert!(state.availability(now, &[]).available);
        state.mark_rejection_retry(429, "rate_limit_error", "slow down", Some(90));
        let limited = state.availability(now, &[]);
        assert!(!limited.available);
        assert!(
            matches!(limited.retry_in_seconds, Some(88..=90)),
            "{limited:?}"
        );
        assert!(limited.reason.unwrap().contains("rate_limit_error"));
        assert!(
            state.availability(now + 91, &[]).available,
            "the vendor's own wait ran out"
        );
        state.mark_ok(200);
        assert!(
            state.availability(now, &[]).available,
            "a later success clears it"
        );

        state.mark_rejection(503, "vendor_down", "unreachable");
        let down = state.availability(now, &[]);
        assert!(!down.available);
        assert!(matches!(down.retry_in_seconds, Some(118..=120)), "{down:?}");
        assert!(state.availability(now + 121, &[]).available);

        state.mark_rejection(401, "vendor_login_required", "token rejected");
        let login = state.availability(now, &[]);
        assert!(!login.available);
        assert_eq!(login.retry_in_seconds, None, "a login has no wait");
        assert!(
            state
                .availability(now, &["vendor_login_required"])
                .available
        );

        state.mark_rejection(400, "invalid_request_error", "bad body");
        assert!(
            state.availability(now, &[]).available,
            "a client error is not the vendor"
        );

        state.mark_row_unavailable("subs/cursor/grok-4.7");
        let rows = state.availability(now, &[]);
        assert!(rows.available);
        assert_eq!(rows.rows_unavailable, vec!["subs/cursor/grok-4.7"]);
        assert!(state
            .availability(now + ROW_UNAVAILABLE_SECONDS, &[])
            .rows_unavailable
            .is_empty());
    }

    #[test]
    fn availability_block_keeps_the_longest_wait() {
        let mut a = Availability::default();
        a.block("rate", Some(30));
        a.block("quota", Some(900));
        assert_eq!(
            (a.retry_in_seconds, a.reason.as_deref()),
            (Some(900), Some("quota"))
        );
        a.block("short", Some(5));
        assert_eq!(a.retry_in_seconds, Some(900));
        a.block("login", None);
        assert_eq!(
            (a.retry_in_seconds, a.reason.as_deref()),
            (None, Some("login"))
        );
        let mut v = json!({});
        a.attach(&mut v);
        assert_eq!(v["available"], false);
        assert_eq!(Availability::from_json(&v), a);
    }

    #[test]
    fn haiku_success_keeps_the_non_haiku_block() {
        let state = VendorState::new();
        state.set_probe_owned(true);
        state.mark_blocked("third-party", 3600);
        let next = state.probe_next_at();
        state.mark_ok_haiku(200);
        assert!(state.is_blocked(), "a Haiku 200 must not reopen the path");
        assert_eq!(state.probe_next_at(), next, "the probe schedule stands");
        assert_eq!(state.successes(), 1);
        let open = VendorState::new();
        open.mark_ok_haiku(200);
        assert!(!open.is_blocked());
        assert_eq!(open.snapshot(true)["status"], "ok");
    }

    #[test]
    fn state_file_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("thin").join("claude_oauth_state.json");

        let fresh = VendorState::with_persistence(path.clone(), Some("2.1.0".into()));
        assert!(!fresh.is_blocked());
        assert!(!path.exists(), "load never creates the file");
        assert_eq!(fresh.snapshot(true)["persisted"], false);

        fresh.mark_blocked("classed as third-party app", 3600);
        assert!(path.exists());
        assert_eq!(fresh.snapshot(true)["persisted"], true);
        let text = std::fs::read_to_string(&path).unwrap();
        let v: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v["blocked"]["reason"], "classed as third-party app");
        assert_eq!(v["last_path"], "bridge");
        assert_eq!(v["cli_version"], "2.1.0");
        assert!(v["updated_at"].as_u64().unwrap() > 0);
        assert!(!dir
            .path()
            .join("thin")
            .join("claude_oauth_state.json.tmp")
            .exists());

        let restarted = VendorState::with_persistence(path.clone(), Some("2.1.0".into()));
        assert!(restarted.is_blocked(), "a restart remembers the block");
        assert_eq!(restarted.last_path(), Some("bridge"));
        assert_eq!(restarted.snapshot(true)["persisted"], true);
        assert!(!restarted.should_try_direct());

        restarted.mark_ok(200);
        let again = VendorState::with_persistence(path.clone(), None);
        assert!(!again.is_blocked());
        assert_eq!(again.last_path(), Some("oauth"));
    }

    #[test]
    fn state_file_malformed_or_expired_means_not_blocked() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("claude_oauth_state.json");
        std::fs::write(&path, b"{not json").unwrap();
        let s = VendorState::with_persistence(path.clone(), None);
        assert!(!s.is_blocked());
        assert_eq!(s.snapshot(true)["persisted"], false);

        let expired = Persisted {
            blocked: Some(Blocked {
                reason: "old".into(),
                since: 1,
                next_probe: now_unix() - 60,
            }),
            last_path: Some("bridge".into()),
            cli_version: None,
            updated_at: 1,
        };
        std::fs::write(&path, serde_json::to_vec(&expired).unwrap()).unwrap();
        let s = VendorState::with_persistence(path, None);
        assert!(!s.is_blocked(), "next_probe in the past is not blocked");
        assert!(s.should_try_direct());
    }

    #[test]
    fn identical_state_is_not_rewritten() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("claude_oauth_state.json");
        let s = VendorState::with_persistence(path.clone(), None);
        s.mark_ok(200);
        let first = std::fs::metadata(&path).unwrap().modified().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(30));
        s.mark_ok(200);
        let second = std::fs::metadata(&path).unwrap().modified().unwrap();
        assert_eq!(
            first, second,
            "same blocked/last_path/cli_version → no write"
        );
    }

    #[test]
    fn bridge_state_drift_keeps_first_since_and_ok_clears() {
        let s = BridgeState::new();
        assert!(!s.is_drifted());
        assert_eq!(s.snapshot()["status"], "ok");
        s.mark_drift("vendor_drift: init.apiKeySource=\"oauth\" expected \"none\"");
        let first = s.drift().unwrap();
        s.mark_drift("vendor_down: claude.exe not found");
        let second = s.drift().unwrap();
        assert_eq!(first.since, second.since, "since is the first fault");
        assert!(second.reason.contains("claude.exe not found"));
        assert!(s.snapshot()["drift"]["since"].is_string());
        s.mark_drift("vendor_drift: Contact ops@example.com with req_011Cf8HRjCC54hiCUpj1PN3J.");
        let serialised = serde_json::to_string(&s.snapshot()).unwrap();
        assert!(
            !serialised.contains("req_011Cf8HRjCC54hiCUpj1PN3J"),
            "{serialised}"
        );
        assert!(!serialised.contains("ops@example.com"), "{serialised}");
        assert!(s
            .drift()
            .unwrap()
            .reason
            .contains("req_011Cf8HRjCC54hiCUpj1PN3J"));
        s.mark_ok();
        assert!(!s.is_drifted());
        assert_eq!(s.snapshot()["status"], "ok");
        s.add_skipped(1, 2, 3, 4);
        assert_eq!(s.snapshot()["skipped"]["records"], 1);
        assert_eq!(s.snapshot()["skipped"]["events"], 2);
        assert_eq!(s.snapshot()["skipped"]["blocks"], 3);
        assert_eq!(s.snapshot()["skipped"]["parallel"], 4);
        s.mark_ok();
        assert_eq!(
            s.snapshot()["skipped"]["records"],
            1,
            "skip totals survive mark_ok"
        );
    }

    #[test]
    fn redact_strips_tokens() {
        let text = r#"{"error":"token sk-ant-oat01-abcdef rejected","hdr":"Bearer eyJabc"}"#;
        let red = redact(text);
        assert!(!red.contains("sk-ant-oat01-abcdef"));
        assert!(red.contains("<redacted>"));
        assert!(red.contains("rejected"));
    }

    #[test]
    fn redact_keeps_model_ids_quota_names_and_overflow() {
        let model = "claude-fable-5-1-thinking-high";
        let overflow = "prompt is too long: 200000 tokens";
        let metric = "generate_requests_per_model";
        let text = format!(
            "{model} {overflow} {metric} sk-ant-oat01-abcdef user@example.com Bearer eyJabc 1//0abcdefghij"
        );
        let red = redact(&text);
        assert!(red.contains(model), "{red}");
        assert!(red.contains(overflow), "{red}");
        assert!(red.contains(metric), "{red}");
        assert!(!red.contains("sk-ant-oat01-abcdef"), "{red}");
        assert!(!red.contains("user@example.com"), "{red}");
        assert!(!red.contains("Bearer eyJabc"), "{red}");
        assert!(!red.contains("1//0abcdefghij"), "{red}");
        assert!(red.contains("<redacted>"), "{red}");
    }

    #[test]
    fn redact_strict_runs_emails_refresh_tokens() {
        let run20 = "a".repeat(20);
        let run19 = "b".repeat(19);
        let email = "user@example.com";
        let refresh = "1//0abcdefghij";
        let text = format!("{run20} {run19} {email} {refresh}");
        let red = redact_log(&text);
        assert!(!red.contains(&run20));
        assert!(red.contains(&run19));
        assert!(!red.contains(email));
        assert!(!red.contains(refresh));
        assert!(red.contains("<redacted>"));

        let exc = rejection_excerpt(&text);
        assert!(exc.chars().count() <= 120);
        assert!(!exc.contains(&run20));
        assert!(!exc.contains(email));
    }

    #[test]
    fn ready_shows_classification_not_body() {
        let state = VendorState::new();
        let body_with_email_and_long_id =
            "Rate limit hit for user@example.com on request_id_abcdef1234567890.";
        state.mark_rejection(429, "rate_limit_error", body_with_email_and_long_id);
        let snap = state.snapshot(true);
        assert_eq!(snap["last_rejection"], "rate_limit_error");
        let serialised = serde_json::to_string(&snap).unwrap();
        assert!(!serialised.contains("user@example.com"));
        assert!(!serialised.contains("request_id"));
        assert!(!serialised.contains("Rate limit hit"));
        let exc = state.last_rejection_excerpt().unwrap();
        assert!(exc.contains("<redacted>"));
        assert!(!exc.contains("user@example.com"));
        assert!(exc.chars().count() <= 120);
    }

    #[test]
    fn a_row_rejection_does_not_overwrite_vendor_last_rejection() {
        let state = VendorState::new();
        state.mark_rejection(401, "vendor_login_required", "token rejected");
        state.mark_row_rejection("cursor-grok-4.7 effort=high", 404, "model_not_found");
        let snap = state.snapshot(true);
        assert_eq!(snap["status"], "ok");
        assert_eq!(snap["last_rejection"], "vendor_login_required");
        assert_eq!(snap["last_row_rejection"]["code"], "model_not_found");
        assert_eq!(snap["last_row_rejection"]["status"], 404);
        assert_eq!(
            snap["last_row_rejection"]["wire"],
            "cursor-grok-4.7 effort=high"
        );
        assert_eq!(snap["last_status"], 401);
    }

    #[test]
    fn ready_blocked_reason_wipes_request_ids() {
        let state = VendorState::new();
        let reason =
            "classed as third-party app: Contact ops@example.com with req_011Cf8HRjCC54hiCUpj1PN3J. Use claude-fable-5-1-thinking-high.";
        state.mark_blocked(reason, 3600);
        let stored = state.blocked().unwrap().reason;
        assert!(
            stored.contains("claude-fable-5-1-thinking-high"),
            "{stored}"
        );
        assert!(stored.contains("req_011Cf8HRjCC54hiCUpj1PN3J"), "{stored}");
        let serialised = serde_json::to_string(&state.snapshot(true)).unwrap();
        assert!(serialised.contains("third-party"), "{serialised}");
        assert!(!serialised.contains("ops@example.com"), "{serialised}");
        assert!(
            !serialised.contains("req_011Cf8HRjCC54hiCUpj1PN3J"),
            "{serialised}"
        );
        assert!(
            !serialised.contains("claude-fable-5-1-thinking-high"),
            "{serialised}"
        );
    }
}
