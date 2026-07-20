use super::archive::{self, ArchiveLimits, DownloadedZip};
use super::{
    InputDocument, RemoteEnv, RemoteOptions, ServerHealth, StatusSnapshot, SubmitResponse,
    normalize_api_url, request_form, validate_health,
};
use crate::error::sanitize_vlm_error_bytes;
use futures_util::StreamExt;
use reqwest::{
    Client, Response,
    multipart::{Form, Part},
    redirect::Policy,
};
use serde_json::Value;
use std::{future::Future, path::Path, time::Duration};

const BODY_CAP: usize = 64 * 1024;

async fn whole_operation_timeout<T>(
    duration: Duration,
    operation: impl Future<Output = Result<T, String>>,
) -> Result<T, String> {
    tokio::time::timeout(duration, operation)
        .await
        .map_err(|_| "task submission timed out".to_string())?
}

#[derive(Clone, Copy)]
struct Timing {
    acquisition: Duration,
    send: Duration,
    interval: Duration,
}
impl Default for Timing {
    fn default() -> Self {
        Self {
            acquisition: Duration::from_secs(60),
            send: Duration::from_secs(300),
            interval: Duration::from_secs(1),
        }
    }
}

pub(crate) struct MineruApiClient {
    base: String,
    client: Client,
    timing: Timing,
}
impl MineruApiClient {
    pub(crate) fn new(base_url: &str) -> Result<Self, String> {
        let base = normalize_api_url(base_url);
        if base.is_empty() {
            return Err("API URL is empty".into());
        }
        let client = Client::builder()
            .redirect(Policy::limited(20))
            .connect_timeout(Duration::from_secs(10))
            .build()
            .map_err(|_| "unable to construct API client".to_string())?;
        Ok(Self {
            base,
            client,
            timing: Timing::default(),
        })
    }
    #[cfg(test)]
    fn with_timing(base: &str, timing: Timing) -> Self {
        let mut client = Self::new(base).unwrap();
        client.timing = timing;
        client
    }

    pub(crate) async fn health(&self) -> Result<ServerHealth, String> {
        tokio::time::timeout(self.timing.acquisition, async {
            let response = self
                .client
                .get(format!("{}/health", self.base))
                .send()
                .await
                .map_err(|_| "request connection failed".to_string())?;
            if response.status() != reqwest::StatusCode::OK {
                return Err(self.http_error("health", response).await);
            }
            validate_health(&self.base, &self.json(response).await?)
        })
        .await
        .map_err(|_| "response acquisition timed out".to_string())?
    }

    pub(crate) async fn submit(
        &self,
        options: &RemoteOptions,
        documents: &[InputDocument],
    ) -> Result<SubmitResponse, String> {
        let mut form = Form::new();
        for (key, value) in request_form(options) {
            form = form.text(key, value);
        }
        for document in documents {
            let extension = document
                .path
                .extension()
                .and_then(|v| v.to_str())
                .unwrap_or("");
            let filename = format!(
                "{}{}",
                document.stem,
                if extension.is_empty() {
                    String::new()
                } else {
                    format!(".{extension}")
                }
            );
            let part = Part::file(&document.path)
                .await
                .map_err(|_| "unable to open upload file".to_string())?
                .file_name(filename)
                .mime_str(mime_for(&document.path))
                .map_err(|_| "invalid upload MIME type".to_string())?;
            form = form.part("files", part);
        }
        // One deadline covers upload and response acquisition; reqwest exposes no separate phase here.
        whole_operation_timeout(self.timing.send, async {
            let response = self
                .client
                .post(format!("{}/tasks", self.base))
                .multipart(form)
                .send()
                .await
                .map_err(|_| "task submission failed".to_string())?;
            if response.status() != reqwest::StatusCode::ACCEPTED {
                return Err(self.http_error("task submission", response).await);
            }
            let body = self.body(response).await?;
            let value = serde_json::from_slice(&body).map_err(|_| {
                format!(
                    "invalid JSON payload: {}",
                    sanitize_vlm_error_bytes(&body, BODY_CAP)
                )
            })?;
            submit_response(&value)
        })
        .await
    }

    pub(crate) async fn poll(
        &self,
        status_url: &str,
        env: RemoteEnv,
        mut callback: Option<&mut dyn FnMut(StatusSnapshot)>,
    ) -> Result<(), String> {
        let timeout = Duration::try_from_secs_f64(env.result_timeout_seconds)
            .map_err(|_| "task result deadline is invalid".to_string())?;
        let deadline = tokio::time::Instant::now()
            .checked_add(timeout)
            .ok_or_else(|| "task result deadline is invalid".to_string())?;
        if timeout.is_zero() {
            return Err("task result deadline expired".into());
        }
        tokio::time::timeout_at(deadline, async {
            loop {
                let value = match self.poll_attempt(status_url).await {
                    Ok(value) => value,
                    Err(e) if e == "response acquisition timed out" => {
                        self.sleep().await;
                        continue;
                    }
                    Err(e) => return Err(e),
                };
                let status = value
                    .get("status")
                    .and_then(Value::as_str)
                    .ok_or_else(|| format!("invalid task status payload: {}", safe_json(&value)))?;
                let snapshot = StatusSnapshot {
                    status: status.into(),
                    queued_ahead: value.get("queued_ahead").and_then(Value::as_i64),
                };
                match status {
                    "pending" | "processing" => {
                        if let Some(callback) = callback.as_mut() {
                            // Synchronous callback work cannot be preempted if it runs past the deadline.
                            callback(snapshot);
                        }
                        self.sleep().await;
                    }
                    "completed" => return Ok(()),
                    _ => return Err(format!("task failed: {}", safe_json(&value))),
                }
            }
        })
        .await
        .map_err(|_| "task result deadline expired".to_string())?
    }

    pub(crate) async fn download_result_zip(
        &self,
        result_url: &str,
        task: &str,
        env: RemoteEnv,
        limits: ArchiveLimits,
    ) -> Result<DownloadedZip, String> {
        let timeout = Duration::try_from_secs_f64(env.download_timeout_seconds)
            .map_err(|_| "result download timeout is invalid".to_string())?;
        archive::download(&self.client, result_url, task, timeout, limits).await
    }

    async fn sleep(&self) {
        tokio::time::sleep(self.timing.interval).await;
    }
    async fn poll_attempt(&self, status_url: &str) -> Result<Value, String> {
        tokio::time::timeout(self.timing.acquisition, async {
            let response = self.client.get(status_url).send().await.map_err(|error| {
                if error.is_connect() {
                    "request connection failed".to_string()
                } else {
                    "task status request failed".to_string()
                }
            })?;
            if response.status() != reqwest::StatusCode::OK {
                return Err(self.http_error("task status", response).await);
            }
            self.json(response).await
        })
        .await
        .map_err(|_| "response acquisition timed out".to_string())?
    }
    async fn json(&self, response: Response) -> Result<Value, String> {
        let body = self.body(response).await?;
        serde_json::from_slice(&body).map_err(|_| {
            format!(
                "invalid JSON payload: {}",
                sanitize_vlm_error_bytes(&body, BODY_CAP)
            )
        })
    }
    async fn http_error(&self, context: &str, response: Response) -> String {
        let status = response.status();
        match self.body(response).await {
            Ok(body) => format!(
                "{context} HTTP {status}: {}",
                sanitize_vlm_error_bytes(&body, BODY_CAP)
            ),
            Err(_) => format!("{context} HTTP {status}"),
        }
    }
    async fn body(&self, response: Response) -> Result<Vec<u8>, String> {
        let mut stream = response.bytes_stream();
        let mut body = Vec::new();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|_| "response body failed".to_string())?;
            if checked_body_len(body.len(), chunk.len())? > BODY_CAP {
                return Err("response body exceeds limit".into());
            }
            body.extend_from_slice(&chunk);
        }
        Ok(body)
    }
}
fn checked_body_len(body_len: usize, chunk_len: usize) -> Result<usize, String> {
    body_len
        .checked_add(chunk_len)
        .ok_or_else(|| "response body exceeds limit".into())
}

fn safe_json(value: &Value) -> String {
    sanitize_vlm_error_bytes(value.to_string().as_bytes(), BODY_CAP)
}
fn submit_response(value: &Value) -> Result<SubmitResponse, String> {
    let string = |name: &str| {
        value
            .get(name)
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| format!("invalid submission payload: missing {name}"))
    };
    Ok(SubmitResponse {
        task_id: string("task_id")?,
        status_url: string("status_url")?,
        result_url: string("result_url")?,
        file_names: value
            .get("file_names")
            .and_then(Value::as_array)
            .filter(|v| v.iter().all(Value::is_string))
            .map(|v| {
                v.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default(),
        queued_ahead: value.get("queued_ahead").and_then(Value::as_i64),
    })
}
fn mime_for(path: &Path) -> &'static str {
    match path
        .extension()
        .and_then(|v| v.to_str())
        .unwrap_or("")
        .to_ascii_lowercase()
        .as_str()
    {
        "pdf" => "application/pdf",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "jp2" => "image/jp2",
        "webp" => "image/webp",
        "gif" => "image/gif",
        "bmp" => "image/bmp",
        "tiff" | "tif" => "image/tiff",
        "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "pptx" => "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        "xlsx" => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        Json, Router,
        body::Body,
        extract::Path as AxumPath,
        http::StatusCode,
        response::{IntoResponse, Redirect},
        routing::{get, post},
    };
    use bytes::Bytes;
    use futures_util::stream;
    use serde_json::json;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    async fn server(app: Router) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(axum::serve(listener, app).into_future());
        format!("http://{address}")
    }
    async fn submit_json(body: Value) -> Result<SubmitResponse, String> {
        let base = server(Router::new().route(
            "/tasks",
            post(move || {
                let body = body.clone();
                async move { (StatusCode::ACCEPTED, Json(body)) }
            }),
        ))
        .await;
        MineruApiClient::new(&base)
            .unwrap()
            .submit(&RemoteOptions::default(), &[])
            .await
    }
    #[tokio::test]
    async fn health_validates_http_and_payloads() {
        let good = server(Router::new().route("/health", get(|| async { Json(json!({"status":"healthy","protocol_version":2,"max_concurrent_requests":2,"processing_window_size":-1})) }))).await;
        assert_eq!(
            MineruApiClient::new(&format!("{good}///"))
                .unwrap()
                .health()
                .await
                .unwrap()
                .processing_window_size,
            1
        );
        for body in [
            json!({"status":"bad"}),
            json!({"status":"healthy","protocol_version":1,"max_concurrent_requests":1,"processing_window_size":1}),
            json!({"status":"healthy","protocol_version":2,"max_concurrent_requests":true,"processing_window_size":1}),
        ] {
            let base = server(Router::new().route(
                "/health",
                get(move || {
                    let body = body.clone();
                    async move { Json(body) }
                }),
            ))
            .await;
            assert!(MineruApiClient::new(&base).unwrap().health().await.is_err());
        }
        let bad = server(Router::new().route(
            "/health",
            get(|| async { (StatusCode::INTERNAL_SERVER_ERROR, "token=secret") }),
        ))
        .await;
        let error = MineruApiClient::new(&bad)
            .unwrap()
            .health()
            .await
            .unwrap_err();
        assert!(error.len() <= BODY_CAP + 64 && !error.contains("secret"));
    }
    #[test]
    fn submission_and_mime_normalization_are_exact() {
        let response = submit_response(&json!({"task_id":"id","status_url":"s","result_url":"r","file_names":["a"],"queued_ahead":-2})).unwrap();
        assert_eq!(
            (response.file_names, response.queued_ahead),
            (vec!["a".into()], Some(-2))
        );
        assert!(submit_response(&json!({"task_id":1,"status_url":"s","result_url":"r"})).is_err());
        let response = submit_response(&json!({"task_id":"id","status_url":"s","result_url":"r","file_names":[1],"queued_ahead":true})).unwrap();
        assert_eq!((response.file_names, response.queued_ahead), (vec![], None));
        for (suffix, mime) in [
            ("pdf", "application/pdf"),
            ("png", "image/png"),
            ("jpeg", "image/jpeg"),
            ("jp2", "image/jp2"),
            ("webp", "image/webp"),
            ("gif", "image/gif"),
            ("bmp", "image/bmp"),
            ("tiff", "image/tiff"),
            (
                "docx",
                "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
            ),
            (
                "pptx",
                "application/vnd.openxmlformats-officedocument.presentationml.presentation",
            ),
            (
                "xlsx",
                "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
            ),
            ("unknown", "application/octet-stream"),
        ] {
            assert_eq!(mime_for(Path::new(&format!("a.{suffix}"))), mime);
        }
    }
    #[test]
    fn checked_body_len_rejects_overflow() {
        assert_eq!(
            checked_body_len(usize::MAX, 1),
            Err("response body exceeds limit".into())
        );
    }
    #[tokio::test]
    async fn submit_accepts_normalized_http_payloads() {
        let response = submit_json(json!({
            "task_id":"id", "status_url":"s", "result_url":"r",
            "file_names":["a", "b"], "queued_ahead":-2
        }))
        .await
        .unwrap();
        assert_eq!(
            (
                response.task_id,
                response.status_url,
                response.result_url,
                response.file_names,
                response.queued_ahead
            ),
            (
                "id".into(),
                "s".into(),
                "r".into(),
                vec!["a".into(), "b".into()],
                Some(-2)
            )
        );
        let response = submit_json(json!({"task_id":"", "status_url":"", "result_url":""}))
            .await
            .unwrap();
        assert_eq!(
            (response.task_id, response.status_url, response.result_url),
            ("".into(), "".into(), "".into())
        );
    }
    #[tokio::test]
    async fn submit_rejects_invalid_http_payloads() {
        for body in [
            json!({"status_url":"s", "result_url":"r"}),
            json!({"task_id":"id", "result_url":"r"}),
            json!({"task_id":"id", "status_url":"s"}),
            json!({"task_id":1, "status_url":"s", "result_url":"r"}),
            json!({"task_id":"id", "status_url":false, "result_url":"r"}),
            json!({"task_id":"id", "status_url":"s", "result_url":null}),
        ] {
            assert!(submit_json(body).await.is_err());
        }
        let base = server(Router::new().route(
            "/tasks",
            post(|| async { (StatusCode::ACCEPTED, "not json") }),
        ))
        .await;
        assert!(
            MineruApiClient::new(&base)
                .unwrap()
                .submit(&RemoteOptions::default(), &[])
                .await
                .unwrap_err()
                .starts_with("invalid JSON payload:")
        );
        let base = server(Router::new().route(
            "/tasks",
            post(|| async { (StatusCode::INTERNAL_SERVER_ERROR, "token=secret") }),
        ))
        .await;
        let error = MineruApiClient::new(&base)
            .unwrap()
            .submit(&RemoteOptions::default(), &[])
            .await
            .unwrap_err();
        assert!(error.contains("task submission HTTP 500") && !error.contains("secret"));
    }
    #[tokio::test]
    async fn submit_normalizes_optional_http_fields() {
        for fields in [
            json!({"file_names":["a", 1]}),
            json!({"file_names":"a"}),
            json!({"file_names":["a"], "queued_ahead":true}),
            json!({"file_names":["a"], "queued_ahead":1.5}),
        ] {
            let mut body = json!({"task_id":"id", "status_url":"s", "result_url":"r"});
            body.as_object_mut()
                .unwrap()
                .extend(fields.as_object().unwrap().clone());
            let response = submit_json(body).await.unwrap();
            assert_eq!(
                response.file_names,
                if fields["file_names"] == json!(["a"]) {
                    vec![String::from("a")]
                } else {
                    vec![]
                }
            );
            assert_eq!(response.queued_ahead, None);
        }
    }
    #[tokio::test]
    async fn submit_rejects_oversized_response_body() {
        let body = "x".repeat(BODY_CAP + 1);
        let base = server(Router::new().route(
            "/tasks",
            post(move || {
                let body = body.clone();
                async move { (StatusCode::ACCEPTED, body) }
            }),
        ))
        .await;
        assert_eq!(
            MineruApiClient::new(&base)
                .unwrap()
                .submit(&RemoteOptions::default(), &[])
                .await,
            Err("response body exceeds limit".into())
        );
    }
    #[tokio::test]
    async fn submit_whole_operation_timeout_covers_response_after_fast_upload() {
        let base = server(Router::new().route(
            "/tasks",
            post(|| async {
                tokio::time::sleep(Duration::from_millis(50)).await;
                (
                    StatusCode::ACCEPTED,
                    Json(json!({"task_id":"id", "status_url":"s", "result_url":"r"})),
                )
            }),
        ))
        .await;
        let client = MineruApiClient::with_timing(
            &base,
            Timing {
                acquisition: Duration::from_secs(1),
                send: Duration::from_millis(1),
                interval: Duration::from_secs(1),
            },
        );
        let started = tokio::time::Instant::now();
        assert_eq!(
            client.submit(&RemoteOptions::default(), &[]).await,
            Err("task submission timed out".into())
        );
        assert!(started.elapsed() < Duration::from_millis(40));
    }
    #[tokio::test]
    async fn submit_waits_past_acquisition_for_response_within_whole_operation_deadline() {
        let base = server(Router::new().route(
            "/tasks",
            post(|| async {
                tokio::time::sleep(Duration::from_millis(15)).await;
                (
                    StatusCode::ACCEPTED,
                    Json(json!({"task_id":"id", "status_url":"s", "result_url":"r"})),
                )
            }),
        ))
        .await;
        let client = MineruApiClient::with_timing(
            &base,
            Timing {
                acquisition: Duration::from_millis(5),
                send: Duration::from_millis(40),
                interval: Duration::from_millis(1),
            },
        );
        assert!(client.submit(&RemoteOptions::default(), &[]).await.is_ok());
    }
    #[tokio::test]
    async fn whole_operation_timeout_cancels_progressing_operation_at_its_deadline() {
        let progress = Arc::new(AtomicUsize::new(0));
        let observed = progress.clone();
        assert_eq!(
            whole_operation_timeout(Duration::from_millis(12), async move {
                loop {
                    observed.fetch_add(1, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_millis(3)).await;
                }
                #[allow(unreachable_code)]
                Ok::<(), String>(())
            })
            .await,
            Err("task submission timed out".into())
        );
        assert!(progress.load(Ordering::SeqCst) >= 2);
    }
    #[tokio::test]
    async fn poll_reports_active_snapshots_and_rejects_terminal_errors() {
        let calls = Arc::new(AtomicUsize::new(0));
        let state = calls.clone();
        let base = server(Router::new().route(
            "/status",
            get(move || {
                let state = state.clone();
                async move {
                    match state.fetch_add(1, Ordering::SeqCst) {
                        0 => Json(json!({"status":"pending","queued_ahead":-1})),
                        1 => Json(json!({"status":"processing","queued_ahead":true})),
                        _ => Json(json!({"status":"completed"})),
                    }
                }
            }),
        ))
        .await;
        let client = MineruApiClient::with_timing(
            &base,
            Timing {
                acquisition: Duration::from_millis(100),
                send: Duration::from_millis(100),
                interval: Duration::from_millis(1),
            },
        );
        let mut seen = Vec::new();
        client
            .poll(
                &format!("{base}/status"),
                RemoteEnv {
                    max_concurrent_requests: 1,
                    result_timeout_seconds: 1.,
                    download_timeout_seconds: 1.,
                },
                Some(&mut |snapshot| seen.push(snapshot)),
            )
            .await
            .unwrap();
        assert_eq!(
            seen,
            vec![
                StatusSnapshot {
                    status: "pending".into(),
                    queued_ahead: Some(-1)
                },
                StatusSnapshot {
                    status: "processing".into(),
                    queued_ahead: None
                }
            ]
        );
    }
    #[tokio::test]
    async fn poll_deadline_bounds_semaphore_acquisition() {
        let calls = Arc::new(AtomicUsize::new(0));
        let state = calls.clone();
        let semaphore = Arc::new(tokio::sync::Semaphore::new(0));
        let base = server(Router::new().route(
            "/status",
            get(move || {
                let state = state.clone();
                let semaphore = semaphore.clone();
                async move {
                    state.fetch_add(1, Ordering::SeqCst);
                    let _permit = semaphore.acquire().await.unwrap();
                    Json(json!({"status":"completed"}))
                }
            }),
        ))
        .await;
        let client = MineruApiClient::with_timing(
            &base,
            Timing {
                acquisition: Duration::from_secs(60),
                send: Duration::from_secs(1),
                interval: Duration::from_secs(1),
            },
        );
        let result = tokio::time::timeout(
            Duration::from_secs(1),
            client.poll(
                &format!("{base}/status"),
                RemoteEnv {
                    max_concurrent_requests: 1,
                    result_timeout_seconds: 0.05,
                    download_timeout_seconds: 1.,
                },
                None,
            ),
        )
        .await
        .expect("poll exceeded its result deadline");
        assert_eq!(result, Err("task result deadline expired".into()));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }
    #[tokio::test]
    async fn poll_deadline_bounds_retry_sleep() {
        let calls = Arc::new(AtomicUsize::new(0));
        let state = calls.clone();
        let base = server(Router::new().route(
            "/status",
            get(move || {
                state.fetch_add(1, Ordering::SeqCst);
                async { Json(json!({"status":"pending"})) }
            }),
        ))
        .await;
        let client = MineruApiClient::with_timing(
            &base,
            Timing {
                acquisition: Duration::from_secs(1),
                send: Duration::from_secs(1),
                interval: Duration::from_secs(60),
            },
        );
        let result = tokio::time::timeout(
            Duration::from_secs(1),
            client.poll(
                &format!("{base}/status"),
                RemoteEnv {
                    max_concurrent_requests: 1,
                    result_timeout_seconds: 0.05,
                    download_timeout_seconds: 1.,
                },
                None,
            ),
        )
        .await
        .expect("retry sleep exceeded the result deadline");
        assert_eq!(result, Err("task result deadline expired".into()));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }
    #[tokio::test]
    async fn poll_success_before_deadline_is_unchanged() {
        let calls = Arc::new(AtomicUsize::new(0));
        let state = calls.clone();
        let base = server(Router::new().route(
            "/status",
            get(move || {
                state.fetch_add(1, Ordering::SeqCst);
                async { Json(json!({"status":"completed"})) }
            }),
        ))
        .await;
        assert_eq!(
            MineruApiClient::new(&base)
                .unwrap()
                .poll(
                    &format!("{base}/status"),
                    RemoteEnv {
                        max_concurrent_requests: 1,
                        result_timeout_seconds: 1.,
                        download_timeout_seconds: 1.,
                    },
                    None,
                )
                .await,
            Ok(())
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }
    #[tokio::test]
    async fn health_follows_twenty_redirects_but_not_twenty_one() {
        // reqwest's limited(20) permits 20 redirect responses total, including /health -> /redirect/19.
        let base = server(
            Router::new()
                .route("/health", get(|| async { Redirect::temporary("/redirect/19") }))
                .route(
                    "/redirect/{remaining}",
                    get(|AxumPath(remaining): AxumPath<usize>| async move {
                        if remaining == 0 {
                            (StatusCode::OK, Json(json!({"status":"healthy","protocol_version":2,"max_concurrent_requests":1,"processing_window_size":1}))).into_response()
                        } else {
                            Redirect::temporary(&format!("/redirect/{}", remaining - 1)).into_response()
                        }
                    }),
                ),
        )
        .await;
        assert!(MineruApiClient::new(&base).unwrap().health().await.is_ok());
        let base = server(
            Router::new()
                .route(
                    "/health",
                    get(|| async { Redirect::temporary("/redirect/20") }),
                )
                .route(
                    "/redirect/{remaining}",
                    get(|AxumPath(remaining): AxumPath<usize>| async move {
                        Redirect::temporary(&format!("/redirect/{}", remaining - 1))
                    }),
                ),
        )
        .await;
        assert_eq!(
            MineruApiClient::new(&base).unwrap().health().await,
            Err("request connection failed".into())
        );
    }
    #[tokio::test]
    async fn health_rejects_malformed_oversized_and_sanitized_bodies_and_times_out() {
        for body in ["token=secret not-json".to_owned(), "x".repeat(BODY_CAP + 1)] {
            let base = server(Router::new().route(
                "/health",
                get(move || {
                    let body = body.clone();
                    async move { (StatusCode::OK, body) }
                }),
            ))
            .await;
            let error = MineruApiClient::new(&base)
                .unwrap()
                .health()
                .await
                .unwrap_err();
            assert!(!error.contains("secret"));
        }
        let base = server(Router::new().route(
            "/health",
            get(|| async {
                tokio::time::sleep(Duration::from_millis(30)).await;
                (StatusCode::OK, "{}")
            }),
        ))
        .await;
        assert_eq!(
            MineruApiClient::with_timing(
                &base,
                Timing {
                    acquisition: Duration::from_millis(5),
                    send: Duration::from_millis(50),
                    interval: Duration::from_millis(1)
                }
            )
            .health()
            .await,
            Err("response acquisition timed out".into())
        );
    }
    #[tokio::test]
    async fn poll_rejects_invalid_and_sanitizes_error_payloads() {
        let cases = [
            (StatusCode::OK, "{}"),
            (StatusCode::OK, r#"{"status":"failed","token":"secret"}"#),
            (StatusCode::OK, r#"{"status":"mystery","token":"secret"}"#),
            (StatusCode::INTERNAL_SERVER_ERROR, "token=secret"),
            (StatusCode::OK, "token=secret not-json"),
        ];
        for (status, body) in cases {
            let base =
                server(Router::new().route("/status", get(move || async move { (status, body) })))
                    .await;
            let error = MineruApiClient::new(&base)
                .unwrap()
                .poll(
                    &format!("{base}/status"),
                    RemoteEnv {
                        max_concurrent_requests: 1,
                        result_timeout_seconds: 1.,
                        download_timeout_seconds: 1.,
                    },
                    None,
                )
                .await
                .unwrap_err();
            assert!(!error.contains("secret"), "{error}");
        }
        let body = "x".repeat(BODY_CAP + 1);
        let base = server(Router::new().route(
            "/status",
            get(move || {
                let body = body.clone();
                async move { (StatusCode::OK, body) }
            }),
        ))
        .await;
        assert_eq!(
            MineruApiClient::new(&base)
                .unwrap()
                .poll(
                    &format!("{base}/status"),
                    RemoteEnv {
                        max_concurrent_requests: 1,
                        result_timeout_seconds: 1.,
                        download_timeout_seconds: 1.
                    },
                    None
                )
                .await,
            Err("response body exceeds limit".into())
        );
    }
    #[tokio::test]
    async fn poll_rejects_huge_deadline_and_escapes_untrusted_terminal_status() {
        let base = server(Router::new().route(
            "/status",
            get(|| async { Json(json!({"status":"failed token=secret\n\u{1b}"})) }),
        ))
        .await;
        let client = MineruApiClient::new(&base).unwrap();
        let env = RemoteEnv {
            max_concurrent_requests: 1,
            result_timeout_seconds: 1.,
            download_timeout_seconds: 1.,
        };
        let error = client
            .poll(&format!("{base}/status"), env, None)
            .await
            .unwrap_err();
        assert!(!error.contains("secret") && !error.contains('\n') && !error.contains('\u{1b}'));
        assert!(error.contains("\\n") || error.contains("\\u001b") || error.contains("[REDACTED]"));
        assert_eq!(
            client
                .poll(
                    &format!("{base}/status"),
                    RemoteEnv {
                        result_timeout_seconds: f64::MAX,
                        ..env
                    },
                    None,
                )
                .await,
            Err("task result deadline is invalid".into())
        );
        for seconds in [0., f64::MIN_POSITIVE] {
            assert_eq!(
                client
                    .poll(
                        &format!("{base}/status"),
                        RemoteEnv {
                            result_timeout_seconds: seconds,
                            ..env
                        },
                        None,
                    )
                    .await,
                Err("task result deadline expired".into())
            );
        }
    }
    #[tokio::test]
    async fn poll_retries_acquisition_timeout_then_reports_pending_and_completes() {
        let calls = Arc::new(AtomicUsize::new(0));
        let state = calls.clone();
        let base = server(Router::new().route(
            "/status",
            get(move || {
                let state = state.clone();
                async move {
                    match state.fetch_add(1, Ordering::SeqCst) {
                        0 => {
                            tokio::time::sleep(Duration::from_millis(25)).await;
                            Json(json!({"status":"completed"}))
                        }
                        1 => Json(json!({"status":"pending"})),
                        _ => Json(json!({"status":"completed"})),
                    }
                }
            }),
        ))
        .await;
        let client = MineruApiClient::with_timing(
            &base,
            Timing {
                acquisition: Duration::from_millis(5),
                send: Duration::from_millis(50),
                interval: Duration::from_millis(10),
            },
        );
        let mut seen = Vec::new();
        client
            .poll(
                &format!("{base}/status"),
                RemoteEnv {
                    max_concurrent_requests: 1,
                    result_timeout_seconds: 1.,
                    download_timeout_seconds: 1.,
                },
                Some(&mut |snapshot| seen.push(snapshot)),
            )
            .await
            .unwrap();
        // The client may time out before a locally scheduled response is observed;
        // only the timeout -> pending -> completed state sequence is contractual.
        assert!(calls.load(Ordering::SeqCst) >= 3);
        assert_eq!(
            seen,
            vec![StatusSnapshot {
                status: "pending".into(),
                queued_ahead: None
            }]
        );
    }
    #[tokio::test]
    async fn poll_connection_errors_and_deadline_expiration_are_preserved() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let refused = format!("http://{}/status", listener.local_addr().unwrap());
        drop(listener);
        let base = "http://127.0.0.1:1";
        let client = MineruApiClient::with_timing(
            base,
            Timing {
                acquisition: Duration::from_millis(50),
                send: Duration::from_millis(50),
                interval: Duration::from_millis(20),
            },
        );
        let started = tokio::time::Instant::now();
        assert_eq!(
            client
                .poll(
                    &refused,
                    RemoteEnv {
                        max_concurrent_requests: 1,
                        result_timeout_seconds: 1.,
                        download_timeout_seconds: 1.
                    },
                    None
                )
                .await,
            Err("request connection failed".into())
        );
        assert!(started.elapsed() < Duration::from_millis(100));
        for timeout in [false, true] {
            let base = server(Router::new().route(
                "/status",
                get(move || async move {
                    if timeout {
                        tokio::time::sleep(Duration::from_millis(15)).await;
                    }
                    Json(json!({"status":"pending"}))
                }),
            ))
            .await;
            let client = MineruApiClient::with_timing(
                &base,
                Timing {
                    acquisition: Duration::from_millis(5),
                    send: Duration::from_millis(50),
                    interval: Duration::from_millis(20),
                },
            );
            assert_eq!(
                client
                    .poll(
                        &format!("{base}/status"),
                        RemoteEnv {
                            max_concurrent_requests: 1,
                            result_timeout_seconds: 0.01,
                            download_timeout_seconds: 1.
                        },
                        None
                    )
                    .await,
                Err("task result deadline expired".into())
            );
        }
    }
    #[tokio::test]
    async fn submit_writes_ordered_multipart_parts() {
        use axum::{
            body::to_bytes,
            extract::{Request, State},
            routing::post,
        };
        use std::{path::PathBuf, sync::Mutex};
        let captured = Arc::new(Mutex::new(Vec::<(String, Vec<u8>)>::new()));
        let state = captured.clone();
        let app =
            Router::new()
                .route(
                    "/tasks",
                    post(
                        |State(state): State<Arc<Mutex<Vec<(String, Vec<u8>)>>>>,
                         request: Request| async move {
                            let content_type = request
                                .headers()
                                .get("content-type")
                                .unwrap()
                                .to_str()
                                .unwrap()
                                .to_owned();
                            let body = to_bytes(request.into_body(), 1024 * 1024)
                                .await
                                .unwrap()
                                .to_vec();
                            state.lock().unwrap().push((content_type, body));
                            (
                                StatusCode::ACCEPTED,
                                Json(json!({"task_id":"","status_url":"","result_url":""})),
                            )
                        },
                    ),
                )
                .with_state(state);
        let base = server(app).await;
        let directory = tempfile::tempdir().unwrap();
        let mut documents = Vec::new();
        let expected = [
            ("pdf", "application/pdf"),
            ("png", "image/png"),
            ("jpeg", "image/jpeg"),
            ("jp2", "image/jp2"),
            ("webp", "image/webp"),
            ("gif", "image/gif"),
            ("bmp", "image/bmp"),
            ("jpg", "image/jpeg"),
            ("tiff", "image/tiff"),
            (
                "docx",
                "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
            ),
            (
                "pptx",
                "application/vnd.openxmlformats-officedocument.presentationml.presentation",
            ),
            (
                "xlsx",
                "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
            ),
        ];
        for (index, (suffix, _)) in expected.iter().enumerate() {
            let path = directory.path().join(format!("source.{suffix}"));
            std::fs::write(&path, format!("bytes-{suffix}")).unwrap();
            documents.push(InputDocument {
                path: PathBuf::from(&path),
                suffix: (*suffix).into(),
                stem: format!("normalized-{index}"),
                effective_pages: 1,
                order: index,
            });
        }
        MineruApiClient::new(&base)
            .unwrap()
            .submit(&RemoteOptions::default(), &documents)
            .await
            .unwrap();
        let (content_type, body) = captured.lock().unwrap().pop().unwrap();
        let boundary = content_type
            .strip_prefix("multipart/form-data; boundary=")
            .unwrap();
        let delimiter = format!("--{boundary}\r\n");
        let body = String::from_utf8(body).unwrap();
        let body = body
            .strip_suffix(&format!("\r\n--{boundary}--\r\n"))
            .unwrap();
        let parts: Vec<_> = body
            .split(&delimiter)
            .skip(1)
            .map(|part| part.strip_suffix("\r\n").unwrap_or(part))
            .take_while(|part| *part != format!("--{boundary}--\r\n"))
            .collect();
        assert_eq!(
            parts.len(),
            request_form(&RemoteOptions::default()).len() + 12
        );
        for ((key, value), part) in request_form(&RemoteOptions::default()).iter().zip(&parts) {
            assert_eq!(
                *part,
                format!("Content-Disposition: form-data; name=\"{key}\"\r\n\r\n{value}")
            );
        }
        for ((index, (suffix, mime)), part) in expected
            .iter()
            .enumerate()
            .zip(&parts[request_form(&RemoteOptions::default()).len()..])
        {
            assert_eq!(
                *part,
                format!(
                    "Content-Disposition: form-data; name=\"files\"; filename=\"normalized-{index}.{suffix}\"\r\nContent-Type: {mime}\r\n\r\nbytes-{suffix}"
                )
            );
        }
    }

    fn download_env(seconds: f64) -> RemoteEnv {
        RemoteEnv {
            max_concurrent_requests: 1,
            result_timeout_seconds: 1.,
            download_timeout_seconds: seconds,
        }
    }

    #[tokio::test]
    async fn download_streams_chunks_and_removes_the_archive_on_drop() {
        let bytes = b"PK\x03\x04first-second".to_vec();
        let base = server(Router::new().route(
            "/result",
            get(|| async {
                (
                    [("content-type", "application/zip")],
                    Body::from_stream(stream::iter(vec![
                        Ok::<_, std::convert::Infallible>(Bytes::from_static(b"PK\x03\x04first-")),
                        Ok(Bytes::from_static(b"second")),
                    ])),
                )
            }),
        ))
        .await;
        let archive = MineruApiClient::new(&base)
            .unwrap()
            .download_result_zip(
                &format!("{base}/result"),
                "task id",
                download_env(1.),
                ArchiveLimits::default(),
            )
            .await
            .unwrap();
        let path = archive.path().to_owned();
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        assert!(path.exists());
        drop(archive);
        assert!(!path.exists());
    }

    #[tokio::test]
    async fn download_rejects_bad_status_and_content_type() {
        let secret = format!("token=secret{}", "x".repeat(BODY_CAP));
        let base = server(
            Router::new()
                .route(
                    "/bad",
                    get(move || {
                        let secret = secret.clone();
                        async move { (StatusCode::INTERNAL_SERVER_ERROR, secret) }
                    }),
                )
                .route("/type", get(|| async { "not a zip" })),
        )
        .await;
        let client = MineruApiClient::new(&base).unwrap();
        let error = client
            .download_result_zip(
                &format!("{base}/bad"),
                "task",
                download_env(1.),
                ArchiveLimits::default(),
            )
            .await
            .unwrap_err();
        assert!(error.len() <= BODY_CAP + 64 && !error.contains("secret"));
        assert!(
            client
                .download_result_zip(
                    &format!("{base}/type"),
                    "task",
                    download_env(1.),
                    ArchiveLimits::default()
                )
                .await
                .unwrap_err()
                .contains("Content-Type")
        );
    }

    #[tokio::test]
    async fn download_times_out_for_acquisition_and_each_chunk() {
        let base = server(
            Router::new()
                .route(
                    "/late",
                    get(|| async {
                        tokio::time::sleep(Duration::from_millis(30)).await;
                        ([("content-type", "application/zip")], "x")
                    }),
                )
                .route(
                    "/stream",
                    get(|| async {
                        let body = Body::from_stream(
                            stream::once(async {
                                Ok::<_, std::convert::Infallible>(Bytes::from_static(b"a"))
                            })
                            .chain(stream::once(async {
                                tokio::time::sleep(Duration::from_millis(30)).await;
                                Ok(Bytes::from_static(b"b"))
                            })),
                        );
                        ([("content-type", "application/zip")], body)
                    }),
                ),
        )
        .await;
        let client = MineruApiClient::new(&base).unwrap();
        for path in ["late", "stream"] {
            assert!(
                client
                    .download_result_zip(
                        &format!("{base}/{path}"),
                        "task",
                        download_env(0.005),
                        ArchiveLimits::default()
                    )
                    .await
                    .unwrap_err()
                    .contains("timed out")
            );
        }
    }

    #[tokio::test]
    async fn download_enforces_actual_streamed_byte_limits_and_validates_timeout() {
        let base = server(Router::new().route(
            "/result",
            get(|| async {
                (
                    [("content-type", "application/zip")],
                    Body::from_stream(stream::iter(vec![
                        Ok::<_, std::convert::Infallible>(Bytes::from_static(b"abc")),
                        Ok(Bytes::from_static(b"de")),
                    ])),
                )
            }),
        ))
        .await;
        let client = MineruApiClient::new(&base).unwrap();
        let mut limits = ArchiveLimits::default();
        limits.max_compressed_bytes = 5;
        assert!(
            client
                .download_result_zip(&format!("{base}/result"), "task", download_env(1.), limits)
                .await
                .is_ok()
        );
        limits.max_compressed_bytes = 4;
        assert!(
            client
                .download_result_zip(&format!("{base}/result"), "task", download_env(1.), limits)
                .await
                .unwrap_err()
                .contains("compressed size")
        );
        for seconds in [f64::MAX, f64::NAN] {
            assert_eq!(
                client
                    .download_result_zip(
                        &format!("{base}/result"),
                        "task",
                        download_env(seconds),
                        ArchiveLimits::default()
                    )
                    .await
                    .unwrap_err(),
                "result download timeout is invalid"
            );
        }
    }

    #[tokio::test]
    async fn download_rejects_a_body_stream_error() {
        let base = server(Router::new().route(
            "/result",
            get(|| async {
                (
                    [("content-type", "application/zip")],
                    Body::from_stream(
                        stream::once(async {
                            Ok::<_, std::io::Error>(Bytes::from_static(b"partial"))
                        })
                        .chain(stream::once(async {
                            tokio::time::sleep(Duration::from_millis(10)).await;
                            Err(std::io::Error::other("broken stream"))
                        })),
                    ),
                )
            }),
        ))
        .await;
        assert_eq!(
            MineruApiClient::new(&base)
                .unwrap()
                .download_result_zip(
                    &format!("{base}/result"),
                    "task",
                    download_env(1.),
                    ArchiveLimits::default()
                )
                .await
                .unwrap_err(),
            "\"task\" result download body failed"
        );
    }

    #[tokio::test]
    async fn non_ok_download_delayed_body_times_out() {
        let base = server(Router::new().route(
            "/result",
            get(|| async {
                let body = Body::from_stream(stream::once(async {
                    tokio::time::sleep(Duration::from_millis(30)).await;
                    Ok::<_, std::convert::Infallible>(Bytes::from_static(b"diagnostic"))
                }));
                (StatusCode::INTERNAL_SERVER_ERROR, body)
            }),
        ))
        .await;
        assert_eq!(
            MineruApiClient::new(&base)
                .unwrap()
                .download_result_zip(
                    &format!("{base}/result"),
                    "task",
                    download_env(0.005),
                    ArchiveLimits::default()
                )
                .await
                .unwrap_err(),
            "\"task\" result download timed out"
        );
    }

    #[tokio::test]
    async fn non_ok_download_body_stream_error_is_sanitized() {
        let base = server(Router::new().route(
            "/result",
            get(|| async {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Body::from_stream(
                        stream::once(async {
                            Ok::<_, std::io::Error>(Bytes::from_static(b"partial diagnostic"))
                        })
                        .chain(stream::once(async {
                            tokio::time::sleep(Duration::from_millis(10)).await;
                            Err(std::io::Error::other("broken diagnostic"))
                        })),
                    ),
                )
            }),
        ))
        .await;
        let error = MineruApiClient::new(&base)
            .unwrap()
            .download_result_zip(
                &format!("{base}/result"),
                "token=secret\n\u{1b}",
                download_env(1.),
                ArchiveLimits::default(),
            )
            .await
            .unwrap_err();
        assert!(error.contains("result download body failed") && !error.contains("HTTP 500"));
        assert!(error.len() <= BODY_CAP + 96);
        assert!(!error.contains("secret") && !error.contains('\n') && !error.contains('\u{1b}'));
    }

    #[tokio::test]
    async fn download_sanitizes_malicious_task_labels_on_all_error_branches() {
        let task = "token=secret\n\u{1b}";
        let delayed = server(Router::new().route(
            "/result",
            get(|| async {
                tokio::time::sleep(Duration::from_millis(30)).await;
                ([("content-type", "application/zip")], "x")
            }),
        ))
        .await;
        let non_ok = server(Router::new().route(
            "/result",
            get(|| async { (StatusCode::INTERNAL_SERVER_ERROR, "diagnostic") }),
        ))
        .await;
        let broken = server(Router::new().route(
            "/result",
            get(|| async {
                (
                    [("content-type", "application/zip")],
                    Body::from_stream(
                        stream::once(async {
                            Ok::<_, std::io::Error>(Bytes::from_static(b"partial"))
                        })
                        .chain(stream::once(async {
                            tokio::time::sleep(Duration::from_millis(10)).await;
                            Err(std::io::Error::other("broken"))
                        })),
                    ),
                )
            }),
        ))
        .await;
        for (base, timeout) in [(delayed, 0.005), (non_ok, 1.), (broken, 1.)] {
            let error = MineruApiClient::new(&base)
                .unwrap()
                .download_result_zip(
                    &format!("{base}/result"),
                    task,
                    download_env(timeout),
                    ArchiveLimits::default(),
                )
                .await
                .unwrap_err();
            assert!(error.len() <= BODY_CAP + 96, "{error}");
            assert!(
                !error.contains("secret") && !error.contains('\n') && !error.contains('\u{1b}')
            );
        }
    }
}
