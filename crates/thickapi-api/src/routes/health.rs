use axum::{extract::State, Json};
use serde::Serialize;

use crate::AppState;

/// Liveness plus build identity, no auth (SPEC §1.7 Health, §3).
#[derive(Serialize)]
pub struct HealthResponse {
    pub status: &'static str,
    pub uptime_seconds: u64,
    pub version: &'static str,
    pub sha: &'static str,
}

pub async fn health(State(state): State<AppState>) -> Json<HealthResponse> {
    Json(HealthResponse {
        status: "ok",
        uptime_seconds: state.uptime_seconds(),
        version: crate::VERSION,
        sha: crate::GIT_SHA,
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use std::sync::Arc;

    use http_body_util::BodyExt;
    use tower::ServiceExt;

    use crate::{build_router, AppState};

    fn test_state() -> AppState {
        let config = thickapi_core::ConfigBundle {
            config_dir: std::path::PathBuf::from("."),
            router: thickapi_core::config::RouterConfig {
                listen: "127.0.0.1:8787".into(),
                data_dir: std::env::temp_dir().join("thickapi-test"),
                lan: false,
                default_timeout_seconds: 5400,
                queue_timeout_seconds: 300,
                max_queue_depth: 16,
                max_paid_usd_per_day: 5.0,
                drain_seconds: 120,
                refuse_paths: vec![],
                ..Default::default()
            },
            accounts: thickapi_core::config::AccountsConfig {
                pools: Default::default(),
            },
            models: thickapi_core::config::ModelsConfig {
                aliases: Default::default(),
                models: vec![],
            },
            combos: thickapi_core::config::CombosConfig {
                combos: Default::default(),
            },
            keys: thickapi_core::config::keys::KeysConfig::default(),
        };
        crate::server::make_state_with_store(
            config,
            Arc::new(thickapi_core::Store::open_in_memory().expect("store")),
        )
    }

    #[tokio::test]
    async fn health_returns_ok() {
        let app = build_router(test_state());
        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/health")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .expect("response");
        assert_eq!(response.status(), axum::http::StatusCode::OK);
        let body = response
            .into_body()
            .collect()
            .await
            .expect("body")
            .to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&body).expect("json");
        assert_eq!(json["status"], "ok");
        assert!(json["uptime_seconds"].is_number());
    }

    /// D1: `/health` names the build over a real socket — the Cargo version (which equals the
    /// SPEC version) and the git sha `build.rs` stamped (`unknown` without `.git`).
    #[tokio::test]
    async fn health_reports_version_and_sha_on_an_ephemeral_port() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, build_router(test_state())).await;
        });

        let mut sock = tokio::net::TcpStream::connect(addr).await.unwrap();
        sock.write_all(b"GET /health HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n")
            .await
            .unwrap();
        let mut raw = String::new();
        tokio::time::timeout(
            std::time::Duration::from_secs(10),
            sock.read_to_string(&mut raw),
        )
        .await
        .expect("answer within 10 s")
        .unwrap();
        server.abort();

        assert!(raw.starts_with("HTTP/1.1 200"), "{raw}");
        let body = &raw[raw.find("\r\n\r\n").expect("head/body split") + 4..];
        let json: serde_json::Value = serde_json::from_str(body).expect("json body");
        assert_eq!(json["status"], "ok");
        assert_eq!(json["version"], env!("CARGO_PKG_VERSION"));
        assert_eq!(json["version"], crate::VERSION);
        let sha = json["sha"].as_str().expect("sha is a string");
        assert_eq!(sha, crate::GIT_SHA);
        assert!(
            sha == "unknown" || (sha.len() >= 7 && sha.chars().all(|c| c.is_ascii_hexdigit())),
            "sha={sha}"
        );
        assert_eq!(
            crate::VERSION_LINE,
            format!("{} ({})", crate::VERSION, crate::GIT_SHA)
        );
    }
}
