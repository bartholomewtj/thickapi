#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
//! Antigravity thin lane end to end (SPEC §19.8, T3): real router, real `AgyAdapter`, Google's
//! Code Assist API replaced by a local stand-in speaking the envelope and SSE shape measured
//! live on 2026-09-17 (`agy 1.2.4`, `daily-cloudcode-pa.googleapis.com`).

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

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
use thickapi_core::config::router::{AgyCfg, RouterConfig, ThinConfig, Vendors};
use thickapi_core::config::{AccountsConfig, CombosConfig};
use thickapi_core::types::BackendId;
use thickapi_core::{ConfigBundle, Store};
use tower::ServiceExt;

const TOKEN: &str = "ta_agy_e2e";

#[derive(Clone)]
struct Script {
    replies: Arc<Mutex<Vec<(u16, String)>>>,
    seen: Arc<Mutex<Vec<(HeaderMap, Value)>>>,
    load_calls: Arc<Mutex<u32>>,
}

async fn load_code_assist(State(s): State<Script>, headers: HeaderMap, body: String) -> Response {
    *s.load_calls.lock().unwrap() += 1;
    // Without the Antigravity client headers the real endpoint answers UNSUPPORTED_CLIENT.
    let cm = headers
        .get("client-metadata")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if !cm.contains("ANTIGRAVITY") {
        return (
            StatusCode::OK,
            r#"{"allowedTiers":[],"ineligibleTiers":[{"reasonCode":"UNSUPPORTED_CLIENT","tierId":"free-tier"}]}"#,
        )
            .into_response();
    }
    let _: Value = serde_json::from_str(&body).unwrap();
    (
        StatusCode::OK,
        r#"{"currentTier":{"id":"free-tier","name":"Antigravity"},"cloudaicompanionProject":"aicode-consumers","gcpManaged":false}"#,
    )
        .into_response()
}

async fn stream_generate(State(s): State<Script>, headers: HeaderMap, body: String) -> Response {
    s.seen
        .lock()
        .unwrap()
        .push((headers, serde_json::from_str(&body).unwrap()));
    let (status, text) = {
        let mut r = s.replies.lock().unwrap();
        if r.is_empty() {
            (
                500,
                r#"{"error":{"code":500,"message":"exhausted","status":"INTERNAL"}}"#.to_string(),
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
    response
}

fn sse(chunks: &[Value]) -> String {
    chunks.iter().map(|d| format!("data: {d}\n\n")).collect()
}

fn resp(parts: Value, finish: Option<&str>, usage: Value) -> Value {
    let mut candidate = json!({ "content": { "role": "model", "parts": parts } });
    if let Some(f) = finish {
        candidate["finishReason"] = json!(f);
    }
    json!({ "response": { "candidates": [candidate], "usageMetadata": usage,
        "modelVersion": "gemini-3.8-flash-tiered", "responseId": "r1" }, "traceId": "t1", "metadata": {} })
}

fn tool_reply() -> (u16, String) {
    (
        200,
        sse(&[
            resp(
                json!([{ "thought": true, "text": "Need the tool." }]),
                None,
                json!({ "promptTokenCount": 58, "totalTokenCount": 58 }),
            ),
            resp(
                json!([{ "thoughtSignature": "SIG-BRISBANE", "functionCall": { "name": "get_weather", "args": { "city": "Brisbane" }, "id": "call_164112" } }]),
                None,
                json!({ "promptTokenCount": 58, "candidatesTokenCount": 17, "thoughtsTokenCount": 5, "totalTokenCount": 80 }),
            ),
            resp(
                json!([{ "text": "" }]),
                Some("STOP"),
                json!({ "promptTokenCount": 58, "candidatesTokenCount": 17, "thoughtsTokenCount": 5, "totalTokenCount": 80 }),
            ),
        ]),
    )
}

fn text_reply(text: &str) -> (u16, String) {
    (
        200,
        sse(&[
            resp(
                json!([{ "text": text }]),
                None,
                json!({ "promptTokenCount": 92, "candidatesTokenCount": 4, "totalTokenCount": 96 }),
            ),
            resp(
                json!([{ "thoughtSignature": "SIG-END", "text": "" }]),
                Some("STOP"),
                json!({ "promptTokenCount": 92, "candidatesTokenCount": 7, "totalTokenCount": 99, "cachedContentTokenCount": 40 }),
            ),
        ]),
    )
}

async fn mock(replies: Vec<(u16, String)>) -> (String, Script) {
    let script = Script {
        replies: Arc::new(Mutex::new(replies)),
        seen: Arc::new(Mutex::new(vec![])),
        load_calls: Arc::new(Mutex::new(0)),
    };
    let app = Router::new()
        .route(
            "/v1internal:loadCodeAssist",
            axum::routing::post(load_code_assist),
        )
        .route(
            "/v1internal:streamGenerateContent",
            axum::routing::post(stream_generate),
        )
        .with_state(script.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr: SocketAddr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}"), script)
}

fn app(base_url: &str, tmp: &tempfile::TempDir, expired: bool) -> Router {
    let creds = tmp.path().join("antigravity-oauth-token");
    let expiry = if expired {
        "2000-01-01T00:00:00Z"
    } else {
        "2999-01-01T00:00:00.000000+00:00"
    };
    std::fs::write(
        &creds,
        json!({
            "token": { "access_token": "ya29.e2e.agy", "token_type": "Bearer", "refresh_token": "1//r", "expiry": expiry },
            "auth_method": "consumer",
            "id_token": "h.e30.s"
        })
        .to_string(),
    )
    .unwrap();
    let config = ConfigBundle {
        config_dir: tmp.path().to_path_buf(),
        router: RouterConfig {
            data_dir: tmp.path().join("data"),
            thin: ThinConfig {
                vendors: Vendors {
                    agy: AgyCfg {
                        base_url: Some(base_url.to_string()),
                        credentials_path: Some(creds),
                        client_version: Some("1.2.4".into()),
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
                id: "subs/agy/gemini-3.8-flash".into(),
                backend: BackendId::Agy,
                cli_model: "gemini-3.8-flash-high".into(),
                context_window: 1_000_000,
                max_tokens: 65_536,
                thinking: true,
                cost: 0.0,
                name: "Gemini 3.8 Flash".into(),
                provider: None,
                transport: Transport::OauthDirect,
                vendor_model: Some("gemini-3.8-flash-tiered".into()),
                enabled: true,
                input: vec!["text".into()],
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

#[tokio::test]
async fn tool_turn_then_replay_carries_gemini_id_and_signature() {
    let (base, script) = mock(vec![tool_reply(), text_reply("Sunny, 27°C.")]).await;
    let tmp = tempfile::tempdir().unwrap();
    let app = app(&base, &tmp, false);

    // Turn 1: model calls the tool.
    let (status, _, body) = post(
        app.clone(),
        json!({
            "model": "agy/gemini-3.8-flash",
            "reasoning_effort": "max",
            "messages": [
                {"role": "system", "content": "You are terse."},
                {"role": "user", "content": "Weather in Brisbane?"}
            ],
            "tools": [{"type":"function","function":{"name":"get_weather","description":"w","parameters":{
                "$schema":"http://json-schema.org/draft-07/schema#","type":"object",
                "properties":{"city":{"type":"string"}},"required":["city"],"additionalProperties":false}}}]
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["object"], "chat.completion");
    assert_eq!(v["model"], "gemini-3.8-flash-tiered");
    assert_eq!(v["choices"][0]["finish_reason"], "tool_calls");
    assert_eq!(
        v["choices"][0]["message"]["reasoning_content"],
        "Need the tool."
    );
    let call = &v["choices"][0]["message"]["tool_calls"][0];
    assert_eq!(call["function"]["name"], "get_weather");
    assert_eq!(call["function"]["arguments"], "{\"city\":\"Brisbane\"}");
    let call_id = call["id"].as_str().unwrap().to_string();
    assert!(
        call_id.starts_with("call_") && call_id.len() == 29,
        "{call_id}"
    );
    // candidates 17 + thoughts 5
    assert_eq!(v["usage"]["completion_tokens"], 22);
    assert_eq!(v["usage"]["prompt_tokens"], 58);

    {
        let seen = script.seen.lock().unwrap();
        let (headers, sent) = &seen[0];
        assert_eq!(headers.get("authorization").unwrap(), "Bearer ya29.e2e.agy");
        assert!(headers
            .get("user-agent")
            .unwrap()
            .to_str()
            .unwrap()
            .starts_with("antigravity/cli/1.2.4 (aidev_client; "));
        assert_eq!(
            headers.get("x-goog-api-client").unwrap(),
            "google-cloud-sdk vscode_cloudshelleditor/0.1"
        );
        assert!(headers
            .get("client-metadata")
            .unwrap()
            .to_str()
            .unwrap()
            .contains("\"ideType\":\"ANTIGRAVITY\""));
        assert_eq!(sent["project"], "aicode-consumers");
        assert_eq!(sent["model"], "gemini-3.8-flash-tiered");
        assert_eq!(sent["requestType"], "agent");
        assert_eq!(sent["userAgent"], "antigravity");
        assert_eq!(
            sent["request"]["systemInstruction"]["parts"][0]["text"],
            "You are terse."
        );
        assert_eq!(
            sent["request"]["generationConfig"]["thinkingConfig"]["thinkingLevel"],
            "high"
        );
        let params = &sent["request"]["tools"][0]["functionDeclarations"][0]["parameters"];
        assert!(params.get("$schema").is_none(), "{params}");
        assert!(params.get("additionalProperties").is_none(), "{params}");
        assert_eq!(params["properties"]["city"]["type"], "string");
    }
    assert_eq!(*script.load_calls.lock().unwrap(), 1);

    // Turn 2: client replays our call id; the adapter restores Gemini's id and signature.
    let (status, headers, body) = post(
        app,
        json!({
            "model": "agy/gemini-3.8-flash",
            "stream": true,
            "stream_options": {"include_usage": true},
            "messages": [
                {"role": "user", "content": "Weather in Brisbane?"},
                {"role": "assistant", "content": null, "tool_calls": [
                    {"id": call_id, "type": "function", "function": {"name": "get_weather", "arguments": "{\"city\":\"Brisbane\"}"}}
                ]},
                {"role": "tool", "tool_call_id": call_id, "content": "{\"temp_c\":27}"}
            ],
            "tools": [{"type":"function","function":{"name":"get_weather","parameters":{"type":"object","properties":{"city":{"type":"string"}}}}}]
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(headers
        .get("content-type")
        .unwrap()
        .to_str()
        .unwrap()
        .starts_with("text/event-stream"));
    assert!(body.contains("\"content\":\"Sunny, 27°C.\""), "{body}");
    assert!(body.contains("\"finish_reason\":\"stop\""), "{body}");
    assert!(body.contains("\"cached_tokens\":40"), "{body}");
    assert!(body.trim_end().ends_with("data: [DONE]"), "{body}");

    let seen = script.seen.lock().unwrap();
    let (_, sent) = &seen[1];
    let contents = sent["request"]["contents"].as_array().unwrap();
    assert_eq!(contents.len(), 3, "{contents:?}");
    let call_part = &contents[1]["parts"][0];
    assert_eq!(contents[1]["role"], "model");
    assert_eq!(call_part["functionCall"]["id"], "call_164112");
    assert_eq!(call_part["functionCall"]["args"]["city"], "Brisbane");
    assert_eq!(call_part["thoughtSignature"], "SIG-BRISBANE");
    let fr = &contents[2]["parts"][0]["functionResponse"];
    assert_eq!(contents[2]["role"], "user");
    assert_eq!(fr["name"], "get_weather");
    assert_eq!(fr["id"], "call_164112");
    assert_eq!(fr["response"]["result"]["temp_c"], 27);
    // project discovery ran once for both turns
    assert_eq!(*script.load_calls.lock().unwrap(), 1);
}

#[tokio::test]
async fn unknown_call_id_replays_skip_sentinel() {
    let (base, script) = mock(vec![text_reply("ok")]).await;
    let tmp = tempfile::tempdir().unwrap();
    let (status, _, body) = post(
        app(&base, &tmp, false),
        json!({
            "model": "agy/gemini-3.8-flash",
            "messages": [
                {"role": "user", "content": "x"},
                {"role": "assistant", "content": null, "tool_calls": [
                    {"id": "call_from_before_restart", "type": "function", "function": {"name": "f", "arguments": "{}"}}
                ]},
                {"role": "tool", "tool_call_id": "call_from_before_restart", "content": "done"}
            ],
            "tools": [{"type":"function","function":{"name":"f","parameters":{"type":"object","properties":{"a":{"type":"string"}}}}}]
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let seen = script.seen.lock().unwrap();
    let (_, sent) = &seen[0];
    let call_part = &sent["request"]["contents"][1]["parts"][0];
    assert_eq!(
        call_part["thoughtSignature"],
        "skip_thought_signature_validator"
    );
    assert_eq!(call_part["functionCall"]["id"], "call_from_before_restart");
    assert_eq!(
        sent["request"]["contents"][2]["parts"][0]["functionResponse"]["name"],
        "f"
    );
}

#[tokio::test]
async fn off_effort_sends_zero_budget_and_google_400_maps_to_invalid_request() {
    let (base, script) = mock(vec![(
        400,
        r#"{"error":{"code":400,"message":"Thinking level MINIMAL is not supported for this model.","status":"INVALID_ARGUMENT"}}"#.into(),
    )])
    .await;
    let tmp = tempfile::tempdir().unwrap();
    let (status, _, body) = post(
        app(&base, &tmp, false),
        json!({ "model": "agy/gemini-3.8-flash", "reasoning_effort": "off",
                "messages": [{"role": "user", "content": "x"}] }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["error"]["vendor"], "agy");
    assert!(v["error"]["message"]
        .as_str()
        .unwrap()
        .contains("Thinking level"));
    let seen = script.seen.lock().unwrap();
    assert_eq!(
        seen[0].1["request"]["generationConfig"]["thinkingConfig"],
        json!({ "thinkingBudget": 0 })
    );
}

#[tokio::test]
async fn resource_exhausted_maps_to_429_with_retry_delay() {
    let (base, _) = mock(vec![(
        429,
        r#"{"error":{"code":429,"message":"Quota exceeded for model","status":"RESOURCE_EXHAUSTED","details":[{"@type":"type.googleapis.com/google.rpc.RetryInfo","retryDelay":"7s"}]}}"#.into(),
    )])
    .await;
    let tmp = tempfile::tempdir().unwrap();
    let (status, headers, body) = post(
        app(&base, &tmp, false),
        json!({ "model": "agy/gemini-3.8-flash", "messages": [{"role": "user", "content": "x"}] }),
    )
    .await;
    // A 7 s wait is a throttle, whatever the wording: Pi must retry, not drop the seat.
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{body}");
    assert_eq!(headers.get("retry-after").unwrap(), "7");
    assert_eq!(headers.get("x-should-retry").unwrap(), "true");
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["error"]["code"], "rate_limit_error");
    assert!(!body.contains("quota exceeded"), "{body}");
}

/// The plan running out names a long wait (or none): that one is terminal.
#[tokio::test]
async fn an_exhausted_plan_is_402_and_not_retryable() {
    let (base, _) = mock(vec![(
        429,
        r#"{"error":{"code":429,"message":"Individual quota reached. Resets in 65h32m31s","status":"RESOURCE_EXHAUSTED","details":[{"@type":"type.googleapis.com/google.rpc.RetryInfo","retryDelay":"235951s"}]}}"#.into(),
    )])
    .await;
    let tmp = tempfile::tempdir().unwrap();
    let (status, headers, body) = post(
        app(&base, &tmp, false),
        json!({ "model": "agy/gemini-3.8-flash", "messages": [{"role": "user", "content": "x"}] }),
    )
    .await;
    assert_eq!(status, StatusCode::PAYMENT_REQUIRED, "{body}");
    assert_eq!(headers.get("x-should-retry").unwrap(), "false");
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["error"]["code"], "insufficient_quota");
    assert!(
        v["error"]["message"]
            .as_str()
            .unwrap()
            .starts_with("quota exceeded"),
        "{body}"
    );
}

#[tokio::test]
async fn expired_token_is_401_login_required_without_calling_google() {
    let (base, script) = mock(vec![text_reply("never")]).await;
    let tmp = tempfile::tempdir().unwrap();
    let (status, _, body) = post(
        app(&base, &tmp, true),
        json!({ "model": "agy/gemini-3.8-flash", "messages": [{"role": "user", "content": "x"}] }),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["error"]["code"], "vendor_login_required");
    assert_eq!(v["error"]["vendor"], "agy");
    assert!(script.seen.lock().unwrap().is_empty());
    assert_eq!(*script.load_calls.lock().unwrap(), 0);
}

#[tokio::test]
async fn ready_reports_agy_vendor() {
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
    let agy = &v["thin"]["vendors"]["agy"];
    assert_eq!(agy["path"], "oauth");
    assert_eq!(agy["token"]["present"], true);
    assert_eq!(agy["token"]["expired"], false);
    assert_eq!(agy["token"]["note"], "auth_method=consumer");
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

fn two_parallel_tools_reply() -> (u16, String) {
    (
        200,
        sse(&[
            resp(
                json!([{ "thought": true, "text": "Need both tools." }]),
                None,
                json!({ "promptTokenCount": 60, "totalTokenCount": 60 }),
            ),
            resp(
                json!([
                    { "thoughtSignature": "SIG-READ", "functionCall": { "name": "read", "args": { "path": "a" }, "id": "call_read_h9" } },
                    { "thoughtSignature": "SIG-BASH", "functionCall": { "name": "bash", "args": { "command": "ls" }, "id": "call_bash_h9" } }
                ]),
                Some("STOP"),
                json!({ "promptTokenCount": 60, "candidatesTokenCount": 20, "thoughtsTokenCount": 6, "totalTokenCount": 86 }),
            ),
        ]),
    )
}

fn one_more_tool_reply() -> (u16, String) {
    (
        200,
        sse(&[
            resp(
                json!([{ "thought": true, "text": "One more read." }]),
                None,
                json!({ "promptTokenCount": 80, "totalTokenCount": 80 }),
            ),
            resp(
                json!([{ "thoughtSignature": "SIG-READ2", "functionCall": { "name": "read", "args": { "path": "b" }, "id": "call_read2_h9" } }]),
                Some("STOP"),
                json!({ "promptTokenCount": 80, "candidatesTokenCount": 10, "thoughtsTokenCount": 4, "totalTokenCount": 94 }),
            ),
        ]),
    )
}

#[tokio::test]
async fn parallel_tool_chain_replays_reasoning_details_and_pairs_ids() {
    let (base, script) = mock(vec![
        two_parallel_tools_reply(),
        one_more_tool_reply(),
        text_reply("done"),
    ])
    .await;
    let tmp = tempfile::tempdir().unwrap();
    let router = app(&base, &tmp, false);
    let details = json!([{"type":"reasoning.text","text":"Need both tools."}]);

    let (status, _, body) = post(
        router.clone(),
        json!({
            "model": "agy/gemini-3.8-flash",
            "reasoning_effort": "high",
            "messages": [{"role":"user","content":"read a and ls"}],
            "tools": chain_tools()
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let hop1: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(hop1["choices"][0]["finish_reason"], "tool_calls");
    assert!(hop1["usage"]["prompt_tokens"].as_u64().unwrap() > 0);
    assert!(hop1["usage"]["completion_tokens"].as_u64().unwrap() > 0);
    let calls = hop1["choices"][0]["message"]["tool_calls"]
        .as_array()
        .unwrap();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0]["function"]["name"], "read");
    assert_eq!(calls[1]["function"]["name"], "bash");
    assert_args_json_string(&calls[0]);
    assert_args_json_string(&calls[1]);
    let id_read = calls[0]["id"].as_str().unwrap().to_string();
    let id_bash = calls[1]["id"].as_str().unwrap().to_string();

    let (status, _, body) = post(
        router.clone(),
        json!({
            "model": "agy/gemini-3.8-flash",
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
                {"role":"tool","tool_call_id":id_bash,"content":"a\\nb"}
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
        let contents = seen[1].1["request"]["contents"].as_array().unwrap();
        let model = contents.iter().find(|c| c["role"] == "model").unwrap();
        let fcs: Vec<&Value> = model["parts"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|p| p.get("functionCall").is_some())
            .collect();
        assert_eq!(fcs.len(), 2, "{model}");
        assert_eq!(fcs[0]["functionCall"]["id"], "call_read_h9");
        assert_eq!(fcs[1]["functionCall"]["id"], "call_bash_h9");
        let responses: Vec<&Value> = contents
            .iter()
            .filter(|c| c["role"] == "user")
            .flat_map(|c| c["parts"].as_array().into_iter().flatten())
            .filter(|p| p.get("functionResponse").is_some())
            .collect();
        assert_eq!(responses.len(), 2, "{contents:?}");
        let ids: Vec<&str> = responses
            .iter()
            .filter_map(|p| p["functionResponse"]["id"].as_str())
            .collect();
        assert!(ids.contains(&"call_read_h9"), "{ids:?}");
        assert!(ids.contains(&"call_bash_h9"), "{ids:?}");
    }

    let (status, _, body) = post(
        router,
        json!({
            "model": "agy/gemini-3.8-flash",
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
    let responses: Vec<&str> = seen[2].1["request"]["contents"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|c| c["role"] == "user")
        .flat_map(|c| c["parts"].as_array().into_iter().flatten())
        .filter_map(|p| p["functionResponse"]["id"].as_str())
        .collect();
    assert!(
        responses.contains(&"call_read2_h9"),
        "hop3 must pair the third Gemini id: {responses:?}"
    );
}
