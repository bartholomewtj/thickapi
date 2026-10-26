#[cfg(windows)]
mod imp {
    use std::io;
    use std::mem::zeroed;
    use std::os::windows::io::AsRawHandle;
    use std::process::Child;

    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, JobObjectExtendedLimitInformation, SetInformationJobObject,
        TerminateJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };

    extern "system" {
        fn CreateJobObjectW(
            lp_job_attributes: *mut core::ffi::c_void,
            lp_name: *const u16,
        ) -> HANDLE;
    }

    pub struct JobObject {
        handle: HANDLE,
    }

    // SAFETY: a job object handle is a kernel handle; it may be used and closed from any thread.
    unsafe impl Send for JobObject {}
    unsafe impl Sync for JobObject {}

    impl JobObject {
        pub fn new() -> io::Result<Self> {
            let handle = unsafe { CreateJobObjectW(std::ptr::null_mut(), std::ptr::null()) };
            if handle.is_null() {
                return Err(io::Error::last_os_error());
            }
            let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { zeroed() };
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            let ok = unsafe {
                SetInformationJobObject(
                    handle,
                    JobObjectExtendedLimitInformation,
                    &info as *const _ as *mut _,
                    std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                )
            };
            if ok == 0 {
                unsafe { CloseHandle(handle) };
                return Err(io::Error::last_os_error());
            }
            Ok(Self { handle })
        }

        pub fn assign(&self, child: &Child) -> io::Result<()> {
            // SAFETY: child is a live std::process::Child; its handle stays open for the call.
            unsafe { self.assign_handle(child.as_raw_handle() as HANDLE) }
        }

        /// Assign by raw process handle (tokio children expose one via `raw_handle()`).
        ///
        /// # Safety
        /// `process` must be a valid, open process handle with `PROCESS_SET_QUOTA | PROCESS_TERMINATE`.
        pub unsafe fn assign_handle(&self, process: HANDLE) -> io::Result<()> {
            let ok = unsafe { AssignProcessToJobObject(self.handle, process) };
            if ok == 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        }

        /// Kill every process in the job with a known exit code (SPEC §19.7 capture).
        pub fn terminate(&self, exit_code: u32) -> io::Result<()> {
            let ok = unsafe { TerminateJobObject(self.handle, exit_code) };
            if ok == 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        }
    }

    impl Drop for JobObject {
        fn drop(&mut self) {
            if !self.handle.is_null() {
                unsafe { CloseHandle(self.handle) };
            }
        }
    }
}

#[cfg(windows)]
pub use imp::JobObject;

#[cfg(not(windows))]
pub struct JobObject;

#[cfg(not(windows))]
impl JobObject {
    pub fn new() -> std::io::Result<Self> {
        Ok(Self)
    }

    pub fn assign(&self, _child: &std::process::Child) -> std::io::Result<()> {
        Ok(())
    }

    pub fn terminate(&self, _exit_code: u32) -> std::io::Result<()> {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "job objects are Windows-only",
        ))
    }
}
