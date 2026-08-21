use std::{
    collections::VecDeque,
    future::Future,
    path::Path,
    pin::Pin,
    sync::{Arc, Mutex as StdMutex},
    task::{Context, Poll},
    time::Instant,
};

mod replay;

use futures_util::task::noop_waker_ref;
#[cfg(windows)]
use std::os::windows::process::CommandExt;
use tokio::{
    io::{AsyncRead, AsyncReadExt, BufReader, ReadBuf},
    process::{Child, ChildStdin, ChildStdout, Command},
    sync::{mpsc, oneshot, watch},
    task::JoinHandle,
};

use super::protocol::{
    PersistentFrame, PersistentResultFrame, parse_persistent_frame, persistent_request_frame,
    persistent_start_frame, read_persistent_frame, validate_persistent_error,
    write_persistent_frame,
};
use super::{OfficialRequest, OfficialSessionConfig};
#[cfg(unix)]
use crate::official_worker::process::ProcessGroup;
#[cfg(windows)]
use crate::official_worker::process::WindowsJob;
#[cfg(target_os = "linux")]
use crate::official_worker::process::install_parent_death_signal;
use crate::official_worker::{
    PERSISTENT_PROTOCOL, PythonShim, REAP_GRACE, STDERR_CAP,
    process::{copy_runtime_environment, with_diagnostic},
};
use replay::RecentRequestIds;

struct DiagnosticRing {
    bytes: VecDeque<u8>,
    truncated: bool,
}

impl DiagnosticRing {
    fn new() -> Self {
        Self {
            bytes: VecDeque::with_capacity(STDERR_CAP),
            truncated: false,
        }
    }

    fn push(&mut self, bytes: &[u8]) {
        for byte in bytes {
            if self.bytes.len() == STDERR_CAP {
                self.bytes.pop_front();
                self.truncated = true;
            }
            self.bytes.push_back(*byte);
        }
    }

    fn take(&mut self) -> Vec<u8> {
        let mut diagnostic: Vec<u8> = self.bytes.drain(..).collect();
        if self.truncated {
            const MARKER: &[u8] = b" [truncated]";
            let keep = STDERR_CAP - MARKER.len();
            let remove = diagnostic.len().saturating_sub(keep);
            if remove > 0 {
                diagnostic.drain(..remove);
            }
            diagnostic.extend_from_slice(MARKER);
            self.truncated = false;
        }
        diagnostic
    }
}

enum StderrCommand {
    Flush(oneshot::Sender<()>),
}

fn drain_ready_stderr(
    stderr: &mut tokio::process::ChildStderr,
    diagnostics: &Arc<StdMutex<DiagnosticRing>>,
    buffer: &mut [u8],
) {
    loop {
        let mut read_buffer = ReadBuf::new(buffer);
        let result = {
            let mut context = Context::from_waker(noop_waker_ref());
            Pin::new(&mut *stderr).poll_read(&mut context, &mut read_buffer)
        };
        match result {
            Poll::Ready(Ok(())) => {
                let bytes = read_buffer.filled();
                if bytes.is_empty() {
                    return;
                }
                if let Ok(mut ring) = diagnostics.lock() {
                    ring.push(bytes);
                }
            }
            Poll::Ready(Err(_)) | Poll::Pending => return,
        }
    }
}

async fn drain_persistent_stderr(
    mut stderr: tokio::process::ChildStderr,
    diagnostics: Arc<StdMutex<DiagnosticRing>>,
    mut commands: mpsc::Receiver<StderrCommand>,
) {
    let mut buffer = [0u8; 8192];
    let mut stderr_open = true;
    loop {
        if !stderr_open {
            match commands.recv().await {
                Some(StderrCommand::Flush(done)) => {
                    let _ = done.send(());
                }
                None => return,
            }
            continue;
        }
        tokio::select! {
            biased;
            command = commands.recv() => match command {
                Some(StderrCommand::Flush(done)) => {
                    drain_ready_stderr(&mut stderr, &diagnostics, &mut buffer);
                    let _ = done.send(());
                }
                None => return,
            },
            result = stderr.read(&mut buffer) => match result {
                Ok(0) | Err(_) => stderr_open = false,
                Ok(size) => {
                    if let Ok(mut ring) = diagnostics.lock() {
                        ring.push(&buffer[..size]);
                    }
                }
            },
        }
    }
}

async fn controlled_io<T, F>(
    future: F,
    deadline: Instant,
    cancel: &mut oneshot::Receiver<()>,
    shutdown: &mut watch::Receiver<bool>,
) -> Result<T, String>
where
    F: Future<Output = Result<T, String>>,
{
    if *shutdown.borrow() {
        return Err("official persistent worker is shutting down".into());
    }
    if Instant::now() >= deadline {
        return Err("official persistent worker deadline expired".into());
    }
    tokio::pin!(future);
    let sleep = tokio::time::sleep_until(tokio::time::Instant::from_std(deadline));
    tokio::pin!(sleep);
    tokio::select! {
        result = &mut future => result,
        _ = &mut *cancel => Err("official persistent worker request cancelled".into()),
        _ = shutdown.changed() => Err("official persistent worker is shutting down".into()),
        _ = &mut sleep => Err("official persistent worker deadline expired".into()),
    }
}

pub(super) struct PersistentChild {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    diagnostics: Arc<StdMutex<DiagnosticRing>>,
    stderr_commands: mpsc::Sender<StderrCommand>,
    stderr_task: Option<JoinHandle<()>>,
    sequence: u64,
    recent_request_ids: RecentRequestIds,
    #[cfg(unix)]
    process_group: ProcessGroup,
    #[cfg(windows)]
    _job: WindowsJob,
    _shim: PythonShim,
}

impl PersistentChild {
    pub(super) fn spawn(executable: &Path) -> Result<Self, String> {
        let shim = PythonShim::new()?;
        let mut command = Command::new(executable);
        command
            .arg(shim.path())
            .arg("--persistent")
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
            .map_err(|error| format!("official Python persistent worker unavailable: {error}"))?;
        #[cfg(unix)]
        let process_group = ProcessGroup::new(child.id());
        #[cfg(windows)]
        let job = WindowsJob::attach(&child).map_err(|error| {
            let _ = child.start_kill();
            error
        })?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| "official persistent stdin pipe unavailable".to_owned())?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| "official persistent stdout pipe unavailable".to_owned())?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| "official persistent stderr pipe unavailable".to_owned())?;
        let diagnostics = Arc::new(StdMutex::new(DiagnosticRing::new()));
        let (stderr_commands, commands) = mpsc::channel(1);
        let stderr_task = tokio::spawn(drain_persistent_stderr(
            stderr,
            diagnostics.clone(),
            commands,
        ));
        Ok(Self {
            _shim: shim,
            child,
            stdin,
            stdout: BufReader::new(stdout),
            diagnostics,
            stderr_commands,
            stderr_task: Some(stderr_task),
            sequence: 1,
            recent_request_ids: RecentRequestIds::new(),
            #[cfg(unix)]
            process_group,
            #[cfg(windows)]
            _job: job,
        })
    }

    pub(super) fn take_diagnostic(&self) -> Vec<u8> {
        self.diagnostics
            .lock()
            .map(|mut ring| ring.take())
            .unwrap_or_default()
    }

    async fn flush_stderr(
        &self,
        deadline: Instant,
        cancel: &mut oneshot::Receiver<()>,
        shutdown: &mut watch::Receiver<bool>,
    ) -> Result<(), String> {
        let (done, ready) = oneshot::channel();
        controlled_io(
            async {
                self.stderr_commands
                    .send(StderrCommand::Flush(done))
                    .await
                    .map_err(|_| "official persistent stderr drain stopped".to_owned())?;
                ready
                    .await
                    .map_err(|_| "official persistent stderr drain stopped".to_owned())
            },
            deadline,
            cancel,
            shutdown,
        )
        .await
    }

    pub(super) async fn handshake(
        &mut self,
        config: &OfficialSessionConfig,
        deadline: Instant,
        cancel: &mut oneshot::Receiver<()>,
        shutdown: &mut watch::Receiver<bool>,
    ) -> Result<(), String> {
        let frame = persistent_start_frame(config);
        controlled_io(
            write_persistent_frame(&mut self.stdin, &frame),
            deadline,
            cancel,
            shutdown,
        )
        .await?;
        let bytes = controlled_io(
            read_persistent_frame(&mut self.stdout),
            deadline,
            cancel,
            shutdown,
        )
        .await?;
        self.flush_stderr(deadline, cancel, shutdown).await?;
        match parse_persistent_frame(bytes.strip_suffix(b"\n").unwrap_or(&bytes))? {
            PersistentFrame::Handshake(frame) => {
                if frame.frame_type != "handshake"
                    || frame.protocol != PERSISTENT_PROTOCOL
                    || frame.status != "ready"
                    || frame.package_version != config.package_version
                    || frame.schema_version != config.schema_version
                    || frame.backend != config.backend
                    || frame.max_in_flight != 1
                    || frame.capabilities != super::protocol::persistent_capabilities()
                {
                    return Err(persistent_error_text(
                        "official persistent handshake mismatch".into(),
                        "handshake fields did not match the pinned protocol",
                        frame.diagnostic.as_deref(),
                        self.take_diagnostic(),
                    ));
                }
                let _ = frame.diagnostic;
                let _ = self.take_diagnostic();
                Ok(())
            }
            PersistentFrame::Error(frame) => {
                validate_persistent_error(&frame, config)?;
                Err(persistent_error_text(
                    "official persistent startup failed".into(),
                    &frame.error,
                    frame.diagnostic.as_deref(),
                    self.take_diagnostic(),
                ))
            }
            PersistentFrame::Result(_) => {
                Err("official persistent handshake frame expected".into())
            }
        }
    }

    pub(super) async fn request(
        &mut self,
        request: &OfficialRequest,
        config: &OfficialSessionConfig,
        deadline: Instant,
        cancel: &mut oneshot::Receiver<()>,
        shutdown: &mut watch::Receiver<bool>,
    ) -> Result<PersistentResultFrame, String> {
        if self
            .child
            .try_wait()
            .map_err(|error| format!("official persistent worker wait failed: {error}"))?
            .is_some()
        {
            return Err("official persistent worker exited before request".into());
        }
        if request.request_id.is_empty() {
            return Err("official persistent request id is empty".into());
        }
        if !self.recent_request_ids.insert(request.request_id.clone()) {
            return Err("official persistent request id was repeated".into());
        }
        let sequence = self.sequence;
        if let Ok(mut ring) = self.diagnostics.lock() {
            let _ = ring.take();
        }
        let frame = persistent_request_frame(request, config, sequence);
        controlled_io(
            write_persistent_frame(&mut self.stdin, &frame),
            deadline,
            cancel,
            shutdown,
        )
        .await?;
        let bytes = controlled_io(
            read_persistent_frame(&mut self.stdout),
            deadline,
            cancel,
            shutdown,
        )
        .await?;
        self.flush_stderr(deadline, cancel, shutdown).await?;
        let frame = match parse_persistent_frame(bytes.strip_suffix(b"\n").unwrap_or(&bytes))? {
            PersistentFrame::Result(frame) => frame,
            PersistentFrame::Error(frame) => {
                validate_persistent_error(&frame, config)?;
                return Err(persistent_error_text(
                    "official persistent protocol error".into(),
                    &frame.error,
                    frame.diagnostic.as_deref(),
                    self.take_diagnostic(),
                ));
            }
            PersistentFrame::Handshake(_) => {
                return Err("official persistent result frame expected".into());
            }
        };
        if frame.frame_type != "result"
            || frame.protocol != PERSISTENT_PROTOCOL
            || frame.request_id != request.request_id
            || frame.sequence != sequence
            || frame.package_version != config.package_version
            || frame.schema_version != config.schema_version
            || frame.backend != config.backend
            || frame.bundle_name != crate::hybrid_v4_output::BUNDLE_NAME
            || !matches!(frame.status.as_str(), "ok" | "error")
        {
            return Err("official persistent result mismatch".into());
        }
        if frame.status == "error" && frame.error.is_none() {
            return Err("official persistent result error has no message".into());
        }
        if self
            .child
            .try_wait()
            .map_err(|error| format!("official persistent worker wait failed: {error}"))?
            .is_some()
        {
            return Err("official persistent worker exited after request".into());
        }
        self.sequence = self.sequence.saturating_add(1);
        Ok(frame)
    }

    pub(super) async fn shutdown(mut self) {
        #[cfg(unix)]
        self.process_group.kill();
        let _ = self.child.start_kill();
        if let Some(task) = self.stderr_task.take() {
            task.abort();
            let _ = task.await;
        }
        let _ = tokio::time::timeout(REAP_GRACE, self.child.wait()).await;
    }
}

impl Drop for PersistentChild {
    fn drop(&mut self) {
        #[cfg(unix)]
        self.process_group.kill();
        let _ = self.child.start_kill();
        if let Some(task) = self.stderr_task.take() {
            task.abort();
        }
    }
}

pub(super) fn persistent_stderr_text(message: String, stderr: Vec<u8>) -> String {
    if stderr.is_empty() {
        message
    } else {
        with_diagnostic(message, Some(&stderr))
    }
}

pub(super) fn persistent_error_text(
    message: String,
    error: &str,
    inline_diagnostic: Option<&str>,
    stderr: Vec<u8>,
) -> String {
    let mut diagnostic = error.as_bytes().to_vec();
    if let Some(inline_diagnostic) = inline_diagnostic.filter(|value| !value.is_empty()) {
        diagnostic.extend_from_slice(b"\n");
        diagnostic.extend_from_slice(inline_diagnostic.as_bytes());
    }
    if !stderr.is_empty() {
        diagnostic.extend_from_slice(b"\n");
        diagnostic.extend_from_slice(&stderr);
    }
    with_diagnostic(message, Some(&diagnostic))
}

#[cfg(test)]
mod tests;
