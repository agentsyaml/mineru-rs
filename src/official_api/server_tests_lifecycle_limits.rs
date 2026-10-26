use super::super::super::uploads;
use super::{job_request, new_store, service};
use crate::official_api::wire::UploadCreate;
use axum::{
    Json,
    body::Body,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
};

#[tokio::test]
async fn inline_and_declared_uploads_obey_effective_file_and_body_caps() {
    let store = new_store().with_transport_limits(64 * 1024, 256 * 1024, 64 * 1024);
    let service = service(store.clone());
    let data = base64::Engine::encode(
        &base64::engine::general_purpose::STANDARD,
        vec![0; 64 * 1024 + 1],
    );
    let request =
        job_request(serde_json::json!([{"source":{"type":"inline","name":"a.pdf","data":data}}]));
    let error = super::super::super::jobs::create(State(service.inner.clone()), Ok(Json(request)))
        .await
        .unwrap_err();
    assert_eq!(error.status, StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(
        store.inner.used.load(std::sync::atomic::Ordering::Acquire),
        0
    );
    assert_eq!(
        store
            .inner
            .meta_used
            .load(std::sync::atomic::Ordering::Acquire),
        0
    );
    assert_eq!(
        store
            .inner
            .artifact_used
            .load(std::sync::atomic::Ordering::Acquire),
        0
    );

    let store2 = new_store().with_transport_limits(64 * 1024, 32 * 1024, 64 * 1024);
    let error = uploads::create(
        State(store2.clone()),
        Ok(Json(UploadCreate {
            filename: "large.pdf".into(),
            bytes: 40 * 1024,
            mime_type: "application/pdf".into(),
            purpose: "parse".into(),
            sha256sum: None,
            expires_after: None,
        })),
    )
    .await
    .unwrap_err();
    assert_eq!(error.status, StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(
        store2.inner.used.load(std::sync::atomic::Ordering::Acquire),
        0
    );
    assert_eq!(
        store2
            .inner
            .meta_used
            .load(std::sync::atomic::Ordering::Acquire),
        0
    );

    let store3 = new_store().with_transport_limits(64 * 1024, 64 * 1024, 64 * 1024);
    let upload = uploads::create(
        State(store3.clone()),
        Ok(Json(UploadCreate {
            filename: "pending.pdf".into(),
            bytes: 40 * 1024,
            mime_type: "application/pdf".into(),
            purpose: "parse".into(),
            sha256sum: None,
            expires_after: None,
        })),
    )
    .await
    .unwrap()
    .0;
    let _ = store3
        .clone()
        .with_transport_limits(64 * 1024, 32 * 1024, 64 * 1024);
    let error = match uploads::put(
        State(store3.clone()),
        Path(upload.id.clone()),
        HeaderMap::new(),
        Body::empty(),
    )
    .await
    {
        Err(error) => error,
        Ok(_) => panic!("oversized declared upload must be rejected"),
    };
    assert_eq!(error.status, StatusCode::PAYLOAD_TOO_LARGE);
    assert!(
        matches!(store3.inner.entries.lock().unwrap().get(&upload.id),
        Some(super::super::super::store::Entry::Upload(value)) if !value.uploading)
    );
    assert_eq!(
        uploads::cancel(State(store3.clone()), Path(upload.id))
            .await
            .unwrap()
            .0
            .status,
        "canceled"
    );
    assert_eq!(
        store3.inner.used.load(std::sync::atomic::Ordering::Acquire),
        0
    );
}

#[tokio::test]
async fn queued_jobs_ignore_submission_age_and_terminal_ttl_starts_at_finish() {
    let store = new_store();
    let service = service(store.clone());
    let (_, response) = super::super::super::jobs::create(
        State(service.inner.clone()),
        Ok(Json(job_request(
            serde_json::json!([{"source":{"type":"inline","name":"a.pdf","data":"YQ=="}}]),
        ))),
    )
    .await
    .unwrap();
    {
        let jobs = service.inner.jobs.lock().unwrap();
        let mut job = jobs[&response.job_id].try_lock().unwrap();
        job.expiry = Some(0);
    }
    service.cleanup_expired();
    assert!(
        service
            .inner
            .jobs
            .lock()
            .unwrap()
            .contains_key(&response.job_id)
    );
    assert_eq!(store.shared_slots().available_permits(), 1);
    {
        let jobs = service.inner.jobs.lock().unwrap();
        let mut job = jobs[&response.job_id].try_lock().unwrap();
        job.response.status = "completed".into();
        job.expiry = Some(super::super::super::store::OfficialFileStore::now() + 3600);
    }
    service.cleanup_expired();
    assert!(
        service
            .inner
            .jobs
            .lock()
            .unwrap()
            .contains_key(&response.job_id)
    );
    {
        let jobs = service.inner.jobs.lock().unwrap();
        jobs[&response.job_id].try_lock().unwrap().expiry = Some(0);
    }
    service.cleanup_expired();
    assert!(
        !service
            .inner
            .jobs
            .lock()
            .unwrap()
            .contains_key(&response.job_id)
    );
    assert_eq!(store.shared_slots().available_permits(), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 3)]
async fn shutdown_barrier_preserves_accepted_commit_and_rejects_later_jobs() {
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
        super::super::super::jobs::create(
            State(inner),
            Ok(Json(job_request(
                serde_json::json!([{"source":{"type":"inline","name":"a.pdf","data":"YQ=="}}]),
            ))),
        )
        .await
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
    assert!(
        !service
            .inner
            .stopping
            .load(std::sync::atomic::Ordering::Acquire)
    );
    resume_tx.send(()).unwrap();
    let (status, response) = create.await.unwrap().unwrap();
    assert_eq!(status, StatusCode::ACCEPTED);
    let job = service.inner.jobs.lock().unwrap()[&response.job_id].clone();
    runner_resume_tx.send(()).unwrap();
    shutdown.await.unwrap().unwrap();
    let record = job.try_lock().unwrap();
    assert!(matches!(
        record.response.status.as_str(),
        "completed" | "failed" | "canceled"
    ));
    assert!(record.response.finished_at.is_some());
    drop(record);
    let error = super::super::super::jobs::create(
        State(service.inner.clone()),
        Ok(Json(job_request(
            serde_json::json!([{"source":{"type":"inline","name":"late.pdf","data":"YQ=="}}]),
        ))),
    )
    .await
    .unwrap_err();
    assert_eq!(error.status, StatusCode::SERVICE_UNAVAILABLE);
    assert!(service.inner.jobs.lock().unwrap().is_empty());
    assert!(service.inner.queue.lock().unwrap().is_empty());
    assert_eq!(
        service
            .inner
            .jobs_created
            .load(std::sync::atomic::Ordering::Acquire),
        1
    );
    assert_eq!(
        store.inner.used.load(std::sync::atomic::Ordering::Acquire),
        0
    );
    assert_eq!(
        store
            .inner
            .meta_used
            .load(std::sync::atomic::Ordering::Acquire),
        1
    );
    assert_eq!(
        store
            .inner
            .artifact_used
            .load(std::sync::atomic::Ordering::Acquire),
        0
    );

    let store2 = new_store();
    let metadata = store2.reserve_metadata(2).unwrap();
    let reservation = store2.input_reservation(3).unwrap();
    store2.stop_admissions();
    let error = uploads::commit_upload(
        &store2,
        "late-upload".into(),
        UploadCreate {
            filename: "x.pdf".into(),
            bytes: 3,
            mime_type: "application/pdf".into(),
            purpose: "parse".into(),
            sha256sum: None,
            expires_after: None,
        },
        1,
        3600,
        metadata,
        reservation,
    )
    .unwrap_err();
    assert_eq!(error.status, StatusCode::SERVICE_UNAVAILABLE);
    assert!(store2.inner.entries.lock().unwrap().is_empty());
    assert_eq!(
        store2.inner.used.load(std::sync::atomic::Ordering::Acquire),
        0
    );
    assert_eq!(
        store2
            .inner
            .meta_used
            .load(std::sync::atomic::Ordering::Acquire),
        0
    );
}

#[tokio::test]
async fn cancel_while_waiting_for_gate_finalizes_all_unfinished_files() {
    let store = crate::official_api::server::OfficialFileStore::new(
        std::sync::Arc::new(tokio::sync::Semaphore::new(2)),
        std::sync::Arc::new(tokio::sync::Semaphore::new(0)),
        std::time::Duration::from_secs(3600),
    )
    .unwrap();
    let service = service(store.clone());
    let (_, response) = super::super::super::jobs::create(
        State(service.inner.clone()),
        Ok(Json(job_request(serde_json::json!([
            {"source":{"type":"inline","name":"first.pdf","data":"YQ=="}},
            {"source":{"type":"inline","name":"second.pdf","data":"Yg=="}}
        ])))),
    )
    .await
    .unwrap();
    let inner = service.inner.clone();
    let id = response.job_id.clone();
    let runner = tokio::spawn(async move { super::super::super::jobs::run_job(inner, id).await });
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop {
            let running = {
                let jobs = service.inner.jobs.lock().unwrap();
                jobs[&response.job_id].try_lock().unwrap().response.files[0].status == "running"
            };
            if running {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        super::super::super::jobs::cancel(
            State(service.inner.clone()),
            Path(response.job_id.clone())
        )
        .await
        .unwrap()
        .0
        .status,
        "canceled"
    );
    tokio::time::timeout(std::time::Duration::from_secs(2), runner)
        .await
        .unwrap()
        .unwrap();
    let jobs = service.inner.jobs.lock().unwrap();
    let record = jobs[&response.job_id].try_lock().unwrap();
    assert_eq!(record.response.status, "canceled");
    assert!(record.response.files.iter().all(|f| f.status == "canceled"));
    assert_eq!(record.response.progress.completed, 0);
    assert_eq!(
        store.inner.used.load(std::sync::atomic::Ordering::Acquire),
        0
    );
}

#[tokio::test]
async fn shutdown_while_waiting_for_gate_finishes_without_releasing_external_permit() {
    let store = new_store();
    let gate = store.shared_gate().clone();
    let held: Vec<_> = (0..gate.available_permits())
        .map(|_| gate.clone().try_acquire_owned().unwrap())
        .collect();
    assert!(!held.is_empty());
    let service = service(store.clone());
    let (_, response) = super::super::super::jobs::create(
        State(service.inner.clone()),
        Ok(Json(job_request(serde_json::json!([
            {"source":{"type":"inline","name":"held.pdf","data":"YQ=="}}
        ])))),
    )
    .await
    .unwrap();
    service.start().unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            let running = {
                let jobs = service.inner.jobs.lock().unwrap();
                jobs[&response.job_id].try_lock().unwrap().response.files[0].status == "running"
            };
            if running {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap_or_else(|_| {
        let jobs = service.inner.jobs.lock().unwrap();
        let record = jobs[&response.job_id].try_lock().unwrap();
        panic!(
            "job never reached gate wait: {:?}",
            record.response.files[0].status
        );
    });
    let record = service.inner.jobs.lock().unwrap()[&response.job_id].clone();
    tokio::time::timeout(std::time::Duration::from_secs(2), service.shutdown())
        .await
        .expect("shutdown must not wait for the external gate permit")
        .unwrap();
    let record = record.lock().await;
    assert_eq!(record.response.status, "canceled");
    assert!(
        record
            .response
            .files
            .iter()
            .all(|file| file.status == "canceled")
    );
    assert_eq!(
        store.inner.used.load(std::sync::atomic::Ordering::Acquire),
        0
    );
    drop(held);
}

#[tokio::test]
async fn unrelated_queued_cancel_does_not_skip_gate_waiting_job_file() {
    let store = new_store();
    let gate = store.shared_gate().clone();
    let held: Vec<_> = (0..gate.available_permits())
        .map(|_| gate.clone().try_acquire_owned().unwrap())
        .collect();
    let service = service(store.clone());
    let (_, active) = super::super::super::jobs::create(
        State(service.inner.clone()),
        Ok(Json(job_request(serde_json::json!([
            {"source":{"type":"inline","name":"first.pdf","data":"YQ=="}},
            {"source":{"type":"inline","name":"second.pdf","data":"Yg=="}}
        ])))),
    )
    .await
    .unwrap();
    let (_, queued) = super::super::super::jobs::create(
        State(service.inner.clone()),
        Ok(Json(job_request(serde_json::json!([
            {"source":{"type":"inline","name":"queued.pdf","data":"Yw=="}}
        ])))),
    )
    .await
    .unwrap();
    let inner = service.inner.clone();
    let id = active.job_id.clone();
    let runner = tokio::spawn(async move { super::super::super::jobs::run_job(inner, id).await });
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            let running = {
                let jobs = service.inner.jobs.lock().unwrap();
                jobs[&active.job_id].try_lock().unwrap().response.files[0].status == "running"
            };
            if running {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let _ = super::super::super::jobs::cancel(
        State(service.inner.clone()),
        Path(queued.job_id.clone()),
    )
    .await
    .unwrap();
    drop(held);
    tokio::time::timeout(std::time::Duration::from_secs(5), runner)
        .await
        .unwrap()
        .unwrap();
    let jobs = service.inner.jobs.lock().unwrap();
    let active = jobs[&active.job_id].try_lock().unwrap();
    assert_eq!(
        active.response.progress.completed + active.response.progress.failed,
        2
    );
}
