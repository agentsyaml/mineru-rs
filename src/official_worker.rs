//! Per-document subprocess boundary for the pinned MinerU 4.0.0a6 parser.

use serde::{Deserialize, Serialize};
#[cfg(windows)]
use std::os::windows::process::CommandExt;
use std::{
    io::Write,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, Instant},
};
use tempfile::TempDir;
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWriteExt},
    process::Command,
    sync::OwnedSemaphorePermit,
    task::JoinHandle,
};

#[cfg(test)]
mod per_document_tests;
mod persistent;
mod process;

pub(crate) use persistent::{OfficialPersistentWorker, OfficialSessionConfig};

const PROTOCOL: &str = "mineru-rs-official-worker/1";
const PACKAGE_VERSION: &str = "4.0.0a6";
const SCHEMA_VERSION: &str = "1.0";
#[allow(dead_code)]
const PERSISTENT_PROTOCOL: &str = "mineru-rs-official-worker/2";
#[allow(dead_code)]
const PERSISTENT_BACKEND: &str = "hybrid-http-client";
const STDOUT_CAP: usize = 64 * 1024;
const STDERR_CAP: usize = 64 * 1024;
const REQUEST_CAP: usize = 64 * 1024;
const REAP_GRACE: Duration = Duration::from_millis(250);
const PYTHON_SHIM: &str = concat!(
    include_str!("../python/mineru_official_worker_protocol.py"),
    "\n",
    include_str!("../python/mineru_official_worker.py"),
);

struct PythonShim {
    _temporary: TempDir,
    path: PathBuf,
}

impl PythonShim {
    fn new() -> Result<Self, String> {
        let temporary = tempfile::tempdir()
            .map_err(|error| format!("official worker Python shim tempdir: {error}"))?;
        let path = temporary.path().join("official_worker.py");
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(|error| format!("official worker Python shim create: {error}"))?;
        file.write_all(PYTHON_SHIM.as_bytes())
            .map_err(|error| format!("official worker Python shim write: {error}"))?;
        file.flush()
            .map_err(|error| format!("official worker Python shim flush: {error}"))?;
        drop(file);
        Ok(Self {
            _temporary: temporary,
            path,
        })
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

#[allow(dead_code)]
const PERSISTENT_EFFORTS: &[&str] = &["medium", "high", "xhigh"];
#[allow(dead_code)]
const PERSISTENT_MODEL_STACKS: &[&str] = &["auto", "light", "full"];
#[allow(dead_code)]
const PERSISTENT_INPUT_FORMATS: &[&str] = &[
    "pdf", "png", "jpeg", "jpg", "jp2", "webp", "gif", "bmp", "tiff",
];

#[cfg(unix)]
use process::ProcessGroup;
#[cfg(windows)]
use process::WindowsJob;
#[cfg(target_os = "linux")]
use process::install_parent_death_signal;
use process::{
    copy_runtime_environment, official_executable, terminate, with_truncated_diagnostic,
};

#[derive(Clone, Debug, Serialize)]
pub(crate) struct OfficialRequest {
    pub(crate) protocol: &'static str,
    pub(crate) request_id: String,
    pub(crate) backend: String,
    pub(crate) effort: String,
    pub(crate) server_url: Option<String>,
    pub(crate) method: String,
    pub(crate) lang: String,
    pub(crate) image_analysis: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) page_range: Option<String>,
    pub(crate) model_stack: String,
    pub(crate) model_base_dir: Option<PathBuf>,
    pub(crate) config: Option<PathBuf>,
    pub(crate) vl_api_key: Option<String>,
    pub(crate) vl_model_name: Option<String>,
    pub(crate) max_bundle_bytes: u64,
    pub(crate) bundle_name: &'static str,
    pub(crate) input_path: PathBuf,
    pub(crate) bundle_path: PathBuf,
}

impl OfficialRequest {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        backend: String,
        effort: String,
        server_url: Option<String>,
        method: String,
        lang: String,
        image_analysis: bool,
        page_range: Option<String>,
        model_stack: String,
        model_base_dir: Option<PathBuf>,
        config: Option<PathBuf>,
        vl_api_key: Option<String>,
        vl_model_name: Option<String>,
        max_bundle_bytes: u64,
    ) -> Self {
        Self {
            protocol: PROTOCOL,
            request_id: next_request_id(),
            backend,
            effort,
            server_url,
            method,
            lang,
            image_analysis,
            page_range,
            model_stack,
            model_base_dir,
            config,
            vl_api_key,
            vl_model_name,
            max_bundle_bytes,
            bundle_name: crate::hybrid_v4_output::BUNDLE_NAME,
            input_path: PathBuf::new(),
            bundle_path: PathBuf::new(),
        }
    }
}

pub(crate) struct OfficialWorker {
    executable: PathBuf,
}

pub(crate) struct OfficialBundle {
    _temporary: TempDir,
    bundle: PathBuf,
    _permit: Option<OwnedSemaphorePermit>,
}

impl OfficialBundle {
    pub(crate) fn path(&self) -> &Path {
        &self.bundle
    }
}

impl OfficialWorker {
    pub(crate) fn new(executable: Option<PathBuf>) -> Result<Self, String> {
        Ok(Self {
            executable: official_executable(executable)?,
        })
    }

    pub(crate) async fn run(
        &self,
        input: &[u8],
        suffix: &str,
        mut request: OfficialRequest,
        deadline: Instant,
    ) -> Result<OfficialBundle, String> {
        if input.is_empty() {
            return Err("official Hybrid input is empty".into());
        }
        let temporary = tempfile::tempdir().map_err(|e| format!("official worker tempdir: {e}"))?;
        let input_path = temporary.path().join(format!("input.{suffix}"));
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&input_path)
            .map_err(|e| format!("official worker input snapshot: {e}"))?;
        file.write_all(input)
            .and_then(|_| file.flush())
            .map_err(|e| format!("official worker input snapshot: {e}"))?;
        let bundle = temporary.path().join(crate::hybrid_v4_output::BUNDLE_NAME);
        request.input_path = input_path;
        request.bundle_path = bundle.clone();
        let mut encoded = serde_json::to_vec(&request).map_err(|e| e.to_string())?;
        encoded.push(b'\n');
        if encoded.len() > REQUEST_CAP {
            return Err("official worker request exceeds protocol limit".into());
        }

        let shim = PythonShim::new()?;
        let mut command = Command::new(&self.executable);
        command
            .arg(shim.path())
            .current_dir(temporary.path())
            .env_clear()
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        copy_runtime_environment(&mut command);
        #[cfg(unix)]
        command.process_group(0);
        #[cfg(target_os = "linux")]
        install_parent_death_signal(&mut command);
        #[cfg(windows)]
        command.creation_flags(0x0000_0200);
        let mut child = command
            .spawn()
            .map_err(|e| format!("official Python worker unavailable: {e}"))?;
        #[cfg(unix)]
        let mut process_group = ProcessGroup::new(child.id());
        #[cfg(windows)]
        let _job = WindowsJob::attach(&child).map_err(|error| {
            let _ = child.start_kill();
            error
        })?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| "official worker stdin pipe unavailable".to_owned())?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| "official worker stdout pipe unavailable".to_owned())?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| "official worker stderr pipe unavailable".to_owned())?;
        let mut input_task = Some(tokio::spawn(write_request(stdin, encoded)));
        let mut stdout_task = Some(tokio::spawn(read_capped(stdout, STDOUT_CAP)));
        let mut stderr_task = Some(tokio::spawn(read_diagnostic(stderr, STDERR_CAP)));
        let mut status = None;
        let mut stdout_bytes = None;
        let mut stderr_bytes = None;
        let mut failure = None;
        let sleep = tokio::time::sleep_until(tokio::time::Instant::from_std(deadline));
        tokio::pin!(sleep);
        loop {
            tokio::select! {
                result = child.wait(), if status.is_none() => {
                    match result {
                        Ok(value) => {
                            status = Some(value);
                            #[cfg(unix)] process_group.kill();
                        }
                        Err(error) => failure = Some(format!("official worker wait failed: {error}")),
                    }
                }
                result = async { input_task.as_mut().expect("input task exists").await }, if input_task.is_some() => {
                    input_task = None;
                    if !matches!(result, Ok(Ok(()))) {
                        failure = Some("official worker stdin failed".into());
                    }
                }
                result = async { stdout_task.as_mut().expect("stdout task exists").await }, if stdout_task.is_some() => {
                    stdout_task = None;
                    match result {
                        Ok(Ok(bytes)) => stdout_bytes = Some(bytes),
                        Ok(Err(ReadError::TooLarge)) => failure = Some("official worker stdout exceeded its cap".into()),
                        _ => failure = Some("official worker stdout failed".into()),
                    }
                }
                result = async { stderr_task.as_mut().expect("stderr task exists").await }, if stderr_task.is_some() => {
                    stderr_task = None;
                    match result {
                        Ok(Ok(bytes)) => stderr_bytes = Some(bytes),
                        _ => failure = Some("official worker stderr failed".into()),
                    }
                }
                _ = &mut sleep => failure = Some("official worker deadline expired".into()),
            }
            if failure.is_some()
                || (status.is_some()
                    && input_task.is_none()
                    && stdout_task.is_none()
                    && stderr_task.is_none())
            {
                break;
            }
        }
        if let Some(error) = failure {
            abort(&mut input_task, &mut stdout_task, &mut stderr_task);
            #[cfg(unix)]
            process_group.kill();
            terminate(&mut child).await;
            return Err(with_stderr_diagnostic(error, stderr_bytes.as_ref()));
        }
        if !status.is_some_and(|status| status.success()) {
            return Err(with_stderr_diagnostic(
                "official worker exited unsuccessfully".into(),
                stderr_bytes.as_ref(),
            ));
        }
        let response: Response = serde_json::from_slice(
            stdout_bytes
                .as_deref()
                .ok_or_else(|| "official worker returned no protocol response".to_owned())?,
        )
        .map_err(|e| format!("official worker protocol JSON is invalid: {e}"))?;
        if response.protocol != PROTOCOL {
            return Err("official worker protocol version mismatch".into());
        }
        if response.request_id != request.request_id {
            return Err("official worker request id mismatch".into());
        }
        if response.package_version != PACKAGE_VERSION {
            return Err("official worker MinerU package version is not 4.0.0a6".into());
        }
        if response.schema_version != SCHEMA_VERSION {
            return Err("official worker result schema version mismatch".into());
        }
        if response.backend != request.backend {
            return Err("official worker backend mismatch".into());
        }
        if response.bundle_name != crate::hybrid_v4_output::BUNDLE_NAME {
            return Err("official worker bundle name mismatch".into());
        }
        if response.status != "ok" {
            return Err(with_stderr_diagnostic(
                response
                    .error
                    .unwrap_or_else(|| "official worker failed".into()),
                stderr_bytes.as_ref(),
            ));
        }
        #[cfg(unix)]
        process_group.disarm();
        Ok(OfficialBundle {
            _temporary: temporary,
            bundle,
            _permit: None,
        })
    }
}

#[derive(Debug, Deserialize)]
struct Response {
    protocol: String,
    request_id: String,
    status: String,
    package_version: String,
    schema_version: String,
    backend: String,
    bundle_name: String,
    error: Option<String>,
}

async fn write_request(mut stdin: tokio::process::ChildStdin, request: Vec<u8>) -> Result<(), ()> {
    stdin.write_all(&request).await.map_err(|_| ())?;
    stdin.shutdown().await.map_err(|_| ())
}

#[derive(Debug)]
enum ReadError {
    TooLarge,
    Io,
}

struct Diagnostic {
    bytes: Vec<u8>,
    truncated: bool,
}

async fn read_capped(mut reader: impl AsyncRead + Unpin, cap: usize) -> Result<Vec<u8>, ReadError> {
    let mut result = Vec::new();
    let mut buffer = [0u8; 8192];
    loop {
        let read = reader.read(&mut buffer).await.map_err(|_| ReadError::Io)?;
        if read == 0 {
            return Ok(result);
        }
        if result.len().checked_add(read).is_none_or(|size| size > cap) {
            return Err(ReadError::TooLarge);
        }
        result.extend_from_slice(&buffer[..read]);
    }
}

async fn read_diagnostic(
    mut reader: impl AsyncRead + Unpin,
    cap: usize,
) -> Result<Diagnostic, ReadError> {
    let mut bytes = Vec::with_capacity(cap);
    let mut truncated = false;
    let mut buffer = [0u8; 8192];
    loop {
        let read = reader.read(&mut buffer).await.map_err(|_| ReadError::Io)?;
        if read == 0 {
            return Ok(Diagnostic { bytes, truncated });
        }
        let remaining = cap.saturating_sub(bytes.len());
        let retained = read.min(remaining);
        bytes.extend_from_slice(&buffer[..retained]);
        truncated |= retained < read;
    }
}

fn with_stderr_diagnostic(message: String, diagnostic: Option<&Diagnostic>) -> String {
    let Some(diagnostic) = diagnostic else {
        return message;
    };
    with_truncated_diagnostic(message, Some(&diagnostic.bytes), diagnostic.truncated)
}

fn abort(
    input: &mut Option<JoinHandle<Result<(), ()>>>,
    stdout: &mut Option<JoinHandle<Result<Vec<u8>, ReadError>>>,
    stderr: &mut Option<JoinHandle<Result<Diagnostic, ReadError>>>,
) {
    if let Some(task) = input.take() {
        task.abort();
    }
    if let Some(task) = stdout.take() {
        task.abort();
    }
    if let Some(task) = stderr.take() {
        task.abort();
    }
}

fn next_request_id() -> String {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    format!(
        "{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    )
}
