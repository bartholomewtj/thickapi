use std::path::Path;
#[cfg(test)]
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result};
use thickapi_core::thin::private_dir::remove_dir_retry;
use thickapi_core::{expand_path, load_config};

/// `reap` also sweeps dead thin private request directories under `<config_dir>/tmp` (SPEC §19.9)
/// and rolling logs older than `thin.log_keep_days` (SPEC §19.15 R5).
#[cfg(test)]
pub fn run_with_tmp(
    data_dir: &Path,
    tmp_root: Option<&Path>,
    days: u64,
    dry_run: bool,
) -> Result<()> {
    run_retention(data_dir, tmp_root, None, 14, days, u32::MAX, dry_run)
}

pub fn run_from_config(config_dir: &Path, turn_days: u64, dry_run: bool) -> Result<()> {
    let config = load_config(config_dir).context("load config")?;
    let data_dir = expand_path(&config.router.data_dir);
    let log_dir = expand_path(&config.router.thin.log_dir);
    let tmp = config_dir.join("tmp");
    run_retention(
        &data_dir,
        Some(&tmp),
        Some(&log_dir),
        config.router.thin.log_keep_days,
        turn_days,
        config.router.thin.trace_keep,
        dry_run,
    )
}

pub fn run_retention(
    data_dir: &Path,
    tmp_root: Option<&Path>,
    log_dir: Option<&Path>,
    log_keep_days: u64,
    turn_days: u64,
    trace_keep: u32,
    dry_run: bool,
) -> Result<()> {
    if let Some(tmp) = tmp_root.filter(|p| p.exists()) {
        if dry_run {
            println!(
                "would sweep dead private request dirs under {}",
                tmp.display()
            );
        } else {
            let swept = thickapi_core::thin::private_dir::sweep_dead_keeping(
                tmp,
                thickapi_core::thin::runtime::SWEEP_STALE_AFTER,
                trace_keep,
            );
            println!(
                "swept {} dead private request dir(s) under {}",
                swept,
                tmp.display()
            );
        }
    }

    reap_turns(data_dir, turn_days, dry_run)?;
    if let Some(log_dir) = log_dir {
        reap_logs(log_dir, log_keep_days, dry_run)?;
    }
    Ok(())
}

fn reap_turns(data_dir: &Path, days: u64, dry_run: bool) -> Result<()> {
    let turns_dir = data_dir.join("turns");
    if !turns_dir.exists() {
        println!("no turns directory at {}", turns_dir.display());
        return Ok(());
    }

    let cutoff = SystemTime::now() - Duration::from_secs(days * 24 * 60 * 60);
    let mut removed = 0u32;
    for entry in
        std::fs::read_dir(&turns_dir).with_context(|| format!("read {}", turns_dir.display()))?
    {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let dir = entry.path();
        let mtime = artifact_mtime(&dir).unwrap_or_else(SystemTime::now);
        if mtime > cutoff {
            continue;
        }
        if dry_run {
            println!("would remove {}", dir.display());
        } else {
            remove_dir_retry(&dir).with_context(|| format!("remove {}", dir.display()))?;
            println!("removed {}", dir.display());
        }
        removed += 1;
    }
    println!(
        "{} {} turn artifact dir(s) older than {} day(s)",
        if dry_run { "would remove" } else { "removed" },
        removed,
        days
    );
    Ok(())
}

fn reap_logs(log_dir: &Path, keep_days: u64, dry_run: bool) -> Result<()> {
    if !log_dir.exists() {
        println!("no log directory at {}", log_dir.display());
        return Ok(());
    }
    let cutoff = SystemTime::now() - Duration::from_secs(keep_days * 24 * 60 * 60);
    let mut removed = 0u32;
    for entry in
        std::fs::read_dir(log_dir).with_context(|| format!("read {}", log_dir.display()))?
    {
        let entry = entry?;
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
        if !is_reapable_log(name) {
            continue;
        }
        let mtime = entry
            .metadata()
            .ok()
            .and_then(|m| m.modified().ok())
            .unwrap_or_else(SystemTime::now);
        if mtime > cutoff {
            continue;
        }
        if dry_run {
            println!("would remove {}", path.display());
        } else {
            std::fs::remove_file(&path).with_context(|| format!("remove {}", path.display()))?;
            println!("removed {}", path.display());
        }
        removed += 1;
    }
    println!(
        "{} {} log file(s) older than {} day(s)",
        if dry_run { "would remove" } else { "removed" },
        removed,
        keep_days
    );
    Ok(())
}

fn is_reapable_log(name: &str) -> bool {
    name == "autostart.log" || (name.starts_with("thickapi.") && name.ends_with(".log"))
}

fn artifact_mtime(dir: &Path) -> Option<SystemTime> {
    let result = dir.join("result.json");
    let target = if result.exists() {
        result
    } else {
        dir.to_path_buf()
    };
    std::fs::metadata(&target)
        .ok()
        .and_then(|m| m.modified().ok())
}

#[cfg(test)]
#[allow(dead_code)]
pub fn resolve_data_dir(config_dir: &Path) -> Result<PathBuf> {
    let config = load_config(config_dir).context("load config")?;
    Ok(expand_path(&config.router.data_dir))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn dry_run_on_missing_turns_dir() {
        let temp = tempfile::tempdir().expect("tempdir");
        run_with_tmp(temp.path(), None, 7, true).expect("dry run");
    }

    #[test]
    fn removes_old_dirs_when_mtime_is_old() {
        let temp = tempfile::tempdir().expect("tempdir");
        let turns = temp.path().join("turns");
        let old = turns.join("old-turn");
        fs::create_dir_all(&old).expect("mkdir");
        fs::write(old.join("result.json"), "{}").expect("write");
        let past = SystemTime::now() - Duration::from_secs(10 * 24 * 60 * 60);
        filetime::set_file_mtime(
            old.join("result.json"),
            filetime::FileTime::from_system_time(past),
        )
        .expect("mtime");

        run_with_tmp(temp.path(), None, 7, false).expect("reap");
        assert!(!old.exists());
    }

    #[test]
    fn removes_old_rolling_logs() {
        let temp = tempfile::tempdir().expect("tempdir");
        let logs = temp.path().join("logs");
        fs::create_dir_all(&logs).expect("mkdir");
        let old = logs.join("thickapi.2020-01-01.log");
        let keep = logs.join("thickapi.2099-01-01.log");
        let autostart = logs.join("autostart.log");
        let autostart_fresh = logs.join("notes.txt");
        fs::write(&old, "old").unwrap();
        fs::write(&keep, "new").unwrap();
        fs::write(&autostart, "spawn").unwrap();
        fs::write(&autostart_fresh, "not a log").unwrap();
        let past = SystemTime::now() - Duration::from_secs(20 * 24 * 60 * 60);
        filetime::set_file_mtime(&old, filetime::FileTime::from_system_time(past)).expect("mtime");
        filetime::set_file_mtime(&autostart, filetime::FileTime::from_system_time(past))
            .expect("autostart mtime");

        run_retention(temp.path(), None, Some(&logs), 14, 7, u32::MAX, false).expect("reap logs");
        assert!(!old.exists());
        assert!(keep.exists());
        assert!(
            !autostart.exists(),
            "stale autostart.log must be reaped with rolling logs"
        );
        assert!(autostart_fresh.exists());
    }
}
