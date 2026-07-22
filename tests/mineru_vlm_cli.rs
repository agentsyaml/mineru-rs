use axum::{
    Json, Router,
    extract::Request,
    response::IntoResponse,
    routing::{get, post},
};
use serde_json::{Value, json};
use std::{
    path::Path,
    process::{Command, Output},
    sync::{Arc, Mutex},
};

fn cli() -> Command {
    Command::new(env!("CARGO_BIN_EXE_mineru-vlm"))
}

#[derive(Clone, Default)]
struct Seen(Arc<Mutex<Vec<(String, Value, Option<String>)>>>);

#[derive(Clone)]
struct Mock {
    seen: Seen,
    fail_from_completion: Option<usize>,
}

async fn mock_with_failure(fail_from_completion: Option<usize>) -> (String, Seen) {
    async fn models(
        axum::extract::State(mock): axum::extract::State<Mock>,
        request: Request,
    ) -> Json<Value> {
        let auth = request
            .headers()
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        mock.seen
            .0
            .lock()
            .unwrap()
            .push(("models".into(), json!({}), auth));
        Json(json!({"data":[{"id":"mock"}]}))
    }
    async fn completion(
        axum::extract::State(mock): axum::extract::State<Mock>,
        request: Request,
    ) -> axum::response::Response {
        let auth = request
            .headers()
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        let (_, body) = request.into_parts();
        let body = axum::body::to_bytes(body, 16 * 1024 * 1024).await.unwrap();
        let mut seen = mock.seen.0.lock().unwrap();
        seen.push((
            "completion".into(),
            serde_json::from_slice(&body).unwrap(),
            auth,
        ));
        let count = seen
            .iter()
            .filter(|(kind, _, _)| kind == "completion")
            .count();
        drop(seen);
        if mock.fail_from_completion.is_some_and(|from| count >= from) {
            return (axum::http::StatusCode::BAD_REQUEST, "mock failure").into_response();
        }
        Json(json!({"choices":[{"finish_reason":"stop","message":{"content":"<|box_start|>1 1 200 200<|box_end|><|ref_start|>text<|ref_end|><|rotate_up|>"}}]})).into_response()
    }
    let seen = Seen::default();
    let app = Router::new()
        .route("/v1/models", get(models))
        .route("/v1/chat/completions", post(completion))
        .with_state(Mock {
            seen: seen.clone(),
            fail_from_completion,
        });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{address}"), seen)
}

async fn mock() -> (String, Seen) {
    mock_with_failure(None).await
}

async fn command(mut command: Command) -> Output {
    tokio::task::spawn_blocking(move || command.output().unwrap())
        .await
        .unwrap()
}

#[tokio::test]
#[ignore = "full legacy-and-official CLI/API/PDF process e2e"]
async fn legacy_stays_flat_and_official_output_is_nested() {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("source.pdf");
    std::fs::copy("tests/fixtures/pdf/minimal.pdf", &input).unwrap();
    let (url, _) = mock().await;

    let legacy = dir.path().join("legacy");
    let result = tokio::task::spawn_blocking({
        let mut command = cli();
        command
            .arg(&input)
            .args(["--base-url", &url, "--model", "mock", "--output"])
            .arg(&legacy);
        move || command.output().unwrap()
    })
    .await
    .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    for name in [
        "document.json",
        "document.md",
        "middle.json",
        "content_list.json",
    ] {
        assert!(legacy.join(name).is_file(), "{name}");
    }
    assert!(legacy.join("source_layout.pdf").is_file());
    assert!(!legacy.join("source/vlm").exists());

    let official = dir.path().join("official");
    let result = tokio::task::spawn_blocking({
        let mut command = cli();
        command
            .arg(&input)
            .args([
                "--official-output",
                "--base-url",
                &url,
                "--model",
                "mock",
                "--output",
            ])
            .arg(&official);
        move || command.output().unwrap()
    })
    .await
    .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let mut files: Vec<_> = std::fs::read_dir(official.join("source/vlm"))
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    files.sort();
    assert_eq!(
        files,
        [
            "source.md",
            "source_content_list.json",
            "source_content_list_v2.json",
            "source_layout.pdf",
            "source_middle.json",
            "source_model.json"
        ]
    );
    for name in [
        "source.md",
        "source_middle.json",
        "source_model.json",
        "source_content_list.json",
        "source_content_list_v2.json",
        "source_layout.pdf",
    ] {
        assert!(official.join("source/vlm").join(name).is_file(), "{name}");
    }
    let preview = lopdf::Document::load(official.join("source/vlm/source_layout.pdf")).unwrap();
    let text = preview.extract_text(&[1]).unwrap();
    assert!(text.contains("minimal") && text.contains('1'), "{text:?}");
    assert!(!official.join("document.json").exists());
}

#[test]
#[ignore = "CLI process contract e2e"]
fn legacy_help_and_official_batch_applicability_are_preserved() {
    let help = cli().arg("--help").output().unwrap();
    assert!(help.status.success());
    let help = String::from_utf8(help.stdout).unwrap();
    assert!(help.contains("--base-url") && help.contains("--model"));
    assert!(help.contains("--official-output") && help.contains("--batch-size"));
    assert!(
        !cli()
            .args(["x.pdf", "--batch-size", "2"])
            .status()
            .unwrap()
            .success()
    );
}

#[tokio::test]
#[ignore = "full CLI/API/PDF environment-auth process e2e"]
async fn legacy_requires_base_url_and_model_but_official_uses_env_and_discovery() {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("source.pdf");
    std::fs::copy("tests/fixtures/pdf/minimal.pdf", &input).unwrap();
    for args in [
        vec![input.as_os_str().to_owned()],
        vec![
            input.as_os_str().to_owned(),
            "--base-url".into(),
            "http://127.0.0.1:1".into(),
        ],
    ] {
        let mut cmd = cli();
        cmd.args(args);
        assert!(!command(cmd).await.status.success());
    }

    let (url, seen) = mock().await;
    let output = dir.path().join("official");
    let mut cmd = cli();
    cmd.arg(&input)
        .args(["--official-output", "--output"])
        .arg(&output)
        .env("MINERU_VL_SERVER", &url)
        .env("MINERU_VL_MODEL_NAME", "environment-model")
        .env("MINERU_VL_API_KEY", "environment-key");
    let result = command(cmd).await;
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let seen = seen.0.lock().unwrap();
    assert!(!seen.iter().any(|(kind, _, _)| kind == "models"));
    let (_, request, auth) = seen
        .iter()
        .find(|(kind, _, _)| kind == "completion")
        .unwrap();
    assert_eq!(request["model"], "environment-model");
    assert_eq!(auth.as_deref(), Some("Bearer environment-key"));
}

#[tokio::test]
#[ignore = "full CLI/API/PDF model-discovery process e2e"]
async fn official_discovers_one_model_with_environment_server_and_key() {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("source.pdf");
    std::fs::copy("tests/fixtures/pdf/minimal.pdf", &input).unwrap();
    let (url, seen) = mock().await;
    let mut cmd = cli();
    cmd.arg(&input)
        .args(["--official-output", "--output"])
        .arg(dir.path().join("official"))
        .env("MINERU_VL_SERVER", &url)
        .env("MINERU_VL_API_KEY", "environment-key")
        .env_remove("MINERU_VL_MODEL_NAME");
    assert!(command(cmd).await.status.success());
    let calls = seen.0.lock().unwrap();
    assert_eq!(
        calls.iter().filter(|(kind, _, _)| kind == "models").count(),
        1
    );
    let (_, _, auth) = calls.iter().find(|(kind, _, _)| kind == "models").unwrap();
    assert_eq!(auth.as_deref(), Some("Bearer environment-key"));
    let (_, request, auth) = calls
        .iter()
        .find(|(kind, _, _)| kind == "completion")
        .unwrap();
    assert_eq!(request["model"], "mock");
    assert_eq!(auth.as_deref(), Some("Bearer environment-key"));
}

#[tokio::test]
#[ignore = "full CLI/API/PDF credential-override process e2e"]
async fn official_cli_model_and_key_override_environment() {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("source.pdf");
    std::fs::copy("tests/fixtures/pdf/minimal.pdf", &input).unwrap();
    let (url, seen) = mock().await;
    let mut cmd = cli();
    cmd.arg(&input)
        .args([
            "--official-output",
            "--base-url",
            &url,
            "--model",
            "cli-model",
            "--api-key",
            "cli-key",
            "--output",
        ])
        .arg(dir.path().join("official"))
        .env("MINERU_VL_MODEL_NAME", "environment-model")
        .env("MINERU_VL_API_KEY", "environment-key");
    assert!(command(cmd).await.status.success());
    let seen = seen.0.lock().unwrap();
    assert!(!seen.iter().any(|(kind, _, _)| kind == "models"));
    let (_, request, auth) = seen
        .iter()
        .find(|(kind, _, _)| kind == "completion")
        .unwrap();
    assert_eq!(request["model"], "cli-model");
    assert_eq!(auth.as_deref(), Some("Bearer cli-key"));
}

#[tokio::test]
#[ignore = "full CLI/API/PDF recursive-directory process e2e"]
async fn official_directory_is_recursive_and_skips_unsupported_files() {
    let dir = tempfile::tempdir().unwrap();
    let nested = dir.path().join("nested");
    std::fs::create_dir(&nested).unwrap();
    std::fs::copy("tests/fixtures/pdf/minimal.pdf", dir.path().join("z.pdf")).unwrap();
    std::fs::copy("tests/fixtures/pdf/minimal.pdf", nested.join("a.pdf")).unwrap();
    std::fs::write(dir.path().join("skip.txt"), "x").unwrap();
    let output = tempfile::tempdir().unwrap();
    let (url, _) = mock().await;
    let result = tokio::task::spawn_blocking({
        let mut command = cli();
        command
            .arg(dir.path())
            .args([
                "--official-output",
                "--base-url",
                &url,
                "--model",
                "mock",
                "--output",
            ])
            .arg(output.path());
        move || command.output().unwrap()
    })
    .await
    .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(String::from_utf8_lossy(&result.stderr).contains("skipped unsupported input"));
    assert!(output.path().join("a/vlm/a.md").is_file());
    assert!(output.path().join("z/vlm/z.md").is_file());
}

#[tokio::test]
#[ignore = "CLI process contract e2e"]
async fn official_static_preflight_makes_no_requests_or_output() {
    let dir = tempfile::tempdir().unwrap();
    let (url, seen) = mock().await;
    let pdf = dir.path().join("input.pdf");
    std::fs::copy("tests/fixtures/pdf/minimal.pdf", &pdf).unwrap();
    let run = |input: &Path, output: &Path, extra: &[&str]| {
        let mut cmd = cli();
        cmd.arg(input)
            .args([
                "--official-output",
                "--base-url",
                &url,
                "--model",
                "mock",
                "--output",
            ])
            .arg(output)
            .args(extra);
        cmd
    };
    let output = dir.path().join("zero-out");
    let zero = command(run(&pdf, &output, &["--batch-size", "0"])).await;
    assert!(
        !zero.status.success()
            && String::from_utf8_lossy(&zero.stderr).contains("greater than zero")
    );
    assert!(!output.exists());
    std::fs::copy("tests/fixtures/pdf/minimal.pdf", dir.path().join("a!.pdf")).unwrap();
    std::fs::copy("tests/fixtures/pdf/minimal.pdf", dir.path().join("a?.pdf")).unwrap();
    let outside = tempfile::tempdir().unwrap();
    let output = outside.path().join("out");
    let duplicate = command(run(dir.path(), &output, &[])).await;
    assert!(!duplicate.status.success());
    assert!(String::from_utf8_lossy(&duplicate.stderr).contains("duplicate"));
    assert!(!output.exists());
    let contained = dir.path().join("contained");
    std::fs::create_dir(&contained).unwrap();
    std::fs::copy("tests/fixtures/pdf/minimal.pdf", contained.join("only.pdf")).unwrap();
    let nested_output = contained.join("nested-out");
    let inside = command(run(&contained, &nested_output, &[])).await;
    assert!(
        !inside.status.success()
            && String::from_utf8_lossy(&inside.stderr).contains("inside input")
    );
    assert!(!nested_output.exists());
    let alias_root = tempfile::tempdir().unwrap();
    std::fs::create_dir(alias_root.path().join("INPUT")).unwrap();
    let alias = command(run(&pdf, &alias_root.path().join("input"), &[])).await;
    assert!(
        !alias.status.success()
            && String::from_utf8_lossy(&alias.stderr).contains("case-insensitive alias")
    );
    assert!(seen.0.lock().unwrap().is_empty());
}

#[cfg(unix)]
#[tokio::test]
#[ignore = "CLI process contract e2e"]
async fn official_symlink_and_special_input_make_no_requests() {
    use std::os::unix::{fs::symlink, net::UnixListener};
    let dir = tempfile::tempdir().unwrap();
    let (url, seen) = mock().await;
    let target = dir.path().join("target.pdf");
    std::fs::copy("tests/fixtures/pdf/minimal.pdf", &target).unwrap();
    let link = dir.path().join("link.pdf");
    symlink(&target, &link).unwrap();
    let socket_path = dir.path().join("input.sock");
    let socket = UnixListener::bind(&socket_path).unwrap();
    for input in [&link, &socket_path] {
        let mut cmd = cli();
        cmd.arg(input)
            .args([
                "--official-output",
                "--base-url",
                &url,
                "--model",
                "mock",
                "--output",
            ])
            .arg(dir.path().join("out"));
        assert!(!command(cmd).await.status.success());
    }
    assert!(seen.0.lock().unwrap().is_empty());
    drop(socket);
}

#[tokio::test]
#[ignore = "full CLI/PDF rollback process e2e"]
async fn official_stops_after_b_and_preserves_outputs() {
    let dir = tempfile::tempdir().unwrap();
    for name in ["a.pdf", "b.pdf", "c.pdf"] {
        std::fs::copy("tests/fixtures/pdf/minimal.pdf", dir.path().join(name)).unwrap();
    }
    let output = tempfile::tempdir().unwrap();
    let root = output.path().join("out");
    std::fs::create_dir_all(root.join("b/vlm")).unwrap();
    std::fs::write(root.join("b/vlm/sentinel"), "keep").unwrap();
    // The fixed response yields layout plus one semantic extraction per document.
    let (url, seen) = mock_with_failure(Some(3)).await;
    let mut cmd = cli();
    cmd.arg(dir.path())
        .args([
            "--official-output",
            "--base-url",
            &url,
            "--model",
            "mock",
            "--batch-size",
            "2",
            "--output",
        ])
        .arg(&root);
    let result = command(cmd).await;
    assert!(!result.status.success());
    let stderr = String::from_utf8_lossy(&result.stderr);
    assert!(
        stderr.contains("processing ") && stderr.contains("a.pdf"),
        "{stderr}"
    );
    assert!(
        stderr.contains("completed ") && stderr.contains("a.pdf"),
        "{stderr}"
    );
    assert!(stderr.find("processing ").unwrap() < stderr.find("completed ").unwrap());
    assert!(stderr.find("completed ").unwrap() < stderr.rfind("processing ").unwrap());
    assert!(stderr.contains("failed ") && stderr.contains("b.pdf"));
    assert_eq!(stderr.matches("completed ").count(), 1);
    assert!(!stderr.contains("c.pdf") && !stderr.contains("batch 1/2: completed"));
    assert!(root.join("a/vlm/a.md").is_file());
    assert_eq!(
        std::fs::read_to_string(root.join("b/vlm/sentinel")).unwrap(),
        "keep"
    );
    assert!(!root.join("c/vlm").exists());
    assert!(!std::fs::read_dir(root.join("b")).unwrap().any(|e| {
        e.unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".vlm-staging-parent-")
    }));
    // Two calls complete a; b fails on its first call and c never starts.
    assert_eq!(
        seen.0
            .lock()
            .unwrap()
            .iter()
            .filter(|(kind, _, _)| kind == "completion")
            .count(),
        3
    );
}
