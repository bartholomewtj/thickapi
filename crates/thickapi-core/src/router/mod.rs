mod session;

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};

use uuid::Uuid;

use crate::adapters::{Account, SessionBinding};
use crate::config::accounts::AccountConfig;
use crate::config::models::ModelEntry;
use crate::config::{AccountsConfig, ConfigBundle, ModelsConfig};
use crate::constants::THICKAPI_CLAUDE_NS;
use crate::error::TurnError;
use crate::store::{account_block, AccountBlock, Store};
use crate::types::{BackendId, TurnSpec};

/// Router failures are the client's to act on — an unknown model is a 400, a blocked account
/// a 429 — so they travel as `TurnError`, not as an `anyhow` string the engine can only call
/// `internal_error` / 500 (C17). Only a store failure is genuinely internal.
type Result<T> = std::result::Result<T, TurnError>;

fn store_error(err: anyhow::Error) -> TurnError {
    TurnError::Other(format!("store: {err}"))
}

pub use session::RouterSession;

pub struct ResolvedTurn<'a> {
    pub account: Account,
    pub model: &'a ModelEntry,
    pub session: SessionBinding,
    pub backend: BackendId,
}

fn round_robin_counters() -> &'static Mutex<HashMap<String, AtomicUsize>> {
    static COUNTERS: OnceLock<Mutex<HashMap<String, AtomicUsize>>> = OnceLock::new();
    COUNTERS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn account_from_config(cfg: &AccountConfig) -> Account {
    Account {
        id: cfg.id.clone(),
        backend: cfg.backend,
        auth: cfg.auth,
        env_key: cfg.env_key.clone(),
    }
}

pub fn resolve_model<'a>(model_path: &str, models: &'a ModelsConfig) -> Result<&'a ModelEntry> {
    models
        .find(model_path)
        .ok_or_else(|| TurnError::UnknownModel(format!("unknown model id: {model_path}")))
}

pub fn resolve_combo(spec: &TurnSpec, config: &ConfigBundle) -> Result<Vec<String>> {
    let members = if let Some(combo_name) = &spec.combo {
        config
            .combos
            .combos
            .get(combo_name)
            .cloned()
            .ok_or_else(|| TurnError::InvalidSpec(format!("unknown combo: {}", combo_name)))?
    } else if let Some(named) = config.combos.combos.get(&spec.model) {
        named.clone()
    } else {
        vec![spec.model.clone()]
    };

    for member in &members {
        resolve_model(member, &config.models).map_err(|_| {
            TurnError::UnknownModel(format!("combo member not in catalogue: {}", member))
        })?;
    }
    Ok(members)
}

fn ensure_account_available(
    store: &Store,
    accounts: &AccountsConfig,
    account_id: &str,
) -> Result<()> {
    let Some(row) = store
        .get_account_health_row(account_id)
        .map_err(store_error)?
    else {
        return Ok(());
    };
    let ttl = accounts.health_ttl_seconds(account_id, &row.status);
    let wait = match account_block(&row, ttl, chrono::Utc::now()) {
        AccountBlock::Clear => return Ok(()),
        AccountBlock::For(seconds) => format!("retry in {seconds}s"),
        AccountBlock::UntilCleared => "until cleared by hand".to_string(),
    };
    let message = format!(
        "account {} is blocked ({}; {})",
        account_id, row.status, wait
    );
    // SPEC §3.1: a vendor policy block is 402, not the 429 of a rate window.
    Err(if row.status == "subscription_blocked" {
        TurnError::SubscriptionBlocked(message)
    } else {
        TurnError::RateLimited(message)
    })
}

pub fn pick_account(
    backend: BackendId,
    accounts: &AccountsConfig,
    store: &Store,
    session_id: &str,
) -> Result<Account> {
    let pool_name = match backend {
        BackendId::ClaudeCode => "claude",
        BackendId::Grok => "grok",
        BackendId::Agy => "agy",
        BackendId::Cursor => "cursor",
        BackendId::Codex => "codex",
        BackendId::Pi => "openrouter",
    };
    let pool = accounts
        .pools
        .get(pool_name)
        .ok_or_else(|| TurnError::UnknownModel(format!("missing pool {}", pool_name)))?;
    if pool.accounts.is_empty() {
        return Err(TurnError::UnknownModel(format!("empty pool {}", pool_name)));
    }

    if let Some(binding) = store.get_session_binding(session_id).map_err(store_error)? {
        if binding.backend == backend {
            if let Some(cfg) = pool.accounts.iter().find(|a| a.id == binding.account_id) {
                if cfg.backend == backend {
                    match ensure_account_available(store, accounts, &cfg.id) {
                        Ok(()) => return Ok(account_from_config(cfg)),
                        Err(_) => {
                            // Sticky, but the bound account is blocked: step over it and
                            // pick another member of the pool.
                        }
                    }
                }
            }
        }
    }

    // A pool with more than one account rotates, mixed-auth included, and a blocked member
    // is stepped over so one rate-limited key does not fail every n-th turn while its
    // neighbours are fine. A single-account pool has one live account: the first.
    let rotates = pool.accounts.len() > 1;
    let (start, candidates) = if rotates {
        let mut counters = round_robin_counters()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let counter = counters
            .entry(pool_name.to_string())
            .or_insert_with(|| AtomicUsize::new(0));
        let start = counter.fetch_add(1, Ordering::SeqCst) % pool.accounts.len();
        (start, pool.accounts.len())
    } else {
        (0, 1)
    };

    let mut blocked = None;
    for cfg in pool.accounts.iter().cycle().skip(start).take(candidates) {
        if cfg.backend != backend {
            return Err(TurnError::UnknownModel("account backend mismatch".into()));
        }
        match ensure_account_available(store, accounts, &cfg.id) {
            Ok(()) => return Ok(account_from_config(cfg)),
            Err(err) => blocked = blocked.or(Some(err)),
        }
    }
    Err(blocked.unwrap_or_else(|| TurnError::UnknownModel(format!("empty pool {}", pool_name))))
}

pub fn claude_cli_session(session_id: &str) -> String {
    Uuid::new_v5(&THICKAPI_CLAUDE_NS, session_id.as_bytes()).to_string()
}

pub fn resolve<'a>(
    spec: &TurnSpec,
    config: &'a ConfigBundle,
    store: &Store,
) -> Result<ResolvedTurn<'a>> {
    if spec.model.starts_with("or/") && !spec.allow_paid {
        return Err(TurnError::InvalidSpec(
            "openrouter models require allow_paid".into(),
        ));
    }

    let model = resolve_model(&spec.model, &config.models)?;
    let backend = model.backend;
    let account = pick_account(backend, &config.accounts, store, &spec.session_id)?;

    let existing = store
        .get_session_binding(&spec.session_id)
        .map_err(store_error)?;
    let cwd = dunce::canonicalize(&spec.cwd).map_err(|e| TurnError::InvalidSpec(e.to_string()))?;

    // Only resume when a prior turn stored cli_session; first turn uses --session-id.
    let cli_session = existing
        .as_ref()
        .filter(|b| b.backend == backend)
        .and_then(|b| b.cli_session.clone());

    let session = SessionBinding {
        session_id: spec.session_id.clone(),
        account_id: account.id.clone(),
        backend,
        cli_session,
        cwd,
    };

    Ok(ResolvedTurn {
        account,
        model,
        session,
        backend,
    })
}

pub fn update_session_after_turn(
    store: &Store,
    session: &SessionBinding,
    cli_session: Option<String>,
) -> Result<()> {
    let binding = SessionBinding {
        session_id: session.session_id.clone(),
        account_id: session.account_id.clone(),
        backend: session.backend,
        cli_session,
        cwd: session.cwd.clone(),
    };
    store
        .upsert_session_binding(&binding)
        .map_err(store_error)?;
    Ok(())
}
