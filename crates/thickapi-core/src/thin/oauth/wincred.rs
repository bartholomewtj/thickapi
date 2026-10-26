//! Read-only access to Windows Credential Manager generic credentials (`CredReadW`). The `agy`
//! CLI keeps its Google OAuth token there (`gemini:antigravity`); the daemon never writes.

#![cfg(windows)]

use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;

use windows_sys::Win32::Foundation::{GetLastError, ERROR_NOT_FOUND};
use windows_sys::Win32::Security::Credentials::{
    CredFree, CredReadW, CREDENTIALW, CRED_TYPE_GENERIC,
};

#[derive(Debug, Clone, thiserror::Error)]
pub enum WinCredError {
    #[error("credential not found")]
    NotFound,
    #[error("CredReadW failed: Win32 error {0}")]
    Win32(u32),
}

/// The credential blob (bytes as stored; `agy` stores UTF-8 JSON).
pub fn read_generic(target: &str) -> Result<Vec<u8>, WinCredError> {
    let wide: Vec<u16> = OsStr::new(target)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let mut cred: *mut CREDENTIALW = std::ptr::null_mut();
    // SAFETY: `wide` is NUL-terminated and outlives the call; `cred` receives an allocation we
    // release with `CredFree` before returning.
    let ok = unsafe { CredReadW(wide.as_ptr(), CRED_TYPE_GENERIC, 0, &mut cred) };
    if ok == 0 {
        let err = unsafe { GetLastError() };
        return Err(if err == ERROR_NOT_FOUND {
            WinCredError::NotFound
        } else {
            WinCredError::Win32(err)
        });
    }
    // SAFETY: `cred` is non-null on success and points at a CREDENTIALW whose blob pointer and
    // size describe `CredentialBlobSize` readable bytes.
    let blob = unsafe {
        let c = &*cred;
        if c.CredentialBlob.is_null() || c.CredentialBlobSize == 0 {
            Vec::new()
        } else {
            std::slice::from_raw_parts(c.CredentialBlob, c.CredentialBlobSize as usize).to_vec()
        }
    };
    unsafe { CredFree(cred as *const _ as *mut _) };
    Ok(blob)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    #[test]
    fn missing_target_is_not_found() {
        let err = read_generic("thickapi:test:does-not-exist-7f3a").unwrap_err();
        assert!(matches!(err, WinCredError::NotFound), "{err}");
    }
}
