use super::super::store::{OfficialFileStore, TaskFile, TaskRecord};
use super::payload;
use crate::official_api::wire::*;
use std::{sync::Arc, time::Duration};
use tokio::sync::{Mutex as AsyncMutex, Semaphore};

fn store() -> OfficialFileStore {
    OfficialFileStore::new(
        Arc::new(Semaphore::new(2)),
        Arc::new(Semaphore::new(1)),
        Duration::from_secs(3600),
    )
    .unwrap()
}

fn file(id: &str, name: &str, status: TaskFileState) -> TaskFile {
    TaskFile {
        file_id: id.into(),
        file_name: name.into(),
        declared: Some(4),
        actual: None,
        url: None,
        sha256sum: None,
        page_range: String::new(),
        tier: Tier::Standard,
        status,
        expired: false,
        uploaded: false,
        upload_active: false,
        submitted: false,
        started_at: None,
        finished_at: None,
        parse: None,
        error: None,
        data: None,
        upload_lock: Arc::new(AsyncMutex::new(())),
    }
}

fn record(store: &OfficialFileStore, files: Vec<TaskFile>) -> TaskRecord {
    TaskRecord {
        id: "job_1".into(),
        created_at: "2026-01-01T00:00:00Z".into(),
        finished_at: None,
        status: TaskStatus::WaitingFile,
        options: UploadTaskOptions {
            tier: None,
            ocr_mode: OcrMode::Auto,
            page_range: String::new(),
            output_formats: vec![TaskOutputFormat::Default],
        },
        expires_at: OfficialFileStore::now() + 3600,
        files,
        retention_at: None,
        error: None,
        running: false,
        _slot: store.shared_slots().clone().try_acquire_owned().unwrap(),
        _metadata: store.reserve_metadata(3).unwrap(),
        _artifact_budget: store.artifact_budget(1024).unwrap(),
    }
}

#[tokio::test]
async fn create_receipt_offers_uploads_only_while_waiting() {
    let store = store();
    let mut pending = file("file_b", "b.pdf", TaskFileState::Pending);
    pending.actual = Some(9);
    let record = record(
        &store,
        vec![file("file_a", "a.pdf", TaskFileState::WaitingFile), pending],
    );
    let shared = AsyncMutex::new(record);
    let created = payload::created(&shared, "http://host").await;
    assert_eq!(created.status, TaskStatus::WaitingFile);
    let first = &created.files[0];
    let upload = first.upload.as_ref().unwrap();
    assert_eq!(upload.url, "http://host/v1/tasks/job_1/uploads/file_a");
    assert_eq!(
        upload.headers.get("Content-Type").map(String::as_str),
        Some("application/octet-stream")
    );
    assert!(created.files[1].upload.is_none());
}

#[tokio::test]
async fn detail_reports_progress_start_and_result_url() {
    let store = store();
    let mut done = file("file_a", "a.pdf", TaskFileState::Completed);
    done.started_at = Some("2026-01-01T00:00:05Z".into());
    done.finished_at = Some("2026-01-01T00:00:09Z".into());
    done.parse = Some(FileParseInfo {
        tier: Tier::Flash,
        duration_ms: Some(4000),
        parser_version: Some("4.2.3".into()),
    });
    let mut failed = file("file_b", "b.pdf", TaskFileState::Failed);
    failed.started_at = Some("2026-01-01T00:00:02Z".into());
    let mut task = record(&store, vec![done, failed]);
    task.finished_at = Some("2026-01-01T00:00:10Z".into());
    store.aggregate(&mut task);
    let detail = payload::detail(&task, "");
    assert_eq!(detail.status, TaskStatus::Partial);
    assert_eq!(detail.started_at.as_deref(), Some("2026-01-01T00:00:02Z"));
    assert_eq!(detail.progress.completed, 1);
    assert_eq!(detail.progress.failed, 1);
    assert_eq!(detail.progress.total, 2);
    assert_eq!(
        detail.files[0].result_url.as_deref(),
        Some("/v1/files/file_a/content")
    );
    assert!(detail.files[0].parse.is_some());
    assert!(detail.files[1].result_url.is_none());
    assert!(detail.files[1].parse.is_none());
}

#[tokio::test]
async fn cancel_receipt_keeps_completed_files_and_marks_the_rest() {
    let store = store();
    let done = file("file_a", "a.pdf", TaskFileState::Completed);
    let waiting = file("file_b", "b.pdf", TaskFileState::WaitingFile);
    let mut task = record(&store, vec![done, waiting]);
    task.status = TaskStatus::Canceled;
    task.finished_at = Some("2026-01-01T00:00:10Z".into());
    task.files[1].status = TaskFileState::Canceled;
    let receipt = payload::canceled(&task, "http://host");
    assert_eq!(receipt.status, "canceled");
    assert_eq!(receipt.files.len(), 2);
    assert_eq!(receipt.files[0].status, TaskFileState::Completed);
    assert_eq!(
        receipt.files[0].result_url.as_deref(),
        Some("http://host/v1/files/file_a/content")
    );
    assert_eq!(receipt.files[1].status, TaskFileState::Canceled);
    assert!(receipt.files[1].result_url.is_none());
}
