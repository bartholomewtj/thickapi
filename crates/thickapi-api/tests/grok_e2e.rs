#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
//! Grok thin lane end to end (SPEC §19.8, T2): real router, real `GrokAdapter`, xAI replaced
//! by a local stand-in speaking the OpenAI chunk format measured live on 2026-09-17.

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
use thickapi_core::config::router::{GrokCfg, RouterConfig, ThinConfig};
use thickapi_core::config::{AccountsConfig, CombosConfig};
use thickapi_core::types::BackendId;
use thickapi_core::{ConfigBundle, Store};
use tower::ServiceExt;

const TOKEN: &str = "ta_grok_e2e";

/// One scripted upstream reply: HTTP status, body text, and extra `(header, value)` pairs.
type Reply = (u16, String, Vec<(&'static str, &'static str)>);

#[derive(Clone)]
struct Script {
    replies: Arc<Mutex<Vec<Reply>>>,
    seen: Arc<Mutex<Vec<(HeaderMap, Value)>>>,
}

async fn completions(State(s): State<Script>, headers: HeaderMap, body: String) -> Response {
    s.seen
        .lock()
        .unwrap()
        .push((headers, serde_json::from_str(&body).unwrap()));
    let (status, text, extra) = {
        let mut r = s.replies.lock().unwrap();
        if r.is_empty() {
            (
                500,
                r#"{"code":"test","error":"exhausted"}"#.to_string(),
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

fn sse(chunks: &[Value]) -> String {
    let mut out: String = chunks.iter().map(|d| format!("data: {d}\n\n")).collect();
    out.push_str("data: [DONE]\n\n");
    out
}

fn chunk(delta: Value, finish: Option<&str>) -> Value {
    let mut choice = json!({ "index": 0, "delta": delta });
    if let Some(f) = finish {
        choice["finish_reason"] = json!(f);
    }
    json!({ "id": "x", "object": "chat.completion.chunk", "created": 0, "model": "grok-4.7", "choices": [choice] })
}

fn usage_chunk() -> Value {
    json!({ "id": "x", "object": "chat.completion.chunk", "created": 0, "model": "grok-4.7", "choices": [],
        "usage": { "prompt_tokens": 723, "completion_tokens": 3, "total_tokens": 826,
                   "prompt_tokens_details": { "cached_tokens": 640 } } })
}

fn tool_reply() -> (u16, String, Vec<(&'static str, &'static str)>) {
    (
        200,
        sse(&[
            chunk(
                json!({ "reasoning_content": "The user", "role": "assistant" }),
                None,
            ),
            chunk(json!({ "reasoning_content": " wants weather." }), None),
            chunk(
                json!({ "tool_calls": [{ "id": "call-a5f5-0", "function": { "name": "get_weather", "arguments": "{\"city\":\"Brisbane\"}" }, "index": 0, "type": "function" }] }),
                None,
            ),
            chunk(json!({}), Some("tool_calls")),
            usage_chunk(),
        ]),
        vec![
            ("x-ratelimit-limit-tokens", "1000"),
            ("x-ratelimit-remaining-tokens", "250"),
        ],
    )
}

fn text_reply(text: &str) -> (u16, String, Vec<(&'static str, &'static str)>) {
    (
        200,
        sse(&[
            chunk(json!({ "content": text, "role": "assistant" }), None),
            chunk(json!({}), Some("stop")),
            usage_chunk(),
        ]),
        vec![],
    )
}

async fn mock(replies: Vec<Reply>) -> (String, Script) {
    let script = Script {
        replies: Arc::new(Mutex::new(replies)),
        seen: Arc::new(Mutex::new(vec![])),
    };
    let app = Router::new()
        .route("/v1/chat/completions", axum::routing::post(completions))
        .with_state(script.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr: SocketAddr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}"), script)
}

fn app(base_url: &str, tmp: &tempfile::TempDir, expired: bool) -> Router {
    app_tuned(base_url, tmp, expired, |_| {})
}

fn app_tuned(
    base_url: &str,
    tmp: &tempfile::TempDir,
    expired: bool,
    tune: impl FnOnce(&mut ThinConfig),
) -> Router {
    let creds = tmp.path().join("auth.json");
    let expires = if expired {
        "2000-01-01T00:00:00Z"
    } else {
        "2999-01-01T00:00:00Z"
    };
    std::fs::write(
        &creds,
        json!({
            "https://auth.x.ai::client": {
                "key": "eyJ.e2e.grok",
                "auth_mode": "oidc",
                "refresh_token": "r",
                "expires_at": expires,
                "oidc_issuer": "https://auth.x.ai",
                "oidc_client_id": "client",
                "email": "e2e@example.com"
            }
        })
        .to_string(),
    )
    .unwrap();
    let mut thin = ThinConfig::default();
    thin.vendors.grok = GrokCfg {
        base_url: Some(base_url.to_string()),
        credentials_path: Some(creds),
        client_version: Some("1.0.30".into()),
        ..Default::default()
    };
    tune(&mut thin);
    let config = ConfigBundle {
        config_dir: tmp.path().to_path_buf(),
        router: RouterConfig {
            data_dir: tmp.path().join("data"),
            thin,
            ..Default::default()
        },
        accounts: AccountsConfig {
            pools: Default::default(),
        },
        models: ModelsConfig {
            aliases: Default::default(),
            models: vec![ModelEntry {
                id: "subs/grok/grok-4.7".into(),
                backend: BackendId::Grok,
                cli_model: "grok-4.7".into(),
                context_window: 131_072,
                max_tokens: 32_000,
                thinking: true,
                cost: 0.0,
                name: "Grok 4.7".into(),
                provider: None,
                transport: Transport::OauthDirect,
                vendor_model: None,
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
async fn non_stream_tool_turn_with_client_headers_and_rate_limit() {
    let (base, script) = mock(vec![tool_reply()]).await;
    let tmp = tempfile::tempdir().unwrap();
    let (status, headers, body) = post(
        app(&base, &tmp, false),
        json!({
            "model": "grok/grok-4.7",
            "reasoning_effort": "max",
            "messages": [{"role": "user", "content": "Weather in Brisbane?"}],
            "tools": [{"type":"function","function":{"name":"get_weather","parameters":{"type":"object","properties":{"city":{"type":"string"}}}}}]
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["object"], "chat.completion");
    assert_eq!(v["choices"][0]["finish_reason"], "tool_calls");
    let call = &v["choices"][0]["message"]["tool_calls"][0];
    assert_eq!(call["function"]["name"], "get_weather");
    assert_eq!(call["function"]["arguments"], "{\"city\":\"Brisbane\"}");
    let id = call["id"].as_str().unwrap();
    assert!(id.starts_with("call_"), "vendor id replaced: {id}");
    assert_eq!(
        v["choices"][0]["message"]["reasoning_content"],
        "The user wants weather."
    );
    assert_eq!(v["usage"]["prompt_tokens"], 723);
    assert_eq!(v["usage"]["prompt_tokens_details"]["cached_tokens"], 640);
    // 1 - 250/1000
    let rl = headers
        .get("x-thickapi-rate-limit")
        .unwrap()
        .to_str()
        .unwrap();
    assert!(rl.starts_with("0.75"), "{rl}");

    let seen = script.seen.lock().unwrap();
    let (req_headers, sent) = &seen[0];
    assert_eq!(
        req_headers.get("authorization").unwrap(),
        "Bearer eyJ.e2e.grok"
    );
    assert_eq!(
        req_headers.get("x-grok-client-identifier").unwrap(),
        "grok-cli"
    );
    assert_eq!(req_headers.get("x-grok-client-version").unwrap(), "1.0.30");
    assert_eq!(sent["model"], "grok-4.7");
    assert_eq!(sent["reasoning_effort"], "xhigh");
    assert_eq!(sent["stream"], true);
    assert_eq!(sent["tools"][0]["function"]["name"], "get_weather");
}

#[tokio::test]
async fn stream_round_trip_emits_reasoning_text_and_usage() {
    let (base, _) = mock(vec![text_reply("pong")]).await;
    let tmp = tempfile::tempdir().unwrap();
    let (status, headers, body) = post(
        app(&base, &tmp, false),
        json!({
            "model": "subs/grok/grok-4.7",
            "stream": true,
            "stream_options": {"include_usage": true},
            "messages": [{"role": "user", "content": "ping"}]
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
    assert!(body.contains("\"content\":\"pong\""), "{body}");
    assert!(body.contains("\"finish_reason\":\"stop\""), "{body}");
    assert!(body.contains("\"prompt_tokens\":723"), "{body}");
    assert!(body.trim_end().ends_with("data: [DONE]"), "{body}");
}

#[tokio::test]
async fn xai_400_maps_to_invalid_request_with_vendor() {
    let (base, _) = mock(vec![(
        400,
        r#"{"code":"invalid-argument","error":"This model does not support `reasoning_effort` value `none`."}"#.into(),
        vec![],
    )])
    .await;
    let tmp = tempfile::tempdir().unwrap();
    let (status, _, body) = post(
        app(&base, &tmp, false),
        json!({ "model": "grok/grok-4.7", "messages": [{"role": "user", "content": "x"}] }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["error"]["vendor"], "grok");
    assert!(v["error"]["message"]
        .as_str()
        .unwrap()
        .contains("reasoning_effort"));
}

#[tokio::test]
async fn expired_token_is_401_login_required_without_calling_xai() {
    let (base, script) = mock(vec![text_reply("never")]).await;
    let tmp = tempfile::tempdir().unwrap();
    let (status, _, body) = post(
        app(&base, &tmp, true),
        json!({ "model": "grok/grok-4.7", "messages": [{"role": "user", "content": "x"}] }),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["error"]["code"], "vendor_login_required");
    assert!(
        script.seen.lock().unwrap().is_empty(),
        "no upstream call on an expired token"
    );
}

#[tokio::test]
async fn ready_reports_grok_vendor() {
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
    let grok = &v["thin"]["vendors"]["grok"];
    assert_eq!(grok["path"], "oauth");
    assert_eq!(grok["token"]["present"], true);
    assert_eq!(grok["token"]["expired"], false);
    assert!(grok["cli_version"].is_null() || grok["cli_version"].is_string());
    if let Some(ver) = grok["cli_version"].as_str().filter(|s| !s.is_empty()) {
        assert!(
            grok["fixture_set"].is_string()
                || grok["fixture_warn"]
                    .as_str()
                    .is_some_and(|w| w.contains(ver)),
            "ready must name a fixture set or warn: {}",
            grok
        );
    }
}

async fn grok_ready(app: Router) -> Value {
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
    let v: Value = serde_json::from_slice(&bytes).unwrap();
    v["thin"]["vendors"]["grok"].clone()
}

/// #110: a throttle with the vendor's wait turns `/ready` `available` off with that reset,
/// and a success after it turns it back on.
#[tokio::test]
async fn ready_folds_a_rate_limit_into_available_false_with_the_reset() {
    let (base, _) = mock(vec![
        (
            429,
            r#"{"error":{"message":"Rate limit exceeded, please try again"}}"#.into(),
            vec![("retry-after", "90")],
        ),
        text_reply("back"),
    ])
    .await;
    let tmp = tempfile::tempdir().unwrap();
    let app = app(&base, &tmp, false);
    let before = grok_ready(app.clone()).await;
    assert_eq!(before["available"], true, "{before}");
    assert!(before["retry_in_seconds"].is_null(), "{before}");
    assert_eq!(before["rows_unavailable"], json!([]), "{before}");

    let request =
        json!({ "model": "grok/grok-4.7", "messages": [{"role": "user", "content": "x"}] });
    let (status, _, body) = post(app.clone(), request.clone()).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{body}");
    let limited = grok_ready(app.clone()).await;
    assert_eq!(limited["available"], false, "{limited}");
    let retry = limited["retry_in_seconds"].as_u64().unwrap();
    assert!((85..=90).contains(&retry), "{limited}");
    assert!(
        limited["reason"]
            .as_str()
            .unwrap()
            .contains("rate_limit_error"),
        "{limited}"
    );

    let (status, _, body) = post(app.clone(), request).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let back = grok_ready(app).await;
    assert_eq!(back["available"], true, "{back}");
}

/// #110: no stored credential means the vendor cannot serve.
#[tokio::test]
async fn ready_says_unavailable_without_a_credential() {
    let (base, _) = mock(vec![]).await;
    let tmp = tempfile::tempdir().unwrap();
    let app = app_tuned(&base, &tmp, false, |thin| {
        thin.vendors.grok.credentials_path = Some(std::path::PathBuf::from("does-not-exist.json"));
    });
    let grok = grok_ready(app).await;
    assert_eq!(grok["token"]["present"], false, "{grok}");
    assert_eq!(grok["available"], false, "{grok}");
    assert!(grok["retry_in_seconds"].is_null(), "{grok}");
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

fn two_parallel_tools_reply() -> (u16, String, Vec<(&'static str, &'static str)>) {
    (
        200,
        sse(&[
            chunk(
                json!({ "reasoning_content": "Need both tools.", "role": "assistant" }),
                None,
            ),
            chunk(
                json!({ "tool_calls": [
                    { "id": "call-h9-0", "function": { "name": "read", "arguments": "{\"path\":\"a\"}" }, "index": 0, "type": "function" },
                    { "id": "call-h9-1", "function": { "name": "bash", "arguments": "{\"command\":\"ls\"}" }, "index": 1, "type": "function" }
                ] }),
                None,
            ),
            chunk(json!({}), Some("tool_calls")),
            usage_chunk(),
        ]),
        vec![],
    )
}

fn one_more_tool_reply() -> (u16, String, Vec<(&'static str, &'static str)>) {
    (
        200,
        sse(&[
            chunk(
                json!({ "reasoning_content": "One more read.", "role": "assistant" }),
                None,
            ),
            chunk(
                json!({ "tool_calls": [
                    { "id": "call-h9-2", "function": { "name": "read", "arguments": "{\"path\":\"b\"}" }, "index": 0, "type": "function" }
                ] }),
                None,
            ),
            chunk(json!({}), Some("tool_calls")),
            usage_chunk(),
        ]),
        vec![],
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
    let details = json!([{"type":"reasoning.text","text":"Need both tools."}]);

    let (status, _, body) = post(
        app(&base, &tmp, false),
        json!({
            "model": "grok/grok-4.7",
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
        app(&base, &tmp, false),
        json!({
            "model": "grok/grok-4.7",
            "reasoning_effort": "high",
            "messages": [
                {"role":"user","content":"read a and ls"},
                {"role":"assistant","content":null,
                 "tool_calls":[
                    {"id":id_read,"type":"function","function":{"name":"read","arguments":"{\"path\":\"a\"}"}},
                    {"id":id_bash,"type":"function","function":{"name":"bash","arguments":"{\"command\":\"ls\"}"}}
                 ],
                 "reasoning_content": "Need both tools.",
                 "reasoning_details": details
                },
                {"role":"tool","tool_call_id":id_read,"content":"file a"},
                {"role":"tool","tool_call_id":id_bash,"content":"a\nb"}
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
        let msgs = seen[1].1["messages"].as_array().unwrap();
        let assistant = msgs.iter().find(|m| m["role"] == "assistant").unwrap();
        assert_eq!(assistant["tool_calls"].as_array().unwrap().len(), 2);
        assert_eq!(assistant["tool_calls"][0]["id"], id_read);
        assert_eq!(assistant["tool_calls"][1]["id"], id_bash);
        let tool_ids: Vec<&str> = msgs
            .iter()
            .filter(|m| m["role"] == "tool")
            .filter_map(|m| m["tool_call_id"].as_str())
            .collect();
        assert!(tool_ids.contains(&id_read.as_str()), "{tool_ids:?}");
        assert!(tool_ids.contains(&id_bash.as_str()), "{tool_ids:?}");
    }

    let (status, _, body) = post(
        app(&base, &tmp, false),
        json!({
            "model": "grok/grok-4.7",
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
                {"role":"tool","tool_call_id":id_bash,"content":"a\nb"},
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
    let tool_ids: Vec<&str> = seen[2].1["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m["role"] == "tool")
        .filter_map(|m| m["tool_call_id"].as_str())
        .collect();
    assert!(tool_ids.contains(&id_read2.as_str()), "{tool_ids:?}");
}

/// A vendor that answers 200 and then only ever sends keepalives. `first` is sent once.
async fn keepalive_forever(first: &'static str) -> String {
    let body = move || {
        let stream = async_stream::stream! {
            if !first.is_empty() {
                yield Ok::<_, std::io::Error>(axum::body::Bytes::from(first));
            }
            loop {
                tokio::time::sleep(std::time::Duration::from_millis(40)).await;
                yield Ok(axum::body::Bytes::from(": keepalive

"));
            }
        };
        let mut response = Body::from_stream(stream).into_response();
        response
            .headers_mut()
            .insert("content-type", "text/event-stream".parse().unwrap());
        response
    };
    let app = Router::new().route(
        "/v1/chat/completions",
        axum::routing::post(move || async move { body() }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr: SocketAddr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}")
}

fn quick_budgets(thin: &mut ThinConfig) {
    thin.vendors.grok.budgets.first_event_seconds = 1;
    thin.vendors.grok.budgets.idle_seconds = 1;
}

/// Headers then silence used to hang the request for as long as the vendor liked.
#[tokio::test]
async fn a_vendor_that_only_sends_keepalives_is_a_typed_503() {
    let base = keepalive_forever("").await;
    let tmp = tempfile::tempdir().unwrap();
    let started = std::time::Instant::now();
    let (status, _, body) = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        post(
            app_tuned(&base, &tmp, false, quick_budgets),
            json!({ "model": "grok/grok-4.7", "messages": [{"role": "user", "content": "x"}] }),
        ),
    )
    .await
    .expect("the route must answer on its own");
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["error"]["code"], "vendor_down");
    assert!(
        v["error"]["message"]
            .as_str()
            .unwrap()
            .contains("sent nothing"),
        "{body}"
    );
    assert!(started.elapsed() >= std::time::Duration::from_secs(1));
}

/// Same silence after the answer began: the stream ends with an error, not a hang.
#[tokio::test]
async fn a_vendor_that_goes_quiet_mid_answer_ends_the_stream_with_an_error() {
    let first = "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Hel\"}}]}

";
    let base = keepalive_forever(first).await;
    let tmp = tempfile::tempdir().unwrap();
    let (status, _, body) = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        post(
            app_tuned(&base, &tmp, false, quick_budgets),
            json!({ "model": "grok/grok-4.7", "stream": true,
                    "messages": [{"role": "user", "content": "x"}] }),
        ),
    )
    .await
    .expect("the stream must end on its own");
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("Hel"), "{body}");
    assert!(body.contains("went quiet mid-answer"), "{body}");
}

/// A vendor that streams `frames` in order, sleeping `ms` before each, then closes.
async fn paced(frames: Vec<(u64, &'static str)>) -> String {
    let frames = Arc::new(frames);
    let body = move || {
        let frames = Arc::clone(&frames);
        let stream = async_stream::stream! {
            for (ms, frame) in frames.iter() {
                tokio::time::sleep(std::time::Duration::from_millis(*ms)).await;
                yield Ok::<_, std::io::Error>(axum::body::Bytes::from(*frame));
            }
        };
        let mut response = Body::from_stream(stream).into_response();
        response
            .headers_mut()
            .insert("content-type", "text/event-stream".parse().unwrap());
        response
    };
    let app = Router::new().route(
        "/v1/chat/completions",
        axum::routing::post(move || async move { body() }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr: SocketAddr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}")
}

const REASONING_THEN_SILENCE: &str =
    "data: {\"choices\":[{\"index\":0,\"delta\":{\"reasoning_content\":\"planning\"}}]}\n\n";
const WHOLE_TOOL_CALL: &str = "data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call-abc-0\",\"type\":\"function\",\"function\":{\"name\":\"write\",\"arguments\":\"{\\\"path\\\":\\\"spec.md\\\"}\"}}]},\"finish_reason\":\"tool_calls\"}]}\n\ndata: [DONE]\n\n";

fn tool_request() -> Value {
    json!({
        "model": "grok/grok-4.7", "stream": true,
        "messages": [{"role": "user", "content": "write the spec"}],
        "tools": [{"type":"function","function":{"name":"write","parameters":{"type":"object","properties":{"path":{"type":"string"}}}}}]
    })
}

/// xAI delivers a tool call whole: a long `write` is silence past `budgets.idle_seconds`
/// (measured 2026-09-22: Pi's `grok went quiet mid-answer for 180s`). With tools offered
/// the turn runs on `budgets.tool_call_floor_seconds` and the call still lands.
#[tokio::test]
async fn a_whole_tool_call_that_outlives_the_idle_budget_still_lands_when_tools_are_offered() {
    let base = paced(vec![(0, REASONING_THEN_SILENCE), (1500, WHOLE_TOOL_CALL)]).await;
    let tmp = tempfile::tempdir().unwrap();
    let (status, _, body) = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        post(
            app_tuned(&base, &tmp, false, |thin| {
                quick_budgets(thin);
                thin.vendors.grok.budgets.tool_call_floor_seconds = 5;
            }),
            tool_request(),
        ),
    )
    .await
    .expect("the stream must end on its own");
    assert_eq!(status, StatusCode::OK);
    assert!(!body.contains("went quiet"), "{body}");
    assert!(body.contains("\"name\":\"write\""), "{body}");
    assert!(body.contains("\"finish_reason\":\"tool_calls\""), "{body}");
    assert!(body.contains("data: [DONE]"), "{body}");
}

/// The same silence with no tools on offer is still the vendor going away.
#[tokio::test]
async fn the_same_silence_without_tools_is_still_quiet_mid_answer() {
    // 3 s against a 1 s idle: when a stalled runtime wakes with both ready, `timeout` polls
    // the stream first, so the margin has to outlast a loaded runner (#257).
    let base = paced(vec![(0, REASONING_THEN_SILENCE), (3000, WHOLE_TOOL_CALL)]).await;
    let tmp = tempfile::tempdir().unwrap();
    let mut request = tool_request();
    request.as_object_mut().unwrap().remove("tools");
    let (status, _, body) = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        post(
            app_tuned(&base, &tmp, false, |thin| {
                quick_budgets(thin);
                thin.vendors.grok.budgets.tool_call_floor_seconds = 5;
            }),
            request,
        ),
    )
    .await
    .expect("the stream must end on its own");
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("went quiet mid-answer for 1s"), "{body}");
}

/// The floor is a floor, not forever: a vendor that keeps a tool turn warm and never
/// delivers is ended at `budgets.tool_call_floor_seconds`.
#[tokio::test]
async fn a_tool_turn_that_never_delivers_ends_at_the_tool_call_budget() {
    let base = keepalive_forever(REASONING_THEN_SILENCE).await;
    let tmp = tempfile::tempdir().unwrap();
    let started = std::time::Instant::now();
    let (status, _, body) = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        post(
            app_tuned(&base, &tmp, false, |thin| {
                quick_budgets(thin);
                thin.vendors.grok.budgets.tool_call_floor_seconds = 2;
            }),
            tool_request(),
        ),
    )
    .await
    .expect("the stream must end on its own");
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("went quiet mid-answer for 2s"), "{body}");
    assert!(started.elapsed() >= std::time::Duration::from_secs(2));
}

/// The tool-call floor is for silence mid-answer, not before it (#129): a tool turn whose
/// vendor never sends a first event still ends at `budgets.first_event_seconds`.
#[tokio::test]
async fn a_tool_turn_keeps_the_default_first_event_budget() {
    let base = keepalive_forever("").await;
    let tmp = tempfile::tempdir().unwrap();
    let started = std::time::Instant::now();
    let (status, _, body) = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        post(
            app_tuned(&base, &tmp, false, |thin| {
                quick_budgets(thin);
                thin.vendors.grok.budgets.tool_call_floor_seconds = 6;
            }),
            tool_request(),
        ),
    )
    .await
    .expect("the route must answer on its own");
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert!(body.contains("sent nothing"), "{body}");
    assert!(
        started.elapsed() < std::time::Duration::from_secs(5),
        "first event ran on the 1 s budget, not the 6 s floor"
    );
}

/// "Rate limit exceeded" contains "limit" and "exceeded"; it is still only a throttle.
#[tokio::test]
async fn a_throttle_is_a_retryable_429_with_the_vendors_wait() {
    let (base, _) = mock(vec![(
        429,
        r#"{"error":{"message":"Rate limit exceeded, please try again"}}"#.into(),
        vec![("retry-after", "10")],
    )])
    .await;
    let tmp = tempfile::tempdir().unwrap();
    let (status, headers, body) = post(
        app(&base, &tmp, false),
        json!({ "model": "grok/grok-4.7", "messages": [{"role": "user", "content": "x"}] }),
    )
    .await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{body}");
    assert_eq!(headers["retry-after"], "10");
    assert_eq!(headers["x-should-retry"], "true");
    assert!(!body.contains("quota exceeded"), "{body}");
}

#[tokio::test]
async fn running_out_of_credits_is_not_retryable() {
    let (base, _) = mock(vec![(
        429,
        r#"{"error":{"message":"You have used all your credits for this billing period"}}"#.into(),
        vec![],
    )])
    .await;
    let tmp = tempfile::tempdir().unwrap();
    let (_, headers, body) = post(
        app(&base, &tmp, false),
        json!({ "model": "grok/grok-4.7", "messages": [{"role": "user", "content": "x"}] }),
    )
    .await;
    assert_eq!(headers["x-should-retry"], "false", "{body}");
}

/// The route's own limit: even an adapter with no budget at all cannot hold a request open
/// without a status.
#[tokio::test]
async fn the_route_answers_503_when_no_adapter_does() {
    let base = keepalive_forever("").await;
    let tmp = tempfile::tempdir().unwrap();
    let tune = |thin: &mut ThinConfig| {
        thin.vendors.grok.budgets.first_event_seconds = 0;
        thin.vendors.grok.budgets.idle_seconds = 0;
        thin.start_deadline_seconds = 1;
    };
    let (status, _, body) = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        post(
            app_tuned(&base, &tmp, false, tune),
            json!({ "model": "grok/grok-4.7", "messages": [{"role": "user", "content": "x"}] }),
        ),
    )
    .await
    .expect("the route must answer on its own");
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert!(body.contains("no response from grok in 1s"), "{body}");
}
