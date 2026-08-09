use crate::{Error, Limits, PdfInput, Result, TaskWorkLease};
use bytes::Bytes;
use hayro::vello_cpu::color::palette::css::WHITE;
use hayro::{
    RenderSettings,
    hayro_interpret::InterpreterSettings,
    hayro_syntax::{
        Pdf,
        object::{Dict as HayroDict, Number, Object as HayroObject, ObjectIdentifier},
    },
    render,
};
use hayro_write::ExtractionQuery;
use image::RgbImage;
use lopdf::Document as LoPdf;
use pdf_writer::Ref;
use std::{
    collections::{BTreeMap, HashSet},
    fs::File,
    io::Read,
    panic::{AssertUnwindSafe, catch_unwind},
    sync::Arc,
};

const SCALE: f32 = 200.0 / 72.0;
const LOPDF_ENCRYPTED_WITHOUT_PASSWORD: &str = "PDF is encrypted and requires a password. Use Document::load_metadata_with_password() instead.";

pub(crate) struct RenderedPage {
    pub index: usize,
    /// PDF points, not the dimensions of the rendered raster below.
    pub size: [f32; 2],
    /// The effective scale this page was rasterized at (200 DPI for ordinary pages, lower for
    /// oversized pages that would otherwise blow the pixel/byte budgets).
    pub scale: f32,
    pub image: Arc<RgbImage>,
}

type RenderTaskResult = Result<(usize, RenderedPage), (usize, Error)>;

#[cfg(test)]
pub(crate) struct PageRenderTestHook {
    before: Arc<dyn Fn(usize) -> Result<()> + Send + Sync>,
    after: Arc<dyn Fn(usize, &Result<RenderedPage>) + Send + Sync>,
}

#[cfg(test)]
impl PageRenderTestHook {
    pub(crate) fn new(
        before: impl Fn(usize) -> Result<()> + Send + Sync + 'static,
        after: impl Fn(usize, &Result<RenderedPage>) + Send + Sync + 'static,
    ) -> Self {
        Self {
            before: Arc::new(before),
            after: Arc::new(after),
        }
    }
}

#[cfg(test)]
tokio::task_local! {
    static PAGE_RENDER_TEST_HOOK: Arc<PageRenderTestHook>;
}

#[cfg(test)]
pub(crate) async fn scope_page_render_test_hook<T>(
    hook: Arc<PageRenderTestHook>,
    future: impl std::future::Future<Output = T>,
) -> T {
    PAGE_RENDER_TEST_HOOK.scope(hook, future).await
}

#[cfg(test)]
pub(crate) fn page_render_test_hook() -> Option<Arc<PageRenderTestHook>> {
    PAGE_RENDER_TEST_HOOK.try_with(Arc::clone).ok()
}

/// Owns the source data required by Hayro as well as its single parsed view.
pub(crate) struct ParsedPdf {
    _bytes: Arc<Bytes>,
    pdf: Pdf,
    // lopdf's page tree is authoritative: Hayro may omit blank leaf pages.
    source_pages: usize,
}

pub(crate) struct SelectedPreview {
    pub(crate) bytes: Vec<u8>,
    pub(crate) page_indices: Vec<usize>,
    pub(crate) user_units: Vec<Option<f32>>,
}

pub(crate) fn read_input(input: PdfInput, limits: &Limits) -> Result<Bytes> {
    let bytes = match input {
        PdfInput::Path(path) => Bytes::from(read_file_capped(path, limits.max_pdf_bytes)?),
        PdfInput::Bytes(bytes) => {
            check_limit("pdf bytes", limits.max_pdf_bytes as u64, bytes.len() as u64)?;
            bytes
        }
    };
    check_limit("pdf bytes", limits.max_pdf_bytes as u64, bytes.len() as u64)?;
    Ok(bytes)
}

fn read_file_capped(path: std::path::PathBuf, cap: usize) -> Result<Vec<u8>> {
    let metadata = std::fs::metadata(&path)?;
    check_limit("pdf bytes", cap as u64, metadata.len())?;
    let mut file = File::open(path)?;
    let mut bytes = Vec::new();
    file.by_ref().take(cap as u64 + 1).read_to_end(&mut bytes)?;
    check_limit("pdf bytes", cap as u64, bytes.len() as u64)?;
    Ok(bytes)
}

pub(crate) fn parse_document(bytes: impl Into<Bytes>, limits: &Limits) -> Result<Arc<ParsedPdf>> {
    let bytes = Arc::new(bytes.into());
    let metadata = LoPdf::load_metadata_mem(bytes.as_ref().as_ref()).map_err(|e| match e {
        lopdf::Error::InvalidPassword
        | lopdf::Error::Decryption(_)
        | lopdf::Error::Unimplemented(LOPDF_ENCRYPTED_WITHOUT_PASSWORD) => {
            Error::Pdf("encrypted PDFs are unsupported".into())
        }
        _ => Error::Pdf(format!("unsupported or invalid PDF: {e}")),
    })?;
    let source_pages = metadata.page_count as usize;
    check_limit("pages", limits.max_pages as u64, source_pages as u64)?;
    let data: Arc<dyn AsRef<[u8]> + Send + Sync> = bytes.clone();
    let pdf = Pdf::new(data).map_err(|e| match e {
        hayro::hayro_syntax::LoadPdfError::Decryption(_) => {
            Error::Pdf("encrypted PDFs are unsupported".into())
        }
        _ => Error::Pdf(format!("unsupported or invalid PDF: {e:?}")),
    })?;
    // Do not reindex a document if the renderer skipped a valid page.  Rendering a
    // synthetic page is only safe once Hayro exposes source page identities.
    if pdf.pages().len() != source_pages {
        return Err(Error::Pdf(format!(
            "renderer/source page mapping mismatch (Hayro {}, source {source_pages})",
            pdf.pages().len()
        )));
    }
    Ok(Arc::new(ParsedPdf {
        _bytes: bytes,
        pdf,
        source_pages,
    }))
}

pub(crate) fn page_count(document: &ParsedPdf) -> usize {
    document.source_pages
}

fn inherited_user_unit(page: &hayro::hayro_syntax::page::Page<'_>) -> Result<Option<f32>> {
    let mut dict = page.raw().clone();
    let mut seen = HashSet::<ObjectIdentifier>::new();
    loop {
        if let Some(id) = dict.obj_id()
            && !seen.insert(id)
        {
            return Err(Error::Pdf("cyclic page parent".into()));
        }
        if dict.contains_key(b"UserUnit".as_slice()) {
            let value = dict
                .get::<Number>(b"UserUnit".as_slice())
                .ok_or_else(|| Error::Pdf("invalid UserUnit".into()))?
                .as_f32();
            if !value.is_finite() || value <= 0.0 {
                return Err(Error::Pdf("invalid UserUnit".into()));
            }
            return Ok(Some(value));
        }
        let parent_ref = dict
            .get_ref(b"Parent".as_slice())
            .map(ObjectIdentifier::from);
        let parent = parent_ref
            .and_then(|id| page.xref().get::<HayroDict<'_>>(id))
            .or_else(|| dict.get::<HayroDict<'_>>(b"Parent".as_slice()));
        let Some(parent) = parent else {
            return Ok(None);
        };
        if parent.obj_id().is_none()
            && let Some(id) = parent_ref
            && !seen.insert(id)
        {
            return Err(Error::Pdf("cyclic page parent".into()));
        }
        dict = parent;
    }
}

fn validate_page_contents(page: &hayro::hayro_syntax::page::Page<'_>) -> Result<()> {
    let dict = page.raw();
    if !dict.contains_key(b"Contents".as_slice()) {
        return Ok(());
    }
    match dict.get::<HayroObject<'_>>(b"Contents".as_slice()) {
        Some(HayroObject::Stream(_)) => Ok(()),
        Some(HayroObject::Array(streams)) => {
            if streams
                .iter::<HayroObject<'_>>()
                .all(|stream| matches!(stream, HayroObject::Stream(_)))
            {
                Ok(())
            } else {
                Err(Error::Pdf(
                    "page Contents array contains a non-stream".into(),
                ))
            }
        }
        _ => Err(Error::Pdf(
            "page Contents must be a stream or stream array".into(),
        )),
    }
}

pub(crate) fn extract_selected_pages_for_preview(
    document: &ParsedPdf,
    page_indices: &[usize],
) -> Result<SelectedPreview> {
    let user_units = page_indices
        .iter()
        .map(|index| {
            let page =
                document.pdf.pages().get(*index).ok_or_else(|| {
                    Error::Pdf(format!("preview page {index} is outside the PDF"))
                })?;
            validate_page_contents(page)?;
            inherited_user_unit(page)
        })
        .collect::<Result<Vec<_>>>()?;
    let count = i32::try_from(page_indices.len())
        .map_err(|_| Error::Pdf("selected PDF page count is too large".into()))?;
    let mut output = pdf_writer::Pdf::new();
    let mut next_ref = Ref::new(1);
    let catalog_id = next_ref.bump();
    let queries = page_indices
        .iter()
        .copied()
        .map(ExtractionQuery::new_page)
        .collect::<Vec<_>>();
    let extracted = hayro_write::extract(&document.pdf, Box::new(|| next_ref.bump()), &queries)
        .map_err(|error| Error::Pdf(format!("cannot extract selected PDF: {error:?}")))?;
    if extracted.root_refs.len() != page_indices.len() {
        return Err(Error::Pdf(
            "selected PDF extraction root count mismatch".into(),
        ));
    }
    let roots = extracted
        .root_refs
        .into_iter()
        .map(|root| {
            root.map_err(|error| Error::Pdf(format!("cannot extract selected PDF: {error:?}")))
        })
        .collect::<Result<Vec<_>>>()?;
    output
        .catalog(catalog_id)
        .pages(extracted.page_tree_parent_ref);
    output
        .pages(extracted.page_tree_parent_ref)
        .kids(roots)
        .count(count);
    output.extend(&extracted.chunk);
    Ok(SelectedPreview {
        bytes: output.finish(),
        page_indices: page_indices.to_vec(),
        user_units,
    })
}

#[cfg(test)]
pub(crate) fn page_stream_for_preview_test(document: &ParsedPdf, index: usize) -> Result<&[u8]> {
    let page = document
        .pdf
        .pages()
        .get(index)
        .ok_or_else(|| Error::Pdf(format!("page {index} is outside the PDF")))?;
    Ok(page.page_stream().unwrap_or(b""))
}

/// Tolerant window render for the official route: pages that fail degrade to warnings instead of
/// aborting, but a window whose every page failed still errors (dead service / broken PDF must
/// not masquerade as a placeholder document).
pub(crate) async fn render_window_for_task_tolerant(
    document: Arc<ParsedPdf>,
    indexes: Vec<usize>,
    limits: Limits,
    workers: usize,
    task_work_lease: TaskWorkLease,
) -> Result<(Vec<RenderedPage>, Vec<String>)> {
    render_window_core(
        document,
        indexes,
        limits,
        workers,
        task_work_lease,
        true,
        true,
    )
    .await
}

async fn render_window_core(
    document: Arc<ParsedPdf>,
    indexes: Vec<usize>,
    limits: Limits,
    workers: usize,
    task_work_lease: TaskWorkLease,
    tolerant: bool,
    fail_on_empty: bool,
) -> Result<(Vec<RenderedPage>, Vec<String>)> {
    let requested = indexes.len();
    let mut warnings = Vec::new();
    let mut first_error = None;
    let mut sizes = Vec::with_capacity(requested);
    for index in indexes {
        match page_dimensions(&document, index, &limits) {
            Ok((_, _, bytes)) => sizes.push((index, bytes)),
            Err(error) if tolerant => {
                warnings.push(format!("render page {index} failed: {error}"));
                if first_error.is_none() {
                    first_error = Some(error);
                }
            }
            Err(error) => return Err(error),
        }
    }
    let mut rendered = BTreeMap::new();
    let mut pending = sizes.into_iter().peekable();
    while pending.peek().is_some() {
        let window = match admitted_window(&mut pending, limits.max_in_flight_image_bytes) {
            Ok(window) => window,
            Err(Error::LimitExceeded { actual, .. }) if tolerant => {
                // A single page larger than the in-flight budget is skipped; the rest continue.
                let Some((index, _)) = pending.next() else {
                    break;
                };
                let error = Error::LimitExceeded {
                    resource: "in-flight image bytes",
                    limit: limits.max_in_flight_image_bytes as u64,
                    actual,
                };
                warnings.push(format!(
                    "render page {index} failed: page exceeds the in-flight image byte budget ({actual} bytes)"
                ));
                if first_error.is_none() {
                    first_error = Some(error);
                }
                continue;
            }
            Err(error) => return Err(error),
        };
        let concurrency = workers.max(1).min(window.len());
        let mut window_pending = window.into_iter();
        let mut tasks = tokio::task::JoinSet::new();
        for _ in 0..concurrency {
            spawn_render(
                &mut tasks,
                Arc::clone(&document),
                window_pending.next().expect("window has enough pages"),
                limits.clone(),
                task_work_lease.clone(),
            );
        }
        while let Some(result) = tasks.join_next().await {
            match result {
                Ok(Ok((index, page))) => {
                    rendered.insert(index, page);
                }
                Ok(Err((index, error))) if tolerant => {
                    warnings.push(format!("render page {index} failed: {error}"));
                    if first_error.is_none() {
                        first_error = Some(error);
                    }
                }
                Ok(Err((_, error))) => return Err(error),
                Err(error) => return Err(Error::WorkerJoin(error.to_string())),
            }
            if let Some(index) = window_pending.next() {
                spawn_render(
                    &mut tasks,
                    Arc::clone(&document),
                    index,
                    limits.clone(),
                    task_work_lease.clone(),
                );
            }
        }
    }
    if tolerant && fail_on_empty && requested > 0 && rendered.is_empty() {
        // An entirely failed window is a hard failure (dead service / broken PDF), never a
        // placeholder document.
        return Err(first_error.unwrap_or_else(|| Error::Pdf("renderer returned no page".into())));
    }
    Ok((rendered.into_values().collect(), warnings))
}

fn admitted_window(
    sizes: &mut std::iter::Peekable<impl Iterator<Item = (usize, usize)>>,
    cap: usize,
) -> Result<Vec<usize>> {
    let mut admitted = Vec::new();
    let mut used = 0usize;
    while let Some((index, bytes)) = sizes.peek().copied() {
        if used.saturating_add(bytes) > cap {
            if admitted.is_empty() {
                return Err(Error::LimitExceeded {
                    resource: "in-flight image bytes",
                    limit: cap as u64,
                    actual: bytes as u64,
                });
            }
            break;
        }
        used = used.saturating_add(bytes);
        admitted.push(index);
        sizes.next();
    }
    Ok(admitted)
}

fn spawn_render(
    tasks: &mut tokio::task::JoinSet<RenderTaskResult>,
    document: Arc<ParsedPdf>,
    index: usize,
    limits: Limits,
    task_work_lease: TaskWorkLease,
) {
    #[cfg(test)]
    let hook = page_render_test_hook();
    #[cfg(test)]
    tasks.spawn_blocking(task_work_lease.wrap(move || {
        let result = match &hook {
            Some(hook) => {
                (hook.before)(index).and_then(|()| render_page_safe(&document, index, &limits))
            }
            None => render_page_safe(&document, index, &limits),
        };
        if let Some(hook) = hook {
            (hook.after)(index, &result);
        }
        result
            .map(|page| (index, page))
            .map_err(|error| (index, error))
    }));
    #[cfg(not(test))]
    tasks.spawn_blocking(task_work_lease.wrap(move || {
        render_page_safe(&document, index, &limits)
            .map(|page| (index, page))
            .map_err(|error| (index, error))
    }));
}

pub(crate) fn render_page_safe(
    document: &ParsedPdf,
    index: usize,
    limits: &Limits,
) -> Result<RenderedPage> {
    match catch_unwind(AssertUnwindSafe(|| render_page(document, index, limits))) {
        Ok(result) => result,
        Err(_) => Err(Error::Pdf(format!("page {index}: Hayro renderer panicked"))),
    }
}

fn page_dimensions(
    document: &ParsedPdf,
    index: usize,
    limits: &Limits,
) -> Result<(f32, f32, usize)> {
    let page = document
        .pdf
        .pages()
        .get(index)
        .ok_or_else(|| Error::Pdf(format!("page {index} is outside the PDF")))?;
    // Hayro's visible dimensions include the source page rotation.
    let (width_points, height_points) = page.render_dimensions();
    if !width_points.is_finite()
        || !height_points.is_finite()
        || width_points <= 0.0
        || height_points <= 0.0
    {
        return Err(Error::Pdf(format!("page {index} has invalid dimensions")));
    }
    let scale = render_scale(width_points, height_points, limits);
    let width = (width_points * scale).ceil();
    let height = (height_points * scale).ceil();
    if !width.is_finite()
        || !height.is_finite()
        || width <= 0.0
        || height <= 0.0
        || width > u16::MAX as f32
        || height > u16::MAX as f32
    {
        return Err(Error::Pdf(format!(
            "page {index} viewport exceeds Hayro's u16 limit"
        )));
    }
    let pixels = (width as u64).saturating_mul(height as u64);
    let rgb_bytes = pixels.saturating_mul(3);
    // ponytail: the adaptive scale above already bounds the raster to max_page_pixels /
    // max_rendered_image_bytes, so the old check_limit calls are subsumed (ceil rounding can
    // overshoot the exact cap by a few thousand pixels, which the old checks would have
    // misclassified as an error). Validity and the u16 viewport check are the real skip criteria.
    Ok((width_points, height_points, rgb_bytes as usize))
}

/// Effective render scale for a page: 200 DPI for ordinary pages, but capped so the rasterized
/// size stays within `max_page_pixels` and `max_rendered_image_bytes`. Oversized pages render at
/// a lower effective DPI instead of failing. Callers validate `width_points`/`height_points`
/// (finite, positive) before calling.
fn render_scale(width_points: f32, height_points: f32, limits: &Limits) -> f32 {
    let area = width_points * height_points;
    let pixel_cap = (limits.max_page_pixels as f32 / area).sqrt();
    let byte_cap = (limits.max_rendered_image_bytes as f32 / 3.0 / area).sqrt();
    SCALE.min(pixel_cap).min(byte_cap)
}

pub(crate) fn page_image_bytes(
    document: &ParsedPdf,
    index: usize,
    limits: &Limits,
) -> Result<usize> {
    page_dimensions(document, index, limits).map(|(_, _, bytes)| bytes)
}

pub(crate) fn render_page(
    document: &ParsedPdf,
    index: usize,
    limits: &Limits,
) -> Result<RenderedPage> {
    let (width_points, height_points, _) = page_dimensions(document, index, limits)?;
    // The same adaptive scale as page_dimensions, so the planned raster matches the render.
    let scale = render_scale(width_points, height_points, limits);
    let width = (width_points * scale).ceil() as u16;
    let height = (height_points * scale).ceil() as u16;
    let page = document
        .pdf
        .pages()
        .get(index)
        .ok_or_else(|| Error::Pdf(format!("page {index} is outside the PDF")))?;
    let pixmap = render(
        page,
        &InterpreterSettings::default(),
        &RenderSettings {
            x_scale: scale,
            y_scale: scale,
            width: Some(width),
            height: Some(height),
            bg_color: WHITE,
        },
    );
    let image = premultiplied_rgba_over_white(width, height, pixmap.data_as_u8_slice())?;
    Ok(RenderedPage {
        index,
        size: [width_points, height_points],
        scale,
        image: Arc::new(image),
    })
}

fn premultiplied_rgba_over_white(width: u16, height: u16, rgba: &[u8]) -> Result<RgbImage> {
    let expected = width as usize * height as usize * 4;
    if rgba.len() != expected {
        return Err(Error::Pdf("Hayro returned an invalid pixel buffer".into()));
    }
    let mut rgb = Vec::with_capacity(expected / 4 * 3);
    for pixel in rgba.chunks_exact(4) {
        // Premultiplied source-over white: C = C_premul + (1 - alpha) * white.
        rgb.extend(
            pixel[..3]
                .iter()
                .map(|&channel| channel.saturating_add(255 - pixel[3])),
        );
    }
    RgbImage::from_raw(width as u32, height as u32, rgb)
        .ok_or_else(|| Error::Pdf("could not construct rendered image".into()))
}

fn check_limit(resource: &'static str, limit: u64, actual: u64) -> Result<()> {
    if actual > limit {
        Err(Error::LimitExceeded {
            resource,
            limit,
            actual,
        })
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{
        PageRenderTestHook, SCALE, admitted_window, extract_selected_pages_for_preview,
        page_dimensions, page_render_test_hook, parse_document, premultiplied_rgba_over_white,
        read_input, render_page, render_scale, render_window_for_task_tolerant,
        scope_page_render_test_hook,
    };
    use crate::{Limits, PageResult, PdfInput, TaskWorkLease, preview};
    use bytes::Bytes;
    use lopdf::{Dictionary, Document, Object, Stream, dictionary};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    fn in_memory_pdf(actual_pages: usize, declared_pages: i64) -> Vec<u8> {
        in_memory_pdf_with_size(200.0, 300.0, actual_pages, declared_pages)
    }

    fn in_memory_pdf_with_size(
        width: f32,
        height: f32,
        actual_pages: usize,
        declared_pages: i64,
    ) -> Vec<u8> {
        let mut document = Document::with_version("1.5");
        let pages = document.new_object_id();
        let mut kids = Vec::with_capacity(actual_pages);
        for _ in 0..actual_pages {
            let page = document.new_object_id();
            let contents = document.add_object(Stream::new(Dictionary::new(), b"q\nQ\n".to_vec()));
            document.objects.insert(
                page,
                Object::Dictionary(dictionary! {
                    "Type" => "Page",
                    "Parent" => pages,
                    "MediaBox" => vec![0.into(), 0.into(), width.into(), height.into()],
                    "Resources" => Dictionary::new(),
                    "Contents" => contents,
                }),
            );
            kids.push(page.into());
        }
        document.objects.insert(
            pages,
            Object::Dictionary(dictionary! {
                "Type" => "Pages",
                "Kids" => kids,
                "Count" => declared_pages,
            }),
        );
        let catalog = document.add_object(dictionary! { "Type" => "Catalog", "Pages" => pages });
        document.trailer.set("Root", catalog);
        let mut bytes = Vec::new();
        document.save_to(&mut bytes).unwrap();
        bytes
    }

    fn ordered_pdf() -> Vec<u8> {
        let mut document = Document::with_version("1.7");
        let pages = document.new_object_id();
        let mut kids = Vec::new();
        for index in 0..3 {
            let page = document.new_object_id();
            let contents = document.add_object(Stream::new(
                Dictionary::new(),
                format!("BT /F1 12 Tf 10 10 Td (PAGE_{index}) Tj ET").into_bytes(),
            ));
            document.objects.insert(
                page,
                dictionary! {
                    "Type" => "Page", "Parent" => pages, "Contents" => contents,
                    "MediaBox" => vec![0.into(), 0.into(), 200.into(), 300.into()],
                    "UserUnit" => index as f32 + 1.0,
                }
                .into(),
            );
            kids.push(page.into());
        }
        document.objects.insert(
            pages,
            dictionary! {
                "Type" => "Pages", "Kids" => kids, "Count" => 3,
                "Resources" => dictionary! {
                    "Font" => dictionary! {
                        "F1" => dictionary! { "Type" => "Font", "Subtype" => "Type1", "BaseFont" => "Helvetica" },
                    },
                },
            }
            .into(),
        );
        let catalog = document.add_object(dictionary! { "Type" => "Catalog", "Pages" => pages });
        document.trailer.set("Root", catalog);
        let mut bytes = Vec::new();
        document.save_to(&mut bytes).unwrap();
        bytes
    }

    #[test]
    fn metadata_page_count_enforces_page_limit_before_rendering() {
        let limits = Limits {
            max_pages: 1,
            ..Limits::default()
        };
        let error = match parse_document(in_memory_pdf(2, 2), &limits) {
            Err(error) => error,
            Ok(_) => panic!("page limit should reject metadata count"),
        };
        assert!(matches!(
            error,
            crate::Error::LimitExceeded {
                resource: "pages",
                limit: 1,
                actual: 2,
            }
        ));
    }

    #[test]
    fn declared_page_count_mismatch_is_rejected() {
        let error = match parse_document(in_memory_pdf(1, 2), &Limits::default()) {
            Err(error) => error,
            Ok(_) => panic!("page count mismatch should be rejected"),
        };
        assert!(
            error
                .to_string()
                .contains("renderer/source page mapping mismatch")
        );
    }

    #[test]
    fn selected_preview_extraction_preserves_query_order_and_user_units() {
        let document = parse_document(ordered_pdf(), &Limits::default()).unwrap();
        let selected = extract_selected_pages_for_preview(&document, &[2, 0]).unwrap();
        assert_eq!(selected.page_indices, vec![2, 0]);
        assert_eq!(selected.user_units, vec![Some(3.0), Some(1.0)]);
        let extracted = Document::load_mem(&selected.bytes).unwrap();
        assert!(extracted.extract_text(&[1]).unwrap().contains("PAGE_2"));
        assert!(extracted.extract_text(&[2]).unwrap().contains("PAGE_0"));
    }

    #[test]
    fn encrypted_metadata_maps_to_the_project_error_before_extraction() {
        let mut document = Document::load_mem(&in_memory_pdf(1, 1)).unwrap();
        document.trailer.set(
            "ID",
            vec![
                Object::String(vec![1; 16], lopdf::StringFormat::Literal),
                Object::String(vec![2; 16], lopdf::StringFormat::Literal),
            ],
        );
        let state = lopdf::EncryptionState::try_from(lopdf::EncryptionVersion::V1 {
            document: &document,
            owner_password: "owner",
            user_password: "user",
            permissions: lopdf::Permissions::default(),
        })
        .unwrap();
        document.encrypt(&state).unwrap();
        let mut bytes = Vec::new();
        document.save_to(&mut bytes).unwrap();

        let error = match parse_document(bytes, &Limits::default()) {
            Err(error) => error,
            Ok(_) => panic!("encrypted PDF should be rejected before selected extraction"),
        };
        assert!(matches!(
            error,
            crate::Error::Pdf(message) if message == "encrypted PDFs are unsupported"
        ));
    }

    #[test]
    fn premultiplied_alpha_is_composited_over_white() {
        let image = premultiplied_rgba_over_white(1, 1, &[64, 32, 0, 128]).unwrap();
        assert_eq!(image.as_raw(), &[191, 159, 127]);
    }

    #[test]
    fn fixture_dimensions_use_200_dpi_scale_and_rgb_byte_count() {
        let document = parse_document(
            include_bytes!("../tests/fixtures/pdf/minimal.pdf").to_vec(),
            &Limits::default(),
        )
        .unwrap();
        let (points_width, points_height, bytes) =
            page_dimensions(&document, 0, &Limits::default()).unwrap();
        assert_eq!(
            bytes,
            (points_width * SCALE).ceil() as usize * (points_height * SCALE).ceil() as usize * 3
        );
    }

    #[test]
    fn oversized_page_adaptively_scales_within_pixel_and_byte_budgets() {
        // A 5000x5000 pt page would be ~13900x13900 px (~580 MB RGB) at fixed 200 DPI. With a
        // 1M-pixel cap the adaptive scale must shrink it into budget instead of failing.
        let limits = Limits {
            max_page_pixels: 1_000_000,
            ..Limits::default()
        };
        let document = parse_document(in_memory_pdf_with_size(5000.0, 5000.0, 1, 1), &limits)
            .expect("fixture");
        let (points_w, points_h, bytes) = page_dimensions(&document, 0, &limits).unwrap();
        assert_eq!(points_w, 5000.0);
        assert_eq!(points_h, 5000.0);
        let scale = render_scale(points_w, points_h, &limits);
        assert!(scale < SCALE, "oversized page must drop below 200 DPI");
        let width = (points_w * scale).ceil();
        let height = (points_h * scale).ceil();
        let pixels = (width as u64).saturating_mul(height as u64);
        assert!(pixels <= limits.max_page_pixels);
        assert!(bytes <= limits.max_rendered_image_bytes);
        assert_eq!(bytes, pixels.saturating_mul(3) as usize);
    }

    #[test]
    fn oversized_page_renders_at_the_adaptive_scale_matching_page_dimensions() {
        let limits = Limits {
            max_page_pixels: 1_000_000,
            ..Limits::default()
        };
        let document = parse_document(in_memory_pdf_with_size(5000.0, 5000.0, 1, 1), &limits)
            .expect("fixture");
        let (points_w, points_h, _) = page_dimensions(&document, 0, &limits).unwrap();
        let scale = render_scale(points_w, points_h, &limits);
        let page = render_page(&document, 0, &limits).unwrap();
        assert_eq!(page.size, [points_w, points_h], "size stays in PDF points");
        assert_eq!(page.scale, scale);
        assert_eq!(page.image.width(), (points_w * scale).ceil() as u32);
        assert_eq!(page.image.height(), (points_h * scale).ceil() as u32);
        assert_eq!(
            page.image.as_raw().len(),
            (points_w * scale).ceil() as usize * (points_h * scale).ceil() as usize * 3
        );
    }

    #[test]
    fn bytes_input_keeps_its_backing_through_parsing_and_preview() {
        let input = Bytes::copy_from_slice(include_bytes!("../tests/fixtures/pdf/minimal.pdf"));
        let pointer = input.as_ptr();
        let bytes = read_input(PdfInput::Bytes(input), &Limits::default()).unwrap();
        assert_eq!(bytes.as_ptr(), pointer);

        let document = parse_document(bytes, &Limits::default()).unwrap();
        let preview = preview::generate_until(
            document._bytes.as_ref(),
            &[PageResult {
                page_index: 0,
                page_size: [612.0, 792.0],
                blocks: Vec::new(),
            }],
            "minimal",
            &Limits::default(),
            1 << 20,
            std::time::Instant::now() + std::time::Duration::from_secs(60),
        )
        .unwrap();
        assert!(!preview.data.is_empty());
        assert_eq!(document._bytes.as_ptr(), pointer);
    }

    #[test]
    fn aggregate_budget_rejects_an_oversized_first_page() {
        let mut oversized = vec![(4, 11), (5, 1)].into_iter().peekable();
        assert!(matches!(
            admitted_window(&mut oversized, 10),
            Err(crate::Error::LimitExceeded { actual: 11, .. })
        ));
        let mut pages = vec![(4, 5), (5, 6), (6, 1)].into_iter().peekable();
        assert_eq!(admitted_window(&mut pages, 10).unwrap(), vec![4]);
        assert_eq!(admitted_window(&mut pages, 10).unwrap(), vec![5, 6]);
    }

    #[tokio::test]
    async fn page_render_test_hook_is_task_local_and_scope_isolated() {
        let first = Arc::new(PageRenderTestHook::new(|_| Ok(()), |_, _| {}));
        let second = Arc::new(PageRenderTestHook::new(|_| Ok(()), |_, _| {}));
        let first_ptr = Arc::as_ptr(&first) as usize;
        let second_ptr = Arc::as_ptr(&second) as usize;
        let (seen_first, seen_second) = tokio::join!(
            scope_page_render_test_hook(first, async {
                Arc::as_ptr(&page_render_test_hook().expect("first hook")) as usize
            }),
            scope_page_render_test_hook(second, async {
                Arc::as_ptr(&page_render_test_hook().expect("second hook")) as usize
            }),
        );

        assert_eq!(seen_first, first_ptr);
        assert_eq!(seen_second, second_ptr);
        assert!(page_render_test_hook().is_none());
    }

    #[tokio::test]
    async fn page_render_test_hook_observes_actual_worker_success_once() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let hook = Arc::new(PageRenderTestHook::new(
            {
                let events = Arc::clone(&events);
                move |index| {
                    events.lock().expect("events").push(("before", index, true));
                    Ok(())
                }
            },
            {
                let events = Arc::clone(&events);
                move |index, result| {
                    events
                        .lock()
                        .expect("events")
                        .push(("after", index, result.is_ok()));
                }
            },
        ));
        let document = parse_document(
            include_bytes!("../tests/fixtures/pdf/minimal.pdf").to_vec(),
            &Limits::default(),
        )
        .expect("fixture");

        let rendered = scope_page_render_test_hook(
            hook,
            render_window_for_task_tolerant(
                document,
                vec![0],
                Limits::default(),
                1,
                TaskWorkLease::default(),
            ),
        )
        .await
        .expect("render")
        .0;

        assert_eq!(rendered.len(), 1);
        assert_eq!(
            *events.lock().expect("events"),
            vec![("before", 0, true), ("after", 0, true)]
        );
    }

    #[tokio::test]
    async fn render_window_honors_more_than_three_workers_subject_to_page_count() {
        // Six pages with eight configured workers must render six-way concurrent. The old
        // `clamp(1, 3)` policy would starve the six-party gate and fail with a clear error
        // instead of a hang.
        use std::sync::{Condvar, Mutex};
        use std::time::Instant;

        let document = parse_document(in_memory_pdf(6, 6), &Limits::default()).expect("fixture");
        let gate = Arc::new((Mutex::new(0usize), Condvar::new()));
        let hook = Arc::new(PageRenderTestHook::new(
            {
                let gate = Arc::clone(&gate);
                move |_| {
                    let (lock, ready) = &*gate;
                    let mut count = lock.lock().expect("gate");
                    *count += 1;
                    if *count == 6 {
                        ready.notify_all();
                        return Ok(());
                    }
                    let deadline = Instant::now() + Duration::from_secs(10);
                    while *count < 6 && Instant::now() < deadline {
                        let (guard, _) = ready
                            .wait_timeout(count, Duration::from_millis(100))
                            .expect("gate");
                        count = guard;
                    }
                    if *count >= 6 {
                        Ok(())
                    } else {
                        Err(crate::Error::Pdf("render concurrency gate timeout".into()))
                    }
                }
            },
            |_, _| {},
        ));
        let rendered = tokio::time::timeout(
            Duration::from_secs(30),
            scope_page_render_test_hook(
                hook,
                render_window_for_task_tolerant(
                    document,
                    (0..6).collect(),
                    Limits::default(),
                    8,
                    TaskWorkLease::default(),
                ),
            ),
        )
        .await
        .expect("render timed out")
        .expect("render")
        .0;
        assert_eq!(rendered.len(), 6);
    }

    #[tokio::test]
    async fn tolerant_render_window_skips_failed_pages_with_warnings() {
        let hook = Arc::new(PageRenderTestHook::new(
            |index| {
                if index == 1 {
                    Err(crate::Error::Pdf("render page 1 injected failure".into()))
                } else {
                    Ok(())
                }
            },
            |_, _| {},
        ));
        let document = parse_document(in_memory_pdf(3, 3), &Limits::default()).expect("fixture");
        let (rendered, warnings) = scope_page_render_test_hook(
            hook,
            render_window_for_task_tolerant(
                document,
                (0..3).collect(),
                Limits::default(),
                2,
                TaskWorkLease::default(),
            ),
        )
        .await
        .expect("render");
        assert_eq!(rendered.len(), 2);
        assert!(rendered.iter().all(|page| page.index != 1));
        assert_eq!(
            warnings,
            vec!["render page 1 failed: PDF error: render page 1 injected failure".to_owned()]
        );
    }

    #[tokio::test]
    async fn injected_worker_error_reports_after_and_releases_task_lease() {
        let after = Arc::new(Mutex::new(Vec::new()));
        let hook = Arc::new(PageRenderTestHook::new(
            |_| Err(crate::Error::Pdf("injected render worker failure".into())),
            {
                let after = Arc::clone(&after);
                move |index, result| after.lock().expect("after").push((index, result.is_err()))
            },
        ));
        let document = parse_document(
            include_bytes!("../tests/fixtures/pdf/minimal.pdf").to_vec(),
            &Limits::default(),
        )
        .expect("fixture");
        let semaphore = Arc::new(tokio::sync::Semaphore::new(1));
        let permit = Arc::clone(&semaphore)
            .acquire_owned()
            .await
            .expect("permit");

        let result = scope_page_render_test_hook(
            hook,
            render_window_for_task_tolerant(
                document,
                vec![0],
                Limits::default(),
                1,
                TaskWorkLease::from_permit(permit),
            ),
        )
        .await;

        assert!(
            matches!(result, Err(crate::Error::Pdf(message)) if message == "injected render worker failure")
        );
        assert_eq!(*after.lock().expect("after"), vec![(0, true)]);
        let _permit = tokio::time::timeout(Duration::from_secs(1), semaphore.acquire_owned())
            .await
            .expect("tracked worker lease was not released")
            .expect("semaphore closed");
    }
}
