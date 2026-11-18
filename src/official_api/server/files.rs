use super::store::OfficialFileStore;
use crate::official_api::wire::{FileObjectModel, WireError};
use axum::{
    Json,
    body::Body,
    extract::{Path, State},
    http::{HeaderValue, StatusCode, header},
    response::Response,
};

pub(super) async fn get(
    State(store): State<OfficialFileStore>,
    Path(id): Path<String>,
) -> Result<Json<FileObjectModel>, WireError> {
    store.cleanup_expired();
    store.file(&id).map(Json).ok_or_else(not_found)
}

pub(super) async fn content(
    State(store): State<OfficialFileStore>,
    Path(id): Path<String>,
) -> Result<Response, WireError> {
    store.cleanup_expired();
    let (object, bytes) = store.content(&id).ok_or_else(not_found)?;
    let safe: String = object
        .file_name
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
        .take(128)
        .collect();
    let disposition = HeaderValue::from_str(&format!(
        "attachment; filename=\"{}\"",
        if safe.is_empty() { "artifact" } else { &safe }
    ))
    .map_err(|_| WireError::internal())?;
    let mut response = Response::new(Body::from(bytes));
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static(media_type(&object.file_name)),
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

fn media_type(name: &str) -> &'static str {
    match name
        .rsplit('.')
        .next()
        .unwrap_or("")
        .to_ascii_lowercase()
        .as_str()
    {
        "zip" => "application/zip",
        "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "html" => "text/html",
        "tex" => "text/plain; charset=utf-8",
        "md" | "json" => "application/octet-stream",
        _ => "application/octet-stream",
    }
}

fn not_found() -> WireError {
    WireError::new(
        StatusCode::NOT_FOUND,
        "file_not_found",
        "The requested file was not found.",
        None,
    )
}
