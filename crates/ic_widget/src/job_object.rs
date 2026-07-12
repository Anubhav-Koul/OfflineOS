//! Making child processes die with us.
//!
//! The widget supervises `ironclaw-reborn` and `llama-server`. Both hold
//! expensive resources — a libSQL write lock, a multi-gigabyte GPU allocation —
//! and both outlive their parent by default on Windows. If the widget is killed
//! from Task Manager, crashes, or is terminated by the installer during an
//! upgrade, its `Drop` impls never run and the children are orphaned. The next
//! launch then finds the port taken and the GPU full.
//!
//! A **Job Object** with `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` fixes this at the
//! kernel level: when the last handle to the job closes — which happens when the
//! process holding it dies, however it dies — every process in the job is
//! terminated. Assigning a child to the job also captures that child's
//! descendants, so `llama-server`'s workers go too.
//!
//! This is the "kill-tree on exit via Windows Job Objects" the project plan
//! calls for. On other platforms the type is a no-op so the supervisor code
//! stays single-path.

use crate::error::{Error, Result};

/// A kill-on-close job object. Every process assigned to it — and every process
/// they spawn — is terminated when this value is dropped or the owning process
/// dies.
pub struct ProcessJob(imp::Job);

impl ProcessJob {
    /// Create a job whose members die when it closes.
    pub fn new() -> Result<Self> {
        imp::Job::new().map(Self)
    }

    /// Put a spawned child, and everything it goes on to spawn, into the job.
    ///
    /// Call this immediately after `spawn`. A child that exits before being
    /// assigned is reported as an error rather than silently escaping.
    pub fn assign(&self, child: &tokio::process::Child) -> Result<()> {
        self.0.assign(child)
    }

    /// Enlist a synchronous [`std::process::Child`] — the shape `ic_voice`'s Piper
    /// TTS spawns (a blocking subprocess), so it dies with us like the async
    /// children do.
    pub fn assign_std(&self, child: &std::process::Child) -> Result<()> {
        self.0.assign_std(child)
    }
}

#[cfg(windows)]
mod imp {
    use super::{Error, Result};

    use windows::Win32::Foundation::{CloseHandle, HANDLE};
    use windows::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
        SetInformationJobObject,
    };
    use windows::core::PCWSTR;

    pub(super) struct Job {
        handle: HANDLE,
    }

    // The handle is owned solely by this value and every use is a syscall that
    // takes it by value; Windows job handles are safe to use from any thread.
    unsafe impl Send for Job {}
    unsafe impl Sync for Job {}

    impl Job {
        pub(super) fn new() -> Result<Self> {
            // SAFETY: an anonymous job object with a default security
            // descriptor. The returned handle is owned by `Job` and closed on
            // drop.
            let handle = unsafe { CreateJobObjectW(None, PCWSTR::null()) }
                .map_err(|error| job_error("creating the job object", &error))?;

            let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
            limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;

            // SAFETY: `limits` is a correctly initialized
            // JOBOBJECT_EXTENDED_LIMIT_INFORMATION, and the size we pass matches
            // the pointer we pass.
            let result = unsafe {
                SetInformationJobObject(
                    handle,
                    JobObjectExtendedLimitInformation,
                    (&raw const limits).cast(),
                    u32::try_from(size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>())
                        .unwrap_or(u32::MAX),
                )
            };
            if let Err(error) = result {
                // SAFETY: `handle` is a live job handle we just created.
                let _ = unsafe { CloseHandle(handle) };
                return Err(job_error("configuring kill-on-close", &error));
            }
            Ok(Self { handle })
        }

        pub(super) fn assign(&self, child: &tokio::process::Child) -> Result<()> {
            let Some(raw) = child.raw_handle() else {
                // `raw_handle` is `None` only once the child has been reaped.
                return Err(Error::io(
                    "assigning a child to the job object: it has already exited",
                    std::io::Error::from(std::io::ErrorKind::NotFound),
                ));
            };
            // SAFETY: `raw` is the live process handle tokio owns for the child;
            // we only read it for the duration of this call.
            unsafe { AssignProcessToJobObject(self.handle, HANDLE(raw)) }
                .map_err(|error| job_error("assigning a child to the job object", &error))
        }

        pub(super) fn assign_std(&self, child: &std::process::Child) -> Result<()> {
            use std::os::windows::io::AsRawHandle;
            // A live `std::process::Child` always has a handle (unlike tokio's,
            // which is dropped on reap); we only read it for this call.
            let raw = child.as_raw_handle();
            // SAFETY: `raw` is the live process handle std owns for the child.
            unsafe { AssignProcessToJobObject(self.handle, HANDLE(raw)) }
                .map_err(|error| job_error("assigning a child to the job object", &error))
        }
    }

    impl Drop for Job {
        fn drop(&mut self) {
            // Closing the last handle is what kills the members. There is no
            // recovery from a failure here, and nothing to log it to at process
            // teardown.
            // SAFETY: `handle` is live and owned by this value.
            let _ = unsafe { CloseHandle(self.handle) };
        }
    }

    fn job_error(context: &str, error: &windows::core::Error) -> Error {
        Error::io(
            format!("{context}: {error}"),
            std::io::Error::from_raw_os_error(error.code().0),
        )
    }
}

#[cfg(not(windows))]
mod imp {
    use super::Result;

    /// No job objects off Windows. The supervisor still kills its children
    /// explicitly on shutdown; only the "parent was hard-killed" case is
    /// uncovered, and the desktop target is Windows.
    pub(super) struct Job;

    impl Job {
        pub(super) fn new() -> Result<Self> {
            Ok(Self)
        }

        pub(super) fn assign(&self, _child: &tokio::process::Child) -> Result<()> {
            Ok(())
        }

        pub(super) fn assign_std(&self, _child: &std::process::Child) -> Result<()> {
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A trivially long-lived child.
    ///
    /// On Windows this deliberately avoids `ping` as the sleeper: `ping` touches the
    /// network stack, and once this crate links ONNX Runtime (via `ic_voice`) the
    /// loaded `onnxruntime.dll` perturbs the process enough that a spawned `ping`
    /// fails immediately (exit 1) instead of sleeping — which would make the guard
    /// below fire for the wrong reason. PowerShell `Start-Sleep` waits without any
    /// network and does not need a console/stdin (unlike `timeout`).
    fn sleeper() -> tokio::process::Command {
        let mut command = if cfg!(windows) {
            let mut command = tokio::process::Command::new("powershell");
            command.args(["-NoProfile", "-Command", "Start-Sleep -Seconds 60"]);
            command
        } else {
            let mut command = tokio::process::Command::new("sleep");
            command.arg("60");
            command
        };
        command.stdin(std::process::Stdio::null());
        command.stdout(std::process::Stdio::null());
        command.stderr(std::process::Stdio::null());
        command
    }

    #[tokio::test]
    async fn a_job_can_be_created_and_dropped() {
        let job = ProcessJob::new().expect("create a job");
        drop(job);
    }

    #[tokio::test]
    async fn a_spawned_child_can_be_assigned() {
        let job = ProcessJob::new().expect("create a job");
        let mut child = sleeper().spawn().expect("spawn");
        job.assign(&child).expect("assign the child");
        child.kill().await.expect("kill");
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn dropping_the_job_kills_its_members() {
        let mut child = sleeper().spawn().expect("spawn");
        let pid = child.id().expect("a pid");

        {
            let job = ProcessJob::new().expect("create a job");
            job.assign(&child).expect("assign");

            // Establish that the child is genuinely alive and would have kept
            // running. Otherwise a child that exited on its own would make the
            // assertion below pass for the wrong reason.
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            assert!(
                child.try_wait().expect("try_wait").is_none(),
                "the child exited on its own; this test proves nothing"
            );
            // Closing the last job handle terminates every member, even though
            // nothing kills the child directly.
        }

        // `wait` reaps it. Without kill-on-close this would block for a minute.
        // The exit code is not asserted: Windows reaps job members with a status
        // of its own choosing, and on this build that is zero.
        tokio::time::timeout(std::time::Duration::from_secs(10), child.wait())
            .await
            .unwrap_or_else(|_| panic!("process {pid} outlived its job object"))
            .expect("wait");
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn assigning_an_exited_child_is_an_error_not_a_silent_escape() {
        let job = ProcessJob::new().expect("create a job");
        let mut child = tokio::process::Command::new("cmd")
            .args(["/C", "exit 0"])
            .stdout(std::process::Stdio::null())
            .spawn()
            .expect("spawn");
        child.wait().await.expect("wait");

        // Once reaped, tokio drops the handle. Reporting this is important: a
        // silent success would mean the caller believes a child is contained
        // when it is not.
        assert!(job.assign(&child).is_err());
    }
}
