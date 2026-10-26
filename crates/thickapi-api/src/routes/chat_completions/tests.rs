use std::sync::Arc;
use std::time::Duration;

use http_body_util::BodyExt;
use serde_json::{json, Value};
use thickapi_core::config::keys::{
    hash_token, KeyEntry, KeysConfig, SCOPE_CHAT_WRITE, SCOPE_TURN_WRITE,
};
use thickapi_core::config::models::{ModelEntry, Transport};
use thickapi_core::types::BackendId;
use tower::ServiceExt;

use super::access_log;
use super::collect::collect_response;
use super::stream::{stream_response, StreamBounds, StreamGuards, KEEPALIVE};
use super::validate::validate;
use super::*;
use crate::{build_router, AppState};
use axum::http::{HeaderMap, StatusCode};
use thickapi_core::thin::openai::{ChatRequest, FinishReason, Usage};
use thickapi_core::thin::{ThinError, ThinEvent};
use thickapi_core::thinking::normalise_effort;
use tokio::sync::mpsc;

fn row(id: &str, transport: Transport, image: bool) -> ModelEntry {
    ModelEntry {
        id: id.into(),
        backend: BackendId::ClaudeCode,
        cli_model: "sonnet".into(),
        context_window: 200_000,
        max_tokens: 8192,
        thinking: true,
        cost: 0.0,
        name: id.into(),
        provider: None,
        transport,
        vendor_model: Some("claude-sonnet-4-5".into()),
        enabled: true,
        input: if image {
            vec!["text".into(), "image".into()]
        } else {
            vec!["text".into()]
        },
    }
}

fn state(token: &str, scopes: &[&str], models: Vec<ModelEntry>) -> AppState {
    let config = thickapi_core::ConfigBundle {
        config_dir: std::env::temp_dir().join("thickapi-chat-test-cfg"),
        router: thickapi_core::config::RouterConfig {
            data_dir: std::env::temp_dir().join("thickapi-chat-test"),
            ..Default::default()
        },
        accounts: thickapi_core::config::AccountsConfig {
            pools: Default::default(),
        },
        models: thickapi_core::config::ModelsConfig {
            aliases: Default::default(),
            models,
        },
        combos: thickapi_core::config::CombosConfig {
            combos: Default::default(),
        },
        keys: KeysConfig {
            keys: vec![KeyEntry {
                id: "t".into(),
                hash: hash_token(token),
                scopes: scopes.iter().map(|s| s.to_string()).collect(),
                ..Default::default()
            }],
        },
    };
    crate::server::make_state_with_store(
        config,
        Arc::new(thickapi_core::Store::open_in_memory().expect("store")),
    )
}

async fn post(app: axum::Router, token: &str, body: Value) -> (StatusCode, HeaderMap, Value) {
    let response = app
        .oneshot(
            axum::http::Request::builder()
                .method("POST")
                .uri("/v1/chat/completions")
                .header("Authorization", format!("Bearer {}", token))
                .header("content-type", "application/json")
                .body(axum::body::Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .expect("response");
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes();
    (
        status,
        headers,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

fn basic(model: &str) -> Value {
    json!({"model": model, "messages": [{"role": "user", "content": "hi"}]})
}

#[tokio::test]
async fn turn_write_key_gets_403_on_thin_route() {
    let app = build_router(state("ta_turn", &[SCOPE_TURN_WRITE], vec![]));
    let (status, _, body) = post(app, "ta_turn", basic("subs/claude/sonnet")).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(body["error"]["type"], "permission_error");
}

#[tokio::test]
async fn unknown_disabled_and_thick_only_rows_are_404() {
    let mut thick = row("claude/thick", Transport::CliTurn, false);
    thick.enabled = true;
    let mut off = row("claude/off", Transport::OauthDirect, false);
    off.enabled = false;
    let app = build_router(state("ta_chat", &[SCOPE_CHAT_WRITE], vec![thick, off]));
    for m in ["subs/nope/x", "claude/thick", "subs/claude/off"] {
        let (status, _, body) = post(app.clone(), "ta_chat", basic(m)).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{m}");
        assert_eq!(body["error"]["code"], "model_not_found");
        assert_eq!(body["error"]["hint"], "thickapi models list");
    }
}

#[tokio::test]
async fn dump_tools_writes_first_request_then_clears() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("tools.json");
    let st = state(
        "ta_chat",
        &[SCOPE_CHAT_WRITE],
        vec![row("claude/sonnet", Transport::OauthDirect, false)],
    );
    *st.dump_tools.lock().unwrap() = Some(path.clone());
    let app = build_router(st);
    let tools = json!([{
        "type": "function",
        "function": {
            "name": "read",
            "parameters": {"type": "object", "properties": {"p": {"type": "string"}}}
        }
    }]);
    let _ = post(
        app.clone(),
        "ta_chat",
        json!({"model":"claude/sonnet","messages":[{"role":"user","content":"hi"}],"tools": tools}),
    )
    .await;
    let dumped: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(dumped[0]["function"]["name"], "read");
    std::fs::write(&path, b"stale").unwrap();
    let _ = post(
        app,
        "ta_chat",
        json!({"model":"claude/sonnet","messages":[{"role":"user","content":"hi"}],"tools":[{"type":"function","function":{"name":"other"}}]}),
    )
    .await;
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "stale");
}

#[tokio::test]
async fn malformed_body_is_400() {
    let app = build_router(state("ta_chat", &[SCOPE_CHAT_WRITE], vec![]));
    let response = app
        .oneshot(
            axum::http::Request::builder()
                .method("POST")
                .uri("/v1/chat/completions")
                .header("Authorization", "Bearer ta_chat")
                .body(axum::body::Body::from("{not json"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn request_contract_rejections_name_the_param() {
    let app = build_router(state(
        "ta_chat",
        &[SCOPE_CHAT_WRITE],
        vec![
            row("claude/sonnet", Transport::OauthDirect, false),
            row("claude/bridge", Transport::CliBridge, false),
        ],
    ));
    let cases: Vec<(Value, &str)> = vec![
        (
            json!({"model":"claude/sonnet","messages":[{"role":"user","content":"x"}],"n":2}),
            "n",
        ),
        (
            json!({"model":"claude/sonnet","messages":[{"role":"user","content":"x"}],"logprobs":true}),
            "logprobs",
        ),
        (
            json!({"model":"claude/sonnet","messages":[{"role":"user","content":"x"}],"response_format":{"type":"json_object"}}),
            "response_format",
        ),
        (
            json!({"model":"claude/sonnet","messages":[{"role":"user","content":[{"type":"image_url","image_url":{"url":"data:image/png;base64,AA=="}}]}]}),
            "messages[0].content",
        ),
        (
            json!({"model":"claude/sonnet","messages":[{"role":"robot","content":"x"}]}),
            "messages[0].role",
        ),
        (
            json!({"model":"claude/sonnet","messages":[{"role":"user","content":"x"}],"reasoning_effort":"turbo"}),
            "reasoning_effort",
        ),
        (
            json!({"model":"claude/bridge","messages":[{"role":"user","content":"x"}],"tool_choice":"sometimes"}),
            "tool_choice",
        ),
        (
            json!({"model":"claude/sonnet","messages":[{"role":"user","content":"x"}],"tool_choice":1}),
            "tool_choice",
        ),
        (json!({"model":"claude/sonnet","messages":[]}), "messages"),
    ];
    for (body, param) in cases {
        let (status, _, resp) = post(app.clone(), "ta_chat", body.clone()).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert_eq!(resp["error"]["type"], "invalid_request_error");
        assert_eq!(resp["error"]["param"], param, "{body}");
    }
}

#[tokio::test]
async fn image_allowed_when_row_declares_it() {
    let entry = row("claude/vision", Transport::OauthDirect, true);
    let req: ChatRequest = serde_json::from_value(json!({"model":"claude/vision","messages":[{"role":"user","content":[{"type":"image_url","image_url":{"url":"data:image/png;base64,AA=="}}]}]})).unwrap();
    assert!(validate(&req, &entry).is_ok());
}

#[tokio::test]
async fn bridge_rows_report_ignored_sampling_fields() {
    let entry = row("claude/bridge", Transport::CliBridge, false);
    let req: ChatRequest = serde_json::from_value(json!({"model":"claude/bridge","messages":[{"role":"user","content":"x"}],"temperature":0.2,"top_p":0.9})).unwrap();
    assert_eq!(
        validate(&req, &entry).unwrap(),
        vec!["temperature", "top_p"]
    );
    let direct = row("claude/sonnet", Transport::OauthDirect, false);
    assert!(validate(&req, &direct).unwrap().is_empty());
}

#[test]
fn effort_levels() {
    use thickapi_core::thinking::Effort;
    assert_eq!(normalise_effort(None).unwrap(), None);
    assert_eq!(normalise_effort(Some("none")).unwrap(), Some(Effort::Off));
    assert_eq!(normalise_effort(Some("High")).unwrap(), Some(Effort::High));
    assert!(normalise_effort(Some("ultra")).is_err());
}

// ---- rendering with a scripted adapter (no vendor) ----

fn scripted(
    events: Vec<ThinEvent>,
    stream: bool,
    include_usage: bool,
    parallel: bool,
) -> (Ctx, mpsc::Receiver<ThinEvent>) {
    let (tx, rx) = mpsc::channel(64);
    tokio::spawn(async move {
        for ev in events {
            let _ = tx.send(ev).await;
        }
    });
    let _ = stream;
    (
        Ctx {
            id: "chatcmpl-test".into(),
            created: 1,
            model: "claude-sonnet-4-5".into(),
            parallel,
            include_usage,
        },
        rx,
    )
}

fn two_calls() -> Vec<ThinEvent> {
    vec![
        ThinEvent::ReasoningDelta {
            text: "think".into(),
            signature: Some("sig".into()),
            details: None,
        },
        ThinEvent::TextDelta("Reading".into()),
        ThinEvent::ToolCall {
            id: "call_a".into(),
            name: "read".into(),
            arguments: "{\"path\":\"a\"}".into(),
        },
        ThinEvent::ToolCall {
            id: "call_b".into(),
            name: "read".into(),
            arguments: "{\"path\":\"b\"}".into(),
        },
        ThinEvent::Usage(Usage::from_parts(10, 5, 2)),
        ThinEvent::Finish(FinishReason::ToolCalls),
    ]
}

#[tokio::test]
async fn non_stream_collects_two_tool_calls_and_usage() {
    let (ctx, rx) = scripted(two_calls(), false, false, true);
    let response = collect_response(
        ctx,
        HeaderMap::new(),
        vec![],
        rx,
        None,
        access_log::test_guard(),
    )
    .await
    .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let v: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(v["object"], "chat.completion");
    let msg = &v["choices"][0]["message"];
    assert_eq!(msg["role"], "assistant");
    assert_eq!(msg["content"], "Reading");
    assert_eq!(msg["reasoning_content"], "think");
    assert_eq!(msg["reasoning_signature"], "sig");
    assert_eq!(msg["tool_calls"].as_array().unwrap().len(), 2);
    assert_eq!(msg["tool_calls"][1]["index"], 1);
    assert_eq!(
        msg["tool_calls"][1]["function"]["arguments"],
        "{\"path\":\"b\"}"
    );
    assert_eq!(v["choices"][0]["finish_reason"], "tool_calls");
    assert_eq!(v["usage"]["prompt_tokens"], 12);
    assert_eq!(v["usage"]["prompt_tokens_details"]["cached_tokens"], 2);
    assert_eq!(v["usage"]["total_tokens"], 17);
}

#[tokio::test]
async fn parallel_false_keeps_only_first_call() {
    let (ctx, rx) = scripted(two_calls(), false, false, false);
    let response = collect_response(
        ctx,
        HeaderMap::new(),
        vec![],
        rx,
        None,
        access_log::test_guard(),
    )
    .await
    .unwrap();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let v: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(
        v["choices"][0]["message"]["tool_calls"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
}

/// #121: an adapter channel that closes with no `Finish` is a truncated answer, not a
/// whole one with zero usage.
#[tokio::test]
async fn non_stream_without_finish_is_503_not_stop() {
    let events = vec![
        ThinEvent::TextDelta("half an ans".into()),
        ThinEvent::Usage(Usage::from_parts(10, 5, 0)),
    ];
    let (ctx, rx) = scripted(events, false, false, true);
    let err = collect_response(
        ctx,
        HeaderMap::new(),
        vec![],
        rx,
        None,
        access_log::test_guard(),
    )
    .await
    .expect_err("no Finish must not be a completion");
    assert_eq!(err.status, 503);
    assert_eq!(err.code, "vendor_down");
    let message = err.body()["error"]["message"].as_str().unwrap().to_string();
    assert!(message.ends_with(" [code=vendor_down]"), "{message}");
}

fn parse_sse(text: &str) -> Vec<Value> {
    text.split("\n\n")
        .filter_map(|f| f.strip_prefix("data: "))
        .filter(|d| *d != "[DONE]")
        .map(|d| serde_json::from_str(d).unwrap())
        .collect()
}

#[tokio::test]
async fn stream_renders_chunks_usage_and_done() {
    let (ctx, rx) = scripted(two_calls(), true, true, true);
    let response = stream_response(
        ctx,
        HeaderMap::new(),
        vec![],
        rx,
        StreamBounds {
            keepalive: None,
            idle: None,
        },
        StreamGuards {
            runner: AbortOnDrop::none(),
            inflight: None,
            log: access_log::test_guard(),
        },
    );
    assert_eq!(response.headers()["content-type"], "text/event-stream");
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let text = String::from_utf8(bytes.to_vec()).unwrap();
    assert!(text.trim_end().ends_with("data: [DONE]"), "{text}");
    let chunks = parse_sse(&text);
    assert!(chunks
        .iter()
        .all(|c| c["object"] == "chat.completion.chunk"));
    assert_eq!(chunks[0]["choices"][0]["delta"]["role"], "assistant");
    assert_eq!(
        chunks[1]["choices"][0]["delta"]["reasoning_content"],
        "think"
    );
    assert_eq!(
        chunks[1]["choices"][0]["delta"]["reasoning_signature"],
        "sig"
    );
    assert_eq!(chunks[2]["choices"][0]["delta"]["content"], "Reading");
    assert_eq!(
        chunks[3]["choices"][0]["delta"]["tool_calls"][0]["index"],
        0
    );
    let id = chunks[3]["choices"][0]["delta"]["tool_calls"][0]["id"]
        .as_str()
        .unwrap();
    assert!(id.starts_with("call_") && id.len() == 29, "{id}");
    assert_eq!(
        chunks[4]["choices"][0]["delta"]["tool_calls"][0]["index"],
        1
    );
    assert_eq!(chunks[5]["choices"][0]["finish_reason"], "tool_calls");
    let last = chunks.last().unwrap();
    assert!(last["choices"].as_array().unwrap().is_empty());
    assert_eq!(last["usage"]["completion_tokens"], 5);
}

/// `[DONE]` says "completed"; after a failure the error frame is the last word, and it
/// carries what the headers could no longer say.
#[tokio::test]
async fn a_mid_stream_error_ends_the_stream_without_done_and_carries_retry_hints() {
    let events = vec![
        ThinEvent::TextDelta("partial".into()),
        ThinEvent::Error(ThinError::rate_limited("grok", "slow down", 7, false)),
    ];
    let (ctx, rx) = scripted(events, true, false, true);
    let response = stream_response(
        ctx,
        HeaderMap::new(),
        vec![],
        rx,
        StreamBounds {
            keepalive: None,
            idle: None,
        },
        StreamGuards {
            runner: AbortOnDrop::none(),
            inflight: None,
            log: access_log::test_guard(),
        },
    );
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let text = String::from_utf8(bytes.to_vec()).unwrap();
    assert!(!text.contains("[DONE]"), "{text}");
    let chunks = parse_sse(&text);
    let error = &chunks.last().unwrap()["error"];
    assert_eq!(error["type"], "rate_limit_error");
    assert_eq!(error["retry_after"], 7);
    assert_eq!(error["should_retry"], true);
    let message = error["message"].as_str().unwrap();
    assert!(message.contains("slow down"), "{message}");
    assert!(message.contains("retry_after=7s"), "{message}");
    assert!(message.contains("should_retry=true"), "{message}");
    assert!(
        chunks
            .iter()
            .all(|c| c["choices"][0]["finish_reason"].is_null()),
        "no finish_reason may precede an error: {text}"
    );
}

fn every_thin_error() -> Vec<ThinError> {
    vec![
        ThinError::invalid_request("bad", None),
        ThinError::route_not_found("GET", "/nope"),
        ThinError::method_not_allowed("PUT", "/v1/chat/completions"),
        ThinError::body_too_large(32),
        ThinError::model_not_found("subs/x/y"),
        ThinError::vendor_login_required("grok", "grok 401: expired", "grok login"),
        ThinError::permission("grok 403: no").with_vendor("grok"),
        ThinError::rate_limited("grok", "slow down", 401, false),
        ThinError::rate_limited("claude", "out", 7200, true),
        ThinError::insufficient_quota("claude", "out"),
        ThinError::vendor_drift("cursor", "moved"),
        ThinError::vendor_down("cursor", "cursor unavailable: Server is shutting down"),
        ThinError::server("boom"),
        ThinError::not_implemented("later"),
    ]
}

/// #136: Pi records only `message`, so every thin error names its code at the end of it, on
/// the HTTP answer and on the mid-stream frame alike.
#[tokio::test]
async fn every_error_code_is_the_trailing_marker_on_http_and_stream() {
    for err in every_thin_error() {
        let marker = format!(" [code={}]", err.code);

        let response = thin_error_response(&err);
        assert_eq!(response.status().as_u16(), err.status);
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let v: Value = serde_json::from_slice(&bytes).unwrap();
        let message = v["error"]["message"].as_str().unwrap();
        assert!(message.ends_with(&marker), "HTTP {message}");
        assert_eq!(v["error"]["code"], err.code);

        let (ctx, rx) = scripted(
            vec![
                ThinEvent::TextDelta("partial".into()),
                ThinEvent::Error(err.clone()),
            ],
            true,
            false,
            true,
        );
        let response = stream_response(
            ctx,
            HeaderMap::new(),
            vec![],
            rx,
            StreamBounds {
                keepalive: None,
                idle: None,
            },
            StreamGuards {
                runner: AbortOnDrop::none(),
                inflight: None,
                log: access_log::test_guard(),
            },
        );
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let text = String::from_utf8(bytes.to_vec()).unwrap();
        let chunks = parse_sse(&text);
        let message = chunks.last().unwrap()["error"]["message"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(message.ends_with(&marker), "stream {message}");
        assert_eq!(message.matches("[code=").count(), 1, "{message}");
        if err.retry_after_seconds().is_some() {
            // The marker comes after the folded retry hints, not inside them.
            assert!(message.contains("s, should_retry="), "{message}");
        }
    }
}

#[tokio::test]
async fn an_adapter_that_vanishes_mid_stream_is_named_not_done() {
    let (ctx, rx) = scripted(
        vec![ThinEvent::TextDelta("partial".into())],
        true,
        false,
        true,
    );
    let response = stream_response(
        ctx,
        HeaderMap::new(),
        vec![],
        rx,
        StreamBounds {
            keepalive: None,
            idle: None,
        },
        StreamGuards {
            runner: AbortOnDrop::none(),
            inflight: None,
            log: access_log::test_guard(),
        },
    );
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let text = String::from_utf8(bytes.to_vec()).unwrap();
    assert!(!text.contains("[DONE]"), "{text}");
    assert_eq!(
        parse_sse(&text).last().unwrap()["error"]["type"],
        "vendor_down"
    );
}

/// Read as a stream, frame by frame: a collected body cannot tell a heartbeat from none.
#[tokio::test]
async fn a_silent_stream_carries_keepalive_comments_until_the_answer_resumes() {
    use http_body_util::BodyExt;
    let (tx, rx) = mpsc::channel(8);
    let ctx = Ctx {
        id: "chatcmpl-test".into(),
        created: 1,
        model: "m".into(),
        parallel: true,
        include_usage: false,
    };
    let response = stream_response(
        ctx,
        HeaderMap::new(),
        vec![],
        rx,
        StreamBounds {
            keepalive: Some(Duration::from_millis(30)),
            idle: None,
        },
        StreamGuards {
            runner: AbortOnDrop::none(),
            inflight: None,
            log: access_log::test_guard(),
        },
    );
    let mut body = response.into_body();
    let mut next = async || {
        let frame = tokio::time::timeout(Duration::from_secs(2), body.frame())
            .await
            .expect("a frame within 2s")
            .expect("stream still open")
            .unwrap();
        String::from_utf8(frame.into_data().unwrap().to_vec()).unwrap()
    };
    assert_eq!(next().await, KEEPALIVE);
    assert_eq!(next().await, KEEPALIVE);
    tx.send(ThinEvent::TextDelta("hello".into())).await.unwrap();
    let mut seen = next().await;
    while seen == KEEPALIVE {
        seen = next().await;
    }
    assert!(seen.contains("hello"), "{seen}");
    tx.send(ThinEvent::Finish(FinishReason::Stop))
        .await
        .unwrap();
    let mut rest = String::new();
    while !rest.contains("[DONE]") {
        rest.push_str(&next().await);
    }
}

/// Keep-alive must not hold a started stream forever when the adapter idle is off.
/// Paused time: on a loaded runner the first poll can land after the idle bound (#257).
#[tokio::test(start_paused = true)]
async fn a_silent_stream_ends_at_the_route_idle_even_with_keepalive() {
    let (_tx, rx) = mpsc::channel(8);
    let ctx = Ctx {
        id: "chatcmpl-test".into(),
        created: 1,
        model: "m".into(),
        parallel: true,
        include_usage: false,
    };
    let response = stream_response(
        ctx,
        HeaderMap::new(),
        vec![],
        rx,
        StreamBounds {
            keepalive: Some(Duration::from_millis(20)),
            idle: Some(Duration::from_millis(80)),
        },
        StreamGuards {
            runner: AbortOnDrop::none(),
            inflight: None,
            log: access_log::test_guard(),
        },
    );
    let bytes = tokio::time::timeout(Duration::from_secs(2), response.into_body().collect())
        .await
        .expect("the stream must end on its own")
        .unwrap()
        .to_bytes();
    let text = String::from_utf8(bytes.to_vec()).unwrap();
    assert!(text.contains(KEEPALIVE.trim()), "{text}");
    assert!(text.contains("route idle"), "{text}");
    assert!(!text.contains("[DONE]"), "{text}");
}

#[tokio::test]
async fn a_mid_stream_vendor_401_is_prefixed_so_pi_does_not_see_a_stale_key() {
    let events = vec![
        ThinEvent::TextDelta("partial".into()),
        ThinEvent::Error(ThinError::vendor_login_required(
            "grok",
            "grok 401: expired",
            "grok login",
        )),
    ];
    let (ctx, rx) = scripted(events, true, false, true);
    let response = stream_response(
        ctx,
        HeaderMap::new(),
        vec![],
        rx,
        StreamBounds {
            keepalive: None,
            idle: None,
        },
        StreamGuards {
            runner: AbortOnDrop::none(),
            inflight: None,
            log: access_log::test_guard(),
        },
    );
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let text = String::from_utf8(bytes.to_vec()).unwrap();
    let message = parse_sse(&text).last().unwrap()["error"]["message"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(message.starts_with("vendor 401: "), "{message}");
}

#[test]
fn error_response_carries_retry_headers() {
    let err = ThinError::rate_limited("claude", "quota", 120, true);
    let r = thin_error_response(&err);
    assert_eq!(r.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(r.headers()["retry-after"], "120");
    assert_eq!(r.headers()["x-should-retry"], "false");
}

#[test]
fn retryable_rate_limit_sets_should_retry_true() {
    let err = ThinError::rate_limited("claude", "bridge busy", 5, false);
    let r = thin_error_response(&err);
    assert_eq!(r.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(r.headers()["retry-after"], "5");
    assert_eq!(r.headers()["x-should-retry"], "true");
}
