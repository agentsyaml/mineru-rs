use super::{files, store::OfficialFileStore, uploads};
use crate::official_api::wire::{Completion, UploadCreate};
use axum::{
    Json,
    body::Body,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
};
use std::sync::Arc;
use tokio::sync::Semaphore;

fn store() -> OfficialFileStore {
    OfficialFileStore::new(
        Arc::new(Semaphore::new(2)),
        Arc::new(Semaphore::new(1)),
        std::time::Duration::from_secs(3600),
    )
    .unwrap()
}

fn job_request(files: serde_json::Value) -> crate::official_api::wire::JobCreate {
    serde_json::from_value(serde_json::json!({"files": files})).unwrap()
}

fn service(store: OfficialFileStore) -> super::service::OfficialService {
    super::service::OfficialService::new(crate::official::OfficialRuntime::new(), store).unwrap()
}

#[tokio::test]
async fn upload_complete_source_forbidden_and_delete_keeps_lease_charged() {
    let store = store();
    let request = UploadCreate {
        filename: "doc.pdf".into(),
        bytes: 3,
        mime_type: "application/pdf".into(),
        purpose: "parse".into(),
        sha256sum: None,
        expires_after: None,
    };
    let created = uploads::create(State(store.clone()), Ok(Json(request)))
        .await
        .unwrap()
        .0;
    let put = uploads::put(
        State(store.clone()),
        Path(created.id.clone()),
        HeaderMap::new(),
        Body::from("abc"),
    )
    .await
    .unwrap();
    assert_eq!(put.into_response().status(), StatusCode::OK);
    let completed = uploads::complete(
        State(store.clone()),
        Path(created.id),
        Ok(Some(Json(Completion { sha256sum: None }))),
    )
    .await
    .unwrap()
    .0;
    let file_id = completed.file.unwrap().id;
    assert_eq!(
        uploads::complete(State(store.clone()), Path(completed.id.clone()), Ok(None))
            .await
            .unwrap()
            .0
            .file
            .unwrap()
            .id,
        file_id
    );
    let terminal = uploads::cancel(State(store.clone()), Path(completed.id))
        .await
        .unwrap_err();
    assert_eq!(terminal.status, StatusCode::CONFLICT);
    let denied = files::content(State(store.clone()), Path(file_id.clone()))
        .await
        .unwrap_err();
    assert_eq!(denied.status, StatusCode::FORBIDDEN);
    let lease = store.input_lease(&file_id).unwrap().1;
    let deleted = files::delete(State(store.clone()), Path(file_id.clone()))
        .await
        .unwrap();
    assert!(deleted.0.deleted);
    assert!(store.input_lease(&file_id).is_none());
    assert_eq!(
        store.inner.used.load(std::sync::atomic::Ordering::Acquire),
        3
    );
    drop(lease);
    assert_eq!(
        store.inner.used.load(std::sync::atomic::Ordering::Acquire),
        0
    );
}

#[tokio::test]
async fn bad_upload_length_rolls_back_reservation() {
    let store = store();
    let req = UploadCreate {
        filename: "x.pdf".into(),
        bytes: 4,
        mime_type: "application/pdf".into(),
        purpose: "parse".into(),
        sha256sum: None,
        expires_after: None,
    };
    let upload = uploads::create(State(store.clone()), Ok(Json(req)))
        .await
        .unwrap()
        .0;
    let error = match uploads::put(
        State(store.clone()),
        Path(upload.id),
        HeaderMap::new(),
        Body::from("abc"),
    )
    .await
    {
        Err(error) => error,
        Ok(_) => panic!("short body should fail"),
    };
    assert_eq!(error.status, StatusCode::BAD_REQUEST);
    assert_eq!(
        store.inner.used.load(std::sync::atomic::Ordering::Acquire),
        0
    );
}

#[tokio::test]
async fn upload_hash_mismatch_rolls_back_once() {
    let store = store();
    let req = UploadCreate {
        filename: "x.pdf".into(),
        bytes: 3,
        mime_type: "application/pdf".into(),
        purpose: "parse".into(),
        sha256sum: Some("0".repeat(64)),
        expires_after: None,
    };
    let upload = uploads::create(State(store.clone()), Ok(Json(req)))
        .await
        .unwrap()
        .0;
    let error = match uploads::put(
        State(store.clone()),
        Path(upload.id),
        HeaderMap::new(),
        Body::from("abc"),
    )
    .await
    {
        Err(error) => error,
        Ok(_) => panic!("hash mismatch should fail"),
    };
    assert_eq!(error.status, StatusCode::BAD_REQUEST);
    assert_eq!(
        store.inner.used.load(std::sync::atomic::Ordering::Acquire),
        0
    );
}

#[tokio::test]
async fn expired_reservation_released_and_saturation_rejected() {
    let store = store();
    let req = UploadCreate {
        filename: "x.pdf".into(),
        bytes: 4,
        mime_type: "application/pdf".into(),
        purpose: "parse".into(),
        sha256sum: None,
        expires_after: None,
    };
    let upload = uploads::create(State(store.clone()), Ok(Json(req.clone())))
        .await
        .unwrap()
        .0;
    if let Some(super::store::Entry::Upload(u)) =
        store.inner.entries.lock().unwrap().get_mut(&upload.id)
    {
        u.expires = 0;
    }
    store.cleanup_expired();
    assert_eq!(
        store.inner.used.load(std::sync::atomic::Ordering::Acquire),
        0
    );
    store
        .inner
        .used
        .store(256 * 1024 * 1024, std::sync::atomic::Ordering::Release);
    let error = match uploads::create(State(store.clone()), Ok(Json(req))).await {
        Err(error) => error,
        Ok(_) => panic!("saturated pool should reject"),
    };
    assert_eq!(error.status, StatusCode::TOO_MANY_REQUESTS);
}

#[tokio::test]
async fn queued_job_cancel_is_terminal_and_releases_input_before_history_expiry() {
    let store = store();
    let service = service(store.clone());
    let request = job_request(serde_json::json!([
        {"source":{"type":"inline","name":"a.pdf","data":"YQ=="}}
    ]));
    let (status, response) = super::jobs::create(State(service.inner.clone()), Ok(Json(request)))
        .await
        .unwrap();
    assert_eq!(status, StatusCode::ACCEPTED);
    let canceled = super::jobs::cancel(State(service.inner.clone()), Path(response.job_id.clone()))
        .await
        .unwrap()
        .0;
    assert_eq!(canceled.status, "canceled");
    assert_eq!(
        store.inner.used.load(std::sync::atomic::Ordering::Acquire),
        0
    );
    assert!(
        service.inner.jobs.lock().unwrap()[&response.job_id]
            .try_lock()
            .unwrap()
            .files
            .is_empty()
    );
    assert_eq!(
        super::jobs::get(State(service.inner.clone()), Path(response.job_id.clone()))
            .await
            .unwrap()
            .0
            .status,
        "canceled"
    );
    service.shutdown().await.unwrap();
}

#[path = "server_tests_lifecycle.rs"]
mod lifecycle;
