// MinerU 4.x convenience Task API over the same job pipeline as
// /v1/parse/jobs: POST /v1/tasks, GET/DELETE /v1/tasks/{id},
// GET /v1/tasks/{id}/result, POST /v1/file_parse.
use super::{
    jobs::prepare_job,
    service::ServiceInner,
    submission::{InputSource, Submission, SubmittedFile},
};
use crate::official_api::wire::{JobCreate, JobResponse, OutputFormat, Tier, WireError};
use axum::{
    Json, Router,
    extract::{FromRequest, Path, Query, State, multipart::Multipart},
    http::{StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use sha2::Digest;
use std::sync::Arc;

mod result;
pub(super) mod result_helpers;
use result::serve_result;

mod query;
use query::{IngestQuery, parse_query};

#[cfg(test)]
#[path = "tasks/tests.rs"]
mod tests;

pub(super) fn router(state: Arc<ServiceInner>) -> Router {
    let body_cap = state.store.ingest_body_cap();
    Router::new()
        .route("/v1/tasks", post(create))
        .route("/v1/tasks/{id}", get(status).delete(cancel))
        .route("/v1/tasks/{id}/result", get(task_result))
        .route("/v1/file_parse", post(file_parse))
        .with_state(state)
        .layer(axum::extract::DefaultBodyLimit::max(body_cap))
}
fn parse_response_format(value: Option<&str>) -> Result<&'static str, WireError> {
    match value {
        None | Some("json") => Ok("json"),
        Some("zip") => Ok("zip"),
        Some(_) => Err(WireError::bad(
            "response_format",
            "Unsupported response format.",
        )),
    }
}

/// Wrap the shared `JobResponse` into a task payload: `job_id` renamed to
/// `task_id`, status/result URLs added, links replaced with task links.
pub(in crate::official_api::server) fn task_payload(job: &JobResponse) -> serde_json::Value {
    let mut value = serde_json::to_value(job).unwrap_or_default();
    let object = value.as_object_mut().expect("serialized object");
    let id = object
        .get("job_id")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_owned();
    object.remove("job_id");
    object.insert("task_id".into(), serde_json::Value::String(id.clone()));
    object.insert(
        "status_url".into(),
        serde_json::json!(format!("/v1/tasks/{id}")),
    );
    object.insert(
        "result_url".into(),
        serde_json::json!(format!("/v1/tasks/{id}/result")),
    );
    object.insert(
        "links".into(),
        serde_json::json!({
            "self": format!("/v1/tasks/{id}"),
            "cancel": format!("/v1/tasks/{id}")
        }),
    );
    value
}

fn stopping(s: &ServiceInner) -> Option<WireError> {
    s.stopping
        .load(std::sync::atomic::Ordering::Acquire)
        .then_some(WireError {
            status: StatusCode::SERVICE_UNAVAILABLE,
            code: "server_shutting_down",
            message: "The server is shutting down.",
            param: None,
        })
}

async fn create(
    State(s): State<Arc<ServiceInner>>,
    request: axum::http::Request<axum::body::Body>,
) -> Result<Response, WireError> {
    if let Some(error) = stopping(&s) {
        return Err(error);
    }
    let _ingest = ingest_permit(&s)?;
    let submission = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        submission_from_request(&s, request),
    )
    .await
    .map_err(|_| ingest_timeout())??;
    let result = submit(&s, submission).await;
    drop(_ingest);
    result
}

pub(super) fn ingest_permit(
    s: &ServiceInner,
) -> Result<tokio::sync::OwnedSemaphorePermit, WireError> {
    s.ingest_slots
        .clone()
        .try_acquire_owned()
        .map_err(|_| WireError {
            status: StatusCode::TOO_MANY_REQUESTS,
            code: "capacity_exceeded",
            message: "Request body capacity is exhausted.",
            param: None,
        })
}

pub(super) fn ingest_timeout() -> WireError {
    WireError {
        status: StatusCode::REQUEST_TIMEOUT,
        code: "body_timeout",
        message: "Request body was not received before the deadline.",
        param: None,
    }
}

/// Submit through the shared admission pipeline and return HTTP 202 with the
/// task payload.
async fn submit(s: &Arc<ServiceInner>, submission: Submission) -> Result<Response, WireError> {
    if let Some(error) = stopping(s) {
        return Err(error);
    }
    let (_, Json(job)) = prepare_job(s, submission)?;
    let payload = task_payload(&job);
    Ok((StatusCode::ACCEPTED, Json(payload)).into_response())
}

/// Build a `Submission` from the request body: JSON, multipart/form-data
/// (repeated `files` fields), or application/octet-stream (single document,
/// options from the query string).
async fn submission_from_request(
    s: &ServiceInner,
    request: axum::http::Request<axum::body::Body>,
) -> Result<Submission, WireError> {
    let content_type = request
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_ascii_lowercase();
    if content_type.starts_with("application/json") || content_type.is_empty() {
        // JSON options live in the body; only file_parse's control keys may
        // ride along in the query string.
        let query = parse_query(request.uri().query())?;
        if query.tier.is_some()
            || query.ocr_mode.is_some()
            || query.page_range.is_some()
            || !query.output_formats.is_empty()
            || query.filename.is_some()
            || query.sha256sum.is_some()
        {
            return Err(WireError::bad(
                "query",
                "JSON parse options must be supplied in the request body.",
            ));
        }
        let bytes = read_body(request.into_body(), s.body_limit()).await?;
        let create: JobCreate = serde_json::from_slice(&bytes)
            .map_err(|_| WireError::bad("body", "Invalid task request."))?;
        return Ok(Submission::from_create(create));
    }
    if content_type.starts_with("multipart/form-data") {
        let query = parse_query(request.uri().query())?;
        if query.filename.is_some() || query.sha256sum.is_some() {
            return Err(WireError::bad(
                "query",
                "filename and sha256sum are not supported with multipart submissions.",
            ));
        }
        let mut multipart = Multipart::from_request(request, &())
            .await
            .map_err(multipart_rejection)?;
        let mut files = Vec::new();
        while let Some(field) = multipart
            .next_field()
            .await
            .map_err(|_| WireError::bad("body", "Invalid multipart body."))?
        {
            if field.name() != Some("files") {
                return Err(WireError::bad("files", "Only files fields are accepted."));
            }
            let Some(filename) = field.file_name() else {
                return Err(WireError::bad(
                    "files",
                    "Every files field needs a filename.",
                ));
            };
            let name = filename.trim().to_owned();
            let data = field.bytes().await.map_err(|_| WireError::too_large())?;
            if data.is_empty() {
                return Err(WireError::bad("files", "Empty files are not supported."));
            }
            files.push(SubmittedFile {
                source: InputSource::Raw { name, bytes: data },
                page_range: query.page_range.clone(),
            });
            if files.len() > 100 {
                return Err(WireError::bad("files", "Expected between 1 and 100 files."));
            }
        }
        return submission_from_files(files, &query);
    }
    if content_type.starts_with("application/octet-stream") {
        let query = parse_query(request.uri().query())?;
        // Declared length must be present and within the configured cap;
        // this is the store limit, not upstream's hardcoded 200 MiB literal.
        let declared = request
            .headers()
            .get(header::CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<u64>().ok())
            .ok_or(WireError {
                status: StatusCode::LENGTH_REQUIRED,
                code: "length_required",
                message: "A Content-Length header is required.",
                param: None,
            })?;
        if declared == 0 || declared > s.store.file_cap() {
            return Err(WireError::too_large());
        }
        let name = query
            .filename
            .clone()
            .filter(|name| !name.trim().is_empty())
            .ok_or_else(|| WireError::bad("filename", "A filename parameter is required."))?;
        let data = read_body(request.into_body(), s.body_limit()).await?;
        if data.is_empty() {
            return Err(WireError::bad("files", "Empty files are not supported."));
        }
        if data.len() as u64 != declared {
            return Err(WireError {
                status: StatusCode::BAD_REQUEST,
                code: "upload_size_mismatch",
                message: "Body length does not match the declared Content-Length.",
                param: None,
            });
        }
        if let Some(expected) = &query.sha256sum {
            let digest: String = sha2::Sha256::digest(&data)
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect();
            if expected != &digest {
                return Err(WireError::bad("sha256sum", "Digest does not match."));
            }
        }
        return submission_from_files(
            vec![SubmittedFile {
                source: InputSource::Raw { name, bytes: data },
                page_range: query.page_range.clone(),
            }],
            &query,
        );
    }
    Err(WireError {
        status: StatusCode::UNSUPPORTED_MEDIA_TYPE,
        code: "unsupported_media_type",
        message: "Expected application/json, multipart/form-data, or application/octet-stream.",
        param: None,
    })
}

fn multipart_rejection(_: axum::extract::multipart::MultipartRejection) -> WireError {
    WireError::bad("body", "Invalid multipart body.")
}

fn submission_from_files(
    files: Vec<SubmittedFile>,
    query: &IngestQuery,
) -> Result<Submission, WireError> {
    Ok(Submission {
        files,
        tier: query.tier.as_deref().map(parse_tier).transpose()?,
        ocr_mode: query
            .ocr_mode
            .as_deref()
            .map(parse_ocr_mode)
            .transpose()?
            .unwrap_or_default(),
        output_formats: parse_formats(&query.output_formats)?,
    })
}

async fn status(
    State(s): State<Arc<ServiceInner>>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, WireError> {
    let payload = super::jobs::get(State(s), Path(id)).await?;
    Ok(Json(task_payload(&payload)))
}

async fn cancel(
    State(s): State<Arc<ServiceInner>>,
    Path(id): Path<String>,
) -> Result<Response, WireError> {
    // Cancel through the shared job pipeline; then return the task payload.
    let _ = super::jobs::cancel(State(s.clone()), Path(id.clone())).await?;
    let payload = super::jobs::get(State(s), Path(id)).await?;
    Ok(Json(task_payload(&payload)).into_response())
}

async fn task_result(
    State(s): State<Arc<ServiceInner>>,
    Path(id): Path<String>,
    query: axum::extract::RawQuery,
) -> Result<Response, WireError> {
    let query = parse_query(query.0.as_deref())?;
    let format = parse_response_format(query.response_format.as_deref())?;
    serve_result(s, id, Some(format)).await
}

#[derive(serde::Deserialize)]
struct FileParseQuery {
    #[serde(default)]
    response_format: Option<String>,
    #[serde(default)]
    wait_timeout: Option<u64>,
}

async fn file_parse(
    State(s): State<Arc<ServiceInner>>,
    Query(query): Query<FileParseQuery>,
    request: axum::http::Request<axum::body::Body>,
) -> Result<Response, WireError> {
    if let Some(error) = stopping(&s) {
        return Err(error);
    }
    let timeout = query.wait_timeout.unwrap_or(300);
    if !(1..=3600).contains(&timeout) {
        return Err(WireError::bad(
            "wait_timeout",
            "wait_timeout must be between 1 and 3600 seconds.",
        ));
    }
    let format = parse_response_format(query.response_format.as_deref())?;
    // JSON bodies reject parse options in the query string inside
    // `submission_from_request`; multipart and octet-stream legitimately read
    // their upload options from the query string.
    let _ingest = ingest_permit(&s)?;
    let mut submission = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        submission_from_request(&s, request),
    )
    .await
    .map_err(|_| ingest_timeout())??;
    // Upstream file_parse forces a zip output when the caller asks for one.
    if format == "zip" && !submission.output_formats.contains(&OutputFormat::Zip) {
        submission.output_formats.push(OutputFormat::Zip);
    }
    let (_, Json(job)) = prepare_job(&s, submission)?;
    drop(_ingest);
    let id = job.job_id.clone();
    wait_for_terminal(&s, &id, timeout).await;
    match serve_result(s.clone(), id.clone(), Some(format)).await {
        Ok(response) if response.status() != StatusCode::ACCEPTED => Ok(response),
        Ok(_) => {
            // Timeout elapsed: return the latest task payload without
            // cancelling the background job.
            let payload = super::jobs::get(State(s), Path(id)).await?;
            Ok((StatusCode::ACCEPTED, Json(task_payload(&payload))).into_response())
        }
        Err(error) => Err(error),
    }
}

/// Bounded poll for a terminal state; never holds the record lock across a
/// sleep and never disturbs the job on timeout.
async fn wait_for_terminal(s: &Arc<ServiceInner>, id: &str, timeout: u64) {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(timeout);
    loop {
        let status = {
            let record = s.jobs.lock().ok().and_then(|j| j.get(id).cloned());
            match record {
                Some(record) => record.lock().await.response.status.clone(),
                None => return,
            }
        };
        if matches!(
            status.as_str(),
            "completed" | "partial" | "failed" | "canceled"
        ) {
            return;
        }
        if tokio::time::Instant::now() >= deadline {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}

fn parse_tier(value: &str) -> Result<Tier, WireError> {
    match value {
        "flash" => Ok(Tier::Flash),
        "basic" => Ok(Tier::Basic),
        "standard" => Ok(Tier::Standard),
        "advanced" => Ok(Tier::Advanced),
        _ => Err(WireError::bad("tier", "Unsupported tier.")),
    }
}

fn parse_ocr_mode(value: &str) -> Result<crate::official_api::wire::OcrMode, WireError> {
    match value {
        "auto" => Ok(crate::official_api::wire::OcrMode::Auto),
        "txt" => Ok(crate::official_api::wire::OcrMode::Txt),
        "ocr" => Ok(crate::official_api::wire::OcrMode::Ocr),
        _ => Err(WireError::bad("ocr_mode", "Unsupported OCR mode.")),
    }
}

fn parse_formats(values: &[String]) -> Result<Vec<OutputFormat>, WireError> {
    if values.is_empty() {
        return Ok(vec![OutputFormat::Markdown]);
    }
    values
        .iter()
        .map(|value| match value.as_str() {
            "markdown" => Ok(OutputFormat::Markdown),
            "middle_json" => Ok(OutputFormat::MiddleJson),
            "structured_content" => Ok(OutputFormat::StructuredContent),
            "zip" => Ok(OutputFormat::Zip),
            _ => Err(WireError::bad(
                "output_formats",
                "Unsupported output format.",
            )),
        })
        .collect()
}

async fn read_body(body: axum::body::Body, limit: usize) -> Result<bytes::Bytes, WireError> {
    axum::body::to_bytes(body, limit)
        .await
        .map_err(|_| WireError::too_large())
}

impl ServiceInner {
    fn body_limit(&self) -> usize {
        self.store.ingest_body_cap()
    }
}
