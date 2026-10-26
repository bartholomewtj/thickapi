use std::path::Path;
use std::sync::Mutex;

use anyhow::{Context, Result};
use chrono::Utc;
use rusqlite::Connection;

use crate::adapters::SessionBinding;
use crate::process::{
    kill_process_tree, process_alive, process_start_time, same_process, ORPHAN_PID_KILL,
};
use crate::types::BackendId;

#[derive(Debug, Clone)]
pub struct OpenProcess {
    pub turn_id: String,
    pub pid: u32,
    pub backend: String,
    /// The process that spawned it (a `serve` daemon or a `thickapi turn`); `None` on rows
    /// written before R11.
    pub owner_pid: Option<u32>,
    pub owner_started: Option<u64>,
    pub pid_started: Option<u64>,
}

#[derive(Debug, Clone)]
pub struct AccountHealthRow {
    pub account_id: String,
    pub status: String,
    pub updated_at: String,
}

pub fn account_health_is_blocking(status: &str) -> bool {
    status != "ok"
}

/// Whether a recorded account-health row keeps the account out of routing right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccountBlock {
    /// `ok`, or a block whose cooldown has passed: the next turn is the probe.
    Clear,
    /// Blocked for this many more seconds.
    For(u64),
    /// Blocked until cleared by hand (TTL `0`).
    UntilCleared,
}

impl AccountBlock {
    pub fn is_blocking(self) -> bool {
        self != AccountBlock::Clear
    }

    pub fn retry_in_seconds(self) -> Option<u64> {
        match self {
            AccountBlock::For(seconds) => Some(seconds),
            AccountBlock::Clear | AccountBlock::UntilCleared => None,
        }
    }
}

/// A non-`ok` status used to block forever, so one rate limit took a thick pool out until
/// someone cleared it by hand — and a blocked account never gets the turn that would record
/// `ok` again. The status now lapses `ttl_seconds` after `updated_at` (C12). A timestamp
/// that does not parse counts as lapsed: the probe turn rewrites the row.
pub fn account_block(
    row: &AccountHealthRow,
    ttl_seconds: u64,
    now: chrono::DateTime<Utc>,
) -> AccountBlock {
    if !account_health_is_blocking(&row.status) {
        return AccountBlock::Clear;
    }
    if ttl_seconds == 0 {
        return AccountBlock::UntilCleared;
    }
    let Ok(updated) = chrono::DateTime::parse_from_rfc3339(&row.updated_at) else {
        return AccountBlock::Clear;
    };
    let age = now
        .signed_duration_since(updated.with_timezone(&Utc))
        .num_seconds();
    match u64::try_from(age) {
        // Written in the future (clock stepped back): the full cooldown still applies.
        Err(_) => AccountBlock::For(ttl_seconds),
        Ok(age) if age >= ttl_seconds => AccountBlock::Clear,
        Ok(age) => AccountBlock::For(ttl_seconds - age),
    }
}

const SCHEMA: &str = include_str!("schema.sql");

/// Columns added after the first release; `CREATE TABLE IF NOT EXISTS` does not add them to
/// an existing database.
fn migrate(conn: &Connection) -> Result<()> {
    let mut stmt = conn.prepare("PRAGMA table_info(processes)")?;
    let names = stmt
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    for column in ["owner_pid", "owner_started", "pid_started"] {
        if !names.iter().any(|n| n == column) {
            conn.execute_batch(&format!(
                "ALTER TABLE processes ADD COLUMN {column} INTEGER"
            ))?;
        }
    }
    Ok(())
}

pub struct Store {
    conn: Mutex<Connection>,
}

impl Store {
    pub fn open(db_path: &Path) -> Result<Self> {
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create db parent {}", parent.display()))?;
        }

        let conn = Connection::open(db_path)
            .with_context(|| format!("open database {}", db_path.display()))?;
        conn.busy_timeout(std::time::Duration::from_millis(5000))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.execute_batch(SCHEMA)
            .context("apply database schema")?;
        migrate(&conn).context("migrate database schema")?;

        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory().context("open in-memory database")?;
        conn.busy_timeout(std::time::Duration::from_millis(5000))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.execute_batch(SCHEMA)
            .context("apply database schema")?;
        migrate(&conn).context("migrate database schema")?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn ping(&self) -> Result<()> {
        let conn = self.lock();
        conn.query_row("SELECT 1", [], |_| Ok(()))
            .context("database ping failed")?;
        Ok(())
    }

    pub fn table_exists(&self, name: &str) -> Result<bool> {
        let conn = self.lock();
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name=?1",
            [name],
            |row| row.get(0),
        )?;
        Ok(count > 0)
    }

    pub fn journal_mode(&self) -> Result<String> {
        let conn = self.lock();
        conn.query_row("PRAGMA journal_mode", [], |row| row.get(0))
            .context("read journal_mode")
    }

    pub fn get_session_binding(&self, session_id: &str) -> Result<Option<SessionBinding>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT session_id, account_id, backend, cli_session, cwd FROM session_bindings WHERE session_id = ?1",
        )?;
        let mut rows = stmt.query([session_id])?;
        if let Some(row) = rows.next()? {
            let backend_str: String = row.get(2)?;
            let backend = match backend_str.as_str() {
                "claude_code" => BackendId::ClaudeCode,
                "grok" => BackendId::Grok,
                "agy" => BackendId::Agy,
                "cursor" => BackendId::Cursor,
                "codex" => BackendId::Codex,
                "pi" => BackendId::Pi,
                _ => return Ok(None),
            };
            return Ok(Some(SessionBinding {
                session_id: row.get(0)?,
                account_id: row.get(1)?,
                backend,
                cli_session: row.get(3)?,
                cwd: std::path::PathBuf::from(row.get::<_, String>(4)?),
            }));
        }
        Ok(None)
    }

    /// Drop a stored CLI resume id. The session keeps its account and cwd; the next turn
    /// starts a fresh CLI conversation (`doctor clear-session`, or an unknown-session retry).
    pub fn clear_cli_session(&self, session_id: &str) -> Result<bool> {
        let conn = self.lock();
        let now = Utc::now().to_rfc3339();
        let updated = conn.execute(
            "UPDATE session_bindings SET cli_session = NULL, updated_at = ?1 WHERE session_id = ?2",
            rusqlite::params![now, session_id],
        )?;
        Ok(updated > 0)
    }

    pub fn upsert_session_binding(&self, binding: &SessionBinding) -> Result<()> {
        let conn = self.lock();
        let backend = match binding.backend {
            BackendId::ClaudeCode => "claude_code",
            BackendId::Grok => "grok",
            BackendId::Agy => "agy",
            BackendId::Cursor => "cursor",
            BackendId::Codex => "codex",
            BackendId::Pi => "pi",
        };
        let now = Utc::now().to_rfc3339();
        conn.execute(
            "INSERT INTO session_bindings (session_id, account_id, backend, cli_session, cwd, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(session_id) DO UPDATE SET
               account_id = excluded.account_id,
               backend = excluded.backend,
               cli_session = excluded.cli_session,
               cwd = excluded.cwd,
               updated_at = excluded.updated_at",
            rusqlite::params![
                binding.session_id,
                binding.account_id,
                backend,
                binding.cli_session,
                binding.cwd.to_string_lossy().to_string(),
                now,
            ],
        )?;
        Ok(())
    }

    /// Record a spawned child with its identity: the owner (this process) and both start
    /// stamps, so a later `reconcile_orphan_processes` never acts on a reused pid (R11).
    pub fn register_process(&self, turn_id: &str, pid: u32, backend: BackendId) -> Result<()> {
        let conn = self.lock();
        let backend_str = match backend {
            BackendId::ClaudeCode => "claude_code",
            BackendId::Grok => "grok",
            BackendId::Agy => "agy",
            BackendId::Cursor => "cursor",
            BackendId::Codex => "codex",
            BackendId::Pi => "pi",
        };
        let now = Utc::now().to_rfc3339();
        let owner = std::process::id();
        let owner_started = process_start_time(owner).map(|v| v as i64);
        let pid_started = process_start_time(pid).map(|v| v as i64);
        conn.execute(
            "INSERT INTO processes (turn_id, pid, backend, started_at, ended_at, owner_pid, owner_started, pid_started) VALUES (?1, ?2, ?3, ?4, NULL, ?5, ?6, ?7)",
            rusqlite::params![turn_id, pid, backend_str, now, owner, owner_started, pid_started],
        )?;
        Ok(())
    }

    pub fn finish_process(&self, turn_id: &str) -> Result<()> {
        let conn = self.lock();
        let now = Utc::now().to_rfc3339();
        conn.execute(
            "UPDATE processes SET ended_at = ?1 WHERE turn_id = ?2 AND ended_at IS NULL",
            rusqlite::params![now, turn_id],
        )?;
        Ok(())
    }

    pub fn open_processes(&self) -> Result<Vec<OpenProcess>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT turn_id, pid, backend, owner_pid, owner_started, pid_started FROM processes WHERE ended_at IS NULL",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(OpenProcess {
                turn_id: row.get(0)?,
                pid: row.get::<_, i64>(1)? as u32,
                backend: row.get(2)?,
                owner_pid: row.get::<_, Option<i64>>(3)?.map(|v| v as u32),
                owner_started: row.get::<_, Option<i64>>(4)?.map(|v| v as u64),
                pid_started: row.get::<_, Option<i64>>(5)?.map(|v| v as u64),
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// Close the open `processes` rows a dead owner left behind (`serve` start). Returns the
    /// number of rows closed.
    ///
    /// A row whose owner is still alive (another daemon, or a `thickapi turn` on the same
    /// database) is left alone. A stale row is closed; its pid is killed only where
    /// [`ORPHAN_PID_KILL`] holds (not Windows: the job object already took the tree with its
    /// owner) and only when the pid still carries the recorded start stamp. A row without
    /// identity (written before R11) is closed and never killed (R11, A4).
    pub fn reconcile_orphan_processes(&self) -> Result<u32> {
        let open = self.open_processes()?;
        let me = std::process::id();
        let mut count = 0u32;
        for proc in open {
            if let Some(owner) = proc.owner_pid {
                // `owner == me`: only an earlier process that had our pid wrote this row.
                let owner_live = owner != me
                    && match proc.owner_started {
                        Some(_) => same_process(owner, proc.owner_started),
                        None => process_alive(owner),
                    };
                if owner_live {
                    continue;
                }
            }
            if ORPHAN_PID_KILL && same_process(proc.pid, proc.pid_started) {
                kill_process_tree(proc.pid);
            }
            self.finish_process(&proc.turn_id)?;
            count += 1;
        }
        Ok(count)
    }

    pub fn upsert_account_health(&self, account_id: &str, status: &str) -> Result<()> {
        let conn = self.lock();
        let now = Utc::now().to_rfc3339();
        conn.execute(
            "INSERT INTO account_health (account_id, status, updated_at) VALUES (?1, ?2, ?3)
             ON CONFLICT(account_id) DO UPDATE SET status = excluded.status, updated_at = excluded.updated_at",
            rusqlite::params![account_id, status, now],
        )?;
        Ok(())
    }

    pub fn get_account_health(&self, account_id: &str) -> Result<Option<String>> {
        let conn = self.lock();
        let mut stmt = conn.prepare("SELECT status FROM account_health WHERE account_id = ?1")?;
        let mut rows = stmt.query([account_id])?;
        if let Some(row) = rows.next()? {
            return Ok(Some(row.get(0)?));
        }
        Ok(None)
    }

    pub fn get_account_health_row(&self, account_id: &str) -> Result<Option<AccountHealthRow>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT account_id, status, updated_at FROM account_health WHERE account_id = ?1",
        )?;
        let mut rows = stmt.query([account_id])?;
        if let Some(row) = rows.next()? {
            return Ok(Some(AccountHealthRow {
                account_id: row.get(0)?,
                status: row.get(1)?,
                updated_at: row.get(2)?,
            }));
        }
        Ok(None)
    }

    /// Record a status as of `updated_at` (RFC 3339) — for tests and tooling that need a row
    /// of a known age.
    pub fn upsert_account_health_at(
        &self,
        account_id: &str,
        status: &str,
        updated_at: &str,
    ) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "INSERT INTO account_health (account_id, status, updated_at) VALUES (?1, ?2, ?3)
             ON CONFLICT(account_id) DO UPDATE SET status = excluded.status, updated_at = excluded.updated_at",
            rusqlite::params![account_id, status, updated_at],
        )?;
        Ok(())
    }

    pub fn list_account_health(&self) -> Result<Vec<AccountHealthRow>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT account_id, status, updated_at FROM account_health ORDER BY account_id",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(AccountHealthRow {
                account_id: row.get(0)?,
                status: row.get(1)?,
                updated_at: row.get(2)?,
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    pub fn clear_account_health(&self, account_id: &str) -> Result<bool> {
        let conn = self.lock();
        let now = Utc::now().to_rfc3339();
        let updated = conn.execute(
            "INSERT INTO account_health (account_id, status, updated_at) VALUES (?1, 'ok', ?2)
             ON CONFLICT(account_id) DO UPDATE SET status = 'ok', updated_at = excluded.updated_at",
            rusqlite::params![account_id, now],
        )?;
        Ok(updated > 0)
    }

    fn utc_day() -> String {
        Utc::now().format("%Y-%m-%d").to_string()
    }

    pub fn paid_spend_today(&self) -> Result<f64> {
        let conn = self.lock();
        let day = Self::utc_day();
        let usd: f64 = conn
            .query_row("SELECT usd FROM paid_daily WHERE day = ?1", [day], |row| {
                row.get(0)
            })
            .unwrap_or(0.0);
        Ok(usd)
    }

    pub fn add_paid_spend(&self, delta: f64) -> Result<f64> {
        if delta <= 0.0 {
            return self.paid_spend_today();
        }
        let conn = self.lock();
        let day = Self::utc_day();
        conn.execute(
            "INSERT INTO paid_daily (day, usd) VALUES (?1, ?2)
             ON CONFLICT(day) DO UPDATE SET usd = paid_daily.usd + excluded.usd",
            rusqlite::params![day, delta],
        )?;
        let total: f64 =
            conn.query_row("SELECT usd FROM paid_daily WHERE day = ?1", [day], |row| {
                row.get(0)
            })?;
        Ok(total)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    #[test]
    fn store_opens_wal_schema() {
        let temp = tempfile::tempdir().expect("tempdir");
        let db_path = temp.path().join("thickapi.db");
        let store = Store::open(&db_path).expect("open");
        for table in [
            "session_bindings",
            "processes",
            "account_health",
            "paid_daily",
        ] {
            assert!(store.table_exists(table).expect("check table"));
        }
        assert!(
            !store.table_exists("events").expect("check table"),
            "unused events table must not be created"
        );
        let mode: String = store.journal_mode().expect("journal mode");
        assert_eq!(mode.to_lowercase(), "wal");
    }

    #[test]
    fn store_ping() {
        let store = Store::open_in_memory().expect("open");
        store.ping().expect("ping");
    }

    #[test]
    fn process_register_finish() {
        let store = Store::open_in_memory().expect("open");
        store
            .register_process("t1", 1234, BackendId::ClaudeCode)
            .expect("register");
        let open = store.open_processes().expect("open");
        assert_eq!(open.len(), 1);
        assert_eq!(open[0].turn_id, "t1");
        store.finish_process("t1").expect("finish");
        assert!(store.open_processes().expect("open").is_empty());
    }

    #[test]
    fn process_row_records_owner_identity() {
        let store = Store::open_in_memory().expect("open");
        let me = std::process::id();
        store
            .register_process("t1", me, BackendId::ClaudeCode)
            .expect("register");
        let open = store.open_processes().expect("open");
        assert_eq!(open[0].owner_pid, Some(me));
        assert!(open[0].owner_started.is_some());
        assert_eq!(open[0].pid_started, open[0].owner_started);
    }

    #[test]
    fn migrate_adds_identity_columns_to_old_database() {
        let temp = tempfile::tempdir().expect("tempdir");
        let db_path = temp.path().join("thickapi.db");
        {
            let conn = Connection::open(&db_path).expect("open raw");
            conn.execute_batch(
                "CREATE TABLE processes (id INTEGER PRIMARY KEY, turn_id TEXT, pid INTEGER, backend TEXT, started_at TEXT, ended_at TEXT);
                 INSERT INTO processes (turn_id, pid, backend, started_at) VALUES ('old', 4242, 'grok', 'x');",
            )
            .expect("old schema");
        }
        let store = Store::open(&db_path).expect("open migrates");
        let open = store.open_processes().expect("open");
        assert_eq!(open.len(), 1);
        assert_eq!(open[0].owner_pid, None);
        // Legacy row: closed, never killed.
        assert_eq!(store.reconcile_orphan_processes().expect("reconcile"), 1);
        assert!(store.open_processes().expect("open").is_empty());
    }

    fn spawn_sleeper() -> std::process::Child {
        #[cfg(windows)]
        let mut cmd = {
            let mut c = std::process::Command::new("ping");
            c.args(["-n", "30", "127.0.0.1"]);
            c
        };
        #[cfg(not(windows))]
        let mut cmd = {
            let mut c = std::process::Command::new("sleep");
            c.arg("30");
            c
        };
        cmd.stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn sleeper")
    }

    fn insert_row(
        store: &Store,
        turn_id: &str,
        pid: u32,
        owner: (u32, Option<u64>),
        pid_started: Option<u64>,
    ) {
        let conn = store.lock();
        conn.execute(
            "INSERT INTO processes (turn_id, pid, backend, started_at, ended_at, owner_pid, owner_started, pid_started) VALUES (?1, ?2, 'claude_code', 'x', NULL, ?3, ?4, ?5)",
            rusqlite::params![
                turn_id,
                pid,
                owner.0,
                owner.1.map(|v| v as i64),
                pid_started.map(|v| v as i64)
            ],
        )
        .expect("insert");
    }

    /// R11: a stale row whose pid now belongs to an unrelated live process must not kill it.
    #[test]
    fn reconcile_spares_unrelated_process_on_reused_pid() {
        let mut child = spawn_sleeper();
        let pid = child.id();
        let store = Store::open_in_memory().expect("open");
        // Dead owner (pid 0), and a start stamp that is not the live process's.
        let wrong = process_start_time(pid).map(|s| s.wrapping_add(1));
        insert_row(&store, "stale", pid, (0, Some(1)), wrong);
        assert_eq!(store.reconcile_orphan_processes().expect("reconcile"), 1);
        assert!(process_alive(pid), "unrelated process was killed");
        assert!(store.open_processes().expect("open").is_empty());
        let _ = child.kill();
        let _ = child.wait();
    }

    /// R11: a row owned by a live process (e.g. `thickapi turn`) stays open, its child alive.
    #[test]
    fn reconcile_leaves_rows_of_a_live_owner() {
        let mut owner = spawn_sleeper();
        let owner_pid = owner.id();
        let owner_started = process_start_time(owner_pid);
        let store = Store::open_in_memory().expect("open");
        insert_row(
            &store,
            "live",
            owner_pid,
            (owner_pid, owner_started),
            owner_started,
        );
        assert_eq!(store.reconcile_orphan_processes().expect("reconcile"), 0);
        assert_eq!(store.open_processes().expect("open").len(), 1);
        assert!(process_alive(owner_pid));
        let _ = owner.kill();
        let _ = owner.wait();
    }

    /// On Windows the job object owns child lifetime: even a matching stale child is not
    /// killed by pid. Elsewhere a matching stamp is killed.
    #[test]
    fn reconcile_kills_matching_orphan_only_off_windows() {
        let mut child = spawn_sleeper();
        let pid = child.id();
        let store = Store::open_in_memory().expect("open");
        insert_row(&store, "orphan", pid, (0, Some(1)), process_start_time(pid));
        assert_eq!(store.reconcile_orphan_processes().expect("reconcile"), 1);
        if ORPHAN_PID_KILL {
            let _ = child.wait();
        } else {
            assert!(
                process_alive(pid),
                "Windows must not taskkill a recorded pid"
            );
            let _ = child.kill();
            let _ = child.wait();
        }
    }

    #[test]
    fn account_health_upsert_list() {
        let store = Store::open_in_memory().expect("open");
        store
            .upsert_account_health("claude-main", "ok")
            .expect("upsert");
        store
            .upsert_account_health("claude-main", "rate_limited")
            .expect("upsert");
        let status = store
            .get_account_health("claude-main")
            .expect("get")
            .expect("row");
        assert_eq!(status, "rate_limited");
        let rows = store.list_account_health().expect("list");
        assert_eq!(rows.len(), 1);
        assert!(account_health_is_blocking(&rows[0].status));
    }

    #[test]
    fn paid_daily_accumulates_and_resets_by_day() {
        let store = Store::open_in_memory().expect("open");
        assert_eq!(store.paid_spend_today().expect("today"), 0.0);
        let total = store.add_paid_spend(0.25).expect("add");
        assert!((total - 0.25).abs() < f64::EPSILON);
        let total = store.add_paid_spend(0.10).expect("add");
        assert!((total - 0.35).abs() < f64::EPSILON);
        assert!((store.paid_spend_today().expect("today") - 0.35).abs() < f64::EPSILON);
    }

    #[test]
    fn account_block_lapses_after_the_ttl() {
        let now = Utc::now();
        let row = |status: &str, age_seconds: i64| AccountHealthRow {
            account_id: "a".into(),
            status: status.into(),
            updated_at: (now - chrono::Duration::seconds(age_seconds)).to_rfc3339(),
        };
        assert_eq!(account_block(&row("ok", 0), 900, now), AccountBlock::Clear);
        assert_eq!(
            account_block(&row("rate_limited", 100), 900, now),
            AccountBlock::For(800)
        );
        assert_eq!(
            account_block(&row("rate_limited", 900), 900, now),
            AccountBlock::Clear
        );
        assert_eq!(
            account_block(&row("rate_limited", 99_999), 0, now),
            AccountBlock::UntilCleared
        );
        assert_eq!(
            account_block(&row("rate_limited", -50), 900, now),
            AccountBlock::For(900)
        );
        let garbled = AccountHealthRow {
            account_id: "a".into(),
            status: "rate_limited".into(),
            updated_at: "yesterday".into(),
        };
        assert_eq!(account_block(&garbled, 900, now), AccountBlock::Clear);
    }

    #[test]
    fn clear_account_health_sets_ok() {
        let store = Store::open_in_memory().expect("open");
        store
            .upsert_account_health("or-main", "suspected_blocked")
            .expect("upsert");
        store.clear_account_health("or-main").expect("clear");
        let status = store
            .get_account_health("or-main")
            .expect("get")
            .expect("row");
        assert_eq!(status, "ok");
    }

    #[test]
    fn clear_cli_session_drops_the_resume_id_and_keeps_the_binding() {
        let store = Store::open_in_memory().expect("open");
        let binding = SessionBinding {
            session_id: "sess-1".into(),
            account_id: "grok-main".into(),
            backend: BackendId::Grok,
            cli_session: Some("cli-abc".into()),
            cwd: std::path::PathBuf::from("."),
        };
        store.upsert_session_binding(&binding).expect("bind");
        assert!(store.clear_cli_session("sess-1").expect("clear"));
        let back = store
            .get_session_binding("sess-1")
            .expect("get")
            .expect("row");
        assert!(back.cli_session.is_none());
        assert_eq!(back.account_id, "grok-main");
        assert!(!store.clear_cli_session("missing").expect("missing"));
    }
}
