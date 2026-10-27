// Task result assembly: inlines parsed outputs into the JSON task payload or
// streams a zip, reading retained artifact bytes from the store.
use super::result_helpers::{internal, not_found};
use super::task_payload;
use crate::official_api::server::service::ServiceInner;
use crate::official_api::server::store::Entry;
use crate::official_api::wire::{JobResponse, OutputReference, WireError};
use axum::{
    Json,
    body::Body,
    http::{HeaderName, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use bytes::Bytes;
use std::collections::BTreeMap;
use std::sync::Arc;

pub(in crate::official_api::server) async fn serve_result(
    s: Arc<ServiceInner>,
    id: String,
    format: Option<&str>,
) -> Result<Response, WireError> {
    let record = s
        .jobs
        .lock()
        .map_err(|_| internal())?
        .get(&id)
        .cloned()
        .ok_or_else(not_found)?;
    let payload = record.lock().await.response.clone();
    let is_zip = format == Some("zip");
    if !matches!(
        payload.status.as_str(),
        "completed" | "partial" | "failed" | "canceled"
    ) {
        return Ok((StatusCode::ACCEPTED, Json(task_payload(&payload))).into_response());
    }
    if matches!(payload.status.as_str(), "failed" | "canceled") {
        return Ok((StatusCode::CONFLICT, Json(task_payload(&payload))).into_response());
    }
    if is_zip {
        return zip_response(&s, &id, payload).await;
    }
    let mut value = task_payload(&payload);
    let object = value.as_object_mut().expect("serialized object");
    let files = object
        .get_mut("files")
        .and_then(|v| v.as_array_mut())
        .expect("files array");
    // Every file gets a `content` object; only completed files inline the
    // requested text formats (zip artifacts are never read here).
    for (index, file) in payload.files.iter().enumerate() {
        let entry = files
            .get_mut(index)
            .and_then(|v| v.as_object_mut())
            .expect("file object");
        if file.status != "completed" {
            entry.insert("content".into(), serde_json::json!({}));
            continue;
        }
        let output_files = file.output_files.as_ref().ok_or_else(invalid_response)?;
        let content = inline_content(&s, output_files, &payload.output_formats)?;
        entry.insert("content".into(), content);
    }
    Ok(Json(value).into_response())
}

/// Inline the requested text formats by format name: markdown as UTF-8 text,
/// middle_json and structured_content as parsed JSON. Zip is skipped.
fn inline_content(
    s: &ServiceInner,
    output_files: &BTreeMap<String, OutputReference>,
    formats: &[crate::official_api::wire::OutputFormat],
) -> Result<serde_json::Value, WireError> {
    use crate::official_api::wire::OutputFormat;
    let mut content = serde_json::Map::new();
    for format in formats {
        let name: &str = match format {
            OutputFormat::Markdown => "markdown",
            OutputFormat::MiddleJson => "middle_json",
            OutputFormat::StructuredContent => "structured_content",
            // Zip artifacts are never inlined into the json response.
            OutputFormat::Zip => continue,
        };
        let reference = output_files.get(name).ok_or_else(invalid_response)?;
        let bytes = artifact_bytes(s, &reference.file_id)?;
        let value = match name {
            "markdown" => serde_json::Value::String(String::from_utf8_lossy(&bytes).into_owned()),
            _ => serde_json::from_slice::<serde_json::Value>(&bytes)
                .map_err(|_| invalid_response())?,
        };
        content.insert(name.into(), value);
    }
    Ok(serde_json::Value::Object(content))
}

fn artifact_bytes(s: &ServiceInner, file_id: &str) -> Result<Bytes, WireError> {
    match s
        .store
        .inner
        .entries
        .lock()
        .map_err(|_| internal())?
        .get(file_id)
    {
        Some(Entry::File(file)) => Ok(file.bytes.clone()),
        _ => Err(invalid_response()),
    }
}

async fn zip_response(
    s: &ServiceInner,
    id: &str,
    payload: JobResponse,
) -> Result<Response, WireError> {
    if !payload
        .output_formats
        .contains(&crate::official_api::wire::OutputFormat::Zip)
    {
        return Err(WireError {
            status: StatusCode::BAD_REQUEST,
            code: "unsupported_output_format",
            message: "Zip output was not requested for this task.",
            param: None,
        });
    }
    // Preserve original file indices: prefix members with {index+1:04d}/ so
    // a combined zip matches the task payload's file order.
    let mut sources: Vec<(usize, Bytes)> = Vec::new();
    for (index, file) in payload.files.iter().enumerate() {
        if file.status != "completed" {
            continue;
        }
        let reference = file
            .output_files
            .as_ref()
            .and_then(|refs| refs.get("zip"))
            .ok_or_else(invalid_response)?;
        sources.push((index, artifact_bytes(s, &reference.file_id)?));
    }
    let bytes = if payload.files.len() == 1 {
        sources.into_iter().next().expect("one zip").1
    } else {
        // The archive scanner's production entry limit is 100,000; retain it
        // across all inputs and the generated manifest.
        combine_zips(&payload, &sources, s.store.zip_cap(), 100_000)?
    };
    zip_http(id, &payload.status, bytes)
}

fn combine_zips(
    payload: &JobResponse,
    sources: &[(usize, Bytes)],
    cap: u64,
    max_entries: u64,
) -> Result<Bytes, WireError> {
    use std::io::{Read, Write};
    let manifest = serde_json::to_vec(&task_payload(payload)).map_err(|_| invalid_response())?;
    let mut expanded = u64::try_from(manifest.len()).map_err(|_| invalid_response())?;
    let mut entries = 1u64;
    for (_, source) in sources {
        let mut archive = zip::ZipArchive::new(std::io::Cursor::new(&source[..]))
            .map_err(|_| invalid_response())?;
        entries = entries
            .checked_add(archive.len() as u64)
            .ok_or_else(invalid_response)?;
        for item in 0..archive.len() {
            let file = archive.by_index(item).map_err(|_| invalid_response())?;
            expanded = expanded
                .checked_add(file.size())
                .ok_or_else(invalid_response)?;
            if expanded > cap || entries > max_entries {
                return Err(invalid_response());
            }
        }
    }
    if expanded > cap || entries > max_entries {
        return Err(invalid_response());
    }
    let sink = BoundedCursor::new(cap);
    let mut writer = zip::ZipWriter::new(sink);
    writer
        .start_file("manifest.json", zip::write::SimpleFileOptions::default())
        .map_err(|_| invalid_response())?;
    writer
        .write_all(&manifest)
        .map_err(|_| invalid_response())?;
    let mut copied_expanded = u64::try_from(manifest.len()).map_err(|_| invalid_response())?;
    for (index, source) in sources {
        let prefix = format!("{:04}/", index + 1);
        let mut reader = zip::ZipArchive::new(std::io::Cursor::new(&source[..]))
            .map_err(|_| invalid_response())?;
        for i in 0..reader.len() {
            let entry = reader.by_index(i).map_err(|_| invalid_response())?;
            let name = entry.name().map_err(|_| invalid_response())?.into_owned();
            if name.starts_with('/') || name.contains('\\') || name.contains("..") {
                return Err(invalid_response());
            }
            writer
                .start_file(
                    format!("{prefix}{name}"),
                    zip::write::SimpleFileOptions::default(),
                )
                .map_err(|_| invalid_response())?;
            let entry_size = entry.size();
            let copied = std::io::copy(&mut entry.take(entry_size.saturating_add(1)), &mut writer)
                .map_err(|_| invalid_response())?;
            if copied != entry_size {
                return Err(invalid_response());
            }
            copied_expanded = copied_expanded
                .checked_add(copied)
                .ok_or_else(invalid_response)?;
            if copied_expanded > cap {
                return Err(invalid_response());
            }
        }
    }
    let cursor = writer.finish().map_err(|_| invalid_response())?;
    Ok(Bytes::from(cursor.inner.into_inner()))
}

struct BoundedCursor {
    inner: std::io::Cursor<Vec<u8>>,
    cap: u64,
}
impl BoundedCursor {
    fn new(cap: u64) -> Self {
        Self {
            inner: std::io::Cursor::new(Vec::new()),
            cap,
        }
    }
}
impl std::io::Write for BoundedCursor {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        let end = self
            .inner
            .position()
            .checked_add(data.len() as u64)
            .filter(|end| *end <= self.cap)
            .ok_or_else(|| std::io::Error::other("zip output limit exceeded"))?;
        let n = std::io::Write::write(&mut self.inner, data)?;
        debug_assert!(self.inner.position() <= end);
        Ok(n)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        std::io::Write::flush(&mut self.inner)
    }
}
impl std::io::Seek for BoundedCursor {
    fn seek(&mut self, pos: std::io::SeekFrom) -> std::io::Result<u64> {
        std::io::Seek::seek(&mut self.inner, pos)
    }
}

#[cfg(test)]
#[path = "result_tests.rs"]
mod tests;

fn zip_http(id: &str, status: &str, bytes: Bytes) -> Result<Response, WireError> {
    let disposition = HeaderValue::from_str(&format!("attachment; filename=\"{id}.zip\""))
        .map_err(|_| internal())?;
    let mut response = Response::new(Body::from(bytes));
    *response.status_mut() = StatusCode::OK;
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/zip"),
    );
    response
        .headers_mut()
        .insert(header::CONTENT_DISPOSITION, disposition);
    response.headers_mut().insert(
        HeaderName::from_static("x-mineru-task-id"),
        HeaderValue::from_str(id).map_err(|_| internal())?,
    );
    let status_value = HeaderValue::from_str(status).map_err(|_| internal())?;
    response.headers_mut().insert(
        HeaderName::from_static("x-mineru-task-status"),
        status_value,
    );
    Ok(response)
}

fn invalid_response() -> WireError {
    WireError {
        status: StatusCode::BAD_GATEWAY,
        code: "invalid_response",
        message: "The task artifacts are unavailable.",
        param: None,
    }
}
