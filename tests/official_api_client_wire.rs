use axum::{
    Json, Router,
    body::Bytes,
    extract::State,
    routing::{get, post, put},
};
use mineru::official_api::client::{
    OfficialApiClient, OfficialApiError, OfficialInput, OfficialRemoteParseOptions,
};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};

#[derive(Clone, Default)]
struct Wire(Arc<Mutex<Vec<String>>>);

fn zip_with_markdown(text: &str) -> Vec<u8> {
    use std::io::Write;
    let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    writer
        .start_file("markdown.md", zip::write::SimpleFileOptions::default())
        .unwrap();
    writer.write_all(text.as_bytes()).unwrap();
    writer.finish().unwrap().into_inner()
}

fn created_upload(task: &str, file: &str) -> Value {
    json!({
        "file_id": file,
        "file_name": "x.pdf",
        "status": "waiting-file",
        "upload": {
            "url": format!("/v1/tasks/{task}/uploads/{file}"),
            "headers": {"Content-Type": "application/octet-stream"},
            "expires_at": 4102444800i64
        }
    })
}

async fn create_task(
    State(state): State<Wire>,
    Json(body): Json<Value>,
) -> (axum::http::StatusCode, Json<Value>) {
    assert_eq!(body["files"][0]["file_name"], "x.pdf");
    assert_eq!(body["files"][0]["bytes"], 3);
    assert_eq!(body["files"][0]["page_range"], "r3-r1, 5");
    assert_eq!(
        body["files"][0]["sha256sum"].as_str().map(str::len),
        Some(64)
    );
    state.0.lock().unwrap().push("create".into());
    (
        axum::http::StatusCode::ACCEPTED,
        Json(json!({
            "task_id": "job-1",
            "status": "waiting-file",
            "files": [created_upload("job-1", "file-1")]
        })),
    )
}
async fn put_upload(State(state): State<Wire>, body: Bytes) {
    assert_eq!(&body[..], b"pdf");
    state.0.lock().unwrap().push("put".into());
}
async fn completed_task(State(state): State<Wire>) -> Json<Value> {
    state.0.lock().unwrap().push("poll".into());
    Json(json!({
        "task_id": "job-1",
        "status": "completed",
        "created_at": "2026-01-01T00:00:00Z",
        "finished_at": "2026-01-01T00:00:05Z",
        "output_formats": ["default"],
        "progress": {"completed": 1, "failed": 0, "total": 1},
        "files": [{
            "file_id": "file-1",
            "file_name": "x.zip",
            "status": "completed",
            "bytes": 3,
            "result_url": "/v1/files/file-1/content",
            "parse": {"tier": "flash", "duration_ms": 5, "parser_version": "4.2.3"}
        }]
    }))
}
async fn content(State(state): State<Wire>) -> Bytes {
    state.0.lock().unwrap().push("download".into());
    Bytes::from(zip_with_markdown("hello"))
}

#[tokio::test]
async fn exercises_exact_create_upload_poll_and_download_wire() {
    let wire = Wire::default();
    let app = Router::new()
        .route("/v1/tasks", post(create_task))
        .route("/v1/tasks/{task}/uploads/{file}", put(put_upload))
        .route("/v1/tasks/{task}", get(completed_task))
        .route("/v1/files/{id}/content", get(content))
        .with_state(wire.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let client = OfficialApiClient::new(&format!("http://{address}")).unwrap();
    let input = OfficialInput::new("x.pdf", b"pdf".to_vec()).with_page_range("r3-r1, 5");
    let result = client
        .parse(input, &OfficialRemoteParseOptions::default())
        .await
        .unwrap();
    assert_eq!(result.markdown().as_deref(), Some("hello"));
    assert_eq!(result.artifacts[0].kind, "zip");
    assert_eq!(
        *wire.0.lock().unwrap(),
        ["create", "put", "poll", "download"]
    );
}

async fn batch_create(Json(body): Json<Value>) -> (axum::http::StatusCode, Json<Value>) {
    let count = body["files"].as_array().map(Vec::len).unwrap_or(0);
    let files: Vec<Value> = (0..count)
        .map(|index| created_upload("job-2", &format!("file-{index}")))
        .collect();
    (
        axum::http::StatusCode::ACCEPTED,
        Json(json!({"task_id": "job-2", "status": "waiting-file", "files": files})),
    )
}
async fn batch_put() {}
async fn partial_task() -> Json<Value> {
    Json(json!({
        "task_id": "job-2",
        "status": "partial",
        "created_at": "2026-01-01T00:00:00Z",
        "output_formats": ["default"],
        "progress": {"completed": 1, "failed": 1, "total": 2},
        "files": [
            {
                "file_id": "file-0",
                "file_name": "a.zip",
                "status": "completed",
                "result_url": "/v1/files/file-0/content"
            },
            {
                "file_id": "file-1",
                "file_name": "b.zip",
                "status": "failed",
                "error": {"code": "parse_failed", "message": "bad input"}
            }
        ]
    }))
}
async fn good_content() -> Bytes {
    Bytes::from(zip_with_markdown("ok"))
}

#[tokio::test]
async fn preserves_ordered_results_for_partial_batches() {
    let app = Router::new()
        .route("/v1/tasks", post(batch_create))
        .route("/v1/tasks/{task}/uploads/{file}", put(batch_put))
        .route("/v1/tasks/{task}", get(partial_task))
        .route("/v1/files/{id}/content", get(good_content));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let client = OfficialApiClient::new(&format!("http://{address}")).unwrap();
    let batch = vec![
        OfficialInput::new("a.pdf", b"a".to_vec()),
        OfficialInput::new("b.pdf", b"b".to_vec()),
    ];
    let results = client
        .parse_batch(batch, &OfficialRemoteParseOptions::default())
        .await;
    assert_eq!(results.len(), 2);
    assert_eq!(
        results[0].as_ref().unwrap().markdown().as_deref(),
        Some("ok")
    );
    let error = results[1].as_ref().unwrap_err().to_string();
    assert!(error.contains("bad input"), "{error}");
}

async fn failed_create() -> (axum::http::StatusCode, Json<Value>) {
    (
        axum::http::StatusCode::ACCEPTED,
        Json(json!({
            "task_id": "job-3",
            "status": "waiting-file",
            "files": [created_upload("job-3", "file-0"), created_upload("job-3", "file-1")]
        })),
    )
}
async fn failed_task() -> Json<Value> {
    Json(json!({
        "task_id": "job-3",
        "status": "failed",
        "created_at": "2026-01-01T00:00:00Z",
        "output_formats": ["default"],
        "progress": {"completed": 0, "failed": 2, "total": 2},
        "files": [
            {"file_id": "file-0", "file_name": "a.zip", "status": "failed",
             "error": {"code": "bad_pdf", "message": "first failed"}},
            {"file_id": "file-1", "file_name": "b.zip", "status": "failed",
             "error": {"code": "bad_type", "message": "second failed"}}
        ]
    }))
}

#[tokio::test]
async fn fully_failed_batch_preserves_each_remote_file_error() {
    let app = Router::new()
        .route("/v1/tasks", post(failed_create))
        .route("/v1/tasks/{task}/uploads/{file}", put(batch_put))
        .route("/v1/tasks/{task}", get(failed_task));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let client = OfficialApiClient::new(&format!("http://{address}")).unwrap();
    let results = client
        .parse_batch(
            vec![
                OfficialInput::new("a.pdf", b"a".to_vec()),
                OfficialInput::new("b.pdf", b"b".to_vec()),
            ],
            &OfficialRemoteParseOptions::default(),
        )
        .await;
    for (result, (code, message)) in results
        .iter()
        .zip([("bad_pdf", "first failed"), ("bad_type", "second failed")])
    {
        match result.as_ref().unwrap_err() {
            OfficialApiError::File {
                code: actual,
                message: detail,
                ..
            } => {
                assert_eq!(actual, code);
                assert_eq!(detail, message);
            }
            error => panic!("unexpected error: {error}"),
        }
    }
}

async fn cross_origin_create() -> (axum::http::StatusCode, Json<Value>) {
    (
        axum::http::StatusCode::ACCEPTED,
        Json(json!({
            "task_id": "job-4",
            "status": "waiting-file",
            "files": [{
                "file_id": "file-0",
                "file_name": "a.pdf",
                "status": "waiting-file",
                "upload": {"url": "http://127.0.0.1:9/steal", "headers": {}, "expires_at": 1}
            }]
        })),
    )
}
async fn rejected_task() -> Json<Value> {
    Json(json!({
        "task_id": "job-4",
        "status": "failed",
        "created_at": "2026-01-01T00:00:00Z",
        "output_formats": ["default"],
        "progress": {"completed": 0, "failed": 1, "total": 1},
        "files": [{"file_id": "file-0", "file_name": "a.zip", "status": "failed",
                   "error": {"code": "upload_expired", "message": "Upload expired"}}]
    }))
}

#[tokio::test]
async fn refuses_cross_origin_upload_and_does_not_follow_redirects() {
    let app = Router::new()
        .route("/v1/tasks", post(cross_origin_create))
        .route("/v1/tasks/{task}", get(rejected_task));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let client = OfficialApiClient::new(&format!("http://{address}"))
        .unwrap()
        .with_token("secret")
        .unwrap();
    let result = client
        .parse(
            OfficialInput::new("x.pdf", b"pdf".to_vec()),
            &OfficialRemoteParseOptions::default(),
        )
        .await;
    match result.unwrap_err() {
        OfficialApiError::Protocol(message) => assert!(message.contains("same-origin")),
        error => panic!("unexpected error: {error}"),
    }
}

async fn pending_create() -> (axum::http::StatusCode, Json<Value>) {
    (
        axum::http::StatusCode::ACCEPTED,
        Json(json!({
            "task_id": "job-5",
            "status": "waiting-file",
            "files": [created_upload("job-5", "file-0")]
        })),
    )
}
async fn pending_task() -> Json<Value> {
    Json(json!({
        "task_id": "job-5",
        "status": "running",
        "created_at": "2026-01-01T00:00:00Z",
        "output_formats": ["default"],
        "progress": {"completed": 0, "failed": 0, "total": 1},
        "files": [{"file_id": "file-0", "file_name": "a.pdf", "status": "running"}]
    }))
}
async fn record_cancel(State(cancelled): State<Arc<std::sync::atomic::AtomicBool>>) -> Json<Value> {
    cancelled.store(true, std::sync::atomic::Ordering::SeqCst);
    Json(
        json!({"task_id": "job-5", "status": "canceled", "finished_at": "2026-01-01T00:00:01Z", "files": []}),
    )
}

#[tokio::test]
async fn overall_deadline_cancels_an_admitted_task() {
    let cancelled = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let app = Router::new()
        .route("/v1/tasks", post(pending_create))
        .route("/v1/tasks/{task}/uploads/{file}", put(batch_put))
        .route("/v1/tasks/{task}", get(pending_task))
        .route("/v1/tasks/{task}/cancel", post(record_cancel))
        .with_state(cancelled.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let client = OfficialApiClient::new(&format!("http://{address}"))
        .unwrap()
        .with_timeout(std::time::Duration::from_millis(120))
        .unwrap();
    let result = client
        .parse(
            OfficialInput::new("x.pdf", b"x".to_vec()),
            &OfficialRemoteParseOptions::default(),
        )
        .await;
    assert!(result.is_err());
    assert!(cancelled.load(std::sync::atomic::Ordering::SeqCst));
}
