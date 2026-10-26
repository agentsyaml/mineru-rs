use super::*;

pub(super) fn runtime_with_missing_python() -> (OfficialRuntime, tempfile::TempDir) {
    let directory = tempfile::tempdir().unwrap();
    let runtime = OfficialRuntime::new().with_python(directory.path().join("missing-python"));
    (runtime, directory)
}

#[tokio::test]
async fn oversized_non_csv_path_is_rejected_before_worker_start() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("large.pdf");
    std::fs::File::create(&path)
        .unwrap()
        .set_len(crate::official::runtime::INPUT_BYTE_CAP as u64 + 1)
        .unwrap();
    let (runtime, _python_dir) = runtime_with_missing_python();
    let parser = OfficialParser::new(runtime).unwrap();
    let error = parser
        .parse_rich(OfficialInput::path(path), OfficialParseOptions::new())
        .await
        .unwrap_err();
    assert_eq!(error.kind, OfficialErrorKind::LimitExceeded);
}

#[tokio::test]
async fn non_csv_input_acquisition_obeys_runtime_deadline() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("slow.pdf");
    std::fs::File::create(&path)
        .unwrap()
        .set_len(1024 * 1024)
        .unwrap();
    let parser = OfficialParser::new(
        OfficialRuntime::new().with_timeout(std::time::Duration::from_nanos(1)),
    )
    .unwrap();
    let error = parser
        .parse_rich(OfficialInput::path(path), OfficialParseOptions::new())
        .await
        .unwrap_err();
    assert_eq!(error.kind, OfficialErrorKind::Deadline);
}

#[test]
fn aborting_queued_acquisition_prevents_the_blocking_read_from_starting() {
    use std::sync::{
        Arc, Barrier,
        atomic::{AtomicBool, Ordering},
    };

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .max_blocking_threads(1)
        .build()
        .unwrap();
    runtime.block_on(async {
        let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
        let release = Arc::new(Barrier::new(2));
        let worker_release = release.clone();
        let blocker = tokio::task::spawn_blocking(move || {
            entered_tx.send(()).unwrap();
            worker_release.wait();
        });
        entered_rx.await.unwrap();

        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("queued.pdf");
        std::fs::write(&path, b"input").unwrap();
        let started = Arc::new(AtomicBool::new(false));
        let worker_started = started.clone();
        let acquisition = tokio::spawn(OfficialInput::path(path).acquire_limited_with_start(
            1024,
            std::time::Instant::now() + std::time::Duration::from_secs(10),
            move || worker_started.store(true, Ordering::Relaxed),
        ));
        tokio::task::yield_now().await;
        acquisition.abort();
        assert!(acquisition.await.unwrap_err().is_cancelled());
        release.wait();
        blocker.await.unwrap();
        tokio::task::spawn_blocking(|| {}).await.unwrap();
        assert!(!started.load(Ordering::Relaxed));
    });
}

#[test]
fn timed_out_queued_acquisition_prevents_the_blocking_read_from_starting() {
    use std::sync::{
        Arc, Barrier,
        atomic::{AtomicBool, Ordering},
    };

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .max_blocking_threads(1)
        .build()
        .unwrap();
    runtime.block_on(async {
        let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
        let release = Arc::new(Barrier::new(2));
        let worker_release = release.clone();
        let blocker = tokio::task::spawn_blocking(move || {
            entered_tx.send(()).unwrap();
            worker_release.wait();
        });
        entered_rx.await.unwrap();
        let started = Arc::new(AtomicBool::new(false));
        let worker_started = started.clone();
        let acquisition = tokio::spawn(
            OfficialInput::path("not-opened.pdf").acquire_limited_with_start(
                1024,
                std::time::Instant::now() + std::time::Duration::from_millis(50),
                move || worker_started.store(true, Ordering::Relaxed),
            ),
        );
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert_eq!(
            acquisition.await.unwrap().unwrap_err().kind,
            OfficialErrorKind::Deadline
        );
        release.wait();
        blocker.await.unwrap();
        tokio::task::spawn_blocking(|| {}).await.unwrap();
        assert!(!started.load(Ordering::Relaxed));
    });
}
