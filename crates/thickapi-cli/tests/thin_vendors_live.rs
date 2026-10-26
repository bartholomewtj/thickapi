#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
//! Live per-vendor thin checks (SPEC §19.11): one text turn through the same runtime `serve`
//! builds, from the real `~/.thickapi` config. Run with:
//! `THICKAPI_LIVE=1 cargo test -p thickapi-cli --test thin_vendors_live -- --ignored --nocapture`
//!
//! Claude has its own files (`claude_oauth_live.rs`, `claude_bridge_live.rs`). Missing config,
//! a missing row or a missing token skip with a note rather than fail; a vendor that answers
//! must produce `Started`, text containing `LIVE OK` and `Finish(Stop)`.

use serde_json::json;
use thickapi_core::config::models::Transport;
use thickapi_core::thin::openai::{ChatRequest, FinishReason};
use thickapi_core::thin::runtime::ThinRuntime;
use thickapi_core::thin::{ThinEvent, ThinRequest};
use thickapi_core::types::BackendId;

fn live() -> bool {
    std::env::var("THICKAPI_LIVE").is_ok_and(|v| v == "1")
}

async fn text_turn(backend: BackendId, vendor_key: &str) {
    text_turn_matching(vendor_key, |m| {
        m.backend == backend && m.enabled && m.transport != Transport::CliTurn
    })
    .await;
}

async fn text_turn_matching(
    vendor_key: &str,
    pred: impl Fn(&thickapi_core::config::models::ModelEntry) -> bool,
) {
    if !live() {
        eprintln!("skipped: THICKAPI_LIVE != 1");
        return;
    }
    let config = match thickapi_core::load_config(&thickapi_core::default_config_dir()) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("skipped: no config: {e}");
            return;
        }
    };
    let Some(entry) = config.models.models.iter().find(|m| pred(m)).cloned() else {
        eprintln!("skipped: no matching thin row for {vendor_key}");
        return;
    };
    let runtime = ThinRuntime::new(&config);
    let snapshot = runtime.vendor_snapshot();
    if snapshot[vendor_key]["token"]["present"] != true {
        eprintln!(
            "skipped: no {vendor_key} token: {}",
            snapshot[vendor_key]["token"]
        );
        return;
    }
    let adapter = runtime.adapter_for(&entry).expect("adapter");
    let request: ChatRequest = serde_json::from_value(json!({
        "model": entry.id,
        "messages": [{"role": "user", "content": "Reply with exactly: LIVE OK"}],
        "max_tokens": 32
    }))
    .unwrap();
    let req = ThinRequest {
        request,
        entry: entry.clone(),
        effort: Some(thickapi_core::Effort::Low),
        request_id: format!("live-{}", uuid::Uuid::new_v4()),
        path: "oauth",
        fallback: None,
        cursor_cwd: None,
    };
    let (tx, mut rx) = tokio::sync::mpsc::channel(256);
    let run = adapter.run(&req, tx);
    let collect = async {
        let mut events = Vec::new();
        while let Some(ev) = rx.recv().await {
            events.push(ev);
        }
        events
    };
    let (result, events) = tokio::join!(run, collect);
    eprintln!("{}: {result:?}\n{events:#?}", entry.id);
    result.unwrap_or_else(|e| panic!("{} failed before Started: {e:?}", entry.id));
    assert!(
        matches!(events.first(), Some(ThinEvent::Started { .. })),
        "no Started"
    );
    let text: String = events
        .iter()
        .filter_map(|e| match e {
            ThinEvent::TextDelta(t) => Some(t.as_str()),
            _ => None,
        })
        .collect();
    assert!(text.contains("LIVE OK"), "text was {text:?}");
    assert!(events
        .iter()
        .any(|e| matches!(e, ThinEvent::Finish(FinishReason::Stop))));
}

#[tokio::test]
#[ignore]
async fn live_grok_text_turn() {
    text_turn(BackendId::Grok, "grok").await;
}

#[tokio::test]
#[ignore]
async fn live_agy_text_turn() {
    text_turn(BackendId::Agy, "agy").await;
}

#[tokio::test]
#[ignore]
async fn live_cursor_text_turn() {
    text_turn(BackendId::Cursor, "cursor").await;
}

#[tokio::test]
#[ignore]
async fn live_cursor_grok_text_turn() {
    text_turn_matching("cursor", |m| m.id == "subs/cursor/grok-4.7").await;
}
