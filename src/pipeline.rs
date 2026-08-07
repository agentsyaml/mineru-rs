use crate::{
    Asset, AssetKind, ClientConfig, Error, ParseOptions, PdfInput, Result, Rotation,
    document_postprocess, extractor::PageExtractor, pdf,
};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use bytes::Bytes;
use image::{ExtendedColorType, ImageEncoder, codecs::png::PngEncoder, imageops::FilterType};
use regex::Regex;
use std::{
    collections::{BTreeMap, HashSet},
    io::{self, Read, Write},
    path::PathBuf,
    sync::Arc,
};
use tokio::task::JoinSet;

const MAX_DOCUMENT_WARNINGS: usize = 100;
const DOCUMENT_WARNING_CAP: usize = 512;

pub(crate) async fn parse(
    config: &ClientConfig,
    extractor: &PageExtractor,
    input: PdfInput,
    options: ParseOptions,
) -> Result<crate::Document> {
    let stem = sanitized_stem(&input);
    let bytes = pdf::read_input(input, &config.limits)?;
    let pdf = pdf::parse(bytes, config.limits.clone()).await?;
    let count = pdf::page_count(&pdf);
    let range = options.page_range;
    if let Some(range) = range {
        range.validate()?;
    }
    let start = range.map_or(0, |r| r.start as usize);
    let end = range
        .and_then(|r| r.end)
        .map_or(count.saturating_sub(1), |end| end as usize);
    if start >= count || end >= count {
        return Err(Error::InvalidInput(format!(
            "page range {start}..={end} is outside a {count}-page PDF"
        )));
    }
    let options = Arc::new(options);
    let mut warnings: Vec<String> = Vec::new();
    let mut pages = Vec::new();
    let mut assets: Vec<Asset> = Vec::new();
    let mut used_asset_bytes = 0usize;
    let mut asset_paths = HashSet::new();
    let window = config.limits.page_window_size;
    let mut first = start;
    while first <= end {
        let last = (first + window).min(end + 1);
        let (rendered, render_warnings) = pdf::render_window(
            pdf.clone(),
            (first..last).collect(),
            config.limits.clone(),
            config.render_workers,
        )
        .await?;
        warnings.extend(render_warnings);
        if rendered.is_empty() {
            return Err(Error::Pdf("renderer returned no page".into()));
        }
        let mut jobs = JoinSet::new();
        let mut rendered_sizes = BTreeMap::new();
        for page in rendered {
            rendered_sizes.insert(page.index, page.size);
            let index = page.index;
            let image = page.image;
            let source = image.clone();
            let extractor = extractor.clone();
            let options = options.clone();
            jobs.spawn(async move {
                let (result, page_warnings) = extractor
                    .extract_page(index, page.size, image, &options)
                    .await
                    .map_err(|source| Error::Page {
                        page: index,
                        source: Box::new(source),
                    })?;
                Ok::<_, Error>((index, source, result, page_warnings))
            });
        }
        let mut extracted = Vec::new();
        // The first window must fail loudly when the service is down or the key is wrong:
        // an all-placeholder first window would otherwise masquerade as an empty document.
        let first_window = first == start;
        while let Some(job) = jobs.join_next().await {
            match job {
                Ok(Ok((index, source, result, page_warnings))) => {
                    warnings.extend(page_warnings);
                    extracted.push((index, Some(source), result));
                }
                Ok(Err(error)) => {
                    let (index, source) = match error {
                        Error::Page { page, source } => (page, *source),
                        other => return Err(other),
                    };
                    if first_window {
                        return Err(Error::Page {
                            page: index,
                            source: Box::new(source),
                        });
                    }
                    // A failed page degrades to a warning plus an empty placeholder so the
                    // document continues and page indices stay coherent.
                    warnings.push(format!("page {index} failed: {source}"));
                    extracted.push((
                        index,
                        None,
                        crate::PageResult {
                            page_index: index,
                            page_size: rendered_sizes.get(&index).copied().unwrap_or([0.0, 0.0]),
                            blocks: Vec::new(),
                        },
                    ));
                }
                Err(error) => return Err(Error::WorkerJoin(error.to_string())),
            }
        }
        // One entry per page in the window, ascending: successes keep their source image,
        // placeholders (failed extraction or render) have none. Placeholders must be sorted
        // with the successes — appending them in job-completion order scrambles page order
        // for downstream adjacency checks (cross-page table merge) and previews.
        let extracted = order_window(extracted, first..last);
        for (_, source, result) in extracted {
            let mut page = result;
            if let Some(source) = source {
                let (generated, asset_warnings) = attach_assets(
                    &mut page,
                    &source,
                    used_asset_bytes,
                    config.limits.max_total_asset_bytes,
                    &asset_paths,
                )?;
                warnings.extend(asset_warnings);
                extend_assets(
                    &mut assets,
                    &mut used_asset_bytes,
                    &mut asset_paths,
                    generated,
                    config.limits.max_total_asset_bytes,
                )?;
            }
            pages.push(page);
        }
        // Every page in [first, last) is now accounted for (extracted or placeholder), so
        // the window bounds advance unconditionally; failed pages must not stall or re-run.
        first = last;
    }
    // Cross-page table merge uses adjacency (`windows(2)`), so page order must be strict
    // even if a window ever leaves placeholders and successes out of order.
    pages.sort_by_key(|page| page.page_index);
    warnings.extend(
        extractor
            .merge_cross_page_tables(&mut pages, &options)
            .await,
    );
    match crate::preview::generate(
        &pdf::source_bytes(&pdf),
        &pages,
        &stem,
        &config.limits,
        config
            .limits
            .max_total_asset_bytes
            .saturating_sub(used_asset_bytes),
    ) {
        Ok(preview) => {
            extend_assets(
                &mut assets,
                &mut used_asset_bytes,
                &mut asset_paths,
                vec![preview],
                config.limits.max_total_asset_bytes,
            )?;
        }
        Err(error) => {
            warnings.push(format!("preview generation failed: {error}"));
        }
    }
    let mut document = document_postprocess::build(pages);
    document.assets = assets;
    document.warnings = aggregate_warnings(warnings);
    Ok(document)
}

fn aggregate_warnings(warnings: Vec<String>) -> Vec<String> {
    let mut seen = HashSet::new();
    warnings
        .into_iter()
        .map(|warning| {
            crate::error::sanitize_vlm_error_bytes(warning.as_bytes(), DOCUMENT_WARNING_CAP)
        })
        .filter(|warning| seen.insert(warning.clone()))
        .take(MAX_DOCUMENT_WARNINGS)
        .collect()
}

/// Orders one window's pages and fills any missing index with an empty placeholder, so
/// every page in `[window.start, window.end)` appears exactly once, ascending. Placeholders
/// (failed extraction or render) carry no source image; extraction failures keep their
/// rendered page size, render failures (unknown size) fall back to zero.
fn order_window(
    mut extracted: Vec<(usize, Option<Arc<image::RgbImage>>, crate::PageResult)>,
    window: std::ops::Range<usize>,
) -> Vec<(usize, Option<Arc<image::RgbImage>>, crate::PageResult)> {
    let present: HashSet<usize> = extracted.iter().map(|(index, _, _)| *index).collect();
    for index in window {
        if !present.contains(&index) {
            extracted.push((
                index,
                None,
                crate::PageResult {
                    page_index: index,
                    page_size: [0.0, 0.0],
                    blocks: Vec::new(),
                },
            ));
        }
    }
    extracted.sort_unstable_by_key(|(index, _, _)| *index);
    extracted
}

fn sanitized_stem(input: &PdfInput) -> String {
    match input {
        PdfInput::Path(path) => path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .filter(|stem| !stem.is_empty())
            .filter(|stem| is_safe_stem(stem))
            .map_or_else(|| "document".into(), |stem| stem.to_owned()),
        PdfInput::Bytes(_) => "document".into(),
    }
}

fn is_safe_stem(stem: &str) -> bool {
    !stem.contains(['/', '\\', '\0']) && stem != "." && stem != ".."
}

fn extend_assets(
    assets: &mut Vec<Asset>,
    used: &mut usize,
    paths: &mut HashSet<PathBuf>,
    mut generated: Vec<Asset>,
    limit: usize,
) -> Result<()> {
    let mut generated_paths = HashSet::new();
    generated.retain(|asset| {
        !paths.contains(&asset.relative_path) && generated_paths.insert(asset.relative_path.clone())
    });
    let actual = generated
        .iter()
        .fold(*used, |total, asset| total.saturating_add(asset.data.len()));
    if actual > limit {
        return Err(Error::LimitExceeded {
            resource: "total asset bytes",
            limit: limit as u64,
            actual: actual as u64,
        });
    }
    *used = actual;
    paths.extend(generated_paths);
    assets.extend(generated);
    Ok(())
}

fn attach_assets(
    page: &mut crate::PageResult,
    image: &image::RgbImage,
    used: usize,
    limit: usize,
    existing_paths: &HashSet<PathBuf>,
) -> Result<(Vec<Asset>, Vec<String>)> {
    let mut remaining = limit
        .checked_sub(used)
        .ok_or_else(|| asset_limit(limit, used))?;
    let mut assets = Vec::new();
    let mut warnings = Vec::new();
    let mut generated_paths = HashSet::new();
    let mut content_updates = Vec::new();
    let mut metadata_updates = Vec::new();
    let data_image =
        Regex::new(r#"(?i)src\s*=\s*[\"']data:image/([a-z0-9.+-]+);base64,([^\"']*)[\"']"#)
            .unwrap();
    for (index, block) in page.blocks.iter().enumerate() {
        if block.kind.as_str() == crate::BlockKind::TABLE && block.content.is_some() {
            match attach_table_images(
                block,
                &data_image,
                &mut assets,
                &mut generated_paths,
                existing_paths,
                &mut remaining,
                limit,
            ) {
                Ok((updated, table_warnings)) => {
                    warnings.extend(table_warnings.into_iter().map(|warning| {
                        format!("page {} block {}: {warning}", page.page_index, index)
                    }));
                    if let Some(content) = updated {
                        content_updates.push((index, content));
                    }
                }
                Err(error @ Error::LimitExceeded { .. }) => return Err(error),
                Err(error) => {
                    warnings.push(format!("page {} block {}: {error}", page.page_index, index));
                }
            }
        }
        let kind = match block.kind.as_str() {
            crate::BlockKind::IMAGE | crate::BlockKind::IMAGE_BLOCK => AssetKind::Image,
            crate::BlockKind::TABLE => AssetKind::Table,
            crate::BlockKind::EQUATION | crate::BlockKind::EQUATION_BLOCK => AssetKind::Equation,
            crate::BlockKind::CHART => AssetKind::Chart,
            _ => continue,
        };
        let kind_name = match kind {
            AssetKind::Image => "image",
            AssetKind::Table => "table",
            AssetKind::Equation => "equation",
            AssetKind::Chart => "chart",
            AssetKind::Other(_) => "other",
        };
        let generated = (|| -> Result<(PathBuf, String, Bytes), Error> {
            let bounds = asset_crop_bounds(image, block.bbox)?;
            let (crop_width, crop_height) = (bounds.2 - bounds.0, bounds.3 - bounds.1);
            let (crop_width, crop_height) = match block.angle {
                Some(Rotation::Deg90 | Rotation::Deg270) => (crop_height, crop_width),
                _ => (crop_width, crop_height),
            };
            let target_width = crop_width
                .checked_mul(2)
                .ok_or_else(|| asset_limit_overflow(limit))?;
            let target_height = crop_height
                .checked_mul(2)
                .ok_or_else(|| asset_limit_overflow(limit))?;
            // Raw RGB size is only an overflow guard for the resize allocation below; it must not be
            // charged against `max_total_asset_bytes`, which budgets encoded PNG output.
            let _raw_rgb_overflow_guard = usize::try_from(target_width)
                .ok()
                .and_then(|width| {
                    usize::try_from(target_height)
                        .ok()
                        .and_then(|height| width.checked_mul(height))
                })
                .and_then(|pixels| pixels.checked_mul(3))
                .ok_or_else(|| asset_limit_overflow(limit))?;

            let crop = asset_crop_from_bounds(image, bounds, block.angle);
            // Assets, unlike model input, intentionally retain a 2x crop for output fidelity.
            let crop =
                image::imageops::resize(&crop, target_width, target_height, FilterType::Lanczos3);
            let mut output = CappedWriter::new(remaining);
            PngEncoder::new(&mut output)
                .write_image(
                    crop.as_raw(),
                    crop.width(),
                    crop.height(),
                    ExtendedColorType::Rgb8,
                )
                .map_err(|error| {
                    if output.attempted > remaining {
                        asset_limit(
                            limit,
                            limit
                                .saturating_sub(remaining)
                                .saturating_add(output.attempted),
                        )
                    } else {
                        Error::Image(error.to_string())
                    }
                })?;
            let data = output.data;
            let md5 = format!("{:x}", md5::compute(&data));
            let relative_path = PathBuf::from(format!(
                "assets/{}-{}-{}-{}.png",
                page.page_index,
                index,
                kind_name,
                &md5[..8]
            ));
            Ok((relative_path, md5, Bytes::from(data)))
        })();
        match generated {
            Ok((relative_path, md5, data)) => {
                metadata_updates.push((index, relative_path.clone(), md5.clone()));
                if !existing_paths.contains(&relative_path)
                    && generated_paths.insert(relative_path.clone())
                {
                    remaining -= data.len();
                    assets.push(Asset {
                        kind,
                        relative_path,
                        media_type: "image/png".into(),
                        data,
                        md5,
                    });
                }
            }
            Err(error @ Error::LimitExceeded { .. }) => return Err(error),
            Err(error) => {
                warnings.push(format!(
                    "page {} block {} asset generation failed: {error}",
                    page.page_index, index
                ));
            }
        }
    }
    for (index, content) in content_updates {
        page.blocks[index].content = Some(content);
    }
    for (index, relative_path, md5) in metadata_updates {
        page.blocks[index].metadata.insert(
            "asset_path".into(),
            serde_json::Value::String(relative_path.to_string_lossy().into_owned()),
        );
        page.blocks[index]
            .metadata
            .insert("asset_md5".into(), serde_json::Value::String(md5));
    }
    Ok((assets, warnings))
}

fn attach_table_images(
    block: &crate::ContentBlock,
    image: &Regex,
    assets: &mut Vec<Asset>,
    generated_paths: &mut HashSet<PathBuf>,
    existing_paths: &HashSet<PathBuf>,
    remaining: &mut usize,
    limit: usize,
) -> Result<(Option<String>, Vec<String>)> {
    let Some(content) = block.content.as_ref() else {
        return Ok((None, Vec::new()));
    };
    let mut warnings = Vec::new();
    let mut rewritten = String::with_capacity(content.len());
    let mut end = 0;
    for capture in image.captures_iter(content) {
        let matched = capture.get(0).unwrap();
        let subtype = capture.get(1).unwrap().as_str().to_ascii_lowercase();
        let Ok(extension) = image_extension(&subtype) else {
            warnings.push(format!(
                "unsupported table image media type: image/{subtype}; leaving data URI as-is"
            ));
            continue;
        };
        let encoded = capture.get(2).unwrap().as_str();
        let Some(upper) = base64_decoded_upper_bound(encoded) else {
            return Err(asset_limit_overflow(limit));
        };
        if upper > *remaining {
            // Resolve a possible duplicate without allocating the decoded asset.
            let (md5, decoded) = match streamed_base64_md5(encoded, limit) {
                Ok(v) => v,
                Err(Error::InvalidInput(message)) => {
                    warnings.push(format!("{message}; leaving data URI as-is"));
                    continue;
                }
                Err(error) => return Err(error),
            };
            let relative_path = PathBuf::from(format!("assets/table-image-{md5}.{extension}"));
            if existing_paths.contains(&relative_path) || generated_paths.contains(&relative_path) {
                rewritten.push_str(&content[end..matched.start()]);
                rewritten.push_str(&format!(r#"src="{}""#, relative_path.to_string_lossy()));
                end = matched.end();
                continue;
            }
            return Err(asset_limit(
                limit,
                limit.saturating_sub(*remaining).saturating_add(decoded),
            ));
        }
        let mut data = vec![0; upper];
        let decoded = match STANDARD.decode_slice(encoded, &mut data) {
            Ok(decoded) => decoded,
            Err(error) => {
                warnings.push(format!(
                    "invalid table image data URI: {error}; leaving data URI as-is"
                ));
                continue;
            }
        };
        data.truncate(decoded);
        let md5 = format!("{:x}", md5::compute(&data));
        let relative_path = PathBuf::from(format!("assets/table-image-{md5}.{extension}"));
        rewritten.push_str(&content[end..matched.start()]);
        rewritten.push_str(&format!(r#"src="{}""#, relative_path.to_string_lossy()));
        end = matched.end();
        if !existing_paths.contains(&relative_path) && generated_paths.insert(relative_path.clone())
        {
            *remaining -= data.len();
            assets.push(Asset {
                kind: AssetKind::Image,
                relative_path,
                media_type: format!("image/{subtype}"),
                data: Bytes::from(data),
                md5,
            });
        }
    }
    if end != 0 {
        rewritten.push_str(&content[end..]);
        return Ok((Some(rewritten), warnings));
    }
    Ok((None, warnings))
}

fn base64_decoded_upper_bound(encoded: &str) -> Option<usize> {
    let len = encoded.len();
    let padding = encoded
        .as_bytes()
        .iter()
        .rev()
        .take_while(|&&byte| byte == b'=')
        .take(2)
        .count();
    if padding != 0 && len.is_multiple_of(4) {
        return len.checked_div(4)?.checked_mul(3)?.checked_sub(padding);
    }
    let tail = match len % 4 {
        0 => 0,
        2 => 1,
        3 => 2,
        _ => 3,
    };
    len.checked_div(4)?.checked_mul(3)?.checked_add(tail)
}

fn streamed_base64_md5(encoded: &str, limit: usize) -> Result<(String, usize)> {
    let mut decoder = base64::read::DecoderReader::new(encoded.as_bytes(), &STANDARD);
    let mut context = md5::Context::new();
    let mut total = 0usize;
    let mut buffer = [0; 1024];
    loop {
        let read = decoder
            .read(&mut buffer)
            .map_err(|e| Error::InvalidInput(format!("invalid table image data URI: {e}")))?;
        if read == 0 {
            break;
        }
        total = total
            .checked_add(read)
            .ok_or_else(|| asset_limit_overflow(limit))?;
        context.consume(&buffer[..read]);
    }
    Ok((format!("{:x}", context.compute()), total))
}

fn asset_limit(limit: usize, actual: usize) -> Error {
    Error::LimitExceeded {
        resource: "total asset bytes",
        limit: limit as u64,
        actual: actual as u64,
    }
}

fn asset_limit_overflow(limit: usize) -> Error {
    Error::LimitExceeded {
        resource: "total asset bytes",
        limit: limit as u64,
        actual: u64::MAX,
    }
}

struct CappedWriter {
    data: Vec<u8>,
    limit: usize,
    attempted: usize,
}

impl CappedWriter {
    fn new(limit: usize) -> Self {
        Self {
            data: Vec::new(),
            limit,
            attempted: 0,
        }
    }
}

impl Write for CappedWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.attempted = self.data.len().saturating_add(bytes.len());
        if self.attempted > self.limit {
            return Err(io::Error::other("asset output limit"));
        }
        self.data.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn image_extension(subtype: &str) -> Result<&'static str> {
    match subtype {
        "png" => Ok("png"),
        "jpeg" | "jpg" => Ok("jpg"),
        "gif" => Ok("gif"),
        "webp" => Ok("webp"),
        "bmp" => Ok("bmp"),
        "tiff" => Ok("tiff"),
        _ => Err(Error::InvalidInput(format!(
            "unsupported table image media type: image/{subtype}"
        ))),
    }
}

fn asset_crop_bounds(
    image: &image::RgbImage,
    bbox: crate::NormalizedBbox,
) -> Result<(u32, u32, u32, u32)> {
    let (width, height) = (image.width(), image.height());
    if width == 0 || height == 0 {
        return Err(Error::Image("cannot crop an empty page image".into()));
    }
    let x = (bbox.left * width as f32)
        .floor()
        .clamp(0.0, width as f32 - 1.0) as u32;
    let y = (bbox.top * height as f32)
        .floor()
        .clamp(0.0, height as f32 - 1.0) as u32;
    let right = (bbox.right * width as f32)
        .ceil()
        .clamp((x + 1) as f32, width as f32) as u32;
    let bottom = (bbox.bottom * height as f32)
        .ceil()
        .clamp((y + 1) as f32, height as f32) as u32;
    Ok((x, y, right, bottom))
}

fn asset_crop_from_bounds(
    image: &image::RgbImage,
    (x, y, right, bottom): (u32, u32, u32, u32),
    rotation: Option<Rotation>,
) -> image::RgbImage {
    let crop = image::imageops::crop_imm(image, x, y, right - x, bottom - y).to_image();
    match rotation {
        Some(Rotation::Deg90) => image::imageops::rotate90(&crop),
        Some(Rotation::Deg180) => image::imageops::rotate180(&crop),
        Some(Rotation::Deg270) => image::imageops::rotate270(&crop),
        _ => crop,
    }
}

#[cfg(test)]
mod tests {
    use super::{attach_assets, extend_assets, order_window};
    use crate::{Asset, AssetKind, BlockKind, ContentBlock, Error, NormalizedBbox, PageResult};
    use base64::{Engine as _, engine::general_purpose::STANDARD};
    use bytes::Bytes;
    use image::RgbImage;
    use serde_json::Map;
    use std::{collections::HashSet, io::Cursor, sync::Arc};

    fn attach(page: &mut PageResult, image: &RgbImage, limit: usize) -> crate::Result<Vec<Asset>> {
        attach_assets(page, image, 0, limit, &HashSet::new()).map(|(assets, _)| assets)
    }

    #[test]
    fn order_window_sorts_placeholders_with_successes_and_fills_missing_indices() {
        let page = |index: usize, page_size: [f32; 2]| PageResult {
            page_index: index,
            page_size,
            blocks: Vec::new(),
        };
        // Jobs complete out of order: page 2's placeholder is ready before page 0's result.
        let extracted = vec![
            (2, None, page(2, [5.0, 6.0])),
            (
                0,
                Some(Arc::new(RgbImage::new(1, 1))),
                page(0, [10.0, 20.0]),
            ),
        ];
        let ordered = order_window(extracted, 0..3);
        let indices: Vec<_> = ordered.iter().map(|(index, _, _)| *index).collect();
        assert_eq!(indices, vec![0, 1, 2]);
        // Success keeps its source image; the render-failed page 1 got a placeholder with
        // unknown size; the extraction-failed page 2 keeps its rendered size, no source.
        assert!(ordered[0].1.is_some());
        assert_eq!(ordered[1].2.page_size, [0.0, 0.0]);
        assert!(ordered[1].1.is_none());
        assert_eq!(ordered[2].2.page_size, [5.0, 6.0]);
        assert!(ordered[2].1.is_none());
    }

    #[test]
    fn assets_are_two_times_the_source_crop_and_recorded_on_blocks() {
        let mut page = PageResult {
            page_index: 3,
            page_size: [10.0, 10.0],
            blocks: vec![ContentBlock {
                kind: BlockKind::new(BlockKind::IMAGE),
                bbox: NormalizedBbox::new(0.0, 0.0, 1.0, 1.0).unwrap(),
                angle: None,
                content: Some("figure".into()),
                merge_previous: false,
                metadata: Map::new(),
            }],
        };
        let assets = attach(&mut page, &RgbImage::new(10, 10), usize::MAX).unwrap();
        let decoded = image::load_from_memory(&assets[0].data).unwrap();
        assert_eq!((decoded.width(), decoded.height()), (20, 20));
        let mut previous_encoding = Vec::new();
        image::DynamicImage::ImageRgb8(RgbImage::new(20, 20))
            .write_to(
                &mut Cursor::new(&mut previous_encoding),
                image::ImageFormat::Png,
            )
            .unwrap();
        assert_eq!(assets[0].data.as_ref(), previous_encoding);
        assert!(
            assets[0]
                .relative_path
                .to_string_lossy()
                .starts_with("assets/3-0-image-")
        );
        assert_eq!(
            page.blocks[0].metadata["asset_path"],
            assets[0].relative_path.to_string_lossy().as_ref()
        );
    }

    #[test]
    fn asset_total_limit_accepts_boundary_and_rejects_before_extend() {
        let asset = |data: &'static [u8]| Asset {
            kind: AssetKind::Image,
            relative_path: format!("asset-{}.png", data[0]).into(),
            media_type: "image/png".into(),
            data: Bytes::from_static(data),
            md5: String::new(),
        };
        let mut assets = vec![asset(b"abc")];
        let mut used = 3;
        let mut paths = HashSet::from([assets[0].relative_path.clone()]);
        extend_assets(&mut assets, &mut used, &mut paths, vec![asset(b"de")], 5).unwrap();
        assert_eq!(assets.len(), 2);
        assert_eq!(used, 5);
        let paths_before_failure = paths.clone();
        let error =
            extend_assets(&mut assets, &mut used, &mut paths, vec![asset(b"f")], 5).unwrap_err();
        assert!(matches!(
            error,
            Error::LimitExceeded {
                resource: "total asset bytes",
                limit: 5,
                actual: 6
            }
        ));
        assert_eq!(assets.len(), 2);
        assert_eq!(used, 5);
        assert_eq!(paths, paths_before_failure);
    }

    #[test]
    fn table_data_images_become_image_assets_and_relative_sources() {
        let mut png = Vec::new();
        image::DynamicImage::ImageRgb8(RgbImage::new(1, 1))
            .write_to(&mut Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();
        let mut page = PageResult {
            page_index: 2,
            page_size: [10.0, 10.0],
            blocks: vec![ContentBlock {
                kind: BlockKind::new(BlockKind::TABLE),
                bbox: NormalizedBbox::new(0.0, 0.0, 1.0, 1.0).unwrap(),
                angle: None,
                content: Some(format!(
                    r#"<table><tr><td><img src="data:image/png;base64,{}"/></td></tr></table>"#,
                    STANDARD.encode(&png)
                )),
                merge_previous: false,
                metadata: Map::new(),
            }],
        };

        let assets = attach(&mut page, &RgbImage::new(10, 10), usize::MAX).unwrap();
        let image = assets
            .iter()
            .find(|asset| asset.kind == AssetKind::Image)
            .unwrap();
        let content = page.blocks[0].content.as_deref().unwrap();
        assert_eq!(image.data.as_ref(), png);
        assert!(
            image
                .relative_path
                .to_string_lossy()
                .starts_with("assets/table-image-")
        );
        assert!(content.contains(&format!(
            r#"src="{}""#,
            image.relative_path.to_string_lossy()
        )));
        assert!(!content.contains("data:"));
    }

    #[test]
    fn repeated_table_data_image_uses_one_asset_and_one_path() {
        let png = vec![7; 1024];
        let uri = STANDARD.encode(&png);
        let make_page = || PageResult {
            page_index: 0,
            page_size: [1.0, 1.0],
            blocks: vec![ContentBlock {
                kind: BlockKind::new(BlockKind::TABLE),
                bbox: NormalizedBbox::new(0.0, 0.0, 1.0, 1.0).unwrap(),
                angle: None,
                content: Some(format!(
                    r#"<img src="data:image/png;base64,{uri}"><img src="data:image/png;base64,{uri}">"#
                )),
                merge_previous: false,
                metadata: Map::new(),
            }],
        };
        let mut sizing_page = make_page();
        let exact = attach(&mut sizing_page, &RgbImage::new(1, 1), usize::MAX)
            .unwrap()
            .iter()
            .map(|asset| asset.data.len())
            .sum();
        let mut page = make_page();
        let assets = attach(&mut page, &RgbImage::new(1, 1), exact).unwrap();
        assert_eq!(
            assets
                .iter()
                .filter(|asset| asset.kind == AssetKind::Image)
                .count(),
            1
        );
        let path = assets[0].relative_path.to_string_lossy();
        assert_eq!(
            page.blocks[0]
                .content
                .as_ref()
                .unwrap()
                .matches(path.as_ref())
                .count(),
            2
        );
    }

    #[test]
    fn incremental_assets_across_pages_are_cumulative_deduplicated_and_atomic() {
        let shared = vec![7; 128];
        let make_page = |page_index, data: &[u8]| PageResult {
            page_index,
            page_size: [2.0, 2.0],
            blocks: vec![ContentBlock {
                kind: BlockKind::new(BlockKind::TABLE),
                bbox: NormalizedBbox::new(0.0, 0.0, 1.0, 1.0).unwrap(),
                angle: None,
                content: Some(format!(
                    r#"<img src="data:image/png;base64,{}">"#,
                    STANDARD.encode(data)
                )),
                merge_previous: false,
                metadata: Map::new(),
            }],
        };
        let image = RgbImage::new(2, 2);
        let mut pages = Vec::new();
        let mut assets = Vec::new();
        let mut used = 0;
        let mut paths = HashSet::new();

        for page_index in 0..2 {
            let mut page = make_page(page_index, &shared);
            let (generated, _) =
                attach_assets(&mut page, &image, used, usize::MAX, &paths).unwrap();
            extend_assets(&mut assets, &mut used, &mut paths, generated, usize::MAX).unwrap();
            pages.push(page);
        }

        assert_eq!(
            used,
            assets.iter().map(|asset| asset.data.len()).sum::<usize>()
        );
        assert_eq!(assets.len(), 3);
        assert_eq!(paths.len(), assets.len());
        let shared_path = assets
            .iter()
            .find(|asset| {
                asset
                    .relative_path
                    .to_string_lossy()
                    .starts_with("assets/table-image-")
            })
            .unwrap()
            .relative_path
            .clone();
        assert_eq!(
            assets
                .iter()
                .filter(|asset| asset.relative_path == shared_path)
                .count(),
            1
        );
        assert!(pages.iter().all(|page| {
            page.blocks[0]
                .content
                .as_ref()
                .unwrap()
                .contains(shared_path.to_string_lossy().as_ref())
        }));

        let used_before_failure = used;
        let paths_before_failure = paths.clone();
        let output_before_failure = assets
            .iter()
            .map(|asset| (asset.relative_path.clone(), asset.data.clone()))
            .collect::<Vec<_>>();
        let mut failed_page = make_page(2, &[9; 128]);
        let failed_content = failed_page.blocks[0].content.clone();

        assert!(attach_assets(&mut failed_page, &image, used, used, &paths).is_err());
        assert_eq!(used, used_before_failure);
        assert_eq!(paths, paths_before_failure);
        assert_eq!(
            assets
                .iter()
                .map(|asset| (asset.relative_path.clone(), asset.data.clone()))
                .collect::<Vec<_>>(),
            output_before_failure
        );
        assert_eq!(pages.len(), 2);
        assert_eq!(failed_page.blocks[0].content, failed_content);
        assert!(failed_page.blocks[0].metadata.is_empty());
    }

    #[test]
    fn large_table_data_uri_is_rejected_before_allocation_and_is_atomic() {
        let original = format!(
            r#"<img src="data:image/png;base64,{}">"#,
            STANDARD.encode(vec![1; 4096])
        );
        let mut page = PageResult {
            page_index: 0,
            page_size: [1.0, 1.0],
            blocks: vec![ContentBlock {
                kind: BlockKind::new(BlockKind::TABLE),
                bbox: NormalizedBbox::new(0.0, 0.0, 1.0, 1.0).unwrap(),
                angle: None,
                content: Some(original.clone()),
                merge_previous: false,
                metadata: Map::new(),
            }],
        };

        let error = attach(&mut page, &RgbImage::new(1, 1), 128).unwrap_err();
        assert!(matches!(
            error,
            Error::LimitExceeded {
                resource: "total asset bytes",
                limit: 128,
                actual: 4096
            }
        ));
        assert_eq!(page.blocks[0].content.as_deref(), Some(original.as_str()));
        assert!(page.blocks[0].metadata.is_empty());
    }

    #[test]
    fn large_compressible_crop_can_exceed_its_encoded_budget_in_raw_bytes() {
        let make_page = || PageResult {
            page_index: 0,
            page_size: [100.0, 100.0],
            blocks: vec![ContentBlock {
                kind: BlockKind::new(BlockKind::IMAGE),
                bbox: NormalizedBbox::new(0.0, 0.0, 1.0, 1.0).unwrap(),
                angle: None,
                content: None,
                merge_previous: false,
                metadata: Map::new(),
            }],
        };
        let image = RgbImage::new(100, 100);
        let mut sizing_page = make_page();
        let encoded = attach(&mut sizing_page, &image, usize::MAX).unwrap()[0]
            .data
            .len();
        assert!(100 * 100 * 4 * 3 > encoded * 50);
        let mut page = make_page();

        let assets = attach(&mut page, &image, encoded).unwrap();
        assert_eq!(assets[0].data.len(), encoded);
    }

    #[test]
    fn asset_generation_accepts_the_exact_encoded_boundary() {
        let make_page = || PageResult {
            page_index: 0,
            page_size: [1.0, 1.0],
            blocks: vec![ContentBlock {
                kind: BlockKind::new(BlockKind::IMAGE),
                bbox: NormalizedBbox::new(0.0, 0.0, 1.0, 1.0).unwrap(),
                angle: None,
                content: None,
                merge_previous: false,
                metadata: Map::new(),
            }],
        };
        let image = RgbImage::new(1, 1);
        let mut sizing_page = make_page();
        let exact = attach(&mut sizing_page, &image, usize::MAX).unwrap()[0]
            .data
            .len();
        let mut page = make_page();

        let assets = attach(&mut page, &image, exact).unwrap();
        assert_eq!(assets[0].data.len(), exact);
    }

    #[test]
    fn later_asset_failure_does_not_leave_earlier_metadata() {
        let block = |bbox| ContentBlock {
            kind: BlockKind::new(BlockKind::IMAGE),
            bbox,
            angle: None,
            content: None,
            merge_previous: false,
            metadata: Map::new(),
        };
        let mut page = PageResult {
            page_index: 0,
            page_size: [100.0, 100.0],
            blocks: vec![
                block(NormalizedBbox::new(0.0, 0.0, 0.01, 0.01).unwrap()),
                block(NormalizedBbox::new(0.0, 0.0, 1.0, 1.0).unwrap()),
            ],
        };
        let mut sizing_page = PageResult {
            page_index: 0,
            page_size: [100.0, 100.0],
            blocks: vec![block(NormalizedBbox::new(0.0, 0.0, 0.01, 0.01).unwrap())],
        };
        let first_asset_bytes = attach(&mut sizing_page, &RgbImage::new(100, 100), usize::MAX)
            .unwrap()[0]
            .data
            .len();

        assert!(attach(&mut page, &RgbImage::new(100, 100), first_asset_bytes).is_err());
        assert!(page.blocks.iter().all(|block| block.metadata.is_empty()));
    }

    #[test]
    fn unsupported_table_subtype_and_bad_base64_degrade_to_warnings() {
        let make_page = || {
            PageResult {
            page_index: 1,
            page_size: [1.0, 1.0],
            blocks: vec![ContentBlock {
                kind: BlockKind::new(BlockKind::TABLE),
                bbox: NormalizedBbox::new(0.0, 0.0, 1.0, 1.0).unwrap(),
                angle: None,
                content: Some(
                    r#"<table><tr><td><img src="data:image/heic;base64,AAAA"/></td></tr><tr><td><img src="data:image/png;base64,not!base64"/></td></tr></table>"#
                        .to_owned(),
                ),
                merge_previous: false,
                metadata: Map::new(),
            }],
        }
        };
        let mut page = make_page();
        let (assets, warnings) = attach_assets(
            &mut page,
            &RgbImage::new(1, 1),
            0,
            usize::MAX,
            &HashSet::new(),
        )
        .unwrap();
        // No table-image assets are materialized from the degraded data URIs.
        assert!(assets.iter().all(|asset| asset.kind != AssetKind::Image));
        assert_eq!(warnings.len(), 2);
        assert!(
            warnings
                .iter()
                .any(|w| w.contains("unsupported table image"))
        );
        assert!(
            warnings
                .iter()
                .any(|w| w.contains("invalid table image data URI"))
        );
        // Both data URIs are left as-is in the content.
        let content = page.blocks[0].content.as_deref().unwrap();
        assert!(content.contains("data:image/heic;base64,AAAA"));
        assert!(content.contains("data:image/png;base64,not!base64"));
    }

    #[test]
    fn per_block_asset_generation_failure_warns_and_skips_that_asset() {
        let mut page = PageResult {
            page_index: 0,
            page_size: [1.0, 1.0],
            blocks: vec![ContentBlock {
                kind: BlockKind::new(BlockKind::IMAGE),
                bbox: NormalizedBbox::new(0.0, 0.0, 1.0, 1.0).unwrap(),
                angle: None,
                content: None,
                merge_previous: false,
                metadata: Map::new(),
            }],
        };
        // An empty page image makes cropping fail with a recoverable image error.
        let (assets, warnings) = attach_assets(
            &mut page,
            &RgbImage::new(0, 0),
            0,
            usize::MAX,
            &HashSet::new(),
        )
        .unwrap();
        assert!(assets.is_empty());
        assert_eq!(warnings.len(), 1);
        assert!(
            warnings[0].contains("block 0 asset generation failed"),
            "{warnings:?}"
        );
    }

    #[test]
    fn aggregate_warnings_dedupes_sanitizes_and_caps() {
        use super::{MAX_DOCUMENT_WARNINGS, aggregate_warnings};
        let repeated = format!("Bearer s {}", "x".repeat(512));
        let many = vec![
            repeated.clone(),
            repeated.clone(),
            "page 1 failed: boom".into(),
        ];
        let mut warnings = aggregate_warnings(many);
        warnings.extend(aggregate_warnings(vec![
            "leaked data:image/png;base64,secret-tail".into(),
        ]));
        assert_eq!(warnings.len(), 3);
        assert!(!warnings[0].contains("Bearer s"));
        assert!(warnings[0].contains("Bearer [REDACTED]"));
        assert_eq!(warnings[1], "page 1 failed: boom");
        assert!(!warnings[2].contains("secret-tail"));
        assert!(warnings[2].contains("[REDACTED_DATA_URL]"));

        let many: Vec<String> = (0..MAX_DOCUMENT_WARNINGS + 20)
            .map(|i| format!("warning {i}"))
            .collect();
        assert_eq!(aggregate_warnings(many).len(), MAX_DOCUMENT_WARNINGS);
    }
}
