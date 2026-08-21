#[cfg(unix)]
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use super::{
    OfficialRequest, OfficialWorker, PYTHON_SHIM, PythonShim, STDERR_CAP,
    process::with_truncated_diagnostic, read_diagnostic,
};

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

#[test]
fn python_shim_materializes_and_cleans_up() {
    let shim = PythonShim::new().unwrap();
    let path = shim.path().to_owned();
    let directory = path.parent().unwrap().to_owned();

    assert_eq!(std::fs::read(&path).unwrap(), PYTHON_SHIM.as_bytes());
    assert!(path.is_file());
    assert!(directory.is_dir());

    drop(shim);
    assert!(!directory.exists());
}

#[tokio::test]
async fn stderr_reader_drains_and_retains_a_bounded_prefix() {
    use tokio::io::AsyncWriteExt;

    let payload = vec![b'x'; STDERR_CAP + 1];
    let (mut writer, reader) = tokio::io::duplex(payload.len() + 1);
    writer.write_all(&payload).await.unwrap();
    writer.shutdown().await.unwrap();

    let diagnostic = read_diagnostic(reader, STDERR_CAP).await.unwrap();
    assert_eq!(diagnostic.bytes.as_slice(), &payload[..STDERR_CAP]);
    assert!(diagnostic.truncated);
}

#[test]
fn diagnostic_sanitization_handles_invalid_utf8() {
    let raw = b"prefix-\xff-suffix";
    let error = with_truncated_diagnostic("worker failed".into(), Some(raw), false);

    assert!(error.contains("prefix-�-suffix"));
    assert!(error.len() < "worker failed: ".len() + STDERR_CAP);
}

#[test]
fn truncated_diagnostic_has_one_marker_and_exact_bound() {
    let raw = vec![b'x'; STDERR_CAP];
    let error = with_truncated_diagnostic("worker failed".into(), Some(&raw), true);
    let diagnostic = error.strip_prefix("worker failed: ").unwrap();

    assert_eq!(diagnostic.len(), STDERR_CAP);
    assert_eq!(diagnostic.matches(" [truncated]").count(), 1);
    assert!(diagnostic.ends_with(" [truncated]"));
}

#[cfg(unix)]
fn executable_script(path: &Path, source: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;

    std::fs::write(path, source).unwrap();
    let mut permissions = std::fs::metadata(path).unwrap().permissions();
    permissions.set_mode(0o700);
    std::fs::set_permissions(path, permissions).unwrap();
    path.to_owned()
}

#[cfg(unix)]
#[tokio::test]
async fn verbose_stderr_does_not_reject_a_valid_response() {
    let temp = tempfile::tempdir().unwrap();
    let script = executable_script(
        &temp.path().join("verbose-worker"),
        r#"#!/bin/sh
request=$(cat)
request_id=$(printf '%s' "$request" | python3 -c 'import json,sys; print(json.load(sys.stdin)["request_id"])')
python3 -c 'import sys; sys.stderr.buffer.write(b"x" * 65537 + bytes([255]))'
printf '{"protocol":"mineru-rs-official-worker/1","request_id":"%s","status":"ok","package_version":"4.0.0a6","schema_version":"1.0","backend":"hybrid-http-client","bundle_name":"hybrid-v4","error":null}\n' "$request_id"
"#,
    );
    let worker = OfficialWorker::new(Some(script)).unwrap();

    let result = worker
        .run(
            b"input",
            "pdf",
            test_request(),
            Instant::now() + Duration::from_secs(10),
        )
        .await;

    assert!(result.is_ok(), "verbose stderr rejected valid response");
}

#[cfg(unix)]
#[tokio::test]
async fn caller_abort_kills_and_reaps_worker_descendants() {
    use std::os::unix::fs::PermissionsExt;

    let temp = tempfile::tempdir().unwrap();
    let pid_file = temp.path().join("worker-pids");
    let script = temp.path().join("hanging-worker");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\nsleep 30 &\nbackground=$!\nprintf '%s %s' \"$$\" \"$background\" > \"{}\"\ncat >/dev/null\nsleep 30\n",
            pid_file.display()
        ),
    )
    .unwrap();
    let mut permissions = std::fs::metadata(&script).unwrap().permissions();
    permissions.set_mode(0o700);
    std::fs::set_permissions(&script, permissions).unwrap();

    let worker = OfficialWorker::new(Some(script)).unwrap();
    let task = tokio::spawn(async move {
        worker
            .run(
                b"input",
                "pdf",
                test_request(),
                Instant::now() + Duration::from_secs(60),
            )
            .await
    });
    let wait_until = Instant::now() + Duration::from_secs(10);
    while !pid_file.exists() && Instant::now() < wait_until {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let pids = std::fs::read_to_string(&pid_file).expect("worker did not start");
    task.abort();
    let join = task.await;
    assert!(matches!(join, Err(error) if error.is_cancelled()));

    let pids = pids
        .split_whitespace()
        .map(|pid| pid.parse::<libc::pid_t>().unwrap())
        .collect::<Vec<_>>();
    let reap_deadline = Instant::now() + Duration::from_secs(10);
    while pids.iter().any(|pid| unsafe { libc::kill(*pid, 0) == 0 })
        && Instant::now() < reap_deadline
    {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        pids.iter().all(|pid| unsafe { libc::kill(*pid, 0) != 0 }),
        "worker or descendant survived caller cancellation: {pids:?}"
    );
}
