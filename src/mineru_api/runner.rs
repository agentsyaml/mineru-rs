#[cfg(feature = "internal-mineru-api-client")]
use super::discovery;
use super::{
    InputDocument, RemoteEnv, RemoteOptions, TaskFailure,
    archive::{self, ArchiveLimits, DownloadedZip},
    http::MineruApiClient,
    planning,
};
use crate::{OfficeWorkers, RasterWorkers};
use crate::{ProgressCallback, ProgressEvent};
use futures_util::future::join_all;
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

#[cfg(feature = "internal-mineru-api-client")]
pub(super) async fn run_remote(
    input: &Path,
    output: &Path,
    api_url: &str,
    options: RemoteOptions,
    env: RemoteEnv,
) -> Result<Vec<TaskFailure>, String> {
    run_remote_with_discovery(input, output, api_url, options, env, |input, options| {
        discovery::discover(&input, &options)
    })
    .await
}

pub(super) async fn run_documents(
    documents: Vec<super::RemoteApiDocument>,
    output: &Path,
    api_url: &str,
    options: super::RemoteApiOptions,
    env: super::RemoteApiEnv,
    events: Option<ProgressCallback>,
) -> Result<Vec<super::RemoteApiFailure>, String> {
    if options.client_side_output_generation {
        return Err("client-side output generation is unsupported".into());
    }
    if options.backend != "vlm-http-client" {
        return Err(format!("unsupported backend: {}", options.backend));
    }
    let backend = super::Backend::parse(&options.backend)?;
    let method = super::ParseMethod::parse(&options.method)?;
    let effort = super::Effort::parse(&options.effort)?;
    let lang = super::normalize_remote_language(&options.language)?;
    if options.end.is_some_and(|end| options.start > end)
        || documents.is_empty()
        || env.max_concurrent_requests == 0
        || !env.result_timeout_seconds.is_finite()
        || env.result_timeout_seconds < 1.
        || !env.download_timeout_seconds.is_finite()
        || env.download_timeout_seconds < 1.
    {
        return Err("invalid remote API options".into());
    }
    options
        .route
        .validate()
        .map_err(|_| "invalid route options")?;
    let api_url = super::normalize_api_url(api_url);
    if !valid_url(&api_url) {
        return Err("invalid API URL".into());
    }
    if let Some(url) = options
        .server_url
        .as_deref()
        .filter(|url| !url.trim().is_empty())
    {
        if !valid_url(url) {
            return Err("invalid model server URL".into());
        }
    }
    let server_url = options
        .server_url
        .as_deref()
        .map(str::trim)
        .filter(|url| !url.is_empty())
        .map(str::to_owned);
    let raw_stems = documents.iter().map(|d| d.stem.clone()).collect::<Vec<_>>();
    if planning::unique_stems(&raw_stems) != raw_stems {
        return Err("invalid document stems".into());
    }
    let mut last_order = None;
    let documents = documents
        .into_iter()
        .map(|d| {
            let suffix = d
                .path
                .extension()
                .and_then(|s| s.to_str())
                .and_then(crate::input_prepare::DocumentKind::from_suffix);
            if d.effective_pages == 0
                || suffix != Some(d.kind)
                || crate::canonical_stem(&d.stem).ok().as_deref() != Some(&d.stem)
                || last_order.is_some_and(|order| d.order <= order)
                || std::fs::symlink_metadata(&d.path)
                    .ok()
                    .filter(|m| m.file_type().is_file() && !m.file_type().is_symlink())
                    .is_none()
            {
                return Err("invalid document record".into());
            }
            last_order = Some(d.order);
            Ok(InputDocument {
                path: d.path,
                suffix: d.kind.suffix().into(),
                stem: d.stem,
                effective_pages: d.effective_pages,
                order: d.order,
            })
        })
        .collect::<Result<Vec<_>, String>>()?;
    let route = options.route.clone();
    let options = RemoteOptions {
        lang,
        backend,
        method,
        effort,
        formula: options.formula,
        table: options.table,
        image_analysis: options.image_analysis,
        server_url,
        start: options.start,
        end: options.end,
        client_side: false,
    };
    archive::preflight_output_root(output)?;
    let office = OfficeWorkers::new().map_err(|_| "remote preview workers unavailable")?;
    let raster = RasterWorkers::default();
    run_core_owned(
        documents,
        output,
        &api_url,
        options,
        env.into(),
        events,
        route,
        office,
        raster,
    )
    .await
    .map(|v| {
        v.into_iter()
            .map(|f| super::RemoteApiFailure {
                task_index: f.task_index,
                document_stems: f.document_stems,
                message: f.message,
            })
            .collect()
    })
}

async fn run_core_owned(
    documents: Vec<InputDocument>,
    output: &Path,
    api_url: &str,
    options: RemoteOptions,
    env: RemoteEnv,
    events: Option<ProgressCallback>,
    route: crate::OfficialPdfOptions,
    office: OfficeWorkers,
    raster: RasterWorkers,
) -> Result<Vec<TaskFailure>, String> {
    let result = run_core(
        documents,
        output,
        api_url,
        options,
        env,
        events,
        Some((&route, &office, &raster)),
    )
    .await;
    office.drain().await;
    raster.drain().await;
    result
}

fn valid_url(value: &str) -> bool {
    url::Url::parse(value).ok().is_some_and(|url| {
        matches!(url.scheme(), "http" | "https")
            && url.host_str().is_some()
            && url.username().is_empty()
            && url.password().is_none()
            && url.query().is_none()
            && url.fragment().is_none()
    })
}

async fn run_remote_with_discovery<F>(
    input: &Path,
    output: &Path,
    api_url: &str,
    options: RemoteOptions,
    env: RemoteEnv,
    discover: F,
) -> Result<Vec<TaskFailure>, String>
where
    F: FnOnce(PathBuf, RemoteOptions) -> Result<Vec<InputDocument>, String> + Send + 'static,
{
    if options.client_side {
        return Err("client-side output generation is unsupported".into());
    }
    archive::preflight_output_root(output)?;
    let input = input.to_path_buf();
    let discovery_options = options.clone();
    let documents = tokio::task::spawn_blocking(move || discover(input, discovery_options))
        .await
        .map_err(|_| "internal discovery task failed")??;
    run_core(documents, output, api_url, options, env, None, None).await
}

async fn run_core(
    documents: Vec<InputDocument>,
    output: &Path,
    api_url: &str,
    options: RemoteOptions,
    env: RemoteEnv,
    events: Option<ProgressCallback>,
    preview: Option<(&crate::OfficialPdfOptions, &OfficeWorkers, &RasterWorkers)>,
) -> Result<Vec<TaskFailure>, String> {
    archive::preflight_output_root(output)?;
    let client = Arc::new(MineruApiClient::new(api_url)?);
    let health = client.health().await?;
    let tasks = planning::plan_tasks(options.backend, &documents, health.processing_window_size)?;
    let concurrency = planning::effective_concurrency(
        env.max_concurrent_requests,
        health.max_concurrent_requests,
        tasks.len(),
    )?;
    drop(super::request_form(&options));

    let output = output.to_path_buf();
    let mut failures = Vec::new();
    for wave in tasks.chunks(concurrency) {
        let staged = join_all(wave.iter().cloned().map(|task| {
            let client = Arc::clone(&client);
            let options = options.clone();
            {
                let events = events.clone();
                async move {
                    (
                        task.clone(),
                        stage(client, &options, env, task, events).await,
                    )
                }
            }
        }))
        .await;
        for (task, result) in staged {
            match result {
                Ok(zip) => {
                    crate::progress_events::emit(
                        &events,
                        ProgressEvent::ApiExtracting {
                            label: task_label(&task),
                        },
                    );
                    let destination = output.clone();
                    let index = task.index;
                    let stems = stems(&task.documents);
                    let extracted = tokio::task::spawn_blocking(move || {
                        zip.extract(&destination, ArchiveLimits::default())
                    })
                    .await
                    .unwrap_or_else(|_| Err("internal archive extraction task failed".into()));
                    if let Err(message) = extracted {
                        crate::progress_events::emit(
                            &events,
                            ProgressEvent::ApiFailed {
                                label: task_label(&task),
                                message: message.clone(),
                            },
                        );
                        failures.push(TaskFailure {
                            task_index: index,
                            document_stems: stems,
                            message,
                        });
                    } else {
                        if let Some((route, office, raster)) = preview {
                            for document in &task.documents {
                                let kind = crate::DocumentKind::from_suffix(&document.suffix)
                                    .expect("validated document kind");
                                if let Err(message) = crate::mineru_api::remote_preview::prepare_and_publish_downloaded(&output, &document.stem, kind, route, office, raster, events.clone()).await {
                                    crate::progress_events::emit(&events, ProgressEvent::ApiWarning { label: task_label(&task), message });
                                }
                            }
                        }
                        crate::progress_events::emit(
                            &events,
                            ProgressEvent::ApiCompleted {
                                label: task_label(&task),
                            },
                        );
                    }
                }
                Err(message) => {
                    crate::progress_events::emit(
                        &events,
                        ProgressEvent::ApiFailed {
                            label: task_label(&task),
                            message: message.clone(),
                        },
                    );
                    failures.push(TaskFailure {
                        task_index: task.index,
                        document_stems: stems(&task.documents),
                        message,
                    })
                }
            }
        }
    }
    failures.sort_by_key(|failure| failure.task_index);
    Ok(failures)
}

async fn stage(
    client: Arc<MineruApiClient>,
    options: &RemoteOptions,
    env: RemoteEnv,
    task: super::PlannedTask,
    events: Option<ProgressCallback>,
) -> Result<DownloadedZip, String> {
    let submitted = client.submit(options, &task.documents).await?;
    crate::progress_events::emit(
        &events,
        ProgressEvent::ApiSubmitted {
            label: task_label(&task),
        },
    );
    let label = task_label(&task);
    let mut last = None;
    client
        .poll(
            &submitted.status_url,
            env,
            Some(&mut |snapshot| {
                let value = (snapshot.status.clone(), snapshot.queued_ahead);
                if last.replace(value.clone()) != Some(value) {
                    match snapshot.status.as_str() {
                        "pending" => crate::progress_events::emit(
                            &events,
                            ProgressEvent::ApiPending {
                                label: label.clone(),
                                queued_ahead: snapshot.queued_ahead,
                            },
                        ),
                        "processing" => crate::progress_events::emit(
                            &events,
                            ProgressEvent::ApiProcessing {
                                label: label.clone(),
                            },
                        ),
                        _ => {}
                    }
                }
            }),
        )
        .await?;
    crate::progress_events::emit(
        &events,
        ProgressEvent::ApiDownloading {
            label: task_label(&task),
        },
    );
    client
        .download_result_zip(
            &submitted.result_url,
            &task_label(&task),
            env,
            ArchiveLimits::default(),
        )
        .await
}

fn stems(documents: &[InputDocument]) -> Vec<String> {
    documents
        .iter()
        .map(|document| document.stem.clone())
        .collect()
}

fn task_label(task: &super::PlannedTask) -> String {
    format!(
        "task#{} [{}]",
        task.index,
        stems(&task.documents).join(", ")
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        Router,
        body::Bytes,
        extract::{Path as AxumPath, State},
        http::{HeaderMap, StatusCode},
        response::IntoResponse,
        routing::{get, post},
    };
    use serde_json::{Value, json};
    use std::{
        io::{Cursor, Write},
        sync::{
            Mutex,
            atomic::{AtomicUsize, Ordering},
        },
        time::Duration,
    };
    use tokio::sync::{Barrier, Notify};
    use zip::{ZipWriter, write::SimpleFileOptions};

    #[derive(Clone)]
    struct TestState {
        events: Arc<Mutex<Vec<String>>>,
        posts: Arc<AtomicUsize>,
        zip: Arc<Vec<u8>>,
        window: usize,
    }

    async fn test_server(state: TestState) -> String {
        async fn health(State(state): State<TestState>) -> impl IntoResponse {
            state.events.lock().unwrap().push("health".into());
            axum::Json(
                json!({"status":"healthy","protocol_version":2,"max_concurrent_requests":2,"processing_window_size":state.window}),
            )
        }
        async fn task(
            State(state): State<TestState>,
            headers: HeaderMap,
            body: Bytes,
        ) -> impl IntoResponse {
            let index = state.posts.fetch_add(1, Ordering::SeqCst) + 1;
            state
                .events
                .lock()
                .unwrap()
                .push(format!("task:{index}:{}", String::from_utf8_lossy(&body)));
            let base = format!("http://{}", headers.get("host").unwrap().to_str().unwrap());
            (
                StatusCode::ACCEPTED,
                axum::Json(
                    json!({"task_id":index.to_string(),"status_url":format!("{base}/status/{index}"),"result_url":format!("{base}/result/{index}")}),
                ),
            )
        }
        async fn status(AxumPath(_): AxumPath<usize>) -> axum::Json<Value> {
            axum::Json(json!({"status":"completed"}))
        }
        async fn result(
            State(state): State<TestState>,
            AxumPath(_): AxumPath<usize>,
        ) -> impl IntoResponse {
            ([("content-type", "application/zip")], (*state.zip).clone())
        }
        let app = Router::new()
            .route("/health", get(health))
            .route("/tasks", post(task))
            .route("/status/{id}", get(status))
            .route("/result/{id}", get(result))
            .with_state(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(axum::serve(listener, app).into_future());
        format!("http://{address}")
    }

    fn zip_bytes() -> Vec<u8> {
        zip_file("result.txt", b"ok")
    }

    fn zip_file(name: &str, bytes: &[u8]) -> Vec<u8> {
        let mut zip = ZipWriter::new(Cursor::new(Vec::new()));
        zip.start_file(
            name,
            SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored),
        )
        .unwrap();
        zip.write_all(bytes).unwrap();
        zip.finish().unwrap().into_inner()
    }
    fn preview_zips(artifacts: &[(&str, bool)]) -> Vec<u8> {
        let mut png = Vec::new();
        image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(1, 1, image::Rgb([255; 3])))
            .write_to(&mut Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();
        let mut zip = ZipWriter::new(Cursor::new(Vec::new()));
        for &(stem, malformed) in artifacts {
            let middle = if malformed {
                b"{}".as_slice()
            } else {
                br#"{"pdf_info":[{"page_idx":0,"page_size":[200,100],"preproc_blocks":[{"type":"text","bbox":[20,10,150,80]}],"discarded_blocks":[]}]}"#.as_slice()
            };
            for (name, bytes) in [
                (format!("{stem}/vlm/{stem}_middle.json"), middle),
                (format!("{stem}/vlm/{stem}_origin.png"), png.as_slice()),
            ] {
                zip.start_file(name, SimpleFileOptions::default()).unwrap();
                zip.write_all(bytes).unwrap();
            }
        }
        zip.finish().unwrap().into_inner()
    }

    fn preview_zip(stem: &str, malformed: bool) -> Vec<u8> {
        preview_zips(&[(stem, malformed)])
    }

    fn max(counter: &AtomicUsize, value: usize) {
        let mut seen = counter.load(Ordering::SeqCst);
        while value > seen {
            match counter.compare_exchange(seen, value, Ordering::SeqCst, Ordering::SeqCst) {
                Ok(_) => break,
                Err(current) => seen = current,
            }
        }
    }

    fn document(path: PathBuf, stem: &str, pages: usize, order: usize) -> InputDocument {
        InputDocument {
            path,
            suffix: "png".into(),
            stem: stem.into(),
            effective_pages: pages,
            order,
        }
    }

    fn env() -> RemoteEnv {
        RemoteEnv {
            max_concurrent_requests: 2,
            result_timeout_seconds: 2.,
            download_timeout_seconds: 2.,
        }
    }

    async fn assert_workers_draining(office: &OfficeWorkers, raster: &RasterWorkers) {
        assert!(matches!(
            office
                .convert("docx", Bytes::from_static(b"x"), Duration::from_secs(1))
                .await,
            Err(crate::OfficeConvertError::Draining)
        ));
        assert_eq!(
            raster.test_admission().await,
            Err("image preparation workers are draining".into())
        );
    }

    #[test]
    fn labels_are_exact() {
        let documents = vec![
            InputDocument {
                path: PathBuf::from("a"),
                suffix: "png".into(),
                stem: "one".into(),
                effective_pages: 1,
                order: 0,
            },
            InputDocument {
                path: PathBuf::from("b"),
                suffix: "png".into(),
                stem: "two".into(),
                effective_pages: 1,
                order: 1,
            },
        ];
        assert_eq!(
            task_label(&super::super::PlannedTask {
                index: 4,
                documents,
                total_pages: 2
            }),
            "task#4 [one, two]"
        );
    }

    #[tokio::test]
    async fn run_core_owned_drains_workers_after_returns() {
        let root = tempfile::tempdir().unwrap();
        let input = root.path().join("doc.png");
        std::fs::write(&input, b"x").unwrap();
        let state = TestState {
            events: Arc::new(Mutex::new(Vec::new())),
            posts: Arc::new(AtomicUsize::new(0)),
            zip: Arc::new(preview_zip("doc", false)),
            window: 1,
        };
        let base = test_server(state).await;
        let office = OfficeWorkers::with_executable("unused".into()).unwrap();
        let raster = RasterWorkers::default();
        let office_clone = office.clone();
        let raster_clone = raster.clone();
        assert!(
            run_core_owned(
                vec![document(input, "doc", 1, 0)],
                root.path(),
                &base,
                RemoteOptions::default(),
                env(),
                None,
                crate::OfficialPdfOptions::default(),
                office,
                raster,
            )
            .await
            .unwrap()
            .is_empty()
        );
        assert_workers_draining(&office_clone, &raster_clone).await;

        let office = OfficeWorkers::with_executable("unused".into()).unwrap();
        let raster = RasterWorkers::default();
        let office_clone = office.clone();
        let raster_clone = raster.clone();
        assert!(
            run_core_owned(
                vec![],
                root.path(),
                "",
                RemoteOptions::default(),
                env(),
                None,
                crate::OfficialPdfOptions::default(),
                office,
                raster,
            )
            .await
            .is_err()
        );
        assert_workers_draining(&office_clone, &raster_clone).await;
    }

    #[tokio::test]
    async fn runner_orders_gates_and_packs_pipeline_tasks() {
        let input = tempfile::tempdir().unwrap();
        let output = tempfile::tempdir().unwrap();
        let paths = ["a.png", "b.png", "c.png"].map(|name| {
            let path = input.path().join(name);
            std::fs::write(&path, b"x").unwrap();
            path
        });
        let events = Arc::new(Mutex::new(Vec::new()));
        let state = TestState {
            events: Arc::clone(&events),
            posts: Arc::new(AtomicUsize::new(0)),
            zip: Arc::new(preview_zip("doc", false)),
            window: 3,
        };
        let base = test_server(state.clone()).await;
        let docs = vec![
            document(paths[0].clone(), "a", 2, 0),
            document(paths[1].clone(), "b", 2, 1),
            document(paths[2].clone(), "c", 1, 2),
        ];
        let output_path = output.path().join("created");
        let events_for_discovery = Arc::clone(&events);
        let output_for_discovery = output_path.clone();
        let mut options = RemoteOptions {
            backend: super::super::Backend::Pipeline,
            server_url: None,
            ..Default::default()
        };
        assert!(
            run_remote_with_discovery(
                input.path(),
                &output_path,
                &base,
                options.clone(),
                env(),
                move |_, _| {
                    assert!(output_for_discovery.exists());
                    events_for_discovery
                        .lock()
                        .unwrap()
                        .push("discovery".into());
                    Ok(docs)
                }
            )
            .await
            .unwrap()
            .is_empty()
        );
        let event_log = Arc::clone(&events);
        let events = events.lock().unwrap().clone();
        assert_eq!(events[0], "discovery");
        assert_eq!(events[1], "health");
        assert_eq!(state.posts.load(Ordering::SeqCst), 2);
        let submitted = &events[2..];
        assert!(
            submitted
                .iter()
                .any(|event| event.contains("a.png") && event.contains("c.png"))
        );
        assert!(
            submitted
                .iter()
                .any(|event| event.contains("b.png") && !event.contains("a.png"))
        );

        options.backend = super::super::Backend::HybridEngine;
        let posts = Arc::new(AtomicUsize::new(0));
        let state = TestState {
            posts: Arc::clone(&posts),
            ..state
        };
        let base = test_server(state).await;
        options.server_url = Some("http://model.invalid/v1".into());
        let docs = ["a", "b", "c"]
            .iter()
            .enumerate()
            .map(|(i, stem)| document(paths[i].clone(), stem, 1, i))
            .collect();
        run_remote_with_discovery(
            input.path(),
            output.path(),
            &base,
            options,
            env(),
            move |_, _| Ok(docs),
        )
        .await
        .unwrap();
        assert_eq!(posts.load(Ordering::SeqCst), 3);
        assert!(
            event_log
                .lock()
                .unwrap()
                .iter()
                .any(|event| event.contains("http://model.invalid/v1"))
        );
    }

    #[tokio::test]
    async fn client_side_gate_runs_before_discovery_and_health() {
        let output = tempfile::tempdir().unwrap();
        let events = Arc::new(Mutex::new(Vec::new()));
        let state = TestState {
            events: Arc::clone(&events),
            posts: Arc::new(AtomicUsize::new(0)),
            zip: Arc::new(zip_bytes()),
            window: 1,
        };
        let base = test_server(state.clone()).await;
        let output_path = output.path().join("created");
        let options = RemoteOptions {
            client_side: true,
            server_url: None,
            ..Default::default()
        };
        assert_eq!(
            run_remote_with_discovery(
                Path::new("ignored"),
                &output_path,
                &base,
                options,
                env(),
                move |_, _| panic!("discovery must not run")
            )
            .await,
            Err("client-side output generation is unsupported".into())
        );
        assert!(events.lock().unwrap().is_empty());
        assert_eq!(state.posts.load(Ordering::SeqCst), 0);
        assert!(!output_path.exists());
    }

    #[tokio::test]
    async fn facade_rejects_before_all_side_effects() {
        let health = Arc::new(AtomicUsize::new(0));
        let posts = Arc::new(AtomicUsize::new(0));
        let app = Router::new()
            .route(
                "/health",
                get({
                    let health = health.clone();
                    move || {
                        let health = health.clone();
                        async move {
                            health.fetch_add(1, Ordering::SeqCst);
                            axum::Json(json!({}))
                        }
                    }
                }),
            )
            .route(
                "/tasks",
                post({
                    let posts = posts.clone();
                    move || {
                        let posts = posts.clone();
                        async move {
                            posts.fetch_add(1, Ordering::SeqCst);
                            StatusCode::OK
                        }
                    }
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(axum::serve(listener, app).into_future());
        let root = tempfile::tempdir().unwrap();
        let input = root.path().join("doc.png");
        std::fs::write(&input, b"x").unwrap();
        let output = root.path().join("missing");
        let events = Arc::new(Mutex::new(Vec::new()));
        let callback: ProgressCallback = {
            let events = events.clone();
            Arc::new(move |event| events.lock().unwrap().push(event))
        };
        let doc = super::super::RemoteApiDocument {
            path: input.clone(),
            kind: crate::DocumentKind::Png,
            stem: "doc".into(),
            effective_pages: 1,
            order: 0,
        };
        let mut options = super::super::RemoteApiOptions::default();
        options.backend = "pipeline".into();
        assert_eq!(
            run_documents(
                vec![doc.clone()],
                &output,
                &base,
                options,
                super::super::RemoteApiEnv {
                    max_concurrent_requests: 1,
                    result_timeout_seconds: 1.,
                    download_timeout_seconds: 1.
                },
                Some(callback.clone())
            )
            .await,
            Err("unsupported backend: pipeline".into())
        );
        let mut options = super::super::RemoteApiOptions::default();
        options.client_side_output_generation = true;
        assert_eq!(
            run_documents(
                vec![doc.clone()],
                &output,
                &base,
                options,
                super::super::RemoteApiEnv {
                    max_concurrent_requests: 1,
                    result_timeout_seconds: 1.,
                    download_timeout_seconds: 1.
                },
                Some(callback.clone())
            )
            .await,
            Err("client-side output generation is unsupported".into())
        );
        assert!(
            run_documents(
                vec![doc.clone()],
                &output,
                "not a url",
                super::super::RemoteApiOptions::default(),
                super::super::RemoteApiEnv {
                    max_concurrent_requests: 1,
                    result_timeout_seconds: 1.,
                    download_timeout_seconds: 1.
                },
                Some(callback.clone())
            )
            .await
            .is_err()
        );
        let mut bad = doc;
        bad.kind = crate::DocumentKind::Pdf;
        assert_eq!(
            run_documents(
                vec![bad],
                &output,
                &base,
                super::super::RemoteApiOptions::default(),
                super::super::RemoteApiEnv {
                    max_concurrent_requests: 1,
                    result_timeout_seconds: 1.,
                    download_timeout_seconds: 1.
                },
                Some(callback)
            )
            .await,
            Err("invalid document record".into())
        );
        assert!(!output.exists());
        assert_eq!(health.load(Ordering::SeqCst), 0);
        assert_eq!(posts.load(Ordering::SeqCst), 0);
        assert!(events.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn facade_events_deduplicate_active_snapshots_and_ignore_callback_panics() {
        #[derive(Clone)]
        struct EventState {
            status: Arc<AtomicUsize>,
            zip: Arc<Mutex<Vec<u8>>>,
        }
        async fn health() -> axum::Json<Value> {
            axum::Json(
                json!({"status":"healthy","protocol_version":2,"max_concurrent_requests":1,"processing_window_size":1}),
            )
        }
        async fn task(headers: HeaderMap, _body: Bytes) -> impl IntoResponse {
            let base = format!("http://{}", headers["host"].to_str().unwrap());
            (
                StatusCode::ACCEPTED,
                axum::Json(
                    json!({"task_id":"1","status_url":format!("{base}/status"),"result_url":format!("{base}/result")}),
                ),
            )
        }
        async fn status(State(state): State<EventState>) -> axum::Json<Value> {
            let n = state.status.fetch_add(1, Ordering::SeqCst);
            axum::Json(match n {
                0 | 1 => json!({"status":"pending","queued_ahead":2}),
                2 => json!({"status":"pending","queued_ahead":1}),
                3 | 4 => json!({"status":"processing"}),
                _ => json!({"status":"completed"}),
            })
        }
        async fn result(State(state): State<EventState>) -> impl IntoResponse {
            (
                [("content-type", "application/zip")],
                state.zip.lock().unwrap().clone(),
            )
        }
        let state = EventState {
            status: Arc::new(AtomicUsize::new(0)),
            zip: Arc::new(Mutex::new(preview_zip("doc", false))),
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(
            axum::serve(
                listener,
                Router::new()
                    .route("/health", get(health))
                    .route("/tasks", post(task))
                    .route("/status", get(status))
                    .route("/result", get(result))
                    .with_state(state.clone()),
            )
            .into_future(),
        );
        let root = tempfile::tempdir().unwrap();
        let input = root.path().join("doc.png");
        std::fs::write(&input, b"x").unwrap();
        let output = root.path().join("out");
        let events = Arc::new(Mutex::new(Vec::new()));
        let callback: ProgressCallback = {
            let events = events.clone();
            Arc::new(move |event| events.lock().unwrap().push(event))
        };
        let doc = super::super::RemoteApiDocument {
            path: input,
            kind: crate::DocumentKind::Png,
            stem: "doc".into(),
            effective_pages: 1,
            order: 0,
        };
        let failures = run_documents(
            vec![doc],
            &output,
            &base,
            super::super::RemoteApiOptions::default(),
            super::super::RemoteApiEnv {
                max_concurrent_requests: 1,
                result_timeout_seconds: 10.,
                download_timeout_seconds: 10.,
            },
            Some(callback),
        )
        .await
        .unwrap();
        assert!(failures.is_empty(), "{failures:?}");
        assert_eq!(
            lopdf::Document::load(output.join("doc/vlm/doc_layout.pdf"))
                .unwrap()
                .get_pages()
                .len(),
            1
        );
        assert_eq!(
            *events.lock().unwrap(),
            vec![
                ProgressEvent::ApiSubmitted {
                    label: "task#1 [doc]".into()
                },
                ProgressEvent::ApiPending {
                    label: "task#1 [doc]".into(),
                    queued_ahead: Some(2)
                },
                ProgressEvent::ApiPending {
                    label: "task#1 [doc]".into(),
                    queued_ahead: Some(1)
                },
                ProgressEvent::ApiProcessing {
                    label: "task#1 [doc]".into()
                },
                ProgressEvent::ApiDownloading {
                    label: "task#1 [doc]".into()
                },
                ProgressEvent::ApiExtracting {
                    label: "task#1 [doc]".into()
                },
                ProgressEvent::ApiCompleted {
                    label: "task#1 [doc]".into()
                }
            ]
        );
        *state.zip.lock().unwrap() = preview_zip("panic", false);
        let input = root.path().join("panic.png");
        std::fs::write(&input, b"x").unwrap();
        assert!(
            run_documents(
                vec![super::super::RemoteApiDocument {
                    path: input,
                    kind: crate::DocumentKind::Png,
                    stem: "panic".into(),
                    effective_pages: 1,
                    order: 0
                }],
                &root.path().join("panic-out"),
                &base,
                super::super::RemoteApiOptions::default(),
                super::super::RemoteApiEnv {
                    max_concurrent_requests: 1,
                    result_timeout_seconds: 10.,
                    download_timeout_seconds: 10.
                },
                Some(Arc::new(|_| panic!("event")))
            )
            .await
            .unwrap()
            .is_empty()
        );
        assert!(
            root.path()
                .join("panic-out/panic/vlm/panic_layout.pdf")
                .is_file()
        );
    }

    #[tokio::test]
    async fn facade_warns_for_malformed_preview_and_continues() {
        let root = tempfile::tempdir().unwrap();
        let input = root.path().join("input");
        std::fs::create_dir(&input).unwrap();
        let output = root.path().join("out");
        let paths = ["bad", "good"].map(|stem| {
            let path = input.join(format!("{stem}.png"));
            std::fs::write(&path, b"x").unwrap();
            path
        });
        let state = TestState {
            events: Arc::new(Mutex::new(Vec::new())),
            posts: Arc::new(AtomicUsize::new(0)),
            zip: Arc::new(preview_zips(&[("bad", true), ("good", false)])),
            window: 1,
        };
        let base = test_server(state).await;
        let events = Arc::new(Mutex::new(Vec::new()));
        let callback: ProgressCallback = {
            let events = Arc::clone(&events);
            Arc::new(move |event| events.lock().unwrap().push(event))
        };
        let failures = run_documents(
            paths
                .into_iter()
                .zip(["bad", "good"])
                .enumerate()
                .map(|(order, (path, stem))| super::super::RemoteApiDocument {
                    path,
                    kind: crate::DocumentKind::Png,
                    stem: stem.into(),
                    effective_pages: 1,
                    order,
                })
                .collect(),
            &output,
            &base,
            super::super::RemoteApiOptions::default(),
            super::super::RemoteApiEnv {
                max_concurrent_requests: 1,
                result_timeout_seconds: 2.,
                download_timeout_seconds: 2.,
            },
            Some(callback),
        )
        .await
        .unwrap();
        assert!(failures.is_empty(), "{failures:?}");
        assert!(output.join("bad/vlm/bad_middle.json").is_file());
        assert!(output.join("bad/vlm/bad_origin.png").is_file());
        assert!(!output.join("bad/vlm/bad_layout.pdf").exists());
        assert_eq!(
            lopdf::Document::load(output.join("good/vlm/good_layout.pdf"))
                .unwrap()
                .get_pages()
                .len(),
            1
        );
        let events = events.lock().unwrap();
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, ProgressEvent::ApiFailed { .. }))
        );
        let warning = ProgressEvent::ApiWarning {
            label: "task#1 [bad]".into(),
            message: "invalid preview middle JSON".into(),
        };
        assert_eq!(events.iter().filter(|event| **event == warning).count(), 1);
        let warning_index = events.iter().position(|event| *event == warning).unwrap();
        assert_eq!(
            events.get(warning_index + 1),
            Some(&ProgressEvent::ApiCompleted {
                label: "task#1 [bad]".into(),
            })
        );
        assert!(events.contains(&ProgressEvent::ApiCompleted {
            label: "task#2 [good]".into(),
        }));
        assert!(!events.iter().any(|event| matches!(event, ProgressEvent::ApiWarning { label, .. } if label == "task#2 [good]")));
    }

    #[tokio::test]
    async fn runner_global_failures_short_circuit_before_submission() {
        let output = tempfile::tempdir().unwrap();
        let health = Arc::new(AtomicUsize::new(0));
        let posts = Arc::new(AtomicUsize::new(0));
        let app = Router::new()
            .route("/health", get({ let health = Arc::clone(&health); move || { let health = Arc::clone(&health); async move { health.fetch_add(1, Ordering::SeqCst); axum::Json(json!({"status":"healthy","protocol_version":2,"max_concurrent_requests":1,"processing_window_size":1})) } } }))
            .route("/tasks", post({ let posts = Arc::clone(&posts); move || { let posts = Arc::clone(&posts); async move { posts.fetch_add(1, Ordering::SeqCst); StatusCode::ACCEPTED } } }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(axum::serve(listener, app).into_future());
        let options = RemoteOptions {
            server_url: Some(base.clone()),
            ..Default::default()
        };
        let discovery = Arc::new(AtomicUsize::new(0));
        let file = output.path().join("file");
        std::fs::write(&file, b"x").unwrap();
        let seen = Arc::clone(&discovery);
        assert!(
            run_remote_with_discovery(
                Path::new("x"),
                &file,
                &base,
                options.clone(),
                env(),
                move |_, _| {
                    seen.fetch_add(1, Ordering::SeqCst);
                    Ok(vec![])
                }
            )
            .await
            .is_err()
        );
        assert_eq!(
            (
                discovery.load(Ordering::SeqCst),
                health.load(Ordering::SeqCst)
            ),
            (0, 0)
        );
        assert!(
            run_remote_with_discovery(
                Path::new("x"),
                output.path(),
                &base,
                options.clone(),
                env(),
                |_, _| Err("discovery failed".into())
            )
            .await
            .is_err()
        );
        assert_eq!(health.load(Ordering::SeqCst), 0);
        for (status, body) in [
            (StatusCode::INTERNAL_SERVER_ERROR, "bad".to_owned()),
            (StatusCode::OK, json!({"status":"bad"}).to_string()),
        ] {
            let posts = Arc::new(AtomicUsize::new(0));
            let app = Router::new()
                .route(
                    "/health",
                    get(move || {
                        let body = body.clone();
                        async move { (status, body) }
                    }),
                )
                .route(
                    "/tasks",
                    post({
                        let posts = Arc::clone(&posts);
                        move || {
                            let posts = Arc::clone(&posts);
                            async move {
                                posts.fetch_add(1, Ordering::SeqCst);
                                StatusCode::ACCEPTED
                            }
                        }
                    }),
                );
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let base = format!("http://{}", listener.local_addr().unwrap());
            tokio::spawn(axum::serve(listener, app).into_future());
            assert!(
                run_remote_with_discovery(
                    Path::new("x"),
                    output.path(),
                    &base,
                    RemoteOptions {
                        server_url: None,
                        ..Default::default()
                    },
                    env(),
                    |_, _| Ok(vec![document(PathBuf::from("x"), "x", 1, 0)])
                )
                .await
                .is_err()
            );
            assert_eq!(posts.load(Ordering::SeqCst), 0);
        }
    }

    #[tokio::test]
    async fn runner_uses_fixed_waves_and_extracts_in_task_order() {
        #[derive(Clone)]
        struct WaveState {
            active: Arc<AtomicUsize>,
            peak: Arc<AtomicUsize>,
            completed: Arc<Mutex<Vec<usize>>>,
            posts: Arc<AtomicUsize>,
            first: Arc<Notify>,
            results: Arc<Barrier>,
            output: PathBuf,
        }
        async fn health() -> axum::Json<Value> {
            axum::Json(
                json!({"status":"healthy","protocol_version":2,"max_concurrent_requests":2,"processing_window_size":1}),
            )
        }
        async fn task(
            State(state): State<WaveState>,
            headers: HeaderMap,
            body: Bytes,
        ) -> impl IntoResponse {
            state.posts.fetch_add(1, Ordering::SeqCst);
            let id = (1..=4)
                .find(|id| {
                    body.windows(6)
                        .any(|part| part == format!("d{id}.png").as_bytes())
                })
                .unwrap();
            if id == 3 {
                assert_eq!(state.completed.lock().unwrap().as_slice(), &[2, 1]);
                assert_eq!(std::fs::read(state.output.join("one")).unwrap(), b"one");
                assert_eq!(std::fs::read(state.output.join("two")).unwrap(), b"two");
                assert_eq!(std::fs::read(state.output.join("shared")).unwrap(), b"two");
            }
            let base = format!("http://{}", headers["host"].to_str().unwrap());
            (
                StatusCode::ACCEPTED,
                axum::Json(
                    json!({"task_id":id.to_string(),"status_url":format!("{base}/status/{id}"),"result_url":format!("{base}/result/{id}")}),
                ),
            )
        }
        async fn status(
            State(state): State<WaveState>,
            AxumPath(id): AxumPath<usize>,
        ) -> axum::Json<Value> {
            let now = state.active.fetch_add(1, Ordering::SeqCst) + 1;
            max(&state.peak, now);
            state.active.fetch_sub(1, Ordering::SeqCst);
            axum::Json(json!({"status":"completed","id":id}))
        }
        async fn result(
            State(state): State<WaveState>,
            AxumPath(id): AxumPath<usize>,
        ) -> impl IntoResponse {
            let now = state.active.fetch_add(1, Ordering::SeqCst) + 1;
            max(&state.peak, now);
            if id <= 2 {
                tokio::time::timeout(Duration::from_secs(1), state.results.wait())
                    .await
                    .unwrap();
            }
            if id == 1 {
                tokio::time::timeout(Duration::from_secs(1), state.first.notified())
                    .await
                    .unwrap();
            } else if id == 2 {
                state.completed.lock().unwrap().push(2);
                state.first.notify_one();
            }
            state.active.fetch_sub(1, Ordering::SeqCst);
            if id == 1 {
                state.completed.lock().unwrap().push(1);
            }
            let zip = match id {
                1 => {
                    let mut z = ZipWriter::new(Cursor::new(Vec::new()));
                    z.start_file("one", SimpleFileOptions::default()).unwrap();
                    z.write_all(b"one").unwrap();
                    z.start_file("shared", SimpleFileOptions::default())
                        .unwrap();
                    z.write_all(b"one").unwrap();
                    z.finish().unwrap().into_inner()
                }
                2 => {
                    let mut z = ZipWriter::new(Cursor::new(Vec::new()));
                    z.start_file("two", SimpleFileOptions::default()).unwrap();
                    z.write_all(b"two").unwrap();
                    z.start_file("shared", SimpleFileOptions::default())
                        .unwrap();
                    z.write_all(b"two").unwrap();
                    z.finish().unwrap().into_inner()
                }
                _ => zip_file(&format!("later-{id}"), b"later"),
            };
            ([("content-type", "application/zip")], zip)
        }
        let output = tempfile::tempdir().unwrap();
        let state = WaveState {
            active: Arc::new(AtomicUsize::new(0)),
            peak: Arc::new(AtomicUsize::new(0)),
            completed: Arc::new(Mutex::new(Vec::new())),
            posts: Arc::new(AtomicUsize::new(0)),
            first: Arc::new(Notify::new()),
            results: Arc::new(Barrier::new(2)),
            output: output.path().to_owned(),
        };
        let app = Router::new()
            .route("/health", get(health))
            .route("/tasks", post(task))
            .route("/status/{id}", get(status))
            .route("/result/{id}", get(result))
            .with_state(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(axum::serve(listener, app).into_future());
        let input = tempfile::tempdir().unwrap();
        let docs = (1..=4)
            .map(|n| {
                let p = input.path().join(format!("{n}.png"));
                std::fs::write(&p, b"x").unwrap();
                document(p, &format!("d{n}"), 1, n)
            })
            .collect();
        assert!(
            tokio::time::timeout(
                Duration::from_secs(3),
                run_remote_with_discovery(
                    input.path(),
                    output.path(),
                    &base,
                    RemoteOptions {
                        backend: super::super::Backend::HybridEngine,
                        server_url: None,
                        ..Default::default()
                    },
                    env(),
                    move |_, _| Ok(docs)
                )
            )
            .await
            .unwrap()
            .unwrap()
            .is_empty()
        );
        assert_eq!(state.peak.load(Ordering::SeqCst), 2);
        assert_eq!(state.completed.lock().unwrap().as_slice(), &[2, 1]);
        assert_eq!(state.posts.load(Ordering::SeqCst), 4);
    }

    #[tokio::test]
    async fn runner_keeps_sibling_output_when_later_colliding_zip_is_corrupt() {
        #[derive(Clone)]
        struct FailureState {
            posts: Arc<AtomicUsize>,
            zips: Arc<(Vec<u8>, Vec<u8>)>,
        }
        let good = zip_file("sentinel", b"good");
        let mut bad = zip_file("sentinel", b"bad");
        let p = bad.windows(3).position(|v| v == b"bad").unwrap();
        bad[p] ^= 1;
        async fn health() -> axum::Json<Value> {
            axum::Json(
                json!({"status":"healthy","protocol_version":2,"max_concurrent_requests":3,"processing_window_size":1}),
            )
        }
        async fn task(
            headers: HeaderMap,
            State(state): State<FailureState>,
            body: Bytes,
        ) -> impl IntoResponse {
            state.posts.fetch_add(1, Ordering::SeqCst);
            let id = (1..=3)
                .find(|id| {
                    body.windows(9)
                        .any(|part| part == format!("stem{id}.png").as_bytes())
                })
                .unwrap();
            if id == 2 {
                return (StatusCode::BAD_REQUEST, axum::Json(json!({}))).into_response();
            }
            let base = format!("http://{}", headers["host"].to_str().unwrap());
            (StatusCode::ACCEPTED, axum::Json(json!({"task_id":id.to_string(),"status_url":format!("{base}/status/{id}"),"result_url":format!("{base}/result/{id}")}))).into_response()
        }
        async fn status() -> axum::Json<Value> {
            axum::Json(json!({"status":"completed"}))
        }
        async fn result(
            AxumPath(id): AxumPath<usize>,
            State(state): State<FailureState>,
        ) -> impl IntoResponse {
            (
                [("content-type", "application/zip")],
                if id == 1 {
                    state.zips.0.clone()
                } else {
                    state.zips.1.clone()
                },
            )
        }
        let state = FailureState {
            posts: Arc::new(AtomicUsize::new(0)),
            zips: Arc::new((good, bad)),
        };
        let app = Router::new()
            .route("/health", get(health))
            .route("/tasks", post(task))
            .route("/status/{id}", get(status))
            .route("/result/{id}", get(result))
            .with_state(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(axum::serve(listener, app).into_future());
        let input = tempfile::tempdir().unwrap();
        let output = tempfile::tempdir().unwrap();
        let docs = (1..=3)
            .map(|n| {
                let p = input.path().join(format!("{n}.png"));
                std::fs::write(&p, b"x").unwrap();
                document(p, &format!("stem{n}"), 1, n)
            })
            .collect();
        let failures = run_remote_with_discovery(
            input.path(),
            output.path(),
            &base,
            RemoteOptions {
                backend: super::super::Backend::HybridEngine,
                server_url: None,
                ..Default::default()
            },
            RemoteEnv {
                max_concurrent_requests: 3,
                ..env()
            },
            move |_, _| Ok(docs),
        )
        .await
        .unwrap();
        assert_eq!(state.posts.load(Ordering::SeqCst), 3);
        assert_eq!(
            std::fs::read(output.path().join("sentinel")).unwrap(),
            b"good"
        );
        assert_eq!(
            failures
                .iter()
                .map(|f| (f.task_index, f.document_stems.clone()))
                .collect::<Vec<_>>(),
            vec![(2, vec!["stem2".into()]), (3, vec!["stem3".into()])]
        );
        assert!(failures[0].message.contains("task submission HTTP 400"));
        assert_eq!(failures[1].message, "invalid result archive");
    }
}
