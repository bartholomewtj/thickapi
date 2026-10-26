#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
//! A2 — thin access log, one line per request end, `warn` on a post-`Started` error.
//!
//! Real router, real `ClaudeAdapter` (mode `only`, so no bridge spawn), Anthropic replaced by a
//! local stand-in, exactly like `chat_completions_e2e.rs`. A `tracing_subscriber::fmt` layer
//! writing into a shared buffer stands in for the daily rolling log file (SPEC §19.15 R5); the
//! tests read that buffer instead of `~/.thickapi/logs/thickapi.<date>.log`.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, Request, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Router;
use http_body_util::BodyExt;
use serde_json::{json, Value};
use thickapi_api::{build_router, make_state_with_store, AppState};
use thickapi_core::config::keys::{hash_token, KeyEntry, KeysConfig, SCOPE_CHAT_WRITE};
use thickapi_core::config::models::{ModelEntry, ModelsConfig, Transport};
use thickapi_core::config::router::{
    ClaudeCfg, ClaudeOauthConfig, ClaudeOauthMode, CursorCfg, OauthBudgets, RouterConfig,
    ThinConfig, Vendors,
};
use thickapi_core::config::{AccountsConfig, CombosConfig};
use thickapi_core::thin::oauth::cursor::{
    connect_frame, field_ld, field_str, FrameDecoder, FLAG_END,
};
use thickapi_core::types::BackendId;
use thickapi_core::{ConfigBundle, Store};
use tower::ServiceExt;
use tracing_subscriber::layer::SubscriberExt;

const TOKEN: &str = "ta_e2e";
const PROMPT: &str = "ZEBRA-7731 reply OK";

// ---- a capturing writer standing in for the rolling log file ----

#[derive(Clone, Default)]
struct Buf(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Buf {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(data);
        Ok(data.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Buf {
    type Writer = Buf;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

impl Buf {
    fn lines(&self) -> Vec<String> {
        let bytes = self.0.lock().unwrap().clone();
        String::from_utf8_lossy(&bytes)
            .lines()
            .map(str::to_string)
            .collect()
    }

    fn access_lines(&self) -> Vec<String> {
        self.lines()
            .into_iter()
            .filter(|l| l.contains("thickapi_api::access"))
            .collect()
    }

    fn warn_lines(&self) -> Vec<String> {
        self.lines()
            .into_iter()
            .filter(|l| l.contains("WARN") && l.contains("thin request error after Started"))
            .collect()
    }
}

/// Every test below builds one of these directly: single-threaded (the default `#[tokio::test]`
/// flavor), so a `tokio::spawn`ed adapter task lands on the same thread as the request and the
/// mock server, and `tracing::subscriber::set_default`'s thread-local guard covers all three.
fn capture() -> (Buf, tracing::subscriber::DefaultGuard) {
    let buf = Buf::default();
    let fmt_layer = tracing_subscriber::fmt::layer()
        .with_writer(buf.clone())
        .with_ansi(false)
        .with_target(true);
    let subscriber = tracing_subscriber::registry().with(fmt_layer);
    let guard = tracing::subscriber::set_default(subscriber);
    (buf, guard)
}

// ---- stand-in Anthropic server (same shape as chat_completions_e2e.rs) ----

/// status, SSE body, extra headers.
type ReplySpec = (u16, String, Vec<(&'static str, &'static str)>);

#[derive(Clone)]
struct Script {
    replies: Arc<Mutex<Vec<ReplySpec>>>,
    hang: Arc<Mutex<bool>>,
}

async fn messages(State(s): State<Script>, _body: String) -> Response {
    if *s.hang.lock().unwrap() {
        // Longer than any `start_deadline_seconds` used below; the route's own deadline ends
        // the request first (SPEC §19.4 — "no request waits ... longer than start_deadline").
        tokio::time::sleep(Duration::from_secs(3600)).await;
    }
    let (status, text, extra) = {
        let mut r = s.replies.lock().unwrap();
        if r.is_empty() {
            (
                500,
                r#"{"error":{"type":"test","message":"exhausted"}}"#.to_string(),
                vec![],
            )
        } else {
            r.remove(0)
        }
    };
    let mut response = (StatusCode::from_u16(status).unwrap(), text).into_response();
    if status == 200 {
        response
            .headers_mut()
            .insert("content-type", "text/event-stream".parse().unwrap());
    }
    for (k, v) in extra {
        response.headers_mut().insert(k, v.parse().unwrap());
    }
    response
}

fn sse(events: &[(&str, Value)]) -> String {
    events
        .iter()
        .map(|(n, d)| format!("event: {n}\ndata: {d}\n\n"))
        .collect()
}

fn text_reply(text: &str) -> ReplySpec {
    (
        200,
        sse(&[
            (
                "message_start",
                json!({"type":"message_start","message":{"id":"m","model":"claude-sonnet-4-5","usage":{"input_tokens":7,"output_tokens":1}}}),
            ),
            (
                "content_block_start",
                json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
            ),
            (
                "content_block_delta",
                json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":text}}),
            ),
            (
                "content_block_stop",
                json!({"type":"content_block_stop","index":0}),
            ),
            (
                "message_delta",
                json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":2}}),
            ),
            ("message_stop", json!({"type":"message_stop"})),
        ]),
        vec![],
    )
}

/// `message_start` (`Started`), a little text, then a mid-stream Anthropic `error` event — the
/// same shape `claude_oauth.rs`'s own unit tests use for a post-`Started` `ThinEvent::Error`.
fn mid_stream_error_reply() -> ReplySpec {
    (
        200,
        sse(&[
            (
                "message_start",
                json!({"type":"message_start","message":{"id":"m","model":"claude-sonnet-4-5","usage":{"input_tokens":7,"output_tokens":1}}}),
            ),
            (
                "content_block_start",
                json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
            ),
            (
                "content_block_delta",
                json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"partial"}}),
            ),
            (
                "error",
                json!({"type":"error","error":{"type":"overloaded_error","message":"busy"}}),
            ),
        ]),
        vec![],
    )
}

async fn mock(replies: Vec<ReplySpec>, hang: bool) -> String {
    let script = Script {
        replies: Arc::new(Mutex::new(replies)),
        hang: Arc::new(Mutex::new(hang)),
    };
    let app = Router::new()
        .route("/v1/messages", axum::routing::post(messages))
        .with_state(script);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr: SocketAddr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}")
}

fn row() -> ModelEntry {
    ModelEntry {
        id: "claude/sonnet".into(),
        backend: BackendId::ClaudeCode,
        cli_model: "sonnet".into(),
        context_window: 200_000,
        max_tokens: 8192,
        thinking: true,
        cost: 0.0,
        name: "Claude Sonnet".into(),
        provider: None,
        transport: Transport::OauthDirect,
        vendor_model: Some("claude-sonnet-4-5".into()),
        enabled: true,
        input: vec!["text".into()],
    }
}

fn app(base_url: &str, tmp: &tempfile::TempDir, start_deadline_seconds: u64) -> Router {
    let creds = tmp.path().join(".credentials.json");
    std::fs::write(
        &creds,
        r#"{"claudeAiOauth":{"accessToken":"sk-ant-oat01-e2e","refreshToken":"r","expiresAt":9999999999999,"subscriptionType":"max"}}"#,
    )
    .unwrap();
    let config = ConfigBundle {
        config_dir: tmp.path().to_path_buf(),
        router: RouterConfig {
            data_dir: tmp.path().join("data"),
            claude_oauth: ClaudeOauthConfig {
                mode: ClaudeOauthMode::Only,
                base_url: Some(base_url.to_string()),
                credentials_path: Some(creds),
                ..Default::default()
            },
            thin: ThinConfig {
                // `0` = the adapter's own first-event budget is off, so a hung mock is caught
                // only by the route's own `start_deadline_seconds` (SPEC §19.4).
                vendors: Vendors {
                    claude: ClaudeCfg {
                        budgets: OauthBudgets {
                            first_event_seconds: 0,
                            ..Default::default()
                        },
                        ..Default::default()
                    },
                    ..Default::default()
                },
                start_deadline_seconds,
                ..Default::default()
            },
            ..Default::default()
        },
        accounts: AccountsConfig {
            pools: Default::default(),
        },
        models: ModelsConfig {
            aliases: Default::default(),
            models: vec![row()],
        },
        combos: CombosConfig {
            combos: Default::default(),
        },
        keys: KeysConfig {
            keys: vec![KeyEntry {
                id: "e2e".into(),
                hash: hash_token(TOKEN),
                scopes: vec![SCOPE_CHAT_WRITE.into()],
                ..Default::default()
            }],
        },
    };
    let state: AppState = make_state_with_store(config, Arc::new(Store::open_in_memory().unwrap()));
    build_router(state)
}

async fn post(app: Router, stream: bool) -> (StatusCode, HeaderMap, String) {
    let body = json!({
        "model": "claude/sonnet",
        "messages": [{"role": "user", "content": PROMPT}],
        "stream": stream,
    });
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/chat/completions")
                .header("authorization", format!("Bearer {TOKEN}"))
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, headers, String::from_utf8(bytes.to_vec()).unwrap())
}

fn assert_no_prompt_leak(buf: &Buf) {
    for line in buf.lines() {
        assert!(
            !line.contains("ZEBRA-7731"),
            "prompt text leaked into the log: {line}"
        );
    }
}

fn assert_request_id_consistent(headers: &HeaderMap, body: &str, access_line: &str) {
    let header_id = headers["x-request-id"].to_str().unwrap();
    assert!(header_id.starts_with("chatcmpl-"), "{header_id}");
    assert!(
        access_line.contains(&format!("request_id={header_id}")),
        "access line does not carry the response's request_id: {access_line}"
    );
    if let Ok(v) = serde_json::from_str::<Value>(body) {
        if let Some(id) = v.get("id") {
            assert_eq!(id.as_str(), Some(header_id), "envelope id vs x-request-id");
        }
    }
}

#[tokio::test]
async fn a_clean_streamed_stop_logs_exactly_one_access_line() {
    let tmp = tempfile::tempdir().unwrap();
    let base = mock(vec![text_reply("OK")], false).await;
    let (buf, _guard) = capture();

    let (status, headers, body) = post(app(&base, &tmp, 300), true).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.trim_end().ends_with("data: [DONE]"), "{body}");

    let access = buf.access_lines();
    assert_eq!(
        access.len(),
        1,
        "expected exactly one access line, got {}: {:?}",
        access.len(),
        access
    );
    let line = &access[0];
    assert!(line.contains("status=200"), "{line}");
    assert!(line.contains("finish_reason=stop"), "{line}");
    assert!(line.contains("code=null"), "{line}");
    assert!(line.contains("disconnect=false"), "{line}");
    assert!(line.contains("tools_out=-"), "{line}");
    assert!(line.contains("spawn_ms=null"), "{line}");
    assert!(line.contains("key=e2e"), "{line}");
    assert!(line.contains("model=claude/sonnet"), "{line}");
    assert!(line.contains("wire_model=claude-sonnet-4-5"), "{line}");
    assert_request_id_consistent(&headers, &body, line);
    assert!(buf.warn_lines().is_empty(), "{:?}", buf.warn_lines());
    assert_no_prompt_leak(&buf);
}

#[tokio::test]
async fn a_non_stream_stop_logs_exactly_one_access_line() {
    let tmp = tempfile::tempdir().unwrap();
    let base = mock(vec![text_reply("OK")], false).await;

    let (buf, _guard) = capture();

    let (status, headers, body) = post(app(&base, &tmp, 300), false).await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let access = buf.access_lines();
    assert_eq!(access.len(), 1, "{:?}", access);
    let line = &access[0];
    assert!(line.contains("status=200"), "{line}");
    assert!(line.contains("finish_reason=stop"), "{line}");
    assert!(line.contains("prompt_tokens=7"), "{line}");
    assert!(line.contains("completion_tokens=2"), "{line}");
    assert!(line.contains("code=null"), "{line}");
    assert_request_id_consistent(&headers, &body, line);
    assert!(buf.warn_lines().is_empty(), "{:?}", buf.warn_lines());
    assert_no_prompt_leak(&buf);
}

#[tokio::test]
async fn a_mid_stream_error_logs_one_access_line_and_a_warn() {
    let tmp = tempfile::tempdir().unwrap();
    let base = mock(vec![mid_stream_error_reply()], false).await;

    let (buf, _guard) = capture();

    let (status, headers, body) = post(app(&base, &tmp, 300), true).await;
    // Headers already committed 200 before the mid-stream error arrived (SPEC §19.4).
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(!body.contains("[DONE]"), "{body}");

    let access = buf.access_lines();
    assert_eq!(access.len(), 1, "{:?}", access);
    let line = &access[0];
    assert!(line.contains("status=200 stream_error"), "{line}");
    assert!(line.contains("finish_reason=null"), "{line}");
    assert!(line.contains("disconnect=false"), "{line}");
    assert!(!line.contains("code=null"), "{line}");
    assert_request_id_consistent(&headers, &body, line);

    let warns = buf.warn_lines();
    assert_eq!(warns.len(), 1, "{:?}", warns);
    assert!(warns[0].contains("model=claude/sonnet"), "{}", warns[0]);
    assert!(
        warns[0].contains("wire_model=claude-sonnet-4-5"),
        "{}",
        warns[0]
    );
    assert_no_prompt_leak(&buf);
}

/// The vendor side must stay genuinely open (never close its own connection) for this test to
/// mean anything: a mock that returns its whole SSE body as one complete HTTP response would
/// have already finished the turn (one way or another) before the client ever gets to drop
/// anything. So this streams `message_start` + one delta, then hangs — the adapter's `rx` stays
/// pending on real upstream silence, exactly like a client that never gets a real answer.
async fn hang_after_started() -> String {
    async fn handler() -> Response {
        let prefix = sse(&[
            (
                "message_start",
                json!({"type":"message_start","message":{"id":"m","model":"claude-sonnet-4-5","usage":{"input_tokens":7,"output_tokens":1}}}),
            ),
            (
                "content_block_start",
                json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
            ),
            (
                "content_block_delta",
                json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"partial"}}),
            ),
        ]);
        let body_stream = async_stream::stream! {
            yield Ok::<_, std::io::Error>(axum::body::Bytes::from(prefix));
            futures_util::future::pending::<()>().await;
        };
        let mut response = Response::builder()
            .status(StatusCode::OK)
            .body(Body::from_stream(body_stream))
            .unwrap();
        response
            .headers_mut()
            .insert("content-type", "text/event-stream".parse().unwrap());
        response
    }
    let app = Router::new().route("/v1/messages", axum::routing::post(handler));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr: SocketAddr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}")
}

#[tokio::test]
async fn a_client_that_drops_the_body_logs_one_disconnect_line() {
    let tmp = tempfile::tempdir().unwrap();
    let base = hang_after_started().await;

    let (buf, _guard) = capture();

    let body = json!({
        "model": "claude/sonnet",
        "messages": [{"role": "user", "content": PROMPT}],
        "stream": true,
    });
    let response = app(&base, &tmp, 300)
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/chat/completions")
                .header("authorization", format!("Bearer {TOKEN}"))
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let mut body = response.into_body();
    // Read one frame (the role chunk), then drop the body: a client that walked away.
    let _ = tokio::time::timeout(Duration::from_secs(2), body.frame())
        .await
        .expect("a frame within 2s");
    drop(body);
    // `RequestLogGuard::drop` runs when the stream's state is dropped; poll for it rather than
    // guess how long a loaded runner takes to get there (#257).
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while buf.access_lines().is_empty() && std::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    let access = buf.access_lines();
    assert_eq!(access.len(), 1, "{:?}", access);
    let line = &access[0];
    assert!(line.contains("disconnect=true"), "{line}");
    assert_no_prompt_leak(&buf);
}

#[tokio::test]
async fn a_start_deadline_503_logs_one_access_line() {
    let tmp = tempfile::tempdir().unwrap();
    let base = mock(vec![], true).await;

    let (buf, _guard) = capture();

    let (status, headers, body) = post(app(&base, &tmp, 1), false).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["error"]["code"], "vendor_down");
    let request_id = headers["x-request-id"].to_str().unwrap().to_string();
    assert_eq!(v["error"]["request_id"], request_id);

    let access = buf.access_lines();
    assert_eq!(access.len(), 1, "{:?}", access);
    let line = &access[0];
    assert!(line.contains(&format!("request_id={request_id}")), "{line}");
    assert!(line.contains("code=vendor_down"), "{line}");
    assert!(line.contains("ttfe_ms=null"), "{line}");
    // No post-`Started` `warn` for a request that never reached `Started` (deliverable 4).
    assert!(buf.warn_lines().is_empty(), "{:?}", buf.warn_lines());
    assert_no_prompt_leak(&buf);
}

#[tokio::test]
async fn a_bogus_key_401_carries_request_id_in_header_and_body() {
    let tmp = tempfile::tempdir().unwrap();
    let base = mock(vec![text_reply("OK")], false).await;
    let router = app(&base, &tmp, 300);
    let body = json!({"model":"claude/sonnet","messages":[{"role":"user","content":"hi"}]});
    let response = router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/chat/completions")
                .header("authorization", "Bearer ta_bogus")
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let header_id = response.headers()["x-request-id"]
        .to_str()
        .unwrap()
        .to_string();
    assert!(header_id.starts_with("chatcmpl-"), "{header_id}");
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let v: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(v["error"]["request_id"], header_id);
}

// ---- E2 (#180): `wire_params` on the access line ----

/// Loopback stand-in for Cursor's agent service: reads frame 0, then answers `OK` and ends.
async fn cursor_run(body: Body) -> Response {
    let mut stream = body.into_data_stream();
    let mut decoder = FrameDecoder::default();
    let mut got = false;
    while !got {
        let Some(chunk) = futures_util::StreamExt::next(&mut stream).await else {
            break;
        };
        got = !decoder.push(&chunk.unwrap()).unwrap().is_empty();
    }
    tokio::spawn(
        async move { while futures_util::StreamExt::next(&mut stream).await.is_some() {} },
    );
    let text = connect_frame(&field_ld(1, &field_ld(1, &field_str(1, "OK"))), 0).to_vec();
    let end = connect_frame(b"{}", FLAG_END).to_vec();
    let mut response = [text, end].concat().into_response();
    response
        .headers_mut()
        .insert("content-type", "application/connect+proto".parse().unwrap());
    response
}

fn cursor_row(id: &str, cli_model: &str) -> ModelEntry {
    ModelEntry {
        id: id.into(),
        backend: BackendId::Cursor,
        cli_model: cli_model.into(),
        context_window: 256_000,
        max_tokens: 32_000,
        thinking: true,
        cost: 0.0,
        name: id.into(),
        provider: None,
        transport: Transport::OauthDirect,
        vendor_model: Some(cli_model.into()),
        enabled: true,
        input: vec!["text".into()],
    }
}

async fn cursor_app(tmp: &tempfile::TempDir) -> Router {
    use base64::Engine;
    let router = Router::new().route(
        "/agent.v1.AgentService/Run",
        axum::routing::post(cursor_run),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr: SocketAddr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let claims = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(json!({ "sub": "user_e2e", "exp": 32_503_680_000u64 }).to_string());
    let jwt = format!("h.{claims}.s");
    let creds = tmp.path().join("auth.json");
    std::fs::write(
        &creds,
        json!({ "accessToken": jwt, "refreshToken": jwt }).to_string(),
    )
    .unwrap();
    let config = ConfigBundle {
        config_dir: tmp.path().to_path_buf(),
        router: RouterConfig {
            data_dir: tmp.path().join("data"),
            thin: ThinConfig {
                vendors: Vendors {
                    cursor: CursorCfg {
                        base_url: Some(format!("http://{addr}")),
                        credentials_path: Some(creds),
                        client_version: Some("cli-2026.09.15-test".into()),
                        cwd: Some("C:\\e2e".into()),
                        ..Default::default()
                    },
                    ..Default::default()
                },
                ..Default::default()
            },
            ..Default::default()
        },
        accounts: AccountsConfig {
            pools: Default::default(),
        },
        models: ModelsConfig {
            aliases: Default::default(),
            models: vec![
                cursor_row("subs/cursor/grok-4.7", "grok-4.7-high"),
                cursor_row("subs/cursor/auto", "auto"),
            ],
        },
        combos: CombosConfig {
            combos: Default::default(),
        },
        keys: KeysConfig {
            keys: vec![KeyEntry {
                id: "e2e".into(),
                hash: hash_token(TOKEN),
                scopes: vec![SCOPE_CHAT_WRITE.into()],
                ..Default::default()
            }],
        },
    };
    build_router(make_state_with_store(
        config,
        Arc::new(Store::open_in_memory().unwrap()),
    ))
}

async fn cursor_post(app: Router, model: &str, effort: Option<&str>) -> StatusCode {
    let mut body = json!({
        "model": model,
        "messages": [{"role": "user", "content": PROMPT}],
    });
    if let Some(e) = effort {
        body["reasoning_effort"] = json!(e);
    }
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/chat/completions")
                .header("authorization", format!("Bearer {TOKEN}"))
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let _ = response.into_body().collect().await.unwrap();
    status
}

#[tokio::test]
async fn a_cursor_request_at_low_logs_the_effort_it_sent() {
    let tmp = tempfile::tempdir().unwrap();
    let app = cursor_app(&tmp).await;
    let (buf, _guard) = capture();

    let status = cursor_post(app, "subs/cursor/grok-4.7", Some("low")).await;
    assert_eq!(status, StatusCode::OK);

    let access = buf.access_lines();
    assert_eq!(access.len(), 1, "{access:?}");
    let line = &access[0];
    // The catalogue id stays; the params show the level the row's own `-high` suffix lost to.
    assert!(line.contains("wire_model=grok-4.7-high"), "{line}");
    assert!(
        line.contains("wire_params=context=256k,reasoning_effort=low,fast=false"),
        "{line}"
    );
    assert_no_prompt_leak(&buf);
}

#[tokio::test]
async fn a_cursor_row_without_an_effort_suffix_logs_no_effort_key() {
    let tmp = tempfile::tempdir().unwrap();
    let app = cursor_app(&tmp).await;
    let (buf, _guard) = capture();

    let status = cursor_post(app, "subs/cursor/auto", Some("low")).await;
    assert_eq!(status, StatusCode::OK);

    let access = buf.access_lines();
    assert_eq!(access.len(), 1, "{access:?}");
    let line = &access[0];
    assert!(line.contains("wire_params=fast=false"), "{line}");
    assert!(!line.contains("effort"), "{line}");
}

#[tokio::test]
async fn a_claude_request_with_no_thinking_logs_a_dash() {
    let tmp = tempfile::tempdir().unwrap();
    let base = mock(vec![text_reply("OK")], false).await;
    let (buf, _guard) = capture();

    let (status, _, body) = post(app(&base, &tmp, 300), false).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let access = buf.access_lines();
    assert_eq!(access.len(), 1, "{access:?}");
    assert!(access[0].contains("wire_params=-"), "{}", access[0]);
}
