use std::process::Command;

pub fn process_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::System::Threading::{
            GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
        };

        const STILL_ACTIVE: u32 = 259;

        let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
        if handle.is_null() {
            return false;
        }
        let mut code: u32 = 0;
        let ok = unsafe { GetExitCodeProcess(handle, &mut code) };
        unsafe { CloseHandle(handle) };
        ok != 0 && code == STILL_ACTIVE
    }
    #[cfg(not(windows))]
    {
        Command::new("kill")
            .args(["-0", &pid.to_string()])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }
}

/// An opaque, per-boot-stable start stamp for a live process, so a recorded pid can be told
/// apart from a later process that reused it (R11). Windows: creation `FILETIME` as `u64`.
/// Linux: `/proc/<pid>/stat` field 22 (start time in clock ticks). `None` when the process is
/// gone or the platform gives no stamp.
pub fn process_start_time(pid: u32) -> Option<u64> {
    if pid == 0 {
        return None;
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::{CloseHandle, FILETIME};
        use windows_sys::Win32::System::Threading::{
            GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
        };

        let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
        if handle.is_null() {
            return None;
        }
        let zero = FILETIME {
            dwLowDateTime: 0,
            dwHighDateTime: 0,
        };
        let (mut created, mut exited, mut kernel, mut user) = (zero, zero, zero, zero);
        let ok =
            unsafe { GetProcessTimes(handle, &mut created, &mut exited, &mut kernel, &mut user) };
        unsafe { CloseHandle(handle) };
        if ok == 0 {
            return None;
        }
        Some((u64::from(created.dwHighDateTime) << 32) | u64::from(created.dwLowDateTime))
    }
    #[cfg(not(windows))]
    {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        // `comm` (field 2) may contain spaces; the fields after it follow the last `)`.
        let rest = stat.rsplit_once(')')?.1;
        rest.split_whitespace().nth(19)?.parse().ok()
    }
}

/// `pid` is alive and is the same process that was stamped `started` (both known and equal).
/// An unknown stamp on either side is not a match: nothing is killed on a guess.
pub fn same_process(pid: u32, started: Option<u64>) -> bool {
    match started {
        Some(recorded) => process_alive(pid) && process_start_time(pid) == Some(recorded),
        None => false,
    }
}

/// Whether a recorded orphan pid may be killed at all. On Windows every child thickAPI spawns
/// sits in a `KILL_ON_JOB_CLOSE` job object (`engine/job.rs`), so the job — not a pid — owns
/// its lifetime: a dead owner's tree is already gone, and a recorded pid can only name an
/// unrelated process now (R11).
pub const ORPHAN_PID_KILL: bool = cfg!(not(windows));

pub fn kill_process_tree(pid: u32) {
    if pid == 0 {
        return;
    }
    #[cfg(windows)]
    {
        let _ = Command::new("taskkill")
            .args(["/F", "/T", "/PID", &pid.to_string()])
            .output();
    }
    #[cfg(not(windows))]
    {
        let _ = Command::new("kill").args(["-9", &pid.to_string()]).output();
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    #[test]
    fn start_time_identifies_this_process() {
        let me = std::process::id();
        let stamp = process_start_time(me);
        assert!(stamp.is_some(), "own start time must be readable");
        assert!(same_process(me, stamp));
        assert!(!same_process(me, stamp.map(|s| s.wrapping_add(1))));
        assert!(!same_process(me, None));
        assert_eq!(process_start_time(0), None);
    }
}
