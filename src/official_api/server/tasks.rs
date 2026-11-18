//! MinerU 4.2.3 task surface: JSON task creation, per-file uploads, task
//! detail and cancellation receipts.
use super::{
    jobs,
    service::ServiceInner,
    store::{OfficialFileStore, TaskFile, TaskRecord},
    submission::{self, Submission},
};
use crate::official_api::wire::{CreateTaskRequest, TaskFileState, WireError, code};
use axum::{
    Json, Router,
    body::Body,
    extract::{Path, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post, put},
};
use std::sync::Arc;
use tokio::sync::Mutex as AsyncMutex;

pub(super) mod payload;
pub(super) mod upload;

#[cfg(test)]
#[path = "tasks/tests.rs"]
mod tests;

pub(super) fn router(state: Arc<ServiceInner>) -> Router {
    let body_cap = state.store.ingest_body_cap();
    Router::new()
        .route("/v1/tasks", post(create))
        .route("/v1/tasks/{id}", get(status))
        .route("/v1/tasks/{id}/uploads/{file_id}", put(upload::put))
        .route("/v1/tasks/{id}/cancel", post(cancel))
        .with_state(state)
        .layer(axum::extract::DefaultBodyLimit::max(body_cap))
}

fn stopping(s: &ServiceInner) -> Option<WireError> {
    s.stopping
        .load(std::sync::atomic::Ordering::Acquire)
        .then_some(WireError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "server_shutting_down",
            "The server is shutting down.",
            None,
        ))
}

pub(super) fn not_found(task_id: &str) -> WireError {
    WireError::task_not_found(task_id)
}

pub(super) fn ingest_permit(
    s: &ServiceInner,
) -> Result<tokio::sync::OwnedSemaphorePermit, WireError> {
    s.ingest_slots
        .clone()
        .try_acquire_owned()
        .map_err(|_| capacity("Request body capacity is exhausted."))
}

pub(super) fn capacity(message: &'static str) -> WireError {
    WireError::new(
        StatusCode::TOO_MANY_REQUESTS,
        "capacity_exceeded",
        message,
        None,
    )
}

pub(super) fn ingest_timeout() -> WireError {
    WireError::new(
        StatusCode::REQUEST_TIMEOUT,
        "body_timeout",
        "Request body was not received before the deadline.",
        None,
    )
}

pub(super) async fn create(
    State(s): State<Arc<ServiceInner>>,
    request: axum::http::Request<Body>,
) -> Result<Response, WireError> {
    if let Some(error) = stopping(&s) {
        return Err(error);
    }
    if request.uri().query().is_some_and(|query| !query.is_empty()) {
        return Err(WireError::invalid_unattributed(
            "POST parameters must be supplied in the JSON body, not the URL",
        ));
    }
    let media = media_type(request.headers());
    if media != "application/json" {
        return Err(WireError::new(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            code::INVALID_REQUEST,
            "Use application/json",
            None,
        ));
    }
    let _ingest = ingest_permit(&s)?;
    let (parts, body) = request.into_parts();
    let base = payload::base_url(&parts.headers);
    let bytes = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        read_manifest(body, s.store.ingest_body_cap()),
    )
    .await
    .map_err(|_| ingest_timeout())??;
    let request: CreateTaskRequest = serde_json::from_slice(&bytes)
        .map_err(|error| WireError::invalid_unattributed(format!("Invalid request: {error}")))?;
    request.validate()?;
    submit(&s, submission::normalize(request), &base).await
}

/// Reads the JSON manifest, distinguishing a client disconnect (`request_interrupted`)
/// from a body that exceeds the ingest cap (`413`).
async fn read_manifest(body: Body, cap: usize) -> Result<bytes::Bytes, WireError> {
    let mut stream = body.into_data_stream();
    let mut data = Vec::new();
    while let Some(chunk) = futures_util::StreamExt::next(&mut stream).await {
        let chunk = chunk.map_err(|_| {
            WireError::new(
                StatusCode::BAD_REQUEST,
                code::REQUEST_INTERRUPTED,
                "Request interrupted",
                None,
            )
        })?;
        if data.len() + chunk.len() > cap {
            return Err(WireError::too_large());
        }
        data.extend_from_slice(&chunk);
    }
    Ok(data.into())
}

fn media_type(headers: &HeaderMap) -> String {
    headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("")
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase()
}

/// `x-mineru-task-lease-seconds`: bounded result retention requested by a
/// parent task. Upstream validates it on task detail and uploads only.
fn lease_seconds(headers: &HeaderMap) -> Result<Option<u64>, WireError> {
    let Some(value) = headers.get("x-mineru-task-lease-seconds") else {
        return Ok(None);
    };
    let invalid =
        || WireError::invalid_unattributed("Task result lease must be between 0 and 3600 seconds");
    let seconds = value
        .to_str()
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .ok_or_else(invalid)?;
    if seconds > crate::official_api::wire::MAX_LEASE_SECS {
        return Err(invalid());
    }
    Ok(Some(seconds))
}

async fn submit(
    s: &Arc<ServiceInner>,
    submission: Submission,
    base: &str,
) -> Result<Response, WireError> {
    if let Some(error) = stopping(s) {
        return Err(error);
    }
    let Submission { options, files } = submission;
    let slot = s
        .store
        .shared_slots()
        .clone()
        .try_acquire_owned()
        .map_err(|_| capacity("Task capacity is exhausted."))?;
    let metadata = s
        .store
        .reserve_metadata(1 + files.len())
        .ok_or_else(|| capacity("Metadata capacity is exhausted."))?;
    let artifact_budget = s
        .store
        .artifact_budget(64 * 1024 * 1024)
        .ok_or_else(|| capacity("Artifact storage capacity is exhausted."))?;
    let resolved = jobs::resolve_tier(options.tier, &files);
    let task_id = s.store.next_id("job");
    let task_files: Vec<TaskFile> = files
        .into_iter()
        .map(|file| {
            let tier = jobs::file_tier(resolved, &file.file_name);
            TaskFile {
                file_id: s.store.next_id("file"),
                file_name: file.file_name,
                declared: file.bytes,
                actual: None,
                url: file.url,
                sha256sum: file.sha256sum,
                page_range: file.page_range.unwrap_or_default(),
                tier,
                status: TaskFileState::WaitingFile,
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
        })
        .collect();
    let shared = Arc::new(AsyncMutex::new(TaskRecord {
        id: task_id.clone(),
        created_at: super::store::timestamp(),
        finished_at: None,
        status: crate::official_api::wire::TaskStatus::WaitingFile,
        options,
        files: task_files,
        expires_at: OfficialFileStore::now()
            + crate::official_api::wire::TASK_UPLOAD_TTL_SECS as i64,
        retention_at: None,
        error: None,
        running: false,
        _slot: slot,
        _metadata: metadata,
        _artifact_budget: artifact_budget,
    }));
    let response = payload::created(&shared, base).await;
    commit(s, &task_id, shared)?;
    Ok((StatusCode::ACCEPTED, Json(response)).into_response())
}

fn commit(
    s: &ServiceInner,
    task_id: &str,
    task: Arc<AsyncMutex<TaskRecord>>,
) -> Result<(), WireError> {
    let _admission = s
        .store
        .inner
        .admission
        .lock()
        .map_err(|_| WireError::internal())?;
    if s.stopping.load(std::sync::atomic::Ordering::Acquire)
        || !s
            .store
            .inner
            .accepting
            .load(std::sync::atomic::Ordering::Acquire)
    {
        return Err(stopping(s).unwrap_or_else(WireError::internal));
    }
    #[cfg(test)]
    {
        let pause = s
            .commit_pause
            .lock()
            .map_err(|_| WireError::internal())?
            .take();
        if let Some((ready, resume)) = pause {
            let _ = ready.send(());
            let _ = resume.recv();
        }
    }
    s.store.insert_task(task_id.to_owned(), task);
    s.jobs_created
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    enqueue(s, task_id);
    Ok(())
}

pub(super) fn enqueue(s: &ServiceInner, task_id: &str) {
    if let Ok(mut queue) = s.queue.lock() {
        queue.push_back(task_id.to_owned());
    }
    s.notify.notify_one();
}

pub(super) async fn status(
    State(s): State<Arc<ServiceInner>>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<crate::official_api::wire::TaskDetailResponse>, WireError> {
    let shared = s.store.lookup(&id).ok_or_else(|| not_found(&id))?;
    let lease = lease_seconds(&headers)?;
    let base = payload::base_url(&headers);
    let mut task = shared.lock().await;
    s.store.update_lease(&mut task, lease);
    s.store.expire_uploads(&mut task);
    Ok(Json(payload::detail(&task, &base)))
}

pub(super) async fn cancel(
    State(s): State<Arc<ServiceInner>>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<crate::official_api::wire::CancelTaskResponse>, WireError> {
    let shared = s.store.lookup(&id).ok_or_else(|| not_found(&id))?;
    let mut task = shared.lock().await;
    s.store.expire_uploads(&mut task);
    if super::store::task_terminal(task.status) {
        return Err(WireError::new(
            StatusCode::CONFLICT,
            code::TASK_ALREADY_TERMINAL,
            format!("Task is {}", payload::status_str(task.status)),
            None,
        ));
    }
    jobs::cancel_task(&s, &mut task);
    let base = payload::base_url(&headers);
    Ok(Json(payload::canceled(&task, &base)))
}
