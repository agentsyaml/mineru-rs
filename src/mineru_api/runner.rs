use super::{
    InputDocument, RemoteEnv, RemoteOptions, TaskFailure,
    archive::{self, DownloadedZip},
    http::MineruApiClient,
    planning,
};
use crate::{OfficeWorkers, RasterWorkers};
use crate::{ProgressCallback, ProgressEvent};
use futures_util::future::join_all;
use std::{path::Path, sync::Arc};

pub(super) async fn run_documents_scoped_with_workers(
    documents: Vec<super::RemoteApiDocument>,
    output: &Path,
    api_url: &str,
    options: super::RemoteApiOptions,
    env: super::RemoteApiEnv,
    events: Option<crate::command::CommandCallback>,
    office: OfficeWorkers,
    policy: crate::DocumentLimitPolicy,
    response_cap: usize,
    service: crate::command::service::ResolvedService,
) -> Result<Vec<super::RemoteApiFailure>, String> {
    run_documents_impl(
        documents,
        output,
        api_url,
        options,
        env,
        None,
        events,
        office,
        policy,
        response_cap,
        service,
    )
    .await
}

async fn run_documents_impl(
    documents: Vec<super::RemoteApiDocument>,
    output: &Path,
    api_url: &str,
    options: super::RemoteApiOptions,
    env: super::RemoteApiEnv,
    events: Option<ProgressCallback>,
    command_events: Option<crate::command::CommandCallback>,
    office: OfficeWorkers,
    policy: crate::DocumentLimitPolicy,
    response_cap: usize,
    service: crate::command::service::ResolvedService,
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
        api_key: options.api_key.clone(),
        start: options.start,
        end: options.end,
        client_side: false,
        max_input_bytes: policy.max_input_bytes,
        archive_limits: service.archive,
    };
    archive::preflight_output_root(output)?;
    let raster = RasterWorkers::default();
    run_core_owned(
        documents,
        output,
        &api_url,
        options,
        env.into(),
        events,
        command_events,
        route,
        office,
        raster,
        response_cap,
        service,
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
    command_events: Option<crate::command::CommandCallback>,
    route: crate::OfficialPdfOptions,
    office: OfficeWorkers,
    raster: RasterWorkers,
    response_cap: usize,
    service: crate::command::service::ResolvedService,
) -> Result<Vec<TaskFailure>, String> {
    let result = run_core(
        documents,
        output,
        api_url,
        options,
        env,
        events,
        command_events,
        Some((&route, response_cap, &office, &raster)),
        service,
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

async fn run_core(
    documents: Vec<InputDocument>,
    output: &Path,
    api_url: &str,
    options: RemoteOptions,
    env: RemoteEnv,
    events: Option<ProgressCallback>,
    command_events: Option<crate::command::CommandCallback>,
    preview: Option<(
        &crate::OfficialPdfOptions,
        usize,
        &OfficeWorkers,
        &RasterWorkers,
    )>,
    service: crate::command::service::ResolvedService,
) -> Result<Vec<TaskFailure>, String> {
    archive::preflight_output_root(output)?;
    let client = Arc::new(MineruApiClient::new_with_transport(
        api_url,
        service.api_connect_timeout,
        service.api_acquisition_timeout,
        service.api_send_timeout,
        service.api_poll_interval,
        options.api_key.clone(),
    )?);
    let health = client.health().await?;
    let tasks = planning::plan_tasks(options.backend, &documents, health.processing_window_size)?;
    let concurrency = planning::effective_concurrency(
        env.max_concurrent_requests,
        health.max_concurrent_requests,
        tasks.len(),
    )?;
    crate::command::emit_command(
        &command_events,
        crate::command::CommandEvent::RunPlanned {
            documents: 0,
            api_tasks: tasks.len(),
        },
    );
    drop(super::request_form(&options));

    let output = output.to_path_buf();
    let mut failures = Vec::new();
    for wave in tasks.chunks(concurrency) {
        let staged = join_all(wave.iter().cloned().map(|task| {
            let client = Arc::clone(&client);
            let options = options.clone();
            let task_events = command_events
                .as_ref()
                .map(|events| {
                    crate::command::scoped_progress(
                        Some(Arc::clone(events)),
                        crate::command::CommandScope::ApiTask(crate::command::ApiTaskId(
                            task.index,
                        )),
                    )
                })
                .or_else(|| events.clone());
            async move {
                let result = stage(client, &options, env, task.clone(), task_events.clone()).await;
                (task, task_events, result)
            }
        }))
        .await;
        for (task, task_events, result) in staged {
            match result {
                Ok(zip) => {
                    crate::progress_events::emit(
                        &task_events,
                        ProgressEvent::ApiExtracting {
                            label: task_label(&task),
                        },
                    );
                    let destination = output.clone();
                    let index = task.index;
                    let stems = stems(&task.documents);
                    let extracted = tokio::task::spawn_blocking(move || {
                        zip.extract(&destination, options.archive_limits)
                    })
                    .await
                    .unwrap_or_else(|_| Err("internal archive extraction task failed".into()));
                    if let Err(message) = extracted {
                        crate::progress_events::emit(
                            &task_events,
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
                        if let Some((route, response_cap, office, raster)) = preview {
                            for document in &task.documents {
                                let kind = crate::DocumentKind::from_suffix(&document.suffix)
                                    .ok_or_else(|| {
                                        format!("unsupported document kind: {}", document.suffix)
                                    })?;
                                if let Err(message) =
                                crate::mineru_api::remote_preview::prepare_and_publish_downloaded(
                                    &output,
                                    &document.stem,
                                    kind,
                                    route,
                                    office,
                                    raster,
                                    task_events.clone(),
                                    response_cap,
                                    service.ooxml,
                                )
                                .await
                            {
                                crate::progress_events::emit(
                                    &task_events,
                                    ProgressEvent::ApiWarning {
                                        label: task_label(&task),
                                        message,
                                    },
                                );
                            }
                            }
                        }
                        crate::progress_events::emit(
                            &task_events,
                            ProgressEvent::ApiCompleted {
                                label: task_label(&task),
                            },
                        );
                    }
                }
                Err(message) => {
                    crate::progress_events::emit(
                        &task_events,
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
            options.archive_limits,
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
        path::PathBuf,
        sync::{
            Mutex,
            atomic::{AtomicUsize, Ordering},
        },
        time::Duration,
    };
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

    fn service() -> crate::command::service::ResolvedService {
        crate::command::service::resolve_service(
            &(|_| None),
            &crate::command::service::ServiceOverrides::default(),
            crate::DocumentLimitPolicy::defaults(),
        )
        .unwrap()
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
        let office = OfficeWorkers::with_executable("unused".into());
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
                None,
                crate::OfficialPdfOptions::default(),
                office,
                raster,
                10 * 1024 * 1024,
                service(),
            )
            .await
            .unwrap()
            .is_empty()
        );
        assert_workers_draining(&office_clone, &raster_clone).await;

        let office = OfficeWorkers::with_executable("unused".into());
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
                None,
                crate::OfficialPdfOptions::default(),
                office,
                raster,
                10 * 1024 * 1024,
                service(),
            )
            .await
            .is_err()
        );
        assert_workers_draining(&office_clone, &raster_clone).await;
    }

    #[tokio::test]
    async fn scoped_facade_keeps_two_task_ids_through_preview_warning_and_terminal() {
        use crate::command::{ApiTaskId, CommandEvent, CommandScope};

        let root = tempfile::tempdir().unwrap();
        let paths = ["bad", "good"].map(|stem| {
            let path = root.path().join(format!("{stem}.png"));
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
        let commands = Arc::new(Mutex::new(Vec::new()));
        let callback = {
            let commands = Arc::clone(&commands);
            Arc::new(move |event| commands.lock().unwrap().push(event))
                as crate::command::CommandCallback
        };
        let failures = run_documents_scoped_with_workers(
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
            &root.path().join("out"),
            &base,
            super::super::RemoteApiOptions::default(),
            super::super::RemoteApiEnv {
                max_concurrent_requests: 2,
                result_timeout_seconds: 2.,
                download_timeout_seconds: 2.,
            },
            Some(callback),
            OfficeWorkers::with_executable(std::env::current_exe().unwrap()),
            crate::DocumentLimitPolicy::defaults(),
            10 * 1024 * 1024,
            service(),
        )
        .await
        .unwrap();
        assert!(failures.is_empty());
        let commands = commands.lock().unwrap();
        assert!(matches!(
            commands[0],
            CommandEvent::RunPlanned {
                documents: 0,
                api_tasks: 2
            }
        ));
        for (id, label) in [(1, "task#1 [bad]"), (2, "task#2 [good]")] {
            let scope = CommandScope::ApiTask(ApiTaskId(id));
            assert!(commands.iter().any(|event| matches!(event, CommandEvent::Progress { scope: seen, event: ProgressEvent::ApiSubmitted { label: seen_label } } if *seen == scope && seen_label == label)));
            assert!(commands.iter().any(|event| matches!(event, CommandEvent::Progress { scope: seen, event: ProgressEvent::ApiCompleted { label: seen_label } } if *seen == scope && seen_label == label)));
        }
        assert!(commands.iter().any(|event| matches!(event, CommandEvent::Progress { scope: CommandScope::ApiTask(ApiTaskId(1)), event: ProgressEvent::ApiWarning { message, .. } } if message == "invalid preview middle JSON")));
        drop(commands);

        let state = TestState {
            events: Arc::new(Mutex::new(Vec::new())),
            posts: Arc::new(AtomicUsize::new(0)),
            zip: Arc::new(b"not a zip".to_vec()),
            window: 1,
        };
        let base = test_server(state).await;
        let commands = Arc::new(Mutex::new(Vec::new()));
        let callback = {
            let commands = Arc::clone(&commands);
            Arc::new(move |event| commands.lock().unwrap().push(event))
                as crate::command::CommandCallback
        };
        let failures = run_documents_scoped_with_workers(
            ["bad", "good"]
                .into_iter()
                .enumerate()
                .map(|(order, stem)| super::super::RemoteApiDocument {
                    path: root.path().join(format!("{stem}.png")),
                    kind: crate::DocumentKind::Png,
                    stem: stem.into(),
                    effective_pages: 1,
                    order,
                })
                .collect(),
            &root.path().join("failed-out"),
            &base,
            super::super::RemoteApiOptions::default(),
            super::super::RemoteApiEnv {
                max_concurrent_requests: 2,
                result_timeout_seconds: 2.,
                download_timeout_seconds: 2.,
            },
            Some(callback),
            OfficeWorkers::with_executable(std::env::current_exe().unwrap()),
            crate::DocumentLimitPolicy::defaults(),
            10 * 1024 * 1024,
            service(),
        )
        .await
        .unwrap();
        assert_eq!(failures.len(), 2);
        let commands = commands.lock().unwrap();
        for id in [1, 2] {
            assert!(commands.iter().any(|event| matches!(event, CommandEvent::Progress { scope: CommandScope::ApiTask(ApiTaskId(seen)), event: ProgressEvent::ApiFailed { .. } } if *seen == id)));
        }
    }

    #[tokio::test]
    async fn scoped_events_deduplicate_snapshots_and_survive_callback_panic() {
        use crate::command::{ApiTaskId, CommandEvent, CommandScope};

        #[derive(Clone)]
        struct EventState {
            status: Arc<AtomicUsize>,
            zip: Arc<Vec<u8>>,
        }
        async fn health() -> axum::Json<Value> {
            axum::Json(
                json!({"status":"healthy","protocol_version":2,"max_concurrent_requests":1,"processing_window_size":1}),
            )
        }
        async fn task(headers: HeaderMap, body: Bytes) -> impl IntoResponse {
            assert!(
                body.windows(b"filename=\"doc.png\"".len())
                    .any(|part| part == b"filename=\"doc.png\"")
            );
            let base = format!("http://{}", headers["host"].to_str().unwrap());
            (
                StatusCode::ACCEPTED,
                axum::Json(
                    json!({"task_id":"1","status_url":format!("{base}/status"),"result_url":format!("{base}/result")}),
                ),
            )
        }
        async fn status(State(state): State<EventState>) -> axum::Json<Value> {
            axum::Json(match state.status.fetch_add(1, Ordering::SeqCst) {
                0 | 1 => json!({"status":"pending","queued_ahead":2}),
                2 => json!({"status":"processing"}),
                _ => json!({"status":"completed"}),
            })
        }
        async fn result(State(state): State<EventState>) -> impl IntoResponse {
            ([("content-type", "application/zip")], (*state.zip).clone())
        }

        let state = EventState {
            status: Arc::new(AtomicUsize::new(0)),
            zip: Arc::new(preview_zip("doc", false)),
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
        let output = root.path().join("out");
        std::fs::write(&input, b"x").unwrap();
        let commands = Arc::new(Mutex::new(Vec::new()));
        let callback = {
            let commands = Arc::clone(&commands);
            Arc::new(move |event| {
                let panic = matches!(
                    &event,
                    CommandEvent::Progress {
                        event: ProgressEvent::ApiPending { .. },
                        ..
                    }
                );
                commands.lock().unwrap().push(event);
                if panic {
                    panic!("event callback");
                }
            }) as crate::command::CommandCallback
        };

        let failures = run_documents_scoped_with_workers(
            vec![super::super::RemoteApiDocument {
                path: input,
                kind: crate::DocumentKind::Png,
                stem: "doc".into(),
                effective_pages: 1,
                order: 0,
            }],
            &output,
            &base,
            super::super::RemoteApiOptions::default(),
            super::super::RemoteApiEnv {
                max_concurrent_requests: 1,
                result_timeout_seconds: 10.,
                download_timeout_seconds: 10.,
            },
            Some(callback),
            OfficeWorkers::with_executable(std::env::current_exe().unwrap()),
            crate::DocumentLimitPolicy::defaults(),
            10 * 1024 * 1024,
            service(),
        )
        .await
        .unwrap();

        assert!(failures.is_empty(), "{failures:?}");
        assert_eq!(state.status.load(Ordering::SeqCst), 4);
        assert!(output.join("doc/vlm/doc_layout.pdf").is_file());
        let progress = commands
            .lock()
            .unwrap()
            .iter()
            .filter_map(|event| match event {
                CommandEvent::Progress {
                    scope: CommandScope::ApiTask(ApiTaskId(1)),
                    event,
                } => Some(event.clone()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            progress,
            vec![
                ProgressEvent::ApiSubmitted {
                    label: "task#1 [doc]".into(),
                },
                ProgressEvent::ApiPending {
                    label: "task#1 [doc]".into(),
                    queued_ahead: Some(2),
                },
                ProgressEvent::ApiProcessing {
                    label: "task#1 [doc]".into(),
                },
                ProgressEvent::ApiDownloading {
                    label: "task#1 [doc]".into(),
                },
                ProgressEvent::ApiExtracting {
                    label: "task#1 [doc]".into(),
                },
                ProgressEvent::ApiCompleted {
                    label: "task#1 [doc]".into(),
                },
            ]
        );
    }

    #[tokio::test]
    async fn runner_uses_fixed_waves_and_publishes_in_task_order() {
        #[derive(Clone)]
        struct WaveState {
            completed: Arc<Mutex<Vec<usize>>>,
            published: Arc<Mutex<Vec<String>>>,
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
            let id = (1..=3)
                .find(|id| {
                    body.windows(6)
                        .any(|part| part == format!("d{id}.png").as_bytes())
                })
                .unwrap();
            if id == 3 {
                let mut completed = state.completed.lock().unwrap().clone();
                completed.sort_unstable();
                assert_eq!(completed, [1, 2]);
                assert_eq!(
                    *state.published.lock().unwrap(),
                    vec!["task#1 [d1]".to_owned(), "task#2 [d2]".to_owned()]
                );
            }
            let base = format!("http://{}", headers["host"].to_str().unwrap());
            (
                StatusCode::ACCEPTED,
                axum::Json(
                    json!({"task_id":id.to_string(),"status_url":format!("{base}/status/{id}"),"result_url":format!("{base}/result/{id}")}),
                ),
            )
        }
        async fn status() -> axum::Json<Value> {
            axum::Json(json!({"status":"completed"}))
        }
        async fn result(
            State(state): State<WaveState>,
            AxumPath(id): AxumPath<usize>,
        ) -> impl IntoResponse {
            state.completed.lock().unwrap().push(id);
            let mut zip = ZipWriter::new(Cursor::new(Vec::new()));
            zip.start_file(format!("task-{id}"), SimpleFileOptions::default())
                .unwrap();
            zip.write_all(id.to_string().as_bytes()).unwrap();
            zip.start_file("shared", SimpleFileOptions::default())
                .unwrap();
            zip.write_all(id.to_string().as_bytes()).unwrap();
            (
                [("content-type", "application/zip")],
                zip.finish().unwrap().into_inner(),
            )
        }
        let state = WaveState {
            completed: Arc::new(Mutex::new(Vec::new())),
            published: Arc::new(Mutex::new(Vec::new())),
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
        let documents = (1..=3)
            .map(|id| {
                let path = input.path().join(format!("{id}.png"));
                std::fs::write(&path, b"x").unwrap();
                document(path, &format!("d{id}"), 1, id)
            })
            .collect();
        let callback: ProgressCallback = {
            let published = Arc::clone(&state.published);
            Arc::new(move |event| match event {
                ProgressEvent::ApiCompleted { label } => {
                    published.lock().unwrap().push(label);
                }
                _ => {}
            })
        };
        assert!(
            tokio::time::timeout(
                Duration::from_secs(5),
                run_core(
                    documents,
                    output.path(),
                    &base,
                    RemoteOptions {
                        backend: super::super::Backend::HybridEngine,
                        server_url: None,
                        ..Default::default()
                    },
                    env(),
                    Some(callback),
                    None,
                    None,
                    service(),
                )
            )
            .await
            .unwrap()
            .unwrap()
            .is_empty()
        );
        let mut completed = state.completed.lock().unwrap().clone();
        completed.sort_unstable();
        assert_eq!(completed, [1, 2, 3]);
        assert_eq!(
            *state.published.lock().unwrap(),
            vec![
                "task#1 [d1]".to_owned(),
                "task#2 [d2]".to_owned(),
                "task#3 [d3]".to_owned(),
            ]
        );
        assert_eq!(std::fs::read(output.path().join("shared")).unwrap(), b"3");
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
        let failures = run_core(
            docs,
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
            None,
            None,
            None,
            service(),
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
