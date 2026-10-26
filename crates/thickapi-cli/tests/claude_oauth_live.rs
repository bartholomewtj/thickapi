#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
//! Live Claude OAuth-direct checks (SPEC §19.8 / §19.12 T1 exit). Run with:
//! `THICKAPI_LIVE=1 cargo test -p thickapi-cli --test claude_oauth_live -- --ignored --nocapture`
//!
//! Outcome A: direct works → text streams. Outcome B: Anthropic blocks the token for non-CLI
//! use → `Blocked` before `Started`, which is exactly what flips the adapter onto the bridge.
//! Both are valid; the test records which, and fails only on shapes the spec does not allow.

use serde_json::json;
use thickapi_core::config::models::ModelEntry;
use thickapi_core::config::router::ClaudeThinkingBudgets;
use thickapi_core::thin::oauth::claude_oauth::{ClaudeOauth, OauthFailure};
use thickapi_core::thin::oauth::{creds, state::VendorState};
use thickapi_core::thin::openai::{ChatRequest, FinishReason};
use thickapi_core::thin::{ThinEvent, ThinRequest};

fn live() -> bool {
    std::env::var("THICKAPI_LIVE").is_ok_and(|v| v == "1")
}

fn request(body: serde_json::Value, wire_model: &str) -> ThinRequest {
    let request: ChatRequest = serde_json::from_value(body).expect("request");
    let mut entry = ModelEntry::bridge_probe(&request.model);
    entry.transport = thickapi_core::config::models::Transport::OauthDirect;
    entry.vendor_model = Some(wire_model.to_string());
    entry.thinking = true;
    entry.max_tokens = 4096;
    ThinRequest {
        request,
        entry,
        effort: Some(thickapi_core::Effort::Off),
        request_id: format!("live-{}", uuid::Uuid::new_v4()),
        path: "oauth",
        fallback: None,
        cursor_cwd: None,
    }
}

#[tokio::test]
#[ignore]
async fn live_oauth_direct_text_turn_on_haiku() {
    if !live() {
        eprintln!("skipped: THICKAPI_LIVE != 1");
        return;
    }
    let token = match creds::claude_token() {
        Ok(t) => t,
        Err(e) => {
            eprintln!("skipped: no claude token: {e}");
            return;
        }
    };
    eprintln!(
        "token source {:?}, expires in {}s, note {:?}",
        token.source,
        token.seconds_left(),
        token.note
    );
    if token.expired() {
        eprintln!("skipped: token expired (run `claude -p hi` to refresh)");
        return;
    }

    let oauth = ClaudeOauth::new(ClaudeThinkingBudgets::default(), Some("2.0.0".into()), None);
    let state = VendorState::new();
    let req = request(
        json!({
            "model": "haiku",
            "messages": [{"role": "user", "content": "Reply with exactly: OAUTH OK"}],
            "max_tokens": 16
        }),
        "claude-haiku-4-5",
    );
    let (tx, mut rx) = tokio::sync::mpsc::channel(256);
    let result = oauth.run(&req, &tx, &token, &state).await;
    drop(tx);
    let mut events = Vec::new();
    while let Some(ev) = rx.recv().await {
        events.push(ev);
    }
    eprintln!("events: {events:#?}");
    eprintln!("state: {}", state.snapshot(true));

    match result {
        Ok(()) => {
            let text: String = events
                .iter()
                .filter_map(|e| match e {
                    ThinEvent::TextDelta(t) => Some(t.as_str()),
                    _ => None,
                })
                .collect();
            assert!(
                matches!(events.first(), Some(ThinEvent::Started { .. })),
                "no Started"
            );
            assert!(text.contains("OAUTH OK"), "text was {text:?}");
            assert!(
                events
                    .iter()
                    .any(|e| matches!(e, ThinEvent::Usage(u) if u.prompt_tokens > 0)),
                "no usage"
            );
            assert!(events
                .iter()
                .any(|e| matches!(e, ThinEvent::Finish(FinishReason::Stop))));
            eprintln!("OUTCOME A: oauth-direct works");
        }
        Err(OauthFailure::Blocked(reason)) => {
            assert!(events.is_empty(), "block detected after events were sent");
            eprintln!("OUTCOME B: oauth-direct blocked → bridge fallback. reason: {reason}");
        }
        Err(OauthFailure::SuspectedBlock(reason)) => {
            assert!(events.is_empty(), "suspected block after events were sent");
            eprintln!("OUTCOME B2: oauth-direct suspected block → bridge replay. reason: {reason}");
        }
        Err(OauthFailure::Error(e)) => panic!("unexpected non-block failure: {e:?}"),
    }
}
