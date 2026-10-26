//! `thickapi models list|check` (SPEC §10.3, §11): the catalogue as served, and whether every
//! `subs/*` row still names the newest version of its family on the vendor's own list.

use std::path::Path;

use anyhow::Result;
use thickapi_core::config::models::Transport;
use thickapi_core::thin::model_check;
use thickapi_core::thin::runtime::ThinRuntime;

/// Print every catalogue row: id, backend, wire id, transport, enabled, aliases.
pub fn list(config_dir: &Path) -> Result<i32> {
    let config = thickapi_core::load_config(config_dir)?;
    let models = &config.models;
    println!("models.yaml: {}", config_dir.join("models.yaml").display());
    for m in &models.models {
        let aliases: Vec<&str> = models
            .aliases
            .iter()
            .filter(|(_, target)| **target == m.id)
            .map(|(alias, _)| alias.as_str())
            .collect();
        println!(
            "  {:<28} {:<12} {:<34} {:<13} {}{}",
            m.id,
            serde_json::to_value(m.backend)
                .ok()
                .and_then(|v| v.as_str().map(str::to_string))
                .unwrap_or_default(),
            m.wire_model(),
            match m.transport {
                Transport::CliTurn => "cli_turn",
                Transport::OauthDirect => "oauth_direct",
                Transport::CliBridge => "cli_bridge",
            },
            if m.enabled { "enabled" } else { "disabled" },
            if aliases.is_empty() {
                String::new()
            } else {
                format!("  aliases: {}", aliases.join(", "))
            }
        );
    }
    Ok(0)
}

/// Fetch each vendor's live list and compare. Exit 1 when any row is stale or missing; an
/// unreachable vendor is a warning (its rows stay unchecked).
pub async fn check(config_dir: &Path) -> Result<i32> {
    let config = thickapi_core::load_config(config_dir)?;
    let runtime = ThinRuntime::new(&config);
    println!("thickapi models check");
    println!("  policy: one row per family, the vendor's newest version (SPEC 10.3)");
    let report = model_check::run(&runtime, &config.models).await;
    for line in report.lines() {
        println!("{line}");
    }
    if report.current() {
        println!("  catalogue current");
        Ok(0)
    } else {
        println!(
            "  {} row(s) behind or gone: bump cli_model / vendor_model in models.yaml, keep the id, restart serve, run sync-pi",
            report.stale_or_missing()
        );
        Ok(1)
    }
}
