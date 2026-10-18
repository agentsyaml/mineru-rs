// Focused regression tests from the review pass against the upstream
// 4.0.12 task API contract.
use super::*;
use std::io::Read;

/// Publish the zip artifact for file `index` and mark it completed, leaving
/// the task otherwise untouched (used for partial-result tests).
async fn complete_zip_file(s: &Arc<ServiceInner>, id: &str, index: usize) {
    let shared = s.jobs.lock().unwrap()[id].clone();
    let result = synthetic_result();
    publish_outputs(&s.clone(), &shared, index, &result, &mut 0)
        .await
        .expect("publish");
    let mut record = shared.lock().await;
    record.response.files[index].status = "completed".into();
    record.response.progress.completed += 1;
}

#[tokio::test]
async fn partial_two_document_zip_combines_with_manifest_and_status() {
    let s = service(store());
    let payload = submit_body(
        &s.inner,
        serde_json::json!({
            "files": [
                {"source":{"type":"inline","name":"a.pdf","data":"YQ=="}},
                {"source":{"type":"inline","name":"b.pdf","data":"Yg=="}}
            ],
            "output_formats": ["zip"]
        }),
    )
    .await;
    let id = payload["task_id"].as_str().unwrap().to_owned();
    complete_zip_file(&s.inner, &id, 0).await;
    {
        // Second file fails; the task ends partial.
        let shared = s.inner.jobs.lock().unwrap()[&id].clone();
        let mut record = shared.lock().await;
        record.response.files[1].status = "failed".into();
        record.response.progress.failed = 1;
        record.response.status = "partial".into();
        record.response.finished_at = Some("2026-01-01T00:00:00Z".into());
    }
    let response = serve_result(s.inner.clone(), id.clone(), Some("zip"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers().get("x-mineru-task-status").unwrap(),
        "partial"
    );
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .unwrap();
    let mut archive = zip::ZipArchive::new(std::io::Cursor::new(bytes)).unwrap();
    let manifest = {
        let mut entry = archive.by_name("manifest.json").unwrap();
        let mut data = String::new();
        entry.read_to_string(&mut data).unwrap();
        serde_json::from_str::<serde_json::Value>(&data).unwrap()
    };
    assert!(manifest.get("job_id").is_none());
    assert_eq!(manifest["task_id"], id.as_str());
    assert_eq!(manifest["status"], "partial");
    assert!(archive.by_name("0001/markdown.md").is_ok());
    assert!(archive.by_name("0001/middle_json.json").is_ok());
    assert!(archive.by_name("0002/anything").is_err());
    s.shutdown().await.unwrap();
}

#[tokio::test]
async fn partial_json_result_gives_empty_content_to_failed_file() {
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
    complete_zip_file(&s.inner, &id, 0).await;
    {
        let shared = s.inner.jobs.lock().unwrap()[&id].clone();
        let mut record = shared.lock().await;
        record.response.progress.completed = 0;
        record.response.files[0].status = "failed".into();
        record.response.progress.failed = 1;
        record.response.status = "partial".into();
    }
    let response = serve_result(s.inner.clone(), id, Some("json"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .unwrap();
    let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(value["files"][0]["content"], serde_json::json!({}));
    s.shutdown().await.unwrap();
}

#[tokio::test]
async fn json_result_ignores_zip_artifact_and_omits_zip_content_key() {
    let s = service(store());
    let payload = submit_body(
        &s.inner,
        serde_json::json!({
            "files": [{"source":{"type":"inline","name":"a.pdf","data":"YQ=="}}],
            "output_formats": ["markdown", "zip"]
        }),
    )
    .await;
    let id = payload["task_id"].as_str().unwrap().to_owned();
    complete_first_file(&s.inner, &id).await;
    let response = serve_result(s.inner.clone(), id, Some("json"))
        .await
        .unwrap();
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .unwrap();
    let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(value["files"][0]["content"]["markdown"], "# hello");
    assert!(value["files"][0]["content"].get("zip").is_none());
    s.shutdown().await.unwrap();
}

#[tokio::test]
async fn requested_text_format_without_reference_is_502() {
    let s = service(store());
    let payload = submit_body(
        &s.inner,
        serde_json::json!({
            "files": [{"source":{"type":"inline","name":"a.pdf","data":"YQ=="}}],
            "output_formats": ["markdown", "middle_json"]
        }),
    )
    .await;
    let id = payload["task_id"].as_str().unwrap().to_owned();
    {
        // Complete with only the markdown reference present.
        let shared = s.inner.jobs.lock().unwrap()[&id].clone();
        let result = synthetic_result();
        publish_outputs(&s.inner, &shared, 0, &result, &mut 0)
            .await
            .expect("publish");
        let mut record = shared.lock().await;
        if let Some(refs) = record.response.files[0].output_files.as_mut() {
            refs.remove("middle_json");
        }
        record.response.files[0].status = "completed".into();
        record.response.progress.completed = 1;
        record.response.status = "completed".into();
    }
    let error = serve_result(s.inner.clone(), id, Some("json"))
        .await
        .unwrap_err();
    assert_eq!(error.status, StatusCode::BAD_GATEWAY);
    assert_eq!(error.code, "invalid_response");
    s.shutdown().await.unwrap();
}

#[tokio::test]
async fn raw_upload_without_content_length_is_411() {
    let s = service(store());
    let request = axum::http::Request::builder()
        .method("POST")
        .header("content-type", "application/octet-stream")
        .header("transfer-encoding", "chunked")
        .body(axum::body::Body::from("abc"))
        .unwrap();
    let error = create(State(s.inner.clone()), request).await.unwrap_err();
    assert_eq!(error.status, StatusCode::LENGTH_REQUIRED);
    s.shutdown().await.unwrap();
}

#[tokio::test]
async fn json_submission_with_query_parameter_is_rejected() {
    let s = service(store());
    let mut request = json_request(serde_json::json!({
        "files": [{"source":{"type":"inline","name":"a.pdf","data":"YQ=="}}]
    }));
    *request.uri_mut() = "/v1/tasks?tier=flash".parse().unwrap();
    let error = create(State(s.inner.clone()), request).await.unwrap_err();
    assert_eq!(error.status, StatusCode::BAD_REQUEST);
    s.shutdown().await.unwrap();
}

#[tokio::test]
async fn file_parse_multipart_accepts_upload_query_options() {
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
        .uri("/v1/file_parse?tier=flash&response_format=json&wait_timeout=1")
        .body(axum::body::Body::from(body))
        .unwrap();
    let response = file_parse(
        State(s.inner.clone()),
        Query(FileParseQuery {
            response_format: Some("json".into()),
            wait_timeout: Some(1),
        }),
        request,
    )
    .await
    .expect("multipart file_parse must accept upload query options");
    // No runner is started, so the synchronous wait times out and returns the
    // task payload; the upload query option (`tier`) must not be rejected.
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    s.shutdown().await.unwrap();
}
