#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
//! `/v1/chat/completions` end to end: real router, real `ClaudeAdapter` (mode `only`, so no
//! bridge spawn), Anthropic replaced by a local stand-in. Covers non-stream, stream, the 502
//! `vendor_drift` a blocked model returns under `only` (SPEC §19.12 T1 exit), 429 headers,
//! `/ready` vendor block and CORS preflight.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, Request, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Router;
use http_body_util::BodyExt;
use serde_json::{json, Value};
use thickapi_api::{build_router, make_state_with_store, AppState};
use thickapi_core::config::keys::{
    hash_token, KeyEntry, KeysConfig, SCOPE_CHAT_WRITE, SCOPE_TURN_WRITE,
};
use thickapi_core::config::models::{ModelEntry, ModelsConfig, Transport};
use thickapi_core::config::router::{
    ClaudeCfg, ClaudeOauthConfig, ClaudeOauthMode, RouterConfig, ThinConfig, Vendors,
};
use thickapi_core::config::{AccountsConfig, CombosConfig};
use thickapi_core::thin::oauth::state::ProbeResult;
use thickapi_core::types::BackendId;
use thickapi_core::{ConfigBundle, Store};
use tower::ServiceExt;

const TOKEN: &str = "ta_e2e";

/// One scripted upstream reply: HTTP status, body text, and extra `(header, value)` pairs.
type Reply = (u16, String, Vec<(&'static str, &'static str)>);

#[derive(Clone)]
struct Script {
    replies: Arc<Mutex<Vec<Reply>>>,
    seen: Arc<Mutex<Vec<Value>>>,
}

async fn messages(State(s): State<Script>, body: String) -> Response {
    s.seen
        .lock()
        .unwrap()
        .push(serde_json::from_str(&body).unwrap());
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

fn text_reply(text: &str) -> (u16, String, Vec<(&'static str, &'static str)>) {
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
        vec![("anthropic-ratelimit-unified-5h-utilization", "0.25")],
    )
}

async fn mock(replies: Vec<Reply>) -> (String, Script) {
    let script = Script {
        replies: Arc::new(Mutex::new(replies)),
        seen: Arc::new(Mutex::new(vec![])),
    };
    let app = Router::new()
        .route("/v1/messages", axum::routing::post(messages))
        .with_state(script.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr: SocketAddr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}"), script)
}

fn app(
    base_url: &str,
    tmp: &tempfile::TempDir,
    cors: Vec<String>,
    mode: ClaudeOauthMode,
) -> Router {
    build_router(state(base_url, tmp, cors, mode))
}

fn state(
    base_url: &str,
    tmp: &tempfile::TempDir,
    cors: Vec<String>,
    mode: ClaudeOauthMode,
) -> AppState {
    state_with(base_url, tmp, cors, mode, Opts::default())
}

/// R3 knobs: a bridge stub that faults before `Started` (`claude.exe` unresolved → 503
/// `vendor_down`, one of SPEC §19.15 R3's named triggers), the `cross_fallback` switch, and a
/// `cli_bridge` row so the bridge is the first path while the direct state is open.
#[derive(Clone)]
struct Opts {
    bridge_faults: bool,
    cross_fallback: bool,
    bridge_row: bool,
    haiku_row: bool,
    max_body_mib: u64,
    /// `thin.vendors.claude.path` (the `fake-claude` stub) when the bridge must answer.
    claude_path: Option<std::path::PathBuf>,
    /// Wire model of the `claude/sonnet` row: a `fake-claude` mode.
    wire_model: Option<&'static str>,
    /// `thin.vendors.claude.bridge.dialect`, when not the default (`claude_code`).
    dialect: Option<&'static str>,
    /// Extra `keys.yaml` rows beside the default `TOKEN` (chat:write) key (D3 deliverable 4).
    extra_keys: Vec<KeyEntry>,
    /// `thin.reply_timeout_seconds` and `thin.oauth_in_flight`, when not the defaults.
    reply_timeout_seconds: Option<u64>,
    oauth_in_flight: Option<u32>,
}

impl Default for Opts {
    fn default() -> Self {
        Self {
            bridge_faults: false,
            cross_fallback: true,
            bridge_row: false,
            haiku_row: false,
            max_body_mib: ThinConfig::default().max_body_mib,
            claude_path: None,
            wire_model: None,
            dialect: None,
            extra_keys: vec![],
            reply_timeout_seconds: None,
            oauth_in_flight: None,
        }
    }
}

fn row(id: &str, transport: Transport) -> ModelEntry {
    ModelEntry {
        id: id.into(),
        backend: BackendId::ClaudeCode,
        cli_model: "sonnet".into(),
        context_window: 200_000,
        max_tokens: 8192,
        thinking: true,
        cost: 0.0,
        name: "Claude Sonnet".into(),
        provider: None,
        transport,
        vendor_model: Some("claude-sonnet-4-5".into()),
        enabled: true,
        input: vec!["text".into()],
    }
}

fn state_with(
    base_url: &str,
    tmp: &tempfile::TempDir,
    cors: Vec<String>,
    mode: ClaudeOauthMode,
    opts: Opts,
) -> AppState {
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
            cors_origins: cors,
            claude_oauth: ClaudeOauthConfig {
                mode,
                base_url: Some(base_url.to_string()),
                credentials_path: Some(creds),
                cross_fallback: opts.cross_fallback,
                ..Default::default()
            },
            thin: ThinConfig {
                vendors: Vendors {
                    claude: ClaudeCfg {
                        path: opts
                            .bridge_faults
                            .then(|| tmp.path().join("missing").join("claude.exe"))
                            .or_else(|| opts.claude_path.clone()),
                        bridge: thickapi_core::config::router::ClaudeBridgeCfg {
                            dialect: opts.dialect.unwrap_or("claude_code").to_string(),
                            ..Default::default()
                        },
                        ..Default::default()
                    },
                    ..Default::default()
                },
                max_body_mib: opts.max_body_mib,
                reply_timeout_seconds: opts
                    .reply_timeout_seconds
                    .unwrap_or(ThinConfig::default().reply_timeout_seconds),
                oauth_in_flight: opts
                    .oauth_in_flight
                    .unwrap_or(ThinConfig::default().oauth_in_flight),
                ..Default::default()
            },
            ..Default::default()
        },
        accounts: AccountsConfig {
            pools: Default::default(),
        },
        models: ModelsConfig {
            aliases: Default::default(),
            models: {
                let mut sonnet = row("claude/sonnet", Transport::OauthDirect);
                if let Some(wire) = opts.wire_model {
                    sonnet.vendor_model = Some(wire.into());
                }
                let mut rows = vec![sonnet];
                if opts.bridge_row {
                    rows.push(row("claude/bridge", Transport::CliBridge));
                }
                if opts.haiku_row {
                    let mut h = row("claude/haiku", Transport::OauthDirect);
                    h.cli_model = "haiku".into();
                    h.vendor_model = Some("claude-haiku-4-5".into());
                    rows.push(h);
                }
                rows
            },
        },
        combos: CombosConfig {
            combos: Default::default(),
        },
        keys: KeysConfig {
            keys: {
                let mut keys = vec![KeyEntry {
                    id: "e2e".into(),
                    hash: hash_token(TOKEN),
                    scopes: vec![SCOPE_CHAT_WRITE.into()],
                    ..Default::default()
                }];
                keys.extend(opts.extra_keys);
                keys
            },
        },
    };
    make_state_with_store(config, Arc::new(Store::open_in_memory().unwrap()))
}

async fn ready(app: Router) -> Value {
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
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap()
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
async fn non_stream_round_trip_with_rate_limit_header() {
    let (base, script) = mock(vec![text_reply("OK")]).await;
    let tmp = tempfile::tempdir().unwrap();
    let (status, headers, body) = post(
        app(&base, &tmp, vec![], ClaudeOauthMode::Only),
        json!({"model":"subs/claude/sonnet","messages":[{"role":"user","content":"Reply OK"}]}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["object"], "chat.completion");
    assert_eq!(v["model"], "claude-sonnet-4-5");
    assert_eq!(v["choices"][0]["message"]["content"], "OK");
    assert_eq!(v["choices"][0]["finish_reason"], "stop");
    assert_eq!(v["usage"]["prompt_tokens"], 7);
    assert_eq!(v["usage"]["completion_tokens"], 2);
    assert_eq!(headers["x-thickapi-rate-limit"], "0.250");
    assert_eq!(headers["x-thickapi-path"], "oauth");
    assert!(headers.get("x-thickapi-fallback").is_none());
    assert!(headers["x-request-id"]
        .to_str()
        .unwrap()
        .starts_with("chatcmpl-"));
    let seen = script.seen.lock().unwrap();
    assert_eq!(seen[0]["model"], "claude-sonnet-4-5");
    assert!(seen[0]["system"][0]["text"]
        .as_str()
        .unwrap()
        .starts_with("You are Claude Code"));
}

#[tokio::test]
async fn stream_round_trip_emits_chunks_and_done() {
    let (base, _) = mock(vec![text_reply("streamed")]).await;
    let tmp = tempfile::tempdir().unwrap();
    let (status, headers, body) = post(
        app(&base, &tmp, vec![], ClaudeOauthMode::Only),
        json!({"model":"claude/sonnet","messages":[{"role":"user","content":"hi"}],"stream":true,"stream_options":{"include_usage":true}}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(headers["content-type"], "text/event-stream");
    let frames: Vec<Value> = body
        .split("\n\n")
        .filter_map(|f| f.strip_prefix("data: "))
        .filter(|d| *d != "[DONE]")
        .map(|d| serde_json::from_str(d).unwrap())
        .collect();
    assert!(body.trim_end().ends_with("data: [DONE]"));
    assert_eq!(frames[0]["choices"][0]["delta"]["role"], "assistant");
    assert!(frames
        .iter()
        .any(|f| f["choices"][0]["delta"]["content"] == "streamed"));
    assert!(frames
        .iter()
        .any(|f| f["choices"][0]["finish_reason"] == "stop"));
    assert_eq!(frames.last().unwrap()["usage"]["total_tokens"], 9);
}

#[tokio::test]
async fn blocked_under_only_is_502_vendor_drift_naming_the_block() {
    let (base, _) = mock(vec![(
        400,
        json!({"type":"error","error":{"type":"invalid_request_error","message":"This credential is only authorized for use with Claude Code and cannot be used for other API requests."}}).to_string(),
        vec![],
    )])
    .await;
    let tmp = tempfile::tempdir().unwrap();
    let (status, headers, body) = post(
        app(&base, &tmp, vec![], ClaudeOauthMode::Only),
        json!({"model":"claude/sonnet","messages":[{"role":"user","content":"hi"}]}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_GATEWAY, "{body}");
    assert_eq!(headers["x-thickapi-path"], "oauth;reason=blocked");
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["error"]["type"], "vendor_drift");
    assert_eq!(v["error"]["vendor"], "claude");
    assert!(
        v["error"]["message"]
            .as_str()
            .unwrap()
            .contains("only authorized for use with Claude Code"),
        "{body}"
    );
    assert_eq!(v["error"]["hint"], "thickapi doctor");
}

#[tokio::test]
async fn quota_429_carries_retry_after_and_no_retry() {
    let (base, _) = mock(vec![
        text_reply("warm"),
        (
            429,
            json!({"type":"error","error":{"type":"rate_limit_error","message":"You have exceeded your usage limit for this period"}}).to_string(),
            vec![("retry-after", "3600")],
        ),
    ])
    .await;
    let tmp = tempfile::tempdir().unwrap();
    let router = app(&base, &tmp, vec![], ClaudeOauthMode::Only);
    let req = json!({"model":"claude/sonnet","messages":[{"role":"user","content":"hi"}]});
    let (s1, _, _) = post(router.clone(), req.clone()).await;
    assert_eq!(s1, StatusCode::OK);
    let (status, headers, body) = post(router, req).await;
    assert_eq!(status, StatusCode::PAYMENT_REQUIRED, "{body}");
    assert_eq!(headers["retry-after"], "3600");
    assert_eq!(headers["x-should-retry"], "false");
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["error"]["type"], "insufficient_quota");
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
async fn ready_reports_claude_vendor_and_cors_preflight_answers() {
    let (base, _) = mock(vec![text_reply("OK")]).await;
    let tmp = tempfile::tempdir().unwrap();
    let router = app(
        &base,
        &tmp,
        vec!["http://localhost:5173".into()],
        ClaudeOauthMode::Prefer,
    );

    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .uri("/ready")
                .header("authorization", format!("Bearer {TOKEN}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let v: Value = serde_json::from_slice(&bytes).unwrap();
    let claude = &v["thin"]["vendors"]["claude"];
    assert_eq!(claude["oauth_mode"], "prefer");
    assert_eq!(claude["token"]["present"], true);
    assert_eq!(claude["token"]["expired"], false);
    assert_eq!(claude["path"], "oauth", "{claude}");
    assert_eq!(claude["oauth"]["status"], "ok");
    if let Some(ver) = claude["cli_version"].as_str().filter(|s| !s.is_empty()) {
        assert!(
            claude["fixture_set"].is_string()
                || claude["fixture_warn"]
                    .as_str()
                    .is_some_and(|w| w.contains(ver)),
            "ready must name a fixture set or warn: {claude}"
        );
    }

    let preflight = router
        .clone()
        .oneshot(
            Request::builder()
                .method("OPTIONS")
                .uri("/v1/chat/completions")
                .header("origin", "http://localhost:5173")
                .header("access-control-request-method", "POST")
                .header(
                    "access-control-request-headers",
                    "authorization,content-type",
                )
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        preflight.status(),
        StatusCode::OK,
        "preflight must not hit auth"
    );
    assert_eq!(
        preflight.headers()["access-control-allow-origin"],
        "http://localhost:5173"
    );
    assert!(preflight.headers()["access-control-allow-methods"]
        .to_str()
        .unwrap()
        .contains("POST"));

    let response = router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/chat/completions")
                .header("authorization", format!("Bearer {TOKEN}"))
                .header("content-type", "application/json")
                .header("origin", "http://localhost:5173")
                .body(Body::from(
                    json!({"model":"claude/sonnet","messages":[{"role":"user","content":"hi"}]})
                        .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let expose = response
        .headers()
        .get("access-control-expose-headers")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_ascii_lowercase();
    assert!(expose.contains("x-thickapi-path"), "{expose}");
    assert!(expose.contains("x-thickapi-fallback"), "{expose}");
}

fn third_party_400() -> (u16, String, Vec<(&'static str, &'static str)>) {
    (
        400,
        json!({"type":"error","error":{"type":"invalid_request_error","message":"Third-party apps now draw from your extra usage. Enable extra usage to continue. Contact ops@example.com with req_011Cf8HRjCC54hiCUpj1PN3J."}}).to_string(),
        vec![],
    )
}

/// SPEC §19.15 R1+R2 exit: a probe that finds the block marks the state, `/ready` shows it with
/// the probe schedule, the mirror file survives a "restart" (a second runtime on the same
/// config) which starts blocked with the probe scheduled, and a later probe 200 reopens the path.
#[tokio::test]
async fn probe_block_persists_across_restart_and_probe_ok_reopens() {
    let (base, script) = mock(vec![third_party_400(), text_reply("OK")]).await;
    let tmp = tempfile::tempdir().unwrap();
    let first = state(&base, &tmp, vec![], ClaudeOauthMode::Prefer);
    let claude = first.thin.claude().clone();
    assert!(
        claude.probe_active(),
        "prefer + probe.enabled (default) → serve owns the state"
    );

    assert_eq!(claude.probe_once().await, ProbeResult::Blocked);
    {
        let seen = script.seen.lock().unwrap();
        assert_eq!(
            seen[0]["model"], "claude-fable-5-1",
            "probe uses the non-Haiku probe row"
        );
        assert_eq!(seen[0]["thinking"]["type"], "enabled");
        assert_eq!(seen[0]["thinking"]["budget_tokens"], 4_000);
        assert!(
            seen[0]["max_tokens"].as_u64().unwrap() >= 4_000 + 1_024,
            "max_tokens={}",
            seen[0]["max_tokens"]
        );
        assert_eq!(seen[0]["tools"][0]["name"], "read");
        assert!(seen[0]["system"][0]["text"]
            .as_str()
            .unwrap()
            .starts_with("You are Claude Code"));
    }
    let v = ready(build_router(first.clone())).await;
    let v_str = v.to_string();
    assert!(!v_str.contains("@example.com"));
    assert!(!v_str.contains("req_011Cf8HRjCC54hiCUpj1PN3J"));
    let oauth = &v["thin"]["vendors"]["claude"]["oauth"];
    assert_eq!(oauth["last_rejection"], "blocked");
    assert!(
        oauth["status"]["blocked"]["reason"]
            .as_str()
            .unwrap()
            .contains("third-party"),
        "{oauth}"
    );
    assert_eq!(oauth["persisted"], true);
    assert_eq!(oauth["probe"]["last_result"], "blocked");
    assert!(oauth["probe"]["next_at"].is_string(), "{oauth}");
    assert_eq!(
        oauth["probe"]["next_at"],
        oauth["status"]["blocked"]["next_probe"]
    );
    assert_eq!(v["thin"]["vendors"]["claude"]["path"], "bridge");
    assert!(
        !claude.state.should_try_direct(),
        "blocked: no user request goes direct"
    );

    let file = tmp.path().join("thin").join("claude_oauth_state.json");
    let on_disk: Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
    assert!(on_disk["blocked"]["reason"]
        .as_str()
        .unwrap()
        .contains("third-party"));
    assert_eq!(on_disk["last_path"], "bridge");
    assert!(
        on_disk.get("last_rejection").is_none(),
        "file holds the classification only"
    );

    // "Restart": a fresh runtime on the same config reads the mirror.
    let second = state(&base, &tmp, vec![], ClaudeOauthMode::Prefer);
    let claude2 = second.thin.claude().clone();
    assert!(claude2.state.is_blocked(), "restart remembers the block");
    assert!(!claude2.state.should_try_direct());
    let v = ready(build_router(second.clone())).await;
    let c = &v["thin"]["vendors"]["claude"];
    assert_eq!(c["path"], "bridge", "{c}");
    assert!(
        c["path_reason"].as_str().unwrap().starts_with("blocked:"),
        "{c}"
    );
    assert_eq!(c["oauth"]["persisted"], true);
    assert_eq!(
        script.seen.lock().unwrap().len(),
        1,
        "restart did not spend a request"
    );

    // The scheduled probe (forced now) gets a 200 → direct path returns.
    assert_eq!(claude2.probe_once().await, ProbeResult::Ok);
    assert!(!claude2.state.is_blocked());
    assert!(claude2.state.should_try_direct());
    let v = ready(build_router(second)).await;
    let c = &v["thin"]["vendors"]["claude"];
    assert_eq!(c["path"], "oauth", "{c}");
    assert_eq!(c["oauth"]["status"], "ok");
    assert_eq!(c["oauth"]["probe"]["last_result"], "ok");
    assert!(
        c["oauth"]["probe"]["next_at"].is_null(),
        "nothing scheduled while open"
    );
    let on_disk: Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
    assert!(on_disk["blocked"].is_null());
    assert_eq!(on_disk["last_path"], "oauth");
}

/// A non-block failure leaves the state alone and schedules a retry; a `mode: only` runtime
/// does not own the state (probe off, `/ready` has no probe field).
#[tokio::test]
async fn probe_error_keeps_state_and_only_mode_has_no_probe() {
    let (base, _) = mock(vec![(
        529,
        json!({"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}})
            .to_string(),
        vec![],
    )])
    .await;
    let tmp = tempfile::tempdir().unwrap();
    let st = state(&base, &tmp, vec![], ClaudeOauthMode::Prefer);
    let claude = st.thin.claude().clone();
    assert_eq!(
        claude.probe_once().await,
        ProbeResult::Error("vendor_down".into())
    );
    assert!(!claude.state.is_blocked());
    let v = ready(build_router(st)).await;
    let oauth = &v["thin"]["vendors"]["claude"]["oauth"];
    assert_eq!(oauth["status"], "ok");
    assert_eq!(oauth["probe"]["last_result"], "error(vendor_down)");
    assert!(
        oauth["probe"]["next_at"].is_string(),
        "retry scheduled: {oauth}"
    );

    let only = tempfile::tempdir().unwrap();
    let st = state(&base, &only, vec![], ClaudeOauthMode::Only);
    assert!(!st.thin.claude().probe_active());
    let v = ready(build_router(st)).await;
    assert!(v["thin"]["vendors"]["claude"]["oauth"]
        .get("probe")
        .is_none());
}

/// Haiku never rides the persisted non-Haiku block (SPEC §19.8.1 / §19.15). The mock is the
/// only upstream: if routing fell to the bridge, `seen` would stay empty.
#[tokio::test]
async fn haiku_stays_on_direct_path_while_non_haiku_is_blocked() {
    let (base, script) = mock(vec![text_reply("haiku ok")]).await;
    let tmp = tempfile::tempdir().unwrap();
    let st = state_with(
        &base,
        &tmp,
        vec![],
        ClaudeOauthMode::Prefer,
        Opts {
            haiku_row: true,
            ..Default::default()
        },
    );
    st.thin
        .claude()
        .state
        .mark_blocked("classed as third-party app", 3600);
    assert!(!st.thin.claude().state.should_try_direct());
    let snap = st.thin.vendor_snapshot();
    assert_eq!(
        snap["claude"]["path"], "bridge",
        "non-Haiku / default stays bridged: {snap}"
    );
    assert!(
        snap["claude"]["path_reason"]
            .as_str()
            .unwrap()
            .starts_with("blocked:"),
        "{snap}"
    );
    assert_eq!(
        snap["claude"]["haiku_path"], "oauth",
        "Haiku stays oauth while non-Haiku is blocked: {snap}"
    );
    assert_eq!(
        snap["claude"]["available"], true,
        "#110: a bridged Claude is available: {snap}"
    );
    assert!(
        snap["claude"]["haiku_path_reason"]
            .as_str()
            .unwrap()
            .contains("Haiku stays on the direct path"),
        "{snap}"
    );

    let claude = st.thin.claude().clone();
    let (status, headers, body) = post(
        build_router(st.clone()),
        json!({"model":"claude/haiku","messages":[{"role":"user","content":"hi"}]}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(headers["x-thickapi-path"], "oauth");
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["choices"][0]["message"]["content"], "haiku ok");
    assert_eq!(
        script.seen.lock().unwrap().len(),
        1,
        "Haiku must use the direct mock, not the bridge"
    );
    // #122: the Haiku 200 must not clear the non-Haiku block.
    assert!(
        claude.state.is_blocked(),
        "a Haiku success must not reopen the direct path"
    );
    let snap = st.thin.vendor_snapshot();
    assert_eq!(
        snap["claude"]["path"], "bridge",
        "the next non-Haiku request still goes to the bridge: {snap}"
    );
}

fn faulting_bridge(cross_fallback: bool) -> Opts {
    Opts {
        bridge_faults: true,
        cross_fallback,
        bridge_row: true,
        ..Default::default()
    }
}

/// SPEC §19.15 R3 exit (1): bridge stub faults before `Started`, direct stand-in answers 200 →
/// one answer on the direct path, exactly one direct attempt, `bridge_state = drift` in `/ready`.
#[tokio::test]
async fn bridge_fault_falls_to_direct_path_once_and_marks_bridge_drift() {
    let (base, script) = mock(vec![text_reply("via direct")]).await;
    let tmp = tempfile::tempdir().unwrap();
    let st = state_with(
        &base,
        &tmp,
        vec![],
        ClaudeOauthMode::Prefer,
        faulting_bridge(true),
    );
    assert_eq!(st.thin.bridge().state().snapshot()["status"], "ok");

    let (status, _, body) = post(
        build_router(st.clone()),
        json!({"model":"claude/bridge","messages":[{"role":"user","content":"hi"}]}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["choices"][0]["message"]["content"], "via direct");
    assert_eq!(script.seen.lock().unwrap().len(), 1, "one direct attempt");
    assert_eq!(st.thin.claude().state.last_path(), Some("oauth"));
    assert!(!st.thin.claude().state.is_blocked());

    let v = ready(build_router(st.clone())).await;
    let c = &v["thin"]["vendors"]["claude"];
    let reason = c["bridge"]["drift"]["reason"]
        .as_str()
        .expect("bridge drift in /ready");
    assert!(reason.starts_with("vendor_down: "), "{reason}");
    assert!(reason.contains("claude.exe not found"), "{reason}");
    assert!(c["bridge"]["drift"]["since"].is_string());
    assert_eq!(
        c["path"], "oauth",
        "direct open: the bridge drift does not change the plan"
    );
    assert_eq!(
        c["available"], true,
        "#110: bridge drift alone leaves the direct path serving: {c}"
    );

    // The direct row is untouched by the bridge: still one direct request per turn.
    let (base2, script2) = mock(vec![text_reply("direct row")]).await;
    let st2 = state_with(
        &base2,
        &tmp,
        vec![],
        ClaudeOauthMode::Prefer,
        faulting_bridge(true),
    );
    let (status, _, body) = post(
        build_router(st2),
        json!({"model":"claude/sonnet","messages":[{"role":"user","content":"hi"}]}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(script2.seen.lock().unwrap().len(), 1);
}

/// SPEC §19.15 R3 exit (2): direct blocked (probe found the third-party 400) and the bridge
/// faults → the bridge's error with the combined reason; no direct attempt is spent; `/ready`
/// `path_reason` names both faults.
#[tokio::test]
async fn direct_blocked_and_bridge_fault_is_the_named_double_fault() {
    let (base, script) = mock(vec![third_party_400()]).await;
    let tmp = tempfile::tempdir().unwrap();
    let st = state_with(
        &base,
        &tmp,
        vec![],
        ClaudeOauthMode::Prefer,
        faulting_bridge(true),
    );
    let claude = st.thin.claude().clone();
    assert_eq!(claude.probe_once().await, ProbeResult::Blocked);
    assert_eq!(script.seen.lock().unwrap().len(), 1);

    let (status, _, body) = post(
        build_router(st.clone()),
        json!({"model":"claude/sonnet","messages":[{"role":"user","content":"hi"}]}),
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(
        v["error"]["code"], "vendor_down",
        "the bridge's error is returned"
    );
    assert_eq!(v["error"]["vendor"], "claude");
    let message = v["error"]["message"].as_str().unwrap();
    assert!(message.starts_with("direct blocked: "), "{message}");
    assert!(message.contains("third-party"), "{message}");
    assert!(message.contains("; bridge drift: "), "{message}");
    assert!(message.contains("claude.exe not found"), "{message}");
    assert_eq!(
        script.seen.lock().unwrap().len(),
        1,
        "blocked: no direct attempt was spent"
    );
    assert!(claude.state.is_blocked(), "the block stands");

    let v = ready(build_router(st)).await;
    let c = &v["thin"]["vendors"]["claude"];
    assert_eq!(c["path"], "bridge", "{c}");
    let reason = c["path_reason"].as_str().unwrap();
    assert!(
        reason.starts_with("direct blocked; bridge drift: "),
        "{reason}"
    );
    assert!(c["bridge"]["drift"]["reason"].is_string(), "{c}");
    // #110: the double fault is the one Claude state a client must not spawn into.
    assert_eq!(c["available"], false, "{c}");
    assert!(c["retry_in_seconds"].is_null(), "{c}");
    assert!(
        c["reason"].as_str().unwrap().starts_with("bridge drift: "),
        "{c}"
    );
}

/// The block discovered by a user request (probe off) replays through the bridge; when that
/// replay faults, the request has used its two attempts and gets the combined reason.
#[tokio::test]
async fn block_on_request_then_bridge_fault_returns_combined_reason() {
    let (base, script) = mock(vec![third_party_400()]).await;
    let tmp = tempfile::tempdir().unwrap();
    let st = state_with(
        &base,
        &tmp,
        vec![],
        ClaudeOauthMode::Prefer,
        faulting_bridge(true),
    );
    let (status, headers, body) = post(
        build_router(st.clone()),
        json!({"model":"claude/sonnet","messages":[{"role":"user","content":"hi"}]}),
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert_eq!(headers["x-thickapi-path"], "bridge;reason=blocked");
    assert_eq!(headers["x-thickapi-fallback"], "oauth->bridge");
    let v: Value = serde_json::from_str(&body).unwrap();
    let message = v["error"]["message"].as_str().unwrap();
    assert!(message.starts_with("direct blocked: "), "{message}");
    assert!(message.contains("; bridge drift: "), "{message}");
    assert_eq!(
        script.seen.lock().unwrap().len(),
        1,
        "at most two attempts, one per path"
    );
    assert!(st.thin.claude().state.is_blocked());
}

/// `claude_oauth.cross_fallback: false` restores the pre-R3 behaviour: the bridge's own error,
/// no direct attempt; `bridge_state` still records the drift.
#[tokio::test]
async fn cross_fallback_off_returns_the_bridge_error_without_a_direct_attempt() {
    let (base, script) = mock(vec![text_reply("never")]).await;
    let tmp = tempfile::tempdir().unwrap();
    let st = state_with(
        &base,
        &tmp,
        vec![],
        ClaudeOauthMode::Prefer,
        faulting_bridge(false),
    );
    let (status, _, body) = post(
        build_router(st.clone()),
        json!({"model":"claude/bridge","messages":[{"role":"user","content":"hi"}]}),
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["error"]["code"], "vendor_down");
    assert!(
        !v["error"]["message"]
            .as_str()
            .unwrap()
            .contains("direct blocked"),
        "{body}"
    );
    assert!(script.seen.lock().unwrap().is_empty(), "no direct attempt");
    assert!(st.thin.bridge().state().is_drifted());

    // `mode: off` never touches the token either, whatever `cross_fallback` says.
    let off = tempfile::tempdir().unwrap();
    let st = state_with(
        &base,
        &off,
        vec![],
        ClaudeOauthMode::Off,
        faulting_bridge(true),
    );
    let (status, _, _) = post(
        build_router(st),
        json!({"model":"claude/sonnet","messages":[{"role":"user","content":"hi"}]}),
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert!(
        script.seen.lock().unwrap().is_empty(),
        "off: no direct attempt"
    );
}

fn novel_wording_400() -> (u16, String, Vec<(&'static str, &'static str)>) {
    (
        400,
        json!({"type":"error","error":{"type":"invalid_request_error","message":"This workspace is not eligible for the subscription API."}}).to_string(),
        vec![],
    )
}

/// SPEC §19.8.1: a non-Haiku 400 with novel wording replays on the bridge; the second consecutive
/// one marks the direct path blocked.
#[tokio::test]
async fn novel_wording_400_replays_on_bridge_and_second_marks_blocked() {
    let (base, script) = mock(vec![novel_wording_400(), novel_wording_400()]).await;
    let tmp = tempfile::tempdir().unwrap();
    let st = state_with(
        &base,
        &tmp,
        vec![],
        ClaudeOauthMode::Prefer,
        faulting_bridge(true),
    );

    let (status, _, body) = post(
        build_router(st.clone()),
        json!({"model":"claude/sonnet","messages":[{"role":"user","content":"hi"}]}),
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["error"]["code"], "vendor_down");
    assert!(
        !v["error"]["message"]
            .as_str()
            .unwrap()
            .starts_with("direct blocked:"),
        "first suspected block is not yet marked: {body}"
    );
    assert!(
        !st.thin.claude().state.is_blocked(),
        "first novel 400 does not mark blocked"
    );
    assert_eq!(script.seen.lock().unwrap().len(), 1);

    let (status, _, body) = post(
        build_router(st.clone()),
        json!({"model":"claude/sonnet","messages":[{"role":"user","content":"hi"}]}),
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    let v: Value = serde_json::from_str(&body).unwrap();
    let message = v["error"]["message"].as_str().unwrap();
    assert!(message.starts_with("direct blocked: "), "{message}");
    assert!(
        st.thin.claude().state.is_blocked(),
        "second consecutive suspected block marks blocked"
    );
    assert_eq!(script.seen.lock().unwrap().len(), 2);
}

fn chain_tools() -> Value {
    json!([
        {"type":"function","function":{"name":"read","parameters":{"type":"object","properties":{"path":{"type":"string"}},"required":["path"]}}},
        {"type":"function","function":{"name":"bash","parameters":{"type":"object","properties":{"command":{"type":"string"}},"required":["command"]}}}
    ])
}

fn assert_args_json_string(call: &Value) {
    let args = call["function"]["arguments"]
        .as_str()
        .expect("function.arguments must be a JSON string");
    let parsed: Value = serde_json::from_str(args).expect("function.arguments must parse as JSON");
    assert!(
        parsed.is_object(),
        "function.arguments JSON must be an object: {parsed}"
    );
}

fn assert_usage_nonzero(v: &Value) {
    assert!(
        v["usage"]["prompt_tokens"].as_u64().unwrap_or(0) > 0
            && v["usage"]["completion_tokens"].as_u64().unwrap_or(0) > 0,
        "usage must be non-zero: {}",
        v["usage"]
    );
}

fn two_parallel_tools_reply() -> (u16, String, Vec<(&'static str, &'static str)>) {
    (
        200,
        sse(&[
            (
                "message_start",
                json!({"type":"message_start","message":{"id":"m","model":"claude-sonnet-4-5","usage":{"input_tokens":11,"output_tokens":1}}}),
            ),
            (
                "content_block_start",
                json!({"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}),
            ),
            (
                "content_block_delta",
                json!({"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"Need both tools."}}),
            ),
            (
                "content_block_delta",
                json!({"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"sig-h9"}}),
            ),
            (
                "content_block_stop",
                json!({"type":"content_block_stop","index":0}),
            ),
            (
                "content_block_start",
                json!({"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_read","name":"read","input":{}}}),
            ),
            (
                "content_block_delta",
                json!({"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"path\":\"a\"}"}}),
            ),
            (
                "content_block_stop",
                json!({"type":"content_block_stop","index":1}),
            ),
            (
                "content_block_start",
                json!({"type":"content_block_start","index":2,"content_block":{"type":"tool_use","id":"toolu_bash","name":"bash","input":{}}}),
            ),
            (
                "content_block_delta",
                json!({"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"{\"command\":\"ls\"}"}}),
            ),
            (
                "content_block_stop",
                json!({"type":"content_block_stop","index":2}),
            ),
            (
                "message_delta",
                json!({"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":14}}),
            ),
            ("message_stop", json!({"type":"message_stop"})),
        ]),
        vec![],
    )
}

fn one_more_tool_reply() -> (u16, String, Vec<(&'static str, &'static str)>) {
    (
        200,
        sse(&[
            (
                "message_start",
                json!({"type":"message_start","message":{"id":"m2","model":"claude-sonnet-4-5","usage":{"input_tokens":21,"output_tokens":1}}}),
            ),
            (
                "content_block_start",
                json!({"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}),
            ),
            (
                "content_block_delta",
                json!({"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"One more read."}}),
            ),
            (
                "content_block_delta",
                json!({"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"sig-h9b"}}),
            ),
            (
                "content_block_stop",
                json!({"type":"content_block_stop","index":0}),
            ),
            (
                "content_block_start",
                json!({"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_read2","name":"read","input":{}}}),
            ),
            (
                "content_block_delta",
                json!({"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"path\":\"b\"}"}}),
            ),
            (
                "content_block_stop",
                json!({"type":"content_block_stop","index":1}),
            ),
            (
                "message_delta",
                json!({"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":8}}),
            ),
            ("message_stop", json!({"type":"message_stop"})),
        ]),
        vec![],
    )
}

fn thinking_then_tool_reply() -> (u16, String, Vec<(&'static str, &'static str)>) {
    (
        200,
        sse(&[
            (
                "message_start",
                json!({"type":"message_start","message":{"id":"m","model":"claude-sonnet-4-5","usage":{"input_tokens":9,"output_tokens":1}}}),
            ),
            (
                "content_block_start",
                json!({"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}),
            ),
            (
                "content_block_delta",
                json!({"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"Need a file."}}),
            ),
            (
                "content_block_delta",
                json!({"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"sig-abc"}}),
            ),
            (
                "content_block_stop",
                json!({"type":"content_block_stop","index":0}),
            ),
            (
                "content_block_start",
                json!({"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_1","name":"read","input":{}}}),
            ),
            (
                "content_block_delta",
                json!({"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"path\":\"a\"}"}}),
            ),
            (
                "content_block_stop",
                json!({"type":"content_block_stop","index":1}),
            ),
            (
                "message_delta",
                json!({"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":8}}),
            ),
            ("message_stop", json!({"type":"message_stop"})),
        ]),
        vec![],
    )
}

fn final_text_reply() -> (u16, String, Vec<(&'static str, &'static str)>) {
    text_reply("done")
}

#[tokio::test]
async fn tool_chain_replays_signed_thinking_on_the_second_hop() {
    let (base, script) = mock(vec![thinking_then_tool_reply(), final_text_reply()]).await;
    let tmp = tempfile::tempdir().unwrap();
    let app1 = app(&base, &tmp, vec![], ClaudeOauthMode::Only);
    let (status, _, body) = post(
        app1,
        json!({
            "model": "subs/claude/sonnet",
            "reasoning_effort": "high",
            "messages": [{"role":"user","content":"read a"}],
            "tools": [{"type":"function","function":{"name":"read","parameters":{"type":"object","properties":{"path":{"type":"string"}}}}}]
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["choices"][0]["finish_reason"], "tool_calls");
    assert_eq!(
        v["choices"][0]["message"]["reasoning_content"],
        "Need a file."
    );
    let details = &v["choices"][0]["message"]["reasoning_details"][0];
    assert_eq!(details["type"], "reasoning.text");
    assert_eq!(details["text"], "Need a file.");
    assert_eq!(details["signature"], "sig-abc");
    let call_id = v["choices"][0]["message"]["tool_calls"][0]["id"]
        .as_str()
        .unwrap()
        .to_string();

    let app2 = app(&base, &tmp, vec![], ClaudeOauthMode::Only);
    let (status, _, body) = post(
        app2,
        json!({
            "model": "subs/claude/sonnet",
            "reasoning_effort": "high",
            "messages": [
                {"role":"user","content":"read a"},
                {"role":"assistant","content":null,
                 "tool_calls":[{"id":call_id,"type":"function","function":{"name":"read","arguments":"{\"path\":\"a\"}"}}],
                 "reasoning_details":[{"type":"reasoning.text","text":"Need a file.","signature":"sig-abc"}]
                },
                {"role":"tool","tool_call_id":call_id,"content":"file body"}
            ],
            "tools": [{"type":"function","function":{"name":"read","parameters":{"type":"object","properties":{"path":{"type":"string"}}}}}]
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let seen = script.seen.lock().unwrap();
    assert_eq!(seen.len(), 2);
    let hop2 = &seen[1]["messages"];
    assert_eq!(hop2[1]["content"][0]["type"], "thinking");
    assert_eq!(hop2[1]["content"][0]["thinking"], "Need a file.");
    assert_eq!(hop2[1]["content"][0]["signature"], "sig-abc");
    assert_eq!(hop2[1]["content"][1]["type"], "tool_use");
}

#[tokio::test]
async fn tool_chain_without_signature_sends_disabled_and_ignored_header() {
    let (base, script) = mock(vec![final_text_reply()]).await;
    let tmp = tempfile::tempdir().unwrap();
    let (status, headers, body) = post(
        app(&base, &tmp, vec![], ClaudeOauthMode::Only),
        json!({
            "model": "subs/claude/sonnet",
            "reasoning_effort": "high",
            "messages": [
                {"role":"user","content":"read a"},
                {"role":"assistant","content":null,
                 "tool_calls":[{"id":"call_1","type":"function","function":{"name":"read","arguments":"{}"}}]
                },
                {"role":"tool","tool_call_id":"call_1","content":"ok"}
            ],
            "tools": [{"type":"function","function":{"name":"read","parameters":{"type":"object"}}}]
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(headers["x-thickapi-ignored"], "thinking");
    let seen = script.seen.lock().unwrap();
    assert_eq!(seen[0]["thinking"]["type"], "disabled");
}

#[tokio::test]
async fn parallel_tool_chain_replays_reasoning_details_and_pairs_ids() {
    let (base, script) = mock(vec![
        two_parallel_tools_reply(),
        one_more_tool_reply(),
        final_text_reply(),
    ])
    .await;
    let tmp = tempfile::tempdir().unwrap();
    let details = json!([{"type":"reasoning.text","text":"Need both tools.","signature":"sig-h9"}]);

    let (status, _, body) = post(
        app(&base, &tmp, vec![], ClaudeOauthMode::Only),
        json!({
            "model": "subs/claude/sonnet",
            "reasoning_effort": "high",
            "messages": [{"role":"user","content":"read a and ls"}],
            "tools": chain_tools()
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let hop1: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(hop1["choices"][0]["finish_reason"], "tool_calls");
    assert_usage_nonzero(&hop1);
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
    assert_eq!(
        hop1["choices"][0]["message"]["reasoning_details"][0]["type"],
        "reasoning.text"
    );
    assert_eq!(
        hop1["choices"][0]["message"]["reasoning_details"][0]["signature"],
        "sig-h9"
    );

    let (status, _, body) = post(
        app(&base, &tmp, vec![], ClaudeOauthMode::Only),
        json!({
            "model": "subs/claude/sonnet",
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
                {"role":"tool","tool_call_id":id_bash,"content":"a\nb"}
            ],
            "tools": chain_tools()
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let hop2: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(hop2["choices"][0]["finish_reason"], "tool_calls");
    assert_usage_nonzero(&hop2);
    let hop2_calls = hop2["choices"][0]["message"]["tool_calls"]
        .as_array()
        .unwrap();
    assert_eq!(hop2_calls.len(), 1);
    assert_eq!(hop2_calls[0]["function"]["name"], "read");
    assert_args_json_string(&hop2_calls[0]);
    let id_read2 = hop2_calls[0]["id"].as_str().unwrap().to_string();

    {
        let seen = script.seen.lock().unwrap();
        assert_eq!(seen.len(), 2, "{seen:?}");
        let hop2_msgs = seen[1]["messages"].as_array().unwrap();
        let assistant = hop2_msgs.iter().find(|m| m["role"] == "assistant").unwrap();
        assert_eq!(assistant["content"][0]["type"], "thinking");
        assert_eq!(assistant["content"][0]["signature"], "sig-h9");
        let uses: Vec<&Value> = assistant["content"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|b| b["type"] == "tool_use")
            .collect();
        assert_eq!(uses.len(), 2);
        assert_eq!(uses[0]["id"], id_read);
        assert_eq!(uses[1]["id"], id_bash);
        let results: Vec<&Value> = hop2_msgs
            .iter()
            .filter(|m| m["role"] == "user")
            .flat_map(|m| m["content"].as_array().into_iter().flatten())
            .filter(|b| b["type"] == "tool_result")
            .collect();
        assert_eq!(results.len(), 2, "{hop2_msgs:?}");
        let result_ids: Vec<&str> = results
            .iter()
            .map(|r| r["tool_use_id"].as_str().unwrap())
            .collect();
        assert!(result_ids.contains(&id_read.as_str()), "{result_ids:?}");
        assert!(result_ids.contains(&id_bash.as_str()), "{result_ids:?}");
    }

    let (status, _, body) = post(
        app(&base, &tmp, vec![], ClaudeOauthMode::Only),
        json!({
            "model": "subs/claude/sonnet",
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
                {"role":"tool","tool_call_id":id_bash,"content":"a\nb"},
                {"role":"assistant","content":null,
                 "tool_calls":[{"id":id_read2,"type":"function","function":{"name":"read","arguments":"{\"path\":\"b\"}"}}],
                 "reasoning_details":[{"type":"reasoning.text","text":"One more read.","signature":"sig-h9b"}]
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
    assert_usage_nonzero(&hop3);
    let seen = script.seen.lock().unwrap();
    assert_eq!(seen.len(), 3);
    let hop3_msgs = seen[2]["messages"].as_array().unwrap();
    let last_results: Vec<&str> = hop3_msgs
        .iter()
        .filter(|m| m["role"] == "user")
        .flat_map(|m| m["content"].as_array().into_iter().flatten())
        .filter(|b| b["type"] == "tool_result")
        .filter_map(|b| b["tool_use_id"].as_str())
        .collect();
    assert!(
        last_results.contains(&id_read2.as_str()),
        "hop3 must pair the third call id: {last_results:?}"
    );
}

/// D3 deliverable 4: `/ready` and `/v1/models` answer the same OpenAI envelope as
/// `/v1/chat/completions` on auth failure — missing/unknown key is `401 authentication_error`;
/// a key present but lacking the scope (`turn:write` does not imply `models:read`) is
/// `403 permission_error`, not the old always-401 thick `{code,message}` shape. `/v1/agent/turn`
/// keeps that thick shape (out of scope, unchecked here).
#[tokio::test]
async fn ready_and_models_are_403_permission_error_for_a_turn_write_only_key() {
    const TURN_ONLY: &str = "ta_turn_only_e2e";
    let tmp = tempfile::tempdir().unwrap();
    let opts = Opts {
        extra_keys: vec![KeyEntry {
            id: "turn-only".into(),
            hash: hash_token(TURN_ONLY),
            scopes: vec![SCOPE_TURN_WRITE.into()],
            ..Default::default()
        }],
        ..Default::default()
    };
    let state = state_with(
        "http://127.0.0.1:9",
        &tmp,
        vec![],
        ClaudeOauthMode::Only,
        opts,
    );

    for (method, path) in [("GET", "/ready"), ("GET", "/v1/models")] {
        let response = build_router(state.clone())
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(path)
                    .header("authorization", format!("Bearer {TURN_ONLY}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN, "{method} {path}");
        let request_id = response
            .headers()
            .get("x-request-id")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        assert!(
            request_id.is_some(),
            "{method} {path} must carry x-request-id"
        );
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let v: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(
            v["error"]["code"], "permission_error",
            "{method} {path}: {v}"
        );
        assert_eq!(v["error"]["request_id"], request_id.unwrap(), "{v}");
        assert_marker(&v);

        // Missing key is still 401 authentication_error, same envelope.
        let response = build_router(state.clone())
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(path)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            StatusCode::UNAUTHORIZED,
            "{method} {path}"
        );
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let v: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(
            v["error"]["code"], "authentication_error",
            "{method} {path}: {v}"
        );
        assert_marker(&v);
    }
}

/// Auth used to wrap every sub-router's fallback, so a mistyped path read as a bad key.
#[tokio::test]
async fn an_unknown_path_is_404_not_401() {
    let tmp = tempfile::tempdir().unwrap();
    for (method, path) in [
        ("GET", "/v1/nope"),
        ("POST", "/v1/chat/completion"),
        ("GET", "/"),
    ] {
        let response = app("http://127.0.0.1:9", &tmp, vec![], ClaudeOauthMode::Only)
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(path)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND, "{method} {path}");
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let v: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["error"]["code"], "route_not_found", "{v}");
        assert_marker(&v);
    }
    // And the real routes are still guarded.
    let response = app("http://127.0.0.1:9", &tmp, vec![], ClaudeOauthMode::Only)
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/chat/completions")
                .body(Body::from("{}"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let v: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["error"]["code"], "authentication_error", "{v}");
    assert_marker(&v);
}

/// #136: the envelope `code` is repeated as the trailing ` [code=…]` of `message`, which is
/// all Pi keeps.
fn assert_marker(v: &Value) {
    let code = v["error"]["code"].as_str().unwrap();
    let message = v["error"]["message"].as_str().unwrap();
    assert!(message.ends_with(&format!(" [code={code}]")), "{v}");
}

/// A wrong method on a thin route keeps the envelope and axum's `Allow`, behind the key.
#[tokio::test]
async fn a_wrong_method_on_a_thin_route_is_405_in_the_envelope() {
    let tmp = tempfile::tempdir().unwrap();
    for (method, path, allow) in [
        ("GET", "/v1/chat/completions", "POST"),
        ("POST", "/v1/models", "GET,HEAD"),
    ] {
        let request = |auth: bool| {
            let mut b = Request::builder().method(method).uri(path);
            if auth {
                b = b.header("authorization", format!("Bearer {TOKEN}"));
            }
            b.body(Body::empty()).unwrap()
        };
        let response = app("http://127.0.0.1:9", &tmp, vec![], ClaudeOauthMode::Only)
            .oneshot(request(true))
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            StatusCode::METHOD_NOT_ALLOWED,
            "{method} {path}"
        );
        assert_eq!(response.headers()["allow"], allow, "{method} {path}");
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let v: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["error"]["code"], "method_not_allowed", "{v}");
        assert_marker(&v);
        let response = app("http://127.0.0.1:9", &tmp, vec![], ClaudeOauthMode::Only)
            .oneshot(request(false))
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            StatusCode::UNAUTHORIZED,
            "{method} {path}"
        );
    }
}

/// `thin.max_body_mib`: over it is a `413` in the envelope; `0` is off, not axum's 2 MiB.
#[tokio::test]
async fn max_body_mib_limits_the_body_and_zero_turns_it_off() {
    let tmp = tempfile::tempdir().unwrap();
    // Not JSON, so a body that gets past the limit stops at `400` without a vendor call.
    let three_mib = "x".repeat(3 * 1024 * 1024);
    let send = |max_body_mib: u64| {
        let opts = Opts {
            max_body_mib,
            ..Default::default()
        };
        let state = state_with(
            "http://127.0.0.1:9",
            &tmp,
            vec![],
            ClaudeOauthMode::Only,
            opts,
        );
        build_router(state).oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/chat/completions")
                .header("authorization", format!("Bearer {TOKEN}"))
                .body(Body::from(three_mib.clone()))
                .unwrap(),
        )
    };
    let response = send(1).await.unwrap();
    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let v: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["error"]["code"], "request_too_large", "{v}");
    assert_marker(&v);
    assert!(
        v["error"]["message"].as_str().unwrap().contains("1 MiB"),
        "{v}"
    );
    for off_or_roomy in [0, 4] {
        let response = send(off_or_roomy).await.unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{off_or_roomy}");
    }
}

// ---- SPEC §19.16: empty Claude tool stop ----

/// `message_start`, `end_turn` with no block, `message_stop`: the measured 23 Sep reply.
fn empty_stop_reply() -> (u16, String, Vec<(&'static str, &'static str)>) {
    (
        200,
        sse(&[
            (
                "message_start",
                json!({"type":"message_start","message":{"id":"m0","model":"claude-sonnet-4-5","usage":{"input_tokens":99,"output_tokens":1}}}),
            ),
            (
                "message_delta",
                json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":3}}),
            ),
            ("message_stop", json!({"type":"message_stop"})),
        ]),
        vec![],
    )
}

fn stream_frames(body: &str) -> Vec<Value> {
    body.split("\n\n")
        .filter_map(|f| f.strip_prefix("data: "))
        .filter(|d| *d != "[DONE]")
        .map(|d| serde_json::from_str(d).unwrap())
        .collect()
}

#[tokio::test]
async fn empty_tool_stop_is_retried_once_on_the_same_path() {
    let (base, script) = mock(vec![empty_stop_reply(), one_more_tool_reply()]).await;
    let tmp = tempfile::tempdir().unwrap();
    let st = state(&base, &tmp, vec![], ClaudeOauthMode::Only);
    let (status, headers, body) = post(
        build_router(st.clone()),
        json!({"model":"subs/claude/sonnet","messages":[{"role":"user","content":"read b"}],
               "tools": chain_tools(), "stream": true, "stream_options": {"include_usage": true}}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(headers.get("x-thickapi-fallback").is_none());
    assert!(body.trim_end().ends_with("data: [DONE]"), "{body}");
    let frames = stream_frames(&body);
    assert!(frames
        .iter()
        .any(|f| f["choices"][0]["delta"]["tool_calls"][0]["function"]["name"] == "read"));
    assert!(frames
        .iter()
        .any(|f| f["choices"][0]["finish_reason"] == "tool_calls"));
    assert!(!frames
        .iter()
        .any(|f| f["choices"][0]["finish_reason"] == "stop"));
    // Only the retry's usage: the dropped attempt's 99 input tokens are gone.
    assert_eq!(frames.last().unwrap()["usage"]["prompt_tokens"], 21);
    assert_eq!(script.seen.lock().unwrap().len(), 2);
    // #126: the dropped empty attempt is not a success; only the kept retry is.
    assert_eq!(st.thin.claude().state.successes(), 1);
}

#[tokio::test]
async fn two_empty_tool_stops_are_502_empty_completion() {
    let (base, script) = mock(vec![empty_stop_reply(), empty_stop_reply()]).await;
    let tmp = tempfile::tempdir().unwrap();
    let st = state(&base, &tmp, vec![], ClaudeOauthMode::Only);
    let (status, _, body) = post(
        build_router(st.clone()),
        json!({"model":"subs/claude/sonnet","messages":[{"role":"user","content":"x"}],"tools": chain_tools()}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_GATEWAY, "{body}");
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["error"]["type"], "server_error");
    assert_eq!(v["error"]["code"], "empty_completion");
    assert_eq!(v["error"]["should_retry"], false);
    assert!(v["error"]["message"]
        .as_str()
        .unwrap()
        .contains("claude-sonnet-4-5: empty stop while tools were offered"));
    assert_eq!(
        script.seen.lock().unwrap().len(),
        2,
        "one retry, not a loop"
    );
    // #126: neither dropped attempt counts as a direct-path success.
    assert_eq!(st.thin.claude().state.successes(), 0);
}

#[tokio::test]
async fn two_empty_tool_stops_streamed_end_in_the_error_frame_without_done() {
    let (base, _) = mock(vec![empty_stop_reply(), empty_stop_reply()]).await;
    let tmp = tempfile::tempdir().unwrap();
    let (_, _, body) = post(
        app(&base, &tmp, vec![], ClaudeOauthMode::Only),
        json!({"model":"subs/claude/sonnet","messages":[{"role":"user","content":"x"}],
               "tools": chain_tools(), "stream": true}),
    )
    .await;
    assert!(!body.contains("[DONE]"), "{body}");
    let frames = stream_frames(&body);
    assert!(!frames
        .iter()
        .any(|f| !f["choices"][0]["finish_reason"].is_null()));
    assert!(!frames
        .iter()
        .any(|f| f["choices"][0]["delta"]["role"] == "assistant"));
    assert_eq!(frames.last().unwrap()["error"]["code"], "empty_completion");
}

#[tokio::test]
async fn text_without_a_tool_call_is_a_normal_stop() {
    let (base, script) = mock(vec![text_reply("No.")]).await;
    let tmp = tempfile::tempdir().unwrap();
    let (status, _, body) = post(
        app(&base, &tmp, vec![], ClaudeOauthMode::Only),
        json!({"model":"subs/claude/sonnet","messages":[{"role":"user","content":"x"}],"tools": chain_tools()}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["choices"][0]["finish_reason"], "stop");
    assert_eq!(v["choices"][0]["message"]["content"], "No.");
    assert_eq!(script.seen.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn empty_stop_without_tools_is_unchanged() {
    let (base, script) = mock(vec![empty_stop_reply()]).await;
    let tmp = tempfile::tempdir().unwrap();
    let (status, _, body) = post(
        app(&base, &tmp, vec![], ClaudeOauthMode::Only),
        json!({"model":"subs/claude/sonnet","messages":[{"role":"user","content":"x"}]}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["choices"][0]["finish_reason"], "stop");
    assert_eq!(script.seen.lock().unwrap().len(), 1);
}

// ---- #145: bridge closes a text block left open at message_stop ----

/// thickapi-core's `fake-claude` stub. A dependency's binary has no `CARGO_BIN_EXE_*`, so
/// build it (a no-op when fresh) into this test's own target dir and take it from there.
fn fake_claude() -> std::path::PathBuf {
    static BUILT: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
    BUILT
        .get_or_init(|| {
            // <target>/<profile>/deps/<this test>.exe
            let profile_dir = std::env::current_exe()
                .unwrap()
                .parent()
                .and_then(|deps| deps.parent())
                .unwrap()
                .to_path_buf();
            let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
            let mut build = std::process::Command::new(cargo);
            build
                .args(["build", "-p", "thickapi-core", "--bin", "fake-claude"])
                .current_dir(env!("CARGO_MANIFEST_DIR"));
            if profile_dir.file_name().is_some_and(|n| n == "release") {
                build.arg("--release");
            }
            let status = build.status().expect("cargo build fake-claude");
            assert!(status.success(), "cargo build fake-claude: {status}");
            let exe = profile_dir.join(format!("fake-claude{}", std::env::consts::EXE_SUFFIX));
            assert!(exe.is_file(), "{} missing after build", exe.display());
            exe
        })
        .clone()
}

#[tokio::test]
async fn bridge_open_text_block_streams_stop_and_done_and_counts_the_block() {
    let tmp = tempfile::tempdir().unwrap();
    let st = state_with(
        "http://127.0.0.1:9",
        &tmp,
        vec![],
        ClaudeOauthMode::Off,
        Opts {
            claude_path: Some(fake_claude()),
            wire_model: Some("fake-open-text-block"),
            ..Default::default()
        },
    );
    let before = ready(build_router(st.clone())).await;
    assert_eq!(
        before["thin"]["vendors"]["claude"]["bridge"]["skipped"]["blocks"], 0,
        "{before}"
    );
    let (status, headers, body) = post(
        build_router(st.clone()),
        json!({"model":"subs/claude/sonnet","messages":[{"role":"user","content":"Reply OK"}],
               "stream": true}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        headers["x-thickapi-path"]
            .to_str()
            .unwrap()
            .split(';')
            .next(),
        Some("bridge")
    );
    assert!(body.trim_end().ends_with("data: [DONE]"), "{body}");
    let frames = stream_frames(&body);
    assert!(!frames.iter().any(|f| f.get("error").is_some()), "{body}");
    let text: String = frames
        .iter()
        .filter_map(|f| f["choices"][0]["delta"]["content"].as_str())
        .collect();
    assert_eq!(text, "Hello from the bridge.");
    assert!(frames
        .iter()
        .any(|f| f["choices"][0]["finish_reason"] == "stop"));
    let after = ready(build_router(st)).await;
    assert_eq!(
        after["thin"]["vendors"]["claude"]["bridge"]["skipped"]["blocks"], 1,
        "{after}"
    );
}

// ---- K6: tool_choice and parallel_tool_calls on the bridge ----

fn pi_tools() -> Value {
    json!([
        {"type":"function","function":{"name":"read","description":"Read a file.",
          "parameters":{"type":"object","properties":{"path":{"type":"string"}}}}},
        {"type":"function","function":{"name":"edit","description":"Edit a file.",
          "parameters":{"type":"object","properties":{
            "path":{"type":"string"},
            "edits":{"type":"array","items":{"type":"object","properties":{
              "oldText":{"type":"string"},"newText":{"type":"string"}}}}
          }}}}
    ])
}

fn bridge_state(tmp: &tempfile::TempDir, wire: &'static str) -> AppState {
    state_with(
        "http://127.0.0.1:9",
        tmp,
        vec![],
        ClaudeOauthMode::Off,
        Opts {
            claude_path: Some(fake_claude()),
            wire_model: Some(wire),
            ..Default::default()
        },
    )
}

#[tokio::test]
async fn bridge_tool_choice_none_spawns_with_an_empty_catalog() {
    let tmp = tempfile::tempdir().unwrap();
    let (status, headers, body) = post(
        build_router(bridge_state(&tmp, "claude-sonnet-4-6")),
        json!({"model":"subs/claude/sonnet",
               "messages":[
                 {"role":"assistant","content":null,"tool_calls":[
                   {"id":"call_1","type":"function","function":{"name":"read","arguments":"{\"path\":\"a.txt\"}"}}]},
                 {"role":"tool","tool_call_id":"call_1","content":"hello"},
                 {"role":"user","content":"thanks"}],
               "tools":pi_tools(),"tool_choice":"none"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        headers["x-thickapi-path"]
            .to_str()
            .unwrap()
            .split(';')
            .next(),
        Some("bridge")
    );
    // An empty catalog writes no tools.json and spawns with no MCP server. The stub's
    // init then reports `tools: []`; a nonempty catalog would still answer this text turn,
    // so the assertion is the 200 plus the history that rendered (a missing historical name
    // would have been a 400 before the spawn).
    let _ = tmp;
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(
        v["choices"][0]["message"]["content"],
        "Hello from the bridge."
    );
}

#[tokio::test]
async fn bridge_tool_choice_required_retries_and_hides_the_text() {
    let tmp = tempfile::tempdir().unwrap();
    let (status, _, body) = post(
        build_router(bridge_state(&tmp, "fake-text-then-tool")),
        json!({"model":"subs/claude/sonnet","messages":[{"role":"user","content":"read a.txt"}],
               "tools":pi_tools(),"tool_choice":"required","stream":true}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let frames = stream_frames(&body);
    assert!(!frames.iter().any(|f| f.get("error").is_some()), "{body}");
    let text: String = frames
        .iter()
        .filter_map(|f| f["choices"][0]["delta"]["content"].as_str())
        .collect();
    assert!(
        text.is_empty(),
        "the first attempt's text must not stream: {text}"
    );
    let names: Vec<&str> = frames
        .iter()
        .filter_map(|f| f["choices"][0]["delta"]["tool_calls"][0]["function"]["name"].as_str())
        .collect();
    assert_eq!(names, ["edit"], "{body}");
    assert!(frames
        .iter()
        .any(|f| f["choices"][0]["finish_reason"] == "tool_calls"));
}

#[tokio::test]
async fn bridge_tool_choice_required_twice_is_empty_completion() {
    let tmp = tempfile::tempdir().unwrap();
    let (status, _, body) = post(
        build_router(bridge_state(&tmp, "fake-text-always")),
        json!({"model":"subs/claude/sonnet","messages":[{"role":"user","content":"read a.txt"}],
               "tools":pi_tools(),"tool_choice":"required"}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_GATEWAY, "{body}");
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["error"]["type"], "server_error");
    assert_eq!(v["error"]["code"], "empty_completion");
    assert_eq!(
        v["error"]["message"],
        "tool_choice required: no tool call after retry [code=empty_completion]"
    );
    assert!(
        !body.contains("No."),
        "held text must not reach the client: {body}"
    );
}

#[tokio::test]
async fn bridge_named_tool_choice_offers_one_dialect_tool() {
    let tmp = tempfile::tempdir().unwrap();
    let (status, _, body) = post(
        build_router(bridge_state(&tmp, "fake-text-then-tool")),
        json!({"model":"subs/claude/sonnet","messages":[{"role":"user","content":"edit it"}],
               "tools":pi_tools(),
               "tool_choice":{"type":"function","function":{"name":"edit"}}}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    // The private dir is removed when the request ends. The one-tool catalog
    // (`mcp__pi__Edit`) is asserted in `catalog::named_choice_offers_one_dialect_tool`.
    let _ = tmp;
}

#[tokio::test]
async fn bridge_unknown_tool_choice_is_a_400() {
    let tmp = tempfile::tempdir().unwrap();
    let (status, _, body) = post(
        build_router(bridge_state(&tmp, "claude-sonnet-4-6")),
        json!({"model":"subs/claude/sonnet","messages":[{"role":"user","content":"x"}],
               "tools":pi_tools(),
               "tool_choice":{"type":"function","function":{"name":"missing"}}}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["error"]["param"], "tool_choice");
}

#[tokio::test]
async fn bridge_parallel_false_counts_the_dropped_call() {
    let tmp = tempfile::tempdir().unwrap();
    // The recorded turn calls `mcp__pi__read` then `mcp__pi__bash`. Dialect off keeps those
    // names; dialect on would offer `Read` and `Bash` and the fixture would miss init.
    let st = state_with(
        "http://127.0.0.1:9",
        &tmp,
        vec![],
        ClaudeOauthMode::Off,
        Opts {
            claude_path: Some(fake_claude()),
            wire_model: Some("fake-two-tool-turn"),
            dialect: Some("off"),
            ..Default::default()
        },
    );
    let (status, _, body) = post(
        build_router(st.clone()),
        json!({"model":"subs/claude/sonnet","messages":[{"role":"user","content":"both"}],
               "tools":[pi_tools()[0].clone(),
                 {"type":"function","function":{"name":"bash","parameters":{"type":"object","properties":{"command":{"type":"string"}}}}}],
               "parallel_tool_calls":false}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let v: Value = serde_json::from_str(&body).unwrap();
    let calls = v["choices"][0]["message"]["tool_calls"].as_array().unwrap();
    assert_eq!(calls.len(), 1, "{body}");
    assert_eq!(calls[0]["function"]["name"], "read");
    let after = ready(build_router(st)).await;
    assert_eq!(
        after["thin"]["vendors"]["claude"]["bridge"]["skipped"]["parallel"], 1,
        "{after}"
    );
}

// ---- C3: Claude direct through `VendorSession::run` ----

/// C3 (a), R5 regression guard: the probe sends through the runner, so it is bounded by
/// `thin.reply_timeout_seconds` (#125). A stand-in that accepts and never answers ends the
/// probe as `error(vendor_down)` within the bound, leaves the state alone, reschedules the
/// probe `probe.retry_minutes` out, and releases its `in_flight` permit (with one permit, a
/// second probe still runs rather than waiting for good).
#[tokio::test]
async fn a_stalled_probe_ends_as_vendor_down_and_the_loop_reschedules() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        let mut held = Vec::new();
        while let Ok((sock, _)) = listener.accept().await {
            held.push(sock);
        }
    });
    let tmp = tempfile::tempdir().unwrap();
    let st = state_with(
        &base,
        &tmp,
        vec![],
        ClaudeOauthMode::Prefer,
        Opts {
            reply_timeout_seconds: Some(1),
            oauth_in_flight: Some(1),
            ..Default::default()
        },
    );
    let claude = st.thin.claude().clone();
    let now = || {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
    };
    let retry = claude.probe_config().retry_minutes * 60;
    for _ in 0..2 {
        let (before, started) = (now(), std::time::Instant::now());
        let result = tokio::time::timeout(std::time::Duration::from_secs(10), claude.probe_once())
            .await
            .expect("the probe must give up on its own, and its permit must come back");
        assert_eq!(result, ProbeResult::Error("vendor_down".into()));
        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "{:?}",
            started.elapsed()
        );
        let next = claude.state.probe_next_at().expect("the loop reschedules");
        assert!(
            (before + retry..=now() + retry).contains(&next),
            "next={next} before={before} retry={retry}"
        );
        assert!(!claude.state.is_blocked(), "a stall is not a block");
    }
    let v = ready(build_router(st)).await;
    let oauth = &v["thin"]["vendors"]["claude"]["oauth"];
    assert_eq!(
        oauth["probe"]["last_result"], "error(vendor_down)",
        "{oauth}"
    );
    assert_eq!(oauth["status"], "ok", "{oauth}");
}

/// C3 (c): a `Blocked` classification made by the runner's `classify` still reaches
/// `ClaudeAdapter` as a block — exactly one direct attempt, exactly one bridge replay (the
/// `fake-claude` stub answers), `x-thickapi-fallback: oauth->bridge`, and the block marked.
#[tokio::test]
async fn a_blocked_classification_through_the_runner_replays_once_on_the_bridge() {
    let (base, script) = mock(vec![third_party_400(), text_reply("never")]).await;
    let tmp = tempfile::tempdir().unwrap();
    let st = state_with(
        &base,
        &tmp,
        vec![],
        ClaudeOauthMode::Prefer,
        Opts {
            claude_path: Some(fake_claude()),
            ..Default::default()
        },
    );
    let (status, headers, body) = post(
        build_router(st.clone()),
        json!({"model":"claude/sonnet","messages":[{"role":"user","content":"hi"}]}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(headers["x-thickapi-fallback"], "oauth->bridge");
    assert_eq!(
        headers["x-thickapi-path"]
            .to_str()
            .unwrap()
            .split(';')
            .next(),
        Some("bridge")
    );
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(
        v["choices"][0]["message"]["content"], "Hello from the bridge.",
        "one bridge answer, not two spliced: {body}"
    );
    assert_eq!(
        script.seen.lock().unwrap().len(),
        1,
        "one direct attempt; the block is not retried on the direct path"
    );
    let claude = st.thin.claude();
    assert!(claude.state.is_blocked(), "the block is marked");
    assert_eq!(claude.state.last_path(), Some("bridge"));
    let v = ready(build_router(st)).await;
    let c = &v["thin"]["vendors"]["claude"];
    assert_eq!(c["oauth"]["last_rejection"], "blocked", "{c}");
    assert_eq!(c["bridge"]["status"], "ok", "{c}");
}
