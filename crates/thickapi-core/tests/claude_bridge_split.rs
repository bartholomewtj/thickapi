#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
//! C4 (SPEC §19.7 **Lifecycle**): the bridge child's guard and the one publish path, against
//! the `fake-claude` stub. Does not spawn the live `claude.exe` or bind the live daemon's port.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use serde_json::json;
use thickapi_core::config::models::ModelEntry;
use thickapi_core::thin::claude_bridge::{replay, spawn, BridgeSettings, ChildGuard, ClaudeBridge};
use thickapi_core::thin::openai::FinishReason;
use thickapi_core::thin::{ThinEvent, ThinRequest};
use thickapi_core::thinking::Effort;

fn stub_exe() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_fake-claude"))
}

/// `fake-claude` with no arguments reads stdin to EOF before it prints anything: while the
/// test holds its stdin, it only exits when killed.
fn hanging_stub(cwd: &std::path::Path) -> (ChildGuard, tokio::process::ChildStdin) {
    let env = spawn::claude_env(None, false, None);
    let mut spawned = spawn::spawn(&stub_exe(), cwd, &[], &env).expect("spawn fake-claude");
    let stdin = spawned.child.stdin.take().expect("stdin");
    (
        ChildGuard::new(spawned, BridgeSettings::default_handoff_drain()),
        stdin,
    )
}

async fn gone_within(pid: u32, budget: Duration) -> bool {
    let deadline = Instant::now() + budget;
    while Instant::now() < deadline {
        if !thickapi_core::process::process_alive(pid) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    !thickapi_core::process::process_alive(pid)
}

/// (b) Dropping the guard terminates a live child without blocking; `finish` terminates and
/// reaps within the drain budget (`handoff_drain_seconds`) + 1s.
#[tokio::test]
async fn child_guard_drop_terminates_and_finish_reaps_in_budget() {
    let tmp = tempfile::tempdir().unwrap();

    let (guard, _stdin) = hanging_stub(tmp.path());
    let pid = guard.pid();
    assert!(
        thickapi_core::process::process_alive(pid),
        "stub is running"
    );
    let t = Instant::now();
    drop(guard);
    assert!(
        // Far below the 10 s drain a blocking drop would wait, and loose enough for a loaded runner (#257).
        t.elapsed() < Duration::from_secs(2),
        "Drop must not wait on the child"
    );
    assert!(
        gone_within(pid, Duration::from_secs(5)).await,
        "dropped guard left pid {pid} running"
    );

    let (guard, _stdin) = hanging_stub(tmp.path());
    let pid = guard.pid();
    let t = Instant::now();
    let status = guard.finish().await;
    assert!(
        t.elapsed() < BridgeSettings::default_handoff_drain() + Duration::from_secs(1),
        "finish took {:?}",
        t.elapsed()
    );
    let status = status.expect("finish reaps the child");
    #[cfg(windows)]
    assert_eq!(status.code(), Some(spawn::KNOWN_EXIT as i32), "{status}");
    #[cfg(not(windows))]
    let _ = status;
    assert!(!thickapi_core::process::process_alive(pid));
}

fn two_tool_request() -> ThinRequest {
    let tool = |name: &str| {
        json!({"type": "function", "function": {"name": name,
            "parameters": {"type": "object", "properties": {}}}})
    };
    ThinRequest {
        request: serde_json::from_value(json!({
            "model": "m",
            "messages": [{"role": "user", "content": "read a.txt and list"}],
            "tools": [tool("read"), tool("bash")],
            "max_tokens": 64
        }))
        .unwrap(),
        entry: ModelEntry::bridge_probe("fake-two-tool-turn"),
        effort: Some(Effort::Off),
        request_id: "split-parity".into(),
        path: "bridge",
        fallback: None,
        cursor_cwd: None,
    }
}

/// (c) Publish parity: the live run against `fake-claude` and `replay` of the same recorded
/// lines yield the same events.
#[tokio::test]
async fn live_run_and_replay_publish_the_same_two_tool_turn() {
    let tmp = tempfile::tempdir().unwrap();
    let bridge = ClaudeBridge::new(BridgeSettings {
        tmp_root: tmp.path().to_path_buf(),
        claude_path: Some(stub_exe()),
        thickapi_exe: None,
        total_timeout: Duration::from_secs(15),
        idle_timeout: Duration::from_secs(10),
        mcp_ready_timeout: Duration::from_secs(5),
        trace: false,
        bridge_max_concurrent: 2,
        queue_timeout: Duration::from_secs(5),
        handoff_drain: BridgeSettings::default_handoff_drain(),
        extra_tools: vec![],
        nonempty_inventories: vec![],
        dialect: thickapi_core::thin::dialect::MODE_OFF.to_string(),
        catalogue_ids: vec![],
    });
    let (tx, mut rx) = tokio::sync::mpsc::channel(64);
    bridge
        .run_bridge(&two_tool_request(), &tx)
        .await
        .expect("stub run");
    drop(tx);
    let mut live = Vec::new();
    while let Some(ev) = rx.recv().await {
        live.push(ev);
    }

    let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/thin/claude-bridge/two-tool-turn.jsonl");
    let frames: Vec<Vec<u8>> = std::fs::read_to_string(fixture)
        .unwrap()
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| format!("{l}\n").into_bytes())
        .collect();
    let offline = replay(&frames, &["read", "bash"]);

    // The spawn's effort is known to the live run only (the access line's `wire_params`); a
    // replay of the child's stdout cannot name it, so compare the rest.
    match live.first() {
        Some(ThinEvent::Started { wire_params, .. }) => {
            assert_eq!(wire_params.as_deref(), Some("thinking=disabled"));
        }
        other => panic!("{other:?}"),
    }
    let without_params = |events: &[ThinEvent]| -> String {
        let events: Vec<ThinEvent> = events
            .iter()
            .cloned()
            .map(|e| match e {
                ThinEvent::Started {
                    model,
                    path,
                    fallback,
                    ..
                } => ThinEvent::Started {
                    model,
                    path,
                    fallback,
                    wire_params: None,
                },
                other => other,
            })
            .collect();
        format!("{events:?}")
    };
    assert_eq!(without_params(&live), without_params(&offline));
    assert_eq!(
        live.iter()
            .filter(|e| matches!(e, ThinEvent::ToolCall { .. }))
            .count(),
        2,
        "{live:?}"
    );
    assert!(matches!(
        live.last(),
        Some(ThinEvent::Finish(FinishReason::ToolCalls))
    ));
    assert_eq!(bridge.state().snapshot()["status"], "ok");
    let leftover: Vec<_> = std::fs::read_dir(tmp.path())
        .unwrap()
        .filter_map(|e| e.ok())
        .collect();
    assert!(leftover.is_empty(), "private dir survived: {leftover:?}");
}
