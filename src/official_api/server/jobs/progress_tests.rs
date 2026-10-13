use super::*;
use axum::{
    Json,
    extract::{Path, State},
};
use std::sync::Arc;
use tokio::sync::Semaphore;

#[tokio::test]
async fn progress_tracks_terminal_file_transitions_once_through_cancel() {
    let (service, shared) = new_job().await;
    mark_file(&shared, 0, "completed", None).await;
    assert_progress(&shared, 1, 0).await;
    shared.lock().await.response.files[1].status = "running".into();
    assert_progress(&shared, 1, 0).await;
    mark_file(&shared, 1, "failed", Some("parse failed")).await;
    assert_progress(&shared, 1, 1).await;
    mark_file(&shared, 1, "failed", Some("parse failed")).await;
    assert_progress(&shared, 1, 1).await;
    shared.lock().await.cancel.store(true, Ordering::Release);
    finalize_job(&service.inner, &shared, 1, 1).await;
    finalize_job(&service.inner, &shared, 1, 1).await;

    let record = shared.lock().await;
    assert_eq!(record.response.status, "canceled");
    assert_eq!(record.response.files[0].status, "completed");
    assert_eq!(record.response.files[1].status, "failed");
    assert_eq!(record.response.files[2].status, "canceled");
    let response = serde_json::to_value(&record.response).unwrap();
    assert_eq!(response["progress"]["completed"].as_u64(), Some(1));
    assert_eq!(response["progress"]["failed"].as_u64(), Some(1));
}

#[tokio::test]
async fn cancellation_before_outcomes_and_failure_only_keep_status_counts() {
    let (service, shared) = new_job().await;
    let job_id = shared.lock().await.response.job_id.clone();
    let canceled = cancel(State(service.inner.clone()), Path(job_id))
        .await
        .unwrap()
        .0;
    assert_eq!(canceled.status, "canceled");
    assert_progress(&shared, 0, 0).await;

    let (service, shared) = new_job().await;
    shared.lock().await.cancel.store(true, Ordering::Release);
    finalize_job(&service.inner, &shared, 0, 0).await;
    assert_progress(&shared, 0, 0).await;
    assert!(
        shared
            .lock()
            .await
            .response
            .files
            .iter()
            .all(|file| file.status == "canceled")
    );

    let (service, shared) = new_job().await;
    mark_file(&shared, 0, "failed", Some("parse failed")).await;
    assert_progress(&shared, 0, 1).await;
    shared.lock().await.cancel.store(true, Ordering::Release);
    finalize_job(&service.inner, &shared, 0, 1).await;
    let record = shared.lock().await;
    assert_eq!(record.response.files[0].status, "failed");
    assert_eq!(record.response.files[1].status, "canceled");
    let response = serde_json::to_value(&record.response).unwrap();
    assert_eq!(response["progress"]["completed"].as_u64(), Some(0));
    assert_eq!(response["progress"]["failed"].as_u64(), Some(1));
}

async fn new_job() -> (
    crate::official_api::server::OfficialService,
    Arc<tokio::sync::Mutex<JobRecord>>,
) {
    let store = OfficialFileStore::new(
        Arc::new(Semaphore::new(2)),
        Arc::new(Semaphore::new(1)),
        std::time::Duration::from_secs(3600),
    )
    .unwrap();
    let service = crate::official_api::server::OfficialService::new(
        crate::official::OfficialRuntime::new(),
        store,
    )
    .unwrap();
    let request = serde_json::from_value(serde_json::json!({"files":[
        {"source":{"type":"inline","name":"one.pdf","data":"YQ=="}},
        {"source":{"type":"inline","name":"two.pdf","data":"Yg=="}},
        {"source":{"type":"inline","name":"three.pdf","data":"Yw=="}}
    ]}))
    .unwrap();
    let (_, response) = create(State(service.inner.clone()), Ok(Json(request)))
        .await
        .unwrap();
    let shared = service.inner.jobs.lock().unwrap()[&response.job_id].clone();
    (service, shared)
}

async fn assert_progress(shared: &Arc<tokio::sync::Mutex<JobRecord>>, completed: u64, failed: u64) {
    let record = shared.lock().await;
    let response = serde_json::to_value(&record.response).unwrap();
    assert_eq!(response["progress"]["completed"].as_u64(), Some(completed));
    assert_eq!(response["progress"]["failed"].as_u64(), Some(failed));
}
