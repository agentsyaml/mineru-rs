use crate::{
    Asset, AssetKind, ClientConfig, Error, ParseOptions, PdfInput, Result, Rotation,
    document_postprocess, extractor::PageExtractor, pdf,
};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use bytes::Bytes;
use image::{DynamicImage, ImageFormat, imageops::FilterType};
use regex::Regex;
use std::{collections::HashSet, io::Cursor, path::PathBuf, sync::Arc};
use tokio::task::JoinSet;

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
    let mut pages = Vec::new();
    let mut assets = Vec::new();
    let window = config.limits.page_window_size;
    let mut first = start;
    while first <= end {
        let last = (first + window).min(end + 1);
        let rendered = pdf::render_window(
            pdf.clone(),
            (first..last).collect(),
            config.limits.clone(),
            config.render_workers,
        )
        .await?;
        if rendered.is_empty() {
            return Err(Error::Pdf("renderer returned no page".into()));
        }
        let mut jobs = JoinSet::new();
        for page in rendered {
            let index = page.index;
            let image = page.image;
            let source = image.clone();
            let extractor = extractor.clone();
            let options = options.clone();
            jobs.spawn(async move {
                let result = extractor
                    .extract_page(index, page.size, image, &options)
                    .await
                    .map_err(|source| Error::Page {
                        page: index,
                        source: Box::new(source),
                    })?;
                Ok::<_, Error>((index, source, result))
            });
        }
        let mut extracted = Vec::new();
        while let Some(job) = jobs.join_next().await {
            extracted.push(job.map_err(|error| Error::WorkerJoin(error.to_string()))??);
        }
        extracted.sort_unstable_by_key(|(index, _, _)| *index);
        for (_, source, result) in extracted {
            pages.push(result);
            let generated = attach_assets(pages.last_mut().unwrap(), &source)?;
            extend_assets(&mut assets, generated, config.limits.max_total_asset_bytes)?;
        }
        first = pages.last().map_or(first, |page| page.page_index + 1);
    }
    extractor
        .merge_cross_page_tables(&mut pages, &options)
        .await?;
    let used_assets = assets
        .iter()
        .fold(0usize, |sum, asset| sum.saturating_add(asset.data.len()));
    let preview = crate::preview::generate(
        &pdf::source_bytes(&pdf),
        &pages,
        &stem,
        &config.limits,
        config
            .limits
            .max_total_asset_bytes
            .saturating_sub(used_assets),
    )?;
    extend_assets(
        &mut assets,
        vec![preview],
        config.limits.max_total_asset_bytes,
    )?;
    let mut document = document_postprocess::build(pages);
    document.assets = assets;
    Ok(document)
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

fn extend_assets(assets: &mut Vec<Asset>, mut generated: Vec<Asset>, limit: usize) -> Result<()> {
    let mut paths = assets
        .iter()
        .map(|asset| asset.relative_path.clone())
        .collect::<HashSet<_>>();
    generated.retain(|asset| paths.insert(asset.relative_path.clone()));
    let actual = assets
        .iter()
        .chain(&generated)
        .fold(0usize, |total, asset| {
            total.saturating_add(asset.data.len())
        });
    if actual > limit {
        return Err(Error::LimitExceeded {
            resource: "total asset bytes",
            limit: limit as u64,
            actual: actual as u64,
        });
    }
    assets.extend(generated);
    Ok(())
}

fn attach_assets(page: &mut crate::PageResult, image: &image::RgbImage) -> Result<Vec<Asset>> {
    let mut assets = Vec::new();
    for (index, block) in page.blocks.iter_mut().enumerate() {
        if block.kind.as_str() == crate::BlockKind::TABLE {
            attach_table_images(block, page.page_index, index, &mut assets)?;
        }
        let kind = match block.kind.as_str() {
            crate::BlockKind::IMAGE | crate::BlockKind::IMAGE_BLOCK => AssetKind::Image,
            crate::BlockKind::TABLE => AssetKind::Table,
            crate::BlockKind::EQUATION | crate::BlockKind::EQUATION_BLOCK => AssetKind::Equation,
            crate::BlockKind::CHART => AssetKind::Chart,
            _ => continue,
        };
        let crop = asset_crop(image, block.bbox, block.angle)?;
        // Assets, unlike model input, intentionally retain a 2x crop for output fidelity.
        let crop = image::imageops::resize(
            &crop,
            crop.width().saturating_mul(2),
            crop.height().saturating_mul(2),
            FilterType::Lanczos3,
        );
        let mut data = Vec::new();
        DynamicImage::ImageRgb8(crop)
            .write_to(&mut Cursor::new(&mut data), ImageFormat::Png)
            .map_err(|e| Error::Image(e.to_string()))?;
        let md5 = format!("{:x}", md5::compute(&data));
        let kind_name = match kind {
            AssetKind::Image => "image",
            AssetKind::Table => "table",
            AssetKind::Equation => "equation",
            AssetKind::Chart => "chart",
            AssetKind::Other(_) => "other",
        };
        let relative_path = PathBuf::from(format!(
            "assets/{}-{}-{}-{}.png",
            page.page_index,
            index,
            kind_name,
            &md5[..8]
        ));
        block.metadata.insert(
            "asset_path".into(),
            serde_json::Value::String(relative_path.to_string_lossy().into_owned()),
        );
        block
            .metadata
            .insert("asset_md5".into(), serde_json::Value::String(md5));
        assets.push(Asset {
            kind,
            relative_path,
            media_type: "image/png".into(),
            data: Bytes::from(data),
            md5: block.metadata["asset_md5"].as_str().unwrap().to_owned(),
        });
    }
    Ok(assets)
}

fn attach_table_images(
    block: &mut crate::ContentBlock,
    _page_index: usize,
    _block_index: usize,
    assets: &mut Vec<Asset>,
) -> Result<()> {
    let Some(content) = block.content.as_ref() else {
        return Ok(());
    };
    let image = Regex::new(r#"(?i)src\s*=\s*[\"']data:image/([a-z0-9.+-]+);base64,([^\"']*)[\"']"#)
        .unwrap();
    let mut rewritten = String::with_capacity(content.len());
    let mut end = 0;
    for capture in image.captures_iter(content) {
        let matched = capture.get(0).unwrap();
        let subtype = capture.get(1).unwrap().as_str().to_ascii_lowercase();
        let extension = image_extension(&subtype)?;
        let data = STANDARD
            .decode(capture.get(2).unwrap().as_str())
            .map_err(|e| Error::InvalidInput(format!("invalid table image data URI: {e}")))?;
        let md5 = format!("{:x}", md5::compute(&data));
        let relative_path = PathBuf::from(format!("assets/table-image-{md5}.{extension}"));
        rewritten.push_str(&content[end..matched.start()]);
        rewritten.push_str(&format!(r#"src="{}""#, relative_path.to_string_lossy()));
        end = matched.end();
        if !assets
            .iter()
            .any(|asset| asset.relative_path == relative_path)
        {
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
        block.content = Some(rewritten);
    }
    Ok(())
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

fn asset_crop(
    image: &image::RgbImage,
    bbox: crate::NormalizedBbox,
    rotation: Option<Rotation>,
) -> Result<image::RgbImage> {
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
    let crop = image::imageops::crop_imm(image, x, y, right - x, bottom - y).to_image();
    Ok(match rotation {
        Some(Rotation::Deg90) => image::imageops::rotate90(&crop),
        Some(Rotation::Deg180) => image::imageops::rotate180(&crop),
        Some(Rotation::Deg270) => image::imageops::rotate270(&crop),
        _ => crop,
    })
}

#[cfg(test)]
mod tests {
    use super::{attach_assets, extend_assets};
    use crate::{Asset, AssetKind, BlockKind, ContentBlock, Error, NormalizedBbox, PageResult};
    use base64::{Engine as _, engine::general_purpose::STANDARD};
    use bytes::Bytes;
    use image::RgbImage;
    use serde_json::Map;
    use std::io::Cursor;

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
        let assets = attach_assets(&mut page, &RgbImage::new(10, 10)).unwrap();
        let decoded = image::load_from_memory(&assets[0].data).unwrap();
        assert_eq!((decoded.width(), decoded.height()), (20, 20));
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
        extend_assets(&mut assets, vec![asset(b"de")], 5).unwrap();
        assert_eq!(assets.len(), 2);
        let error = extend_assets(&mut assets, vec![asset(b"f")], 5).unwrap_err();
        assert!(matches!(
            error,
            Error::LimitExceeded {
                resource: "total asset bytes",
                limit: 5,
                actual: 6
            }
        ));
        assert_eq!(assets.len(), 2);
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

        let assets = attach_assets(&mut page, &RgbImage::new(10, 10)).unwrap();
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
        let png = STANDARD.decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAIAAACQd1PeAAAACklEQVR4nGNgAAAAAgABSK+kcQAAAABJRU5ErkJggg==").unwrap();
        let uri = STANDARD.encode(&png);
        let mut page = PageResult {
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
        let assets = attach_assets(&mut page, &RgbImage::new(1, 1)).unwrap();
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
}
