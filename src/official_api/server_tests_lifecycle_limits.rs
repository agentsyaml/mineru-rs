use super::super::{service, store_with_slots};
use crate::official_api::wire::{TaskFileState, TaskStatus, WireError, code};
use axum::{
    body::Body,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
};
use serde_json::{Value, json};
use std::sync::atomic::Ordering;

async fn create(
    service: &super::super::super::service::OfficialService,
    body: Value,
) -> Result<(String, String), WireError> {
    let request = axum::http::Request::builder()
        .method("POST")
        .uri("/v1/tasks")
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    let response =
        super::super::super::tasks::create(State(service.inner.clone()), request).await?;
    let bytes = axum::body::to_bytes(response.into_body(), 64 * 1024)
        .await
        .unwrap();
    let value: Value = serde_json::from_slice(&bytes).unwrap();
    Ok((
        value["task_id"].as_str().unwrap().to_owned(),
        value["files"][0]["file_id"].as_str().unwrap().to_owned(),
    ))
}

async fn upload(
    service: &super::super::super::service::OfficialService,
    task_id: &str,
    file_id: &str,
    headers: HeaderMap,
    body: &'static [u8],
) -> Result<Value, WireError> {
    let response = super::super::super::tasks::upload::put(
        State(service.inner.clone()),
        Path((task_id.to_owned(), file_id.to_owned())),
        headers,
        Body::from(body),
    )
    .await?;
    Ok(serde_json::to_value(response.0).unwrap())
}

async fn pdf_task(
    service: &super::super::super::service::OfficialService,
    bytes: u64,
) -> (String, String) {
    create(
        service,
        json!({"files": [{"file_name": "a.pdf", "bytes": bytes}]}),
    )
    .await
    .unwrap()
}

#[tokio::test]
async fn upload_rejects_short_long_empty_and_mismatched_bodies_without_leaking() {
    let store = store_with_slots(8);
    let service = service(store.clone());
    for (declared, body, expected) in [
        (4u64, &b"abc"[..], code::UPLOAD_SIZE_MISMATCH),
        (2, &b"abc"[..], code::UPLOAD_SIZE_MISMATCH),
        (0, &b""[..], "invalid_request"),
    ] {
        let (task_id, file_id) = pdf_task(&service, declared.max(1)).await;
        let error = upload(&service, &task_id, &file_id, HeaderMap::new(), body)
            .await
            .unwrap_err();
        assert_eq!(error.code, expected, "declared {declared}");
        assert_eq!(error.status, StatusCode::BAD_REQUEST);
        assert_eq!(store.inner.used.load(Ordering::Acquire), 0);
        let state = super::super::super::tasks::status(
            State(service.inner.clone()),
            Path(task_id),
            HeaderMap::new(),
        )
        .await
        .unwrap()
        .0;
        assert_eq!(state.status, TaskStatus::WaitingFile);
        assert_eq!(state.files[0].status, TaskFileState::WaitingFile);
    }
}

#[tokio::test]
async fn upload_checks_content_length_and_declared_hash() {
    let store = store_with_slots(8);
    let service = service(store.clone());
    let (task_id, file_id) = create(
        &service,
        json!({"files": [{"file_name": "a.pdf", "bytes": 4, "sha256sum": "0".repeat(64)}]}),
    )
    .await
    .unwrap();
    let mut headers = HeaderMap::new();
    headers.insert("content-length", "3".parse().unwrap());
    let error = upload(&service, &task_id, &file_id, headers, b"abc")
        .await
        .unwrap_err();
    assert_eq!(error.code, code::UPLOAD_SIZE_MISMATCH);
    let error = upload(&service, &task_id, &file_id, HeaderMap::new(), b"abcd")
        .await
        .unwrap_err();
    assert_eq!(error.code, code::FILE_HASH_MISMATCH);
    assert_eq!(store.inner.used.load(Ordering::Acquire), 0);
    let state = super::super::super::tasks::status(
        State(service.inner.clone()),
        Path(task_id),
        HeaderMap::new(),
    )
    .await
    .unwrap()
    .0;
    assert!(state.files[0].upload.is_some());
}

#[tokio::test]
async fn empty_and_unknown_uploads_are_rejected() {
    let store = store_with_slots(8);
    let service = service(store);
    let (task_id, file_id) = pdf_task(&service, 4).await;
    let error = super::super::super::tasks::upload::put(
        State(service.inner.clone()),
        Path((task_id.clone(), "file_missing".into())),
        HeaderMap::new(),
        Body::from("abcd"),
    )
    .await
    .unwrap_err();
    assert_eq!(error.status, StatusCode::NOT_FOUND);
    let error = super::super::super::tasks::upload::put(
        State(service.inner.clone()),
        Path(("job_missing".into(), file_id)),
        HeaderMap::new(),
        Body::from("abcd"),
    )
    .await
    .unwrap_err();
    assert_eq!(error.status, StatusCode::NOT_FOUND);
    let error = super::super::super::tasks::cancel(
        State(service.inner.clone()),
        Path(task_id),
        HeaderMap::new(),
    )
    .await;
    assert!(error.is_ok());
}

#[tokio::test]
async fn url_inputs_reject_uploads_and_never_show_upload_links() {
    let store = store_with_slots(8);
    let service = service(store);
    let (task_id, file_id) = create(
        &service,
        json!({"files": [{"file_name": "a.pdf", "url": "https://example.test/a.pdf"}]}),
    )
    .await
    .unwrap();
    let state = super::super::super::tasks::status(
        State(service.inner.clone()),
        Path(task_id.clone()),
        HeaderMap::new(),
    )
    .await
    .unwrap()
    .0;
    assert!(state.files[0].upload.is_none());
    assert_eq!(
        state.files[0].url.as_deref(),
        Some("https://example.test/a.pdf")
    );
    let error = upload(&service, &task_id, &file_id, HeaderMap::new(), b"abcd")
        .await
        .unwrap_err();
    assert_eq!(error.status, StatusCode::BAD_REQUEST);
    assert_eq!(error.message, "URL inputs do not accept uploads");
}

#[tokio::test]
async fn create_enforces_json_transport_and_query_rules() {
    let store = store_with_slots(8).with_transport_limits(64 * 1024, 64 * 1024, 64 * 1024);
    let service = service(store.clone());
    let request = axum::http::Request::builder()
        .method("POST")
        .uri("/v1/tasks?tier=flash")
        .header("content-type", "application/json")
        .body(Body::from("{}"))
        .unwrap();
    let error = super::super::super::tasks::create(State(service.inner.clone()), request)
        .await
        .unwrap_err();
    assert_eq!(error.status, StatusCode::BAD_REQUEST);
    assert!(error.message.contains("JSON body"));

    let request = axum::http::Request::builder()
        .method("POST")
        .uri("/v1/tasks")
        .header("content-type", "multipart/form-data")
        .body(Body::from("{}"))
        .unwrap();
    let error = super::super::super::tasks::create(State(service.inner.clone()), request)
        .await
        .unwrap_err();
    assert_eq!(error.status, StatusCode::UNSUPPORTED_MEDIA_TYPE);

    // Upstream compares the declared size against the received upload, never at
    // create time, so an over-cap declaration is accepted here.
    let (task_id, _) = create(
        &service,
        json!({"files": [{"file_name": "a.pdf", "bytes": 64 * 1024 + 1}]}),
    )
    .await
    .unwrap();
    assert!(task_id.starts_with("job_"));
}

#[tokio::test]
async fn terminal_tasks_reject_cancel_and_expired_uploads_fail_the_file() {
    let store = store_with_slots(8);
    let service = service(store.clone());
    let (task_id, file_id) = pdf_task(&service, 4).await;
    let receipt = super::super::super::tasks::cancel(
        State(service.inner.clone()),
        Path(task_id.clone()),
        HeaderMap::new(),
    )
    .await
    .unwrap()
    .0;
    assert_eq!(receipt.status, "canceled");
    let error = super::super::super::tasks::cancel(
        State(service.inner.clone()),
        Path(task_id.clone()),
        HeaderMap::new(),
    )
    .await
    .unwrap_err();
    assert_eq!(error.status, StatusCode::CONFLICT);
    assert_eq!(error.code, code::TASK_ALREADY_TERMINAL);

    let (task_id, file_id2) = pdf_task(&service, 4).await;
    if let Some(task) = store.lookup(&task_id) {
        task.lock().await.expires_at = 0;
    }
    let error = upload(&service, &task_id, &file_id2, HeaderMap::new(), b"abcd")
        .await
        .unwrap_err();
    assert_eq!(error.status, StatusCode::GONE);
    assert_eq!(error.code, "upload_expired");
    let state = super::super::super::tasks::status(
        State(service.inner.clone()),
        Path(task_id),
        HeaderMap::new(),
    )
    .await
    .unwrap()
    .0;
    assert_eq!(state.files[0].status, TaskFileState::Failed);
    assert_eq!(
        state.files[0].error.as_ref().unwrap().code,
        "upload_expired"
    );
    let _ = file_id;
}
