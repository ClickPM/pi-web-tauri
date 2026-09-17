//! Process-tree reclamation through a Win32 job object.
//!
//! WHY NOT `taskkill /T`
//! ---------------------
//! The Electron shell killed the server with `taskkill /pid <pid> /t /f` from
//! its own exit handlers. That works for an orderly quit and for a crash the
//! runtime can still catch — and fails for exactly the case users hit: End Task
//! on the shell, a hard kill, a power-state hiccup. Nothing runs our handlers,
//! and `node.exe` plus the Next.js server it spawned stay resident holding the
//! port and the runtime's file handles.
//!
//! A job object moves the guarantee into the kernel. The job is created with
//! `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`; every server process we spawn is
//! assigned to it. Processes that a job member spawns join the job
//! automatically (there is no breakaway limit set), so the Next.js GRANDCHILD
//! that `bin/pi-web.js` starts is covered without us ever learning its pid.
//! When the last handle to the job closes — which happens when our process
//! object is torn down, however it died — the kernel terminates every member.
//!
//! No raw FFI is needed anywhere here: `win32job` is a safe wrapper over the
//! job-object APIs, and `Child::as_raw_handle()` is a safe accessor. Handing
//! that handle value to the wrapper is an ordinary call. (The project bans the
//! FFI escape hatch outright, so a plain grep of `src-tauri/src` for it must
//! come back empty — including this comment.)

use std::process::Child;

#[cfg(windows)]
use std::os::windows::io::AsRawHandle;

/// Owns the job for the lifetime of the process. Dropping it (or the process
/// dying) closes the last handle, which is what kills the members.
pub struct ProcessJob {
    #[cfg(windows)]
    job: Option<win32job::Job>,
}

impl ProcessJob {
    #[cfg(windows)]
    pub fn create() -> Self {
        use win32job::{ExtendedLimitInfo, Job};

        let mut info = ExtendedLimitInfo::new();
        info.limit_kill_on_job_close();

        match Job::create_with_limit_info(&info) {
            Ok(job) => {
                crate::log::dbg("job object: created with KILL_ON_JOB_CLOSE");
                Self { job: Some(job) }
            }
            Err(e) => {
                // Not fatal: the explicit taskkill fallback in server.rs still
                // covers every orderly exit. Only a hard kill leaks, which is
                // exactly the Electron behaviour we were improving on.
                crate::log::dbg(format!(
                    "job object: creation FAILED ({e}) — falling back to taskkill-only cleanup"
                ));
                Self { job: None }
            }
        }
    }

    #[cfg(not(windows))]
    pub fn create() -> Self {
        Self {}
    }

    /// Put a freshly spawned child (and, transitively, everything it spawns)
    /// under the job. Returns whether the kernel guarantee is in force for it.
    #[cfg(windows)]
    pub fn assign(&self, child: &Child) -> bool {
        let Some(job) = self.job.as_ref() else {
            return false;
        };
        let handle = child.as_raw_handle() as isize;
        match job.assign_process(handle) {
            Ok(()) => {
                crate::log::dbg(format!("job object: assigned pid {}", child.id()));
                true
            }
            Err(e) => {
                crate::log::dbg(format!(
                    "job object: assign pid {} FAILED ({e}) — taskkill fallback applies",
                    child.id()
                ));
                false
            }
        }
    }

    #[cfg(not(windows))]
    pub fn assign(&self, _child: &Child) -> bool {
        false
    }
}
