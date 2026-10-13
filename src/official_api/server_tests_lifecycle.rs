use super::super::{files, uploads};
use super::{job_request, service, store as new_store};
use crate::official_api::wire::UploadCreate;
use axum::{
    Json,
    body::Body,
    extract::{Path, State},
    http::HeaderMap,
};

#[tokio::test]
async fn artifact_bytes_are_charged_to_each_file_lease_not_job_history() {
    let store = new_store();
    let metadata = store.reserve_metadata(3).unwrap();
    let budget = store.artifact_budget(8).unwrap();
    let files = (0..2)
        .map(|n| {
            let id = format!("artifact-{n}");
            (
                crate::official_api::wire::FileObject {
                    id,
                    object: "file",
                    bytes: 4,
                    created_at: 0,
                    expires_at: None,
                    filename: "x.bin".into(),
                    purpose: "parse".into(),
                    sha256sum: None,
                },
                vec![n; 4],
            )
        })
        .collect();
    store
        .publish_artifact_batch(files, metadata.clone(), budget.clone())
        .unwrap();
    budget.shrink_to(0);
    metadata.shrink_to(1);
    let held = match store
        .inner
        .entries
        .lock()
        .unwrap()
        .get("artifact-0")
        .unwrap()
    {
        super::super::store::Entry::File(file) => file.bytes.clone(),
        _ => unreachable!(),
    };
    for id in ["artifact-0", "artifact-1"] {
        assert!(
            files::delete(State(store.clone()), Path(id.into()))
                .await
                .unwrap()
                .0
                .deleted
        );
    }
    assert_eq!(
        store
            .inner
            .artifact_used
            .load(std::sync::atomic::Ordering::Acquire),
        4
    );
    assert_eq!(
        store
            .inner
            .meta_used
            .load(std::sync::atomic::Ordering::Acquire),
        1
    );
    drop(held);
    assert_eq!(
        store
            .inner
            .artifact_used
            .load(std::sync::atomic::Ordering::Acquire),
        0
    );
    drop(metadata);
    assert_eq!(
        store
            .inner
            .meta_used
            .load(std::sync::atomic::Ordering::Acquire),
        0
    );
}

#[tokio::test]
async fn expired_source_cannot_be_leased_but_existing_lease_survives_cleanup() {
    let store = new_store();
    let upload = uploads::create(
        State(store.clone()),
        Ok(Json(UploadCreate {
            filename: "input.pdf".into(),
            bytes: 3,
            mime_type: "application/pdf".into(),
            purpose: "parse".into(),
            sha256sum: None,
            expires_after: None,
        })),
    )
    .await
    .unwrap()
    .0;
    uploads::put(
        State(store.clone()),
        Path(upload.id.clone()),
        HeaderMap::new(),
        Body::from("abc"),
    )
    .await
    .unwrap();
    let file = uploads::complete(State(store.clone()), Path(upload.id), Ok(None))
        .await
        .unwrap()
        .0
        .file
        .unwrap();
    let lease = store.input_lease(&file.id).unwrap().1;
    if let Some(super::super::store::Entry::File(file)) =
        store.inner.entries.lock().unwrap().get_mut(&file.id)
    {
        file.object.expires_at = Some(0);
    }
    assert!(store.input_lease(&file.id).is_none());
    store.cleanup_expired();
    assert_eq!(&lease[..], b"abc");
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
async fn deleting_queued_source_reclaims_its_job_lease_on_cancel() {
    let store = new_store();
    let upload = uploads::create(
        State(store.clone()),
        Ok(Json(UploadCreate {
            filename: "input.pdf".into(),
            bytes: 3,
            mime_type: "application/pdf".into(),
            purpose: "parse".into(),
            sha256sum: None,
            expires_after: None,
        })),
    )
    .await
    .unwrap()
    .0;
    uploads::put(
        State(store.clone()),
        Path(upload.id.clone()),
        HeaderMap::new(),
        Body::from("abc"),
    )
    .await
    .unwrap();
    let source = uploads::complete(State(store.clone()), Path(upload.id), Ok(None))
        .await
        .unwrap()
        .0
        .file
        .unwrap();
    let service = service(store.clone());
    let (_, job) = super::super::jobs::create(
        State(service.inner.clone()),
        Ok(Json(job_request(
            serde_json::json!([{"source":{"type":"file_id","file_id":source.id}}]),
        ))),
    )
    .await
    .unwrap();
    assert!(
        files::delete(State(store.clone()), Path(source.id))
            .await
            .unwrap()
            .0
            .deleted
    );
    assert_eq!(
        store.inner.used.load(std::sync::atomic::Ordering::Acquire),
        3
    );
    assert_eq!(
        super::super::jobs::cancel(State(service.inner.clone()), Path(job.job_id.clone()))
            .await
            .unwrap()
            .0
            .status,
        "canceled"
    );
    assert_eq!(
        store.inner.used.load(std::sync::atomic::Ordering::Acquire),
        0
    );
}

#[tokio::test]
async fn cancellation_before_artifact_commit_prevents_publication() {
    let store = new_store();
    let service = service(store.clone());
    let (_, response) = super::super::jobs::create(
        State(service.inner.clone()),
        Ok(Json(job_request(serde_json::json!([
            {"source":{"type":"inline","name":"done.pdf","data":"YQ=="}},
            {"source":{"type":"inline","name":"current.pdf","data":"Yg=="}},
            {"source":{"type":"inline","name":"later.pdf","data":"Yw=="}}
        ])))),
    )
    .await
    .unwrap();
    let shared = service.inner.jobs.lock().unwrap()[&response.job_id].clone();
    assert!(!super::super::jobs::publication_was_canceled(&service.inner, &shared).await);
    {
        let mut record = shared.lock().await;
        record.response.files[0].status = "completed".into();
        record.response.files[0].parse = Some(serde_json::json!({"pages":1}));
        record.response.files[1].status = "running".into();
    }
    let result = crate::official::OfficialResult {
        middle: serde_json::json!({"pages":[{}]}),
        model: None,
        markdown: "text".into(),
        structured: serde_json::json!({}),
        assets: std::collections::BTreeMap::new(),
        warnings: Vec::new(),
        metadata: crate::official::OfficialVersionMetadata {
            mineru_package: "test".into(),
            schema_version: "test".into(),
            bundle_name: "test".into(),
        },
    };
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
    let (resume_tx, resume_rx) = tokio::sync::oneshot::channel();
    *service.inner.publication_pause.lock().unwrap() = Some((ready_tx, resume_rx));
    let inner = service.inner.clone();
    let target = shared.clone();
    let publish = tokio::spawn(async move {
        super::super::jobs::output::publish_outputs(&inner, &target, 1, &result, &mut 0).await
    });
    tokio::time::timeout(std::time::Duration::from_secs(3), ready_rx)
        .await
        .unwrap()
        .unwrap();
    shared
        .lock()
        .await
        .cancel
        .store(true, std::sync::atomic::Ordering::Release);
    resume_tx.send(()).unwrap();
    assert!(publish.await.unwrap().is_err());
    let mut failures = 0;
    assert!(
        super::super::jobs::record_publication_failure(&service.inner, &shared, 1, &mut failures)
            .await
    );
    assert_eq!(failures, 0);
    {
        let record = shared.lock().await;
        assert!(record.response.files[1].output_files.is_none());
        assert!(record.response.files[1].parse.is_none());
        assert_eq!(record.response.progress.failed, 0);
    }
    assert!(store.inner.entries.lock().unwrap().is_empty());
    assert!(
        shared
            .lock()
            .await
            .cancel
            .load(std::sync::atomic::Ordering::Acquire)
    );
    super::super::jobs::finalize_job(&service.inner, &shared, 1, 0).await;
    let record = shared.lock().await;
    assert_eq!(record.response.status, "canceled");
    assert_eq!(record.response.files[0].status, "completed");
    assert_eq!(
        record.response.files[0].parse,
        Some(serde_json::json!({"pages":1}))
    );
    assert_eq!(record.response.files[1].status, "canceled");
    assert_eq!(record.response.files[2].status, "canceled");
    assert_eq!(record.response.progress.completed, 1);
    assert_eq!(record.response.progress.failed, 0);
    drop(record);
    assert_eq!(
        store
            .inner
            .artifact_used
            .load(std::sync::atomic::Ordering::Acquire),
        0
    );
    assert_eq!(
        store.inner.used.load(std::sync::atomic::Ordering::Acquire),
        0
    );
}

#[tokio::test]
async fn uncanceled_artifact_generation_error_is_counted_as_failed() {
    let service = service(new_store());
    let (_, response) = super::super::jobs::create(
        State(service.inner.clone()),
        Ok(Json(job_request(
            serde_json::json!([{"source":{"type":"inline","name":"bad.pdf","data":"YQ=="}}]),
        ))),
    )
    .await
    .unwrap();
    let shared = service.inner.jobs.lock().unwrap()[&response.job_id].clone();
    let mut failures = 0;
    assert!(
        !super::super::jobs::record_publication_failure(&service.inner, &shared, 0, &mut failures)
            .await
    );
    assert_eq!(failures, 1);
    let record = shared.lock().await;
    assert_eq!(record.response.files[0].status, "failed");
    assert_eq!(
        record.response.files[0].error.as_ref().unwrap().message,
        "artifact generation failed"
    );
}

#[path = "server_tests_lifecycle_limits.rs"]
mod limits;
