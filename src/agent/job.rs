//! Windows Job Object wrapper used to kill the whole Pi process tree.
//!
//! `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` means closing the handle (including
//! on panic or process exit) terminates every process assigned to the job:
//! Pi, its MCP server child, and any tool processes.

#[cfg(windows)]
mod imp {
    use std::ffi::c_void;
    use std::mem::size_of;
    use std::os::windows::io::AsRawHandle;
    use std::process::Child;

    use windows::Win32::Foundation::{CloseHandle, HANDLE};
    use windows::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
        SetInformationJobObject, TerminateJobObject,
    };
    use windows::core::PCWSTR;

    /// Owned Job Object handle.
    pub(crate) struct JobHandle(Option<HANDLE>);

    // SAFETY: a Windows handle is a kernel object reference that may be used
    // from any thread. The handle is only closed once, by Drop.
    unsafe impl Send for JobHandle {}

    impl JobHandle {
        pub(crate) fn create() -> Result<Self, String> {
            // SAFETY: null attributes/name are valid; the returned handle is
            // owned by the wrapper.
            unsafe {
                let handle =
                    CreateJobObjectW(None, PCWSTR::null()).map_err(|error| error.to_string())?;
                let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
                info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
                let result = SetInformationJobObject(
                    handle,
                    JobObjectExtendedLimitInformation,
                    &info as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION as *const c_void,
                    size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                );
                if let Err(error) = result {
                    let _ = CloseHandle(handle);
                    return Err(error.to_string());
                }
                Ok(Self(Some(handle)))
            }
        }

        pub(crate) fn assign(&self, child: &Child) -> Result<(), String> {
            let Some(handle) = self.0 else {
                return Err("job object is already closed".to_owned());
            };
            // SAFETY: both handles are valid for the duration of the call.
            unsafe {
                AssignProcessToJobObject(handle, HANDLE(child.as_raw_handle()))
                    .map_err(|error| error.to_string())
            }
        }

        pub(crate) fn terminate(&self, exit_code: u32) {
            if let Some(handle) = self.0 {
                // SAFETY: handle is valid and owned by this wrapper.
                unsafe {
                    let _ = TerminateJobObject(handle, exit_code);
                }
            }
        }
    }

    impl Drop for JobHandle {
        fn drop(&mut self) {
            if let Some(handle) = self.0.take() {
                // SAFETY: the handle was created by CreateJobObjectW and is
                // closed exactly once here. Closing with
                // KILL_ON_JOB_CLOSE terminates the tree.
                unsafe {
                    let _ = CloseHandle(handle);
                }
            }
        }
    }
}

#[cfg(not(windows))]
mod imp {
    use std::process::Child;

    /// No-op stand-in so the bridge compiles on non-Windows hosts.
    pub(crate) struct JobHandle;

    impl JobHandle {
        pub(crate) fn create() -> Result<Self, String> {
            Ok(Self)
        }

        pub(crate) fn assign(&self, _child: &Child) -> Result<(), String> {
            Ok(())
        }

        pub(crate) fn terminate(&self, _exit_code: u32) {}
    }
}

pub(crate) use imp::JobHandle;
