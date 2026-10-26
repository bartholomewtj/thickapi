#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
//! B10 / D1: a JSON thick turn outlives the connection that started it (SPEC §1.7), and a
//! finished turn the registry has evicted is answered from disk instead of being run again.

#![cfg(windows)]

use std::sync::Arc;
use std::time::Duration;

use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use thickapi_api::{build_router, make_state_with_store, AppState};
use thickapi_core::config::keys::{hash_token, KeyEntry, KeysConfig};
use thickapi_core::config::RouterConfig;
use thickapi_core::{ConfigBundle, Store, TurnStatus};
use tower::ServiceExt;

const TOKEN: &str = "ta_turn_disconnect";
const MARKER: &str = "SURVIVED-THE-HANGUP";

/// A child that thinks for about two seconds, then answers. Every test in this binary uses
/// the same one, so `THICKAPI_TEST_CHILD` is set once and never cleared (it is process-wide
/// and the tests run on parallel threads).
fn slow_child() -> &'static std::path::Path {
    static CHILD: std::sync::OnceLock<(tempfile::TempDir, std::path::PathBuf)> =
        std::sync::OnceLock::new();
    let (_dir, cmd) = CHILD.get_or_init(|| {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            dir.path().join("tail.jsonl"),
            format!(
                "{{\"type\":\"assistant\",\"message\":{{\"content\":[{{\"type\":\"text\",\"text\":\"{MARKER}\"}}]}}}}\r\n"
            ),
        )
        .expect("tail");
        let cmd = dir.path().join("slow.cmd");
        std::fs::write(
            &cmd,
            "@echo off\r\nping -n 3 127.0.0.1 > nul\r\ntype \"%~dp0tail.jsonl\"\r\n",
        )
        .expect("cmd");
        unsafe {
            std::env::set_var("THICKAPI_TEST_CHILD", cmd.to_string_lossy().to_string());
        }
        (dir, cmd)
    });
    cmd
}

fn state() -> (AppState, tempfile::TempDir) {
    slow_child();
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

/// The cwd is the repo root, as in the other route tests: trust seeding records the cwd in the
/// user's real CLI config, so a fresh temp dir per run would litter it.
fn turn_request(accept: &str, turn_id: &str) -> Request<axum::body::Body> {
    let cwd = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let body = json!({
        "model": "subs/claude/fable",
        "prompt": "hello",
        "cwd": cwd,
        "session_id": format!("sess-{turn_id}"),
        "timeout_seconds": 30,
        "turn_id": turn_id
    })
    .to_string();
    Request::builder()
        .method("POST")
        .uri("/v1/agent/turn")
        .header("Authorization", format!("Bearer {TOKEN}"))
        .header("Content-Type", "application/json")
        .header("Accept", accept)
        .body(axum::body::Body::from(body))
        .expect("request")
}

#[tokio::test]
async fn json_turn_completes_after_the_client_hangs_up() {
    let (state, _temp) = state();
    let registry = state.registry.clone();
    let app = build_router(state);
    let turn_id = "t-hangup".to_string();

    // Dropping the in-flight request future is what axum does when the connection closes.
    let hung_up = tokio::time::timeout(
        Duration::from_millis(500),
        app.clone()
            .oneshot(turn_request("application/json", &turn_id)),
    )
    .await;
    assert!(hung_up.is_err(), "the child should still be thinking");

    let mut finished = None;
    for _ in 0..100 {
        if let Some(TurnStatus::Finished(result)) = registry.status(&turn_id) {
            finished = Some(result);
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    let result = finished.expect("the turn must finish without its client");
    assert_eq!(result.state, thickapi_core::TurnState::Success);
    assert!(result.text.contains(MARKER), "{}", result.text);

    // Reconnect the way SPEC §1.7 says to.
    let response = app
        .oneshot(
            Request::builder()
                .uri(format!("/v1/agent/turn/{turn_id}"))
                .header("Authorization", format!("Bearer {TOKEN}"))
                .body(axum::body::Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes();
    let body: Value = serde_json::from_slice(&bytes).expect("json");
    assert!(body["text"].as_str().unwrap_or_default().contains(MARKER));
}

#[tokio::test]
async fn evicted_turn_is_answered_from_disk_not_run_again() {
    let (mut state, _temp) = state();
    state.registry = Arc::new(thickapi_core::TurnRegistry::with_terminal_ttl(
        Duration::from_millis(1),
    ));
    state.engine = Arc::new(
        thickapi_core::TurnEngine::new(state.config.clone(), state.store.clone())
            .with_registry(state.registry.clone())
            .with_pools(state.pools.clone()),
    );
    let registry = state.registry.clone();
    let app = build_router(state);
    let turn_id = "t-evicted".to_string();

    let first = app
        .clone()
        .oneshot(turn_request("application/json", &turn_id))
        .await
        .expect("response");
    assert_eq!(first.status(), StatusCode::OK);

    tokio::time::sleep(Duration::from_millis(20)).await;
    assert_eq!(registry.evict_expired(), 1);
    assert!(registry.is_empty());

    // Same id again: the stored result, straight away — the child takes two seconds.
    let again = tokio::time::timeout(
        Duration::from_millis(1500),
        app.clone()
            .oneshot(turn_request("application/json", &turn_id)),
    )
    .await
    .expect("a replay must not spawn the child again")
    .expect("response");
    assert_eq!(again.status(), StatusCode::OK);
    let bytes = again.into_body().collect().await.expect("body").to_bytes();
    let body: Value = serde_json::from_slice(&bytes).expect("json");
    assert!(body["text"].as_str().unwrap_or_default().contains(MARKER));
    assert!(registry.is_empty(), "a replay registers nothing");

    let sse = tokio::time::timeout(
        Duration::from_millis(1500),
        app.clone()
            .oneshot(turn_request("text/event-stream", &turn_id)),
    )
    .await
    .expect("an SSE replay must not spawn the child again")
    .expect("response");
    let bytes = sse.into_body().collect().await.expect("body").to_bytes();
    let text = String::from_utf8_lossy(&bytes);
    assert!(text.contains("turn_end") && text.contains(MARKER), "{text}");

    // Cancelling it is "already finished" (409), as it was before eviction — not 404.
    let cancel = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/v1/agent/turn/{turn_id}/cancel"))
                .header("Authorization", format!("Bearer {TOKEN}"))
                .body(axum::body::Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(cancel.status(), StatusCode::CONFLICT);
}
