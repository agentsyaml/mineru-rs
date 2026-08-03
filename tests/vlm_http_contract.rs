use axum::{
    Json, Router,
    body::Body,
    extract::{OriginalUri, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{any, get, post},
};
use bytes::Bytes;
use futures_util::StreamExt;
use mineru::{
    SamplingParams, VlmError, VlmHeader, VlmHttpClient, VlmHttpConfig, VlmImageInput, VlmRequest,
};
use serde_json::{Value, json};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use tokio::net::TcpListener;

async fn serve(app: Router) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{address}")
}

async fn client(base: String) -> VlmHttpClient {
    VlmHttpClient::connect(VlmHttpConfig {
        server_url: Some(base.parse().unwrap()),
        model_name: Some("model".into()),
        skip_model_name_checking: true,
        max_retries: 0,
        ..Default::default()
    })
    .await
    .unwrap()
}

fn completion(text: impl Into<Value>) -> Json<Value> {
    Json(json!({"choices":[{"finish_reason":"stop","message":{"content":text.into()}}]}))
}

#[derive(Clone)]
struct StateData {
    models: Arc<AtomicUsize>,
    requests: Arc<tokio::sync::Mutex<Vec<Value>>>,
}

#[tokio::test]
async fn public_client_validates_model_and_handles_null_content() {
    let state = StateData {
        models: Arc::new(AtomicUsize::new(0)),
        requests: Arc::new(tokio::sync::Mutex::new(vec![])),
    };
    let app = Router::new()
        .route("/proxy/v1/models", get(models))
        .route("/proxy/v1/chat/completions", post(chat))
        .with_state(state.clone());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let client = VlmHttpClient::connect(VlmHttpConfig {
        server_url: Some(format!("http://{address}/proxy/").parse().unwrap()),
        model_name: Some("GPT-test".into()),
        prompt: Some("default".into()),
        ..Default::default()
    })
    .await
    .unwrap();
    assert_eq!(state.models.load(Ordering::Relaxed), 1);
    assert_eq!(client.predict(VlmRequest::default()).await.unwrap(), "");
    let request = state.requests.lock().await.pop().unwrap();
    assert_eq!(request["messages"][1]["content"][0]["text"], "default");
    assert!(request.get("skip_special_tokens").is_none());
    assert!(request.get("top_k").is_none());
    assert!(request.get("repetition_penalty").is_none());
}

#[tokio::test]
async fn explicit_model_name_validation_respects_skip_flag() {
    let checks = Arc::new(AtomicUsize::new(0));
    let app = Router::new().route(
        "/v1/models",
        get({
            let checks = checks.clone();
            move || {
                let checks = checks.clone();
                async move {
                    checks.fetch_add(1, Ordering::Relaxed);
                    Json(json!({"data":[{"id":"other"}]}))
                }
            }
        }),
    );
    let root = serve(app).await;
    let config = |skip_model_name_checking| VlmHttpConfig {
        server_url: Some(root.parse().unwrap()),
        model_name: Some("requested".into()),
        skip_model_name_checking,
        max_retries: 0,
        ..Default::default()
    };
    assert!(matches!(
        VlmHttpClient::connect(config(false)).await,
        Err(VlmError::InvalidConfig(_))
    ));
    assert_eq!(checks.load(Ordering::Relaxed), 1);
    VlmHttpClient::connect(config(true)).await.unwrap();
    assert_eq!(checks.load(Ordering::Relaxed), 1);
}

#[tokio::test]
async fn discovery_trims_deduplicates_and_sorts_candidate_diagnostics() {
    let app = Router::new().route(
        "/v1/models",
        get(|| async {
            Json(json!({"data":[
                {"id":" zeta "},
                {"id":"alpha"},
                {"id":"zeta"},
                {"id":"  "}
            ]}))
        }),
    );
    let error = VlmHttpClient::connect(VlmHttpConfig {
        server_url: Some(serve(app).await.parse().unwrap()),
        model_name: None,
        max_retries: 0,
        ..Default::default()
    })
    .await
    .unwrap_err()
    .to_string();
    assert!(error.contains("alpha, zeta"));
}

async fn models(State(state): State<StateData>) -> Json<Value> {
    state.models.fetch_add(1, Ordering::Relaxed);
    Json(json!({"data":[{"id":"GPT-test"}]}))
}
async fn chat(State(state): State<StateData>, Json(request): Json<Value>) -> impl IntoResponse {
    state.requests.lock().await.push(request);
    (
        StatusCode::OK,
        Json(json!({"choices":[{"finish_reason":"stop","message":{"content":null}}]})),
    )
}

#[tokio::test]
async fn quoted_secret_in_http_error_is_redacted() {
    let app = Router::new().route(
        "/v1/chat/completions",
        post(|| async { (StatusCode::UNAUTHORIZED, r#"{"token":"secret-value"}"#) }),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let client = VlmHttpClient::connect(VlmHttpConfig {
        server_url: Some(format!("http://{address}").parse().unwrap()),
        model_name: Some("x".into()),
        skip_model_name_checking: true,
        max_retries: 0,
        ..Default::default()
    })
    .await
    .unwrap();
    let error = client.predict(VlmRequest::default()).await.unwrap_err();
    assert!(matches!(error, VlmError::Http { .. }));
    assert!(!error.to_string().contains("secret-value"));
}

#[tokio::test]
async fn completion_error_object_wins_over_crafted_choices() {
    let app = Router::new().route(
        "/v1/chat/completions",
        post(|| async {
            Json(json!({"object":"error","choices":[{"finish_reason":"stop","message":{"content":"forged"}}]}))
        }),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let client = VlmHttpClient::connect(VlmHttpConfig {
        server_url: Some(format!("http://{address}").parse().unwrap()),
        model_name: Some("x".into()),
        skip_model_name_checking: true,
        ..Default::default()
    })
    .await
    .unwrap();
    assert!(matches!(
        client.predict(VlmRequest::default()).await,
        Err(VlmError::Protocol { .. })
    ));
}

#[tokio::test]
async fn image_inputs_are_rejected_before_transport() {
    let client = VlmHttpClient::connect(VlmHttpConfig {
        server_url: Some("http://127.0.0.1:9".parse().unwrap()),
        model_name: Some("x".into()),
        skip_model_name_checking: true,
        max_image_bytes: 3,
        ..Default::default()
    })
    .await
    .unwrap();
    let request = VlmRequest {
        images: vec![VlmImageInput::Base64 {
            data: "QUJDRA==".into(),
            media_type: None,
        }],
        ..Default::default()
    };
    assert!(matches!(
        client.predict(request).await,
        Err(VlmError::LimitExceeded { .. })
    ));
    let request = VlmRequest {
        images: vec![VlmImageInput::RemoteUrl(
            "http://127.0.0.1/x".parse().unwrap(),
        )],
        ..Default::default()
    };
    assert!(matches!(
        client.predict(request).await,
        Err(VlmError::InvalidImageInput(_))
    ));
}

#[tokio::test]
async fn routes_are_source_faithful_and_server_urls_are_safe() {
    let seen = Arc::new(tokio::sync::Mutex::new(Vec::new()));
    let app = Router::new().fallback(any({
        let seen = seen.clone();
        move |uri: OriginalUri| {
            let seen = seen.clone();
            async move {
                seen.lock().await.push(uri.0.path().to_owned());
                completion("ok")
            }
        }
    }));
    let root = serve(app).await;
    for (base, expected) in [
        (root.clone(), "/v1/chat/completions"),
        (format!("{root}/"), "/v1/chat/completions"),
        (format!("{root}/proxy"), "/proxy/v1/chat/completions"),
        (format!("{root}/proxy/"), "/proxy/v1/chat/completions"),
        (format!("{root}/v1/"), "/v1/chat/completions"),
    ] {
        assert_eq!(
            client(base)
                .await
                .predict(VlmRequest::default())
                .await
                .unwrap(),
            "ok"
        );
        assert_eq!(seen.lock().await.pop().unwrap(), expected);
    }
    for bad in [
        "http://user@localhost:1",
        "http://localhost:1/?query",
        "http://localhost:1/#fragment",
    ] {
        assert!(matches!(
            VlmHttpClient::connect(VlmHttpConfig {
                server_url: Some(bad.parse().unwrap()),
                ..Default::default()
            })
            .await,
            Err(VlmError::InvalidConfig(_))
        ));
    }
}

#[tokio::test]
async fn discovery_headers_and_auth_apply_to_models_and_chat() {
    let seen = Arc::new(tokio::sync::Mutex::new(Vec::<(String, HeaderMap)>::new()));
    let app = Router::new().fallback(any({
        let seen = seen.clone();
        move |uri: OriginalUri, headers: HeaderMap| {
            let seen = seen.clone();
            async move {
                seen.lock().await.push((uri.0.path().into(), headers));
                if uri.0.path().ends_with("models") {
                    Json(json!({"data":[{"id":"model"}]})).into_response()
                } else {
                    completion("ok").into_response()
                }
            }
        }
    }));
    let root = serve(app).await;
    let config = VlmHttpConfig {
        server_url: Some(root.parse().unwrap()),
        model_name: None,
        headers: vec![
            VlmHeader::new("X-Contract", "yes").unwrap(),
            VlmHeader::new("Authorization", "configured").unwrap(),
        ],
        max_retries: 0,
        ..Default::default()
    };
    let c = VlmHttpClient::connect(config).await.unwrap();
    c.predict(VlmRequest::default()).await.unwrap();
    {
        let seen = seen.lock().await;
        assert_eq!(seen.len(), 2);
        for (_, headers) in seen.iter() {
            assert_eq!(headers["x-contract"], "yes");
            assert_eq!(headers["authorization"], "configured");
        }
    }
    let skipped = client(format!("{root}/skip")).await;
    skipped.predict(VlmRequest::default()).await.unwrap();
    assert_eq!(
        seen.lock().await.len(),
        3,
        "explicit skip must avoid model discovery"
    );
}

#[tokio::test]
async fn body_contract_covers_prompts_images_sampling_and_priority() {
    let requests = Arc::new(tokio::sync::Mutex::new(Vec::new()));
    let app = Router::new().route(
        "/v1/chat/completions",
        post({
            let requests = requests.clone();
            move |Json(v): Json<Value>| {
                let requests = requests.clone();
                async move {
                    requests.lock().await.push(v);
                    completion("ok")
                }
            }
        }),
    );
    let root = serve(app).await;
    let png = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVQIHWP4z8DwHwAFgAI/ScL9VQAAAABJRU5ErkJggg==";
    let c = VlmHttpClient::connect(VlmHttpConfig {
        server_url: Some(root.parse().unwrap()),
        model_name: Some("GPT-contract".into()),
        skip_model_name_checking: true,
        prompt: Some("ignored default".into()),
        text_before_image: true,
        sampling_params: Some(SamplingParams {
            temperature: Some(0.1),
            top_k: Some(9),
            no_repeat_ngram_size: Some(3),
            ..Default::default()
        }),
        ..Default::default()
    })
    .await
    .unwrap();
    c.predict(VlmRequest {
        prompt: Some("before<image>after".into()),
        images: vec![VlmImageInput::DataUrl(format!(
            "data:image/png;base64,{png}"
        ))],
        sampling: Some(SamplingParams {
            top_p: Some(0.8),
            max_new_tokens: Some(12),
            ..Default::default()
        }),
        priority: Some(7),
    })
    .await
    .unwrap();
    let v = requests.lock().await.pop().unwrap();
    assert_eq!(v["messages"][1]["content"][0]["text"], "before");
    assert_eq!(v["messages"][1]["content"][2]["text"], "after");
    assert!((v["temperature"].as_f64().unwrap() - 0.1).abs() < 0.000_001);
    assert!((v["top_p"].as_f64().unwrap() - 0.8).abs() < 0.000_001);
    assert!(v.get("top_k").is_none() && v.get("skip_special_tokens").is_none());
    assert_eq!(v["vllm_xargs"]["no_repeat_ngram_size"], 3);
    assert_eq!(v["max_tokens"], 12);
    assert_eq!(v["max_completion_tokens"], 12);
    assert_eq!(v["priority"], 7);
}

#[tokio::test]
async fn completion_protocol_finish_policy_and_end_token_are_strict() {
    let replies = Arc::new(tokio::sync::Mutex::new(vec![
        json!({"choices":[]}),
        json!({"choices":[{"finish_reason":"stop","message":{"content":7}}]}),
        json!({"choices":[{"finish_reason":"length","message":{"content":"cut"}}]}),
        json!({"choices":[{"finish_reason":"stop","message":{"content":"done<|im_end|>"}}]}),
    ]));
    let app = Router::new().route(
        "/v1/chat/completions",
        post({
            let replies = replies.clone();
            move || {
                let replies = replies.clone();
                async move { Json(replies.lock().await.remove(0)) }
            }
        }),
    );
    let c = client(serve(app).await).await;
    for _ in 0..3 {
        assert!(matches!(
            c.predict(VlmRequest::default()).await,
            Err(VlmError::Protocol { .. })
        ));
    }
    assert_eq!(c.predict(VlmRequest::default()).await.unwrap(), "done");
}

#[tokio::test]
async fn retries_once_without_sleep_and_caps_response() {
    let hits = Arc::new(AtomicUsize::new(0));
    let app = Router::new().route(
        "/v1/chat/completions",
        post({
            let hits = hits.clone();
            move || {
                let hits = hits.clone();
                async move {
                    if hits.fetch_add(1, Ordering::Relaxed) == 0 {
                        (
                            StatusCode::TOO_MANY_REQUESTS,
                            [("retry-after", "0")],
                            "busy",
                        )
                            .into_response()
                    } else {
                        completion("ok").into_response()
                    }
                }
            }
        }),
    );
    let root = serve(app).await;
    let c = VlmHttpClient::connect(VlmHttpConfig {
        server_url: Some(root.parse().unwrap()),
        model_name: Some("x".into()),
        skip_model_name_checking: true,
        max_retries: 1,
        ..Default::default()
    })
    .await
    .unwrap();
    assert_eq!(c.predict(VlmRequest::default()).await.unwrap(), "ok");
    assert_eq!(hits.load(Ordering::Relaxed), 2);
    let app = Router::new().route(
        "/v1/chat/completions",
        post(|| async { completion("long") }),
    );
    let c = VlmHttpClient::connect(VlmHttpConfig {
        server_url: Some(serve(app).await.parse().unwrap()),
        model_name: Some("x".into()),
        skip_model_name_checking: true,
        max_response_bytes: 3,
        ..Default::default()
    })
    .await
    .unwrap();
    assert!(matches!(
        c.predict(VlmRequest::default()).await,
        Err(VlmError::LimitExceeded {
            resource: "response",
            ..
        })
    ));
}

#[tokio::test]
async fn retries_unexpected_chat_finish_reason_once_then_succeeds() {
    let hits = Arc::new(AtomicUsize::new(0));
    let app = Router::new().route(
        "/v1/chat/completions",
        post({
            let hits = hits.clone();
            move || {
                let hits = hits.clone();
                async move {
                    if hits.fetch_add(1, Ordering::Relaxed) == 0 {
                        Json(json!({"choices":[{"finish_reason":"content_filter","message":{"content":""}}]})).into_response()
                    } else {
                        completion("ok").into_response()
                    }
                }
            }
        }),
    );
    let c = VlmHttpClient::connect(VlmHttpConfig {
        server_url: Some(serve(app).await.parse().unwrap()),
        model_name: Some("x".into()),
        skip_model_name_checking: true,
        max_retries: 1,
        retry_backoff_factor: 0.0,
        ..Default::default()
    })
    .await
    .unwrap();
    assert_eq!(c.predict(VlmRequest::default()).await.unwrap(), "ok");
    assert_eq!(hits.load(Ordering::Relaxed), 2);
}

#[tokio::test]
async fn chat_retry_budget_is_shared_by_transport_and_finish_reason_retries() {
    let hits = Arc::new(AtomicUsize::new(0));
    let app = Router::new().route(
        "/v1/chat/completions",
        post({
            let hits = hits.clone();
            move || {
                let hits = hits.clone();
                async move {
                    match hits.fetch_add(1, Ordering::Relaxed) {
                        0 => (
                            StatusCode::TOO_MANY_REQUESTS,
                            [("retry-after", "0")],
                            "busy",
                        )
                            .into_response(),
                        1 => Json(json!({"choices":[{"finish_reason":"content_filter","message":{"content":""}}]})).into_response(),
                        _ => completion("unexpected third request").into_response(),
                    }
                }
            }
        }),
    );
    let c = VlmHttpClient::connect(VlmHttpConfig {
        server_url: Some(serve(app).await.parse().unwrap()),
        model_name: Some("x".into()),
        skip_model_name_checking: true,
        max_retries: 1,
        retry_backoff_factor: 0.0,
        ..Default::default()
    })
    .await
    .unwrap();
    assert!(matches!(
        c.predict(VlmRequest::default()).await,
        Err(VlmError::Protocol { operation: "chat", message }) if message == "unexpected finish reason"
    ));
    assert_eq!(hits.load(Ordering::Relaxed), 2);
}

#[tokio::test]
async fn unexpected_chat_finish_reason_retry_is_bounded() {
    let hits = Arc::new(AtomicUsize::new(0));
    let app = Router::new().route(
        "/v1/chat/completions",
        post({
            let hits = hits.clone();
            move || {
                let hits = hits.clone();
                async move {
                    hits.fetch_add(1, Ordering::Relaxed);
                    Json(json!({"choices":[{"finish_reason":"content_filter","message":{"content":""}}]}))
                }
            }
        }),
    );
    let c = VlmHttpClient::connect(VlmHttpConfig {
        server_url: Some(serve(app).await.parse().unwrap()),
        model_name: Some("x".into()),
        skip_model_name_checking: true,
        max_retries: 1,
        retry_backoff_factor: 0.0,
        ..Default::default()
    })
    .await
    .unwrap();
    assert!(matches!(
        c.predict(VlmRequest::default()).await,
        Err(VlmError::Protocol { operation: "chat", message }) if message == "unexpected finish reason"
    ));
    assert_eq!(hits.load(Ordering::Relaxed), 2);
}

fn sse_response(chunks: Vec<&'static [u8]>) -> Response {
    Response::builder()
        .header("content-type", "text/event-stream")
        .body(Body::from_stream(futures_util::stream::iter(
            chunks
                .into_iter()
                .map(|chunk| Ok::<_, std::convert::Infallible>(Bytes::from_static(chunk))),
        )))
        .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn blocking_sse_requires_terminal_done_and_parses_wire_faithfully() {
    let good = Router::new().route(
        "/v1/chat/completions",
        post(|| async {
            sse_response(vec![
            b"data: {\"choices\":[{\"delta\":{\"content\":\"\xc3",
            b"\xa9\"},\"finish_reason\":null}]}\r\n\r\n",
            b"data: {\"choices\":[{\"delta\":{}\r\ndata: ,\"finish_reason\":\"stop\"}]}\r\n\r\n",
            b"data: [DONE]\r\n\r\n",
        ])
        }),
    );
    let c = client(serve(good).await).await;
    let mut stream = c.stream_predict(VlmRequest::default()).unwrap();
    assert_eq!(stream.next().unwrap().unwrap(), "é");
    assert!(stream.next().is_none());

    for wire in [
        b"data: {\"choices\":[{\"delta\":{},\"finish_reason\":null}]}\n\ndata: [DONE]\n\n"
            .as_slice(),
        b"data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: x\n\n"
            .as_slice(),
        b"data: {\"object\":\"error\",\"choices\":[{\"delta\":{\"content\":\"forged\"},\"finish_reason\":\"stop\"}]}\n\n"
            .as_slice(),
    ] {
        let app = Router::new().route(
            "/v1/chat/completions",
            post(move || async move { sse_response(vec![wire]) }),
        );
        let mut s = client(serve(app).await)
            .await
            .stream_predict(VlmRequest::default())
            .unwrap();
        assert!(matches!(s.next().unwrap(), Err(VlmError::Protocol { .. })));
    }
}

#[tokio::test]
async fn batches_preserve_order_and_fail_fast() {
    let hits = Arc::new(AtomicUsize::new(0));
    let app = Router::new().route(
        "/v1/chat/completions",
        post({
            let hits = hits.clone();
            move |Json(v): Json<Value>| {
                let hits = hits.clone();
                async move {
                    hits.fetch_add(1, Ordering::Relaxed);
                    let text = v["messages"][1]["content"]
                        .as_array()
                        .unwrap()
                        .last()
                        .unwrap()["text"]
                        .as_str()
                        .unwrap();
                    if text == "bad" {
                        (StatusCode::BAD_REQUEST, "bad").into_response()
                    } else {
                        completion(text).into_response()
                    }
                }
            }
        }),
    );
    let c = VlmHttpClient::connect(VlmHttpConfig {
        server_url: Some(serve(app).await.parse().unwrap()),
        model_name: Some("x".into()),
        skip_model_name_checking: true,
        max_concurrency: 1,
        max_retries: 0,
        ..Default::default()
    })
    .await
    .unwrap();
    let requests = |texts: &[&str]| {
        texts
            .iter()
            .map(|x| VlmRequest {
                prompt: Some((*x).into()),
                ..Default::default()
            })
            .collect()
    };
    assert_eq!(
        c.batch_predict(requests(&["one", "two"])).await.unwrap(),
        ["one", "two"]
    );
    let mut completed = c
        .aio_batch_predict_as_iter(requests(&["three", "four"]), None)
        .await
        .unwrap();
    assert_eq!(completed.next().await.unwrap().unwrap().0, 0);
    assert_eq!(completed.next().await.unwrap().unwrap().0, 1);
    let mut failed = c
        .aio_batch_predict_as_iter(requests(&["bad", "later"]), None)
        .await
        .unwrap();
    assert!(failed.next().await.unwrap().is_err());
    assert!(failed.next().await.is_none());
    assert_eq!(c.batch_predict(vec![]).await.unwrap(), Vec::<String>::new());
}

#[tokio::test]
async fn image_admission_limits_mime_and_remote_policy_precede_transport() {
    let c = VlmHttpClient::connect(VlmHttpConfig {
        server_url: Some("http://127.0.0.1:9".parse().unwrap()),
        model_name: Some("x".into()),
        skip_model_name_checking: true,
        max_image_bytes: 3,
        ..Default::default()
    })
    .await
    .unwrap();
    for image in [
        VlmImageInput::Base64 {
            data: "QUJDRA==".into(),
            media_type: None,
        },
        VlmImageInput::DataUrl("data:image/png;base64,QUJDRA==".into()),
    ] {
        assert!(matches!(
            c.predict(VlmRequest {
                images: vec![image],
                ..Default::default()
            })
            .await,
            Err(VlmError::LimitExceeded { .. })
        ));
    }
    let path = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(path.path(), b"four").unwrap();
    assert!(matches!(
        c.predict(VlmRequest {
            images: vec![VlmImageInput::Path(path.path().into())],
            ..Default::default()
        })
        .await,
        Err(VlmError::LimitExceeded { .. })
    ));
    let png = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVQIHWP4z8DwHwAFgAI/ScL9VQAAAABJRU5ErkJggg==";
    let normal = client("http://127.0.0.1:9".into()).await;
    assert!(matches!(
        normal
            .predict(VlmRequest {
                images: vec![VlmImageInput::Base64 {
                    data: png.into(),
                    media_type: Some("image/jpeg".into())
                }],
                ..Default::default()
            })
            .await,
        Err(VlmError::InvalidImageInput(_))
    ));
    for url in ["http://127.0.0.1/x", "http://192.0.2.1/x"] {
        assert!(matches!(
            normal
                .predict(VlmRequest {
                    images: vec![VlmImageInput::RemoteUrl(url.parse().unwrap())],
                    ..Default::default()
                })
                .await,
            Err(VlmError::InvalidImageInput(_))
        ));
    }
}

#[tokio::test]
async fn remote_images_allow_private_cap_bytes_and_revalidate_redirects() {
    const PNG: &[u8] = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR\0\0\0\x01\0\0\0\x01\x08\x02\0\0\0\x90wS\xde\0\0\0\x0cIDAT\x08\x99c\xf8\xcf\xc0\0\0\x03\x01\x01\0\x18\xdd\x8d\xb1\0\0\0\0IEND\xaeB`\x82";
    let image_hits = Arc::new(AtomicUsize::new(0));
    let app = Router::new()
        .route("/v1/chat/completions", post(|| async { completion("ok") }))
        .route(
            "/image",
            get({
                let image_hits = image_hits.clone();
                move || {
                    let image_hits = image_hits.clone();
                    async move {
                        image_hits.fetch_add(1, Ordering::Relaxed);
                        ([("content-type", "image/png")], PNG).into_response()
                    }
                }
            }),
        )
        .route(
            "/redirect",
            get(|| async { (StatusCode::FOUND, [("location", "/image")]) }),
        )
        .route("/large", get(|| async { vec![0_u8; 128] }));
    let root = serve(app).await;
    let make = |max_image_bytes| VlmHttpConfig {
        server_url: Some(root.parse().unwrap()),
        model_name: Some("x".into()),
        skip_model_name_checking: true,
        allow_remote_images: true,
        allow_private_remote_images: true,
        max_image_bytes,
        max_retries: 0,
        ..Default::default()
    };
    let c = VlmHttpClient::connect(make(1024)).await.unwrap();
    c.predict(VlmRequest {
        images: vec![VlmImageInput::RemoteUrl(
            format!("{root}/redirect").parse().unwrap(),
        )],
        ..Default::default()
    })
    .await
    .unwrap();
    assert_eq!(image_hits.load(Ordering::Relaxed), 1);
    let capped = VlmHttpClient::connect(make(3)).await.unwrap();
    assert!(matches!(
        capped
            .predict(VlmRequest {
                images: vec![VlmImageInput::RemoteUrl(
                    format!("{root}/large").parse().unwrap()
                )],
                ..Default::default()
            })
            .await,
        Err(VlmError::LimitExceeded {
            resource: "image bytes",
            ..
        })
    ));
    let no_hops = VlmHttpClient::connect(VlmHttpConfig {
        max_redirects: 0,
        ..make(1024)
    })
    .await
    .unwrap();
    assert!(matches!(
        no_hops
            .predict(VlmRequest {
                images: vec![VlmImageInput::RemoteUrl(
                    format!("{root}/redirect").parse().unwrap()
                )],
                ..Default::default()
            })
            .await,
        Err(VlmError::Redirect(_))
    ));
}

#[tokio::test]
async fn batch_iterator_is_as_completed_bounded_and_fail_fast() {
    let active = Arc::new(AtomicUsize::new(0));
    let maximum = Arc::new(AtomicUsize::new(0));
    let app = Router::new().route(
        "/v1/chat/completions",
        post({
            let active = active.clone();
            let maximum = maximum.clone();
            move |Json(v): Json<Value>| {
                let active = active.clone();
                let maximum = maximum.clone();
                async move {
                    let now = active.fetch_add(1, Ordering::SeqCst) + 1;
                    maximum.fetch_max(now, Ordering::SeqCst);
                    let text = v["messages"][1]["content"]
                        .as_array()
                        .unwrap()
                        .last()
                        .unwrap()["text"]
                        .as_str()
                        .unwrap()
                        .to_owned();
                    if text == "slow" {
                        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                    }
                    active.fetch_sub(1, Ordering::SeqCst);
                    if text == "bad" {
                        (StatusCode::BAD_REQUEST, "bad").into_response()
                    } else {
                        completion(text).into_response()
                    }
                }
            }
        }),
    );
    let c = VlmHttpClient::connect(VlmHttpConfig {
        server_url: Some(serve(app).await.parse().unwrap()),
        model_name: Some("x".into()),
        skip_model_name_checking: true,
        max_concurrency: 2,
        max_retries: 0,
        ..Default::default()
    })
    .await
    .unwrap();
    let request = |prompt: &str| VlmRequest {
        prompt: Some(prompt.into()),
        ..Default::default()
    };
    let mut stream = c
        .aio_batch_predict_as_iter(
            vec![request("slow"), request("fast"), request("third")],
            None,
        )
        .await
        .unwrap();
    assert_eq!(stream.next().await.unwrap().unwrap().0, 1);
    assert_eq!(stream.next().await.unwrap().unwrap().0, 2);
    assert_eq!(stream.next().await.unwrap().unwrap().0, 0);
    assert!(maximum.load(Ordering::SeqCst) <= 2);
    let mut failed = c
        .aio_batch_predict_as_iter(vec![request("bad"), request("later")], None)
        .await
        .unwrap();
    match failed.next().await.unwrap() {
        Err(_) => {}
        Ok((1, text)) => {
            assert_eq!(text, "later");
            assert!(failed.next().await.unwrap().is_err());
        }
        Ok((index, _)) => panic!("unexpected successful index {index}"),
    }
    assert!(failed.next().await.is_none());
}
