use serde::{Deserialize, Serialize};

use crate::types::BackendId;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AccountsConfig {
    pub pools: std::collections::BTreeMap<String, PoolConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub struct PoolConfig {
    pub max_concurrent: u32,
    pub accounts: Vec<AccountConfig>,
    /// How long a non-`ok` account-health status keeps an account out of routing, in seconds,
    /// keyed by status (`rate_limited`, `suspected_blocked`, `subscription_blocked`,
    /// `spawn_failed`). Unset statuses take `default_health_ttl_seconds`; `0` means "until
    /// cleared by hand" (`thickapi doctor clear-health <account_id>`).
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub health_ttl_seconds: std::collections::BTreeMap<String, u64>,
}

/// Default cooldown per account-health status. When it has passed the next turn is let
/// through as the probe: success records `ok`, another failure restarts the clock (C12).
pub fn default_health_ttl_seconds(status: &str) -> u64 {
    match status {
        "rate_limited" => 15 * 60,
        "spawn_failed" => 60,
        _ => 6 * 60 * 60,
    }
}

impl AccountsConfig {
    /// Cooldown for `status` on `account_id`: the owning pool's override, else the default.
    pub fn health_ttl_seconds(&self, account_id: &str, status: &str) -> u64 {
        self.pools
            .values()
            .find(|pool| pool.accounts.iter().any(|a| a.id == account_id))
            .and_then(|pool| pool.health_ttl_seconds.get(status).copied())
            .unwrap_or_else(|| default_health_ttl_seconds(status))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub struct AccountConfig {
    pub id: String,
    pub backend: BackendId,
    pub auth: AuthKind,
    #[serde(default)]
    pub policy: Option<AccountPolicy>,
    pub env_key: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthKind {
    CliLogin,
    Env,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub struct AccountPolicy {
    pub tos_url: Option<String>,
    pub reviewed_on: Option<String>,
}
