#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
// `test_child_lock` serialises the process-wide `THICKAPI_TEST_CHILD` env var for the whole of
// a test, awaits included. `#[tokio::test]` is a current-thread runtime, so the guard cannot
// move between workers and the lint's hazard does not apply here.
#![allow(clippy::await_holding_lock)]

use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use thickapi_core::{Effort, PoolManager, Store, TurnEngine, TurnEvent, TurnSpec};

fn test_child_lock() -> std::sync::MutexGuard<'static, ()> {
    test_child_lock_mutex()
        .lock()
        .unwrap_or_else(|err| err.into_inner())
}

fn test_child_lock_mutex() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

fn fixture_child(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures")
        .join(name)
}

fn base_spec(model: &str, session_id: &str, turn_id: &str) -> TurnSpec {
    TurnSpec {
        model: model.into(),
        prompt: "hi".into(),
        system_prompt: None,
        system_prompt_file: None,
        cwd: PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../.."),
        session_id: session_id.into(),
        thinking: Effort::Medium,
        tools: vec![],
        deny_writes: vec![],
        enforce_policy: None,
        mcp: thickapi_core::McpMode::Off,
        mcp_config: None,
        setting_sources: None,
        allow_api: false,
        combo: None,
        allow_paid: false,
        timeout_seconds: 30,
        client: None,
        turn_id: Some(turn_id.into()),
        metadata: serde_json::Map::new(),
    }
}

fn test_config(temp_data: &std::path::Path) -> thickapi_core::ConfigBundle {
    thickapi_core::ConfigBundle {
        config_dir: temp_data.to_path_buf(),
        router: thickapi_core::config::RouterConfig {
            listen: "127.0.0.1:8787".into(),
            data_dir: temp_data.join("data"),
            lan: false,
            default_timeout_seconds: 30,
            queue_timeout_seconds: 300,
            max_queue_depth: 16,
            max_paid_usd_per_day: 5.0,
            drain_seconds: 120,
            refuse_paths: vec![],
            ..Default::default()
        },
        accounts: serde_yaml::from_str(include_str!("../../../configs/accounts.yaml.example"))
            .expect("accounts"),
        models: serde_yaml::from_str(include_str!("../../../configs/models.yaml.example"))
            .expect("models"),
        combos: serde_yaml::from_str(include_str!("../../../configs/combos.yaml.example"))
            .expect("combos"),
        keys: thickapi_core::config::keys::KeysConfig::default(),
    }
}

#[tokio::test]
async fn engine_runs_fake_claude_child() {
    let cmd = fixture_child("fake-claude.cmd");
    if !cmd.exists() {
        return;
    }
    let _child_guard = test_child_lock();
    unsafe {
        std::env::set_var("THICKAPI_TEST_CHILD", cmd.to_string_lossy().to_string());
    }

    let temp = tempfile::tempdir().expect("tempdir");
    let config = test_config(temp.path());
    let store = Arc::new(Store::open_in_memory().expect("store"));
    let engine = TurnEngine::new(config, store);
    let cwd = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let spec = TurnSpec {
        model: "subs/claude/fable".into(),
        prompt: "hi".into(),
        system_prompt: None,
        system_prompt_file: None,
        cwd,
        session_id: "engine-test-1".into(),
        thinking: Effort::Medium,
        tools: vec![],
        deny_writes: vec![],
        enforce_policy: None,
        mcp: thickapi_core::McpMode::Off,
        mcp_config: None,
        setting_sources: None,
        allow_api: false,
        combo: None,
        allow_paid: false,
        timeout_seconds: 30,
        client: None,
        turn_id: Some("turn-fixture-1".into()),
        metadata: serde_json::Map::new(),
    };
    let (tx, mut rx) = tokio::sync::mpsc::channel(64);
    let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });
    let result = engine.run_turn(spec, tx).await.expect("turn");
    drain.await.ok();
    assert!(result.text.contains("Hello from Claude"));
    assert_eq!(result.state, thickapi_core::TurnState::Success);

    let artifact_dir = temp
        .path()
        .join("data")
        .join("turns")
        .join("turn-fixture-1");
    assert!(artifact_dir.join("result.json").exists());
    assert!(artifact_dir.join("stdout.jsonl").exists());

    unsafe {
        std::env::remove_var("THICKAPI_TEST_CHILD");
    }
}

#[tokio::test]
async fn engine_emits_queued_under_pool_contention() {
    let cmd = fixture_child("fake-claude.cmd");
    if !cmd.exists() {
        return;
    }
    let _child_guard = test_child_lock();
    unsafe {
        std::env::set_var("THICKAPI_TEST_CHILD", cmd.to_string_lossy().to_string());
    }

    let temp = tempfile::tempdir().expect("tempdir");
    let mut config = test_config(temp.path());
    config
        .accounts
        .pools
        .get_mut("claude")
        .expect("claude pool")
        .max_concurrent = 1;

    let store = Arc::new(Store::open_in_memory().expect("store"));
    let pools = Arc::new(PoolManager::new(
        &config.accounts,
        config.router.queue_timeout_seconds,
        config.router.max_queue_depth,
    ));
    let guard = pools
        .acquire("claude", &mut |_| {}, None)
        .await
        .expect("hold pool slot");

    let engine = Arc::new(TurnEngine::new(config, store).with_pools(pools));
    let cwd = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let spec = TurnSpec {
        model: "subs/claude/fable".into(),
        prompt: "hi".into(),
        system_prompt: None,
        system_prompt_file: None,
        cwd,
        session_id: "engine-queued-1".into(),
        thinking: Effort::Medium,
        tools: vec![],
        deny_writes: vec![],
        enforce_policy: None,
        mcp: thickapi_core::McpMode::Off,
        mcp_config: None,
        setting_sources: None,
        allow_api: false,
        combo: None,
        allow_paid: false,
        timeout_seconds: 30,
        client: None,
        turn_id: Some("turn-queued-1".into()),
        metadata: serde_json::Map::new(),
    };

    let (tx, mut rx) = tokio::sync::mpsc::channel(64);
    let turn_task = {
        let engine = engine.clone();
        tokio::spawn(async move { engine.run_turn(spec, tx).await })
    };

    let mut saw_queued = false;
    while let Ok(Some(event)) = tokio::time::timeout(Duration::from_secs(3), rx.recv()).await {
        if matches!(event, TurnEvent::Queued { .. }) {
            saw_queued = true;
            break;
        }
    }
    assert!(saw_queued, "expected Queued while pool slot held");

    drop(guard);
    let _ = tokio::time::timeout(Duration::from_secs(30), turn_task)
        .await
        .expect("turn finished");

    unsafe {
        std::env::remove_var("THICKAPI_TEST_CHILD");
    }
}

/// A child that floods stdout before it reads a byte of stdin: the shape that deadlocks a
/// blocking prompt write (B6). `dir` gets `pad.txt`, `tail.jsonl` and the `.cmd` itself.
fn write_stdout_first_child(dir: &std::path::Path) -> PathBuf {
    let pad: String = std::iter::repeat_n("x".repeat(120) + "\r\n", 3000).collect();
    std::fs::write(dir.join("pad.txt"), pad).expect("pad");
    write_jsonl_tail(dir, "STDIN-DRAINED");
    let cmd = dir.join("stdout-first.cmd");
    std::fs::write(
        &cmd,
        "@echo off\r\nsetlocal\r\ntype \"%~dp0pad.txt\"\r\nfindstr /v /c:\"@@nomatch@@\" > nul\r\ntype \"%~dp0tail.jsonl\"\r\n",
    )
    .expect("cmd");
    cmd
}

/// A ~1 MiB prompt sent to a child that writes 360 KiB of stdout before draining stdin. The
/// blocking `write_all` in `spawn_child` never returned here: the child sat on a full stdout
/// pipe with no reader, so this hung until the test harness gave up.
#[cfg(windows)]
#[tokio::test]
async fn large_prompt_to_stdout_first_child_does_not_deadlock() {
    let _child_guard = test_child_lock();
    let temp = tempfile::tempdir().expect("tempdir");
    let cmd = write_stdout_first_child(temp.path());
    unsafe {
        std::env::set_var("THICKAPI_TEST_CHILD", cmd.to_string_lossy().to_string());
    }

    let config = test_config(temp.path());
    let store = Arc::new(Store::open_in_memory().expect("store"));
    let engine = TurnEngine::new(config, store);
    let mut spec = base_spec("subs/claude/fable", "engine-big-stdin", "turn-big-stdin");
    spec.prompt = "A".repeat(1024 * 1024);
    spec.timeout_seconds = 60;

    let (tx, mut rx) = tokio::sync::mpsc::channel(64);
    let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });
    let result = tokio::time::timeout(Duration::from_secs(60), engine.run_turn(spec, tx))
        .await
        .expect("1 MiB prompt must not deadlock the child")
        .expect("turn");
    drain.await.ok();
    assert_eq!(result.state, thickapi_core::TurnState::Success);
    assert!(
        result.text.contains("STDIN-DRAINED"),
        "child must have reached EOF on stdin: {}",
        result.text
    );

    unsafe {
        std::env::remove_var("THICKAPI_TEST_CHILD");
    }
}

/// One assistant line the Claude adapter will parse, for a fixture child to `type` at the end.
fn write_jsonl_tail(dir: &std::path::Path, marker: &str) {
    std::fs::write(
        dir.join("tail.jsonl"),
        format!(
            "{{\"type\":\"assistant\",\"message\":{{\"content\":[{{\"type\":\"text\",\"text\":\"{marker}\"}}]}}}}\r\n"
        ),
    )
    .expect("tail");
}

/// A child that says nothing at all for twenty seconds.
fn write_silent_child(dir: &std::path::Path) -> PathBuf {
    let cmd = dir.join("silent.cmd");
    std::fs::write(&cmd, "@echo off\r\nping -n 20 127.0.0.1 > nul\r\n").expect("cmd");
    cmd
}

/// A child whose only sign of life for ten seconds is stderr, then a real assistant line.
fn write_stderr_only_child(dir: &std::path::Path) -> PathBuf {
    write_jsonl_tail(dir, "STDERR-SURVIVOR");
    let cmd = dir.join("stderr-only.cmd");
    std::fs::write(
        &cmd,
        "@echo off\r\nfor /L %%i in (1,1,5) do (\r\n  ping -n 3 127.0.0.1 > nul\r\n  echo working %%i 1>&2\r\n)\r\ntype \"%~dp0tail.jsonl\"\r\n",
    )
    .expect("cmd");
    cmd
}

/// `router.yaml idle_stall_seconds` is honoured, and the stall is a proper terminal state
/// rather than a bare `Err` (B12, B5).
#[cfg(windows)]
#[tokio::test]
async fn silent_child_hits_the_configured_idle_stall() {
    let _child_guard = test_child_lock();
    let temp = tempfile::tempdir().expect("tempdir");
    let cmd = write_silent_child(temp.path());
    unsafe {
        std::env::set_var("THICKAPI_TEST_CHILD", cmd.to_string_lossy().to_string());
    }

    let mut config = test_config(temp.path());
    config.router.idle_stall_seconds.insert("claude".into(), 1);
    let store = Arc::new(Store::open_in_memory().expect("store"));
    let engine = TurnEngine::new(config, store);
    let mut spec = base_spec("subs/claude/fable", "engine-stall", "turn-stall");
    spec.timeout_seconds = 120;

    let (result, events) = collect_events(engine, spec).await;
    let err = result.expect_err("idle stall must fail the turn");
    assert!(err.to_string().contains("idle stall"), "{err}");
    assert!(events.iter().any(|e| matches!(
        e,
        TurnEvent::TurnEnd {
            state: thickapi_core::TurnState::Timeout,
            ..
        }
    )));

    unsafe {
        std::env::remove_var("THICKAPI_TEST_CHILD");
    }
}

/// Stderr progress used not to reset the stall, so a CLI that logs only there was killed at
/// the window even while it was plainly working (B12).
#[cfg(windows)]
#[tokio::test]
async fn stderr_activity_keeps_a_quiet_child_alive() {
    let _child_guard = test_child_lock();
    let temp = tempfile::tempdir().expect("tempdir");
    let cmd = write_stderr_only_child(temp.path());
    unsafe {
        std::env::set_var("THICKAPI_TEST_CHILD", cmd.to_string_lossy().to_string());
    }

    let mut config = test_config(temp.path());
    config.router.idle_stall_seconds.insert("claude".into(), 4);
    let store = Arc::new(Store::open_in_memory().expect("store"));
    let engine = TurnEngine::new(config, store);
    let mut spec = base_spec("subs/claude/fable", "engine-stderr", "turn-stderr");
    spec.timeout_seconds = 120;

    let (tx, mut rx) = tokio::sync::mpsc::channel(64);
    let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });
    let result = engine.run_turn(spec, tx).await.expect("turn");
    drain.await.ok();
    assert_eq!(result.state, thickapi_core::TurnState::Success);
    assert!(
        result.text.contains("STDERR-SURVIVOR"),
        "child must have run to completion: {}",
        result.text
    );

    unsafe {
        std::env::remove_var("THICKAPI_TEST_CHILD");
    }
}

/// A child that hands its stdout to a grandchild and exits at once. The last assistant line
/// arrives about two seconds after the child is reaped, so the pipe outlives the process (B15).
fn write_late_writer_child(dir: &std::path::Path) -> PathBuf {
    std::fs::write(
        dir.join("late.cmd"),
        "@echo off\r\nping -n 3 127.0.0.1 > nul\r\necho {\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"LATE-LINE\"}]}}\r\n",
    )
    .expect("late");
    let cmd = dir.join("late-writer.cmd");
    std::fs::write(
        &cmd,
        "@echo off\r\necho {\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"EARLY-LINE\"}]}}\r\nstart /b \"\" \"%~dp0late.cmd\"\r\n",
    )
    .expect("cmd");
    cmd
}

/// Draining to the first 50 ms gap and only then joining the reader dropped this line on the
/// floor: `result.text` stopped at `EARLY-LINE`.
#[cfg(windows)]
#[tokio::test]
async fn line_after_child_exit_still_reaches_the_result() {
    let _child_guard = test_child_lock();
    let temp = tempfile::tempdir().expect("tempdir");
    let cmd = write_late_writer_child(temp.path());
    unsafe {
        std::env::set_var("THICKAPI_TEST_CHILD", cmd.to_string_lossy().to_string());
    }

    let config = test_config(temp.path());
    let store = Arc::new(Store::open_in_memory().expect("store"));
    let engine = TurnEngine::new(config, store);
    let spec = base_spec("subs/claude/fable", "engine-late-line", "turn-late-line");

    let (tx, mut rx) = tokio::sync::mpsc::channel(64);
    let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });
    let result = engine.run_turn(spec, tx).await.expect("turn");
    drain.await.ok();
    assert!(
        result.text.contains("LATE-LINE"),
        "a line written after the child exited must reach the accumulator: {}",
        result.text
    );

    unsafe {
        std::env::remove_var("THICKAPI_TEST_CHILD");
    }
}

async fn collect_events(
    engine: TurnEngine,
    spec: TurnSpec,
) -> (
    Result<thickapi_core::TurnResult, thickapi_core::TurnError>,
    Vec<TurnEvent>,
) {
    let (tx, mut rx) = tokio::sync::mpsc::channel(64);
    let collect = tokio::spawn(async move {
        let mut events = Vec::new();
        while let Some(event) = rx.recv().await {
            events.push(event);
        }
        events
    });
    let result = engine.run_turn(spec, tx).await;
    let events = collect.await.expect("events");
    (result, events)
}

#[tokio::test]
async fn combo_grok_rate_limit_without_allow_paid_emits_fallback_then_rate_limited() {
    let rate_cmd = fixture_child("fake-grok-rate-limit.cmd");
    if !rate_cmd.exists() {
        return;
    }
    let _child_guard = test_child_lock();
    unsafe {
        std::env::set_var(
            "THICKAPI_TEST_CHILD",
            rate_cmd.to_string_lossy().to_string(),
        );
    }

    let temp = tempfile::tempdir().expect("tempdir");
    let config = test_config(temp.path());
    let store = Arc::new(Store::open_in_memory().expect("store"));
    let engine = TurnEngine::new(config, store);
    let cwd = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let spec = TurnSpec {
        model: "subs/grok/grok-4.7".into(),
        prompt: "hi".into(),
        system_prompt: None,
        system_prompt_file: None,
        cwd,
        session_id: "combo-no-paid".into(),
        thinking: Effort::Medium,
        tools: vec![],
        deny_writes: vec![],
        enforce_policy: None,
        mcp: thickapi_core::McpMode::Off,
        mcp_config: None,
        setting_sources: None,
        allow_api: false,
        combo: Some("grok-then-or".into()),
        allow_paid: false,
        timeout_seconds: 30,
        client: None,
        turn_id: Some("combo-no-paid-turn".into()),
        metadata: serde_json::Map::new(),
    };

    let (result, events) = collect_events(engine, spec).await;
    let result = result.expect("turn");
    assert_eq!(result.state, thickapi_core::TurnState::RateLimited);
    assert!(events.iter().any(|e| {
        matches!(
            e,
            TurnEvent::TurnFallback {
                from_model,
                to_model,
                reason,
            } if from_model == "subs/grok/grok-4.7"
                && to_model == "or/anthropic/claude-opus-5"
                && reason == "rate_limited"
        )
    }));
    assert!(events.iter().any(|e| matches!(
        e,
        TurnEvent::TurnEnd {
            state: thickapi_core::TurnState::RateLimited,
            ..
        }
    )));

    unsafe {
        std::env::remove_var("THICKAPI_TEST_CHILD");
    }
}

#[tokio::test]
async fn combo_grok_rate_limit_with_allow_paid_runs_pi_tail() {
    let combo_cmd = fixture_child("fake-combo.cmd");
    if !combo_cmd.exists() {
        return;
    }
    let _child_guard = test_child_lock();
    unsafe {
        std::env::set_var(
            "THICKAPI_TEST_CHILD",
            combo_cmd.to_string_lossy().to_string(),
        );
        std::env::set_var("OPENROUTER_API_KEY", "test-key");
    }

    let temp = tempfile::tempdir().expect("tempdir");
    let config = test_config(temp.path());
    let store = Arc::new(Store::open_in_memory().expect("store"));
    let engine = TurnEngine::new(config, store);
    let cwd = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let spec = TurnSpec {
        model: "subs/grok/grok-4.7".into(),
        prompt: "hi".into(),
        system_prompt: None,
        system_prompt_file: None,
        cwd,
        session_id: "combo-paid".into(),
        thinking: Effort::Medium,
        tools: vec![],
        deny_writes: vec![],
        enforce_policy: None,
        mcp: thickapi_core::McpMode::Off,
        mcp_config: None,
        setting_sources: None,
        allow_api: false,
        combo: Some("grok-then-or".into()),
        allow_paid: true,
        timeout_seconds: 30,
        client: None,
        turn_id: Some("combo-paid-turn".into()),
        metadata: serde_json::Map::new(),
    };

    let (result, events) = collect_events(engine, spec).await;
    let result = result.expect("turn");
    assert_eq!(result.state, thickapi_core::TurnState::Success);
    assert!(result.text.contains("Hello from Pi"));
    assert!(events
        .iter()
        .any(|e| matches!(e, TurnEvent::TurnFallback { .. })));

    unsafe {
        std::env::remove_var("THICKAPI_TEST_CHILD");
        std::env::remove_var("OPENROUTER_API_KEY");
    }
}

#[tokio::test]
async fn combo_paid_cap_blocks_pi_tail() {
    let rate_cmd = fixture_child("fake-grok-rate-limit.cmd");
    if !rate_cmd.exists() {
        return;
    }
    let _child_guard = test_child_lock();
    unsafe {
        std::env::set_var(
            "THICKAPI_TEST_CHILD",
            rate_cmd.to_string_lossy().to_string(),
        );
    }

    let temp = tempfile::tempdir().expect("tempdir");
    let mut config = test_config(temp.path());
    config.router.max_paid_usd_per_day = 0.01;
    let store = Arc::new(Store::open_in_memory().expect("store"));
    store.add_paid_spend(0.01).expect("seed spend");
    let engine = TurnEngine::new(config, store);
    let cwd = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let spec = TurnSpec {
        model: "subs/grok/grok-4.7".into(),
        prompt: "hi".into(),
        system_prompt: None,
        system_prompt_file: None,
        cwd,
        session_id: "combo-cap".into(),
        thinking: Effort::Medium,
        tools: vec![],
        deny_writes: vec![],
        enforce_policy: None,
        mcp: thickapi_core::McpMode::Off,
        mcp_config: None,
        setting_sources: None,
        allow_api: false,
        combo: Some("grok-then-or".into()),
        allow_paid: true,
        timeout_seconds: 30,
        client: None,
        turn_id: Some("combo-cap-turn".into()),
        metadata: serde_json::Map::new(),
    };

    let (result, events) = collect_events(engine, spec).await;
    let err = result.expect_err("paid cap is 429, not 200");
    assert!(
        matches!(err, thickapi_core::TurnError::RateLimited(ref msg) if msg.contains("daily paid cap exceeded")),
        "{err:?}"
    );
    let stored = result_json(temp.path(), "combo-cap-turn");
    assert_eq!(stored["state"], "rate_limited");
    assert_eq!(stored["error_code"], "rate_limited");
    assert!(!events
        .iter()
        .any(|e| matches!(e, TurnEvent::TurnFallback { .. })));

    unsafe {
        std::env::remove_var("THICKAPI_TEST_CHILD");
    }
}

#[tokio::test]
async fn pi_headerless_429_sets_suspected_blocked_health() {
    let blocked_cmd = fixture_child("fake-pi-blocked.cmd");
    if !blocked_cmd.exists() {
        return;
    }
    let _child_guard = test_child_lock();
    unsafe {
        std::env::set_var(
            "THICKAPI_TEST_CHILD",
            blocked_cmd.to_string_lossy().to_string(),
        );
        std::env::set_var("OPENROUTER_API_KEY", "test-key");
    }

    let temp = tempfile::tempdir().expect("tempdir");
    let config = test_config(temp.path());
    let store = Arc::new(Store::open_in_memory().expect("store"));
    let engine = TurnEngine::new(config, store.clone());
    let cwd = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let spec = TurnSpec {
        model: "or/anthropic/claude-opus-5".into(),
        prompt: "hi".into(),
        system_prompt: None,
        system_prompt_file: None,
        cwd,
        session_id: "pi-blocked".into(),
        thinking: Effort::Medium,
        tools: vec![],
        deny_writes: vec![],
        enforce_policy: None,
        mcp: thickapi_core::McpMode::Off,
        mcp_config: None,
        setting_sources: None,
        allow_api: false,
        combo: None,
        allow_paid: true,
        timeout_seconds: 30,
        client: None,
        turn_id: Some("pi-blocked-turn".into()),
        metadata: serde_json::Map::new(),
    };

    let (result, _) = collect_events(engine, spec).await;
    let result = result.expect("turn");
    assert_eq!(result.state, thickapi_core::TurnState::RateLimited);
    let status = store
        .get_account_health("or-main")
        .expect("health")
        .expect("row");
    assert_eq!(status, "suspected_blocked");

    unsafe {
        std::env::remove_var("THICKAPI_TEST_CHILD");
        std::env::remove_var("OPENROUTER_API_KEY");
    }
}

// --- failure paths (B5): every one of these used to leave the caller with nothing ---

/// A child that exits non-zero without saying anything on stdout.
fn write_failing_child(dir: &std::path::Path) -> PathBuf {
    let cmd = dir.join("fails.cmd");
    std::fs::write(&cmd, "@echo off\r\necho boom 1>&2\r\nexit /b 3\r\n").expect("cmd");
    cmd
}

/// A child that prints lines no adapter can parse before its real output.
fn write_garbage_child(dir: &std::path::Path) -> PathBuf {
    write_jsonl_tail(dir, "AFTER-GARBAGE");
    let cmd = dir.join("garbage.cmd");
    std::fs::write(
        &cmd,
        "@echo off\r\necho not json at all\r\necho {\"unterminated\": \r\necho ###\r\ntype \"%~dp0tail.jsonl\"\r\n",
    )
    .expect("cmd");
    cmd
}

fn result_json(temp: &std::path::Path, turn_id: &str) -> serde_json::Value {
    let path = temp
        .join("data")
        .join("turns")
        .join(turn_id)
        .join("result.json");
    let text = std::fs::read_to_string(&path).expect("result.json must be written");
    serde_json::from_str(&text).expect("result.json")
}

#[cfg(windows)]
#[tokio::test]
async fn child_past_total_timeout_ends_as_timeout() {
    let _child_guard = test_child_lock();
    let temp = tempfile::tempdir().expect("tempdir");
    let cmd = write_silent_child(temp.path());
    unsafe {
        std::env::set_var("THICKAPI_TEST_CHILD", cmd.to_string_lossy().to_string());
    }

    let config = test_config(temp.path());
    let store = Arc::new(Store::open_in_memory().expect("store"));
    let engine = TurnEngine::new(config, store);
    let mut spec = base_spec("subs/claude/fable", "engine-timeout", "turn-timeout");
    spec.timeout_seconds = 2;

    let (result, events) = collect_events(engine, spec).await;
    let err = result.expect_err("a turn past its timeout is an error to the caller");
    assert!(err.to_string().starts_with("turn_timeout:"), "{err}");
    assert!(events.iter().any(|e| matches!(
        e,
        TurnEvent::TurnEnd {
            state: thickapi_core::TurnState::Timeout,
            ..
        }
    )));
    assert_eq!(result_json(temp.path(), "turn-timeout")["state"], "timeout");

    unsafe {
        std::env::remove_var("THICKAPI_TEST_CHILD");
    }
}

#[cfg(windows)]
#[tokio::test]
async fn cancel_mid_turn_ends_as_cancelled() {
    let _child_guard = test_child_lock();
    let temp = tempfile::tempdir().expect("tempdir");
    let cmd = write_silent_child(temp.path());
    unsafe {
        std::env::set_var("THICKAPI_TEST_CHILD", cmd.to_string_lossy().to_string());
    }

    let config = test_config(temp.path());
    let store = Arc::new(Store::open_in_memory().expect("store"));
    let registry = Arc::new(thickapi_core::TurnRegistry::new());
    let (cancel, _events) = registry
        .register("turn-cancel".to_string())
        .expect("register");
    let engine = Arc::new(TurnEngine::new(config, store).with_registry(registry.clone()));
    let mut spec = base_spec("subs/claude/fable", "engine-cancel", "turn-cancel");
    spec.timeout_seconds = 120;

    let (tx, mut rx) = tokio::sync::mpsc::channel(64);
    let turn = {
        let engine = engine.clone();
        tokio::spawn(async move { engine.run_turn(spec, tx).await })
    };

    while let Some(event) = rx.recv().await {
        if matches!(event, TurnEvent::TurnStart { .. }) {
            break;
        }
    }
    tokio::time::sleep(Duration::from_millis(500)).await;
    cancel.cancel();

    let mut saw_end = false;
    while let Some(event) = rx.recv().await {
        if matches!(
            event,
            TurnEvent::TurnEnd {
                state: thickapi_core::TurnState::Cancelled,
                ..
            }
        ) {
            saw_end = true;
        }
    }
    let result = turn.await.expect("join").expect("turn");
    assert_eq!(result.state, thickapi_core::TurnState::Cancelled);
    assert!(saw_end, "a cancelled turn still announces its end");
    assert!(registry.is_terminal(&"turn-cancel".to_string()));
    assert_eq!(
        result_json(temp.path(), "turn-cancel")["state"],
        "cancelled"
    );

    unsafe {
        std::env::remove_var("THICKAPI_TEST_CHILD");
    }
}

#[cfg(windows)]
#[tokio::test]
async fn non_zero_exit_ends_as_failed() {
    let _child_guard = test_child_lock();
    let temp = tempfile::tempdir().expect("tempdir");
    let cmd = write_failing_child(temp.path());
    unsafe {
        std::env::set_var("THICKAPI_TEST_CHILD", cmd.to_string_lossy().to_string());
    }

    let config = test_config(temp.path());
    let store = Arc::new(Store::open_in_memory().expect("store"));
    let engine = TurnEngine::new(config, store);
    let spec = base_spec("subs/claude/fable", "engine-exit3", "turn-exit3");

    let (result, events) = collect_events(engine, spec).await;
    let result = result.expect("a non-zero exit is still a result");
    assert_eq!(result.state, thickapi_core::TurnState::Failed);
    assert_eq!(result.returncode, 3);
    assert!(events.iter().any(|e| matches!(
        e,
        TurnEvent::TurnEnd {
            state: thickapi_core::TurnState::Failed,
            ..
        }
    )));

    unsafe {
        std::env::remove_var("THICKAPI_TEST_CHILD");
    }
}

#[cfg(windows)]
#[tokio::test]
async fn garbage_lines_do_not_derail_the_turn() {
    let _child_guard = test_child_lock();
    let temp = tempfile::tempdir().expect("tempdir");
    let cmd = write_garbage_child(temp.path());
    unsafe {
        std::env::set_var("THICKAPI_TEST_CHILD", cmd.to_string_lossy().to_string());
    }

    let config = test_config(temp.path());
    let store = Arc::new(Store::open_in_memory().expect("store"));
    let engine = TurnEngine::new(config, store);
    let spec = base_spec("subs/claude/fable", "engine-garbage", "turn-garbage");

    let (tx, mut rx) = tokio::sync::mpsc::channel(64);
    let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });
    let result = engine.run_turn(spec, tx).await.expect("turn");
    drain.await.ok();
    assert_eq!(result.state, thickapi_core::TurnState::Success);
    assert!(result.text.contains("AFTER-GARBAGE"), "{}", result.text);

    unsafe {
        std::env::remove_var("THICKAPI_TEST_CHILD");
    }
}

/// `build_spawn` failing — the CLI is simply not installed — is the one terminal condition
/// that never even reached the engine's child loop.
#[cfg(windows)]
#[tokio::test]
async fn cli_missing_from_path_ends_the_turn_typed() {
    let _child_guard = test_child_lock();
    let temp = tempfile::tempdir().expect("tempdir");
    let saved_path = std::env::var("PATH").unwrap_or_default();
    unsafe {
        std::env::remove_var("THICKAPI_TEST_CHILD");
        std::env::set_var("PATH", "");
    }

    let config = test_config(temp.path());
    let store = Arc::new(Store::open_in_memory().expect("store"));
    let engine = TurnEngine::new(config, store);
    let spec = base_spec("subs/claude/fable", "engine-no-cli", "turn-no-cli");

    let (result, events) = collect_events(engine, spec).await;

    unsafe {
        std::env::set_var("PATH", saved_path);
    }

    let err = result.expect_err("no CLI, no turn");
    assert!(err.to_string().starts_with("spawn_failed:"), "{err}");
    assert!(events.iter().any(|e| matches!(
        e,
        TurnEvent::TurnEnd {
            state: thickapi_core::TurnState::Failed,
            ..
        }
    )));
    assert_eq!(result_json(temp.path(), "turn-no-cli")["state"], "failed");
}

/// A child that holds its turn for about four seconds, then answers.
fn write_slow_child(dir: &std::path::Path) -> PathBuf {
    write_jsonl_tail(dir, "SLOW-DONE");
    let cmd = dir.join("slow.cmd");
    std::fs::write(
        &cmd,
        "@echo off\r\nping -n 5 127.0.0.1 > nul\r\ntype \"%~dp0tail.jsonl\"\r\n",
    )
    .expect("cmd");
    cmd
}

fn enforce_everything() -> thickapi_core::EnforcePolicy {
    thickapi_core::EnforcePolicy {
        writes: Some(vec!["**/*".into()]),
        protected_files: vec![],
        out_of_scope: None,
        always_writable: vec![],
        mode: thickapi_core::EnforceMode::TreeCopy,
    }
}

/// The per-cwd lock used to be a `std::sync::Mutex` taken from async code, so a second turn in
/// the same cwd parked the runtime's only worker for the length of the first child: on this
/// one-worker runtime nothing else got to run at all (B7).
#[cfg(windows)]
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn a_queued_same_cwd_turn_does_not_park_the_runtime() {
    use std::sync::atomic::{AtomicU32, Ordering};

    let _child_guard = test_child_lock();
    let temp = tempfile::tempdir().expect("tempdir");
    let cmd = write_slow_child(temp.path());
    unsafe {
        std::env::set_var("THICKAPI_TEST_CHILD", cmd.to_string_lossy().to_string());
    }

    let work = tempfile::tempdir().expect("workdir");
    std::fs::write(work.path().join("keep.txt"), "x").expect("seed");

    let config = test_config(temp.path());
    let store = Arc::new(Store::open_in_memory().expect("store"));
    let engine = Arc::new(TurnEngine::new(config, store));

    let run = |turn: u32| {
        let engine = engine.clone();
        let cwd = work.path().to_path_buf();
        async move {
            let mut spec = base_spec(
                "subs/claude/fable",
                &format!("cwd-lock-{turn}"),
                &format!("turn-cwd-lock-{turn}"),
            );
            spec.cwd = cwd;
            spec.enforce_policy = Some(enforce_everything());
            spec.timeout_seconds = 120;
            let (tx, mut rx) = tokio::sync::mpsc::channel(64);
            let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });
            let result = engine.run_turn(spec, tx).await;
            drain.await.ok();
            result
        }
    };

    let first = tokio::spawn(run(1));
    // Let the first turn get past its snapshot and into the child, holding the cwd lock.
    tokio::time::sleep(Duration::from_millis(1200)).await;

    let ticks = Arc::new(AtomicU32::new(0));
    let ticker = {
        let ticks = ticks.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_millis(50)).await;
                ticks.fetch_add(1, Ordering::SeqCst);
            }
        })
    };
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Sampled off the runtime, so a parked worker cannot delay the reading itself.
    let probe = {
        let ticks = ticks.clone();
        tokio::task::spawn_blocking(move || {
            std::thread::sleep(Duration::from_millis(1500));
            ticks.load(Ordering::SeqCst)
        })
    };
    let second = tokio::spawn(run(2));

    let sampled = probe.await.expect("probe");
    assert!(
        sampled > 5,
        "the runtime must keep scheduling while a same-cwd turn waits; saw {sampled} ticks"
    );

    ticker.abort();
    for task in [first, second] {
        let result = tokio::time::timeout(Duration::from_secs(120), task)
            .await
            .expect("both same-cwd turns finish")
            .expect("join")
            .expect("turn");
        assert_eq!(result.state, thickapi_core::TurnState::Success);
    }

    unsafe {
        std::env::remove_var("THICKAPI_TEST_CHILD");
    }
}

/// A child that writes far more assistant lines than the 64-slot live channel can hold.
fn write_chatty_child(dir: &std::path::Path) -> PathBuf {
    let mut lines = String::new();
    for i in 0..200 {
        lines.push_str(&format!(
            "{{\"type\":\"assistant\",\"message\":{{\"content\":[{{\"type\":\"text\",\"text\":\"chunk-{i} \"}}]}}}}\r\n"
        ));
    }
    std::fs::write(dir.join("chatty.jsonl"), lines).expect("chatty");
    let cmd = dir.join("chatty.cmd");
    std::fs::write(&cmd, "@echo off\r\ntype \"%~dp0chatty.jsonl\"\r\n").expect("cmd");
    cmd
}

/// A consumer that reads nothing until the child is done used to lose the end of its own turn:
/// `try_send` dropped `TurnEnd` along with the overflowing deltas (B9).
#[cfg(windows)]
#[tokio::test]
async fn the_terminal_event_survives_a_slow_consumer() {
    let _child_guard = test_child_lock();
    let temp = tempfile::tempdir().expect("tempdir");
    let cmd = write_chatty_child(temp.path());
    unsafe {
        std::env::set_var("THICKAPI_TEST_CHILD", cmd.to_string_lossy().to_string());
    }

    let config = test_config(temp.path());
    let store = Arc::new(Store::open_in_memory().expect("store"));
    let engine = TurnEngine::new(config, store);
    let spec = base_spec("subs/claude/fable", "engine-slow-rx", "turn-slow-rx");

    let (tx, mut rx) = tokio::sync::mpsc::channel(64);
    let turn = tokio::spawn(async move { engine.run_turn(spec, tx).await });

    // Read nothing while the child runs, so the channel is full when the turn ends.
    tokio::time::sleep(Duration::from_secs(2)).await;

    let mut events = Vec::new();
    while let Some(event) = rx.recv().await {
        events.push(event);
    }
    let result = turn.await.expect("join").expect("turn");
    assert_eq!(result.state, thickapi_core::TurnState::Success);
    assert!(
        matches!(events.last(), Some(TurnEvent::TurnEnd { .. })),
        "the last live event must be turn_end; got {:?}",
        events.last()
    );

    unsafe {
        std::env::remove_var("THICKAPI_TEST_CHILD");
    }
}

/// A grok stand-in that logs its argv to `argv.log`, then answers under a session id of its
/// own minting, the way the real CLI does.
fn write_argv_logging_grok(dir: &std::path::Path, cli_session: &str) -> PathBuf {
    std::fs::write(
        dir.join("stream.jsonl"),
        format!(
            "{{\"type\":\"init\",\"session_id\":\"{cli_session}\"}}\r\n{{\"type\":\"assistant_message\",\"content\":\"ok\"}}\r\n"
        ),
    )
    .expect("stream");
    let cmd = dir.join("grok-argv.cmd");
    std::fs::write(
        &cmd,
        "@echo off\r\necho %* >> \"%~dp0argv.log\"\r\ntype \"%~dp0stream.jsonl\"\r\n",
    )
    .expect("cmd");
    cmd
}

/// C23: the second turn of a thick grok session resumes the conversation the first one
/// started, instead of opening a new one every time.
#[cfg(windows)]
#[tokio::test]
async fn second_grok_turn_resumes_the_first_turns_cli_session() {
    let _child_guard = test_child_lock();
    let temp = tempfile::tempdir().expect("tempdir");
    let cli_session = "5d0c1f6e-2b7a-4f0e-9a3c-7e1d2c3b4a59";
    let cmd = write_argv_logging_grok(temp.path(), cli_session);
    unsafe {
        std::env::set_var("THICKAPI_TEST_CHILD", cmd.to_string_lossy().to_string());
    }

    let store = Arc::new(Store::open_in_memory().expect("store"));
    for turn_id in ["grok-resume-1", "grok-resume-2"] {
        let engine = TurnEngine::new(test_config(temp.path()), store.clone());
        let spec = base_spec("subs/grok/grok-4.7", "grok-resume-session", turn_id);
        let (result, _events) = collect_events(engine, spec).await;
        assert_eq!(
            result.expect("turn").state,
            thickapi_core::TurnState::Success
        );
    }

    let log = std::fs::read_to_string(temp.path().join("argv.log")).expect("argv.log");
    let turns: Vec<&str> = log.lines().filter(|l| !l.trim().is_empty()).collect();
    assert_eq!(turns.len(), 2, "{log}");
    assert!(!turns[0].contains("--resume"), "first turn: {}", turns[0]);
    assert!(
        turns[1].contains(&format!("--resume {cli_session}")),
        "second turn: {}",
        turns[1]
    );

    unsafe {
        std::env::remove_var("THICKAPI_TEST_CHILD");
    }
}

fn write_stale_resume_then_ok_grok(dir: &std::path::Path, minted: &str) -> PathBuf {
    std::fs::write(
        dir.join("stream.jsonl"),
        format!(
            "{{\"type\":\"init\",\"session_id\":\"{minted}\"}}\r\n{{\"type\":\"assistant_message\",\"content\":\"ok-after-retry\"}}\r\n"
        ),
    )
    .expect("stream");
    let cmd = dir.join("grok-stale.cmd");
    std::fs::write(
        &cmd,
        "@echo off\r\necho %* >> \"%~dp0argv.log\"\r\necho %* | findstr /C:\"--resume\" >nul\r\nif not errorlevel 1 (\r\necho No conversation found with session ID: stale-id 1>&2\r\nexit /b 1\r\n)\r\ntype \"%~dp0stream.jsonl\"\r\n",
    )
    .expect("cmd");
    cmd
}

/// A stored `cli_session` the CLI no longer has must not fail every later turn: clear it and
/// retry once without `--resume`.
#[cfg(windows)]
#[tokio::test]
async fn a_stale_grok_cli_session_is_cleared_and_retried_without_resume() {
    let _child_guard = test_child_lock();
    let temp = tempfile::tempdir().expect("tempdir");
    let minted = "9c1a2b3d-4e5f-6789-abcd-ef0123456789";
    let cmd = write_stale_resume_then_ok_grok(temp.path(), minted);
    unsafe {
        std::env::set_var("THICKAPI_TEST_CHILD", cmd.to_string_lossy().to_string());
    }

    let store = Arc::new(Store::open_in_memory().expect("store"));
    let cwd = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    store
        .upsert_session_binding(&thickapi_core::adapters::SessionBinding {
            session_id: "stale-grok".into(),
            account_id: "grok-main".into(),
            backend: thickapi_core::BackendId::Grok,
            cli_session: Some("stale-id".into()),
            cwd: cwd.clone(),
        })
        .expect("bind");

    let engine = TurnEngine::new(test_config(temp.path()), store.clone());
    let spec = base_spec("subs/grok/grok-4.7", "stale-grok", "turn-stale-resume");
    let (result, _events) = collect_events(engine, spec).await;
    let result = result.expect("retry without --resume succeeds");
    assert_eq!(result.state, thickapi_core::TurnState::Success);
    assert!(result.text.contains("ok-after-retry"), "{}", result.text);

    let log = std::fs::read_to_string(temp.path().join("argv.log")).expect("argv.log");
    let turns: Vec<&str> = log.lines().filter(|l| !l.trim().is_empty()).collect();
    assert_eq!(turns.len(), 2, "{log}");
    assert!(
        turns[0].contains("--resume stale-id"),
        "first: {}",
        turns[0]
    );
    assert!(
        !turns[1].contains("--resume"),
        "retry must drop --resume: {}",
        turns[1]
    );
    let binding = store
        .get_session_binding("stale-grok")
        .expect("get")
        .expect("row");
    assert_eq!(binding.cli_session.as_deref(), Some(minted));

    unsafe {
        std::env::remove_var("THICKAPI_TEST_CHILD");
    }
}

/// Snapshot/enforce failures are a client condition (cwd, tree), not an internal 500.
#[cfg(windows)]
#[tokio::test]
async fn a_snapshot_that_cannot_run_is_invalid_spec_not_500() {
    let _child_guard = test_child_lock();
    let temp = tempfile::tempdir().expect("tempdir");
    let cmd = write_argv_logging_grok(temp.path(), "unused");
    unsafe {
        std::env::set_var("THICKAPI_TEST_CHILD", cmd.to_string_lossy().to_string());
    }

    let file_cwd = temp.path().join("not-a-directory");
    std::fs::write(&file_cwd, b"x").expect("file cwd");
    let engine = TurnEngine::new(
        test_config(temp.path()),
        Arc::new(Store::open_in_memory().expect("store")),
    );
    let mut spec = base_spec("subs/claude/fable", "snap-cwd", "turn-snap-cwd");
    spec.cwd = file_cwd;
    spec.enforce_policy = Some(enforce_everything());

    let (result, _events) = collect_events(engine, spec).await;
    let err = result.expect_err("snapshot of a file is a client error");
    assert_eq!(err.code(), "invalid_spec");
    assert_eq!(err.http_status(), 400);

    unsafe {
        std::env::remove_var("THICKAPI_TEST_CHILD");
    }
}

/// Past the 2 MiB tree cap the turn is refused before spawn (locked: refuse, do not start).
#[cfg(windows)]
#[tokio::test]
async fn a_tree_past_the_snapshot_cap_is_refused_before_spawn() {
    let _child_guard = test_child_lock();
    let temp = tempfile::tempdir().expect("tempdir");
    let cmd = write_argv_logging_grok(temp.path(), "unused");
    unsafe {
        std::env::set_var("THICKAPI_TEST_CHILD", cmd.to_string_lossy().to_string());
    }

    let cwd = temp.path().join("work");
    std::fs::create_dir_all(&cwd).expect("cwd");
    std::fs::write(
        cwd.join("big.bin"),
        vec![0u8; thickapi_core::enforce::TREE_SNAPSHOT_CAP + 1],
    )
    .expect("big");
    let engine = TurnEngine::new(
        test_config(temp.path()),
        Arc::new(Store::open_in_memory().expect("store")),
    );
    let mut spec = base_spec("subs/claude/fable", "snap-cap", "turn-snap-cap");
    spec.cwd = cwd;
    spec.enforce_policy = Some(enforce_everything());

    let (result, _events) = collect_events(engine, spec).await;
    let err = result.expect_err("cap is a client error");
    assert_eq!(err.code(), "invalid_spec");
    assert_eq!(err.http_status(), 400);
    assert!(
        !temp.path().join("argv.log").exists(),
        "child must not spawn after a refused snapshot"
    );

    unsafe {
        std::env::remove_var("THICKAPI_TEST_CHILD");
    }
}

/// Overlay restore used to drop at the end of the spawn retry loop, before enforce, so a
/// grok MCP merge looked like an unpermitted write of `{cwd}/.grok/config.toml`.
#[cfg(windows)]
#[tokio::test]
async fn grok_mcp_overlay_is_restored_after_enforce_not_before() {
    let _child_guard = test_child_lock();
    let temp = tempfile::tempdir().expect("tempdir");
    let cwd = temp.path().join("proj");
    std::fs::create_dir_all(cwd.join(".grok")).expect("grok dir");
    std::fs::write(cwd.join("keep.rs"), "fn main() {}\n").expect("rs");
    let user_toml = "keep_me = true\n";
    std::fs::write(cwd.join(".grok").join("config.toml"), user_toml).expect("user toml");
    let mcp = temp.path().join("mcp.json");
    std::fs::write(
        &mcp,
        r#"{"mcpServers":{"probe":{"command":"echo","args":["ok"]}}}"#,
    )
    .expect("mcp");
    let cmd = write_argv_logging_grok(temp.path(), "mcp-sess");
    unsafe {
        std::env::set_var("THICKAPI_TEST_CHILD", cmd.to_string_lossy().to_string());
    }

    let engine = TurnEngine::new(
        test_config(temp.path()),
        Arc::new(Store::open_in_memory().expect("store")),
    );
    let mut spec = base_spec("subs/grok/grok-4.7", "mcp-overlay", "turn-mcp-overlay");
    spec.cwd = cwd.clone();
    spec.mcp = thickapi_core::McpMode::On;
    spec.mcp_config = Some(mcp);
    spec.enforce_policy = Some(thickapi_core::EnforcePolicy {
        writes: Some(vec!["**/*.rs".into()]),
        protected_files: vec![],
        out_of_scope: None,
        always_writable: vec![],
        mode: thickapi_core::EnforceMode::TreeCopy,
    });

    let (result, _events) = collect_events(engine, spec).await;
    let result = result.expect("turn");
    assert_eq!(result.state, thickapi_core::TurnState::Success);
    let after = std::fs::read_to_string(cwd.join(".grok").join("config.toml")).expect("toml");
    assert_eq!(after, user_toml, "the user's grok config must be restored");

    unsafe {
        std::env::remove_var("THICKAPI_TEST_CHILD");
    }
}

/// A child that answers, hands its stdout to a grandchild that says nothing for twenty seconds,
/// and exits at once.
fn write_pipe_holding_child(dir: &std::path::Path) -> PathBuf {
    std::fs::write(
        dir.join("holder.cmd"),
        "@echo off\r\nping -n 20 127.0.0.1 > nul\r\n",
    )
    .expect("holder");
    let cmd = dir.join("pipe-holder.cmd");
    std::fs::write(
        &cmd,
        "@echo off\r\necho {\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"ANSWERED\"}]}}\r\nstart /b \"\" \"%~dp0holder.cmd\"\r\n",
    )
    .expect("cmd");
    cmd
}

/// The post-exit drain had no deadline, so a grandchild holding the pipe held the whole turn —
/// permit, cwd lock, `Running` registry entry — until it chose to die.
#[cfg(windows)]
#[tokio::test]
async fn a_grandchild_holding_stdout_does_not_hold_the_turn() {
    let _child_guard = test_child_lock();
    let temp = tempfile::tempdir().expect("tempdir");
    let cmd = write_pipe_holding_child(temp.path());
    unsafe {
        std::env::set_var("THICKAPI_TEST_CHILD", cmd.to_string_lossy().to_string());
    }

    let engine = TurnEngine::new(
        test_config(temp.path()),
        Arc::new(Store::open_in_memory().expect("store")),
    );
    let spec = base_spec(
        "subs/claude/fable",
        "engine-pipe-holder",
        "turn-pipe-holder",
    );
    let started = std::time::Instant::now();
    let (result, events) = collect_events(engine, spec).await;
    let took = started.elapsed();

    let result = result.expect("turn");
    assert!(result.text.contains("ANSWERED"), "{}", result.text);
    assert!(events
        .iter()
        .any(|e| matches!(e, TurnEvent::TurnEnd { .. })));
    assert!(
        took < std::time::Duration::from_secs(12),
        "the turn waited {took:?} on a pipe its child no longer owns"
    );

    unsafe {
        std::env::remove_var("THICKAPI_TEST_CHILD");
    }
}

/// First paid-cap site: the current member is already `or/` and the day is spent.
/// Must persist then return `Err(RateLimited)` — a 200 first answer was F4.
#[tokio::test]
async fn paid_cap_on_or_member_returns_err_and_persists() {
    let temp = tempfile::tempdir().expect("tempdir");
    let mut config = test_config(temp.path());
    config.router.max_paid_usd_per_day = 0.01;
    let store = Arc::new(Store::open_in_memory().expect("store"));
    store.add_paid_spend(0.01).expect("seed spend");
    let engine = TurnEngine::new(config, store);
    let mut spec = base_spec("or/anthropic/claude-opus-5", "or-cap", "or-cap-turn");
    spec.allow_paid = true;

    let (result, events) = collect_events(engine, spec).await;
    let err = result.expect_err("first-site paid cap is 429, not 200");
    assert!(
        matches!(err, thickapi_core::TurnError::RateLimited(ref msg) if msg.contains("daily paid cap exceeded")),
        "{err:?}"
    );
    let stored = result_json(temp.path(), "or-cap-turn");
    assert_eq!(stored["state"], "rate_limited");
    assert_eq!(stored["error_code"], "rate_limited");
    assert_eq!(stored["resolved_model"], "or/anthropic/claude-opus-5");
    assert!(events
        .iter()
        .any(|e| matches!(e, TurnEvent::TurnEnd { .. })));
}

/// F7: the panic hook must not ship in the library an integration test links
/// (`debug_assertions` used to inject it there). A bogus model is enough — we
/// only need to get past the hook.
#[tokio::test]
async fn panic_hook_is_not_compiled_into_the_integration_lib() {
    let temp = tempfile::tempdir().expect("tempdir");
    let engine = TurnEngine::new(
        test_config(temp.path()),
        Arc::new(Store::open_in_memory().expect("store")),
    );
    let mut spec = base_spec("subs/no-such/model", "no-hook", "no-hook-turn");
    spec.metadata
        .insert("thickapi_test_panic".into(), serde_json::Value::Bool(true));

    let (result, _events) = collect_events(engine, spec).await;
    let err = result.expect_err("unknown model, not a panic");
    assert!(
        matches!(err, thickapi_core::TurnError::UnknownModel(_)),
        "the hook must not fire outside cfg(test)/test-hooks; got {err:?}"
    );
}
