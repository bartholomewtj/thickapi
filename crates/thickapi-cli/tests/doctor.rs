#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
use thickapi_cli::commands::doctor;

#[tokio::test]
async fn doctor_exits_nonzero_when_clis_missing() {
    let temp = tempfile::tempdir().expect("tempdir");
    thickapi_core::bootstrap_config(temp.path()).expect("bootstrap");

    let empty_path = temp.path().join("empty-bin");
    std::fs::create_dir_all(&empty_path).expect("mkdir");

    let saved_path = std::env::var("PATH").ok();
    unsafe {
        std::env::set_var("PATH", empty_path.to_string_lossy().to_string());
    }

    let code = doctor::run(temp.path(), true).await.expect("doctor run");

    if let Some(path) = saved_path {
        unsafe {
            std::env::set_var("PATH", path);
        }
    }

    assert_eq!(
        code, 1,
        "doctor should fail when required CLIs from the config rows are not on PATH"
    );
}

#[test]
fn doctor_offline_skips_models_live_check() {
    let temp = tempfile::tempdir().expect("tempdir");
    thickapi_core::bootstrap_config(temp.path()).expect("bootstrap");
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_thickapi"))
        .args([
            "doctor",
            "--offline",
            "--config",
            temp.path().to_str().expect("utf8"),
        ])
        .output()
        .expect("run doctor");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("[skip] models live check (--offline)"),
        "doctor --offline must skip models live check, got:\n{stdout}"
    );
}

#[test]
fn doctor_offline_cursor_only_does_not_require_agy() {
    let temp = tempfile::tempdir().expect("tempdir");
    thickapi_core::bootstrap_config(temp.path()).expect("bootstrap");
    std::fs::write(
        temp.path().join("accounts.yaml"),
        r#"
pools:
  cursor:
    max_concurrent: 1
    accounts:
      - id: cursor-main
        backend: cursor
        auth: cli_login
"#,
    )
    .unwrap();
    std::fs::write(
        temp.path().join("models.yaml"),
        r#"
aliases: {}
models:
  - id: subs/cursor/auto
    backend: cursor
    cli_model: auto
    enabled: true
"#,
    )
    .unwrap();
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_thickapi"))
        .args([
            "doctor",
            "--offline",
            "--config",
            temp.path().to_str().expect("utf8"),
        ])
        .output()
        .expect("run doctor");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let bins = cli_resolution_bins(&stdout);
    assert_eq!(
        bins,
        std::collections::BTreeSet::from(["cursor-agent".to_string()]),
        "Cursor-only doctor must name exactly cursor-agent, not the hard-coded four; got {bins:?}\n{stdout}"
    );
}

fn cli_resolution_bins(stdout: &str) -> std::collections::BTreeSet<String> {
    let mut bins = std::collections::BTreeSet::new();
    let mut in_block = false;
    for line in stdout.lines() {
        if line == "cli_resolution:" {
            in_block = true;
            continue;
        }
        if !in_block {
            continue;
        }
        if !line.starts_with("  ") {
            break;
        }
        let Some(after_status) = line.trim_start().split_once("] ").map(|(_, rest)| rest) else {
            continue;
        };
        if let Some(bin) = after_status.split_whitespace().next() {
            bins.insert(bin.to_string());
        }
    }
    bins
}

/// The example router points `data_dir` at `~/.thickapi`, which is the live daemon's
/// database. Parallel doctor tests, and the serve, then share one file and sqlite
/// returns `database is locked`. Point this config at its own temp directory.
fn use_temp_data_dir(config_dir: &std::path::Path) {
    let data_dir = config_dir.join("data");
    std::fs::create_dir_all(&data_dir).expect("data dir");
    let router_path = config_dir.join("router.yaml");
    let router = std::fs::read_to_string(&router_path).expect("read router");
    let data_yaml = data_dir.display().to_string().replace('\\', "/");
    let rewritten = router.replacen(
        "data_dir: \"~/.thickapi\"",
        &format!("data_dir: \"{data_yaml}\""),
        1,
    );
    assert_ne!(
        rewritten, router,
        "router example no longer has the default data_dir line"
    );
    std::fs::write(&router_path, rewritten).expect("write router");
}

#[test]
fn clear_health_resets_blocking_status() {
    let temp = tempfile::tempdir().expect("tempdir");
    thickapi_core::bootstrap_config(temp.path()).expect("bootstrap");
    use_temp_data_dir(temp.path());
    let config = thickapi_core::load_config(temp.path()).expect("config");
    let data_dir = thickapi_core::expand_path(&config.router.data_dir);
    assert!(
        data_dir.starts_with(temp.path()),
        "clear-health must use the temp data dir, got {}",
        data_dir.display()
    );
    std::fs::create_dir_all(&data_dir).expect("data dir");
    let store = thickapi_core::Store::open(&data_dir.join("thickapi.db")).expect("store");
    store
        .upsert_account_health("or-main", "suspected_blocked")
        .expect("seed");
    let code = doctor::clear_health(temp.path(), "or-main").expect("clear");
    assert_eq!(code, 0);
    let status = store
        .get_account_health("or-main")
        .expect("get")
        .expect("row");
    assert_eq!(status, "ok");
}

#[test]
fn clear_session_drops_cli_session() {
    let temp = tempfile::tempdir().expect("tempdir");
    thickapi_core::bootstrap_config(temp.path()).expect("bootstrap");
    use_temp_data_dir(temp.path());
    let config = thickapi_core::load_config(temp.path()).expect("config");
    let data_dir = thickapi_core::expand_path(&config.router.data_dir);
    assert!(
        data_dir.starts_with(temp.path()),
        "clear-session must use the temp data dir, got {}",
        data_dir.display()
    );
    std::fs::create_dir_all(&data_dir).expect("data dir");
    let store = thickapi_core::Store::open(&data_dir.join("thickapi.db")).expect("store");
    store
        .upsert_session_binding(&thickapi_core::adapters::SessionBinding {
            session_id: "sess-clear".into(),
            account_id: "grok-main".into(),
            backend: thickapi_core::BackendId::Grok,
            cli_session: Some("cli-stale".into()),
            cwd: std::path::PathBuf::from("."),
        })
        .expect("bind");
    let code = doctor::clear_session(temp.path(), "sess-clear").expect("clear");
    assert_eq!(code, 0);
    let binding = store
        .get_session_binding("sess-clear")
        .expect("get")
        .expect("row");
    assert!(binding.cli_session.is_none());
    let missing = doctor::clear_session(temp.path(), "no-such").expect("missing");
    assert_eq!(missing, 1);
}
