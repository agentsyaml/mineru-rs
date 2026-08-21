use std::{
    fs,
    path::{Path, PathBuf},
    process::Stdio,
    time::{Duration, Instant},
};

use tempfile::TempDir;
use tokio::process::{Child, Command};

use super::super::{
    OfficialPersistentWorker, OfficialRequest, OfficialSessionConfig, OfficialWorker,
};
use super::WindowsJob;

const POWERSHELL_SCRIPT: &str = r#"
$ready = $env:MINERU_RS_JOB_READY
$go = $env:MINERU_RS_JOB_GO
$pids = $env:MINERU_RS_JOB_PIDS
[System.IO.File]::WriteAllText($ready, [string]$PID)
while (-not [System.IO.File]::Exists($go)) {
    Start-Sleep -Milliseconds 10
}
$descendant = Start-Process -FilePath $env:MINERU_RS_JOB_PING `
    -ArgumentList '127.0.0.1 -n 31' -WindowStyle Hidden -PassThru
[System.IO.File]::WriteAllText($pids, "$PID $($descendant.Id)")
Wait-Process -Id $descendant.Id
"#;

struct FixturePaths {
    ready: PathBuf,
    go: PathBuf,
    pids: PathBuf,
}

fn powershell_path() -> PathBuf {
    let root = std::env::var_os("SystemRoot").expect("SystemRoot is required on Windows");
    let path = PathBuf::from(root)
        .join("System32")
        .join("WindowsPowerShell")
        .join("v1.0")
        .join("powershell.exe");
    assert!(
        path.is_file(),
        "missing Windows PowerShell: {}",
        path.display()
    );
    path
}

fn python_path() -> PathBuf {
    let path = std::env::var_os("PATH")
        .map(|value| {
            std::env::split_paths(&value).find_map(|directory| {
                ["python.exe", "python3.exe"]
                    .into_iter()
                    .map(|name| directory.join(name))
                    .find(|path| path.is_file())
            })
        })
        .flatten()
        .expect("Windows CI must provide a Python executable for the official worker");
    assert!(path.is_absolute(), "Python executable path is not absolute");
    path
}

struct AttachFailureReset;

impl Drop for AttachFailureReset {
    fn drop(&mut self) {
        super::set_attach_failure_for_test(false);
    }
}

fn force_attach_failure() -> AttachFailureReset {
    super::set_attach_failure_for_test(true);
    AttachFailureReset
}

fn attach_failure_pid(error: &str) -> u32 {
    error
        .rsplit_once("pid=")
        .and_then(|(_, pid)| pid.parse().ok())
        .unwrap_or_else(|| panic!("attach failure did not publish a PID: {error}"))
}

fn spawn_fixture() -> (TempDir, FixturePaths, Child) {
    let temp = tempfile::tempdir().unwrap();
    let paths = FixturePaths {
        ready: temp.path().join("ready"),
        go: temp.path().join("go"),
        pids: temp.path().join("pids"),
    };
    let system_root = std::env::var_os("SystemRoot").unwrap();
    let mut command = Command::new(powershell_path());
    command
        .args([
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-Command",
            POWERSHELL_SCRIPT,
        ])
        .env("MINERU_RS_JOB_READY", &paths.ready)
        .env("MINERU_RS_JOB_GO", &paths.go)
        .env("MINERU_RS_JOB_PIDS", &paths.pids)
        .env(
            "MINERU_RS_JOB_PING",
            PathBuf::from(&system_root)
                .join("System32")
                .join("ping.exe"),
        )
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .creation_flags(windows_sys::Win32::System::Threading::CREATE_NO_WINDOW);
    let child = command
        .spawn()
        .expect("Windows PowerShell fixture failed to spawn");
    (temp, paths, child)
}

async fn wait_for_file(path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !path.is_file() && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(path.is_file(), "fixture did not create {}", path.display());
}

async fn attached_fixture() -> (TempDir, FixturePaths, Child, WindowsJob) {
    let (temp, paths, child) = spawn_fixture();
    wait_for_file(&paths.ready).await;
    let job = WindowsJob::attach(&child).expect("worker Job Object attach failed");
    (temp, paths, child, job)
}

async fn release_fixture(paths: &FixturePaths) -> Vec<u32> {
    fs::write(&paths.go, b"go").unwrap();
    wait_for_file(&paths.pids).await;
    let pids = fs::read_to_string(&paths.pids)
        .unwrap()
        .split_whitespace()
        .map(|pid| pid.parse::<u32>().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        pids.len(),
        2,
        "fixture did not publish worker and descendant PIDs"
    );
    assert!(pids.iter().all(|pid| *pid != 0));
    pids
}

fn process_is_running(pid: u32) -> bool {
    use windows_sys::Win32::{
        Foundation::{CloseHandle, INVALID_HANDLE_VALUE},
        System::Threading::{
            GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE,
        },
    };

    const STILL_ACTIVE: u32 = 259;
    unsafe {
        let handle = OpenProcess(
            PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
            0,
            pid,
        );
        if handle.is_null() || handle == INVALID_HANDLE_VALUE {
            return false;
        }
        let mut exit_code = 0;
        let running = GetExitCodeProcess(handle, &mut exit_code) != 0 && exit_code == STILL_ACTIVE;
        CloseHandle(handle);
        running
    }
}

async fn assert_processes_dead(pids: &[u32]) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while pids.iter().any(|pid| process_is_running(*pid)) && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        pids.iter().all(|pid| !process_is_running(*pid)),
        "worker or descendant survived Job Object cleanup: {pids:?}"
    );
}

async fn wait_for_child_exit(child: &mut Child) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while child.try_wait().unwrap().is_none() && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(child.try_wait().unwrap().is_some(), "worker did not exit");
}

fn assert_kill_on_close(job: &WindowsJob) {
    use std::mem::size_of;
    use windows_sys::Win32::System::JobObjects::{
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JobObjectExtendedLimitInformation, QueryInformationJobObject,
    };

    let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
    let result = unsafe {
        QueryInformationJobObject(
            job.0,
            JobObjectExtendedLimitInformation,
            (&mut limits as *mut JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
            size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            std::ptr::null_mut(),
        )
    };
    assert_ne!(result, 0, "Job Object limit query failed");
    assert_ne!(
        limits.BasicLimitInformation.LimitFlags & JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        0,
        "worker Job Object is not configured to kill on close"
    );
}

fn process_is_in_any_job(handle: windows_sys::Win32::Foundation::HANDLE) -> bool {
    use windows_sys::Win32::System::{JobObjects::IsProcessInJob, Threading::GetCurrentProcess};

    let process = if handle.is_null() {
        unsafe { GetCurrentProcess() }
    } else {
        handle
    };
    let mut in_job = 0;
    let result = unsafe { IsProcessInJob(process, std::ptr::null_mut(), &mut in_job) };
    assert_ne!(result, 0, "IsProcessInJob failed for process");
    in_job != 0
}

#[tokio::test]
async fn windows_job_attach_sets_kill_on_close_for_descendants() {
    let (_temp, paths, mut child, job) = attached_fixture().await;
    assert_kill_on_close(&job);
    let pids = release_fixture(&paths).await;
    assert!(pids.iter().all(|pid| process_is_running(*pid)));

    drop(job);
    wait_for_child_exit(&mut child).await;
    assert_processes_dead(&pids).await;
}

async fn hold_process_until_dropped(_child: Child, _job: WindowsJob) {
    std::future::pending::<()>().await;
}

#[tokio::test]
async fn windows_job_cleanup_survives_caller_cancellation() {
    let (_temp, paths, child, job) = attached_fixture().await;
    let pids = release_fixture(&paths).await;
    let task = tokio::spawn(hold_process_until_dropped(child, job));
    task.abort();
    assert!(matches!(task.await, Err(error) if error.is_cancelled()));
    assert_processes_dead(&pids).await;
}

#[tokio::test]
async fn windows_job_cleanup_survives_a_deadline() {
    let (_temp, paths, child, job) = attached_fixture().await;
    let pids = release_fixture(&paths).await;
    let result = tokio::time::timeout(
        Duration::from_millis(100),
        hold_process_until_dropped(child, job),
    )
    .await;
    assert!(
        result.is_err(),
        "fixture unexpectedly completed before deadline"
    );
    assert_processes_dead(&pids).await;
}

#[tokio::test]
async fn windows_job_drop_reaps_an_idle_worker_tree() {
    let (_temp, paths, child, job) = attached_fixture().await;
    let pids = release_fixture(&paths).await;
    {
        let _job = job;
        let _child = child;
    }
    assert_processes_dead(&pids).await;
}

fn test_request() -> OfficialRequest {
    OfficialRequest::new(
        "hybrid-http-client".into(),
        "medium".into(),
        None,
        "auto".into(),
        "ch".into(),
        true,
        None,
        "auto".into(),
        None,
        None,
        None,
        None,
        1024,
    )
}

fn persistent_config() -> OfficialSessionConfig {
    OfficialSessionConfig::new("auto".into(), None, None, None, None).unwrap()
}

#[tokio::test]
async fn windows_attach_failure_cleans_live_per_document_worker() {
    let _failure = force_attach_failure();
    let worker = OfficialWorker::new(Some(python_path())).unwrap();
    let error = match worker
        .run(
            b"input",
            "pdf",
            test_request(),
            Instant::now() + Duration::from_secs(10),
        )
        .await
    {
        Ok(_) => panic!("per-document worker unexpectedly attached"),
        Err(error) => error,
    };
    assert!(
        error.contains("test attach failure"),
        "unexpected error: {error}"
    );
    assert_processes_dead(&[attach_failure_pid(&error)]).await;
}

#[tokio::test]
async fn windows_attach_failure_cleans_live_persistent_worker() {
    let _failure = force_attach_failure();
    let worker = OfficialPersistentWorker::new(Some(python_path()), persistent_config()).unwrap();
    let error = match worker
        .run(
            b"input",
            "pdf",
            test_request(),
            Instant::now() + Duration::from_secs(10),
        )
        .await
    {
        Ok(_) => panic!("persistent worker unexpectedly attached"),
        Err(error) => error,
    };
    worker.drain().await.unwrap();
    assert!(
        error.contains("test attach failure"),
        "unexpected error: {error}"
    );
    assert_processes_dead(&[attach_failure_pid(&error)]).await;
}

#[tokio::test]
async fn windows_job_attach_handles_a_process_already_in_a_job() {
    let (_temp, paths, mut child) = spawn_fixture();
    wait_for_file(&paths.ready).await;
    let child_pid = child.id().unwrap();
    let child_handle = child.raw_handle().unwrap();
    let test_process_in_job = process_is_in_any_job(std::ptr::null_mut());
    assert_eq!(
        process_is_in_any_job(child_handle),
        test_process_in_job,
        "child did not inherit the test process job membership"
    );

    match WindowsJob::attach(&child) {
        Ok(job) => {
            assert_kill_on_close(&job);
            let pids = release_fixture(&paths).await;
            drop(job);
            wait_for_child_exit(&mut child).await;
            assert_processes_dead(&pids).await;
        }
        Err(error) if test_process_in_job => {
            let _ = child.start_kill();
            wait_for_child_exit(&mut child).await;
            assert!(!process_is_running(child_pid));
            assert!(
                !error.is_empty(),
                "existing-job attach failure lost its diagnostic"
            );
        }
        Err(error) => panic!("uncontained test process could not attach worker: {error}"),
    }
}
