use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, State as AxumState},
    http::StatusCode,
    response::IntoResponse,
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
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
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

async fn configured_temperature_retry_client(app: Router) -> MinerUVlmClient {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    MinerUVlmClient::connect_with_temperature_retry(
        VlmHttpConfig {
            server_url: Some(format!("http://{address}").parse().unwrap()),
            model_name: Some("mock".into()),
            skip_model_name_checking: true,
            max_retries: 0,
            max_concurrency: 2,
            ..Default::default()
        },
        MinerUVlmConfig {
            layout_image_size: (8, 8),
            ..Default::default()
        },
        true,
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
            max_retries: 3, // production default; retries absorb transient Windows connection resets
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
            max_retries: 3, // production default; retries absorb transient Windows connection resets
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
            max_retries: 3, // production default; retries absorb transient Windows connection resets
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
    assert_eq!(mineru::canonical_stem("../office").unwrap(), ".._office");
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
            max_retries: 3, // production default; retries absorb transient Windows connection resets
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
    match error {
        mineru::VlmError::LimitExceeded {
            resource: "response",
            limit: 128,
            ..
        } => {}
        other => panic!("raw reply allowance: unexpected error: {other:?}"),
    }
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

/// A PDF whose pages have per-page MediaBox sizes, so a page can exceed one RGB half-slot while
/// staying under the full in-flight image cap.
fn sized_pdf(sizes: &[(f32, f32)]) -> Bytes {
    let mut pdf = Document::with_version("1.5");
    let page_tree = pdf.new_object_id();
    let page_ids: Vec<_> = (0..sizes.len()).map(|_| pdf.new_object_id()).collect();
    for (page, (width, height)) in page_ids.iter().zip(sizes) {
        let contents = pdf.add_object(Stream::new(dictionary! {}, Vec::new()));
        pdf.objects.insert(
            *page,
            Object::Dictionary(dictionary! {
                "Type" => "Page", "Parent" => page_tree,
                "MediaBox" => vec![0.into(), 0.into(), (*width).into(), (*height).into()],
                "Contents" => contents,
            }),
        );
    }
    pdf.objects.insert(page_tree, Object::Dictionary(dictionary! {
        "Type" => "Pages", "Kids" => page_ids.into_iter().map(Object::Reference).collect::<Vec<_>>(), "Count" => sizes.len() as i64,
    }));
    let catalog = pdf.add_object(dictionary! { "Type" => "Catalog", "Pages" => page_tree });
    pdf.trailer.set("Root", catalog);
    let mut bytes = Vec::new();
    pdf.save_to(&mut bytes).unwrap();
    Bytes::from(bytes)
}

/// Server-side observation of VLM requests for the two-window overlap tests. Layout request
/// numbers are 1-based admission order, so window N+1's first layout is `first_extra`.
#[derive(Clone)]
struct OverlapMock {
    requests: Arc<AtomicUsize>,
    active: Arc<AtomicUsize>,
    peak: Arc<AtomicUsize>,
    layouts: Arc<AtomicUsize>,
    layout_active: Arc<AtomicUsize>,
    layout_peak: Arc<AtomicUsize>,
    raw_response_bytes: Arc<AtomicUsize>,
    request_body_bytes: Arc<AtomicUsize>,
    seq: Arc<AtomicUsize>,
    log: Arc<Mutex<Vec<(usize, String)>>>,
    first_extra: usize,
    last_extra: usize,
    second_window_entered: Arc<Notify>,
    release: Arc<tokio::sync::watch::Sender<bool>>,
    hold_next_window: Arc<AtomicBool>,
    fail_next_window: Arc<AtomicBool>,
    page_zero_completed: Arc<Notify>,
}

/// Decrements the server-side activity counters even when a dropped client connection cancels
/// the handler task, so `layout_active` genuinely reflects only live VLM work.
struct ActiveGuard {
    active: Arc<AtomicUsize>,
    layout: Option<Arc<AtomicUsize>>,
}

impl Drop for ActiveGuard {
    fn drop(&mut self) {
        if let Some(layout) = &self.layout {
            layout.fetch_sub(1, Ordering::SeqCst);
        }
        self.active.fetch_sub(1, Ordering::SeqCst);
    }
}

impl OverlapMock {
    fn new(first_extra: usize, last_extra: usize) -> Self {
        Self {
            requests: Arc::new(AtomicUsize::new(0)),
            active: Arc::new(AtomicUsize::new(0)),
            peak: Arc::new(AtomicUsize::new(0)),
            layouts: Arc::new(AtomicUsize::new(0)),
            layout_active: Arc::new(AtomicUsize::new(0)),
            layout_peak: Arc::new(AtomicUsize::new(0)),
            raw_response_bytes: Arc::new(AtomicUsize::new(0)),
            request_body_bytes: Arc::new(AtomicUsize::new(0)),
            seq: Arc::new(AtomicUsize::new(0)),
            log: Arc::new(Mutex::new(Vec::new())),
            first_extra,
            last_extra,
            second_window_entered: Arc::new(Notify::new()),
            release: Arc::new(tokio::sync::watch::channel(false).0),
            hold_next_window: Arc::new(AtomicBool::new(true)),
            fail_next_window: Arc::new(AtomicBool::new(false)),
            page_zero_completed: Arc::new(Notify::new()),
        }
    }

    fn log_seq(&self, label: &str) -> Option<usize> {
        self.log
            .lock()
            .unwrap()
            .iter()
            .find(|(_, entry)| entry == label)
            .map(|(seq, _)| *seq)
    }

    async fn assert_no_lingering_layout(&self, context: &str) {
        let deadline = Instant::now() + Duration::from_secs(5);
        // Async sleep: a blocking sleep would stall every runtime worker the handlers run on.
        while self.layout_active.load(Ordering::SeqCst) != 0 {
            assert!(
                Instant::now() < deadline,
                "lingering VLM work for {context}: layout_active={}",
                self.layout_active.load(Ordering::SeqCst)
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
}

fn overlap_route(mock: OverlapMock) -> axum::routing::MethodRouter {
    axum::routing::post(move |Json(request): Json<Value>| {
        let state = mock.clone();
        async move { overlap_handler_inner(state, axum::Json(request)).await }
    })
}

async fn overlap_handler_inner(
    state: OverlapMock,
    Json(request): Json<Value>,
) -> Result<Json<Value>, StatusCode> {
    let body = request.to_string();
    state
        .request_body_bytes
        .fetch_add(body.len(), Ordering::SeqCst);
    state.requests.fetch_add(1, Ordering::SeqCst);
    let is_layout = body.contains("Layout Detection");
    let active = state.active.fetch_add(1, Ordering::SeqCst) + 1;
    state.peak.fetch_max(active, Ordering::SeqCst);
    let layout_counter = if is_layout {
        let active = state.layout_active.fetch_add(1, Ordering::SeqCst) + 1;
        state.layout_peak.fetch_max(active, Ordering::SeqCst);
        Some(Arc::clone(&state.layout_active))
    } else {
        None
    };
    let _guard = ActiveGuard {
        active: Arc::clone(&state.active),
        layout: layout_counter,
    };
    let layout_number = if is_layout {
        Some(state.layouts.fetch_add(1, Ordering::SeqCst) + 1)
    } else {
        None
    };
    if layout_number == Some(state.first_extra) {
        let seq = state.seq.fetch_add(1, Ordering::SeqCst);
        state
            .log
            .lock()
            .unwrap()
            .push((seq, format!("layout-{}", state.first_extra)));
        state.second_window_entered.notify_one();
    }
    let held = layout_number.is_some_and(|number| {
        state.hold_next_window.load(Ordering::SeqCst)
            && (state.first_extra..=state.last_extra).contains(&number)
    });
    let result = if held {
        if state.fail_next_window.load(Ordering::SeqCst) {
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        } else {
            let release = state.release.subscribe();
            // Timer-driven polling: axum/hyper handler tasks can be parked in a way that
            // skips the internal watch wake-up after a client disconnect, so re-observe the
            // released value on the tokio timer instead of relying on the change notification.
            while !*release.borrow() {
                tokio::time::sleep(Duration::from_millis(10)).await;
                if state.fail_next_window.load(Ordering::SeqCst) {
                    break;
                }
            }
            if state.fail_next_window.load(Ordering::SeqCst) {
                Err(StatusCode::INTERNAL_SERVER_ERROR)
            } else {
                Ok(empty_completion())
            }
        }
    } else {
        Ok(empty_completion())
    };
    if let Ok(reply) = &result {
        state
            .raw_response_bytes
            .fetch_add(reply.to_string().len(), Ordering::SeqCst);
    }
    result
}

/// Polls until no private staging directory remains under `output/stem`. Dropped staging
/// futures schedule capability cleanup on a dedicated thread, so residue removal is async.
async fn assert_clean_stem(output: &Path, stem: &str) {
    let root = output.join(stem);
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let any = match std::fs::read_dir(&root) {
            Ok(entries) => entries
                .flatten()
                .any(|entry| entry.file_name().to_string_lossy().starts_with(".vlm-")),
            Err(_) => false,
        };
        if !any {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "staging residue was not cleaned under {stem}"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// Runs a fresh single-page document through the same client after a failed route: the page
/// semaphore, layout semaphore, and budgets must all still be usable.
async fn assert_permits_reacquirable(client: &MinerUVlmClient, output: &Path) {
    let manifest = tokio::time::timeout(
        Duration::from_secs(10),
        client.parse_and_write_official_pdf(
            PdfInput::Bytes(tiny_pdf(1)),
            route_options(1),
            output,
            "again",
        ),
    )
    .await
    .expect("second parse hung: admission permits were not released")
    .expect("second parse failed: admission permits leaked");
    assert!(manifest.vlm_dir.join("again_middle.json").is_file());
}

fn published_page_indexes(manifest: &mineru::OfficialOutputManifest, stem: &str) -> Vec<u64> {
    let middle: Value = serde_json::from_slice(
        &std::fs::read(manifest.vlm_dir.join(format!("{stem}_middle.json"))).unwrap(),
    )
    .unwrap();
    middle["pdf_info"]
        .as_array()
        .unwrap()
        .iter()
        .map(|page| page["page_idx"].as_u64().unwrap())
        .collect()
}

#[tokio::test]
async fn route_staging_overlaps_next_window_vlm() {
    // 12 pages in two 6-page windows: the first layout of window N+1 (layout #7) must arrive
    // before the final staged DocumentPageCompleted of window N (page 5), while window N+1's
    // VLM is held on the server so the route provably cannot continue past phase B.
    let mock = OverlapMock::new(7, 12);
    let client = configured_client(
        Router::new().route("/v1/chat/completions", overlap_route(mock.clone())),
        8,
    )
    .await;

    let events = Arc::new(Mutex::new(Vec::new()));
    let callback: ProgressCallback = {
        let mock = mock.clone();
        let events = Arc::clone(&events);
        Arc::new(move |event| {
            if let ProgressEvent::DocumentPageCompleted {
                page_index,
                completed,
                total,
                ..
            } = event
            {
                let seq = mock.seq.fetch_add(1, Ordering::SeqCst);
                mock.log
                    .lock()
                    .unwrap()
                    .push((seq, format!("completed-{page_index}")));
                if page_index == 0 {
                    mock.page_zero_completed.notify_one();
                }
                events.lock().unwrap().push((page_index, completed, total));
            }
        })
    };

    let output = tempfile::tempdir().unwrap();
    let output_root = output.path().to_path_buf();
    let prepared = PreparedPdf {
        bytes: tiny_pdf(12),
        kind: DocumentKind::Pdf,
        original: tiny_pdf(12),
    };
    let route = tokio::spawn(async move {
        client
            .parse_and_write_prepared_pdf_with_events(
                prepared,
                route_options(6),
                &output_root,
                "overlap",
                Some(callback),
            )
            .await
    });

    tokio::time::timeout(
        Duration::from_secs(5),
        mock.second_window_entered.notified(),
    )
    .await
    .expect("window N+1 first layout did not enter");
    // Wait for the final staged event of window N (page 5) while window N+1 VLM is held.
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if mock.log_seq("completed-5").is_some() {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("window N staging did not complete");
    assert!(
        !route.is_finished(),
        "route must still wait for held window N+1 VLM"
    );

    let layout_seq = mock
        .log_seq("layout-7")
        .expect("window N+1 first layout recorded");
    let final_stage_seq = mock
        .log_seq("completed-5")
        .expect("window N final event recorded");
    assert!(
        layout_seq < final_stage_seq,
        "window N+1 VLM must start before window N staging finishes (layout {layout_seq} vs staged {final_stage_seq})"
    );

    let _ = mock.release.send(true);
    let manifest = tokio::time::timeout(Duration::from_secs(10), route)
        .await
        .expect("route timed out")
        .unwrap()
        .expect("overlapping route failed");

    mock.assert_no_lingering_layout("overlap").await;
    assert_eq!(mock.layouts.load(Ordering::SeqCst), 12);
    assert_eq!(mock.requests.load(Ordering::SeqCst), 12);
    assert!(mock.peak.load(Ordering::SeqCst) <= 8, "HTTP high water");
    assert!(
        mock.layout_peak.load(Ordering::SeqCst) <= 8,
        "layout HTTP high water"
    );
    assert!(
        mock.raw_response_bytes.load(Ordering::SeqCst) as u64
            <= route_options(6).max_raw_output_bytes as u64
    );
    assert_eq!(
        published_page_indexes(&manifest, "overlap"),
        (0..12u64).collect::<Vec<_>>()
    );
    let events = events.lock().unwrap();
    assert_eq!(
        events
            .iter()
            .map(|(index, _, _)| *index)
            .collect::<Vec<_>>(),
        (0..12).collect::<Vec<_>>()
    );
    assert!(
        events
            .iter()
            .all(|(_, completed, total)| { *completed <= *total && *total == 12 }),
        "completion counters stay bounded and document-total accurate"
    );
}

#[tokio::test]
async fn route_next_window_vlm_failure_disposes_partially_staged_window() {
    let mock = OverlapMock::new(7, 12);
    let client = configured_client(
        Router::new().route("/v1/chat/completions", overlap_route(mock.clone())),
        8,
    )
    .await;
    let callback: ProgressCallback = {
        let mock = mock.clone();
        Arc::new(move |event| {
            if let ProgressEvent::DocumentPageCompleted { page_index: 0, .. } = event {
                mock.page_zero_completed.notify_one();
            }
        })
    };

    let output = tempfile::tempdir().unwrap();
    let output_root = output.path().to_path_buf();
    let client_for_route = client.clone();
    let route_output = output_root.clone();
    let route = tokio::spawn(async move {
        client_for_route
            .parse_and_write_prepared_pdf_with_events(
                PreparedPdf {
                    bytes: tiny_pdf(12),
                    kind: DocumentKind::Pdf,
                    original: tiny_pdf(12),
                },
                route_options(6),
                &route_output,
                "failed-vlm",
                Some(callback),
            )
            .await
    });

    tokio::time::timeout(
        Duration::from_secs(5),
        mock.second_window_entered.notified(),
    )
    .await
    .expect("window N+1 VLM did not start");
    tokio::time::timeout(Duration::from_secs(5), mock.page_zero_completed.notified())
        .await
        .expect("window N was not partially staged");
    assert!(!route.is_finished());
    mock.fail_next_window.store(true, Ordering::SeqCst);
    let _ = mock.release.send(true);

    let error = tokio::time::timeout(Duration::from_secs(10), route)
        .await
        .expect("route timed out after next-window VLM failure")
        .unwrap()
        .expect_err("route must fail");
    assert!(
        matches!(error, mineru::VlmError::Http { status: 500, .. }),
        "unexpected error: {error}"
    );
    mock.assert_no_lingering_layout("next-window VLM failure")
        .await;
    assert!(
        (7..=12).contains(&mock.layouts.load(Ordering::SeqCst)),
        "window N's six layouts plus at least one next-window layout were admitted"
    );
    assert!(!output_root.join("failed-vlm/vlm").exists());
    assert_clean_stem(output.path(), "failed-vlm").await;
    // The failed route's next-window requests may keep the layout counter inside the hold range;
    // the re-acquirability probe must not inherit the failure mode.
    mock.fail_next_window.store(false, Ordering::SeqCst);
    let _ = mock.release.send(true);
    assert_permits_reacquirable(&client, output.path()).await;
}

#[tokio::test]
async fn route_staging_failure_drops_next_window_vlm() {
    let mock = OverlapMock::new(7, 12);
    let client = configured_client(
        Router::new().route("/v1/chat/completions", overlap_route(mock.clone())),
        8,
    )
    .await;
    let callback: ProgressCallback = {
        let mock = mock.clone();
        Arc::new(move |event| {
            if let ProgressEvent::DocumentPageCompleted { page_index: 0, .. } = event {
                mock.page_zero_completed.notify_one();
            }
        })
    };

    // Staging budget calibrated so page 0 of window N stages fully and the next page's first
    // preview write exceeds the cumulative staged-text allowance (see staging_failure_budget()).
    let mut options = route_options(6);
    options.max_staged_text_bytes = staging_failure_budget().await;

    let output = tempfile::tempdir().unwrap();
    let output_root = output.path().to_path_buf();
    let client_for_route = client.clone();
    let route_output = output_root.clone();
    let route = tokio::spawn(async move {
        client_for_route
            .parse_and_write_prepared_pdf_with_events(
                PreparedPdf {
                    bytes: tiny_pdf(12),
                    kind: DocumentKind::Pdf,
                    original: tiny_pdf(12),
                },
                options,
                &route_output,
                "failed-stage",
                Some(callback),
            )
            .await
    });

    tokio::time::timeout(
        Duration::from_secs(5),
        mock.second_window_entered.notified(),
    )
    .await
    .expect("window N+1 VLM did not start before staging failure");
    tokio::time::timeout(Duration::from_secs(5), mock.page_zero_completed.notified())
        .await
        .expect("window N did not stage its first page");

    let error = tokio::time::timeout(Duration::from_secs(10), route)
        .await
        .expect("route timed out after staging failure")
        .unwrap()
        .expect_err("route must fail");
    assert!(
        matches!(
            error,
            mineru::VlmError::LimitExceeded {
                resource: "staged text/JSON bytes",
                ..
            }
        ),
        "unexpected staging error: {error}"
    );
    // Release the in-flight window N+1 handlers so the observation seam drains.
    let _ = mock.release.send(true);
    mock.assert_no_lingering_layout("staging failure").await;
    assert!(!output_root.join("failed-stage/vlm").exists());
    assert_clean_stem(output.path(), "failed-stage").await;
    assert_permits_reacquirable(&client, output.path()).await;
}

/// One page's total staged text allowance: page 0 of the overlap document stages fully, the
/// next page's preview write then exceeds the allowance and fails the route mid-staging.
async fn staging_failure_budget() -> usize {
    // Uses a plain non-holding client: only the route's own staging limits matter here.
    let client = client().await;
    // Probe the smallest budget that lets a single page stage completely.
    let mut low = 0usize;
    let mut high = 4096usize;
    let output = tempfile::tempdir().unwrap();
    while low < high {
        let probe = (low + high) / 2;
        let mut options = route_options(1);
        options.max_staged_text_bytes = probe;
        let succeeded = tokio::time::timeout(
            Duration::from_secs(10),
            client.parse_and_write_official_pdf(
                PdfInput::Bytes(tiny_pdf(1)),
                options,
                output.path(),
                "probe",
            ),
        )
        .await
        .expect("probe hung")
        .is_ok();
        if succeeded {
            high = probe;
        } else {
            low = probe + 1;
        }
    }
    // low is now the smallest per-page allowance that succeeds. Add one byte: page 0 fits, the
    // next page's first preview write pushes the cumulative total over.
    low + 1
}

#[tokio::test]
async fn route_overlap_keeps_raw_encoded_budget_high_water() {
    let mock = OverlapMock::new(7, 12);
    let client = configured_client(
        Router::new().route("/v1/chat/completions", overlap_route(mock.clone())),
        8,
    )
    .await;

    // The budget high water is about cumulative charges, not holding: leave the natural overlap
    // (window N+1 VLM admitted while window N stages) but do not park requests on the server.
    mock.hold_next_window.store(false, Ordering::SeqCst);
    let run = |options: OfficialPdfOptions, stem: &str| {
        let output = tempfile::tempdir().unwrap();
        let output_root = output.path().to_path_buf();
        let bytes = tiny_pdf(12);
        let stem = stem.to_string();
        let client = client.clone();
        Box::pin(async move {
            client
                .parse_and_write_official_pdf(PdfInput::Bytes(bytes), options, &output_root, &stem)
                .await
        })
    };

    // Probe run with generous budgets measures the document's exact raw/request high water.
    let manifest = tokio::time::timeout(Duration::from_secs(10), run(route_options(6), "probe"))
        .await
        .expect("probe timed out")
        .expect("probe failed");
    assert_eq!(
        published_page_indexes(&manifest, "probe"),
        (0..12u64).collect::<Vec<_>>()
    );
    let raw = mock.raw_response_bytes.load(Ordering::SeqCst);
    let request_bytes = mock.request_body_bytes.load(Ordering::SeqCst);
    let peak = mock.peak.load(Ordering::SeqCst);
    let layout_peak = mock.layout_peak.load(Ordering::SeqCst);
    assert!(peak <= 8, "HTTP concurrency high water");
    assert!(layout_peak <= 8, "layout HTTP concurrency high water");
    assert!(raw > 0 && request_bytes > raw, "measured budgets are sane");

    // The exact raw allowance still succeeds and one byte below it fails: the raw document
    // budget is enforced unchanged under the overlap.
    let mut tight = route_options(6);
    tight.max_raw_output_bytes = raw;
    tokio::time::timeout(Duration::from_secs(10), run(tight, "tight-raw"))
        .await
        .expect("exact raw budget timed out")
        .expect("exact raw budget must fit");
    let mut below = route_options(6);
    below.max_raw_output_bytes = raw - 1;
    let error = tokio::time::timeout(Duration::from_secs(10), run(below, "below-raw"))
        .await
        .expect("below-raw timed out")
        .expect_err("one byte below the raw allowance must fail");
    assert!(
        matches!(
            error,
            mineru::VlmError::LimitExceeded {
                resource: "response",
                ..
            }
        ),
        "unexpected raw budget error: {error}"
    );

    // The encoded document budget is charged only from the encoded image payloads embedded in
    // the request bodies, so the measured request-body total is an upper bound on the encoded
    // high water: an allowance of exactly that many bytes must still succeed under overlap.
    let mut encoded = route_options(6);
    encoded.max_encoded_document_bytes = request_bytes;
    tokio::time::timeout(Duration::from_secs(10), run(encoded, "tight-encoded"))
        .await
        .expect("encoded budget timed out")
        .expect("encoded budget upper bound must fit");
}

#[tokio::test]
async fn route_overlap_preserves_source_order_with_uneven_latencies() {
    #[derive(Clone)]
    struct State {
        layouts: Arc<AtomicUsize>,
        page_six_entered: Arc<Notify>,
        release_first_window: Arc<Notify>,
        release_second_window: Arc<Notify>,
    }
    async fn handler(
        AxumState(state): AxumState<State>,
        Json(request): Json<Value>,
    ) -> Json<Value> {
        if request.to_string().contains("Layout Detection") {
            let layout = state.layouts.fetch_add(1, Ordering::SeqCst) + 1;
            // Reverse completion within each window: the first page of a window is held until
            // its siblings have been admitted, forcing out-of-order VLM completion.
            match layout {
                1 => state.release_first_window.notified().await,
                7 => {
                    state.page_six_entered.notify_one();
                    state.release_second_window.notified().await;
                }
                _ => {}
            }
        }
        empty_completion()
    }

    let state = State {
        layouts: Arc::new(AtomicUsize::new(0)),
        page_six_entered: Arc::new(Notify::new()),
        release_first_window: Arc::new(Notify::new()),
        release_second_window: Arc::new(Notify::new()),
    };
    let client = configured_client(
        Router::new()
            .route("/v1/chat/completions", post(handler))
            .with_state(state.clone()),
        8,
    )
    .await;

    let events = Arc::new(Mutex::new(Vec::new()));
    let callback: ProgressCallback = {
        let events = Arc::clone(&events);
        Arc::new(move |event| {
            if let ProgressEvent::DocumentPageCompleted { page_index, .. } = event {
                events.lock().unwrap().push(page_index);
            }
        })
    };
    let output = tempfile::tempdir().unwrap();
    let output_root = output.path().to_path_buf();
    let route = tokio::spawn(async move {
        client
            .parse_and_write_prepared_pdf_with_events(
                PreparedPdf {
                    bytes: tiny_pdf(12),
                    kind: DocumentKind::Pdf,
                    original: tiny_pdf(12),
                },
                route_options(6),
                &output_root,
                "ordered",
                Some(callback),
            )
            .await
    });

    // Window N's page 0 is held; pages 1-5 are admitted and complete. Then release page 0 so
    // window N's VLM finishes out of order while staging of window N overlaps window N+1.
    tokio::time::timeout(Duration::from_secs(5), async {
        while state.layouts.load(Ordering::SeqCst) < 2 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("window N did not admit its siblings");
    assert!(!route.is_finished());
    state.release_first_window.notify_one();

    // Window N+1's first page (layout 7) is held; its siblings are admitted during the overlap.
    tokio::time::timeout(Duration::from_secs(5), state.page_six_entered.notified())
        .await
        .expect("window N+1 VLM did not start during staging");
    tokio::time::timeout(Duration::from_secs(5), async {
        while state.layouts.load(Ordering::SeqCst) < 8 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("window N+1 did not admit its siblings");
    assert!(!route.is_finished());
    state.release_second_window.notify_one();

    let manifest = tokio::time::timeout(Duration::from_secs(10), route)
        .await
        .expect("route timed out")
        .unwrap()
        .expect("ordered route failed");

    assert_eq!(
        published_page_indexes(&manifest, "ordered"),
        (0..12u64).collect::<Vec<_>>()
    );
    assert_eq!(
        *events.lock().unwrap(),
        (0..12).collect::<Vec<_>>(),
        "staged publication stays source ordered despite reverse VLM completion"
    );
}

#[tokio::test]
async fn route_overlap_deadline_timeout_maps_and_drops_stage() {
    let mock = OverlapMock::new(7, 12);
    let client = configured_client(
        Router::new().route("/v1/chat/completions", overlap_route(mock.clone())),
        8,
    )
    .await;
    let callback: ProgressCallback = {
        let mock = mock.clone();
        Arc::new(move |event| {
            if let ProgressEvent::DocumentPageCompleted { page_index: 0, .. } = event {
                mock.page_zero_completed.notify_one();
            }
        })
    };

    let mut options = route_options(6);
    options.total_deadline = Duration::from_secs(2);
    let output = tempfile::tempdir().unwrap();
    let output_root = output.path().to_path_buf();
    let client_for_route = client.clone();
    let route_output = output_root.clone();
    let route = tokio::spawn(async move {
        client_for_route
            .parse_and_write_prepared_pdf_with_events(
                PreparedPdf {
                    bytes: tiny_pdf(12),
                    kind: DocumentKind::Pdf,
                    original: tiny_pdf(12),
                },
                options,
                &route_output,
                "deadline",
                Some(callback),
            )
            .await
    });

    tokio::time::timeout(
        Duration::from_secs(5),
        mock.second_window_entered.notified(),
    )
    .await
    .expect("window N+1 VLM did not start");
    tokio::time::timeout(Duration::from_secs(5), mock.page_zero_completed.notified())
        .await
        .expect("staging did not start before the deadline");

    let started = Instant::now();
    let error = tokio::time::timeout(Duration::from_secs(10), route)
        .await
        .expect("route did not stop at the deadline")
        .unwrap()
        .expect_err("deadline must fail the route");
    assert!(
        matches!(
            error,
            mineru::VlmError::Timeout {
                operation: "official PDF"
            }
        ),
        "unexpected deadline error: {error}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "route must stop promptly after the deadline, not hang"
    );

    // Release the held window N+1 handlers so the observation seam drains.
    let _ = mock.release.send(true);
    mock.assert_no_lingering_layout("deadline timeout").await;
    assert!(!output_root.join("deadline/vlm").exists());
    assert_clean_stem(output.path(), "deadline").await;
    assert_permits_reacquirable(&client, output.path()).await;
}

#[tokio::test]
async fn route_full_cap_fallback_window_does_not_overlap() {
    // Page sizes: 1x1pt (27 RGB bytes, fits a 40-byte half slot), 1.5x1.5pt (75 bytes, exceeds
    // the half slot but fits the 80-byte full cap -> full-cap fallback), then 1x1pt again.
    let input = sized_pdf(&[(1.0, 1.0), (1.5, 1.5), (1.0, 1.0)]);
    #[derive(Clone)]
    struct State {
        layouts: Arc<AtomicUsize>,
        fallback_entered: Arc<Notify>,
        release_fallback: Arc<Notify>,
        next_window_entered: Arc<Notify>,
    }
    async fn handler(
        AxumState(state): AxumState<State>,
        Json(request): Json<Value>,
    ) -> Json<Value> {
        if request.to_string().contains("Layout Detection") {
            let layout = state.layouts.fetch_add(1, Ordering::SeqCst) + 1;
            match layout {
                2 => {
                    state.fallback_entered.notify_one();
                    state.release_fallback.notified().await;
                }
                3 => state.next_window_entered.notify_one(),
                _ => {}
            }
        }
        empty_completion()
    }

    let state = State {
        layouts: Arc::new(AtomicUsize::new(0)),
        fallback_entered: Arc::new(Notify::new()),
        release_fallback: Arc::new(Notify::new()),
        next_window_entered: Arc::new(Notify::new()),
    };
    let client = configured_client(
        Router::new()
            .route("/v1/chat/completions", post(handler))
            .with_state(state.clone()),
        8,
    )
    .await;
    let mut options = route_options(2);
    options.max_in_flight_image_bytes = 80;
    let output = tempfile::tempdir().unwrap();
    let output_root = output.path().to_path_buf();
    let route = tokio::spawn(async move {
        client
            .parse_and_write_official_pdf(PdfInput::Bytes(input), options, &output_root, "fallback")
            .await
    });

    // The full-cap fallback page's layout (layout #2) is held. No next-window VLM (layout #3)
    // may be admitted while it is in flight: the fallback is a declared memory-bound exception.
    tokio::time::timeout(Duration::from_secs(5), state.fallback_entered.notified())
        .await
        .expect("fallback page VLM did not enter");
    assert!(
        tokio::time::timeout(
            Duration::from_millis(200),
            state.next_window_entered.notified()
        )
        .await
        .is_err(),
        "next-window VLM must not overlap the full-cap fallback page"
    );
    assert!(!route.is_finished());
    state.release_fallback.notify_one();
    tokio::time::timeout(Duration::from_secs(5), state.next_window_entered.notified())
        .await
        .expect("next window VLM did not resume after the fallback");

    let manifest = tokio::time::timeout(Duration::from_secs(10), route)
        .await
        .expect("route timed out")
        .unwrap()
        .expect("fallback route failed");
    assert_eq!(state.layouts.load(Ordering::SeqCst), 3);
    assert_eq!(published_page_indexes(&manifest, "fallback"), vec![0, 1, 2]);
}

#[tokio::test]
async fn route_recovers_malformed_layout_with_warning_and_completes() {
    let app = Router::new().route(
        "/v1/chat/completions",
        post(|Json(request): Json<Value>| async move {
            let layout = request.to_string().contains("Layout Detection");
            if layout {
                Json(json!({"choices":[{"finish_reason":"stop","message":{"content":"this is not a layout marker"}}]})).into_response()
            } else {
                Json(json!({"choices":[{"finish_reason":"stop","message":{"content":"recognized"}}]})).into_response()
            }
        }),
    );
    let client = configured_client(app, 2).await;
    let output = tempfile::tempdir().unwrap();
    let events = Arc::new(Mutex::new(Vec::new()));
    let callback: ProgressCallback = {
        let events = Arc::clone(&events);
        Arc::new(move |event| events.lock().unwrap().push(event))
    };
    let pdf = tiny_pdf(1);
    client
        .parse_and_write_prepared_pdf_with_events(
            PreparedPdf {
                bytes: pdf.clone(),
                kind: DocumentKind::Pdf,
                original: pdf,
            },
            route_options(1),
            output.path(),
            "layout-recover",
            Some(callback),
        )
        .await
        .unwrap();
    assert!(
        output
            .path()
            .join("layout-recover/vlm/layout-recover.md")
            .is_file()
    );
    let messages: Vec<_> = events
        .lock()
        .unwrap()
        .iter()
        .filter_map(|event| match event {
            ProgressEvent::VlmWarning { message } => Some(message.clone()),
            _ => None,
        })
        .collect();
    assert!(
        messages.iter().any(|m| m.contains("layout parse failed")),
        "{messages:?}"
    );
}

#[tokio::test]
async fn route_recovers_failed_semantic_replies_with_warning_and_completes() {
    let app = Router::new().route(
        "/v1/chat/completions",
        post(|Json(request): Json<Value>| async move {
            let layout = request.to_string().contains("Layout Detection");
            if layout {
                Json(json!({"choices":[{"finish_reason":"stop","message":{"content":"<|box_start|>0 0 1000 1000<|box_end|><|ref_start|>text<|ref_end|><|rotate_up|>"}}]})).into_response()
            } else {
                // A malformed-200 LLM reply (missing choices) degrades to a warning; a 5xx
                // service failure stays an error and is covered by the abort test below.
                Json(json!({"choices":[]})).into_response()
            }
        }),
    );
    let client = configured_client(app, 2).await;
    let output = tempfile::tempdir().unwrap();
    let events = Arc::new(Mutex::new(Vec::new()));
    let callback: ProgressCallback = {
        let events = Arc::clone(&events);
        Arc::new(move |event| events.lock().unwrap().push(event))
    };
    let pdf = tiny_pdf(1);
    client
        .parse_and_write_prepared_pdf_with_events(
            PreparedPdf {
                bytes: pdf.clone(),
                kind: DocumentKind::Pdf,
                original: pdf,
            },
            route_options(1),
            output.path(),
            "semantic-recover",
            Some(callback),
        )
        .await
        .unwrap();
    assert!(
        output
            .path()
            .join("semantic-recover/vlm/semantic-recover.md")
            .is_file()
    );
    let messages: Vec<_> = events
        .lock()
        .unwrap()
        .iter()
        .filter_map(|event| match event {
            ProgressEvent::VlmWarning { message } => Some(message.clone()),
            _ => None,
        })
        .collect();
    assert!(
        messages.iter().any(|m| m.contains("missing choices")),
        "{messages:?}"
    );
}

#[tokio::test]
async fn official_vlm_warnings_identify_document_page_and_stage() {
    let semantic_calls = Arc::new(AtomicUsize::new(0));
    let app = Router::new().route(
        "/v1/chat/completions",
        post({
            let semantic_calls = Arc::clone(&semantic_calls);
            move |Json(request): Json<Value>| {
                let semantic_calls = Arc::clone(&semantic_calls);
                async move {
                    let layout = request.to_string().contains("Layout Detection");
                    let response = if layout {
                        json!({"choices":[{"finish_reason":"content_filter","message":{"content":"<|box_start|>0 0 1000 1000<|box_end|><|ref_start|>text<|ref_end|><|rotate_up|>"}}]})
                    } else if semantic_calls.fetch_add(1, Ordering::SeqCst) == 0 {
                        json!({"choices":[]})
                    } else {
                        json!({"choices":[{"finish_reason":"stop","message":{"content":"usable"}}]})
                    };
                    Json(response)
                }
            }
        }),
    );
    let client = configured_temperature_retry_client(app).await;
    let output = tempfile::tempdir().unwrap();
    let events = Arc::new(Mutex::new(Vec::new()));
    let callback: ProgressCallback = {
        let events = Arc::clone(&events);
        Arc::new(move |event| events.lock().unwrap().push(event))
    };
    let pdf = tiny_pdf(1);
    client
        .parse_and_write_prepared_pdf_with_events(
            PreparedPdf {
                bytes: pdf.clone(),
                kind: DocumentKind::Pdf,
                original: pdf,
            },
            route_options(1),
            output.path(),
            "retry-warning",
            Some(callback),
        )
        .await
        .unwrap();
    let messages: Vec<_> = events
        .lock()
        .unwrap()
        .iter()
        .filter_map(|event| match event {
            ProgressEvent::VlmWarning { message } => Some(message.clone()),
            _ => None,
        })
        .collect();
    assert!(messages.iter().any(|message| {
        message.contains("document=retry-warning")
            && message.contains("page=0")
            && message.contains("stage=layout")
            && message.contains("content_filter")
    }));
    assert!(messages.iter().any(|message| {
        message.contains("document=retry-warning")
            && message.contains("page=0")
            && message.contains("stage=semantic")
            && message.contains("missing choices")
    }));
    assert!(messages.iter().any(|message| {
        message.contains("document=retry-warning")
            && message.contains("page=0")
            && message.contains("stage=semantic")
            && message.contains("temperature retry")
    }));
}

#[tokio::test]
async fn route_aborts_on_semantic_service_failure() {
    let app = Router::new().route(
        "/v1/chat/completions",
        post(|Json(request): Json<Value>| async move {
            let layout = request.to_string().contains("Layout Detection");
            if layout {
                Json(json!({"choices":[{"finish_reason":"stop","message":{"content":"<|box_start|>0 0 1000 1000<|box_end|><|ref_start|>text<|ref_end|><|rotate_up|>"}}]})).into_response()
            } else {
                // A 5xx is a service failure, not LLM-output malformation: it must abort the
                // route rather than degrade every page to empty content and exit 0.
                (StatusCode::INTERNAL_SERVER_ERROR, "mock semantic failure").into_response()
            }
        }),
    );
    let client = configured_client(app, 2).await;
    let output = tempfile::tempdir().unwrap();
    let pdf = tiny_pdf(1);
    let error = client
        .parse_and_write_prepared_pdf(
            PreparedPdf {
                bytes: pdf.clone(),
                kind: DocumentKind::Pdf,
                original: pdf,
            },
            route_options(1),
            output.path(),
            "semantic-fail",
        )
        .await
        .unwrap_err();
    assert!(matches!(error, mineru::VlmError::Http { .. }), "{error:?}");
    assert!(!output.path().join("semantic-fail/vlm").exists());
}
