pub mod accounts;
pub mod combos;
pub mod keys;
pub mod legacy;
pub mod models;
pub mod router;

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

pub use accounts::AccountsConfig;
pub use combos::CombosConfig;
pub use keys::KeysConfig;
pub use legacy::LegacyKey;
pub use models::ModelsConfig;
pub use router::RouterConfig;

use crate::types::BackendId;

#[derive(Debug, Clone)]
pub struct ConfigBundle {
    pub config_dir: PathBuf,
    pub router: RouterConfig,
    pub accounts: AccountsConfig,
    pub models: ModelsConfig,
    pub combos: CombosConfig,
    pub keys: KeysConfig,
}

/// CLI binary `/ready` and `doctor` require for this backend. OpenRouter (`Pi`) has none.
pub fn cli_bin_for_backend(backend: BackendId) -> Option<&'static str> {
    match backend {
        BackendId::ClaudeCode => Some("claude"),
        BackendId::Grok => Some("grok"),
        BackendId::Agy => Some("agy"),
        BackendId::Cursor => Some("cursor-agent"),
        BackendId::Codex => Some("codex"),
        BackendId::Pi => None,
    }
}

/// Unique CLI names implied by enabled model rows and account pools.
pub fn required_cli_bins(
    models: &ModelsConfig,
    accounts: &AccountsConfig,
) -> BTreeSet<&'static str> {
    let mut bins = BTreeSet::new();
    for entry in &models.models {
        if entry.enabled {
            if let Some(bin) = cli_bin_for_backend(entry.backend) {
                bins.insert(bin);
            }
        }
    }
    for pool in accounts.pools.values() {
        for account in &pool.accounts {
            if let Some(bin) = cli_bin_for_backend(account.backend) {
                bins.insert(bin);
            }
        }
    }
    bins
}

impl ConfigBundle {
    pub fn required_cli_bins(&self) -> BTreeSet<&'static str> {
        required_cli_bins(&self.models, &self.accounts)
    }
}

pub fn default_config_dir() -> PathBuf {
    dirs::home_dir()
        .map(|h| h.join(".thickapi"))
        .unwrap_or_else(|| PathBuf::from(".thickapi"))
}

pub fn expand_path(path: &Path) -> PathBuf {
    let s = path.to_string_lossy();
    if s.starts_with("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(s.trim_start_matches("~/"));
        }
    }
    if s == "~" {
        if let Some(home) = dirs::home_dir() {
            return home;
        }
    }
    path.to_path_buf()
}

/// Load the five config files. A legacy `router.yaml` key is logged at `warn`, once per key
/// per load ([`load_config_with_legacy`] for a caller whose subscriber is not up yet).
pub fn load_config(config_dir: &Path) -> Result<ConfigBundle> {
    let (bundle, legacy) = load_config_with_legacy(config_dir)?;
    for key in &legacy {
        key.warn();
    }
    Ok(bundle)
}

/// [`load_config`] without the warnings: the legacy `router.yaml` keys found are returned for
/// the caller to [`LegacyKey::warn`] about (`serve`, which reads `log_dir` before its tracing
/// subscriber exists).
pub fn load_config_with_legacy(config_dir: &Path) -> Result<(ConfigBundle, Vec<LegacyKey>)> {
    let router_path = config_dir.join("router.yaml");
    let text = std::fs::read_to_string(&router_path).with_context(|| {
        format!(
            "missing or unreadable config file {}",
            router_path.display()
        )
    })?;
    let (router, legacy) = RouterConfig::from_yaml(&text)
        .with_context(|| format!("invalid YAML in config file {}", router_path.display()))?;
    let accounts = load_yaml::<AccountsConfig>(&config_dir.join("accounts.yaml"))?;
    let models = load_yaml::<ModelsConfig>(&config_dir.join("models.yaml"))?;
    let combos = load_yaml::<CombosConfig>(&config_dir.join("combos.yaml"))?;
    let keys = keys::load_keys(&config_dir.join("keys.yaml"))?;

    router
        .validate_lan()
        .map_err(anyhow::Error::msg)
        .with_context(|| format!("invalid {}", config_dir.join("router.yaml").display()))?;
    router
        .thin
        .validate()
        .map_err(anyhow::Error::msg)
        .with_context(|| format!("invalid {}", config_dir.join("router.yaml").display()))?;
    let router = RouterConfig {
        data_dir: expand_path(&router.data_dir),
        thin: crate::config::router::ThinConfig {
            log_dir: expand_path(&router.thin.log_dir),
            ..router.thin
        },
        ..router
    };

    let bundle = ConfigBundle {
        config_dir: config_dir.to_path_buf(),
        router,
        accounts,
        models,
        combos,
        keys,
    };
    Ok((bundle, legacy))
}

fn load_yaml<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("missing or unreadable config file {}", path.display()))?;
    serde_yaml::from_str(&text)
        .with_context(|| format!("invalid YAML in config file {}", path.display()))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    fn examples_dir() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../configs")
    }

    #[test]
    fn parse_router_example() {
        let path = examples_dir().join("router.yaml.example");
        let text = std::fs::read_to_string(path).expect("read example");
        let cfg: RouterConfig = serde_yaml::from_str(&text).expect("parse");
        assert_eq!(cfg.listen, "127.0.0.1:8787");
    }

    #[test]
    fn parse_accounts_example() {
        let path = examples_dir().join("accounts.yaml.example");
        let text = std::fs::read_to_string(path).expect("read example");
        let cfg: AccountsConfig = serde_yaml::from_str(&text).expect("parse");
        assert!(cfg.pools.contains_key("claude"));
    }

    #[test]
    fn parse_models_example() {
        let path = examples_dir().join("models.yaml.example");
        let text = std::fs::read_to_string(path).expect("read example");
        let cfg: ModelsConfig = serde_yaml::from_str(&text).expect("parse");
        assert!(!cfg.models.is_empty());
        assert!(
            cfg.find("subs/claude/fable").is_some(),
            "example catalogue includes the Claude Fable row"
        );
        assert!(
            cfg.find("subs/claude/haiku").is_none(),
            "haiku is not catalogued (A4, SPEC 10.3): nobody pins it"
        );
    }

    #[test]
    fn required_cli_bins_follow_enabled_rows_and_pools() {
        let cursor_only_models: ModelsConfig = serde_yaml::from_str(
            r#"
aliases: {}
models:
  - id: subs/cursor/auto
    backend: cursor
    cli_model: auto
"#,
        )
        .expect("models");
        let cursor_only_accounts: AccountsConfig = serde_yaml::from_str(
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
        .expect("accounts");
        let bins = required_cli_bins(&cursor_only_models, &cursor_only_accounts);
        assert_eq!(
            bins.into_iter().collect::<Vec<_>>(),
            vec!["cursor-agent"],
            "Cursor-only config must not demand agy/claude/grok"
        );

        let disabled: ModelsConfig = serde_yaml::from_str(
            r#"
aliases: {}
models:
  - id: subs/agy/flash
    backend: agy
    cli_model: gemini
    enabled: false
"#,
        )
        .expect("disabled");
        let empty_accounts: AccountsConfig = serde_yaml::from_str("pools: {}").expect("empty");
        assert!(required_cli_bins(&disabled, &empty_accounts).is_empty());
    }

    #[test]
    fn parse_combos_example() {
        let path = examples_dir().join("combos.yaml.example");
        let text = std::fs::read_to_string(path).expect("read example");
        let cfg: CombosConfig = serde_yaml::from_str(&text).expect("parse");
        assert!(cfg.combos.contains_key("claude-then-or"));
    }

    /// Audit D8: the other three yaml files refuse an unknown key by name too.
    #[test]
    fn accounts_models_and_combos_refuse_an_unknown_key_by_name() {
        let accounts = "pools:\n  claude:\n    max_concurrent: 1\n    accounts:\n      - id: a\n        backend: claude_code\n        auth: cli_login\n        polcy: {}\n";
        let err = serde_yaml::from_str::<AccountsConfig>(accounts).unwrap_err();
        assert!(err.to_string().contains("`polcy`"), "{err}");
        let pool = "pools:\n  claude:\n    max_concurent: 1\n    accounts: []\n";
        let err = serde_yaml::from_str::<AccountsConfig>(pool).unwrap_err();
        assert!(err.to_string().contains("`max_concurent`"), "{err}");

        let models = "models:\n  - id: subs/grok/x\n    backend: grok\n    cli_model: x\n    transprot: oauth_direct\n";
        let err = serde_yaml::from_str::<ModelsConfig>(models).unwrap_err();
        assert!(err.to_string().contains("`transprot`"), "{err}");
        let err = serde_yaml::from_str::<ModelsConfig>("alias: {}\nmodels: []\n").unwrap_err();
        assert!(err.to_string().contains("`alias`"), "{err}");

        let err = serde_yaml::from_str::<CombosConfig>("combo: {}\n").unwrap_err();
        assert!(err.to_string().contains("`combo`"), "{err}");
    }

    /// The bootstrapped examples plus a `router.yaml` in the live file's shape: the bundle
    /// loads, the legacy keys come back for the caller to warn about, and an unknown key in any
    /// file fails the load naming it.
    #[test]
    fn load_config_takes_legacy_keys_and_refuses_unknown_ones() {
        let dir = tempfile::tempdir().unwrap();
        crate::bootstrap_config(dir.path()).unwrap();
        let (_, none) = load_config_with_legacy(dir.path()).unwrap();
        assert!(
            none.is_empty(),
            "the shipped examples are current: {none:?}"
        );

        std::fs::write(
            dir.path().join("router.yaml"),
            "listen: \"127.0.0.1:18787\"\ndata_dir: \"~/.thickapi\"\nmeasured_versions:\n  claude: \"2.1.0\"\nthin:\n  cursor_cwd: \"C:/work\"\n",
        )
        .unwrap();
        let (bundle, legacy) = load_config_with_legacy(dir.path()).unwrap();
        let keys: Vec<&str> = legacy.iter().map(|k| k.key.as_str()).collect();
        assert_eq!(keys, vec!["measured_versions", "thin.cursor_cwd"]);
        assert_eq!(
            bundle.router.thin.vendors.cursor.cwd.as_deref(),
            Some("C:/work")
        );
        assert!(load_config(dir.path()).is_ok());

        std::fs::write(
            dir.path().join("combos.yaml"),
            "combos: {}\nfallbacks: {}\n",
        )
        .unwrap();
        let err = format!("{:#}", load_config(dir.path()).unwrap_err());
        assert!(
            err.contains("combos.yaml") && err.contains("`fallbacks`"),
            "{err}"
        );
    }
}
