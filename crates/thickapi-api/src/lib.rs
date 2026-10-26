pub mod middleware;
pub mod routes;
pub mod server;

/// Cargo `workspace.package.version`, which equals the SPEC version (SPEC header).
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
/// Short git sha of the build, or `unknown` without `.git` (`build.rs`).
pub const GIT_SHA: &str = env!("THICKAPI_GIT_SHA");
/// `thickapi --version` prints `thickapi <VERSION_LINE>` (SPEC §11).
pub const VERSION_LINE: &str = concat!(
    env!("CARGO_PKG_VERSION"),
    " (",
    env!("THICKAPI_GIT_SHA"),
    ")"
);

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::{
    extract::DefaultBodyLimit,
    http::{header, HeaderName, HeaderValue, Method, Uri},
    middleware::from_fn_with_state,
    response::Response,
    routing::{get, post},
    Router,
};
use thickapi_core::thin::runtime::ThinRuntime;
use thickapi_core::thin::ThinError;
use thickapi_core::{ConfigBundle, PoolManager, Store, TurnEngine, TurnRegistry};
use tokio_util::sync::CancellationToken;
use tower_http::cors::{AllowOrigin, CorsLayer};

use middleware::auth::{require_chat_write, require_models_read, require_turn_write};
use routes::{
    chat_completions, health, models, ready, turn, turn_cancel, turn_events, turn_status,
};

#[derive(Clone)]
pub struct AppState {
    pub config: ConfigBundle,
    pub store: Arc<Store>,
    pub engine: Arc<TurnEngine>,
    pub registry: Arc<TurnRegistry>,
    pub pools: Arc<PoolManager>,
    pub started_at: Instant,
    /// Unix seconds at start; `created` on `/v1/models` entries.
    pub started_unix: u64,
    /// Thin-surface adapters and per-vendor state (SPEC §19).
    pub thin: Arc<ThinRuntime>,
    /// In-flight thin `/v1/chat/completions` work (streams included).
    pub thin_in_flight: Arc<AtomicU32>,
    /// Cancelled when graceful drain starts; thin routes then 503.
    pub drain: CancellationToken,
    /// `serve --dump-tools <path>`: write the first thin request's `tools` array, then clear.
    pub dump_tools: Arc<std::sync::Mutex<Option<std::path::PathBuf>>>,
}

impl AppState {
    pub fn uptime_seconds(&self) -> u64 {
        self.started_at.elapsed().as_secs()
    }

    pub fn refuse_if_draining(&self) -> Result<(), ThinError> {
        if self.drain.is_cancelled() {
            Err(ThinError::vendor_down("thickapi", "draining"))
        } else {
            Ok(())
        }
    }
}

/// Held for the life of a thin request, including the SSE body.
pub struct ThinInFlightGuard(Arc<AtomicU32>);

impl ThinInFlightGuard {
    pub fn enter(gauge: &Arc<AtomicU32>) -> Self {
        gauge.fetch_add(1, Ordering::SeqCst);
        Self(gauge.clone())
    }
}

impl Drop for ThinInFlightGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Auth is a `route_layer`: it guards the routes it is attached to and nothing else. As a
/// plain `layer` it also wrapped each sub-router's fallback, so every unknown path answered
/// `401` in whichever error shape was merged last.
pub fn build_router(state: AppState) -> Router {
    let protected_ready = Router::new()
        .route("/ready", get(ready::ready))
        .route_layer(from_fn_with_state(state.clone(), require_models_read));

    let protected_models = Router::new()
        .route("/v1/models", get(models::list_models))
        .method_not_allowed_fallback(method_not_allowed)
        .route_layer(from_fn_with_state(state.clone(), require_models_read));

    let protected_turn = Router::new()
        .route("/v1/agent/turn", post(turn::post_turn))
        .route("/v1/agent/turn/{id}", get(turn_status::get_turn_status))
        .route(
            "/v1/agent/turn/{id}/events",
            get(turn_events::get_turn_events),
        )
        .route(
            "/v1/agent/turn/{id}/cancel",
            post(turn_cancel::post_turn_cancel),
        )
        .route_layer(from_fn_with_state(state.clone(), require_turn_write));

    // `0` = off, like the other thin budgets; never axum's 2 MiB default.
    let body_limit = match state.config.router.thin.max_body_mib {
        0 => DefaultBodyLimit::disable(),
        mib => DefaultBodyLimit::max(mib as usize * 1024 * 1024),
    };
    let protected_chat = Router::new()
        .route(
            "/v1/chat/completions",
            post(chat_completions::post_chat_completions),
        )
        .method_not_allowed_fallback(method_not_allowed)
        .route_layer(body_limit)
        .route_layer(from_fn_with_state(state.clone(), require_chat_write));

    // Thin routes answer OPTIONS / carry CORS headers when `cors_origins` is set (SPEC §19.4).
    let thin = protected_models.merge(protected_chat);
    let thin = match cors_layer(&state.config.router.cors_origins) {
        Some(cors) => thin.layer(cors),
        None => thin,
    };

    Router::new()
        .route("/health", get(health::health))
        .merge(protected_ready)
        .merge(protected_turn)
        .merge(thin)
        .fallback(route_not_found)
        .with_state(state)
}

async fn route_not_found(method: Method, uri: Uri) -> Response {
    chat_completions::thin_error_response(&ThinError::route_not_found(method.as_str(), uri.path()))
}

/// Set before the auth `route_layer`, so a wrong method on a thin route is still guarded.
async fn method_not_allowed(method: Method, uri: Uri) -> Response {
    chat_completions::thin_error_response(&ThinError::method_not_allowed(
        method.as_str(),
        uri.path(),
    ))
}

fn cors_layer(origins: &[String]) -> Option<CorsLayer> {
    if origins.is_empty() {
        return None;
    }
    let allow_origin = if origins.iter().any(|o| o == "*") {
        AllowOrigin::any()
    } else {
        AllowOrigin::list(origins.iter().filter_map(|o| HeaderValue::from_str(o).ok()))
    };
    Some(
        CorsLayer::new()
            .allow_origin(allow_origin)
            .allow_methods([Method::GET, Method::POST, Method::OPTIONS])
            .allow_headers([header::AUTHORIZATION, header::CONTENT_TYPE, header::ACCEPT])
            .expose_headers([
                HeaderName::from_static("x-thickapi-rate-limit"),
                HeaderName::from_static("x-thickapi-ignored"),
                HeaderName::from_static("x-thickapi-path"),
                HeaderName::from_static("x-thickapi-fallback"),
                HeaderName::from_static("x-request-id"),
                HeaderName::from_static("retry-after"),
                HeaderName::from_static("x-should-retry"),
            ])
            .max_age(Duration::from_secs(600)),
    )
}

pub use server::{make_state, make_state_with_store, serve, serve_on};
