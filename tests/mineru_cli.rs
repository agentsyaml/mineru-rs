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
#[path = "support/legacy_fixtures.rs"]
#[allow(dead_code)]
mod legacy_fixtures;
#[path = "support/office_fixtures.rs"]
#[allow(dead_code)]
mod office_fixtures;

fn mineru() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_mineru"));
    command.env("MINERU_VL_MODEL_NAME", "mock");
    for name in [
        "MINERU_VL_SERVER",
        "MINERU_VL_API_KEY",
        "MINERU_MODEL_STACK",
        "MINERU_OFFICIAL_PYTHON",
        "MINERU_OFFICIAL_WORKER_MODE",
        "MINERU_MODEL_BASE_DIR",
        "MINERU_CONFIG",
        "MINERU_VLM_END_TOKEN",
        "MINERU_VLM_TEXT_BEFORE_IMAGE",
        "MINERU_VLM_ALLOW_TRUNCATED_CONTENT",
        "MINERU_VLM_ALLOW_REMOTE_IMAGES",
        "MINERU_VLM_ALLOW_PRIVATE_REMOTE_IMAGES",
        "MINERU_OFFICE_INPUT_BYTES",
        "MINERU_OFFICE_OUTPUT_BYTES",
        "MINERU_OFFICE_STDERR_BYTES",
        "MINERU_OFFICE_WALL_SECONDS",
        "MINERU_OFFICE_CPU_SECONDS",
        "MINERU_OFFICE_NOFILE",
        "MINERU_OFFICE_ADDRESS_SPACE_BYTES",
        "MINERU_OFFICE_ACTIVE_PROCESS_LIMIT",
        "MINERU_OFFICE_PROCESS_MEMORY_BYTES",
        "MINERU_OFFICE_JOB_MEMORY_BYTES",
        "MINERU_OFFICE_PROCESS_TIME_SECONDS",
        "MINERU_OFFICE_JOB_TIME_SECONDS",
    ] {
        command.env_remove(name);
    }
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
    command.env_remove("MINERU_VLM_TEMPERATURE_RETRY");
    command.env_remove("MINERU_VL_DEBUG_ENABLE");
    command.env_remove("MINERU_OFFICE_FAKE_CHILD");
    command.env_remove("MINERU_OFFICE_FAKE_MODE");
    command.env_remove("MINERU_OFFICE_FAKE_READY");
    command
}

#[cfg(feature = "legacy-office")]
fn bundled_office_helper() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_BIN_EXE_mineru-office-convert"))
}

#[cfg(unix)]
fn fake_official_python(root: &std::path::Path) -> std::path::PathBuf {
    fake_official_python_with_background(root, false)
}

#[cfg(unix)]
fn fake_official_python_with_background(
    root: &std::path::Path,
    background: bool,
) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let script = root.join("fake-official-python");
    let background = background
        .then(|| {
            format!(
                "sleep 30 &\nbackground_pid=$!\nprintf '%s' \"$background_pid\" > \"{}\"",
                root.join("background-pid").display()
            )
        })
        .unwrap_or_default();
    std::fs::write(
        &script,
        format!(r##"#!/bin/sh
request=$(cat)
id=$(printf '%s' "$request" | sed -n 's/.*"request_id":"\([^"]*\)".*/\1/p')
bundle=$(printf '%s' "$request" | sed -n 's/.*"bundle_path":"\([^"]*\)".*/\1/p')
effort=$(printf '%s' "$request" | sed -n 's/.*"effort":"\([^"]*\)".*/\1/p')
page=$(printf '%s' "$request" | sed -n 's/.*"page_range":"\([^"]*\)".*/\1/p')
{background}
printf '%s\n' "$$" >> "{pid_file}"
mkdir -p "$bundle"
printf 'effort=%s page=%s\n' "$effort" "$page" > "$bundle/markdown.md"
printf '%s' '{{"schema_version":"1.0","pages":[{{}}],"_backend":"hybrid"}}' > "$bundle/middle_json.json"
printf '%s' '[]' > "$bundle/content_list.json"
printf '%s' '{{}}' > "$bundle/structured_content.json"
printf '{{"protocol":"mineru-rs-official-worker/1","request_id":"%s","status":"ok","package_version":"4.0.0a6","schema_version":"1.0","backend":"hybrid-http-client","bundle_name":"hybrid-v4"}}\n' "$id"
"##,
            background = background,
            pid_file = root.join("official-worker-pids").display()
        ),
    )
    .unwrap();
    let mut permissions = std::fs::metadata(&script).unwrap().permissions();
    permissions.set_mode(0o700);
    std::fs::set_permissions(&script, permissions).unwrap();
    script
}

#[cfg(unix)]
fn fake_official_failure(root: &std::path::Path, mode: &str) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let script = root.join(format!("fake-official-{mode}"));
    let (protocol, request_id, package, schema, backend) = match mode {
        "protocol" => ("wrong", "$id", "4.0.0a6", "1.0", "hybrid-http-client"),
        "request" => (
            "mineru-rs-official-worker/1",
            "wrong",
            "4.0.0a6",
            "1.0",
            "hybrid-http-client",
        ),
        "package" => (
            "mineru-rs-official-worker/1",
            "$id",
            "3.4.5",
            "1.0",
            "hybrid-http-client",
        ),
        "schema" => (
            "mineru-rs-official-worker/1",
            "$id",
            "4.0.0a6",
            "0.9",
            "hybrid-http-client",
        ),
        "backend" => (
            "mineru-rs-official-worker/1",
            "$id",
            "4.0.0a6",
            "1.0",
            "hybrid-engine",
        ),
        _ => (
            "mineru-rs-official-worker/1",
            "$id",
            "4.0.0a6",
            "1.0",
            "hybrid-http-client",
        ),
    };
    let body = if mode == "stdout" {
        "dd if=/dev/zero bs=65536 count=2 2>/dev/null".to_owned()
    } else if mode == "stderr" {
        "dd if=/dev/zero bs=65536 count=2 1>&2 2>/dev/null".to_owned()
    } else if mode == "timeout" {
        "sleep 5".to_owned()
    } else if mode == "crash" {
        "exit 7".to_owned()
    } else {
        let response_id = if request_id == "$id" {
            "%s"
        } else {
            request_id
        };
        format!(
            "printf '{{\"protocol\":\"{protocol}\",\"request_id\":\"{response_id}\",\"status\":\"ok\",\"package_version\":\"{package}\",\"schema_version\":\"{schema}\",\"backend\":\"{backend}\",\"bundle_name\":\"hybrid-v4\"}}\\n' \"$id\""
        )
    };
    let script_body = format!(
        r##"#!/bin/sh
request=$(cat)
id=$(printf '%s' "$request" | sed -n 's/.*"request_id":"\([^" ]*\)".*/\1/p')
{body}
"##
    );
    std::fs::write(&script, script_body).unwrap();
    let mut permissions = std::fs::metadata(&script).unwrap().permissions();
    permissions.set_mode(0o700);
    std::fs::set_permissions(&script, permissions).unwrap();
    script
}

#[cfg(unix)]
fn fake_official_shim_python(
    root: &std::path::Path,
    version: &str,
    oversize: bool,
) -> std::path::PathBuf {
    fake_official_shim_python_with_asset(root, version, oversize, None)
}

#[cfg(unix)]
fn fake_official_asset_shim_python(root: &std::path::Path, asset_path: &str) -> std::path::PathBuf {
    fake_official_shim_python_with_asset(root, "4.0.0a6", false, Some(asset_path))
}

#[cfg(unix)]
fn fake_official_shim_python_with_asset(
    root: &std::path::Path,
    version: &str,
    oversize: bool,
    asset_path: Option<&str>,
) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;

    let package_root = root.join("fake-mineru-package");
    let package = package_root.join("mineru");
    let metadata = package_root.join(format!("mineru-{version}.dist-info"));
    std::fs::create_dir_all(&package).unwrap();
    std::fs::create_dir_all(&metadata).unwrap();
    std::fs::write(
        package.join("__init__.py"),
        "print('fake mineru import stdout')\nfrom . import parser\n",
    )
    .unwrap();
    let record = serde_json::to_string(
        &root
            .join("fake-mineru-record.json")
            .to_string_lossy()
            .to_string(),
    )
    .unwrap();
    let payload = if oversize { "200000" } else { "0" };
    let asset_save = asset_path
        .map(|path| {
            let path_literal = serde_json::to_string(path).unwrap();
            let markdown = serde_json::to_string(&format!("![figure]({path})\n")).unwrap();
            let middle = format!(
                r#"{{"schema_version":"1.0","pages":[{{"image_path":{path},{path}:"ordinary-key"}}],"_backend":"hybrid"}}"#,
                path = path_literal
            );
            let middle = serde_json::to_string(&middle).unwrap();
            let content =
                serde_json::to_string(&format!(r#"[{{"img_path":{path}}}]"#, path = path_literal))
                    .unwrap();
            format!(
                "writer.write_string(\"markdown.md\", {markdown})\n        writer.write_string(\"middle_json.json\", {middle})\n        writer.write_string(\"content_list.json\", {content})\n        writer.write(\"structured_content.json\", b'{{}}')\n        writer.write({path}, b'\\xff\\xd8\\x00\\xff\\xd9')",
                markdown = markdown,
                middle = middle,
                content = content,
                path = path_literal,
            )
        })
        .unwrap_or_else(|| {
            "writer.write_string(\"markdown.md\", \"x\" * PAYLOAD if PAYLOAD else \"shim result\\n\")\n        writer.write(\"middle_json.json\", b'{\"schema_version\":\"1.0\",\"pages\":[{}],\"_backend\":\"hybrid\"}')\n        writer.write_string(\"content_list.json\", \"[]\")\n        writer.write(\"structured_content.json\", b'{}')\n        writer.write(\"images/fake.png\", b\"png\")".to_owned()
        });
    let parser = r##"import asyncio
import json
import os

print("fake parser import stdout")
RECORD = __RECORD__
PAYLOAD = __PAYLOAD__

class Result:
    def save(self, writer):
        print("fake result.save stdout")
        __ASSET_SAVE__

async def parse_async(path, **kwargs):
    print("fake parse_async stdout")
    supported = {"backend", "effort", "server_url", "method", "lang", "image_analysis"}
    if kwargs.get("page_range") is not None:
        supported.add("page_range")
    if set(kwargs) != supported:
        raise RuntimeError("unsupported parse_async kwargs: " + repr(sorted(kwargs)))
    with open(RECORD, "w", encoding="utf-8") as output:
        json.dump({"kwargs": kwargs, "env": {name: os.environ.get(name) for name in (
            "MINERU_MODEL_STACK", "MINERU_MODEL_BASE_DIR", "MINERU_CONFIG",
            "MINERU_VL_API_KEY", "MINERU_VL_MODEL_NAME")}}, output, sort_keys=True)
    return Result()
"##
    .replace("__RECORD__", &record)
    .replace("__PAYLOAD__", payload)
    .replace("__ASSET_SAVE__", &asset_save);
    std::fs::write(package.join("parser.py"), parser).unwrap();
    std::fs::write(
        &metadata.join("METADATA"),
        format!("Metadata-Version: 2.1\nName: mineru\nVersion: {version}\n"),
    )
    .unwrap();

    let script = root.join("fake-official-shim-python");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$$\" >> \"{}\"\nexport PYTHONPATH=\"{}\"\nexec python3 \"$@\"\n",
            root.join("persistent-pids").display(),
            package_root.display()
        ),
    )
    .unwrap();
    let mut permissions = std::fs::metadata(&script).unwrap().permissions();
    permissions.set_mode(0o700);
    std::fs::set_permissions(&script, permissions).unwrap();
    script
}

#[cfg(unix)]
fn fake_persistent_python(root: &std::path::Path) -> (std::path::PathBuf, std::path::PathBuf) {
    let python = fake_official_shim_python(root, "4.0.0a6", false);
    let package = root.join("fake-mineru-package/mineru");
    let record = root.join("persistent-record.json");
    let record_literal = serde_json::to_string(&record.to_string_lossy().to_string()).unwrap();
    let init = r##"from pathlib import Path
import json
import sys

print("persistent mineru import stdout")
print("persistent mineru import stderr", file=sys.stderr)
RECORD = __RECORD__
marker = Path(RECORD + ".init")
count = int(marker.read_text() or "0") if marker.exists() else 0
marker.write_text(str(count + 1))
from . import parser
"##
    .replace("__RECORD__", &record_literal);
    std::fs::write(package.join("__init__.py"), init).unwrap();
    let parser = r##"import json
import os
import sys
from pathlib import Path

print("persistent parser import stdout")
print("persistent parser import stderr", file=sys.stderr)
RECORD = __RECORD__
PARSE_MARKER = Path(RECORD + ".parse")
OVERSIZED_FAILURE = Path(RECORD + ".oversized-failure")

class Result:
    def __init__(self, number):
        self.number = number

    def save(self, writer):
        print("persistent result.save stdout")
        print("persistent result.save stderr", file=sys.stderr)
        writer.write_string("markdown.md", "request-%d\\n" % self.number + "x" * 200)
        writer.write("middle_json.json", b'{"schema_version":"1.0","pages":[{}],"_backend":"hybrid"}')
        writer.write_string("content_list.json", "[]")
        writer.write("structured_content.json", b'{}')

async def parse_async(path, **kwargs):
    print("persistent parse_async stdout")
    print("persistent parse_async stderr", file=sys.stderr)
    supported = {"backend", "effort", "server_url", "method", "lang", "image_analysis"}
    if kwargs.get("page_range") is not None:
        supported.add("page_range")
    if set(kwargs) != supported:
        raise RuntimeError("unsupported parse_async kwargs")
    number = int(PARSE_MARKER.read_text() or "0") + 1 if PARSE_MARKER.exists() else 1
    PARSE_MARKER.write_text(str(number))
    if OVERSIZED_FAILURE.exists() and number == 1:
        print("captured parser diagnostic " + "d" * 100000, file=sys.stderr)
        raise RuntimeError("parser error " + "e" * 100000)
    entries = []
    record_path = Path(RECORD)
    if record_path.exists():
        entries = json.loads(record_path.read_text())
    entries.append({"path": path, "kwargs": kwargs, "env": {name: os.environ.get(name) for name in (
        "MINERU_MODEL_STACK", "MINERU_MODEL_BASE_DIR", "MINERU_CONFIG",
        "MINERU_VL_API_KEY", "MINERU_VL_MODEL_NAME")}})
    record_path.write_text(json.dumps(entries, sort_keys=True))
    return Result(number)
"##
        .replace("__RECORD__", &record_literal);
    std::fs::write(package.join("parser.py"), parser).unwrap();
    (python, record)
}

#[cfg(unix)]
fn fake_persistent_oversized_error_python(
    root: &std::path::Path,
) -> (std::path::PathBuf, std::path::PathBuf) {
    let (python, record) = fake_persistent_python(root);
    std::fs::write(format!("{}.oversized-failure", record.display()), b"").unwrap();
    (python, record)
}

#[cfg(unix)]
fn fake_persistent_bad_handshake(root: &std::path::Path) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;

    let script = root.join("fake-persistent-bad-handshake");
    let pids = root.join("persistent-pids");
    std::fs::write(
        &script,
        format!(
            r##"#!/bin/sh
read -r startup || exit 3
printf '%s\n' "$$" >> "{}"
printf '%s\n' '{{"type":"handshake","protocol":"wrong","status":"ready","package_version":"4.0.0a6","schema_version":"1.0","backend":"hybrid-http-client","max_in_flight":1,"capabilities":{{"efforts":["medium","high","xhigh"],"model_stacks":["auto","light","full"],"input_formats":["pdf","png","jpeg","jpg","jp2","webp","gif","bmp","tiff"],"bundle_name":"hybrid-v4","cancellation":"process-terminate"}}}}'
"##,
            pids.display()
        ),
    )
    .unwrap();
    let mut permissions = std::fs::metadata(&script).unwrap().permissions();
    permissions.set_mode(0o700);
    std::fs::set_permissions(&script, permissions).unwrap();
    script
}

#[cfg(unix)]
fn persistent_capabilities() -> Value {
    json!({
        "efforts": ["medium", "high", "xhigh"],
        "model_stacks": ["auto", "light", "full"],
        "input_formats": ["pdf", "png", "jpeg", "jpg", "jp2", "webp", "gif", "bmp", "tiff"],
        "bundle_name": "hybrid-v4",
        "cancellation": "process-terminate",
    })
}

#[cfg(unix)]
fn persistent_start(root: &std::path::Path, capabilities: Value) -> Value {
    json!({
        "type": "start",
        "protocol": "mineru-rs-official-worker/2",
        "package_version": "4.0.0a6",
        "schema_version": "1.0",
        "backend": "hybrid-http-client",
        "model_stack": "full",
        "model_base_dir": root.join("models").to_str().unwrap(),
        "config": root.join("config.toml").to_str().unwrap(),
        "vl_api_key": "persistent-key",
        "vl_model_name": "persistent-model",
        "capabilities": capabilities,
    })
}

#[cfg(unix)]
fn persistent_request(
    root: &std::path::Path,
    request_id: &str,
    sequence: usize,
    effort: &str,
    page_range: Option<&str>,
) -> Value {
    let mut request = json!({
        "type": "request",
        "protocol": "mineru-rs-official-worker/2",
        "request_id": request_id,
        "sequence": sequence,
        "package_version": "4.0.0a6",
        "schema_version": "1.0",
        "backend": "hybrid-http-client",
        "effort": effort,
        "server_url": if effort == "medium" { Value::Null } else { json!("http://model.example/v1") },
        "method": "ocr",
        "lang": "en",
        "image_analysis": false,
        "bundle_name": "hybrid-v4",
        "input_path": root.join(format!("input-{sequence}.pdf")).to_str().unwrap(),
        "bundle_path": root.join(format!("bundle-{sequence}")).to_str().unwrap(),
        "max_bundle_bytes": 1024,
    });
    if let Some(page_range) = page_range {
        request["page_range"] = json!(page_range);
    }
    request
}

#[cfg(unix)]
fn run_persistent(python: &std::path::Path, frames: &[Value]) -> std::process::Output {
    use std::io::Write;

    let shim = std::fs::canonicalize("python/mineru_official_worker.py").unwrap();
    let mut child = Command::new(python)
        .args([shim.to_str().unwrap(), "--persistent"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    for frame in frames {
        if serde_json::to_writer(&mut stdin, frame).is_err() || stdin.write_all(b"\n").is_err() {
            break;
        }
    }
    drop(stdin);
    child.wait_with_output().unwrap()
}

#[cfg(unix)]
fn persistent_stdout_frames(output: &std::process::Output) -> Vec<Value> {
    String::from_utf8(output.stdout.clone())
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
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

fn multipage_pdf(path: &std::path::Path, page_count: usize) {
    use lopdf::{Document, Object, Stream, dictionary};
    let mut pdf = Document::with_version("1.5");
    let pages = pdf.new_object_id();
    let page_ids: Vec<_> = (0..page_count).map(|_| pdf.new_object_id()).collect();
    for id in &page_ids {
        let contents = pdf.add_object(Stream::new(
            dictionary! {},
            b"BT /F1 12 Tf 72 720 Td (x) Tj ET".to_vec(),
        ));
        pdf.objects.insert(*id, Object::Dictionary(dictionary! {
            "Type" => "Page", "Parent" => pages, "MediaBox" => vec![0.into(), 0.into(), 612.into(), 792.into()], "Contents" => contents,
        }));
    }
    pdf.objects.insert(pages, Object::Dictionary(dictionary! { "Type" => "Pages", "Kids" => page_ids.iter().copied().map(Object::Reference).collect::<Vec<_>>(), "Count" => page_count as i64 }));
    let catalog = pdf.add_object(dictionary! { "Type" => "Catalog", "Pages" => pages });
    pdf.trailer.set("Root", catalog);
    pdf.compress();
    pdf.save(path).unwrap();
}

#[cfg(feature = "legacy-office")]
fn native_text_pdf(path: &std::path::Path) {
    use lopdf::{Document, Object, Stream, dictionary};
    let mut pdf = Document::with_version("1.5");
    let pages = pdf.new_object_id();
    let font = pdf.add_object(dictionary! {
        "Type" => "Font", "Subtype" => "Type1", "BaseFont" => "Helvetica",
    });
    let contents = pdf.add_object(Stream::new(
        dictionary! {},
        b"BT /F1 12 Tf 72 720 Td (Native PDF text contains enough clean words for the conservative native assessment.) Tj 0 -20 Td (second line keeps the document readable and long enough for sparse extraction checks.) Tj 0 -20 Td (third line confirms ordinary text operators and stable extraction.) Tj ET".to_vec(),
    ));
    let page = pdf.add_object(dictionary! {
        "Type" => "Page", "Parent" => pages, "MediaBox" => vec![0.into(), 0.into(), 612.into(), 792.into()],
        "Resources" => dictionary! { "Font" => dictionary! { "F1" => font } }, "Contents" => contents,
    });
    pdf.objects.insert(
        pages,
        Object::Dictionary(dictionary! {
            "Type" => "Pages", "Kids" => vec![page.into()], "Count" => 1,
        }),
    );
    let catalog = pdf.add_object(dictionary! { "Type" => "Catalog", "Pages" => pages });
    pdf.trailer.set("Root", catalog);
    pdf.save(path).unwrap();
}

#[cfg(feature = "legacy-office")]
fn low_quality_text_pdf(path: &std::path::Path) {
    use lopdf::{Document, Object, Stream, dictionary};
    let mut pdf = Document::with_version("1.5");
    let pages = pdf.new_object_id();
    let font = pdf.add_object(dictionary! {
        "Type" => "Font", "Subtype" => "Type1", "BaseFont" => "Helvetica",
    });
    let contents = pdf.add_object(Stream::new(
        dictionary! {},
        b"BT /F1 12 Tf 72 720 Td (x) Tj 0 -20 Td (x) Tj 0 -20 Td (x) Tj ET".to_vec(),
    ));
    let page = pdf.add_object(dictionary! {
        "Type" => "Page", "Parent" => pages, "MediaBox" => vec![0.into(), 0.into(), 612.into(), 792.into()],
        "Resources" => dictionary! { "Font" => dictionary! { "F1" => font } }, "Contents" => contents,
    });
    pdf.objects.insert(
        pages,
        Object::Dictionary(dictionary! {
            "Type" => "Pages", "Kids" => vec![page.into()], "Count" => 1,
        }),
    );
    let catalog = pdf.add_object(dictionary! { "Type" => "Catalog", "Pages" => pages });
    pdf.trailer.set("Root", catalog);
    pdf.save(path).unwrap();
}

#[cfg(feature = "legacy-office")]
fn mixed_text_image_pdf(path: &std::path::Path, image_page: usize) {
    use lopdf::{Document, Object, Stream, dictionary};
    let mut pdf = Document::with_version("1.5");
    let pages = pdf.new_object_id();
    let font = pdf.add_object(dictionary! {
        "Type" => "Font", "Subtype" => "Type1", "BaseFont" => "Helvetica",
    });
    let image = pdf.add_object(Stream::new(
        dictionary! {
            "Type" => "XObject", "Subtype" => "Image", "Width" => 100, "Height" => 100,
            "ColorSpace" => "DeviceRGB", "BitsPerComponent" => 8,
        },
        [220, 40, 40].repeat(100 * 100),
    ));
    let mut page_ids = Vec::new();
    for index in 0..10 {
        let contents = if index == image_page {
            "q 612 0 0 792 0 0 cm /Im1 Do Q".to_owned()
        } else {
            format!(
                "BT /F1 12 Tf 72 720 Td (Page {index} contains enough native text for conservative checks.) Tj 0 -20 Td (Second line keeps its text layer reliable for assessment.) Tj 0 -20 Td (Third line is ordinary text.) Tj ET"
            )
        };
        let contents = pdf.add_object(Stream::new(dictionary! {}, contents.into_bytes()));
        let resources = if index == image_page {
            dictionary! {
                "Font" => dictionary! { "F1" => font },
                "XObject" => dictionary! { "Im1" => image },
            }
        } else {
            dictionary! { "Font" => dictionary! { "F1" => font } }
        };
        page_ids.push(pdf.add_object(dictionary! {
            "Type" => "Page", "Parent" => pages,
            "MediaBox" => vec![0.into(), 0.into(), 612.into(), 792.into()],
            "Resources" => resources, "Contents" => contents,
        }));
    }
    pdf.objects.insert(pages, Object::Dictionary(dictionary! {
        "Type" => "Pages", "Kids" => page_ids.into_iter().map(Object::Reference).collect::<Vec<_>>(), "Count" => 10,
    }));
    let catalog = pdf.add_object(dictionary! { "Type" => "Catalog", "Pages" => pages });
    pdf.trailer.set("Root", catalog);
    pdf.save(path).unwrap();
}

#[test]
#[ignore = "CLI process contract e2e"]
fn help_advertises_mixed_inputs_and_the_local_anydoc_lane() {
    let output = mineru().arg("--help").output().unwrap();
    assert!(output.status.success());
    let help = String::from_utf8(output.stdout).unwrap();
    assert!(help.contains("AnyDoc-supported inputs"));
    assert!(help.contains("PDF, image, and Office"));
    assert!(help.contains("--start") && help.contains("--end"));
    assert!(help.contains("--backend") && help.contains("VLM-HTTP"));
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
            "--model-stack <MODEL_STACK>",
            "--official-worker-mode <OFFICIAL_WORKER_MODE>",
            "--official-python <OFFICIAL_PYTHON>",
            "--official-model-dir <OFFICIAL_MODEL_DIR>",
            "--official-config <OFFICIAL_CONFIG>",
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
            "--concurrency-model <CONCURRENCY_MODEL>",
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
            "--temperature-retry[=<true|false>]",
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
        "[possible values: vlm-http-client, hybrid-http-client, local]",
        "[possible values: medium, high, xhigh]",
        "[possible values: per-document, persistent]",
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
            && !help
                .lines()
                .any(|line| line.trim_start().starts_with("--model "))
    );
}

#[test]
fn cli_accepts_local_backend_as_a_distinct_choice() {
    let matches = mineru::command::cli_command()
        .try_get_matches_from([
            "mineru",
            "-p",
            "input.doc",
            "-o",
            "out",
            "--backend",
            "local",
        ])
        .unwrap();
    assert_eq!(
        matches.get_one::<String>("backend").map(String::as_str),
        Some("local")
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
    assert!(help.contains("MINERU_OFFICIAL_WORKER_MODE"));
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
    multipage_pdf(&pdf, 2);
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

#[cfg(feature = "legacy-office")]
#[tokio::test]
async fn non_local_legacy_converts_to_pdf_and_uses_the_vlm_route() {
    let input = tempfile::tempdir().unwrap();
    let output = tempfile::tempdir().unwrap();
    std::fs::write(input.path().join("old.doc"), legacy_fixtures::doc()).unwrap();
    let (url, seen) = mock().await;
    let mut cmd = mineru();
    cmd.args(["-p"])
        .arg(input.path().join("old.doc"))
        .args(["-o"])
        .arg(output.path())
        .args(["--url", &url]);
    let result = command(cmd).await;
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(output.path().join("old/vlm/old.md").is_file());
    assert!(output.path().join("old/vlm/old_origin.doc").is_file());
    let stderr = String::from_utf8_lossy(&result.stderr);
    assert!(stderr.contains("text-only") && stderr.contains("non-ASCII"));
    assert!(stderr.contains("Microsoft Office or LibreOffice"));
    assert_eq!(
        seen.0
            .lock()
            .unwrap()
            .iter()
            .filter(|(kind, _, _)| kind == "completion")
            .count(),
        1
    );
}

#[cfg(feature = "legacy-office")]
#[tokio::test]
async fn multiple_legacy_warnings_keep_document_scope_and_one_recommendation() {
    let input = tempfile::tempdir().unwrap();
    let output = tempfile::tempdir().unwrap();
    std::fs::write(input.path().join("old.doc"), legacy_fixtures::doc()).unwrap();
    std::fs::write(input.path().join("notes.rtf"), legacy_fixtures::rtf()).unwrap();
    let (url, seen) = mock().await;
    let mut cmd = mineru();
    cmd.args(["-p"])
        .arg(input.path())
        .args(["-o"])
        .arg(output.path())
        .args(["--url", &url]);
    let result = command(cmd).await;
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let stderr = String::from_utf8_lossy(&result.stderr);
    assert_eq!(
        stderr.matches("text-only best-effort PDF fallback").count(),
        2
    );
    assert_eq!(
        stderr
            .matches("non-ASCII characters may be replaced with '?'")
            .count(),
        2
    );
    assert_eq!(stderr.matches("Microsoft Office or LibreOffice").count(), 1);
    assert!(output.path().join("old/vlm/old.md").is_file());
    assert!(output.path().join("notes/vlm/notes.md").is_file());
    assert_eq!(
        seen.0
            .lock()
            .unwrap()
            .iter()
            .filter(|(kind, _, _)| kind == "completion")
            .count(),
        2
    );
}

#[cfg(feature = "legacy-office")]
#[tokio::test]
async fn legacy_conversion_failure_is_reported_before_vlm_connection() {
    let input = tempfile::tempdir().unwrap();
    let output = tempfile::tempdir().unwrap();
    // The extension is recognized, but the bytes cannot be a DOC. A dead URL proves that the
    // helper conversion is attempted before the VLM client can make a request.
    std::fs::write(input.path().join("broken.doc"), b"not a legacy document").unwrap();
    let mut cmd = mineru();
    cmd.args(["-p"])
        .arg(input.path().join("broken.doc"))
        .args(["-o"])
        .arg(output.path())
        .args(["--url", "http://127.0.0.1:1"]);
    let result = command(cmd).await;
    assert!(!result.status.success());
    let stderr = String::from_utf8_lossy(&result.stderr);
    assert!(
        stderr.contains("legacy best-effort PDF conversion failed"),
        "{stderr}"
    );
    assert!(
        stderr.contains("text-only") && stderr.contains("non-ASCII"),
        "{stderr}"
    );
    assert_eq!(
        stderr.matches("Microsoft Office or LibreOffice").count(),
        1,
        "{stderr}"
    );
    assert!(!stderr.contains("request connection failed"), "{stderr}");
    assert!(!output.path().join("broken").exists());
}

#[cfg(feature = "legacy-office")]
#[tokio::test]
async fn local_backend_extracts_legacy_markdown_without_a_vlm_request() {
    let input = tempfile::tempdir().unwrap();
    let output = tempfile::tempdir().unwrap();
    std::fs::write(input.path().join("old.doc"), legacy_fixtures::doc()).unwrap();
    let mut cmd = mineru();
    cmd.args(["-p"])
        .arg(input.path().join("old.doc"))
        .args(["-o"])
        .arg(output.path())
        .args(["--backend", "local", "--url", "not a URL"]);
    let result = command(cmd).await;
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let markdown = std::fs::read_to_string(output.path().join("old/office/old.md")).unwrap();
    assert!(markdown.contains("Legacy DOC fixture"));
    assert!(!String::from_utf8_lossy(&result.stderr).contains("request connection failed"));
}

#[cfg(feature = "legacy-office")]
#[tokio::test]
async fn local_backend_ignores_invalid_vlm_environment() {
    let input = tempfile::tempdir().unwrap();
    let output = tempfile::tempdir().unwrap();
    std::fs::write(input.path().join("old.doc"), legacy_fixtures::doc()).unwrap();
    let mut cmd = mineru();
    cmd.args(["-p"])
        .arg(input.path().join("old.doc"))
        .args(["-o"])
        .arg(output.path())
        .args(["--backend", "local"])
        .env("MINERU_VL_SERVER", "not a URL")
        .env("MINERU_VL_API_KEY", "invalid\nkey");
    let result = command(cmd).await;
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(output.path().join("old/office/old.md").is_file());
}

#[cfg(feature = "legacy-office")]
#[tokio::test]
async fn local_backend_ignores_invalid_vlm_transport_environment() {
    let input = tempfile::tempdir().unwrap();
    std::fs::write(input.path().join("old.doc"), legacy_fixtures::doc()).unwrap();
    for (index, (name, value)) in [
        ("MINERU_VLM_HTTP_TIMEOUT", "not-a-duration"),
        ("MINERU_VLM_ALLOW_REMOTE_IMAGES", "maybe"),
    ]
    .into_iter()
    .enumerate()
    {
        let output = input.path().join(format!("out-{index}"));
        let mut cmd = mineru();
        cmd.args(["-p"])
            .arg(input.path().join("old.doc"))
            .args(["-o"])
            .arg(&output)
            .args(["--backend", "local"])
            .env(name, value);
        let result = command(cmd).await;
        assert!(
            result.status.success(),
            "{name}: {}",
            String::from_utf8_lossy(&result.stderr)
        );
        assert!(output.join("old/office/old.md").is_file());
    }
}

#[cfg(feature = "legacy-office")]
#[tokio::test]
async fn non_local_still_rejects_invalid_vlm_transport_environment() {
    let input = tempfile::tempdir().unwrap();
    std::fs::write(input.path().join("old.doc"), legacy_fixtures::doc()).unwrap();
    for (index, (name, value)) in [
        ("MINERU_VLM_HTTP_TIMEOUT", "not-a-duration"),
        ("MINERU_VLM_ALLOW_REMOTE_IMAGES", "maybe"),
    ]
    .into_iter()
    .enumerate()
    {
        let output = input.path().join(format!("out-{index}"));
        let mut cmd = mineru();
        cmd.args(["-p"])
            .arg(input.path().join("old.doc"))
            .args(["-o"])
            .arg(&output)
            .env(name, value);
        let result = command(cmd).await;
        assert!(!result.status.success(), "{name}");
        assert!(
            String::from_utf8_lossy(&result.stderr).contains(name),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        assert!(!output.exists());
    }
}

#[cfg(feature = "legacy-office")]
#[tokio::test]
async fn local_backend_rejects_helper_only_limits_from_flag_or_environment() {
    let input = tempfile::tempdir().unwrap();
    std::fs::write(input.path().join("old.doc"), legacy_fixtures::doc()).unwrap();
    for (index, env_name) in [(0, None), (1, Some("MINERU_OFFICE_CPU_SECONDS"))] {
        let output = input.path().join(format!("out-{index}"));
        let mut cmd = mineru();
        cmd.args(["-p"])
            .arg(input.path().join("old.doc"))
            .args(["-o"])
            .arg(&output)
            .args(["--backend", "local"]);
        if let Some(name) = env_name {
            cmd.env(name, "1");
        } else {
            cmd.args(["--office-wall-seconds", "1"]);
        }
        let result = command(cmd).await;
        assert!(!result.status.success(), "case {index}");
        let stderr = String::from_utf8_lossy(&result.stderr);
        assert!(
            stderr.contains("backend=local") && stderr.contains("helper-only"),
            "{stderr}"
        );
        assert!(!output.join("old").exists());
    }
}

#[cfg(feature = "legacy-office")]
#[tokio::test]
async fn non_local_legacy_still_validates_invalid_url() {
    let input = tempfile::tempdir().unwrap();
    let output = tempfile::tempdir().unwrap();
    std::fs::write(input.path().join("old.doc"), legacy_fixtures::doc()).unwrap();
    let mut cmd = mineru();
    cmd.args(["-p"])
        .arg(input.path().join("old.doc"))
        .args(["-o"])
        .arg(output.path())
        .args(["--url", "not a URL"]);
    let result = command(cmd).await;
    assert!(!result.status.success());
    assert!(
        String::from_utf8_lossy(&result.stderr).contains("best-effort"),
        "conversion warning was not emitted before VLM failure: {}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(!output.path().join("old").exists());
}

#[cfg(feature = "legacy-office")]
#[tokio::test]
async fn local_backend_uses_bounded_helper_for_legacy_conversion_without_vlm() {
    let input = tempfile::tempdir().unwrap();
    let output = tempfile::tempdir().unwrap();
    std::fs::write(input.path().join("old.doc"), legacy_fixtures::doc()).unwrap();
    let mut options = mineru::RunOptions::new(input.path().join("old.doc"), output.path());
    options.backend = "local".into();
    options.url = Some("http://127.0.0.1:1".into());
    let context =
        mineru::command::RunContext::with_office_executable(bundled_office_helper()).unwrap();
    let result = mineru::command::run_with_context(options, context).await;
    assert!(result.is_ok(), "{result:?}");
    assert!(output.path().join("old/office/old.md").is_file());
}

#[cfg(feature = "legacy-office")]
#[tokio::test]
async fn local_backend_extracts_clean_pdf_native_markdown_through_bounded_helper_without_vlm() {
    let input = tempfile::tempdir().unwrap();
    let output = tempfile::tempdir().unwrap();
    let pdf = input.path().join("document.pdf");
    native_text_pdf(&pdf);
    let options = mineru::RunOptions {
        path: pdf,
        output: output.path().to_owned(),
        backend: "local".into(),
        url: Some("not a URL".into()),
        ..mineru::RunOptions::new("unused", "unused")
    };
    let context =
        mineru::command::RunContext::with_office_executable(bundled_office_helper()).unwrap();
    let result = mineru::command::run_with_context(options, context).await;
    assert!(result.is_ok(), "{result:?}");
    let native = output.path().join("document/native/document.md");
    assert!(
        std::fs::read_to_string(native)
            .unwrap()
            .contains("Native PDF text")
    );
    assert!(!output.path().join("document/vlm").exists());
    assert!(!output.path().join("document/document.json").exists());
}

#[cfg(feature = "legacy-office")]
#[tokio::test]
async fn local_backend_rejects_low_quality_pdf_without_vlm_fallback() {
    let input = tempfile::tempdir().unwrap();
    let output = tempfile::tempdir().unwrap();
    let pdf = input.path().join("sparse.pdf");
    low_quality_text_pdf(&pdf);
    let options = mineru::RunOptions {
        path: pdf,
        output: output.path().to_owned(),
        backend: "local".into(),
        url: Some("http://127.0.0.1:1".into()),
        ..mineru::RunOptions::new("unused", "unused")
    };
    let result = mineru::command::run_with_context(
        options,
        mineru::command::RunContext::with_office_executable(bundled_office_helper()).unwrap(),
    )
    .await;
    let error = result.unwrap_err().to_string();
    assert!(error.contains("native PDF Markdown unavailable"), "{error}");
    assert!(
        error.contains("low_confidence") || error.contains("ocr_required"),
        "{error}"
    );
    assert!(!error.contains("request connection failed"));
    assert!(!output.path().join("sparse").exists());
}

#[cfg(feature = "legacy-office")]
#[tokio::test]
async fn local_backend_rejects_ten_page_mixed_text_image_pdf() {
    let input = tempfile::tempdir().unwrap();
    let output = tempfile::tempdir().unwrap();
    let pdf = input.path().join("mixed.pdf");
    mixed_text_image_pdf(&pdf, 7);
    let options = mineru::RunOptions {
        path: pdf,
        output: output.path().to_owned(),
        backend: "local".into(),
        ..mineru::RunOptions::new("unused", "unused")
    };
    let error = mineru::command::run_with_context(
        options,
        mineru::command::RunContext::with_office_executable(bundled_office_helper()).unwrap(),
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(error.contains("mixed_pdf"), "{error}");
    assert!(error.contains("images_present"), "{error}");
    assert!(!output.path().join("mixed").exists());
}

#[cfg(not(feature = "legacy-office"))]
#[tokio::test]
async fn local_backend_rejects_pdf_without_native_feature() {
    let input = tempfile::tempdir().unwrap();
    let output = tempfile::tempdir().unwrap();
    let pdf = input.path().join("document.pdf");
    std::fs::copy("tests/fixtures/pdf/minimal.pdf", &pdf).unwrap();
    let mut cmd = mineru();
    cmd.args(["-p"])
        .arg(&pdf)
        .args(["-o"])
        .arg(output.path())
        .args(["--backend", "local", "--url", "http://127.0.0.1:1"]);
    let result = command(cmd).await;
    assert!(!result.status.success());
    let stderr = String::from_utf8_lossy(&result.stderr);
    assert!(stderr.contains("native PDF Markdown requires the legacy-office feature"));
    assert!(!stderr.contains("request connection failed"));
    assert!(!output.path().join("document").exists());
}

#[cfg(feature = "legacy-office")]
#[tokio::test]
async fn all_legacy_batch_uses_the_vlm_route_after_conversion() {
    let input = tempfile::tempdir().unwrap();
    let output = tempfile::tempdir().unwrap();
    std::fs::write(input.path().join("old.doc"), legacy_fixtures::doc()).unwrap();
    let (url, seen) = mock().await;
    let mut cmd = mineru();
    cmd.args(["-p"])
        .arg(input.path().join("old.doc"))
        .args(["-o"])
        .arg(output.path())
        .args(["--url", &url]);
    let result = command(cmd).await;
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(output.path().join("old/vlm/old.md").is_file());
    assert_eq!(
        seen.0
            .lock()
            .unwrap()
            .iter()
            .filter(|(kind, _, _)| kind == "completion")
            .count(),
        1
    );
}

#[cfg(all(feature = "office", feature = "legacy-office"))]
#[tokio::test]
async fn legacy_batch_with_doomed_ooxml_candidate_still_reaches_vlm() {
    // A preflight-doomed OOXML candidate must not prevent the surviving legacy document from
    // reaching the VLM route after its best-effort PDF conversion.
    let input = tempfile::tempdir().unwrap();
    let output = tempfile::tempdir().unwrap();
    std::fs::write(input.path().join("old.doc"), legacy_fixtures::doc()).unwrap();
    std::fs::write(input.path().join("huge.docx"), vec![0u8; 64 * 1024]).unwrap();
    let (url, seen) = mock().await;
    let mut cmd = mineru();
    cmd.args(["-p"])
        .arg(input.path())
        .args(["-o"])
        .arg(output.path())
        .args(["--url", &url])
        .env("MINERU_OFFICE_INPUT_BYTES", "32768");
    let result = command(cmd).await;
    let stderr = String::from_utf8_lossy(&result.stderr);
    // The doomed .docx is announced in the preflight (the batch fails as a whole), but the
    // legacy document must still reach the VLM route.
    assert!(
        stderr.contains("exceeds office conversion input limit"),
        "{stderr}"
    );
    assert!(
        !stderr.contains("legacy best-effort PDF conversion failed"),
        "legacy conversion failed: {stderr}"
    );
    assert!(
        output.path().join("old/vlm/old.md").is_file(),
        "legacy document did not convert: {stderr}"
    );
    assert_eq!(
        seen.0
            .lock()
            .unwrap()
            .iter()
            .filter(|(kind, _, _)| kind == "completion")
            .count(),
        1
    );
}

#[cfg(feature = "legacy-office")]
#[tokio::test]
async fn legacy_office_directory_input_extracts_every_format() {
    let input = tempfile::tempdir().unwrap();
    let output = tempfile::tempdir().unwrap();
    let fixtures = legacy_fixtures::all();
    for (index, fixture) in fixtures.iter().enumerate() {
        std::fs::write(
            input.path().join(format!("doc{index}.{}", fixture.kind)),
            &fixture.bytes,
        )
        .unwrap();
    }
    std::fs::write(input.path().join("skip.txt"), b"skip").unwrap();
    let mut cmd = mineru();
    cmd.args(["-p"])
        .arg(input.path())
        .args(["-o"])
        .arg(output.path())
        .args(["--backend", "local"]);
    let result = command(cmd).await;
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    for (index, fixture) in fixtures.iter().enumerate() {
        let stem = format!("doc{index}");
        let markdown = std::fs::read_to_string(
            output
                .path()
                .join(&stem)
                .join("office")
                .join(format!("{stem}.md")),
        )
        .unwrap_or_else(|_| panic!("missing output for {}", fixture.kind));
        assert!(
            !markdown.contains('\u{fffd}'),
            "{}: replacement character",
            fixture.kind
        );
        for expected in fixture.expected {
            assert!(
                markdown.contains(expected),
                "{}: missing {expected:?}",
                fixture.kind
            );
        }
    }
    assert!(String::from_utf8_lossy(&result.stderr).contains("skip.txt"));
}

#[cfg(all(feature = "office", feature = "legacy-office"))]
#[tokio::test]
async fn mixed_legacy_and_ooxml_batch_handles_both_kinds_in_order() {
    let input = tempfile::tempdir().unwrap();
    let output = tempfile::tempdir().unwrap();
    std::fs::write(input.path().join("old.doc"), legacy_fixtures::doc()).unwrap();
    std::fs::write(input.path().join("word.docx"), office_fixtures::docx()).unwrap();
    let (url, seen) = mock().await;
    let mut cmd = mineru();
    cmd.args(["-p"])
        .arg(input.path())
        .args(["-o"])
        .arg(output.path())
        .args(["--url", &url]);
    let result = command(cmd).await;
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    // The legacy doc takes the best-effort PDF lane and is parsed by the VLM too.
    let _markdown = std::fs::read_to_string(output.path().join("old/vlm/old.md")).unwrap();
    assert!(output.path().join("old/vlm/old_origin.doc").is_file());
    assert_eq!(
        seen.0
            .lock()
            .unwrap()
            .iter()
            .filter(|(kind, _, _)| kind == "completion")
            .count(),
        2
    );
    // The OOXML doc still takes the PDF -> VLM lane with its origin preserved.
    assert!(output.path().join("word/office/word_origin.docx").is_file());
    assert!(output.path().join("word/office/word.md").is_file());
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
    assert!(seen.0.lock().unwrap().is_empty());
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
async fn default_vlm_client_keeps_pdf_on_vlm_route() {
    let dir = tempfile::tempdir().unwrap();
    let pdf = input(&dir);
    let output = dir.path().join("out");
    let (url, seen) = mock().await;
    let mut cmd = mineru();
    cmd.args(["-p"])
        .arg(pdf)
        .args(["-o"])
        .arg(&output)
        .args(["--url", &url])
        .env("MINERU_OFFICIAL_WORKER_MODE", "invalid");
    let result = command(cmd).await;
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(
        seen.0
            .lock()
            .unwrap()
            .iter()
            .any(|(kind, _, _)| kind == "completion")
    );
    assert!(output.join("document/vlm/document.md").is_file());
    assert!(!output.join("document/native").exists());
}

#[tokio::test]
async fn direct_mode_hybrid_backend_uses_the_official_boundary() {
    let dir = tempfile::tempdir().unwrap();
    let pdf = input(&dir);
    let output = dir.path().join("out");
    let mut cmd = mineru();
    let python = std::env::current_exe().unwrap();
    cmd.args(["-p"])
        .arg(pdf)
        .args(["-o"])
        .arg(&output)
        .args(["--backend", "hybrid-http-client", "--official-python"])
        .arg(python);
    let result = command(cmd).await;
    assert!(!result.status.success());
    assert_eq!(result.stdout, b"");
    let stderr = unstamped(&result.stderr).join("\n");
    assert!(!stderr.contains("local-model worker is integrated"));
    assert!(
        stderr.contains("official worker") || stderr.contains("official Python"),
        "{stderr}"
    );
    assert!(!output.exists());
}

#[cfg(unix)]
#[tokio::test]
async fn direct_hybrid_medium_propagates_options_without_a_url() {
    let dir = tempfile::tempdir().unwrap();
    let pdf = input(&dir);
    multipage_pdf(&pdf, 5);
    let output = dir.path().join("out");
    let python = fake_official_python(dir.path());
    let mut cmd = mineru();
    cmd.args(["-p"])
        .arg(pdf)
        .args(["-o"])
        .arg(&output)
        .args([
            "--backend",
            "hybrid-http-client",
            "--effort",
            "medium",
            "--method",
            "ocr",
            "--lang",
            "en",
            "--image-analysis",
            "false",
            "--model-stack",
            "light",
            "--start",
            "0",
            "--end",
            "4",
            "--official-python",
        ])
        .arg(python);
    let result = command(cmd).await;
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(
        std::fs::read_to_string(output.join("document/hybrid-v4/markdown.md")).unwrap(),
        "effort=medium page=1~5\n"
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("official-worker-pids"))
            .unwrap()
            .lines()
            .count(),
        1
    );
}

#[cfg(unix)]
#[tokio::test]
async fn direct_hybrid_persistent_mode_reuses_one_worker_for_two_documents() {
    let dir = tempfile::tempdir().unwrap();
    let inputs = dir.path().join("inputs");
    std::fs::create_dir(&inputs).unwrap();
    for stem in ["first", "second"] {
        std::fs::copy(
            "tests/fixtures/pdf/minimal.pdf",
            inputs.join(format!("{stem}.pdf")),
        )
        .unwrap();
    }
    let output = dir.path().join("out");
    let (python, record) = fake_persistent_python(dir.path());
    let model_dir = dir.path().join("models");
    let config = dir.path().join("mineru.toml");
    let mut cmd = mineru();
    cmd.args(["-p"])
        .arg(&inputs)
        .args(["-o", output.to_str().unwrap()])
        .args([
            "--backend",
            "hybrid-http-client",
            "--official-worker-mode",
            "persistent",
            "--effort",
            "medium",
            "--method",
            "ocr",
            "--lang",
            "en",
            "--image-analysis",
            "false",
            "--model-stack",
            "full",
            "--official-python",
        ])
        .arg(&python)
        .args(["--official-model-dir", model_dir.to_str().unwrap()])
        .args(["--official-config", config.to_str().unwrap()])
        .env("MINERU_MODEL_STACK", "light")
        .env("MINERU_VL_API_KEY", "persistent-key")
        .env("MINERU_VL_MODEL_NAME", "persistent-model");
    let result = command(cmd).await;
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );

    let entries: Vec<Value> = serde_json::from_slice(&std::fs::read(&record).unwrap()).unwrap();
    assert_eq!(entries.len(), 2);
    for entry in &entries {
        assert_eq!(entry["kwargs"]["backend"], "hybrid-http-client");
        assert_eq!(entry["kwargs"]["effort"], "medium");
        assert!(entry["kwargs"]["server_url"].is_null());
        assert_eq!(entry["kwargs"]["method"], "ocr");
        assert_eq!(entry["kwargs"]["lang"], "en");
        assert_eq!(entry["kwargs"]["image_analysis"], false);
        assert_eq!(entry["env"]["MINERU_MODEL_STACK"], "full");
        assert_eq!(
            entry["env"]["MINERU_MODEL_BASE_DIR"],
            model_dir.to_str().unwrap()
        );
        assert_eq!(entry["env"]["MINERU_CONFIG"], config.to_str().unwrap());
        assert_eq!(entry["env"]["MINERU_VL_API_KEY"], "persistent-key");
        assert_eq!(entry["env"]["MINERU_VL_MODEL_NAME"], "persistent-model");
    }
    assert_ne!(entries[0]["path"], entries[1]["path"]);
    assert_eq!(
        std::fs::read_to_string(format!("{}.init", record.display())).unwrap(),
        "1"
    );
    assert_eq!(
        std::fs::read_to_string(format!("{}.parse", record.display())).unwrap(),
        "2"
    );
    let pids = std::fs::read_to_string(dir.path().join("persistent-pids")).unwrap();
    assert_eq!(pids.lines().collect::<Vec<_>>().len(), 1);
    for stem in ["first", "second"] {
        let markdown = output.join(format!("{stem}/hybrid-v4/markdown.md"));
        assert!(markdown.is_file(), "missing {}", markdown.display());
        assert!(
            std::fs::read_to_string(markdown)
                .unwrap()
                .contains("request-")
        );
    }
}

#[cfg(unix)]
#[tokio::test]
async fn direct_hybrid_automatic_mode_reuses_one_worker_for_two_documents() {
    let dir = tempfile::tempdir().unwrap();
    let inputs = dir.path().join("inputs");
    std::fs::create_dir(&inputs).unwrap();
    for stem in ["first", "second"] {
        std::fs::copy(
            "tests/fixtures/pdf/minimal.pdf",
            inputs.join(format!("{stem}.pdf")),
        )
        .unwrap();
    }
    let output = dir.path().join("out");
    let (python, record) = fake_persistent_python(dir.path());
    let mut cmd = mineru();
    cmd.args(["-p"])
        .arg(&inputs)
        .args(["-o", output.to_str().unwrap()])
        .args(["--backend", "hybrid-http-client", "--official-python"])
        .arg(python);
    let result = command(cmd).await;
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let entries: Vec<Value> = serde_json::from_slice(&std::fs::read(record).unwrap()).unwrap();
    assert_eq!(entries.len(), 2);
    assert_eq!(
        std::fs::read_to_string(dir.path().join("persistent-pids"))
            .unwrap()
            .lines()
            .count(),
        1
    );
    for stem in ["first", "second"] {
        assert!(
            output
                .join(format!("{stem}/hybrid-v4/markdown.md"))
                .is_file()
        );
    }
}

#[cfg(unix)]
#[tokio::test]
async fn direct_hybrid_automatic_mode_counts_only_runnable_documents() {
    use std::io::Write;

    let dir = tempfile::tempdir().unwrap();
    let inputs = dir.path().join("inputs");
    std::fs::create_dir(&inputs).unwrap();
    let runnable = inputs.join("runnable.pdf");
    std::fs::copy("tests/fixtures/pdf/minimal.pdf", &runnable).unwrap();
    let doomed = inputs.join("doomed.pdf");
    std::fs::copy(&runnable, &doomed).unwrap();
    std::fs::OpenOptions::new()
        .append(true)
        .open(&doomed)
        .unwrap()
        .write_all(b"x")
        .unwrap();
    let limit = std::fs::metadata(&runnable).unwrap().len().to_string();
    let output = dir.path().join("out");
    let python = fake_official_python(dir.path());
    let mut cmd = mineru();
    cmd.args([
        "-p",
        inputs.to_str().unwrap(),
        "-o",
        output.to_str().unwrap(),
    ])
    .args(["--backend", "hybrid-http-client", "--official-python"])
    .arg(python)
    .args(["--max-input-bytes", limit.as_str()]);
    let result = command(cmd).await;
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("max-input-bytes"));
    assert_eq!(
        std::fs::read_to_string(dir.path().join("official-worker-pids"))
            .unwrap()
            .lines()
            .count(),
        1
    );
    assert!(output.join("runnable/hybrid-v4/markdown.md").is_file());
    assert!(!output.join("doomed").exists());
}

#[cfg(unix)]
#[tokio::test]
async fn direct_hybrid_automatic_mode_preflights_semantic_rejections() {
    let dir = tempfile::tempdir().unwrap();
    let inputs = dir.path().join("inputs");
    std::fs::create_dir(&inputs).unwrap();
    multipage_pdf(&inputs.join("valid.pdf"), 2);
    multipage_pdf(&inputs.join("rejected.pdf"), 1);
    let output = dir.path().join("out");
    let python = fake_official_python(dir.path());
    let mut cmd = mineru();
    cmd.args([
        "-p",
        inputs.to_str().unwrap(),
        "-o",
        output.to_str().unwrap(),
    ])
    .args([
        "--backend",
        "hybrid-http-client",
        "--start",
        "0",
        "--end",
        "1",
        "--official-python",
    ])
    .arg(python);
    let result = command(cmd).await;
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("outside PDF"));
    assert_eq!(
        std::fs::read_to_string(dir.path().join("official-worker-pids"))
            .unwrap()
            .lines()
            .count(),
        1
    );
    assert!(!dir.path().join("persistent-pids").exists());
    assert!(output.join("valid/hybrid-v4/markdown.md").is_file());
    assert!(!output.join("rejected").exists());
}

#[cfg(unix)]
#[tokio::test]
async fn official_worker_mode_precedence_and_invalid_values_fail_closed() {
    let env_dir = tempfile::tempdir().unwrap();
    let env_inputs = env_dir.path().join("inputs");
    std::fs::create_dir(&env_inputs).unwrap();
    for stem in ["first", "second"] {
        std::fs::copy(
            "tests/fixtures/pdf/minimal.pdf",
            env_inputs.join(format!("{stem}.pdf")),
        )
        .unwrap();
    }
    let env_output = env_dir.path().join("out");
    let (env_python, _) = fake_persistent_python(env_dir.path());
    let mut env_cmd = mineru();
    env_cmd
        .args(["-p"])
        .arg(&env_inputs)
        .args(["-o", env_output.to_str().unwrap()])
        .args(["--backend", "hybrid-http-client", "--official-python"])
        .arg(&env_python)
        .env("MINERU_OFFICIAL_WORKER_MODE", "persistent");
    let env_result = command(env_cmd).await;
    assert!(
        env_result.status.success(),
        "{}",
        String::from_utf8_lossy(&env_result.stderr)
    );
    assert_eq!(
        std::fs::read_to_string(env_dir.path().join("persistent-pids"))
            .unwrap()
            .lines()
            .count(),
        1
    );

    let override_dir = tempfile::tempdir().unwrap();
    let override_inputs = override_dir.path().join("inputs");
    std::fs::create_dir(&override_inputs).unwrap();
    for stem in ["first", "second"] {
        std::fs::copy(
            "tests/fixtures/pdf/minimal.pdf",
            override_inputs.join(format!("{stem}.pdf")),
        )
        .unwrap();
    }
    let override_output = override_dir.path().join("out");
    let override_python = fake_official_python(override_dir.path());
    let mut override_cmd = mineru();
    override_cmd
        .args(["-p"])
        .arg(&override_inputs)
        .args(["-o", override_output.to_str().unwrap()])
        .args([
            "--backend",
            "hybrid-http-client",
            "--official-worker-mode",
            "per-document",
            "--official-python",
        ])
        .arg(&override_python)
        .env("MINERU_OFFICIAL_WORKER_MODE", "persistent");
    let override_result = command(override_cmd).await;
    assert!(
        override_result.status.success(),
        "{}",
        String::from_utf8_lossy(&override_result.stderr)
    );
    assert_eq!(
        std::fs::read_to_string(override_dir.path().join("official-worker-pids"))
            .unwrap()
            .lines()
            .count(),
        2
    );

    let invalid_env_dir = tempfile::tempdir().unwrap();
    let invalid_env_pdf = input(&invalid_env_dir);
    let invalid_env_output = invalid_env_dir.path().join("out");
    let invalid_env_python = fake_official_python(invalid_env_dir.path());
    let mut invalid_env_cmd = mineru();
    invalid_env_cmd
        .args(["-p"])
        .arg(invalid_env_pdf)
        .args(["-o", invalid_env_output.to_str().unwrap()])
        .args(["--backend", "hybrid-http-client", "--official-python"])
        .arg(invalid_env_python)
        .env("MINERU_OFFICIAL_WORKER_MODE", "unexpected");
    let invalid_env_result = command(invalid_env_cmd).await;
    assert!(!invalid_env_result.status.success());
    assert!(
        String::from_utf8_lossy(&invalid_env_result.stderr).contains("MINERU_OFFICIAL_WORKER_MODE")
    );
    assert!(!invalid_env_dir.path().join("official-worker-pids").exists());
    assert!(!invalid_env_output.exists());

    let invalid_cli_dir = tempfile::tempdir().unwrap();
    let invalid_cli_pdf = input(&invalid_cli_dir);
    let invalid_cli_output = invalid_cli_dir.path().join("out");
    let mut invalid_cli_cmd = mineru();
    invalid_cli_cmd.args(["-p"]).arg(invalid_cli_pdf).args([
        "-o",
        invalid_cli_output.to_str().unwrap(),
        "--backend",
        "hybrid-http-client",
        "--official-worker-mode",
        "unexpected",
    ]);
    let invalid_cli_result = command(invalid_cli_cmd).await;
    assert!(!invalid_cli_result.status.success());
    assert!(
        String::from_utf8_lossy(&invalid_cli_result.stderr).contains("invalid value 'unexpected'")
    );
    assert!(!invalid_cli_output.exists());
}

#[cfg(unix)]
#[tokio::test]
async fn persistent_cli_handshake_failure_never_falls_back_to_one_shot() {
    let dir = tempfile::tempdir().unwrap();
    let inputs = dir.path().join("inputs");
    std::fs::create_dir(&inputs).unwrap();
    for stem in ["first", "second"] {
        std::fs::copy(
            "tests/fixtures/pdf/minimal.pdf",
            inputs.join(format!("{stem}.pdf")),
        )
        .unwrap();
    }
    let output = dir.path().join("out");
    let python = fake_persistent_bad_handshake(dir.path());
    let mut cmd = mineru();
    cmd.args(["-p"])
        .arg(&inputs)
        .args([
            "-o",
            output.to_str().unwrap(),
            "--backend",
            "hybrid-http-client",
            "--official-worker-mode",
            "persistent",
            "--official-python",
        ])
        .arg(python);
    let result = command(cmd).await;
    assert!(!result.status.success());
    let stderr = String::from_utf8_lossy(&result.stderr);
    assert!(
        stderr.contains("official persistent handshake mismatch"),
        "{stderr}"
    );
    assert!(!stderr.contains("official worker protocol version mismatch"));
    assert!(!output.exists());
    assert_eq!(
        std::fs::read_to_string(dir.path().join("persistent-pids"))
            .unwrap()
            .lines()
            .count(),
        2
    );
}

#[cfg(unix)]
#[tokio::test]
async fn official_shim_propagates_exact_kwargs_and_environment() {
    for effort in ["high", "xhigh"] {
        let dir = tempfile::tempdir().unwrap();
        let pdf = input(&dir);
        multipage_pdf(&pdf, 3);
        let output = dir.path().join("out");
        let python = fake_official_shim_python(dir.path(), "4.0.0a6", false);
        let model_dir = dir.path().join("models");
        let config = dir.path().join("mineru.toml");
        let mut cmd = mineru();
        cmd.args(["-p"])
            .arg(pdf)
            .args(["-o"])
            .arg(&output)
            .args([
                "--backend",
                "hybrid-http-client",
                "--effort",
                effort,
                "--url",
                "http://model.example/v1",
                "--method",
                "ocr",
                "--lang",
                "en",
                "--image-analysis",
                "false",
                "--model-stack",
                "full",
                "--start",
                "2",
                "--official-python",
            ])
            .arg(python)
            .args(["--official-model-dir"])
            .arg(&model_dir)
            .args(["--official-config"])
            .arg(&config)
            .env("MINERU_MODEL_STACK", "light")
            .env("MINERU_VL_API_KEY", "test-key")
            .env("MINERU_VL_MODEL_NAME", "test-model");
        let result = command(cmd).await;
        assert!(
            result.status.success(),
            "{effort}: {}",
            String::from_utf8_lossy(&result.stderr)
        );
        assert_eq!(result.stdout, b"");
        let record: Value = serde_json::from_slice(
            &std::fs::read(dir.path().join("fake-mineru-record.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(record["kwargs"]["backend"], "hybrid-http-client");
        assert_eq!(record["kwargs"]["effort"], effort);
        assert_eq!(record["kwargs"]["server_url"], "http://model.example/v1");
        assert_eq!(record["kwargs"]["method"], "ocr");
        assert_eq!(record["kwargs"]["lang"], "en");
        assert_eq!(record["kwargs"]["image_analysis"], false);
        assert_eq!(record["kwargs"]["page_range"], "3~-1");
        assert!(record["kwargs"].get("model_stack").is_none());
        assert_eq!(record["env"]["MINERU_MODEL_STACK"], "full");
        assert_eq!(
            record["env"]["MINERU_MODEL_BASE_DIR"],
            model_dir.to_str().unwrap()
        );
        assert_eq!(record["env"]["MINERU_CONFIG"], config.to_str().unwrap());
        assert_eq!(record["env"]["MINERU_VL_API_KEY"], "test-key");
        assert_eq!(record["env"]["MINERU_VL_MODEL_NAME"], "test-model");
        assert!(output.join("document/hybrid-v4/markdown.md").is_file());
        assert_eq!(
            std::fs::read(output.join("document/hybrid-v4/images/fake.png")).unwrap(),
            b"png"
        );
    }
}

#[cfg(unix)]
#[tokio::test]
async fn official_shim_normalizes_bare_image_assets_and_references() {
    let dir = tempfile::tempdir().unwrap();
    let pdf = input(&dir);
    let output = dir.path().join("out");
    let python = fake_official_asset_shim_python(dir.path(), "figure.jpg");
    let mut cmd = mineru();
    cmd.args(["-p"])
        .arg(pdf)
        .args([
            "-o",
            output.to_str().unwrap(),
            "--backend",
            "hybrid-http-client",
            "--official-python",
        ])
        .arg(python);
    let result = command(cmd).await;
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );

    let bundle = output.join("document/hybrid-v4");
    assert_eq!(
        std::fs::read(bundle.join("images/figure.jpg")).unwrap(),
        b"\xff\xd8\x00\xff\xd9"
    );
    assert_eq!(
        std::fs::read_to_string(bundle.join("markdown.md")).unwrap(),
        "![figure](images/figure.jpg)\n"
    );
    let middle: Value =
        serde_json::from_slice(&std::fs::read(bundle.join("middle_json.json")).unwrap()).unwrap();
    assert_eq!(middle["pages"][0]["image_path"], "images/figure.jpg");
    assert_eq!(middle["pages"][0]["figure.jpg"], "ordinary-key");
    let content: Value =
        serde_json::from_slice(&std::fs::read(bundle.join("content_list.json")).unwrap()).unwrap();
    assert_eq!(content[0]["img_path"], "images/figure.jpg");
}

#[cfg(unix)]
#[tokio::test]
async fn official_shim_rejects_unsafe_bare_image_paths() {
    for path in [
        "/figure.jpg",
        "../figure.jpg",
        "nested/figure.jpg",
        r#"nested\figure.jpg"#,
        "figure.jpg\0",
        "figure.jpg.",
        "CON.jpg",
        "images/../figure.jpg",
    ] {
        let dir = tempfile::tempdir().unwrap();
        let pdf = input(&dir);
        let output = dir.path().join("out");
        let python = fake_official_asset_shim_python(dir.path(), path);
        let mut cmd = mineru();
        cmd.args(["-p"])
            .arg(pdf)
            .args([
                "-o",
                output.to_str().unwrap(),
                "--backend",
                "hybrid-http-client",
                "--official-python",
            ])
            .arg(python);
        let result = command(cmd).await;
        assert!(
            !result.status.success(),
            "unsafe path unexpectedly succeeded: {path}"
        );
        assert!(!output.exists(), "unsafe path was published: {path}");
    }
}

#[cfg(unix)]
#[tokio::test]
async fn official_shim_enforces_package_pin_and_save_cap() {
    let bad_dir = tempfile::tempdir().unwrap();
    let bad_pdf = input(&bad_dir);
    let bad_output = bad_dir.path().join("out");
    let bad_python = fake_official_shim_python(bad_dir.path(), "3.4.5", false);
    let mut bad_cmd = mineru();
    bad_cmd
        .args(["-p"])
        .arg(bad_pdf)
        .args(["-o"])
        .arg(&bad_output)
        .args(["--backend", "hybrid-http-client", "--official-python"])
        .arg(bad_python);
    let bad_result = command(bad_cmd).await;
    assert!(!bad_result.status.success());
    assert!(
        unstamped(&bad_result.stderr)
            .join("\n")
            .contains("package version is not 4.0.0a6")
    );
    assert!(!bad_dir.path().join("fake-mineru-record.json").exists());
    assert!(!bad_output.exists());

    let cap_dir = tempfile::tempdir().unwrap();
    let cap_pdf = input(&cap_dir);
    let cap_output = cap_dir.path().join("out");
    let cap_python = fake_official_shim_python(cap_dir.path(), "4.0.0a6", true);
    let mut cap_cmd = mineru();
    cap_cmd
        .args(["-p"])
        .arg(cap_pdf)
        .args(["-o"])
        .arg(&cap_output)
        .args(["--backend", "hybrid-http-client", "--official-python"])
        .arg(cap_python)
        .args(["--max-output-bytes", "1024"]);
    let cap_result = command(cap_cmd).await;
    assert!(!cap_result.status.success());
    assert!(
        unstamped(&cap_result.stderr)
            .join("\n")
            .contains("official bundle exceeds configured byte limit"),
        "{}",
        String::from_utf8_lossy(&cap_result.stderr)
    );
    assert!(!cap_output.exists());
}

#[cfg(unix)]
#[tokio::test]
async fn explicit_auto_model_stack_overrides_environment_in_cli() {
    let dir = tempfile::tempdir().unwrap();
    let pdf = input(&dir);
    let output = dir.path().join("out");
    let python = fake_official_shim_python(dir.path(), "4.0.0a6", false);
    let mut cmd = mineru();
    cmd.args(["-p"])
        .arg(pdf)
        .args(["-o"])
        .arg(&output)
        .args([
            "--backend",
            "hybrid-http-client",
            "--model-stack",
            "auto",
            "--official-python",
        ])
        .arg(python)
        .env("MINERU_MODEL_STACK", "full");
    let result = command(cmd).await;
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let record: Value =
        serde_json::from_slice(&std::fs::read(dir.path().join("fake-mineru-record.json")).unwrap())
            .unwrap();
    assert_eq!(record["env"]["MINERU_MODEL_STACK"], "auto");
    assert!(record["kwargs"].get("page_range").is_none());
}

#[cfg(unix)]
#[test]
fn persistent_c2a_handshake_and_two_requests_are_hermetic() {
    let dir = tempfile::tempdir().unwrap();
    let (python, record) = fake_persistent_python(dir.path());
    let start = persistent_start(dir.path(), persistent_capabilities());
    let mut first = persistent_request(dir.path(), "c2a-1", 1, "high", Some("2~3"));
    first["max_bundle_bytes"] = json!(384);
    let mut second = persistent_request(dir.path(), "c2a-2", 2, "xhigh", None);
    second["max_bundle_bytes"] = json!(384);
    let output = run_persistent(&python, &[start, first, second]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let frames = persistent_stdout_frames(&output);
    assert_eq!(frames.len(), 3, "stdout contained a non-protocol print");
    assert_eq!(frames[0]["type"], "handshake");
    assert_eq!(frames[0]["protocol"], "mineru-rs-official-worker/2");
    assert_eq!(frames[0]["status"], "ready");
    assert_eq!(frames[0]["package_version"], "4.0.0a6");
    assert_eq!(frames[0]["schema_version"], "1.0");
    assert_eq!(frames[0]["backend"], "hybrid-http-client");
    assert_eq!(frames[0]["max_in_flight"], 1);
    assert_eq!(frames[0]["capabilities"], persistent_capabilities());
    assert!(
        frames[0]["diagnostic"]
            .as_str()
            .unwrap()
            .contains("persistent parser import stderr")
    );
    for (frame, request_id, sequence) in [(&frames[1], "c2a-1", 1), (&frames[2], "c2a-2", 2)] {
        assert_eq!(frame["type"], "result");
        assert_eq!(frame["protocol"], "mineru-rs-official-worker/2");
        assert_eq!(frame["status"], "ok");
        assert_eq!(frame["request_id"], request_id);
        assert_eq!(frame["sequence"], sequence);
        assert_eq!(frame["package_version"], "4.0.0a6");
        assert_eq!(frame["schema_version"], "1.0");
        assert_eq!(frame["backend"], "hybrid-http-client");
        assert_eq!(frame["bundle_name"], "hybrid-v4");
        assert!(
            frame["diagnostic"]
                .as_str()
                .unwrap()
                .contains("persistent result.save stderr")
        );
    }

    let entries: Vec<Value> = serde_json::from_slice(&std::fs::read(&record).unwrap()).unwrap();
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0]["kwargs"]["effort"], "high");
    assert_eq!(entries[0]["kwargs"]["page_range"], "2~3");
    assert!(entries[1]["kwargs"].get("page_range").is_none());
    assert_eq!(entries[1]["kwargs"]["effort"], "xhigh");
    assert_eq!(entries[0]["env"]["MINERU_MODEL_STACK"], "full");
    assert_eq!(
        entries[0]["env"]["MINERU_MODEL_BASE_DIR"],
        dir.path().join("models").to_str().unwrap()
    );
    assert_eq!(
        entries[0]["env"]["MINERU_CONFIG"],
        dir.path().join("config.toml").to_str().unwrap()
    );
    assert_eq!(entries[0]["env"]["MINERU_VL_API_KEY"], "persistent-key");
    assert_eq!(
        entries[0]["env"]["MINERU_VL_MODEL_NAME"],
        "persistent-model"
    );
    assert_eq!(
        std::fs::read_to_string(format!("{}.init", record.display())).unwrap(),
        "1"
    );
    assert_eq!(
        std::fs::read_to_string(format!("{}.parse", record.display())).unwrap(),
        "2"
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("bundle-1/markdown.md")).unwrap(),
        format!("request-1\\n{}", "x".repeat(200))
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("bundle-2/markdown.md")).unwrap(),
        format!("request-2\\n{}", "x".repeat(200))
    );
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("persistent mineru import stdout"));
    assert!(stderr.contains("persistent parser import stdout"));
    assert!(stderr.contains("persistent parse_async stdout"));
    assert!(stderr.contains("persistent result.save stdout"));
}

#[cfg(unix)]
#[test]
fn persistent_c2a_rejects_bad_startup_and_frames() {
    let mut cases = Vec::new();
    let dir = tempfile::tempdir().unwrap();
    let (python, _) = fake_persistent_python(dir.path());
    let mut bad_package = persistent_start(dir.path(), persistent_capabilities());
    bad_package["package_version"] = json!("3.4.5");
    cases.push((dir, python, vec![bad_package], false));

    let dir = tempfile::tempdir().unwrap();
    let (python, _) = fake_persistent_python(dir.path());
    let mut bad_capabilities = persistent_capabilities();
    bad_capabilities["cancellation"] = json!("retry");
    let start = persistent_start(dir.path(), bad_capabilities);
    cases.push((dir, python, vec![start], false));

    let dir = tempfile::tempdir().unwrap();
    let (python, _) = fake_persistent_python(dir.path());
    let start = persistent_start(dir.path(), persistent_capabilities());
    cases.push((
        dir,
        python,
        vec![
            start,
            json!({"type":"unknown","protocol":"mineru-rs-official-worker/2"}),
        ],
        true,
    ));

    let dir = tempfile::tempdir().unwrap();
    let (python, _) = fake_persistent_python(dir.path());
    let start = persistent_start(dir.path(), persistent_capabilities());
    let mut same_path = persistent_request(dir.path(), "same-path", 1, "medium", None);
    same_path["bundle_path"] = same_path["input_path"].clone();
    cases.push((dir, python, vec![start, same_path], true));

    let dir = tempfile::tempdir().unwrap();
    let (python, _) = fake_persistent_python(dir.path());
    let mut unsupported = persistent_request(dir.path(), "bad-kwargs", 1, "medium", None);
    unsupported["model_stack"] = json!("full");
    let start = persistent_start(dir.path(), persistent_capabilities());
    cases.push((dir, python, vec![start, unsupported], true));

    let dir = tempfile::tempdir().unwrap();
    let (python, _) = fake_persistent_python(dir.path());
    let start = persistent_start(dir.path(), persistent_capabilities());
    cases.push((
        dir,
        python,
        vec![
            start,
            json!({
                "type": "request",
                "payload": "x".repeat(70 * 1024),
            }),
        ],
        true,
    ));

    for (_dir, python, frames, has_handshake) in cases {
        let output = run_persistent(&python, &frames);
        assert!(!output.status.success());
        let response = persistent_stdout_frames(&output);
        if has_handshake {
            assert_eq!(response.first().unwrap()["type"], "handshake");
        }
        assert_eq!(response.last().unwrap()["type"], "error");
        assert_eq!(response.last().unwrap()["status"], "error");
    }
}

#[cfg(unix)]
#[test]
fn persistent_c2a_request_error_does_not_poison_the_session() {
    let dir = tempfile::tempdir().unwrap();
    let (python, _) = fake_persistent_python(dir.path());
    let start = persistent_start(dir.path(), persistent_capabilities());
    let mut failed = persistent_request(dir.path(), "c2a-error", 1, "medium", None);
    failed["max_bundle_bytes"] = json!(64);
    let recovered = persistent_request(dir.path(), "c2a-recovered", 2, "medium", None);
    let output = run_persistent(&python, &[start, failed, recovered]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let frames = persistent_stdout_frames(&output);
    assert_eq!(frames.len(), 3);
    assert_eq!(frames[0]["type"], "handshake");
    assert_eq!(frames[1]["request_id"], "c2a-error");
    assert_eq!(frames[1]["status"], "error");
    assert_eq!(frames[2]["request_id"], "c2a-recovered");
    assert_eq!(frames[2]["status"], "ok");
    assert!(dir.path().join("bundle-2/markdown.md").is_file());
}

#[cfg(unix)]
#[test]
fn persistent_c2a_oversized_document_error_does_not_poison_the_session() {
    let dir = tempfile::tempdir().unwrap();
    let (python, _) = fake_persistent_oversized_error_python(dir.path());
    let start = persistent_start(dir.path(), persistent_capabilities());
    let first = persistent_request(dir.path(), "c2a-oversized-error", 1, "medium", None);
    let second = persistent_request(dir.path(), "c2a-after-oversized-error", 2, "medium", None);
    let output = run_persistent(&python, &[start, first, second]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        std::str::from_utf8(&output.stdout)
            .unwrap()
            .lines()
            .all(|line| line.len() < 64 * 1024)
    );

    let frames = persistent_stdout_frames(&output);
    assert_eq!(frames.len(), 3);
    assert_eq!(frames[0]["type"], "handshake");
    assert_eq!(frames[1]["type"], "result");
    assert_eq!(frames[1]["status"], "error");
    assert_eq!(frames[1]["request_id"], "c2a-oversized-error");
    assert!(
        frames[1]["error"]
            .as_str()
            .is_some_and(|error| error.starts_with("parser error "))
    );
    assert!(String::from_utf8_lossy(&output.stderr).contains("captured parser diagnostic"));
    assert_eq!(frames[2]["type"], "result");
    assert_eq!(frames[2]["status"], "ok");
    assert_eq!(frames[2]["request_id"], "c2a-after-oversized-error");
    assert!(dir.path().join("bundle-2/markdown.md").is_file());
}

#[cfg(unix)]
#[tokio::test]
async fn official_worker_rejects_bad_protocol_and_lifecycle_failures() {
    for mode in [
        "protocol", "request", "package", "schema", "backend", "stdout", "stderr", "crash",
    ] {
        let dir = tempfile::tempdir().unwrap();
        let pdf = input(&dir);
        let output = dir.path().join("out");
        let python = fake_official_failure(dir.path(), mode);
        let mut cmd = mineru();
        cmd.args(["-p"])
            .arg(pdf)
            .args(["-o"])
            .arg(&output)
            .args(["--backend", "hybrid-http-client", "--official-python"])
            .arg(python);
        let result = command(cmd).await;
        assert!(!result.status.success(), "{mode} unexpectedly succeeded");
        assert!(!output.exists(), "{mode} published output");
        assert!(
            !unstamped(&result.stderr)
                .join("\n")
                .contains("local-model worker is integrated"),
            "{mode} took the C0 path"
        );
    }
}

#[cfg(unix)]
#[tokio::test]
async fn official_worker_deadline_terminates_fake_child() {
    let dir = tempfile::tempdir().unwrap();
    let pdf = input(&dir);
    let output = dir.path().join("out");
    let python = fake_official_failure(dir.path(), "timeout");
    let mut cmd = mineru();
    cmd.args(["-p"])
        .arg(pdf)
        .args(["-o"])
        .arg(&output)
        .args(["--backend", "hybrid-http-client", "--official-python"])
        .arg(python)
        .args(["--total-deadline-seconds", "1"]);
    let result = command(cmd).await;
    assert!(!result.status.success());
    assert!(
        unstamped(&result.stderr)
            .join("\n")
            .contains("official worker deadline expired")
    );
    assert!(!output.exists());
}

#[cfg(unix)]
#[tokio::test]
async fn official_worker_reaps_successful_child_process_group() {
    let dir = tempfile::tempdir().unwrap();
    let pdf = input(&dir);
    let output = dir.path().join("out");
    let python = fake_official_python_with_background(dir.path(), true);
    let mut cmd = mineru();
    cmd.args(["-p"])
        .arg(pdf)
        .args(["-o", output.to_str().unwrap()])
        .args(["--backend", "hybrid-http-client", "--official-python"])
        .arg(python);
    // This covers process startup and input preflight as well as worker cleanup;
    // keep it below the fake descendant's 30-second lifetime while allowing
    // concurrent test suites enough scheduler headroom.
    let result = tokio::time::timeout(std::time::Duration::from_secs(15), command(cmd))
        .await
        .expect("successful worker command timed out during process-group cleanup");
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(output.join("document/hybrid-v4/markdown.md").is_file());
}

#[tokio::test]
async fn direct_hybrid_remote_efforts_require_an_explicit_url() {
    for effort in ["high", "xhigh"] {
        let dir = tempfile::tempdir().unwrap();
        let pdf = input(&dir);
        let output = dir.path().join("out");
        let mut cmd = mineru();
        cmd.args(["-p"]).arg(pdf).args(["-o"]).arg(&output).args([
            "--backend",
            "hybrid-http-client",
            "--effort",
            effort,
        ]);
        let result = command(cmd).await;
        assert!(!result.status.success());
        let stderr = unstamped(&result.stderr).join("\n");
        assert!(
            stderr.contains("requires an explicit HTTP(S) URL"),
            "{effort}: {stderr}"
        );
        assert!(!output.exists());
    }
}

#[tokio::test]
async fn direct_hybrid_rejects_v3_transport_controls() {
    let cases = [
        ("--http-timeout-seconds", "1", "v3-only transport controls"),
        ("--vlm-debug", "true", "v3-only transport controls"),
        (
            "--client-side-output-generation",
            "false",
            "client-side output generation",
        ),
    ];
    for (flag, value, expected) in cases {
        let dir = tempfile::tempdir().unwrap();
        let pdf = input(&dir);
        let output = dir.path().join("out");
        let mut cmd = mineru();
        cmd.args(["-p"]).arg(pdf).args(["-o"]).arg(&output).args([
            "--backend",
            "hybrid-http-client",
            flag,
            value,
        ]);
        let result = command(cmd).await;
        assert!(!result.status.success(), "{flag} unexpectedly succeeded");
        assert!(
            unstamped(&result.stderr).join("\n").contains(expected),
            "{flag}: {}",
            String::from_utf8_lossy(&result.stderr)
        );
        assert!(!output.exists());
    }

    let dir = tempfile::tempdir().unwrap();
    let pdf = input(&dir);
    let output = dir.path().join("out");
    let mut cmd = mineru();
    cmd.args(["-p"])
        .arg(pdf)
        .args(["-o"])
        .arg(&output)
        .args(["--backend", "hybrid-http-client"])
        .env("MINERU_VLM_HTTP_TIMEOUT", "1");
    let result = command(cmd).await;
    assert!(!result.status.success());
    assert!(
        unstamped(&result.stderr)
            .join("\n")
            .contains("v3-only transport controls")
    );
    assert!(!output.exists());
}

#[tokio::test]
async fn direct_hybrid_rejects_invalid_stack_and_relative_official_paths() {
    let dir = tempfile::tempdir().unwrap();
    let pdf = input(&dir);
    let output = dir.path().join("out");
    let mut invalid_stack = mineru();
    invalid_stack.args(["-p"]).arg(&pdf).args([
        "-o",
        output.to_str().unwrap(),
        "--backend",
        "hybrid-http-client",
        "--model-stack",
        "bad",
    ]);
    assert!(!command(invalid_stack).await.status.success());

    let mut relative_python = mineru();
    relative_python
        .args(["-p"])
        .arg(pdf)
        .args(["-o"])
        .arg(&output)
        .args([
            "--backend",
            "hybrid-http-client",
            "--official-python",
            "python",
        ]);
    let result = command(relative_python).await;
    assert!(!result.status.success());
    assert!(
        unstamped(&result.stderr)
            .join("\n")
            .contains("official Python executable path must be absolute")
    );
}

#[cfg(feature = "office")]
#[tokio::test]
async fn direct_hybrid_rejects_office_before_spawning() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("document.docx"), b"not an Office package").unwrap();
    let output = dir.path().join("out");
    let python = std::env::current_exe().unwrap();
    let mut cmd = mineru();
    cmd.args(["-p"])
        .arg(dir.path().join("document.docx"))
        .args([
            "-o",
            output.to_str().unwrap(),
            "--backend",
            "hybrid-http-client",
            "--official-python",
        ])
        .arg(python);
    let result = command(cmd).await;
    assert!(!result.status.success());
    let stderr = unstamped(&result.stderr).join("\n");
    assert!(
        stderr.contains("accepts only PDF and official image inputs"),
        "{stderr}"
    );
    assert!(
        !stderr.contains("official worker"),
        "worker spawned: {stderr}"
    );
}

#[tokio::test]
async fn api_mode_hybrid_backend_is_explicitly_unsupported() {
    let dir = tempfile::tempdir().unwrap();
    let pdf = input(&dir);
    let output = dir.path().join("out");
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let api_url = format!("http://{}", listener.local_addr().unwrap());
    let mut cmd = mineru();
    cmd.args(["-p"]).arg(pdf).args(["-o"]).arg(&output).args([
        "--api-url",
        &api_url,
        "--backend",
        "hybrid-http-client",
        "--official-worker-mode",
        "persistent",
    ]);
    let result = command(cmd).await;
    assert!(!result.status.success());
    assert_eq!(result.stdout, b"");
    assert_eq!(
        unstamped(&result.stderr),
        ["failed: backend=hybrid-http-client is direct-only; API mode does not support Hybrid"]
    );
    assert!(!output.exists());
    assert_eq!(
        listener.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
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
    multipage_pdf(&pdf, 2);
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
