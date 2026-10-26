#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
//! B5: a thick turn that cannot spawn its CLI must reach the client as a typed failure on
//! both routes — a 502 body on JSON, a `turn_end` frame on SSE — not a 200 with nothing in it
//! and not a stream that simply stops.

use std::sync::Arc;

use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use thickapi_api::{build_router, make_state_with_store, AppState};
use thickapi_core::config::keys::{hash_token, KeyEntry, KeysConfig};
use thickapi_core::config::RouterConfig;
use thickapi_core::{ConfigBundle, Store};
use tower::ServiceExt;

const TOKEN: &str = "ta_turn_failure";

/// Every test in this binary wants the same unspawnable child, so the env var is set once and
/// never cleared: `THICKAPI_TEST_CHILD` is process-wide and these run in parallel threads.
fn missing_cli_state() -> (AppState, tempfile::TempDir) {
    let missing = std::env::temp_dir().join("thickapi-no-such-cli-49d1c0.exe");
    let _ = std::fs::remove_file(&missing);
    unsafe {
        std::env::set_var("THICKAPI_TEST_CHILD", missing.to_string_lossy().to_string());
    }

    let temp = tempfile::tempdir().expect("tempdir");
    let config = ConfigBundle {
        config_dir: temp.path().to_path_buf(),
        router: RouterConfig {
            listen: "127.0.0.1:8787".into(),
            data_dir: temp.path().join("data"),
            default_timeout_seconds: 30,
            ..Default::default()
        },
        accounts: serde_yaml::from_str(include_str!("../../../configs/accounts.yaml.example"))
            .expect("accounts"),
        models: serde_yaml::from_str(include_str!("../../../configs/models.yaml.example"))
            .expect("models"),
        combos: serde_yaml::from_str(include_str!("../../../configs/combos.yaml.example"))
            .expect("combos"),
        keys: KeysConfig {
            keys: vec![KeyEntry {
                id: "test".into(),
                hash: hash_token(TOKEN),
                scopes: vec!["turn:write".into()],
                ..Default::default()
            }],
        },
    };
    let state = make_state_with_store(config, Arc::new(Store::open_in_memory().expect("store")));
    (state, temp)
}

fn turn_body(session: &str, turn_id: &str) -> String {
    json!({
        "model": "subs/claude/fable",
        "prompt": "hello",
        "cwd": std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../.."),
        "session_id": session,
        "timeout_seconds": 30,
        "turn_id": turn_id
    })
    .to_string()
}

fn turn_request(accept: &str, session: &str, turn_id: &str) -> Request<axum::body::Body> {
    Request::builder()
        .method("POST")
        .uri("/v1/agent/turn")
        .header("Authorization", format!("Bearer {TOKEN}"))
        .header("Content-Type", "application/json")
        .header("Accept", accept)
        .body(axum::body::Body::from(turn_body(session, turn_id)))
        .expect("request")
}

#[tokio::test]
async fn json_turn_with_unspawnable_cli_is_a_typed_502() {
    let (state, _temp) = missing_cli_state();
    let registry = state.registry.clone();
    let app = build_router(state);
    let response = app
        .oneshot(turn_request(
            "application/json",
            "no-cli-json",
            "t-no-cli-json",
        ))
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes();
    let body: Value = serde_json::from_slice(&bytes).expect("json");
    assert_eq!(body["code"], "spawn_failed");
    assert!(
        registry.is_terminal(&"t-no-cli-json".to_string()),
        "the registry entry must not stay Running"
    );
}

#[tokio::test]
async fn sse_turn_with_unspawnable_cli_emits_turn_end() {
    let (state, _temp) = missing_cli_state();
    let app = build_router(state);
    let response = app
        .oneshot(turn_request(
            "text/event-stream",
            "no-cli-sse",
            "t-no-cli-sse",
        ))
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes();
    let text = String::from_utf8_lossy(&bytes);
    assert!(
        text.contains("turn_end"),
        "stream must not just stop: {text}"
    );
    assert!(text.contains("spawn_failed"), "{text}");
}

/// C17: what the router refuses is the client's to act on, so it keeps its own code and
/// status instead of flattening to `internal_error` / 500.
#[tokio::test]
async fn blocked_account_is_a_429_not_a_500() {
    let (state, _temp) = missing_cli_state();
    state
        .store
        .upsert_account_health("claude-main", "rate_limited")
        .expect("health");
    let app = build_router(state);

    let response = app
        .clone()
        .oneshot(turn_request(
            "application/json",
            "blocked-json",
            "t-blocked-json",
        ))
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes();
    let body: Value = serde_json::from_slice(&bytes).expect("json");
    assert_eq!(body["code"], "rate_limited");
    assert!(
        body["message"]
            .as_str()
            .unwrap_or_default()
            .contains("claude-main"),
        "{body}"
    );

    let response = app
        .oneshot(turn_request(
            "text/event-stream",
            "blocked-sse",
            "t-blocked-sse",
        ))
        .await
        .expect("response");
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes();
    let text = String::from_utf8_lossy(&bytes);
    assert!(text.contains("\"state\":\"rate_limited\""), "{text}");
    assert!(text.contains("retry_after_seconds"), "{text}");
}

#[tokio::test]
async fn paid_model_without_allow_paid_is_a_400_not_a_500() {
    let (state, _temp) = missing_cli_state();
    let model = state
        .config
        .models
        .models
        .iter()
        .map(|m| m.id.clone())
        .find(|id| id.starts_with("or/"))
        .expect("the example catalogue has an or/ row");
    let body = json!({
        "model": model,
        "prompt": "hello",
        "cwd": std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../.."),
        "session_id": "paid-no-opt-in",
        "turn_id": "t-paid-no-opt-in"
    })
    .to_string();
    let response = build_router(state)
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/agent/turn")
                .header("Authorization", format!("Bearer {TOKEN}"))
                .header("Content-Type", "application/json")
                .body(axum::body::Body::from(body))
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes();
    let body: Value = serde_json::from_slice(&bytes).expect("json");
    assert_eq!(body["code"], "invalid_spec");
}

/// A retry of a failed `turn_id` must not read as success: the replay carries the status the
/// first answer had, from the registry and, once that has let go of the turn, from disk.
#[tokio::test]
async fn a_replayed_failure_keeps_its_first_status() {
    let (mut state, _temp) = missing_cli_state();
    state.registry = Arc::new(thickapi_core::TurnRegistry::with_terminal_ttl(
        std::time::Duration::from_millis(1),
    ));
    state.engine = Arc::new(
        thickapi_core::TurnEngine::new(state.config.clone(), state.store.clone())
            .with_registry(state.registry.clone())
            .with_pools(state.pools.clone()),
    );
    let registry = state.registry.clone();
    let app = build_router(state);

    let mut statuses = Vec::new();
    for round in 0..3 {
        if round == 2 {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            assert_eq!(registry.evict_expired(), 1, "third answer comes from disk");
        }
        let response = app
            .clone()
            .oneshot(turn_request(
                "application/json",
                "replay-fail",
                "t-replay-fail",
            ))
            .await
            .expect("response");
        let status = response.status();
        let bytes = response
            .into_body()
            .collect()
            .await
            .expect("body")
            .to_bytes();
        let body: Value = serde_json::from_slice(&bytes).expect("json");
        assert_eq!(body["code"], "spawn_failed", "round {round}: {body}");
        statuses.push(status);
    }
    assert_eq!(statuses, vec![StatusCode::BAD_GATEWAY; 3]);

    // Status lookup is not a replay: it reports the stored result.
    let response = app
        .oneshot(
            Request::builder()
                .uri("/v1/agent/turn/t-replay-fail")
                .header("Authorization", format!("Bearer {TOKEN}"))
                .body(axum::body::Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::OK);
}

/// SPEC §3.1: a subscription block is `402 subscription_blocked`, on the first answer and on
/// the replay, and the SSE stream ends in that state.
#[tokio::test]
async fn subscription_blocked_account_is_a_402() {
    let (state, _temp) = missing_cli_state();
    state
        .store
        .upsert_account_health("claude-main", "subscription_blocked")
        .expect("health");
    let app = build_router(state);

    for round in 0..2 {
        let response = app
            .clone()
            .oneshot(turn_request(
                "application/json",
                "sub-blocked",
                "t-sub-blocked",
            ))
            .await
            .expect("response");
        assert_eq!(
            response.status(),
            StatusCode::PAYMENT_REQUIRED,
            "round {round}"
        );
        let bytes = response
            .into_body()
            .collect()
            .await
            .expect("body")
            .to_bytes();
        let body: Value = serde_json::from_slice(&bytes).expect("json");
        assert_eq!(body["code"], "subscription_blocked");
    }

    let response = app
        .oneshot(turn_request(
            "text/event-stream",
            "sub-blocked-sse",
            "t-sub-blocked-sse",
        ))
        .await
        .expect("response");
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes();
    let text = String::from_utf8_lossy(&bytes);
    assert!(
        text.contains("\"state\":\"subscription_blocked\""),
        "{text}"
    );
}
