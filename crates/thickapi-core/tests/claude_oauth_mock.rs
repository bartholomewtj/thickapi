#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
//! `ClaudeOauth::run` against a local stand-in for api.anthropic.com (SPEC §19.8).
//!
//! Covers the wire contract the live vendor enforces: headers, streaming, the tool-name
//! prefix retry, block detection (400 identity / 401 / first-request 429) and ordinary errors.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::Router;
use serde_json::{json, Value};
use thickapi_core::config::models::{ModelEntry, Transport};
use thickapi_core::config::router::ClaudeThinkingBudgets;
use thickapi_core::thin::oauth::claude_oauth::{ClaudeOauth, OauthFailure};
use thickapi_core::thin::oauth::creds::Token;
use thickapi_core::thin::oauth::state::VendorState;
use thickapi_core::thin::oauth::vendor::StreamBudgets;
use thickapi_core::thin::openai::{ChatRequest, FinishReason};
use thickapi_core::thin::{ThinEvent, ThinRequest};
use thickapi_core::types::BackendId;

#[derive(Clone)]
struct Script {
    /// One canned reply per request, consumed in order.
    replies: Arc<Mutex<Vec<Reply>>>,
    seen: Arc<Mutex<Vec<(HeaderMap, Value)>>>,
}

#[derive(Clone)]
enum Reply {
    Sse(Vec<(&'static str, Value)>),
    /// These events, then `event: ping` every 20 ms and never an end.
    SseThenPings(Vec<(&'static str, Value)>),
    Error(u16, Value, Vec<(&'static str, &'static str)>),
}

async fn messages(State(s): State<Script>, headers: HeaderMap, body: String) -> Response {
    let body: Value = serde_json::from_str(&body).unwrap();
    s.seen.lock().unwrap().push((headers, body));
    let reply = {
        let mut replies = s.replies.lock().unwrap();
        if replies.is_empty() {
            Reply::Error(
                500,
                json!({"error":{"type":"test","message":"script exhausted"}}),
                vec![],
            )
        } else {
            replies.remove(0)
        }
    };
    match reply {
        Reply::SseThenPings(events) => {
            let mut head = String::new();
            for (name, data) in events {
                head.push_str(&format!("event: {name}\ndata: {data}\n\n"));
            }
            let body = async_stream::stream! {
                yield Ok::<_, std::io::Error>(head);
                loop {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                    yield Ok("event: ping\ndata: {\"type\":\"ping\"}\n\n".to_string());
                }
            };
            Response::builder()
                .status(200)
                .header("content-type", "text/event-stream")
                .body(Body::from_stream(body))
                .unwrap()
        }
        Reply::Sse(events) => {
            let mut text = String::new();
            for (name, data) in events {
                text.push_str(&format!("event: {name}\ndata: {data}\n\n"));
            }
            Response::builder()
                .status(200)
                .header("content-type", "text/event-stream")
                .header("anthropic-ratelimit-unified-status", "allowed_warning")
                .header("anthropic-ratelimit-unified-5h-utilization", "0.42")
                .body(Body::from(text))
                .unwrap()
        }
        Reply::Error(status, body, extra) => {
            let mut r = (StatusCode::from_u16(status).unwrap(), body.to_string()).into_response();
            for (k, v) in extra {
                r.headers_mut().insert(k, v.parse().unwrap());
            }
            r
        }
    }
}

async fn serve(replies: Vec<Reply>) -> (String, Script) {
    let script = Script {
        replies: Arc::new(Mutex::new(replies)),
        seen: Arc::new(Mutex::new(Vec::new())),
    };
    let app = Router::new()
        .route("/v1/messages", post(messages))
        .with_state(script.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr: SocketAddr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}"), script)
}

fn entry() -> ModelEntry {
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

fn request(body: Value) -> ThinRequest {
    ThinRequest {
        request: serde_json::from_value::<ChatRequest>(body).unwrap(),
        entry: entry(),
        effort: None,
        request_id: "req_test".into(),
        path: "oauth",
        fallback: None,
        cursor_cwd: None,
    }
}

fn token() -> Token {
    Token {
        access_token: "sk-ant-oat01-testtoken".into(),
        expires_at: Some(u64::MAX / 2),
        source: std::path::PathBuf::from("test"),
        note: None,
    }
}

fn oauth(base: &str) -> ClaudeOauth {
    ClaudeOauth::new(
        ClaudeThinkingBudgets::default(),
        Some("2.0.0".into()),
        Some(base.to_string()),
    )
}

async fn drive(
    oauth: &ClaudeOauth,
    req: &ThinRequest,
    state: &VendorState,
) -> (Result<(), OauthFailure>, Vec<ThinEvent>) {
    let (tx, mut rx) = tokio::sync::mpsc::channel(64);
    let result = oauth.run(req, &tx, &token(), state).await;
    drop(tx);
    let mut events = Vec::new();
    while let Some(ev) = rx.recv().await {
        events.push(ev);
    }
    (result, events)
}

fn text_stream(text: &str) -> Reply {
    Reply::Sse(vec![
        (
            "message_start",
            json!({"type":"message_start","message":{"id":"msg_1","model":"claude-sonnet-4-5","usage":{"input_tokens":12,"output_tokens":1,"cache_read_input_tokens":4}}}),
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
            json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":3}}),
        ),
        ("message_stop", json!({"type":"message_stop"})),
    ])
}

#[tokio::test]
async fn streams_text_with_headers_and_advisory() {
    let (base, script) = serve(vec![text_stream("hello")]).await;
    let state = VendorState::new();
    let req = request(json!({"model":"claude/sonnet","messages":[{"role":"user","content":"hi"}]}));
    let (result, events) = drive(&oauth(&base), &req, &state).await;
    result.expect("stream ok");

    let (headers, body) = script.seen.lock().unwrap()[0].clone();
    assert_eq!(headers["authorization"], "Bearer sk-ant-oat01-testtoken");
    assert_eq!(headers["anthropic-version"], "2023-06-01");
    assert_eq!(headers["x-app"], "cli");
    assert!(headers["user-agent"]
        .to_str()
        .unwrap()
        .starts_with("claude-cli/2.0.0"));
    let betas = headers["anthropic-beta"].to_str().unwrap();
    assert!(
        betas.contains("oauth-2025-04-20") && betas.contains("claude-code-20250219"),
        "{betas}"
    );
    assert_eq!(body["model"], "claude-sonnet-4-5");
    assert_eq!(body["stream"], true);
    assert!(body["system"][0]["text"]
        .as_str()
        .unwrap()
        .starts_with("You are Claude Code"));

    assert!(matches!(events[0], ThinEvent::Started { .. }));
    assert!(events.iter().any(|e| matches!(e, ThinEvent::RateLimitAdvisory { utilization, .. } if (*utilization - 0.42).abs() < 1e-9)));
    assert!(events
        .iter()
        .any(|e| matches!(e, ThinEvent::TextDelta(t) if t == "hello")));
    let usage = events.iter().find_map(|e| match e {
        ThinEvent::Usage(u) => Some(u.clone()),
        _ => None,
    });
    let usage = usage.expect("usage");
    assert_eq!(usage.prompt_tokens, 12 + 4);
    assert_eq!(usage.completion_tokens, 3);
    assert!(matches!(
        events.last(),
        Some(ThinEvent::Finish(FinishReason::Stop))
    ));
    assert_eq!(state.successes(), 1);
    assert!(!state.is_blocked());
}

#[tokio::test]
async fn tool_name_rejection_retries_with_prefix_and_unprefixes_calls() {
    let tool_use = Reply::Sse(vec![
        (
            "message_start",
            json!({"type":"message_start","message":{"id":"msg_2","usage":{"input_tokens":5,"output_tokens":1}}}),
        ),
        (
            "content_block_start",
            json!({"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_1","name":"mcp__pi__read","input":{}}}),
        ),
        (
            "content_block_delta",
            json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"path\":\"a.txt\"}"}}),
        ),
        (
            "content_block_stop",
            json!({"type":"content_block_stop","index":0}),
        ),
        (
            "message_delta",
            json!({"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":9}}),
        ),
        ("message_stop", json!({"type":"message_stop"})),
    ]);
    let (base, script) = serve(vec![
        Reply::Error(400, json!({"type":"error","error":{"type":"invalid_request_error","message":"tools.0.name: String should match pattern"}}), vec![]),
        tool_use,
    ])
    .await;
    let state = VendorState::new();
    let req = request(json!({
        "model":"claude/sonnet",
        "messages":[{"role":"user","content":"read a.txt"}],
        "tools":[{"type":"function","function":{"name":"read","description":"read a file","parameters":{"type":"object","properties":{"path":{"type":"string"}}}}}]
    }));
    let (result, events) = drive(&oauth(&base), &req, &state).await;
    result.expect("retry ok");

    let seen = script.seen.lock().unwrap();
    assert_eq!(seen.len(), 2);
    assert_eq!(seen[0].1["tools"][0]["name"], "read");
    assert_eq!(seen[1].1["tools"][0]["name"], "mcp__pi__read");
    drop(seen);

    let call = events.iter().find_map(|e| match e {
        ThinEvent::ToolCall {
            name, arguments, ..
        } => Some((name.clone(), arguments.clone())),
        _ => None,
    });
    assert_eq!(call, Some(("read".into(), "{\"path\":\"a.txt\"}".into())));
    assert!(matches!(
        events.last(),
        Some(ThinEvent::Finish(FinishReason::ToolCalls))
    ));
}

#[tokio::test]
async fn identity_400_is_a_block_and_nothing_streams() {
    let (base, _) = serve(vec![Reply::Error(
        400,
        json!({"type":"error","error":{"type":"invalid_request_error","message":"This credential is only authorized for use with Claude Code and cannot be used for other API requests."}}),
        vec![],
    )])
    .await;
    let state = VendorState::new();
    let req = request(json!({"model":"claude/sonnet","messages":[{"role":"user","content":"hi"}]}));
    let (result, events) = drive(&oauth(&base), &req, &state).await;
    match result {
        Err(OauthFailure::Blocked(reason)) => assert!(reason.contains("400"), "{reason}"),
        other => panic!("expected Blocked, got {:?}", other.map(|_| ())),
    }
    assert!(
        events.is_empty(),
        "block must be detected before Started: {events:?}"
    );
    assert_eq!(state.last_status(), Some(400));
}

#[tokio::test]
async fn first_request_429_is_rate_limit_not_a_block() {
    let (base, _) = serve(vec![
        Reply::Error(
            429,
            json!({"type":"error","error":{"type":"rate_limit_error","message":"slow down"}}),
            vec![("retry-after", "30")],
        ),
        text_stream("ok"),
        Reply::Error(
            429,
            json!({"type":"error","error":{"type":"rate_limit_error","message":"slow down"}}),
            vec![("retry-after", "30")],
        ),
    ])
    .await;
    let req = request(json!({"model":"claude/sonnet","messages":[{"role":"user","content":"hi"}]}));

    let fresh = VendorState::new();
    let (result, _) = drive(&oauth(&base), &req, &fresh).await;
    match result {
        Err(OauthFailure::Error(e)) => {
            assert_eq!(
                (e.status, e.code, e.retry_after_seconds()),
                (429, "rate_limit_error", Some(30)),
                "first 429 without ToS wording is not a 6h block: {e:?}"
            );
        }
        other => panic!("expected rate_limit_error, got {:?}", other.map(|_| ())),
    }

    let warmed = VendorState::new();
    drive(&oauth(&base), &req, &warmed)
        .await
        .0
        .expect("warm-up ok");
    let (result, _) = drive(&oauth(&base), &req, &warmed).await;
    match result {
        Err(OauthFailure::Error(e)) => {
            assert_eq!(
                (e.status, e.code, e.retry_after_seconds()),
                (429, "rate_limit_error", Some(30)),
                "{e:?}"
            );
        }
        other => panic!("expected RateLimited, got {:?}", other.map(|_| ())),
    }
}

#[tokio::test]
async fn probe_sends_claude_code_shape_and_classifies_like_a_request() {
    let (base, script) = serve(vec![
        text_stream("OK"),
        Reply::Error(
            400,
            json!({"type":"error","error":{"type":"invalid_request_error","message":"Third-party apps now draw from your extra usage."}}),
            vec![],
        ),
        Reply::Error(529, json!({"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}), vec![]),
    ])
    .await;
    let state = VendorState::new();
    let oauth = oauth(&base);

    oauth
        .probe(&token(), "claude-sonnet-4-6", &state)
        .await
        .expect("200 → ok");
    assert_eq!(state.successes(), 1);
    assert_eq!(state.last_status(), Some(200));
    let (headers, body) = script.seen.lock().unwrap()[0].clone();
    assert_eq!(headers["authorization"], "Bearer sk-ant-oat01-testtoken");
    assert_eq!(headers["x-app"], "cli");
    assert!(headers["user-agent"]
        .to_str()
        .unwrap()
        .starts_with("claude-cli/2.0.0"));
    let betas = headers["anthropic-beta"].to_str().unwrap();
    assert!(
        betas.contains("oauth-2025-04-20") && betas.contains("interleaved-thinking"),
        "{betas}"
    );
    assert_eq!(body["model"], "claude-sonnet-4-6");
    assert_eq!(body["thinking"]["type"], "enabled");
    assert_eq!(body["thinking"]["budget_tokens"], 4_000);
    assert!(
        body["max_tokens"].as_u64().unwrap() >= 4_000 + 1_024,
        "max_tokens={} needs Low + THINKING_HEADROOM",
        body["max_tokens"]
    );
    assert_eq!(body["messages"][0]["content"][0]["text"], "OK");
    assert_eq!(body["tools"][0]["name"], "read");
    assert_eq!(
        body["system"][0]["text"].as_str().unwrap(),
        thickapi_core::thin::oauth::claude_oauth::IDENTITY
    );

    match oauth.probe(&token(), "claude-sonnet-4-6", &state).await {
        Err(OauthFailure::Blocked(reason)) => assert!(reason.contains("third-party"), "{reason}"),
        other => panic!("expected Blocked, got {:?}", other.map(|_| ())),
    }
    match oauth.probe(&token(), "claude-sonnet-4-6", &state).await {
        Err(OauthFailure::Error(e)) => assert_eq!((e.status, e.code), (503, "vendor_down")),
        other => panic!("expected vendor_down, got {:?}", other.map(|_| ())),
    }
    assert!(
        !state.is_blocked(),
        "probe itself does not mark; the adapter does"
    );
}

#[tokio::test]
async fn unauthorized_is_block_and_overloaded_is_vendor_down() {
    let (base, _) = serve(vec![
        Reply::Error(401, json!({"type":"error","error":{"type":"authentication_error","message":"invalid bearer"}}), vec![]),
        Reply::Error(529, json!({"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}), vec![]),
    ])
    .await;
    let state = VendorState::new();
    let req = request(json!({"model":"claude/sonnet","messages":[{"role":"user","content":"hi"}]}));
    let (r1, _) = drive(&oauth(&base), &req, &state).await;
    assert!(matches!(r1, Err(OauthFailure::Blocked(_))));
    let (r2, _) = drive(&oauth(&base), &req, &state).await;
    match r2 {
        Err(OauthFailure::Error(e)) => {
            assert_eq!((e.status, e.code), (503, "vendor_down"), "{e:?}")
        }
        other => panic!("expected vendor_down, got {:?}", other.map(|_| ())),
    }
}

fn impatient(base: &str, first_event: Duration) -> ClaudeOauth {
    oauth(base).with_stream_budgets(StreamBudgets {
        first_event,
        idle: Duration::from_millis(150),
    })
}

fn ask() -> ThinRequest {
    request(json!({"model": "claude/sonnet", "messages": [{"role": "user", "content": "hi"}]}))
}

/// Pings are bytes, not events: a stream of nothing else must still time out, and before
/// `message_start` that has to come back as an `Err` so the route can answer with a status.
#[tokio::test]
async fn a_stream_of_only_pings_is_a_503_with_nothing_sent() {
    let (base, _) = serve(vec![Reply::SseThenPings(vec![])]).await;
    let state = VendorState::new();
    let (adapter, req) = (impatient(&base, Duration::from_millis(150)), ask());
    let run = drive(&adapter, &req, &state);
    let (result, events) = tokio::time::timeout(Duration::from_secs(5), run)
        .await
        .expect("the adapter must give up on its own");
    match result {
        Err(OauthFailure::Error(e)) => {
            assert_eq!((e.status, e.code), (503, "vendor_down"), "{}", e.message);
            assert!(e.message.contains("sent nothing"), "{}", e.message);
        }
        other => panic!("{other:?}"),
    }
    assert!(events.is_empty(), "{events:?}");
}

#[tokio::test]
async fn going_quiet_mid_answer_ends_the_stream_with_an_error_and_no_finish() {
    let head = vec![
        (
            "message_start",
            json!({"type":"message_start","message":{"model":"claude-sonnet-4-5","usage":{"input_tokens":3}}}),
        ),
        (
            "content_block_start",
            json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
        ),
        (
            "content_block_delta",
            json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"half"}}),
        ),
    ];
    let (base, _) = serve(vec![Reply::SseThenPings(head)]).await;
    let state = VendorState::new();
    // A roomy first-event budget: a loaded box must not turn this into "sent nothing".
    let (adapter, req) = (impatient(&base, Duration::from_secs(5)), ask());
    let run = drive(&adapter, &req, &state);
    let (result, events) = tokio::time::timeout(Duration::from_secs(5), run)
        .await
        .expect("the adapter must give up on its own");
    assert!(
        result.is_ok(),
        "after Started a failure is an event: {result:?}"
    );
    assert!(matches!(events.first(), Some(ThinEvent::Started { .. })));
    assert!(events
        .iter()
        .any(|e| matches!(e, ThinEvent::TextDelta(t) if t == "half")));
    match events.last() {
        Some(ThinEvent::Error(e)) => assert!(e.message.contains("mid-answer"), "{}", e.message),
        other => panic!("{other:?}"),
    }
    assert!(!events.iter().any(|e| matches!(e, ThinEvent::Finish(_))));
}

/// Nothing may have been streamed when a turn fails before `message_start`, whatever the
/// failure is: that is what lets the route still answer with a real status.
#[tokio::test]
async fn failures_before_message_start_are_returned_not_streamed() {
    let error_first = Reply::Sse(vec![(
        "error",
        json!({"type":"error","error":{"type":"overloaded_error","message":"busy"}}),
    )]);
    let stop_without_start = Reply::Sse(vec![("message_stop", json!({"type":"message_stop"}))]);
    for (reply, status, code) in [
        (error_first, 503, "vendor_down"),
        (stop_without_start, 502, "vendor_drift"),
    ] {
        let (base, _) = serve(vec![reply]).await;
        let state = VendorState::new();
        let (result, events) = drive(&oauth(&base), &ask(), &state).await;
        match result {
            Err(OauthFailure::Error(e)) => assert_eq!((e.status, e.code), (status, code)),
            other => panic!("{code}: {other:?}"),
        }
        assert!(events.is_empty(), "{code}: {events:?}");
    }
}

/// A server that accepts and never answers, or answers with `head` and then stalls. Every
/// connection is held open for the life of the test.
async fn stalling(head: &'static str) -> String {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr: SocketAddr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let mut held = Vec::new();
        while let Ok((mut sock, _)) = listener.accept().await {
            if !head.is_empty() {
                let mut buf = vec![0u8; 64 * 1024];
                let _ = tokio::time::timeout(Duration::from_millis(200), sock.read(&mut buf)).await;
                let _ = sock.write_all(head.as_bytes()).await;
            }
            held.push(sock);
        }
    });
    format!("http://{addr}")
}

/// #125: the probe and `list_models` are bounded by `thin.reply_timeout_seconds`; a probe that
/// hangs must not hold the probe loop (and its permit) for good.
#[tokio::test]
async fn a_silent_server_times_out_the_probe_and_list_models() {
    let base = stalling("").await;
    let oauth = oauth(&base).with_reply_timeout(Duration::from_secs(1));
    let state = VendorState::new();
    let started = std::time::Instant::now();
    match oauth.probe(&token(), "claude-sonnet-4-6", &state).await {
        Err(OauthFailure::Error(e)) => {
            assert_eq!(e.code, "vendor_down");
            assert!(e.message.contains("timed out"), "{}", e.message);
        }
        other => panic!("expected a timeout, got {:?}", other.map(|_| ())),
    }
    let err = oauth.list_models(&token()).await.unwrap_err();
    assert_eq!(err.code, "vendor_down");
    assert!(err.message.contains("timed out"), "{}", err.message);
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "{:?}",
        started.elapsed()
    );
}

/// #125: a rejection whose body never finishes is classified on its status within the bound.
#[tokio::test]
async fn a_stalled_error_body_is_classified_on_the_status_alone() {
    let base = stalling(
        "HTTP/1.1 529 Service Unavailable\r\ncontent-type: application/json\r\ncontent-length: 4096\r\n\r\n{\"type\":",
    )
    .await;
    let oauth = oauth(&base).with_reply_timeout(Duration::from_secs(1));
    let state = VendorState::new();
    let started = std::time::Instant::now();
    let (result, events) = tokio::time::timeout(
        Duration::from_secs(10),
        drive(
            &oauth,
            &request(json!({"model":"claude/sonnet","messages":[{"role":"user","content":"hi"}]})),
            &state,
        ),
    )
    .await
    .expect("the rejected body must not hang the request");
    match result {
        Err(OauthFailure::Error(e)) => assert_eq!(e.status, 503, "{e:?}"),
        other => panic!("expected an error, got {:?}", other.map(|_| ())),
    }
    assert!(events.is_empty(), "{events:?}");
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "{:?}",
        started.elapsed()
    );
}
