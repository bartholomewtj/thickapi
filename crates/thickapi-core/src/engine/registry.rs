use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use tokio::sync::broadcast;

use crate::engine::artifacts;
use crate::types::{
    BackendId, StoredEvent, TurnEvent, TurnId, TurnResult, TurnState, UsageBreakdown,
};

pub struct CancelToken {
    flag: Arc<AtomicBool>,
}

impl Default for CancelToken {
    fn default() -> Self {
        Self::new()
    }
}

impl CancelToken {
    pub fn new() -> Self {
        Self {
            flag: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn cancel(&self) {
        self.flag.store(true, Ordering::SeqCst);
    }

    pub fn is_cancelled(&self) -> bool {
        self.flag.load(Ordering::SeqCst)
    }

    pub fn shared(&self) -> Arc<AtomicBool> {
        self.flag.clone()
    }
}

/// How long a finished turn stays in memory for replay and status. After that the record is
/// dropped and the routes answer from `result.json` / `events.jsonl` on disk (D1).
pub const DEFAULT_TERMINAL_TTL: Duration = Duration::from_secs(60 * 60);

enum TurnPhase {
    Running,
    /// `TurnEnd` seen; `result` is filled by `finish` once the engine has built it.
    /// Until then status is derived from the event log (`result_from_events`).
    Terminal {
        result: Option<Box<TurnResult>>,
        since: Instant,
    },
}

struct TurnRecord {
    events: Vec<StoredEvent>,
    broadcast: broadcast::Sender<TurnEvent>,
    cancel: CancelToken,
    phase: TurnPhase,
}

pub struct TurnRegistry {
    turns: RwLock<HashMap<TurnId, TurnRecord>>,
    terminal_ttl: Duration,
}

impl Default for TurnRegistry {
    fn default() -> Self {
        Self::new()
    }
}

// Lock poison is stepped over throughout: every write leaves the map consistent, and the
// panic guard in `TurnEngine::run_turn` must be able to record a turn's end from an unwind.
impl TurnRegistry {
    pub fn new() -> Self {
        Self::with_terminal_ttl(DEFAULT_TERMINAL_TTL)
    }

    pub fn with_terminal_ttl(terminal_ttl: Duration) -> Self {
        Self {
            turns: RwLock::new(HashMap::new()),
            terminal_ttl,
        }
    }

    /// Drop finished turns older than the TTL. Running turns are never evicted. Returns how
    /// many records went. `register` calls this, so the map is bounded by the turns of the
    /// last TTL window rather than the life of the daemon (D1).
    pub fn evict_expired(&self) -> usize {
        let mut map = self
            .turns
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        Self::evict_expired_locked(&mut map, self.terminal_ttl)
    }

    fn evict_expired_locked(map: &mut HashMap<TurnId, TurnRecord>, ttl: Duration) -> usize {
        let before = map.len();
        map.retain(|_, record| match &record.phase {
            TurnPhase::Running => true,
            TurnPhase::Terminal { since, .. } => since.elapsed() < ttl,
        });
        before - map.len()
    }

    pub fn len(&self) -> usize {
        self.turns
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn register(
        &self,
        turn_id: TurnId,
    ) -> Option<(CancelToken, broadcast::Receiver<TurnEvent>)> {
        let mut map = self
            .turns
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        Self::evict_expired_locked(&mut map, self.terminal_ttl);
        if map.contains_key(&turn_id) {
            return None;
        }
        let (tx, rx) = broadcast::channel(256);
        let cancel = CancelToken::new();
        map.insert(
            turn_id,
            TurnRecord {
                events: Vec::new(),
                broadcast: tx,
                cancel: CancelToken {
                    flag: cancel.flag.clone(),
                },
                phase: TurnPhase::Running,
            },
        );
        Some((cancel, rx))
    }

    pub fn get_existing(&self, turn_id: &TurnId) -> Option<Vec<StoredEvent>> {
        self.turns
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(turn_id)
            .map(|r| r.events.clone())
    }

    pub fn subscribe(&self, turn_id: &TurnId) -> Option<broadcast::Receiver<TurnEvent>> {
        self.turns
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(turn_id)
            .map(|r| r.broadcast.subscribe())
    }

    pub fn cancel_token(&self, turn_id: &TurnId) -> Option<CancelToken> {
        self.turns
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(turn_id)
            .map(|r| CancelToken {
                flag: r.cancel.flag.clone(),
            })
    }

    pub fn emit(&self, data_dir: &Path, turn_id: &TurnId, event: TurnEvent) -> Option<u64> {
        let mut map = self
            .turns
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let record = map.get_mut(turn_id)?;
        let seq = record.events.len() as u64 + 1;
        let stored = StoredEvent {
            seq,
            event: event.clone(),
        };
        record.events.push(stored);
        if let Ok(dir) = artifacts::ensure_turn_dir(data_dir, turn_id) {
            match &event {
                TurnEvent::AssistantDelta { .. } => {}
                TurnEvent::TurnEnd { .. } => {
                    let _ = artifacts::write_events_jsonl(&dir, &record.events);
                }
                _ => {
                    let _ = artifacts::append_event_jsonl(&dir, seq, &event);
                }
            }
        }
        // `finish` may already have stored the engine's result (it runs before the `TurnEnd`
        // emit on most exit paths); do not replace it with the poorer event-derived one.
        if matches!(event, TurnEvent::TurnEnd { .. }) && matches!(record.phase, TurnPhase::Running)
        {
            record.phase = TurnPhase::Terminal {
                result: None,
                since: Instant::now(),
            };
        }
        let _ = record.broadcast.send(event);
        Some(seq)
    }

    pub fn finish(&self, turn_id: &TurnId, result: TurnResult) {
        let mut map = self
            .turns
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(record) = map.get_mut(turn_id) {
            let since = match &record.phase {
                TurnPhase::Terminal { since, .. } => *since,
                TurnPhase::Running => Instant::now(),
            };
            record.phase = TurnPhase::Terminal {
                result: Some(Box::new(result)),
                since,
            };
        }
    }

    pub fn last_seq(&self, turn_id: &TurnId) -> Option<u64> {
        self.turns
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(turn_id)
            .and_then(|r| r.events.last().map(|e| e.seq))
    }

    pub fn status(&self, turn_id: &TurnId) -> Option<TurnStatus> {
        let map = self
            .turns
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let record = map.get(turn_id)?;
        match &record.phase {
            TurnPhase::Running => Some(TurnStatus::Running),
            TurnPhase::Terminal {
                result: Some(r), ..
            } => Some(TurnStatus::Finished(Box::new(r.as_ref().clone()))),
            TurnPhase::Terminal { .. } => {
                if let Some(result) = result_from_events(turn_id, &record.events) {
                    Some(TurnStatus::Finished(Box::new(result)))
                } else {
                    Some(TurnStatus::Running)
                }
            }
        }
    }

    pub fn is_terminal(&self, turn_id: &TurnId) -> bool {
        matches!(
            self.status(turn_id),
            Some(TurnStatus::Finished(_))
                | Some(TurnStatus::RunningWithState(TurnState::Cancelled))
        )
    }

    pub fn events_since(&self, turn_id: &TurnId, since: u64) -> Option<Vec<StoredEvent>> {
        self.turns
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(turn_id)
            .map(|r| r.events.iter().filter(|e| e.seq > since).cloned().collect())
    }

    pub fn active_count(&self) -> usize {
        self.turns
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .values()
            .filter(|r| matches!(r.phase, TurnPhase::Running))
            .count()
    }

    pub fn cancel_all_active(&self) {
        let map = self
            .turns
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        for record in map.values() {
            record.cancel.cancel();
        }
    }
}

#[derive(Debug, Clone)]
pub enum TurnStatus {
    Running,
    RunningWithState(TurnState),
    /// Boxed: `TurnResult` carries the turn's full text and usage, far larger than the other
    /// variants (`clippy::large_enum_variant`).
    Finished(Box<TurnResult>),
}

fn result_from_events(turn_id: &str, events: &[StoredEvent]) -> Option<TurnResult> {
    let mut session_id = String::new();
    let mut backend = BackendId::ClaudeCode;
    let mut account_id = String::new();
    let mut resolved_model = String::new();
    let mut turn_end = None;

    for stored in events {
        match &stored.event {
            TurnEvent::TurnStart {
                session_id: sid,
                backend: b,
                account_id: aid,
                model,
                ..
            } => {
                session_id = sid.clone();
                backend = *b;
                account_id = aid.clone();
                resolved_model = model.clone();
            }
            TurnEvent::TurnEnd {
                state,
                returncode,
                text,
                breaches,
                ..
            } => {
                let effective_state = if breaches.is_empty() {
                    *state
                } else {
                    TurnState::Failed
                };
                turn_end = Some((effective_state, *returncode, text.clone()));
            }
            _ => {}
        }
    }

    let (state, returncode, text) = turn_end?;
    Some(TurnResult {
        turn_id: turn_id.to_string(),
        session_id,
        state,
        returncode,
        text,
        tokens: 0,
        cost: 0.0,
        context_tokens: 0,
        usage: UsageBreakdown::default(),
        context_window: 0,
        backend,
        account_id,
        resolved_model,
        error_code: None,
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::types::PermissionBreach;

    #[test]
    fn status_recovers_finished_from_turn_end_without_finish() {
        let registry = TurnRegistry::new();
        let turn_id = "t-recover".to_string();
        registry.register(turn_id.clone()).expect("register");
        registry.emit(
            std::path::Path::new("/tmp"),
            &turn_id,
            TurnEvent::TurnStart {
                turn_id: turn_id.clone(),
                session_id: "sess".into(),
                backend: BackendId::ClaudeCode,
                account_id: "claude-main".into(),
                model: "subs/claude/sonnet".into(),
            },
        );
        registry.emit(
            std::path::Path::new("/tmp"),
            &turn_id,
            TurnEvent::TurnEnd {
                state: TurnState::RateLimited,
                returncode: -1,
                text: "pool full".into(),
                retry_after_seconds: Some(60),
                touched_paths: vec![],
                breaches: vec![],
            },
        );
        let status = registry.status(&turn_id).expect("status");
        assert!(matches!(status, TurnStatus::Finished(r) if r.state == TurnState::RateLimited));
    }

    fn turn_end(text: &str) -> TurnEvent {
        TurnEvent::TurnEnd {
            state: TurnState::Success,
            returncode: 0,
            text: text.into(),
            retry_after_seconds: None,
            touched_paths: vec![],
            breaches: vec![],
        }
    }

    #[test]
    fn finished_turns_are_evicted_after_the_ttl_and_running_ones_are_kept() {
        let temp = tempfile::tempdir().expect("tempdir");
        let registry = TurnRegistry::with_terminal_ttl(Duration::from_millis(40));
        registry.register("t-done".to_string()).expect("register");
        registry.register("t-live".to_string()).expect("register");
        registry.emit(temp.path(), &"t-done".to_string(), turn_end("done"));

        assert_eq!(registry.evict_expired(), 0, "inside the TTL nothing goes");
        assert_eq!(registry.len(), 2);

        std::thread::sleep(Duration::from_millis(80));
        // `register` sweeps, so a busy daemon needs no timer to stay bounded.
        registry.register("t-next".to_string()).expect("register");
        assert!(registry.status(&"t-done".to_string()).is_none());
        assert!(matches!(
            registry.status(&"t-live".to_string()),
            Some(TurnStatus::Running)
        ));
        assert_eq!(registry.len(), 2);
    }

    #[test]
    fn turn_end_after_finish_keeps_the_engine_result() {
        let temp = tempfile::tempdir().expect("tempdir");
        let registry = TurnRegistry::new();
        let turn_id = "t-order".to_string();
        registry.register(turn_id.clone()).expect("register");
        let mut result = result_from_events(
            &turn_id,
            &[StoredEvent {
                seq: 1,
                event: turn_end("hi"),
            }],
        )
        .expect("result");
        result.tokens = 42;
        registry.finish(&turn_id, result);
        registry.emit(temp.path(), &turn_id, turn_end("hi"));
        assert!(matches!(
            registry.status(&turn_id),
            Some(TurnStatus::Finished(r)) if r.tokens == 42
        ));
    }

    #[test]
    fn result_from_events_marks_breach_as_failed() {
        let events = vec![
            StoredEvent {
                seq: 1,
                event: TurnEvent::TurnStart {
                    turn_id: "t1".into(),
                    session_id: "sess".into(),
                    backend: BackendId::ClaudeCode,
                    account_id: "claude-main".into(),
                    model: "subs/claude/sonnet".into(),
                },
            },
            StoredEvent {
                seq: 2,
                event: TurnEvent::TurnEnd {
                    state: TurnState::Success,
                    returncode: 0,
                    text: "oops".into(),
                    retry_after_seconds: None,
                    touched_paths: vec![],
                    breaches: vec![PermissionBreach {
                        path: "blocked.txt".into(),
                        message: "denied".into(),
                    }],
                },
            },
        ];
        let result = result_from_events("t1", &events).expect("result");
        assert_eq!(result.state, TurnState::Failed);
    }
}
