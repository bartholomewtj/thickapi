use std::convert::Infallible;

use axum::{
    extract::{rejection::JsonRejection, State},
    http::{header, HeaderMap, StatusCode},
    response::{
        sse::{Event, KeepAlive, Sse},
        IntoResponse, Response,
    },
    Json,
};
use futures_core::Stream;
use serde::Serialize;
use thickapi_core::{
    remember_turn_prompt, turn_artifact_dir, turn_prompt_conflicts, StoredEvent, TurnError,
    TurnEvent, TurnResult, TurnSpec,
};
use uuid::Uuid;

use crate::AppState;

#[derive(Serialize)]
pub struct ErrorBody {
    pub code: String,
    pub message: String,
}

pub async fn post_turn(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Result<Json<TurnSpec>, JsonRejection>,
) -> Response {
    let Json(mut spec) = match body {
        Ok(spec) => spec,
        Err(rejection) => return spec_parse_error(rejection),
    };
    if state.pools.is_draining() {
        return error_response(TurnError::Draining(
            "server is draining; new turns are not accepted".into(),
        ));
    }

    if spec.turn_id.is_none() {
        if let Some(key) = headers.get("Idempotency-Key").and_then(|v| v.to_str().ok()) {
            spec.turn_id = Some(key.to_string());
        }
    }
    let turn_id = spec
        .turn_id
        .clone()
        .unwrap_or_else(|| Uuid::new_v4().to_string());
    spec.turn_id = Some(turn_id.clone());

    if turn_prompt_conflicts(&state.config.router.data_dir, &turn_id, &spec.prompt) == Some(true) {
        return error_response(TurnError::TurnConflict(format!(
            "turn_id {turn_id} already used with a different prompt"
        )));
    }

    let accept = headers
        .get(header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/json");

    if accept.contains("text/event-stream") {
        return turn_sse(state, spec, turn_id).await.into_response();
    }

    match state.registry.status(&turn_id) {
        Some(thickapi_core::TurnStatus::Finished(result)) => {
            return replay_response(*result);
        }
        Some(
            thickapi_core::TurnStatus::Running | thickapi_core::TurnStatus::RunningWithState(_),
        ) => {
            return error_response(TurnError::TurnInProgress(turn_id));
        }
        None => {}
    }

    // Evicted from memory (D1) or finished before a restart: the turn id is still spent, and
    // the idempotent answer is the stored result, not a second run.
    if let Some(result) = stored_result(&state, &turn_id) {
        return replay_response(result);
    }

    if state.registry.register(turn_id.clone()).is_none() {
        return error_response(TurnError::TurnInProgress(turn_id));
    }
    let _ = remember_turn_prompt(&state.config.router.data_dir, &turn_id, &spec.prompt);
    // The turn runs on its own task, as on the SSE path: when the client hangs up axum drops
    // this handler, and a turn awaited inline would be dropped with it mid-run. SPEC §1.7 says
    // a thick turn completes and is picked up again through `GET /v1/agent/turn/{id}` (B10).
    let engine = state.engine.clone();
    let run = tokio::spawn(async move {
        let (tx, mut rx) = tokio::sync::mpsc::channel(64);
        let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });
        let result = engine.run_turn(spec, tx).await;
        drain.await.ok();
        result
    });
    match run.await {
        Ok(Ok(result)) => (StatusCode::OK, Json(result)).into_response(),
        Ok(Err(err)) => error_response(err),
        Err(join) => error_response(TurnError::Other(format!("turn task failed: {join}"))),
    }
}

/// Answer a re-`POST` of a spent `turn_id` the way the first `POST` was answered. A turn the
/// engine ended itself (`returncode == -1`) went out as a typed error — 429, 502, 504 — and a
/// replay that turned it into `200` would tell a retrying client its failed turn had worked.
/// A turn the child ended, whatever its state, went out as `200` with the result.
fn replay_response(result: TurnResult) -> Response {
    match replayed_error(&result) {
        Some(err) => error_response(err),
        None => (StatusCode::OK, Json(result)).into_response(),
    }
}

fn replayed_error(result: &TurnResult) -> Option<TurnError> {
    use thickapi_core::TurnState;
    if result.returncode != -1 {
        return None;
    }
    if let Some(code) = result.error_code.as_deref() {
        if let Some(err) = error_from_code(code, &result.text) {
            return Some(err);
        }
    }
    // Older `result.json` files have no `error_code`; fall back to prose then state.
    if let Some((code, message)) = result.text.split_once(": ") {
        if let Some(typed) = error_from_code(code, message) {
            return Some(typed);
        }
    }
    match result.state {
        TurnState::Timeout => Some(TurnError::TurnTimeout(result.text.clone())),
        TurnState::RateLimited => Some(TurnError::RateLimited(result.text.clone())),
        TurnState::SubscriptionBlocked => Some(TurnError::SubscriptionBlocked(result.text.clone())),
        _ => None,
    }
}

fn error_from_code(code: &str, text: &str) -> Option<TurnError> {
    let prefix = format!("{code}: ");
    let message = text.strip_prefix(&prefix).unwrap_or(text).to_string();
    match code {
        "refused_workspace" => Some(TurnError::RefusedWorkspace(message)),
        "unknown_model" => Some(TurnError::UnknownModel(message)),
        "invalid_spec" => Some(TurnError::InvalidSpec(message)),
        "spawn_failed" => Some(TurnError::SpawnFailed(message)),
        "turn_timeout" => Some(TurnError::TurnTimeout(message)),
        "queue_timeout" => Some(TurnError::QueueTimeout(message)),
        "rate_limited" => Some(TurnError::RateLimited(message)),
        "subscription_blocked" => Some(TurnError::SubscriptionBlocked(message)),
        "internal_error" => Some(TurnError::Other(message)),
        _ => None,
    }
}

/// `result.json` of a turn the registry no longer holds.
pub(crate) fn stored_result(state: &AppState, turn_id: &str) -> Option<TurnResult> {
    let dir = turn_artifact_dir(&state.config.router.data_dir, turn_id);
    let content = std::fs::read_to_string(dir.join("result.json")).ok()?;
    serde_json::from_str(&content).ok()
}

/// Replay a persisted terminal turn for SSE. `result.json` is enough: a crash between
/// persist and `TurnEnd` must not start a second run. Missing `TurnEnd` is synthesised.
fn stored_terminal_events(state: &AppState, turn_id: &str) -> Option<Vec<StoredEvent>> {
    let result = stored_result(state, turn_id)?;
    let dir = turn_artifact_dir(&state.config.router.data_dir, turn_id);
    let mut events = thickapi_core::read_events_jsonl(&dir).unwrap_or_default();
    if !events
        .last()
        .is_some_and(|e| matches!(e.event, TurnEvent::TurnEnd { .. }))
    {
        let seq = events.last().map(|e| e.seq).unwrap_or(0) + 1;
        events.push(StoredEvent {
            seq,
            event: TurnEvent::TurnEnd {
                state: result.state,
                returncode: result.returncode,
                text: result.text,
                retry_after_seconds: None,
                touched_paths: vec![],
                breaches: vec![],
            },
        });
    }
    Some(events)
}

async fn turn_sse(
    state: AppState,
    spec: TurnSpec,
    turn_id: String,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    // A finished turn evicted from memory (D1) replays from disk rather than running again.
    let existing = state
        .registry
        .get_existing(&turn_id)
        .or_else(|| stored_terminal_events(&state, &turn_id));
    let (tx, mut rx) = tokio::sync::mpsc::channel(64);
    let spawned = if existing.is_none() {
        if state.registry.register(turn_id.clone()).is_some() {
            let _ = remember_turn_prompt(&state.config.router.data_dir, &turn_id, &spec.prompt);
            let engine = state.engine.clone();
            tokio::spawn(async move {
                let _ = engine.run_turn(spec, tx).await;
            });
            true
        } else {
            false
        }
    } else {
        false
    };

    let registry = state.registry.clone();
    let stream = async_stream::stream! {
        if let Some(events) = existing {
            let mut last_seq = 0u64;
            for stored in events {
                last_seq = stored.seq;
                yield Ok(crate::routes::sse_json_event(stored.seq, stored.event));
            }
            if let Some(mut live) = registry.subscribe(&turn_id) {
                let mut seq = last_seq;
                while let Ok(event) = live.recv().await {
                    seq += 1;
                    let is_end = matches!(&event, thickapi_core::TurnEvent::TurnEnd { .. });
                    yield Ok(crate::routes::sse_json_event(seq, event));
                    if is_end {
                        break;
                    }
                }
            }
        } else if spawned {
            let mut seq = 0u64;
            while let Some(event) = rx.recv().await {
                seq += 1;
                yield Ok(crate::routes::sse_json_event(seq, event));
            }
        } else if let Some(events) = registry.get_existing(&turn_id) {
            for stored in events {
                yield Ok(crate::routes::sse_json_event(stored.seq, stored.event));
            }
            if let Some(mut live) = registry.subscribe(&turn_id) {
                let mut seq = registry.last_seq(&turn_id).unwrap_or(0);
                while let Ok(event) = live.recv().await {
                    seq += 1;
                    let is_end = matches!(&event, thickapi_core::TurnEvent::TurnEnd { .. });
                    yield Ok(crate::routes::sse_json_event(seq, event));
                    if is_end {
                        break;
                    }
                }
            }
        }
    };
    Sse::new(stream).keep_alive(KeepAlive::default())
}

pub fn error_response(err: TurnError) -> Response {
    let status =
        StatusCode::from_u16(err.http_status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    (
        status,
        Json(ErrorBody {
            code: err.code().to_string(),
            message: err.to_string(),
        }),
    )
        .into_response()
}

/// serde `deny_unknown_fields` is axum's 422; keep that status but use the thick envelope
/// (same family as the thin 413, SPEC §3.1).
fn spec_parse_error(rejection: JsonRejection) -> Response {
    (
        rejection.status(),
        Json(ErrorBody {
            code: "invalid_spec".into(),
            message: format!("invalid_spec: {}", rejection.body_text()),
        }),
    )
        .into_response()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use std::sync::Arc;

    use http_body_util::BodyExt;
    use thickapi_core::config::keys::{hash_token, KeyEntry, KeysConfig};
    use tower::ServiceExt;

    use super::*;
    use crate::middleware::auth::SCOPE_TURN_WRITE;
    use crate::{build_router, AppState};

    fn fixture_child() -> Option<String> {
        let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures");
        let cmd = root.join("fake-claude.cmd");
        if cmd.exists() {
            Some(cmd.to_string_lossy().to_string())
        } else {
            None
        }
    }

    fn test_models() -> thickapi_core::config::ModelsConfig {
        serde_yaml::from_str(include_str!("../../../../configs/models.yaml.example"))
            .expect("models")
    }

    fn test_accounts() -> thickapi_core::config::AccountsConfig {
        serde_yaml::from_str(include_str!("../../../../configs/accounts.yaml.example"))
            .expect("accounts")
    }

    fn test_state_with_queue_depth(token: &str, max_queue_depth: u32) -> AppState {
        let hash = hash_token(token);
        let temp = tempfile::tempdir().expect("tempdir");
        let mut accounts = test_accounts();
        if let Some(claude) = accounts.pools.get_mut("claude") {
            claude.max_concurrent = 1;
        }
        let config = thickapi_core::ConfigBundle {
            config_dir: temp.path().to_path_buf(),
            router: thickapi_core::config::RouterConfig {
                listen: "127.0.0.1:8787".into(),
                data_dir: temp.path().join("data"),
                lan: false,
                default_timeout_seconds: 60,
                queue_timeout_seconds: 300,
                max_queue_depth,
                max_paid_usd_per_day: 5.0,
                drain_seconds: 120,
                refuse_paths: vec![],
                ..Default::default()
            },
            accounts,
            models: test_models(),
            combos: thickapi_core::config::CombosConfig {
                combos: Default::default(),
            },
            keys: KeysConfig {
                keys: vec![KeyEntry {
                    id: "test".into(),
                    hash,
                    scopes: vec![SCOPE_TURN_WRITE.into()],
                    ..Default::default()
                }],
            },
        };
        crate::server::make_state_with_store(
            config,
            Arc::new(thickapi_core::Store::open_in_memory().expect("store")),
        )
    }

    fn test_state(token: &str) -> AppState {
        test_state_with_queue_depth(token, 16)
    }

    #[tokio::test]
    async fn turn_post_json_with_fixture_child() {
        let child = fixture_child();
        if child.is_none() {
            return;
        }
        unsafe {
            std::env::set_var("THICKAPI_TEST_CHILD", child.unwrap());
        }

        let state = test_state("ta_turn_test");
        let app = build_router(state);
        let cwd = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
        let body = serde_json::json!({
            "model": "subs/claude/fable",
            "prompt": "hello",
            "cwd": cwd,
            "session_id": "sess-1",
            "timeout_seconds": 30
        });
        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/v1/agent/turn")
                    .header("Authorization", "Bearer ta_turn_test")
                    .header("Content-Type", "application/json")
                    .body(axum::body::Body::from(body.to_string()))
                    .unwrap(),
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
        let result: TurnResult = serde_json::from_slice(&bytes).expect("json");
        assert!(!result.text.is_empty());
        unsafe {
            std::env::remove_var("THICKAPI_TEST_CHILD");
        }
    }

    #[tokio::test]
    async fn turn_post_sse_yields_events() {
        let child = fixture_child();
        if child.is_none() {
            return;
        }
        unsafe {
            std::env::set_var("THICKAPI_TEST_CHILD", child.unwrap());
        }

        let state = test_state("ta_turn_test");
        let app = build_router(state);
        let cwd = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
        let body = serde_json::json!({
            "model": "subs/claude/fable",
            "prompt": "hello",
            "cwd": cwd,
            "session_id": "sess-sse",
            "timeout_seconds": 30
        });
        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/v1/agent/turn")
                    .header("Authorization", "Bearer ta_turn_test")
                    .header("Content-Type", "application/json")
                    .header("Accept", "text/event-stream")
                    .body(axum::body::Body::from(body.to_string()))
                    .unwrap(),
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
        let text = String::from_utf8_lossy(&bytes);
        assert!(text.contains("data:"));
        unsafe {
            std::env::remove_var("THICKAPI_TEST_CHILD");
        }
    }

    #[tokio::test]
    async fn turn_sse_pool_rate_limited_emits_turn_end() {
        let state = test_state_with_queue_depth("ta_turn_test", 0);
        let _guard = state
            .pools
            .acquire("claude", &mut |_| {}, None)
            .await
            .expect("hold pool slot");

        let app = build_router(state);
        let cwd = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
        let body = serde_json::json!({
            "model": "subs/claude/fable",
            "prompt": "hello",
            "cwd": cwd,
            "session_id": "sess-rate-limit",
            "timeout_seconds": 30
        });
        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/v1/agent/turn")
                    .header("Authorization", "Bearer ta_turn_test")
                    .header("Content-Type", "application/json")
                    .header("Accept", "text/event-stream")
                    .body(axum::body::Body::from(body.to_string()))
                    .unwrap(),
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
        let text = String::from_utf8_lossy(&bytes);
        assert!(text.contains("turn_end"));
        assert!(text.contains("rate_limited"));
        assert!(text.contains("retry_after_seconds"));
    }

    fn stored(state: thickapi_core::TurnState, returncode: i32, text: &str) -> TurnResult {
        serde_json::from_value(serde_json::json!({
            "turn_id": "t", "session_id": "s", "state": state, "returncode": returncode,
            "text": text, "tokens": 0, "cost": 0.0, "context_tokens": 0,
            "usage": thickapi_core::UsageBreakdown::default(), "context_window": 0,
            "backend": "claude_code", "account_id": "", "resolved_model": ""
        }))
        .expect("result")
    }

    #[test]
    fn replay_status_follows_how_the_turn_ended() {
        use thickapi_core::TurnState;
        let status = |r: &TurnResult| replayed_error(r).map(|e| e.http_status());
        assert_eq!(
            status(&stored(TurnState::Timeout, -1, "half an answer")),
            Some(504)
        );
        assert_eq!(
            status(&stored(TurnState::Timeout, -1, "queue_timeout: 300s")),
            Some(504)
        );
        assert_eq!(
            status(&stored(TurnState::RateLimited, -1, "pool claude is full")),
            Some(429)
        );
        assert_eq!(
            status(&stored(TurnState::Failed, -1, "invalid_spec: no cwd")),
            Some(400)
        );
        // Ended by the child: the first answer was 200 with this result, so the replay is too.
        assert_eq!(
            status(&stored(
                TurnState::Failed,
                3,
                "spawn_failed: said the model"
            )),
            None
        );
        assert_eq!(
            status(&stored(TurnState::RateLimited, 1, "429 from vendor")),
            None
        );
        assert_eq!(status(&stored(TurnState::Success, 0, "done")), None);
        // F5: stored error_code wins over prose. F3: a panic persist is 500, not 200.
        assert_eq!(
            status(&stored_with_code(
                TurnState::Failed,
                -1,
                "internal_error: the turn panicked before it finished",
                "internal_error"
            )),
            Some(500)
        );
        assert_eq!(
            status(&stored_with_code(
                TurnState::RateLimited,
                -1,
                "daily paid cap exceeded",
                "rate_limited"
            )),
            Some(429)
        );
        // error_code is authoritative even when text would parse as a different code.
        assert_eq!(
            status(&stored_with_code(
                TurnState::Failed,
                -1,
                "invalid_spec: no cwd",
                "internal_error"
            )),
            Some(500)
        );
    }

    fn stored_with_code(
        state: thickapi_core::TurnState,
        returncode: i32,
        text: &str,
        error_code: &str,
    ) -> TurnResult {
        serde_json::from_value(serde_json::json!({
            "turn_id": "t", "session_id": "s", "state": state, "returncode": returncode,
            "text": text, "tokens": 0, "cost": 0.0, "context_tokens": 0,
            "usage": thickapi_core::UsageBreakdown::default(), "context_window": 0,
            "backend": "claude_code", "account_id": "", "resolved_model": "",
            "error_code": error_code
        }))
        .expect("result")
    }

    #[tokio::test]
    async fn turn_post_rejected_while_draining() {
        let state = test_state("ta_turn_test");
        state.pools.set_draining(true);
        let app = build_router(state);
        let cwd = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
        let body = serde_json::json!({
            "model": "subs/claude/fable",
            "prompt": "hello",
            "cwd": cwd,
            "session_id": "sess-drain",
            "timeout_seconds": 30
        });
        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/v1/agent/turn")
                    .header("Authorization", "Bearer ta_turn_test")
                    .header("Content-Type", "application/json")
                    .body(axum::body::Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        let bytes = response
            .into_body()
            .collect()
            .await
            .expect("body")
            .to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&bytes).expect("json");
        assert_eq!(json["code"], "draining");
    }

    fn test_state_kept(token: &str) -> (AppState, tempfile::TempDir) {
        let hash = hash_token(token);
        let temp = tempfile::tempdir().expect("tempdir");
        let config = thickapi_core::ConfigBundle {
            config_dir: temp.path().to_path_buf(),
            router: thickapi_core::config::RouterConfig {
                listen: "127.0.0.1:8787".into(),
                data_dir: temp.path().join("data"),
                lan: false,
                default_timeout_seconds: 60,
                queue_timeout_seconds: 300,
                max_queue_depth: 16,
                max_paid_usd_per_day: 5.0,
                drain_seconds: 120,
                refuse_paths: vec![],
                ..Default::default()
            },
            accounts: test_accounts(),
            models: test_models(),
            combos: thickapi_core::config::CombosConfig {
                combos: Default::default(),
            },
            keys: KeysConfig {
                keys: vec![KeyEntry {
                    id: "test".into(),
                    hash,
                    scopes: vec![SCOPE_TURN_WRITE.into()],
                    ..Default::default()
                }],
            },
        };
        (
            crate::server::make_state_with_store(
                config,
                Arc::new(thickapi_core::Store::open_in_memory().expect("store")),
            ),
            temp,
        )
    }

    fn seed_finished_turn(state: &AppState, turn_id: &str, prompt: &str, text: &str) {
        let dir = turn_artifact_dir(&state.config.router.data_dir, turn_id);
        std::fs::create_dir_all(&dir).expect("turn dir");
        remember_turn_prompt(&state.config.router.data_dir, turn_id, prompt).expect("hash");
        let result = serde_json::json!({
            "turn_id": turn_id, "session_id": "s", "state": "success", "returncode": 0,
            "text": text, "tokens": 0, "cost": 0.0, "context_tokens": 0,
            "usage": thickapi_core::UsageBreakdown::default(), "context_window": 0,
            "backend": "claude_code", "account_id": "claude-main", "resolved_model": "claude-fable-5-1"
        });
        std::fs::write(dir.join("result.json"), result.to_string()).expect("result");
    }

    #[tokio::test]
    async fn a_spent_turn_id_with_a_different_prompt_is_conflict() {
        let (state, _temp) = test_state_kept("ta_turn_test");
        seed_finished_turn(&state, "t-conflict", "first prompt", "FIRST");
        let cwd = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
        let app = build_router(state);
        let body = serde_json::json!({
            "model": "subs/claude/fable",
            "prompt": "a different prompt",
            "cwd": cwd,
            "session_id": "sess-conflict",
            "turn_id": "t-conflict",
            "timeout_seconds": 30
        });
        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/v1/agent/turn")
                    .header("Authorization", "Bearer ta_turn_test")
                    .header("Content-Type", "application/json")
                    .body(axum::body::Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::CONFLICT);
        let bytes = response
            .into_body()
            .collect()
            .await
            .expect("body")
            .to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&bytes).expect("json");
        assert_eq!(json["code"], "turn_conflict");
    }

    #[tokio::test]
    async fn a_spent_turn_id_with_the_same_prompt_replays() {
        let (state, _temp) = test_state_kept("ta_turn_test");
        seed_finished_turn(&state, "t-replay", "same prompt", "STORED-ANSWER");
        let cwd = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
        let app = build_router(state);
        let body = serde_json::json!({
            "model": "subs/claude/fable",
            "prompt": "same prompt",
            "cwd": cwd,
            "session_id": "sess-replay",
            "turn_id": "t-replay",
            "timeout_seconds": 30
        });
        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/v1/agent/turn")
                    .header("Authorization", "Bearer ta_turn_test")
                    .header("Content-Type", "application/json")
                    .body(axum::body::Body::from(body.to_string()))
                    .unwrap(),
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
        let result: TurnResult = serde_json::from_slice(&bytes).expect("json");
        assert_eq!(result.text, "STORED-ANSWER");
    }

    #[tokio::test]
    async fn sse_replays_a_persisted_turn_even_without_turn_end() {
        let (state, _temp) = test_state_kept("ta_turn_test");
        seed_finished_turn(&state, "t-sse-persist", "sse prompt", "FROM-STORE");
        // No events.jsonl — the crash-between-persist-and-emit case.
        let cwd = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
        let app = build_router(state);
        let body = serde_json::json!({
            "model": "subs/claude/fable",
            "prompt": "sse prompt",
            "cwd": cwd,
            "session_id": "sess-sse-persist",
            "turn_id": "t-sse-persist",
            "timeout_seconds": 30
        });
        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/v1/agent/turn")
                    .header("Authorization", "Bearer ta_turn_test")
                    .header("Content-Type", "application/json")
                    .header("Accept", "text/event-stream")
                    .body(axum::body::Body::from(body.to_string()))
                    .unwrap(),
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
        let text = String::from_utf8_lossy(&bytes);
        assert!(text.contains("FROM-STORE"), "{text}");
        assert!(text.contains("turn_end"), "{text}");
        assert!(!text.contains("turn_start"), "{text}");
    }

    #[tokio::test]
    async fn unknown_turn_spec_field_is_422_envelope() {
        let app = build_router(test_state("ta_turn_test"));
        let cwd = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
        let body = serde_json::json!({
            "model": "subs/claude/fable",
            "prompt": "hello",
            "cwd": cwd,
            "session_id": "sess-unknown-field",
            "enforcePolicy": {"writes": []}
        });
        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/v1/agent/turn")
                    .header("Authorization", "Bearer ta_turn_test")
                    .header("Content-Type", "application/json")
                    .body(axum::body::Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
        let bytes = response
            .into_body()
            .collect()
            .await
            .expect("body")
            .to_bytes();
        let body: serde_json::Value = serde_json::from_slice(&bytes).expect("json");
        assert_eq!(body["code"], "invalid_spec", "{body}");
        let message = body["message"].as_str().unwrap_or("");
        assert!(message.contains("unknown field"), "{body}");
        assert!(
            body.get("error").is_none(),
            "thick envelope, not axum: {body}"
        );
    }
}
