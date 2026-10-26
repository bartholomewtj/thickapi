use axum::{
    body::Body,
    extract::State,
    http::{Request, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
    Json,
};
use serde_json::json;
use thickapi_core::config::keys::has_scope;

use crate::AppState;

pub use thickapi_core::config::keys::{SCOPE_CHAT_WRITE, SCOPE_MODELS_READ, SCOPE_TURN_WRITE};

/// The matched key's `id`, so a thin request's access log line can say which key made the
/// call (Pi / draftr / …) without carrying the key itself (SPEC A2 deliverable 1).
#[derive(Clone, Debug)]
pub struct KeyLabel(pub String);

/// Thin routes: `chat:write` required; OpenAI error envelope on failure (SPEC §19.4).
pub async fn require_chat_write(
    State(state): State<AppState>,
    mut request: Request<Body>,
    next: Next,
) -> Response {
    let token = extract_bearer(request.headers());
    let Some(token) = token else {
        return openai_auth_error(
            StatusCode::UNAUTHORIZED,
            "authentication_error",
            "missing bearer token",
        );
    };
    if !token.starts_with("ta_") {
        return openai_auth_error(
            StatusCode::UNAUTHORIZED,
            "authentication_error",
            "invalid bearer token format",
        );
    }
    match state.thin.keys().find(&token) {
        Some(key) if has_scope(&key, SCOPE_CHAT_WRITE) => {
            request.extensions_mut().insert(KeyLabel(key.id.clone()));
            next.run(request).await
        }
        Some(_) => openai_auth_error(
            StatusCode::FORBIDDEN,
            "permission_error",
            "key lacks chat:write; run: thickapi keys create --label <name> --scope chat:write (or thickapi sync-pi when the caller is Pi)",
        ),
        None => openai_auth_error(StatusCode::UNAUTHORIZED, "authentication_error", "invalid bearer token"),
    }
}

/// A thin route answers this before the request ever reaches `chat_completions::handle`, so it
/// mints its own `request_id` here — same shape as a completion id, same header/body placement
/// as every other thin error (SPEC §19.4, A2 deliverable 5).
fn openai_auth_error(status: StatusCode, kind: &str, message: &str) -> Response {
    let request_id = thickapi_core::thin::openai::new_completion_id();
    let mut response = (
        status,
        Json(json!({
            "error": {
                "message": thickapi_core::thin::error::with_code_marker(message, kind),
                "type": kind,
                "param": serde_json::Value::Null,
                "code": kind,
                "request_id": request_id,
            }
        })),
    )
        .into_response();
    if let Ok(v) = axum::http::HeaderValue::from_str(&request_id) {
        response.headers_mut().insert("x-request-id", v);
    }
    response
}

/// Thin-facing routes (`/ready`, `/v1/models`): same OpenAI envelope and 401/403 split as
/// `require_chat_write` (D3 deliverable 4) — `chat:write` satisfies `models:read` (SPEC §19.6).
pub async fn require_models_read(
    State(state): State<AppState>,
    request: Request<Body>,
    next: Next,
) -> Response {
    let token = extract_bearer(request.headers());
    let Some(token) = token else {
        return openai_auth_error(
            StatusCode::UNAUTHORIZED,
            "authentication_error",
            "missing bearer token",
        );
    };
    if !token.starts_with("ta_") {
        return openai_auth_error(
            StatusCode::UNAUTHORIZED,
            "authentication_error",
            "invalid bearer token format",
        );
    }
    match state.thin.keys().find(&token) {
        Some(key) if has_scope(&key, SCOPE_MODELS_READ) => next.run(request).await,
        Some(_) => openai_auth_error(
            StatusCode::FORBIDDEN,
            "permission_error",
            "key lacks models:read; run: thickapi keys create --label <name> --scope chat:write (chat:write satisfies models:read) or --scope models:read",
        ),
        None => openai_auth_error(StatusCode::UNAUTHORIZED, "authentication_error", "invalid bearer token"),
    }
}

/// Thick route (`/v1/agent/turn*`): unchanged `{code,message}` shape (out of scope, D3).
pub async fn require_turn_write(
    State(state): State<AppState>,
    request: Request<Body>,
    next: Next,
) -> Response {
    auth_with_scope(state, request, next, SCOPE_TURN_WRITE).await
}

async fn auth_with_scope(
    state: AppState,
    request: Request<Body>,
    next: Next,
    required_scope: &str,
) -> Response {
    let token = extract_bearer(request.headers());
    let Some(token) = token else {
        return unauthorized("missing bearer token");
    };
    if !token.starts_with("ta_") {
        return unauthorized("invalid bearer token format");
    }

    // Keys minted after start are found when keys.yaml mtime changes (LiveKeys), no restart needed.
    let entry = state.thin.keys().find(&token);
    match entry {
        Some(key) if has_scope(&key, required_scope) => next.run(request).await,
        Some(_) => unauthorized("insufficient scope"),
        None => unauthorized("invalid bearer token"),
    }
}

fn extract_bearer(headers: &axum::http::HeaderMap) -> Option<String> {
    headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn unauthorized(message: &str) -> Response {
    (
        StatusCode::UNAUTHORIZED,
        Json(json!({
            "code": "unauthorized",
            "message": message,
        })),
    )
        .into_response()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use std::sync::Arc;

    use axum::{routing::get, Router};
    use http_body_util::BodyExt;
    use thickapi_core::config::keys::{hash_token, KeyEntry, KeysConfig};
    use tower::ServiceExt;

    use super::*;
    use crate::AppState;

    fn test_state(token: &str) -> (AppState, String) {
        let hash = hash_token(token);
        let config = thickapi_core::ConfigBundle {
            config_dir: std::path::PathBuf::from("."),
            router: thickapi_core::config::RouterConfig {
                listen: "127.0.0.1:8787".into(),
                data_dir: std::env::temp_dir().join("thickapi-test"),
                lan: false,
                default_timeout_seconds: 5400,
                queue_timeout_seconds: 300,
                max_queue_depth: 16,
                max_paid_usd_per_day: 5.0,
                drain_seconds: 120,
                refuse_paths: vec![],
                ..Default::default()
            },
            accounts: thickapi_core::config::AccountsConfig {
                pools: Default::default(),
            },
            models: thickapi_core::config::ModelsConfig {
                aliases: Default::default(),
                models: vec![],
            },
            combos: thickapi_core::config::CombosConfig {
                combos: Default::default(),
            },
            keys: KeysConfig {
                keys: vec![KeyEntry {
                    id: "test".into(),
                    hash,
                    scopes: vec![SCOPE_MODELS_READ.into(), SCOPE_TURN_WRITE.into()],
                    ..Default::default()
                }],
            },
        };
        (
            crate::server::make_state_with_store(
                config,
                Arc::new(thickapi_core::Store::open_in_memory().expect("store")),
            ),
            token.to_string(),
        )
    }

    #[tokio::test]
    async fn auth_middleware_rejects_invalid() {
        let (state, _) = test_state("ta_valid");
        let app = Router::new()
            .route("/protected", get(|| async { "ok" }))
            .layer(axum::middleware::from_fn_with_state(
                state.clone(),
                require_models_read,
            ));

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/protected")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn auth_middleware_accepts_valid() {
        let (state, token) = test_state("ta_valid");
        let app = Router::new()
            .route("/protected", get(|| async { "ok" }))
            .layer(axum::middleware::from_fn_with_state(
                state.clone(),
                require_models_read,
            ));

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/protected")
                    .header("Authorization", format!("Bearer {}", token))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::OK);
        let body = response
            .into_body()
            .collect()
            .await
            .expect("body")
            .to_bytes();
        assert_eq!(body, "ok");
    }

    /// D3 deliverable 4: `require_models_read` (guards `/ready`, `/v1/models`) answers the
    /// OpenAI envelope, not the thick `{code,message}` shape — 403 `permission_error` for a key
    /// present but lacking `models:read`, 401 `authentication_error` for an unknown key.
    #[tokio::test]
    async fn require_models_read_uses_the_openai_envelope() {
        let (state, _) = test_state("ta_scoped");
        let hash = hash_token("ta_scoped");
        let config = thickapi_core::ConfigBundle {
            keys: KeysConfig {
                keys: vec![KeyEntry {
                    id: "turn-only".into(),
                    hash,
                    scopes: vec![SCOPE_TURN_WRITE.into()],
                    ..Default::default()
                }],
            },
            ..state.config.clone()
        };
        let state = crate::server::make_state_with_store(
            config,
            Arc::new(thickapi_core::Store::open_in_memory().expect("store")),
        );
        let app = Router::new()
            .route("/protected", get(|| async { "ok" }))
            .layer(axum::middleware::from_fn_with_state(
                state.clone(),
                require_models_read,
            ));

        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/protected")
                    .header("Authorization", "Bearer ta_scoped")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert!(response.headers().contains_key("x-request-id"));
        let body = response
            .into_body()
            .collect()
            .await
            .expect("body")
            .to_bytes();
        let v: serde_json::Value = serde_json::from_slice(&body).expect("json");
        assert_eq!(v["error"]["code"], "permission_error");
        assert_eq!(v["error"]["type"], "permission_error");
        assert!(
            v["error"]["message"]
                .as_str()
                .expect("message")
                .ends_with(" [code=permission_error]"),
            "{v}"
        );

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/protected")
                    .header("Authorization", "Bearer ta_unknown")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        let body = response
            .into_body()
            .collect()
            .await
            .expect("body")
            .to_bytes();
        let v: serde_json::Value = serde_json::from_slice(&body).expect("json");
        assert_eq!(v["error"]["code"], "authentication_error");
    }
}
