use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::config::AccountsConfig;
use crate::error::TurnError;
use crate::types::TurnEvent;

/// How often a queued turn re-checks its place in the line.
const QUEUE_TICK: Duration = Duration::from_millis(500);

#[derive(Debug, Clone, serde::Serialize)]
pub struct PoolSnapshot {
    pub in_use: u32,
    pub max: u32,
    pub queued: u32,
}

struct PoolState {
    max_concurrent: u32,
    /// The slots themselves. A semaphore hands each permit to exactly one waiter and takes it
    /// back on drop, which the hand-rolled waiter list could not: a waiter timing out while
    /// `PoolGuard::drop` handed it a permit decremented `queued` twice and leaked a slot, and
    /// the `load`-then-`fetch_add` admission test let `max_concurrent` be exceeded (B8).
    slots: Arc<Semaphore>,
    queued: AtomicU32,
    next_ticket: AtomicU64,
    /// Tickets that have left the queue, however they left. `ticket - left` is a waiter's place.
    left: AtomicU64,
}

pub struct PoolManager {
    pools: HashMap<String, Arc<PoolState>>,
    queue_timeout: Duration,
    max_queue_depth: u32,
    draining: AtomicBool,
}

pub struct PoolGuard {
    _permit: OwnedSemaphorePermit,
}

/// A waiter's place in the queue, given up exactly once however the wait ends — acquired,
/// timed out, cancelled, or dropped part-way through a `select!`.
struct QueuedSlot {
    pool: Arc<PoolState>,
    ticket: u64,
}

impl QueuedSlot {
    fn enter(pool: &Arc<PoolState>) -> Self {
        pool.queued.fetch_add(1, Ordering::SeqCst);
        let ticket = pool.next_ticket.fetch_add(1, Ordering::SeqCst);
        Self {
            pool: pool.clone(),
            ticket,
        }
    }

    fn position(&self) -> u32 {
        let left = self.pool.left.load(Ordering::SeqCst);
        self.ticket.saturating_sub(left).saturating_add(1) as u32
    }
}

impl Drop for QueuedSlot {
    fn drop(&mut self) {
        self.pool.queued.fetch_sub(1, Ordering::SeqCst);
        self.pool.left.fetch_add(1, Ordering::SeqCst);
    }
}

impl PoolState {
    fn in_use(&self) -> u32 {
        self.max_concurrent
            .saturating_sub(self.slots.available_permits() as u32)
    }

    fn snapshot(&self) -> PoolSnapshot {
        PoolSnapshot {
            in_use: self.in_use(),
            max: self.max_concurrent,
            queued: self.queued.load(Ordering::SeqCst),
        }
    }
}

impl PoolManager {
    pub fn new(
        accounts: &AccountsConfig,
        queue_timeout_seconds: u64,
        max_queue_depth: u32,
    ) -> Self {
        let mut pools = HashMap::new();
        for (name, pool) in &accounts.pools {
            pools.insert(
                name.clone(),
                Arc::new(PoolState {
                    max_concurrent: pool.max_concurrent,
                    slots: Arc::new(Semaphore::new(pool.max_concurrent as usize)),
                    queued: AtomicU32::new(0),
                    next_ticket: AtomicU64::new(0),
                    left: AtomicU64::new(0),
                }),
            );
        }
        Self {
            pools,
            queue_timeout: Duration::from_secs(queue_timeout_seconds),
            max_queue_depth,
            draining: AtomicBool::new(false),
        }
    }

    pub fn set_draining(&self, draining: bool) {
        self.draining.store(draining, Ordering::SeqCst);
    }

    pub fn is_draining(&self) -> bool {
        self.draining.load(Ordering::SeqCst)
    }

    pub fn snapshot(&self, pool_name: &str) -> Option<PoolSnapshot> {
        self.pools.get(pool_name).map(|pool| pool.snapshot())
    }

    pub fn all_snapshots(&self) -> HashMap<String, PoolSnapshot> {
        self.pools
            .iter()
            .map(|(name, pool)| (name.clone(), pool.snapshot()))
            .collect()
    }

    pub async fn acquire<F>(
        &self,
        pool_name: &str,
        emit: &mut F,
        cancel: Option<Arc<AtomicBool>>,
    ) -> Result<PoolGuard, TurnError>
    where
        F: FnMut(TurnEvent),
    {
        if self.is_draining() {
            return Err(TurnError::Draining(
                "server is draining; new turns are not accepted".into(),
            ));
        }

        let pool = self
            .pools
            .get(pool_name)
            .ok_or_else(|| TurnError::UnknownModel(format!("missing pool {}", pool_name)))?
            .clone();

        if let Ok(permit) = pool.slots.clone().try_acquire_owned() {
            return Ok(PoolGuard { _permit: permit });
        }

        if pool.queued.load(Ordering::SeqCst) >= self.max_queue_depth {
            return Err(TurnError::RateLimited(format!(
                "pool {} queue depth exceeded",
                pool_name
            )));
        }

        let slot = QueuedSlot::enter(&pool);
        let start = Instant::now();
        let mut last_position = slot.position();
        emit(TurnEvent::Queued {
            position: last_position,
        });

        // Pinned outside the loop so the wait keeps its place in the semaphore's line, and so
        // dropping it on a timeout or a cancel hands on any permit it was about to take.
        let acquire = pool.slots.clone().acquire_owned();
        tokio::pin!(acquire);

        loop {
            if cancel.as_ref().is_some_and(|c| c.load(Ordering::SeqCst)) {
                return Err(TurnError::Other("turn cancelled while queued".into()));
            }
            let remaining = self.queue_timeout.saturating_sub(start.elapsed());
            if remaining.is_zero() {
                return Err(TurnError::QueueTimeout(format!(
                    "waited {}s for pool {} slot",
                    self.queue_timeout.as_secs(),
                    pool_name
                )));
            }

            tokio::select! {
                permit = &mut acquire => {
                    let permit = permit
                        .map_err(|_| TurnError::Other(format!("pool {} closed", pool_name)))?;
                    return Ok(PoolGuard { _permit: permit });
                }
                _ = tokio::time::sleep(QUEUE_TICK.min(remaining)) => {
                    let position = slot.position();
                    if position != last_position {
                        last_position = position;
                        emit(TurnEvent::Queued { position });
                    }
                }
            }
        }
    }
}

pub fn pool_name_for_backend(backend: crate::types::BackendId) -> &'static str {
    match backend {
        crate::types::BackendId::ClaudeCode => "claude",
        crate::types::BackendId::Grok => "grok",
        crate::types::BackendId::Agy => "agy",
        crate::types::BackendId::Cursor => "cursor",
        crate::types::BackendId::Codex => "codex",
        crate::types::BackendId::Pi => "openrouter",
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::config::accounts::{AccountConfig, AuthKind, PoolConfig};
    use crate::types::BackendId;

    fn test_accounts(max_concurrent: u32) -> AccountsConfig {
        AccountsConfig {
            pools: [(
                "claude".to_string(),
                PoolConfig {
                    max_concurrent,
                    accounts: vec![AccountConfig {
                        id: "claude-main".into(),
                        backend: BackendId::ClaudeCode,
                        auth: AuthKind::CliLogin,
                        policy: None,
                        env_key: None,
                    }],
                    health_ttl_seconds: Default::default(),
                },
            )]
            .into(),
        }
    }

    /// Cancels, timeouts and releases all racing on the same three slots. The waiter list
    /// double-decremented `queued` when a timeout met a release — wrapping the u32 — leaked the
    /// slot it had just handed on, and admitted more than `max_concurrent` because the check
    /// and the increment were two separate operations (B8).
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn pool_stress_keeps_counters_and_cap_honest() {
        const MAX: u32 = 3;
        let mgr = Arc::new(PoolManager::new(&test_accounts(MAX), 1, 512));
        let live = Arc::new(AtomicU32::new(0));
        let peak = Arc::new(AtomicU32::new(0));

        let mut tasks = Vec::new();
        for i in 0..400u32 {
            let mgr = mgr.clone();
            let live = live.clone();
            let peak = peak.clone();
            tasks.push(tokio::spawn(async move {
                let cancel = Arc::new(AtomicBool::new(false));
                if i % 5 == 0 {
                    let cancel = cancel.clone();
                    tokio::spawn(async move {
                        tokio::time::sleep(Duration::from_millis(u64::from(i % 17))).await;
                        cancel.store(true, Ordering::SeqCst);
                    });
                }
                let mut noop = |_| {};
                if let Ok(guard) = mgr.acquire("claude", &mut noop, Some(cancel)).await {
                    let now = live.fetch_add(1, Ordering::SeqCst) + 1;
                    peak.fetch_max(now, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_millis(20)).await;
                    live.fetch_sub(1, Ordering::SeqCst);
                    drop(guard);
                }
            }));
        }
        for task in tasks {
            task.await.expect("task");
        }

        let snap = mgr.snapshot("claude").expect("snap");
        assert_eq!(snap.in_use, 0, "every slot must come back");
        assert_eq!(snap.queued, 0, "the queue counter must come back to zero");
        assert!(
            peak.load(Ordering::SeqCst) <= MAX,
            "max_concurrent {MAX} exceeded: {} at once",
            peak.load(Ordering::SeqCst)
        );
    }

    #[tokio::test]
    async fn pool_acquire_release() {
        let mgr = PoolManager::new(&test_accounts(1), 5, 4);
        let mut events = Vec::new();
        let g1 = mgr
            .acquire("claude", &mut |e| events.push(e), None)
            .await
            .expect("first");
        let snap = mgr.snapshot("claude").expect("snap");
        assert_eq!(snap.in_use, 1);
        drop(g1);
        let snap = mgr.snapshot("claude").expect("snap");
        assert_eq!(snap.in_use, 0);
    }

    #[tokio::test]
    async fn pool_queue_timeout() {
        let mgr = PoolManager::new(&test_accounts(1), 1, 4);
        let mut noop = |_| {};
        let _g1 = mgr.acquire("claude", &mut noop, None).await.expect("hold");
        let err = mgr.acquire("claude", &mut noop, None).await;
        assert!(matches!(err, Err(TurnError::QueueTimeout(_))));
    }

    #[tokio::test]
    async fn pool_depth_refusal() {
        let mgr = PoolManager::new(&test_accounts(1), 30, 0);
        let mut noop = |_| {};
        let _g1 = mgr.acquire("claude", &mut noop, None).await.expect("hold");
        let err = mgr.acquire("claude", &mut noop, None).await;
        assert!(matches!(err, Err(TurnError::RateLimited(_))));
    }
}
