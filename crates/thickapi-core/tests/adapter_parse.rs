#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
use std::path::PathBuf;

use thickapi_core::adapters::{
    for_backend, AgyAdapter, BackendAdapter, ClaudeAdapter, CursorAdapter, GrokAdapter,
    OpenRouterAdapter,
};
use thickapi_core::BackendId;

fn fixture_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures")
}

fn read_fixture(name: &str) -> String {
    std::fs::read_to_string(fixture_root().join(name)).expect("read fixture")
}

#[test]
fn claude_parse_stream_fixture() {
    let adapter = for_backend(BackendId::ClaudeCode);
    let text = read_fixture("claude_stream.jsonl");
    let mut assistant = String::new();
    for line in text.lines() {
        for parsed in adapter.parse_line(line) {
            if let thickapi_core::adapters::ParsedLine::AssistantDelta(t) = parsed {
                assistant = t;
            }
        }
    }
    assert!(assistant.contains("Hello from Claude"));
}

#[test]
fn claude_init_session_followed() {
    let adapter = ClaudeAdapter;
    let text = read_fixture("claude_init.jsonl");
    let line = text.lines().next().expect("line");
    let value = serde_json::from_str::<serde_json::Value>(line).expect("json");
    assert_eq!(
        adapter.session_followed(&value),
        Some("sess-init-123".to_string())
    );
}

#[test]
fn grok_parse_stream_fixture() {
    let adapter = for_backend(BackendId::Grok);
    let text = read_fixture("grok_stream.jsonl");
    let mut assistant = String::new();
    for line in text.lines() {
        for parsed in adapter.parse_line(line) {
            if let thickapi_core::adapters::ParsedLine::AssistantDelta(t) = parsed {
                assistant = t;
            }
        }
    }
    assert!(assistant.contains("Hello from Grok"));
}

#[test]
fn grok_init_session_followed() {
    let adapter = GrokAdapter;
    let text = read_fixture("grok_init.jsonl");
    let line = text.lines().next().expect("line");
    let value = serde_json::from_str::<serde_json::Value>(line).expect("json");
    assert_eq!(
        adapter.session_followed(&value),
        Some("grok-init-99".to_string())
    );
}

#[test]
fn agy_parse_stream_fixture() {
    let adapter = for_backend(BackendId::Agy);
    let text = read_fixture("agy_stream.jsonl");
    let mut assistant = String::new();
    for line in text.lines() {
        for parsed in adapter.parse_line(line) {
            if let thickapi_core::adapters::ParsedLine::AssistantDelta(t) = parsed {
                assistant = t;
            }
        }
    }
    assert!(assistant.contains("Hello from agy"));
}

#[test]
fn agy_init_session_followed() {
    let adapter = AgyAdapter;
    let text = read_fixture("agy_init.jsonl");
    let line = text.lines().next().expect("line");
    let value = serde_json::from_str::<serde_json::Value>(line).expect("json");
    assert_eq!(
        adapter.session_followed(&value),
        Some("agy-init-42".to_string())
    );
}

#[test]
fn cursor_parse_stream_fixture() {
    let adapter = for_backend(BackendId::Cursor);
    let text = read_fixture("cursor_stream.jsonl");
    let mut assistant = String::new();
    for line in text.lines() {
        for parsed in adapter.parse_line(line) {
            if let thickapi_core::adapters::ParsedLine::AssistantDelta(t) = parsed {
                assistant = t;
            }
        }
    }
    assert!(assistant.contains("Hello from Cursor"));
}

#[test]
fn cursor_init_session_followed() {
    let adapter = CursorAdapter;
    let text = read_fixture("cursor_init.jsonl");
    let line = text.lines().next().expect("line");
    let value = serde_json::from_str::<serde_json::Value>(line).expect("json");
    assert_eq!(
        adapter.session_followed(&value),
        Some("cursor-init-55".to_string())
    );
}

#[test]
fn openrouter_parse_stream_fixture() {
    let adapter = OpenRouterAdapter;
    let text = read_fixture("pi_stream.jsonl");
    let mut assistant = String::new();
    for line in text.lines() {
        for parsed in adapter.parse_line(line) {
            if let thickapi_core::adapters::ParsedLine::AssistantDelta(t) = parsed {
                assistant = t;
            }
        }
    }
    assert!(assistant.contains("Hello from Pi"));
}

#[test]
fn openrouter_init_session_followed() {
    let adapter = OpenRouterAdapter;
    let text = read_fixture("pi_init.jsonl");
    let line = text.lines().next().expect("line");
    let value = serde_json::from_str::<serde_json::Value>(line).expect("json");
    assert_eq!(
        adapter.session_followed(&value),
        Some("pi-init-77".to_string())
    );
}
