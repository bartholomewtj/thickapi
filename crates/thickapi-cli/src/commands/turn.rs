use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use thickapi_core::{
    bootstrap_config, load_config, ConfigBundle, Effort, Store, TurnEngine, TurnEvent, TurnSpec,
};

pub fn load_turn_config(config_dir: &Path) -> Result<ConfigBundle> {
    bootstrap_config(config_dir).context("bootstrap config")?;
    load_config(config_dir).context("load config")
}

pub async fn run(
    config_dir: &Path,
    cwd: PathBuf,
    model: Option<String>,
    combo: Option<String>,
    allow_paid: bool,
    stream: bool,
    prompt: Option<String>,
) -> Result<()> {
    let prompt = prompt.context("prompt argument is required")?;
    let config = load_turn_config(config_dir)?;
    let data_dir = thickapi_core::expand_path(&config.router.data_dir);
    std::fs::create_dir_all(&data_dir).ok();
    let db_path = data_dir.join("thickapi.db");
    let store = Store::open(&db_path).context("open database")?;
    let engine = TurnEngine::new(config, Arc::new(store));

    let spec = TurnSpec {
        model: model.unwrap_or_else(|| "subs/claude/fable".to_string()),
        prompt,
        system_prompt: None,
        system_prompt_file: None,
        cwd,
        session_id: uuid::Uuid::new_v4().to_string(),
        thinking: Effort::Medium,
        tools: vec![],
        deny_writes: vec![],
        enforce_policy: None,
        mcp: thickapi_core::McpMode::Auto,
        mcp_config: None,
        setting_sources: None,
        allow_api: false,
        combo,
        allow_paid,
        timeout_seconds: engine.config.router.default_timeout_seconds,
        client: Some("cli".into()),
        turn_id: None,
        metadata: serde_json::Map::new(),
    };

    let (tx, mut rx) = tokio::sync::mpsc::channel(64);
    let handle = tokio::spawn(async move {
        while let Some(event) = rx.recv().await {
            if stream {
                if let Ok(line) = serde_json::to_string(&event) {
                    println!("data: {}", line);
                }
            }
        }
    });
    let result = engine.run_turn(spec, tx).await;
    let _ = handle.await;

    match result {
        Ok(turn) => {
            if !stream {
                println!("{}", turn.text);
            }
            if turn.returncode != 0 {
                std::process::exit(turn.returncode);
            }
            Ok(())
        }
        Err(err) => {
            if stream {
                let event = TurnEvent::Error {
                    code: err.code().to_string(),
                    message: err.to_string(),
                };
                println!("data: {}", serde_json::to_string(&event)?);
            }
            bail!("{}", err);
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    #[test]
    fn turn_loads_the_passed_config_dir() {
        let temp = tempfile::tempdir().expect("tempdir");
        let config = load_turn_config(temp.path()).expect("load");
        assert_eq!(config.config_dir, temp.path());
        let keys_path = temp.path().join("keys.yaml");
        std::fs::write(&keys_path, "keys: []\n").unwrap();
        let again = load_turn_config(temp.path()).expect("reload");
        assert_eq!(again.config_dir, temp.path());
        assert!(again.keys.keys.is_empty());
    }
}
