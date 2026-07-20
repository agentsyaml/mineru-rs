//! Bounded subprocess isolation for the Office converter helper.
use bytes::Bytes;
#[cfg(test)]
use std::sync::atomic::{AtomicUsize, Ordering};
use std::{collections::HashMap, ffi::OsString, path::PathBuf, sync::Arc, time::Duration};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWriteExt},
    process::Command,
    sync::{Mutex, oneshot, watch},
    task::{JoinHandle, JoinSet},
};

#[cfg(test)]
#[derive(Default)]
struct TestProbe {
    waited: AtomicUsize,
    detached: AtomicUsize,
    stdin: AtomicUsize,
    stdout: AtomicUsize,
    stderr: AtomicUsize,
}

const INPUT_CAP: usize = 32 * 1024 * 1024;
const OUTPUT_CAP: usize = 64 * 1024 * 1024;
const STDERR_CAP: usize = 4096;
// Only the direct child is reaped. After this grace, cleanup continues detached so drain stays bounded.
const CHILD_REAP_GRACE: Duration = Duration::from_millis(250);

#[derive(Debug, thiserror::Error)]
#[doc(hidden)]
pub enum OfficeConvertError {
    #[error("office conversion is unavailable")]
    Unavailable,
    #[error("office conversion is shutting down")]
    Draining,
    #[error("office conversion failed: {0}")]
    Failed(String),
}

struct State {
    accepting: bool,
    next: u64,
    owners: JoinSet<()>,
    cancels: HashMap<u64, watch::Sender<bool>>,
}
#[derive(Clone)]
#[doc(hidden)]
pub struct OfficeWorkers {
    executable: PathBuf,
    prefix: Vec<OsString>,
    state: Arc<Mutex<State>>,
    #[cfg(test)]
    probe: Arc<TestProbe>,
    #[cfg(test)]
    ready: Option<PathBuf>,
}
struct CancelGuard(Option<watch::Sender<bool>>);
impl CancelGuard {
    fn disarm(&mut self) {
        self.0.take();
    }
}
impl Drop for CancelGuard {
    fn drop(&mut self) {
        if let Some(cancel) = self.0.take() {
            let _ = cancel.send(true);
        }
    }
}

impl OfficeWorkers {
    #[doc(hidden)]
    pub fn new() -> Result<Self, OfficeConvertError> {
        let executable = std::env::current_exe()
            .map_err(|_| OfficeConvertError::Unavailable)?
            .with_file_name(if cfg!(windows) {
                "mineru-office-convert.exe"
            } else {
                "mineru-office-convert"
            });
        Self::with_executable(executable)
    }
    #[doc(hidden)]
    pub fn with_executable(executable: PathBuf) -> Result<Self, OfficeConvertError> {
        Ok(Self {
            executable,
            prefix: Vec::new(),
            state: Arc::new(Mutex::new(State {
                accepting: true,
                next: 0,
                owners: JoinSet::new(),
                cancels: HashMap::new(),
            })),
            #[cfg(test)]
            probe: Arc::new(TestProbe::default()),
            #[cfg(test)]
            ready: None,
        })
    }
    #[cfg(test)]
    pub(crate) fn with_test_executable(executable: PathBuf) -> Self {
        Self {
            executable,
            prefix: vec![
                "--exact".into(),
                "office_workers::tests::fake_child".into(),
                "--nocapture".into(),
            ],
            state: Arc::new(Mutex::new(State {
                accepting: true,
                next: 0,
                owners: JoinSet::new(),
                cancels: HashMap::new(),
            })),
            probe: Arc::new(TestProbe::default()),
            ready: None,
        }
    }
    #[doc(hidden)]
    pub async fn convert(
        &self,
        format: &'static str,
        input: impl Into<Bytes>,
        timeout: Duration,
    ) -> Result<Vec<u8>, OfficeConvertError> {
        self.convert_with_warning(format, input, timeout)
            .await
            .map(|(pdf, _)| pdf)
    }
    pub(crate) async fn convert_with_warning(
        &self,
        format: &'static str,
        input: impl Into<Bytes>,
        timeout: Duration,
    ) -> Result<(Vec<u8>, Option<String>), OfficeConvertError> {
        let input = input.into();
        if input.len() > INPUT_CAP {
            return Err(OfficeConvertError::Failed("input too large".into()));
        }
        if timeout.is_zero() {
            return Err(OfficeConvertError::Failed("invalid timeout".into()));
        }
        let (result_tx, result_rx) = oneshot::channel();
        let (cancel_tx, cancel_rx) = watch::channel(false);
        {
            let mut state = self.state.lock().await;
            while state.owners.try_join_next().is_some() {}
            if !state.accepting {
                return Err(OfficeConvertError::Draining);
            }
            let id = state.next;
            state.next = state.next.wrapping_add(1);
            state.cancels.insert(id, cancel_tx.clone());
            let executable = self.executable.clone();
            let prefix = self.prefix.clone();
            let state_ref = self.state.clone();
            #[cfg(test)]
            let probe = self.probe.clone();
            #[cfg(test)]
            let ready = self.ready.clone();
            state.owners.spawn(async move {
                #[cfg(test)]
                let result = owner(
                    executable, prefix, format, input, timeout, cancel_rx, probe, ready,
                )
                .await;
                #[cfg(not(test))]
                let result = owner(executable, prefix, format, input, timeout, cancel_rx).await;
                let _ = result_tx.send(result);
                state_ref.lock().await.cancels.remove(&id);
            });
            id
        };
        let mut guard = CancelGuard(Some(cancel_tx));
        let result = result_rx
            .await
            .map_err(|_| OfficeConvertError::Failed("owner stopped".into()))?;
        guard.disarm();
        result
    }
    #[doc(hidden)]
    pub async fn drain(&self) {
        let mut state = self.state.lock().await;
        state.accepting = false;
        for (_, cancel) in std::mem::take(&mut state.cancels) {
            let _ = cancel.send(true);
        }
        let mut owners = std::mem::replace(&mut state.owners, JoinSet::new());
        drop(state);
        while owners.join_next().await.is_some() {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lopdf::{Dictionary, Document, Object, Stream, dictionary};
    use std::{
        io::{Read, Write},
        sync::{Arc, Barrier},
    };

    fn tiny_pdf() -> Vec<u8> {
        let mut document = Document::with_version("1.4");
        let pages = document.new_object_id();
        let page = document.new_object_id();
        let font = document.add_object(
            dictionary! { "Type" => "Font", "Subtype" => "Type1", "BaseFont" => "Helvetica" },
        );
        let resources = document.add_object(dictionary! { "Font" => dictionary! { "F1" => font } });
        let contents = document.add_object(Stream::new(
            Dictionary::new(),
            b"BT /F1 12 Tf 20 40 Td (office) Tj ET".to_vec(),
        ));
        document.objects.insert(
            pages,
            Object::Dictionary(
                dictionary! {"Type" => "Pages", "Kids" => vec![page.into()], "Count" => 1},
            ),
        );
        document.objects.insert(page, Object::Dictionary(dictionary! {"Type" => "Page", "Parent" => pages, "MediaBox" => vec![0.into(), 0.into(), 200.into(), 300.into()], "Resources" => resources, "Contents" => contents}));
        let root = document.new_object_id();
        document.objects.insert(
            root,
            Object::Dictionary(dictionary! {"Type" => "Catalog", "Pages" => pages}),
        );
        document.trailer.set("Root", root);
        let mut bytes = Vec::new();
        document.save_to(&mut bytes).unwrap();
        bytes
    }

    // This is invoked by the current test executable via with_test_executable.
    #[test]
    fn fake_child() {
        if std::env::var_os("MINERU_OFFICE_FAKE_CHILD").is_none() {
            return;
        }
        let mode = std::env::var("MINERU_OFFICE_FAKE_MODE").unwrap_or_default();
        match mode.as_str() {
            "abort_activity" => {
                let barrier = Arc::new(Barrier::new(4));
                let stdin_barrier = barrier.clone();
                let _in = std::thread::spawn(move || {
                    let mut byte = [0];
                    assert_eq!(std::io::stdin().read(&mut byte).unwrap(), 1);
                    stdin_barrier.wait();
                    loop {
                        let _ = std::io::stdin().read(&mut [0; 8192]);
                    }
                });
                let out_barrier = barrier.clone();
                let _out = std::thread::spawn(move || {
                    std::io::stdout().write_all(b"activity\n").unwrap();
                    std::io::stdout().flush().unwrap();
                    out_barrier.wait();
                    loop {
                        let _ = std::io::stdout().write_all(b"activity\n");
                        let _ = std::io::stdout().flush();
                        std::thread::sleep(Duration::from_millis(5));
                    }
                });
                let err_barrier = barrier.clone();
                let _err = std::thread::spawn(move || {
                    std::io::stderr().write_all(b"activity\n").unwrap();
                    std::io::stderr().flush().unwrap();
                    err_barrier.wait();
                    loop {
                        let _ = std::io::stderr().write_all(b"activity\n");
                        let _ = std::io::stderr().flush();
                        std::thread::sleep(Duration::from_millis(5));
                    }
                });
                barrier.wait();
                if let Some(path) = std::env::var_os("MINERU_OFFICE_FAKE_READY") {
                    std::fs::write(path, b"ready").unwrap();
                }
                loop {
                    std::thread::sleep(Duration::from_secs(1));
                }
            }
            "unicode_invalid_stderr" => {
                for _ in 0..(STDERR_CAP * 2) {
                    let _ = std::io::stderr().write_all("€".as_bytes());
                }
                let _ = std::io::stderr().write_all(&[0xff, 0, 0x80]);
                std::process::exit(7);
            }
            "signature_only" => {
                let _ = std::io::stdout().write_all(b"%PDF-1.4\nok");
                std::process::exit(0);
            }
            "hang" => {
                eprintln!("entered");
                std::thread::sleep(Duration::from_secs(30));
            }
            "pipe_failure" | "reap_timeout" | "reap_wait_error" => {
                std::thread::sleep(Duration::from_secs(30));
            }
            "child_exit_pipe_hang" | "normal_completion_delayed_pipe" => {
                let _ = std::io::stdout().write_all(&tiny_pdf());
                std::process::exit(0);
            }
            "crash" => {
                eprintln!("bad\x00diagnostic {}", "x".repeat(STDERR_CAP * 2));
                std::process::exit(7);
            }
            "child_failure_stderr_hang" => {
                eprintln!("bad diagnostic held by reader");
                std::process::exit(7);
            }
            "largeout" => {
                let _ = std::io::stdout().write_all(&vec![b'x'; OUTPUT_CAP + 1]);
                std::thread::sleep(Duration::from_secs(30));
            }
            "errlarge" => {
                let _ = std::io::stderr().write_all(&vec![b'e'; STDERR_CAP * 4]);
                let _ = std::io::stdout().write_all(&tiny_pdf());
                std::process::exit(0);
            }
            "warning" => {
                eprint!("Bearer secret https://example.test/a\nwarning\t\0");
                let _ = std::io::stdout().write_all(&tiny_pdf());
                std::process::exit(0);
            }
            _ => {
                let mut input = Vec::new();
                let _ = std::io::stdin().read_to_end(&mut input);
                eprintln!("simultaneous stderr");
                let _ = std::io::stdout().write_all(&tiny_pdf());
                std::process::exit(0);
            }
        }
    }

    fn workers() -> OfficeWorkers {
        OfficeWorkers::with_test_executable(std::env::current_exe().unwrap())
    }
    async fn convert(w: &OfficeWorkers, mode: &'static str) -> Result<Vec<u8>, OfficeConvertError> {
        w.convert(mode, b"input".to_vec(), Duration::from_secs(5))
            .await
    }

    #[tokio::test]
    async fn valid_pdf_with_concurrent_pipes_succeeds() {
        assert!(
            convert(&workers(), "ok")
                .await
                .unwrap()
                .starts_with(b"%PDF-")
        );
    }

    #[tokio::test]
    async fn successful_warning_is_sanitized_and_convert_discards_it() {
        let w = workers();
        let (pdf, warning) = w
            .convert_with_warning("warning", b"input".to_vec(), Duration::from_secs(5))
            .await
            .unwrap();
        let warning = warning.unwrap();
        assert!(pdf.starts_with(b"%PDF-") && warning.len() <= STDERR_CAP);
        assert!(!warning.contains("secret") && !warning.contains("example.test"));
        assert!(!warning.chars().any(char::is_control));
        assert!(
            w.convert("warning", b"input".to_vec(), Duration::from_secs(5))
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn deadline_and_caller_abort_are_reaped_by_drain() {
        let w = workers();
        let start = tokio::time::Instant::now();
        assert!(
            w.convert("hang", vec![1], Duration::from_millis(100))
                .await
                .is_err()
        );
        assert!(start.elapsed() < Duration::from_secs(2));
        let task = tokio::spawn({
            let w = w.clone();
            async move { convert(&w, "hang").await }
        });
        tokio::time::sleep(Duration::from_millis(100)).await;
        task.abort();
        w.drain().await;
        assert!(w.probe.waited.load(Ordering::Relaxed) >= 2);
    }

    #[tokio::test]
    async fn child_exit_does_not_complete_hung_pipe() {
        let w = workers();
        let start = tokio::time::Instant::now();
        let error = w
            .convert("child_exit_pipe_hang", vec![], Duration::from_millis(100))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("timed out"));
        assert!(start.elapsed() < Duration::from_secs(1));
    }

    #[tokio::test]
    async fn pipe_failure_has_bounded_cleanup() {
        let w = workers();
        let start = tokio::time::Instant::now();
        let error = w
            .convert("pipe_failure", vec![], Duration::from_secs(5))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("pipe failure"));
        assert!(start.elapsed() < Duration::from_secs(1));
    }

    #[tokio::test]
    async fn inner_wait_error_detaches_without_replacing_pipe_failure() {
        let w = workers();
        let error = w
            .convert("reap_wait_error", vec![], Duration::from_secs(5))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("pipe failure"));
        tokio::time::timeout(Duration::from_secs(1), w.drain())
            .await
            .expect("drain must not own the best-effort reaper");
        assert_eq!(w.probe.detached.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn normal_completion_waits_for_every_pipe() {
        let w = workers();
        let start = tokio::time::Instant::now();
        w.convert(
            "normal_completion_delayed_pipe",
            vec![],
            Duration::from_secs(5),
        )
        .await
        .unwrap();
        assert!(start.elapsed() >= Duration::from_millis(100));
        assert_eq!(w.probe.stdin.load(Ordering::Relaxed), 1);
        assert_eq!(w.probe.stdout.load(Ordering::Relaxed), 1);
        assert_eq!(w.probe.stderr.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn deadline_and_cancellation_cannot_block_drain_on_reap() {
        let w = workers();
        assert!(
            w.convert("reap_timeout", vec![], Duration::from_millis(50))
                .await
                .is_err()
        );

        let task = tokio::spawn({
            let w = w.clone();
            async move {
                w.convert("reap_timeout", vec![], Duration::from_secs(5))
                    .await
            }
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        task.abort();
        tokio::time::timeout(Duration::from_secs(1), w.drain())
            .await
            .expect("drain must not wait for detached reapers");
    }

    #[tokio::test]
    async fn crash_and_stderr_overflow_have_bounded_diagnostics() {
        let error = convert(&workers(), "crash").await.unwrap_err().to_string();
        assert!(error.contains("bad") && error.contains("diagnostic"));
        assert!(error.len() <= STDERR_CAP + 64 && !error.contains('\0'));
        assert!(!error.contains("secret"));
        assert!(
            convert(&workers(), "errlarge")
                .await
                .unwrap()
                .starts_with(b"%PDF-")
        );
    }
    #[tokio::test]
    async fn nonzero_status_never_waits_unbounded_for_stderr() {
        let w = workers();
        let start = tokio::time::Instant::now();
        let error = convert(&w, "child_failure_stderr_hang").await.unwrap_err();
        assert!(error.to_string().contains("child failed"));
        assert!(start.elapsed() < Duration::from_secs(1));
    }
    #[tokio::test]
    async fn invalid_multibyte_stderr_is_bounded_and_safe() {
        let error = convert(&workers(), "unicode_invalid_stderr")
            .await
            .unwrap_err();
        let OfficeConvertError::Failed(message) = error else {
            panic!("wrong error")
        };
        assert!(!message.is_empty() && message.len() <= STDERR_CAP);
        assert!(!message.chars().any(|c| c.is_control()));
    }

    #[tokio::test]
    async fn output_cap_kills_without_waiting_for_deadline() {
        let w = workers();
        let start = tokio::time::Instant::now();
        assert!(
            w.convert("largeout", vec![], Duration::from_secs(10))
                .await
                .is_err()
        );
        assert!(start.elapsed() < Duration::from_secs(3));
    }

    #[tokio::test]
    async fn rejects_invalid_requests_and_drain_race() {
        let w = workers();
        assert!(
            w.convert("ok", vec![0; INPUT_CAP + 1], Duration::from_secs(1))
                .await
                .is_err()
        );
        assert!(w.convert("ok", vec![], Duration::ZERO).await.is_err());
        let unavailable =
            OfficeWorkers::with_executable(PathBuf::from("definitely-not-an-executable")).unwrap();
        assert!(convert(&unavailable, "ok").await.is_err());
        w.drain().await;
        assert!(matches!(
            convert(&w, "ok").await,
            Err(OfficeConvertError::Draining)
        ));
    }

    #[tokio::test]
    async fn early_pipe_completion_does_not_panic_and_completed_owner_is_reaped() {
        let w = workers();
        convert(&w, "early").await.unwrap();
        convert(&w, "ok").await.unwrap();
        w.drain().await;
    }
    #[tokio::test]
    async fn signature_only_pdf_is_rejected() {
        assert!(convert(&workers(), "signature_only").await.is_err());
    }
    #[tokio::test]
    async fn abort_after_activity_reaps_everything_once() {
        let mut w = workers();
        let ready = tempfile::NamedTempFile::new().unwrap();
        let path = ready.path().to_path_buf();
        std::fs::remove_file(&path).unwrap();
        w.ready = Some(path.clone());
        let task = tokio::spawn({
            let w = w.clone();
            async move {
                w.convert(
                    "abort_activity",
                    vec![7; 1024 * 1024],
                    Duration::from_secs(5),
                )
                .await
            }
        });
        tokio::time::timeout(Duration::from_secs(2), async {
            while !path.exists() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        task.abort();
        w.drain().await;
        assert_eq!(w.probe.waited.load(Ordering::Relaxed), 1);
        assert_eq!(w.probe.stdin.load(Ordering::Relaxed), 1);
        assert_eq!(w.probe.stdout.load(Ordering::Relaxed), 1);
        assert_eq!(w.probe.stderr.load(Ordering::Relaxed), 1);
    }
    #[tokio::test]
    async fn overflow_timeout_rejects_before_spawning() {
        let w = workers();
        assert!(w.convert("ok", vec![], Duration::MAX).await.is_err());
        assert_eq!(w.probe.waited.load(Ordering::Relaxed), 0);
        assert_eq!(w.probe.stdin.load(Ordering::Relaxed), 0);
        assert_eq!(w.probe.stdout.load(Ordering::Relaxed), 0);
        assert_eq!(w.probe.stderr.load(Ordering::Relaxed), 0);
    }
}

async fn owner(
    executable: PathBuf,
    prefix: Vec<OsString>,
    format: &'static str,
    input: Bytes,
    timeout: Duration,
    mut cancel: watch::Receiver<bool>,
    #[cfg(test)] probe: Arc<TestProbe>,
    #[cfg(test)] ready: Option<PathBuf>,
) -> Result<(Vec<u8>, Option<String>), OfficeConvertError> {
    let deadline = tokio::time::Instant::now()
        .checked_add(timeout)
        .ok_or_else(|| OfficeConvertError::Failed("invalid timeout".into()))?;
    let mut command = Command::new(executable);
    #[cfg(test)]
    let fake_child = !prefix.is_empty();
    command.args(prefix);
    #[cfg(test)]
    if fake_child {
        command.env("MINERU_OFFICE_FAKE_MODE", format);
        if let Some(path) = ready {
            command.env("MINERU_OFFICE_FAKE_READY", path);
        }
    } else {
        command.arg(format);
    }
    #[cfg(not(test))]
    command.arg(format);
    command
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    #[cfg(test)]
    command.env("MINERU_OFFICE_FAKE_CHILD", "1");
    let mut child = command
        .spawn()
        .map_err(|_| OfficeConvertError::Unavailable)?;
    let stdin = child.stdin.take();
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let (mut stdin, mut stdout, mut stderr) = match (stdin, stdout, stderr) {
        (Some(stdin), Some(stdout), Some(stderr)) => (stdin, stdout, stderr),
        (stdin, stdout, stderr) => {
            drop(stdin);
            drop(stdout);
            drop(stderr);
            let _reap = kill_and_reap(child, false, ReapMode::Normal).await;
            #[cfg(test)]
            match _reap {
                ReapOutcome::Reaped => {
                    probe.waited.fetch_add(1, Ordering::Relaxed);
                }
                ReapOutcome::Detached => {
                    probe.detached.fetch_add(1, Ordering::Relaxed);
                }
                ReapOutcome::AlreadyReaped => {}
            }
            return Err(OfficeConvertError::Failed("pipe unavailable".into()));
        }
    };
    let mut write: Option<JoinHandle<Result<(), ()>>> = Some(tokio::spawn(async move {
        stdin.write_all(&input).await.map_err(|_| ())?;
        stdin.shutdown().await.map_err(|_| ())
    }));
    let mut out = Some(tokio::spawn(async move {
        #[cfg(test)]
        if matches!(format, "pipe_failure" | "reap_wait_error") {
            return Err(ReadCapError::Io);
        }
        let result = read_stdout_cap(&mut stdout, OUTPUT_CAP).await;
        #[cfg(test)]
        match format {
            "child_exit_pipe_hang" => std::future::pending().await,
            "normal_completion_delayed_pipe" => {
                tokio::time::sleep(Duration::from_millis(100)).await
            }
            _ => {}
        }
        result
    }));
    let mut err = Some(tokio::spawn(async move {
        let result = read_stderr_cap(&mut stderr, STDERR_CAP).await;
        #[cfg(test)]
        if format == "child_failure_stderr_hang" {
            std::future::pending().await
        }
        result
    }));
    let mut status = None;
    let mut child_reaped = false;
    let mut output = None;
    let mut diagnostic = None;
    let mut failure = None;
    let mut child_failed = false;

    loop {
        tokio::select! {
            result = child.wait(), if status.is_none() => {
                #[cfg(test)] probe.waited.fetch_add(1, Ordering::Relaxed);
                match result {
                    Ok(value) => {
                        child_reaped = true;
                        status = Some(value);
                    }
                    Err(_) => {
                        failure = Some("child failed");
                    }
                }
            },
            result = async { write.as_mut().expect("guarded writer").await }, if write.is_some() => {
                write = None;
                #[cfg(test)] probe.stdin.fetch_add(1, Ordering::Relaxed);
                if !matches!(result, Ok(Ok(()))) {
                    failure = Some("pipe failure");
                }
            },
            result = async { out.as_mut().expect("guarded stdout reader").await }, if out.is_some() => {
                out = None;
                #[cfg(test)] probe.stdout.fetch_add(1, Ordering::Relaxed);
                match result {
                    Ok(Ok(value)) => output = Some(value),
                    Ok(Err(ReadCapError::TooLarge)) => failure = Some("output too large"),
                    _ => failure = Some("pipe failure"),
                }
            },
            result = async { err.as_mut().expect("guarded stderr reader").await }, if err.is_some() => {
                err = None;
                #[cfg(test)] probe.stderr.fetch_add(1, Ordering::Relaxed);
                match result {
                    Ok(Ok(value)) => diagnostic = Some(value.bytes),
                    _ => failure = Some("pipe failure"),
                }
            },
            _ = cancel.changed() => failure = Some("cancelled"),
            _ = tokio::time::sleep_until(deadline) => failure = Some("timed out"),
        }
        if status.as_ref().is_some_and(|value| !value.success()) {
            child_failed = true;
            failure = Some("child failed");
        }
        if failure.is_some()
            || (status.is_some() && write.is_none() && out.is_none() && err.is_none())
        {
            break;
        }
    }

    if child_failed {
        if let Some(task) = write.take() {
            task.abort();
            #[cfg(test)]
            probe.stdin.fetch_add(1, Ordering::Relaxed);
        }
        if let Some(task) = out.take() {
            task.abort();
            #[cfg(test)]
            probe.stdout.fetch_add(1, Ordering::Relaxed);
        }
        if let Some(reader) = err.as_mut() {
            let diagnostic_deadline = deadline.min(tokio::time::Instant::now() + CHILD_REAP_GRACE);
            tokio::select! {
                result = reader => {
                    err = None;
                    #[cfg(test)] probe.stderr.fetch_add(1, Ordering::Relaxed);
                    if let Ok(Ok(value)) = result {
                        diagnostic = Some(value.bytes);
                    }
                },
                _ = cancel.changed() => {},
                _ = tokio::time::sleep_until(diagnostic_deadline) => {},
            }
        }
    }

    if let Some(message) = failure {
        if let Some(task) = write.take() {
            task.abort();
            #[cfg(test)]
            probe.stdin.fetch_add(1, Ordering::Relaxed);
        }
        if let Some(task) = out.take() {
            task.abort();
            #[cfg(test)]
            probe.stdout.fetch_add(1, Ordering::Relaxed);
        }
        if let Some(task) = err.take() {
            task.abort();
            #[cfg(test)]
            probe.stderr.fetch_add(1, Ordering::Relaxed);
        }
        #[cfg(test)]
        let reap_mode = match format {
            "reap_timeout" => ReapMode::Timeout,
            "reap_wait_error" => ReapMode::WaitError,
            _ => ReapMode::Normal,
        };
        #[cfg(not(test))]
        let reap_mode = ReapMode::Normal;
        let _reap = kill_and_reap(child, child_reaped, reap_mode).await;
        #[cfg(test)]
        match _reap {
            ReapOutcome::Reaped => {
                probe.waited.fetch_add(1, Ordering::Relaxed);
            }
            ReapOutcome::Detached => {
                probe.detached.fetch_add(1, Ordering::Relaxed);
            }
            ReapOutcome::AlreadyReaped => {}
        }
        return Err(OfficeConvertError::Failed(if child_failed {
            sanitize(diagnostic.as_deref().unwrap_or_default())
        } else {
            message.into()
        }));
    }

    if !status.as_ref().is_some_and(|value| value.success()) {
        return Err(OfficeConvertError::Failed(sanitize(
            diagnostic.as_deref().unwrap_or_default(),
        )));
    }
    let output = output.unwrap_or_default();
    #[cfg(test)]
    let output = if fake_child {
        output
            .windows(5)
            .position(|window| window == b"%PDF-")
            .map_or(output.clone(), |start| output[start..].to_vec())
    } else {
        output
    };
    if !output.starts_with(b"%PDF-") {
        return Err(OfficeConvertError::Failed(sanitize(
            diagnostic.as_deref().unwrap_or_default(),
        )));
    }
    let document = lopdf::Document::load_mem(&output)
        .map_err(|_| OfficeConvertError::Failed("invalid PDF output".into()))?;
    if document.is_encrypted() || document.get_pages().is_empty() {
        return Err(OfficeConvertError::Failed("invalid PDF output".into()));
    }
    let warning = diagnostic
        .as_deref()
        .filter(|bytes| !bytes.is_empty())
        .map(|bytes| crate::sanitize_event_text(&String::from_utf8_lossy(bytes), STDERR_CAP));
    Ok((output, warning))
}

#[derive(Clone, Copy)]
enum ReapMode {
    Normal,
    #[cfg(test)]
    Timeout,
    #[cfg(test)]
    WaitError,
}
enum ReapOutcome {
    AlreadyReaped,
    Reaped,
    Detached,
}
async fn kill_and_reap(
    mut child: tokio::process::Child,
    already_reaped: bool,
    mode: ReapMode,
) -> ReapOutcome {
    // Tokio only controls the direct child; descendants that inherit pipes are not process-tree managed.
    let _ = child.start_kill();
    if already_reaped {
        return ReapOutcome::AlreadyReaped;
    }
    let reaped = tokio::time::timeout(CHILD_REAP_GRACE, async {
        #[cfg(test)]
        match mode {
            ReapMode::Timeout => return std::future::pending().await,
            ReapMode::WaitError => return Err(std::io::Error::other("test wait error")),
            ReapMode::Normal => {}
        }
        #[cfg(not(test))]
        let _ = mode;
        child.wait().await
    })
    .await;
    match reaped {
        Ok(Ok(_)) => ReapOutcome::Reaped,
        Ok(Err(_)) | Err(_) => {
            tokio::spawn(async move {
                let _ = child.start_kill();
                let _ = child.wait().await;
            });
            ReapOutcome::Detached
        }
    }
}
enum ReadCapError {
    TooLarge,
    Io,
}
struct Stderr {
    bytes: Vec<u8>,
    #[allow(dead_code)]
    truncated: bool,
}
async fn read_stdout_cap(
    reader: &mut (impl AsyncRead + Unpin),
    cap: usize,
) -> Result<Vec<u8>, ReadCapError> {
    let mut bytes = Vec::new();
    let mut buf = [0; 8192];
    loop {
        let n = reader.read(&mut buf).await.map_err(|_| ReadCapError::Io)?;
        if n == 0 {
            return Ok(bytes);
        }
        let next = bytes.len().checked_add(n).ok_or(ReadCapError::TooLarge)?;
        if next > cap {
            return Err(ReadCapError::TooLarge);
        }
        bytes.extend_from_slice(&buf[..n]);
    }
}
async fn read_stderr_cap(
    reader: &mut (impl AsyncRead + Unpin),
    cap: usize,
) -> Result<Stderr, ReadCapError> {
    let mut bytes = Vec::new();
    let mut total = 0usize;
    let mut buf = [0; 8192];
    loop {
        let n = reader.read(&mut buf).await.map_err(|_| ReadCapError::Io)?;
        if n == 0 {
            return Ok(Stderr {
                bytes,
                truncated: total > cap,
            });
        }
        total = total.checked_add(n).ok_or(ReadCapError::Io)?;
        let keep = cap.saturating_sub(bytes.len()).min(n);
        bytes.extend_from_slice(&buf[..keep]);
    }
}
fn sanitize(bytes: &[u8]) -> String {
    let mut text = crate::error::sanitize_vlm_error_bytes(bytes, STDERR_CAP)
        .chars()
        .filter(|character| !character.is_control() || matches!(character, '\n' | '\t'))
        .collect::<String>();
    if text.len() > STDERR_CAP {
        let mut end = 0;
        for (start, character) in text.char_indices() {
            let next = start + character.len_utf8();
            if next > STDERR_CAP {
                break;
            }
            end = next;
        }
        text.truncate(end);
    }
    if text.is_empty() {
        "child failed".into()
    } else {
        text
    }
}
