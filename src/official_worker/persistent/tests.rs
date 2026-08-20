use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

use serde_json::Value;

use super::super::{
    OfficialPersistentWorker, OfficialRequest, OfficialSessionConfig, PACKAGE_VERSION,
    PERSISTENT_BACKEND, PERSISTENT_PROTOCOL, SCHEMA_VERSION,
};
use super::protocol::persistent_capabilities;

#[cfg(unix)]
fn shell_quote(path: &Path) -> String {
    format!("'{}'", path.to_string_lossy().replace('\'', "'\\''"))
}

#[cfg(unix)]
fn persistent_fixture(root: &Path, mode: &str) -> (PathBuf, PathBuf, PathBuf, PathBuf, PathBuf) {
    use std::os::unix::fs::PermissionsExt;

    let script = root.join("persistent-worker");
    let log = root.join("requests.log");
    let starts = root.join("starts");
    let pids = root.join("pids");
    let marker = root.join("marker");
    let body = r##"#!/bin/sh
MODE=__MODE__
LOG=__LOG__
STARTS=__STARTS__
PIDS=__PIDS__
MARKER=__MARKER__

read -r startup || exit 3
printf '%s' "$startup" > "$MARKER"
starts=0
if [ -f "$STARTS" ]; then starts=$(cat "$STARTS"); fi
starts=$((starts + 1))
printf '%s' "$starts" > "$STARTS"

if [ "$MODE" = bad-handshake ]; then
    printf '%s\n' '{"type":"handshake","protocol":"wrong"}'
    exit 0
fi
printf '%s\n' '{"type":"handshake","protocol":"mineru-rs-official-worker/2","status":"ready","package_version":"4.0.0a6","schema_version":"1.0","backend":"hybrid-http-client","max_in_flight":1,"capabilities":{"efforts":["medium","high","xhigh"],"model_stacks":["auto","light","full"],"input_formats":["pdf","png","jpeg","jpg","jp2","webp","gif","bmp","tiff"],"bundle_name":"hybrid-v4","cancellation":"process-terminate"}}'

requests=0
while IFS= read -r request; do
    requests=$((requests + 1))
    id=$(printf '%s' "$request" | sed -n 's/.*"request_id":"\([^"]*\)".*/\1/p')
    sequence=$(printf '%s' "$request" | sed -n 's/.*"sequence":\([0-9]*\).*/\1/p')
    bundle=$(printf '%s' "$request" | sed -n 's/.*"bundle_path":"\([^"]*\)".*/\1/p')
    printf '%s %s %s\n' "$$" "$id" "$sequence" >> "$LOG"
    printf '%s\n' "$$" > "$PIDS"

    if [ "$MODE" = crash-once ] && [ "$starts" -eq 1 ]; then
        exit 7
    fi
    if [ "$MODE" = hang-first ] && [ "$requests" -eq 1 ] && [ "$starts" -eq 1 ]; then
        sleep 30 &
        background=$!
        printf '%s %s\n' "$$" "$background" > "$PIDS"
        while :; do sleep 1; done
    fi
    if [ "$MODE" = oversize ]; then
        i=0
        while [ "$i" -lt 70000 ]; do printf x; i=$((i + 1)); done
        printf '\n'
        exit 0
    fi
    if [ "$MODE" = bad-result ]; then
        id=wrong-request
    fi
    if [ "$MODE" = stderr-error-once ] && [ "$requests" -eq 1 ]; then
        printf '%s\n' 'first-request-stderr' >&2
        printf '%s\n' "{\"type\":\"result\",\"protocol\":\"mineru-rs-official-worker/2\",\"request_id\":\"$id\",\"sequence\":$sequence,\"status\":\"error\",\"package_version\":\"4.0.0a6\",\"schema_version\":\"1.0\",\"backend\":\"hybrid-http-client\",\"bundle_name\":\"hybrid-v4\",\"error\":\"document failed\"}"
        continue
    fi
    mkdir -p "$bundle"
    printf 'request-%s\n' "$sequence" > "$bundle/markdown.md"
    printf '%s\n' "{\"type\":\"result\",\"protocol\":\"mineru-rs-official-worker/2\",\"request_id\":\"$id\",\"sequence\":$sequence,\"status\":\"ok\",\"package_version\":\"4.0.0a6\",\"schema_version\":\"1.0\",\"backend\":\"hybrid-http-client\",\"bundle_name\":\"hybrid-v4\"}"
done
"##
        .replace("__MODE__", &shell_quote(Path::new(mode)))
        .replace("__LOG__", &shell_quote(&log))
        .replace("__STARTS__", &shell_quote(&starts))
        .replace("__PIDS__", &shell_quote(&pids))
        .replace("__MARKER__", &shell_quote(&marker));
    std::fs::write(&script, body).unwrap();
    let mut permissions = std::fs::metadata(&script).unwrap().permissions();
    permissions.set_mode(0o700);
    std::fs::set_permissions(&script, permissions).unwrap();
    (script, log, starts, pids, marker)
}

#[cfg(unix)]
fn persistent_config(root: &Path) -> OfficialSessionConfig {
    OfficialSessionConfig::new(
        "full".into(),
        Some(root.join("models")),
        Some(root.join("config.toml")),
        Some("test-key".into()),
        Some("test-model".into()),
    )
    .unwrap()
}

#[cfg(unix)]
fn persistent_request(
    root: &Path,
    config: &OfficialSessionConfig,
    request_id: &str,
    effort: &str,
) -> OfficialRequest {
    let server_url = (effort != "medium").then(|| "http://model.example/v1".to_owned());
    let mut request = OfficialRequest::new(
        PERSISTENT_BACKEND.into(),
        effort.into(),
        server_url,
        "ocr".into(),
        "en".into(),
        false,
        None,
        config.model_stack.clone(),
        config.model_base_dir.clone(),
        config.config.clone(),
        config.vl_api_key.clone(),
        config.vl_model_name.clone(),
        1024,
    );
    request.request_id = request_id.into();
    let _ = root;
    request
}

#[cfg(unix)]
async fn wait_for_file(path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !path.exists() && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(path.exists(), "fixture did not create {}", path.display());
}

#[cfg(unix)]
async fn assert_pids_dead(path: &Path) {
    wait_for_file(path).await;
    let pids = std::fs::read_to_string(path)
        .unwrap()
        .split_whitespace()
        .map(|pid| pid.parse::<libc::pid_t>().unwrap())
        .collect::<Vec<_>>();
    let deadline = Instant::now() + Duration::from_secs(2);
    while pids.iter().any(|pid| unsafe { libc::kill(*pid, 0) == 0 }) && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        pids.iter().all(|pid| unsafe { libc::kill(*pid, 0) != 0 }),
        "persistent worker descendant survived: {pids:?}"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn persistent_worker_reuses_one_pid_and_keeps_bundles_independent() {
    let temp = tempfile::tempdir().unwrap();
    let (script, log, starts, _, startup) = persistent_fixture(temp.path(), "success");
    let config = persistent_config(temp.path());
    let worker = OfficialPersistentWorker::new(Some(script), config.clone()).unwrap();
    assert!(!starts.exists(), "persistent startup must be lazy");

    let first = worker
        .run(
            b"first",
            "pdf",
            persistent_request(temp.path(), &config, "first", "high"),
            Instant::now() + Duration::from_secs(10),
        )
        .await
        .unwrap();
    let first_markdown = std::fs::read_to_string(first.path().join("markdown.md")).unwrap();
    drop(first);

    let second = worker
        .run(
            b"second",
            "pdf",
            persistent_request(temp.path(), &config, "second", "xhigh"),
            Instant::now() + Duration::from_secs(10),
        )
        .await
        .unwrap();
    let second_markdown = std::fs::read_to_string(second.path().join("markdown.md")).unwrap();
    drop(second);
    worker.drain().await.unwrap();

    assert_eq!(first_markdown, "request-1\n");
    assert_eq!(second_markdown, "request-2\n");
    let startup: Value = serde_json::from_slice(&std::fs::read(startup).unwrap()).unwrap();
    assert_eq!(startup["type"], "start");
    assert_eq!(startup["protocol"], PERSISTENT_PROTOCOL);
    assert_eq!(startup["package_version"], PACKAGE_VERSION);
    assert_eq!(startup["schema_version"], SCHEMA_VERSION);
    assert_eq!(startup["backend"], PERSISTENT_BACKEND);
    assert_eq!(startup["model_stack"], "full");
    assert_eq!(
        startup["config"],
        temp.path().join("config.toml").to_str().unwrap()
    );
    assert_eq!(startup["vl_api_key"], "test-key");
    assert_eq!(startup["vl_model_name"], "test-model");
    assert_eq!(startup["capabilities"], persistent_capabilities());
    assert_eq!(std::fs::read_to_string(starts).unwrap(), "1");
    let entries = std::fs::read_to_string(log).unwrap();
    let entries = entries.lines().collect::<Vec<_>>();
    assert_eq!(entries.len(), 2);
    let first_parts = entries[0].split_whitespace().collect::<Vec<_>>();
    let second_parts = entries[1].split_whitespace().collect::<Vec<_>>();
    assert_eq!(first_parts[0], second_parts[0]);
    assert_eq!(first_parts[1..], ["first", "1"]);
    assert_eq!(second_parts[1..], ["second", "2"]);
}

#[cfg(unix)]
#[tokio::test]
async fn persistent_worker_restarts_only_after_crash_without_retrying_request() {
    let temp = tempfile::tempdir().unwrap();
    let (script, log, starts, _, _) = persistent_fixture(temp.path(), "crash-once");
    let config = persistent_config(temp.path());
    let worker = OfficialPersistentWorker::new(Some(script), config.clone()).unwrap();
    let first = worker
        .run(
            b"first",
            "pdf",
            persistent_request(temp.path(), &config, "crashed", "medium"),
            Instant::now() + Duration::from_secs(10),
        )
        .await;
    assert!(first.is_err());

    let second = worker
        .run(
            b"second",
            "pdf",
            persistent_request(temp.path(), &config, "recovered", "medium"),
            Instant::now() + Duration::from_secs(10),
        )
        .await
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(second.path().join("markdown.md")).unwrap(),
        "request-1\n"
    );
    drop(second);
    worker.shutdown().await.unwrap();

    assert_eq!(std::fs::read_to_string(starts).unwrap(), "2");
    let entries = std::fs::read_to_string(log).unwrap();
    let entries = entries.lines().collect::<Vec<_>>();
    assert_eq!(
        entries.len(),
        2,
        "the committed request must not be retried"
    );
    assert_ne!(
        entries[0].split_whitespace().next(),
        entries[1].split_whitespace().next()
    );
}

#[cfg(unix)]
#[tokio::test]
async fn persistent_worker_cancellation_kills_group_and_allows_new_session() {
    let temp = tempfile::tempdir().unwrap();
    let (script, log, starts, pids, _) = persistent_fixture(temp.path(), "hang-first");
    let config = persistent_config(temp.path());
    let worker = Arc::new(OfficialPersistentWorker::new(Some(script), config.clone()).unwrap());
    let task_worker = Arc::clone(&worker);
    let request = persistent_request(temp.path(), &config, "cancelled", "medium");
    let task = tokio::spawn(async move {
        task_worker
            .run(
                b"first",
                "pdf",
                request,
                Instant::now() + Duration::from_secs(30),
            )
            .await
    });
    wait_for_file(&pids).await;
    task.abort();
    assert!(
        matches!(task.await, Err(error) if error.is_cancelled()),
        "caller abort must cancel the request future"
    );
    assert_pids_dead(&pids).await;

    let second = worker
        .run(
            b"second",
            "pdf",
            persistent_request(temp.path(), &config, "after-cancel", "medium"),
            Instant::now() + Duration::from_secs(10),
        )
        .await
        .unwrap();
    drop(second);
    worker.shutdown().await.unwrap();
    assert_eq!(std::fs::read_to_string(starts).unwrap(), "2");
    let entries = std::fs::read_to_string(log).unwrap();
    let entries = entries.lines().collect::<Vec<_>>();
    assert_eq!(entries.len(), 2);
    assert_ne!(
        entries[0].split_whitespace().next(),
        entries[1].split_whitespace().next()
    );
}

#[cfg(unix)]
#[tokio::test]
async fn persistent_worker_drains_stderr_and_keeps_document_errors_reusable() {
    let temp = tempfile::tempdir().unwrap();
    let (script, _, starts, _, _) = persistent_fixture(temp.path(), "stderr-error-once");
    let config = persistent_config(temp.path());
    let worker = OfficialPersistentWorker::new(Some(script), config.clone()).unwrap();
    let first = worker
        .run(
            b"first",
            "pdf",
            persistent_request(temp.path(), &config, "document-error", "medium"),
            Instant::now() + Duration::from_secs(10),
        )
        .await;
    let first = match first {
        Err(error) => error,
        Ok(_) => panic!("document error fixture unexpectedly succeeded"),
    };
    assert!(first.contains("document failed"));
    assert!(first.contains("first-request-stderr"));

    let second = worker
        .run(
            b"second",
            "pdf",
            persistent_request(temp.path(), &config, "document-ok", "medium"),
            Instant::now() + Duration::from_secs(10),
        )
        .await
        .unwrap();
    drop(second);
    worker.shutdown().await.unwrap();
    assert_eq!(std::fs::read_to_string(starts).unwrap(), "1");
}

#[cfg(unix)]
#[tokio::test]
async fn persistent_worker_fails_closed_on_handshake_frame_and_protocol_errors() {
    for mode in ["bad-handshake", "bad-result", "oversize"] {
        let temp = tempfile::tempdir().unwrap();
        let (script, _, _, _, _) = persistent_fixture(temp.path(), mode);
        let config = persistent_config(temp.path());
        let worker = OfficialPersistentWorker::new(Some(script), config.clone()).unwrap();
        let result = worker
            .run(
                b"input",
                "pdf",
                persistent_request(temp.path(), &config, "bad", "medium"),
                Instant::now() + Duration::from_secs(10),
            )
            .await;
        assert!(result.is_err(), "mode={mode} must fail closed");
        worker.shutdown().await.unwrap();
    }
}

#[cfg(unix)]
#[tokio::test]
async fn persistent_worker_shutdown_reaps_an_active_owner() {
    let temp = tempfile::tempdir().unwrap();
    let (script, _, _, pids, _) = persistent_fixture(temp.path(), "hang-first");
    let config = persistent_config(temp.path());
    let worker = Arc::new(OfficialPersistentWorker::new(Some(script), config.clone()).unwrap());
    let task_worker = Arc::clone(&worker);
    let task = tokio::spawn(async move {
        task_worker
            .run(
                b"input",
                "pdf",
                persistent_request(Path::new("unused"), &config, "shutdown", "medium"),
                Instant::now() + Duration::from_secs(30),
            )
            .await
    });
    wait_for_file(&pids).await;
    worker.shutdown().await.unwrap();
    assert!(task.await.unwrap().is_err());
    assert_pids_dead(&pids).await;
}

#[cfg(unix)]
#[tokio::test]
async fn persistent_worker_drop_reaps_an_idle_owner() {
    let temp = tempfile::tempdir().unwrap();
    let (script, _, _, pids, _) = persistent_fixture(temp.path(), "success");
    let config = persistent_config(temp.path());
    let worker = OfficialPersistentWorker::new(Some(script), config.clone()).unwrap();
    let bundle = worker
        .run(
            b"input",
            "pdf",
            persistent_request(temp.path(), &config, "drop", "medium"),
            Instant::now() + Duration::from_secs(10),
        )
        .await
        .unwrap();
    drop(bundle);
    drop(worker);
    assert_pids_dead(&pids).await;
}
