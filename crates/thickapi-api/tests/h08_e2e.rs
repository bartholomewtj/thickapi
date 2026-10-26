#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
//! H8 (SPEC §1.7, §19.2, §19.7): drop-body abort, bridge concurrency 429, drain timeout.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::extract::State;
use axum::http::{Request, StatusCode};
use axum::response::Response;
use axum::Router;
use futures_util::stream;
use http_body_util::BodyExt;
use serde_json::{json, Value};
use thickapi_api::{build_router, make_state_with_store, serve_on, AppState};
use thickapi_core::config::keys::{hash_token, KeyEntry, KeysConfig, SCOPE_CHAT_WRITE};
use thickapi_core::config::models::{ModelEntry, ModelsConfig, Transport};
use thickapi_core::config::router::{
    ClaudeBridgeCfg, ClaudeCfg, ClaudeOauthConfig, ClaudeOauthMode, RouterConfig, ThinConfig,
    Vendors,
};
use thickapi_core::config::{AccountsConfig, CombosConfig};
use thickapi_core::types::BackendId;
use thickapi_core::{ConfigBundle, Store};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::Notify;
use tower::{Service, ServiceExt};

const TOKEN: &str = "ta_h08";

struct Disconnect(Arc<Notify>);
impl Drop for Disconnect {
    fn drop(&mut self) {
        self.0.notify_waiters();
    }
}

#[derive(Clone)]
struct Hang {
    disconnected: Arc<Notify>,
}

fn hang_sse() -> String {
    let events = [
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
    ];
    events
        .iter()
        .map(|(n, d)| format!("event: {n}\ndata: {d}\n\n"))
        .collect()
}

async fn hang_messages(State(h): State<Hang>) -> Response {
    enum Phase {
        First(String, Disconnect),
        Wait(Disconnect),
    }
    let first = hang_sse();
    let gone = Disconnect(h.disconnected);
    let body = stream::unfold(Phase::First(first, gone), |phase| async move {
        match phase {
            Phase::First(s, d) => Some((Ok::<_, Infallible>(s), Phase::Wait(d))),
            Phase::Wait(d) => {
                tokio::time::sleep(Duration::from_secs(3600)).await;
                Some((Ok(": keepalive\n\n".into()), Phase::Wait(d)))
            }
        }
    });
    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "text/event-stream")
        .body(Body::from_stream(body))
        .unwrap()
}

async fn hanging_mock() -> (String, Arc<Notify>) {
    let disconnected = Arc::new(Notify::new());
    let hang = Hang {
        disconnected: disconnected.clone(),
    };
    let app = Router::new()
        .route("/v1/messages", axum::routing::post(hang_messages))
        .with_state(hang);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr: SocketAddr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}"), disconnected)
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

struct Opts {
    mode: ClaudeOauthMode,
    bridge_row: bool,
    claude_path: Option<std::path::PathBuf>,
    bridge_max_concurrent: u32,
    queue_timeout_seconds: u64,
    mcp_ready_timeout_ms: u64,
    drain_seconds: u64,
}

impl Default for Opts {
    fn default() -> Self {
        Self {
            mode: ClaudeOauthMode::Only,
            bridge_row: false,
            claude_path: None,
            bridge_max_concurrent: 2,
            queue_timeout_seconds: 300,
            mcp_ready_timeout_ms: 5_000,
            drain_seconds: 120,
        }
    }
}

fn state(base_url: &str, tmp: &tempfile::TempDir, opts: Opts) -> AppState {
    let creds = tmp.path().join(".credentials.json");
    std::fs::write(
        &creds,
        r#"{"claudeAiOauth":{"accessToken":"sk-ant-oat01-h08","refreshToken":"r","expiresAt":9999999999999,"subscriptionType":"max"}}"#,
    )
    .unwrap();
    let config = ConfigBundle {
        config_dir: tmp.path().to_path_buf(),
        router: RouterConfig {
            data_dir: tmp.path().join("data"),
            queue_timeout_seconds: opts.queue_timeout_seconds,
            drain_seconds: opts.drain_seconds,
            claude_oauth: ClaudeOauthConfig {
                mode: opts.mode,
                base_url: Some(base_url.to_string()),
                credentials_path: Some(creds),
                probe: thickapi_core::config::router::ClaudeProbeConfig {
                    enabled: false,
                    ..Default::default()
                },
                ..Default::default()
            },
            thin: ThinConfig {
                vendors: Vendors {
                    claude: ClaudeCfg {
                        path: opts.claude_path,
                        bridge: ClaudeBridgeCfg {
                            max_concurrent: opts.bridge_max_concurrent,
                            mcp_ready_timeout_ms: opts.mcp_ready_timeout_ms,
                            ..Default::default()
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
            models: {
                let mut rows = vec![row("claude/sonnet", Transport::OauthDirect)];
                if opts.bridge_row {
                    rows.push(row("claude/bridge", Transport::CliBridge));
                }
                rows
            },
        },
        combos: CombosConfig {
            combos: Default::default(),
        },
        keys: KeysConfig {
            keys: vec![KeyEntry {
                id: "h08".into(),
                hash: hash_token(TOKEN),
                scopes: vec![SCOPE_CHAT_WRITE.into()],
                ..Default::default()
            }],
        },
    };
    make_state_with_store(config, Arc::new(Store::open_in_memory().unwrap()))
}

#[cfg(windows)]
fn hang_stub() -> std::path::PathBuf {
    use std::sync::OnceLock;
    static EXE: OnceLock<std::path::PathBuf> = OnceLock::new();
    EXE.get_or_init(|| {
        let dir = std::env::temp_dir();
        let src = dir.join("thickapi-h08-hang.rs");
        let exe = dir.join("thickapi-h08-hang.exe");
        std::fs::write(
            &src,
            r#"fn main() {
    if std::env::args().any(|a| a == "--version") {
        println!("hang 0.0.0");
        return;
    }
    loop { std::thread::sleep(std::time::Duration::from_secs(1)); }
}
"#,
        )
        .unwrap();
        let rustc = std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into());
        let status = std::process::Command::new(rustc)
            .args(["--edition", "2021", "-O"])
            .arg(&src)
            .arg("-o")
            .arg(&exe)
            .status()
            .expect("spawn rustc");
        assert!(status.success(), "rustc hang stub failed: {status}");
        exe
    })
    .clone()
}

fn chat_body(model: &str, stream: bool, tools: bool) -> Value {
    let mut body = json!({
        "model": model,
        "messages": [{"role":"user","content":"hi"}],
        "stream": stream,
    });
    if tools {
        body["tools"] = json!([{
            "type": "function",
            "function": {
                "name": "read",
                "parameters": {"type":"object","properties":{}}
            }
        }]);
    }
    body
}

async fn post(app: Router, body: Value) -> (StatusCode, axum::http::HeaderMap, String) {
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
async fn drop_mid_stream_closes_standin_within_1s() {
    let (base, disconnected) = hanging_mock().await;
    let tmp = tempfile::tempdir().unwrap();
    let mut app = build_router(state(&base, &tmp, Opts::default()));
    let req = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("authorization", format!("Bearer {TOKEN}"))
        .header("content-type", "application/json")
        .body(Body::from(
            chat_body("claude/sonnet", true, false).to_string(),
        ))
        .unwrap();
    let response = ServiceExt::<Request<Body>>::ready(&mut app)
        .await
        .unwrap()
        .call(req)
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let mut body = response.into_body();
    let frame = tokio::time::timeout(Duration::from_secs(5), body.frame())
        .await
        .expect("first SSE frame")
        .expect("frame")
        .expect("ok frame");
    assert!(frame.is_data(), "{frame:?}");
    drop(body);
    tokio::time::timeout(Duration::from_secs(1), disconnected.notified())
        .await
        .expect("stand-in saw the connection close within 1s");
}

#[cfg(windows)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn drop_mid_request_removes_bridge_private_dir() {
    let tmp = tempfile::tempdir().unwrap();
    let stub = hang_stub();
    let mut app = build_router(state(
        "http://127.0.0.1:1",
        &tmp,
        Opts {
            mode: ClaudeOauthMode::Off,
            bridge_row: true,
            claude_path: Some(stub),
            mcp_ready_timeout_ms: 20_000,
            ..Default::default()
        },
    ));
    let req = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("authorization", format!("Bearer {TOKEN}"))
        .header("content-type", "application/json")
        .body(Body::from(
            chat_body("claude/bridge", true, true).to_string(),
        ))
        .unwrap();
    let task = tokio::spawn(async move {
        ServiceExt::<Request<Body>>::ready(&mut app)
            .await
            .unwrap()
            .call(req)
            .await
    });
    let tmp_root = tmp.path().join("tmp");
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let present = tmp_root.is_dir()
            && std::fs::read_dir(&tmp_root)
                .map(|mut d| d.next().is_some())
                .unwrap_or(false);
        if present {
            break;
        }
        if Instant::now() > deadline {
            task.abort();
            panic!("private dir never appeared under {}", tmp_root.display());
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    task.abort();
    let _ = task.await;
    let gone = Instant::now() + Duration::from_secs(5);
    loop {
        let leftover: Vec<_> = std::fs::read_dir(&tmp_root)
            .map(|d| d.filter_map(|e| e.ok()).map(|e| e.path()).collect())
            .unwrap_or_default();
        if leftover.is_empty() {
            break;
        }
        if Instant::now() > gone {
            panic!("private dir survived drop: {leftover:?}");
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[cfg(windows)]
#[tokio::test]
async fn third_bridge_request_is_429_when_cap_is_two() {
    let tmp = tempfile::tempdir().unwrap();
    let stub = hang_stub();
    let app = build_router(state(
        "http://127.0.0.1:1",
        &tmp,
        Opts {
            mode: ClaudeOauthMode::Off,
            bridge_row: true,
            claude_path: Some(stub),
            bridge_max_concurrent: 2,
            queue_timeout_seconds: 1,
            mcp_ready_timeout_ms: 8_000,
            ..Default::default()
        },
    ));
    let body = chat_body("claude/bridge", false, true);
    let t1 = tokio::spawn(post(app.clone(), body.clone()));
    let t2 = tokio::spawn(post(app.clone(), body.clone()));
    let t3 = tokio::spawn(post(app, body));
    let (a, b, c) = tokio::join!(t1, t2, t3);
    let results = [a.unwrap(), b.unwrap(), c.unwrap()];
    let too_many = results
        .iter()
        .filter(|(s, _, _)| *s == StatusCode::TOO_MANY_REQUESTS)
        .count();
    assert_eq!(
        too_many,
        1,
        "expected one 429 among {:?}",
        results.iter().map(|(s, _, b)| (s, b)).collect::<Vec<_>>()
    );
    let (_, headers, body) = results
        .iter()
        .find(|(s, _, _)| *s == StatusCode::TOO_MANY_REQUESTS)
        .unwrap();
    assert_eq!(headers["retry-after"], "5");
    assert_eq!(headers["x-should-retry"], "true");
    let v: Value = serde_json::from_str(body).unwrap();
    assert_eq!(v["error"]["code"], "rate_limit_error");
    assert_eq!(v["error"]["type"], "rate_limit_error");
}

#[tokio::test]
async fn draining_refuses_thin_with_503() {
    let (base, _) = hanging_mock().await;
    let tmp = tempfile::tempdir().unwrap();
    let st = state(&base, &tmp, Opts::default());
    st.drain.cancel();
    let (status, _, body) = post(build_router(st), chat_body("claude/sonnet", false, false)).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["error"]["code"], "vendor_down");
    assert!(
        v["error"]["message"].as_str().unwrap().contains("draining"),
        "{body}"
    );
}

#[tokio::test]
async fn drain_with_stalled_client_exits_at_drain_seconds() {
    let (base, _) = hanging_mock().await;
    let tmp = tempfile::tempdir().unwrap();
    let st = state(
        &base,
        &tmp,
        Opts {
            drain_seconds: 1,
            ..Default::default()
        },
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    let serve = tokio::spawn(async move {
        serve_on(listener, st, async {
            let _ = shutdown_rx.await;
        })
        .await
        .unwrap();
    });

    let body = chat_body("claude/sonnet", true, false).to_string();
    let mut client = tokio::net::TcpStream::connect(addr).await.unwrap();
    let req = format!(
        "POST /v1/chat/completions HTTP/1.1\r\nHost: 127.0.0.1\r\nAuthorization: Bearer {TOKEN}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );
    client.write_all(req.as_bytes()).await.unwrap();
    let mut buf = vec![0u8; 2048];
    let n = tokio::time::timeout(Duration::from_secs(5), client.read(&mut buf))
        .await
        .expect("headers")
        .expect("read");
    let head = String::from_utf8_lossy(&buf[..n]);
    assert!(
        head.contains("200") || head.contains("text/event-stream") || head.contains("assistant"),
        "unexpected head: {head}"
    );

    let start = Instant::now();
    shutdown_tx.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(3), serve)
        .await
        .expect("serve exited at drain_seconds")
        .expect("join");
    let elapsed = start.elapsed();
    assert!(
        elapsed >= Duration::from_millis(700),
        "exited too fast: {elapsed:?}"
    );
    assert!(
        elapsed < Duration::from_secs(3),
        "held past drain_seconds: {elapsed:?}"
    );
    drop(client);
}
