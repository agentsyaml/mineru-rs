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

#[cfg(test)]
const INPUT_CAP: usize = 32 * 1024 * 1024;
#[cfg(test)]
const OUTPUT_CAP: usize = 64 * 1024 * 1024;
#[cfg(test)]
const STDERR_CAP: usize = 4096;
/// The fake child emits its markdown behind this marker so the owner's test-mode scan can
/// strip the libtest harness preamble ("running 1 test") that precedes any test stdout.
#[cfg(test)]
const MARKDOWN_FAKE_MARKER: &[u8] = b"\0__markdown_fake__\0";
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

/// Output contract of a conversion: PDF mode validates the `%PDF-` signature plus lopdf
/// structure (OOXML family); Markdown mode validates non-empty UTF-8 text (legacy family).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ConvertMode {
    Pdf,
    Markdown,
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
    limits: crate::command::service::OfficeLimits,
    ooxml: crate::command::service::OoxmlLimits,
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
    #[cfg(test)]
    pub(crate) fn executable(&self) -> &std::path::Path {
        &self.executable
    }

    #[doc(hidden)]
    pub fn new() -> Result<Self, OfficeConvertError> {
        let executable = std::env::current_exe()
            .map_err(|_| OfficeConvertError::Unavailable)?
            .with_file_name(if cfg!(windows) {
                "mineru-office-convert.exe"
            } else {
                "mineru-office-convert"
            });
        Ok(Self::with_executable(executable))
    }
    #[doc(hidden)]
    pub fn with_executable(executable: PathBuf) -> Self {
        Self::with_executable_and_limits(
            executable,
            crate::command::service::OfficeLimits::default(),
        )
    }

    /// Crate-private construction with a frozen Phase-1B office resource policy. The resolved
    /// limits are enforced in this parent and written into the explicit child environment so the
    /// helper reads them exactly once at startup.
    pub(crate) fn with_executable_and_limits(
        executable: PathBuf,
        limits: crate::command::service::OfficeLimits,
    ) -> Self {
        Self::with_executable_and_policy(
            executable,
            limits,
            crate::command::service::OoxmlLimits::default_resolved(),
        )
    }

    /// Crate-private construction with the frozen office AND OOXML policy; both are written into
    /// the explicit child environment so the helper's own preflight honors the same limits.
    pub(crate) fn with_executable_and_policy(
        executable: PathBuf,
        limits: crate::command::service::OfficeLimits,
        ooxml: crate::command::service::OoxmlLimits,
    ) -> Self {
        Self {
            executable,
            prefix: Vec::new(),
            limits,
            ooxml,
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
        }
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
            limits: crate::command::service::OfficeLimits::default(),
            ooxml: crate::command::service::OoxmlLimits::default_resolved(),
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
        self.convert_inner(format, input, timeout, ConvertMode::Pdf)
            .await
    }
    /// Legacy-family text extraction: converts to Markdown with the same bounded subprocess
    /// skeleton as [`Self::convert`], validating the output as UTF-8 text instead of a PDF.
    #[doc(hidden)]
    pub async fn convert_text(
        &self,
        format: &'static str,
        input: impl Into<Bytes>,
        timeout: Duration,
    ) -> Result<Vec<u8>, OfficeConvertError> {
        self.convert_text_with_warning(format, input, timeout)
            .await
            .map(|(text, _)| text)
    }
    pub(crate) async fn convert_text_with_warning(
        &self,
        format: &'static str,
        input: impl Into<Bytes>,
        timeout: Duration,
    ) -> Result<(Vec<u8>, Option<String>), OfficeConvertError> {
        self.convert_inner(format, input, timeout, ConvertMode::Markdown)
            .await
    }
    async fn convert_inner(
        &self,
        format: &'static str,
        input: impl Into<Bytes>,
        timeout: Duration,
        mode: ConvertMode,
    ) -> Result<(Vec<u8>, Option<String>), OfficeConvertError> {
        let input = input.into();
        let limits = self.limits;
        let ooxml = self.ooxml;
        if input.len() > limits.input_bytes {
            return Err(OfficeConvertError::Failed(format!(
                "input too large: office input exceeds limit of {} bytes; limit {} bytes; raise with --office-input-bytes or MINERU_OFFICE_INPUT_BYTES",
                input.len(),
                limits.input_bytes
            )));
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
                    executable, prefix, format, input, timeout, cancel_rx, limits, ooxml, mode,
                    probe, ready,
                )
                .await;
                #[cfg(not(test))]
                let result = owner(
                    executable, prefix, format, input, timeout, cancel_rx, limits, ooxml, mode,
                )
                .await;
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
            "ok_markdown" => {
                let mut out = MARKDOWN_FAKE_MARKER.to_vec();
                out.extend_from_slice(b"hello markdown \xe4\xb8\xad\xe6\x96\x87");
                let _ = std::io::stdout().write_all(&out);
                std::process::exit(0);
            }
            "invalid_text" => {
                let mut out = MARKDOWN_FAKE_MARKER.to_vec();
                out.extend_from_slice(&[0xff, 0xfe, 0x80, 0x00]);
                let _ = std::io::stdout().write_all(&out);
                std::process::exit(0);
            }
            "empty_markdown" => {
                let _ = std::io::stdout().write_all(MARKDOWN_FAKE_MARKER);
                std::process::exit(0);
            }
            "hang" => {
                eprintln!("entered");
                std::thread::sleep(Duration::from_secs(30));
            }
            #[cfg(unix)]
            "group_hang" => {
                let grandchild = std::process::Command::new("sleep")
                    .arg("30")
                    .spawn()
                    .unwrap();
                if let Some(path) = std::env::var_os("MINERU_OFFICE_FAKE_READY") {
                    std::fs::write(path, grandchild.id().to_string()).unwrap();
                }
                loop {
                    std::thread::sleep(Duration::from_secs(1));
                }
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
            OfficeWorkers::with_executable(PathBuf::from("definitely-not-an-executable"));
        assert!(convert(&unavailable, "ok").await.is_err());
        w.drain().await;
        assert!(matches!(
            convert(&w, "ok").await,
            Err(OfficeConvertError::Draining)
        ));
    }

    #[tokio::test]
    async fn input_limit_error_names_the_raise_knob() {
        let mut w = workers();
        w.limits.input_bytes = 16;
        let error = w
            .convert("ok", vec![0; 17], Duration::from_secs(1))
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("input too large"));
        assert!(
            error.contains("--office-input-bytes") && error.contains("MINERU_OFFICE_INPUT_BYTES")
        );
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
    async fn text_mode_accepts_utf8_markdown_and_discards_warning() {
        let w = workers();
        let (text, warning) = w
            .convert_text_with_warning("ok_markdown", b"input".to_vec(), Duration::from_secs(5))
            .await
            .unwrap();
        assert!(text.starts_with(b"hello markdown"));
        assert!(String::from_utf8_lossy(&text).contains('中'));
        assert_eq!(warning, None);
        assert!(
            w.convert_text("ok_markdown", b"input".to_vec(), Duration::from_secs(5))
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn text_mode_rejects_empty_and_non_utf8_output() {
        let w = workers();
        let error = w
            .convert_text("invalid_text", b"input".to_vec(), Duration::from_secs(5))
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("invalid text output"), "{error}");
        let error = w
            .convert_text("empty_markdown", b"input".to_vec(), Duration::from_secs(5))
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("invalid text output"), "{error}");
    }

    #[tokio::test]
    async fn text_mode_honors_deadline_and_output_cap() {
        let w = workers();
        let start = tokio::time::Instant::now();
        assert!(
            w.convert_text("hang", vec![], Duration::from_millis(100))
                .await
                .is_err()
        );
        assert!(start.elapsed() < Duration::from_secs(2));
        let start = tokio::time::Instant::now();
        assert!(
            w.convert_text("largeout", vec![], Duration::from_secs(10))
                .await
                .is_err()
        );
        assert!(start.elapsed() < Duration::from_secs(3));
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

    #[test]
    fn managed_wall_time_is_capped_independently_of_route_deadline() {
        assert_eq!(
            managed_timeout(Duration::from_secs(1), Duration::from_secs(180)),
            Duration::from_secs(1)
        );
        assert_eq!(
            managed_timeout(Duration::from_secs(24 * 60 * 60), Duration::from_secs(180)),
            Duration::from_secs(180)
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn timeout_kills_the_helper_process_group() {
        let mut w = workers();
        let ready = tempfile::NamedTempFile::new().unwrap();
        let path = ready.path().to_path_buf();
        std::fs::remove_file(&path).unwrap();
        w.ready = Some(path.clone());
        let conversion = tokio::spawn({
            let w = w.clone();
            async move {
                w.convert("group_hang", vec![], Duration::from_millis(100))
                    .await
            }
        });
        let pid = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if let Ok(pid) = std::fs::read_to_string(&path)
                    && let Ok(pid) = pid.trim().parse::<libc::pid_t>()
                {
                    break pid;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(conversion.await.unwrap().is_err());
        let gone = tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                // SAFETY: `pid` was reported by the disposable test child.
                if unsafe { libc::kill(pid, 0) } == -1 {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await;
        if gone.is_err() {
            // SAFETY: prevent a failed assertion from leaking the disposable child.
            unsafe { libc::kill(pid, libc::SIGKILL) };
        }
        assert!(gone.is_ok(), "process-group descendant survived cleanup");
    }
}

async fn owner(
    executable: PathBuf,
    prefix: Vec<OsString>,
    format: &'static str,
    input: Bytes,
    timeout: Duration,
    mut cancel: watch::Receiver<bool>,
    limits: crate::command::service::OfficeLimits,
    ooxml: crate::command::service::OoxmlLimits,
    mode: ConvertMode,
    #[cfg(test)] probe: Arc<TestProbe>,
    #[cfg(test)] ready: Option<PathBuf>,
) -> Result<(Vec<u8>, Option<String>), OfficeConvertError> {
    let now = tokio::time::Instant::now();
    let route_deadline = now
        .checked_add(timeout)
        .ok_or_else(|| OfficeConvertError::Failed("invalid timeout".into()))?;
    let deadline = route_deadline.min(now + managed_timeout(timeout, limits.wall));
    let mut command = Command::new(executable);
    #[cfg(test)]
    let fake_child = !prefix.is_empty();
    command.args(prefix);
    // The resolved parent policy is written into the explicit child environment; the helper
    // reads it exactly once at startup and never re-reads a drifting process environment.
    limits.apply_to_child_env(&mut command);
    ooxml.apply_to_child_env(&mut command);
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
    #[cfg(unix)]
    command.process_group(0);
    // Linux-only: die with the parent if it is SIGKILLed, so a killed parent never strands the
    // helper as an orphan. macOS has no equivalent mechanism; on Windows the job object
    // KILL_ON_JOB_CLOSE already covers this. Note PDEATHSIG only signals the direct child, not
    // the helper's own grandchildren (e.g. soffice).
    #[cfg(target_os = "linux")]
    {
        let parent_pid = std::process::id();
        // SAFETY: PR_SET_PDEATHSIG is a single async-signal-safe prctl before exec; the parent
        // pid captured above identifies the parent this process will die with.
        unsafe {
            command.pre_exec(move || {
                if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                // The parent may have died between fork() and prctl(), in which case
                // PDEATHSIG was never armed; exiting here avoids orphaning the helper.
                if libc::getppid() != parent_pid as libc::pid_t {
                    libc::_exit(1);
                }
                Ok(())
            });
        }
    }
    #[cfg(test)]
    command.env("MINERU_OFFICE_FAKE_CHILD", "1");
    let mut child = command
        .spawn()
        .map_err(|_| OfficeConvertError::Unavailable)?;
    let process_group = child.id();
    #[cfg(unix)]
    let mut process_group_guard = ProcessGroup(process_group);
    let stdin = child.stdin.take();
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let (mut stdin, mut stdout, mut stderr) = match (stdin, stdout, stderr) {
        (Some(stdin), Some(stdout), Some(stderr)) => (stdin, stdout, stderr),
        (stdin, stdout, stderr) => {
            drop(stdin);
            drop(stdout);
            drop(stderr);
            let _reap = kill_and_reap(child, process_group, false, ReapMode::Normal).await;
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
    let output_cap = limits.output_bytes;
    let stderr_cap = limits.stderr_bytes;
    let mut out = Some(tokio::spawn(async move {
        #[cfg(test)]
        if matches!(format, "pipe_failure" | "reap_wait_error") {
            return Err(ReadCapError::Io);
        }
        let result = read_stdout_cap(&mut stdout, output_cap).await;
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
        let result = read_stderr_cap(&mut stderr, stderr_cap).await;
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
        let _reap = kill_and_reap(child, process_group, child_reaped, reap_mode).await;
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
            sanitize(diagnostic.as_deref().unwrap_or_default(), stderr_cap)
        } else {
            message.into()
        }));
    }

    if !status.as_ref().is_some_and(|value| value.success()) {
        return Err(OfficeConvertError::Failed(sanitize(
            diagnostic.as_deref().unwrap_or_default(),
            stderr_cap,
        )));
    }
    #[cfg(unix)]
    {
        // The direct child and every pipe are complete; clear lingering descendants before any
        // PDF validation can delay us, then discard the PGID before it can be reused.
        kill_process_group(process_group);
        process_group_guard.disarm();
    }
    let output = output.unwrap_or_default();
    #[cfg(test)]
    let output = if fake_child {
        match mode {
            ConvertMode::Pdf => output
                .windows(5)
                .position(|window| window == b"%PDF-")
                .map_or(output.clone(), |start| output[start..].to_vec()),
            ConvertMode::Markdown => output
                .windows(MARKDOWN_FAKE_MARKER.len())
                .position(|window| window == MARKDOWN_FAKE_MARKER)
                .map_or(output.clone(), |start| {
                    output[start + MARKDOWN_FAKE_MARKER.len()..].to_vec()
                }),
        }
    } else {
        output
    };
    match mode {
        ConvertMode::Pdf => {
            if !output.starts_with(b"%PDF-") {
                return Err(OfficeConvertError::Failed(sanitize(
                    diagnostic.as_deref().unwrap_or_default(),
                    stderr_cap,
                )));
            }
            let document = lopdf::Document::load_mem(&output)
                .map_err(|_| OfficeConvertError::Failed("invalid PDF output".into()))?;
            if document.is_encrypted() || document.get_pages().is_empty() {
                return Err(OfficeConvertError::Failed("invalid PDF output".into()));
            }
        }
        ConvertMode::Markdown => {
            if output.is_empty() || std::str::from_utf8(&output).is_err() {
                return Err(OfficeConvertError::Failed("invalid text output".into()));
            }
        }
    }
    let warning = diagnostic
        .as_deref()
        .filter(|bytes| !bytes.is_empty())
        .map(|bytes| crate::sanitize_event_text(&String::from_utf8_lossy(bytes), stderr_cap));
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
    #[cfg_attr(not(unix), allow(unused_variables))] process_group: Option<u32>,
    already_reaped: bool,
    mode: ReapMode,
) -> ReapOutcome {
    #[cfg(unix)]
    kill_process_group(process_group);
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
                #[cfg(unix)]
                kill_process_group(process_group);
                let _ = child.start_kill();
                let _ = child.wait().await;
            });
            ReapOutcome::Detached
        }
    }
}

#[cfg(unix)]
fn kill_process_group(process_group: Option<u32>) {
    let Some(process_group) = process_group.filter(|id| *id != 0 && *id != std::process::id())
    else {
        return;
    };
    // SAFETY: the child was launched with process_group(0), so -pid denotes only its new group.
    unsafe { libc::kill(-(process_group as libc::pid_t), libc::SIGKILL) };
}

#[cfg(unix)]
struct ProcessGroup(Option<u32>);

#[cfg(unix)]
impl ProcessGroup {
    fn disarm(&mut self) {
        self.0 = None;
    }
}

#[cfg(unix)]
impl Drop for ProcessGroup {
    fn drop(&mut self) {
        kill_process_group(self.0);
    }
}

fn managed_timeout(remaining_route_deadline: Duration, wall: Duration) -> Duration {
    remaining_route_deadline.min(wall)
}
enum ReadCapError {
    TooLarge,
    Io,
}
struct Stderr {
    bytes: Vec<u8>,
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
    let mut buf = [0; 8192];
    loop {
        let n = reader.read(&mut buf).await.map_err(|_| ReadCapError::Io)?;
        if n == 0 {
            return Ok(Stderr { bytes });
        }
        let keep = cap.saturating_sub(bytes.len()).min(n);
        bytes.extend_from_slice(&buf[..keep]);
    }
}
fn sanitize(bytes: &[u8], cap: usize) -> String {
    let mut text = crate::error::sanitize_vlm_error_bytes(bytes, cap)
        .chars()
        .filter(|character| !character.is_control() || matches!(character, '\n' | '\t'))
        .collect::<String>();
    if text.len() > cap {
        let mut end = 0;
        for (start, character) in text.char_indices() {
            let next = start + character.len_utf8();
            if next > cap {
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
