use axum::{
    Json, Router,
    body::Bytes,
    extract::State,
    routing::{get, post, put},
};
use mineru::official_api::client::{OfficialApiClient, OfficialInput, OfficialRemoteParseOptions};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};

#[derive(Clone, Default)]
struct Wire(Arc<Mutex<Vec<String>>>);

async fn create_upload(State(state): State<Wire>, Json(body): Json<Value>) -> Json<Value> {
    assert_eq!(body["purpose"], "parse");
    assert_eq!(body["filename"], "x.pdf");
    state.0.lock().unwrap().push("create".into());
    Json(
        json!({"id":"upload-1","status":"uploading","upload_url":"/v1/uploads/upload-1/content","upload_method":"PUT","upload_headers":{"content-type":"application/pdf"}}),
    )
}
async fn put_upload(State(state): State<Wire>, body: Bytes) {
    assert_eq!(&body[..], b"pdf");
    state.0.lock().unwrap().push("put".into());
}
async fn complete(State(state): State<Wire>) -> Json<Value> {
    state.0.lock().unwrap().push("complete".into());
    Json(json!({"id":"upload-1","status":"completed","file":{"id":"file-1"}}))
}
async fn create_job(
    State(state): State<Wire>,
    Json(body): Json<Value>,
) -> (axum::http::StatusCode, Json<Value>) {
    assert_eq!(
        body["files"][0]["source"],
        json!({"type":"file_id","file_id":"file-1"})
    );
    assert_eq!(body["files"][0]["page_range"], "r3-r1, 5");
    state.0.lock().unwrap().push("job".into());
    (
        axum::http::StatusCode::ACCEPTED,
        Json(json!({"job_id":"job-1"})),
    )
}
async fn get_job(State(state): State<Wire>) -> Json<Value> {
    state.0.lock().unwrap().push("poll".into());
    Json(
        json!({"job_id":"job-1","status":"completed","files":[{"status":"completed","output_files":{"markdown":{"file_id":"out-1","bytes":5},"middle_json":null,"structured_content":null,"zip":null}}]}),
    )
}
async fn content(State(state): State<Wire>) -> Bytes {
    state.0.lock().unwrap().push("download".into());
    Bytes::from_static(b"hello")
}
async fn dedup_upload() -> Json<Value> {
    Json(json!({"id":"up","status":"completed","file":{"id":"input"}}))
}
async fn partial_job(Json(body): Json<Value>) -> (axum::http::StatusCode, Json<Value>) {
    assert_eq!(body["files"].as_array().unwrap().len(), 2);
    (
        axum::http::StatusCode::ACCEPTED,
        Json(json!({"job_id":"partial"})),
    )
}
async fn partial_poll() -> Json<Value> {
    Json(json!({"job_id":"partial","status":"partial","files":[
        {"status":"completed","output_files":{"markdown":{"file_id":"good","bytes":2}}},
        {"status":"failed","error":{"type":"parse_error","code":"invalid","message":"bad input"}}
    ]}))
}
async fn failed_job(Json(body): Json<Value>) -> (axum::http::StatusCode, Json<Value>) {
    assert_eq!(body["files"].as_array().unwrap().len(), 2);
    (
        axum::http::StatusCode::ACCEPTED,
        Json(json!({"job_id":"failed"})),
    )
}
async fn failed_poll() -> Json<Value> {
    Json(json!({"job_id":"failed","status":"failed","files":[
        {"status":"failed","error":{"type":"parse_error","code":"bad_pdf","message":"first failed"}},
        {"status":"failed","error":{"type":"input_error","code":"bad_type","message":"second failed"}}
    ]}))
}
async fn good_content() -> Bytes {
    Bytes::from_static(b"ok")
}
async fn cross_origin_upload() -> Json<Value> {
    Json(
        json!({"id":"up","status":"uploading","upload_url":"http://127.0.0.1:9/steal","upload_method":"PUT","upload_headers":{}}),
    )
}
async fn redirect_upload(State(target): State<String>) -> axum::response::Redirect {
    axum::response::Redirect::temporary(&target)
}
async fn mark_redirect(State(hit): State<Arc<std::sync::atomic::AtomicBool>>) -> Bytes {
    hit.store(true, std::sync::atomic::Ordering::SeqCst);
    Bytes::new()
}
async fn pending_job() -> (axum::http::StatusCode, Json<Value>) {
    (
        axum::http::StatusCode::ACCEPTED,
        Json(json!({"job_id":"slow"})),
    )
}
async fn pending_poll() -> Json<Value> {
    Json(json!({"job_id":"slow","status":"running","files":[]}))
}
async fn record_cancel(State(cancelled): State<Arc<std::sync::atomic::AtomicBool>>) -> Json<Value> {
    cancelled.store(true, std::sync::atomic::Ordering::SeqCst);
    Json(json!({"job_id":"slow","status":"canceled"}))
}

#[tokio::test]
async fn exercises_exact_upload_job_poll_and_download_wire() {
    let wire = Wire::default();
    let app = Router::new()
        .route("/v1/uploads", post(create_upload))
        .route("/v1/uploads/{id}/content", put(put_upload))
        .route("/v1/uploads/{id}/complete", post(complete))
        .route("/v1/parse/jobs", post(create_job))
        .route(
            "/v1/parse/jobs/{id}",
            get(get_job).delete(|| async { Json(json!({})) }),
        )
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
    assert_eq!(result.markdown(), Some("hello"));
    assert_eq!(
        *wire.0.lock().unwrap(),
        ["create", "put", "complete", "job", "poll", "download"]
    );
}

#[tokio::test]
async fn preserves_ordered_results_for_partial_batches() {
    let app = Router::new()
        .route("/v1/uploads", post(dedup_upload))
        .route("/v1/parse/jobs", post(partial_job))
        .route("/v1/parse/jobs/{id}", get(partial_poll))
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
    assert_eq!(results[0].as_ref().unwrap().markdown(), Some("ok"));
    assert!(
        results[1]
            .as_ref()
            .unwrap_err()
            .to_string()
            .contains("bad input")
    );
}

#[tokio::test]
async fn fully_failed_batch_preserves_each_remote_file_error() {
    let app = Router::new()
        .route("/v1/uploads", post(dedup_upload))
        .route("/v1/parse/jobs", post(failed_job))
        .route("/v1/parse/jobs/{id}", get(failed_poll));
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
            mineru::official_api::client::OfficialApiError::File {
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

#[tokio::test]
async fn refuses_cross_origin_upload_and_does_not_follow_redirects() {
    let app = Router::new().route("/v1/uploads", post(cross_origin_upload));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let client = OfficialApiClient::new(&format!("http://{address}"))
        .unwrap()
        .with_token("secret")
        .unwrap();
    assert!(client.create_upload("x.pdf", b"pdf").await.is_err());

    let hit = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let target = Router::new()
        .route("/redirected", get(mark_redirect))
        .with_state(hit.clone());
    let target_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let target_address = target_listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(target_listener, target).await.unwrap();
    });
    let redirect = Router::new()
        .route("/v1/uploads", post(redirect_upload))
        .with_state(format!("http://{target_address}/redirected"));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, redirect).await.unwrap();
    });
    let client = OfficialApiClient::new(&format!("http://{address}"))
        .unwrap()
        .with_token("secret")
        .unwrap();
    assert!(client.create_upload("x.pdf", b"pdf").await.is_err());
    assert!(!hit.load(std::sync::atomic::Ordering::SeqCst));
}

#[tokio::test]
async fn overall_deadline_cancels_an_admitted_job() {
    let cancelled = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let app = Router::new()
        .route("/v1/uploads", post(dedup_upload))
        .route("/v1/parse/jobs", post(pending_job))
        .route(
            "/v1/parse/jobs/{id}",
            get(pending_poll).delete(record_cancel),
        )
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
