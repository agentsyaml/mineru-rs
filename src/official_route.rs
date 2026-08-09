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
use tokio::sync::Semaphore;

#[derive(Clone)]
pub(crate) struct OfficialPageConcurrency(Arc<Semaphore>);

impl OfficialPageConcurrency {
    pub(crate) fn new(configured: usize, window: usize, http: usize) -> VlmResult<Self> {
        // The tokio capacity is a legitimate representability bound; an explicit value above it
        // fails here instead of being silently reduced.
        if configured > Semaphore::MAX_PERMITS {
            return Err(VlmError::InvalidConfig(
                "page concurrency exceeds the tokio semaphore capacity".into(),
            ));
        }
        Ok(Self(Arc::new(Semaphore::new(effective_page_concurrency(
            configured, window, http,
        )))))
    }

    fn semaphore(&self) -> Arc<Semaphore> {
        Arc::clone(&self.0)
    }

    pub(crate) fn from_semaphore(semaphore: Arc<Semaphore>) -> Self {
        Self(semaphore)
    }
}

pub(crate) fn effective_page_concurrency(configured: usize, window: usize, http: usize) -> usize {
    // Runtime-derived minima express actual downstream capacity and remain. The lower bound of
    // one keeps a degenerate derived input from creating a zero-permit semaphore; the
    // `Semaphore::MAX_PERMITS` representability guard lives in `OfficialPageConcurrency::new`.
    configured.min(window).min(http).max(1)
}

fn effective_render_workers(
    available_cpus: usize,
    configured_workers: usize,
    selected_pages: usize,
) -> usize {
    available_cpus
        .min(configured_workers)
        .min(selected_pages)
        .max(1)
}

#[cfg(test)]
tokio::task_local! {
    static AVAILABLE_RENDER_PARALLELISM: usize;
}

#[cfg(test)]
async fn scope_available_render_parallelism<T>(
    available: usize,
    future: impl Future<Output = T>,
) -> T {
    AVAILABLE_RENDER_PARALLELISM.scope(available, future).await
}

#[cfg(test)]
fn available_render_parallelism() -> usize {
    AVAILABLE_RENDER_PARALLELISM
        .try_with(|available| *available)
        .unwrap_or_else(|_| std::thread::available_parallelism().map_or(1, std::num::NonZero::get))
}

#[cfg(not(test))]
fn available_render_parallelism() -> usize {
    std::thread::available_parallelism().map_or(1, std::num::NonZero::get)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WindowPlanMode {
    Slot,
    FullCapFallback,
}

#[derive(Debug, PartialEq, Eq)]
struct PlannedWindow {
    indexes: Vec<usize>,
    bytes: usize,
    mode: WindowPlanMode,
    /// Number of source page positions this window consumed, including pages skipped during
    /// planning (unrenderable or over the in-flight budget). `retain_window` advances the cursor
    /// past them so a skipped page is never re-planned by the next window.
    consumed: usize,
}

struct WindowState {
    plan: PlannedWindow,
    rendered: Vec<pdf::RenderedPage>,
    /// Render degradations for this window, surfaced as VlmWarning events during staging.
    warnings: Vec<String>,
}

enum PrefetchState {
    Rendered(WindowState),
    PendingFallback((PlannedWindow, Vec<String>)),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RenderRole {
    Current,
    Prefetch,
    Fallback,
}

#[cfg(test)]
#[derive(Debug, Clone, PartialEq, Eq)]
struct RenderTestInfo {
    role: RenderRole,
    indexes: Vec<usize>,
    planned_bytes: usize,
}

#[cfg(test)]
type RenderBeforeCallback = Arc<
    dyn Fn(RenderTestInfo) -> std::pin::Pin<Box<dyn Future<Output = VlmResult<()>> + Send>>
        + Send
        + Sync,
>;

#[cfg(test)]
struct WindowRenderTestHook {
    before: RenderBeforeCallback,
    after: Arc<dyn Fn(RenderTestInfo, usize) + Send + Sync>,
    on_drop: Arc<dyn Fn(RenderTestInfo) + Send + Sync>,
}

#[cfg(test)]
tokio::task_local! {
    static WINDOW_RENDER_TEST_HOOK: Arc<WindowRenderTestHook>;
}

#[cfg(test)]
async fn scope_window_render_test_hook<T>(
    hook: Arc<WindowRenderTestHook>,
    future: impl Future<Output = T>,
) -> T {
    WINDOW_RENDER_TEST_HOOK.scope(hook, future).await
}

#[cfg(test)]
fn window_render_test_hook() -> Option<Arc<WindowRenderTestHook>> {
    WINDOW_RENDER_TEST_HOOK.try_with(Arc::clone).ok()
}

#[cfg(test)]
struct RenderDropGuard {
    hook: Arc<WindowRenderTestHook>,
    info: RenderTestInfo,
    complete: bool,
}

#[cfg(test)]
impl Drop for RenderDropGuard {
    fn drop(&mut self) {
        if !self.complete {
            (self.hook.on_drop)(self.info.clone());
        }
    }
}

fn split_image_slot_caps(full_cap: usize) -> (usize, usize) {
    let first = full_cap / 2;
    (first, full_cap - first)
}

fn in_flight_image_limit(full_cap: usize, actual: usize) -> VlmError {
    VlmError::LimitExceeded {
        resource: "in-flight image bytes",
        limit: full_cap as u64,
        actual: actual as u64,
    }
}

fn retain_window(cursor: &mut usize, window: &PlannedWindow) -> VlmResult<()> {
    *cursor = cursor
        .checked_add(window.consumed)
        .ok_or_else(|| in_flight_image_limit(usize::MAX, usize::MAX))?;
    Ok(())
}

fn plan_window(
    indexes: &[usize],
    cursor: usize,
    processing_window_size: usize,
    slot_cap: usize,
    full_cap: usize,
    mut page_bytes: impl FnMut(usize) -> VlmResult<usize>,
) -> VlmResult<(PlannedWindow, Vec<String>)> {
    if processing_window_size == 0 || slot_cap == 0 || full_cap == 0 || slot_cap > full_cap {
        return Err(VlmError::InvalidConfig(
            "invalid official PDF options".into(),
        ));
    }
    if cursor >= indexes.len() {
        return Err(VlmError::InvalidInput(
            "window cursor is outside selected pages".into(),
        ));
    }

    let mut planned = Vec::new();
    let mut total = 0usize;
    let mut warnings = Vec::new();
    let mut first_error = None;
    let mut consumed = 0usize;
    for &index in indexes.iter().skip(cursor) {
        if planned.len() >= processing_window_size {
            break;
        }
        // A page whose byte estimate fails (invalid dimensions, viewport beyond u16) is skipped
        // with a warning instead of aborting the window; only an entirely skipped window errors.
        let bytes = match page_bytes(index) {
            Ok(bytes) => bytes,
            Err(error) => {
                consumed += 1;
                warnings.push(format!("page {index} skipped: {error}"));
                if first_error.is_none() {
                    first_error = Some(error);
                }
                continue;
            }
        };
        if bytes > full_cap {
            consumed += 1;
            warnings.push(format!(
                "page {index} skipped: page exceeds the in-flight image byte budget ({bytes} bytes)"
            ));
            if first_error.is_none() {
                first_error = Some(in_flight_image_limit(full_cap, bytes));
            }
            continue;
        }
        if planned.is_empty() && bytes > slot_cap {
            consumed += 1;
            return Ok((
                PlannedWindow {
                    indexes: vec![index],
                    bytes,
                    mode: WindowPlanMode::FullCapFallback,
                    consumed,
                },
                warnings,
            ));
        }
        let next_total = total
            .checked_add(bytes)
            .ok_or_else(|| in_flight_image_limit(full_cap, usize::MAX))?;
        if next_total > slot_cap {
            break;
        }
        consumed += 1;
        total = next_total;
        planned.push(index);
    }

    if planned.is_empty() {
        // Every page in this window was skipped: a hard error, never an empty placeholder
        // document masquerading as output.
        return Err(first_error.unwrap_or_else(|| in_flight_image_limit(full_cap, usize::MAX)));
    }
    Ok((
        PlannedWindow {
            indexes: planned,
            bytes: total,
            mode: WindowPlanMode::Slot,
            consumed,
        },
        warnings,
    ))
}

fn plan_route_window(
    parsed: &pdf::ParsedPdf,
    indexes: &[usize],
    cursor: usize,
    options: &OfficialPdfOptions,
    render_limits: &Limits,
) -> VlmResult<(PlannedWindow, Vec<String>)> {
    plan_window(
        indexes,
        cursor,
        options.processing_window_size,
        options.max_in_flight_image_bytes,
        options.max_in_flight_image_bytes,
        |index| pdf::page_image_bytes(parsed, index, render_limits).map_err(map),
    )
}

fn plan_route_window_in_slot(
    parsed: &pdf::ParsedPdf,
    indexes: &[usize],
    cursor: usize,
    options: &OfficialPdfOptions,
    render_limits: &Limits,
    slot_cap: usize,
) -> VlmResult<(PlannedWindow, Vec<String>)> {
    plan_window(
        indexes,
        cursor,
        options.processing_window_size,
        slot_cap,
        options.max_in_flight_image_bytes,
        |index| pdf::page_image_bytes(parsed, index, render_limits).map_err(map),
    )
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

#[cfg(test)]
async fn observe_route_render<T>(
    hook: Option<Arc<WindowRenderTestHook>>,
    info: RenderTestInfo,
    render: impl Future<Output = VlmResult<T>>,
    actual_bytes: impl FnOnce(&T) -> VlmResult<usize>,
) -> VlmResult<T> {
    let Some(hook) = hook else {
        return render.await;
    };
    let mut guard = RenderDropGuard {
        hook: Arc::clone(&hook),
        info: info.clone(),
        complete: false,
    };
    (hook.before)(info.clone()).await?;
    let rendered = render.await?;
    let actual = actual_bytes(&rendered)?;
    (hook.after)(info, actual);
    guard.complete = true;
    Ok(rendered)
}

async fn render_route_window(
    role: RenderRole,
    deadline: RouteDeadline,
    render_timeout: Duration,
    parsed: Arc<pdf::ParsedPdf>,
    plan: PlannedWindow,
    plan_warnings: Vec<String>,
    render_limits: Limits,
    render_workers: usize,
    task_work_lease: TaskWorkLease,
) -> VlmResult<WindowState> {
    #[cfg(not(test))]
    let _ = role;
    #[cfg(test)]
    let hook = window_render_test_hook();
    #[cfg(test)]
    let info = RenderTestInfo {
        role,
        indexes: plan.indexes.clone(),
        planned_bytes: plan.bytes,
    };
    let render = render_with_timeout(deadline, render_timeout, async {
        pdf::render_window_for_task_tolerant(
            parsed,
            plan.indexes.clone(),
            render_limits,
            render_workers,
            task_work_lease,
        )
        .await
        .map_err(map)
    });
    #[cfg(test)]
    let (rendered, render_warnings) = observe_route_render(hook, info, render, |(pages, _)| {
        pages.iter().try_fold(0usize, |total, page| {
            total
                .checked_add(page.image.as_raw().len())
                .ok_or_else(|| in_flight_image_limit(usize::MAX, usize::MAX))
        })
    })
    .await?;
    #[cfg(not(test))]
    let (rendered, render_warnings) = render.await?;

    // Tolerant within a window: failed pages degrade to placeholders (with a warning) rather
    // than failing the document. Only an entirely failed window hard-errors, which the tolerant
    // renderer already reports as an Err. `stage_window` re-derives which pages are placeholders
    // from `plan.indexes` vs `rendered`, and surfaces `warnings` as VlmWarning events.
    let mut warnings = plan_warnings;
    warnings.extend(render_warnings);
    Ok(WindowState {
        plan,
        rendered,
        warnings,
    })
}

/// Snapshot output of one window's two-step VLM work, in source page order. The final element
/// carries LLM-output warnings collected for that page (malformed layout/semantic replies).
type WindowSnapshots = Vec<(
    Vec<crate::ModelBlock>,
    Vec<crate::VlmLayoutBlock>,
    usize,
    usize,
    Vec<String>,
)>;

struct StagedWindow {
    stage: OfficialOutputStage,
    completed: usize,
}

/// Serial, source-ordered staging/publication of one rendered window. Extracted so Phase B can
/// poll it concurrently with the next window's VLM future. Internal failures keep today's
/// `dispose_stage` semantics; if the future itself is dropped (a sibling VLM failure), the owned
/// stage's capability-only Drop schedules the same cleanup.
async fn stage_window(
    mut stage: OfficialOutputStage,
    deadline: RouteDeadline,
    task_work_lease: &TaskWorkLease,
    events: &Option<ProgressCallback>,
    stem: &str,
    total: usize,
    current: WindowState,
    snapshots: WindowSnapshots,
    mut completed: usize,
) -> VlmResult<StagedWindow> {
    for warning in &current.warnings {
        crate::progress_events::emit(
            events,
            ProgressEvent::VlmWarning { message: warning.clone() },
        );
    }
    let mut rendered = current.rendered.into_iter().peekable();
    let mut snapshots = snapshots.into_iter();
    for &index in &current.plan.indexes {
        if let Err(error) = deadline.check() {
            return Err(dispose_stage(stage, error).await);
        }
        // A page missing from the rendered window (tolerant render failure) degrades to an
        // empty placeholder so the output keeps every source page index.
        if !rendered.peek().is_some_and(|page| page.index == index) {
            stage = write_placeholder_page(stage, deadline, task_work_lease, index).await?;
            completed += 1;
            crate::progress_events::emit(
                events,
                ProgressEvent::DocumentPageCompleted {
                    document: stem.to_string(),
                    page_index: index,
                    completed,
                    total,
                },
            );
            continue;
        }
        let page = rendered.next().expect("peeked rendered page");
        let (snapshot, cleaned, _raw, _encoded, warnings) =
            snapshots.next().expect("snapshots match rendered pages");
        for warning in warnings {
            crate::progress_events::emit(events, ProgressEvent::VlmWarning { message: warning });
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
            .blocking(task_work_lease, move || {
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
        let remaining_assets = stage.remaining_asset_buffer_bytes();
        let remaining_text = stage.remaining_text_buffer_bytes();
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
            .blocking(task_work_lease, move || {
                prepare_official_page_until(
                    OfficialBuildPage {
                        slice_page_idx: page.index,
                        page_size_points: page.size,
                        // The page's actual adaptive scale, so bbox mapping and the RGB-size
                        // validation match the downscaled raster of oversized pages.
                        render_scale: page.scale,
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
            .blocking(task_work_lease, move || {
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
            events,
            ProgressEvent::DocumentPageCompleted {
                document: stem.to_string(),
                page_index: page.index,
                completed,
                total,
            },
        );
    }
    Ok(StagedWindow { stage, completed })
}

/// Stages one page whose render failed: an empty preview page plus a minimal blank prepared
/// page, so `pages == preview_pages` holds and every source index stays in the manifest.
async fn write_placeholder_page(
    mut stage: OfficialOutputStage,
    deadline: RouteDeadline,
    task_work_lease: &TaskWorkLease,
    index: usize,
) -> VlmResult<OfficialOutputStage> {
    let preview_page = PageResult {
        page_index: index,
        page_size: [1.0, 1.0],
        blocks: Vec::new(),
    };
    let (returned_stage, result) = deadline
        .blocking(task_work_lease, move || {
            let result = (|| {
                deadline.check()?;
                stage.write_preview_page(&preview_page)
            })();
            (stage, result)
        })
        .await?;
    let mut stage = returned_stage;
    if let Err(error) = result {
        return Err(dispose_stage(stage, error).await);
    }
    let remaining_assets = stage.remaining_asset_buffer_bytes();
    let remaining_text = stage.remaining_text_buffer_bytes();
    // ponytail: a failed page is staged as a minimal 1x1 pt blank page (empty blocks, no
    // assets). Its true size is unknowable when dimensions extraction itself failed; a fixed
    // valid size keeps `prepare_official_page_until` happy. Pass real sizes if that matters.
    let built = match deadline
        .blocking(task_work_lease, move || {
            prepare_official_page_until(
                OfficialBuildPage {
                    slice_page_idx: index,
                    page_size_points: [1.0, 1.0],
                    render_scale: 200.0 / 72.0,
                    rgb: image::RgbImage::new(3, 3),
                    snapshot: Vec::new(),
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
        .blocking(task_work_lease, move || {
            let result = (|| {
                deadline.check()?;
                stage.write_prepared_page(index, built.page, &built.assets)
            })();
            (stage, result)
        })
        .await?;
    let stage = returned_stage;
    if let Err(error) = result {
        return Err(dispose_stage(stage, error).await);
    }
    Ok(stage)
}

pub(crate) fn route_limits(options: &OfficialPdfOptions, response_cap: usize) -> Limits {
    Limits {
        max_pdf_bytes: options.max_pdf_bytes,
        max_total_asset_bytes: options.max_total_asset_bytes,
        max_pages: options.max_pages,
        max_page_pixels: options.max_page_pixels,
        max_response_bytes: response_cap,
        max_rendered_image_bytes: options.max_rendered_image_bytes,
        max_in_flight_image_bytes: options.max_in_flight_image_bytes,
        max_blocks_per_page: options.max_layout_blocks_per_page,
        ..Limits::default()
    }
}

pub(crate) type CleanupWarningCallback = Arc<dyn Fn() + Send + Sync + 'static>;

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
    let totals = crate::document_limits::OfficialDocumentTotals::from_options(&options);
    parse_and_write_to(
        client,
        input,
        options,
        root,
        stem,
        OfficialOutputTarget::Vlm,
        None,
        None,
        None,
        totals,
        client.official_page_concurrency(),
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
    let totals = crate::document_limits::OfficialDocumentTotals::from_options(&options);
    parse_and_write_to(
        client,
        input,
        options,
        root,
        stem,
        OfficialOutputTarget::Office,
        None,
        None,
        None,
        totals,
        client.official_page_concurrency(),
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
    let totals = crate::document_limits::OfficialDocumentTotals::from_options(&options);
    parse_and_write_prepared_with_events_and_cleanup_warning_with_totals(
        client, prepared, options, root, stem, None, None, totals,
    )
    .await
}

pub(crate) async fn parse_and_write_prepared_with_events_and_cleanup_warning_with_totals(
    client: &MinerUVlmClient,
    prepared: crate::input_prepare::PreparedPdf,
    mut options: OfficialPdfOptions,
    root: &Path,
    stem: &str,
    events: Option<ProgressCallback>,
    cleanup_warning: Option<CleanupWarningCallback>,
    totals: crate::document_limits::OfficialDocumentTotals,
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
    parse_and_write_to(
        client,
        PdfInput::Bytes(prepared.bytes),
        options,
        root,
        stem,
        target,
        Some((prepared.original, prepared.kind.suffix())),
        events,
        cleanup_warning,
        totals,
        client.official_page_concurrency(),
    )
    .await
}

pub(crate) async fn parse_and_write_prepared_with_events_and_cleanup_warning_with_totals_and_page_concurrency(
    client: &MinerUVlmClient,
    prepared: crate::input_prepare::PreparedPdf,
    options: OfficialPdfOptions,
    root: &Path,
    stem: &str,
    events: Option<ProgressCallback>,
    cleanup_warning: Option<CleanupWarningCallback>,
    totals: crate::document_limits::OfficialDocumentTotals,
    page_concurrency: OfficialPageConcurrency,
) -> VlmResult<OfficialOutputManifest> {
    // Keep the existing preparation semantics; only page admission differs.
    let mut options = options;
    if !prepared.kind.supports_page_range() {
        options.start_page = 0;
        options.end_page = None;
    }
    let target = if prepared.kind.is_office() {
        OfficialOutputTarget::Office
    } else {
        OfficialOutputTarget::Vlm
    };
    parse_and_write_to(
        client,
        PdfInput::Bytes(prepared.bytes),
        options,
        root,
        stem,
        target,
        Some((prepared.original, prepared.kind.suffix())),
        events,
        cleanup_warning,
        totals,
        page_concurrency,
    )
    .await
}

pub(crate) async fn parse_and_write_prepared_with_events(
    client: &MinerUVlmClient,
    prepared: crate::input_prepare::PreparedPdf,
    options: OfficialPdfOptions,
    root: &Path,
    stem: &str,
    events: Option<ProgressCallback>,
) -> VlmResult<OfficialOutputManifest> {
    let totals = crate::document_limits::OfficialDocumentTotals::from_options(&options);
    parse_and_write_prepared_with_events_and_cleanup_warning_with_totals(
        client, prepared, options, root, stem, events, None, totals,
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
    cleanup_warning: Option<CleanupWarningCallback>,
    totals: crate::document_limits::OfficialDocumentTotals,
    page_concurrency: OfficialPageConcurrency,
) -> VlmResult<OfficialOutputManifest> {
    options.validate()?;
    let task_work_lease = client.task_work_lease();
    let stem = crate::canonical_stem(stem)?;
    let deadline = RouteDeadline::new(options.total_deadline)?;
    let limits = route_limits(&options, client.official_response_cap());

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
    let max_stage_assets = totals.assets;
    let max_stage_text = totals.staged_text;
    let resident_stage_assets = options.max_total_asset_bytes;
    let resident_stage_text = options.max_staged_text_bytes;
    let mut stage = deadline
        .blocking(&task_work_lease, move || {
            OfficialOutputStage::begin_with_resident(
                &root,
                &stage_stem,
                target,
                max_stage_assets,
                max_stage_text,
                resident_stage_assets,
                resident_stage_text,
                origin,
            )
        })
        .await??;
    let total = indexes.len();
    let mut completed = 0usize;
    let raw_budget = Arc::new(crate::vlm_http::ByteBudget::new(totals.raw));
    let encoded_budget = Arc::new(crate::vlm_http::ByteBudget::new(totals.encoded));
    let mut cursor = 0usize;
    let mut render_limits = limits.clone();
    render_limits.max_rendered_image_bytes = options
        .max_rendered_image_bytes
        .min(options.max_in_flight_image_bytes);
    let render_workers = effective_render_workers(
        available_render_parallelism(),
        options.render_workers,
        indexes.len(),
    );
    let slot_caps = {
        let (first, second) = split_image_slot_caps(options.max_in_flight_image_bytes);
        [first, second]
    };
    let overlap_enabled = slot_caps[0] != 0 && slot_caps[1] != 0;
    let mut next_slot = 0usize;
    let mut prefetch: Option<PrefetchState> = None;
    // Snapshots produced by phase B for the window held in `prefetch` as `Rendered`.
    let mut next_snapshots: Option<WindowSnapshots> = None;

    while cursor < indexes.len() || prefetch.is_some() {
        if let Err(error) = deadline.check() {
            return Err(dispose_stage(stage, error).await);
        }
        let (current, pending_snapshots) = match prefetch.take() {
            Some(PrefetchState::Rendered(current)) => (
                current,
                Some(
                    next_snapshots
                        .take()
                        .expect("a rendered prefetch always carries its phase-B snapshots"),
                ),
            ),
            Some(PrefetchState::PendingFallback((plan, plan_warnings))) => {
                match render_route_window(
                    RenderRole::Fallback,
                    deadline,
                    options.render_timeout,
                    parsed.clone(),
                    plan,
                    plan_warnings,
                    render_limits.clone(),
                    render_workers,
                    task_work_lease.clone(),
                )
                .await
                {
                    Ok(current) => (current, None),
                    Err(error) => return Err(dispose_stage(stage, error).await),
                }
            }
            None => {
                let (plan, plan_warnings) = match if overlap_enabled {
                    plan_route_window_in_slot(
                        &parsed,
                        &indexes,
                        cursor,
                        &options,
                        &render_limits,
                        slot_caps[next_slot],
                    )
                } else {
                    plan_route_window(&parsed, &indexes, cursor, &options, &render_limits)
                } {
                    Ok(window) => window,
                    Err(error) => return Err(dispose_stage(stage, error).await),
                };
                if let Err(error) = retain_window(&mut cursor, &plan) {
                    return Err(dispose_stage(stage, error).await);
                }
                match render_route_window(
                    RenderRole::Current,
                    deadline,
                    options.render_timeout,
                    parsed.clone(),
                    plan,
                    plan_warnings,
                    render_limits.clone(),
                    render_workers,
                    task_work_lease.clone(),
                )
                .await
                {
                    Ok(current) => (current, None),
                    Err(error) => return Err(dispose_stage(stage, error).await),
                }
            }
        };

        // The current window's VLM future exists only when phase B of the previous iteration did
        // not already complete it (a prefetched current carries its snapshots).
        let vlm = if pending_snapshots.is_none() {
            let images: Vec<_> = current
                .rendered
                .iter()
                .map(|page| Arc::clone(&page.image))
                .collect();
            Some(
                client.official_two_step_snapshot_window_with_budgets_and_page_semaphore(
                    images,
                    options.image_analysis,
                    options.formula_enable,
                    options.table_enable,
                    options.max_layout_blocks_per_page,
                    options.max_semantic_requests_per_page,
                    options.max_requests_per_batch,
                    options.max_encoded_request_bytes,
                    options.max_encoded_batch_bytes,
                    Arc::clone(&encoded_budget),
                    Arc::clone(&raw_budget),
                    deadline.instant(),
                    page_concurrency.semaphore(),
                ),
            )
        } else {
            None
        };

        let mut pending_fallback = None;
        let next = if overlap_enabled
            && current.plan.mode == WindowPlanMode::Slot
            && cursor < indexes.len()
        {
            match plan_route_window_in_slot(
                &parsed,
                &indexes,
                cursor,
                &options,
                &render_limits,
                slot_caps[1 - next_slot],
            ) {
                Ok((next, next_warnings)) if next.mode == WindowPlanMode::Slot => {
                    if let Err(error) = retain_window(&mut cursor, &next) {
                        return Err(dispose_stage(stage, error).await);
                    }
                    Some((next, next_warnings))
                }
                Ok((fallback, fallback_warnings)) => {
                    if let Err(error) = retain_window(&mut cursor, &fallback) {
                        return Err(dispose_stage(stage, error).await);
                    }
                    // The full-cap fallback consumes both half slots, so resume at A.
                    next_slot = 0;
                    pending_fallback = Some((fallback, fallback_warnings));
                    None
                }
                Err(error) => return Err(dispose_stage(stage, error).await),
            }
        } else {
            None
        };

        // Phase A: remaining VLM(current) || render(next). A prefetched current already has its
        // snapshots from the previous phase-B overlap, so only the next render remains here.
        let snapshots = if let Some((next, next_warnings)) = next {
            if current
                .plan
                .bytes
                .checked_add(next.bytes)
                .is_none_or(|bytes| bytes > options.max_in_flight_image_bytes)
            {
                return Err(dispose_stage(
                    stage,
                    in_flight_image_limit(options.max_in_flight_image_bytes, usize::MAX),
                )
                .await);
            }
            let render = render_route_window(
                RenderRole::Prefetch,
                deadline,
                options.render_timeout,
                parsed.clone(),
                next,
                next_warnings,
                render_limits.clone(),
                render_workers,
                task_work_lease.clone(),
            );
            match pending_snapshots {
                Some(snapshots) => match render.await {
                    Ok(prefetched) => {
                        prefetch = Some(PrefetchState::Rendered(prefetched));
                        next_slot = 1 - next_slot;
                        Ok(snapshots)
                    }
                    Err(error) => Err(error),
                },
                None => {
                    match tokio::try_join!(deadline.future(vlm.expect("vlm pending")), render) {
                        Ok((snapshots, prefetched)) => {
                            prefetch = Some(PrefetchState::Rendered(prefetched));
                            next_slot = 1 - next_slot;
                            Ok(snapshots)
                        }
                        Err(error) => Err(error),
                    }
                }
            }
        } else {
            match pending_snapshots {
                Some(snapshots) => Ok(snapshots),
                None => deadline.future(vlm.expect("vlm pending")).await,
            }
        };
        let snapshots = match snapshots {
            Ok(snapshots) if snapshots.len() == current.rendered.len() => snapshots,
            Ok(_) => {
                return Err(dispose_stage(
                    stage,
                    VlmError::Pdf("VLM returned an unexpected page count".into()),
                )
                .await);
            }
            Err(error) => return Err(dispose_stage(stage, error).await),
        };

        // Phase B: stage(current) || deadline-wrapped VLM(next). Staging/publication stays source
        // ordered; the prefetched next window keeps the engine supplied for the whole staging
        // loop, so the staging-time inference bubble is gone. A staging failure disposes the stage
        // and drops the next-window VLM future; a VLM failure drops the staging future (the owned
        // stage's capability-only Drop schedules cleanup) and rolls back.
        let phase_b = match prefetch.take() {
            Some(PrefetchState::Rendered(next_window)) => {
                let next_images: Vec<_> = next_window
                    .rendered
                    .iter()
                    .map(|page| Arc::clone(&page.image))
                    .collect();
                let vlm_next = client
                    .official_two_step_snapshot_window_with_budgets_and_page_semaphore(
                        next_images,
                        options.image_analysis,
                        options.formula_enable,
                        options.table_enable,
                        options.max_layout_blocks_per_page,
                        options.max_semantic_requests_per_page,
                        options.max_requests_per_batch,
                        options.max_encoded_request_bytes,
                        options.max_encoded_batch_bytes,
                        Arc::clone(&encoded_budget),
                        Arc::clone(&raw_budget),
                        deadline.instant(),
                        page_concurrency.semaphore(),
                    );
                let staging = stage_window(
                    stage,
                    deadline,
                    &task_work_lease,
                    &events,
                    &stem,
                    total,
                    current,
                    snapshots,
                    completed,
                );
                match tokio::try_join!(staging, deadline.future(vlm_next)) {
                    Ok((staged, next_window_snapshots)) => {
                        if next_window_snapshots.len() != next_window.rendered.len() {
                            let error =
                                VlmError::Pdf("VLM returned an unexpected page count".into());
                            return Err(dispose_stage(staged.stage, error).await);
                        }
                        next_snapshots = Some(next_window_snapshots);
                        prefetch = Some(PrefetchState::Rendered(next_window));
                        Ok(staged)
                    }
                    Err(error) => Err(error),
                }
            }
            Some(PrefetchState::PendingFallback(_)) => {
                // A pending fallback is stored only after phase B completes, never during it.
                return Err(dispose_stage(
                    stage,
                    VlmError::Pdf("invalid prefetch state during phase B".into()),
                )
                .await);
            }
            None => {
                stage_window(
                    stage,
                    deadline,
                    &task_work_lease,
                    &events,
                    &stem,
                    total,
                    current,
                    snapshots,
                    completed,
                )
                .await
            }
        };
        match phase_b {
            Ok(staged) => {
                stage = staged.stage;
                completed = staged.completed;
            }
            Err(error) => return Err(error),
        }
        if let Some(fallback) = pending_fallback {
            prefetch = Some(PrefetchState::PendingFallback(fallback));
        }
    }

    if let Err(error) = deadline.check() {
        return Err(dispose_stage(stage, error).await);
    }
    let preview_limits = limits.clone();
    let preview_stem = stem.clone();
    let formula_enable = options.formula_enable;
    let table_enable = options.table_enable;
    let (stage, result) = deadline
        .blocking(&task_work_lease, move || {
            let result = (|| {
                stage.finalize_document(formula_enable, table_enable, deadline.instant())?;
                let preview_pages = stage.preview_pages()?;
                let preview_indexes = preview_pages
                    .iter()
                    .map(|page| page.page_index)
                    .collect::<Vec<_>>();
                deadline.check()?;
                let selected = pdf::extract_selected_pages_for_preview(&parsed, &preview_indexes)
                    .map_err(map)?;
                deadline.check()?;
                let preview = preview::generate_selected_until(
                    selected,
                    &preview_pages,
                    &preview_stem,
                    &preview_limits,
                    stage.remaining_asset_buffer_bytes(),
                    deadline.instant(),
                )
                .map_err(map)?;
                stage.write_preview(&preview)?;
                stage.assemble(deadline.instant())
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
    let committed = commit_stage(stage, deadline, &task_work_lease).await?;
    if committed.cleanup.failed() {
        if let Some(callback) = cleanup_warning {
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| callback()));
        }
    }
    Ok(committed.manifest)
}

async fn commit_stage(
    stage: OfficialOutputStage,
    deadline: RouteDeadline,
    task_work_lease: &TaskWorkLease,
) -> VlmResult<crate::official_output::OfficialCommit> {
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
) -> VlmResult<crate::official_output::OfficialCommit> {
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
        OfficialPageConcurrency, RenderRole, RenderTestInfo, RouteDeadline, WindowPlanMode,
        WindowRenderTestHook, admitted_commit, effective_page_concurrency,
        effective_render_workers, map, observe_route_render, plan_window, render_with_timeout,
        retain_window, scope_available_render_parallelism, scope_window_render_test_hook,
        split_image_slot_caps, window_render_test_hook,
    };
    use crate::{TaskWorkLease, official_output::OfficialOutputStage};
    use axum::{Json, Router, extract::State, routing::post};
    use bytes::Bytes;
    use lopdf::{Document, Object, Stream, dictionary};
    use serde_json::{Value, json};
    use std::{
        sync::{
            Arc, Condvar, Mutex,
            atomic::{AtomicBool, Ordering},
            mpsc,
        },
        time::{Duration, Instant},
    };
    use tokio::sync::{Notify, Semaphore};

    fn route_pdf(pages: usize) -> Bytes {
        route_pdf_sized(&vec![(1.0, 1.0); pages])
    }

    fn route_pdf_sized(sizes: &[(f32, f32)]) -> Bytes {
        let mut pdf = Document::with_version("1.5");
        let tree = pdf.new_object_id();
        let ids: Vec<_> = (0..sizes.len()).map(|_| pdf.new_object_id()).collect();
        for (id, (width, height)) in ids.iter().zip(sizes) {
            let contents = pdf.add_object(Stream::new(dictionary! {}, Vec::new()));
            pdf.objects.insert(*id, Object::Dictionary(dictionary! {
                "Type" => "Page", "Parent" => tree,
                "MediaBox" => vec![0.into(), 0.into(), (*width).into(), (*height).into()], "Contents" => contents,
            }));
        }
        pdf.objects.insert(tree, Object::Dictionary(dictionary! {
            "Type" => "Pages", "Kids" => ids.into_iter().map(Object::Reference).collect::<Vec<_>>(), "Count" => sizes.len() as i64,
        }));
        let catalog = pdf.add_object(dictionary! { "Type" => "Catalog", "Pages" => tree });
        pdf.trailer.set("Root", catalog);
        let mut bytes = Vec::new();
        pdf.save_to(&mut bytes).unwrap();
        bytes.into()
    }

    async fn route_client(app: Router) -> crate::MinerUVlmClient {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        crate::MinerUVlmClient::connect(
            crate::VlmHttpConfig {
                server_url: Some(format!("http://{address}").parse().unwrap()),
                model_name: Some("mock".into()),
                skip_model_name_checking: true,
                max_retries: 0,
                max_concurrency: 2,
                ..Default::default()
            },
            crate::MinerUVlmConfig {
                layout_image_size: (8, 8),
                ..Default::default()
            },
        )
        .await
        .unwrap()
    }

    fn route_options() -> crate::OfficialPdfOptions {
        crate::OfficialPdfOptions {
            processing_window_size: 1,
            max_in_flight_image_bytes: 1024 * 1024,
            max_rendered_image_bytes: 1024 * 1024,
            max_raw_output_bytes: 1024 * 1024,
            max_encoded_document_bytes: 1024 * 1024,
            max_encoded_request_bytes: 1024 * 1024,
            max_encoded_batch_bytes: 1024 * 1024,
            ..Default::default()
        }
    }

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

    fn render_test_hook(
        before: impl Fn(
            RenderTestInfo,
        ) -> std::pin::Pin<Box<dyn Future<Output = crate::VlmResult<()>> + Send>>
        + Send
        + Sync
        + 'static,
        after: impl Fn(RenderTestInfo, usize) + Send + Sync + 'static,
        on_drop: impl Fn(RenderTestInfo) + Send + Sync + 'static,
    ) -> Arc<WindowRenderTestHook> {
        Arc::new(WindowRenderTestHook {
            before: Arc::new(before),
            after: Arc::new(after),
            on_drop: Arc::new(on_drop),
        })
    }

    #[tokio::test]
    async fn route_prefetch_render_starts_while_current_vlm_is_blocked() {
        #[derive(Clone)]
        struct Mock {
            entered: Arc<Notify>,
            release: Arc<Notify>,
            layouts: Arc<std::sync::atomic::AtomicUsize>,
        }
        async fn handler(State(mock): State<Mock>, Json(_): Json<Value>) -> Json<Value> {
            if mock.layouts.fetch_add(1, Ordering::SeqCst) == 0 {
                mock.entered.notify_one();
                mock.release.notified().await;
            }
            Json(json!({"choices":[{"finish_reason":"stop","message":{"content":""}}]}))
        }
        let mock = Mock {
            entered: Arc::new(Notify::new()),
            release: Arc::new(Notify::new()),
            layouts: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        };
        let client = route_client(
            Router::new()
                .route("/v1/chat/completions", post(handler))
                .with_state(mock.clone()),
        )
        .await;
        let prefetch = Arc::new(Notify::new());
        let events = Arc::new(Mutex::new(Vec::new()));
        let hook = render_test_hook(
            {
                let prefetch = Arc::clone(&prefetch);
                let events = Arc::clone(&events);
                move |info| {
                    events.lock().unwrap().push(("before", info.role));
                    if info.role == RenderRole::Prefetch {
                        prefetch.notify_one();
                    }
                    Box::pin(async { Ok(()) })
                }
            },
            {
                let events = Arc::clone(&events);
                move |info, _| events.lock().unwrap().push(("after", info.role))
            },
            |_| {},
        );
        let output = tempfile::tempdir().unwrap();
        let task = tokio::spawn(scope_window_render_test_hook(hook, async move {
            client
                .parse_and_write_official_pdf(
                    crate::PdfInput::Bytes(route_pdf(2)),
                    route_options(),
                    output.path(),
                    "two",
                )
                .await
        }));
        tokio::time::timeout(Duration::from_secs(5), mock.entered.notified())
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(5), prefetch.notified())
            .await
            .unwrap();
        assert!(!task.is_finished());
        mock.release.notify_one();
        assert!(
            tokio::time::timeout(Duration::from_secs(5), task)
                .await
                .unwrap()
                .unwrap()
                .is_ok()
        );
        let events = events.lock().unwrap();
        let current_rendered = events
            .iter()
            .position(|event| *event == ("after", RenderRole::Current))
            .expect("current render completed");
        let prefetch_started = events
            .iter()
            .position(|event| *event == ("before", RenderRole::Prefetch))
            .expect("prefetch render started");
        assert!(current_rendered < prefetch_started);
    }

    #[tokio::test]
    async fn route_render_hook_proves_combined_rgb_high_water() {
        async fn handler(Json(_): Json<Value>) -> Json<Value> {
            Json(json!({"choices":[{"finish_reason":"stop","message":{"content":""}}]}))
        }
        let client = route_client(Router::new().route("/v1/chat/completions", post(handler))).await;
        let observed = Arc::new(Mutex::new(Vec::new()));
        let hook = render_test_hook(
            |_| Box::pin(async { Ok(()) }),
            {
                let observed = Arc::clone(&observed);
                move |info, actual| observed.lock().unwrap().push((info, actual))
            },
            |_| {},
        );
        let options = route_options();
        let output = tempfile::tempdir().unwrap();
        let manifest = scope_window_render_test_hook(
            hook,
            client.parse_and_write_official_pdf(
                crate::PdfInput::Bytes(route_pdf(2)),
                options.clone(),
                output.path(),
                "two",
            ),
        )
        .await
        .unwrap();
        let observed = observed.lock().unwrap();
        assert_eq!(observed.len(), 2);
        assert_eq!(observed[0].0.indexes, vec![0]);
        assert_eq!(observed[1].0.indexes, vec![1]);
        assert_eq!(observed[0].0.role, RenderRole::Current);
        assert_eq!(observed[1].0.role, RenderRole::Prefetch);
        assert!(
            observed
                .iter()
                .all(|(info, actual)| *actual > 0 && *actual == info.planned_bytes)
        );
        assert!(observed[0].1 + observed[1].1 <= options.max_in_flight_image_bytes);
        let middle: serde_json::Value = serde_json::from_slice(
            &std::fs::read(manifest.vlm_dir.join("two_middle.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(
            middle["pdf_info"]
                .as_array()
                .unwrap()
                .iter()
                .map(|page| page["page_idx"].as_u64())
                .collect::<Vec<_>>(),
            vec![Some(0), Some(1)]
        );
    }

    #[tokio::test]
    async fn route_reverse_render_completion_preserves_source_staging_order() {
        async fn handler(Json(_): Json<Value>) -> Json<Value> {
            Json(json!({"choices":[{"finish_reason":"stop","message":{"content":""}}]}))
        }

        assert_eq!(effective_render_workers(2, 2, 60), 2);
        let starts = Arc::new(Mutex::new(Vec::new()));
        let completions = Arc::new(Mutex::new(Vec::new()));
        let errors = Arc::new(Mutex::new(Vec::new()));
        let (release_zero, wait_for_one) = mpsc::sync_channel(1);
        let wait_for_one = Arc::new(Mutex::new(Some(wait_for_one)));
        let rest_gate = Arc::new((Mutex::new(false), Condvar::new()));
        let worker_hook = Arc::new(crate::pdf::PageRenderTestHook::new(
            {
                let starts = Arc::clone(&starts);
                let wait_for_one = Arc::clone(&wait_for_one);
                let rest_gate = Arc::clone(&rest_gate);
                move |index| {
                    starts.lock().expect("starts").push(index);
                    match index {
                        0 => wait_for_one
                            .lock()
                            .expect("zero gate")
                            .take()
                            .expect("one releases zero")
                            .recv()
                            .expect("one completed"),
                        1 => {}
                        _ => {
                            let (open, ready) = &*rest_gate;
                            let mut open = open.lock().expect("rest gate");
                            while !*open {
                                open = ready.wait(open).expect("rest gate");
                            }
                        }
                    }
                    Ok(())
                }
            },
            {
                let completions = Arc::clone(&completions);
                let errors = Arc::clone(&errors);
                let rest_gate = Arc::clone(&rest_gate);
                move |index, result| {
                    if let Err(error) = result {
                        errors.lock().expect("errors").push(error.to_string());
                    }
                    completions.lock().expect("completions").push(index);
                    match index {
                        1 if result.is_ok() => release_zero.send(()).expect("zero worker"),
                        0 if result.is_ok() => {
                            let (open, ready) = &*rest_gate;
                            *open.lock().expect("rest gate") = true;
                            ready.notify_all();
                        }
                        _ => {}
                    }
                }
            },
        ));
        let client = route_client(Router::new().route("/v1/chat/completions", post(handler))).await;
        let mut options = route_options();
        options.processing_window_size = 60;
        options.render_workers = 2;
        let output = tempfile::tempdir().unwrap();
        let manifest = tokio::time::timeout(
            Duration::from_secs(30),
            scope_available_render_parallelism(
                2,
                crate::pdf::scope_page_render_test_hook(
                    worker_hook,
                    client.parse_and_write_official_pdf(
                        crate::PdfInput::Bytes(route_pdf(60)),
                        options,
                        output.path(),
                        "ordered",
                    ),
                ),
            ),
        )
        .await
        .expect("route timed out")
        .expect("route");

        let starts = starts.lock().expect("starts");
        assert!(starts.contains(&0));
        assert!(starts.contains(&1));
        drop(starts);
        let completions = completions.lock().expect("completions");
        assert_eq!(&completions[..2], &[1, 0]);
        let mut completed = completions.clone();
        completed.sort_unstable();
        assert_eq!(completed, (0..60).collect::<Vec<_>>());
        drop(completions);
        assert!(errors.lock().expect("errors").is_empty());
        let middle: Value = serde_json::from_slice(
            &std::fs::read(manifest.vlm_dir.join("ordered_middle.json")).expect("middle"),
        )
        .expect("middle JSON");
        let pages = middle["pdf_info"].as_array().expect("pdf_info");
        assert_eq!(pages.len(), 60);
        assert_eq!(
            pages
                .iter()
                .map(|page| page["page_idx"].as_u64().expect("page index"))
                .collect::<Vec<_>>(),
            (0..60).map(|index| index as u64).collect::<Vec<_>>(),
        );
    }

    #[tokio::test]
    async fn route_prefetch_failure_cancels_current_and_cleans_stage() {
        #[derive(Clone)]
        struct Mock {
            current_entered: Arc<Notify>,
            release_current: Arc<Notify>,
            response_sent: Arc<AtomicBool>,
            layouts: Arc<std::sync::atomic::AtomicUsize>,
            worker_active: mpsc::SyncSender<()>,
        }
        async fn handler(State(mock): State<Mock>, Json(_): Json<Value>) -> Json<Value> {
            if mock.layouts.fetch_add(1, Ordering::SeqCst) == 0 {
                mock.current_entered.notify_one();
                mock.worker_active
                    .send(())
                    .expect("prefetch worker is waiting");
                mock.release_current.notified().await;
                mock.response_sent.store(true, Ordering::SeqCst);
            }
            Json(json!({"choices":[{"finish_reason":"stop","message":{"content":""}}]}))
        }

        let (worker_active, worker_wait) = mpsc::sync_channel(1);
        let worker_wait = Arc::new(Mutex::new(Some(worker_wait)));
        let worker_entered = Arc::new(Notify::new());
        let worker_finished = Arc::new(Notify::new());
        let worker_events = Arc::new(Mutex::new(Vec::new()));
        let current_entered = Arc::new(Notify::new());
        let release_current = Arc::new(Notify::new());
        let response_sent = Arc::new(AtomicBool::new(false));
        let layouts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mock = Mock {
            current_entered: Arc::clone(&current_entered),
            release_current: Arc::clone(&release_current),
            response_sent: Arc::clone(&response_sent),
            layouts: Arc::clone(&layouts),
            worker_active,
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (stop_server, server_stopped) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            axum::serve(
                listener,
                Router::new()
                    .route("/v1/chat/completions", post(handler))
                    .with_state(mock),
            )
            .with_graceful_shutdown(async {
                let _ = server_stopped.await;
            })
            .await
            .expect("server");
        });

        let lease_semaphore = Arc::new(tokio::sync::Semaphore::new(1));
        let root_lease = TaskWorkLease::from_permit(
            Arc::clone(&lease_semaphore)
                .acquire_owned()
                .await
                .expect("lease permit"),
        );
        let client = crate::MinerUVlmClient::connect_for_task(
            crate::VlmHttpConfig {
                server_url: Some(format!("http://{address}").parse().unwrap()),
                model_name: Some("mock".into()),
                skip_model_name_checking: true,
                max_retries: 0,
                max_concurrency: 2,
                ..Default::default()
            },
            crate::MinerUVlmConfig {
                layout_image_size: (8, 8),
                ..Default::default()
            },
            root_lease,
        )
        .await
        .expect("client");
        let route_events = Arc::new(Mutex::new(Vec::new()));
        let route_hook = render_test_hook(
            {
                let route_events = Arc::clone(&route_events);
                move |info| {
                    route_events
                        .lock()
                        .expect("route events")
                        .push(("before", info.role));
                    Box::pin(async { Ok(()) })
                }
            },
            {
                let route_events = Arc::clone(&route_events);
                move |info, _| {
                    route_events
                        .lock()
                        .expect("route events")
                        .push(("after", info.role))
                }
            },
            {
                let route_events = Arc::clone(&route_events);
                move |info| {
                    route_events
                        .lock()
                        .expect("route events")
                        .push(("drop", info.role))
                }
            },
        );
        let worker_hook = Arc::new(crate::pdf::PageRenderTestHook::new(
            {
                let worker_wait = Arc::clone(&worker_wait);
                let worker_entered = Arc::clone(&worker_entered);
                move |index| match index {
                    0 => Ok(()),
                    1 => {
                        worker_entered.notify_one();
                        worker_wait
                            .lock()
                            .expect("worker wait")
                            .take()
                            .expect("one prefetch worker")
                            .recv()
                            .expect("current VLM handler");
                        Err(crate::Error::Pdf("injected prefetch render failure".into()))
                    }
                    _ => panic!("later page {index} was admitted after prefetch failure"),
                }
            },
            {
                let worker_events = Arc::clone(&worker_events);
                let worker_finished = Arc::clone(&worker_finished);
                move |index, result| {
                    worker_events
                        .lock()
                        .expect("worker events")
                        .push((index, result.as_ref().err().map(ToString::to_string)));
                    // Only the prefetch worker's completion may wake the test; the current page
                    // (index 0) completes early and would otherwise resolve the wait before the
                    // prefetch event is pushed, racing the assertion below.
                    if index == 1 {
                        worker_finished.notify_one();
                    }
                }
            },
        ));
        let output = tempfile::tempdir().unwrap();
        let output_root = output.path().to_path_buf();
        let route = tokio::spawn(scope_window_render_test_hook(
            route_hook,
            crate::pdf::scope_page_render_test_hook(worker_hook, async move {
                client
                    .parse_and_write_official_pdf(
                        crate::PdfInput::Bytes(route_pdf(3)),
                        route_options(),
                        &output_root,
                        "failed",
                    )
                    .await
            }),
        ));

        tokio::time::timeout(Duration::from_secs(5), current_entered.notified())
            .await
            .expect("current VLM did not enter");
        tokio::time::timeout(Duration::from_secs(5), worker_entered.notified())
            .await
            .expect("prefetch worker did not enter");
        tokio::time::timeout(Duration::from_secs(5), worker_finished.notified())
            .await
            .expect("prefetch worker did not finish");
        assert!(!response_sent.load(Ordering::SeqCst));
        let worker_events = worker_events.lock().expect("worker events");
        assert!(worker_events.contains(&(
            1,
            Some("PDF error: injected prefetch render failure".into())
        )));
        assert!(!worker_events.iter().any(|(index, _)| *index == 2));
        drop(worker_events);

        let result = tokio::time::timeout(Duration::from_secs(5), route)
            .await
            .expect("route did not stop after prefetch failure")
            .expect("route task");
        assert!(matches!(
            result,
            Err(crate::VlmError::Pdf(message)) if message == "injected prefetch render failure"
        ));
        assert_eq!(layouts.load(Ordering::SeqCst), 1);
        let route_events = route_events.lock().expect("route events");
        assert!(route_events.contains(&("before", RenderRole::Prefetch)));
        assert!(route_events.contains(&("drop", RenderRole::Prefetch)));
        assert!(!route_events.contains(&("after", RenderRole::Prefetch)));
        drop(route_events);
        assert!(!output.path().join("failed/vlm").exists());
        if let Ok(entries) = std::fs::read_dir(output.path().join("failed")) {
            assert!(!entries.flatten().any(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".vlm-staging-parent-")
            }));
        }
        let _permit = tokio::time::timeout(Duration::from_secs(5), lease_semaphore.acquire_owned())
            .await
            .expect("tracked task work did not drain")
            .expect("lease semaphore closed");

        release_current.notify_one();
        let _ = stop_server.send(());
        tokio::time::timeout(Duration::from_secs(5), server)
            .await
            .expect("server did not stop")
            .expect("server task");
    }

    #[tokio::test]
    async fn route_partial_render_failure_degrades_to_placeholder_and_continues() {
        async fn handler(Json(_): Json<Value>) -> Json<Value> {
            Json(json!({"choices":[{"finish_reason":"stop","message":{"content":""}}]}))
        }
        let client =
            route_client(Router::new().route("/v1/chat/completions", post(handler))).await;
        let events = Arc::new(Mutex::new(Vec::new()));
        let callback: crate::ProgressCallback = {
            let events = Arc::clone(&events);
            Arc::new(move |event| events.lock().unwrap().push(event))
        };
        let worker_hook = Arc::new(crate::pdf::PageRenderTestHook::new(
            |index| {
                if index == 1 {
                    Err(crate::Error::Pdf("injected page render failure".into()))
                } else {
                    Ok(())
                }
            },
            |_, _| {},
        ));
        let mut options = route_options();
        options.processing_window_size = 3;
        let pdf = route_pdf(3);
        let output = tempfile::tempdir().unwrap();
        let output_root = output.path().to_path_buf();
        let manifest = tokio::time::timeout(
            Duration::from_secs(30),
            scope_window_render_test_hook(
                render_test_hook(|_| Box::pin(async { Ok(()) }), |_, _| {}, |_| {}),
                crate::pdf::scope_page_render_test_hook(worker_hook, async move {
                    client
                        .parse_and_write_prepared_pdf_with_events(
                            crate::input_prepare::PreparedPdf {
                                bytes: pdf.clone(),
                                kind: crate::input_prepare::DocumentKind::Pdf,
                                original: pdf,
                            },
                            options,
                            &output_root,
                            "partial",
                            Some(callback),
                        )
                        .await
                }),
            ),
        )
        .await
        .expect("route timed out")
        .expect("route");

        // Every source page index stays in the manifest; the failed page is an empty placeholder.
        let middle: Value = serde_json::from_slice(
            &std::fs::read(manifest.vlm_dir.join("partial_middle.json")).expect("middle"),
        )
        .expect("middle JSON");
        let pages = middle["pdf_info"].as_array().expect("pdf_info");
        assert_eq!(
            pages
                .iter()
                .map(|page| page["page_idx"].as_u64().expect("page index"))
                .collect::<Vec<_>>(),
            vec![0, 1, 2],
        );
        let messages: Vec<_> = events
            .lock()
            .unwrap()
            .iter()
            .filter_map(|event| match event {
                crate::ProgressEvent::VlmWarning { message } => Some(message.clone()),
                _ => None,
            })
            .collect();
        assert!(
            messages.iter().any(|message| message.contains("page 1 failed")),
            "{messages:?}"
        );
    }

    #[tokio::test]
    async fn route_unrenderable_page_is_skipped_with_warning_and_others_continue() {
        async fn handler(Json(_): Json<Value>) -> Json<Value> {
            Json(json!({"choices":[{"finish_reason":"stop","message":{"content":""}}]}))
        }
        let client =
            route_client(Router::new().route("/v1/chat/completions", post(handler))).await;
        let events = Arc::new(Mutex::new(Vec::new()));
        let callback: crate::ProgressCallback = {
            let events = Arc::clone(&events);
            Arc::new(move |event| events.lock().unwrap().push(event))
        };
        // Page 1 is 1,000,000 x 1 pt: even at 200 DPI its viewport exceeds Hayro's u16 limit,
        // so it is unrenderable and must be skipped with a warning rather than failing the
        // document; pages 0 and 2 still render.
        let pdf = route_pdf_sized(&[(1.0, 1.0), (1_000_000.0, 1.0), (1.0, 1.0)]);
        let mut options = route_options();
        options.processing_window_size = 3;
        let output = tempfile::tempdir().unwrap();
        let output_root = output.path().to_path_buf();
        let manifest = tokio::time::timeout(
            Duration::from_secs(30),
            scope_window_render_test_hook(
                render_test_hook(|_| Box::pin(async { Ok(()) }), |_, _| {}, |_| {}),
                async move {
                    client
                        .parse_and_write_prepared_pdf_with_events(
                            crate::input_prepare::PreparedPdf {
                                bytes: pdf.clone(),
                                kind: crate::input_prepare::DocumentKind::Pdf,
                                original: pdf,
                            },
                            options,
                            &output_root,
                            "skipped",
                            Some(callback),
                        )
                        .await
                },
            ),
        )
        .await
        .expect("route timed out")
        .expect("route");

        let middle: Value = serde_json::from_slice(
            &std::fs::read(manifest.vlm_dir.join("skipped_middle.json")).expect("middle"),
        )
        .expect("middle JSON");
        let pages = middle["pdf_info"].as_array().expect("pdf_info");
        assert_eq!(
            pages
                .iter()
                .map(|page| page["page_idx"].as_u64().expect("page index"))
                .collect::<Vec<_>>(),
            vec![0, 2],
            "the unrenderable page is dropped, the rest continue",
        );
        let messages: Vec<_> = events
            .lock()
            .unwrap()
            .iter()
            .filter_map(|event| match event {
                crate::ProgressEvent::VlmWarning { message } => Some(message.clone()),
                _ => None,
            })
            .collect();
        assert!(
            messages.iter().any(|message| message.contains("page 1 skipped")),
            "{messages:?}"
        );
    }

    #[tokio::test]
    async fn route_entire_window_unrenderable_is_a_hard_error() {
        async fn handler(Json(_): Json<Value>) -> Json<Value> {
            Json(json!({"choices":[{"finish_reason":"stop","message":{"content":""}}]}))
        }
        let client =
            route_client(Router::new().route("/v1/chat/completions", post(handler))).await;
        // Every page is unrenderable (viewport beyond Hayro's u16 limit): the window must still
        // hard-error instead of producing an empty placeholder document.
        let pdf = route_pdf_sized(&[(1_000_000.0, 1.0)]);
        let output = tempfile::tempdir().unwrap();
        let output_root = output.path().to_path_buf();
        let result = tokio::time::timeout(
            Duration::from_secs(30),
            scope_window_render_test_hook(
                render_test_hook(|_| Box::pin(async { Ok(()) }), |_, _| {}, |_| {}),
                async move {
                    client
                        .parse_and_write_official_pdf(
                            crate::PdfInput::Bytes(pdf),
                            route_options(),
                            &output_root,
                            "whole-skip",
                        )
                        .await
                },
            ),
        )
        .await
        .expect("route timed out");

        assert!(matches!(
            result,
            Err(crate::VlmError::Pdf(message)) if message.contains("u16 limit")
        ));
        assert!(!output.path().join("whole-skip/vlm").exists());
    }

    #[tokio::test]
    async fn route_whole_window_render_failure_is_a_hard_error() {
        async fn handler(Json(_): Json<Value>) -> Json<Value> {
            Json(json!({"choices":[{"finish_reason":"stop","message":{"content":""}}]}))
        }
        let client =
            route_client(Router::new().route("/v1/chat/completions", post(handler))).await;
        let worker_hook = Arc::new(crate::pdf::PageRenderTestHook::new(
            |_| Err(crate::Error::Pdf("injected window render failure".into())),
            |_, _| {},
        ));
        let output = tempfile::tempdir().unwrap();
        let output_root = output.path().to_path_buf();
        let pdf = route_pdf(1);
        let result = tokio::time::timeout(
            Duration::from_secs(30),
            scope_window_render_test_hook(
                render_test_hook(|_| Box::pin(async { Ok(()) }), |_, _| {}, |_| {}),
                crate::pdf::scope_page_render_test_hook(worker_hook, async move {
                    client
                        .parse_and_write_official_pdf(
                            crate::PdfInput::Bytes(pdf),
                            route_options(),
                            &output_root,
                            "whole-fail",
                        )
                        .await
                }),
            ),
        )
        .await
        .expect("route timed out");

        assert!(matches!(
            result,
            Err(crate::VlmError::Pdf(message)) if message == "injected window render failure"
        ));
        assert!(!output.path().join("whole-fail/vlm").exists());
    }

    #[tokio::test]
    async fn render_test_hook_is_task_local_and_scope_isolated() {
        let first = render_test_hook(|_| Box::pin(async { Ok(()) }), |_, _| {}, |_| {});
        let second = render_test_hook(|_| Box::pin(async { Ok(()) }), |_, _| {}, |_| {});
        let first_ptr = Arc::as_ptr(&first) as usize;
        let second_ptr = Arc::as_ptr(&second) as usize;
        let (seen_first, seen_second) = tokio::join!(
            scope_window_render_test_hook(first, async {
                Arc::as_ptr(&window_render_test_hook().expect("first hook")) as usize
            }),
            scope_window_render_test_hook(second, async {
                Arc::as_ptr(&window_render_test_hook().expect("second hook")) as usize
            }),
        );

        assert_eq!(seen_first, first_ptr);
        assert_eq!(seen_second, second_ptr);
        assert!(window_render_test_hook().is_none());
    }

    #[tokio::test]
    async fn render_test_hook_reports_start_completion_and_actual_rgb_bytes_once() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let hook = render_test_hook(
            {
                let events = Arc::clone(&events);
                move |info| {
                    events.lock().expect("events").push(("before", info, 0));
                    Box::pin(async { Ok(()) })
                }
            },
            {
                let events = Arc::clone(&events);
                move |info, bytes| events.lock().expect("events").push(("after", info, bytes))
            },
            |_| panic!("completed render must not report drop"),
        );
        let info = RenderTestInfo {
            role: RenderRole::Prefetch,
            indexes: vec![3, 4],
            planned_bytes: 17,
        };

        let value = observe_route_render(Some(hook), info.clone(), async { Ok(7usize) }, |value| {
            Ok(*value * 3)
        })
        .await
        .expect("render observation");

        assert_eq!(value, 7);
        assert_eq!(
            *events.lock().expect("events"),
            vec![("before", info.clone(), 0), ("after", info, 21)]
        );
    }

    #[tokio::test]
    async fn dropped_controlled_render_reports_drop_without_completion() {
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let started_tx = Arc::new(Mutex::new(Some(started_tx)));
        let drops = Arc::new(Mutex::new(Vec::new()));
        let after = Arc::new(AtomicBool::new(false));
        let hook = render_test_hook(
            {
                let started_tx = Arc::clone(&started_tx);
                move |_| {
                    if let Some(started_tx) = started_tx.lock().expect("start sender").take() {
                        let _ = started_tx.send(());
                    }
                    Box::pin(std::future::pending())
                }
            },
            {
                let after = Arc::clone(&after);
                move |_, _| after.store(true, Ordering::SeqCst)
            },
            {
                let drops = Arc::clone(&drops);
                move |info| drops.lock().expect("drops").push(info)
            },
        );
        let info = RenderTestInfo {
            role: RenderRole::Current,
            indexes: vec![8],
            planned_bytes: 9,
        };
        let task = tokio::spawn(observe_route_render(
            Some(hook),
            info.clone(),
            async { Ok(()) },
            |_| Ok(0),
        ));

        started_rx.await.expect("before started");
        task.abort();
        let _ = task.await;
        assert_eq!(*drops.lock().expect("drops"), vec![info]);
        assert!(!after.load(Ordering::SeqCst));
    }

    #[test]
    fn window_planner_preserves_order_count_bound_and_byte_sum() {
        let (window, warnings) = plan_window(&[7, 3, 9], 0, 2, 16, 16, |index| {
            Ok(match index {
                7 => 4,
                3 => 5,
                9 => 6,
                _ => unreachable!(),
            })
        })
        .expect("window");
        assert!(warnings.is_empty());

        assert_eq!(window.indexes, vec![7, 3]);
        assert_eq!(window.bytes, 9);
        assert_eq!(window.mode, WindowPlanMode::Slot);
        assert_eq!(window.consumed, 2);
    }

    #[test]
    fn window_planner_admits_slot_boundary_and_stops_before_next_page() {
        let (window, warnings) =
            plan_window(&[0, 1, 2], 0, 3, 10, 10, |index| Ok([7, 3, 1][index]))
                .expect("window");
        assert!(warnings.is_empty());
        assert_eq!(window.indexes, vec![0, 1]);
        assert_eq!(window.bytes, 10);

        let (next, warnings) = plan_window(&[0, 1, 2], 2, 3, 10, 10, |index| Ok([7, 3, 1][index]))
            .expect("next window");
        assert!(warnings.is_empty());
        assert_eq!(next.indexes, vec![2]);
        assert_eq!(next.bytes, 1);
    }

    #[test]
    fn window_planner_uses_single_page_full_cap_fallback_then_resumes() {
        let (window, warnings) = plan_window(&[4, 5], 0, 2, 5, 10, |index| Ok([6, 2][index - 4]))
            .expect("fallback window");
        assert!(warnings.is_empty());
        assert_eq!(window.indexes, vec![4]);
        assert_eq!(window.bytes, 6);
        assert_eq!(window.mode, WindowPlanMode::FullCapFallback);

        let (next, warnings) = plan_window(&[4, 5], 1, 2, 5, 10, |index| Ok([6, 2][index - 4]))
            .expect("resumed window");
        assert!(warnings.is_empty());
        assert_eq!(next.indexes, vec![5]);
        assert_eq!(next.bytes, 2);
        assert_eq!(next.mode, WindowPlanMode::Slot);
    }

    #[test]
    fn window_planner_skips_full_cap_pages_with_warning() {
        let mut looked_up = Vec::new();
        let (window, warnings) = plan_window(&[12, 13], 0, 2, 5, 10, |index| {
            looked_up.push(index);
            Ok(if index == 12 { 11 } else { 3 })
        })
        .expect("a page over the full cap is skipped, not fatal");

        assert_eq!(looked_up, vec![12, 13]);
        assert_eq!(window.indexes, vec![13]);
        assert_eq!(window.bytes, 3);
        assert_eq!(window.mode, WindowPlanMode::Slot);
        assert_eq!(window.consumed, 2);
        assert_eq!(
            warnings,
            vec!["page 12 skipped: page exceeds the in-flight image byte budget (11 bytes)".to_owned()]
        );
    }

    #[test]
    fn window_planner_skips_pages_whose_byte_estimate_fails_with_warning() {
        let (window, warnings) = plan_window(&[0, 1, 2], 0, 3, 16, 16, |index| {
            if index == 1 {
                Err(crate::VlmError::Pdf("page 1 has invalid dimensions".into()))
            } else {
                Ok(match index {
                    0 => 4,
                    2 => 6,
                    _ => unreachable!(),
                })
            }
        })
        .expect("an unrenderable page is skipped, not fatal");

        assert_eq!(window.indexes, vec![0, 2]);
        assert_eq!(window.bytes, 10);
        assert_eq!(window.mode, WindowPlanMode::Slot);
        assert_eq!(window.consumed, 3);
        assert_eq!(
            warnings,
            vec!["page 1 skipped: PDF error: page 1 has invalid dimensions".to_owned()]
        );
    }

    #[test]
    fn window_planner_hard_errors_when_every_page_is_skipped() {
        let error = plan_window(&[0, 1], 0, 2, 16, 16, |_| {
            Err(crate::VlmError::Pdf("unrenderable page".into()))
        })
        .expect_err("an all-skipped window must stay a hard error");

        assert!(matches!(
            error,
            crate::VlmError::Pdf(message) if message == "unrenderable page"
        ));
    }

    #[test]
    fn image_slot_caps_split_odd_and_tiny_full_caps_without_overflow() {
        assert_eq!(split_image_slot_caps(1), (0, 1));
        let (first, second) = split_image_slot_caps(11);
        assert_eq!((first, second), (5, 6));
        assert_eq!(first + second, 11);
        let (first, second) = split_image_slot_caps(usize::MAX);
        assert_eq!(first, usize::MAX / 2);
        assert_eq!(second, usize::MAX - first);
        assert_eq!(first.checked_add(second), Some(usize::MAX));
    }

    #[test]
    fn window_planner_rejects_overflowing_hostile_byte_estimates() {
        let error = plan_window(&[0, 1], 0, 2, usize::MAX, usize::MAX, |index| {
            Ok(if index == 0 { usize::MAX } else { 1 })
        })
        .expect_err("overflow must not wrap into an empty or undercounted window");

        assert!(matches!(
            error,
            crate::VlmError::LimitExceeded {
                resource: "in-flight image bytes",
                limit,
                actual: u64::MAX,
            } if limit == usize::MAX as u64
        ));
    }

    #[test]
    fn window_ownership_three_window_trace_advances_cursor_once_per_acceptance() {
        let indexes = [0, 1, 2, 3, 4, 5];
        let mut cursor = 0;
        let mut trace = Vec::new();

        let (current, _) = plan_window(&indexes, cursor, 2, 2, 4, |_| Ok(1)).expect("current");
        retain_window(&mut cursor, &current).expect("retain current");
        trace.extend(current.indexes.iter().copied());

        let (prefetch, _) = plan_window(&indexes, cursor, 2, 2, 4, |_| Ok(1)).expect("prefetch");
        retain_window(&mut cursor, &prefetch).expect("retain prefetch");
        trace.extend(prefetch.indexes.iter().copied());
        let cursor_after_prefetch = cursor;
        let promoted = prefetch; // Promotion moves ownership; it never changes the cursor.
        assert_eq!(cursor, cursor_after_prefetch);

        let (next, _) = plan_window(&indexes, cursor, 2, 2, 4, |_| Ok(1)).expect("next");
        retain_window(&mut cursor, &next).expect("retain next");
        trace.extend(next.indexes.iter().copied());

        assert_eq!(promoted.indexes, vec![2, 3]);
        assert_eq!(trace, indexes);
        assert_eq!(cursor, indexes.len());
    }

    #[test]
    fn last_prefetched_window_is_promoted_without_extra_planning() {
        let indexes = [0, 1, 2, 3];
        let mut cursor = 0;
        let (current, _) = plan_window(&indexes, cursor, 2, 2, 4, |_| Ok(1)).expect("current");
        retain_window(&mut cursor, &current).expect("retain current");
        let (prefetch, _) = plan_window(&indexes, cursor, 2, 2, 4, |_| Ok(1)).expect("prefetch");
        retain_window(&mut cursor, &prefetch).expect("retain prefetch");

        let promoted = prefetch;
        assert_eq!(current.indexes, vec![0, 1]);
        assert_eq!(promoted.indexes, vec![2, 3]);
        assert_eq!(cursor, indexes.len());
    }

    #[test]
    fn fallback_ownership_is_sequential_then_slot_a_resumes() {
        let indexes = [0, 1, 2, 3];
        let bytes = [2, 6, 2, 2];
        let mut cursor = 0;
        let (current, _) = plan_window(&indexes, cursor, 2, 5, 10, |index| Ok(bytes[index]))
            .expect("current slot");
        retain_window(&mut cursor, &current).expect("retain current");
        let (fallback, _) =
            plan_window(&indexes, cursor, 2, 5, 10, |index| Ok(bytes[index])).expect("fallback");
        retain_window(&mut cursor, &fallback).expect("retain fallback");
        let cursor_after_fallback = cursor;
        let promoted = fallback;
        assert_eq!(cursor, cursor_after_fallback);

        let (resumed, _) = plan_window(&indexes, cursor, 2, 5, 10, |index| Ok(bytes[index]))
            .expect("resumed slot A");
        assert_eq!(current.indexes, vec![0]);
        assert_eq!(promoted.mode, WindowPlanMode::FullCapFallback);
        assert_eq!(promoted.indexes, vec![1]);
        assert_eq!(resumed.indexes, vec![2, 3]);
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
        // No arbitrary 3-worker or /30 page ceiling: only CPU, configured, and page capacities.
        assert_eq!(effective_render_workers(8, 8, 29), 8);
        assert_eq!(effective_render_workers(8, 8, 30), 8);
        assert_eq!(effective_render_workers(8, 8, 59), 8);
        assert_eq!(effective_render_workers(8, 8, 60), 8);
        assert_eq!(effective_render_workers(8, 8, 89), 8);
        assert_eq!(effective_render_workers(8, 8, 90), 8);
        assert_eq!(effective_render_workers(1, 8, 90), 1);
        assert_eq!(effective_render_workers(8, 1, 90), 1);
        assert_eq!(effective_render_workers(8, 99, 90), 8);
        assert_eq!(effective_render_workers(16, 16, 100), 16);
        assert_eq!(effective_render_workers(64, 64, 200), 64);
        assert_eq!(effective_render_workers(64, 64, 3), 3);
    }

    #[test]
    fn page_concurrency_obeys_window_and_http_bounds_and_rejects_unrepresentable() {
        assert_eq!(effective_page_concurrency(4, 2, 8), 2);
        assert_eq!(effective_page_concurrency(4, 8, 3), 3);
        assert_eq!(effective_page_concurrency(4, 8, 8), 4);
        // Values above the removed 1..=8 ceiling flow through and meet actual capacities.
        assert_eq!(effective_page_concurrency(16, 8, 8), 8);
        // No silent clamp: the pure derivation is min(...), and the tokio-capacity guard lives
        // in `OfficialPageConcurrency::new`, where explicit values fail.
        assert_eq!(
            effective_page_concurrency(usize::MAX, usize::MAX, usize::MAX),
            usize::MAX
        );
        assert!(OfficialPageConcurrency::new(usize::MAX, usize::MAX, usize::MAX).is_err());
        assert!(
            OfficialPageConcurrency::new(Semaphore::MAX_PERMITS, usize::MAX, usize::MAX).is_ok()
        );
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
            u64::MAX,
            u64::MAX,
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
            u64::MAX,
            u64::MAX,
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
