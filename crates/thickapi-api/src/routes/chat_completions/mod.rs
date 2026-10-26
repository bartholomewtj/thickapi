//! `POST /v1/chat/completions` (SPEC §19.3–§19.5): parse, validate, resolve the catalogue row,
//! run the vendor adapter, and answer as `chat.completion` or a `chat.completion.chunk` stream.

mod access_log;
mod collect;
mod headers;
mod start;
mod stream;
mod validate;

use axum::{
    extract::{Extension, State},
    http::{HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use thickapi_core::thin::ThinError;

use crate::middleware::auth::KeyLabel;
use crate::AppState;

use headers::insert_path_headers;
use start::handle;

/// Bounded by the adapter's own back-pressure; text deltas are small.
pub(super) const CHANNEL_DEPTH: usize = 256;

pub fn thin_error_response(err: &ThinError) -> Response {
    let status = StatusCode::from_u16(err.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    let mut response = (status, Json(err.body())).into_response();
    let headers = response.headers_mut();
    if let Some(retry) = err.retry_after_seconds() {
        if let Ok(v) = retry.to_string().parse() {
            headers.insert("retry-after", v);
        }
    }
    if let Some(retry) = err.should_retry() {
        headers.insert(
            "x-should-retry",
            HeaderValue::from_static(if retry { "true" } else { "false" }),
        );
    }
    if let Some(path) = err.path() {
        insert_path_headers(headers, path, err.fallback(), err.path_reason(), None);
    }
    if let Some(request_id) = err.request_id() {
        if let Ok(v) = HeaderValue::from_str(request_id) {
            headers.insert("x-request-id", v);
        }
    }
    response
}

pub async fn post_chat_completions(
    State(state): State<AppState>,
    headers: HeaderMap,
    key_label: Option<Extension<KeyLabel>>,
    body: Result<axum::body::Bytes, axum::extract::rejection::BytesRejection>,
) -> Response {
    // An oversize or unreadable body is a client error like any other: same envelope.
    let body = match body {
        Ok(body) => body,
        Err(rejection) => {
            let limit = state.config.router.thin.max_body_mib;
            let err = if rejection.status() == StatusCode::PAYLOAD_TOO_LARGE {
                ThinError::body_too_large(limit)
            } else {
                ThinError::invalid_request(format!("unreadable request body: {rejection}"), None)
            };
            return thin_error_response(&err);
        }
    };
    // `require_chat_write` always sets this on the success path it guards; absent only in a
    // unit test that calls this handler without going through the middleware.
    let key = key_label
        .map(|Extension(KeyLabel(id))| id)
        .unwrap_or_else(|| "unknown".into());
    match handle(state, key, headers, body).await {
        Ok(response) => response,
        Err(err) => thin_error_response(&err),
    }
}

pub(super) struct Ctx {
    pub(super) id: String,
    pub(super) created: u64,
    pub(super) model: String,
    pub(super) parallel: bool,
    pub(super) include_usage: bool,
}

/// Owns the adapter `JoinHandle` so dropping the HTTP body (or the pre-`Started` handler)
/// aborts the runner within one tick (SPEC §1.7 Disconnect, §19.7 Lifecycle).
pub(super) struct AbortOnDrop(Option<tokio::task::JoinHandle<()>>);

impl AbortOnDrop {
    pub(super) fn new(handle: tokio::task::JoinHandle<()>) -> Self {
        Self(Some(handle))
    }

    #[cfg(test)]
    pub(super) fn none() -> Self {
        Self(None)
    }
}

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        if let Some(handle) = self.0.take() {
            handle.abort();
        }
    }
}

pub(super) fn route_idle_error(bound: std::time::Duration) -> ThinError {
    ThinError::vendor_down(
        "thickapi",
        format!("no events for {bound:?} (route idle; the vendor's idle budget is 0)"),
    )
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests;
