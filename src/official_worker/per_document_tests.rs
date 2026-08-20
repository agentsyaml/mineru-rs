use std::time::{Duration, Instant};

use super::{OfficialRequest, OfficialWorker};

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
    let request = OfficialRequest::new(
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
    );
    let task = tokio::spawn(async move {
        worker
            .run(
                b"input",
                "pdf",
                request,
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
