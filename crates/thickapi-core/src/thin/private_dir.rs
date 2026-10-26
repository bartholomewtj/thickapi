//! Private per-request directories under `~/.thickapi/tmp/<req>/` (SPEC §19.9).
//!
//! Each directory carries a `pid-marker.json` (daemon pid, child pid) so a crashed daemon's
//! leftovers can be swept by the next `serve`, by every request, and by `reap`. The scope guard
//! removes the directory on every exit path unless `keep()` was called (trace opt-in).

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;
use std::thread;
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};

use crate::process::{process_alive, process_start_time, same_process, ORPHAN_PID_KILL};

pub const MARKER_FILE: &str = "pid-marker.json";

#[derive(Debug, Default, Serialize, Deserialize)]
struct PidMarker {
    daemon_pid: u32,
    /// Start stamp of the daemon (`process_start_time`), so a reused pid does not read as
    /// the daemon still being alive (R11). Absent on markers written before R11.
    #[serde(default)]
    daemon_started: Option<u64>,
    #[serde(default)]
    child_pid: Option<u32>,
    #[serde(default)]
    child_started: Option<u64>,
    created_unix: u64,
    /// Trace opt-in (`PrivateDir::keep`): survive drop and dead-pid sweep; bounded by `trace_keep`.
    #[serde(default)]
    keep: bool,
}

/// Scope guard for one request's private directory.
#[derive(Debug)]
pub struct PrivateDir {
    path: PathBuf,
    keep: bool,
}

impl PrivateDir {
    /// Create `root/<request_id>`; fails if it already exists (never reuse a directory).
    pub fn create(root: &Path, request_id: &str) -> io::Result<Self> {
        fs::create_dir_all(root)?;
        secure_private_file(root).map_err(|e| {
            io::Error::other(format!("private dir ACL failed: {}: {e}", root.display()))
        })?;
        let path = root.join(sanitize(request_id));
        fs::create_dir(&path)?;
        if let Err(e) = secure_private_file(&path) {
            let _ = fs::remove_dir_all(&path);
            return Err(io::Error::other(format!(
                "private dir ACL failed: {}: {e}",
                path.display()
            )));
        }
        let me = std::process::id();
        let marker = PidMarker {
            daemon_pid: me,
            daemon_started: process_start_time(me),
            child_pid: None,
            child_started: None,
            created_unix: now_unix(),
            keep: false,
        };
        write_marker(&path, &marker)?;
        Ok(Self { path, keep: false })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn record_child(&self, pid: u32) -> io::Result<()> {
        let mut marker = read_marker(&self.path)?;
        marker.child_pid = Some(pid);
        marker.child_started = process_start_time(pid);
        write_marker(&self.path, &marker)
    }

    /// Retain the directory after drop (trace opt-in).
    pub fn keep(&mut self) {
        self.keep = true;
        if let Ok(mut marker) = read_marker(&self.path) {
            marker.keep = true;
            let _ = write_marker(&self.path, &marker);
        }
    }

    /// Write a private file inside the directory (`create_new`; refuses to overwrite).
    pub fn write_file(&self, name: &str, bytes: &[u8]) -> io::Result<PathBuf> {
        let path = self.path.join(name);
        let mut opts = fs::OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut file = opts.open(&path)?;
        io::Write::write_all(&mut file, bytes)?;
        Ok(path)
    }
}

impl Drop for PrivateDir {
    fn drop(&mut self) {
        if !self.keep {
            if let Err(err) = remove_dir_retry(&self.path) {
                tracing::warn!(
                    path = %self.path.display(),
                    error = %err,
                    "private request dir not removed"
                );
            }
        }
    }
}

fn sanitize(id: &str) -> String {
    id.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn write_marker(dir: &Path, marker: &PidMarker) -> io::Result<()> {
    let tmp = dir.join(format!("{MARKER_FILE}.tmp"));
    fs::write(&tmp, serde_json::to_vec(marker).map_err(io::Error::other)?)?;
    fs::rename(&tmp, dir.join(MARKER_FILE))
}

fn read_marker(dir: &Path) -> io::Result<PidMarker> {
    let bytes = fs::read(dir.join(MARKER_FILE))?;
    serde_json::from_slice(&bytes).map_err(io::Error::other)
}

/// Remove request directories whose daemon is dead, or whose daemon is us but whose child is
/// gone and the marker is older than `stale_after` (a request that panicked past its guard).
/// A dead daemon's child is killed only where [`ORPHAN_PID_KILL`] holds and only when the pid
/// still carries the recorded start stamp; on Windows the child's job object died with the
/// daemon, so no pid is killed (R11).
/// Directories without a readable marker older than `stale_after` are removed too.
/// Traced (`keep`) directories survive that sweep; only the newest `trace_keep` are retained.
/// Returns the number of directories removed.
pub fn sweep_dead(root: &Path, stale_after: Duration) -> usize {
    sweep_dead_keeping(root, stale_after, u32::MAX)
}

pub fn sweep_dead_keeping(root: &Path, stale_after: Duration, trace_keep: u32) -> usize {
    let Ok(entries) = fs::read_dir(root) else {
        return 0;
    };
    let me = std::process::id();
    let now = now_unix();
    let mut removed = 0;
    let mut kept: Vec<(u64, PathBuf)> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        match read_marker(&path) {
            Ok(marker) if marker.keep => {
                kept.push((marker.created_unix, path));
            }
            Ok(marker) => {
                let age = Duration::from_secs(now.saturating_sub(marker.created_unix));
                let daemon_is_us = marker.daemon_pid == me
                    && marker
                        .daemon_started
                        .is_none_or(|t| process_start_time(me) == Some(t));
                let dead = if !daemon_is_us {
                    match marker.daemon_started {
                        Some(_) => !same_process(marker.daemon_pid, marker.daemon_started),
                        None => !process_alive(marker.daemon_pid),
                    }
                } else {
                    age > stale_after && !marker.child_pid.is_some_and(process_alive)
                };
                if dead {
                    if let Some(child) = marker.child_pid {
                        if ORPHAN_PID_KILL && same_process(child, marker.child_started) {
                            crate::process::kill_process_tree(child);
                        }
                    }
                    if fs::remove_dir_all(&path).is_ok() {
                        removed += 1;
                    }
                }
            }
            Err(_) => {
                let stale = entry
                    .metadata()
                    .and_then(|m| m.modified())
                    .ok()
                    .and_then(|m| SystemTime::now().duration_since(m).ok())
                    .is_some_and(|age| age > stale_after);
                if stale && fs::remove_dir_all(&path).is_ok() {
                    removed += 1;
                }
            }
        }
    }
    kept.sort_by_key(|a| std::cmp::Reverse(a.0));
    for (_, path) in kept.into_iter().skip(trace_keep as usize) {
        if fs::remove_dir_all(&path).is_ok() {
            removed += 1;
        }
    }
    removed
}

const REMOVE_RETRIES: u32 = 5;

/// Retry `remove_dir_all` on a sharing violation (Windows ERROR_SHARING_VIOLATION = 32).
pub fn remove_dir_retry(dir: &Path) -> io::Result<()> {
    if !dir.exists() {
        return Ok(());
    }
    for attempt in 0..REMOVE_RETRIES {
        match fs::remove_dir_all(dir) {
            Ok(()) => return Ok(()),
            Err(err) if is_sharing_violation(&err) && attempt + 1 < REMOVE_RETRIES => {
                thread::sleep(Duration::from_millis(100 * (attempt as u64 + 1)));
            }
            Err(err) => return Err(err),
        }
    }
    Ok(())
}

fn is_sharing_violation(err: &io::Error) -> bool {
    #[cfg(windows)]
    {
        err.raw_os_error() == Some(32)
    }
    #[cfg(not(windows))]
    {
        err.kind() == io::ErrorKind::PermissionDenied
    }
}

/// Write `bytes` to `path` via a sibling temp that is ACL-tightened **before** the secret
/// is written (SPEC §10.5, §19.9). Order: create empty temp (Unix mode 600) → `secure_private_file`
/// → write bytes → `must_be_private` → rename → verify dest. A crash after create+secure
/// leaves an empty private file, never plaintext with inherited ACEs.
pub fn write_private_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = atomic_temp_path(path);
    {
        let mut opts = fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        if let Err(e) = opts.open(&tmp) {
            let _ = fs::remove_file(&tmp);
            return Err(e);
        }
    }
    if let Err(e) = secure_private_file(&tmp) {
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }
    #[cfg(test)]
    observe_after_secure(&tmp);
    if let Err(e) = fs::write(&tmp, bytes) {
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }
    if let Err(e) = must_be_private(&tmp) {
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }
    fs::rename(&tmp, path)?;
    must_be_private(path)
}

#[cfg(test)]
thread_local! {
    static AFTER_SECURE_ARM: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static AFTER_SECURE_LEN: std::cell::Cell<Option<u64>> = const { std::cell::Cell::new(None) };
    static AFTER_SECURE_INHERITED: std::cell::Cell<Option<bool>> =
        const { std::cell::Cell::new(None) };
}

/// Snapshot the temp after ACL tighten and *before* secret bytes land (A11).
#[cfg(test)]
fn observe_after_secure(path: &Path) {
    if !AFTER_SECURE_ARM.with(|a| a.get()) {
        return;
    }
    AFTER_SECURE_ARM.with(|a| a.set(false));
    AFTER_SECURE_LEN.with(|c| c.set(fs::metadata(path).ok().map(|m| m.len())));
    AFTER_SECURE_INHERITED.with(|c| c.set(has_inherited_access(path)));
}

fn atomic_temp_path(path: &Path) -> PathBuf {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "file".into());
    path.with_file_name(format!("{name}.{}.tmp", std::process::id()))
}

fn must_be_private(path: &Path) -> io::Result<()> {
    match has_inherited_access(path) {
        Some(false) => Ok(()),
        Some(true) => Err(io::Error::other(format!(
            "{} still has inherited ACEs",
            path.display()
        ))),
        None => Err(io::Error::other(format!(
            "could not verify ACL on {}",
            path.display()
        ))),
    }
}

/// Restrict `path` to the current user (SPEC §19.9). Windows: `icacls /inheritance:r
/// /grant:r *<SID>:F`; Unix: mode 600 (file) / 700 (directory).
pub fn secure_private_file(path: &Path) -> io::Result<()> {
    #[cfg(windows)]
    {
        let sid = current_user_sid()
            .ok_or_else(|| io::Error::other("cannot determine current user SID for icacls"))?;
        let output = Command::new(icacls_exe())
            .arg(path)
            .args(["/inheritance:r", "/grant:r", &format!("*{sid}:F")])
            .output()?;
        if !output.status.success() {
            return Err(io::Error::other(format!(
                "icacls failed ({}): {}",
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }
        Ok(())
    }
    #[cfg(not(windows))]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = if path.is_dir() { 0o700 } else { 0o600 };
        fs::set_permissions(path, fs::Permissions::from_mode(mode))
    }
}

/// `Some(true)` when `path` still carries inherited ACEs (Windows) or is group/world readable
/// (Unix); `None` when the check itself failed. `doctor` fails on `Some(true)`.
pub fn has_inherited_access(path: &Path) -> Option<bool> {
    #[cfg(windows)]
    {
        let output = Command::new(icacls_exe()).arg(path).output().ok()?;
        if !output.status.success() {
            return None;
        }
        let text = String::from_utf8_lossy(&output.stdout);
        Some(text.lines().any(|line| line.contains("(I)")))
    }
    #[cfg(not(windows))]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(path).ok()?.permissions().mode();
        Some(mode & 0o077 != 0)
    }
}

#[cfg(windows)]
fn icacls_exe() -> PathBuf {
    let root = std::env::var_os("SystemRoot")
        .or_else(|| std::env::var_os("WINDIR"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\Windows"));
    root.join("System32").join("icacls.exe")
}

#[cfg(windows)]
fn current_user_sid() -> Option<String> {
    static SID: OnceLock<Option<String>> = OnceLock::new();
    SID.get_or_init(query_user_sid).clone()
}

#[cfg(windows)]
fn query_user_sid() -> Option<String> {
    let output = Command::new("whoami").args(["/user"]).output().ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8_lossy(&output.stdout)
        .split_whitespace()
        .find(|part| part.starts_with("S-1-"))
        .map(str::to_string)
}

#[cfg(not(windows))]
#[allow(dead_code)]
fn current_user_sid() -> Option<String> {
    None
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    #[test]
    fn create_writes_marker_and_drop_removes() {
        let root = tempfile::tempdir().expect("tempdir");
        let dir = PrivateDir::create(root.path(), "req-1").expect("create");
        let path = dir.path().to_path_buf();
        assert!(path.join(MARKER_FILE).is_file());
        dir.record_child(4242).expect("record child");
        let marker = read_marker(&path).expect("marker");
        assert_eq!(marker.daemon_pid, std::process::id());
        assert_eq!(marker.child_pid, Some(4242));
        drop(dir);
        assert!(!path.exists());
    }

    #[test]
    fn keep_retains_directory() {
        let root = tempfile::tempdir().expect("tempdir");
        let mut dir = PrivateDir::create(root.path(), "req-2").expect("create");
        dir.keep();
        let path = dir.path().to_path_buf();
        drop(dir);
        assert!(path.exists());
    }

    #[test]
    fn create_refuses_existing_directory() {
        let root = tempfile::tempdir().expect("tempdir");
        let _first = PrivateDir::create(root.path(), "dup").expect("create");
        assert!(PrivateDir::create(root.path(), "dup").is_err());
    }

    #[test]
    fn write_file_refuses_overwrite() {
        let root = tempfile::tempdir().expect("tempdir");
        let dir = PrivateDir::create(root.path(), "req-3").expect("create");
        dir.write_file("a.txt", b"x").expect("first write");
        assert!(dir.write_file("a.txt", b"y").is_err());
    }

    #[test]
    fn sweep_removes_dead_daemon_dirs_and_keeps_live_ones() {
        let root = tempfile::tempdir().expect("tempdir");
        let live = PrivateDir::create(root.path(), "live").expect("create");
        let dead = root.path().join("dead");
        fs::create_dir(&dead).unwrap();
        write_marker(
            &dead,
            &PidMarker {
                daemon_pid: u32::MAX - 7,
                child_pid: None,
                created_unix: 0,
                keep: false,
                ..Default::default()
            },
        )
        .unwrap();
        let removed = sweep_dead(root.path(), Duration::from_secs(3600));
        assert_eq!(removed, 1);
        assert!(!dead.exists());
        assert!(live.path().exists());
    }

    /// R11: a dead daemon's recorded child pid, now reused by an unrelated live process, is
    /// never killed; the directory still goes.
    #[test]
    fn sweep_spares_unrelated_process_on_reused_child_pid() {
        #[cfg(windows)]
        let mut cmd = {
            let mut c = Command::new("ping");
            c.args(["-n", "30", "127.0.0.1"]);
            c
        };
        #[cfg(not(windows))]
        let mut cmd = {
            let mut c = Command::new("sleep");
            c.arg("30");
            c
        };
        let mut other = cmd
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn");
        let pid = other.id();
        let root = tempfile::tempdir().expect("tempdir");
        let dead = root.path().join("dead");
        fs::create_dir(&dead).unwrap();
        write_marker(
            &dead,
            &PidMarker {
                daemon_pid: u32::MAX - 7,
                child_pid: Some(pid),
                child_started: process_start_time(pid).map(|t| t.wrapping_add(1)),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(sweep_dead(root.path(), Duration::from_secs(3600)), 1);
        assert!(!dead.exists());
        assert!(process_alive(pid), "unrelated process was killed");
        let _ = other.kill();
        let _ = other.wait();
    }

    /// R11: a marker naming our pid with another start stamp was written by an earlier
    /// daemon that had our pid; it is dead, not ours.
    #[test]
    fn sweep_treats_reused_own_pid_as_dead_daemon() {
        let root = tempfile::tempdir().expect("tempdir");
        let old = root.path().join("old");
        fs::create_dir(&old).unwrap();
        let me = std::process::id();
        write_marker(
            &old,
            &PidMarker {
                daemon_pid: me,
                daemon_started: process_start_time(me).map(|t| t.wrapping_add(1)),
                created_unix: now_unix(),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(sweep_dead(root.path(), Duration::from_secs(3600)), 1);
        assert!(!old.exists());
    }

    #[test]
    fn sweep_removes_stale_own_dirs_without_live_child() {
        let root = tempfile::tempdir().expect("tempdir");
        let stale = root.path().join("stale");
        fs::create_dir(&stale).unwrap();
        write_marker(
            &stale,
            &PidMarker {
                daemon_pid: std::process::id(),
                child_pid: None,
                created_unix: 1,
                keep: false,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(sweep_dead(root.path(), Duration::from_secs(60)), 1);
        assert!(!stale.exists());
    }

    #[test]
    fn secure_private_file_removes_inheritance() {
        let root = tempfile::tempdir().expect("tempdir");
        let file = root.path().join("keys.yaml");
        fs::write(&file, "keys: []\n").unwrap();
        if has_inherited_access(&file) != Some(true) {
            eprintln!("skipped: runner has no ACL inheritance");
            return;
        }
        secure_private_file(&file).expect("secure");
        assert_eq!(has_inherited_access(&file), Some(false));
        assert_eq!(fs::read_to_string(&file).unwrap(), "keys: []\n");
    }

    #[test]
    fn sweep_keeps_newest_traced_dirs() {
        let root = tempfile::tempdir().expect("tempdir");
        let mut paths = Vec::new();
        for i in 0..4u64 {
            let mut dir = PrivateDir::create(root.path(), &format!("t{i}")).expect("create");
            dir.keep();
            let path = dir.path().to_path_buf();
            drop(dir);
            let mut marker = read_marker(&path).expect("marker");
            marker.created_unix = 1_000 + i;
            write_marker(&path, &marker).expect("stamp");
            paths.push(path);
        }
        let removed = sweep_dead_keeping(root.path(), Duration::from_secs(3600), 2);
        assert_eq!(removed, 2);
        assert!(!paths[0].exists());
        assert!(!paths[1].exists());
        assert!(paths[2].exists());
        assert!(paths[3].exists());
    }

    #[test]
    fn write_private_atomic_secures_empty_temp_before_bytes() {
        let root = tempfile::tempdir().expect("tempdir");
        let dest = root.path().join("secret.yaml");
        AFTER_SECURE_ARM.with(|a| a.set(true));
        AFTER_SECURE_LEN.with(|c| c.set(None));
        AFTER_SECURE_INHERITED.with(|c| c.set(None));

        write_private_atomic(&dest, b"k: v\n").expect("atomic write");

        let len = AFTER_SECURE_LEN.with(|c| c.get());
        assert_eq!(
            len,
            Some(0),
            "temp must still be empty after ACL tighten; write-then-secure would already hold the secret"
        );
        assert_eq!(
            AFTER_SECURE_INHERITED.with(|c| c.get()),
            Some(false),
            "crash-window temp must already be private before bytes land"
        );
        assert_eq!(has_inherited_access(&dest), Some(false), "final file");
        assert_eq!(fs::read_to_string(&dest).unwrap(), "k: v\n");
        let leftover_tmp = fs::read_dir(root.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .any(|e| e.file_name().to_string_lossy().ends_with(".tmp"));
        assert!(!leftover_tmp, "temp must be renamed away");
    }
}
