use super::store::{Entry, OfficialFileStore};
use crate::official_api::wire::{FileDeletion, FileObject, ListOrder, ListResponse, WireError};
use axum::{
    Json,
    body::Body,
    extract::{Path, Query, State, rejection::QueryRejection},
    http::{HeaderValue, StatusCode, header},
    response::Response,
};
use serde::Deserialize;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct FileQuery {
    #[serde(default)]
    after: Option<String>,
    #[serde(default)]
    limit: Option<u16>,
    #[serde(default)]
    order: Option<ListOrder>,
    #[serde(default)]
    purpose: Option<String>,
}

pub(super) async fn list(
    State(store): State<OfficialFileStore>,
    query: Result<Query<FileQuery>, QueryRejection>,
) -> Result<Json<ListResponse<FileObject>>, WireError> {
    store.cleanup_expired();
    let Query(query) = query.map_err(|_| WireError::bad("query", "Invalid file list query."))?;
    if query
        .purpose
        .as_ref()
        .is_some_and(|p| !matches!(p.as_str(), "parse" | "input_image"))
    {
        return Err(WireError::bad("purpose", "Unsupported file purpose."));
    }
    let page = crate::official_api::wire::Pagination {
        after: query.after,
        limit: query.limit,
        order: query.order,
    };
    let limit = page.validate(100, 1000)? as usize;
    let map = store.inner.entries.lock().map_err(|_| internal())?;
    let mut data: Vec<_> = map
        .values()
        .filter_map(|v| match v {
            Entry::File(f)
                if query
                    .purpose
                    .as_ref()
                    .is_none_or(|p| p == &f.object.purpose) =>
            {
                Some(f.object.clone())
            }
            _ => None,
        })
        .collect();
    data.sort_by(|a, b| (a.created_at, &a.id).cmp(&(b.created_at, &b.id)));
    if matches!(page.order, Some(ListOrder::Desc)) {
        data.reverse();
    }
    if let Some(after) = &page.after
        && let Some(i) = data.iter().position(|f| &f.id == after)
    {
        data.drain(..=i);
    }
    let has_more = data.len() > limit;
    data.truncate(limit);
    let first_id = data.first().map(|f| f.id.clone());
    let last_id = data.last().map(|f| f.id.clone());
    Ok(Json(ListResponse {
        object: "list",
        data,
        first_id,
        last_id,
        has_more,
    }))
}
pub(super) async fn get(
    State(store): State<OfficialFileStore>,
    Path(id): Path<String>,
) -> Result<Json<FileObject>, WireError> {
    store.cleanup_expired();
    store.file(&id).map(Json).ok_or_else(not_found)
}
pub(super) async fn delete(
    State(store): State<OfficialFileStore>,
    Path(id): Path<String>,
) -> Result<Json<FileDeletion>, WireError> {
    store.cleanup_expired();
    let entry = store
        .inner
        .entries
        .lock()
        .map_err(|_| internal())?
        .remove(&id)
        .ok_or_else(not_found)?;
    match entry {
        Entry::File(_) => Ok(Json(FileDeletion {
            id,
            object: "file",
            deleted: true,
        })),
        other => {
            store
                .inner
                .entries
                .lock()
                .map_err(|_| internal())?
                .insert(id, other);
            Err(not_found())
        }
    }
}
pub(super) async fn content(
    State(store): State<OfficialFileStore>,
    Path(id): Path<String>,
) -> Result<Response, WireError> {
    store.cleanup_expired();
    let (bytes, name, source) = {
        let map = store.inner.entries.lock().map_err(|_| internal())?;
        match map.get(&id) {
            Some(Entry::File(f)) => (f.bytes.clone(), f.object.filename.clone(), f.source),
            _ => return Err(not_found()),
        }
    };
    if source {
        return Err(WireError {
            status: StatusCode::FORBIDDEN,
            code: "file_not_downloadable",
            message: "Source file content is not available for download.",
            param: None,
        });
    }
    let safe: String = name
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
        .take(128)
        .collect();
    let disposition = HeaderValue::from_str(&format!(
        "attachment; filename=\"{}\"",
        if safe.is_empty() { "artifact" } else { &safe }
    ))
    .map_err(|_| internal())?;
    let mut response = Response::new(Body::from(bytes));
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/octet-stream"),
    );
    response
        .headers_mut()
        .insert(header::CONTENT_DISPOSITION, disposition);
    response.headers_mut().insert(
        "x-content-type-options",
        HeaderValue::from_static("nosniff"),
    );
    Ok(response)
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
#[path = "files/tests.rs"]
mod tests;
