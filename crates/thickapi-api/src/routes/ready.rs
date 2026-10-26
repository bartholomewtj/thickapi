use std::collections::BTreeMap;

use axum::{extract::State, Json};
use serde::Serialize;
use thickapi_core::account_block;
use thickapi_core::adapters::resolve_cli;
use thickapi_core::config::expand_path;

use crate::AppState;

const DISK_FREE_THRESHOLD_BYTES: u64 = 100 * 1024 * 1024;

#[derive(Serialize)]
pub struct ReadyResponse {
    pub ready: bool,
    pub message: String,
    pub database: DatabaseStatus,
    pub disk: DiskStatus,
    pub pool: BTreeMap<String, PoolEntry>,
    pub cli: BTreeMap<String, CliEntry>,
    pub accounts: BTreeMap<String, AccountEntry>,
    /// Thin-surface vendors (SPEC A19.11): token, CLI version, path in use, last rejection.
    pub thin: serde_json::Value,
}

#[derive(Serialize)]
pub struct DatabaseStatus {
    pub ok: bool,
    pub message: String,
}

#[derive(Serialize)]
pub struct DiskStatus {
    pub ok: bool,
    pub free_bytes: u64,
    pub threshold_bytes: u64,
}

#[derive(Serialize)]
pub struct PoolEntry {
    pub in_use: u32,
    pub max: u32,
    pub queued: u32,
}

#[derive(Serialize)]
pub struct CliEntry {
    pub resolved: bool,
    pub path: Option<String>,
}

#[derive(Serialize)]
pub struct AccountEntry {
    pub status: String,
    pub blocked: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retry_in_seconds: Option<u64>,
}

pub async fn ready(State(state): State<AppState>) -> Json<ReadyResponse> {
    let database = match state.store.ping() {
        Ok(()) => DatabaseStatus {
            ok: true,
            message: "ok".into(),
        },
        Err(err) => DatabaseStatus {
            ok: false,
            message: err.to_string(),
        },
    };

    let data_dir = expand_path(&state.config.router.data_dir);
    let free_bytes = thickapi_core::free_disk_bytes(&data_dir);
    let disk = DiskStatus {
        ok: free_bytes >= DISK_FREE_THRESHOLD_BYTES,
        free_bytes,
        threshold_bytes: DISK_FREE_THRESHOLD_BYTES,
    };

    let pool = state
        .pools
        .all_snapshots()
        .into_iter()
        .map(|(name, snap)| {
            (
                name,
                PoolEntry {
                    in_use: snap.in_use,
                    max: snap.max,
                    queued: snap.queued,
                },
            )
        })
        .collect();

    let mut cli = BTreeMap::new();
    let mut cli_ok = true;
    for bin in state.config.required_cli_bins() {
        let path = resolve_cli(bin);
        let resolved = path.is_some();
        if !resolved {
            cli_ok = false;
        }
        cli.insert(
            bin.to_string(),
            CliEntry {
                resolved,
                path: path.map(|p| p.to_string_lossy().to_string()),
            },
        );
    }

    let mut accounts = BTreeMap::new();
    let mut accounts_ok = true;
    // Account blocks in force, by thin vendor: the subscription is the same on both lanes,
    // so a quota block on the account stops the vendor for clients too (#110).
    let mut vendor_blocks: Vec<(&'static str, String, Option<u64>)> = Vec::new();
    if let Ok(rows) = state.store.list_account_health() {
        let now = chrono::Utc::now();
        for row in rows {
            // A block whose cooldown has lapsed no longer stops routing, so it no longer
            // fails readiness; the status stays visible until the probe turn rewrites it.
            let ttl = state
                .config
                .accounts
                .health_ttl_seconds(&row.account_id, &row.status);
            let block = account_block(&row, ttl, now);
            if block.is_blocking() {
                accounts_ok = false;
                if let Some(vendor) = thin_vendor_of(&state.config.accounts, &row.account_id) {
                    vendor_blocks.push((
                        vendor,
                        format!("account {}: {}", row.account_id, row.status),
                        block.retry_in_seconds(),
                    ));
                }
            }
            accounts.insert(
                row.account_id,
                AccountEntry {
                    status: row.status,
                    blocked: block.is_blocking(),
                    retry_in_seconds: block.retry_in_seconds(),
                },
            );
        }
    }

    let pool_config_ok = state
        .config
        .accounts
        .pools
        .values()
        .all(|pool| pool.max_concurrent > 0 && !pool.accounts.is_empty());

    let ready = database.ok
        && disk.ok
        && cli_ok
        && accounts_ok
        && pool_config_ok
        && !state.pools.is_draining();

    let message = if ready {
        "ok".to_string()
    } else if state.pools.is_draining() {
        "draining".to_string()
    } else if !database.ok {
        database.message.clone()
    } else if !disk.ok {
        "disk below threshold".to_string()
    } else if !cli_ok {
        "cli not resolved".to_string()
    } else if !accounts_ok {
        "account health blocking".to_string()
    } else {
        "pool config invalid".to_string()
    };

    Json(ReadyResponse {
        ready,
        message,
        database,
        disk,
        pool,
        cli,
        accounts,
        thin: serde_json::json!({ "vendors": fold_account_blocks(state.thin.vendor_snapshot(), &vendor_blocks) }),
    })
}

/// The thin vendor key (`/ready` → `thin.vendors.<key>`) an account's backend serves.
fn thin_vendor_of(
    accounts: &thickapi_core::config::AccountsConfig,
    account_id: &str,
) -> Option<&'static str> {
    let account = accounts
        .pools
        .values()
        .flat_map(|pool| pool.accounts.iter())
        .find(|a| a.id == account_id)?;
    thickapi_core::thin::model_check::vendor_of(account.backend)
}

/// Fold account-health blocks into each vendor's `available` summary (#110).
fn fold_account_blocks(
    mut vendors: serde_json::Value,
    blocks: &[(&'static str, String, Option<u64>)],
) -> serde_json::Value {
    use thickapi_core::thin::oauth::state::Availability;
    for (vendor, reason, retry) in blocks {
        let Some(view) = vendors.get_mut(*vendor) else {
            continue;
        };
        let mut availability = Availability::from_json(view);
        // An account block with no lapse (`ttl 0`) is "until cleared": no retry hint.
        availability.block(reason.clone(), *retry);
        availability.attach(view);
    }
    vendors
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod fold_tests {
    use serde_json::json;

    use super::fold_account_blocks;

    #[test]
    fn an_account_block_turns_its_vendor_off_and_keeps_the_longest_wait() {
        let vendors = json!({
            "grok": {"available": true, "retry_in_seconds": null, "reason": null, "rows_unavailable": []},
            "cursor": {"available": false, "retry_in_seconds": 30, "reason": "rate_limit_error (429)", "rows_unavailable": ["subs/cursor/x"]},
        });
        let out = fold_account_blocks(
            vendors,
            &[
                ("grok", "account grok-main: rate_limited".into(), Some(600)),
                ("cursor", "account cursor-main: quota".into(), Some(900)),
                ("agy", "account agy-main: x".into(), Some(5)),
            ],
        );
        assert_eq!(out["grok"]["available"], false);
        assert_eq!(out["grok"]["retry_in_seconds"], 600);
        assert_eq!(out["grok"]["reason"], "account grok-main: rate_limited");
        assert_eq!(out["cursor"]["retry_in_seconds"], 900);
        assert_eq!(out["cursor"]["rows_unavailable"], json!(["subs/cursor/x"]));
        assert!(out.get("agy").is_none(), "no vendor view, nothing invented");
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use std::sync::Arc;

    use http_body_util::BodyExt;
    use thickapi_core::config::keys::{hash_token, KeyEntry, KeysConfig};
    use tower::ServiceExt;

    use crate::middleware::auth::SCOPE_MODELS_READ;
    use crate::{build_router, AppState};

    fn test_state() -> (AppState, String) {
        test_state_with(
            serde_yaml::from_str(include_str!("../../../../configs/accounts.yaml.example"))
                .expect("accounts"),
            thickapi_core::config::ModelsConfig {
                aliases: Default::default(),
                models: vec![],
            },
        )
    }

    fn test_state_with(
        accounts: thickapi_core::config::AccountsConfig,
        models: thickapi_core::config::ModelsConfig,
    ) -> (AppState, String) {
        let token = "ta_ready_test";
        let hash = hash_token(token);
        let config = thickapi_core::ConfigBundle {
            config_dir: std::path::PathBuf::from("."),
            router: thickapi_core::config::RouterConfig {
                listen: "127.0.0.1:8787".into(),
                data_dir: std::env::temp_dir().join("thickapi-ready-test"),
                lan: false,
                default_timeout_seconds: 5400,
                queue_timeout_seconds: 300,
                max_queue_depth: 16,
                max_paid_usd_per_day: 5.0,
                drain_seconds: 120,
                refuse_paths: vec![],
                ..Default::default()
            },
            accounts,
            models,
            combos: thickapi_core::config::CombosConfig {
                combos: Default::default(),
            },
            keys: KeysConfig {
                keys: vec![KeyEntry {
                    id: "test".into(),
                    hash,
                    scopes: vec![SCOPE_MODELS_READ.into()],
                    ..Default::default()
                }],
            },
        };
        (
            crate::server::make_state_with_store(
                config,
                Arc::new(thickapi_core::Store::open_in_memory().expect("store")),
            ),
            token.to_string(),
        )
    }

    fn claude_only_state() -> (AppState, String) {
        test_state_with(
            serde_yaml::from_str(
                r#"
pools:
  claude:
    max_concurrent: 2
    accounts:
      - id: claude-main
        backend: claude_code
        auth: cli_login
"#,
            )
            .expect("claude accounts"),
            thickapi_core::config::ModelsConfig {
                aliases: Default::default(),
                models: vec![],
            },
        )
    }

    async fn ready_json(state: AppState, token: &str) -> serde_json::Value {
        let app = build_router(state);
        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/ready")
                    .header("Authorization", format!("Bearer {}", token))
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .expect("response");
        assert_eq!(response.status(), axum::http::StatusCode::OK);
        let body = response
            .into_body()
            .collect()
            .await
            .expect("body")
            .to_bytes();
        serde_json::from_slice(&body).expect("json")
    }

    #[tokio::test]
    async fn ready_db_probe() {
        let (state, token) = test_state();
        let json = ready_json(state, &token).await;
        assert_eq!(json["database"]["ok"], true);
        assert!(json["pool"]["claude"]["max"].as_u64().unwrap() > 0);
    }

    #[tokio::test]
    async fn ready_reports_real_free_bytes() {
        let (state, token) = test_state();
        let json = ready_json(state, &token).await;
        let free = json["disk"]["free_bytes"].as_u64().expect("free_bytes");
        assert!(free > 0);
        assert_ne!(
            free,
            super::DISK_FREE_THRESHOLD_BYTES + 1,
            "must not be the fabricated THRESHOLD+1 probe"
        );
    }

    #[tokio::test]
    async fn ready_cursor_only_lists_cursor_agent_not_agy() {
        let (state, token) = test_state_with(
            serde_yaml::from_str(
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
            .expect("accounts"),
            serde_yaml::from_str(
                r#"
aliases: {}
models:
  - id: subs/cursor/auto
    backend: cursor
    cli_model: auto
    enabled: true
"#,
            )
            .expect("models"),
        );
        let json = ready_json(state, &token).await;
        let cli = json["cli"].as_object().expect("cli map");
        assert!(
            cli.contains_key("cursor-agent"),
            "Cursor rows must list cursor-agent: {cli:?}"
        );
        assert!(
            !cli.contains_key("agy"),
            "Cursor-only config must not demand agy: {cli:?}"
        );
        assert!(!cli.contains_key("claude"));
        assert!(!cli.contains_key("grok"));
        if json["message"] == "cli not resolved" {
            assert_eq!(cli["cursor-agent"]["resolved"], false);
        }
    }

    #[tokio::test]
    async fn ready_fails_on_blocking_account_health() {
        let (state, token) = claude_only_state();
        state
            .store
            .upsert_account_health("claude-main", "rate_limited")
            .expect("health");
        let app = build_router(state);
        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/ready")
                    .header("Authorization", format!("Bearer {}", token))
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .expect("response");
        let body = response
            .into_body()
            .collect()
            .await
            .expect("body")
            .to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&body).expect("json");
        assert_eq!(json["ready"], false);
        assert_eq!(json["accounts"]["claude-main"]["status"], "rate_limited");
        assert_eq!(json["accounts"]["claude-main"]["blocked"], true);
        assert!(json["accounts"]["claude-main"]["retry_in_seconds"]
            .as_u64()
            .is_some());
    }

    #[tokio::test]
    async fn ready_a_lapsed_block_is_visible_but_not_blocking() {
        let (state, token) = claude_only_state();
        let age = (chrono::Utc::now() - chrono::Duration::hours(1)).to_rfc3339();
        state
            .store
            .upsert_account_health_at("claude-main", "rate_limited", &age)
            .expect("health");
        let app = build_router(state);
        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/ready")
                    .header("Authorization", format!("Bearer {}", token))
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .expect("response");
        let body = response
            .into_body()
            .collect()
            .await
            .expect("body")
            .to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&body).expect("json");
        // Not asserting `json["ready"] == true`: overall readiness also folds in
        // `cli_ok` (real `resolve_cli` against PATH), which is false on a CI
        // runner with no vendor CLIs installed. This test is only about the
        // lapsed block staying visible without blocking, so it checks the
        // account-block fields directly.
        assert_eq!(json["accounts"]["claude-main"]["status"], "rate_limited");
        assert_eq!(json["accounts"]["claude-main"]["blocked"], false);
        assert!(json["accounts"]["claude-main"]["retry_in_seconds"].is_null());
    }
}
