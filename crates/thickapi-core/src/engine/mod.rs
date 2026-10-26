mod artifacts;
pub mod job;
pub mod jsonl;
pub mod registry;
mod spawn;
mod validate;

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::Result;
use tokio::sync::mpsc;
use uuid::Uuid;

use crate::adapters::{for_backend, inject_openrouter_env, TurnFailureClass};
use crate::config::ConfigBundle;
use crate::enforce::Enforcer;
use crate::error::TurnError;
use crate::pool::{pool_name_for_backend, PoolManager};
use crate::router::{resolve, resolve_combo, update_session_after_turn, ResolvedTurn};
use crate::store::Store;
use crate::trust::seed_backend_trust;
use crate::types::{
    BackendId, PermissionBreach, TurnEvent, TurnId, TurnResult, TurnSpec, TurnState, UsageBreakdown,
};

pub use artifacts::{
    path_component, read_events_jsonl, remember_turn_prompt, turn_artifact_dir,
    turn_prompt_conflicts,
};
pub use registry::{CancelToken, TurnRegistry, TurnStatus};
pub use validate::validate_spec;

/// Default idle stall when a child writes nothing at all (seconds).
pub const DEFAULT_IDLE_STALL_SECONDS: u64 = 120;

/// Agy prints nothing between its init line and the final result, and asks for a 240-minute
/// `--print-timeout` of its own; two minutes of silence is normal, not a stall (B12). For agy
/// `timeout_seconds` is the bound that actually bites. `adapters::agy` reads this constant so
/// the flag it passes and the guard the engine applies cannot drift apart.
pub const AGY_IDLE_STALL_SECONDS: u64 = 240 * 60;

/// Idle stall for a backend before any `router.yaml idle_stall_seconds` override.
pub fn default_idle_stall_seconds(backend: BackendId) -> u64 {
    match backend {
        BackendId::Agy => AGY_IDLE_STALL_SECONDS,
        _ => DEFAULT_IDLE_STALL_SECONDS,
    }
}

type CwdLock = Arc<tokio::sync::Mutex<()>>;

fn cwd_locks() -> &'static Mutex<HashMap<PathBuf, CwdLock>> {
    static LOCKS: OnceLock<Mutex<HashMap<PathBuf, CwdLock>>> = OnceLock::new();
    LOCKS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// The per-cwd turn lock. A `tokio` mutex, so a second turn in the same cwd waits on the
/// scheduler instead of parking a worker thread for the length of a child run, and a panic
/// under the guard cannot poison it for every later turn in that directory (B7). The registry
/// itself is a plain mutex held only for the map lookup, and its poison is stepped over for
/// the same reason.
fn acquire_cwd_lock(cwd: &Path) -> CwdLock {
    let key = dunce::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
    let mut map = cwd_locks()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    map.entry(key)
        .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
        .clone()
}

/// Step over poison: the only thing under this lock is one held-back `TurnEnd`.
fn lock_terminal(slot: &Mutex<Option<TurnEvent>>) -> std::sync::MutexGuard<'_, Option<TurnEvent>> {
    slot.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// The state a client sees when an error, rather than the child, ended the turn (SPEC §3.1).
fn terminal_state_for(err: &TurnError) -> TurnState {
    match err {
        TurnError::TurnTimeout(_) | TurnError::QueueTimeout(_) => TurnState::Timeout,
        TurnError::RateLimited(_) => TurnState::RateLimited,
        TurnError::SubscriptionBlocked(_) => TurnState::SubscriptionBlocked,
        _ => TurnState::Failed,
    }
}

fn effective_turn_state(state: TurnState, breaches: &[PermissionBreach]) -> TurnState {
    if breaches.is_empty() {
        state
    } else {
        TurnState::Failed
    }
}

/// `build_turn_result`'s arguments, grouped so the function itself takes one (`clippy::
/// too_many_arguments` at 12 positional args).
struct TurnResultArgs<'a> {
    turn_id: &'a str,
    session_id: &'a str,
    state: TurnState,
    returncode: i32,
    text: String,
    usage: &'a UsageBreakdown,
    cost: f64,
    context_window: u64,
    backend: BackendId,
    account_id: &'a str,
    resolved_model: &'a str,
    breaches: &'a [PermissionBreach],
}

fn build_turn_result(args: TurnResultArgs<'_>) -> TurnResult {
    let state = effective_turn_state(args.state, args.breaches);
    TurnResult {
        turn_id: args.turn_id.to_string(),
        session_id: args.session_id.to_string(),
        state,
        returncode: args.returncode,
        text: args.text,
        tokens: args.usage.input + args.usage.output,
        cost: args.cost,
        context_tokens: args.usage.input,
        usage: args.usage.clone(),
        context_window: args.context_window,
        backend: args.backend,
        account_id: args.account_id.to_string(),
        resolved_model: args.resolved_model.to_string(),
        error_code: None,
    }
}

fn with_error_code(mut result: TurnResult, code: &str) -> TurnResult {
    result.error_code = Some(code.to_string());
    result
}

fn persist_turn_result(
    registry: Option<&Arc<TurnRegistry>>,
    data_dir: &Path,
    turn_id: &str,
    result: &TurnResult,
) {
    let turn_id = turn_id.to_string();
    if let Ok(dir) = artifacts::ensure_turn_dir(data_dir, &turn_id) {
        let _ = artifacts::write_result_json(&dir, result);
    }
    if let Some(registry) = registry {
        registry.finish(&turn_id, result.clone());
    }
}

/// Ends the turn if `run_turn` does not get to. A panic anywhere under it — or the future being
/// dropped mid-run — used to leave the registry entry `Running` for the life of the daemon:
/// shutdown drain waited out its whole window, the turn id answered `409 turn_in_progress` for
/// ever, and the SSE stream just stopped. On drop while armed this records a `Failed` result
/// and emits the `TurnEnd` nothing else will.
struct FinishOnDrop<'a> {
    registry: Option<&'a Arc<TurnRegistry>>,
    data_dir: &'a Path,
    turn_id: &'a str,
    session_id: &'a str,
    event_tx: &'a mpsc::Sender<TurnEvent>,
    armed: bool,
}

impl FinishOnDrop<'_> {
    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for FinishOnDrop<'_> {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let turn_id = self.turn_id.to_string();
        if self.registry.is_some_and(|r| r.is_terminal(&turn_id)) {
            return;
        }
        let text = if std::thread::panicking() {
            "internal_error: the turn panicked before it finished"
        } else {
            "internal_error: the turn was dropped before it finished"
        }
        .to_string();
        let result = with_error_code(
            build_turn_result(TurnResultArgs {
                turn_id: self.turn_id,
                session_id: self.session_id,
                state: TurnState::Failed,
                returncode: -1,
                text: text.clone(),
                usage: &UsageBreakdown::default(),
                cost: 0.0,
                context_window: 0,
                backend: BackendId::ClaudeCode,
                account_id: "",
                resolved_model: "",
                breaches: &[],
            }),
            "internal_error",
        );
        persist_turn_result(self.registry, self.data_dir, self.turn_id, &result);
        let event = TurnEvent::TurnEnd {
            state: TurnState::Failed,
            returncode: -1,
            text,
            retry_after_seconds: None,
            touched_paths: vec![],
            breaches: vec![],
        };
        if let Some(registry) = self.registry {
            registry.emit(self.data_dir, &turn_id, event.clone());
        }
        let _ = self.event_tx.try_send(event);
    }
}

pub struct TurnEngine {
    pub config: ConfigBundle,
    pub store: Arc<Store>,
    pub registry: Option<Arc<TurnRegistry>>,
    pub pools: Option<Arc<PoolManager>>,
}

impl TurnEngine {
    pub fn new(config: ConfigBundle, store: Arc<Store>) -> Self {
        Self {
            config,
            store,
            registry: None,
            pools: None,
        }
    }

    pub fn with_registry(mut self, registry: Arc<TurnRegistry>) -> Self {
        self.registry = Some(registry);
        self
    }

    pub fn with_pools(mut self, pools: Arc<PoolManager>) -> Self {
        self.pools = Some(pools);
        self
    }

    pub async fn run_turn(
        &self,
        spec: TurnSpec,
        event_tx: mpsc::Sender<TurnEvent>,
    ) -> Result<TurnResult, TurnError> {
        let turn_id = spec
            .turn_id
            .clone()
            .unwrap_or_else(|| Uuid::new_v4().to_string());
        let session_id = spec.session_id.clone();

        if artifacts::turn_prompt_conflicts(&self.config.router.data_dir, &turn_id, &spec.prompt)
            == Some(true)
        {
            return Err(TurnError::TurnConflict(format!(
                "turn_id {turn_id} already used with a different prompt"
            )));
        }
        let _ =
            artifacts::remember_turn_prompt(&self.config.router.data_dir, &turn_id, &spec.prompt);

        // `try_send` sheds events once a slow SSE consumer has filled the 64-slot channel. For
        // deltas that is tolerable — the registry and `events.jsonl` keep the whole log — but
        // losing `TurnEnd` leaves the client with a stream that simply stops. So the terminal
        // event is held back here and awaited once the turn is over, when nothing is left to
        // shed it behind (B9).
        let mut finish_guard = FinishOnDrop {
            registry: self.registry.as_ref(),
            data_dir: &self.config.router.data_dir,
            turn_id: &turn_id,
            session_id: &session_id,
            event_tx: &event_tx,
            armed: true,
        };

        let terminal: Mutex<Option<TurnEvent>> = Mutex::new(None);
        let emit_event = |event: TurnEvent| {
            if let Some(registry) = &self.registry {
                registry.emit(&self.config.router.data_dir, &turn_id, event.clone());
            }
            if matches!(event, TurnEvent::TurnEnd { .. }) {
                *lock_terminal(&terminal) = Some(event);
                return;
            }
            let _ = event_tx.try_send(event);
        };

        let outcome = self
            .run_members(spec, &turn_id, &event_tx, &emit_event)
            .await;

        // One exit path (B5). An error that reached no `TurnEnd` — a refused cwd, an unknown
        // model, a combo member off the catalogue — used to leave the SSE stream simply
        // stopping and the registry entry `Running` for the life of the daemon. Nothing is
        // resolved this early, so the identity fields stay empty; `TurnStart` never fired.
        if let Err(err) = &outcome {
            if lock_terminal(&terminal).is_none() {
                let state = terminal_state_for(err);
                let result = build_turn_result(TurnResultArgs {
                    turn_id: &turn_id,
                    session_id: &session_id,
                    state,
                    returncode: -1,
                    text: err.to_string(),
                    usage: &UsageBreakdown::default(),
                    cost: 0.0,
                    context_window: 0,
                    backend: BackendId::ClaudeCode,
                    account_id: "",
                    resolved_model: "",
                    breaches: &[],
                });
                persist_turn_result(
                    self.registry.as_ref(),
                    &self.config.router.data_dir,
                    &turn_id,
                    &with_error_code(result, err.code()),
                );
                emit_event(TurnEvent::TurnEnd {
                    state,
                    returncode: -1,
                    text: err.to_string(),
                    retry_after_seconds: err.retry_after_seconds(),
                    touched_paths: vec![],
                    breaches: vec![],
                });
            }
        }

        // Every path above has recorded its own end by now.
        finish_guard.disarm();

        let terminal_event = lock_terminal(&terminal).take();
        if let Some(event) = terminal_event {
            let _ = event_tx.send(event).await;
        }

        outcome
    }

    async fn run_members<E: Fn(TurnEvent)>(
        &self,
        spec: TurnSpec,
        turn_id: &str,
        event_tx: &mpsc::Sender<TurnEvent>,
        emit_event: &E,
    ) -> Result<TurnResult, TurnError> {
        // Opt-in only: `cfg(test)` for crate unit tests, or `--features test-hooks`.
        // Never compiled into `thickapi serve` (F7).
        #[cfg(any(test, feature = "test-hooks"))]
        if spec.metadata.contains_key("thickapi_test_panic") {
            panic!("injected by thickapi_test_panic");
        }

        let _cwd = validate_spec(&spec, &self.config.router)?;
        let members = resolve_combo(&spec, &self.config)?;

        let cancel = self
            .registry
            .as_ref()
            .and_then(|r| r.cancel_token(&turn_id.to_string()))
            .map(|t| t.shared())
            .unwrap_or_else(|| Arc::new(AtomicBool::new(false)));

        let mut working = spec;
        for (index, member_model) in members.iter().enumerate() {
            if cancel.load(Ordering::SeqCst) {
                return self.finish_cancelled(turn_id, &working.session_id, emit_event);
            }

            working.model = member_model.clone();

            if working.model.starts_with("or/") && working.allow_paid {
                let spent = self
                    .store
                    .paid_spend_today()
                    .map_err(|e| TurnError::Other(e.to_string()))?;
                if spent >= self.config.router.max_paid_usd_per_day {
                    return Err(self.refuse_paid_cap(
                        turn_id,
                        &working.session_id,
                        &working.model,
                        emit_event,
                    ));
                }
            }

            match self
                .run_single_member(&working, turn_id, cancel.clone(), event_tx, emit_event)
                .await
            {
                Ok(result) if result.state == TurnState::Success => {
                    if result.backend == BackendId::Pi && result.cost > 0.0 {
                        let _ = self.store.add_paid_spend(result.cost);
                    }
                    return Ok(result);
                }
                Ok(result) if result.state == TurnState::Cancelled => {
                    return Ok(result);
                }
                Ok(result) if result.state == TurnState::RateLimited => {
                    let Some(next) = members.get(index + 1) else {
                        emit_event(TurnEvent::TurnEnd {
                            state: TurnState::RateLimited,
                            returncode: result.returncode,
                            text: result.text.clone(),
                            retry_after_seconds: Some(60),
                            touched_paths: vec![],
                            breaches: vec![],
                        });
                        if let Some(registry) = &self.registry {
                            registry.finish(&turn_id.to_string(), result.clone());
                        }
                        return Ok(result);
                    };
                    if next.starts_with("or/") && working.allow_paid {
                        let spent = self
                            .store
                            .paid_spend_today()
                            .map_err(|e| TurnError::Other(e.to_string()))?;
                        if spent >= self.config.router.max_paid_usd_per_day {
                            return Err(self.refuse_paid_cap(
                                turn_id,
                                &working.session_id,
                                next,
                                emit_event,
                            ));
                        }
                    }
                    emit_event(TurnEvent::TurnFallback {
                        from_model: member_model.clone(),
                        to_model: next.clone(),
                        reason: "rate_limited".into(),
                    });
                    if next.starts_with("or/") && !working.allow_paid {
                        emit_event(TurnEvent::TurnEnd {
                            state: TurnState::RateLimited,
                            returncode: result.returncode,
                            text: result.text.clone(),
                            retry_after_seconds: Some(60),
                            touched_paths: vec![],
                            breaches: vec![],
                        });
                        if let Some(registry) = &self.registry {
                            registry.finish(&turn_id.to_string(), result.clone());
                        }
                        return Ok(result);
                    }
                    if cancel.load(Ordering::SeqCst) {
                        return self.finish_cancelled(turn_id, &working.session_id, emit_event);
                    }
                    continue;
                }
                Ok(result) => return Ok(result),
                Err(err) => return Err(err),
            }
        }

        Err(TurnError::Other("combo produced no result".into()))
    }

    /// Daily paid cap: persist `rate_limited`, emit `TurnEnd`, then return the error so the
    /// first JSON answer is 429 (F4). Replay uses `error_code` (F5).
    fn refuse_paid_cap<E: Fn(TurnEvent)>(
        &self,
        turn_id: &str,
        session_id: &str,
        resolved_model: &str,
        emit_event: &E,
    ) -> TurnError {
        let msg = "daily paid cap exceeded".to_string();
        let err = TurnError::RateLimited(msg.clone());
        let result = with_error_code(
            build_turn_result(TurnResultArgs {
                turn_id,
                session_id,
                state: TurnState::RateLimited,
                returncode: -1,
                text: msg.clone(),
                usage: &UsageBreakdown::default(),
                cost: 0.0,
                context_window: 0,
                backend: BackendId::Pi,
                account_id: "",
                resolved_model,
                breaches: &[],
            }),
            err.code(),
        );
        persist_turn_result(
            self.registry.as_ref(),
            &self.config.router.data_dir,
            turn_id,
            &result,
        );
        emit_event(TurnEvent::TurnEnd {
            state: TurnState::RateLimited,
            returncode: -1,
            text: msg,
            retry_after_seconds: Some(60),
            touched_paths: vec![],
            breaches: vec![],
        });
        err
    }

    /// Announce a terminal state the child never produced — pool refusal, a CLI missing from
    /// PATH, a snapshot that would not take, a child the engine had to kill. Persists
    /// `result.json`, finishes the registry entry and emits `TurnEnd`; the caller still returns
    /// the `TurnError` so the JSON route keeps its SPEC §3.1 code (B5).
    #[allow(clippy::too_many_arguments)]
    fn emit_terminal<E: Fn(TurnEvent)>(
        &self,
        turn_id: &str,
        session_id: &str,
        resolved: &ResolvedTurn<'_>,
        state: TurnState,
        text: String,
        retry_after_seconds: Option<u64>,
        error_code: &str,
        emit_event: &E,
    ) {
        let result = with_error_code(
            build_turn_result(TurnResultArgs {
                turn_id,
                session_id,
                state,
                returncode: -1,
                text: text.clone(),
                usage: &UsageBreakdown::default(),
                cost: 0.0,
                context_window: resolved.model.context_window,
                backend: resolved.backend,
                account_id: &resolved.account.id,
                resolved_model: &resolved.model.cli_model,
                breaches: &[],
            }),
            error_code,
        );
        persist_turn_result(
            self.registry.as_ref(),
            &self.config.router.data_dir,
            turn_id,
            &result,
        );
        emit_event(TurnEvent::TurnEnd {
            state,
            returncode: -1,
            text,
            retry_after_seconds,
            touched_paths: vec![],
            breaches: vec![],
        });
    }

    fn finish_cancelled<E: Fn(TurnEvent)>(
        &self,
        turn_id: &str,
        session_id: &str,
        emit_event: &E,
    ) -> Result<TurnResult, TurnError> {
        let result = TurnResult {
            turn_id: turn_id.to_string(),
            session_id: session_id.to_string(),
            state: TurnState::Cancelled,
            returncode: -1,
            text: String::new(),
            tokens: 0,
            cost: 0.0,
            context_tokens: 0,
            usage: UsageBreakdown::default(),
            context_window: 0,
            backend: BackendId::ClaudeCode,
            account_id: String::new(),
            resolved_model: String::new(),
            error_code: None,
        };
        // A cancelled turn is reconnectable like any other (SPEC §1.7), so it gets its
        // `result.json` too, not only a registry entry (B5).
        persist_turn_result(
            self.registry.as_ref(),
            &self.config.router.data_dir,
            turn_id,
            &result,
        );
        emit_event(TurnEvent::TurnEnd {
            state: TurnState::Cancelled,
            returncode: -1,
            text: String::new(),
            retry_after_seconds: None,
            touched_paths: vec![],
            breaches: vec![],
        });
        Ok(result)
    }

    async fn run_single_member<E: Fn(TurnEvent)>(
        &self,
        spec: &TurnSpec,
        turn_id: &str,
        cancel: Arc<AtomicBool>,
        event_tx: &mpsc::Sender<TurnEvent>,
        emit_event: &E,
    ) -> Result<TurnResult, TurnError> {
        let resolved = resolve(spec, &self.config, &self.store)?;
        let adapter = for_backend(resolved.backend);

        let mut spec = spec.clone();
        if let Some(policy) = &spec.enforce_policy {
            let globs = Enforcer::deny_globs(policy, &self.config.router.data_dir);
            spec.deny_writes.extend(globs);
        }

        emit_event(TurnEvent::TurnStart {
            turn_id: turn_id.to_string(),
            session_id: spec.session_id.clone(),
            backend: resolved.backend,
            account_id: resolved.account.id.clone(),
            model: resolved.model.id.clone(),
        });

        let pool_name = pool_name_for_backend(resolved.backend);
        let _pool_guard = if let Some(pools) = &self.pools {
            match pools
                .acquire(
                    pool_name,
                    &mut |event| emit_event(event),
                    Some(cancel.clone()),
                )
                .await
            {
                Ok(guard) => Some(guard),
                Err(TurnError::RateLimited(msg)) => {
                    self.emit_terminal(
                        turn_id,
                        &spec.session_id,
                        &resolved,
                        TurnState::RateLimited,
                        msg.clone(),
                        Some(60),
                        "rate_limited",
                        emit_event,
                    );
                    return Err(TurnError::RateLimited(msg));
                }
                Err(TurnError::QueueTimeout(msg)) => {
                    emit_event(TurnEvent::Error {
                        code: "queue_timeout".into(),
                        message: msg.clone(),
                    });
                    self.emit_terminal(
                        turn_id,
                        &spec.session_id,
                        &resolved,
                        TurnState::Timeout,
                        msg.clone(),
                        None,
                        "queue_timeout",
                        emit_event,
                    );
                    return Err(TurnError::QueueTimeout(msg));
                }
                Err(TurnError::Other(msg)) if msg.contains("cancelled while queued") => {
                    return self.finish_cancelled(turn_id, &spec.session_id, emit_event);
                }
                Err(err) => return Err(err),
            }
        } else {
            None
        };

        let cwd_lock = spec
            .enforce_policy
            .as_ref()
            .map(|_| acquire_cwd_lock(&spec.cwd));

        let mut session = resolved.session.clone();
        let mut retried_stale = false;
        let mut enforce_snapshot = None;
        let account_id_for_health = resolved.account.id.clone();
        let timeout = Duration::from_secs(spec.timeout_seconds);
        let idle_stall = Duration::from_secs(
            self.config
                .router
                .idle_stall_seconds
                .get(pool_name)
                .copied()
                .unwrap_or_else(|| default_idle_stall_seconds(resolved.backend)),
        );

        let mut mcp_restore: Option<crate::adapters::RestoreOnDrop> = None;
        let outcome = loop {
            // Drop the previous attempt's overlay before `build_spawn` writes a new one.
            drop(mcp_restore.take());
            if cancel.load(Ordering::SeqCst) {
                return self.finish_cancelled(turn_id, &spec.session_id, emit_event);
            }

            let mut plan = match adapter.build_spawn(
                &spec,
                resolved.model,
                &session,
                &self.config.router.data_dir,
            ) {
                Ok(plan) => plan,
                Err(e) => {
                    let _ = self
                        .store
                        .upsert_account_health(&resolved.account.id, "spawn_failed");
                    let err = TurnError::SpawnFailed(e.to_string());
                    self.emit_terminal(
                        turn_id,
                        &spec.session_id,
                        &resolved,
                        TurnState::Failed,
                        err.to_string(),
                        None,
                        err.code(),
                        emit_event,
                    );
                    return Err(err);
                }
            };
            mcp_restore = Some(crate::adapters::RestoreOnDrop {
                items: plan.cleanup.clone(),
            });

            if resolved.backend == BackendId::Pi {
                match inject_openrouter_env(std::mem::take(&mut plan.env), &resolved.account) {
                    Ok(env) => plan.env = env,
                    Err(e) => {
                        let err = TurnError::SpawnFailed(e.to_string());
                        self.emit_terminal(
                            turn_id,
                            &spec.session_id,
                            &resolved,
                            TurnState::Failed,
                            err.to_string(),
                            None,
                            err.code(),
                            emit_event,
                        );
                        return Err(err);
                    }
                }
            }

            if !retried_stale {
                let seeded = seed_backend_trust(plan.backend, &plan.trust_cwd);
                if !seeded.ok || seeded.message.is_some() {
                    tracing::warn!(
                        backend = ?plan.backend,
                        key = %seeded.key,
                        message = seeded.message.as_deref().unwrap_or(""),
                        "trust seeding did not complete; the CLI may prompt for folder trust"
                    );
                }

                if let Some(policy) = &spec.enforce_policy {
                    if cancel.load(Ordering::SeqCst) {
                        return self.finish_cancelled(turn_id, &spec.session_id, emit_event);
                    }
                    let _guard = match &cwd_lock {
                        Some(lock) => Some(lock.lock().await),
                        None => None,
                    };
                    let mode = Enforcer::resolve_mode(&spec.cwd, policy);
                    let backup = turn_artifact_dir(&self.config.router.data_dir, turn_id)
                        .join("tree-snapshot");
                    let snapshot = match Enforcer.snapshot(&spec.cwd, mode, &backup) {
                        Ok(snapshot) => snapshot,
                        Err(e) => {
                            drop(_guard);
                            let err = TurnError::from(e);
                            self.emit_terminal(
                                turn_id,
                                &spec.session_id,
                                &resolved,
                                TurnState::Failed,
                                err.to_string(),
                                None,
                                err.code(),
                                emit_event,
                            );
                            return Err(err);
                        }
                    };
                    drop(_guard);
                    if cancel.load(Ordering::SeqCst) {
                        return self.finish_cancelled(turn_id, &spec.session_id, emit_event);
                    }
                    enforce_snapshot = Some(snapshot);
                }
            }

            let data_dir = self.config.router.data_dir.clone();
            let turn_id_for_child = turn_id.to_string();
            let cancel_flag = cancel.clone();
            let registry_child = self.registry.clone();
            let data_dir_child = data_dir.clone();
            let event_tx_child = event_tx.clone();
            let store_child = self.store.clone();
            let backend_child = resolved.backend;
            let account_id_child = resolved.account.id.clone();
            // The blocking task owns the guard for the child's whole life; taking it here keeps
            // the wait on the scheduler (B7).
            let cwd_guard = match &cwd_lock {
                Some(lock) => Some(lock.clone().lock_owned().await),
                None => None,
            };
            let child_run = tokio::task::spawn_blocking(move || {
                let _guard = cwd_guard;
                let emit_child = |event: TurnEvent| {
                    if let Some(registry) = &registry_child {
                        registry.emit(&data_dir_child, &turn_id_for_child, event.clone());
                    }
                    let _ = event_tx_child.try_send(event);
                };
                run_child_turn(
                    adapter,
                    &plan,
                    &data_dir,
                    &turn_id_for_child,
                    timeout,
                    idle_stall,
                    cancel_flag,
                    emit_child,
                    Some(&store_child),
                    backend_child,
                    &account_id_child,
                )
            })
            .await;

            let outcome = match child_run {
                Ok(Ok(outcome)) => outcome,
                Ok(Err(err)) => {
                    self.emit_terminal(
                        turn_id,
                        &spec.session_id,
                        &resolved,
                        terminal_state_for(&err),
                        err.to_string(),
                        None,
                        err.code(),
                        emit_event,
                    );
                    return Err(err);
                }
                Err(join) => {
                    let err = TurnError::Other(join.to_string());
                    self.emit_terminal(
                        turn_id,
                        &spec.session_id,
                        &resolved,
                        TurnState::Failed,
                        err.to_string(),
                        None,
                        err.code(),
                        emit_event,
                    );
                    return Err(err);
                }
            };

            if outcome.unknown_session && session.cli_session.is_some() && !retried_stale {
                let _ = self.store.clear_cli_session(&session.session_id);
                session.cli_session = None;
                retried_stale = true;
                continue;
            }
            break outcome;
        };
        let _mcp_restore = mcp_restore;

        if let Some(status) = outcome.health_status.as_deref() {
            let _ = self
                .store
                .upsert_account_health(&account_id_for_health, status);
        } else {
            // A timeout says nothing about the account, so leave the recorded health alone.
            let health_status = match outcome.state {
                TurnState::RateLimited => Some("rate_limited"),
                TurnState::SubscriptionBlocked => Some("subscription_blocked"),
                TurnState::Timeout => None,
                _ => Some("ok"),
            };
            if let Some(health_status) = health_status {
                let _ = self
                    .store
                    .upsert_account_health(&account_id_for_health, health_status);
            }
        }

        let mut touched_paths = vec![];
        let mut breaches = vec![];

        if let (Some(policy), Some(snapshot)) = (&spec.enforce_policy, &enforce_snapshot) {
            // Enforce even if the turn was cancelled during the child so writes still roll back.
            let _guard = match &cwd_lock {
                Some(lock) => Some(lock.lock().await),
                None => None,
            };
            let enforce_outcome =
                match Enforcer.enforce(&spec.cwd, policy, &self.config.router.data_dir, snapshot) {
                    Ok(enforce_outcome) => enforce_outcome,
                    Err(e) => {
                        drop(_guard);
                        let err = TurnError::from(e);
                        self.emit_terminal(
                            turn_id,
                            &spec.session_id,
                            &resolved,
                            TurnState::Failed,
                            err.to_string(),
                            None,
                            err.code(),
                            emit_event,
                        );
                        return Err(err);
                    }
                };
            drop(_guard);
            let has_breach = enforce_outcome.has_breach();
            touched_paths = enforce_outcome.touched_paths;
            breaches = enforce_outcome.breaches;
            if has_breach {
                let breach_err = breaches.clone();
                let end_state = effective_turn_state(outcome.state, &breaches);
                let result = build_turn_result(TurnResultArgs {
                    turn_id,
                    session_id: &spec.session_id,
                    state: outcome.state,
                    returncode: outcome.returncode,
                    text: outcome.text.clone(),
                    usage: &outcome.usage,
                    cost: outcome.cost,
                    context_window: resolved.model.context_window,
                    backend: resolved.backend,
                    account_id: &resolved.account.id,
                    resolved_model: &resolved.model.cli_model,
                    breaches: &breaches,
                });
                persist_turn_result(
                    self.registry.as_ref(),
                    &self.config.router.data_dir,
                    turn_id,
                    &with_error_code(result, "permission_breach"),
                );
                emit_event(TurnEvent::TurnEnd {
                    state: end_state,
                    returncode: outcome.returncode,
                    text: outcome.text.clone(),
                    retry_after_seconds: outcome.retry_after_seconds,
                    touched_paths,
                    breaches,
                });
                return Err(TurnError::PermissionBreach(breach_err));
            }
        }

        if outcome.state == TurnState::Success {
            let cli_session = outcome.cli_session.clone().or(session.cli_session.clone());
            if let Err(e) = update_session_after_turn(&self.store, &resolved.session, cli_session) {
                let err = TurnError::Other(e.to_string());
                self.emit_terminal(
                    turn_id,
                    &spec.session_id,
                    &resolved,
                    TurnState::Failed,
                    err.to_string(),
                    None,
                    err.code(),
                    emit_event,
                );
                return Err(err);
            }
        }

        let end_state = effective_turn_state(outcome.state, &breaches);
        let result = build_turn_result(TurnResultArgs {
            turn_id,
            session_id: &spec.session_id,
            state: outcome.state,
            returncode: outcome.returncode,
            text: outcome.text.clone(),
            usage: &outcome.usage,
            cost: outcome.cost,
            context_window: resolved.model.context_window,
            backend: resolved.backend,
            account_id: &resolved.account.id,
            resolved_model: &resolved.model.cli_model,
            breaches: &breaches,
        });
        persist_turn_result(
            self.registry.as_ref(),
            &self.config.router.data_dir,
            turn_id,
            &result,
        );
        emit_event(TurnEvent::TurnEnd {
            state: end_state,
            returncode: outcome.returncode,
            text: outcome.text.clone(),
            retry_after_seconds: outcome.retry_after_seconds,
            touched_paths,
            breaches,
        });

        // A killed child ends the turn exactly like any other: events, `result.json` with
        // whatever it managed to say, registry finished — and only then the typed error the
        // JSON route turns into 504 (B5).
        match outcome.terminal_error {
            Some(err) => Err(err),
            None => Ok(result),
        }
    }
}

struct ChildOutcome {
    state: TurnState,
    returncode: i32,
    text: String,
    usage: UsageBreakdown,
    cost: f64,
    cli_session: Option<String>,
    retry_after_seconds: Option<u64>,
    health_status: Option<String>,
    /// Set when the engine, not the child, ended the turn. The outcome still carries whatever
    /// the child produced, so the turn ends normally and the error only decides the HTTP code.
    terminal_error: Option<TurnError>,
    unknown_session: bool,
}

#[allow(clippy::too_many_arguments)]
fn run_child_turn<E: Fn(TurnEvent)>(
    adapter: &dyn crate::adapters::BackendAdapter,
    plan: &crate::adapters::SpawnPlan,
    data_dir: &std::path::Path,
    turn_id: &TurnId,
    timeout: Duration,
    idle_stall: Duration,
    cancel: Arc<AtomicBool>,
    emit: E,
    store: Option<&Store>,
    backend: crate::types::BackendId,
    account_id: &str,
) -> Result<ChildOutcome, TurnError> {
    let _ = account_id;
    let artifact_dir = artifacts::ensure_turn_dir(data_dir, turn_id)
        .map_err(|e| TurnError::SpawnFailed(e.to_string()))?;
    artifacts::write_spawn_json(&artifact_dir, plan)
        .map_err(|e| TurnError::SpawnFailed(e.to_string()))?;

    let job = job::JobObject::new().map_err(|e| TurnError::SpawnFailed(e.to_string()))?;
    let mut child =
        spawn::spawn_child(plan, &job).map_err(|e| TurnError::SpawnFailed(e.to_string()))?;
    let child_pid = child.id();
    if let Some(store) = store {
        let _ = store.register_process(turn_id, child_pid, backend);
    }

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| TurnError::SpawnFailed("no stdout".into()))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| TurnError::SpawnFailed("no stderr".into()))?;

    let stdout_log = artifacts::open_stdout_log(&artifact_dir)
        .map_err(|e| TurnError::SpawnFailed(e.to_string()))?;
    let stderr_log = artifacts::open_stderr_log(&artifact_dir)
        .map_err(|e| TurnError::SpawnFailed(e.to_string()))?;

    let (line_tx, line_rx) = std::sync::mpsc::channel::<String>();

    let stdout_thread = std::thread::spawn(move || {
        let reader = BufReader::new(stdout);
        let mut log = stdout_log;
        for line in reader.lines() {
            match line {
                Ok(line) => {
                    let _ = writeln!(log, "{}", line);
                    if line_tx.send(line).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
        let _ = log.flush();
    });

    // A child that only logs progress to stderr is working, not stalled (B12).
    let stderr_seen = Arc::new(AtomicBool::new(false));
    let stderr_seen_thread = stderr_seen.clone();

    let stderr_thread =
        std::thread::spawn(move || read_stderr_tail(stderr, stderr_log, &stderr_seen_thread));

    // Readers first, then stdin: the prompt write must never be able to fill the stdin pipe
    // while the child is blocked on a stdout pipe nobody drains (B6).
    let mut stdin_thread = spawn::spawn_stdin_writer(&mut child, plan);

    let mut acc = jsonl::JsonlAccumulator::new();
    let started = Instant::now();
    let mut last_line = Instant::now();

    let finish_process = || {
        if let Some(store) = store {
            let _ = store.finish_process(turn_id);
        }
    };

    while child.try_wait().ok().flatten().is_none() {
        if cancel.load(Ordering::SeqCst) {
            terminate_and_wait(&job, &mut child);
            join_stdin(&mut stdin_thread);
            let _ = join_within(stdout_thread, READER_JOIN_GRACE);
            let _ = join_within(stderr_thread, READER_JOIN_GRACE);
            finish_process();
            return Ok(ChildOutcome {
                state: TurnState::Cancelled,
                returncode: -1,
                text: acc.text,
                usage: acc.usage,
                cost: acc.cost,
                cli_session: acc.cli_session,
                retry_after_seconds: None,
                health_status: None,
                terminal_error: None,
                unknown_session: false,
            });
        }
        if started.elapsed() > timeout {
            terminate_and_wait(&job, &mut child);
            join_stdin(&mut stdin_thread);
            let _ = join_within(stdout_thread, READER_JOIN_GRACE);
            let stderr = join_within(stderr_thread, READER_JOIN_GRACE).unwrap_or_default();
            finish_process();
            let message = format!(
                "exceeded {}s timeout; stderr tail: {}",
                timeout.as_secs(),
                stderr_tail(&stderr)
            );
            return Ok(timed_out(acc, message));
        }
        if stderr_seen.swap(false, Ordering::Relaxed) {
            last_line = Instant::now();
        }
        if last_line.elapsed() > idle_stall {
            terminate_and_wait(&job, &mut child);
            join_stdin(&mut stdin_thread);
            let _ = join_within(stdout_thread, READER_JOIN_GRACE);
            let stderr = join_within(stderr_thread, READER_JOIN_GRACE).unwrap_or_default();
            finish_process();
            let message = format!(
                "idle stall {}s exceeded; stderr tail: {}",
                idle_stall.as_secs(),
                stderr_tail(&stderr)
            );
            return Ok(timed_out(acc, message));
        }
        match line_rx.recv_timeout(Duration::from_millis(200)) {
            Ok(line) => {
                last_line = Instant::now();
                acc.process_line(adapter, &line, &emit);
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    }

    // The root is gone, but its stdout may not be: a grandchild that inherited the pipe keeps
    // it open, and a late flush can still land. Stopping at the first 50 ms gap lost those
    // lines (B15); waiting for `Disconnected` with no deadline let a detached grandchild hold
    // the pool permit, the cwd lock and the `Running` registry entry for as long as it lived.
    // So: drain until the pipe closes or a short deadline passes, then kill the job tree —
    // that closes every handle still open, the readers see EOF — and only then join them.
    let drain_deadline = Instant::now()
        + timeout
            .saturating_sub(started.elapsed())
            .min(POST_EXIT_DRAIN);
    loop {
        let left = drain_deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            break;
        }
        match line_rx.recv_timeout(left) {
            Ok(line) => {
                acc.process_line(adapter, &line, &emit);
            }
            Err(_) => break,
        }
    }
    terminate_and_wait(&job, &mut child);
    join_stdin(&mut stdin_thread);
    let _ = join_within(stdout_thread, READER_JOIN_GRACE);
    while let Ok(line) = line_rx.try_recv() {
        acc.process_line(adapter, &line, &emit);
    }

    let stderr = join_within(stderr_thread, READER_JOIN_GRACE).unwrap_or_default();
    let status = child
        .wait()
        .map_err(|e| TurnError::SpawnFailed(e.to_string()))?;
    let returncode = status.code().unwrap_or(-1);
    finish_process();

    if let Some(prompt_file) = &plan.prompt_file {
        let _ = std::fs::remove_file(prompt_file);
    }

    let failure_class = if returncode != 0 {
        adapter.classify_failure(returncode, &stderr, &acc.last_events)
    } else {
        TurnFailureClass::Generic
    };

    let unknown_session = returncode != 0 && failure_class == TurnFailureClass::UnknownSession;

    let health_status = if failure_class == TurnFailureClass::SuspectedBlocked {
        Some("suspected_blocked".to_string())
    } else {
        None
    };

    let state = if returncode == 0 {
        TurnState::Success
    } else {
        match failure_class {
            TurnFailureClass::RateLimited | TurnFailureClass::SuspectedBlocked => {
                TurnState::RateLimited
            }
            TurnFailureClass::SubscriptionBlocked => TurnState::SubscriptionBlocked,
            _ => TurnState::Failed,
        }
    };

    Ok(ChildOutcome {
        state,
        returncode,
        text: acc.text,
        usage: acc.usage,
        cost: acc.cost,
        cli_session: acc.cli_session,
        retry_after_seconds: if state == TurnState::RateLimited {
            Some(60)
        } else {
            None
        },
        health_status,
        terminal_error: None,
        unknown_session,
    })
}

/// The turn ends the normal way; `terminal_error` carries the 504 the JSON route owes the
/// caller (B5). Whatever the child said before the engine killed it stays in the result.
fn timed_out(acc: jsonl::JsonlAccumulator, message: String) -> ChildOutcome {
    ChildOutcome {
        state: TurnState::Timeout,
        returncode: -1,
        text: acc.text,
        usage: acc.usage,
        cost: acc.cost,
        cli_session: acc.cli_session,
        retry_after_seconds: None,
        health_status: None,
        terminal_error: Some(TurnError::TurnTimeout(message)),
        unknown_session: false,
    }
}

fn stderr_tail(stderr: &str) -> String {
    let tail: String = stderr.chars().rev().take(500).collect();
    tail.chars().rev().collect()
}

/// Reap the stdin feeder. The child is already dead or done reading at every call site, so the
/// write has either finished or broken; nothing here can block.
fn join_stdin(handle: &mut Option<std::thread::JoinHandle<()>>) {
    if let Some(thread) = handle.take() {
        let _ = thread.join();
    }
}

/// How long after the root process exits its stdout is still read before the job tree is
/// killed. Long enough for a late flush, short next to any turn.
const POST_EXIT_DRAIN: Duration = Duration::from_secs(5);

/// How long a reader thread gets to see EOF once the job tree is dead.
const READER_JOIN_GRACE: Duration = Duration::from_secs(2);

/// Join a reader thread, but never wait on it for ever: if the kill could not close the pipe
/// (a process that broke away from the job), the thread is left behind rather than the turn.
fn join_within<T>(handle: std::thread::JoinHandle<T>, grace: Duration) -> Option<T> {
    let deadline = Instant::now() + grace;
    while !handle.is_finished() {
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    handle.join().ok()
}

/// Kill the child tree: Job Object terminate on Windows, `kill_process_tree` on Unix.
/// Root-pid `child.kill()` alone leaves grandchildren (SPEC §1.7 Hung child).
/// On Unix, `JobObject::terminate` is unsupported. If the root has already been
/// reaped, do not `kill -9` that pid — it may have been reused (F1).
fn terminate_and_wait(job: &job::JobObject, child: &mut std::process::Child) {
    if job.terminate(1).is_ok() {
        let _ = child.wait();
        return;
    }
    let already_exited = child.try_wait().ok().flatten().is_some();
    if !kill_tree_after_job_failed(already_exited) {
        return;
    }
    crate::process::kill_process_tree(child.id());
    let _ = child.kill();
    let _ = child.wait();
}

/// After `JobObject::terminate` failed (Unix), signal the tree only if the root
/// is still ours. A reaped pid must not be `kill -9`'d (F1).
fn kill_tree_after_job_failed(child_already_exited: bool) -> bool {
    !child_already_exited
}

/// Bytes of child stderr kept for `classify_failure`.
const STDERR_TAIL_BYTES: usize = 16 * 1024;

/// Drain child stderr into `log`, flag activity in `seen` (B12), and return the last
/// [`STDERR_TAIL_BYTES`] decoded once at the end. The tail is kept as bytes: decoding each
/// read on its own turned a multibyte char split across reads into U+FFFD and let the trim
/// land off a char boundary, which panicked the reader (R12).
fn read_stderr_tail(stderr: impl Read, mut log: impl Write, seen: &AtomicBool) -> String {
    let mut reader = BufReader::new(stderr);
    let mut buf = [0u8; 4096];
    let mut tail: Vec<u8> = Vec::with_capacity(STDERR_TAIL_BYTES * 2);
    while let Ok(n) = reader.read(&mut buf) {
        if n == 0 {
            break;
        }
        seen.store(true, Ordering::Relaxed);
        let Some(chunk) = buf.get(..n) else {
            break;
        };
        let _ = log.write_all(chunk);
        tail.extend_from_slice(chunk);
        if tail.len() > STDERR_TAIL_BYTES * 2 {
            tail.drain(..tail.len() - STDERR_TAIL_BYTES);
        }
    }
    let _ = log.flush();
    let start = tail.len().saturating_sub(STDERR_TAIL_BYTES);
    let mut tail = tail.get(start..).unwrap_or_default();
    // Drop the continuation bytes of a char the trim cut in half.
    while let Some((&b, rest)) = tail.split_first() {
        if b & 0xC0 != 0x80 {
            break;
        }
        tail = rest;
    }
    String::from_utf8_lossy(tail).into_owned()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use std::process::{Command, Stdio};

    /// R12: more than 16 KiB of multibyte stderr, with chars split across 4 KiB reads, must
    /// not panic the reader, and the tail must decode cleanly.
    #[test]
    fn stderr_tail_survives_multibyte_split_across_reads() {
        // "é" is 2 bytes and "€" 3 bytes: with a 1-byte lead the pattern straddles every
        // 4096-byte read boundary at some point.
        let mut input = b"x".to_vec();
        for _ in 0..20_000 {
            input.extend_from_slice("é€".as_bytes());
        }
        input.extend_from_slice(b" rate limit reached");
        let seen = AtomicBool::new(false);
        let mut log = Vec::new();
        let tail = read_stderr_tail(input.as_slice(), &mut log, &seen);
        assert!(seen.load(Ordering::Relaxed));
        assert_eq!(log, input, "log gets every byte");
        assert!(tail.len() <= STDERR_TAIL_BYTES);
        assert!(tail.len() >= STDERR_TAIL_BYTES - 3);
        assert!(
            !tail.contains('\u{FFFD}'),
            "tail must decode without replacement chars"
        );
        assert!(tail.ends_with("é€ rate limit reached"));
    }

    #[test]
    fn stderr_tail_short_input_is_whole() {
        let seen = AtomicBool::new(false);
        let tail = read_stderr_tail("unknown session ✓".as_bytes(), std::io::sink(), &seen);
        assert_eq!(tail, "unknown session ✓");
    }

    #[tokio::test]
    async fn cwd_lock_survives_a_panic_under_the_guard() {
        let dir = std::env::temp_dir().join("thickapi-cwd-lock-panic-test");
        std::fs::create_dir_all(&dir).expect("dir");
        let lock = acquire_cwd_lock(&dir);
        let panicker = lock.clone();
        let handle = tokio::spawn(async move {
            let _guard = panicker.lock().await;
            panic!("a turn blew up holding the cwd lock");
        });
        assert!(handle.await.is_err(), "the task must have panicked");

        let again = acquire_cwd_lock(&dir);
        assert!(Arc::ptr_eq(&lock, &again), "one lock per cwd");
        // A `std::sync::Mutex` is poisoned at this point, and every later turn in this cwd
        // panics on `.expect("cwd lock")` (B7).
        let _guard = tokio::time::timeout(Duration::from_millis(500), again.lock())
            .await
            .expect("a later turn must still be able to take the lock");
    }

    #[test]
    fn terminate_and_wait_kills_assigned_process() {
        let job = job::JobObject::new().expect("job");
        let mut cmd = {
            #[cfg(windows)]
            {
                let mut c = Command::new("ping");
                c.args(["-n", "30", "127.0.0.1"]);
                c
            }
            #[cfg(not(windows))]
            {
                let mut c = Command::new("sleep");
                c.arg("30");
                c
            }
        };
        cmd.stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(0x0800_0000);
        }
        let mut child = cmd.spawn().expect("spawn hang child");
        job.assign(&child).expect("assign");
        let pid = child.id();
        assert!(
            crate::process::process_alive(pid),
            "child should be running"
        );
        terminate_and_wait(&job, &mut child);
        assert!(
            !crate::process::process_alive(pid),
            "Job terminate / process tree must reap pid {pid}"
        );
    }

    #[test]
    fn join_within_gives_up_when_the_thread_does_not_finish() {
        let handle = std::thread::spawn(|| loop {
            std::thread::sleep(Duration::from_secs(60));
        });
        let started = Instant::now();
        assert!(join_within(handle, Duration::from_millis(80)).is_none());
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "a stuck reader must not hold the turn"
        );
    }

    #[test]
    fn a_reaped_pid_is_not_signalled() {
        assert!(
            !kill_tree_after_job_failed(true),
            "Unix fallback must not kill -9 a pid try_wait already reaped"
        );
        assert!(
            kill_tree_after_job_failed(false),
            "a still-running child after a failed job.terminate is still ours to kill"
        );
    }

    #[test]
    fn timeout_cancel_stall_do_not_join_readers_unbounded() {
        let prod = include_str!("mod.rs")
            .split("#[cfg(test)]")
            .next()
            .expect("production source");
        assert!(
            !prod.contains("stdout_thread.join()") && !prod.contains("stderr_thread.join()"),
            "timeout/cancel/stall must join readers with join_within, not thread.join()"
        );
        assert!(
            prod.contains("join_within(stdout_thread, READER_JOIN_GRACE)"),
            "abort paths must use the same reader grace as a normal exit"
        );
    }

    fn unit_test_config(temp: &std::path::Path) -> crate::config::ConfigBundle {
        crate::config::ConfigBundle {
            config_dir: temp.to_path_buf(),
            router: crate::config::RouterConfig {
                listen: "127.0.0.1:8787".into(),
                data_dir: temp.join("data"),
                lan: false,
                default_timeout_seconds: 30,
                queue_timeout_seconds: 300,
                max_queue_depth: 16,
                max_paid_usd_per_day: 5.0,
                drain_seconds: 120,
                refuse_paths: vec![],
                ..Default::default()
            },
            accounts: serde_yaml::from_str(include_str!(
                "../../../../configs/accounts.yaml.example"
            ))
            .expect("accounts"),
            models: serde_yaml::from_str(include_str!("../../../../configs/models.yaml.example"))
                .expect("models"),
            combos: serde_yaml::from_str(include_str!("../../../../configs/combos.yaml.example"))
                .expect("combos"),
            keys: crate::config::keys::KeysConfig::default(),
        }
    }

    #[tokio::test]
    async fn a_panicking_turn_persists_internal_error() {
        let temp = tempfile::tempdir().expect("tempdir");
        let registry = Arc::new(TurnRegistry::new());
        registry
            .register("turn-panics".to_string())
            .expect("register");
        let engine = TurnEngine::new(
            unit_test_config(temp.path()),
            Arc::new(Store::open_in_memory().expect("store")),
        )
        .with_registry(registry.clone());
        let mut spec = crate::types::TurnSpec {
            model: "subs/claude/fable".into(),
            prompt: "hi".into(),
            system_prompt: None,
            system_prompt_file: None,
            cwd: std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../.."),
            session_id: "engine-panics".into(),
            thinking: crate::types::Effort::Medium,
            tools: vec![],
            deny_writes: vec![],
            enforce_policy: None,
            mcp: crate::types::McpMode::Off,
            mcp_config: None,
            setting_sources: None,
            allow_api: false,
            combo: None,
            allow_paid: false,
            timeout_seconds: 30,
            client: None,
            turn_id: Some("turn-panics".into()),
            metadata: serde_json::Map::new(),
        };
        spec.metadata
            .insert("thickapi_test_panic".into(), serde_json::Value::Bool(true));

        let (tx, mut rx) = mpsc::channel(64);
        let run = tokio::spawn(async move { engine.run_turn(spec, tx).await });
        assert!(run.await.is_err(), "the injected panic must surface");

        let mut saw_end = false;
        while let Some(event) = rx.recv().await {
            if let TurnEvent::TurnEnd { state, text, .. } = event {
                assert_eq!(state, TurnState::Failed);
                assert!(text.starts_with("internal_error:"), "{text}");
                saw_end = true;
            }
        }
        assert!(saw_end, "the stream must not just stop");
        match registry.status(&"turn-panics".to_string()) {
            Some(TurnStatus::Finished(r)) => {
                assert_eq!(r.state, TurnState::Failed);
                assert_eq!(r.error_code.as_deref(), Some("internal_error"));
            }
            other => panic!("expected finished, got {other:?}"),
        }
        let stored = std::fs::read_to_string(
            artifacts::turn_artifact_dir(&temp.path().join("data"), "turn-panics")
                .join("result.json"),
        )
        .expect("result.json");
        let value: serde_json::Value = serde_json::from_str(&stored).expect("json");
        assert_eq!(value["error_code"], "internal_error");
        assert_eq!(registry.active_count(), 0);
    }
}
