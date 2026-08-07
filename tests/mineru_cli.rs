#[cfg(feature = "office")]
use axum::http::header;
use axum::{
    Json, Router,
    extract::{Multipart, Path as AxumPath, Request, State},
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
};
use image::{DynamicImage, ImageBuffer, ImageFormat, Rgba};
use serde_json::{Value, json};
use std::{
    process::{Command, Output},
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};
#[path = "support/office_fixtures.rs"]
#[allow(dead_code)]
mod office_fixtures;

fn mineru() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_mineru"));
    command.env("MINERU_VL_MODEL_NAME", "mock");
    command.env_remove("MINERU_LOG_LEVEL");
    command.env_remove("MINERU_PROCESSING_WINDOW_SIZE");
    command.env_remove("MINERU_API_MAX_CONCURRENT_REQUESTS");
    command.env_remove("MINERU_TASK_RESULT_TIMEOUT_SECONDS");
    command.env_remove("MINERU_TASK_RESULT_DOWNLOAD_TIMEOUT_SECONDS");
    command.env_remove("MINERU_PDF_RENDER_THREADS");
    command.env_remove("MINERU_PDF_RENDER_TIMEOUT");
    command.env_remove("MINERU_FORMULA_ENABLE");
    command.env_remove("MINERU_TABLE_ENABLE");
    command.env_remove("MINERU_IMAGE_ANALYSIS_ENABLE");
    command.env_remove("MINERU_OFFICIAL_PAGE_CONCURRENCY");
    command.env_remove("MINERU_BATCH_SIZE");
    command.env_remove("MINERU_VLM_HTTP_CONCURRENCY");
    command.env_remove("MINERU_VLM_HTTP_TIMEOUT");
    command.env_remove("MINERU_VLM_CONNECT_TIMEOUT");
    command.env_remove("MINERU_VLM_HTTP_MAX_KEEPALIVE_CONNECTIONS");
    command.env_remove("MINERU_VLM_HTTP_KEEPALIVE_EXPIRY");
    command.env_remove("MINERU_VLM_HTTP_MAX_RETRIES");
    command.env_remove("MINERU_VLM_HTTP_RETRY_BACKOFF_FACTOR");
    command.env_remove("MINERU_VLM_MAX_IMAGE_BYTES");
    command.env_remove("MINERU_VLM_MAX_DECODED_PIXELS");
    command.env_remove("MINERU_VLM_MAX_IMAGES_PER_REQUEST");
    command.env_remove("MINERU_VLM_MAX_REDIRECTS");
    command.env_remove("MINERU_VLM_HTTP_MAX_RESPONSE_BYTES");
    command.env_remove("MINERU_VL_DEBUG_ENABLE");
    command.env_remove("MINERU_OFFICE_FAKE_CHILD");
    command.env_remove("MINERU_OFFICE_FAKE_MODE");
    command.env_remove("MINERU_OFFICE_FAKE_READY");
    command
}

/// Strip the `[+HH:MM:SS] ` run-elapsed stamp that the CLI plain renderer
/// prepends, so e2e snapshots stay exact without pinning wall-clock timing.
fn unstamped(stderr: &[u8]) -> Vec<String> {
    std::str::from_utf8(stderr)
        .unwrap()
        .lines()
        .map(|line| {
            line.strip_prefix("[+")
                .and_then(|rest| rest.split_once("] "))
                .map(|(_, body)| body.to_owned())
                .unwrap_or_else(|| line.to_owned())
        })
        .collect()
}

#[derive(Clone, Default)]
struct Seen(Arc<Mutex<Vec<(String, Value, Option<String>)>>>);

#[derive(Clone)]
struct Mock {
    seen: Seen,
    layout: bool,
    #[cfg_attr(not(unix), allow(dead_code))]
    mutate_output: Option<std::path::PathBuf>,
    fail_after: Option<usize>,
    active: Arc<AtomicUsize>,
    peak: Arc<AtomicUsize>,
}

const THREE_CANDIDATE_LAYOUT: &str = "<|box_start|>1 1 200 200<|box_end|><|ref_start|>equation<|ref_end|><|rotate_up|><|box_start|>250 1 500 200<|box_end|><|ref_start|>table<|ref_end|><|rotate_up|><|box_start|>1 250 200 500<|box_end|><|ref_start|>image<|ref_end|><|rotate_up|>";

async fn mock_with(layout: bool, mutate_output: Option<std::path::PathBuf>) -> (String, Seen) {
    async fn models(State(mock): State<Mock>) -> Json<Value> {
        mock.seen
            .0
            .lock()
            .unwrap()
            .push(("models".into(), json!({}), None));
        #[cfg(unix)]
        if let Some(output) = &mock.mutate_output {
            use std::os::unix::fs::symlink;
            std::fs::create_dir_all(output).unwrap();
            symlink("/", output.join("document")).unwrap();
        }
        Json(json!({"data":[{"id":"discovered"}]}))
    }
    async fn completion(State(mock): State<Mock>, request: Request) -> axum::response::Response {
        let auth = request
            .headers()
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        let (_, body) = request.into_parts();
        let body = axum::body::to_bytes(body, 16 * 1024 * 1024).await.unwrap();
        let mut calls = mock.seen.0.lock().unwrap();
        calls.push((
            "completion".into(),
            serde_json::from_slice(&body).unwrap(),
            auth,
        ));
        let completion_count = calls
            .iter()
            .filter(|(kind, _, _)| kind == "completion")
            .count();
        drop(calls);
        if mock.fail_after == Some(completion_count) {
            return (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                "mock failure",
            )
                .into_response();
        }
        let content = if mock.layout {
            THREE_CANDIDATE_LAYOUT
        } else {
            ""
        };
        Json(json!({"choices":[{"finish_reason":"stop","message":{"content":content}}]}))
            .into_response()
    }
    let seen = Seen::default();
    let app = Router::new()
        .route("/v1/models", get(models))
        .route("/v1/chat/completions", post(completion))
        .with_state(Mock {
            seen: seen.clone(),
            layout,
            mutate_output,
            fail_after: None,
            active: Arc::new(AtomicUsize::new(0)),
            peak: Arc::new(AtomicUsize::new(0)),
        });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{address}"), seen)
}

async fn failing_mock(after: usize) -> (String, Seen) {
    async fn models(State(mock): State<Mock>) -> Json<Value> {
        mock.seen
            .0
            .lock()
            .unwrap()
            .push(("models".into(), json!({}), None));
        Json(json!({"data":[{"id":"mock"}]}))
    }
    async fn completion(State(mock): State<Mock>, request: Request) -> axum::response::Response {
        let (_, body) = request.into_parts();
        let body = axum::body::to_bytes(body, 16 * 1024 * 1024).await.unwrap();
        let mut calls = mock.seen.0.lock().unwrap();
        calls.push((
            "completion".into(),
            serde_json::from_slice(&body).unwrap(),
            None,
        ));
        let count = calls
            .iter()
            .filter(|(kind, _, _)| kind == "completion")
            .count();
        drop(calls);
        if count >= mock.fail_after.unwrap() {
            return (axum::http::StatusCode::BAD_REQUEST, "mock failure").into_response();
        }
        Json(json!({"choices":[{"finish_reason":"stop","message":{"content":""}}]})).into_response()
    }
    let seen = Seen::default();
    let app = Router::new()
        .route("/v1/models", get(models))
        .route("/v1/chat/completions", post(completion))
        .with_state(Mock {
            seen: seen.clone(),
            layout: false,
            mutate_output: None,
            fail_after: Some(after),
            active: Arc::new(AtomicUsize::new(0)),
            peak: Arc::new(AtomicUsize::new(0)),
        });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{address}"), seen)
}

async fn mock() -> (String, Seen) {
    mock_with(false, None).await
}

/// Deterministic multi-candidate mock: every completion is held briefly while the handler
/// tracks active and peak concurrent requests, so the CLI batch admission is observable.
async fn batch_mock() -> (String, Seen, Arc<AtomicUsize>) {
    async fn models(State(mock): State<Mock>) -> Json<Value> {
        mock.seen
            .0
            .lock()
            .unwrap()
            .push(("models".into(), json!({}), None));
        Json(json!({"data":[{"id":"discovered"}]}))
    }
    async fn completion(State(mock): State<Mock>, request: Request) -> axum::response::Response {
        let (_, body) = request.into_parts();
        let body = axum::body::to_bytes(body, 16 * 1024 * 1024).await.unwrap();
        mock.seen.0.lock().unwrap().push((
            "completion".into(),
            serde_json::from_slice(&body).unwrap(),
            None,
        ));
        let active = mock.active.fetch_add(1, Ordering::SeqCst) + 1;
        mock.peak.fetch_max(active, Ordering::SeqCst);
        tokio::time::sleep(std::time::Duration::from_millis(120)).await;
        mock.active.fetch_sub(1, Ordering::SeqCst);
        Json(json!({"choices":[{"finish_reason":"stop","message":{"content":THREE_CANDIDATE_LAYOUT}}]}))
            .into_response()
    }
    let seen = Seen::default();
    let peak = Arc::new(AtomicUsize::new(0));
    let app = Router::new()
        .route("/v1/models", get(models))
        .route("/v1/chat/completions", post(completion))
        .with_state(Mock {
            seen: seen.clone(),
            layout: true,
            mutate_output: None,
            fail_after: None,
            active: Arc::new(AtomicUsize::new(0)),
            peak: peak.clone(),
        });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{address}"), seen, peak)
}

async fn command(mut command: Command) -> Output {
    tokio::task::spawn_blocking(move || command.output().unwrap())
        .await
        .unwrap()
}

#[cfg(feature = "office")]
#[derive(Clone, Debug)]
struct MultipartPart {
    name: String,
    filename: Option<String>,
    content_type: Option<String>,
    bytes: Vec<u8>,
}

#[cfg(feature = "office")]
struct ApiState {
    base: String,
    output: std::path::PathBuf,
    barrier: Arc<tokio::sync::Barrier>,
    archives: Arc<std::collections::BTreeMap<String, Vec<u8>>>,
    health: usize,
    tasks: Vec<(String, Vec<MultipartPart>)>,
    statuses: std::collections::BTreeMap<String, usize>,
    results: usize,
    third_after_layouts: bool,
}

#[cfg(feature = "office")]
fn result_zip(stem: &str, kind: &str, extension: &str, origin: &[u8]) -> Vec<u8> {
    use std::io::Write;
    use zip::{CompressionMethod, ZipWriter, write::SimpleFileOptions};
    let mut zip = ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let options = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
    let root = format!("{stem}/{kind}/{stem}");
    zip.start_file(format!("{root}_middle.json"), options)
        .unwrap();
    zip.write_all(br#"{"pdf_info":[{"page_idx":0,"page_size":[200,100],"preproc_blocks":[{"type":"text","bbox":[20,10,150,80]}],"discarded_blocks":[]}]}"#).unwrap();
    zip.start_file(format!("{root}_origin.{extension}"), options)
        .unwrap();
    zip.write_all(origin).unwrap();
    zip.finish().unwrap().into_inner()
}

fn input(dir: &tempfile::TempDir) -> std::path::PathBuf {
    let path = dir.path().join("document.pdf");
    std::fs::copy("tests/fixtures/pdf/minimal.pdf", &path).unwrap();
    path
}

fn multipage_pdf(path: &std::path::Path) {
    use lopdf::{Document, Object, Stream, dictionary};
    let mut pdf = Document::with_version("1.5");
    let pages = pdf.new_object_id();
    let page_ids: Vec<_> = (0..2).map(|_| pdf.new_object_id()).collect();
    for id in &page_ids {
        let contents = pdf.add_object(Stream::new(
            dictionary! {},
            b"BT /F1 12 Tf 72 720 Td (x) Tj ET".to_vec(),
        ));
        pdf.objects.insert(*id, Object::Dictionary(dictionary! {
            "Type" => "Page", "Parent" => pages, "MediaBox" => vec![0.into(), 0.into(), 612.into(), 792.into()], "Contents" => contents,
        }));
    }
    pdf.objects.insert(pages, Object::Dictionary(dictionary! { "Type" => "Pages", "Kids" => page_ids.iter().copied().map(Object::Reference).collect::<Vec<_>>(), "Count" => 2 }));
    let catalog = pdf.add_object(dictionary! { "Type" => "Catalog", "Pages" => pages });
    pdf.trailer.set("Root", catalog);
    pdf.compress();
    pdf.save(path).unwrap();
}

#[test]
#[ignore = "CLI process contract e2e"]
fn help_advertises_mixed_inputs_without_api_or_local_engines() {
    let output = mineru().arg("--help").output().unwrap();
    assert!(output.status.success());
    let help = String::from_utf8(output.stdout).unwrap();
    assert!(help.contains("external VLM-HTTP subset"));
    assert!(help.contains("PDF, image, and Office"));
    assert!(help.contains("--start") && help.contains("--end"));
    assert!(help.contains("--backend") && help.contains("VLM-HTTP subset"));
    assert_eq!(
        help.lines()
            .map(str::trim)
            .filter(|line| line.starts_with('-'))
            .collect::<Vec<_>>(),
        [
            "-v, --version",
            "-p, --path <PATH>",
            "-o, --output <OUTPUT>",
            "--api-url <API_URL>",
            "--api-key <API_KEY>",
            "-m, --method <METHOD>",
            "-b, --backend <BACKEND>",
            "--effort <EFFORT>",
            "-l, --lang <LANG>",
            "-u, --url <URL>",
            "-s, --start <START>",
            "-e, --end <END>",
            "-f, --formula <FORMULA>",
            "-t, --table <TABLE>",
            "--image-analysis <IMAGE_ANALYSIS>",
            "--client-side-output-generation <CLIENT_SIDE_OUTPUT_GENERATION>",
            "--max-input-bytes <MAX_INPUT_BYTES>",
            "--max-encoded-document-bytes <MAX_ENCODED_DOCUMENT_BYTES>",
            "--max-output-bytes <MAX_OUTPUT_BYTES>",
            "--log-level <LOG_LEVEL>",
            "--processing-window-size <PROCESSING_WINDOW_SIZE>",
            "--page-concurrency <PAGE_CONCURRENCY>",
            "--render-workers <RENDER_WORKERS>",
            "--render-timeout-seconds <RENDER_TIMEOUT_SECONDS>",
            "--max-pdf-bytes <MAX_PDF_BYTES>",
            "--max-pages <MAX_PAGES>",
            "--max-page-pixels <MAX_PAGE_PIXELS>",
            "--max-rendered-image-bytes <MAX_RENDERED_IMAGE_BYTES>",
            "--max-in-flight-image-bytes <MAX_IN_FLIGHT_IMAGE_BYTES>",
            "--max-raw-output-bytes <MAX_RAW_OUTPUT_BYTES>",
            "--max-layout-blocks-per-page <MAX_LAYOUT_BLOCKS_PER_PAGE>",
            "--max-semantic-requests-per-page <MAX_SEMANTIC_REQUESTS_PER_PAGE>",
            "--batch-size <BATCH_SIZE>",
            "--max-encoded-request-bytes <MAX_ENCODED_REQUEST_BYTES>",
            "--max-encoded-batch-bytes <MAX_ENCODED_BATCH_BYTES>",
            "--max-total-asset-bytes <MAX_TOTAL_ASSET_BYTES>",
            "--max-staged-text-bytes <MAX_STAGED_TEXT_BYTES>",
            "--total-deadline-seconds <TOTAL_DEADLINE_SECONDS>",
            "--http-max-concurrency <HTTP_MAX_CONCURRENCY>",
            "--http-timeout-seconds <HTTP_TIMEOUT_SECONDS>",
            "--connect-timeout-seconds <CONNECT_TIMEOUT_SECONDS>",
            "--http-max-keepalive-connections <HTTP_MAX_KEEPALIVE_CONNECTIONS>",
            "--http-keepalive-expiry-seconds <HTTP_KEEPALIVE_EXPIRY_SECONDS>",
            "--http-max-retries <HTTP_MAX_RETRIES>",
            "--http-retry-backoff-factor <HTTP_RETRY_BACKOFF_FACTOR>",
            "--max-remote-image-bytes <MAX_REMOTE_IMAGE_BYTES>",
            "--max-decoded-pixels <MAX_DECODED_PIXELS>",
            "--max-images-per-request <MAX_IMAGES_PER_REQUEST>",
            "--max-redirects <MAX_REDIRECTS>",
            "--http-max-response-bytes <HTTP_MAX_RESPONSE_BYTES>",
            "--vlm-debug <VLM_DEBUG>",
            "--vlm-text-before-image <VLM_TEXT_BEFORE_IMAGE>",
            "--vlm-allow-truncated-content <VLM_ALLOW_TRUNCATED_CONTENT>",
            "--vlm-allow-remote-images <VLM_ALLOW_REMOTE_IMAGES>",
            "--vlm-allow-private-remote-images <VLM_ALLOW_PRIVATE_REMOTE_IMAGES>",
            "--api-max-concurrent-requests <API_MAX_CONCURRENT_REQUESTS>",
            "--task-result-timeout-seconds <TASK_RESULT_TIMEOUT_SECONDS>",
            "--task-result-download-timeout-seconds <TASK_RESULT_DOWNLOAD_TIMEOUT_SECONDS>",
            "--api-connect-timeout-seconds <API_CONNECT_TIMEOUT_SECONDS>",
            "--api-acquisition-timeout-seconds <API_ACQUISITION_TIMEOUT_SECONDS>",
            "--api-send-timeout-seconds <API_SEND_TIMEOUT_SECONDS>",
            "--api-poll-interval-seconds <API_POLL_INTERVAL_SECONDS>",
            "--archive-max-entries <ARCHIVE_MAX_ENTRIES>",
            "--archive-max-ratio <ARCHIVE_MAX_RATIO>",
            "--zip-scan-central-cap <ZIP_SCAN_CENTRAL_CAP>",
            "--zip-scan-name-cap <ZIP_SCAN_NAME_CAP>",
            "--zip-scan-depth-cap <ZIP_SCAN_DEPTH_CAP>",
            "--zip-scan-total-name-cap <ZIP_SCAN_TOTAL_NAME_CAP>",
            "--zip-scan-total-component-cap <ZIP_SCAN_TOTAL_COMPONENT_CAP>",
            "--ooxml-archive-bytes <OOXML_ARCHIVE_BYTES>",
            "--ooxml-expanded-bytes <OOXML_EXPANDED_BYTES>",
            "--ooxml-xml-entry-bytes <OOXML_XML_ENTRY_BYTES>",
            "--ooxml-xml-total-bytes <OOXML_XML_TOTAL_BYTES>",
            "--ooxml-ratio <OOXML_RATIO>",
            "--ooxml-xml-depth <OOXML_XML_DEPTH>",
            "--ooxml-xml-events <OOXML_XML_EVENTS>",
            "--ooxml-xml-attributes <OOXML_XML_ATTRIBUTES>",
            "--ooxml-xml-namespaces <OOXML_XML_NAMESPACES>",
            "--office-input-bytes <OFFICE_INPUT_BYTES>",
            "--office-output-bytes <OFFICE_OUTPUT_BYTES>",
            "--office-stderr-bytes <OFFICE_STDERR_BYTES>",
            "--office-wall-seconds <OFFICE_WALL_SECONDS>",
            "--office-cpu-seconds <OFFICE_CPU_SECONDS>",
            "--office-nofile <OFFICE_NOFILE>",
            "--office-address-space-bytes <OFFICE_ADDRESS_SPACE_BYTES>",
            "--office-active-process-limit <OFFICE_ACTIVE_PROCESS_LIMIT>",
            "--office-process-memory-bytes <OFFICE_PROCESS_MEMORY_BYTES>",
            "--office-job-memory-bytes <OFFICE_JOB_MEMORY_BYTES>",
            "--office-process-time-seconds <OFFICE_PROCESS_TIME_SECONDS>",
            "--office-job-time-seconds <OFFICE_JOB_TIME_SECONDS>",
            "-h, --help",
        ]
    );
    for values in [
        "[possible values: auto, txt, ocr]",
        "[possible values: vlm-http-client]",
        "[possible values: medium, high]",
        "[possible values: ch, ch_server, korean, ta, te, ka, th, el, arabic, east_slavic, cyrillic, devanagari, en, japan, chinese_cht, latin]",
        "[possible values: true, false]",
    ] {
        assert!(help.contains(values), "{values}");
    }
    assert!(
        help.contains("--method")
            && help.contains("--log-level")
            && help.contains("--batch-size")
            && !help.contains("--server-url")
            && !help.contains("--model")
    );
}

#[test]
#[ignore = "CLI process contract e2e"]
fn help_documents_environment_variables() {
    let output = mineru().arg("--help").output().unwrap();
    assert!(output.status.success());
    let help = String::from_utf8(output.stdout).unwrap();
    assert!(help.contains("Environment:"));
    assert!(help.contains("MINERU_VL_SERVER"));
    assert!(help.contains("MINERU_VL_MODEL_NAME"));
    assert!(help.contains("MINERU_VL_API_KEY"));
    assert!(help.contains("preferred over --api-key"));
    assert!(help.contains("docs/usage.en.md"));
}

#[test]
fn help_styles_render_ansi_only_when_color_is_enabled() {
    // Forced color (TTY-equivalent): the styled help carries ANSI escapes and the
    // uv-style layered palette (bold bright-green usage, cyan section headers,
    // bright-green flag names, italic gray placeholders).
    let mut colored = mineru::command::cli_command().color(clap::ColorChoice::Always);
    let ansi = colored.render_long_help().ansi().to_string();
    assert!(
        ansi.contains("\x1b["),
        "colored help must contain ANSI escapes"
    );
    assert!(
        ansi.contains("\x1b[1m\x1b[92m"),
        "bold bright-green usage style: {ansi:?}"
    );
    assert!(
        ansi.contains("\x1b[1m\x1b[96m"),
        "bold cyan section-header style: {ansi:?}"
    );
    assert!(
        ansi.contains("\x1b[92m"),
        "bright-green flag-name style: {ansi:?}"
    );
    assert!(
        ansi.contains("\x1b[3m\x1b[90m"),
        "italic gray placeholder style: {ansi:?}"
    );
    assert!(ansi.contains("--path"), "flag names must survive styling");

    // Plain sink (piped output / ColorChoice::Never): no escape sequences at all.
    let mut plain = mineru::command::cli_command().color(clap::ColorChoice::Never);
    let rendered = plain.render_long_help().to_string();
    assert!(
        !rendered.contains("\x1b["),
        "plain help must not contain ANSI escapes: {rendered:?}"
    );
    assert!(rendered.contains("-p, --path <PATH>"));
}

#[tokio::test]
#[ignore = "CLI process contract e2e"]
async fn api_key_flag_parses_and_warns_without_network() {
    // Parse-only: a missing input path is a runtime failure (exit 1), never a clap
    // parse failure (exit 2), proving `--api-key` is accepted without any network.
    let mut cmd = mineru();
    cmd.args(["-p", "x", "-o", "y", "--api-key", "secret"]);
    let result = command(cmd).await;
    assert_ne!(result.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&result.stderr);
    assert!(stderr.contains(
        "warning: --api-key is visible in the process list and shell history; prefer MINERU_VL_API_KEY"
    ));
}

#[test]
#[ignore = "CLI process contract e2e"]
fn version_flags_are_exact_and_need_no_inputs() {
    let expected = format!("mineru {}\n", env!("CARGO_PKG_VERSION"));
    for flag in ["-v", "--version"] {
        let output = mineru().arg(flag).output().unwrap();
        assert!(output.status.success());
        assert_eq!(String::from_utf8(output.stdout).unwrap(), expected);
        assert!(output.stderr.is_empty());
    }
}

fn encoded_image(format: ImageFormat) -> Vec<u8> {
    let mut out = Vec::new();
    DynamicImage::ImageRgba8(ImageBuffer::from_pixel(2, 2, Rgba([1, 2, 3, 255])))
        .write_to(&mut std::io::Cursor::new(&mut out), format)
        .unwrap();
    out
}

#[cfg(feature = "office")]
#[tokio::test]
#[ignore = "real Office conversion e2e"]
async fn one_level_mixed_inputs_publish_exact_origins_and_selected_pdf_source_page() {
    let input = tempfile::tempdir().unwrap();
    let output = tempfile::tempdir().unwrap();
    let pdf = input.path().join("PDF.PDF");
    multipage_pdf(&pdf);
    let png = input.path().join("image.PnG");
    std::fs::write(&png, encoded_image(ImageFormat::Png)).unwrap();
    let jpg = input.path().join("photo.JpG");
    std::fs::write(&jpg, encoded_image(ImageFormat::Jpeg)).unwrap();
    let office = [
        ("word.DoCx", office_fixtures::docx()),
        ("slides.PpTx", office_fixtures::pptx()),
        ("sheet.XlSx", office_fixtures::xlsx()),
    ];
    for (name, bytes) in &office {
        std::fs::write(input.path().join(name), bytes).unwrap();
    }
    std::fs::write(input.path().join("skip.txt"), b"skip").unwrap();
    std::fs::create_dir(input.path().join("nested")).unwrap();
    std::fs::copy(&pdf, input.path().join("nested/hidden.pdf")).unwrap();
    let (url, seen) = mock().await;
    let mut cmd = mineru();
    cmd.args(["-p"])
        .arg(input.path())
        .args(["-o"])
        .arg(output.path())
        .args(["--url", &url, "--start", "1", "--end", "1"]);
    let result = command(cmd).await;
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let stderr = String::from_utf8_lossy(&result.stderr);
    assert!(stderr.contains("skip.txt") && stderr.contains("nested"));
    for (stem, target, suffix, source) in [
        ("PDF", "vlm", "pdf", std::fs::read(&pdf).unwrap()),
        ("image", "vlm", "png", std::fs::read(&png).unwrap()),
        ("photo", "vlm", "jpg", std::fs::read(&jpg).unwrap()),
        ("word", "office", "docx", office[0].1.clone()),
        ("slides", "office", "pptx", office[1].1.clone()),
        ("sheet", "office", "xlsx", office[2].1.clone()),
    ] {
        let root = output.path().join(stem).join(target);
        assert!(root.join(format!("{stem}.md")).is_file());
        assert_eq!(
            std::fs::read(root.join(format!("{stem}_origin.{suffix}"))).unwrap(),
            source
        );
    }
    let middle: Value = serde_json::from_slice(
        &std::fs::read(output.path().join("PDF/vlm/PDF_middle.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(middle["pdf_info"][0]["page_idx"], 1);
    assert!(!output.path().join("hidden").exists());
    assert_eq!(
        seen.0
            .lock()
            .unwrap()
            .iter()
            .filter(|(kind, _, _)| kind == "completion")
            .count(),
        6
    );
}

#[cfg(feature = "office")]
#[tokio::test]
#[ignore = "real Office conversion e2e"]
async fn non_pdf_ranges_are_ignored_by_the_direct_consumer() {
    let input = tempfile::tempdir().unwrap();
    let output = tempfile::tempdir().unwrap();
    std::fs::write(
        input.path().join("image.png"),
        encoded_image(ImageFormat::Png),
    )
    .unwrap();
    std::fs::write(input.path().join("word.docx"), office_fixtures::docx()).unwrap();
    std::fs::create_dir(input.path().join("ignored.pdf")).unwrap();
    let (url, _) = mock().await;
    let mut cmd = mineru();
    cmd.args(["-p"])
        .arg(input.path())
        .args(["-o"])
        .arg(output.path())
        .args(["--url", &url, "--start", "2", "--end", "1"]);
    assert!(command(cmd).await.status.success());
    assert!(output.path().join("image/vlm/image_origin.png").is_file());
    assert!(output.path().join("word/office/word_origin.docx").is_file());
}

#[tokio::test]
#[ignore = "CLI process contract e2e"]
async fn declared_image_mismatch_preserves_existing_target_without_a_completion() {
    let input = tempfile::tempdir().unwrap();
    let output = tempfile::tempdir().unwrap();
    std::fs::write(
        input.path().join("bad.jpg"),
        encoded_image(ImageFormat::Png),
    )
    .unwrap();
    let target = output.path().join("bad/vlm");
    std::fs::create_dir_all(&target).unwrap();
    std::fs::write(target.join("sentinel"), b"old").unwrap();
    let (url, seen) = mock().await;
    let mut cmd = mineru();
    cmd.args(["-p"])
        .arg(input.path())
        .args(["-o"])
        .arg(output.path())
        .args(["--url", &url]);
    let result = command(cmd).await;
    assert!(!result.status.success());
    let stderr = String::from_utf8(result.stderr).unwrap();
    let lines = unstamped(stderr.as_bytes());
    let started = lines
        .iter()
        .position(|line| *line == "document started: bad")
        .unwrap();
    assert!(lines[started + 1].starts_with("document failed: bad:"));
    assert_eq!(
        lines
            .iter()
            .filter(|line| line.starts_with("document failed: bad:"))
            .count(),
        1
    );
    assert!(!lines.iter().any(|line| line.starts_with("failed:")));
    assert!(
        !lines
            .iter()
            .any(|line| line.starts_with("document completed:"))
    );
    assert_eq!(std::fs::read(target.join("sentinel")).unwrap(), b"old");
    assert!(!target.join("bad_origin.jpg").exists());
    assert!(
        !seen
            .0
            .lock()
            .unwrap()
            .iter()
            .any(|(kind, _, _)| kind == "completion")
    );
    assert!(!output.path().join("bad").read_dir().unwrap().any(|entry| {
        entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".vlm-")
    }));
}

#[tokio::test]
#[ignore = "CLI process contract e2e"]
async fn invalid_static_options_make_no_request_or_output() {
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("new-output");
    let (url, seen) = mock().await;
    let mut cmd = mineru();
    cmd.args(["-p", "tests/fixtures/pdf/minimal.pdf", "-o"])
        .arg(&output)
        .args(["--start", "3", "--end", "2"])
        .args(["--url", &url]);
    let result = command(cmd).await;
    assert!(!result.status.success());
    assert!(result.stdout.is_empty());
    assert_eq!(
        unstamped(&result.stderr),
        ["failed: --end must not be less than --start"]
    );
    assert!(!output.exists());
    assert!(seen.0.lock().unwrap().is_empty());
}

#[tokio::test]
#[ignore = "full CLI/API/PDF output process e2e"]
async fn behaviorless_options_warn_once_with_canonical_progress() {
    let dir = tempfile::tempdir().unwrap();
    let pdf = input(&dir);
    let output = dir.path().join("out");
    let (url, _) = mock().await;
    let mut cmd = mineru();
    cmd.args(["-p"]).arg(pdf).args(["-o"]).arg(&output).args([
        "--url",
        &url,
        "--method",
        "txt",
        "--effort",
        "high",
        "--lang",
        "en",
        "--client-side-output-generation",
        "true",
    ]);
    let result = command(cmd).await;
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(result.stdout, b"");
    assert_eq!(
        unstamped(&result.stderr),
        [
            "warning: ignored direct options: method=txt, effort=high, lang=en, client-side-output-generation=true",
            "document started: document",
            "document prepared: document",
            "document page completed: document: page=0 completed=1/1",
            "document completed: document",
        ]
    );
    assert!(output.join("document/vlm/document.md").is_file());
}

#[tokio::test]
#[ignore = "CLI process contract e2e"]
async fn api_client_side_output_rejection_precedes_input_and_network() {
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("out");
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let api_url = format!("http://{}", listener.local_addr().unwrap());
    let mut cmd = mineru();
    cmd.args(["-p"])
        .arg(dir.path().join("missing.pdf"))
        .args(["-o"])
        .arg(&output)
        .args([
            "--api-url",
            &api_url,
            "--client-side-output-generation",
            "true",
        ]);
    let result = command(cmd).await;
    assert!(!result.status.success());
    assert!(result.stdout.is_empty());
    assert_eq!(
        unstamped(&result.stderr),
        ["failed: client-side output generation is unsupported"]
    );
    assert!(!output.exists());
    assert!(!String::from_utf8_lossy(&result.stderr).contains("missing.pdf"));
    assert_eq!(
        listener.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
}

#[tokio::test]
#[ignore = "CLI process contract e2e"]
async fn api_concurrency_env_rejection_precedes_input_and_network() {
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("out");
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let api_url = format!("http://{}", listener.local_addr().unwrap());
    let mut cmd = mineru();
    cmd.args(["-p"])
        .arg(dir.path().join("missing.pdf"))
        .args(["-o"])
        .arg(&output)
        .args(["--api-url", &api_url])
        .env("MINERU_API_MAX_CONCURRENT_REQUESTS", "0");
    let result = command(cmd).await;
    assert!(!result.status.success());
    assert!(result.stdout.is_empty());
    assert_eq!(
        unstamped(&result.stderr),
        ["failed: MINERU_API_MAX_CONCURRENT_REQUESTS must be greater than zero"]
    );
    assert!(!output.exists());
    assert!(!String::from_utf8_lossy(&result.stderr).contains("missing.pdf"));
    assert_eq!(
        listener.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
}

#[tokio::test]
#[ignore = "CLI process contract e2e"]
async fn invalid_log_level_fails_before_network_or_output() {
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("out");
    let (url, seen) = mock().await;
    let mut cmd = mineru();
    cmd.args(["-p", "tests/fixtures/pdf/minimal.pdf", "-o"])
        .arg(&output)
        .args(["--url", &url])
        .env("MINERU_LOG_LEVEL", "Bearer secret");
    let result = command(cmd).await;
    assert!(!result.status.success());
    assert!(result.stdout.is_empty());
    assert_eq!(result.stderr, b"invalid MINERU_LOG_LEVEL\n");
    assert!(!output.exists());
    assert!(seen.0.lock().unwrap().is_empty());
}

#[test]
#[ignore = "full CLI/PDF network-failure process e2e"]
fn unsupported_file_is_accepted_when_a_pdf_exists() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::copy("tests/fixtures/pdf/minimal.pdf", dir.path().join("a.pdf")).unwrap();
    std::fs::write(dir.path().join("note.txt"), "x").unwrap();
    // Connection is intentionally invalid: reaching it proves enumeration accepted the PDF.
    let output_root = tempfile::tempdir().unwrap();
    let output = mineru()
        .args(["-p"])
        .arg(dir.path())
        .args(["-o"])
        .arg(output_root.path())
        .args(["--url", "http://127.0.0.1:1"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("note.txt"));
}

#[test]
#[ignore = "CLI process contract e2e"]
fn parser_rejects_noop_flags() {
    let status = mineru()
        .args(["-p", "x", "-o", "y", "--backend", "x"])
        .status()
        .unwrap();
    assert_eq!(status.code(), Some(2));
    // --log-level and --batch-size are real options now; they must parse (and fail on input,
    // not on the option spelling) instead of being rejected as no-ops.
    let status = mineru()
        .args(["-p", "x", "-o", "y", "--log-level", "debug"])
        .status()
        .unwrap();
    assert_ne!(status.code(), Some(2));
    let status = mineru()
        .args(["-p", "x", "-o", "y", "--batch-size", "4"])
        .status()
        .unwrap();
    assert_ne!(status.code(), Some(2));
}

#[tokio::test]
#[ignore = "CLI process contract e2e"]
async fn malformed_core_flags_and_env_fail_before_network_or_output() {
    let dir = tempfile::tempdir().unwrap();
    let pdf = input(&dir);
    let (url, seen) = mock().await;
    let cases: Vec<(&[&str], &str)> = vec![
        (
            &["--processing-window-size", "0"],
            "MINERU_PROCESSING_WINDOW_SIZE",
        ),
        (&["--batch-size", "0"], "MINERU_BATCH_SIZE"),
        (&["--render-workers", "+5"], "MINERU_PDF_RENDER_THREADS"),
        (
            &["--http-retry-backoff-factor", "NaN"],
            "MINERU_VLM_HTTP_RETRY_BACKOFF_FACTOR",
        ),
        (
            &["--http-max-concurrency", "184467440737095516160"],
            "MINERU_VLM_HTTP_CONCURRENCY",
        ),
    ];
    for (index, (extra, needle)) in cases.into_iter().enumerate() {
        let output = dir.path().join(format!("case-{index}"));
        let mut cmd = mineru();
        cmd.args(["-p"])
            .arg(&pdf)
            .args(["-o"])
            .arg(&output)
            .args(["--url", &url])
            .args(extra);
        let result = command(cmd).await;
        assert!(!result.status.success(), "{extra:?}");
        let stderr = String::from_utf8_lossy(&result.stderr);
        assert!(stderr.contains(needle), "{extra:?}: {stderr}");
        assert!(!output.exists(), "{extra:?}");
    }
    let output = dir.path().join("env-bad");
    let mut cmd = mineru();
    cmd.args(["-p"])
        .arg(&pdf)
        .args(["-o"])
        .arg(&output)
        .args(["--url", &url])
        .env("MINERU_MAX_PAGE_PIXELS", "1e6");
    let result = command(cmd).await;
    assert!(!result.status.success());
    let stderr = String::from_utf8_lossy(&result.stderr);
    assert!(stderr.contains("MINERU_MAX_PAGE_PIXELS"), "{stderr}");
    assert!(!output.exists());
    assert!(seen.0.lock().unwrap().is_empty());

    // Strict boolean grammar: `1`/`yes`/`on` are rejected, not silently treated as false.
    for (env_name, value) in [
        ("MINERU_FORMULA_ENABLE", "1"),
        ("MINERU_TABLE_ENABLE", "yes"),
        ("MINERU_IMAGE_ANALYSIS_ENABLE", "on"),
        ("MINERU_VL_DEBUG_ENABLE", ""),
    ] {
        let output = dir.path().join("env-bool-bad");
        let mut cmd = mineru();
        cmd.args(["-p"])
            .arg(&pdf)
            .args(["-o"])
            .arg(&output)
            .args(["--url", &url])
            .env(env_name, value);
        let result = command(cmd).await;
        assert!(!result.status.success(), "{env_name}={value}");
        let stderr = String::from_utf8_lossy(&result.stderr);
        assert!(stderr.contains(env_name), "{env_name}: {stderr}");
        assert!(!output.exists(), "{env_name}={value}");
    }
    assert!(seen.0.lock().unwrap().is_empty());
}

#[tokio::test]
#[ignore = "full CLI/API/PDF request-and-output process e2e"]
async fn boolean_env_and_flags_reach_the_route_with_strict_precedence() {
    let dir = tempfile::tempdir().unwrap();
    let pdf = input(&dir);
    // Env-disabled semantics suppress all three semantic candidates: one layout request only.
    let (url, seen) = mock_with(true, None).await;
    let mut cmd = mineru();
    cmd.args(["-p"])
        .arg(&pdf)
        .args(["-o"])
        .arg(dir.path().join("env-out"))
        .args(["--url", &url])
        .env("MINERU_FORMULA_ENABLE", "false")
        .env("MINERU_TABLE_ENABLE", "false")
        .env("MINERU_IMAGE_ANALYSIS_ENABLE", "false");
    assert!(command(cmd).await.status.success());
    let calls = seen.0.lock().unwrap();
    assert_eq!(
        calls
            .iter()
            .filter(|(kind, _, _)| kind == "completion")
            .count(),
        1
    );
    assert!(calls.iter().any(|(kind, body, _)| kind == "completion"
        && body["messages"].to_string().contains("Layout Detection")));
    drop(calls);

    // An explicit CLI flag beats the frozen environment (false via env, true via CLI).
    let (url, seen) = mock_with(true, None).await;
    let mut cmd = mineru();
    cmd.args(["-p"])
        .arg(&pdf)
        .args(["-o"])
        .arg(dir.path().join("cli-out"))
        .args([
            "--url",
            &url,
            "--formula",
            "true",
            "--table",
            "true",
            "--image-analysis",
            "true",
        ])
        .env("MINERU_FORMULA_ENABLE", "false")
        .env("MINERU_TABLE_ENABLE", "false")
        .env("MINERU_IMAGE_ANALYSIS_ENABLE", "false");
    assert!(command(cmd).await.status.success());
    let calls = seen.0.lock().unwrap();
    let completions = calls
        .iter()
        .filter(|(kind, _, _)| kind == "completion")
        .count();
    // Env said false but the explicit CLI booleans re-enable all three semantic candidates.
    assert_eq!(completions, 4, "{calls:?}");
}

#[tokio::test]
#[ignore = "CLI process contract e2e"]
async fn remote_mode_rejects_local_vlm_transport_controls_before_work() {
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("out");
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let api_url = format!("http://{}", listener.local_addr().unwrap());
    for extra in [
        &["--http-max-concurrency", "50"][..],
        &["--batch-size", "8"][..],
        &["--vlm-debug", "true"][..],
        &["--page-concurrency", "9"][..],
    ] {
        let mut cmd = mineru();
        cmd.args(["-p", "tests/fixtures/pdf/minimal.pdf", "-o"])
            .arg(&output)
            .args(["--api-url", &api_url])
            .args(extra);
        let result = command(cmd).await;
        assert!(!result.status.success(), "{extra:?}");
        assert!(
            String::from_utf8_lossy(&result.stderr).contains("local VLM transport controls"),
            "{extra:?}"
        );
        assert!(!output.exists(), "{extra:?}");
    }
    let mut cmd = mineru();
    cmd.args(["-p", "tests/fixtures/pdf/minimal.pdf", "-o"])
        .arg(&output)
        .args(["--api-url", &api_url])
        .env("MINERU_VL_DEBUG_ENABLE", "true");
    let result = command(cmd).await;
    assert!(!result.status.success());
    assert!(
        String::from_utf8_lossy(&result.stderr).contains("MINERU_VL_DEBUG_ENABLE"),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(!output.exists());
    assert_eq!(
        listener.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
}

#[tokio::test]
#[ignore = "full CLI/API/PDF request-and-output process e2e"]
async fn cli_core_overrides_and_batch_reach_the_scheduler() {
    let dir = tempfile::tempdir().unwrap();
    let pdf = input(&dir);
    // A deterministic three-candidate layout plus held completions makes the real semantic
    // scheduler's active/peak admissions observable: batch 1 admits one request at a time,
    // batch 3 admits all three candidates concurrently.
    for (batch, expected_peak) in [("1", 1), ("3", 3)] {
        let (url, seen, peak) = batch_mock().await;
        let out = dir.path().join(format!("out-{batch}"));
        let mut cmd = mineru();
        cmd.args(["-p"])
            .arg(&pdf)
            .args(["-o"])
            .arg(&out)
            .args([
                "--url",
                &url,
                "--processing-window-size",
                "1",
                "--page-concurrency",
                "9",
                "--render-workers",
                "4",
                "--batch-size",
                batch,
                "--http-max-concurrency",
                "6",
            ])
            // The frozen environment must lose to the explicit CLI batch value.
            .env("MINERU_PROCESSING_WINDOW_SIZE", "2")
            .env("MINERU_OFFICIAL_PAGE_CONCURRENCY", "5")
            .env("MINERU_BATCH_SIZE", "1");
        let result = command(cmd).await;
        assert!(
            result.status.success(),
            "batch {batch}: {}",
            String::from_utf8_lossy(&result.stderr)
        );
        let observed_peak = peak.load(Ordering::SeqCst);
        assert_eq!(
            observed_peak, expected_peak,
            "batch {batch}: observed peak concurrent semantic requests"
        );
        let calls = seen.0.lock().unwrap();
        let completions = calls
            .iter()
            .filter(|(kind, _, _)| kind == "completion")
            .count();
        // One layout plus three semantic candidates per page prove real admission.
        assert_eq!(completions, 4, "batch {batch}: {calls:?}");
        assert!(calls.iter().any(|(kind, body, _)| kind == "completion"
            && body["messages"].to_string().contains("Layout Detection")));
        let middle: Value = serde_json::from_slice(
            &std::fs::read(out.join("document/vlm/document_middle.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(middle["pdf_info"][0]["page_idx"], 0);
    }
}

#[tokio::test]
#[ignore = "full CLI/API/PDF request-and-output process e2e"]
async fn vlm_debug_flag_reaches_the_http_request_body() {
    let dir = tempfile::tempdir().unwrap();
    let pdf = input(&dir);
    let (url, seen, _) = batch_mock().await;
    let mut cmd = mineru();
    cmd.args(["-p"])
        .arg(&pdf)
        .args(["-o"])
        .arg(dir.path().join("out"))
        .args(["--url", &url, "--vlm-debug", "true"]);
    assert!(command(cmd).await.status.success(), "vlm-debug run failed");
    let calls = seen.0.lock().unwrap();
    let (_, request, _) = calls
        .iter()
        .find(|(kind, _, _)| kind == "completion")
        .unwrap();
    assert_eq!(request["vllm_xargs"]["debug"], json!(true), "{request}");
}

#[tokio::test]
#[ignore = "CLI process contract e2e"]
async fn backend_and_zero_batch_fail_before_network_or_output() {
    let dir = tempfile::tempdir().unwrap();
    let pdf = input(&dir);
    let (url, seen) = mock().await;
    for extra in [["--backend", "pipeline"], ["--batch-size", "0"]] {
        let output = dir.path().join(extra[1]);
        let mut cmd = mineru();
        cmd.args(["-p"])
            .arg(&pdf)
            .args(["-o"])
            .arg(&output)
            .args(["--url", &url])
            .args(extra);
        assert!(!command(cmd).await.status.success());
        assert!(!output.exists());
    }
    assert!(seen.0.lock().unwrap().is_empty());
}

#[tokio::test]
#[ignore = "full CLI/PDF fail-stop process e2e"]
async fn sequential_documents_stop_at_failed_document() {
    let dir = tempfile::tempdir().unwrap();
    for name in ["a.pdf", "b.pdf", "c.pdf"] {
        std::fs::copy("tests/fixtures/pdf/minimal.pdf", dir.path().join(name)).unwrap();
    }
    let output_root = tempfile::tempdir().unwrap();
    let output = output_root.path().join("out");
    let (url, seen) = failing_mock(2).await;
    let mut cmd = mineru();
    cmd.args(["-p"])
        .arg(dir.path())
        .args(["-o"])
        .arg(&output)
        .args(["--url", &url]);
    let result = command(cmd).await;
    assert!(!result.status.success());
    let stderr = String::from_utf8_lossy(&result.stderr);
    assert!(stderr.contains("failed"));
    assert!(!stderr.contains(&format!("completed {}", dir.path().join("b.pdf").display())));
    let calls = seen.0.lock().unwrap();
    assert_eq!(
        calls
            .iter()
            .filter(|(kind, _, _)| kind == "completion")
            .count(),
        2
    );
    assert!(output.join("a/vlm/a.md").is_file());
    assert!(!output.join("b/vlm").exists());
    assert!(!output.join("c/vlm").exists());
}

#[tokio::test]
#[ignore = "CLI process contract e2e"]
async fn invalid_process_inputs_make_no_network_or_output() {
    let dir = tempfile::tempdir().unwrap();
    let (url, seen) = mock().await;
    assert!(!command(mineru()).await.status.success());
    for args in [["-o", "out"], ["-p", "input.pdf"]] {
        let mut cmd = mineru();
        cmd.args(args);
        assert!(!command(cmd).await.status.success());
    }
    for path in [dir.path().join("empty"), dir.path().join("no-match")].iter() {
        std::fs::create_dir_all(path).unwrap();
        if path.ends_with("no-match") {
            std::fs::write(path.join("x.txt"), b"x").unwrap();
        }
        let output = path.join("out");
        let mut cmd = mineru();
        cmd.args(["-p"])
            .arg(path)
            .args(["-o"])
            .arg(&output)
            .args(["--url", &url]);
        assert!(!command(cmd).await.status.success());
        assert!(!output.exists());
    }
    assert!(seen.0.lock().unwrap().is_empty());
}

#[cfg(unix)]
#[tokio::test]
#[ignore = "CLI process contract e2e"]
async fn special_files_make_no_network_or_socket_output() {
    use std::os::unix::net::UnixListener;
    let dir = tempfile::tempdir().unwrap();
    let (url, seen) = mock().await;
    let socket = UnixListener::bind(dir.path().join("input.sock")).unwrap();
    let mut cmd = mineru();
    cmd.args(["-p"])
        .arg(dir.path().join("input.sock"))
        .args(["-o"])
        .arg(dir.path().join("socket-out"))
        .args(["--url", &url]);
    let result = command(cmd).await;
    assert!(!result.status.success());
    assert!(
        String::from_utf8_lossy(&result.stderr).contains("unsupported symlink or special file")
    );
    assert!(!dir.path().join("socket-out").exists());
    assert!(seen.0.lock().unwrap().is_empty());
    drop(socket);
}

#[tokio::test]
#[ignore = "full CLI/API/PDF multi-document process e2e"]
async fn duplicate_canonical_stems_receive_smallest_suffixes() {
    let dir = tempfile::tempdir().unwrap();
    let (url, seen) = mock().await;
    std::fs::copy("tests/fixtures/pdf/minimal.pdf", dir.path().join("a?.pdf")).unwrap();
    std::fs::copy("tests/fixtures/pdf/minimal.pdf", dir.path().join("a*.pdf")).unwrap();
    let output_root = tempfile::tempdir().unwrap();
    let output = output_root.path().join("duplicate-out");
    let mut cmd = mineru();
    cmd.args(["-p"])
        .arg(dir.path())
        .args(["-o"])
        .arg(&output)
        .args(["--url", &url]);
    let result = command(cmd).await;
    assert!(result.status.success());
    assert!(output.join("a_/vlm/a__origin.pdf").is_file());
    assert!(output.join("a__2/vlm/a__2_origin.pdf").is_file());
    assert_eq!(seen.0.lock().unwrap().len(), 2);
}

#[tokio::test]
#[ignore = "full CLI/API/PDF request-and-output process e2e"]
async fn cli_env_precedence_and_canonical_request_are_real() {
    let dir = tempfile::tempdir().unwrap();
    let pdf = input(&dir);
    let (url, seen) = mock().await;
    let mut cmd = mineru();
    cmd.args(["-p"])
        .arg(&pdf)
        .args(["-o"])
        .arg(dir.path().join("out"))
        .args([
            "--url",
            &url,
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
        .env("MINERU_VL_SERVER", "not-a-url")
        .env("MINERU_VL_MODEL_NAME", "env-model")
        .env("MINERU_VL_API_KEY", "env-key");
    assert!(command(cmd).await.status.success());
    let calls = seen.0.lock().unwrap();
    assert_eq!(
        calls.iter().filter(|(kind, _, _)| kind == "models").count(),
        0
    );
    let (_, request, auth) = calls
        .iter()
        .find(|(kind, _, _)| kind == "completion")
        .unwrap();
    assert_eq!(request["model"], "env-model");
    assert_eq!(auth.as_deref(), Some("Bearer env-key"));
    assert_eq!(
        calls
            .iter()
            .filter(|(kind, _, _)| kind == "completion")
            .count(),
        1
    );
    // Options are consumed by the official route before its canonical OpenAI request.
    let vlm = dir.path().join("out/document/vlm");
    for name in [
        "document.md",
        "document_middle.json",
        "document_model.json",
        "document_content_list.json",
        "document_content_list_v2.json",
        "document_layout.pdf",
    ] {
        assert!(vlm.join(name).is_file(), "{name}");
    }
    assert!(!dir.path().join("out/document/document.json").exists());
}

#[tokio::test]
async fn chinese_basename_stem_reaches_final_output_paths_and_artifacts() {
    let dir = tempfile::tempdir().unwrap();
    let pdf = dir.path().join("文档《报告》·2026.pdf");
    std::fs::copy("tests/fixtures/pdf/minimal.pdf", &pdf).unwrap();
    let (url, seen) = mock().await;
    let mut cmd = mineru();
    cmd.args(["-p"])
        .arg(&pdf)
        .args(["-o"])
        .arg(dir.path().join("out"))
        .args(["--url", &url]);
    let result = command(cmd).await;
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let vlm = dir.path().join("out/文档《报告》·2026/vlm");
    for name in [
        "文档《报告》·2026.md",
        "文档《报告》·2026_middle.json",
        "文档《报告》·2026_model.json",
        "文档《报告》·2026_content_list.json",
        "文档《报告》·2026_content_list_v2.json",
        "文档《报告》·2026_layout.pdf",
    ] {
        assert!(vlm.join(name).is_file(), "{name}");
    }
    assert!(
        seen.0
            .lock()
            .unwrap()
            .iter()
            .any(|(kind, _, _)| kind == "completion")
    );
}

#[tokio::test]
#[ignore = "full CLI/API/PDF rendering process e2e"]
async fn selected_page_and_disabled_semantics_only_request_layout() {
    let dir = tempfile::tempdir().unwrap();
    let pdf = dir.path().join("document.pdf");
    multipage_pdf(&pdf);
    let (url, seen) = mock_with(true, None).await;
    let mut cmd = mineru();
    cmd.args(["-p"])
        .arg(&pdf)
        .args(["-o"])
        .arg(dir.path().join("out"))
        .args([
            "--url",
            &url,
            "--start",
            "1",
            "--end",
            "1",
            "--formula",
            "false",
            "--table",
            "false",
            "--image-analysis",
            "false",
        ]);
    let result = command(cmd).await;
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let middle: Value = serde_json::from_slice(
        &std::fs::read(dir.path().join("out/document/vlm/document_middle.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(middle["pdf_info"][0]["page_idx"], 1);
    let calls = seen.0.lock().unwrap();
    assert_eq!(
        calls
            .iter()
            .filter(|(kind, _, _)| kind == "completion")
            .count(),
        1
    );
    assert!(calls.iter().any(|(kind, body, _)| kind == "completion"
        && body["messages"].to_string().contains("Layout Detection")));
}

#[cfg(unix)]
#[tokio::test]
#[ignore = "CLI process contract e2e"]
async fn output_recheck_rejects_model_discovery_mutation_before_completion() {
    let dir = tempfile::tempdir().unwrap();
    let pdf = input(&dir);
    let output = dir.path().join("out");
    let (url, seen) = mock_with(false, Some(output.clone())).await;
    let mut cmd = mineru();
    cmd.args(["-p"])
        .arg(pdf)
        .args(["-o"])
        .arg(&output)
        .arg("--url")
        .arg(url)
        .env_remove("MINERU_VL_MODEL_NAME");
    assert!(!command(cmd).await.status.success());
    let calls = seen.0.lock().unwrap();
    assert_eq!(
        calls.iter().filter(|(kind, _, _)| kind == "models").count(),
        1
    );
    assert!(
        std::fs::symlink_metadata(output.join("document"))
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert_eq!(
        calls
            .iter()
            .filter(|(kind, _, _)| kind == "completion")
            .count(),
        0
    );
    assert!(!output.join("document/vlm").exists());
}

#[tokio::test]
#[ignore = "full CLI/API/PDF authenticated process e2e"]
async fn env_model_and_key_bypass_discovery() {
    let dir = tempfile::tempdir().unwrap();
    let pdf = input(&dir);
    let (url, seen) = mock().await;
    let mut cmd = mineru();
    cmd.args(["-p"])
        .arg(pdf)
        .args(["-o"])
        .arg(dir.path().join("out"))
        .env("MINERU_VL_SERVER", &url)
        .env("MINERU_VL_MODEL_NAME", "env-model")
        .env("MINERU_VL_API_KEY", "env-key");
    assert!(command(cmd).await.status.success());
    let calls = seen.0.lock().unwrap();
    assert_eq!(
        calls.iter().filter(|(kind, _, _)| kind == "models").count(),
        0
    );
    let (_, request, auth) = calls
        .iter()
        .find(|(kind, _, _)| kind == "completion")
        .unwrap();
    assert_eq!(request["model"], "env-model");
    assert_eq!(auth.as_deref(), Some("Bearer env-key"));
}

#[tokio::test]
#[ignore = "full CLI/API/PDF model-discovery process e2e"]
async fn absent_model_discovers_exactly_once() {
    let dir = tempfile::tempdir().unwrap();
    let pdf = input(&dir);
    let (url, seen) = mock().await;
    let mut cmd = mineru();
    cmd.args(["-p"])
        .arg(pdf)
        .args(["-o"])
        .arg(dir.path().join("out"))
        .env("MINERU_VL_SERVER", &url)
        .env_remove("MINERU_VL_MODEL_NAME");
    assert!(command(cmd).await.status.success());
    let calls = seen.0.lock().unwrap();
    assert_eq!(
        calls.iter().filter(|(kind, _, _)| kind == "models").count(),
        1
    );
    assert_eq!(
        calls
            .iter()
            .filter(|(kind, _, _)| kind == "completion")
            .count(),
        1
    );
    assert_eq!(
        calls
            .iter()
            .find(|(kind, _, _)| kind == "completion")
            .unwrap()
            .1["model"],
        "discovered"
    );
}

#[cfg(feature = "office")]
#[tokio::test]
#[ignore = "full CLI/API/PDF process e2e"]
async fn api_mode_mixed_inputs_use_exact_forms_waves_and_layouts() {
    async fn health(State(state): State<Arc<Mutex<ApiState>>>) -> Json<Value> {
        state.lock().unwrap().health += 1;
        Json(
            json!({"status":"healthy","protocol_version":2,"max_concurrent_requests":2,"processing_window_size":1}),
        )
    }
    async fn submit(
        State(state): State<Arc<Mutex<ApiState>>>,
        mut multipart: Multipart,
    ) -> (StatusCode, Json<Value>) {
        let mut parts = Vec::new();
        while let Some(field) = multipart.next_field().await.unwrap() {
            let part = MultipartPart {
                name: field.name().unwrap().to_owned(),
                filename: field.file_name().map(str::to_owned),
                content_type: field.content_type().map(str::to_owned),
                bytes: field.bytes().await.unwrap().to_vec(),
            };
            parts.push(part);
        }
        let file = parts.iter().find(|part| part.name == "files").unwrap();
        let id = file
            .filename
            .as_deref()
            .unwrap()
            .split('.')
            .next()
            .unwrap()
            .to_owned();
        let mut state = state.lock().unwrap();
        if id == "c" {
            state.third_after_layouts = state.output.join("a/vlm/a_layout.pdf").is_file()
                && state.output.join("b/vlm/b_layout.pdf").is_file();
        }
        state.tasks.push((id.clone(), parts));
        let base = state.base.clone();
        (
            StatusCode::ACCEPTED,
            Json(
                json!({"task_id":format!("{base}/task/{id}"),"status_url":format!("{base}/status/{id}"),"result_url":format!("{base}/result/{id}")}),
            ),
        )
    }
    async fn status(
        State(state): State<Arc<Mutex<ApiState>>>,
        AxumPath(id): AxumPath<String>,
    ) -> Json<Value> {
        let mut state = state.lock().unwrap();
        let count = state.statuses.entry(id.clone()).or_default();
        *count += 1;
        Json(json!({"status":"completed"}))
    }
    async fn result(
        State(state): State<Arc<Mutex<ApiState>>>,
        AxumPath(id): AxumPath<String>,
    ) -> axum::response::Response {
        let (barrier, archive) = {
            let mut state = state.lock().unwrap();
            state.results += 1;
            (state.barrier.clone(), state.archives[&id].clone())
        };
        if matches!(id.as_str(), "a" | "b")
            && tokio::time::timeout(std::time::Duration::from_secs(15), barrier.wait())
                .await
                .is_err()
        {
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
        ([(header::CONTENT_TYPE, "application/zip")], archive).into_response()
    }

    let input = tempfile::tempdir().unwrap();
    let output_root = tempfile::tempdir().unwrap();
    let output = output_root.path().join("absent-output");
    let pdf = std::fs::read("tests/fixtures/pdf/minimal.pdf").unwrap();
    let png = encoded_image(ImageFormat::Png);
    let docx = office_fixtures::docx();
    std::fs::write(input.path().join("a.pdf"), &pdf).unwrap();
    std::fs::write(input.path().join("b.png"), &png).unwrap();
    std::fs::write(input.path().join("c.docx"), &docx).unwrap();
    let archives = Arc::new(std::collections::BTreeMap::from([
        ("a".into(), result_zip("a", "vlm", "pdf", &pdf)),
        ("b".into(), result_zip("b", "vlm", "png", &png)),
        ("c".into(), result_zip("c", "office", "docx", &docx)),
    ]));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let state = Arc::new(Mutex::new(ApiState {
        base: base.clone(),
        output: output.clone(),
        barrier: Arc::new(tokio::sync::Barrier::new(2)),
        archives,
        health: 0,
        tasks: Vec::new(),
        statuses: std::collections::BTreeMap::new(),
        results: 0,
        third_after_layouts: false,
    }));
    let app = Router::new()
        .route("/health", get(health))
        .route("/tasks", post(submit))
        .route("/status/{id}", get(status))
        .route("/result/{id}", get(result))
        .with_state(state.clone());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let _ = env!("CARGO_BIN_EXE_mineru-office-convert");
    let mut cmd = mineru();
    cmd.args(["-p"])
        .arg(input.path())
        .args(["-o"])
        .arg(&output)
        .args([
            "--api-url",
            &format!("{base}///"),
            "--url",
            "http://model.example/v1/",
            "--method",
            "ocr",
            "--effort",
            "high",
            "--lang",
            "en",
            "--formula",
            "false",
            "--table",
            "false",
            "--image-analysis",
            "false",
            "--start",
            "0",
            "--end",
            "0",
        ])
        .env("MINERU_API_MAX_CONCURRENT_REQUESTS", "3")
        .env("MINERU_TASK_RESULT_TIMEOUT_SECONDS", "30")
        .env("MINERU_TASK_RESULT_DOWNLOAD_TIMEOUT_SECONDS", "30")
        .env("MINERU_VL_SERVER", "http://poison.invalid");
    let run = command(cmd).await;
    assert!(
        run.status.success(),
        "{}",
        String::from_utf8_lossy(&run.stderr)
    );
    assert!(run.stdout.is_empty());
    let stderr = String::from_utf8(run.stderr).unwrap();
    let events: Vec<_> = unstamped(stderr.as_bytes())
        .into_iter()
        .filter(|line| line.contains("task#1 [a]"))
        .collect();
    assert_eq!(
        events,
        [
            "api submitted: task#1 [a]",
            "api downloading: task#1 [a]",
            "api extracting: task#1 [a]",
            "api completed: task#1 [a]"
        ]
    );
    assert!(!stderr.contains("api warning:") && !stderr.contains("api failed:"));
    let observed = state.lock().unwrap();
    assert_eq!(
        (observed.health, observed.tasks.len(), observed.results),
        (1, 3, 3)
    );
    assert!(observed.third_after_layouts);
    assert!(observed.statuses.values().all(|count| *count == 1));
    drop(observed);
    let expected = [
        ("a", "a.pdf", "application/pdf", &pdf, "vlm", "pdf"),
        ("b", "b.png", "image/png", &png, "vlm", "png"),
        (
            "c",
            "c.docx",
            "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
            &docx,
            "office",
            "docx",
        ),
    ];
    let mut tasks = state.lock().unwrap().tasks.clone();
    tasks.sort_by(|left, right| left.0.cmp(&right.0));
    for ((stem, parts), (_, filename, mime, bytes, kind, extension)) in tasks.iter().zip(expected) {
        assert_eq!(parts.len(), 19);
        let expected_parts = [
            ("lang_list", "ch"),
            ("backend", "vlm-http-client"),
            ("effort", "high"),
            ("parse_method", "ocr"),
            ("formula_enable", "false"),
            ("table_enable", "false"),
            ("image_analysis", "false"),
            ("return_md", "true"),
            ("return_middle_json", "true"),
            ("return_model_output", "true"),
            ("return_content_list", "true"),
            ("return_images", "true"),
            ("response_format_zip", "true"),
            ("return_original_file", "true"),
            ("client_side_output_generation", "false"),
            ("start_page_id", "0"),
            ("end_page_id", "0"),
            ("server_url", "http://model.example/v1/"),
        ];
        for (part, (name, value)) in parts[..18].iter().zip(expected_parts) {
            assert_eq!(
                (part.name.as_str(), part.bytes.as_slice()),
                (name, value.as_bytes())
            );
        }
        let file = &parts[18];
        assert_eq!(
            (
                file.name.as_str(),
                file.filename.as_deref(),
                file.content_type.as_deref(),
                file.bytes.as_slice()
            ),
            ("files", Some(filename), Some(mime), bytes.as_slice())
        );
        assert_eq!(stem, &filename[..1]);
        let root = output.join(stem).join(kind);
        assert_eq!(
            std::fs::read(root.join(format!("{stem}_origin.{extension}"))).unwrap(),
            bytes.as_slice()
        );
        assert!(root.join(format!("{stem}_middle.json")).is_file());
        assert_eq!(
            lopdf::Document::load(root.join(format!("{stem}_layout.pdf")))
                .unwrap()
                .get_pages()
                .len(),
            1
        );
    }
    assert!(!output.read_dir().unwrap().any(|entry| {
        entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".mineru-extract-")
    }));
}

#[tokio::test]
#[ignore = "CLI process contract e2e"]
async fn api_mode_sorts_typed_failures_without_generic_duplicate() {
    struct FailureState {
        barrier: Arc<tokio::sync::Barrier>,
        replied: Arc<tokio::sync::Notify>,
        health: usize,
        stems: Vec<String>,
        replies: Vec<String>,
        statuses: usize,
        results: usize,
    }
    async fn health(State(state): State<Arc<Mutex<FailureState>>>) -> Json<Value> {
        state.lock().unwrap().health += 1;
        Json(
            json!({"status":"healthy","protocol_version":2,"max_concurrent_requests":3,"processing_window_size":1}),
        )
    }
    async fn submit(
        State(state): State<Arc<Mutex<FailureState>>>,
        mut multipart: Multipart,
    ) -> (StatusCode, String) {
        let mut files = Vec::new();
        while let Some(field) = multipart.next_field().await.unwrap() {
            let name = field.name().unwrap().to_owned();
            let filename = field.file_name().map(str::to_owned);
            let _ = field.bytes().await.unwrap();
            if name == "files" {
                files.push(filename.unwrap());
            }
        }
        assert_eq!(files.len(), 1);
        let stem = files.pop().unwrap().split('.').next().unwrap().to_owned();
        let (barrier, replied) = {
            let mut state = state.lock().unwrap();
            state.stems.push(stem.clone());
            (state.barrier.clone(), state.replied.clone())
        };
        if tokio::time::timeout(std::time::Duration::from_secs(15), barrier.wait())
            .await
            .is_err()
        {
            return (StatusCode::INTERNAL_SERVER_ERROR, "barrier timeout".into());
        }
        if stem == "a" {
            if tokio::time::timeout(std::time::Duration::from_secs(15), replied.notified())
                .await
                .is_err()
            {
                return (StatusCode::INTERNAL_SERVER_ERROR, "reply timeout".into());
            }
        }
        state.lock().unwrap().replies.push(stem.clone());
        if stem != "a" {
            replied.notify_one();
        }
        (StatusCode::BAD_REQUEST, format!("failure-{stem}"))
    }
    async fn status(
        State(state): State<Arc<Mutex<FailureState>>>,
        AxumPath(_): AxumPath<String>,
    ) -> StatusCode {
        state.lock().unwrap().statuses += 1;
        StatusCode::INTERNAL_SERVER_ERROR
    }
    async fn result(
        State(state): State<Arc<Mutex<FailureState>>>,
        AxumPath(_): AxumPath<String>,
    ) -> StatusCode {
        state.lock().unwrap().results += 1;
        StatusCode::INTERNAL_SERVER_ERROR
    }

    let input = tempfile::tempdir().unwrap();
    let output_root = tempfile::tempdir().unwrap();
    let output = output_root.path().join("absent-output");
    std::fs::copy("tests/fixtures/pdf/minimal.pdf", input.path().join("a.pdf")).unwrap();
    std::fs::write(input.path().join("b.png"), encoded_image(ImageFormat::Png)).unwrap();
    std::fs::write(input.path().join("c.docx"), office_fixtures::docx()).unwrap();
    let state = Arc::new(Mutex::new(FailureState {
        barrier: Arc::new(tokio::sync::Barrier::new(3)),
        replied: Arc::new(tokio::sync::Notify::new()),
        health: 0,
        stems: Vec::new(),
        replies: Vec::new(),
        statuses: 0,
        results: 0,
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let app = Router::new()
        .route("/health", get(health))
        .route("/tasks", post(submit))
        .route("/status/{id}", get(status))
        .route("/result/{id}", get(result))
        .with_state(state.clone());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let mut cmd = mineru();
    cmd.args(["-p"])
        .arg(input.path())
        .args(["-o"])
        .arg(&output)
        .args(["--api-url", &base])
        .env("MINERU_API_MAX_CONCURRENT_REQUESTS", "3")
        .env("MINERU_TASK_RESULT_TIMEOUT_SECONDS", "30")
        .env("MINERU_TASK_RESULT_DOWNLOAD_TIMEOUT_SECONDS", "30");
    let run = command(cmd).await;
    assert!(!run.status.success());
    assert!(run.stdout.is_empty());
    let stderr = String::from_utf8(run.stderr).unwrap();
    let lines = unstamped(stderr.as_bytes());
    assert_eq!(
        lines,
        [
            "api failed: task#1 [a]: task submission HTTP 400 Bad Request: failure-a",
            "api failed: task#2 [b]: task submission HTTP 400 Bad Request: failure-b",
            "api failed: task#3 [c]: task submission HTTP 400 Bad Request: failure-c",
        ]
    );
    assert!(!lines.iter().any(|line| line.starts_with("failed:")));
    assert!(!stderr.contains("api submitted:") && !stderr.contains("api completed:"));
    assert!(!stderr.contains("document started:") && !stderr.contains("document completed:"));
    let observed = state.lock().unwrap();
    assert_eq!(observed.health, 1);
    let mut stems = observed.stems.clone();
    stems.sort();
    assert_eq!(stems, ["a", "b", "c"]);
    assert_ne!(observed.replies.first().map(String::as_str), Some("a"));
    assert_eq!((observed.statuses, observed.results), (0, 0));
    drop(observed);
    assert!(output.is_dir());
    assert!(output.read_dir().unwrap().next().is_none());
}
