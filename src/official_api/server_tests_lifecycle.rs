use super::{service, store as new_store};
use axum::{
    body::Body,
    extract::{Path, State},
    http::HeaderMap,
};
use serde_json::{Value, json};
use std::sync::atomic::Ordering;

async fn create(
    service: &super::super::service::OfficialService,
    body: Value,
) -> (String, Vec<(String, String)>) {
    let request = axum::http::Request::builder()
        .method("POST")
        .uri("/v1/tasks")
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    let response = super::super::tasks::create(State(service.inner.clone()), request)
        .await
        .unwrap();
    assert_eq!(response.status(), axum::http::StatusCode::ACCEPTED);
    let bytes = axum::body::to_bytes(response.into_body(), 64 * 1024)
        .await
        .unwrap();
    let value: Value = serde_json::from_slice(&bytes).unwrap();
    let task_id = value["task_id"].as_str().unwrap().to_owned();
    let files = value["files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|file| {
            (
                file["file_id"].as_str().unwrap().to_owned(),
                file["file_name"].as_str().unwrap().to_owned(),
            )
        })
        .collect();
    (task_id, files)
}

async fn upload(
    service: &super::super::service::OfficialService,
    task_id: &str,
    file_id: &str,
    body: &'static [u8],
) -> Result<Value, crate::official_api::wire::WireError> {
    let response = super::super::tasks::upload::put(
        State(service.inner.clone()),
        Path((task_id.to_owned(), file_id.to_owned())),
        HeaderMap::new(),
        Body::from(body),
    )
    .await?;
    Ok(serde_json::to_value(response.0).unwrap())
}

#[tokio::test]
async fn canceled_upload_releases_the_input_reservation() {
    let store = new_store();
    let service = service(store.clone());
    let (task_id, files) = create(
        &service,
        json!({"files": [{"file_name": "a.pdf", "bytes": 4}]}),
    )
    .await;
    let response = upload(&service, &task_id, &files[0].0, b"abcd")
        .await
        .unwrap();
    assert_eq!(response["status"], "pending");
    assert_eq!(store.inner.used.load(Ordering::Acquire), 4);
    let receipt = super::super::tasks::cancel(
        State(service.inner.clone()),
        Path(task_id.clone()),
        HeaderMap::new(),
    )
    .await
    .unwrap()
    .0;
    assert_eq!(receipt.status, "canceled");
    assert_eq!(store.inner.used.load(Ordering::Acquire), 0);
    let state = super::super::tasks::status(
        State(service.inner.clone()),
        Path(task_id),
        HeaderMap::new(),
    )
    .await
    .unwrap()
    .0;
    assert_eq!(
        state.status,
        crate::official_api::wire::TaskStatus::Canceled
    );
    assert!(
        state
            .files
            .iter()
            .all(|file| file.status == crate::official_api::wire::TaskFileState::Canceled)
    );
    service.shutdown().await.unwrap();
}

#[tokio::test]
async fn repeated_upload_of_a_completed_file_is_idempotent() {
    let store = new_store();
    let service = service(store.clone());
    let (task_id, files) = create(
        &service,
        json!({"files": [{"file_name": "a.pdf", "bytes": 4}]}),
    )
    .await;
    assert_eq!(
        upload(&service, &task_id, &files[0].0, b"abcd")
            .await
            .unwrap()["status"],
        "pending"
    );
    assert_eq!(
        upload(&service, &task_id, &files[0].0, b"abcd")
            .await
            .unwrap()["status"],
        "pending"
    );
    assert_eq!(store.inner.used.load(Ordering::Acquire), 4);
    service.shutdown().await.unwrap();
}

#[tokio::test]
async fn cancel_before_execution_prevents_publication() {
    let store = new_store();
    let service = service(store.clone());
    let (task_id, files) = create(
        &service,
        json!({"files": [
            {"file_name": "a.pdf", "bytes": 4},
            {"file_name": "b.pdf", "bytes": 4}
        ]}),
    )
    .await;
    upload(&service, &task_id, &files[0].0, b"abcd")
        .await
        .unwrap();
    let shared = store.lookup(&task_id).unwrap();
    {
        let mut task = shared.lock().await;
        super::super::jobs::cancel_task(&service.inner, &mut task);
    }
    assert!(
        super::super::jobs::output::publish_result(
            &service.inner,
            &shared,
            0,
            &crate::official::OfficialResult {
                middle: json!({"pages":[{}]}),
                model: None,
                markdown: "text".into(),
                structured: json!({}),
                assets: std::collections::BTreeMap::new(),
                warnings: Vec::new(),
                metadata: crate::official::OfficialVersionMetadata {
                    mineru_package: "test".into(),
                    schema_version: "test".into(),
                    bundle_name: "test".into(),
                },
            },
        )
        .await
        .is_err()
    );
    assert!(store.inner.files.lock().unwrap().is_empty());
    assert_eq!(store.inner.used.load(Ordering::Acquire), 0);
    assert_eq!(store.inner.artifact_used.load(Ordering::Acquire), 0);
    service.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 3)]
async fn shutdown_barrier_preserves_accepted_commit_and_rejects_later_tasks() {
    let store = new_store();
    let service = service(store.clone());
    let (runner_ready_tx, runner_ready_rx) = tokio::sync::oneshot::channel();
    let (runner_resume_tx, runner_resume_rx) = tokio::sync::oneshot::channel();
    *service.inner.runner_empty_pause.lock().unwrap() = Some((runner_ready_tx, runner_resume_rx));
    service.start().unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(3), runner_ready_rx)
        .await
        .unwrap()
        .unwrap();
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    let (resume_tx, resume_rx) = std::sync::mpsc::channel();
    *service.inner.commit_pause.lock().unwrap() = Some((ready_tx, resume_rx));
    let (stop_waiting_tx, stop_waiting_rx) = tokio::sync::oneshot::channel();
    *service.inner.stop_waiting.lock().unwrap() = Some(stop_waiting_tx);
    let inner = service.inner.clone();
    let create = tokio::spawn(async move {
        let request = axum::http::Request::builder()
            .method("POST")
            .uri("/v1/tasks")
            .header("content-type", "application/json")
            .body(Body::from(
                json!({"files": [{"file_name": "a.pdf", "bytes": 1}]}).to_string(),
            ))
            .unwrap();
        super::super::tasks::create(State(inner), request).await
    });
    tokio::time::timeout(
        std::time::Duration::from_secs(3),
        tokio::task::spawn_blocking(move || ready_rx.recv().unwrap()),
    )
    .await
    .unwrap()
    .unwrap();
    let stopping = service.clone();
    let shutdown = tokio::spawn(async move { stopping.shutdown().await });
    tokio::time::timeout(std::time::Duration::from_secs(3), stop_waiting_rx)
        .await
        .unwrap()
        .unwrap();
    assert!(!service.inner.stopping.load(Ordering::Acquire));
    resume_tx.send(()).unwrap();
    let response = create.await.unwrap().unwrap();
    assert_eq!(response.status(), axum::http::StatusCode::ACCEPTED);
    runner_resume_tx.send(()).unwrap();
    shutdown.await.unwrap().unwrap();
    let error = {
        let request = axum::http::Request::builder()
            .method("POST")
            .uri("/v1/tasks")
            .header("content-type", "application/json")
            .body(Body::from(
                json!({"files": [{"file_name": "late.pdf", "bytes": 1}]}).to_string(),
            ))
            .unwrap();
        super::super::tasks::create(State(service.inner.clone()), request)
            .await
            .unwrap_err()
    };
    assert_eq!(error.status, axum::http::StatusCode::SERVICE_UNAVAILABLE);
    assert!(service.inner.queue.lock().unwrap().is_empty());
    assert_eq!(service.inner.jobs_created.load(Ordering::Acquire), 1);
    assert_eq!(store.inner.used.load(Ordering::Acquire), 0);

    let store2 = new_store();
    store2.stop_admissions();
    assert!(!store2.inner.accepting.load(Ordering::Acquire));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 3)]
async fn shutdown_while_waiting_for_gate_does_not_need_the_external_permit() {
    let store = new_store();
    let gate = store.shared_gate().clone();
    let held: Vec<_> = (0..gate.available_permits())
        .map(|_| gate.clone().try_acquire_owned().unwrap())
        .collect();
    assert!(!held.is_empty());
    let service = service(store.clone());
    let (task_id, files) = create(
        &service,
        json!({"files": [{"file_name": "a.pdf", "bytes": 4}]}),
    )
    .await;
    upload(&service, &task_id, &files[0].0, b"abcd")
        .await
        .unwrap();
    service.start().unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    tokio::time::timeout(std::time::Duration::from_secs(2), service.shutdown())
        .await
        .expect("shutdown must not wait for the external gate permit")
        .unwrap();
    assert_eq!(store.inner.used.load(Ordering::Acquire), 0);
    drop(held);
}

#[path = "server_tests_lifecycle_limits.rs"]
mod limits;
