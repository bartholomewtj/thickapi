#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
//! SPEC §19.16 on the bridge path: `ClaudeAdapter` (mode `off`) against the `fake-claude`
//! stub's empty-stop modes. Does not spawn the live `claude.exe` or bind the live daemon's port.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use serde_json::json;
use thickapi_core::config::models::{ModelEntry, ModelsConfig};
use thickapi_core::config::router::{ClaudeOauthConfig, ClaudeOauthMode, RouterConfig};
use thickapi_core::config::{AccountsConfig, CombosConfig};
use thickapi_core::thin::claude_bridge::{BridgeSettings, ClaudeBridge};
use thickapi_core::thin::oauth::state::VendorState;
use thickapi_core::thin::oauth::ClaudeAdapter;
use thickapi_core::thin::openai::FinishReason;
use thickapi_core::thin::{ModelAdapter, ThinEvent, ThinRequest};
use thickapi_core::thinking::Effort;
use thickapi_core::ConfigBundle;

fn adapter(tmp: &tempfile::TempDir, trace: bool) -> ClaudeAdapter {
    let bridge = ClaudeBridge::new(BridgeSettings {
        tmp_root: tmp.path().to_path_buf(),
        claude_path: Some(PathBuf::from(env!("CARGO_BIN_EXE_fake-claude"))),
        thickapi_exe: None,
        total_timeout: Duration::from_secs(15),
        idle_timeout: Duration::from_secs(10),
        mcp_ready_timeout: Duration::from_secs(5),
        trace,
        bridge_max_concurrent: 1,
        queue_timeout: Duration::from_millis(200),
        handoff_drain: BridgeSettings::default_handoff_drain(),
        extra_tools: vec![],
        nonempty_inventories: vec![],
        dialect: thickapi_core::thin::dialect::MODE_OFF.to_string(),
        catalogue_ids: vec![],
    });
    let cfg = ConfigBundle {
        config_dir: tmp.path().to_path_buf(),
        router: RouterConfig {
            data_dir: tmp.path().join("data"),
            claude_oauth: ClaudeOauthConfig {
                mode: ClaudeOauthMode::Off,
                credentials_path: Some(tmp.path().join("no-credentials.json")),
                ..Default::default()
            },
            ..Default::default()
        },
        accounts: AccountsConfig {
            pools: Default::default(),
        },
        models: ModelsConfig {
            aliases: Default::default(),
            models: vec![],
        },
        combos: CombosConfig {
            combos: Default::default(),
        },
        keys: Default::default(),
    };
    ClaudeAdapter::new(&cfg, Arc::new(bridge), Arc::new(VendorState::new()))
}

fn request(model: &str, tools: bool) -> ThinRequest {
    let mut body = json!({
        "model": "m",
        "messages": [{"role": "user", "content": "read seed.txt"}],
        "max_tokens": 32
    });
    if tools {
        body["tools"] = json!([{"type": "function", "function": {"name": "read_file",
            "parameters": {"type": "object", "properties": {"path": {"type": "string"}}}}}]);
    }
    ThinRequest {
        request: serde_json::from_value(body).unwrap(),
        entry: ModelEntry::bridge_probe(model),
        effort: Some(Effort::Off),
        request_id: "stub-empty".into(),
        path: "bridge",
        fallback: None,
        cursor_cwd: None,
    }
}

async fn run(adapter: &ClaudeAdapter, req: &ThinRequest) -> Vec<ThinEvent> {
    let (tx, mut rx) = tokio::sync::mpsc::channel(64);
    adapter.run(req, tx).await.expect("adapter run");
    let mut events = Vec::new();
    while let Some(ev) = rx.recv().await {
        events.push(ev);
    }
    events
}

fn runs(tmp: &tempfile::TempDir) -> u32 {
    std::fs::read_to_string(tmp.path().join("fake-claude-runs"))
        .map(|t| t.trim().parse().unwrap())
        .unwrap_or(0)
}

#[tokio::test]
async fn bridge_empty_stop_is_retried_once_on_the_bridge() {
    let tmp = tempfile::tempdir().unwrap();
    // `trace` keeps the first private dir: the retry must not reuse its name.
    let adapter = adapter(&tmp, true);
    let events = run(&adapter, &request("fake-empty-once", true)).await;
    assert_eq!(runs(&tmp), 2);
    let started = events
        .iter()
        .filter(|e| matches!(e, ThinEvent::Started { .. }))
        .count();
    assert_eq!(started, 1, "{events:?}");
    let text: String = events
        .iter()
        .filter_map(|e| match e {
            ThinEvent::TextDelta(t) => Some(t.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(text, "Hello from the bridge.");
    assert!(matches!(
        events.last(),
        Some(ThinEvent::Finish(FinishReason::Stop))
    ));
    assert!(!events.iter().any(|e| matches!(e, ThinEvent::Error(_))));
    // Only the retry's usage reached the route.
    let usage: Vec<u64> = events
        .iter()
        .filter_map(|e| match e {
            ThinEvent::Usage(u) => Some(u.prompt_tokens),
            _ => None,
        })
        .collect();
    assert_eq!(usage, vec![1]);
    assert!(tmp.path().join("stub-empty").is_dir());
    assert!(tmp.path().join("stub-empty-r1").is_dir());
}

#[tokio::test]
async fn bridge_empty_stop_twice_is_empty_completion() {
    let tmp = tempfile::tempdir().unwrap();
    let adapter = adapter(&tmp, false);
    let events = run(&adapter, &request("fake-empty-always", true)).await;
    assert!(matches!(events.first(), Some(ThinEvent::Started { .. })));
    let Some(ThinEvent::Error(err)) = events.last() else {
        panic!("expected the error last: {events:?}");
    };
    assert_eq!(err.status, 502);
    assert_eq!(err.code, "empty_completion");
    assert!(err.no_retry());
    assert!(
        err.message.starts_with("fake-empty-always (claude ")
            || err.message.starts_with("fake-empty-always:"),
        "{}",
        err.message
    );
    assert!(!events
        .iter()
        .any(|e| matches!(e, ThinEvent::Finish(_) | ThinEvent::Usage(_))));
}

#[tokio::test]
async fn bridge_empty_stop_without_tools_is_a_stop() {
    let tmp = tempfile::tempdir().unwrap();
    let adapter = adapter(&tmp, false);
    let events = run(&adapter, &request("fake-empty-always", false)).await;
    assert!(matches!(
        events.last(),
        Some(ThinEvent::Finish(FinishReason::Stop))
    ));
}
