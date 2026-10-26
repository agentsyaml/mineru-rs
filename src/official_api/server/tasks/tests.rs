// Tests for the MinerU 4.x convenience Task API under `/v1/tasks`.
use super::*;
use crate::official::OfficialResult;
use crate::official_api::server::jobs::output::publish_outputs;
use crate::official_api::server::store::OfficialFileStore;
use std::sync::Arc;

fn store() -> OfficialFileStore {
    OfficialFileStore::new(
        Arc::new(tokio::sync::Semaphore::new(2)),
        Arc::new(tokio::sync::Semaphore::new(1)),
        std::time::Duration::from_secs(3600),
    )
    .unwrap()
}

fn service(store: OfficialFileStore) -> super::super::service::OfficialService {
    super::super::service::OfficialService::new(crate::official::OfficialRuntime::new(), store)
        .unwrap()
}

#[test]
fn ingest_admission_is_bounded_and_released() {
    let service = service(store());
    let first = ingest_permit(&service.inner).unwrap();
    let second = ingest_permit(&service.inner).unwrap();
    assert_eq!(
        ingest_permit(&service.inner).unwrap_err().code,
        "capacity_exceeded"
    );
    drop(first);
    assert!(ingest_permit(&service.inner).is_ok());
    drop(second);
}

#[tokio::test]
async fn stalled_body_times_out_and_releases_ingest_admission() {
    let service = service(store());
    let body = axum::body::Body::from_stream(futures_util::stream::once(std::future::pending::<
        Result<bytes::Bytes, std::convert::Infallible>,
    >()));
    let permit = ingest_permit(&service.inner).unwrap();
    let read = axum::body::to_bytes(body, 1024);
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(10), read)
            .await
            .is_err()
    );
    drop(permit);
    assert!(ingest_permit(&service.inner).is_ok());
}

#[tokio::test]
async fn canceled_ingest_task_releases_admission() {
    let service = service(store());
    let inner = service.inner.clone();
    let task = tokio::spawn(async move {
        let _permit = ingest_permit(&inner).unwrap();
        std::future::pending::<()>().await;
    });
    task.abort();
    let _ = task.await;
    assert!(ingest_permit(&service.inner).is_ok());
}

#[tokio::test]
async fn job_route_rejects_non_json_content_type() {
    let service = service(store());
    let request = axum::http::Request::builder()
        .method("POST")
        .header("content-type", "text/plain")
        .body(axum::body::Body::from("{}"))
        .unwrap();
    let error = super::super::ingestion::create_job(State(service.inner.clone()), request)
        .await
        .unwrap_err();
    assert_eq!(error.status, StatusCode::BAD_REQUEST);
    assert!(ingest_permit(&service.inner).is_ok());
}

#[tokio::test]
async fn job_ingress_deadline_releases_admission() {
    let service = service(store());
    let body = axum::body::Body::from_stream(futures_util::stream::once(std::future::pending::<
        Result<bytes::Bytes, std::convert::Infallible>,
    >()));
    let request = axum::http::Request::builder()
        .method("POST")
        .header("content-type", "application/json")
        .body(body)
        .unwrap();
    let error = super::super::ingestion::create_job_with_timeout(
        State(service.inner.clone()),
        request,
        std::time::Duration::from_millis(10),
    )
    .await
    .unwrap_err();
    assert_eq!(error.status, StatusCode::REQUEST_TIMEOUT);
    assert!(ingest_permit(&service.inner).is_ok());
}

#[tokio::test]
async fn multipart_deadline_and_oversized_raw_body_release_admission() {
    let service = service(store());
    let body = axum::body::Body::from_stream(futures_util::stream::once(std::future::pending::<
        Result<bytes::Bytes, std::convert::Infallible>,
    >()));
    let request = axum::http::Request::builder()
        .header("content-type", "multipart/form-data; boundary=x")
        .body(body)
        .unwrap();
    let permit = ingest_permit(&service.inner).unwrap();
    assert!(
        tokio::time::timeout(
            std::time::Duration::from_millis(10),
            submission_from_request(&service.inner, request),
        )
        .await
        .is_err()
    );
    drop(permit);
    assert!(ingest_permit(&service.inner).is_ok());

    let oversized = vec![0; service.inner.body_limit() + 1];
    let request = axum::http::Request::builder()
        .uri("/?filename=a.pdf")
        .header("content-type", "application/octet-stream")
        .header("content-length", oversized.len().to_string())
        .body(axum::body::Body::from(oversized))
        .unwrap();
    let permit = ingest_permit(&service.inner).unwrap();
    let error = submission_from_request(&service.inner, request)
        .await
        .unwrap_err();
    assert_eq!(error.status, StatusCode::PAYLOAD_TOO_LARGE);
    drop(permit);
    assert!(ingest_permit(&service.inner).is_ok());
}

fn json_request(body: serde_json::Value) -> axum::http::Request<axum::body::Body> {
    axum::http::Request::builder()
        .method("POST")
        .header("content-type", "application/json")
        .body(axum::body::Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap()
}

async fn submit_json(s: &Arc<ServiceInner>, files: serde_json::Value) -> serde_json::Value {
    let response = create(
        State(s.clone()),
        json_request(serde_json::json!({"files": files})),
    )
    .await
    .unwrap_or_else(|error| panic!("submit failed: {error:?}"));
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

/// Submit with a full request body (extra fields like output_formats).
async fn submit_body(s: &Arc<ServiceInner>, body: serde_json::Value) -> serde_json::Value {
    let response = create(State(s.clone()), json_request(body))
        .await
        .unwrap_or_else(|error| panic!("submit failed: {error:?}"));
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

fn synthetic_result() -> OfficialResult {
    OfficialResult {
        middle: serde_json::json!({"pages": [{}]}),
        model: None,
        markdown: "# hello".into(),
        structured: serde_json::json!({"blocks": []}),
        assets: std::collections::BTreeMap::new(),
        warnings: Vec::new(),
        metadata: crate::official::OfficialVersionMetadata {
            mineru_package: "test".into(),
            schema_version: "test".into(),
            bundle_name: "test".into(),
        },
    }
}

/// Complete a task's first file by publishing artifacts through the store and
/// driving the record into a completed terminal state.
async fn complete_first_file(s: &Arc<ServiceInner>, id: &str) {
    let shared = s.jobs.lock().unwrap()[id].clone();
    let result = synthetic_result();
    publish_outputs(&s.clone(), &shared, 0, &result, &mut 0)
        .await
        .expect("publish");
    let mut record = shared.lock().await;
    record.response.files[0].status = "completed".into();
    record.response.progress.completed = 1;
    record.response.status = "completed".into();
    record.response.finished_at = Some("2026-01-01T00:00:00Z".into());
}

#[tokio::test]
async fn json_submission_returns_task_payload() {
    let s = service(store());
    let payload = submit_json(
        &s.inner,
        serde_json::json!([
            {"source":{"type":"inline","name":"a.pdf","data":"YQ=="}}
        ]),
    )
    .await;
    assert!(payload["task_id"].as_str().is_some());
    assert!(payload.get("job_id").is_none());
    assert_eq!(
        payload["status_url"],
        format!("/v1/tasks/{}", payload["task_id"].as_str().unwrap())
    );
    assert_eq!(
        payload["result_url"],
        format!("/v1/tasks/{}/result", payload["task_id"].as_str().unwrap())
    );
    assert_eq!(payload["links"]["self"], payload["status_url"]);
    assert_eq!(payload["links"]["cancel"], payload["status_url"]);
    s.shutdown().await.unwrap();
}

#[tokio::test]
async fn multipart_submission_produces_equivalent_task() {
    let s = service(store());
    let boundary = "XBOUNDARY";
    let body = format!(
        "--{boundary}\r\ncontent-disposition: form-data; name=\"files\"; filename=\"a.pdf\"\r\ncontent-type: application/pdf\r\n\r\nabc\r\n--{boundary}--\r\n"
    );
    let request = axum::http::Request::builder()
        .method("POST")
        .header(
            "content-type",
            format!("multipart/form-data; boundary={boundary}"),
        )
        .body(axum::body::Body::from(body))
        .unwrap();
    let response = create(State(s.inner.clone()), request).await.unwrap();
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .unwrap();
    let payload: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(payload["status"], "queued");
    assert_eq!(payload["files"][0]["name"], "a.pdf");
    assert!(payload["task_id"].as_str().is_some());
    s.shutdown().await.unwrap();
}

#[tokio::test]
async fn get_status_and_unknown_404() {
    let s = service(store());
    let payload = submit_json(
        &s.inner,
        serde_json::json!([
            {"source":{"type":"inline","name":"a.pdf","data":"YQ=="}}
        ]),
    )
    .await;
    let id = payload["task_id"].as_str().unwrap().to_owned();
    let status_payload = status(State(s.inner.clone()), Path(id)).await.unwrap();
    assert_eq!(status_payload.0["status"], "queued");
    let error = status(State(s.inner.clone()), Path("job_nope".into()))
        .await
        .unwrap_err();
    assert_eq!(error.status, StatusCode::NOT_FOUND);
    s.shutdown().await.unwrap();
}

#[tokio::test]
async fn result_on_non_terminal_is_202() {
    let s = service(store());
    let payload = submit_json(
        &s.inner,
        serde_json::json!([
            {"source":{"type":"inline","name":"a.pdf","data":"YQ=="}}
        ]),
    )
    .await;
    let id = payload["task_id"].as_str().unwrap().to_owned();
    let response = serve_result(s.inner.clone(), id, Some("json"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    s.shutdown().await.unwrap();
}

#[tokio::test]
async fn result_json_inlines_artifacts() {
    let s = service(store());
    let payload = submit_body(
        &s.inner,
        serde_json::json!({
            "files": [{"source":{"type":"inline","name":"a.pdf","data":"YQ=="}}],
            "output_formats": ["markdown", "middle_json", "structured_content"]
        }),
    )
    .await;
    let id = payload["task_id"].as_str().unwrap().to_owned();
    complete_first_file(&s.inner, &id).await;
    let response = serve_result(s.inner.clone(), id, Some("json"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .unwrap();
    let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(value["files"][0]["content"]["markdown"], "# hello");
    assert_eq!(
        value["files"][0]["content"]["middle_json"]["pages"],
        serde_json::json!([{}])
    );
    assert_eq!(
        value["files"][0]["content"]["structured_content"]["blocks"],
        serde_json::json!([])
    );
    s.shutdown().await.unwrap();
}

#[tokio::test]
async fn delete_returns_canceled_payload() {
    let s = service(store());
    let payload = submit_json(
        &s.inner,
        serde_json::json!([
            {"source":{"type":"inline","name":"a.pdf","data":"YQ=="}}
        ]),
    )
    .await;
    let id = payload["task_id"].as_str().unwrap().to_owned();
    let response = cancel(State(s.inner.clone()), Path(id)).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .unwrap();
    let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(value["status"], "canceled");
    assert!(value["task_id"].as_str().is_some());
    s.shutdown().await.unwrap();
}

#[tokio::test]
async fn unsupported_media_type_is_415() {
    let s = service(store());
    let request = axum::http::Request::builder()
        .method("POST")
        .header("content-type", "text/plain")
        .body(axum::body::Body::from("hello"))
        .unwrap();
    let error = create(State(s.inner.clone()), request).await.unwrap_err();
    assert_eq!(error.status, StatusCode::UNSUPPORTED_MEDIA_TYPE);
    s.shutdown().await.unwrap();
}

#[tokio::test]
async fn file_parse_times_out_without_runner_and_keeps_job() {
    let s = service(store());
    let request = json_request(serde_json::json!({
        "files": [{"source":{"type":"inline","name":"a.pdf","data":"YQ=="}}]
    }));
    // wait_timeout=1 with no runner: the poll loop hits the deadline.
    let start = std::time::Instant::now();
    let response = file_parse(
        State(s.inner.clone()),
        Query(FileParseQuery {
            response_format: Some("json".into()),
            wait_timeout: Some(1),
        }),
        request,
    )
    .await
    .unwrap();
    assert!(start.elapsed() >= std::time::Duration::from_millis(900));
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .unwrap();
    let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(value["status"], "queued");
    // The background job is untouched.
    assert!(
        s.inner
            .jobs
            .lock()
            .unwrap()
            .contains_key(value["task_id"].as_str().unwrap())
    );
    s.shutdown().await.unwrap();
}

#[tokio::test]
async fn zip_result_requires_zip_output_format() {
    let s = service(store());
    let payload = submit_json(
        &s.inner,
        serde_json::json!([
            {"source":{"type":"inline","name":"a.pdf","data":"YQ=="}}
        ]),
    )
    .await;
    let id = payload["task_id"].as_str().unwrap().to_owned();
    complete_first_file(&s.inner, &id).await;
    let error = serve_result(s.inner.clone(), id, Some("zip"))
        .await
        .unwrap_err();
    assert_eq!(error.status, StatusCode::BAD_REQUEST);
    assert_eq!(error.code, "unsupported_output_format");
    s.shutdown().await.unwrap();
}

#[tokio::test]
async fn zip_result_streams_retained_artifact() {
    let s = service(store());
    let payload = submit_body(
        &s.inner,
        serde_json::json!({
            "files": [{"source":{"type":"inline","name":"a.pdf","data":"YQ=="}}],
            "output_formats": ["zip"]
        }),
    )
    .await;
    let id = payload["task_id"].as_str().unwrap().to_owned();
    complete_first_file(&s.inner, &id).await;
    let response = serve_result(s.inner.clone(), id.clone(), Some("zip"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers().get("content-type").unwrap(),
        "application/zip"
    );
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .unwrap();
    let mut archive = zip::ZipArchive::new(std::io::Cursor::new(bytes)).unwrap();
    assert!(archive.by_name("markdown.md").is_ok());
    assert!(archive.by_name("middle_json.json").is_ok());
    s.shutdown().await.unwrap();
}

#[tokio::test]
async fn missing_artifact_returns_invalid_response() {
    let s = service(store());
    let payload = submit_json(
        &s.inner,
        serde_json::json!([
            {"source":{"type":"inline","name":"a.pdf","data":"YQ=="}}
        ]),
    )
    .await;
    let id = payload["task_id"].as_str().unwrap().to_owned();
    {
        let shared = s.inner.jobs.lock().unwrap()[&id].clone();
        let mut record = shared.lock().await;
        record.response.files[0].status = "completed".into();
        record.response.progress.completed = 1;
        record.response.status = "completed".into();
        // No output_files published.
    }
    let error = serve_result(s.inner.clone(), id, Some("json"))
        .await
        .unwrap_err();
    assert_eq!(error.status, StatusCode::BAD_GATEWAY);
    assert_eq!(error.code, "invalid_response");
    s.shutdown().await.unwrap();
}

#[tokio::test]
async fn result_query_selects_zip_response_format() {
    let s = service(store());
    let payload = submit_body(
        &s.inner,
        serde_json::json!({
            "files": [{"source":{"type":"inline","name":"a.pdf","data":"YQ=="}}],
            "output_formats": ["zip"]
        }),
    )
    .await;
    let id = payload["task_id"].as_str().unwrap().to_owned();
    complete_first_file(&s.inner, &id).await;
    let response = task_result(
        State(s.inner.clone()),
        Path(id),
        axum::extract::RawQuery(Some("response_format=zip".into())),
    )
    .await
    .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers().get("content-type").unwrap(),
        "application/zip"
    );
    s.shutdown().await.unwrap();
}

#[tokio::test]
async fn result_query_rejects_unknown_response_format() {
    let s = service(store());
    let error = task_result(
        State(s.inner.clone()),
        Path("job_missing".into()),
        axum::extract::RawQuery(Some("response_format=xml".into())),
    )
    .await
    .unwrap_err();
    assert_eq!(error.status, StatusCode::BAD_REQUEST);
    s.shutdown().await.unwrap();
}

#[path = "tests/review.rs"]
mod review;
