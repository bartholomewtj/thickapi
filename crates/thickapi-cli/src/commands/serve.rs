use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use thickapi_api::{make_state, serve};
use thickapi_core::{bootstrap_config, expand_path, load_config_with_legacy, InstanceLock, Store};
use tracing_subscriber::fmt;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

pub async fn run(config_dir: &Path, dump_tools: Option<std::path::PathBuf>) -> Result<()> {
    bootstrap_config(config_dir).context("bootstrap config files")?;
    // `log_dir` comes from the config, so the subscriber starts after the load; its legacy
    // `router.yaml` keys are logged once it is up.
    let (config, legacy) = load_config_with_legacy(config_dir).context("load config")?;
    let data_dir = expand_path(&config.router.data_dir);
    let log_dir = expand_path(&config.router.thin.log_dir);
    std::fs::create_dir_all(&log_dir)
        .with_context(|| format!("create log dir {}", log_dir.display()))?;

    let appender = tracing_appender::rolling::RollingFileAppender::builder()
        .rotation(tracing_appender::rolling::Rotation::DAILY)
        .filename_prefix("thickapi")
        .filename_suffix("log")
        .build(&log_dir)
        .with_context(|| format!("open rolling log in {}", log_dir.display()))?;
    let (non_blocking, _guard) = tracing_appender::non_blocking(appender);
    tracing_subscriber::registry()
        .with(crate::log_filter::env_filter())
        .with(fmt::layer().with_writer(std::io::stderr))
        // The file is read by scripts and grep; colours split `key=value`.
        .with(fmt::layer().with_ansi(false).with_writer(non_blocking))
        .init();
    tracing::info!(log_dir = %log_dir.display(), "thickapi serve starting");
    for key in &legacy {
        key.warn();
    }

    let _lock = InstanceLock::acquire(&data_dir).map_err(|err| anyhow::anyhow!(err))?;

    let db_path = data_dir.join("thickapi.db");
    let store = Store::open(&db_path).context("open database")?;
    let state = make_state(config, store);
    if let Some(path) = dump_tools {
        *state.dump_tools.lock().unwrap_or_else(|e| e.into_inner()) = Some(path);
    }

    let reap_config = config_dir.to_path_buf();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(24 * 60 * 60));
        loop {
            interval.tick().await;
            if let Err(err) = crate::commands::reap::run_from_config(&reap_config, 7, false) {
                tracing::warn!(error = %err, "daily reap failed");
            }
        }
    });

    serve(state).await?;
    Ok(())
}
