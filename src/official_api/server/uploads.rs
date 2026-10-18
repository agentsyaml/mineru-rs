use super::store::{Entry, File, OfficialFileStore, Upload};
use crate::official_api::wire::{Completion, UploadCreate, UploadResponse, WireError};
use axum::{
    Json,
    body::Body,
    extract::{Path, State, rejection::JsonRejection},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
};
use futures_util::StreamExt;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

pub(super) async fn create(
    State(store): State<OfficialFileStore>,
    request: Result<Json<UploadCreate>, JsonRejection>,
) -> Result<Json<UploadResponse>, WireError> {
    let Json(req) = request.map_err(|_| WireError::bad("body", "Invalid upload request."))?;
    if !store
        .inner
        .accepting
        .load(std::sync::atomic::Ordering::Acquire)
    {
        return Err(WireError {
            status: StatusCode::SERVICE_UNAVAILABLE,
            code: "server_shutting_down",
            message: "The server is shutting down.",
            param: None,
        });
    }
    store.cleanup_expired();
    req.validate()?;
    if req.bytes > store.file_cap() || req.bytes > store.body_cap() as u64 {
        return Err(WireError::too_large());
    }
    let ttl = req.expires_after.as_ref().map_or(3600, |v| v.seconds) as u64;
    if !store.retain_for(ttl) {
        return Err(WireError::bad(
            "expires_after",
            "Expiry exceeds configured retention.",
        ));
    }
    let id = store.id();
    let now = OfficialFileStore::now();
    let metadata = store.reserve_metadata(2).ok_or(WireError {
        status: StatusCode::TOO_MANY_REQUESTS,
        code: "capacity_exceeded",
        message: "Metadata capacity is exhausted.",
        param: None,
    })?;
    let reservation = store.input_reservation(req.bytes).ok_or(WireError {
        status: StatusCode::TOO_MANY_REQUESTS,
        code: "capacity_exceeded",
        message: "Input storage capacity is exhausted.",
        param: None,
    })?;
    commit_upload(&store, id, req, now, ttl, metadata, reservation)
}

pub(super) fn commit_upload(
    store: &OfficialFileStore,
    id: String,
    req: UploadCreate,
    now: i64,
    ttl: u64,
    metadata: std::sync::Arc<super::store::MetadataReservation>,
    reservation: std::sync::Arc<super::store::Reservation>,
) -> Result<Json<UploadResponse>, WireError> {
    let _admission = store.inner.admission.lock().map_err(|_| internal())?;
    if !store
        .inner
        .accepting
        .load(std::sync::atomic::Ordering::Acquire)
    {
        return Err(WireError {
            status: StatusCode::SERVICE_UNAVAILABLE,
            code: "server_shutting_down",
            message: "The server is shutting down.",
            param: None,
        });
    }
    let mut entries = store
        .inner
        .entries
        .lock()
        .map_err(|_| WireError::bad("request", "Request could not be processed."))?;
    entries.insert(
        id.clone(),
        Entry::Upload(Upload {
            request: req.clone(),
            created: now,
            expires: now + ttl as i64,
            bytes: None,
            reservation: Some(reservation),
            metadata,
            file_id: None,
            uploading: false,
        }),
    );
    Ok(Json(upload_response(id, &req, now, now + ttl as i64, None)))
}

fn upload_response(
    id: String,
    req: &UploadCreate,
    created: i64,
    expires: i64,
    file: Option<crate::official_api::wire::FileObject>,
) -> UploadResponse {
    UploadResponse {
        id: id.clone(),
        object: "upload",
        bytes: req.bytes,
        created_at: created,
        expires_at: expires,
        filename: req.filename.clone(),
        purpose: req.purpose.clone(),
        mime_type: req.mime_type.clone(),
        sha256sum: req.sha256sum.clone(),
        status: if file.is_some() {
            "completed"
        } else {
            "pending"
        }
        .into(),
        upload_url: if file.is_some() {
            None
        } else {
            Some(format!("/v1/uploads/{id}/content"))
        },
        upload_method: if file.is_some() { None } else { Some("PUT") },
        upload_headers: if file.is_some() {
            BTreeMap::new()
        } else {
            BTreeMap::from([("Content-Type".into(), req.mime_type.clone())])
        },
        file,
    }
}

pub(super) async fn get(
    State(store): State<OfficialFileStore>,
    Path(id): Path<String>,
) -> Result<Json<UploadResponse>, WireError> {
    store.cleanup_expired();
    let entries = store
        .inner
        .entries
        .lock()
        .map_err(|_| WireError::bad("request", "Request could not be processed."))?;
    match entries.get(&id) {
        Some(Entry::Upload(u)) if u.expires > OfficialFileStore::now() => {
            let file = match &u.file_id {
                Some(fid) => Some(match entries.get(fid) {
                    Some(Entry::File(f)) => f.object.clone(),
                    _ => return Err(not_found()),
                }),
                None => None,
            };
            Ok(Json(upload_response(
                id, &u.request, u.created, u.expires, file,
            )))
        }
        _ => Err(not_found()),
    }
}

pub(super) async fn put(
    State(store): State<OfficialFileStore>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: Body,
) -> Result<impl IntoResponse, WireError> {
    store.cleanup_expired();
    let (expected_len, reservation) = {
        let mut map = store.inner.entries.lock().map_err(|_| internal())?;
        match map.get_mut(&id) {
            Some(Entry::Upload(u))
                if u.expires > OfficialFileStore::now()
                    && u.file_id.is_none()
                    && u.bytes.is_none()
                    && !u.uploading =>
            {
                if u.request.bytes > store.file_cap() || u.request.bytes > store.body_cap() as u64 {
                    return Err(WireError::too_large());
                }
                u.uploading = true;
                (
                    u.request.bytes,
                    u.reservation.as_ref().cloned().ok_or_else(internal)?,
                )
            }
            Some(Entry::Upload(_)) => {
                return Err(WireError {
                    status: StatusCode::CONFLICT,
                    code: "upload_conflict",
                    message: "Upload is not writable.",
                    param: None,
                });
            }
            _ => return Err(not_found()),
        }
    };
    let mut writer = WriterGuard {
        store: store.clone(),
        id: id.clone(),
        reservation,
        active: true,
    };
    if headers
        .get("content-length")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok())
        .is_some_and(|n| n != expected_len)
    {
        return Err(WireError::bad(
            "content-length",
            "Content length does not match upload.",
        ));
    }
    let mut stream = body.into_data_stream();
    let mut data = Vec::with_capacity(expected_len as usize);
    let mut hash = Sha256::new();
    let deadline = tokio::time::sleep(std::time::Duration::from_secs(600));
    tokio::pin!(deadline);
    loop {
        let next = tokio::select! { chunk = stream.next() => chunk, _ = &mut deadline => return Err(WireError { status: StatusCode::REQUEST_TIMEOUT, code: "upload_timeout", message: "Upload deadline exceeded.", param: None }) };
        let Some(chunk) = next else { break };
        let chunk = match chunk {
            Ok(v) => v,
            Err(_) => return Err(WireError::bad("body", "Upload body was interrupted.")),
        };
        if data.len().saturating_add(chunk.len()) as u64 > expected_len {
            return Err(WireError::too_large());
        }
        hash.update(&chunk);
        data.extend_from_slice(&chunk);
    }
    if data.len() as u64 != expected_len {
        return Err(WireError::bad(
            "body",
            "Upload length does not match declaration.",
        ));
    }
    let digest = hash
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>();
    let mut map = store.inner.entries.lock().map_err(|_| internal())?;
    match map.get_mut(&id) {
        Some(Entry::Upload(u)) if u.request.sha256sum.as_ref().is_none_or(|v| v == &digest) => {
            u.bytes = Some(store.charged(data, writer.reservation.clone()));
            u.request.sha256sum = Some(digest);
            u.reservation = None;
            u.uploading = false;
            writer.active = false;
            Ok(StatusCode::OK)
        }
        _ => Err(WireError::bad("sha256sum", "Upload digest does not match.")),
    }
}

struct WriterGuard {
    store: OfficialFileStore,
    id: String,
    reservation: std::sync::Arc<super::store::Reservation>,
    active: bool,
}
impl Drop for WriterGuard {
    fn drop(&mut self) {
        if !self.active {
            return;
        }
        let removed = self
            .store
            .inner
            .entries
            .lock()
            .ok()
            .and_then(|mut map| map.remove(&self.id));
        drop(removed);
    }
}

pub(super) async fn complete(
    State(store): State<OfficialFileStore>,
    Path(id): Path<String>,
    body: Result<Option<Json<Completion>>, JsonRejection>,
) -> Result<Json<UploadResponse>, WireError> {
    store.cleanup_expired();
    let digest = body
        .map_err(|_| WireError::bad("body", "Invalid completion request."))?
        .and_then(|v| v.0.sha256sum);
    let mut map = store.inner.entries.lock().map_err(|_| internal())?;
    let Some(Entry::Upload(u)) = map.get(&id) else {
        return Err(not_found());
    };
    if u.expires <= OfficialFileStore::now() {
        return Err(not_found());
    }
    if let Some(file_id) = &u.file_id {
        let req = u.request.clone();
        let (created, expires) = (u.created, u.expires);
        let file = match map.get(file_id) {
            Some(Entry::File(f)) => f.object.clone(),
            _ => return Err(not_found()),
        };
        if digest
            .as_ref()
            .is_some_and(|d| req.sha256sum.as_ref() != Some(d))
        {
            return Err(WireError::bad("sha256sum", "Upload digest does not match."));
        }
        return Ok(Json(upload_response(
            id,
            &req,
            created,
            expires,
            Some(file),
        )));
    }
    let (req, created, expires, bytes) = (u.request.clone(), u.created, u.expires, u.bytes.clone());
    let Some(bytes) = bytes else {
        return Err(WireError {
            status: StatusCode::CONFLICT,
            code: "upload_incomplete",
            message: "Upload content is missing.",
            param: None,
        });
    };
    if digest
        .as_ref()
        .is_some_and(|d| req.sha256sum.as_ref() != Some(d))
    {
        return Err(WireError::bad("sha256sum", "Upload digest does not match."));
    }
    let file_id = store.next_id("file");
    let object = crate::official_api::wire::FileObject {
        id: file_id.clone(),
        object: "file",
        bytes: req.bytes,
        created_at: created,
        expires_at: Some(expires),
        filename: req.filename.clone(),
        purpose: req.purpose.clone(),
        sha256sum: req.sha256sum.clone(),
    };
    let metadata = match map.get(&id) {
        Some(Entry::Upload(u)) => u.metadata.clone(),
        _ => return Err(not_found()),
    };
    map.insert(
        file_id.clone(),
        Entry::File(File {
            object: object.clone(),
            bytes,
            source: true,
            _metadata: Some(metadata),
        }),
    );
    if let Some(Entry::Upload(upload)) = map.get_mut(&id) {
        upload.file_id = Some(file_id);
        upload.bytes = None;
    }
    Ok(Json(upload_response(
        id,
        &req,
        created,
        expires,
        Some(object),
    )))
}

pub(super) async fn cancel(
    State(store): State<OfficialFileStore>,
    Path(id): Path<String>,
) -> Result<Json<UploadResponse>, WireError> {
    store.cleanup_expired();
    let mut map = store.inner.entries.lock().map_err(|_| internal())?;
    if !matches!(map.get(&id), Some(Entry::Upload(_))) {
        return Err(not_found());
    }
    if matches!(map.get(&id), Some(Entry::Upload(u)) if u.file_id.is_some()) {
        return Err(WireError {
            status: StatusCode::CONFLICT,
            code: "upload_conflict",
            message: "Completed uploads cannot be canceled.",
            param: None,
        });
    }
    let Some(Entry::Upload(u)) = map.remove(&id) else {
        return Err(not_found());
    };
    let mut response = upload_response(id, &u.request, u.created, u.expires, None);
    response.status = "canceled".into();
    response.upload_url = None;
    response.upload_method = None;
    response.upload_headers.clear();
    Ok(Json(response))
}

fn not_found() -> WireError {
    WireError {
        status: StatusCode::NOT_FOUND,
        code: "not_found",
        message: "The requested resource was not found.",
        param: None,
    }
}
fn internal() -> WireError {
    WireError {
        status: StatusCode::INTERNAL_SERVER_ERROR,
        code: "server_error",
        message: "The request could not be processed.",
        param: None,
    }
}

#[cfg(test)]
#[path = "uploads/tests.rs"]
mod tests;
