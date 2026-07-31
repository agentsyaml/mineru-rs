use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, State as AxumState},
    http::StatusCode,
    routing::post,
};
use bytes::Bytes;
use lopdf::{Dictionary, Document, Object, ObjectId, Stream, dictionary};
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
use tokio::sync::{Barrier, Notify, oneshot};

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

async fn production_client() -> (
    MinerUVlmClient,
    oneshot::Sender<()>,
    tokio::task::JoinHandle<()>,
) {
    let app = Router::new()
        .route(
            "/v1/chat/completions",
            post(|_: Bytes| async { empty_completion() }),
        )
        .layer(DefaultBodyLimit::disable());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (stop, stopped) = oneshot::channel();
    let server = tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(async {
                let _ = stopped.await;
            })
            .await
            .unwrap();
    });
    let client = MinerUVlmClient::connect(
        VlmHttpConfig {
            server_url: Some(format!("http://{address}").parse().unwrap()),
            model_name: Some("mock".into()),
            api_key: None,
            skip_model_name_checking: true,
            max_retries: 0,
            max_concurrency: 8,
            ..Default::default()
        },
        MinerUVlmConfig::default(),
    )
    .await
    .unwrap();
    (client, stop, server)
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
#[ignore = "requires retained MINERU_OFFICIAL_ROUTE_BENCH_PDF and MINERU_OFFICIAL_ROUTE_BENCH_OUT paths"]
async fn real_pdf_selected_200_produces_official_preview() {
    #[derive(Default)]
    struct Features {
        type0_font: bool,
        transparency_group: bool,
    }

    fn resolve<'a>(doc: &'a Document, mut object: &'a Object) -> Result<&'a Object, String> {
        let mut seen = std::collections::HashSet::new();
        while let Object::Reference(id) = object {
            if !seen.insert(*id) {
                return Err(format!("cyclic indirect object {} {} R", id.0, id.1));
            }
            object = doc
                .get_object(*id)
                .map_err(|error| format!("cannot resolve {} {} R: {error}", id.0, id.1))?;
        }
        Ok(object)
    }

    fn dictionary<'a>(
        doc: &'a Document,
        object: &'a Object,
        label: &str,
    ) -> Result<&'a Dictionary, String> {
        resolve(doc, object)?
            .as_dict()
            .map_err(|_| format!("{label} is not a dictionary"))
    }

    fn name_is(doc: &Document, object: &Object, expected: &[u8]) -> bool {
        resolve(doc, object)
            .ok()
            .and_then(|object| object.as_name().ok())
            == Some(expected)
    }

    fn valid_type0_font(doc: &Document, object: &Object) -> bool {
        let Ok(font) = dictionary(doc, object, "font") else {
            return false;
        };
        if !font
            .get(b"Subtype")
            .is_ok_and(|subtype| name_is(doc, subtype, b"Type0"))
            || !font.get(b"ToUnicode").is_ok_and(|to_unicode| {
                resolve(doc, to_unicode).is_ok_and(|object| matches!(object, Object::Stream(_)))
            })
        {
            return false;
        }
        let Ok(descendants) = font
            .get(b"DescendantFonts")
            .map_err(|_| ())
            .and_then(|object| resolve(doc, object).map_err(|_| ()))
            .and_then(|object| object.as_array().map_err(|_| ()))
        else {
            return false;
        };
        !descendants.is_empty()
            && descendants
                .iter()
                .all(|descendant| dictionary(doc, descendant, "descendant font").is_ok())
    }

    fn has_transparency_group(doc: &Document, dictionary: &Dictionary) -> bool {
        dictionary
            .get(b"Group")
            .ok()
            .and_then(|group| resolve(doc, group).ok())
            .and_then(|group| group.as_dict().ok())
            .and_then(|group| group.get(b"S").ok())
            .is_some_and(|subtype| name_is(doc, subtype, b"Transparency"))
    }

    fn scan_resources(
        doc: &Document,
        resources: &Object,
        visited: &mut std::collections::HashSet<ObjectId>,
        features: &mut Features,
    ) -> Result<(), String> {
        let resources = dictionary(doc, resources, "Resources")?;
        if let Ok(fonts) = resources.get(b"Font") {
            let fonts = dictionary(doc, fonts, "Font resources")?;
            features.type0_font |= fonts.iter().any(|(_, font)| valid_type0_font(doc, font));
        }
        let Ok(xobjects) = resources.get(b"XObject") else {
            return Ok(());
        };
        for (_, object) in dictionary(doc, xobjects, "XObject resources")? {
            let resolved = resolve(doc, object)?;
            let Object::Stream(form) = resolved else {
                continue;
            };
            if !form
                .dict
                .get(b"Subtype")
                .is_ok_and(|subtype| name_is(doc, subtype, b"Form"))
            {
                continue;
            }
            features.transparency_group |= has_transparency_group(doc, &form.dict);
            if let Object::Reference(id) = object
                && !visited.insert(*id)
            {
                continue;
            }
            if let Ok(resources) = form.dict.get(b"Resources") {
                scan_resources(doc, resources, visited, features)?;
            }
        }
        Ok(())
    }

    fn required_path(name: &str) -> std::path::PathBuf {
        std::env::var_os(name)
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| panic!("{name} is required for this ignored benchmark"))
    }

    let input = required_path("MINERU_OFFICIAL_ROUTE_BENCH_PDF");
    assert!(
        input.is_absolute() && input.is_file(),
        "MINERU_OFFICIAL_ROUTE_BENCH_PDF must be an absolute existing PDF file: {}",
        input.display()
    );
    let output = required_path("MINERU_OFFICIAL_ROUTE_BENCH_OUT");
    assert!(
        output.is_absolute() && output.is_dir(),
        "MINERU_OFFICIAL_ROUTE_BENCH_OUT must be an absolute existing retained directory: {}",
        output.display()
    );
    let window = match std::env::var("MINERU_OFFICIAL_ROUTE_BENCH_WINDOW") {
        Ok(value) => value.parse::<usize>().unwrap_or_else(|_| {
            panic!("MINERU_OFFICIAL_ROUTE_BENCH_WINDOW must be a positive integer: {value:?}")
        }),
        Err(std::env::VarError::NotPresent) => 64,
        Err(std::env::VarError::NotUnicode(_)) => {
            panic!("MINERU_OFFICIAL_ROUTE_BENCH_WINDOW must be a positive integer")
        }
    };
    assert!(
        window > 0,
        "MINERU_OFFICIAL_ROUTE_BENCH_WINDOW must be a positive integer"
    );

    let (client, stop, server) = production_client().await;
    let result = client
        .parse_and_write_official_pdf(
            PdfInput::Path(input),
            OfficialPdfOptions {
                start_page: 0,
                end_page: Some(199),
                processing_window_size: window,
                total_deadline: Duration::from_secs(60 * 60),
                ..Default::default()
            },
            &output,
            "phase2-real",
        )
        .await;
    let _ = stop.send(());
    server.await.expect("local VLM server task failed");
    let manifest = result.expect("official route failed");
    assert!(manifest.root.is_dir(), "returned manifest root is missing");
    assert_eq!(manifest.vlm_dir, output.join("phase2-real/vlm"));
    assert!(manifest.vlm_dir.is_dir(), "returned manifest is missing");
    let preview_path = output.join("phase2-real/vlm/phase2-real_layout.pdf");
    assert!(
        preview_path.is_file(),
        "durable layout PDF is missing: {}",
        preview_path.display()
    );

    let preview = Document::load(&preview_path).expect("final layout PDF is not a valid PDF");
    let pages = preview.get_pages();
    assert_eq!(pages.len(), 200, "final layout PDF page count");
    let mut features = Features::default();
    let mut visited = std::collections::HashSet::new();
    for (number, id) in pages {
        let page = preview
            .get_object(id)
            .expect("page object is missing")
            .as_dict()
            .expect("page object is not a dictionary");
        let resources = page
            .get(b"Resources")
            .unwrap_or_else(|_| panic!("page {number} has no Resources"));
        let resource_dict = dictionary(&preview, resources, "page Resources")
            .unwrap_or_else(|error| panic!("page {number}: {error}"));
        let fonts = resource_dict
            .get(b"Font")
            .ok()
            .and_then(|fonts| dictionary(&preview, fonts, "page Font resources").ok());
        assert!(
            fonts.is_some_and(|fonts| fonts.iter().any(|(name, font)| {
                name.starts_with(b"MinerUPreviewHelvetica")
                    && dictionary(&preview, font, "overlay font").is_ok_and(|font| {
                        font.get(b"BaseFont")
                            .is_ok_and(|base| name_is(&preview, base, b"Helvetica"))
                    })
            })),
            "page {number} is missing its MinerUPreviewHelvetica font resource"
        );
        let states = resource_dict
            .get(b"ExtGState")
            .ok()
            .and_then(|states| dictionary(&preview, states, "page ExtGState resources").ok());
        assert!(
            states.is_some_and(|states| states
                .iter()
                .any(|(name, _)| name.starts_with(b"MinerUPreviewAlpha"))),
            "page {number} is missing its MinerUPreviewAlpha resource"
        );
        features.transparency_group |= has_transparency_group(&preview, page);
        scan_resources(&preview, resources, &mut visited, &mut features)
            .unwrap_or_else(|error| panic!("page {number} resource scan failed: {error}"));
    }
    let mut missing = Vec::new();
    if !features.type0_font {
        missing.push("a valid Type0 font/ToUnicode/DescendantFonts chain");
    }
    if !features.transparency_group {
        missing.push("a Form/page /Group with /S /Transparency");
    }
    assert!(
        missing.is_empty(),
        "selected 200-page layout PDF is missing required source features: {}",
        missing.join(", ")
    );
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
async fn processing_window_two_does_not_roll_into_the_next_window() {
    #[derive(Clone)]
    struct State {
        layouts: Arc<AtomicUsize>,
        second_entered: Arc<Notify>,
        third_entered: Arc<Notify>,
        fourth_entered: Arc<Notify>,
        fifth_entered: Arc<Notify>,
        release_second: Arc<Notify>,
        release_fourth: Arc<Notify>,
    }
    async fn handler(
        AxumState(state): AxumState<State>,
        Json(request): Json<Value>,
    ) -> Json<Value> {
        if request.to_string().contains("Layout Detection") {
            let layout = state.layouts.fetch_add(1, Ordering::SeqCst) + 1;
            match layout {
                2 => {
                    state.second_entered.notify_one();
                    state.release_second.notified().await;
                }
                3 => state.third_entered.notify_one(),
                4 => {
                    state.fourth_entered.notify_one();
                    state.release_fourth.notified().await;
                }
                5 => state.fifth_entered.notify_one(),
                _ => {}
            }
        }
        empty_completion()
    }

    let state = State {
        layouts: Arc::new(AtomicUsize::new(0)),
        second_entered: Arc::new(Notify::new()),
        third_entered: Arc::new(Notify::new()),
        fourth_entered: Arc::new(Notify::new()),
        fifth_entered: Arc::new(Notify::new()),
        release_second: Arc::new(Notify::new()),
        release_fourth: Arc::new(Notify::new()),
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
    let route = tokio::spawn({
        let input = tiny_pdf(6);
        async move {
            client
                .parse_and_write_official_pdf(
                    PdfInput::Bytes(input),
                    route_options(2),
                    &output_root,
                    "many",
                )
                .await
        }
    });

    tokio::time::timeout(Duration::from_secs(5), state.second_entered.notified())
        .await
        .expect("layout #2 did not enter");
    assert!(
        tokio::time::timeout(Duration::from_millis(100), state.third_entered.notified())
            .await
            .is_err()
    );
    state.release_second.notify_one();
    tokio::time::timeout(Duration::from_secs(5), state.fourth_entered.notified())
        .await
        .expect("layout #4 did not enter");
    assert!(
        tokio::time::timeout(Duration::from_millis(100), state.fifth_entered.notified())
            .await
            .is_err()
    );
    state.release_fourth.notify_one();

    let manifest = tokio::time::timeout(Duration::from_secs(10), route)
        .await
        .expect("route test timed out")
        .unwrap()
        .unwrap();
    assert_eq!(state.layouts.load(Ordering::SeqCst), 6);
    let middle: Value =
        serde_json::from_slice(&std::fs::read(manifest.vlm_dir.join("many_middle.json")).unwrap())
            .unwrap();
    let pages = middle["pdf_info"].as_array().unwrap();
    assert_eq!(pages.len(), 6);
    for (index, page) in pages.iter().enumerate() {
        assert_eq!(page["page_idx"], index);
    }
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
