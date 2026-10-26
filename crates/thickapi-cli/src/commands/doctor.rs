use std::path::Path;

use anyhow::Result;
use thickapi_core::adapters::resolve_cli;
use thickapi_core::config::router::ClaudeOauthMode;
use thickapi_core::{
    account_block, account_health_is_blocking, bootstrap_config, expand_path, load_config,
    AccountBlock, Store,
};

const CONFIG_FILES: &[&str] = &[
    "router.yaml",
    "accounts.yaml",
    "models.yaml",
    "combos.yaml",
    "keys.yaml",
];

pub async fn run(config_dir: &Path, offline: bool) -> Result<i32> {
    println!("thickapi doctor");
    println!("config_dir: {}", config_dir.display());
    println!("  suspected_blocked: headerless HTTP 429 cooldown ({}h); clear with: thickapi doctor clear-health <account_id>", thickapi_core::constants::SUSPECTED_BLOCKED_COOLDOWN_HOURS);

    let mut failed = false;

    let _ = bootstrap_config(config_dir);
    for name in CONFIG_FILES {
        let path = config_dir.join(name);
        if !path.exists() {
            println!("  [fail] {} missing: {}", name, path.display());
            failed = true;
            continue;
        }
        match std::fs::read_to_string(&path) {
            Ok(text) => match serde_yaml::from_str::<serde_yaml::Value>(&text) {
                Ok(_) => println!("  [ok] {} parseable", name),
                Err(err) => {
                    println!("  [fail] {} invalid YAML: {}", name, err);
                    failed = true;
                }
            },
            Err(err) => {
                println!("  [fail] {} unreadable: {}", name, err);
                failed = true;
            }
        }
    }

    let config = match load_config(config_dir) {
        Ok(config) => {
            println!("  [ok] config bundle loads");
            config
        }
        Err(err) => {
            println!("  [fail] config bundle: {}", err);
            return Ok(1);
        }
    };

    let data_dir = expand_path(&config.router.data_dir);
    println!("data_dir: {}", data_dir.display());
    let db_path = data_dir.join("thickapi.db");
    match Store::open(&db_path) {
        Ok(store) => match store.ping() {
            Ok(()) => println!("  [ok] database open and pingable"),
            Err(err) => {
                println!("  [fail] database ping: {}", err);
                failed = true;
            }
        },
        Err(err) => {
            println!("  [fail] database: {}", err);
            failed = true;
        }
    }

    println!("listen: {}", config.router.listen);
    println!("  [ok] listen address configured");

    let disk_probe = data_dir.join(".thickapi-doctor-disk");
    match std::fs::write(&disk_probe, b"x") {
        Ok(()) => {
            let _ = std::fs::remove_file(&disk_probe);
            println!("  [ok] data_dir writable");
        }
        Err(err) => println!("  [warn] data_dir not writable: {}", err),
    }

    println!("cli_resolution:");
    for bin in config.required_cli_bins() {
        match resolve_cli(bin) {
            Some(path) => println!("  [ok] {} -> {}", bin, path.display()),
            None => {
                println!("  [fail] {} not found on PATH", bin);
                failed = true;
            }
        }
    }

    println!("pool_config:");
    for (name, pool) in &config.accounts.pools {
        if pool.max_concurrent == 0 || pool.accounts.is_empty() {
            println!(
                "  [fail] pool {} invalid (max_concurrent={}, accounts={})",
                name,
                pool.max_concurrent,
                pool.accounts.len()
            );
            failed = true;
        } else {
            println!(
                "  [ok] pool {} max_concurrent={} accounts={}",
                name,
                pool.max_concurrent,
                pool.accounts.len()
            );
        }
    }

    println!("account_health:");
    if let Ok(store) = Store::open(&db_path) {
        match store.list_account_health() {
            Ok(rows) if rows.is_empty() => println!("  [ok] no account health rows"),
            Ok(rows) => {
                let now = chrono::Utc::now();
                for row in rows {
                    let ttl = config
                        .accounts
                        .health_ttl_seconds(&row.account_id, &row.status);
                    let block = account_block(&row, ttl, now);
                    if block.is_blocking() {
                        let wait = match block {
                            AccountBlock::For(seconds) => format!("retry_in={seconds}s"),
                            _ => "retry_in=never (clear by hand)".to_string(),
                        };
                        println!(
                            "  [fail] {} status={} updated_at={} {}",
                            row.account_id, row.status, row.updated_at, wait
                        );
                        failed = true;
                    } else if account_health_is_blocking(&row.status) {
                        println!(
                            "  [ok] {} status={} updated_at={} (cooldown over; next turn probes)",
                            row.account_id, row.status, row.updated_at
                        );
                    } else {
                        println!("  [ok] {} status={}", row.account_id, row.status);
                    }
                }
            }
            Err(err) => {
                println!("  [fail] list account health: {}", err);
                failed = true;
            }
        }
    }

    if thin_section(config_dir, &config, offline).await {
        failed = true;
    }

    if offline {
        println!("models:");
        println!("  [skip] models live check (--offline)");
    } else if models_section(&config).await {
        failed = true;
    }

    Ok(if failed { 1 } else { 0 })
}

/// Catalogue currency (SPEC §10.3): every `subs/*` row must be the newest of its family on
/// the vendor's own list. Stale or gone rows fail; an unreachable vendor only warns. Returns
/// `true` on failure.
async fn models_section(config: &thickapi_core::ConfigBundle) -> bool {
    use thickapi_core::thin::model_check;
    use thickapi_core::thin::runtime::ThinRuntime;

    println!("models:");
    let runtime = ThinRuntime::new(config);
    let report = model_check::run(&runtime, &config.models).await;
    for line in report.lines() {
        println!("{line}");
    }
    if report.current() {
        println!("  [ok] catalogue current (one row per family, newest version)");
        false
    } else {
        println!(
            "  [fail] {} row(s) behind or gone; fix models.yaml (keep the id, bump cli_model / vendor_model), restart serve, run sync-pi",
            report.stale_or_missing()
        );
        true
    }
}

/// Thin API checks (SPEC §19.11): private-file ACLs, the Claude path in use, and a real
/// `claude.exe` handshake against `mcp-proposal` with the production argv. Returns `true` on failure.
async fn thin_section(
    config_dir: &Path,
    config: &thickapi_core::ConfigBundle,
    offline: bool,
) -> bool {
    use thickapi_core::thin::runtime::ThinRuntime;

    let mut failed = false;
    println!("thin_api:");
    println!(
        "  log_dir: {} (keep {} day(s))",
        thickapi_core::expand_path(&config.router.thin.log_dir).display(),
        config.router.thin.log_keep_days
    );
    let v = &config.router.thin.vendors;
    println!(
        "  budgets (thin.vendors.<id>.budgets, s): claude {}/{}, grok {}/{} tool_call_floor {}, \
         agy {}/{}, cursor {}/{}/{} list_models {}; start_deadline {}",
        v.claude.budgets.first_event_seconds,
        v.claude.budgets.idle_seconds,
        v.grok.budgets.first_event_seconds,
        v.grok.budgets.idle_seconds,
        v.grok.budgets.tool_call_floor_seconds,
        v.agy.budgets.first_event_seconds,
        v.agy.budgets.idle_seconds,
        v.cursor.budgets.first_output_seconds,
        v.cursor.budgets.idle_seconds,
        v.cursor.budgets.idle_after_text_with_tools_seconds,
        v.cursor.budgets.list_models_seconds,
        config.router.thin.start_deadline_seconds,
    );

    // Secret-bearing files must not be readable through inherited ACEs (SPEC §10.5 / §19.9).
    report_acl(&config_dir.join("keys.yaml"), "keys.yaml", &mut failed);
    let pi_agent = pi_agent_dir();
    report_acl(&pi_agent.join("models.json"), "Pi models.json", &mut failed);
    report_acl(
        &pi_agent.join("thickapi-subs.json"),
        "Pi sidecar",
        &mut failed,
    );
    report_acl(
        &config_dir.join("thin").join("claude_oauth_state.json"),
        "claude oauth state",
        &mut failed,
    );

    let runtime = ThinRuntime::new(config);
    runtime.wait_cli_versions();
    let swept = runtime.sweep();
    println!(
        "  [ok] tmp/ swept {} dead private dir(s) at {}",
        swept,
        runtime.tmp_root().display()
    );
    if runtime.tmp_root().is_dir() {
        if let Ok(entries) = std::fs::read_dir(runtime.tmp_root()) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    report_acl(
                        &path,
                        &format!(
                            "tmp/{}",
                            path.file_name().unwrap_or_default().to_string_lossy()
                        ),
                        &mut failed,
                    );
                }
            }
        }
    }

    // The same per-vendor summary `/ready` serves (#110); this process has no traffic, so
    // it reflects credentials, the persisted Claude block and bridge state only.
    let snapshot = runtime.vendor_snapshot();
    for vendor in ["claude", "grok", "agy", "cursor", "codex"] {
        let a = thickapi_core::thin::oauth::state::Availability::from_json(
            snapshot.get(vendor).unwrap_or(&serde_json::Value::Null),
        );
        println!(
            "  {vendor} available={} retry_in_seconds={} reason={} rows_unavailable={:?}",
            a.available,
            a.retry_in_seconds
                .map(|s| s.to_string())
                .unwrap_or_else(|| "-".into()),
            a.reason.as_deref().unwrap_or("-"),
            a.rows_unavailable
        );
    }

    let claude = runtime.claude();
    let (path, reason) = claude.planned_path();
    let (haiku_path, haiku_reason) = claude.planned_path_for("haiku");
    println!(
        "  claude oauth_mode={} path={} ({})",
        match claude.mode {
            thickapi_core::config::router::ClaudeOauthMode::Prefer => "prefer",
            thickapi_core::config::router::ClaudeOauthMode::Off => "off",
            thickapi_core::config::router::ClaudeOauthMode::Only => "only",
        },
        path,
        reason
    );
    println!("  claude haiku_path={} ({})", haiku_path, haiku_reason);
    if path == "oauth" || haiku_path == "oauth" {
        println!("  [warn] OAuth-direct is best-effort and a Consumer ToS risk (SPEC §19.14); the bridge is the fallback");
    }
    if let Some(file) = claude.state.persist_path() {
        println!(
            "  claude state file: {} ({})",
            file.display(),
            if file.exists() { "present" } else { "absent" }
        );
    }
    match claude.state.blocked() {
        Some(b) => println!(
            "  claude persisted: blocked since {} next_probe={} ({})",
            b.since, b.next_probe, b.reason
        ),
        None => println!("  claude persisted: not blocked"),
    }
    let (last_at, last_result, next_at) = claude.state.probe_view();
    println!(
        "  claude last probe: {} at {} next {}",
        last_result.as_deref().unwrap_or("none"),
        last_at.map(|t| t.to_string()).unwrap_or_else(|| "-".into()),
        next_at.map(|t| t.to_string()).unwrap_or_else(|| "-".into())
    );
    if claude.probe_active() {
        let probe = claude.probe_config();
        println!(
            "  claude probe: model={} on_start={} retry_minutes={} (serve owns the block -> ok transition)",
            probe.model, probe.on_start, probe.retry_minutes
        );
    } else {
        let note = match config.router.claude_oauth.mode {
            ClaudeOauthMode::Off => "mode off: Claude stays on the bridge",
            ClaudeOauthMode::Only => "mode only: a blocked direct path fails, with no probe",
            ClaudeOauthMode::Prefer => {
                "probe disabled: the next request after the cooldown probes the direct path"
            }
        };
        println!("  claude probe: off ({note})");
    }

    println!(
        "  claude cross_fallback={} (bridge fault before Started under prefer -> direct path once)",
        config.router.claude_oauth.cross_fallback
    );

    let bridge = runtime.bridge();
    match bridge.claude_version() {
        Some(version) => {
            println!("  [ok] claude {version}");
            report_fixture_set("claude", &version);
            report_fixture_set("claude-bridge", &version);
        }
        None => {
            println!("  [fail] claude --version did not answer");
            failed = true;
        }
    }

    let probe_model = config.router.claude_oauth.probe_model.clone();
    if offline {
        println!("  [skip] live vendor pings (--offline)");
    } else {
        match bridge.handshake(&probe_model).await {
            Ok(hs) => println!(
                "  [ok] bridge handshake on {} -> {} ({:?})",
                probe_model,
                hs.resolved_model,
                hs.text.trim()
            ),
            Err(err) => {
                println!(
                    "  [fail] bridge handshake: {} (hint: {})",
                    err.message,
                    err.hint().unwrap_or("-")
                );
                failed = true;
            }
        }
        match bridge.state().drift() {
            Some(d) => println!(
                "  [warn] bridge_state=drift: {} (a successful bridge run clears it)",
                d.reason
            ),
            None => println!("  bridge_state=ok"),
        }
    }

    // Grok (T2): OAuth-direct only; a missing or expired token is a warning, not a failure —
    // the row simply answers 401 vendor_login_required until `grok` is run again.
    let grok = runtime.grok();
    let grok_cli = grok
        .cli
        .cli_version()
        .unwrap_or_else(|| "not found".to_string());
    match grok.cli.token() {
        Ok(t) if t.expired() => println!(
            "  [warn] grok {}: token expired ({}); run any `grok` command to refresh",
            grok_cli,
            t.note.as_deref().unwrap_or("-")
        ),
        Ok(t) => println!(
            "  [ok] grok {} path=oauth token ok ({}s left, {})",
            grok_cli,
            t.seconds_left(),
            t.note.as_deref().unwrap_or("-")
        ),
        Err(e) => println!(
            "  [warn] grok {}: no token ({}); `grok login` enables subs/grok rows",
            grok_cli, e
        ),
    }
    if grok_cli != "not found" {
        report_fixture_set("grok", &grok_cli);
    }

    // Antigravity (T3): token lives in the Windows Credential Manager (`gemini:antigravity`);
    // `agy models` refreshes it. Missing/expired is a warning for the same reason as grok.
    let agy = runtime.agy();
    let agy_cli = agy
        .cli
        .cli_version()
        .unwrap_or_else(|| "not found".to_string());
    match agy.cli.token() {
        Ok(t) if t.expired() => println!(
            "  [warn] agy {}: token expired ({}); run `agy models` to refresh",
            agy_cli,
            t.note.as_deref().unwrap_or("-")
        ),
        Ok(t) => println!(
            "  [ok] agy {} path=oauth token ok ({}s left, {}, from {})",
            agy_cli,
            t.seconds_left(),
            t.note.as_deref().unwrap_or("-"),
            t.source.display()
        ),
        Err(e) => println!(
            "  [warn] agy {}: no token ({}); sign in with `agy` to enable subs/agy rows",
            agy_cli, e
        ),
    }
    if agy_cli != "not found" {
        report_fixture_set("agy", &agy_cli);
    }

    // Cursor (T4): session JWT in `%APPDATA%\Cursor\auth.json` (~60 days); the agent service
    // pins a client-version floor, so the advertised version is shown next to the CLI's.
    let cursor = runtime.cursor();
    let cursor_cli = cursor
        .cli
        .cli_version()
        .unwrap_or_else(|| "not found".to_string());
    match cursor.cli.token() {
        Ok(t) if t.expired() => println!(
            "  [warn] cursor-agent {}: session expired ({}); run `cursor-agent login`",
            cursor_cli,
            t.note.as_deref().unwrap_or("-")
        ),
        Ok(t) => println!(
            "  [ok] cursor-agent {} path=oauth as {} session ok ({}s left, {}, from {})",
            cursor_cli,
            cursor.cli.client_version(),
            t.seconds_left(),
            t.note.as_deref().unwrap_or("-"),
            t.source.display()
        ),
        Err(e) => println!(
            "  [warn] cursor-agent {}: no session ({}); `cursor-agent login` enables subs/cursor rows",
            cursor_cli, e
        ),
    }
    if cursor_cli != "not found" {
        report_fixture_set("cursor", &cursor_cli);
    }

    // Codex: the codex CLI's ChatGPT login (`~/.codex/auth.json`, ten-day JWT); `codex login
    // status` is asked to refresh it inside the last day.
    let codex = runtime.codex();
    let codex_cli = codex
        .cli
        .cli_version()
        .unwrap_or_else(|| "not found".to_string());
    match codex.cli.token() {
        Ok(t) if t.expired() => println!(
            "  [warn] codex {}: token expired ({}); run `codex login`",
            codex_cli,
            t.note.as_deref().unwrap_or("-")
        ),
        Ok(t) => println!(
            "  [ok] codex {} path=oauth token ok ({}s left, {}, from {})",
            codex_cli,
            t.seconds_left(),
            t.note.as_deref().unwrap_or("-"),
            t.source.display()
        ),
        Err(e) => println!(
            "  [warn] codex {}: no token ({}); `codex login` (ChatGPT) enables subs/codex rows",
            codex_cli, e
        ),
    }
    if codex_cli != "not found" {
        report_fixture_set("codex", &codex_cli);
    }

    if !offline {
        if grok.cli.token().is_ok_and(|t| !t.expired())
            && live_oauth_ping(
                &runtime,
                config,
                thickapi_core::types::BackendId::Grok,
                "grok",
            )
            .await
            .is_err()
        {
            failed = true;
        }
        if agy.cli.token().is_ok_and(|t| !t.expired())
            && live_oauth_ping(
                &runtime,
                config,
                thickapi_core::types::BackendId::Agy,
                "agy",
            )
            .await
            .is_err()
        {
            failed = true;
        }
        if cursor.cli.token().is_ok_and(|t| !t.expired())
            && live_oauth_ping(
                &runtime,
                config,
                thickapi_core::types::BackendId::Cursor,
                "cursor",
            )
            .await
            .is_err()
        {
            failed = true;
        }
        if codex.cli.token().is_ok_and(|t| !t.expired())
            && live_oauth_ping(
                &runtime,
                config,
                thickapi_core::types::BackendId::Codex,
                "codex",
            )
            .await
            .is_err()
        {
            failed = true;
        }
    }
    failed
}

fn report_fixture_set(vendor: &str, cli_version: &str) {
    use thickapi_core::thin::fixture_set::{compiled_fixtures_root, missing_fixture_warn};
    match missing_fixture_warn(&compiled_fixtures_root(), vendor, cli_version) {
        None => println!("  [ok] {vendor} {cli_version} fixture set present"),
        Some(warn) => println!("  [warn] {warn}"),
    }
}

/// One-token live request, same shape as the Claude bridge handshake (SPEC §19.11).
async fn live_oauth_ping(
    runtime: &thickapi_core::thin::runtime::ThinRuntime,
    config: &thickapi_core::ConfigBundle,
    backend: thickapi_core::types::BackendId,
    name: &str,
) -> Result<(), ()> {
    use serde_json::json;
    use thickapi_core::thin::openai::ChatRequest;
    use thickapi_core::thin::{ThinEvent, ThinRequest};
    use thickapi_core::thinking::Effort;

    let Some(entry) = config
        .models
        .models
        .iter()
        .find(|m| m.backend == backend && m.enabled && m.thin_capable())
        .cloned()
    else {
        println!("  [warn] {name} live: no enabled thin catalogue row");
        return Ok(());
    };
    let Some(adapter) = runtime.adapter_for(&entry) else {
        println!("  [warn] {name} live: no adapter");
        return Ok(());
    };
    let request: ChatRequest = match serde_json::from_value(json!({
        "model": entry.id,
        "messages": [{"role": "user", "content": "Reply with the single word OK."}],
        "max_tokens": 8
    })) {
        Ok(r) => r,
        Err(e) => {
            println!("  [fail] {name} live: build request: {e}");
            return Err(());
        }
    };
    let req = ThinRequest {
        request,
        entry,
        effort: Some(Effort::Off),
        request_id: format!("doctor-{name}"),
        path: "oauth",
        fallback: None,
        cursor_cwd: None,
    };
    let (tx, mut rx) = tokio::sync::mpsc::channel(64);
    let run = adapter.run(&req, tx);
    let collect = async {
        let mut text = String::new();
        let mut err = None;
        while let Some(ev) = rx.recv().await {
            match ev {
                ThinEvent::TextDelta(t) => text.push_str(&t),
                ThinEvent::Error(e) => err = Some(e),
                _ => {}
            }
        }
        (text, err)
    };
    match tokio::time::timeout(std::time::Duration::from_secs(45), async {
        let (run_res, (text, err)) = tokio::join!(run, collect);
        if let Err(e) = run_res {
            return Err(e.message);
        }
        if let Some(e) = err {
            return Err(e.message);
        }
        Ok(text)
    })
    .await
    {
        Ok(Ok(text)) => {
            println!("  [ok] {name} live 1-token -> {:?}", text.trim());
            Ok(())
        }
        Ok(Err(message)) => {
            println!("  [fail] {name} live: {message}");
            Err(())
        }
        Err(_) => {
            println!("  [fail] {name} live: timed out (45s)");
            Err(())
        }
    }
}

pub fn clear_health(config_dir: &Path, account_id: &str) -> Result<i32> {
    let config = load_config(config_dir).map_err(|e| anyhow::anyhow!("load config: {e}"))?;
    let known = config
        .accounts
        .pools
        .values()
        .any(|pool| pool.accounts.iter().any(|account| account.id == account_id));
    if !known {
        eprintln!("unknown account: {}", account_id);
        return Ok(1);
    }

    let data_dir = expand_path(&config.router.data_dir);
    let db_path = data_dir.join("thickapi.db");
    let store = Store::open(&db_path).map_err(|e| anyhow::anyhow!("open database: {e}"))?;
    store
        .clear_account_health(account_id)
        .map_err(|e| anyhow::anyhow!("clear account health: {e}"))?;
    println!("cleared account health for {}", account_id);
    Ok(0)
}

pub fn clear_session(config_dir: &Path, session_id: &str) -> Result<i32> {
    let config = load_config(config_dir).map_err(|e| anyhow::anyhow!("load config: {e}"))?;
    let data_dir = expand_path(&config.router.data_dir);
    let db_path = data_dir.join("thickapi.db");
    let store = Store::open(&db_path).map_err(|e| anyhow::anyhow!("open database: {e}"))?;
    if store
        .get_session_binding(session_id)
        .map_err(|e| anyhow::anyhow!("read session: {e}"))?
        .is_none()
    {
        eprintln!("unknown session: {}", session_id);
        return Ok(1);
    }
    store
        .clear_cli_session(session_id)
        .map_err(|e| anyhow::anyhow!("clear session: {e}"))?;
    println!("cleared cli_session for {}", session_id);
    Ok(0)
}

fn pi_agent_dir() -> std::path::PathBuf {
    std::env::var_os("THICKAPI_PI_AGENT")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            dirs::home_dir()
                .unwrap_or_else(|| std::path::PathBuf::from("."))
                .join(".pi")
                .join("agent")
        })
}

fn report_acl(path: &Path, label: &str, failed: &mut bool) {
    use thickapi_core::thin::private_dir::has_inherited_access;
    if !path.exists() {
        return;
    }
    match has_inherited_access(path) {
        Some(true) => {
            println!(
                "  [fail] {label} has inherited ACEs; fix with: icacls \"{}\" /inheritance:r /grant:r *SID:F",
                path.display()
            );
            *failed = true;
        }
        Some(false) => println!("  [ok] {label} ACL is private"),
        None => println!("  [warn] {label} ACL not checked (icacls unavailable)"),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    #[test]
    fn inherited_models_json_fails_acl_check() {
        use thickapi_core::thin::private_dir::has_inherited_access;
        let temp = tempfile::tempdir().expect("tempdir");
        let models = temp.path().join("models.json");
        std::fs::write(&models, "{}").expect("write");
        if has_inherited_access(&models) != Some(true) {
            eprintln!("skipped: runner has no ACL inheritance");
            return;
        }
        let mut failed = false;
        report_acl(&models, "Pi models.json", &mut failed);
        assert!(failed, "inherited models.json must fail doctor");
    }

    #[test]
    fn missing_fixture_set_uses_spec_wording() {
        use thickapi_core::thin::fixture_set::{compiled_fixtures_root, missing_fixture_warn};
        let root = compiled_fixtures_root();
        assert_eq!(
            missing_fixture_warn(&root, "agy", "1.2.5").as_deref(),
            Some("agy 1.2.5: no fixture set (run thickapi capture agy)")
        );
        assert_eq!(
            missing_fixture_warn(&root, "agy", "1.2.4"),
            None,
            "committed agy 1.2.4 set must be found"
        );
    }
}
