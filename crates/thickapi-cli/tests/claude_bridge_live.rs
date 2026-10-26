#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
//! Live Claude bridge checks (SPEC §19.12 T1 exit). Run with:
//! `THICKAPI_LIVE=1 cargo test -p thickapi-cli --test claude_bridge_live -- --ignored --nocapture`

use std::path::PathBuf;
use std::time::Duration;

use serde_json::json;
use thickapi_core::config::models::ModelEntry;
use thickapi_core::thin::claude_bridge::{BridgeSettings, ClaudeBridge};
use thickapi_core::thin::openai::{ChatRequest, FinishReason};
use thickapi_core::thin::{ThinEvent, ThinRequest};

fn live() -> bool {
    std::env::var("THICKAPI_LIVE").is_ok_and(|v| v == "1")
}

fn bridge(tmp: &tempfile::TempDir) -> ClaudeBridge {
    ClaudeBridge::new(BridgeSettings {
        tmp_root: tmp.path().to_path_buf(),
        claude_path: None,
        thickapi_exe: Some(PathBuf::from(env!("CARGO_BIN_EXE_thickapi"))),
        total_timeout: Duration::from_secs(180),
        idle_timeout: Duration::from_secs(90),
        mcp_ready_timeout: Duration::from_secs(20),
        trace: false,
        bridge_max_concurrent: 2,
        queue_timeout: Duration::from_secs(300),
        handoff_drain: BridgeSettings::default_handoff_drain(),
        extra_tools: vec![],
        nonempty_inventories: vec![],
        dialect: thickapi_core::thin::dialect::MODE_OFF.to_string(),
        catalogue_ids: vec![],
    })
}

async fn run(
    bridge: &ClaudeBridge,
    body: serde_json::Value,
    effort: Option<&str>,
) -> Vec<ThinEvent> {
    let request: ChatRequest = serde_json::from_value(body).expect("request");
    let model = request.model.clone();
    let req = ThinRequest {
        request,
        entry: ModelEntry::bridge_probe(&model),
        effort: thickapi_core::thinking::normalise_effort(effort).unwrap(),
        request_id: format!("live-{}", uuid::Uuid::new_v4()),
        path: "bridge",
        fallback: None,
        cursor_cwd: None,
    };
    let (tx, mut rx) = tokio::sync::mpsc::channel(256);
    let runner = async {
        let result = bridge.run_bridge(&req, &tx).await;
        drop(tx);
        result
    };
    let collector = async {
        let mut events = Vec::new();
        while let Some(ev) = rx.recv().await {
            events.push(ev);
        }
        events
    };
    let (result, events) = tokio::join!(runner, collector);
    if let Err(err) = result {
        panic!("bridge failed before start: {} {:?}", err.message, err);
    }
    events
}

#[tokio::test]
#[ignore]
async fn live_bridge_text_turn_on_haiku() {
    if !live() {
        eprintln!("skipped: THICKAPI_LIVE != 1");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let bridge = bridge(&tmp);
    let events = run(
        &bridge,
        json!({
            "model": "claude-haiku-4-5",
            "messages": [{"role": "user", "content": "Reply with exactly: BRIDGE OK"}],
            "max_tokens": 16
        }),
        Some("off"),
    )
    .await;
    let text: String = events
        .iter()
        .filter_map(|e| match e {
            ThinEvent::TextDelta(t) => Some(t.as_str()),
            _ => None,
        })
        .collect();
    eprintln!("events: {events:#?}");
    assert!(
        events
            .iter()
            .any(|e| matches!(e, ThinEvent::Started { .. })),
        "no Started"
    );
    assert!(text.contains("BRIDGE OK"), "text was {text:?}");
    assert!(
        events
            .iter()
            .any(|e| matches!(e, ThinEvent::Usage(u) if u.prompt_tokens > 0)),
        "no usage"
    );
    assert!(events
        .iter()
        .any(|e| matches!(e, ThinEvent::Finish(FinishReason::Stop))));
    assert!(
        std::fs::read_dir(tmp.path()).unwrap().next().is_none(),
        "private dir not cleaned"
    );
}

#[tokio::test]
#[ignore]
async fn live_bridge_two_tool_calls_on_sonnet() {
    if !live() {
        eprintln!("skipped: THICKAPI_LIVE != 1");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let bridge = bridge(&tmp);
    let events = run(
        &bridge,
        json!({
            "model": "claude-sonnet-4-6",
            "messages": [
                {"role": "system", "content": "You are a coding agent with tools. Do not explain; call tools."},
                {"role": "user", "content": "Read the files a.txt and b.txt. Call the read tool for both files in a single response, in parallel, before saying anything."}
            ],
            "tools": [{"type": "function", "function": {"name": "read", "description": "Read a file from the workspace.",
                "parameters": {"type": "object", "properties": {"path": {"type": "string"}}, "required": ["path"]}}}]
        }),
        Some("low"),
    )
    .await;
    eprintln!("events: {events:#?}");
    let calls: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            ThinEvent::ToolCall {
                name, arguments, ..
            } => Some((name.clone(), arguments.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(calls.len(), 2, "expected two tool_calls, got {calls:?}");
    assert!(calls.iter().all(|(n, _)| n == "read"));
    assert!(events
        .iter()
        .any(|e| matches!(e, ThinEvent::Finish(FinishReason::ToolCalls))));
    assert!(events.iter().any(|e| matches!(e, ThinEvent::Usage(_))));
    assert!(
        std::fs::read_dir(tmp.path()).unwrap().next().is_none(),
        "private dir not cleaned"
    );
}

#[tokio::test]
#[ignore]
async fn live_bridge_handshake() {
    if !live() {
        eprintln!("skipped: THICKAPI_LIVE != 1");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let bridge = bridge(&tmp);
    let hs = bridge
        .handshake("claude-haiku-4-5")
        .await
        .expect("handshake");
    eprintln!("{hs:#?}");
    assert!(hs.claude_version.is_some());
    assert!(!hs.resolved_model.is_empty());
}
