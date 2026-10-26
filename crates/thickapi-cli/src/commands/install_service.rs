use std::path::Path;

#[cfg(windows)]
use std::process::Command;

#[cfg(windows)]
use anyhow::anyhow;
use anyhow::{Context, Result};

pub const TASK_NAME: &str = "thickapi-serve";
pub const REAP_TASK_NAME: &str = "thickapi-reap";
pub const CAPTURE_TASK_NAME: &str = "thickapi-capture";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedTask {
    pub name: &'static str,
    pub schedule: &'static str,
    pub action: String,
}

/// Tasks `install-service` would register. Weekly capture is opt-in (default off). `data_dir`
/// is `router.yaml`'s `data_dir` (expanded), used only for the weekly capture's absolute
/// `--out` (D3 deliverable 7) — `serve` and `reap` need no `--out`.
pub fn planned_tasks(
    exe: &Path,
    config_dir: &Path,
    data_dir: &Path,
    weekly_capture: bool,
) -> Vec<PlannedTask> {
    let mut tasks = vec![
        PlannedTask {
            name: TASK_NAME,
            schedule: "ONLOGON",
            action: serve_action(exe, config_dir),
        },
        PlannedTask {
            name: REAP_TASK_NAME,
            schedule: "DAILY",
            action: reap_action(exe, config_dir),
        },
    ];
    if weekly_capture {
        tasks.push(PlannedTask {
            name: CAPTURE_TASK_NAME,
            schedule: "WEEKLY",
            action: weekly_capture_action(exe, config_dir, data_dir),
        });
    }
    tasks
}

pub fn run(config_dir: &Path, weekly_capture: bool) -> Result<()> {
    #[cfg(not(windows))]
    {
        let _ = weekly_capture;
        eprintln!("thickapi install-service is only supported on Windows");
        std::process::exit(1);
    }

    #[cfg(windows)]
    {
        let exe = std::env::current_exe().context("resolve thickapi executable path")?;
        let data_dir = resolve_data_dir(config_dir);
        for task in planned_tasks(&exe, config_dir, &data_dir, weekly_capture) {
            register_task(task.name, task.schedule, &task.action)?;
            if task.name == TASK_NAME {
                relax_long_running_task(task.name)?;
            }
            println!("registered scheduled task {}", task.name);
        }
        println!("remove with: schtasks /Delete /TN {} /F", TASK_NAME);
        println!("remove with: schtasks /Delete /TN {} /F", REAP_TASK_NAME);
        if weekly_capture {
            println!("remove with: schtasks /Delete /TN {} /F", CAPTURE_TASK_NAME);
        }
        Ok(())
    }
}

/// Best-effort `router.yaml` `data_dir`, expanded; falls back to `config_dir` itself so the
/// weekly capture action is still an absolute path even before `router.yaml` exists.
#[cfg(windows)]
fn resolve_data_dir(config_dir: &Path) -> std::path::PathBuf {
    thickapi_core::load_config(config_dir)
        .map(|bundle| bundle.router.data_dir)
        .unwrap_or_else(|_| config_dir.to_path_buf())
}

pub fn serve_action(exe: &Path, config_dir: &Path) -> String {
    format!("\"{}\" serve{}", exe.display(), config_flag(config_dir))
}

pub fn reap_action(exe: &Path, config_dir: &Path) -> String {
    format!("\"{}\" reap{}", exe.display(), config_flag(config_dir))
}

/// Weekly opt-in: `{exe} capture --all --out <data_dir>/captures` (plus `--config` when
/// needed). Always an absolute `--out`, never cwd-relative (D3 deliverable 7) — Task
/// Scheduler's working directory is not the repo checkout, so the CLI's own `fixtures`
/// default would land wherever Windows starts the task. Never `cargo test`.
pub fn weekly_capture_action(exe: &Path, config_dir: &Path, data_dir: &Path) -> String {
    format!(
        "\"{}\" capture --all --out \"{}\"{}",
        exe.display(),
        data_dir.join("captures").display(),
        config_flag(config_dir)
    )
}

fn config_flag(config_dir: &Path) -> String {
    if config_dir == thickapi_core::default_config_dir() {
        String::new()
    } else {
        format!(" --config \"{}\"", config_dir.display())
    }
}

/// `schtasks /Create` leaves the defaults that stop a long-running process: 72-hour
/// `ExecutionTimeLimit`, stop on battery, and `StopOnIdleEnd` (hard-killed when the session
/// leaves idle, which skips the lock destructor). A daemon task turns those off.
///
/// `ExecutionTimeLimit` has to be the string `PT0S`. Assigning a zero `TimeSpan` is written
/// back as `00:00:00`, which Task Scheduler rejects. The task file is writable by
/// Administrators, so this script has to run elevated (same as `schtasks /Create /F`).
pub fn serve_task_settings_script(name: &str) -> String {
    format!(
        r#"$xml = Export-ScheduledTask -TaskName '{name}'
$xml = $xml.Replace('<StopOnIdleEnd>true</StopOnIdleEnd>', '<StopOnIdleEnd>false</StopOnIdleEnd>')
$xml = $xml.Replace('<DisallowStartIfOnBatteries>true</DisallowStartIfOnBatteries>', '<DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>')
$xml = $xml.Replace('<StopIfGoingOnBatteries>true</StopIfGoingOnBatteries>', '<StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>')
if ($xml -match '<ExecutionTimeLimit>') {{
    $xml = [regex]::Replace($xml, '<ExecutionTimeLimit>[^<]*</ExecutionTimeLimit>', '<ExecutionTimeLimit>PT0S</ExecutionTimeLimit>')
}} else {{
    $xml = $xml.Replace('<Settings>', "<Settings>`r`n    <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>")
}}
Register-ScheduledTask -TaskName '{name}' -Xml $xml -Force | Out-Null
"#
    )
}

#[cfg(windows)]
fn relax_long_running_task(name: &str) -> Result<()> {
    let script = serve_task_settings_script(name);
    let path = std::env::temp_dir().join(format!("thickapi-task-{name}.ps1"));
    std::fs::write(&path, &script).with_context(|| format!("write {}", path.display()))?;
    let output = Command::new("powershell.exe")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-File",
        ])
        .arg(&path)
        .output()
        .context("relax scheduled task settings")?;
    let _ = std::fs::remove_file(&path);
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        return Err(anyhow!("relax {name} failed: {stdout}{stderr}"));
    }
    Ok(())
}

#[cfg(windows)]
fn register_task(name: &str, schedule: &str, action: &str) -> Result<()> {
    let output = Command::new("schtasks")
        .args(["/Create", "/SC", schedule, "/TN", name, "/TR", action, "/F"])
        .output()
        .context("run schtasks")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        return Err(anyhow!("schtasks {name} failed: {stdout}{stderr}"));
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn serve_task_settings_turn_off_scheduler_stops() {
        let script = serve_task_settings_script(TASK_NAME);
        assert!(script.contains("<ExecutionTimeLimit>PT0S</ExecutionTimeLimit>"));
        assert!(script.contains("if ($xml -match '<ExecutionTimeLimit>')"));
        assert!(script.contains("<StopOnIdleEnd>false</StopOnIdleEnd>"));
        assert!(script.contains("<DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>"));
        assert!(script.contains("<StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>"));
        assert!(script.contains("Register-ScheduledTask -TaskName 'thickapi-serve'"));
        assert!(!script.contains("New-TimeSpan"));
    }

    #[test]
    fn reap_task_is_daily_reap_not_serve() {
        let exe = PathBuf::from(r"C:\tools\thickapi.exe");
        let cfg = PathBuf::from(r"C:\custom\thickapi");
        assert!(serve_action(&exe, &cfg).contains("serve"));
        assert!(reap_action(&exe, &cfg).contains("reap"));
        assert!(reap_action(&exe, &cfg).contains(r"--config"));
        assert!(!reap_action(&exe, &thickapi_core::default_config_dir()).contains("--config"));
    }

    #[test]
    fn default_install_registers_serve_and_daily_reap_only() {
        let exe = PathBuf::from(r"C:\tools\thickapi.exe");
        let cfg = PathBuf::from(r"C:\custom\thickapi");
        let data_dir = PathBuf::from(r"C:\custom\thickapi\data");
        let tasks = planned_tasks(&exe, &cfg, &data_dir, false);
        assert_eq!(tasks.len(), 2);
        assert!(tasks.iter().all(|t| t.name != CAPTURE_TASK_NAME));
        assert_eq!(tasks[0].name, TASK_NAME);
        assert_eq!(tasks[1].name, REAP_TASK_NAME);
    }

    /// D3 deliverable 7: the weekly action's `--out` is an absolute `<data_dir>/captures`,
    /// never the CLI's own cwd-relative `fixtures` default (Task Scheduler's cwd is not the
    /// repo checkout).
    #[test]
    fn weekly_capture_opt_in_is_capture_all_not_cargo() {
        let exe = PathBuf::from(r"C:\tools\thickapi.exe");
        let cfg = PathBuf::from(r"C:\custom\thickapi");
        let data_dir = PathBuf::from(r"C:\custom\thickapi\data");
        let action = weekly_capture_action(&exe, &cfg, &data_dir);
        assert!(
            action.contains("capture --all"),
            "weekly action must run capture --all: {action}"
        );
        assert!(
            !action.to_ascii_lowercase().contains("cargo"),
            "weekly action must not bake cargo test: {action}"
        );
        assert!(
            !action.contains("CARGO_MANIFEST"),
            "weekly action must not bake CARGO_MANIFEST_DIR: {action}"
        );
        assert!(
            !action.contains("serve"),
            "weekly action must not be serve: {action}"
        );
        assert!(
            action.contains(r#"--out "C:\custom\thickapi\data\captures""#),
            "weekly action must pass an absolute --out under data_dir: {action}"
        );
        assert!(
            !action.contains("--out fixtures"),
            "weekly action must never fall back to the cwd-relative default: {action}"
        );
        let tasks = planned_tasks(&exe, &cfg, &data_dir, true);
        assert!(tasks.iter().any(|t| t.name == CAPTURE_TASK_NAME));
        let capture_task = tasks.iter().find(|t| t.name == CAPTURE_TASK_NAME).unwrap();
        assert!(capture_task.action.contains("--config"));
        assert!(capture_task.action.contains("--out"));
    }

    /// `resolve_data_dir` must not fail (or default to a relative path) when `router.yaml`
    /// does not exist yet — `install-service` can run before the first `serve`.
    #[test]
    fn resolve_data_dir_falls_back_to_config_dir_without_router_yaml() {
        let temp = tempfile::tempdir().unwrap();
        let cfg = temp.path().join("does-not-exist");
        assert_eq!(resolve_data_dir(&cfg), cfg);
    }
}
