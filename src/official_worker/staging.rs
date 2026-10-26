use std::{
    io::Write,
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Instant,
};
use tempfile::TempDir;

#[derive(Debug)]
pub(super) struct WorkerStage {
    pub(super) temporary: TempDir,
    pub(super) input_path: std::path::PathBuf,
    pub(super) operation_assets: Option<std::path::PathBuf>,
}

pub(super) async fn stage_input(
    input: bytes::Bytes,
    suffix: &str,
    files: Vec<(String, Vec<u8>)>,
    operation: bool,
    deadline: Instant,
) -> Result<WorkerStage, String> {
    stage_input_with_start(input, suffix, files, operation, deadline, |_| {}).await
}

async fn stage_input_with_start(
    input: bytes::Bytes,
    suffix: &str,
    files: Vec<(String, Vec<u8>)>,
    operation: bool,
    deadline: Instant,
    on_start: impl FnOnce(std::path::PathBuf) + Send + 'static,
) -> Result<WorkerStage, String> {
    if Instant::now() >= deadline {
        return Err("official worker input staging deadline expired".into());
    }
    let suffix = suffix.to_owned();
    let cancelled = Arc::new(AtomicBool::new(false));
    let worker_cancelled = cancelled.clone();
    let mut cancel_guard = StageCancel(Some(cancelled));
    let task = tokio::task::spawn_blocking(move || {
        let temporary = tempfile::tempdir().map_err(|e| format!("official worker tempdir: {e}"))?;
        on_start(temporary.path().to_owned());
        let input_path = temporary.path().join(format!("input.{suffix}"));
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&input_path)
            .map_err(|e| format!("official worker input snapshot: {e}"))?;
        write_stage_file(&mut file, &input, &worker_cancelled, deadline)
            .and_then(|_| file.flush())
            .map_err(|e| format!("official worker input snapshot: {e}"))?;
        let operation_assets = if !operation && files.is_empty() {
            None
        } else {
            let root = temporary.path().join("operation-assets");
            std::fs::create_dir_all(&root)
                .map_err(|e| format!("operation asset directory: {e}"))?;
            for (name, bytes) in files {
                ensure_stage_active(&worker_cancelled, deadline)
                    .map_err(|e| format!("operation asset write: {e}"))?;
                let relative = Path::new(&name);
                if relative.is_absolute()
                    || relative
                        .components()
                        .any(|p| !matches!(p, std::path::Component::Normal(_)))
                {
                    return Err("unsafe private operation asset path".into());
                }
                let target = root.join(relative);
                if let Some(parent) = target.parent() {
                    std::fs::create_dir_all(parent)
                        .map_err(|e| format!("operation asset directory: {e}"))?;
                }
                let mut target = std::fs::File::create(target)
                    .map_err(|e| format!("operation asset write: {e}"))?;
                write_stage_file(&mut target, &bytes, &worker_cancelled, deadline)
                    .map_err(|e| format!("operation asset write: {e}"))?;
            }
            Some(root)
        };
        Ok(WorkerStage {
            temporary,
            input_path,
            operation_assets,
        })
    });
    let result = tokio::time::timeout_at(tokio::time::Instant::from_std(deadline), task)
        .await
        .map_err(|_| "official worker input staging deadline expired".to_owned())?
        .map_err(|e| format!("official worker input staging task failed: {e}"))?;
    cancel_guard.disarm();
    result
}

struct StageCancel(Option<Arc<AtomicBool>>);

impl StageCancel {
    fn disarm(&mut self) {
        self.0 = None;
    }
}

impl Drop for StageCancel {
    fn drop(&mut self) {
        if let Some(cancelled) = self.0.take() {
            cancelled.store(true, Ordering::Relaxed);
        }
    }
}

fn ensure_stage_active(cancelled: &AtomicBool, deadline: Instant) -> std::io::Result<()> {
    if cancelled.load(Ordering::Relaxed) || Instant::now() >= deadline {
        return Err(std::io::Error::new(
            std::io::ErrorKind::Interrupted,
            "staging cancelled or expired",
        ));
    }
    Ok(())
}

fn write_stage_file(
    file: &mut std::fs::File,
    bytes: &[u8],
    cancelled: &AtomicBool,
    deadline: Instant,
) -> std::io::Result<()> {
    for chunk in bytes.chunks(64 * 1024) {
        ensure_stage_active(cancelled, deadline)?;
        file.write_all(chunk)?;
    }
    Ok(())
}

#[cfg(test)]
#[tokio::test]
async fn caller_abort_signals_stager_before_tempdir_cleanup() {
    let barrier = Arc::new(std::sync::Barrier::new(2));
    let worker_barrier = barrier.clone();
    let (started_tx, started_rx) = std::sync::mpsc::channel();
    let task = tokio::spawn(stage_input_with_start(
        bytes::Bytes::from_static(b"input"),
        "pdf",
        Vec::new(),
        false,
        Instant::now() + std::time::Duration::from_secs(10),
        move |path| {
            started_tx.send(path).unwrap();
            worker_barrier.wait();
        },
    ));
    let path = tokio::task::spawn_blocking(move || started_rx.recv().unwrap())
        .await
        .unwrap();
    assert!(path.is_dir());
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    tokio::task::spawn_blocking(move || barrier.wait())
        .await
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while path.exists() {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("cancelled staging task did not clean up its temporary directory");
}
