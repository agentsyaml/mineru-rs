use axum::{
    Json, Router,
    extract::State,
    routing::{get, post},
};
use serde_json::{Value, json};
use std::{
    collections::VecDeque,
    io::Read,
    net::TcpListener,
    path::Path,
    process::{Child, ChildStdin, Command, Stdio},
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicUsize, Ordering},
    },
    thread,
    time::{Duration, Instant},
};
use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::{Mutex as TokioMutex, oneshot};

static SERIAL: OnceLock<TokioMutex<()>> = OnceLock::new();
const DIAGNOSTIC_CAP: usize = 16 * 1024;

struct Server {
    child: Child,
    stdin: Option<ChildStdin>,
    stderr: Arc<Mutex<VecDeque<u8>>>,
    reader: Option<thread::JoinHandle<()>>,
    cwd: TempDir,
}
impl Server {
    fn diagnostics(&self) -> String {
        String::from_utf8_lossy(
            &self
                .stderr
                .lock()
                .unwrap()
                .iter()
                .copied()
                .collect::<Vec<_>>(),
        )
        .into_owned()
    }
    fn close_stdin(&mut self) {
        self.stdin.take();
    }
    fn finish_reader(&mut self) {
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
    async fn exit(&mut self, limit: Duration) -> std::process::ExitStatus {
        let deadline = Instant::now() + limit;
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                self.finish_reader();
                return status;
            }
            assert!(
                Instant::now() < deadline,
                "server did not exit: {}",
                self.diagnostics()
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }
    async fn alive_after(&mut self, duration: Duration) {
        tokio::time::sleep(duration).await;
        assert!(
            self.child.try_wait().unwrap().is_none(),
            "server exited early: {}",
            self.diagnostics()
        );
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        self.close_stdin();
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
        }
        let _ = self.child.wait();
        self.finish_reader();
    }
}

fn scrub(command: &mut Command) {
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("MINERU_API_") {
            command.env_remove(key);
        }
    }
    for key in [
        "MINERU_PROCESSING_WINDOW_SIZE",
        "MINERU_PDF_RENDER_THREADS",
        "MINERU_PDF_RENDER_TIMEOUT",
        "MINERU_FORMULA_ENABLE",
        "MINERU_TABLE_ENABLE",
        "MINERU_VL_SERVER",
        "MINERU_VL_MODEL_NAME",
        "MINERU_VL_API_KEY",
        "MINERU_VL_DEBUG_ENABLE",
        "MINERU_VLM_END_TOKEN",
        "MINERU_LOG_LEVEL",
    ] {
        command.env_remove(key);
    }
}
fn run_help() -> (std::process::ExitStatus, Vec<u8>, Vec<u8>) {
    struct HelpOwner {
        child: Child,
        stdout: Option<thread::JoinHandle<Vec<u8>>>,
        stderr: Option<thread::JoinHandle<Vec<u8>>>,
    }
    impl HelpOwner {
        fn reap(&mut self) {
            if self.child.try_wait().ok().flatten().is_none() {
                let _ = self.child.kill();
            }
            let _ = self.child.wait();
        }
    }
    impl Drop for HelpOwner {
        fn drop(&mut self) {
            self.reap();
            if let Some(reader) = self.stdout.take() {
                let _ = reader.join();
            }
            if let Some(reader) = self.stderr.take() {
                let _ = reader.join();
            }
        }
    }
    let mut command = Command::new(env!("CARGO_BIN_EXE_mineru-api"));
    scrub(&mut command);
    let mut owner = HelpOwner {
        child: command
            .arg("--help")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
        stdout: None,
        stderr: None,
    };
    let mut out_pipe = owner.child.stdout.take().unwrap();
    owner.stdout = Some(thread::spawn(move || {
        let mut bytes = Vec::new();
        out_pipe.read_to_end(&mut bytes).unwrap();
        bytes
    }));
    let mut err_pipe = owner.child.stderr.take().unwrap();
    owner.stderr = Some(thread::spawn(move || {
        let mut bytes = Vec::new();
        err_pipe.read_to_end(&mut bytes).unwrap();
        bytes
    }));
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match owner.child.try_wait() {
            Ok(Some(status)) => {
                let _ = owner.child.wait();
                let stdout = owner.stdout.take().unwrap().join().unwrap();
                let stderr = owner.stderr.take().unwrap().join().unwrap();
                return (status, stdout, stderr);
            }
            Ok(None) => {}
            Err(error) => {
                owner.reap();
                let diagnostics =
                    String::from_utf8_lossy(&owner.stderr.take().unwrap().join().unwrap())
                        .into_owned();
                let _ = owner.stdout.take().unwrap().join();
                panic!("--help status check failed: {error}: {diagnostics}");
            }
        }
        if Instant::now() >= deadline {
            owner.reap();
            let diagnostics =
                String::from_utf8_lossy(&owner.stderr.take().unwrap().join().unwrap()).into_owned();
            let _ = owner.stdout.take().unwrap().join();
            panic!("--help timed out: {diagnostics}");
        }
        thread::sleep(Duration::from_millis(25));
    }
}
async fn pre_body_policy(port: u16) -> (u16, String) {
    tokio::time::timeout(Duration::from_secs(3), async move {
        let mut stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        stream
            .write_all(b"POST /tasks HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: multipart/form-data; boundary=test\r\nContent-Length: 1\r\nConnection: close\r\n\r\n")
            .await
            .unwrap();
        let mut response = Vec::new();
        stream.read_to_end(&mut response).await.unwrap();
        let response = String::from_utf8(response).unwrap();
        let (headers, body) = response.split_once("\r\n\r\n").unwrap();
        let status = headers.split_whitespace().nth(1).unwrap().parse().unwrap();
        (status, body.into())
    })
    .await
    .unwrap()
}
fn reserve_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}
async fn health(server: &mut Server, port: u16) {
    let client = reqwest::Client::new();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Ok(Ok(response)) = tokio::time::timeout(
            Duration::from_secs(1),
            client.get(format!("http://127.0.0.1:{port}/health")).send(),
        )
        .await
        {
            if response.status() == reqwest::StatusCode::OK {
                return;
            }
        }
        if let Some(status) = server.child.try_wait().unwrap() {
            server.finish_reader();
            panic!(
                "server exited before health ({status}): {}",
                server.diagnostics()
            );
        }
        assert!(
            Instant::now() < deadline,
            "health did not become ready: {}",
            server.diagnostics()
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}
fn spawn(host: &str, envs: &[(&str, &str)]) -> (Server, u16) {
    let port = reserve_port();
    // Reserve immediately before spawn; the listener is dropped before the child binds.
    let server = Server::start_at(host, port, envs);
    (server, port)
}
impl Server {
    fn start_at(host: &str, port: u16, envs: &[(&str, &str)]) -> Self {
        let cwd = tempfile::tempdir().unwrap();
        let mut command = Command::new(env!("CARGO_BIN_EXE_mineru-api"));
        scrub(&mut command);
        command
            .args(["--host", host, "--port", &port.to_string()])
            .current_dir(cwd.path())
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        for &(key, value) in envs {
            command.env(key, value);
        }
        let mut child = command.spawn().unwrap();
        let stderr = Arc::new(Mutex::new(VecDeque::new()));
        let saved = stderr.clone();
        let mut pipe = child.stderr.take().unwrap();
        let reader = thread::spawn(move || {
            let mut bytes = [0; 4096];
            while let Ok(n) = pipe.read(&mut bytes) {
                if n == 0 {
                    break;
                }
                let mut out = saved.lock().unwrap();
                out.extend(bytes[..n].iter().copied());
                while out.len() > DIAGNOSTIC_CAP {
                    out.pop_front();
                }
            }
        });
        Self {
            stdin: child.stdin.take(),
            child,
            stderr,
            reader: Some(reader),
            cwd,
        }
    }
}

#[tokio::test]
#[ignore = "process-level API server e2e"]
async fn help_health_cwd_and_eof() {
    let _lock = SERIAL.get_or_init(|| TokioMutex::new(())).lock().await;
    let (status, stdout, stderr) = run_help();
    assert!(status.success(), "{}", String::from_utf8_lossy(&stderr));
    let text = String::from_utf8(stdout).unwrap();
    assert!(text.contains("--host") && text.contains("--port"));
    let (mut server, port) = spawn(
        "127.0.0.1",
        &[
            ("MINERU_API_SHUTDOWN_ON_STDIN_EOF", "1"),
            ("MINERU_LOG_LEVEL", "iNfO"),
        ],
    );
    health(&mut server, port).await;
    let output = server.cwd.path().join("output");
    assert!(output.is_dir() && std::fs::read_dir(&output).unwrap().next().is_none());
    server.close_stdin();
    assert!(server.exit(Duration::from_secs(10)).await.success());
    assert_eq!(
        server.diagnostics().lines().collect::<Vec<_>>(),
        vec![
            format!(
                "server started: http://127.0.0.1:{port}: health=http://127.0.0.1:{port}/health"
            ),
            "server stopped: server".into(),
        ]
    );
}

#[tokio::test]
#[ignore = "process-level API server e2e"]
async fn invalid_log_level_exits_before_bind_or_output() {
    let _lock = SERIAL.get_or_init(|| TokioMutex::new(())).lock().await;
    let port = reserve_port();
    let mut server = Server::start_at(
        "127.0.0.1",
        port,
        &[("MINERU_LOG_LEVEL", "raw-secret-invalid")],
    );
    assert!(!server.exit(Duration::from_secs(10)).await.success());
    assert_eq!(server.diagnostics(), "invalid MINERU_LOG_LEVEL\n");
    assert!(!server.cwd.path().join("output").exists());
    TcpListener::bind(("127.0.0.1", port)).unwrap();
}

#[tokio::test]
#[ignore = "process-level API server e2e"]
async fn public_bind_matrix() {
    let _lock = SERIAL.get_or_init(|| TokioMutex::new(())).lock().await;
    let mut bad = Server::start_at("0.0.0.0", reserve_port(), &[]);
    assert!(!bad.exit(Duration::from_secs(10)).await.success());
    assert!(
        bad.diagnostics()
            .contains("--host must be a loopback IP address")
    );
    for (allow, expected) in [
        (
            false,
            (
                400,
                r#"{"detail":"public HTTP-client requests are disabled"}"#,
            ),
        ),
        (
            true,
            (422, r#"{"detail":"exactly one document is required"}"#),
        ),
    ] {
        let mut envs = vec![
            ("MINERU_API_PUBLIC_BIND_EXPOSED", "1"),
            ("MINERU_API_SHUTDOWN_ON_STDIN_EOF", "1"),
        ];
        if allow {
            envs.push(("MINERU_API_ALLOW_PUBLIC_HTTP_CLIENT", "1"));
        }
        let (mut server, port) = spawn("0.0.0.0", &envs);
        health(&mut server, port).await;
        if allow {
            let response = tokio::time::timeout(
                Duration::from_secs(3),
                reqwest::Client::new()
                    .post(format!("http://127.0.0.1:{port}/tasks"))
                    .multipart(reqwest::multipart::Form::new().text("backend", "vlm-http-client"))
                    .send(),
            )
            .await
            .unwrap()
            .unwrap();
            assert_eq!(response.status().as_u16(), expected.0);
            assert_eq!(response.text().await.unwrap(), expected.1);
        } else {
            let (status, body) = pre_body_policy(port).await;
            assert_eq!(status, expected.0);
            assert_eq!(body, expected.1);
        }
        server.close_stdin();
        assert!(server.exit(Duration::from_secs(10)).await.success());
    }
}

#[cfg(unix)]
unsafe extern "C" {
    fn kill(pid: i32, signal: i32) -> i32;
}
#[cfg(unix)]
const SIGINT: i32 = 2;
#[cfg(unix)]
const SIGTERM: i32 = 15;
#[cfg(unix)]
fn signal(pid: u32, value: i32) {
    assert_eq!(unsafe { kill(pid as i32, value) }, 0);
}

#[cfg(unix)]
#[tokio::test]
#[ignore = "process-level API server e2e"]
async fn sigint_exits_while_stdin_watcher_is_blocked() {
    let _lock = SERIAL.get_or_init(|| TokioMutex::new(())).lock().await;
    let (mut server, port) = spawn("127.0.0.1", &[("MINERU_API_SHUTDOWN_ON_STDIN_EOF", "1")]);
    health(&mut server, port).await;

    #[cfg(unix)]
    signal(server.child.id(), SIGINT);
    assert!(server.exit(Duration::from_secs(10)).await.success());
}

#[derive(Clone)]
struct Mock {
    entered: Arc<Mutex<Option<oneshot::Sender<()>>>>,
    release: Arc<TokioMutex<Option<oneshot::Receiver<()>>>>,
    chats: Arc<AtomicUsize>,
}
async fn models(State(mock): State<Mock>) -> Json<Value> {
    if let Some(tx) = mock.entered.lock().unwrap().take() {
        let _ = tx.send(());
    }
    if let Some(rx) = mock.release.lock().await.take() {
        let _ = rx.await;
    }
    Json(json!({"data":[{"id":"mock"}]}))
}
async fn chat(State(mock): State<Mock>, Json(request): Json<Value>) -> Json<Value> {
    mock.chats.fetch_add(1, Ordering::SeqCst);
    let text = if request.to_string().to_ascii_lowercase().contains("layout") {
        "<|box_start|>0 0 1000 1000<|box_end|><|ref_start|>text<|ref_end|><|rotate_up|>"
    } else {
        "recognized"
    };
    Json(json!({"choices":[{"finish_reason":"stop","message":{"content":text}}]}))
}

#[tokio::test]
#[ignore = "process-level API server e2e"]
async fn active_worker_drains_before_exit() {
    let _lock = SERIAL.get_or_init(|| TokioMutex::new(())).lock().await;
    let (entered_tx, entered_rx) = oneshot::channel();
    let (release_tx, release_rx) = oneshot::channel();
    let mock = Mock {
        entered: Arc::new(Mutex::new(Some(entered_tx))),
        release: Arc::new(TokioMutex::new(Some(release_rx))),
        chats: Arc::new(AtomicUsize::new(0)),
    };
    let chats = mock.chats.clone();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let (stop_tx, stop_rx) = oneshot::channel::<()>();
    let mock_task = tokio::spawn(async move {
        axum::serve(
            listener,
            Router::new()
                .route("/v1/models", get(models))
                .route("/v1/chat/completions", post(chat))
                .with_state(mock.clone()),
        )
        .with_graceful_shutdown(async {
            let _ = stop_rx.await;
        })
        .await
        .unwrap();
    });
    let (mut server, port) = spawn("127.0.0.1", &[("MINERU_API_SHUTDOWN_ON_STDIN_EOF", "1")]);
    health(&mut server, port).await;
    let file = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/pdf/minimal.pdf");
    let form = reqwest::multipart::Form::new()
        .text("backend", "vlm-http-client")
        .text("server_url", base)
        .text("formula_enable", "false")
        .text("table_enable", "false")
        .text("image_analysis", "false")
        .part(
            "files",
            reqwest::multipart::Part::file(file)
                .await
                .unwrap()
                .file_name("input.pdf")
                .mime_str("application/pdf")
                .unwrap(),
        );
    assert_eq!(
        tokio::time::timeout(
            Duration::from_secs(5),
            reqwest::Client::new()
                .post(format!("http://127.0.0.1:{port}/tasks"))
                .multipart(form)
                .send(),
        )
        .await
        .unwrap()
        .unwrap()
        .status(),
        reqwest::StatusCode::ACCEPTED
    );
    tokio::time::timeout(Duration::from_secs(30), entered_rx)
        .await
        .unwrap()
        .unwrap();
    assert!(
        std::fs::read_dir(server.cwd.path().join("output"))
            .unwrap()
            .next()
            .is_some()
    );
    #[cfg(unix)]
    signal(server.child.id(), SIGTERM);
    #[cfg(not(unix))]
    server.close_stdin();
    server.alive_after(Duration::from_millis(200)).await;
    release_tx.send(()).unwrap();
    assert!(server.exit(Duration::from_secs(30)).await.success());
    let diagnostics = server.diagnostics();
    let events: Vec<_> = diagnostics
        .lines()
        .filter(|line| {
            line.starts_with("server started:")
                || line.starts_with("request accepted:")
                || line.starts_with("document started:")
                || line.starts_with("document prepared:")
                || line.starts_with("document page completed:")
                || line.starts_with("document completed:")
                || line.starts_with("request completed:")
                || line.starts_with("server stopped:")
        })
        .collect();
    assert_eq!(
        events,
        vec![
            format!(
                "server started: http://127.0.0.1:{port}: health=http://127.0.0.1:{port}/health"
            ),
            "request accepted: local-1".into(),
            "document started: input".into(),
            "document prepared: input".into(),
            "document page completed: input: page=0 completed=1/1".into(),
            "document completed: input".into(),
            "request completed: local-1".into(),
            "server stopped: server".into(),
        ]
    );
    assert!(chats.load(Ordering::SeqCst) >= 1);
    assert!(
        std::fs::read_dir(server.cwd.path().join("output"))
            .unwrap()
            .next()
            .is_none()
    );
    let _ = stop_tx.send(());
    mock_task.await.unwrap();
}

#[cfg(feature = "office")]
#[tokio::test]
#[ignore = "process-level API server e2e"]
async fn canonical_client_consumes_real_api_server_zip_and_publishes_layout() {
    let _lock = SERIAL.get_or_init(|| TokioMutex::new(())).lock().await;
    let _office_convert = env!("CARGO_BIN_EXE_mineru-office-convert");
    let (entered_tx, entered_rx) = oneshot::channel();
    let (release_tx, release_rx) = oneshot::channel();
    let mock = Mock {
        entered: Arc::new(Mutex::new(Some(entered_tx))),
        release: Arc::new(TokioMutex::new(Some(release_rx))),
        chats: Arc::new(AtomicUsize::new(0)),
    };
    let chats = mock.chats.clone();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let (stop_tx, stop_rx) = oneshot::channel::<()>();
    let mock_task = tokio::spawn(async move {
        axum::serve(
            listener,
            Router::new()
                .route("/v1/models", get(models))
                .route("/v1/chat/completions", post(chat))
                .with_state(mock),
        )
        .with_graceful_shutdown(async {
            let _ = stop_rx.await;
        })
        .await
        .unwrap();
    });
    let port = reserve_port();
    let mut server = Server::start_at(
        "127.0.0.1",
        port,
        &[
            ("MINERU_API_OUTPUT_ROOT", "server-output"),
            ("MINERU_API_MAX_CONCURRENT_REQUESTS", "1"),
            ("MINERU_API_SHUTDOWN_ON_STDIN_EOF", "1"),
            ("MINERU_LOG_LEVEL", "INFO"),
            ("MINERU_VL_SERVER", &base),
            ("MINERU_VL_MODEL_NAME", "mock"),
        ],
    );
    health(&mut server, port).await;
    let health: Value = reqwest::Client::new()
        .get(format!("http://127.0.0.1:{port}/health"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        health,
        json!({"status":"healthy","protocol_version":2,"max_concurrent_requests":1,"processing_window_size":64,"task_count":0})
    );
    let server_output = server.cwd.path().join("server-output");
    assert!(server_output.is_dir() && std::fs::read_dir(&server_output).unwrap().next().is_none());

    let client_dir = tempfile::tempdir().unwrap();
    let document = client_dir.path().join("document.pdf");
    std::fs::copy(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/pdf/minimal.pdf"),
        &document,
    )
    .unwrap();
    let client_output = client_dir.path().join("client-output");
    let api_url = format!("http://127.0.0.1:{port}");
    release_tx.send(()).unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_mineru"));
    scrub(&mut command);
    for key in [
        "MINERU_TASK_RESULT_TIMEOUT_SECONDS",
        "MINERU_TASK_RESULT_DOWNLOAD_TIMEOUT_SECONDS",
        "MINERU_OFFICE_FAKE_CHILD",
        "MINERU_OFFICE_FAKE_MODE",
        "MINERU_OFFICE_FAKE_READY",
    ] {
        command.env_remove(key);
    }
    command
        .args([
            "-p",
            document.to_str().unwrap(),
            "-o",
            client_output.to_str().unwrap(),
            "--api-url",
            &api_url,
            "--start",
            "0",
            "--end",
            "0",
            "--formula",
            "false",
            "--table",
            "false",
            "--image-analysis",
            "false",
        ])
        .env("MINERU_API_MAX_CONCURRENT_REQUESTS", "1")
        .env("MINERU_TASK_RESULT_TIMEOUT_SECONDS", "30")
        .env("MINERU_TASK_RESULT_DOWNLOAD_TIMEOUT_SECONDS", "30")
        .env("MINERU_LOG_LEVEL", "INFO");
    let client = tokio::task::spawn_blocking(move || command.output())
        .await
        .unwrap();
    let output_was_nonempty = std::fs::read_dir(&server_output).unwrap().next().is_some();
    server.close_stdin();
    let server_status = server.exit(Duration::from_secs(30)).await;
    let diagnostics = server.diagnostics();
    let _ = stop_tx.send(());
    let mock_status = tokio::time::timeout(Duration::from_secs(5), mock_task).await;

    let client = client.unwrap();
    let stdout = String::from_utf8_lossy(&client.stdout);
    let stderr = String::from_utf8_lossy(&client.stderr);
    assert!(client.status.success(), "{stderr}");
    assert!(stdout.is_empty(), "{stdout}");
    for event in ["submitted", "downloading", "extracting", "completed"] {
        assert!(
            stderr.contains(&format!("{event}: task#1 [document]")),
            "{stderr}"
        );
    }
    assert!(!stderr.contains("api warning:") && !stderr.contains("api failed:"));
    assert!(
        !stderr.lines().any(|line| line.starts_with("failed:")),
        "{stderr}"
    );
    assert!(
        tokio::time::timeout(Duration::from_secs(5), entered_rx)
            .await
            .unwrap()
            .is_ok()
    );
    assert!(chats.load(Ordering::SeqCst) >= 1);
    assert!(mock_status.is_ok() && mock_status.unwrap().is_ok());
    for file in [
        client_output.join("document/vlm/document_middle.json"),
        client_output.join("document/vlm/document_origin.pdf"),
        client_output.join("document/vlm/document_layout.pdf"),
    ] {
        assert!(file.is_file(), "missing {}", file.display());
    }
    let middle: Value = serde_json::from_slice(
        &std::fs::read(client_output.join("document/vlm/document_middle.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(middle["pdf_info"][0]["page_idx"], 0);
    assert!(
        middle["pdf_info"][0]["preproc_blocks"][0]["bbox"]
            .as_array()
            .is_some_and(|bbox| bbox.len() == 4)
    );
    let layout =
        lopdf::Document::load(client_output.join("document/vlm/document_layout.pdf")).unwrap();
    assert_eq!(layout.get_pages().len(), 1);
    assert!(output_was_nonempty);
    assert!(server_status.success(), "{diagnostics}");
    for event in [
        "request accepted: local-1",
        "document completed: document",
        "request completed: local-1",
        "server stopped: server",
    ] {
        assert!(diagnostics.contains(event), "{diagnostics}");
    }
    assert!(std::fs::read_dir(&server_output).unwrap().next().is_none());
}
