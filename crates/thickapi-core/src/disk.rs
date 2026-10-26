//! Free space on the volume that holds a path (`/ready` disk probe).

use std::path::Path;

/// Bytes free to the current user on the volume that contains `path`.
///
/// `path` need not exist: the nearest existing ancestor is used. A failure to
/// query the volume is `0` (not ready), never a fabricated threshold.
pub fn free_disk_bytes(path: &Path) -> u64 {
    let target = existing_ancestor(path);
    query_volume_free(target).unwrap_or(0)
}

fn existing_ancestor(path: &Path) -> &Path {
    let mut cur = path;
    loop {
        if cur.as_os_str().is_empty() {
            return Path::new(".");
        }
        if cur.exists() {
            return cur;
        }
        match cur.parent() {
            Some(parent) if parent != cur => cur = parent,
            _ => return Path::new("."),
        }
    }
}

#[cfg(windows)]
fn query_volume_free(path: &Path) -> Option<u64> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;

    let wide: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let mut available: u64 = 0;
    let mut total: u64 = 0;
    let mut total_free: u64 = 0;
    let ok =
        unsafe { GetDiskFreeSpaceExW(wide.as_ptr(), &mut available, &mut total, &mut total_free) };
    if ok == 0 {
        None
    } else {
        Some(available)
    }
}

#[cfg(unix)]
fn query_volume_free(path: &Path) -> Option<u64> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let c_path = CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut vfs: libc::statvfs = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::statvfs(c_path.as_ptr(), &mut vfs) };
    if rc != 0 {
        return None;
    }
    let frsize = vfs.f_frsize as u64;
    Some((vfs.f_bavail as u64).saturating_mul(frsize))
}

#[cfg(not(any(windows, unix)))]
fn query_volume_free(_path: &Path) -> Option<u64> {
    None
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    /// The old `/ready` probe returned this when a probe file was writable.
    const FABRICATED_THRESHOLD_PLUS_ONE: u64 = 100 * 1024 * 1024 + 1;

    #[test]
    fn writable_temp_dir_reports_real_free_bytes() {
        let dir = tempfile::tempdir().expect("tempdir");
        let free = free_disk_bytes(dir.path());
        assert!(free > 0, "writable temp volume must report free_bytes > 0");
        assert_ne!(
            free, FABRICATED_THRESHOLD_PLUS_ONE,
            "must not be the fabricated THRESHOLD+1 probe"
        );
    }

    #[test]
    fn missing_path_uses_an_existing_ancestor() {
        let dir = tempfile::tempdir().expect("tempdir");
        let missing = dir.path().join("no").join("such").join("dir");
        let free = free_disk_bytes(&missing);
        assert!(free > 0);
        assert_ne!(free, FABRICATED_THRESHOLD_PLUS_ONE);
    }
}
