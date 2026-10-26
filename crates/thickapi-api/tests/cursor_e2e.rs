#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
//! Cursor thin lane end to end (SPEC §19.8, T4): real router, real `CursorAdapter`, the agent
//! service replaced by a loopback h2c stand-in speaking the Connect/protobuf shape measured live
//! on 2026-09-17 (`cursor-agent 2026.09.15-d2fe57e`, `agentn.global.api5.cursor.sh`).

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
use thickapi_api::{build_router, make_state_with_store};
use thickapi_core::config::keys::{hash_token, KeyEntry, KeysConfig, SCOPE_CHAT_WRITE};
use thickapi_core::config::models::{ModelEntry, ModelsConfig, Transport};
use thickapi_core::config::router::{CursorCfg, RouterConfig, ThinConfig, Vendors};
use thickapi_core::config::{AccountsConfig, CombosConfig};
use thickapi_core::thin::oauth::cursor::{
    connect_frame, encode_protobuf_value, extract_request_context_exec, field_ld, field_str,
    field_varint, FrameDecoder, FLAG_END,
};
use thickapi_core::types::BackendId;
use thickapi_core::{ConfigBundle, Store};
use tower::ServiceExt;

const TOKEN: &str = "ta_cursor_e2e";

/// One scripted upstream reply: HTTP status plus the Connect frames of the response body.
type Reply = (u16, Vec<Vec<u8>>);

/// `(request headers, decoded frame-0 payload)` per upstream call.
type SeenCall = (HeaderMap, Vec<u8>);

#[derive(Clone)]
struct Script {
    replies: Arc<Mutex<Vec<Reply>>>,
    seen: Arc<Mutex<Vec<SeenCall>>>,
}

async fn run(State(s): State<Script>, headers: HeaderMap, body: Body) -> Response {
    // Read the paced request stream until frame 0 (the RunRequest) is complete, then keep the
    // rest draining in the background: the real service answers while the client still sends
    // marker frames and heartbeats.
    let mut stream = body.into_data_stream();
    let mut decoder = FrameDecoder::default();
    let mut frame0 = None;
    while frame0.is_none() {
        let Some(chunk) = futures_util::StreamExt::next(&mut stream).await else {
            break;
        };
        let chunk = chunk.unwrap();
        if let Some(f) = decoder.push(&chunk).unwrap().into_iter().next() {
            frame0 = Some(f.payload);
        }
    }
    tokio::spawn(
        async move { while futures_util::StreamExt::next(&mut stream).await.is_some() {} },
    );
    s.seen
        .lock()
        .unwrap()
        .push((headers, frame0.unwrap_or_default()));
    let (status, frames) = {
        let mut r = s.replies.lock().unwrap();
        if r.is_empty() {
            (500, vec![])
        } else {
            r.remove(0)
        }
    };
    let body: Vec<u8> = frames.concat();
    let mut response = (StatusCode::from_u16(status).unwrap(), body).into_response();
    if status == 464 {
        response
            .headers_mut()
            .insert("server", "awselb/2.0".parse().unwrap());
    } else {
        response
            .headers_mut()
            .insert("content-type", "application/connect+proto".parse().unwrap());
    }
    response
}

fn text_frame(text: &str) -> Vec<u8> {
    connect_frame(&field_ld(1, &field_ld(1, &field_str(1, text))), 0).to_vec()
}

fn reasoning_frame(text: &str) -> Vec<u8> {
    connect_frame(&field_ld(1, &field_ld(4, &field_str(1, text))), 0).to_vec()
}

fn tool_call_frame(name: &str, args: &[(&str, Value)], wire_id: &str) -> Vec<u8> {
    let mut mcp = field_str(5, name);
    for (k, v) in args {
        let mut entry = field_str(1, k);
        entry.extend(field_ld(2, &encode_protobuf_value(v)));
        mcp.extend(field_ld(2, &entry));
    }
    mcp.extend(field_str(3, wire_id));
    connect_frame(&field_ld(2, &field_ld(11, &mcp)), 0).to_vec()
}

/// Wire type 1 (64-bit little-endian). Field must be < 16 so the tag is one byte.
fn field_fixed64(field: u64, value: f64) -> Vec<u8> {
    let mut out = vec![(field << 3 | 1) as u8];
    out.extend_from_slice(&value.to_le_bytes());
    out
}

fn timing_metrics_f8() -> Vec<u8> {
    let mut inner = field_fixed64(1, 1.0);
    inner.extend(field_fixed64(2, 2.0));
    inner.extend(field_fixed64(3, 3.0));
    inner.extend(field_fixed64(4, 4.0));
    field_ld(8, &inner)
}

/// ≥5 silent nested Connect frames matching the live Fable tool trip, then MCP.
fn fable_nested_preamble_frames() -> Vec<Vec<u8>> {
    let mut timed = field_ld(1, &field_varint(25, 1_725_000_000));
    timed.extend(timing_metrics_f8());
    [
        field_ld(1, &field_ld(13, &[])),
        field_ld(1, &field_ld(7, &field_str(1, "tool_fable"))),
        field_ld(1, &field_ld(7, &field_str(2, "tool_fable"))),
        field_ld(1, &field_ld(7, &field_str(4, "tool_fable"))),
        timed,
        field_ld(1, &field_ld(8, &field_varint(1, 1))),
        field_ld(1, &field_ld(5, &field_str(1, "seq"))),
        field_ld(1, &field_ld(2, &field_str(1, "block"))),
    ]
    .into_iter()
    .map(|payload| connect_frame(&payload, 0).to_vec())
    .collect()
}

fn builtin_read_frame(path: &str) -> Vec<u8> {
    let mut inner = field_str(1, path);
    inner.extend(field_str(2, "tool_0d5c7c1e"));
    connect_frame(&field_ld(2, &field_ld(7, &inner)), 0).to_vec()
}

fn unknown_tool_field_frame() -> Vec<u8> {
    let mut inner = field_str(1, "C:\\secret.txt");
    inner.extend(field_str(2, "tool_moved"));
    connect_frame(&field_ld(2, &field_ld(99, &inner)), 0).to_vec()
}

fn end_ok() -> Vec<u8> {
    connect_frame(b"{}", FLAG_END).to_vec()
}

fn end_err(code: &str, message: &str) -> Vec<u8> {
    connect_frame(
        json!({ "error": { "code": code, "message": message } })
            .to_string()
            .as_bytes(),
        FLAG_END,
    )
    .to_vec()
}

async fn mock(replies: Vec<Reply>) -> (String, Script) {
    let script = Script {
        replies: Arc::new(Mutex::new(replies)),
        seen: Arc::new(Mutex::new(vec![])),
    };
    let app = Router::new()
        .route("/agent.v1.AgentService/Run", axum::routing::post(run))
        .with_state(script.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr: SocketAddr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}"), script)
}

fn jwt_with_exp(exp: u64) -> String {
    use base64::Engine;
    let claims = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(
        json!({ "sub": "google-oauth2|user_e2e", "exp": exp, "type": "session" }).to_string(),
    );
    format!("h.{claims}.s")
}

fn app(base_url: &str, tmp: &tempfile::TempDir, expired: bool) -> Router {
    app_with_loop_limit(base_url, tmp, expired, None, None)
}

fn app_with_loop_limit(
    base_url: &str,
    tmp: &tempfile::TempDir,
    expired: bool,
    builtin_loop_limit: Option<u32>,
    idle_after_text: Option<u64>,
) -> Router {
    let creds = tmp.path().join("auth.json");
    let exp = if expired { 946_684_800 } else { 32_503_680_000 };
    std::fs::write(
        &creds,
        json!({ "accessToken": jwt_with_exp(exp), "refreshToken": jwt_with_exp(exp) }).to_string(),
    )
    .unwrap();
    let config = ConfigBundle {
        config_dir: tmp.path().to_path_buf(),
        router: RouterConfig {
            data_dir: tmp.path().join("data"),
            thin: ThinConfig {
                vendors: Vendors {
                    cursor: CursorCfg {
                        base_url: Some(base_url.to_string()),
                        credentials_path: Some(creds),
                        client_version: Some("cli-2026.09.15-test".into()),
                        cwd: Some(tmp.path().to_string_lossy().into_owned()),
                        builtin_loop_limit,
                        budgets: {
                            let mut b = thickapi_core::config::router::CursorBudgetsCfg::default();
                            if let Some(secs) = idle_after_text {
                                b.idle_after_text_with_tools_seconds = secs;
                            }
                            b
                        },
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
            models: vec![ModelEntry {
                id: "subs/cursor/auto".into(),
                backend: BackendId::Cursor,
                cli_model: "auto".into(),
                context_window: 128_000,
                max_tokens: 32_000,
                thinking: true,
                cost: 0.0,
                name: "Auto".into(),
                provider: None,
                transport: Transport::OauthDirect,
                vendor_model: Some("auto".into()),
                enabled: true,
                input: vec!["text".into(), "image".into()],
            }],
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

async fn post(app: Router, body: Value) -> (StatusCode, HeaderMap, String) {
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

async fn get_ready(app: Router) -> Value {
    let response = app
        .oneshot(
            Request::builder()
                .uri("/ready")
                .header("authorization", format!("Bearer {TOKEN}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap()
}

fn weather_tool() -> Value {
    json!([{ "type": "function", "function": {
        "name": "get_weather",
        "description": "Get the current weather for a city",
        "parameters": { "type": "object", "properties": { "city": { "type": "string" } }, "required": ["city"] }
    }}])
}

#[tokio::test]
async fn tool_turn_then_replay_flattens_transcript_and_declares_tools_natively() {
    let (base, script) = mock(vec![
        (
            200,
            vec![
                reasoning_frame("Need the tool."),
                tool_call_frame(
                    "get_weather",
                    &[("city", json!("Brisbane")), ("days", json!(3))],
                    "tool_9f1c",
                ),
                end_ok(),
            ],
        ),
        (
            200,
            vec![text_frame("Brisbane is "), text_frame("23.5°C."), end_ok()],
        ),
    ])
    .await;
    let tmp = tempfile::tempdir().unwrap();

    // Turn 1: the model calls the bridged tool.
    let (status, headers, body) = post(
        app(&base, &tmp, false),
        json!({
            "model": "cursor/auto",
            "messages": [
                { "role": "system", "content": "Be terse." },
                { "role": "user", "content": "Weather in Brisbane?" }
            ],
            "tools": weather_tool(),
            "reasoning_effort": "high"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(headers["x-thickapi-usage"], "estimated");
    let v: Value = serde_json::from_str(&body).unwrap();
    assert!(
        v["usage"]["prompt_tokens"].as_u64().unwrap() > 0
            && v["usage"]["completion_tokens"].as_u64().unwrap() > 0,
        "estimated usage should be non-zero: {}",
        v["usage"]
    );
    let msg = &v["choices"][0]["message"];
    assert_eq!(v["choices"][0]["finish_reason"], "tool_calls");
    assert_eq!(msg["reasoning_content"], "Need the tool.");
    let call = &msg["tool_calls"][0];
    let call_id = call["id"].as_str().unwrap().to_string();
    assert!(
        call_id.starts_with("call_") && call_id.len() == 29,
        "{call_id}"
    );
    assert_eq!(call["function"]["name"], "get_weather");
    let args: Value =
        serde_json::from_str(call["function"]["arguments"].as_str().unwrap()).unwrap();
    assert_eq!(args, json!({ "city": "Brisbane", "days": 3 }));
    assert_eq!(v["model"], "auto");

    {
        let seen = script.seen.lock().unwrap();
        assert_eq!(seen.len(), 1);
        let (headers, frame0) = &seen[0];
        assert_eq!(headers["x-cursor-client-type"], "cli");
        assert_eq!(headers["x-cursor-client-version"], "cli-2026.09.15-test");
        assert_eq!(headers["content-type"], "application/connect+proto");
        assert_eq!(headers["connect-protocol-version"], "1");
        assert!(headers["authorization"]
            .to_str()
            .unwrap()
            .starts_with("Bearer h."));
        let hay = String::from_utf8_lossy(frame0);
        assert!(hay.contains("<system>\nBe terse.\n</system>"), "{hay}");
        assert!(
            hay.contains("<user>\nWeather in Brisbane?\n</user>"),
            "{hay}"
        );
        // Tools are declared natively (McpToolDefinition), not inlined in the prompt.
        assert!(hay.contains("Get the current weather for a city"));
        assert!(hay.contains("thickapi"));
        assert!(!hay.contains("<tools>"));
        assert!(hay.contains("default"));
    }

    // Turn 2: the client replays the call and its result; the transcript carries both.
    let (status, _, body) = post(
        app(&base, &tmp, false),
        json!({
            "model": "subs/cursor/auto",
            "messages": [
                { "role": "user", "content": "Weather in Brisbane?" },
                { "role": "assistant", "content": null, "tool_calls": [
                    { "id": call_id, "type": "function", "function": { "name": "get_weather", "arguments": "{\"city\":\"Brisbane\"}" } }
                ]},
                { "role": "tool", "tool_call_id": call_id, "content": "{\"temperature_c\":23.5}" }
            ],
            "tools": weather_tool()
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["choices"][0]["message"]["content"], "Brisbane is 23.5°C.");
    assert_eq!(v["choices"][0]["finish_reason"], "stop");
    let seen = script.seen.lock().unwrap();
    let hay = String::from_utf8_lossy(&seen[1].1);
    assert!(
        hay.contains(&format!(
            "<tool_call id=\"{call_id}\" name=\"get_weather\">"
        )),
        "{hay}"
    );
    assert!(hay.contains("{\"city\":\"Brisbane\"}"), "{hay}");
    assert!(hay.contains("<<<TR-") && hay.contains(">>>TR-"), "{hay}");
    assert!(
        hay.contains(&format!("<tool_result tool_call_id=\"{call_id}\">")),
        "{hay}"
    );
    assert!(hay.contains("{\"temperature_c\":23.5}"), "{hay}");
}

fn whistlr_tools() -> Value {
    json!([
        { "type": "function", "function": {
            "name": "whistlr_whoami",
            "description": "Identify this seat.",
            "parameters": { "type": "object", "properties": {}, "required": [], "additionalProperties": false }
        }},
        { "type": "function", "function": {
            "name": "whistlr_send",
            "description": "Send a message.",
            "parameters": {
                "type": "object",
                "properties": {
                    "to": { "type": "string" },
                    "body": { "type": "string" },
                    "priority": { "type": "string", "enum": ["normal", "urgent"] },
                    "payload": { "type": "object" }
                },
                "required": ["to", "body"],
                "additionalProperties": false
            }
        }}
    ])
}

#[tokio::test]
async fn whistlr_tools_declared_and_whoami_call_bridged() {
    let (base, script) = mock(vec![(
        200,
        vec![
            tool_call_frame("whistlr_whoami", &[], "tool_whoami"),
            end_ok(),
        ],
    )])
    .await;
    let tmp = tempfile::tempdir().unwrap();
    let (status, _, body) = post(
        app(&base, &tmp, false),
        json!({
            "model": "subs/cursor/auto",
            "messages": [{ "role": "user", "content": "Call whistlr_whoami." }],
            "tools": whistlr_tools()
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["choices"][0]["finish_reason"], "tool_calls");
    assert_eq!(
        v["choices"][0]["message"]["tool_calls"][0]["function"]["name"],
        "whistlr_whoami"
    );
    let seen = script.seen.lock().unwrap();
    let hay = String::from_utf8_lossy(&seen[0].1);
    assert!(hay.contains("whistlr_whoami"), "{hay}");
    assert!(hay.contains("whistlr_send"), "{hay}");
    assert!(hay.contains("thickapi"), "{hay}");
    assert!(hay.contains("Identify this seat."), "{hay}");
    assert!(!hay.contains("<tools>"), "{hay}");
}

#[tokio::test]
async fn streaming_turn_ends_cleanly_when_body_closes_without_trailer() {
    // No Connect trailer: the body just ends after the answer — that is a normal end once
    // output has arrived (the real service holds the stream open for an exec-result instead).
    let (base, _) = mock(vec![(200, vec![text_frame("PONG")])]).await;
    let tmp = tempfile::tempdir().unwrap();
    let (status, headers, body) = post(
        app(&base, &tmp, false),
        json!({ "model": "cursor/auto", "messages": [{"role": "user", "content": "ping"}], "stream": true }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(headers["content-type"]
        .to_str()
        .unwrap()
        .starts_with("text/event-stream"));
    assert!(body.contains("\"content\":\"PONG\""), "{body}");
    assert!(body.contains("\"finish_reason\":\"stop\""), "{body}");
    assert!(body.trim_end().ends_with("data: [DONE]"), "{body}");
}

#[tokio::test]
async fn fable_nested_preamble_then_mcp_is_tool_calls_not_drift() {
    let mut frames = fable_nested_preamble_frames();
    frames.push(tool_call_frame(
        "read_file",
        &[("path", json!("x"))],
        "tool_mcp",
    ));
    frames.push(end_ok());
    let (base, _) = mock(vec![(200, frames)]).await;
    let tmp = tempfile::tempdir().unwrap();
    let (status, _, body) = post(
        app(&base, &tmp, false),
        json!({
            "model": "cursor/auto",
            "messages": [{ "role": "user", "content": "read it" }],
            "tools": [{
                "type": "function",
                "function": {
                    "name": "read_file",
                    "description": "Read a file.",
                    "parameters": { "type": "object", "properties": { "path": { "type": "string" } } }
                }
            }]
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["choices"][0]["finish_reason"], "tool_calls");
    assert_eq!(
        v["choices"][0]["message"]["tool_calls"][0]["function"]["name"],
        "read_file"
    );
}

/// A built-in read of a file that exists is Pi `read` (G2); one of a missing file is
/// answered in protocol (G1, `builtin_pre_read_of_a_missing_file_is_answered_then_the_write_is_pi_write`).
#[tokio::test]
async fn builtin_read_is_a_pi_read() {
    let tmp = tempfile::tempdir().unwrap();
    let secret = tmp.path().join("secret.txt");
    std::fs::write(&secret, "x").unwrap();
    let secret = secret.to_string_lossy().into_owned();
    let (base, _) = mock(vec![(200, vec![builtin_read_frame(&secret), end_ok()])]).await;
    let (status, _, body) = post(
        app(&base, &tmp, false),
        json!({
            "model": "cursor/auto",
            "messages": [{ "role": "user", "content": "read it" }],
            "tools": [{
                "type": "function",
                "function": {
                    "name": "read",
                    "description": "Read a file.",
                    "parameters": { "type": "object", "properties": { "path": { "type": "string" } } }
                }
            }]
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["choices"][0]["finish_reason"], "tool_calls");
    assert_eq!(
        v["choices"][0]["message"]["tool_calls"][0]["function"]["name"],
        "read"
    );
    assert_eq!(
        v["choices"][0]["message"]["tool_calls"][0]["function"]["arguments"],
        json!({ "path": secret }).to_string()
    );
}

#[tokio::test]
async fn unknown_tool_field_is_non_retryable_vendor_drift() {
    let (base, _) = mock(vec![(200, vec![unknown_tool_field_frame(), end_ok()])]).await;
    let tmp = tempfile::tempdir().unwrap();
    let (status, _, body) = post(
        app(&base, &tmp, false),
        json!({ "model": "cursor/auto", "messages": [{"role": "user", "content": "read it"}] }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_GATEWAY, "{body}");
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["error"]["code"], "vendor_drift");
    assert_eq!(v["error"]["vendor"], "cursor");
    assert_eq!(v["error"]["should_retry"], false);
    assert!(v["error"]["message"].as_str().unwrap().contains("99"));
}

#[tokio::test]
async fn connect_not_found_names_the_wire_model() {
    let (base, _) = mock(vec![(200, vec![end_err("not_found", "Error")])]).await;
    let tmp = tempfile::tempdir().unwrap();
    let router = app(&base, &tmp, false);
    let (status, _, body) = post(
        router.clone(),
        json!({ "model": "cursor/auto", "messages": [{"role": "user", "content": "x"}] }),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["error"]["code"], "model_not_found");
    assert_eq!(v["error"]["vendor"], "cursor");
    let message = v["error"]["message"].as_str().unwrap();
    assert!(message.contains("default"), "{message}");
    assert!(message.contains("fast=false"), "{message}");
    assert!(message.contains("Error"), "{message}");
    let ready = get_ready(router).await;
    let cursor = &ready["thin"]["vendors"]["cursor"];
    let oauth = &cursor["oauth"];
    assert_eq!(oauth["status"], "ok", "{oauth}");
    assert!(oauth["last_rejection"].is_null(), "{oauth}");
    assert_eq!(oauth["successes"], 0, "{oauth}");
    assert_eq!(oauth["last_row_rejection"]["code"], "model_not_found");
    assert_eq!(oauth["last_row_rejection"]["status"], 404);
    assert!(
        oauth["last_row_rejection"]["wire"]
            .as_str()
            .unwrap()
            .contains("default"),
        "{oauth}"
    );
    // #147: a Connect `not_found` trailer marks the row unavailable the same as HTTP 404,
    // without touching the vendor's own `last_rejection` (checked above).
    assert_eq!(
        cursor["rows_unavailable"],
        json!(["subs/cursor/auto"]),
        "{cursor}"
    );
}

/// C3 (b): the Connect `not_found` trailer now goes through `VendorSession::run` and Cursor's
/// `on_outcome` hook. On a streamed request as on a plain one it lands in
/// `last_row_rejection` and `rows_unavailable`, never in the vendor's `last_rejection` or
/// `successes`, and it opens no second Run (only a reasoning-only end is held for one).
#[tokio::test]
async fn connect_not_found_through_the_runner_is_a_row_fact_not_a_vendor_rejection() {
    let (base, script) = mock(vec![
        (200, vec![end_err("not_found", "Error")]),
        (200, vec![text_frame("never"), end_ok()]),
    ])
    .await;
    let tmp = tempfile::tempdir().unwrap();
    let router = app(&base, &tmp, false);
    let (status, _, body) = post(
        router.clone(),
        json!({ "model": "cursor/auto", "stream": true,
                "messages": [{"role": "user", "content": "x"}] }),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["error"]["code"], "model_not_found", "{body}");
    assert_eq!(script.seen.lock().unwrap().len(), 1, "one Run, no retry");
    let ready = get_ready(router).await;
    let cursor = &ready["thin"]["vendors"]["cursor"];
    let oauth = &cursor["oauth"];
    assert!(oauth["last_rejection"].is_null(), "{oauth}");
    assert_eq!(oauth["successes"], 0, "{oauth}");
    assert_eq!(
        oauth["last_row_rejection"]["code"], "model_not_found",
        "{oauth}"
    );
    assert_eq!(oauth["last_row_rejection"]["status"], 404, "{oauth}");
    assert_eq!(
        cursor["rows_unavailable"],
        json!(["subs/cursor/auto"]),
        "{cursor}"
    );
}

#[tokio::test]
async fn connect_unauthenticated_trailer_is_401_login_required() {
    let (base, _) = mock(vec![(
        200,
        vec![end_err("unauthenticated", "token rejected")],
    )])
    .await;
    let tmp = tempfile::tempdir().unwrap();
    let router = app(&base, &tmp, false);
    let (status, _, body) = post(
        router.clone(),
        json!({ "model": "cursor/auto", "messages": [{"role": "user", "content": "x"}] }),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["error"]["code"], "vendor_login_required");
    assert_eq!(v["error"]["hint"], "cursor-agent login");
    let ready = get_ready(router).await;
    let oauth = &ready["thin"]["vendors"]["cursor"]["oauth"];
    assert_eq!(oauth["status"], "ok", "{oauth}");
    assert_eq!(oauth["last_rejection"], "vendor_login_required");
    assert_eq!(oauth["successes"], 0, "{oauth}");
    assert!(oauth.get("last_row_rejection").is_none(), "{oauth}");
}

#[tokio::test]
async fn http_404_is_a_row_fact_not_vendor_rejection() {
    let (base, _) = mock(vec![(404, vec![])]).await;
    let tmp = tempfile::tempdir().unwrap();
    let router = app(&base, &tmp, false);
    let (status, _, body) = post(
        router.clone(),
        json!({ "model": "cursor/auto", "messages": [{"role": "user", "content": "x"}] }),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["error"]["code"], "model_not_found");
    assert_eq!(v["error"]["vendor"], "cursor");
    let ready = get_ready(router).await;
    let cursor = &ready["thin"]["vendors"]["cursor"];
    let oauth = &cursor["oauth"];
    assert_eq!(oauth["status"], "ok", "{oauth}");
    assert!(oauth["last_rejection"].is_null(), "{oauth}");
    assert_eq!(oauth["last_row_rejection"]["code"], "model_not_found");
    assert_eq!(oauth["last_row_rejection"]["status"], 404);
    assert!(
        oauth["last_row_rejection"]["wire"]
            .as_str()
            .unwrap()
            .contains("default"),
        "{oauth}"
    );
    assert_eq!(
        cursor["rows_unavailable"],
        json!(["subs/cursor/auto"]),
        "{cursor}"
    );
}

#[tokio::test]
async fn reasoning_only_turn_is_vendor_drift_not_empty_stop() {
    let (base, script) = mock(vec![
        (200, vec![reasoning_frame("planning"), end_ok()]),
        (200, vec![reasoning_frame("still planning"), end_ok()]),
    ])
    .await;
    let tmp = tempfile::tempdir().unwrap();
    let (status, _, body) = post(
        app(&base, &tmp, false),
        json!({ "model": "cursor/auto", "messages": [{"role": "user", "content": "x"}] }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_GATEWAY, "{body}");
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["error"]["code"], "vendor_drift");
    assert!(
        v["error"]["message"]
            .as_str()
            .unwrap()
            .contains("reasoning"),
        "{body}"
    );
    assert!(v["choices"].is_null(), "{body}");
    let seen = script.seen.lock().unwrap();
    assert_eq!(seen.len(), 2, "one fresh Run, then the error is final");
    assert_ne!(seen[0].1, seen[1].1, "the retry is a new conversation");
}

/// #127: a rendered built-in is answer text the client already has. A second Run would be
/// appended to it as one message, so there is no second Run and the drift is final.
#[tokio::test]
async fn rendered_builtin_after_streamed_text_is_final_not_spliced() {
    let (base, script) = mock(vec![
        (200, vec![text_frame("⚡ read(1)\nReading now."), end_ok()]),
        (200, vec![text_frame("the answer"), end_ok()]),
    ])
    .await;
    let tmp = tempfile::tempdir().unwrap();
    let (status, _, body) = post(
        app(&base, &tmp, false),
        json!({ "model": "cursor/auto", "messages": [{"role": "user", "content": "x"}] }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_GATEWAY, "{body}");
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["error"]["code"], "vendor_drift");
    assert!(!body.contains("the answer"), "{body}");
    assert_eq!(script.seen.lock().unwrap().len(), 1, "no second Run");
}

fn turn_ended_frame() -> Vec<u8> {
    connect_frame(&field_ld(1, &field_varint(14, 1)), 0).to_vec()
}

fn bash_tool() -> Value {
    json!([{ "type": "function", "function": {
        "name": "bash",
        "description": "Run a command",
        "parameters": { "type": "object", "properties": {
            "command": { "type": "string" }
        }, "required": ["command"] }
    }}])
}

/// #215: a transcript-echo `<tool_call>` written as answer text is the tool call.
/// The markup is not assistant content.
#[tokio::test]
async fn text_tool_call_transcript_echo_finishes_tool_calls() {
    let text = "<tool_call id=\"model-id\" name=\"bash\">\n<<<TR-22a89cfe\n{\"command\":\"ls\"}\n>>>TR-22a89cfe\n</tool_call>";
    let (base, _) = mock(vec![(
        200,
        vec![text_frame(text), turn_ended_frame(), end_ok()],
    )])
    .await;
    let tmp = tempfile::tempdir().unwrap();
    let (status, _, body) = post(
        app(&base, &tmp, false),
        json!({
            "model": "cursor/auto",
            "messages": [{ "role": "user", "content": "list the directory" }],
            "tools": bash_tool()
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["choices"][0]["finish_reason"], "tool_calls");
    let call = &v["choices"][0]["message"]["tool_calls"][0]["function"];
    assert_eq!(call["name"], "bash");
    let args: Value = serde_json::from_str(call["arguments"].as_str().unwrap()).unwrap();
    assert_eq!(args, json!({ "command": "ls" }));
    assert!(!body.contains("<tool_call"), "{body}");
    assert!(!body.contains("<<<TR-"), "{body}");
    let content = &v["choices"][0]["message"]["content"];
    assert!(
        content.is_null() || content.as_str().is_some_and(|s| s.is_empty()),
        "{content}"
    );
}

#[tokio::test]
async fn reasoning_only_turn_retries_once_and_keeps_the_answer() {
    let (base, script) = mock(vec![
        (200, vec![reasoning_frame("planning"), end_ok()]),
        (200, vec![text_frame("the answer"), end_ok()]),
    ])
    .await;
    let tmp = tempfile::tempdir().unwrap();
    let (status, _, body) = post(
        app(&base, &tmp, false),
        json!({ "model": "cursor/auto", "messages": [{"role": "user", "content": "x"}] }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["choices"][0]["finish_reason"], "stop");
    assert_eq!(v["choices"][0]["message"]["content"], "the answer");
    assert_eq!(script.seen.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn http_464_is_vendor_drift_naming_the_client_version() {
    let (base, _) = mock(vec![(464, vec![])]).await;
    let tmp = tempfile::tempdir().unwrap();
    let (status, _, body) = post(
        app(&base, &tmp, false),
        json!({ "model": "cursor/auto", "messages": [{"role": "user", "content": "x"}] }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_GATEWAY, "{body}");
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["error"]["code"], "vendor_drift");
    assert!(v["error"]["message"]
        .as_str()
        .unwrap()
        .contains("cli-2026.09.15-test"));
}

#[tokio::test]
async fn expired_session_is_401_without_calling_cursor() {
    let (base, script) = mock(vec![(200, vec![text_frame("never"), end_ok()])]).await;
    let tmp = tempfile::tempdir().unwrap();
    let (status, _, body) = post(
        app(&base, &tmp, true),
        json!({ "model": "cursor/auto", "messages": [{"role": "user", "content": "x"}] }),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["error"]["code"], "vendor_login_required");
    assert_eq!(v["error"]["vendor"], "cursor");
    assert!(script.seen.lock().unwrap().is_empty());
}

#[tokio::test]
async fn ready_reports_cursor_vendor() {
    let (base, _) = mock(vec![]).await;
    let tmp = tempfile::tempdir().unwrap();
    let response = app(&base, &tmp, false)
        .oneshot(
            Request::builder()
                .uri("/ready")
                .header("authorization", format!("Bearer {TOKEN}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let v: Value = serde_json::from_slice(&bytes).unwrap();
    let cursor = &v["thin"]["vendors"]["cursor"];
    assert_eq!(cursor["path"], "oauth");
    assert_eq!(cursor["client_version"], "cli-2026.09.15-test");
    assert_eq!(cursor["token"]["present"], true);
    assert_eq!(cursor["token"]["expired"], false);
    assert_eq!(cursor["token"]["note"], "type=session idp=google-oauth2");
}

fn write_tool() -> Value {
    json!([{ "type": "function", "function": {
        "name": "write",
        "description": "Write a file",
        "parameters": { "type": "object", "properties": {
            "path": { "type": "string" },
            "contents": { "type": "string" }
        }, "required": ["path", "contents"] }
    }}])
}

#[derive(Clone)]
struct Paced {
    frames: Vec<(u64, Vec<u8>)>,
}

async fn run_paced(State(s): State<Paced>, body: Body) -> Response {
    let mut stream = body.into_data_stream();
    tokio::spawn(
        async move { while futures_util::StreamExt::next(&mut stream).await.is_some() {} },
    );
    let frames = s.frames;
    let out = async_stream::stream! {
        for (ms, frame) in frames {
            if ms > 0 {
                tokio::time::sleep(Duration::from_millis(ms)).await;
            }
            yield Ok::<_, std::io::Error>(axum::body::Bytes::from(frame));
        }
    };
    let mut response = Response::new(Body::from_stream(out));
    *response.status_mut() = StatusCode::OK;
    response
        .headers_mut()
        .insert("content-type", "application/connect+proto".parse().unwrap());
    response
}

async fn mock_paced(frames: Vec<(u64, Vec<u8>)>) -> String {
    let app = Router::new()
        .route("/agent.v1.AgentService/Run", axum::routing::post(run_paced))
        .with_state(Paced { frames });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr: SocketAddr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}")
}

fn request_context_args_frame(id: u64, exec_id: &str) -> Vec<u8> {
    let mut esm = field_varint(1, id);
    esm.extend(field_str(15, exec_id));
    esm.extend(field_ld(10, &[]));
    connect_frame(&field_ld(2, &esm), 0).to_vec()
}

/// 2026.09.18 handshake: emit `request_context_args`, wait for a matching exec result
/// on the duplex body, then stream the answer. Without the reply the live service
/// trailers `internal: No exec result`. `asks` repeats the same exec, each wanting its own
/// result, as the live service may. `tail` is what follows the answered asks.
async fn run_handshake(body: Body, asks: usize, tail: Vec<Vec<u8>>) -> Response {
    use futures_util::StreamExt;
    let mut stream = body.into_data_stream();
    let (got_tx, mut got_rx) = tokio::sync::mpsc::unbounded_channel::<()>();
    tokio::spawn(async move {
        let mut decoder = FrameDecoder::default();
        while let Some(chunk) = stream.next().await {
            let Ok(chunk) = chunk else {
                continue;
            };
            for f in decoder.push(&chunk).unwrap_or_default() {
                // Frame 1 is a proactive env result with id=0; the handshake reply echoes id=7.
                if extract_request_context_exec(&f.payload).is_some_and(|e| e.id == 7) {
                    let _ = got_tx.send(());
                }
            }
        }
    });
    let out = async_stream::stream! {
        for _ in 0..asks {
            yield Ok::<_, std::io::Error>(axum::body::Bytes::from(request_context_args_frame(
                7, "abc123",
            )));
            if !matches!(
                tokio::time::timeout(Duration::from_secs(2), got_rx.recv()).await,
                Ok(Some(()))
            ) {
                yield Ok(axum::body::Bytes::from(end_err("internal", "No exec result")));
                return;
            }
        }
        for frame in tail {
            yield Ok(axum::body::Bytes::from(frame));
        }
    };
    let mut response = Response::new(Body::from_stream(out));
    *response.status_mut() = StatusCode::OK;
    response
        .headers_mut()
        .insert("content-type", "application/connect+proto".parse().unwrap());
    response
}

async fn mock_handshake(asks: usize) -> String {
    let (base, _) = mock_handshake_runs(vec![vec![text_frame("OK"), end_ok()]; 4], asks).await;
    base
}

/// One answered ask per Run, then the next scripted tail (the last one repeats). The counter
/// is the number of upstream Runs.
async fn mock_handshake_runs(
    tails: Vec<Vec<Vec<u8>>>,
    asks: usize,
) -> (String, Arc<std::sync::atomic::AtomicUsize>) {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let runs = Arc::new(AtomicUsize::new(0));
    let counter = runs.clone();
    let tails = Arc::new(tails);
    let app = Router::new().route(
        "/agent.v1.AgentService/Run",
        axum::routing::post(move |body: Body| {
            let n = counter.fetch_add(1, Ordering::SeqCst);
            let tail = tails[n.min(tails.len() - 1)].clone();
            async move { run_handshake(body, asks, tail).await }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr: SocketAddr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}"), runs)
}

async fn handshake_turn(asks: usize) {
    let base = mock_handshake(asks).await;
    let tmp = tempfile::tempdir().unwrap();
    let (status, _, body) = post(
        app(&base, &tmp, false),
        json!({
            "model": "cursor/auto",
            "messages": [{"role": "user", "content": "Reply with the single word OK."}]
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["choices"][0]["message"]["content"], "OK", "{body}");
    assert_eq!(v["choices"][0]["finish_reason"], "stop");
}

#[tokio::test]
async fn request_context_args_is_answered_instead_of_no_exec_result() {
    handshake_turn(1).await;
}

/// The same exec asked again wants another result; one answer per exec is not enough.
#[tokio::test]
async fn a_repeated_request_context_ask_is_answered_every_time() {
    handshake_turn(3).await;
}

/// A varint at `buf[*pos..]`, advancing `pos`.
fn read_varint(buf: &[u8], pos: &mut usize) -> Option<u64> {
    let mut value = 0u64;
    for shift in (0..64).step_by(7) {
        let byte = *buf.get(*pos)?;
        *pos += 1;
        value |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Some(value);
        }
    }
    None
}

/// Top-level `(field, varint)` and `(field, bytes)` of one protobuf message.
fn proto_fields(buf: &[u8]) -> Vec<(u64, Option<u64>, &[u8])> {
    let mut out = Vec::new();
    let mut pos = 0;
    while pos < buf.len() {
        let Some(tag) = read_varint(buf, &mut pos) else {
            break;
        };
        match tag & 7 {
            0 => match read_varint(buf, &mut pos) {
                Some(v) => out.push((tag >> 3, Some(v), &buf[0..0])),
                None => break,
            },
            2 => {
                let Some(len) = read_varint(buf, &mut pos) else {
                    break;
                };
                let end = pos + len as usize;
                if end > buf.len() {
                    break;
                }
                out.push((tag >> 3, None, &buf[pos..end]));
                pos = end;
            }
            _ => break,
        }
    }
    out
}

/// `AgentClientMessage.exec_client_control_message.stream_close.id` (`f5.f1.f1`, 0 when
/// unset), when the frame is one.
fn stream_close_id(payload: &[u8]) -> Option<u64> {
    let control = proto_fields(payload)
        .into_iter()
        .find(|(f, v, _)| *f == 5 && v.is_none())?
        .2;
    let close = proto_fields(control)
        .into_iter()
        .find(|(f, v, _)| *f == 1 && v.is_none())?
        .2;
    Some(
        proto_fields(close)
            .into_iter()
            .find(|(f, _, _)| *f == 1)
            .and_then(|(_, v, _)| v)
            .unwrap_or(0),
    )
}

/// What the race stand-in read on the duplex once its ask existed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Duplex {
    /// `request_context_result` for exec 0, echoing the ask's `exec_id`.
    Result,
    /// `stream_close {id: 0}`.
    Close,
}

/// The P4 race as the live service plays it (F1, #181/#186): the ask is exec **0**, issued
/// at `issue_ms` and landing at the client `deliver_ms` later (one-way latency). A
/// `stream_close {id: 0}` read after the ask was issued and before its result closes the
/// exec with no result: `internal: No exec result`. A close read before the ask existed
/// does nothing (every live ask that landed at ≥ 1.83 s succeeded).
async fn run_exec_race(
    body: Body,
    issue_ms: u64,
    deliver_ms: u64,
    seen: Arc<Mutex<Vec<Duplex>>>,
) -> Response {
    use futures_util::StreamExt;
    let start = tokio::time::Instant::now();
    let issued = start + Duration::from_millis(issue_ms);
    let mut stream = body.into_data_stream();
    let (got_tx, mut got_rx) = tokio::sync::mpsc::unbounded_channel::<Duplex>();
    tokio::spawn(async move {
        let mut decoder = FrameDecoder::default();
        while let Some(chunk) = stream.next().await {
            let Ok(chunk) = chunk else {
                continue;
            };
            for f in decoder.push(&chunk).unwrap_or_default() {
                if tokio::time::Instant::now() < issued {
                    continue;
                }
                let event = if extract_request_context_exec(&f.payload)
                    .is_some_and(|e| e.id == 0 && e.exec_id.as_deref() == Some("abc123"))
                {
                    Duplex::Result
                } else if stream_close_id(&f.payload) == Some(0) {
                    Duplex::Close
                } else {
                    continue;
                };
                seen.lock().unwrap().push(event);
                let _ = got_tx.send(event);
            }
        }
    });
    let out = async_stream::stream! {
        tokio::time::sleep_until(start + Duration::from_millis(deliver_ms)).await;
        yield Ok::<_, std::io::Error>(axum::body::Bytes::from(request_context_args_frame(
            0, "abc123",
        )));
        let answered = matches!(
            tokio::time::timeout(Duration::from_secs(2), got_rx.recv()).await,
            Ok(Some(Duplex::Result))
        );
        if answered {
            yield Ok(axum::body::Bytes::from(text_frame("OK")));
            yield Ok(axum::body::Bytes::from(end_ok()));
        } else {
            yield Ok(axum::body::Bytes::from(end_err("internal", "No exec result")));
        }
    };
    let mut response = Response::new(Body::from_stream(out));
    *response.status_mut() = StatusCode::OK;
    response
        .headers_mut()
        .insert("content-type", "application/connect+proto".parse().unwrap());
    response
}

/// H2 deliverable 0: the ask lands 1.6 s after the Run opens (issued 200 ms earlier), inside
/// the 1.507–1.723 s window where all 15 live deaths fell. The adapter used to send a paced
/// `stream_close {id: 0}` at 1.5 s, which crossed the ask in flight. Now the close goes out
/// only after the result for that exec, as `cursor-agent` does, so the turn succeeds on the
/// first Run.
#[tokio::test]
async fn request_context_ask_at_1_6_s_is_answered_before_its_stream_close() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = seen.clone();
    let runs = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counter = runs.clone();
    let app_router = Router::new().route(
        "/agent.v1.AgentService/Run",
        axum::routing::post(move |body: Body| {
            counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let log = log.clone();
            async move { run_exec_race(body, 1_400, 1_600, log).await }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr: SocketAddr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app_router).await.unwrap() });
    let base = format!("http://{addr}");
    let tmp = tempfile::tempdir().unwrap();
    let (status, _, body) = post(
        app(&base, &tmp, false),
        json!({
            "model": "cursor/auto",
            "messages": [{"role": "user", "content": "Reply with the single word OK."}]
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["choices"][0]["message"]["content"], "OK", "{body}");
    assert_eq!(
        runs.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "no fresh Run needed"
    );
    // The close for exec 0 follows its result (it is written right after it).
    for _ in 0..50 {
        if seen.lock().unwrap().len() >= 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(
        seen.lock().unwrap().as_slice(),
        [Duplex::Result, Duplex::Close]
    );
}

async fn handshake_runs_turn(
    tails: Vec<Vec<Vec<u8>>>,
    stream: bool,
) -> (StatusCode, HeaderMap, String, usize) {
    let (base, runs) = mock_handshake_runs(tails, 1).await;
    let tmp = tempfile::tempdir().unwrap();
    let (status, headers, body) = post(
        app(&base, &tmp, false),
        json!({
            "model": "cursor/auto",
            "stream": stream,
            "messages": [{"role": "user", "content": "Reply with the single word OK."}]
        }),
    )
    .await;
    (
        status,
        headers,
        body,
        runs.load(std::sync::atomic::Ordering::SeqCst),
    )
}

fn no_exec_result() -> Vec<u8> {
    end_err("internal", "No exec result")
}

/// #186 backstop: `internal: No exec result` before any text, reasoning or tool call opens
/// one fresh Run under the same permit; the client sees only the second Run.
#[tokio::test]
async fn no_exec_result_before_output_opens_one_fresh_run() {
    let (status, _, body, runs) = handshake_runs_turn(
        vec![vec![no_exec_result()], vec![text_frame("OK"), end_ok()]],
        false,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["choices"][0]["message"]["content"], "OK", "{body}");
    assert_eq!(v["choices"][0]["finish_reason"], "stop");
    assert_eq!(runs, 2, "one fresh Run");
}

/// The first Run emitted `Started` only; nothing else of it reaches a streaming client.
#[tokio::test]
async fn no_exec_result_retry_is_invisible_on_the_stream() {
    let (status, _, body, runs) = handshake_runs_turn(
        vec![vec![no_exec_result()], vec![text_frame("OK"), end_ok()]],
        true,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(runs, 2);
    assert!(!body.contains("error"), "{body}");
    assert!(!body.contains("No exec result"), "{body}");
    assert!(body.contains("\"content\":\"OK\""), "{body}");
    assert!(body.contains("\"finish_reason\":\"stop\""), "{body}");
    assert!(body.trim_end().ends_with("data: [DONE]"), "{body}");
}

/// A second `No exec result` in the same request is final: 503 `vendor_down`, retryable.
#[tokio::test]
async fn a_second_no_exec_result_is_a_retryable_503() {
    let (status, headers, body, runs) =
        handshake_runs_turn(vec![vec![no_exec_result()]], false).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert_eq!(headers.get("x-should-retry").unwrap(), "true");
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["error"]["code"], "vendor_down");
    assert_eq!(v["error"]["should_retry"], true, "{body}");
    assert!(
        v["error"]["message"]
            .as_str()
            .unwrap()
            .contains("No exec result"),
        "{body}"
    );
    assert_eq!(runs, 2, "one fresh Run, then the error is final");
}

/// After output the trailer keeps today's rule: an error after `Started`, no second Run.
#[tokio::test]
async fn no_exec_result_after_text_is_not_retried() {
    let (status, _, body, runs) = handshake_runs_turn(
        vec![
            vec![text_frame("partial"), no_exec_result()],
            vec![text_frame("second run"), end_ok()],
        ],
        false,
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["error"]["code"], "vendor_down");
    assert!(!body.contains("second run"), "{body}");
    assert_eq!(runs, 1, "no fresh Run after output");
}

#[tokio::test]
async fn text_then_delayed_write_is_tool_calls_not_truncated_stop() {
    // Live 1f2839e7: grok-high streamed "I'll write the plan" then paused; the 4 s idle
    // ended the turn as stop with no write tool_call. 5 s > 4 s and under the 300 s net.
    let base = mock_paced(vec![
        (0, text_frame("I'll write envelope.json")),
        (
            5_000,
            tool_call_frame(
                "write",
                &[
                    ("path", json!("envelope.json")),
                    ("contents", json!("{\"status\":\"success\"}")),
                ],
                "tool_write",
            ),
        ),
        (0, end_ok()),
    ])
    .await;
    let tmp = tempfile::tempdir().unwrap();
    let (status, _, body) = post(
        app(&base, &tmp, false),
        json!({
            "model": "subs/cursor/auto",
            "messages": [{ "role": "user", "content": "write the envelope" }],
            "tools": write_tool()
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["choices"][0]["finish_reason"], "tool_calls", "{body}");
    assert_eq!(
        v["choices"][0]["message"]["tool_calls"][0]["function"]["name"],
        "write"
    );
}

/// 30 s of quiet after answer text used to be `Finish(Stop)` (the 25 s budget).
/// The knob scales that clock: 4 s stands in for the 300 s safety net, and 2 s
/// stands in for the 30 s gap (2/4 of the net, as 30 s is of a net above 25 s).
/// The tool call must finish the turn.
#[tokio::test]
async fn text_then_scaled_thirty_second_quiet_then_a_tool_call_is_not_stop() {
    let base = mock_paced(vec![
        (0, text_frame("Next I'll look up the thickapi tool schemas")),
        (
            2_000,
            tool_call_frame(
                "write",
                &[("path", json!("a.txt")), ("contents", json!("x"))],
                "tool_write",
            ),
        ),
        (0, end_ok()),
    ])
    .await;
    let tmp = tempfile::tempdir().unwrap();
    let (status, _, body) = post(
        app_with_loop_limit(&base, &tmp, false, None, Some(4)),
        json!({
            "model": "subs/cursor/auto",
            "messages": [{ "role": "user", "content": "write the file" }],
            "tools": write_tool()
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(!body.contains("went quiet"), "{body}");
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["choices"][0]["finish_reason"], "tool_calls", "{body}");
    assert_eq!(
        v["choices"][0]["message"]["tool_calls"][0]["function"]["name"],
        "write"
    );
}

#[tokio::test]
async fn reasoning_then_delayed_write_is_not_truncated_by_four_second_idle() {
    let base = mock_paced(vec![
        (0, reasoning_frame("planning the write")),
        (
            5_000,
            tool_call_frame(
                "write",
                &[("path", json!("envelope.json")), ("contents", json!("{}"))],
                "tool_write",
            ),
        ),
        (0, end_ok()),
    ])
    .await;
    let tmp = tempfile::tempdir().unwrap();
    let (status, _, body) = post(
        app(&base, &tmp, false),
        json!({
            "model": "subs/cursor/auto",
            "messages": [{ "role": "user", "content": "write the envelope" }],
            "tools": write_tool()
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["choices"][0]["finish_reason"], "tool_calls", "{body}");
    assert_eq!(
        v["choices"][0]["message"]["tool_calls"][0]["function"]["name"],
        "write"
    );
}

fn chain_tools() -> Value {
    json!([
        {"type":"function","function":{"name":"read","parameters":{"type":"object","properties":{"path":{"type":"string"}}}}},
        {"type":"function","function":{"name":"bash","parameters":{"type":"object","properties":{"command":{"type":"string"}}}}}
    ])
}

fn assert_args_json_string(call: &Value) {
    let args = call["function"]["arguments"]
        .as_str()
        .expect("function.arguments must be a JSON string");
    let parsed: Value = serde_json::from_str(args).unwrap();
    assert!(parsed.is_object(), "{parsed}");
}

fn mcp_args(name: &str, args: &[(&str, Value)], wire_id: &str) -> Vec<u8> {
    let mut mcp = field_str(5, name);
    for (k, v) in args {
        let mut entry = field_str(1, k);
        entry.extend(field_ld(2, &encode_protobuf_value(v)));
        mcp.extend(field_ld(2, &entry));
    }
    mcp.extend(field_str(3, wire_id));
    mcp
}

/// Two f11 MCP calls in one Connect frame so the mapper does not finish after the first.
fn two_tool_call_frame(
    a: (&str, &[(&str, Value)], &str),
    b: (&str, &[(&str, Value)], &str),
) -> Vec<u8> {
    let mut esm = field_ld(11, &mcp_args(a.0, a.1, a.2));
    esm.extend(field_ld(11, &mcp_args(b.0, b.1, b.2)));
    connect_frame(&field_ld(2, &esm), 0).to_vec()
}

#[tokio::test]
async fn parallel_tool_chain_replays_reasoning_details_and_pairs_ids() {
    let (base, script) = mock(vec![
        (
            200,
            vec![
                reasoning_frame("Need both tools."),
                two_tool_call_frame(
                    ("read", &[("path", json!("a"))], "tool_read"),
                    ("bash", &[("command", json!("ls"))], "tool_bash"),
                ),
                end_ok(),
            ],
        ),
        (
            200,
            vec![
                reasoning_frame("One more read."),
                tool_call_frame("read", &[("path", json!("b"))], "tool_read2"),
                end_ok(),
            ],
        ),
        (200, vec![text_frame("done"), end_ok()]),
    ])
    .await;
    let tmp = tempfile::tempdir().unwrap();
    let details = json!([{"type":"reasoning.text","text":"Need both tools."}]);

    let (status, _, body) = post(
        app(&base, &tmp, false),
        json!({
            "model": "cursor/auto",
            "reasoning_effort": "high",
            "messages": [{"role":"user","content":"read a and ls"}],
            "tools": chain_tools()
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let hop1: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(hop1["choices"][0]["finish_reason"], "tool_calls");
    assert!(
        hop1["usage"]["prompt_tokens"].as_u64().unwrap() > 0
            && hop1["usage"]["completion_tokens"].as_u64().unwrap() > 0,
        "{}",
        hop1["usage"]
    );
    let calls = hop1["choices"][0]["message"]["tool_calls"]
        .as_array()
        .unwrap();
    assert_eq!(calls.len(), 2, "{calls:?}");
    assert_eq!(calls[0]["function"]["name"], "read");
    assert_eq!(calls[1]["function"]["name"], "bash");
    assert_args_json_string(&calls[0]);
    assert_args_json_string(&calls[1]);
    let id_read = calls[0]["id"].as_str().unwrap().to_string();
    let id_bash = calls[1]["id"].as_str().unwrap().to_string();

    let (status, _, body) = post(
        app(&base, &tmp, false),
        json!({
            "model": "cursor/auto",
            "reasoning_effort": "high",
            "messages": [
                {"role":"user","content":"read a and ls"},
                {"role":"assistant","content":null,
                 "tool_calls":[
                    {"id":id_read,"type":"function","function":{"name":"read","arguments":"{\"path\":\"a\"}"}},
                    {"id":id_bash,"type":"function","function":{"name":"bash","arguments":"{\"command\":\"ls\"}"}}
                 ],
                 "reasoning_details": details
                },
                {"role":"tool","tool_call_id":id_read,"content":"file a"},
                {"role":"tool","tool_call_id":id_bash,"content":"ls out"}
            ],
            "tools": chain_tools()
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let hop2: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(hop2["choices"][0]["finish_reason"], "tool_calls");
    let hop2_calls = hop2["choices"][0]["message"]["tool_calls"]
        .as_array()
        .unwrap();
    assert_eq!(hop2_calls.len(), 1);
    assert_args_json_string(&hop2_calls[0]);
    let id_read2 = hop2_calls[0]["id"].as_str().unwrap().to_string();
    {
        let seen = script.seen.lock().unwrap();
        assert_eq!(seen.len(), 2);
        let hay = String::from_utf8_lossy(&seen[1].1);
        assert!(
            hay.contains(&format!("<tool_call id=\"{id_read}\" name=\"read\">")),
            "{hay}"
        );
        assert!(
            hay.contains(&format!("<tool_call id=\"{id_bash}\" name=\"bash\">")),
            "{hay}"
        );
        assert!(
            hay.contains(&format!("<tool_result tool_call_id=\"{id_read}\">")),
            "{hay}"
        );
        assert!(
            hay.contains(&format!("<tool_result tool_call_id=\"{id_bash}\">")),
            "{hay}"
        );
    }

    let (status, _, body) = post(
        app(&base, &tmp, false),
        json!({
            "model": "cursor/auto",
            "messages": [
                {"role":"user","content":"read a and ls"},
                {"role":"assistant","content":null,
                 "tool_calls":[
                    {"id":id_read,"type":"function","function":{"name":"read","arguments":"{\"path\":\"a\"}"}},
                    {"id":id_bash,"type":"function","function":{"name":"bash","arguments":"{\"command\":\"ls\"}"}}
                 ],
                 "reasoning_details": details
                },
                {"role":"tool","tool_call_id":id_read,"content":"file a"},
                {"role":"tool","tool_call_id":id_bash,"content":"ls out"},
                {"role":"assistant","content":null,
                 "tool_calls":[{"id":id_read2,"type":"function","function":{"name":"read","arguments":"{\"path\":\"b\"}"}}],
                 "reasoning_details":[{"type":"reasoning.text","text":"One more read."}]
                },
                {"role":"tool","tool_call_id":id_read2,"content":"file b"}
            ],
            "tools": chain_tools()
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let hop3: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(hop3["choices"][0]["finish_reason"], "stop");
    assert_eq!(hop3["choices"][0]["message"]["content"], "done");
    assert!(hop3["usage"]["prompt_tokens"].as_u64().unwrap() > 0);
    let seen = script.seen.lock().unwrap();
    assert_eq!(seen.len(), 3);
    let hay = String::from_utf8_lossy(&seen[2].1);
    assert!(
        hay.contains(&format!("<tool_result tool_call_id=\"{id_read2}\">")),
        "{hay}"
    );
}

/// G3 (#183): a history of `pairs` assistant `read` calls, each answered by `result(i)`.
fn read_loop_history(pairs: usize, result: impl Fn(usize) -> String) -> Value {
    let mut messages = vec![json!({"role": "user", "content": "read the file"})];
    for i in 0..pairs {
        messages.push(
            json!({"role": "assistant", "content": null, "tool_calls": [{
                "id": format!("call_{i}"), "type": "function",
                "function": {"name": "read", "arguments": "{\"path\":\"src/missing.rs\"}"}
            }]}),
        );
        messages.push(
            json!({"role": "tool", "tool_call_id": format!("call_{i}"), "content": result(i)}),
        );
    }
    json!({ "model": "cursor/auto", "messages": messages, "tools": [{"type": "function", "function": {
        "name": "read", "description": "read a file",
        "parameters": {"type": "object", "properties": {"path": {"type": "string"}}}
    }}]})
}

fn enoent(_: usize) -> String {
    "ENOENT: no such file or directory, open 'src/missing.rs'".to_string()
}

#[tokio::test]
async fn three_identical_read_enoent_pairs_are_502_builtin_loop_before_any_run() {
    let (base, script) = mock(vec![(200, vec![text_frame("never"), end_ok()])]).await;
    let tmp = tempfile::tempdir().unwrap();
    let router = app(&base, &tmp, false);
    let (status, headers, body) = post(router.clone(), read_loop_history(3, enoent)).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY, "{body}");
    assert_eq!(headers.get("x-should-retry").unwrap(), "false");
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["error"]["type"], "vendor_drift", "{body}");
    assert_eq!(v["error"]["code"], "builtin_loop");
    let message = v["error"]["message"].as_str().unwrap();
    // The route appends its own `[code=...]` hint; the sentence before it is exact.
    assert!(
        message.starts_with(
            "cursor is looping: `read` was called with the same arguments (src/missing.rs) and got the same result 3 times in a row; refused before opening a Run"
        ),
        "{message}"
    );
    assert!(!message.contains("  "), "{message}");
    assert!(script.seen.lock().unwrap().is_empty(), "no Run was opened");
    let ready = get_ready(router).await;
    let oauth = &ready["thin"]["vendors"]["cursor"]["oauth"];
    assert_eq!(
        oauth["last_row_rejection"]["code"], "builtin_loop",
        "{ready}"
    );
    assert_eq!(oauth["last_row_rejection"]["status"], 502);
    assert!(
        oauth.get("last_rejection").is_none_or(Value::is_null),
        "{oauth}"
    );
}

#[tokio::test]
async fn two_identical_pairs_still_go_upstream() {
    let (base, script) = mock(vec![(200, vec![text_frame("ok"), end_ok()])]).await;
    let tmp = tempfile::tempdir().unwrap();
    let (status, _, body) = post(app(&base, &tmp, false), read_loop_history(2, enoent)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(script.seen.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn three_identical_calls_with_different_results_go_upstream() {
    let (base, script) = mock(vec![(200, vec![text_frame("ok"), end_ok()])]).await;
    let tmp = tempfile::tempdir().unwrap();
    let (status, _, body) = post(
        app(&base, &tmp, false),
        read_loop_history(3, |i| format!("file contents, revision {i}")),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(script.seen.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn builtin_loop_limit_zero_turns_the_guard_off() {
    let (base, script) = mock(vec![(200, vec![text_frame("ok"), end_ok()])]).await;
    let tmp = tempfile::tempdir().unwrap();
    let (status, _, body) = post(
        app_with_loop_limit(&base, &tmp, false, Some(0), None),
        read_loop_history(5, enoent),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(script.seen.lock().unwrap().len(), 1);
}

/// `f2.<field>` built-in exec with these args (which carry the tool-call id).
fn builtin_frame(field: u64, args: &[u8]) -> Vec<u8> {
    connect_frame(&field_ld(2, &field_ld(field, args)), 0).to_vec()
}

/// One built-in of every G2 row (SPEC §19.8): `(label, field, args)`. The glob-only grep
/// is the 29 Sep smoke's; the grep pattern carries a quote.
fn g2_rows() -> Vec<(&'static str, u64, Vec<u8>)> {
    let with_id = |mut args: Vec<u8>, id_field: u64, id: &str| {
        args.extend(field_str(id_field, id));
        args
    };
    let mut grep = field_str(1, "it's");
    grep.extend(field_str(2, "src"));
    grep.extend(field_str(3, "*.rs"));
    let mut glob = field_str(3, "**/NOTES.md");
    glob.extend(field_str(4, "files_with_matches"));
    let mut write = field_str(1, "a.txt");
    write.extend(field_str(2, "hi"));
    vec![
        (
            "read",
            7,
            with_id(field_str(1, "src/a b.rs"), 2, "toolu_01"),
        ),
        ("grep", 5, with_id(grep, 14, "tool_grep")),
        ("glob", 5, with_id(glob, 14, "call-ca92f921-0\nfc_abbf")),
        ("ls", 8, with_id(field_str(1, "src"), 3, "tool_ls")),
        ("write", 3, with_id(write, 3, "tool_w")),
        ("shell", 2, with_id(field_str(1, "bun test"), 4, "tool_sh")),
        (
            "shell_stream",
            14,
            with_id(field_str(1, "bun test"), 4, "tool_sh"),
        ),
    ]
}

/// Function tools with these names, as Pi declares them.
fn declare(names: &[&str]) -> Value {
    Value::Array(
        names
            .iter()
            .map(|name| {
                json!({ "type": "function", "function": {
                    "name": name,
                    "description": "a Pi tool",
                    "parameters": { "type": "object", "properties": {} }
                }})
            })
            .collect(),
    )
}

/// G2 (#182) through the real router: every row with its own tool declared, with only
/// `bash`, and with neither. A mapped built-in is that declared call; a denied one emits
/// nothing and the Run goes on to the declared MCP tool the model falls back to. No tool
/// name outside the declared set ever reaches the client.
#[tokio::test]
async fn builtins_map_only_onto_declared_tools() {
    let all: &[&str] = &["read", "grep", "find", "ls", "write", "bash"];
    let bash_only: &[&str] = &["bash"];
    let neither: &[&str] = &["edit"];
    // (row, [its own tool, only bash, neither]) → the tool name, or None for a denial;
    // plus the bash-only command.
    type Row<'a> = (&'a str, [Option<&'a str>; 3], Option<&'a str>);
    let expected: &[Row] = &[
        (
            "read",
            [Some("read"), Some("bash"), None],
            Some("cat -- 'src/a b.rs'"),
        ),
        (
            "grep",
            [Some("grep"), Some("bash"), None],
            Some(r"grep -rnE --include='*.rs' -- 'it'\''s' 'src'"),
        ),
        (
            "glob",
            [Some("find"), Some("bash"), None],
            Some("find '.' -name 'NOTES.md'"),
        ),
        (
            "ls",
            [Some("ls"), Some("bash"), None],
            Some("ls -la -- 'src'"),
        ),
        ("write", [Some("write"), None, None], None),
        (
            "shell",
            [Some("bash"), Some("bash"), None],
            Some("bun test"),
        ),
        (
            "shell_stream",
            [Some("bash"), Some("bash"), None],
            Some("bun test"),
        ),
    ];
    for ((label, field, args), (want_label, want, bash_command)) in
        g2_rows().into_iter().zip(expected)
    {
        assert_eq!(label, *want_label);
        for (set, names) in [all, bash_only, neither].into_iter().enumerate() {
            // A denied built-in is followed by the model's fallback to a declared MCP tool.
            let fallback = names[0];
            let (base, _) = mock(vec![(
                200,
                vec![
                    builtin_frame(field, &args),
                    tool_call_frame(fallback, &[("path", json!("a.txt"))], "tool_mcp"),
                    end_ok(),
                ],
            )])
            .await;
            let tmp = tempfile::tempdir().unwrap();
            // The read row's target exists: G2's table decides, not G1's missing-file answer.
            std::fs::create_dir_all(tmp.path().join("src")).unwrap();
            std::fs::write(tmp.path().join("src").join("a b.rs"), "x").unwrap();
            let (status, _, body) = post(
                app(&base, &tmp, false),
                json!({
                    "model": "cursor/auto",
                    "messages": [{ "role": "user", "content": "go" }],
                    "tools": declare(names)
                }),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{label} {names:?}: {body}");
            let v: Value = serde_json::from_str(&body).unwrap();
            assert_eq!(v["choices"][0]["finish_reason"], "tool_calls", "{body}");
            let calls = v["choices"][0]["message"]["tool_calls"].as_array().unwrap();
            assert_eq!(calls.len(), 1, "{label} {names:?}: {body}");
            let name = calls[0]["function"]["name"].as_str().unwrap();
            assert!(
                names.contains(&name),
                "{label}: {name} is outside {names:?}"
            );
            assert_eq!(
                name,
                want[set].unwrap_or(fallback),
                "{label} {names:?}: {body}"
            );
            if set == 1 {
                if let Some(command) = bash_command {
                    let arguments: Value =
                        serde_json::from_str(calls[0]["function"]["arguments"].as_str().unwrap())
                            .unwrap();
                    assert_eq!(arguments, json!({ "command": command }), "{label}");
                }
            }
        }
    }
}

/// The 29 Sep smoke (`subs/cursor/grok-4.7:high`, draftr builder, `find` x30): a glob-only
/// grep on a seat that runs Pi's default tools is `bash find`, never an undeclared `find`.
#[tokio::test]
async fn glob_only_grep_on_a_default_pi_seat_is_bash_find() {
    let (_, field, args) = g2_rows()
        .into_iter()
        .find(|(label, ..)| *label == "glob")
        .unwrap();
    let (base, _) = mock(vec![(200, vec![builtin_frame(field, &args), end_ok()])]).await;
    let tmp = tempfile::tempdir().unwrap();
    let (status, _, body) = post(
        app(&base, &tmp, false),
        json!({
            "model": "cursor/auto",
            "messages": [{ "role": "user", "content": "find NOTES.md" }],
            "tools": declare(&["read", "bash", "edit", "write"])
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let v: Value = serde_json::from_str(&body).unwrap();
    let call = &v["choices"][0]["message"]["tool_calls"][0]["function"];
    assert_eq!(call["name"], "bash", "{body}");
    assert_eq!(
        call["arguments"],
        r#"{"command":"find '.' -name 'NOTES.md'"}"#
    );
}

/// Top-level protobuf fields `(field, wire type, bytes, varint)`; stops at a wire type it
/// does not read.
fn pb_fields(mut buf: &[u8]) -> Vec<(u64, u8, Vec<u8>, u64)> {
    fn varint(buf: &mut &[u8]) -> Option<u64> {
        let mut out = 0u64;
        for shift in (0..64).step_by(7) {
            let (&b, rest) = buf.split_first()?;
            *buf = rest;
            out |= u64::from(b & 0x7f) << shift;
            if b & 0x80 == 0 {
                return Some(out);
            }
        }
        None
    }
    let mut out = Vec::new();
    while !buf.is_empty() {
        let Some(key) = varint(&mut buf) else { break };
        let (field, wire) = (key >> 3, (key & 7) as u8);
        match wire {
            0 => {
                let Some(v) = varint(&mut buf) else { break };
                out.push((field, wire, Vec::new(), v));
            }
            2 => {
                let Some(n) = varint(&mut buf) else { break };
                let n = n as usize;
                if n > buf.len() {
                    break;
                }
                let (data, rest) = buf.split_at(n);
                out.push((field, wire, data.to_vec(), 0));
                buf = rest;
            }
            _ => break,
        }
    }
    out
}

/// `exec_client_message.read_result` in a client frame: `(exec id, exec_id, ReadResult
/// oneof field, that variant's bytes)`.
type ReadReply = (u64, String, u64, Vec<u8>);

fn read_result_of(payload: &[u8]) -> Option<ReadReply> {
    let (_, _, esm, _) = pb_fields(payload)
        .into_iter()
        .find(|f| f.0 == 2 && f.1 == 2)?;
    let mut id = 0;
    let mut exec_id = String::new();
    let mut result = None;
    for (field, _, data, v) in pb_fields(&esm) {
        match field {
            1 => id = v,
            15 => exec_id = String::from_utf8_lossy(&data).into_owned(),
            7 => result = Some(data),
            _ => {}
        }
    }
    let (variant, _, data, _) = pb_fields(&result?).into_iter().next()?;
    Some((id, exec_id, variant, data))
}

/// `ExecServerMessage { id, exec_id, <field>: args }` as one Connect frame.
fn exec_frame(id: u64, exec_id: &str, field: u64, args: &[u8]) -> Vec<u8> {
    let mut esm = field_varint(1, id);
    esm.extend(field_str(15, exec_id));
    esm.extend(field_ld(field, args));
    connect_frame(&field_ld(2, &esm), 0).to_vec()
}

/// F1's measured create (#181, `cursor-agent 2026.09.28-64d2043`): Cursor's edit built-in
/// pre-reads its target (`f7 read_args`, exec id 1), waits for the `ReadResult`, and only
/// after `file_not_found` sends `f3 write_args` (exec id 2, the same `toolu_` id). The stand-in
/// trailers `internal: No exec result` when the answer does not come, and ends the edit with
/// no write on any other answer (as it did on `rejected`).
async fn run_edit_create(body: Body, target: String, seen: Arc<Mutex<Vec<ReadReply>>>) -> Response {
    use futures_util::StreamExt;
    let mut stream = body.into_data_stream();
    let (got_tx, mut got_rx) = tokio::sync::mpsc::unbounded_channel::<u64>();
    tokio::spawn(async move {
        let mut decoder = FrameDecoder::default();
        while let Some(chunk) = stream.next().await {
            let Ok(chunk) = chunk else {
                continue;
            };
            for f in decoder.push(&chunk).unwrap_or_default() {
                if let Some(reply) = read_result_of(&f.payload) {
                    let variant = reply.2;
                    seen.lock().unwrap().push(reply);
                    let _ = got_tx.send(variant);
                }
            }
        }
    });
    let out = async_stream::stream! {
        let mut read = field_str(1, &target);
        read.extend(field_str(2, "toolu_01Create"));
        yield Ok::<_, std::io::Error>(axum::body::Bytes::from(exec_frame(1, "exec-read", 7, &read)));
        match tokio::time::timeout(Duration::from_secs(2), got_rx.recv()).await {
            Ok(Some(4)) => {
                let mut write = field_str(1, &target);
                write.extend(field_str(2, "ok\n"));
                write.extend(field_str(3, "toolu_01Create"));
                yield Ok(axum::body::Bytes::from(exec_frame(2, "exec-write", 3, &write)));
                yield Ok(axum::body::Bytes::from(end_ok()));
            }
            Ok(Some(_)) => {
                yield Ok(axum::body::Bytes::from(text_frame("The write operation was rejected")));
                yield Ok(axum::body::Bytes::from(end_ok()));
            }
            _ => yield Ok(axum::body::Bytes::from(end_err("internal", "No exec result"))),
        }
    };
    let mut response = Response::new(Body::from_stream(out));
    *response.status_mut() = StatusCode::OK;
    response
        .headers_mut()
        .insert("content-type", "application/connect+proto".parse().unwrap());
    response
}

/// G1 (#175) through the real router: a pre-read of a missing file is answered on the
/// duplex with `ReadResult.file_not_found { path }` (echoing the exec id and exec_id), no
/// `read` reaches Pi, and the write Cursor sends next is one Pi `write` with its path and
/// content. Before G1 the turn ended on the read and Pi's ENOENT text restarted the edit.
#[tokio::test]
async fn builtin_pre_read_of_a_missing_file_is_answered_then_the_write_is_pi_write() {
    let tmp = tempfile::tempdir().unwrap();
    let target = tmp
        .path()
        .join("src")
        .join("smoke-marker.txt")
        .to_string_lossy()
        .into_owned();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let app_seen = seen.clone();
    let app_target = target.clone();
    let stand_in = Router::new().route(
        "/agent.v1.AgentService/Run",
        axum::routing::post(move |body: Body| {
            let (target, seen) = (app_target.clone(), app_seen.clone());
            async move { run_edit_create(body, target, seen).await }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr: SocketAddr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, stand_in).await.unwrap() });

    let (status, _, body) = post(
        app(&format!("http://{addr}"), &tmp, false),
        json!({
            "model": "cursor/auto",
            "messages": [{ "role": "user", "content": "Create src/smoke-marker.txt containing ok" }],
            "tools": declare(&["read", "bash", "edit", "write"])
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["choices"][0]["finish_reason"], "tool_calls", "{body}");
    let calls = v["choices"][0]["message"]["tool_calls"].as_array().unwrap();
    assert_eq!(calls.len(), 1, "{body}");
    assert_eq!(calls[0]["function"]["name"], "write", "{body}");
    let arguments: Value =
        serde_json::from_str(calls[0]["function"]["arguments"].as_str().unwrap()).unwrap();
    assert_eq!(arguments, json!({ "path": target, "content": "ok\n" }));

    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 1, "one ReadResult on the duplex: {seen:?}");
    let (id, exec_id, variant, data) = &seen[0];
    assert_eq!((*id, exec_id.as_str()), (1, "exec-read"));
    assert_eq!(*variant, 4, "ReadResult.file_not_found, not rejected (3)");
    let path: Vec<Vec<u8>> = pb_fields(data)
        .into_iter()
        .filter(|f| f.0 == 1)
        .map(|f| f.2)
        .collect();
    assert_eq!(path, [target.as_bytes().to_vec()], "ReadFileNotFound.path");
    assert!(
        !std::path::Path::new(&target).exists(),
        "nothing is written in the daemon"
    );
}
