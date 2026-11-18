use super::{files, store::OfficialFileStore};
use crate::official_api::wire::{FileObjectModel, FilePurpose};
use axum::{
    extract::{Path, State},
    http::StatusCode,
};
use std::{sync::Arc, time::Duration};
use tokio::sync::Semaphore;

pub(super) fn store() -> OfficialFileStore {
    store_with_slots(2)
}

pub(super) fn store_with_slots(slots: usize) -> OfficialFileStore {
    OfficialFileStore::new(
        Arc::new(Semaphore::new(slots)),
        Arc::new(Semaphore::new(1)),
        Duration::from_secs(3600),
    )
    .unwrap()
}

pub(super) fn service(store: OfficialFileStore) -> super::service::OfficialService {
    super::service::OfficialService::new(crate::official::OfficialRuntime::new(), store).unwrap()
}

pub(super) fn object(id: &str, name: &str, bytes: u64) -> FileObjectModel {
    FileObjectModel {
        id: id.into(),
        object: FileObjectModel::OBJECT,
        bytes,
        created_at: 7,
        expires_at: None,
        file_name: name.into(),
        purpose: FilePurpose::ParseOutput,
        sha256sum: None,
    }
}

/// Publish one artifact through the bounded batch path used by the runner.
pub(super) fn publish(store: &OfficialFileStore, id: &str, name: &str, data: &[u8]) {
    store
        .publish_artifact_batch(
            vec![(object(id, name, data.len() as u64), data.to_vec())],
            store.reserve_metadata(1).unwrap(),
            store.artifact_budget(data.len() as u64).unwrap(),
        )
        .unwrap();
}

#[tokio::test]
async fn content_serves_published_bytes_and_unknown_is_not_found() {
    let store = store();
    publish(&store, "file_a", "a.pdf", b"abcd");
    let Json(object) = files::get(State(store.clone()), Path("file_a".into()))
        .await
        .unwrap();
    assert_eq!(object.file_name, "a.pdf");
    let response = files::content(State(store.clone()), Path("file_a".into()))
        .await
        .unwrap();
    assert_eq!(
        response.headers().get("content-type").unwrap(),
        "application/octet-stream"
    );
    let body = axum::body::to_bytes(response.into_body(), 16)
        .await
        .unwrap();
    assert_eq!(&body[..], b"abcd");
    let error = files::get(State(store), Path("missing".into()))
        .await
        .unwrap_err();
    assert_eq!(error.status, StatusCode::NOT_FOUND);
}

use axum::Json;

#[tokio::test]
async fn published_artifacts_stay_charged_until_their_lease_drops() {
    use std::sync::atomic::Ordering;
    let store = store();
    publish(&store, "file_a", "a.pdf", b"abcd");
    let (_, held) = store.content("file_a").unwrap();
    assert_eq!(store.inner.artifact_used.load(Ordering::Acquire), 4);
    assert_eq!(store.inner.meta_used.load(Ordering::Acquire), 1);
    store.clear_retained();
    assert_eq!(store.inner.artifact_used.load(Ordering::Acquire), 4);
    assert_eq!(store.inner.meta_used.load(Ordering::Acquire), 0);
    drop(held);
    assert_eq!(store.inner.artifact_used.load(Ordering::Acquire), 0);
    assert_eq!(store.inner.meta_used.load(Ordering::Acquire), 0);
}

#[tokio::test]
async fn expired_artifacts_are_hidden_and_collected() {
    let store = store();
    publish(&store, "file_a", "a.pdf", b"abcd");
    if let Some(file) = store.inner.files.lock().unwrap().get_mut("file_a") {
        file.object.expires_at = Some(0);
    }
    assert!(store.file("file_a").is_none());
    store.cleanup_expired();
    assert!(store.inner.files.lock().unwrap().is_empty());
}

#[path = "server_tests_lifecycle.rs"]
mod lifecycle;
