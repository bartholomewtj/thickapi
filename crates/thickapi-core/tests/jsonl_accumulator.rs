#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
use thickapi_core::adapters::for_backend;
use thickapi_core::engine::jsonl::JsonlAccumulator;
use thickapi_core::types::BackendId;
use thickapi_core::TurnEvent;

#[test]
fn assistant_delta_emitted_once_per_line_not_duplicated_from_raw() {
    let adapter = for_backend(BackendId::ClaudeCode);
    let line = r#"{"type":"assistant","message":{"content":[{"type":"text","text":"Hello"}]}}"#;
    let mut acc = JsonlAccumulator::new();
    let emitted = acc.process_line(adapter, line, &|_| {});
    assert_eq!(emitted.len(), 1);
    assert!(matches!(
        &emitted[0],
        TurnEvent::AssistantDelta { text } if text == "Hello"
    ));
}

#[test]
fn assistant_delta_is_incremental_not_cumulative() {
    let adapter = for_backend(BackendId::ClaudeCode);
    let mut acc = JsonlAccumulator::new();
    let line1 = r#"{"type":"assistant","message":{"content":[{"type":"text","text":"Hel"}]}}"#;
    let line2 = r#"{"type":"assistant","message":{"content":[{"type":"text","text":"Hello"}]}}"#;
    let e1 = acc.process_line(adapter, line1, &|_| {});
    let e2 = acc.process_line(adapter, line2, &|_| {});
    assert_eq!(e1.len(), 1);
    assert_eq!(e2.len(), 1);
    assert!(matches!(
        &e1[0],
        TurnEvent::AssistantDelta { text } if text == "Hel"
    ));
    assert!(matches!(
        &e2[0],
        TurnEvent::AssistantDelta { text } if text == "lo"
    ));
    assert_eq!(acc.text, "Hello");
}

#[test]
fn tool_call_emitted_once_per_id() {
    let adapter = for_backend(BackendId::ClaudeCode);
    let line = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"toolu_1","name":"Read","input":{"path":"README.md"}}]}}"#;
    let mut acc = JsonlAccumulator::new();
    let e1 = acc.process_line(adapter, line, &|_| {});
    let e2 = acc.process_line(adapter, line, &|_| {});
    assert_eq!(e1.len(), 1);
    assert!(matches!(&e1[0], TurnEvent::ToolCall { tool, .. } if tool == "Read"));
    assert_eq!(e2.len(), 0);
}

#[test]
fn duplicate_snapshot_is_skipped() {
    let adapter = for_backend(BackendId::ClaudeCode);
    let line = r#"{"type":"assistant","message":{"content":[{"type":"text","text":"PONG"}]}}"#;
    let mut acc = JsonlAccumulator::new();
    let e1 = acc.process_line(adapter, line, &|_| {});
    let e2 = acc.process_line(adapter, line, &|_| {});
    assert_eq!(e1.len(), 1);
    assert_eq!(e2.len(), 0);
}

fn assistant_line(text: &str) -> String {
    serde_json::json!({
        "type": "assistant",
        "message": {"content": [{"type": "text", "text": text}]}
    })
    .to_string()
}

fn deltas(events: &[TurnEvent]) -> String {
    events
        .iter()
        .filter_map(|e| match e {
            TurnEvent::AssistantDelta { text } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

#[test]
fn every_assistant_message_of_a_turn_reaches_the_text() {
    let adapter = for_backend(BackendId::ClaudeCode);
    let mut acc = JsonlAccumulator::new();
    let tool = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"toolu_9","name":"Read","input":{"path":"a.rs"}}]}}"#;
    let mut events = Vec::new();
    events.extend(acc.process_line(
        adapter,
        &assistant_line("I'll read the file first."),
        &|_| {},
    ));
    events.extend(acc.process_line(adapter, tool, &|_| {}));
    events.extend(acc.process_line(adapter, &assistant_line("The bug is on line 4."), &|_| {}));

    assert_eq!(
        acc.text,
        "I'll read the file first.\n\nThe bug is on line 4."
    );
    assert_eq!(deltas(&events), acc.text, "deltas must add up to the text");
}

#[test]
fn a_closing_result_line_does_not_repeat_the_text() {
    let adapter = for_backend(BackendId::Cursor);
    let mut acc = JsonlAccumulator::new();
    acc.process_line(adapter, &assistant_line("First."), &|_| {});
    acc.process_line(adapter, &assistant_line("Second."), &|_| {});
    let before = acc.text.clone();

    // cursor-agent closes with the messages run together; the last message alone and the
    // joined text are the other two shapes a repeat takes.
    for repeat in ["First.Second.", "Second.", before.as_str()] {
        let line = serde_json::json!({"type": "result", "result": repeat}).to_string();
        let emitted = acc.process_line(adapter, &line, &|_| {});
        assert!(emitted.is_empty(), "{repeat:?} is a repeat");
        assert_eq!(acc.text, before);
    }
}

#[test]
fn a_cursor_result_with_trailing_text_keeps_the_tail() {
    let adapter = for_backend(BackendId::Cursor);
    let mut acc = JsonlAccumulator::new();
    acc.process_line(
        adapter,
        &assistant_line("Hello from Cursor fixture."),
        &|_| {},
    );
    let line = serde_json::json!({
        "type": "result",
        "result": "Hello from Cursor fixture.\nThanks."
    })
    .to_string();
    let emitted = acc.process_line(adapter, &line, &|_| {});
    assert_eq!(deltas(&emitted), "\nThanks.");
    assert_eq!(acc.text, "Hello from Cursor fixture.\nThanks.");

    let mut acc = JsonlAccumulator::new();
    acc.process_line(adapter, &assistant_line("First."), &|_| {});
    acc.process_line(adapter, &assistant_line("Second."), &|_| {});
    let recap = serde_json::json!({
        "type": "result",
        "result": "First.\n\nSecond.\nThanks."
    })
    .to_string();
    let emitted = acc.process_line(adapter, &recap, &|_| {});
    assert_eq!(deltas(&emitted), "\n\nThanks.");
    assert_eq!(acc.text, "First.\n\nSecond.\n\nThanks.");
}

#[test]
fn grok_live_claude_shaped_stream_keeps_the_final_text() {
    // Live grok 1.0.34+ (side-port #64): not the committed `assistant_message` fixture.
    // Fails without the Claude-shaped arm — `TurnResult.text` stayed empty on a 200.
    let adapter = for_backend(BackendId::Grok);
    let mut acc = JsonlAccumulator::new();
    let lines = [
        r#"{"type":"system","subtype":"init","session_id":"g1"}"#,
        r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"thinking","thinking":"x"},{"type":"text","text":"I'll look."},{"type":"tool_use","id":"c1","name":"read_file","input":{"target_file":"notes.txt"}}]}}"#,
        r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"SIDEPORT OK"}]}}"#,
        r#"{"type":"result","subtype":"success","result":"SIDEPORT OK","total_cost_usd":0.01,"usage":{"input_tokens":10,"output_tokens":3,"cache_read_input_tokens":4}}"#,
    ];
    for line in lines {
        acc.process_line(adapter, line, &|_| {});
    }
    assert!(
        acc.text.contains("SIDEPORT OK"),
        "empty text on a successful grok turn: {:?}",
        acc.text
    );
    assert_eq!(acc.cli_session.as_deref(), Some("g1"));
    assert_eq!(acc.usage.input, 10);
    assert_eq!(acc.usage.cache_read, 4);
    assert!((acc.cost - 0.01).abs() < f64::EPSILON);
}

#[test]
fn thick_stream_fixtures_replay_through_the_accumulator() {
    let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures");
    let cases = [
        (
            BackendId::ClaudeCode,
            "claude_stream.jsonl",
            "Hello from Claude fixture.",
        ),
        (
            BackendId::Grok,
            "grok_stream.jsonl",
            "Hello from Grok fixture.",
        ),
        (
            BackendId::Cursor,
            "cursor_stream.jsonl",
            "Hello from Cursor fixture.",
        ),
        (
            BackendId::Agy,
            "agy_stream.jsonl",
            "Hello from agy fixture.",
        ),
        (BackendId::Pi, "pi_stream.jsonl", "Hello from Pi fixture."),
    ];
    for (backend, file, expect) in cases {
        let adapter = for_backend(backend);
        let mut acc = JsonlAccumulator::new();
        let text =
            std::fs::read_to_string(root.join(file)).unwrap_or_else(|e| panic!("{file}: {e}"));
        for line in text.lines() {
            acc.process_line(adapter, line, &|_| {});
        }
        assert_eq!(acc.text, expect, "{file}");
    }
}

#[test]
fn a_growing_snapshot_after_an_earlier_message_extends_only_the_last_one() {
    let adapter = for_backend(BackendId::ClaudeCode);
    let mut acc = JsonlAccumulator::new();
    acc.process_line(adapter, &assistant_line("Done reading."), &|_| {});
    acc.process_line(adapter, &assistant_line("The fix"), &|_| {});
    let grown = acc.process_line(adapter, &assistant_line("The fix is small."), &|_| {});
    assert_eq!(deltas(&grown), " is small.");
    assert_eq!(acc.text, "Done reading.\n\nThe fix is small.");
}
