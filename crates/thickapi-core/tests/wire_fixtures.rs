#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
//! Replay the newest captured fixture set per vendor (`thickapi capture <vendor>`, SPEC §19.11)
//! through the adapter's offline `replay` and require the same event summary the live run
//! produced. A vendor wire change is fixed by: capture, watch these fail, fix, tests pass.
//!
//! Vendors with no set under `fixtures/<vendor>/` are skipped with a note, so a fresh clone
//! stays green until the first capture is committed. Every committed set must also be free of
//! `fixture_set::TOKEN_PATTERNS` leaks; the decoded-body check with repo roots is the CLI's
//! `identity_violations` (`thickapi-cli/tests/capture_redact.rs`).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::{json, Value};
use thickapi_core::config::router::ThinConfig;
use thickapi_core::thin::capture::{
    load_events, load_frames, load_request, summarize, FrameRecord,
};
use thickapi_core::thin::claude_bridge;
use thickapi_core::thin::fixture_set::{
    self, committed_sets, newest_set, scenarios, Scope, EVENTS_FILE, FRAMES_FILE, META_FILE,
    REQUEST_FILE, RESPONSE_FILE,
};
use thickapi_core::thin::oauth::vendor::{StreamBudgets, VendorRequest, WireMapper};
use thickapi_core::thin::oauth::{antigravity, claude_oauth, codex, cursor, grok};
use thickapi_core::thin::openai::{FinishReason, Usage};
use thickapi_core::thin::ThinEvent;

fn fixtures_root() -> PathBuf {
    fixture_set::compiled_fixtures_root()
}

fn meta(set: &Path) -> Value {
    std::fs::read_to_string(set.join(META_FILE))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or(Value::Null)
}

fn bridge_request(scenario_dir: &Path) -> Value {
    std::fs::read_to_string(scenario_dir.join(REQUEST_FILE))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or(Value::Null)
}

/// Client tool names the bridge declared for a scenario (`request.json` → `body.tools`).
/// A string is a name; an object is a declared tool schema (`body.declared`).
fn bridge_tools(scenario_dir: &Path) -> Vec<String> {
    bridge_request(scenario_dir)["body"]["tools"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .filter_map(|v| {
            v.as_str()
                .map(str::to_string)
                .or_else(|| v["function"]["name"].as_str().map(str::to_string))
        })
        .collect()
}

/// Declared tool schemas, when the capture stored them (`body.declared`). Empty for a set
/// captured before that field existed, which replays with the dialect off.
fn bridge_declared(scenario_dir: &Path) -> Vec<thickapi_core::thin::openai::ToolDef> {
    bridge_request(scenario_dir)["body"]["declared"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .filter_map(|v| serde_json::from_value(v.clone()).ok())
        .collect()
}

fn replay_for(vendor: &str, dir: &Path, frames: &[Vec<u8>], wire_model: &str) -> Vec<ThinEvent> {
    match vendor {
        // Every OAuth-direct vendor replays through `VendorRequest::replay`, the decoder its
        // live run uses (C2, C3).
        "claude" => claude_oauth::ClaudeDirectRequest::offline().replay(wire_model, frames),
        "claude-bridge" => {
            let name = dir.file_name().and_then(|n| n.to_str()).unwrap_or("");
            let dialect = name == "dialect" || name == "todo";
            let declared = bridge_declared(dir);
            if dialect && !declared.is_empty() {
                claude_bridge::replay_with_tools(frames, &declared, true)
            } else {
                let tools = bridge_tools(dir);
                let names: Vec<&str> = tools.iter().map(String::as_str).collect();
                claude_bridge::replay(frames, &names)
            }
        }
        "grok" => grok::GrokAdapter::new(&ThinConfig::default(), Arc::default())
            .replay(wire_model, frames),
        "agy" => antigravity::AgyAdapter::new(&ThinConfig::default(), Arc::default())
            .replay(wire_model, frames),
        "codex" => codex::CodexAdapter::new(&ThinConfig::default(), Arc::default())
            .replay(wire_model, frames),
        // A built-in read of a missing file is answered in protocol, not handed to Pi (G1),
        // judged as the capture host saw the path.
        "cursor" => cursor_request(dir)
            .answering_missing_reads(capture_read_miss())
            .replay(wire_model, frames),
        other => panic!("no replay for vendor {other}"),
    }
}

/// Offline Cursor replay declaring what the captured frame 0 declared (G2, #182): a
/// built-in becomes one of those tools or is denied, as on the live Run.
fn cursor_request(dir: &Path) -> cursor::CursorRequest<'static> {
    let body = load_request(dir)
        .ok()
        .and_then(|r| r.body_bytes())
        .unwrap_or_default();
    cursor::CursorRequest::offline().declaring(&cursor::declared_tools_in_body(&body))
}

/// Whether a captured built-in read's target is missing, as the capture host saw it: the
/// capture ran from the repo root, and its paths were redacted to `<REPO>` / `<HOME>`
/// (`ReadMiss::for_fixtures`). The shape only changes the duplex reply, which replay does
/// not send.
fn capture_read_miss() -> cursor::ReadMiss {
    let root = fixtures_root()
        .parent()
        .expect("fixtures/ sits in the repo root")
        .to_path_buf();
    cursor::ReadMiss::for_fixtures(&root)
}

fn check_vendor(vendor: &str) {
    let Some(set) = newest_set(&fixtures_root(), vendor) else {
        eprintln!("skipped: no fixtures/{vendor}/<version>/ set (run `thickapi capture {vendor}`)");
        return;
    };
    check_set(vendor, &set);
}

fn check_set(vendor: &str, set: &Path) {
    let meta = meta(set);
    let wire_model = meta["wire_model"].as_str().unwrap_or("model");
    let dirs = scenarios(set);
    assert!(
        !dirs.is_empty(),
        "{} has no scenario directories",
        set.display()
    );
    for dir in dirs {
        let name = dir.file_name().unwrap().to_string_lossy().into_owned();
        let frames = load_frames(&dir).expect("frames.jsonl");
        assert!(!frames.is_empty(), "{}: no frames", dir.display());
        // The rate-limit advisory is derived from response *headers*, not the body
        // replay. Cursor estimated Usage is derived from the prompt the body replay
        // does not have. Other vendors carry usage on the wire; keep comparing it.
        let skip_usage = vendor == "cursor";
        let recorded: Vec<Value> = load_events(&dir)
            .expect("events.jsonl")
            .into_iter()
            .filter(|v| v["type"] != "rate_limit" && !(skip_usage && v["type"] == "usage"))
            .collect();
        // Per-scenario model wins: a set may mix catalogue rows.
        let replay_model = recorded
            .iter()
            .find(|v| v["type"] == "started")
            .and_then(|v| v["model"].as_str())
            .unwrap_or(wire_model);
        let replayed: Vec<Value> = summarize(&replay_for(vendor, &dir, &frames, replay_model))
            .into_iter()
            .filter(|v| v["type"] != "rate_limit" && !(skip_usage && v["type"] == "usage"))
            .collect();
        assert_eq!(
            replayed,
            recorded,
            "{vendor}/{name}: replaying {} no longer yields the events recorded at capture time",
            dir.display()
        );
        // Shape the scenarios promise, independent of what was recorded.
        let kinds: Vec<&str> = replayed.iter().filter_map(|v| v["type"].as_str()).collect();
        assert_eq!(
            kinds.first().copied(),
            Some("started"),
            "{vendor}/{name}: no Started"
        );
        match name.as_str() {
            "text" => {
                assert!(kinds.contains(&"text"), "{vendor}/text: no text");
                assert!(
                    replayed
                        .iter()
                        .any(|v| v["type"] == "finish" && v["reason"] == "stop"),
                    "{vendor}/text: no finish stop"
                );
            }
            "tool" => {
                assert!(
                    replayed
                        .iter()
                        .any(|v| v["type"] == "tool_call" && v["name"] == "read_file"),
                    "{vendor}/tool: no read_file tool_call"
                );
                assert!(
                    replayed
                        .iter()
                        .any(|v| v["type"] == "finish" && v["reason"] == "tool_calls"),
                    "{vendor}/tool: no finish tool_calls"
                );
            }
            // Dialect on: the client sees the Pi name. An Edit prompt yields `edit`; a
            // TodoWrite prompt yields `todo` (the vendor name stays on the access line).
            "dialect" => {
                assert!(
                    replayed
                        .iter()
                        .any(|v| v["type"] == "tool_call" && v["name"] == "edit"),
                    "{vendor}/dialect: no edit tool_call"
                );
            }
            "todo" => {
                assert!(
                    replayed
                        .iter()
                        .any(|v| v["type"] == "tool_call" && v["name"] == "todo"),
                    "{vendor}/todo: no todo tool_call"
                );
            }
            _ => {}
        }
    }
}

/// Every `TOKEN_PATTERNS` hit in a set's files as stored, plus its response headers.
fn set_leaks(set: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let mut files = vec![set.join(META_FILE)];
    for dir in scenarios(set) {
        for file in [REQUEST_FILE, RESPONSE_FILE, FRAMES_FILE, EVENTS_FILE] {
            files.push(dir.join(file));
        }
        let response = std::fs::read_to_string(dir.join(RESPONSE_FILE)).unwrap_or_default();
        let response: Value = serde_json::from_str(&response).unwrap_or_default();
        for pair in response["headers"].as_array().into_iter().flatten() {
            let (name, value) = (
                pair[0].as_str().unwrap_or(""),
                pair[1].as_str().unwrap_or(""),
            );
            let mut hits = fixture_set::leaks(Scope::HeaderName, name);
            hits.extend(fixture_set::leaks(Scope::HeaderValue, value));
            out.extend(
                hits.iter()
                    .map(|h| format!("{}: header {name}: {h}", dir.display())),
            );
        }
    }
    for file in files {
        let text = std::fs::read_to_string(&file).unwrap_or_default();
        let hits = fixture_set::leaks(Scope::Text, &text);
        out.extend(hits.iter().map(|h| format!("{}: {h}", file.display())));
    }
    out
}

/// No token, path, e-mail or identifying header survived redaction in any committed set
/// (not only the newest): the one `fixture_set::TOKEN_PATTERNS` table.
#[test]
fn committed_sets_carry_no_token_patterns() {
    let sets = committed_sets(
        &fixtures_root(),
        &["claude", "claude-bridge", "grok", "agy", "cursor"],
    );
    assert!(!sets.is_empty(), "no committed fixture sets found");
    let leaks: Vec<String> = sets.iter().flat_map(|s| set_leaks(s)).collect();
    assert!(
        leaks.is_empty(),
        "committed sets leak:\n{}",
        leaks.join("\n")
    );
}

#[test]
fn summary_coalesces_deltas_and_drops_ids() {
    let events = vec![
        ThinEvent::Started {
            model: "m".into(),
            path: "oauth",
            fallback: None,
            wire_params: None,
        },
        ThinEvent::ReasoningDelta {
            text: "th".into(),
            signature: None,
            details: None,
        },
        ThinEvent::ReasoningDelta {
            text: "ink".into(),
            signature: Some("sig".into()),
            details: None,
        },
        ThinEvent::TextDelta("Hel".into()),
        ThinEvent::TextDelta("lo".into()),
        ThinEvent::ToolCall {
            id: "call_random".into(),
            name: "read".into(),
            arguments: "{\"path\":\"a\"}".into(),
        },
        ThinEvent::Usage(Usage {
            prompt_tokens: 3,
            completion_tokens: 4,
            total_tokens: 7,
            prompt_tokens_details: None,
        }),
        ThinEvent::Finish(FinishReason::ToolCalls),
    ];
    assert_eq!(
        summarize(&events),
        vec![
            json!({"type": "started", "model": "m"}),
            json!({"type": "reasoning", "text": "think"}),
            json!({"type": "text", "text": "Hello"}),
            json!({"type": "tool_call", "name": "read", "arguments": {"path": "a"}}),
            json!({"type": "usage", "prompt_tokens": 3, "completion_tokens": 4}),
            json!({"type": "finish", "reason": "tool_calls"}),
        ]
    );
}

#[test]
fn claude_oauth_newest_set_replays() {
    check_vendor("claude");
}

#[test]
fn claude_bridge_newest_set_replays() {
    check_vendor("claude-bridge");
}

/// C4: every committed bridge set (2.1.274 and 2.1.283), not only the newest, replays through
/// the one Mapper + publish path.
#[test]
fn claude_bridge_every_set_replays() {
    let sets = committed_sets(&fixtures_root(), &["claude-bridge"]);
    let names: Vec<String> = sets
        .iter()
        .filter_map(|s| s.file_name().map(|n| n.to_string_lossy().into_owned()))
        .collect();
    for version in ["2.1.274", "2.1.283"] {
        assert!(
            names.iter().any(|n| n == version),
            "fixtures/claude-bridge/{version} missing: {names:?}"
        );
    }
    for set in &sets {
        check_set("claude-bridge", set);
    }
}

/// #145: `replay` applies the live loop's rule — a text block left open at `message_stop`
/// is closed and the turn finishes `stop` with its text; it is not drift.
#[test]
fn claude_bridge_open_text_block_replays_as_a_stop() {
    let path = fixtures_root().join("thin/claude-bridge/text-turn-open-block.jsonl");
    let text = std::fs::read_to_string(&path).expect("text-turn-open-block.jsonl");
    assert!(
        !text.contains("content_block_stop"),
        "fixture must leave the block open"
    );
    let frames: Vec<Vec<u8>> = text
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| format!("{l}\n").into_bytes())
        .collect();
    let events = claude_bridge::replay(&frames, &["read", "bash"]);
    assert!(
        !events.iter().any(|e| matches!(e, ThinEvent::Error(_))),
        "{events:?}"
    );
    let summary = summarize(&events);
    let streamed: String = summary
        .iter()
        .filter(|v| v["type"] == "text")
        .filter_map(|v| v["text"].as_str())
        .collect();
    assert_eq!(streamed, "Hello from the bridge.", "{summary:?}");
    assert!(
        summary
            .last()
            .is_some_and(|v| v["type"] == "finish" && v["reason"] == "stop"),
        "{summary:?}"
    );
}

#[test]
fn grok_newest_set_replays() {
    check_vendor("grok");
}

#[test]
fn agy_newest_set_replays() {
    check_vendor("agy");
}

#[test]
fn cursor_newest_set_replays() {
    check_vendor("cursor");
}

#[test]
fn codex_newest_set_replays() {
    check_vendor("codex");
}

// ---- F1 (#181): Cursor built-in and stream-edge captures -------------------------------
//
// Sets under the newest `fixtures/cursor/<version>/` from `thickapi capture cursor
// --scenario {create,create-fnf,quiet,mcp-repeat}` (plus `handshake-fable`, a P4 death
// caught on the way). The generic replay above already pins their events; these tests
// state what the frames show, and the `#[ignore]`d ones the behaviour a later slice
// (H1 #185, H3 #187) must deliver — that slice turns its test on (G1 #175 has).

/// The newest Cursor set's scenario directories whose name starts with `prefix`.
fn cursor_scenarios(prefix: &str) -> Vec<PathBuf> {
    let Some(set) = newest_set(&fixtures_root(), "cursor") else {
        return Vec::new();
    };
    scenarios(&set)
        .into_iter()
        .filter(|d| {
            d.file_name()
                .is_some_and(|n| n.to_string_lossy().starts_with(prefix))
        })
        .collect()
}

fn recorded_model(dir: &Path) -> String {
    load_events(dir)
        .unwrap_or_default()
        .iter()
        .find(|v| v["type"] == "started")
        .and_then(|v| v["model"].as_str())
        .unwrap_or("model")
        .to_string()
}

/// `frames.jsonl` with each chunk's arrival time.
fn timed_frames(dir: &Path) -> Vec<(u64, Vec<u8>)> {
    let text = std::fs::read_to_string(dir.join(FRAMES_FILE)).expect("frames.jsonl");
    text.lines()
        .filter_map(|l| serde_json::from_str::<FrameRecord>(l).ok())
        .map(|f| (f.ms, f.bytes()))
        .collect()
}

/// Drive frames through a Cursor wire on their recorded clock, as the live loop does:
/// quiet *event* time past the wire's current budget ends the turn (`on_quiet`) before the
/// next frame is read. Frames that run out without an end are waited out the same way.
fn drive_timed<W: WireMapper>(
    wire: &mut W,
    frames: &[(u64, Vec<u8>)],
) -> (Vec<ThinEvent>, Vec<bytes::Bytes>) {
    let mut out = Vec::new();
    let mut replies = Vec::new();
    let mut last_event = frames.first().map_or(0, |(ms, _)| *ms);
    for (ms, chunk) in frames {
        let budget = wire.budget();
        if u128::from(ms.saturating_sub(last_event)) > budget.as_millis() {
            wire.on_quiet(budget);
            out.extend(wire.core().pending.drain(..));
            return (out, replies);
        }
        wire.on_chunk(chunk, &mut replies);
        let fresh: Vec<ThinEvent> = wire.core().pending.drain(..).collect();
        if !fresh.is_empty() {
            last_event = *ms;
        }
        out.extend(fresh);
        if wire.core().done {
            return (out, replies);
        }
    }
    let budget = wire.budget();
    wire.on_quiet(budget);
    out.extend(wire.core().pending.drain(..));
    (out, replies)
}

/// Minimal protobuf reader for the duplex replies (field, wire type, bytes or varint).
fn pb_fields(mut buf: &[u8]) -> Vec<(u64, u8, Vec<u8>, u64)> {
    fn varint(buf: &mut &[u8]) -> Option<u64> {
        let mut out = 0u64;
        for shift in (0..64).step_by(7) {
            let (&b, rest) = buf.split_first()?;
            *buf = rest;
            out |= u64::from(b & 0x7f) << shift;
            if b & 0x80 == 0 {
                return Some(out);
            }
        }
        None
    }
    let mut out = Vec::new();
    while !buf.is_empty() {
        let Some(key) = varint(&mut buf) else { break };
        let (field, wire) = (key >> 3, (key & 7) as u8);
        match wire {
            0 => {
                let Some(v) = varint(&mut buf) else { break };
                out.push((field, wire, Vec::new(), v));
            }
            2 => {
                let Some(n) = varint(&mut buf) else { break };
                let n = n as usize;
                if n > buf.len() {
                    break;
                }
                let (data, rest) = buf.split_at(n);
                out.push((field, wire, data.to_vec(), 0));
                buf = rest;
            }
            _ => break,
        }
    }
    out
}

/// A duplex reply frame as `AgentClientMessage.exec_client_message`: `(id, result field,
/// result bytes)`.
fn exec_reply(frame: &[u8]) -> Option<(u64, u64, Vec<u8>)> {
    let payload = frame.get(5..)?;
    let (_, _, esm, _) = pb_fields(payload).into_iter().find(|f| f.0 == 2)?;
    let mut id = 0;
    let mut result = None;
    for (field, wire, data, v) in pb_fields(&esm) {
        match (field, wire) {
            (1, 0) => id = v,
            (15 | 19, _) => {}
            (n, 2) => result = Some((n, data)),
            _ => {}
        }
    }
    result.map(|(n, data)| (id, n, data))
}

/// The Connect frames in one recorded chunk: `(flags, payload)`, gzip payloads opened.
fn chunk_payloads(chunk: &[u8]) -> Vec<(u8, Vec<u8>)> {
    let mut decoder = cursor::FrameDecoder::default();
    decoder
        .push(chunk)
        .unwrap_or_default()
        .into_iter()
        .map(|frame| {
            if frame.flags & cursor::FLAG_GZIP == 0 {
                return (frame.flags, frame.payload);
            }
            let mut out = Vec::new();
            std::io::Read::read_to_end(
                &mut flate2::read::GzDecoder::new(frame.payload.as_slice()),
                &mut out,
            )
            .expect("gzip frame");
            (frame.flags, out)
        })
        .collect()
}

/// `AgentServerMessage.interaction_update` (`f1`) field numbers in a payload.
fn interaction_updates(payload: &[u8]) -> Vec<u64> {
    pb_fields(payload)
        .into_iter()
        .filter(|f| f.0 == 1 && f.1 == 2)
        .flat_map(|(_, _, data, _)| pb_fields(&data).into_iter().map(|f| f.0))
        .collect()
}

fn tool_calls(summary: &[Value]) -> Vec<(String, Value)> {
    summary
        .iter()
        .filter(|v| v["type"] == "tool_call")
        .map(|v| {
            (
                v["name"].as_str().unwrap_or("").to_string(),
                v["arguments"].clone(),
            )
        })
        .collect()
}

/// G1's wire fact: answered with `ReadResult.file_not_found` (the CLI's own answer on
/// ENOENT), Cursor's edit/write built-in goes on to `f3 write_args` for the same path; the
/// write is bridged to Pi `write`. Answered with `ReadResult.rejected`, it does not: the
/// server reports an empty `EditResult` and the model falls back to a shell exec.
#[test]
fn cursor_create_sets_show_the_pre_read_answer_decides_the_write() {
    let fnf = cursor_scenarios("create-fnf-");
    let rejected: Vec<PathBuf> = cursor_scenarios("create-")
        .into_iter()
        .filter(|d| !d.to_string_lossy().contains("create-fnf-"))
        .collect();
    if fnf.is_empty() && rejected.is_empty() {
        eprintln!("skipped: no F1 create sets");
        return;
    }
    for dir in &fnf {
        let frames = load_frames(dir).unwrap();
        let replayed = summarize(
            &cursor_request(dir)
                .answering_missing_reads(capture_read_miss())
                .replay(&recorded_model(dir), &frames),
        );
        let calls = tool_calls(&replayed);
        assert_eq!(calls.len(), 1, "{}: {calls:?}", dir.display());
        let (name, args) = &calls[0];
        assert_eq!(name, "write", "{}: {calls:?}", dir.display());
        assert!(
            args["path"]
                .as_str()
                .is_some_and(|p| p.ends_with("smoke-marker.txt")),
            "{args}"
        );
        assert_eq!(args["content"], "ok\n", "{args}");
    }
    for dir in &rejected {
        let frames = load_frames(dir).unwrap();
        let replayed = summarize(
            &cursor_request(dir)
                .answering_missing_reads(capture_read_miss())
                .replay(&recorded_model(dir), &frames),
        );
        let names: Vec<String> = tool_calls(&replayed).into_iter().map(|c| c.0).collect();
        assert_eq!(names, ["bash"], "{}: {replayed:?}", dir.display());
    }
}

/// G1 (#175): `serve` itself answers a built-in pre-read of a missing file on the duplex
/// with `ReadResult.file_not_found { path }` and lets the Run go on to the write, which is
/// bridged to Pi `write`. Before G1 the pre-read was handed to Pi as `read` and the turn
/// ended there (P1). The wire here is the default one (no capture switch, no replay
/// override): the capture's fictional absolute path does not exist on this machine, as it
/// did not on the capture host.
#[test]
fn cursor_serve_answers_a_missing_pre_read_with_file_not_found() {
    let dirs = cursor_scenarios("create-fnf-");
    assert!(!dirs.is_empty(), "no F1 create-fnf sets");
    for dir in dirs {
        let frames = load_frames(&dir).unwrap();
        let request = cursor_request(&dir);
        let mut wire = request.wire(&recorded_model(&dir), StreamBudgets::OFFLINE);
        let mut replies = Vec::new();
        let mut events = Vec::new();
        for chunk in &frames {
            wire.on_chunk(chunk, &mut replies);
            events.extend(wire.core().pending.drain(..));
            if wire.core().done {
                break;
            }
        }
        let summary = summarize(&events);
        let names: Vec<String> = tool_calls(&summary).into_iter().map(|c| c.0).collect();
        assert_eq!(names, ["write"], "{}: {summary:?}", dir.display());
        let read_answers: Vec<(u64, u64, Vec<u8>)> = replies
            .iter()
            .filter_map(|r| exec_reply(r))
            .filter(|(_, field, _)| *field == 7)
            .collect();
        assert_eq!(read_answers.len(), 1, "one ReadResult on the duplex");
        let (_, _, result) = &read_answers[0];
        assert!(
            pb_fields(result).iter().any(|f| f.0 == 4),
            "ReadResult.file_not_found (f4), not rejected (f3)"
        );
    }
}

/// H1 (#185), measured: a Cursor answer ends on the wire — `f1.17 step_completed`,
/// `f3` checkpoint, `f1.14 turn_ended`, then the Connect `END {}` trailer (2026.09.28 and
/// 2026.09.15 `text` sets). The adapter finishes on the trailer; after text on a request
/// that offers tools it would otherwise wait out 25 s.
#[test]
fn cursor_text_answer_ends_with_turn_ended_then_the_trailer() {
    let Some(dir) = cursor_scenarios("text").into_iter().next() else {
        eprintln!("skipped: no cursor text set");
        return;
    };
    let frames = timed_frames(&dir);
    let request = cursor_request(&dir);
    let mut wire = request.wire(&recorded_model(&dir), StreamBudgets::OFFLINE);
    let (events, _) = drive_timed(&mut wire, &frames);
    let summary = summarize(&events);
    assert!(
        summary
            .last()
            .is_some_and(|v| v["type"] == "finish" && v["reason"] == "stop"),
        "{summary:?}"
    );
    let decoded: Vec<(u8, Vec<u8>)> = frames
        .iter()
        .flat_map(|(_, chunk)| chunk_payloads(chunk))
        .collect();
    let (last_flags, _) = decoded.last().expect("frames");
    assert!(
        last_flags & cursor::FLAG_END != 0,
        "the last frame is the Connect END trailer"
    );
    let (_, before_end) = &decoded[decoded.len() - 2];
    assert_eq!(
        interaction_updates(before_end),
        [14],
        "f1.14 turn_ended right before the trailer"
    );
}

/// H1 (#185): the turn ends on `f1.14 turn_ended`, not on silence. A server that holds the
/// stream after it (no trailer) must not cost the after-text budget.
#[test]
fn cursor_turn_ends_on_turn_ended_without_the_trailer() {
    let Some(dir) = cursor_scenarios("text").into_iter().next() else {
        panic!("no cursor text set");
    };
    let mut frames = timed_frames(&dir);
    frames.pop(); // the Connect END trailer
    let request = cursor_request(&dir);
    let mut wire = request.wire(&recorded_model(&dir), StreamBudgets::OFFLINE);
    let mut replies = Vec::new();
    for (_, chunk) in &frames {
        wire.on_chunk(chunk, &mut replies);
    }
    assert!(
        wire.core().done,
        "finished on f1.14 turn_ended, before any quiet budget"
    );
    let summary = summarize(&wire.core().pending.drain(..).collect::<Vec<_>>());
    assert!(
        summary
            .last()
            .is_some_and(|v| v["type"] == "finish" && v["reason"] == "stop"),
        "{summary:?}"
    );
}

/// F1 `quiet` (Cursor Grok 4.7, the retest's second request): after the first answer text
/// the model asked for its tool schemas (`mcp_state`), and the ThinEvent gaps before its
/// `find` call were ~3 s — inside the 25 s after-text budget, so the recorded clock replays
/// to the tool call.
#[test]
fn cursor_quiet_set_replays_on_its_clock_to_the_tool_call() {
    let Some(dir) = cursor_scenarios("quiet-").into_iter().next() else {
        eprintln!("skipped: no F1 quiet set");
        return;
    };
    let frames = timed_frames(&dir);
    let request = cursor_request(&dir);
    let mut wire = request.wire(&recorded_model(&dir), StreamBudgets::OFFLINE);
    let (events, _) = drive_timed(&mut wire, &frames);
    let summary = summarize(&events);
    let names: Vec<String> = tool_calls(&summary).into_iter().map(|c| c.0).collect();
    assert_eq!(names, ["find"], "{summary:?}");
}

/// H1 (#185): P3 on real frames. The 29 Sep turn went quiet for more than 25 s after its
/// text (request `chatcmpl-25c983bb…`); stretched to 30 s, the `quiet` set must still reach
/// its `find` call, not end as `stop` (a text-with-tools floor well above measured gaps,
/// or an end taken from the wire).
#[test]
fn cursor_quiet_gap_after_text_is_not_a_stop() {
    let Some(dir) = cursor_scenarios("quiet-").into_iter().next() else {
        panic!("no F1 quiet set");
    };
    let frames = timed_frames(&dir);
    let first_text = frames
        .iter()
        .position(|(_, chunk)| {
            let request = cursor::CursorRequest::offline();
            let mut wire = request.wire("m", StreamBudgets::OFFLINE);
            wire.on_chunk(chunk, &mut Vec::new());
            wire.core()
                .pending
                .iter()
                .any(|e| matches!(e, ThinEvent::TextDelta(_)))
        })
        .expect("the quiet set has answer text");
    // Every frame that arrived after the first answer text arrives 30 s later.
    let text_ms = frames[first_text].0;
    let stretched: Vec<(u64, Vec<u8>)> = frames
        .iter()
        .map(|(ms, chunk)| {
            let ms = if *ms > text_ms { ms + 30_000 } else { *ms };
            (ms, chunk.clone())
        })
        .collect();
    let request = cursor_request(&dir);
    let mut wire = request.wire(&recorded_model(&dir), StreamBudgets::OFFLINE);
    let (events, _) = drive_timed(&mut wire, &stretched);
    let summary = summarize(&events);
    let names: Vec<String> = tool_calls(&summary).into_iter().map(|c| c.0).collect();
    assert_eq!(names, ["find"], "{summary:?}");
}

/// F1 `mcp-repeat` (Cursor Fable, the retest's fifth request): the model called
/// `GetDynamicTools` twice; each call is `f1.2 tool_call_started` → `f1.27
/// tool_requests_listed {call_count}` → `f2.f36 mcp_state_exec_args {id}` → `f1.3
/// tool_call_completed {get_mcp_tools_tool_call (ToolCall f44)}`. Every ask gets exactly one
/// `mcp_state_exec_result` echoing its own id.
#[test]
fn cursor_mcp_repeat_asks_are_each_answered_with_their_own_id() {
    let Some(dir) = cursor_scenarios("mcp-repeat-").into_iter().next() else {
        eprintln!("skipped: no F1 mcp-repeat set");
        return;
    };
    let frames = load_frames(&dir).unwrap();
    let request = cursor_request(&dir);
    let mut wire = request.wire(&recorded_model(&dir), StreamBudgets::OFFLINE);
    let mut asks = Vec::new();
    let mut answered = Vec::new();
    for chunk in &frames {
        let mut replies = Vec::new();
        wire.on_chunk(chunk, &mut replies);
        for (_, payload) in chunk_payloads(chunk) {
            if let Some(ask) = cursor::extract_mcp_state_exec(&payload) {
                asks.push(ask.id);
            }
        }
        answered.extend(
            replies
                .iter()
                .filter_map(|r| exec_reply(r))
                .filter(|(_, field, _)| *field == 36)
                .map(|(id, _, _)| id),
        );
        if wire.core().done {
            break;
        }
    }
    assert!(asks.len() > 1, "the set repeats the state ask: {asks:?}");
    assert_eq!(answered, asks, "one result per ask, each with its own id");
}

/// H3 (#187): P5 on real frames. On 29 Sep the `GetDynamicTools` burst came before any
/// output (request `chatcmpl-c7b8fba0…`), so `1.27` / `1.3` fed the nested five-frame
/// counter and the turn failed `vendor_drift`. The `mcp-repeat` set with its reasoning
/// frames removed is that shape; `tool_requests_listed` and `tool_call_completed` are
/// measured-healthy progress, so the replay reaches its `write` call.
#[test]
fn cursor_tool_progress_frames_before_output_are_not_drift() {
    let Some(dir) = cursor_scenarios("mcp-repeat-").into_iter().next() else {
        panic!("no F1 mcp-repeat set");
    };
    let frames: Vec<Vec<u8>> = load_frames(&dir)
        .unwrap()
        .into_iter()
        .filter(|chunk| {
            let request = cursor::CursorRequest::offline();
            let mut wire = request.wire("m", StreamBudgets::OFFLINE);
            wire.on_chunk(chunk, &mut Vec::new());
            !wire
                .core()
                .pending
                .iter()
                .any(|e| matches!(e, ThinEvent::ReasoningDelta { .. }))
        })
        .collect();
    let summary = summarize(&cursor_request(&dir).replay(&recorded_model(&dir), &frames));
    assert!(!summary.iter().any(|v| v["type"] == "error"), "{summary:?}");
    let names: Vec<String> = tool_calls(&summary).into_iter().map(|c| c.0).collect();
    assert_eq!(names, ["write"], "{summary:?}");
}

/// P4 (plan F1 (d), H2 #186), caught on the way: the Run died `internal: No exec result`
/// right after the adapter answered `request_context_args`, before any output. It replays
/// as `503 vendor_down`, never a `stop`.
#[test]
fn cursor_handshake_death_replays_as_vendor_down() {
    let Some(dir) = cursor_scenarios("handshake-").into_iter().next() else {
        eprintln!("skipped: no handshake set");
        return;
    };
    let frames = load_frames(&dir).unwrap();
    let summary = summarize(&cursor_request(&dir).replay(&recorded_model(&dir), &frames));
    assert!(
        summary
            .iter()
            .any(|v| v["type"] == "error" && v["code"] == "vendor_down"),
        "{summary:?}"
    );
    assert!(
        !summary.iter().any(|v| v["type"] == "finish"),
        "{summary:?}"
    );
}
