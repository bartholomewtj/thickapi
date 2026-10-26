use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};

use thiserror::Error;

use crate::process::{process_alive, process_start_time, same_process};

/// A live pid created more than this long after the lock's `started=` time is a reuse, not the
/// holder. The holder writes `started=` a moment after process creation.
#[cfg(windows)]
const REUSE_SLACK_SECS: i64 = 120;

pub const LOCK_FILE_NAME: &str = ".thickapi.lock";

#[derive(Debug, Error)]
pub enum LockError {
    #[error("another thickapi instance is already running (lock: {0})")]
    AlreadyHeld(PathBuf),
    #[error("lock file I/O failed at {path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
}

pub struct InstanceLock {
    path: PathBuf,
    _file: File,
}

impl InstanceLock {
    pub fn acquire(data_dir: &Path) -> std::result::Result<Self, LockError> {
        if let Err(err) = fs::create_dir_all(data_dir) {
            return Err(LockError::Io {
                path: data_dir.to_path_buf(),
                source: err,
            });
        }

        let path = data_dir.join(LOCK_FILE_NAME);
        let mut file = match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(file) => file,
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
                // A hard stop (Task Scheduler idle-end, `TerminateProcess`) skips `Drop`, so the
                // file outlives its holder. A later process can reuse that pid; pid-only liveness
                // then refuses every auto-start.
                if !holder_still_running(&path) {
                    let _ = fs::remove_file(&path);
                    return Self::acquire(data_dir);
                }
                return Err(LockError::AlreadyHeld(path));
            }
            Err(source) => {
                return Err(LockError::Io { path, source });
            }
        };

        let pid = std::process::id();
        let mut body = format!("pid={pid}\nstarted={}\n", chrono::Utc::now().to_rfc3339());
        if let Some(ticks) = process_start_time(pid) {
            body.push_str(&format!("start_ticks={ticks}\n"));
        }
        if let Err(source) = file.write_all(body.as_bytes()) {
            return Err(LockError::Io { path, source });
        }

        Ok(Self { path, _file: file })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for InstanceLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

/// The recorded holder is still this process. `start_ticks` (process start stamp, R11) wins.
/// An older file has only `pid` and RFC3339 `started=`: a dead pid is stale, and on Windows a
/// live pid created long after `started=` reused the number.
fn holder_still_running(path: &Path) -> bool {
    let Some(text) = fs::read_to_string(path).ok() else {
        return true;
    };
    let Some(pid) = prefixed_u32(&text, "pid=") else {
        return true;
    };
    if let Some(ticks) = prefixed_u64(&text, "start_ticks=") {
        return same_process(pid, Some(ticks));
    }
    if !process_alive(pid) {
        return false;
    }
    #[cfg(windows)]
    if let Some(recorded) = prefixed_time(&text, "started=") {
        if windows_pid_reused(pid, recorded) {
            return false;
        }
    }
    true
}

fn prefixed_u32(text: &str, prefix: &str) -> Option<u32> {
    prefixed(text, prefix)?.parse().ok()
}

fn prefixed_u64(text: &str, prefix: &str) -> Option<u64> {
    prefixed(text, prefix)?.parse().ok()
}

fn prefixed<'a>(text: &'a str, prefix: &str) -> Option<&'a str> {
    text.lines()
        .find_map(|line| line.strip_prefix(prefix))
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

#[cfg(windows)]
fn prefixed_time(text: &str, prefix: &str) -> Option<chrono::DateTime<chrono::Utc>> {
    chrono::DateTime::parse_from_rfc3339(prefixed(text, prefix)?)
        .ok()
        .map(|dt| dt.with_timezone(&chrono::Utc))
}

/// `process_start_time` is a Windows `FILETIME`. A creation time well after the lock's
/// wall-clock `started=` means that pid is a later process.
#[cfg(windows)]
fn windows_pid_reused(pid: u32, recorded: chrono::DateTime<chrono::Utc>) -> bool {
    let Some(created) = process_start_time(pid).and_then(filetime_to_utc) else {
        return false;
    };
    created > recorded + chrono::Duration::seconds(REUSE_SLACK_SECS)
}

#[cfg(windows)]
fn filetime_to_utc(ticks: u64) -> Option<chrono::DateTime<chrono::Utc>> {
    const UNIX_EPOCH_TICKS: u64 = 11_644_473_600 * 10_000_000;
    let unix_100ns = ticks.checked_sub(UNIX_EPOCH_TICKS)?;
    let secs = i64::try_from(unix_100ns / 10_000_000).ok()?;
    let nanos = u32::try_from((unix_100ns % 10_000_000) * 100).ok()?;
    chrono::DateTime::from_timestamp(secs, nanos)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    #[test]
    fn lock_refuses_second_instance() {
        let temp = tempfile::tempdir().expect("tempdir");
        let first = InstanceLock::acquire(temp.path()).expect("first lock");
        let second = InstanceLock::acquire(temp.path());
        assert!(matches!(second, Err(LockError::AlreadyHeld(_))));
        drop(first);
        InstanceLock::acquire(temp.path()).expect("lock after release");
    }

    #[test]
    fn lock_removes_stale_pid() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join(LOCK_FILE_NAME);
        fs::write(&path, "pid=999999999\nstarted=2026-01-01T00:00:00Z\n").expect("write");
        InstanceLock::acquire(temp.path()).expect("stale lock removed");
    }

    #[test]
    fn lock_reclaims_when_start_ticks_name_another_process() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join(LOCK_FILE_NAME);
        let pid = std::process::id();
        let ticks = process_start_time(pid).expect("own start stamp");
        fs::write(
            &path,
            format!(
                "pid={pid}\nstarted=2020-01-01T00:00:00Z\nstart_ticks={}\n",
                ticks.wrapping_add(1)
            ),
        )
        .expect("write");
        InstanceLock::acquire(temp.path()).expect("reused pid reclaimed");
    }

    #[test]
    fn lock_keeps_a_live_pid_with_its_own_start_ticks() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join(LOCK_FILE_NAME);
        let pid = std::process::id();
        let ticks = process_start_time(pid).expect("own start stamp");
        fs::write(
            &path,
            format!(
                "pid={pid}\nstarted={}\nstart_ticks={ticks}\n",
                chrono::Utc::now().to_rfc3339()
            ),
        )
        .expect("write");
        let second = InstanceLock::acquire(temp.path());
        assert!(matches!(second, Err(LockError::AlreadyHeld(_))));
    }

    #[cfg(windows)]
    #[test]
    fn lock_reclaims_old_file_when_pid_was_reused() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join(LOCK_FILE_NAME);
        let pid = std::process::id();
        fs::write(&path, format!("pid={pid}\nstarted=2020-01-01T00:00:00Z\n")).expect("write");
        InstanceLock::acquire(temp.path()).expect("old lock with a reused pid reclaimed");
    }
}
