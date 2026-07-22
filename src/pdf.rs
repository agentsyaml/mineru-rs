use crate::{Error, Limits, PdfInput, Result, TaskWorkLease};
use bytes::Bytes;
use hayro::vello_cpu::color::palette::css::WHITE;
use hayro::{RenderSettings, hayro_interpret::InterpreterSettings, hayro_syntax::Pdf, render};
use image::RgbImage;
use lopdf::Document as LoPdf;
use std::{
    collections::BTreeMap,
    fs::File,
    io::Read,
    panic::{AssertUnwindSafe, catch_unwind},
    sync::Arc,
};

const SCALE: f32 = 200.0 / 72.0;

pub(crate) struct RenderedPage {
    pub index: usize,
    /// PDF points, not the dimensions of the 200 DPI raster below.
    pub size: [f32; 2],
    pub image: Arc<RgbImage>,
}

/// Owns the source data required by Hayro as well as its single parsed view.
pub(crate) struct ParsedPdf {
    _bytes: Arc<Bytes>,
    pdf: Pdf,
    // lopdf's page tree is authoritative: Hayro may omit blank leaf pages.
    source_pages: usize,
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

pub(crate) async fn parse(
    bytes: impl Into<Bytes> + Send + 'static,
    limits: Limits,
) -> Result<Arc<ParsedPdf>> {
    tokio::task::spawn_blocking(move || parse_document(bytes, &limits))
        .await
        .map_err(|e| Error::WorkerJoin(e.to_string()))?
}

pub(crate) fn parse_document(bytes: impl Into<Bytes>, limits: &Limits) -> Result<Arc<ParsedPdf>> {
    let bytes = Arc::new(bytes.into());
    let source = LoPdf::load_mem(bytes.as_ref().as_ref())
        .map_err(|e| Error::Pdf(format!("unsupported or invalid PDF: {e}")))?;
    if source.is_encrypted() {
        return Err(Error::Pdf("encrypted PDFs are unsupported".into()));
    }
    let source_pages = source.get_pages().len();
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

pub(crate) fn source_bytes(document: &ParsedPdf) -> Arc<Bytes> {
    Arc::clone(&document._bytes)
}

pub(crate) async fn render_window(
    document: Arc<ParsedPdf>,
    indexes: Vec<usize>,
    limits: Limits,
    workers: usize,
) -> Result<Vec<RenderedPage>> {
    render_window_for_task(document, indexes, limits, workers, TaskWorkLease::default()).await
}

pub(crate) async fn render_window_for_task(
    document: Arc<ParsedPdf>,
    indexes: Vec<usize>,
    limits: Limits,
    workers: usize,
    task_work_lease: TaskWorkLease,
) -> Result<Vec<RenderedPage>> {
    let mut sizes = Vec::with_capacity(indexes.len());
    for index in indexes {
        sizes.push((index, page_dimensions(&document, index, &limits)?.2));
    }
    let mut rendered = BTreeMap::new();
    let mut pending = sizes.into_iter().peekable();
    while pending.peek().is_some() {
        let window = admitted_window(&mut pending, limits.max_in_flight_image_bytes)?;
        let concurrency = workers.clamp(1, 3).min(window.len());
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
            let page = result.map_err(|e| Error::WorkerJoin(e.to_string()))??;
            rendered.insert(page.index, page);
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
    Ok(rendered.into_values().collect())
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
    tasks: &mut tokio::task::JoinSet<Result<RenderedPage>>,
    document: Arc<ParsedPdf>,
    index: usize,
    limits: Limits,
    task_work_lease: TaskWorkLease,
) {
    tasks.spawn_blocking(task_work_lease.wrap(move || render_page_safe(&document, index, &limits)));
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
    let width = (width_points * SCALE).ceil();
    let height = (height_points * SCALE).ceil();
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
    check_limit("page pixels", limits.max_page_pixels, pixels)?;
    let rgb_bytes = pixels.saturating_mul(3);
    check_limit(
        "rendered image bytes",
        limits.max_rendered_image_bytes as u64,
        rgb_bytes,
    )?;
    Ok((width_points, height_points, rgb_bytes as usize))
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
    let width = (width_points * SCALE).ceil() as u16;
    let height = (height_points * SCALE).ceil() as u16;
    let page = document
        .pdf
        .pages()
        .get(index)
        .ok_or_else(|| Error::Pdf(format!("page {index} is outside the PDF")))?;
    let pixmap = render(
        page,
        &InterpreterSettings::default(),
        &RenderSettings {
            x_scale: SCALE,
            y_scale: SCALE,
            width: Some(width),
            height: Some(height),
            bg_color: WHITE,
        },
    );
    let image = premultiplied_rgba_over_white(width, height, pixmap.data_as_u8_slice())?;
    Ok(RenderedPage {
        index,
        size: [width_points, height_points],
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
        SCALE, admitted_window, page_dimensions, parse_document, premultiplied_rgba_over_white,
        read_input, source_bytes,
    };
    use crate::{Limits, PageResult, PdfInput, preview};
    use bytes::Bytes;

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
    fn bytes_input_keeps_its_backing_through_parsing_and_preview() {
        let input = Bytes::copy_from_slice(include_bytes!("../tests/fixtures/pdf/minimal.pdf"));
        let pointer = input.as_ptr();
        let bytes = read_input(PdfInput::Bytes(input), &Limits::default()).unwrap();
        assert_eq!(bytes.as_ptr(), pointer);

        let document = parse_document(bytes, &Limits::default()).unwrap();
        let source = source_bytes(&document);
        assert_eq!(source.as_ptr(), pointer);
        let preview = preview::generate(
            source.as_ref(),
            &[PageResult {
                page_index: 0,
                page_size: [612.0, 792.0],
                blocks: Vec::new(),
            }],
            "minimal",
            &Limits::default(),
            1 << 20,
        )
        .unwrap();
        assert!(!preview.data.is_empty());
        assert_eq!(source.as_ptr(), pointer);
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
}
