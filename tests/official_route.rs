use axum::{Json, Router, extract::State as AxumState, http::StatusCode, routing::post};
use bytes::Bytes;
use lopdf::{Document, Object, Stream, dictionary};
use mineru::{
    MinerUVlmClient, MinerUVlmConfig, OfficialPdfOptions, PdfInput, ProgressCallback,
    ProgressEvent, VlmHttpConfig,
    input_prepare::{DocumentKind, PreparedPdf},
};
use serde_json::{Value, json};
use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::sync::{Barrier, Notify};

fn tiny_pdf(pages: usize) -> Bytes {
    let mut pdf = Document::with_version("1.5");
    let page_tree = pdf.new_object_id();
    let page_ids: Vec<_> = (0..pages).map(|_| pdf.new_object_id()).collect();
    for page in &page_ids {
        let contents = pdf.add_object(Stream::new(dictionary! {}, Vec::new()));
        pdf.objects.insert(
            *page,
            Object::Dictionary(dictionary! {
                "Type" => "Page", "Parent" => page_tree,
                "MediaBox" => vec![0.into(), 0.into(), 1.into(), 1.into()],
                "Contents" => contents,
            }),
        );
    }
    pdf.objects.insert(page_tree, Object::Dictionary(dictionary! {
        "Type" => "Pages", "Kids" => page_ids.into_iter().map(Object::Reference).collect::<Vec<_>>(), "Count" => pages as i64,
    }));
    let catalog = pdf.add_object(dictionary! { "Type" => "Catalog", "Pages" => page_tree });
    pdf.trailer.set("Root", catalog);
    let mut bytes = Vec::new();
    pdf.save_to(&mut bytes).unwrap();
    Bytes::from(bytes)
}

async fn configured_client(app: Router, max_concurrency: usize) -> MinerUVlmClient {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    MinerUVlmClient::connect(
        VlmHttpConfig {
            server_url: Some(format!("http://{address}").parse().unwrap()),
            model_name: Some("mock".into()),
            skip_model_name_checking: true,
            max_retries: 0,
            max_concurrency,
            ..Default::default()
        },
        MinerUVlmConfig {
            layout_image_size: (8, 8),
            ..Default::default()
        },
    )
    .await
    .unwrap()
}

fn route_options(window: usize) -> OfficialPdfOptions {
    OfficialPdfOptions {
        processing_window_size: window,
        max_in_flight_image_bytes: 1024 * 1024,
        max_rendered_image_bytes: 1024 * 1024,
        max_raw_output_bytes: 1024 * 1024,
        max_encoded_document_bytes: 1024 * 1024,
        max_encoded_request_bytes: 1024 * 1024,
        max_encoded_batch_bytes: 1024 * 1024,
        ..Default::default()
    }
}

fn empty_completion() -> Json<Value> {
    Json(json!({"choices":[{"finish_reason":"stop","message":{"content":""}}]}))
}

async fn client() -> MinerUVlmClient {
    let app = Router::new().route(
        "/v1/chat/completions",
        post(|| async {
            Json(json!({"choices":[{"finish_reason":"stop","message":{"content":""}}]}))
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    MinerUVlmClient::connect(
        VlmHttpConfig {
            server_url: Some(format!("http://{address}").parse().unwrap()),
            model_name: Some("mock".into()),
            skip_model_name_checking: true,
            max_retries: 0,
            ..Default::default()
        },
        MinerUVlmConfig::default(),
    )
    .await
    .unwrap()
}

async fn client_with_reply(reply: String) -> MinerUVlmClient {
    let app = Router::new().route(
        "/v1/chat/completions",
        post(move || {
            let reply = reply.clone();
            async move {
                Json(json!({"choices":[{"finish_reason":"stop","message":{"content":reply}}]}))
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    MinerUVlmClient::connect(
        VlmHttpConfig {
            server_url: Some(format!("http://{address}").parse().unwrap()),
            model_name: Some("mock".into()),
            skip_model_name_checking: true,
            max_retries: 0,
            ..Default::default()
        },
        MinerUVlmConfig::default(),
    )
    .await
    .unwrap()
}

fn malformed_local_contents() -> Bytes {
    let mut document = Document::load_mem(include_bytes!("fixtures/pdf/minimal.pdf")).unwrap();
    let page = *document.get_pages().get(&1).unwrap();
    document
        .get_object_mut(page)
        .unwrap()
        .as_dict_mut()
        .unwrap()
        .set("Contents", Object::Integer(1));
    let mut bytes = Vec::new();
    document.save_to(&mut bytes).unwrap();
    Bytes::from(bytes)
}

#[tokio::test]
async fn official_route_stages_and_publishes_complete_artifacts() {
    let output = tempfile::tempdir().unwrap();
    let manifest = client()
        .await
        .parse_and_write_official_pdf(
            PdfInput::Path("tests/fixtures/pdf/minimal.pdf".into()),
            OfficialPdfOptions::default(),
            output.path(),
            "minimal",
        )
        .await
        .unwrap();

    assert_eq!(manifest.vlm_dir, output.path().join("minimal/vlm"));
    for name in [
        "minimal.md",
        "minimal_middle.json",
        "minimal_model.json",
        "minimal_content_list.json",
        "minimal_content_list_v2.json",
        "minimal_layout.pdf",
    ] {
        assert!(manifest.vlm_dir.join(name).is_file(), "{name}");
    }
    assert!(!manifest.vlm_dir.join("parts").exists());
    assert!(!manifest.vlm_dir.join("minimal_origin.pdf").exists());
    let middle: Value = serde_json::from_slice(
        &std::fs::read(manifest.vlm_dir.join("minimal_middle.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(middle["pdf_info"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn official_office_route_publishes_only_the_closed_office_target() {
    let output = tempfile::tempdir().unwrap();
    let manifest = client()
        .await
        .parse_and_write_official_office_pdf(
            PdfInput::Path("tests/fixtures/pdf/minimal.pdf".into()),
            OfficialPdfOptions::default(),
            output.path(),
            "minimal",
        )
        .await
        .unwrap();

    assert_eq!(manifest.vlm_dir, output.path().join("minimal/office"));
    for name in [
        "minimal.md",
        "minimal_middle.json",
        "minimal_model.json",
        "minimal_content_list.json",
        "minimal_content_list_v2.json",
        "minimal_layout.pdf",
    ] {
        assert!(manifest.vlm_dir.join(name).is_file(), "{name}");
    }
    assert!(!output.path().join("minimal/vlm").exists());
    assert!(!manifest.vlm_dir.join("minimal_origin.pdf").exists());
}

#[tokio::test]
#[ignore = "full PDF route/output integration e2e"]
async fn prepared_routes_publish_exact_closed_origins_and_normalize_ranges() {
    let output = tempfile::tempdir().unwrap();
    let client = client().await;
    let pdf = tiny_pdf(2);
    let manifest = client
        .parse_and_write_prepared_pdf(
            PreparedPdf {
                bytes: pdf.clone(),
                kind: DocumentKind::Pdf,
                original: pdf.clone(),
            },
            OfficialPdfOptions {
                start_page: 1,
                end_page: Some(1),
                ..route_options(1)
            },
            output.path(),
            "pdf",
        )
        .await
        .unwrap();
    assert_eq!(
        std::fs::read(manifest.vlm_dir.join("pdf_origin.pdf")).unwrap(),
        pdf
    );
    let middle: Value =
        serde_json::from_slice(&std::fs::read(manifest.vlm_dir.join("pdf_middle.json")).unwrap())
            .unwrap();
    assert_eq!(middle["pdf_info"][0]["page_idx"], 1);

    for (kind, suffix, root) in [
        (DocumentKind::Png, "png", "vlm"),
        (DocumentKind::Docx, "docx", "office"),
    ] {
        let original = Bytes::from_static(b"exact origin");
        let manifest = client
            .parse_and_write_prepared_pdf(
                PreparedPdf {
                    bytes: tiny_pdf(1),
                    kind,
                    original: original.clone(),
                },
                OfficialPdfOptions {
                    start_page: 2,
                    end_page: Some(1),
                    ..route_options(1)
                },
                output.path(),
                kind.suffix(),
            )
            .await
            .unwrap();
        assert_eq!(
            manifest.vlm_dir,
            output.path().join(format!("{}/{root}", kind.suffix()))
        );
        assert_eq!(
            std::fs::read(
                manifest
                    .vlm_dir
                    .join(format!("{}_origin.{suffix}", kind.suffix()))
            )
            .unwrap(),
            original
        );
    }
}

#[tokio::test]
#[ignore = "full PDF route/output integration e2e"]
async fn prepared_route_page_events_follow_successful_staging_and_ignore_panics() {
    let output = tempfile::tempdir().unwrap();
    let events = Arc::new(std::sync::Mutex::new(Vec::new()));
    let callback: ProgressCallback = {
        let events = events.clone();
        Arc::new(move |event| events.lock().unwrap().push(event))
    };
    let pdf = tiny_pdf(3);
    client()
        .await
        .parse_and_write_prepared_pdf_with_events(
            PreparedPdf {
                bytes: pdf.clone(),
                kind: DocumentKind::Pdf,
                original: pdf,
            },
            OfficialPdfOptions {
                start_page: 1,
                end_page: Some(2),
                ..route_options(1)
            },
            output.path(),
            "event-doc",
            Some(callback),
        )
        .await
        .unwrap();
    assert_eq!(
        *events.lock().unwrap(),
        vec![
            ProgressEvent::DocumentPageCompleted {
                document: "event-doc".into(),
                page_index: 1,
                completed: 1,
                total: 2
            },
            ProgressEvent::DocumentPageCompleted {
                document: "event-doc".into(),
                page_index: 2,
                completed: 2,
                total: 2
            },
        ]
    );
    let pdf = tiny_pdf(1);
    client()
        .await
        .parse_and_write_prepared_pdf_with_events(
            PreparedPdf {
                bytes: pdf.clone(),
                kind: DocumentKind::Pdf,
                original: pdf,
            },
            route_options(1),
            output.path(),
            "panic",
            Some(Arc::new(|_| panic!("event"))),
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn failed_office_route_preserves_existing_office_output_without_leaks() {
    let output = tempfile::tempdir().unwrap();
    let existing = output.path().join("minimal/office");
    std::fs::create_dir_all(&existing).unwrap();
    std::fs::write(existing.join("preserved"), b"old").unwrap();

    client()
        .await
        .parse_and_write_official_office_pdf(
            PdfInput::Bytes(malformed_local_contents()),
            OfficialPdfOptions::default(),
            output.path(),
            "minimal",
        )
        .await
        .unwrap_err();

    assert_eq!(std::fs::read(existing.join("preserved")).unwrap(), b"old");
    assert!(
        !std::fs::read_dir(output.path().join("minimal"))
            .unwrap()
            .any(|entry| entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".vlm-"))
    );
}

#[tokio::test]
#[ignore = "full PDF route/output integration e2e"]
async fn closed_targets_allow_stem_collisions_without_cross_publication() {
    assert_eq!(mineru::canonical_stem("../office").unwrap(), "___office");
    let output = tempfile::tempdir().unwrap();
    let client = client().await;
    client
        .parse_and_write_official_pdf(
            PdfInput::Path("tests/fixtures/pdf/minimal.pdf".into()),
            OfficialPdfOptions::default(),
            output.path(),
            "same",
        )
        .await
        .unwrap();
    client
        .parse_and_write_official_office_pdf(
            PdfInput::Path("tests/fixtures/pdf/minimal.pdf".into()),
            OfficialPdfOptions::default(),
            output.path(),
            "same",
        )
        .await
        .unwrap();

    assert!(output.path().join("same/vlm/same.md").is_file());
    assert!(output.path().join("same/office/same.md").is_file());
}

#[tokio::test]
async fn staged_text_limit_cleans_failed_product_output() {
    let output = tempfile::tempdir().unwrap();
    let error = client()
        .await
        .parse_and_write_official_pdf(
            PdfInput::Path("tests/fixtures/pdf/minimal.pdf".into()),
            OfficialPdfOptions {
                max_staged_text_bytes: 1,
                ..Default::default()
            },
            output.path(),
            "minimal",
        )
        .await
        .unwrap_err();
    assert!(error.to_string().contains("staged text/JSON bytes"));
    assert!(!output.path().join("minimal/vlm").exists());
    assert!(
        !std::fs::read_dir(output.path().join("minimal"))
            .unwrap()
            .any(|entry| entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".vlm-staging-parent-"))
    );
}

#[tokio::test]
async fn image_limit_after_stage_preserves_existing_target_and_cleans() {
    let output = tempfile::tempdir().unwrap();
    let existing = output.path().join("minimal/vlm");
    std::fs::create_dir_all(&existing).unwrap();
    std::fs::write(existing.join("preserved"), b"old").unwrap();
    let error = client()
        .await
        .parse_and_write_official_pdf(
            PdfInput::Path("tests/fixtures/pdf/minimal.pdf".into()),
            OfficialPdfOptions {
                max_in_flight_image_bytes: 1,
                ..Default::default()
            },
            output.path(),
            "minimal",
        )
        .await
        .unwrap_err();
    assert!(matches!(error, mineru::VlmError::LimitExceeded { .. }));
    assert_eq!(std::fs::read(existing.join("preserved")).unwrap(), b"old");
    assert!(
        !std::fs::read_dir(output.path().join("minimal"))
            .unwrap()
            .any(|entry| entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".vlm-staging-parent-"))
    );
}

#[tokio::test]
async fn malformed_preview_contents_rolls_back_and_preserves_existing_output() {
    let output = tempfile::tempdir().unwrap();
    let existing = output.path().join("minimal/vlm");
    std::fs::create_dir_all(&existing).unwrap();
    std::fs::write(existing.join("preserved"), b"old").unwrap();

    let error = client()
        .await
        .parse_and_write_official_pdf(
            PdfInput::Bytes(malformed_local_contents()),
            OfficialPdfOptions::default(),
            output.path(),
            "minimal",
        )
        .await
        .unwrap_err();

    assert!(matches!(error, mineru::VlmError::Pdf(_)));
    assert_eq!(std::fs::read(existing.join("preserved")).unwrap(), b"old");
    assert!(
        !std::fs::read_dir(output.path().join("minimal"))
            .unwrap()
            .any(|entry| entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".vlm-staging-parent-"))
    );
}

#[tokio::test]
async fn raw_reply_allowance_caps_the_http_body_before_accepting_it() {
    let output = tempfile::tempdir().unwrap();
    let error = client_with_reply("x".repeat(16 * 1024))
        .await
        .parse_and_write_official_pdf(
            PdfInput::Path("tests/fixtures/pdf/minimal.pdf".into()),
            OfficialPdfOptions {
                max_raw_output_bytes: 64,
                ..Default::default()
            },
            output.path(),
            "minimal",
        )
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        mineru::VlmError::LimitExceeded {
            resource: "response",
            limit: 64,
            ..
        }
    ));
}

#[tokio::test]
async fn raw_reply_allowance_counts_ignored_json_fields() {
    let app = Router::new().route(
        "/v1/chat/completions",
        post(|| async {
            Json(json!({"choices":[{"finish_reason":"stop","message":{"content":""}}], "ignored":"x".repeat(16 * 1024)}))
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let client = MinerUVlmClient::connect(
        VlmHttpConfig {
            server_url: Some(format!("http://{address}").parse().unwrap()),
            model_name: Some("mock".into()),
            skip_model_name_checking: true,
            max_retries: 0,
            ..Default::default()
        },
        MinerUVlmConfig::default(),
    )
    .await
    .unwrap();
    let error = client
        .parse_and_write_official_pdf(
            PdfInput::Path("tests/fixtures/pdf/minimal.pdf".into()),
            OfficialPdfOptions {
                max_raw_output_bytes: 128,
                ..Default::default()
            },
            tempfile::tempdir().unwrap().path(),
            "minimal",
        )
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        mineru::VlmError::LimitExceeded {
            resource: "response",
            limit: 128,
            ..
        }
    ));
}

#[tokio::test]
#[ignore = "130-page scheduler stress test"]
async fn default_window_130_does_not_roll_into_the_next_operation() {
    tokio::time::timeout(Duration::from_secs(20), async {
        #[derive(Clone)]
        struct State {
            requests: Arc<AtomicUsize>,
            first: Arc<Barrier>,
            second: Arc<Barrier>,
            release_64: Arc<Notify>,
            release_128: Arc<Notify>,
        }
        async fn handler(AxumState(state): AxumState<State>) -> Json<Value> {
            let request = state.requests.fetch_add(1, Ordering::SeqCst) + 1;
            match request {
                1..=64 => {
                    state.first.wait().await;
                    if request == 64 {
                        state.release_64.notified().await;
                    }
                }
                65..=128 => {
                    state.second.wait().await;
                    if request == 128 {
                        state.release_128.notified().await;
                    }
                }
                _ => {}
            }
            empty_completion()
        }

        let state = State {
            requests: Arc::new(AtomicUsize::new(0)),
            first: Arc::new(Barrier::new(65)),
            second: Arc::new(Barrier::new(65)),
            release_64: Arc::new(Notify::new()),
            release_128: Arc::new(Notify::new()),
        };
        let client = configured_client(
            Router::new()
                .route("/v1/chat/completions", post(handler))
                .with_state(state.clone()),
            64,
        )
        .await;
        let output = tempfile::tempdir().unwrap();
        let output_root = output.path().to_path_buf();
        let route = tokio::spawn({
            let input = tiny_pdf(130);
            async move {
                client
                    .parse_and_write_official_pdf(
                        PdfInput::Bytes(input),
                        route_options(64),
                        &output_root,
                        "many",
                    )
                    .await
            }
        });

        state.first.wait().await;
        assert!(
            tokio::time::timeout(Duration::from_millis(100), async {
                while state.requests.load(Ordering::SeqCst) < 65 {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .is_err()
        );
        state.release_64.notify_one();
        state.second.wait().await;
        assert!(
            tokio::time::timeout(Duration::from_millis(100), async {
                while state.requests.load(Ordering::SeqCst) < 129 {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .is_err()
        );
        state.release_128.notify_one();

        let manifest = route.await.unwrap().unwrap();
        assert_eq!(state.requests.load(Ordering::SeqCst), 130);
        let middle: Value = serde_json::from_slice(
            &std::fs::read(manifest.vlm_dir.join("many_middle.json")).unwrap(),
        )
        .unwrap();
        let pages = middle["pdf_info"].as_array().unwrap();
        assert_eq!(pages.len(), 130);
        for (index, page) in pages.iter().enumerate() {
            assert_eq!(page["page_idx"], index);
        }
    })
    .await
    .expect("route test timed out");
}

#[tokio::test]
async fn selected_custom_and_rgb_limited_windows_preserve_boundaries() {
    #[derive(Clone)]
    struct State {
        requests: Arc<AtomicUsize>,
        first: Arc<Barrier>,
        release: Arc<Notify>,
    }
    async fn handler(AxumState(state): AxumState<State>) -> Json<Value> {
        let request = state.requests.fetch_add(1, Ordering::SeqCst) + 1;
        if request == 1 {
            state.first.wait().await;
            state.release.notified().await;
        }
        empty_completion()
    }

    let state = State {
        requests: Arc::new(AtomicUsize::new(0)),
        first: Arc::new(Barrier::new(2)),
        release: Arc::new(Notify::new()),
    };
    let client = configured_client(
        Router::new()
            .route("/v1/chat/completions", post(handler))
            .with_state(state.clone()),
        8,
    )
    .await;
    let output = tempfile::tempdir().unwrap();
    let output_root = output.path().to_path_buf();
    let route = tokio::spawn({
        let input = tiny_pdf(6);
        async move {
            let mut options = route_options(3);
            options.start_page = 1;
            options.end_page = Some(4);
            // At the route's 200dpi scale, a 1pt page is 3x3 RGB pixels (27 bytes).
            options.max_in_flight_image_bytes = 27;
            client
                .parse_and_write_official_pdf(
                    PdfInput::Bytes(input),
                    options,
                    &output_root,
                    "selected",
                )
                .await
        }
    });
    state.first.wait().await;
    assert!(
        tokio::time::timeout(Duration::from_millis(100), async {
            while state.requests.load(Ordering::SeqCst) < 2 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .is_err()
    );
    state.release.notify_one();
    let manifest = tokio::time::timeout(Duration::from_secs(10), route)
        .await
        .expect("route timed out")
        .unwrap()
        .unwrap();
    assert_eq!(state.requests.load(Ordering::SeqCst), 4);
    let middle: Value = serde_json::from_slice(
        &std::fs::read(manifest.vlm_dir.join("selected_middle.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(
        middle["pdf_info"]
            .as_array()
            .unwrap()
            .iter()
            .map(|page| page["page_idx"].as_u64().unwrap())
            .collect::<Vec<_>>(),
        vec![1, 2, 3, 4]
    );
}

#[tokio::test]
async fn size_one_windows_do_not_overlap() {
    #[derive(Clone)]
    struct State {
        requests: Arc<AtomicUsize>,
        entered: Arc<Barrier>,
        release: Arc<Notify>,
    }
    async fn handler(AxumState(state): AxumState<State>) -> Json<Value> {
        let request = state.requests.fetch_add(1, Ordering::SeqCst) + 1;
        if request == 1 {
            state.entered.wait().await;
            state.release.notified().await;
        }
        empty_completion()
    }

    let state = State {
        requests: Arc::new(AtomicUsize::new(0)),
        entered: Arc::new(Barrier::new(2)),
        release: Arc::new(Notify::new()),
    };
    let client = configured_client(
        Router::new()
            .route("/v1/chat/completions", post(handler))
            .with_state(state.clone()),
        8,
    )
    .await;
    let output = tempfile::tempdir().unwrap();
    let output_root = output.path().to_path_buf();
    let route = tokio::spawn({
        let input = tiny_pdf(2);
        async move {
            client
                .parse_and_write_official_pdf(
                    PdfInput::Bytes(input),
                    route_options(1),
                    &output_root,
                    "one",
                )
                .await
        }
    });
    state.entered.wait().await;
    assert!(
        tokio::time::timeout(Duration::from_millis(100), async {
            while state.requests.load(Ordering::SeqCst) < 2 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .is_err()
    );
    state.release.notify_one();
    assert!(
        tokio::time::timeout(Duration::from_secs(10), route)
            .await
            .expect("route timed out")
            .unwrap()
            .is_ok()
    );
    assert_eq!(state.requests.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn window_failure_rolls_back_before_the_next_window() {
    #[derive(Clone)]
    struct State {
        requests: Arc<AtomicUsize>,
        second: Arc<Barrier>,
        fifth: Arc<Notify>,
    }
    async fn handler(AxumState(state): AxumState<State>) -> Result<Json<Value>, StatusCode> {
        let request = state.requests.fetch_add(1, Ordering::SeqCst) + 1;
        if request == 5 {
            state.fifth.notify_one();
        }
        if (3..=4).contains(&request) {
            state.second.wait().await;
            return Err(StatusCode::INTERNAL_SERVER_ERROR);
        }
        Ok(empty_completion())
    }

    let state = State {
        requests: Arc::new(AtomicUsize::new(0)),
        second: Arc::new(Barrier::new(3)),
        fifth: Arc::new(Notify::new()),
    };
    let client = configured_client(
        Router::new()
            .route("/v1/chat/completions", post(handler))
            .with_state(state.clone()),
        2,
    )
    .await;
    let output = tempfile::tempdir().unwrap();
    let output_root = output.path().to_path_buf();
    let existing = output.path().join("five/vlm");
    std::fs::create_dir_all(&existing).unwrap();
    std::fs::write(existing.join("preserved"), b"old").unwrap();
    let route = tokio::spawn({
        let input = tiny_pdf(5);
        async move {
            client
                .parse_and_write_official_pdf(
                    PdfInput::Bytes(input),
                    route_options(2),
                    &output_root,
                    "five",
                )
                .await
        }
    });
    state.second.wait().await;
    let error = tokio::time::timeout(Duration::from_secs(10), route)
        .await
        .expect("route timed out")
        .unwrap()
        .unwrap_err();
    assert!(matches!(error, mineru::VlmError::Http { .. }));
    assert!(
        tokio::time::timeout(Duration::from_millis(100), state.fifth.notified())
            .await
            .is_err()
    );
    assert_eq!(state.requests.load(Ordering::SeqCst), 4);
    assert_eq!(std::fs::read(existing.join("preserved")).unwrap(), b"old");
    assert_eq!(std::fs::read_dir(&existing).unwrap().count(), 1);
    assert!(
        !std::fs::read_dir(output.path().join("five"))
            .unwrap()
            .any(|entry| entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".vlm-staging-parent-"))
    );
}
