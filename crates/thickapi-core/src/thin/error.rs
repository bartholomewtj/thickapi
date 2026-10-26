//! Thin-surface errors → OpenAI envelope `{"error":{message,type,param,code}}` plus `vendor`
//! and `hint` (SPEC §19.4).

use std::path::Path;
use std::sync::OnceLock;

use serde_json::{json, Value};
use thiserror::Error;

/// The rarely-populated, non-hot-path fields, boxed behind one pointer so `ThinError` itself
/// stays small (`clippy::result_large_err`; the 224-byte unboxed shape was the lint's 52-warning
/// culprit — SPEC §14, brief `requests/archive/b1-clippy-zero.md`). `status`, `kind`, `code` and
/// `message` stay inline: every hot path reads them.
#[derive(Debug, Clone, Default)]
struct ThinErrorMeta {
    param: Option<String>,
    vendor: Option<String>,
    hint: Option<String>,
    retry_after_seconds: Option<u64>,
    /// `x-should-retry: false` on quota exhaustion (SPEC §19.4).
    no_retry: bool,
    /// `x-should-retry: true` on a transient fault a fresh request clears (Cursor's
    /// handshake `internal: No exec result`, H2 #186). `no_retry` wins.
    retry: bool,
    /// Serving path for `x-thickapi-path` on pre-`Started` errors (SPEC §19.15 R4).
    path: Option<&'static str>,
    fallback: Option<&'static str>,
    path_reason: Option<&'static str>,
    /// Correlates the error with the access log line and the `x-request-id` header on every
    /// thin response, success or error (SPEC §19.4, A2).
    request_id: Option<String>,
    /// Set only by the Claude bridge, from `BridgeFault::is_bridge_fault` (SPEC §19.15 R3):
    /// the typed cause survives the trip through `ThinError` to the cross-fallback decision.
    bridge_fault: bool,
    /// Set only by Claude direct's §19.8.1 rule table: the rejection is a (suspected) block,
    /// so `ClaudeAdapter` replays the request on the bridge rather than returning it.
    direct_block: Option<DirectBlock>,
}

/// A Claude direct-path rejection that the §19.8.1 rule table reads as a block. It travels on
/// the [`ThinError`] the shared runner returns, the way the bridge's typed fault does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DirectBlock {
    /// Refused outside Claude Code: replay on the bridge and mark the path blocked.
    Blocked,
    /// A non-Haiku 400 that is not request validation: replay on the bridge once.
    Suspected,
}

#[derive(Debug, Clone, Error)]
#[error("{kind}: {message}")]
pub struct ThinError {
    pub status: u16,
    /// OpenAI `type`.
    pub kind: &'static str,
    /// OpenAI `code` (defaults to `kind`).
    pub code: &'static str,
    pub message: String,
    meta: Box<ThinErrorMeta>,
}

impl ThinError {
    fn new(
        status: u16,
        kind: &'static str,
        code: &'static str,
        message: impl Into<String>,
    ) -> Self {
        Self {
            status,
            kind,
            code,
            message: message.into(),
            meta: Box::default(),
        }
    }

    pub fn param(&self) -> Option<&str> {
        self.meta.param.as_deref()
    }

    pub fn vendor(&self) -> Option<&str> {
        self.meta.vendor.as_deref()
    }

    pub fn hint(&self) -> Option<&str> {
        self.meta.hint.as_deref()
    }

    pub fn retry_after_seconds(&self) -> Option<u64> {
        self.meta.retry_after_seconds
    }

    pub fn no_retry(&self) -> bool {
        self.meta.no_retry
    }

    pub fn path(&self) -> Option<&'static str> {
        self.meta.path
    }

    pub fn fallback(&self) -> Option<&'static str> {
        self.meta.fallback
    }

    pub fn path_reason(&self) -> Option<&'static str> {
        self.meta.path_reason
    }

    pub fn request_id(&self) -> Option<&str> {
        self.meta.request_id.as_deref()
    }

    /// A bridge fault before `Started` (SPEC §19.15 R3): set from the bridge's typed cause,
    /// never inferred from the message.
    pub fn is_bridge_fault(&self) -> bool {
        self.meta.bridge_fault
    }

    pub fn with_bridge_fault(mut self) -> Self {
        self.meta.bridge_fault = true;
        self
    }

    /// Claude direct's block verdict (SPEC §19.8.1), when the rule table found one.
    pub fn direct_block(&self) -> Option<DirectBlock> {
        self.meta.direct_block
    }

    pub fn with_direct_block(mut self, block: DirectBlock) -> Self {
        self.meta.direct_block = Some(block);
        self
    }

    pub fn invalid_request(message: impl Into<String>, param: Option<&str>) -> Self {
        let mut e = Self::new(
            400,
            "invalid_request_error",
            "invalid_request_error",
            message,
        );
        e.meta.param = param.map(str::to_string);
        e
    }

    /// A thin request the daemon will not run: `400` with `code: invalid_spec` (the same
    /// `{error: {code, message}}` envelope as every other thin rejection).
    pub fn invalid_spec(message: impl Into<String>) -> Self {
        Self::new(400, "invalid_request_error", "invalid_spec", message)
    }

    /// No such route. Its own code, so a mistyped path does not read as a missing model
    /// or, as it used to, as a bad key.
    pub fn route_not_found(method: &str, path: &str) -> Self {
        Self::new(
            404,
            "invalid_request_error",
            "route_not_found",
            format!("no route for {method} {path}"),
        )
    }

    /// A thin route, the wrong method. axum's bare `405` would be the one thin answer
    /// without the envelope; its `Allow` header is kept.
    pub fn method_not_allowed(method: &str, path: &str) -> Self {
        Self::new(
            405,
            "invalid_request_error",
            "method_not_allowed",
            format!("{method} is not allowed on {path}"),
        )
    }

    pub fn body_too_large(limit_mib: u64) -> Self {
        Self::new(
            413,
            "invalid_request_error",
            "request_too_large",
            format!("request body exceeds {limit_mib} MiB (`thin.max_body_mib`)"),
        )
    }

    pub fn model_not_found(model: &str) -> Self {
        Self::new(
            404,
            "not_found",
            "model_not_found",
            format!("model not found or disabled: {}", model),
        )
        .with_hint("thickapi models list")
    }

    pub fn vendor_login_required(vendor: &str, detail: impl Into<String>, login_cmd: &str) -> Self {
        Self::new(401, "vendor_auth", "vendor_login_required", detail)
            .with_vendor(vendor)
            .with_hint(login_cmd)
    }

    pub fn permission(message: impl Into<String>) -> Self {
        Self::new(403, "permission_error", "permission_error", message)
    }

    pub fn rate_limited(
        vendor: &str,
        message: impl Into<String>,
        retry_after: u64,
        quota_exhausted: bool,
    ) -> Self {
        let raw = message.into();
        let message = if quota_exhausted {
            prefix_quota(raw)
        } else {
            raw
        };
        let mut e =
            Self::new(429, "rate_limit_error", "rate_limit_error", message).with_vendor(vendor);
        e.meta.retry_after_seconds = Some(retry_after);
        e.meta.no_retry = quota_exhausted;
        e
    }

    pub fn insufficient_quota(vendor: &str, message: impl Into<String>) -> Self {
        let mut e = Self::new(
            402,
            "insufficient_quota",
            "insufficient_quota",
            prefix_quota(message),
        )
        .with_vendor(vendor);
        e.meta.no_retry = true;
        e
    }

    /// Quota-exhausted vendor text: message begins `quota exceeded` (Pi's non-retryable
    /// pattern). Names quota/usage/payment → `402 insufficient_quota`; otherwise a 429.
    /// `Retry-After` is kept either way (SPEC §19.4 429 semantics).
    pub fn vendor_limit(
        vendor: &str,
        message: impl Into<String>,
        retry_after: u64,
        vendor_text: &str,
        exhausted: bool,
    ) -> Self {
        let message = message.into();
        let lower = vendor_text.to_ascii_lowercase();
        let named = ["quota", "usage", "payment"]
            .iter()
            .any(|k| lower.contains(k));
        if exhausted && named {
            let mut e = Self::insufficient_quota(vendor, message);
            e.meta.retry_after_seconds = Some(retry_after);
            e
        } else if exhausted {
            Self::rate_limited(vendor, message, retry_after, true)
        } else {
            Self::rate_limited(vendor, message, retry_after, false)
        }
    }

    pub fn vendor_drift(vendor: &str, message: impl Into<String>) -> Self {
        Self::new(502, "vendor_drift", "vendor_drift", message)
            .with_vendor(vendor)
            .with_hint("thickapi doctor")
    }

    /// G3: the client's history shows the same tool call answered the same way N times in a
    /// row. `vendor_drift` with `code: builtin_loop`, never retried.
    pub fn builtin_loop(vendor: &str, message: impl Into<String>) -> Self {
        Self::new(502, "vendor_drift", "builtin_loop", message)
            .with_vendor(vendor)
            .with_hint("send a different tool call")
            .without_retry()
    }

    /// #215: a `<tool_call>` block written as answer text did not parse to one declared tool.
    /// `vendor_drift` with `code: text_tool_call`. Not `/ready` `rendered_builtin`.
    pub fn text_tool_call(vendor: &str, message: impl Into<String>) -> Self {
        Self::new(502, "vendor_drift", "text_tool_call", message)
            .with_vendor(vendor)
            .with_hint("use the tool-call interface")
    }

    pub fn vendor_down(vendor: &str, message: impl Into<String>) -> Self {
        Self::new(503, "vendor_down", "vendor_down", message)
            .with_vendor(vendor)
            .with_hint("thickapi doctor")
    }

    pub fn server(message: impl Into<String>) -> Self {
        Self::new(500, "server_error", "server_error", message)
    }

    /// SPEC §19.7 Dialect: `to_pi` refused a Claude call twice in one request. Same envelope
    /// as §19.16 (`502`, `type: server_error`, `should_retry: false`), its own code.
    pub fn dialect_args(vendor: &str, message: impl Into<String>) -> Self {
        let mut err = Self::new(502, "server_error", "dialect_args", message).with_vendor(vendor);
        err.meta.no_retry = true;
        err
    }

    /// SPEC §19.16: a Claude stop with no text, reasoning or tool call while `tools` were
    /// offered, twice on the same path. Not drift: `doctor` and a fixture refresh do not fix it.
    pub fn empty_completion(vendor: &str, message: impl Into<String>) -> Self {
        let mut err = Self::new(502, "server_error", "empty_completion", message)
            .with_vendor(vendor)
            .with_hint("name only tools present on this request");
        err.meta.no_retry = true;
        err
    }

    pub fn not_implemented(message: impl Into<String>) -> Self {
        Self::new(501, "server_error", "not_implemented", message)
    }

    pub fn with_vendor(mut self, vendor: &str) -> Self {
        self.meta.vendor = Some(vendor.to_string());
        self
    }

    pub fn with_hint(mut self, hint: &str) -> Self {
        self.meta.hint = Some(rewrite_hint(hint));
        self
    }

    pub fn with_param(mut self, param: &str) -> Self {
        self.meta.param = Some(param.to_string());
        self
    }

    pub fn with_retry_after(mut self, seconds: u64) -> Self {
        self.meta.retry_after_seconds = Some(seconds);
        self
    }

    /// Tag a pre-`Started` error with the path the adapter actually used (SPEC §19.15 R4).
    pub fn with_path(
        mut self,
        path: &'static str,
        fallback: Option<&'static str>,
        reason: Option<&'static str>,
    ) -> Self {
        self.meta.path = Some(path);
        self.meta.fallback = fallback;
        self.meta.path_reason = reason;
        self
    }

    /// Tag the error with the request id so the HTTP/stream envelope and the `x-request-id`
    /// header can carry the same value the access log line does (SPEC §19.4, A2).
    pub fn with_request_id(mut self, id: impl Into<String>) -> Self {
        self.meta.request_id = Some(id.into());
        self
    }

    /// `x-should-retry: false` (Pi otherwise retries 502/rate-limit text).
    pub fn without_retry(mut self) -> Self {
        self.meta.no_retry = true;
        self
    }

    /// `x-should-retry: true` (H2, #186): a fault a fresh request clears although the status
    /// alone (503) does not say so.
    pub fn with_retry(mut self) -> Self {
        self.meta.retry = true;
        self
    }

    /// `x-should-retry`: `false` when retrying cannot help, `true` for a 429 that will clear
    /// or a fault marked [`Self::with_retry`], unset when the status speaks for itself.
    pub fn should_retry(&self) -> Option<bool> {
        if self.meta.no_retry {
            Some(false)
        } else {
            (self.status == 429 || self.meta.retry).then_some(true)
        }
    }

    pub fn body(&self) -> Value {
        self.body_with_message(&with_code_marker(&self.message, self.code))
    }

    /// SSE error frame after the HTTP headers have gone. Pi's client surfaces only `message`,
    /// so retry hints and a vendor 401/403 prefix live in that string (SPEC §19.4).
    pub fn stream_body(&self) -> Value {
        self.body_with_message(&self.mid_stream_message())
    }

    /// `message` Pi actually shows for a mid-stream error: retry fields folded in, a vendor
    /// 401/403 prefixed so `rewriteError` does not treat it as a stale `ta_` key, and the
    /// code as the trailing `[code=…]` marker.
    pub fn mid_stream_message(&self) -> String {
        with_code_marker(&self.folded_message(), self.code)
    }

    fn folded_message(&self) -> String {
        let mut msg = self.message.clone();
        if self.meta.vendor.is_some()
            && matches!(self.status, 401 | 403)
            && !msg.to_ascii_lowercase().starts_with("vendor ")
        {
            msg = format!("vendor {}: {msg}", self.status);
        }
        let mut hints = Vec::new();
        if let Some(seconds) = self.meta.retry_after_seconds {
            hints.push(format!("retry_after={seconds}s"));
        }
        if let Some(retry) = self.should_retry() {
            hints.push(format!("should_retry={retry}"));
        }
        if hints.is_empty() {
            msg
        } else {
            format!("{msg} ({})", hints.join(", "))
        }
    }

    fn body_with_message(&self, message: &str) -> Value {
        let mut error = json!({
            "message": message,
            "type": self.kind,
            "param": self.meta.param,
            "code": self.code,
        });
        if let Some(vendor) = &self.meta.vendor {
            crate::thin::json_insert(&mut error, "vendor", json!(vendor));
        }
        if let Some(hint) = &self.meta.hint {
            crate::thin::json_insert(&mut error, "hint", json!(hint));
        }
        if let Some(request_id) = &self.meta.request_id {
            crate::thin::json_insert(&mut error, "request_id", json!(request_id));
        }
        // What `Retry-After` and `x-should-retry` say, for an error that arrives as an SSE
        // frame after the headers have gone.
        if let Some(seconds) = self.meta.retry_after_seconds {
            crate::thin::json_insert(&mut error, "retry_after", json!(seconds));
        }
        if let Some(retry) = self.should_retry() {
            crate::thin::json_insert(&mut error, "should_retry", json!(retry));
        }
        json!({ "error": error })
    }
}

/// Pi records an error's `message` and nothing else, so the envelope `code` is repeated at
/// the end of it as ` [code=<code>]` (SPEC §19.4, #136). Consumers (draftr's seat classifier)
/// read the class from there instead of matching vendor prose. Idempotent.
pub fn with_code_marker(message: &str, code: &str) -> String {
    if message.contains("[code=") {
        message.to_string()
    } else {
        format!("{message} [code={code}]")
    }
}

fn prefix_quota(message: impl Into<String>) -> String {
    let m = message.into();
    if m.to_ascii_lowercase().starts_with("quota exceeded") {
        m
    } else {
        format!("quota exceeded: {m}")
    }
}

/// `thickapi <subcommand>` hints use `current_exe()` when the binary is not on PATH
/// (SPEC §12). Test binaries keep the short name.
fn rewrite_hint(hint: &str) -> String {
    let exe = daemon_exe();
    if hint == "thickapi" {
        return exe.to_string();
    }
    if let Some(rest) = hint.strip_prefix("thickapi ") {
        return format!("{exe} {rest}");
    }
    hint.to_string()
}

fn daemon_exe() -> &'static str {
    static EXE: OnceLock<String> = OnceLock::new();
    EXE.get_or_init(|| {
        if thickapi_on_path() {
            return "thickapi".into();
        }
        match std::env::current_exe() {
            Ok(path) if is_thickapi_bin(&path) => path.display().to_string(),
            _ => "thickapi".into(),
        }
    })
}

fn is_thickapi_bin(path: &Path) -> bool {
    path.file_stem()
        .and_then(|s| s.to_str())
        .is_some_and(|s| s.eq_ignore_ascii_case("thickapi"))
}

fn thickapi_on_path() -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    let exe = if cfg!(windows) {
        "thickapi.exe"
    } else {
        "thickapi"
    };
    std::env::split_paths(&path).any(|dir| dir.join(exe).is_file())
}

impl From<anyhow::Error> for ThinError {
    fn from(err: anyhow::Error) -> Self {
        ThinError::server(err.to_string())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    #[test]
    fn envelope_shape() {
        let e = ThinError::vendor_drift(
            "claude",
            "claude 2.1.290: init.apiKeySource=\"oauth\" expected \"none\"",
        );
        let body = e.body();
        assert_eq!(body["error"]["type"], "vendor_drift");
        assert_eq!(body["error"]["vendor"], "claude");
        assert_eq!(body["error"]["hint"], "thickapi doctor");
        assert!(body["error"]["param"].is_null());
        assert_eq!(e.status, 502);
    }

    #[test]
    fn thickapi_hints_stay_short_in_unit_tests() {
        let e = ThinError::vendor_drift("cursor", "frame moved");
        assert_eq!(e.hint(), Some("thickapi doctor"));
        assert_eq!(rewrite_hint("cursor login"), "cursor login");
    }

    #[test]
    fn rate_limit_flags() {
        let e = ThinError::rate_limited("claude", "slow down", 3600, true);
        assert_eq!(e.status, 429);
        assert!(e.no_retry());
        assert_eq!(e.retry_after_seconds(), Some(3600));
        assert!(e.message.starts_with("quota exceeded"), "{}", e.message);
    }

    #[test]
    fn mid_stream_message_folds_retry_and_prefixes_vendor_auth() {
        let limited = ThinError::rate_limited("grok", "slow down", 7, false);
        let msg = limited.mid_stream_message();
        assert!(msg.contains("slow down"), "{msg}");
        assert!(msg.contains("retry_after=7s"), "{msg}");
        assert!(msg.contains("should_retry=true"), "{msg}");
        assert!(msg.ends_with(" [code=rate_limit_error]"), "{msg}");
        assert_eq!(
            limited.body()["error"]["message"],
            "slow down [code=rate_limit_error]"
        );
        assert_eq!(limited.stream_body()["error"]["message"], msg);

        let auth = ThinError::vendor_login_required("grok", "grok 401: expired", "grok login");
        let msg = auth.mid_stream_message();
        assert!(msg.starts_with("vendor 401: "), "{msg}");
        assert!(!ThinError::permission("key lacks chat:write")
            .mid_stream_message()
            .starts_with("vendor "));
    }

    #[test]
    fn every_constructor_carries_its_code_marker() {
        let cases = [
            ThinError::invalid_request("bad", None),
            ThinError::route_not_found("GET", "/nope"),
            ThinError::method_not_allowed("PUT", "/v1/chat/completions"),
            ThinError::body_too_large(32),
            ThinError::model_not_found("subs/x/y"),
            ThinError::vendor_login_required("grok", "expired", "grok login"),
            ThinError::permission("no"),
            ThinError::rate_limited("grok", "slow", 7, false),
            ThinError::insufficient_quota("claude", "out"),
            ThinError::vendor_drift("cursor", "moved"),
            ThinError::vendor_down("agy", "down"),
            ThinError::server("boom"),
            ThinError::not_implemented("later"),
        ];
        for e in cases {
            let marker = format!(" [code={}]", e.code);
            let http = e.body()["error"]["message"].as_str().unwrap().to_string();
            let frame = e.stream_body()["error"]["message"]
                .as_str()
                .unwrap()
                .to_string();
            assert!(http.ends_with(&marker), "{http}");
            assert!(frame.ends_with(&marker), "{frame}");
            assert_eq!(http.matches("[code=").count(), 1, "{http}");
            // The struct keeps the bare message; only the wire carries the marker.
            assert!(!e.message.contains("[code="), "{}", e.message);
        }
        assert_eq!(with_code_marker("x [code=a]", "b"), "x [code=a]");
    }

    #[test]
    fn vendor_limit_quota_wording_is_402() {
        let e = ThinError::vendor_limit(
            "claude",
            "claude oauth 429: You have exceeded your usage limit",
            30,
            "You have exceeded your usage limit",
            true,
        );
        assert_eq!(e.status, 402);
        assert_eq!(e.code, "insufficient_quota");
        assert!(e.message.starts_with("quota exceeded"), "{}", e.message);
        assert_eq!(e.retry_after_seconds(), Some(30));
        assert!(e.no_retry());
    }
}
