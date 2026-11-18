//! `PUT /v1/tasks/{task_id}/uploads/{file_id}`: one bounded, hashed upload.
use super::super::{service::ServiceInner, store::OfficialFileStore};
use crate::official_api::wire::{TaskFileState, TaskStatus, UploadFileResponse, WireError, code};
use axum::{
    Json,
    body::Body,
    extract::{Path, State},
    http::{HeaderMap, StatusCode, header},
};
use futures_util::StreamExt;
use sha2::{Digest, Sha256};
use std::{sync::Arc, sync::atomic::Ordering};

pub(in crate::official_api::server) async fn put(
    State(s): State<Arc<ServiceInner>>,
    Path((task_id, file_id)): Path<(String, String)>,
    headers: HeaderMap,
    body: Body,
) -> Result<Json<UploadFileResponse>, WireError> {
    if s.stopping.load(Ordering::Acquire) {
        return Err(super::stopping(&s).unwrap_or_else(WireError::internal));
    }
    let lease = super::lease_seconds(&headers)?;
    let shared = s
        .store
        .lookup(&task_id)
        .ok_or_else(|| super::not_found(&task_id))?;
    let (index, upload_lock) = {
        let mut task = shared.lock().await;
        s.store.update_lease(&mut task, lease);
        s.store.expire_uploads(&mut task);
        let index = task
            .files
            .iter()
            .position(|file| file.file_id == file_id)
            .ok_or_else(WireError::upload_not_found)?;
        (index, task.files[index].upload_lock.clone())
    };
    let _upload = upload_lock.lock().await;
    let (declared, sha) = {
        let mut task = shared.lock().await;
        let file = &task.files[index];
        if file.url.is_some() {
            return Err(WireError::invalid_unattributed(
                "URL inputs do not accept uploads",
            ));
        }
        if task.expires_at <= OfficialFileStore::now() {
            return Err(expired());
        }
        if task.status == TaskStatus::Canceled {
            return Err(terminal("Task was canceled"));
        }
        if file.uploaded {
            let status = file.status;
            drop(task);
            drain(body).await;
            return Ok(Json(UploadFileResponse {
                task_id,
                file_id,
                status,
            }));
        }
        if file.status != TaskFileState::WaitingFile {
            return Err(WireError::new(
                StatusCode::CONFLICT,
                "file_already_terminal",
                "File no longer accepts uploads",
                None,
            ));
        }
        let declared = file.declared.ok_or_else(WireError::internal)?;
        if let Some(length) = headers
            .get(header::CONTENT_LENGTH)
            .and_then(|value| value.to_str().ok())
        {
            let parsed = length.parse::<u64>().ok();
            if parsed != Some(declared) {
                return Err(size_mismatch());
            }
        }
        let file = &mut task.files[index];
        file.upload_active = true;
        (declared, file.sha256sum.clone())
    };
    let Some(reservation) = s.store.input_reservation(declared) else {
        shared.lock().await.files[index].upload_active = false;
        return Err(super::capacity("Input storage capacity is exhausted."));
    };
    let outcome = stream_body(
        &s,
        &shared,
        index,
        body,
        declared,
        &sha,
        reservation.clone(),
    )
    .await;
    let mut task = shared.lock().await;
    task.files[index].upload_active = false;
    let (data, size) = match outcome {
        Ok(upload) => upload,
        Err(error) => {
            s.store.aggregate(&mut task);
            return Err(error);
        }
    };
    if task.status == TaskStatus::Canceled {
        return Err(terminal("Task was canceled during upload"));
    }
    {
        let file = &mut task.files[index];
        file.data = Some(data);
        file.actual = Some(size);
        file.uploaded = true;
        file.status = TaskFileState::Pending;
    }
    s.store.aggregate(&mut task);
    let status = task.files[index].status;
    drop(task);
    super::enqueue(&s, &task_id);
    Ok(Json(UploadFileResponse {
        task_id,
        file_id,
        status,
    }))
}

#[allow(clippy::too_many_arguments)]
async fn stream_body(
    s: &ServiceInner,
    shared: &Arc<tokio::sync::Mutex<super::super::store::TaskRecord>>,
    index: usize,
    body: Body,
    declared: u64,
    sha: &Option<String>,
    reservation: Arc<super::super::store::Reservation>,
) -> Result<(bytes::Bytes, u64), WireError> {
    let mut stream = body.into_data_stream();
    let mut data = Vec::with_capacity(declared as usize);
    let mut hash = Sha256::new();
    while let Some(chunk) = stream.next().await {
        if canceled(shared, index).await {
            return Err(terminal("Task was canceled during upload"));
        }
        let chunk = chunk.map_err(|_| {
            WireError::new(
                StatusCode::BAD_REQUEST,
                code::UPLOAD_INTERRUPTED,
                "Upload interrupted; retry this file on the same task",
                None,
            )
        })?;
        if data.len() as u64 + chunk.len() as u64 > declared {
            return Err(size_mismatch());
        }
        hash.update(&chunk);
        data.extend_from_slice(&chunk);
    }
    let size = data.len() as u64;
    if size == 0 {
        return Err(WireError::invalid("Empty files are not supported", "files"));
    }
    if size != declared {
        return Err(size_mismatch());
    }
    let digest: String = hash
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    if sha.as_ref().is_some_and(|expected| expected != &digest) {
        return Err(WireError::new(
            StatusCode::BAD_REQUEST,
            code::FILE_HASH_MISMATCH,
            "SHA-256 mismatch",
            None,
        ));
    }
    reservation.shrink_to(size);
    Ok((s.store.charged(data, reservation), size))
}

async fn canceled(
    shared: &Arc<tokio::sync::Mutex<super::super::store::TaskRecord>>,
    index: usize,
) -> bool {
    match shared.try_lock() {
        Ok(task) => {
            task.status == TaskStatus::Canceled
                || task
                    .files
                    .get(index)
                    .is_some_and(|file| file.status == TaskFileState::Canceled)
        }
        Err(_) => false,
    }
}

async fn drain(body: Body) {
    let mut stream = body.into_data_stream();
    while stream.next().await.is_some() {}
}

fn expired() -> WireError {
    WireError::new(
        StatusCode::GONE,
        "upload_expired",
        "Upload link expired",
        None,
    )
}

fn terminal(message: &'static str) -> WireError {
    WireError::new(
        StatusCode::CONFLICT,
        code::TASK_ALREADY_TERMINAL,
        message,
        None,
    )
}

fn size_mismatch() -> WireError {
    WireError::new(
        StatusCode::BAD_REQUEST,
        code::UPLOAD_SIZE_MISMATCH,
        "Declared size does not match uploaded bytes",
        None,
    )
}
