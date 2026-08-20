use std::path::PathBuf;

use tokio::process::{Child, Command};

use super::{REAP_GRACE, STDERR_CAP};

pub(super) fn official_executable(executable: Option<PathBuf>) -> Result<PathBuf, String> {
    if let Some(path) = &executable
        && !path.is_absolute()
    {
        return Err("official Python executable path must be absolute".into());
    }
    Ok(executable.unwrap_or_else(|| {
        if cfg!(windows) {
            PathBuf::from("python")
        } else {
            PathBuf::from("python3")
        }
    }))
}

pub(super) fn copy_runtime_environment(command: &mut Command) {
    // The worker starts from a clean environment. These are process-runtime values, not MinerU
    // configuration; all MINERU_* settings arrive in the bounded request and are set by the shim.
    for name in [
        "PATH",
        "HOME",
        "USERPROFILE",
        "TMPDIR",
        "TEMP",
        "TMP",
        "SYSTEMROOT",
    ] {
        if let Some(value) = std::env::var_os(name) {
            command.env(name, value);
        }
    }
}

pub(super) async fn terminate(child: &mut Child) {
    #[cfg(unix)]
    kill_process_group(child.id());
    let _ = child.start_kill();
    let _ = tokio::time::timeout(REAP_GRACE, child.wait()).await;
}

pub(super) fn with_diagnostic(message: String, diagnostic: Option<&[u8]>) -> String {
    let diagnostic = diagnostic
        .map(|bytes| crate::error::sanitize_vlm_error_bytes(bytes, STDERR_CAP))
        .filter(|text| !text.is_empty());
    match diagnostic {
        Some(diagnostic) => format!("{message}: {diagnostic}"),
        None => message,
    }
}

#[cfg(unix)]
fn kill_process_group(process_group: Option<u32>) {
    let Some(process_group) = process_group.filter(|id| *id != 0 && *id != std::process::id())
    else {
        return;
    };
    unsafe { libc::kill(-(process_group as libc::pid_t), libc::SIGKILL) };
}

#[cfg(unix)]
pub(super) struct ProcessGroup(Option<u32>);

#[cfg(unix)]
impl ProcessGroup {
    pub(super) fn new(process_id: Option<u32>) -> Self {
        Self(process_id)
    }

    pub(super) fn kill(&self) {
        kill_process_group(self.0);
    }

    pub(super) fn disarm(&mut self) {
        self.0 = None;
    }
}

#[cfg(unix)]
impl Drop for ProcessGroup {
    fn drop(&mut self) {
        kill_process_group(self.0);
    }
}

#[cfg(target_os = "linux")]
pub(super) fn install_parent_death_signal(command: &mut Command) {
    use std::os::unix::process::CommandExt;
    let parent = std::process::id();
    unsafe {
        command.pre_exec(move || {
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            if libc::getppid() != parent as libc::pid_t {
                libc::_exit(1);
            }
            Ok(())
        });
    }
}

#[cfg(windows)]
pub(super) struct WindowsJob(windows_sys::Win32::Foundation::HANDLE);

#[cfg(windows)]
impl WindowsJob {
    pub(super) fn attach(child: &Child) -> Result<Self, String> {
        use std::mem::size_of;
        use windows_sys::Win32::{
            Foundation::INVALID_HANDLE_VALUE,
            System::JobObjects::{
                AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
                JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
                SetInformationJobObject,
            },
        };
        let handle = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        if handle.is_null() || handle == INVALID_HANDLE_VALUE {
            return Err("official worker job creation failed".into());
        }
        let job = Self(handle);
        let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        let set = unsafe {
            SetInformationJobObject(
                job.0,
                JobObjectExtendedLimitInformation,
                (&raw const limits).cast(),
                size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        };
        let process_handle = child
            .raw_handle()
            .ok_or_else(|| "official worker process handle unavailable".to_owned())?;
        let assigned = unsafe { AssignProcessToJobObject(job.0, process_handle as _) };
        if set == 0 || assigned == 0 {
            return Err("official worker job assignment failed".into());
        }
        Ok(job)
    }
}

#[cfg(windows)]
// SAFETY: A Windows HANDLE is a process-local reference to a kernel object and
// may be used from any thread in that process. WindowsJob uniquely owns this
// handle; it is not Clone or Copy, and Drop is the only place that closes it.
unsafe impl Send for WindowsJob {}

#[cfg(windows)]
unsafe impl Sync for WindowsJob {}

#[cfg(windows)]
impl Drop for WindowsJob {
    fn drop(&mut self) {
        unsafe { windows_sys::Win32::Foundation::CloseHandle(self.0) };
    }
}
