use crate::{
    Limits, MinerUVlmClient, OfficialOutputManifest, OfficialPdfOptions, PageResult, PdfInput,
    ProgressCallback, ProgressEvent, TaskWorkLease, VlmError, VlmResult,
    official_builders::{OfficialBuildPage, prepare_official_page_until},
    official_output::{OfficialOutputStage, OfficialOutputTarget},
    pdf, preview,
};
use bytes::Bytes;
use std::{
    future::Future,
    path::Path,
    sync::Arc,
    time::{Duration, Instant},
};

fn effective_render_workers(
    available_cpus: usize,
    configured_workers: usize,
    selected_pages: usize,
) -> usize {
    available_cpus
        .min(configured_workers)
        .min(3)
        .min((selected_pages / 30).max(1))
}

async fn render_with_timeout<T>(
    deadline: RouteDeadline,
    render_timeout: Duration,
    future: impl Future<Output = VlmResult<T>>,
) -> VlmResult<T> {
    let remaining = deadline.remaining()?;
    if render_timeout < remaining {
        tokio::time::timeout(render_timeout, future)
            .await
            .map_err(|_| VlmError::Timeout {
                operation: "PDF rendering",
            })?
    } else {
        deadline.future(future).await
    }
}

pub(crate) fn route_limits(options: &OfficialPdfOptions) -> Limits {
    Limits {
        max_pdf_bytes: options.max_pdf_bytes,
        max_total_asset_bytes: options.max_total_asset_bytes,
        max_pages: options.max_pages,
        max_page_pixels: options.max_page_pixels,
        max_response_bytes: options.max_raw_output_bytes,
        max_rendered_image_bytes: options.max_rendered_image_bytes,
        max_in_flight_image_bytes: options.max_in_flight_image_bytes,
        max_blocks_per_page: options.max_layout_blocks_per_page,
        ..Limits::default()
    }
}

fn map(error: crate::Error) -> VlmError {
    match error {
        crate::Error::LimitExceeded {
            resource,
            limit,
            actual,
        } => VlmError::LimitExceeded {
            resource,
            limit,
            actual,
        },
        crate::Error::Timeout { operation } => VlmError::Timeout { operation },
        crate::Error::InvalidInput(message) => VlmError::InvalidInput(message),
        crate::Error::Pdf(message) => VlmError::Pdf(message),
        crate::Error::Image(message) => VlmError::InvalidInput(message),
        crate::Error::Io(error) => VlmError::Io {
            operation: "official PDF",
            message: error.to_string(),
        },
        error => VlmError::Pdf(error.to_string()),
    }
}

#[derive(Clone, Copy)]
struct RouteDeadline(Instant);

impl RouteDeadline {
    fn new(duration: std::time::Duration) -> VlmResult<Self> {
        Instant::now()
            .checked_add(duration)
            .map(Self)
            .ok_or(VlmError::InvalidConfig(
                "official PDF deadline is outside Instant's range".into(),
            ))
    }

    fn remaining(self) -> VlmResult<std::time::Duration> {
        self.0
            .checked_duration_since(Instant::now())
            .filter(|remaining| !remaining.is_zero())
            .ok_or(VlmError::Timeout {
                operation: "official PDF",
            })
    }

    fn check(self) -> VlmResult<()> {
        self.remaining().map(|_| ())
    }

    fn instant(self) -> Instant {
        self.0
    }

    async fn future<T>(self, future: impl Future<Output = VlmResult<T>>) -> VlmResult<T> {
        tokio::time::timeout(self.remaining()?, future)
            .await
            .map_err(|_| VlmError::Timeout {
                operation: "official PDF",
            })?
    }

    async fn blocking<T: Send + 'static>(
        self,
        task_work_lease: &TaskWorkLease,
        job: impl FnOnce() -> T + Send + 'static,
    ) -> VlmResult<T> {
        let remaining = self.remaining()?;
        let mut task = tokio::task::spawn_blocking(task_work_lease.wrap(job));
        let wait = tokio::time::sleep(remaining);
        tokio::pin!(wait);
        tokio::select! {
            result = &mut task => result.map_err(|error| VlmError::Pdf(error.to_string())),
            _ = &mut wait => {
                // Blocking library work may not be cancellable after it starts. Its task lease
                // keeps admission held until it exits; staged output retains its cleanup guard.
                task.abort();
                drop(task);
                Err(VlmError::Timeout { operation: "official PDF" })
            }
        }
    }
}

pub(crate) async fn parse_and_write(
    client: &MinerUVlmClient,
    input: PdfInput,
    options: OfficialPdfOptions,
    root: &Path,
    stem: &str,
) -> VlmResult<OfficialOutputManifest> {
    parse_and_write_to(
        client,
        input,
        options,
        root,
        stem,
        OfficialOutputTarget::Vlm,
        None,
        None,
    )
    .await
}

pub(crate) async fn parse_and_write_office(
    client: &MinerUVlmClient,
    input: PdfInput,
    options: OfficialPdfOptions,
    root: &Path,
    stem: &str,
) -> VlmResult<OfficialOutputManifest> {
    parse_and_write_to(
        client,
        input,
        options,
        root,
        stem,
        OfficialOutputTarget::Office,
        None,
        None,
    )
    .await
}

pub(crate) async fn parse_and_write_prepared(
    client: &MinerUVlmClient,
    prepared: crate::input_prepare::PreparedPdf,
    options: OfficialPdfOptions,
    root: &Path,
    stem: &str,
) -> VlmResult<OfficialOutputManifest> {
    parse_and_write_prepared_with_events(client, prepared, options, root, stem, None).await
}

pub(crate) async fn parse_and_write_prepared_with_events(
    client: &MinerUVlmClient,
    prepared: crate::input_prepare::PreparedPdf,
    mut options: OfficialPdfOptions,
    root: &Path,
    stem: &str,
    events: Option<ProgressCallback>,
) -> VlmResult<OfficialOutputManifest> {
    if !prepared.kind.supports_page_range() {
        options.start_page = 0;
        options.end_page = None;
    }
    let target = if prepared.kind.is_office() {
        OfficialOutputTarget::Office
    } else {
        OfficialOutputTarget::Vlm
    };
    let origin = Some((prepared.original, prepared.kind.suffix()));
    parse_and_write_to(
        client,
        PdfInput::Bytes(prepared.bytes),
        options,
        root,
        stem,
        target,
        origin,
        events,
    )
    .await
}

async fn parse_and_write_to(
    client: &MinerUVlmClient,
    input: PdfInput,
    options: OfficialPdfOptions,
    root: &Path,
    stem: &str,
    target: OfficialOutputTarget,
    origin: Option<(Bytes, &'static str)>,
    events: Option<ProgressCallback>,
) -> VlmResult<OfficialOutputManifest> {
    options.validate()?;
    let task_work_lease = client.task_work_lease();
    let stem = crate::canonical_stem(stem)?;
    let deadline = RouteDeadline::new(options.total_deadline)?;
    let limits = route_limits(&options);

    let read_limits = limits.clone();
    let bytes = deadline
        .blocking(&task_work_lease, move || {
            pdf::read_input(input, &read_limits)
        })
        .await?
        .map_err(map)?;
    let parse_limits = limits.clone();
    let parsed = deadline
        .blocking(&task_work_lease, move || {
            pdf::parse_document(bytes, &parse_limits)
        })
        .await?
        .map_err(map)?;
    let count = pdf::page_count(&parsed);
    let end = options.end_page.unwrap_or_else(|| count.saturating_sub(1));
    if options.start_page >= count || end >= count {
        return Err(VlmError::InvalidInput(
            "selected page range is outside the PDF".into(),
        ));
    }
    let indexes: Vec<_> = (options.start_page..=end).collect();
    if indexes.len() > options.max_pages {
        return Err(VlmError::LimitExceeded {
            resource: "pages",
            limit: options.max_pages as u64,
            actual: indexes.len() as u64,
        });
    }

    let root = root.to_path_buf();
    let stage_stem = stem.clone();
    let max_stage_assets = options.max_total_asset_bytes;
    let max_stage_text = options.max_staged_text_bytes;
    let mut stage = deadline
        .blocking(&task_work_lease, move || {
            OfficialOutputStage::begin(
                &root,
                &stage_stem,
                target,
                max_stage_assets,
                max_stage_text,
                origin,
            )
        })
        .await??;
    let total = indexes.len();
    let mut completed = 0usize;
    let mut raw_reply_bytes = 0usize;
    let mut encoded_document_bytes = 0usize;
    let mut cursor = 0usize;
    let mut render_limits = limits.clone();
    render_limits.max_rendered_image_bytes = options
        .max_rendered_image_bytes
        .min(options.max_in_flight_image_bytes);
    let render_workers = effective_render_workers(
        std::thread::available_parallelism().map_or(1, std::num::NonZero::get),
        options.render_workers,
        indexes.len(),
    );

    while cursor < indexes.len() {
        if let Err(error) = deadline.check() {
            return Err(dispose_stage(stage, error).await);
        }
        let mut window = Vec::new();
        let mut window_bytes = 0usize;
        while cursor + window.len() < indexes.len() && window.len() < options.processing_window_size
        {
            let index = indexes[cursor + window.len()];
            let bytes = match pdf::page_image_bytes(&parsed, index, &render_limits).map_err(map) {
                Ok(bytes) => bytes,
                Err(error) => return Err(dispose_stage(stage, error).await),
            };
            if bytes > options.max_in_flight_image_bytes {
                return Err(dispose_stage(
                    stage,
                    VlmError::LimitExceeded {
                        resource: "in-flight image bytes",
                        limit: options.max_in_flight_image_bytes as u64,
                        actual: bytes as u64,
                    },
                )
                .await);
            }
            if !window.is_empty()
                && window_bytes.saturating_add(bytes) > options.max_in_flight_image_bytes
            {
                break;
            }
            window_bytes = window_bytes.saturating_add(bytes);
            window.push(index);
        }
        let rendered = match render_with_timeout(deadline, options.render_timeout, async {
            pdf::render_window_for_task(
                parsed.clone(),
                window.clone(),
                render_limits.clone(),
                render_workers,
                task_work_lease.clone(),
            )
            .await
            .map_err(map)
        })
        .await
        {
            Ok(rendered) => rendered,
            Err(error) => return Err(dispose_stage(stage, error).await),
        };
        cursor += window.len();

        if rendered.len() != window.len() {
            return Err(dispose_stage(
                stage,
                VlmError::Pdf("PDF renderer returned an unexpected page count".into()),
            )
            .await);
        }
        let images: Vec<_> = rendered
            .iter()
            .map(|page| Arc::clone(&page.image))
            .collect();
        let remaining_raw = options.max_raw_output_bytes.saturating_sub(raw_reply_bytes);
        let remaining_encoded = options
            .max_encoded_document_bytes
            .saturating_sub(encoded_document_bytes);
        if remaining_raw == 0 || remaining_encoded == 0 {
            return Err(dispose_stage(
                stage,
                VlmError::LimitExceeded {
                    resource: if remaining_raw == 0 {
                        "raw reply bytes"
                    } else {
                        "encoded document bytes"
                    },
                    limit: if remaining_raw == 0 {
                        options.max_raw_output_bytes as u64
                    } else {
                        options.max_encoded_document_bytes as u64
                    },
                    actual: if remaining_raw == 0 {
                        raw_reply_bytes as u64
                    } else {
                        encoded_document_bytes as u64
                    },
                },
            )
            .await);
        }
        let snapshots = match deadline
            .future(client.official_two_step_snapshot_window(
                images,
                options.image_analysis,
                options.formula_enable,
                options.table_enable,
                options.max_layout_blocks_per_page,
                options.max_semantic_requests_per_page,
                options.max_requests_per_batch,
                options.max_encoded_request_bytes,
                options.max_encoded_batch_bytes,
                remaining_encoded,
                remaining_raw,
                deadline.instant(),
            ))
            .await
        {
            Ok(snapshots) if snapshots.len() == rendered.len() => snapshots,
            Ok(_) => {
                return Err(dispose_stage(
                    stage,
                    VlmError::Pdf("VLM returned an unexpected page count".into()),
                )
                .await);
            }
            Err(error) => return Err(dispose_stage(stage, error).await),
        };
        let window_raw = snapshots
            .iter()
            .try_fold(0usize, |total, page| total.checked_add(page.2))
            .ok_or(VlmError::LimitExceeded {
                resource: "raw reply bytes",
                limit: options.max_raw_output_bytes as u64,
                actual: u64::MAX,
            });
        let window_encoded = snapshots
            .iter()
            .try_fold(0usize, |total, page| total.checked_add(page.3))
            .ok_or(VlmError::LimitExceeded {
                resource: "encoded document bytes",
                limit: options.max_encoded_document_bytes as u64,
                actual: u64::MAX,
            });
        let (window_raw, window_encoded) = match (window_raw, window_encoded) {
            (Ok(raw), Ok(encoded)) => (raw, encoded),
            (Err(error), _) | (_, Err(error)) => return Err(dispose_stage(stage, error).await),
        };
        raw_reply_bytes = match raw_reply_bytes.checked_add(window_raw) {
            Some(total) => total,
            None => {
                return Err(dispose_stage(
                    stage,
                    VlmError::LimitExceeded {
                        resource: "raw reply bytes",
                        limit: options.max_raw_output_bytes as u64,
                        actual: u64::MAX,
                    },
                )
                .await);
            }
        };
        encoded_document_bytes = match encoded_document_bytes.checked_add(window_encoded) {
            Some(total) => total,
            None => {
                return Err(dispose_stage(
                    stage,
                    VlmError::LimitExceeded {
                        resource: "encoded document bytes",
                        limit: options.max_encoded_document_bytes as u64,
                        actual: u64::MAX,
                    },
                )
                .await);
            }
        };

        for (page, (snapshot, cleaned, _raw, _encoded)) in rendered.into_iter().zip(snapshots) {
            if let Err(error) = deadline.check() {
                return Err(dispose_stage(stage, error).await);
            }

            let preview_page = PageResult {
                page_index: page.index,
                page_size: page.size,
                blocks: cleaned
                    .into_iter()
                    .map(|block| crate::ContentBlock {
                        kind: crate::BlockKind::new(block.block_type),
                        bbox: block.bbox,
                        angle: block.angle,
                        content: block.content,
                        merge_previous: block.merge_prev.unwrap_or(false),
                        metadata: block.metadata,
                    })
                    .collect(),
            };
            let (returned_stage, result) = deadline
                .blocking(&task_work_lease, move || {
                    let result = (|| {
                        deadline.check()?;
                        stage.write_preview_page(&preview_page)
                    })();
                    (stage, result)
                })
                .await?;
            stage = returned_stage;
            if let Err(error) = result {
                return Err(dispose_stage(stage, error).await);
            }
            let remaining_assets = stage.remaining_asset_bytes();
            let remaining_text = stage.remaining_text_bytes();
            let image = match Arc::try_unwrap(page.image) {
                Ok(image) => image,
                Err(_) => {
                    return Err(dispose_stage(
                        stage,
                        VlmError::Transport {
                            operation: "official PDF",
                            message: "official PDF image ownership was retained".into(),
                        },
                    )
                    .await);
                }
            };
            let built = match deadline
                .blocking(&task_work_lease, move || {
                    prepare_official_page_until(
                        OfficialBuildPage {
                            slice_page_idx: page.index,
                            page_size_points: page.size,
                            render_scale: 200.0 / 72.0,
                            rgb: image,
                            snapshot,
                        },
                        remaining_assets,
                        remaining_text,
                        Some(deadline.instant()),
                    )
                })
                .await
            {
                Ok(Ok(built)) => built,
                Ok(Err(error)) => return Err(dispose_stage(stage, error).await),
                Err(error) => return Err(dispose_stage(stage, error).await),
            };
            if let Err(error) = deadline.check() {
                return Err(dispose_stage(stage, error).await);
            }
            let (returned_stage, result) = deadline
                .blocking(&task_work_lease, move || {
                    let result = (|| {
                        deadline.check()?;
                        stage.write_prepared_page(page.index, built.page, &built.assets)
                    })();
                    (stage, result)
                })
                .await?;
            stage = returned_stage;
            if let Err(error) = result {
                return Err(dispose_stage(stage, error).await);
            }
            completed += 1;
            crate::progress_events::emit(
                &events,
                ProgressEvent::DocumentPageCompleted {
                    document: stem.clone(),
                    page_index: page.index,
                    completed,
                    total,
                },
            );
        }
    }

    if let Err(error) = deadline.check() {
        return Err(dispose_stage(stage, error).await);
    }
    let source = match deadline
        .blocking(&task_work_lease, move || {
            deadline.check()?;
            Ok(pdf::source_bytes(&parsed))
        })
        .await
    {
        Ok(Ok(source)) => source,
        Ok(Err(error)) => return Err(dispose_stage(stage, error).await),
        Err(error) => return Err(dispose_stage(stage, error).await),
    };
    let preview_limits = limits.clone();
    let preview_stem = stem.clone();
    let formula_enable = options.formula_enable;
    let table_enable = options.table_enable;
    let (stage, result) = deadline
        .blocking(&task_work_lease, move || {
            let result = (|| {
                stage.finalize_document(formula_enable, table_enable, deadline.instant())?;
                let preview_pages = stage.preview_pages()?;
                let preview = preview::generate_until(
                    source.as_ref(),
                    &preview_pages,
                    &preview_stem,
                    &preview_limits,
                    stage.remaining_asset_bytes(),
                    deadline.instant(),
                )
                .map_err(map)?;
                stage.write_preview(&preview)?;
                stage.assemble(deadline.instant())?;
                stage.prepare_commit()
            })();
            (stage, result)
        })
        .await?;
    if let Err(error) = result {
        return Err(dispose_stage(stage, error).await);
    }
    if let Err(error) = deadline.check() {
        return Err(dispose_stage(stage, error).await);
    }
    commit_stage(stage, deadline, &task_work_lease).await
}

async fn commit_stage(
    stage: OfficialOutputStage,
    deadline: RouteDeadline,
    task_work_lease: &TaskWorkLease,
) -> VlmResult<OfficialOutputManifest> {
    let (started_tx, mut started_rx) = tokio::sync::oneshot::channel();
    let (permit_tx, permit_rx) = std::sync::mpsc::sync_channel(1);
    let task = tokio::task::spawn_blocking(task_work_lease.wrap(move || {
        let _ = started_tx.send(());
        admitted_commit(stage, permit_rx, deadline)
    }));
    // A queued blocking task owns the stage but cannot publish: it is parked on the permit.
    // Abort before admission drops that stage, whose capability-only Drop schedules cleanup.
    tokio::select! {
        result = &mut started_rx => result.map_err(|_| VlmError::Pdf("commit worker stopped".into()))?,
        _ = tokio::time::sleep(deadline.remaining()?) => {
            task.abort();
            return Err(VlmError::Timeout { operation: "official PDF" });
        }
    }
    deadline.check()?;
    // The worker rechecks the deadline immediately before this irreversible admission. Once the
    // permit is sent, await without a timeout: atomic rename must finish rather than be cancelled.
    permit_tx
        .send(())
        .map_err(|_| VlmError::Pdf("commit worker stopped".into()))?;
    task.await
        .map_err(|error| VlmError::Pdf(error.to_string()))?
}

fn admitted_commit(
    stage: OfficialOutputStage,
    permit_rx: std::sync::mpsc::Receiver<()>,
    deadline: RouteDeadline,
) -> VlmResult<OfficialOutputManifest> {
    match permit_rx.recv() {
        Ok(()) if deadline.check().is_ok() => stage.commit(),
        _ => {
            // Admission failed or expired before commit: Drop keeps no-follow cleanup detached.
            drop(stage);
            Err(VlmError::Timeout {
                operation: "official PDF",
            })
        }
    }
}

async fn dispose_stage(stage: OfficialOutputStage, error: VlmError) -> VlmError {
    if matches!(error, VlmError::Timeout { .. }) {
        // Timeout must return promptly; Drop retains detached capability cleanup.
        drop(stage);
    } else {
        // Non-timeout failures own a recovered stage, so complete cleanup off-runtime.
        let _ = tokio::task::spawn_blocking(move || stage.cleanup()).await;
    }
    error
}

#[cfg(test)]
mod tests {
    use super::{
        RouteDeadline, admitted_commit, effective_render_workers, map, render_with_timeout,
    };
    use crate::official_output::OfficialOutputStage;
    use std::{
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
            mpsc,
        },
        time::{Duration, Instant},
    };

    #[test]
    fn native_input_and_pdf_failures_are_not_vlm_protocol_failures() {
        assert!(matches!(
            map(crate::Error::InvalidInput("bad input".into())),
            crate::VlmError::InvalidInput(_)
        ));
        assert!(matches!(
            map(crate::Error::Pdf("bad PDF".into())),
            crate::VlmError::Pdf(_)
        ));
    }

    #[tokio::test]
    async fn expired_deadline_rejects_work_without_waiting() {
        let deadline = RouteDeadline(
            Instant::now()
                .checked_sub(Duration::from_secs(1))
                .expect("one second is representable"),
        );
        assert!(matches!(
            deadline
                .future(std::future::pending::<crate::VlmResult<()>>())
                .await,
            Err(crate::VlmError::Timeout { .. })
        ));
    }

    #[test]
    fn deadline_construction_uses_checked_add() {
        assert!(RouteDeadline::new(Duration::MAX).is_err());
    }

    #[test]
    fn effective_render_workers_uses_selected_page_total_cpu_and_configured_caps() {
        assert_eq!(effective_render_workers(8, 8, 29), 1);
        assert_eq!(effective_render_workers(8, 8, 30), 1);
        assert_eq!(effective_render_workers(8, 8, 59), 1);
        assert_eq!(effective_render_workers(8, 8, 60), 2);
        assert_eq!(effective_render_workers(8, 8, 89), 2);
        assert_eq!(effective_render_workers(8, 8, 90), 3);
        assert_eq!(effective_render_workers(1, 8, 90), 1);
        assert_eq!(effective_render_workers(8, 1, 90), 1);
        assert_eq!(effective_render_workers(8, 99, 90), 3);
    }

    #[tokio::test]
    async fn render_timeout_is_specific_and_never_extends_the_route_deadline() {
        let deadline = RouteDeadline::new(Duration::from_secs(1)).expect("deadline");
        assert!(matches!(
            render_with_timeout(
                deadline,
                Duration::from_millis(1),
                std::future::pending::<crate::VlmResult<()>>(),
            )
            .await,
            Err(crate::VlmError::Timeout {
                operation: "PDF rendering"
            })
        ));
        let expired = RouteDeadline(
            Instant::now()
                .checked_sub(Duration::from_secs(1))
                .expect("representable"),
        );
        assert!(matches!(
            render_with_timeout(
                expired,
                Duration::MAX,
                std::future::pending::<crate::VlmResult<()>>(),
            )
            .await,
            Err(crate::VlmError::Timeout {
                operation: "official PDF"
            })
        ));
    }

    #[test]
    fn timed_out_queued_blocking_job_is_aborted() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .max_blocking_threads(1)
            .build()
            .expect("runtime");
        let (release, held) = mpsc::channel();
        let ran = Arc::new(AtomicBool::new(false));
        runtime.block_on(async {
            tokio::task::spawn_blocking(move || held.recv().expect("release worker"));
            tokio::time::sleep(Duration::from_millis(10)).await;
            let deadline = RouteDeadline::new(Duration::from_millis(10)).expect("deadline");
            let task_work_lease = crate::TaskWorkLease::default();
            let job_ran = Arc::clone(&ran);
            assert!(matches!(
                deadline
                    .blocking(&task_work_lease, move || job_ran
                        .store(true, Ordering::SeqCst))
                    .await,
                Err(crate::VlmError::Timeout { .. })
            ));
        });
        release.send(()).expect("release worker");
        std::thread::sleep(Duration::from_millis(20));
        assert!(!ran.load(Ordering::SeqCst));
    }

    #[test]
    fn timed_out_queued_stage_owner_detaches_cleanup() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .max_blocking_threads(1)
            .build()
            .expect("runtime");
        let root = tempfile::tempdir().expect("root");
        let stage = OfficialOutputStage::begin(
            root.path(),
            "document",
            crate::official_output::OfficialOutputTarget::Vlm,
            usize::MAX,
            usize::MAX,
            None,
        )
        .expect("stage");
        let (release, held) = mpsc::channel();
        runtime.block_on(async {
            tokio::task::spawn_blocking(move || held.recv().expect("release worker"));
            tokio::time::sleep(Duration::from_millis(10)).await;
            let started = Instant::now();
            let task_work_lease = crate::TaskWorkLease::default();
            assert!(matches!(
                RouteDeadline::new(Duration::from_millis(10))
                    .expect("deadline")
                    .blocking(&task_work_lease, move || drop(stage))
                    .await,
                Err(crate::VlmError::Timeout { .. })
            ));
            assert!(started.elapsed() < Duration::from_secs(1));
        });
        release.send(()).expect("release worker");
        std::thread::spawn({
            let root = root.path().join("document");
            move || {
                let deadline = Instant::now() + Duration::from_secs(1);
                while std::fs::read_dir(&root)
                    .expect("document root")
                    .any(|entry| {
                        entry
                            .expect("entry")
                            .file_name()
                            .to_string_lossy()
                            .starts_with(".vlm-staging-parent-")
                    })
                {
                    assert!(Instant::now() < deadline, "stage cleanup timed out");
                    std::thread::yield_now();
                }
            }
        })
        .join()
        .expect("cleanup waiter");
    }

    #[test]
    fn expired_admitted_commit_detaches_cleanup_without_waiting() {
        let root = tempfile::tempdir().expect("root");
        let stage = OfficialOutputStage::begin(
            root.path(),
            "document",
            crate::official_output::OfficialOutputTarget::Vlm,
            usize::MAX,
            usize::MAX,
            None,
        )
        .expect("stage");
        let (permit, receiver) = mpsc::sync_channel(1);
        permit.send(()).expect("permit");
        let expired = RouteDeadline(
            Instant::now()
                .checked_sub(Duration::from_secs(1))
                .expect("representable"),
        );
        let started = Instant::now();
        assert!(matches!(
            admitted_commit(stage, receiver, expired),
            Err(crate::VlmError::Timeout { .. })
        ));
        assert!(started.elapsed() < Duration::from_secs(1));
        let document = root.path().join("document");
        for _ in 0..100 {
            if !std::fs::read_dir(&document)
                .expect("document root")
                .any(|entry| {
                    entry
                        .expect("entry")
                        .file_name()
                        .to_string_lossy()
                        .starts_with(".vlm-staging-parent-")
                })
            {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("detached cleanup did not finish within one second");
    }
}
