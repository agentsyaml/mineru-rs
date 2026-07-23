use crate::vlm_http::ByteBudget;
use crate::*;
#[cfg(test)]
use base64::{Engine as _, engine::general_purpose::STANDARD};
use futures_util::future::try_join_all;
use image::{DynamicImage, ImageFormat, RgbImage, imageops::FilterType};
use serde_json::Map;
use std::io::Cursor;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::Semaphore;

const COVERED_IMAGE_CAPTION: &str = "_covered_image_caption";
const POST_PROCESS_ORDER: &str = "mineru_post_process_order";

#[derive(Clone)]
struct SemanticCandidate {
    original_index: usize,
    block: ContentBlock,
    absorbed: Vec<ContentBlock>,
}

#[allow(dead_code)]
fn official_snapshot_block(block: VlmLayoutBlock) -> VlmResult<ModelBlock> {
    if block.block_type.is_empty() {
        return Err(protocol(
            "official model snapshot",
            "layout block type is required",
        ));
    }
    let angle = block
        .angle
        .ok_or_else(|| protocol("official model snapshot", "layout block angle is required"))?;
    let mut extra = block.metadata;
    let sub_type = extra
        .remove("sub_type")
        .and_then(|value| value.as_str().map(str::to_owned));
    for key in [
        "type",
        "bbox",
        "angle",
        "content",
        "merge_prev",
        COVERED_IMAGE_CAPTION,
        "_table_image_token_map",
        "_absorbed_by_table",
        "_skip_asset",
    ] {
        extra.remove(key);
    }
    Ok(ModelBlock {
        block_type: block.block_type.clone(),
        bbox: Some(block.bbox),
        angle: Some(angle),
        content: block.content,
        merge_prev: (block.block_type == BlockKind::TEXT)
            .then_some(block.merge_prev.unwrap_or(false)),
        sub_type,
        extra,
    })
}

fn protocol(operation: &'static str, message: impl Into<String>) -> VlmError {
    VlmError::Protocol {
        operation,
        message: message.into(),
    }
}
fn to_vlm(block: ContentBlock) -> VlmLayoutBlock {
    VlmLayoutBlock {
        block_type: block.kind.as_str().into(),
        bbox: block.bbox,
        angle: block.angle,
        content: block.content,
        merge_prev: Some(block.merge_previous),
        metadata: block.metadata,
    }
}
fn from_vlm(block: VlmLayoutBlock) -> ContentBlock {
    ContentBlock {
        kind: BlockKind::new(block.block_type),
        bbox: block.bbox,
        angle: block.angle,
        content: block.content,
        merge_previous: block.merge_prev.unwrap_or(false),
        metadata: block.metadata,
    }
}
fn area(b: NormalizedBbox) -> f32 {
    (b.right - b.left) * (b.bottom - b.top)
}
fn covered_by(inner: NormalizedBbox, outer: NormalizedBbox) -> bool {
    area(inner) > 0.0
        && (inner.right.min(outer.right) - inner.left.max(outer.left)).max(0.0)
            * (inner.bottom.min(outer.bottom) - inner.top.max(outer.top)).max(0.0)
            / area(inner)
            >= 0.9
}
fn is_known_kind(kind: &str) -> bool {
    matches!(
        kind,
        BlockKind::TEXT
            | BlockKind::TITLE
            | BlockKind::TABLE
            | BlockKind::EQUATION
            | BlockKind::FORMULA_NUMBER
            | BlockKind::CODE
            | BlockKind::ALGORITHM
            | BlockKind::ASIDE_TEXT
            | BlockKind::REF_TEXT
            | BlockKind::INDEX
            | BlockKind::PHONETIC
            | BlockKind::LIST_ITEM
            | BlockKind::TABLE_CAPTION
            | BlockKind::IMAGE_CAPTION
            | BlockKind::CODE_CAPTION
            | BlockKind::TABLE_FOOTNOTE
            | BlockKind::IMAGE_FOOTNOTE
            | BlockKind::HEADER
            | BlockKind::FOOTER
            | BlockKind::PAGE_NUMBER
            | BlockKind::PAGE_FOOTNOTE
            | BlockKind::IMAGE
            | BlockKind::CHART
            | BlockKind::LIST
            | BlockKind::IMAGE_BLOCK
            | BlockKind::EQUATION_BLOCK
            | BlockKind::UNKNOWN
    )
}
fn is_paratext(kind: &str) -> bool {
    matches!(
        kind,
        BlockKind::HEADER
            | BlockKind::FOOTER
            | BlockKind::PAGE_NUMBER
            | BlockKind::ASIDE_TEXT
            | BlockKind::PAGE_FOOTNOTE
            | BlockKind::UNKNOWN
    )
}
fn image_pixel_limit(width: u32, height: u32, limit: u64) -> VlmResult<()> {
    let actual = u64::from(width)
        .checked_mul(u64::from(height))
        .unwrap_or(u64::MAX);
    if actual > limit {
        return Err(VlmError::LimitExceeded {
            resource: "image pixels",
            limit,
            actual,
        });
    }
    Ok(())
}

fn crop_rect(image: &RgbImage, bbox: NormalizedBbox) -> (u32, u32, u32, u32) {
    let x = (bbox.left * image.width() as f32)
        .floor()
        .clamp(0., image.width().saturating_sub(1) as f32) as u32;
    let y = (bbox.top * image.height() as f32)
        .floor()
        .clamp(0., image.height().saturating_sub(1) as f32) as u32;
    let right = (bbox.right * image.width() as f32)
        .ceil()
        .clamp((x + 1) as f32, image.width() as f32) as u32;
    let bottom = (bbox.bottom * image.height() as f32)
        .ceil()
        .clamp((y + 1) as f32, image.height() as f32) as u32;
    (x, y, right - x, bottom - y)
}

fn semantic_crop(
    image: &RgbImage,
    bbox: NormalizedBbox,
    angle: Option<Rotation>,
    max_pixels: u64,
) -> VlmResult<RgbImage> {
    let (x, y, width, height) = crop_rect(image, bbox);
    image_pixel_limit(width, height, max_pixels)?;
    let out = image::imageops::crop_imm(image, x, y, width, height).to_image();
    Ok(match angle {
        Some(Rotation::Deg90) => image::imageops::rotate270(&out),
        Some(Rotation::Deg180) => image::imageops::rotate180(&out),
        Some(Rotation::Deg270) => image::imageops::rotate90(&out),
        _ => out,
    })
}
fn priority_for(
    n: usize,
    priority: VlmBatchPriority,
    incremental: bool,
) -> VlmResult<Vec<VlmPriority>> {
    match priority {
        VlmBatchPriority::All(None) if incremental => Ok((0..n).map(|i| Some(i as i32)).collect()),
        VlmBatchPriority::All(p) => Ok(vec![p; n]),
        VlmBatchPriority::PerItem(p) if p.len() == n => Ok(p),
        VlmBatchPriority::PerItem(p) => Err(protocol(
            "priority",
            format!("expected {n} priorities, got {}", p.len()),
        )),
    }
}

#[derive(Debug, Clone)]
pub struct MinerUVlmPreprocessor {
    pub config: MinerUVlmConfig,
}
impl MinerUVlmPreprocessor {
    fn prompt_for(&self, kind: &str) -> String {
        self.config
            .prompts
            .get(kind)
            .filter(|prompt| !prompt.trim().is_empty())
            .or_else(|| self.config.prompts.get("[default]"))
            .filter(|prompt| !prompt.trim().is_empty())
            .cloned()
            .unwrap_or_default()
    }
    fn sampling_for(&self, kind: &str) -> Option<SamplingParams> {
        self.config
            .sampling_params
            .get(kind)
            .or_else(|| self.config.sampling_params.get("[default]"))
            .cloned()
    }
    pub fn resize_by_need(&self, image: DynamicImage) -> VlmResult<DynamicImage> {
        self.resize_by_need_capped(image, u64::MAX)
    }
    fn resize_by_need_capped(
        &self,
        image: DynamicImage,
        max_pixels: u64,
    ) -> VlmResult<DynamicImage> {
        image_pixel_limit(image.width(), image.height(), max_pixels)?;
        let mut out = image.to_rgb8();
        let w = out.width();
        let h = out.height();
        let ratio = self.config.max_image_edge_ratio.max(1);
        if w.max(h) > w.min(h).saturating_mul(ratio) {
            let (new_w, new_h) = if w >= h {
                (w, w.div_ceil(ratio))
            } else {
                (h.div_ceil(ratio), h)
            };
            image_pixel_limit(new_w, new_h, max_pixels)?;
            let mut padded = RgbImage::from_pixel(new_w, new_h, image::Rgb([255; 3]));
            image::imageops::overlay(
                &mut padded,
                &out,
                ((new_w - w) / 2) as i64,
                ((new_h - h) / 2) as i64,
            );
            out = padded;
        }
        let edge = out.width().min(out.height()).max(1);
        if edge < self.config.min_image_edge {
            let (new_width, new_height) = image_pipeline::min_edge_dimensions(
                out.width(),
                out.height(),
                self.config.min_image_edge,
            );
            image_pixel_limit(new_width, new_height, max_pixels)?;
            out = image::imageops::resize(&out, new_width, new_height, FilterType::CatmullRom);
        }
        Ok(DynamicImage::ImageRgb8(out))
    }
    fn prepare_rgb_for_layout_capped(
        &self,
        image: &RgbImage,
        max_pixels: u64,
    ) -> VlmResult<VlmPreparedLayout> {
        image_pixel_limit(
            self.config.layout_image_size.0,
            self.config.layout_image_size.1,
            max_pixels,
        )?;
        let resized = image::imageops::resize(
            image,
            self.config.layout_image_size.0,
            self.config.layout_image_size.1,
            FilterType::CatmullRom,
        );
        let mut data = Vec::new();
        DynamicImage::ImageRgb8(resized)
            .write_to(&mut Cursor::new(&mut data), ImageFormat::Png)
            .map_err(|e| protocol("layout image", e.to_string()))?;
        Ok(VlmPreparedLayout {
            image: VlmEncodedImage {
                data: data.into(),
                media_type: "image/png".into(),
                width: self.config.layout_image_size.0,
                height: self.config.layout_image_size.1,
            },
        })
    }
    pub fn prepare_for_layout(&self, image: DynamicImage) -> VlmResult<VlmPreparedLayout> {
        self.prepare_for_layout_capped(image, u64::MAX)
    }
    fn prepare_for_layout_capped(
        &self,
        image: DynamicImage,
        max_pixels: u64,
    ) -> VlmResult<VlmPreparedLayout> {
        image_pixel_limit(image.width(), image.height(), max_pixels)?;
        self.prepare_rgb_for_layout_capped(&image.to_rgb8(), max_pixels)
    }
    pub fn parse_layout_output(&self, output: &str) -> VlmResult<Vec<VlmLayoutBlock>> {
        self.parse_layout_output_capped(output, usize::MAX)
    }
    fn parse_layout_output_capped(
        &self,
        output: &str,
        max_layout_blocks: usize,
    ) -> VlmResult<Vec<VlmLayoutBlock>> {
        let output = output.replace(
            "<|ref_start|>unknown<|ref_end|>",
            "<|ref_start|>image<|ref_end|>",
        );
        layout::parse_layout(&output, max_layout_blocks)
            .map_err(|error| match error {
                Error::LimitExceeded { actual, .. } => VlmError::LimitExceeded {
                    resource: "layout blocks",
                    limit: max_layout_blocks as u64,
                    actual,
                },
                error => protocol("layout parse", error.to_string()),
            })
            .map(|v| {
                let mut v: Vec<_> = v.into_iter().map(to_vlm).collect();
                let tables: Vec<_> = v
                    .iter()
                    .filter(|b| b.block_type == BlockKind::TABLE)
                    .map(|b| b.bbox)
                    .collect();
                v.retain(|b| {
                    !matches!(
                        b.block_type.as_str(),
                        BlockKind::TEXT | BlockKind::EQUATION | BlockKind::EQUATION_BLOCK
                    ) || !tables.iter().any(|t| covered_by(b.bbox, *t))
                });
                v
            })
    }
    pub fn prepare_for_extract(
        &self,
        image: &DynamicImage,
        blocks: &mut [VlmLayoutBlock],
        prompts: &[String],
        image_analysis: Option<bool>,
    ) -> VlmResult<VlmPreparedExtraction> {
        self.prepare_for_extract_capped(image, blocks, prompts, image_analysis, usize::MAX)
    }
    fn prepare_for_extract_capped(
        &self,
        image: &DynamicImage,
        blocks: &mut [VlmLayoutBlock],
        prompts: &[String],
        image_analysis: Option<bool>,
        max_semantic_requests: usize,
    ) -> VlmResult<VlmPreparedExtraction> {
        self.prepare_for_extract_limited(
            image,
            blocks,
            prompts,
            image_analysis,
            max_semantic_requests,
            u64::MAX,
        )
    }
    fn prepare_for_extract_limited(
        &self,
        image: &DynamicImage,
        blocks: &mut [VlmLayoutBlock],
        prompts: &[String],
        image_analysis: Option<bool>,
        max_semantic_requests: usize,
        max_pixels: u64,
    ) -> VlmResult<VlmPreparedExtraction> {
        let candidates =
            self.semantic_candidates(blocks, prompts, image_analysis, max_semantic_requests)?;
        image_pixel_limit(image.width(), image.height(), max_pixels)?;
        let page = image.to_rgb8();
        let mut out = VlmPreparedExtraction {
            images: vec![],
            prompts: vec![],
            sampling: vec![],
            block_indices: vec![],
        };
        for candidate in candidates {
            let (image, prompt, sampling, block_index) =
                self.encode_semantic_candidate_capped(&page, blocks, &candidate, max_pixels)?;
            out.images.push(image);
            out.prompts.push(prompt);
            out.sampling.push(sampling);
            out.block_indices.push(block_index);
        }
        Ok(out)
    }
    fn semantic_candidates(
        &self,
        blocks: &mut [VlmLayoutBlock],
        prompts: &[String],
        image_analysis: Option<bool>,
        max_semantic_requests: usize,
    ) -> VlmResult<Vec<SemanticCandidate>> {
        let mut native: Vec<_> = blocks.iter().cloned().map(from_vlm).enumerate().collect();
        let suppressed = |kind: &str| prompts.iter().any(|p| p == kind) && is_known_kind(kind);
        let covered_captions: Vec<_> = native
            .iter()
            .filter(|(_, caption)| {
                caption.kind.as_str() == BlockKind::IMAGE_CAPTION
                    && native.iter().any(|other| {
                        matches!(
                            other.1.kind.as_str(),
                            BlockKind::IMAGE | BlockKind::CHART | BlockKind::IMAGE_BLOCK
                        ) && covered_by(caption.bbox, other.1.bbox)
                    })
            })
            .map(|(index, _)| *index)
            .collect();
        // `blocks` is a slice, so mark rather than compact caller-owned storage.
        for &index in &covered_captions {
            blocks[index]
                .metadata
                .insert(COVERED_IMAGE_CAPTION.into(), true.into());
        }
        native.retain(|(index, _)| !covered_captions.contains(index));
        let mut table_images = Vec::<(usize, Vec<usize>)>::new();
        for table in 0..native.len() {
            if native[table].1.kind.as_str() != BlockKind::TABLE || suppressed(BlockKind::TABLE) {
                continue;
            }
            let ids = (0..native.len())
                .filter(|&image| {
                    native[image].1.kind.as_str() == BlockKind::IMAGE
                        && covered_by(native[image].1.bbox, native[table].1.bbox)
                })
                .collect::<Vec<_>>();
            for &image in &ids {
                native[image]
                    .1
                    .metadata
                    .insert("_absorbed_by_table".into(), table.into());
                native[image]
                    .1
                    .metadata
                    .insert("_skip_asset".into(), true.into());
            }
            table_images.push((table, ids));
        }
        for (original_index, src) in &native {
            blocks[*original_index] = to_vlm(src.clone());
        }
        let analyze = image_analysis.unwrap_or(self.config.image_analysis);
        let mut candidates = Vec::new();
        for (index, (original_index, block)) in native.iter().enumerate() {
            let kind = block.kind.as_str();
            if block
                .metadata
                .get("_skip_asset")
                .and_then(serde_json::Value::as_bool)
                == Some(true)
                || matches!(
                    kind,
                    BlockKind::LIST | BlockKind::IMAGE_BLOCK | BlockKind::EQUATION_BLOCK
                )
                || suppressed(kind)
                || (matches!(kind, BlockKind::IMAGE | BlockKind::CHART) && !analyze)
            {
                continue;
            }
            if matches!(kind, BlockKind::IMAGE | BlockKind::CHART)
                && (native.iter().any(|other| {
                    other.1.kind.as_str() == BlockKind::IMAGE_BLOCK
                        && covered_by(block.bbox, other.1.bbox)
                }) || !((block.bbox.right - block.bbox.left > 0.1
                    && block.bbox.bottom - block.bbox.top > 0.1)
                    || area(block.bbox) > 0.01))
            {
                continue;
            }
            if candidates.len() >= max_semantic_requests {
                return Err(VlmError::LimitExceeded {
                    resource: "semantic requests per page",
                    limit: max_semantic_requests as u64,
                    actual: candidates.len().saturating_add(1) as u64,
                });
            }
            let absorbed = if kind == BlockKind::TABLE {
                table_images
                    .iter()
                    .find(|(i, _)| *i == index)
                    .map(|(_, ids)| ids.iter().map(|i| native[*i].1.clone()).collect())
                    .unwrap_or_default()
            } else {
                vec![]
            };
            candidates.push(SemanticCandidate {
                original_index: *original_index,
                block: block.clone(),
                absorbed,
            });
        }
        Ok(candidates)
    }
    fn check_table_candidate_allocations(
        &self,
        page: &RgbImage,
        candidate: &SemanticCandidate,
        max_pixels: u64,
    ) -> VlmResult<()> {
        let (_, _, raw_width, raw_height) = crop_rect(page, candidate.block.bbox);
        image_pixel_limit(raw_width, raw_height, max_pixels)?;
        let (mut width, mut height) = match candidate.block.angle {
            Some(Rotation::Deg90 | Rotation::Deg270) => (raw_height, raw_width),
            _ => (raw_width, raw_height),
        };
        let edge = width.min(height).max(1);
        if edge < 28 {
            (width, height) = image_pipeline::min_edge_dimensions(width, height, 28);
            image_pixel_limit(width, height, max_pixels)?;
        }
        if width.max(height) as f32 / width.min(height).max(1) as f32 > 50.0 {
            let side = width.max(height);
            image_pixel_limit(side, side, max_pixels)?;
        }
        for absorbed in &candidate.absorbed {
            let (_, _, width, height) = crop_rect(page, absorbed.bbox);
            image_pixel_limit(width, height, max_pixels)?;
        }
        Ok(())
    }
    fn encode_semantic_candidate_capped(
        &self,
        page: &RgbImage,
        blocks: &mut [VlmLayoutBlock],
        candidate: &SemanticCandidate,
        max_pixels: u64,
    ) -> VlmResult<(VlmEncodedImage, String, Option<SamplingParams>, usize)> {
        image_pixel_limit(page.width(), page.height(), max_pixels)?;
        let kind = candidate.block.kind.as_str();
        let (crop, tokens) = if kind == BlockKind::TABLE {
            self.check_table_candidate_allocations(page, candidate, max_pixels)?;
            image_pipeline::mask_and_encode_table_image(page, &candidate.block, &candidate.absorbed)
                .map_err(|e| protocol("extract image", e.to_string()))?
        } else {
            (
                semantic_crop(
                    page,
                    candidate.block.bbox,
                    candidate.block.angle,
                    max_pixels,
                )?,
                Map::new(),
            )
        };
        let crop = self
            .resize_by_need_capped(DynamicImage::ImageRgb8(crop), max_pixels)?
            .to_rgb8();
        if !tokens.is_empty() {
            blocks[candidate.original_index].metadata.insert(
                "_table_image_token_map".into(),
                serde_json::Value::Object(tokens),
            );
        }
        let data = image_pipeline::png_bytes(&crop)
            .map_err(|e| protocol("extract image", e.to_string()))?;
        Ok((
            VlmEncodedImage {
                data: data.into(),
                media_type: "image/png".into(),
                width: crop.width(),
                height: crop.height(),
            },
            self.prompt_for(kind),
            self.sampling_for(kind),
            candidate.original_index,
        ))
    }
    pub fn post_process(&self, blocks: Vec<VlmLayoutBlock>) -> VlmResult<Vec<VlmLayoutBlock>> {
        let mut native: Vec<_> = blocks.into_iter().map(from_vlm).collect();
        // Keep caller metadata in a non-internal envelope while the legacy cleaner consumes its keys.
        for block in &mut native {
            let external = block
                .metadata
                .iter()
                .filter(|(k, _)| {
                    !matches!(
                        k.as_str(),
                        "_table_image_token_map" | "_absorbed_by_table" | "_skip_asset"
                    ) && k.as_str() != COVERED_IMAGE_CAPTION
                })
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect::<Map<_, _>>();
            block.metadata.retain(|k, _| {
                matches!(
                    k.as_str(),
                    "_table_image_token_map" | "_absorbed_by_table" | "_skip_asset"
                ) || k == COVERED_IMAGE_CAPTION
            });
            if !external.is_empty() {
                block.metadata.insert(
                    "mineru_external_metadata".into(),
                    serde_json::Value::Object(external),
                );
            }
        }
        // Tables and images use the same lightweight normalization in both modes.
        for block in &mut native {
            if matches!(
                block.kind.as_str(),
                BlockKind::TABLE | BlockKind::IMAGE | BlockKind::CHART
            ) && let Some(content) = block.content.clone()
            {
                vlm_postprocess::clean_block(block, content);
            }
        }
        // The shared full-mode cleaner strips internal metadata, so remove these first.
        native.retain(|block| !block.metadata.contains_key(COVERED_IMAGE_CAPTION));
        if !self.config.simple_post_process {
            // The shared handler must see equation containers and their covered equations together.
            // It also drops lists; preserve them here when the MinerU-compatible option allows it.
            let mut lists = Vec::new();
            if !self.config.abandon_list {
                let mut retained = Vec::new();
                for (index, mut block) in native.into_iter().enumerate() {
                    if block.kind.as_str() == BlockKind::LIST {
                        lists.push((index, block));
                    } else {
                        block
                            .metadata
                            .insert(POST_PROCESS_ORDER.into(), index.into());
                        retained.push(block);
                    }
                }
                native = retained;
            }
            if !self.config.handle_equation_block {
                for block in &mut native {
                    if block.kind.as_str() == BlockKind::EQUATION_BLOCK {
                        block.kind = BlockKind::new("_mineru_equation_block");
                    }
                }
            }
            vlm_postprocess::post_process(&mut native);
            for block in &mut native {
                if block.kind.as_str() == "_mineru_equation_block" {
                    block.kind = BlockKind::new(BlockKind::EQUATION_BLOCK);
                }
            }
            if !lists.is_empty() {
                let mut ordered: Vec<_> = native
                    .into_iter()
                    .map(|mut block| {
                        let index = block
                            .metadata
                            .remove(POST_PROCESS_ORDER)
                            .and_then(|value| value.as_u64())
                            .expect("post-process order is retained")
                            as usize;
                        (index, block)
                    })
                    .chain(lists)
                    .collect();
                ordered.sort_by_key(|(index, _)| *index);
                native = ordered.into_iter().map(|(_, block)| block).collect();
            }
        }
        native.retain(|b| {
            !b.metadata.contains_key(COVERED_IMAGE_CAPTION)
                && !b.metadata.contains_key("_absorbed_by_table")
                && (self.config.simple_post_process
                    || !((self.config.abandon_list && b.kind.as_str() == BlockKind::LIST)
                        || (self.config.abandon_paratext && is_paratext(b.kind.as_str()))))
                && (self.config.simple_post_process || b.kind.as_str() != BlockKind::EQUATION_BLOCK)
        });
        if self.config.simple_post_process {
            for block in &mut native {
                if let Some(content) = block.content.clone() {
                    vlm_postprocess::clean_block(block, content);
                }
            }
        }
        for block in &mut native {
            if let Some(serde_json::Value::Object(external)) =
                block.metadata.remove("mineru_external_metadata")
            {
                block.metadata.extend(external);
            }
            block.metadata.remove("_table_image_token_map");
            block.metadata.remove("_absorbed_by_table");
            block.metadata.remove("_skip_asset");
            block.metadata.remove(COVERED_IMAGE_CAPTION);
            if !self.config.enable_table_formula_eq_wrap
                && block.kind.as_str() == BlockKind::TABLE
                && let Some(content) = &mut block.content
            {
                *content = content.replace("<eq>", r"\(").replace("</eq>", r"\)");
            }
        }
        Ok(native.into_iter().map(to_vlm).collect())
    }
    pub fn batch_prepare_for_layout(
        &self,
        images: Vec<DynamicImage>,
    ) -> VlmResult<Vec<VlmPreparedLayout>> {
        images
            .into_iter()
            .map(|x| self.prepare_for_layout(x))
            .collect()
    }
    pub fn batch_parse_layout_output(
        &self,
        outputs: Vec<String>,
    ) -> VlmResult<Vec<Vec<VlmLayoutBlock>>> {
        outputs
            .iter()
            .map(|x| self.parse_layout_output(x))
            .collect()
    }
    pub fn batch_prepare_for_extract(
        &self,
        images: &[DynamicImage],
        blocks: &mut [Vec<VlmLayoutBlock>],
        prompts: &[String],
        image_analysis: Option<bool>,
    ) -> VlmResult<Vec<VlmPreparedExtraction>> {
        if images.len() != blocks.len() {
            return Err(protocol("extract", "image/layout length mismatch"));
        }
        images
            .iter()
            .zip(blocks)
            .map(|(i, b)| self.prepare_for_extract(i, b, prompts, image_analysis))
            .collect()
    }
    pub fn batch_post_process(
        &self,
        blocks: Vec<Vec<VlmLayoutBlock>>,
    ) -> VlmResult<Vec<Vec<VlmLayoutBlock>>> {
        blocks.into_iter().map(|b| self.post_process(b)).collect()
    }
    pub async fn aio_prepare_for_layout(
        &self,
        image: DynamicImage,
    ) -> VlmResult<VlmPreparedLayout> {
        let preprocessor = self.clone();
        tokio::task::spawn_blocking(move || preprocessor.prepare_for_layout(image))
            .await
            .map_err(|_| VlmError::Transport {
                operation: "image",
                message: "image worker failed".into(),
            })?
    }
    pub async fn aio_parse_layout_output(&self, output: String) -> VlmResult<Vec<VlmLayoutBlock>> {
        self.parse_layout_output(&output)
    }
    pub async fn aio_prepare_for_extract(
        &self,
        image: DynamicImage,
        mut blocks: Vec<VlmLayoutBlock>,
        prompts: Vec<String>,
        image_analysis: Option<bool>,
    ) -> VlmResult<(Vec<VlmLayoutBlock>, VlmPreparedExtraction)> {
        let preprocessor = self.clone();
        tokio::task::spawn_blocking(move || {
            let prepared =
                preprocessor.prepare_for_extract(&image, &mut blocks, &prompts, image_analysis)?;
            Ok((blocks, prepared))
        })
        .await
        .map_err(|_| VlmError::Transport {
            operation: "image",
            message: "image worker failed".into(),
        })?
    }
    pub async fn aio_post_process(
        &self,
        blocks: Vec<VlmLayoutBlock>,
    ) -> VlmResult<Vec<VlmLayoutBlock>> {
        self.post_process(blocks)
    }
}

#[derive(Debug, Clone)]
pub struct MinerUVlmClient {
    http: VlmHttpClient,
    preprocessor: MinerUVlmPreprocessor,
    layout_semaphore: Arc<Semaphore>,
}
impl MinerUVlmClient {
    pub async fn parse_and_write_official_pdf(
        &self,
        input: PdfInput,
        options: OfficialPdfOptions,
        output_root: &std::path::Path,
        stem: &str,
    ) -> VlmResult<OfficialOutputManifest> {
        crate::official_route::parse_and_write(self, input, options, output_root, stem).await
    }
    pub async fn parse_and_write_official_office_pdf(
        &self,
        input: PdfInput,
        options: OfficialPdfOptions,
        output_root: &std::path::Path,
        stem: &str,
    ) -> VlmResult<OfficialOutputManifest> {
        crate::official_route::parse_and_write_office(self, input, options, output_root, stem).await
    }
    #[doc(hidden)]
    pub async fn parse_and_write_prepared_pdf(
        &self,
        prepared: crate::input_prepare::PreparedPdf,
        options: OfficialPdfOptions,
        output_root: &std::path::Path,
        stem: &str,
    ) -> VlmResult<OfficialOutputManifest> {
        crate::official_route::parse_and_write_prepared(self, prepared, options, output_root, stem)
            .await
    }
    #[doc(hidden)]
    pub async fn parse_and_write_prepared_pdf_with_events(
        &self,
        prepared: crate::input_prepare::PreparedPdf,
        options: OfficialPdfOptions,
        output_root: &std::path::Path,
        stem: &str,
        events: Option<ProgressCallback>,
    ) -> VlmResult<OfficialOutputManifest> {
        crate::official_route::parse_and_write_prepared_with_events(
            self,
            prepared,
            options,
            output_root,
            stem,
            events,
        )
        .await
    }
    /// Official-route seam: this deliberately snapshots replies before the shared
    /// cleaner mutates them.  It is crate-private so public two-step semantics stay
    /// unchanged.
    async fn official_blocking<T: Send + 'static>(
        &self,
        deadline: Instant,
        job: impl FnOnce() -> VlmResult<T> + Send + 'static,
    ) -> VlmResult<T> {
        let deadline = tokio::time::Instant::from_std(deadline);
        if tokio::time::Instant::now() >= deadline {
            return Err(VlmError::Timeout {
                operation: "official PDF",
            });
        }
        tokio::time::timeout_at(
            deadline,
            tokio::task::spawn_blocking(self.task_work_lease().wrap(job)),
        )
        .await
        .map_err(|_| VlmError::Timeout {
            operation: "official PDF",
        })?
        .map_err(|_| VlmError::Transport {
            operation: "official PDF",
            message: "worker failed".into(),
        })?
    }

    fn official_charge(
        total: &mut usize,
        bytes: usize,
        cap: usize,
        resource: &'static str,
    ) -> VlmResult<()> {
        let actual = total.checked_add(bytes).unwrap_or(usize::MAX);
        if actual > cap {
            return Err(VlmError::LimitExceeded {
                resource,
                limit: cap as u64,
                actual: actual as u64,
            });
        }
        *total = actual;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    async fn official_two_step_snapshot_page_core(
        &self,
        image: Arc<RgbImage>,
        image_analysis: bool,
        formula_enable: bool,
        table_enable: bool,
        max_layout_blocks: usize,
        max_semantic_requests: usize,
        max_requests_per_batch: usize,
        max_encoded_request_bytes: usize,
        max_encoded_batch_bytes: usize,
        raw_budget: Arc<ByteBudget>,
        encoded_budget: Arc<ByteBudget>,
        deadline: Instant,
    ) -> VlmResult<(Vec<ModelBlock>, Vec<VlmLayoutBlock>, usize, usize)> {
        let preprocessor = self.preprocessor.clone();
        let layout_image = Arc::clone(&image);
        let max_pixels = self.http.max_decoded_pixels();
        let prepared_layout = self
            .official_blocking(deadline, move || {
                preprocessor.prepare_rgb_for_layout_capped(&layout_image, max_pixels)
            })
            .await?;
        let layout_bytes = prepared_layout.image.data.len();
        if layout_bytes > max_encoded_request_bytes {
            return Err(VlmError::LimitExceeded {
                resource: "encoded request bytes",
                limit: max_encoded_request_bytes as u64,
                actual: layout_bytes as u64,
            });
        }
        if layout_bytes > max_encoded_batch_bytes {
            return Err(VlmError::LimitExceeded {
                resource: "encoded batch bytes",
                limit: max_encoded_batch_bytes as u64,
                actual: layout_bytes as u64,
            });
        }
        encoded_budget.charge(layout_bytes, "encoded document bytes")?;
        let mut encoded_bytes = layout_bytes;
        let layout_request = self.request(
            VlmImageInput::Bytes {
                data: prepared_layout.image.data,
                media_type: Some(prepared_layout.image.media_type),
            },
            self.preprocessor.prompt_for("[layout]"),
            self.preprocessor.sampling_for("[layout]"),
            None,
        );
        let _permit = self
            .layout_semaphore
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| protocol("official PDF", "semaphore closed"))?;
        let (layout_text, mut raw_bytes) = self
            .http
            .predict_official_budgeted(
                layout_request,
                raw_budget.cap(),
                Some(raw_budget.clone()),
                tokio::time::Instant::from_std(deadline),
            )
            .await?;
        drop(_permit);
        let preprocessor = self.preprocessor.clone();
        let mut blocks = self
            .official_blocking(deadline, move || {
                preprocessor.parse_layout_output_capped(&layout_text, max_layout_blocks)
            })
            .await?;
        let suppress = [
            (!formula_enable).then_some(BlockKind::EQUATION.into()),
            (!table_enable).then_some(BlockKind::TABLE.into()),
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();
        let preprocessor = self.preprocessor.clone();
        let (mut blocks, candidates) = self
            .official_blocking(deadline, move || {
                let candidates = preprocessor.semantic_candidates(
                    &mut blocks,
                    &suppress,
                    Some(image_analysis),
                    max_semantic_requests,
                )?;
                Ok((blocks, candidates))
            })
            .await?;
        let page = image;
        for candidates in candidates.chunks(max_requests_per_batch) {
            let preprocessor = self.preprocessor.clone();
            let page = page.clone();
            let candidates = candidates.to_vec();
            let current_blocks = blocks;
            let current_encoded_bytes = encoded_bytes;
            let encoded_budget = encoded_budget.clone();
            let max_pixels = self.http.max_decoded_pixels();
            let (next_blocks, prepared, next_encoded_bytes) = self
                .official_blocking(deadline, move || {
                    let mut blocks = current_blocks;
                    let mut batch_bytes = 0;
                    let mut encoded_bytes = current_encoded_bytes;
                    let mut prepared = VlmPreparedExtraction {
                        images: Vec::with_capacity(candidates.len()),
                        prompts: Vec::with_capacity(candidates.len()),
                        sampling: Vec::with_capacity(candidates.len()),
                        block_indices: Vec::with_capacity(candidates.len()),
                    };
                    for candidate in candidates {
                        let (image, prompt, sampling, block_index) = preprocessor
                            .encode_semantic_candidate_capped(
                                &page,
                                &mut blocks,
                                &candidate,
                                max_pixels,
                            )?;
                        let bytes = image.data.len();
                        if bytes > max_encoded_request_bytes {
                            return Err(VlmError::LimitExceeded {
                                resource: "encoded request bytes",
                                limit: max_encoded_request_bytes as u64,
                                actual: bytes as u64,
                            });
                        }
                        Self::official_charge(
                            &mut batch_bytes,
                            bytes,
                            max_encoded_batch_bytes,
                            "encoded batch bytes",
                        )?;
                        encoded_budget.charge(bytes, "encoded document bytes")?;
                        encoded_bytes = encoded_bytes.saturating_add(bytes);
                        prepared.images.push(image);
                        prepared.prompts.push(prompt);
                        prepared.sampling.push(sampling);
                        prepared.block_indices.push(block_index);
                    }
                    Ok((blocks, prepared, encoded_bytes))
                })
                .await?;
            blocks = next_blocks;
            encoded_bytes = next_encoded_bytes;
            let requests = prepared
                .images
                .into_iter()
                .zip(prepared.prompts)
                .zip(prepared.sampling)
                .zip(prepared.block_indices)
                .map(|(((image, prompt), sampling), index)| {
                    (
                        self.request(
                            VlmImageInput::Bytes {
                                data: image.data,
                                media_type: Some(image.media_type),
                            },
                            prompt,
                            sampling,
                            None,
                        ),
                        index,
                    )
                })
                .collect::<Vec<_>>();
            let replies = try_join_all(requests.into_iter().map(|(request, index)| {
                let http = self.http.clone();
                let semaphore = self.layout_semaphore.clone();
                let raw_budget = raw_budget.clone();
                async move {
                    let _permit = semaphore
                        .acquire_owned()
                        .await
                        .map_err(|_| protocol("official PDF", "semaphore closed"))?;
                    let (reply, bytes) = http
                        .predict_official_budgeted(
                            request,
                            raw_budget.cap(),
                            Some(raw_budget),
                            tokio::time::Instant::from_std(deadline),
                        )
                        .await?;
                    Ok::<_, VlmError>((index, reply, bytes))
                }
            }))
            .await?;
            for (index, reply, bytes) in replies {
                raw_bytes = raw_bytes.saturating_add(bytes);
                blocks[index].content = Some(reply);
            }
        }
        let preprocessor = self.preprocessor.clone();
        let (snapshot, cleaned) = self
            .official_blocking(deadline, move || {
                let snapshot = blocks
                    .clone()
                    .into_iter()
                    .map(official_snapshot_block)
                    .collect::<VlmResult<Vec<_>>>()?;
                for block in &mut blocks {
                    if let Some(content) = block.content.clone() {
                        let mut native = from_vlm(block.clone());
                        vlm_postprocess::clean_block(&mut native, content);
                        *block = to_vlm(native);
                    }
                }
                Ok((snapshot, preprocessor.post_process(blocks)?))
            })
            .await?;
        Ok((snapshot, cleaned, raw_bytes, encoded_bytes))
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn official_two_step_snapshot_window(
        &self,
        images: Vec<Arc<RgbImage>>,
        image_analysis: bool,
        formula_enable: bool,
        table_enable: bool,
        max_layout_blocks: usize,
        max_semantic_requests: usize,
        max_requests_per_batch: usize,
        max_encoded_request_bytes: usize,
        max_encoded_batch_bytes: usize,
        remaining_encoded_document_bytes: usize,
        remaining_raw_reply_bytes: usize,
        deadline: Instant,
    ) -> VlmResult<Vec<(Vec<ModelBlock>, Vec<VlmLayoutBlock>, usize, usize)>> {
        let raw = Arc::new(ByteBudget::new(remaining_raw_reply_bytes));
        let encoded = Arc::new(ByteBudget::new(remaining_encoded_document_bytes));
        try_join_all(images.into_iter().map(|image| {
            self.official_two_step_snapshot_page_core(
                image,
                image_analysis,
                formula_enable,
                table_enable,
                max_layout_blocks,
                max_semantic_requests,
                max_requests_per_batch,
                max_encoded_request_bytes,
                max_encoded_batch_bytes,
                raw.clone(),
                encoded.clone(),
                deadline,
            )
        }))
        .await
    }

    pub async fn connect(http: VlmHttpConfig, config: MinerUVlmConfig) -> VlmResult<Self> {
        Self::connect_for_task(http, config, TaskWorkLease::default()).await
    }
    pub(crate) async fn connect_for_task(
        http: VlmHttpConfig,
        config: MinerUVlmConfig,
        task_work_lease: TaskWorkLease,
    ) -> VlmResult<Self> {
        let layout_semaphore = Arc::new(Semaphore::new(http.max_concurrency.max(1)));
        Ok(Self {
            http: VlmHttpClient::connect_for_task(http, task_work_lease).await?,
            preprocessor: MinerUVlmPreprocessor { config },
            layout_semaphore,
        })
    }
    pub(crate) fn task_work_lease(&self) -> TaskWorkLease {
        self.http.task_work_lease()
    }
    fn request(
        &self,
        image: VlmImageInput,
        prompt: String,
        sampling: Option<SamplingParams>,
        priority: VlmPriority,
    ) -> VlmRequest {
        VlmRequest {
            images: vec![image],
            prompt: Some(prompt),
            sampling,
            priority,
        }
    }
    fn default_batch_semaphore(&self) -> VlmSemaphore {
        Some(self.layout_semaphore.clone())
    }
    async fn image_blocking<T: Send + 'static>(
        &self,
        job: impl FnOnce() -> VlmResult<T> + Send + 'static,
    ) -> VlmResult<T> {
        tokio::task::spawn_blocking(self.task_work_lease().wrap(job))
            .await
            .map_err(|_| VlmError::Transport {
                operation: "image",
                message: "image worker failed".into(),
            })?
    }
    async fn admit_semantic_image(&self, image: VlmImageInput) -> VlmResult<VlmImageInput> {
        if matches!(image, VlmImageInput::RemoteUrl(_)) {
            return Err(VlmError::InvalidImageInput(
                "semantic operations require a local image".into(),
            ));
        }
        Ok(self
            .http
            .admit_local_image(image)
            .await?
            .unwrap_or(VlmImageInput::None))
    }
    async fn layout_raw(
        &self,
        image: VlmImageInput,
        priority: VlmPriority,
        semaphore: VlmSemaphore,
    ) -> VlmResult<VlmExtractResult> {
        let image = self.admit_semantic_image(image).await?;
        self.layout_admitted_raw(image, priority, semaphore).await
    }
    async fn layout_admitted_raw(
        &self,
        image: VlmImageInput,
        priority: VlmPriority,
        semaphore: VlmSemaphore,
    ) -> VlmResult<VlmExtractResult> {
        let semaphore = semaphore.or_else(|| self.default_batch_semaphore());
        let image = if let Some(image) = self.http.decode_admitted_image(image).await? {
            let preprocessor = self.preprocessor.clone();
            let max_pixels = self.http.max_decoded_pixels();
            let prepared = self
                .image_blocking(move || preprocessor.prepare_for_layout_capped(image, max_pixels))
                .await?;
            VlmImageInput::Bytes {
                data: prepared.image.data,
                media_type: Some(prepared.image.media_type),
            }
        } else {
            VlmImageInput::None
        };
        let request = self.request(
            image,
            self.preprocessor.prompt_for("[layout]"),
            self.preprocessor.sampling_for("[layout]"),
            priority,
        );
        let text = self
            .http
            .aio_batch_predict(vec![request], semaphore)
            .await?
            .pop()
            .unwrap_or_default();
        Ok(VlmExtractResult {
            blocks: self.preprocessor.parse_layout_output(&text)?,
            layout_completion: Some(VlmCompletion {
                text,
                finish_reason: "stop".into(),
                request_id: None,
            }),
        })
    }
    #[allow(clippy::too_many_arguments)]
    async fn extract(
        &self,
        image: VlmImageInput,
        blocks: Vec<VlmLayoutBlock>,
        priority: VlmPriority,
        not_extract_list: &[String],
        image_analysis: Option<bool>,
        semaphore: VlmSemaphore,
    ) -> VlmResult<VlmExtractResult> {
        let image = self.admit_semantic_image(image).await?;
        self.extract_admitted(
            image,
            blocks,
            priority,
            not_extract_list,
            image_analysis,
            semaphore,
        )
        .await
    }
    #[allow(clippy::too_many_arguments)]
    async fn extract_admitted(
        &self,
        image: VlmImageInput,
        mut blocks: Vec<VlmLayoutBlock>,
        priority: VlmPriority,
        not_extract_list: &[String],
        image_analysis: Option<bool>,
        semaphore: VlmSemaphore,
    ) -> VlmResult<VlmExtractResult> {
        let semaphore = semaphore.or_else(|| self.default_batch_semaphore());
        if let Some(decoded) = self.http.decode_admitted_image(image).await? {
            let preprocessor = self.preprocessor.clone();
            let prompts = not_extract_list.to_vec();
            let max_pixels = self.http.max_decoded_pixels();
            let (next_blocks, prepared) = self
                .image_blocking(move || {
                    let prepared = preprocessor.prepare_for_extract_limited(
                        &decoded,
                        &mut blocks,
                        &prompts,
                        image_analysis,
                        usize::MAX,
                        max_pixels,
                    )?;
                    Ok((blocks, prepared))
                })
                .await?;
            blocks = next_blocks;
            let requests = prepared
                .images
                .into_iter()
                .zip(prepared.prompts)
                .zip(prepared.sampling)
                .map(|((image, prompt), sampling)| {
                    self.request(
                        VlmImageInput::Bytes {
                            data: image.data,
                            media_type: Some(image.media_type),
                        },
                        prompt,
                        sampling,
                        priority,
                    )
                })
                .collect();
            let responses = self.http.aio_batch_predict(requests, semaphore).await?;
            for (index, response) in prepared.block_indices.into_iter().zip(responses) {
                let mut native = from_vlm(blocks[index].clone());
                vlm_postprocess::clean_block(&mut native, response);
                blocks[index] = to_vlm(native);
            }
            return Ok(VlmExtractResult {
                blocks: self.preprocessor.post_process(blocks)?,
                layout_completion: None,
            });
        }
        Err(VlmError::InvalidImageInput(
            "semantic operations require an image".into(),
        ))
    }
    pub async fn layout_detect(
        &self,
        i: VlmImageInput,
        p: VlmPriority,
    ) -> VlmResult<VlmExtractResult> {
        self.layout_raw(i, p, self.default_batch_semaphore()).await
    }
    pub async fn batch_layout_detect(
        &self,
        images: Vec<VlmImageInput>,
        p: VlmBatchPriority,
    ) -> VlmResult<Vec<VlmExtractResult>> {
        let ps = priority_for(
            images.len(),
            p,
            self.preprocessor.config.incremental_priority,
        )?;
        try_join_all(images.into_iter().zip(ps).map(|(image, priority)| {
            self.layout_raw(image, priority, Some(self.layout_semaphore.clone()))
        }))
        .await
    }
    pub async fn aio_layout_detect(
        &self,
        i: VlmImageInput,
        p: VlmPriority,
        sem: VlmSemaphore,
    ) -> VlmResult<VlmExtractResult> {
        self.layout_raw(i, p, sem.or_else(|| self.default_batch_semaphore()))
            .await
    }
    pub async fn aio_batch_layout_detect(
        &self,
        i: Vec<VlmImageInput>,
        p: VlmBatchPriority,
        sem: VlmSemaphore,
    ) -> VlmResult<Vec<VlmExtractResult>> {
        let sem = sem.or_else(|| self.default_batch_semaphore());
        let ps = priority_for(i.len(), p, self.preprocessor.config.incremental_priority)?;
        try_join_all(
            i.into_iter()
                .zip(ps)
                .map(|(image, priority)| self.layout_raw(image, priority, sem.clone())),
        )
        .await
    }
    async fn content_raw(
        &self,
        i: VlmImageInput,
        prompt: String,
        p: VlmPriority,
        semaphore: VlmSemaphore,
    ) -> VlmResult<Option<String>> {
        let semaphore = semaphore.or_else(|| self.default_batch_semaphore());
        if matches!(i, VlmImageInput::RemoteUrl(_)) {
            return Err(VlmError::InvalidImageInput(
                "semantic operations require a local image".into(),
            ));
        }
        let image = self.http.decode_local_image(i).await?.ok_or_else(|| {
            VlmError::InvalidImageInput("semantic operations require an image".into())
        })?;
        let mut blocks = vec![VlmLayoutBlock {
            block_type: prompt,
            bbox: NormalizedBbox::new(0., 0., 1., 1.).unwrap(),
            angle: None,
            content: None,
            merge_prev: None,
            metadata: Map::new(),
        }];
        let preprocessor = self.preprocessor.clone();
        let max_pixels = self.http.max_decoded_pixels();
        let (next_blocks, prepared) = self
            .image_blocking(move || {
                let prepared = preprocessor.prepare_for_extract_limited(
                    &image,
                    &mut blocks,
                    &[],
                    None,
                    usize::MAX,
                    max_pixels,
                )?;
                Ok((blocks, prepared))
            })
            .await?;
        blocks = next_blocks;
        let Some((encoded, prompt, sampling, index)) = prepared
            .images
            .into_iter()
            .zip(prepared.prompts)
            .zip(prepared.sampling)
            .zip(prepared.block_indices)
            .next()
            .map(|(((a, b), c), d)| (a, b, c, d))
        else {
            return Ok(None);
        };
        let request = self.request(
            VlmImageInput::Bytes {
                data: encoded.data,
                media_type: Some(encoded.media_type),
            },
            prompt,
            sampling,
            p,
        );
        let text = self
            .http
            .aio_batch_predict(vec![request], semaphore)
            .await?
            .pop()
            .unwrap_or_default();
        let mut native = from_vlm(blocks[index].clone());
        vlm_postprocess::clean_block(&mut native, text);
        Ok(self
            .preprocessor
            .post_process(vec![to_vlm(native)])?
            .into_iter()
            .next()
            .and_then(|block| block.content)
            .filter(|x| !x.trim().is_empty()))
    }
    pub async fn content_extract(
        &self,
        i: VlmImageInput,
        prompt: String,
        p: VlmPriority,
    ) -> VlmResult<Option<String>> {
        self.content_raw(i, prompt, p, self.default_batch_semaphore())
            .await
    }
    pub async fn batch_content_extract(
        &self,
        images: Vec<VlmImageInput>,
        prompts: Vec<String>,
        p: VlmBatchPriority,
    ) -> VlmResult<Vec<Option<String>>> {
        if images.len() != prompts.len() {
            return Err(protocol("content", "image/prompt length mismatch"));
        }
        let ps = priority_for(
            images.len(),
            p,
            self.preprocessor.config.incremental_priority,
        )?;
        let semaphore = self.default_batch_semaphore();
        try_join_all(
            images
                .into_iter()
                .zip(prompts)
                .zip(ps)
                .map(|((image, prompt), priority)| {
                    self.content_raw(image, prompt, priority, semaphore.clone())
                }),
        )
        .await
    }
    pub async fn aio_content_extract(
        &self,
        i: VlmImageInput,
        q: String,
        p: VlmPriority,
        sem: VlmSemaphore,
    ) -> VlmResult<Option<String>> {
        self.content_raw(i, q, p, sem.or_else(|| self.default_batch_semaphore()))
            .await
    }
    pub async fn aio_batch_content_extract(
        &self,
        i: Vec<VlmImageInput>,
        q: Vec<String>,
        p: VlmBatchPriority,
        sem: VlmSemaphore,
    ) -> VlmResult<Vec<Option<String>>> {
        if i.len() != q.len() {
            return Err(protocol("content", "image/prompt length mismatch"));
        }
        let sem = sem.or_else(|| self.default_batch_semaphore());
        let ps = priority_for(i.len(), p, self.preprocessor.config.incremental_priority)?;
        try_join_all(
            i.into_iter()
                .zip(q)
                .zip(ps)
                .map(|((image, prompt), priority)| {
                    self.content_raw(image, prompt, priority, sem.clone())
                }),
        )
        .await
    }
    pub async fn two_step_extract(
        &self,
        i: VlmImageInput,
        p: VlmPriority,
        q: Vec<String>,
        image_analysis: Option<bool>,
    ) -> VlmResult<VlmExtractResult> {
        let i = self.admit_semantic_image(i).await?;
        let semaphore = self.default_batch_semaphore();
        let layout = self
            .layout_admitted_raw(i.clone(), p, semaphore.clone())
            .await?;
        let mut out = self
            .extract_admitted(i, layout.blocks, p, &q, image_analysis, semaphore)
            .await?;
        out.layout_completion = layout.layout_completion;
        Ok(out)
    }
    pub async fn aio_two_step_extract(
        &self,
        i: VlmImageInput,
        p: VlmPriority,
        sem: VlmSemaphore,
        q: Vec<String>,
        image_analysis: Option<bool>,
    ) -> VlmResult<VlmExtractResult> {
        let i = self.admit_semantic_image(i).await?;
        let sem = sem.or_else(|| self.default_batch_semaphore());
        let layout = self.layout_admitted_raw(i.clone(), p, sem.clone()).await?;
        let mut out = self
            .extract_admitted(i, layout.blocks, p, &q, image_analysis, sem)
            .await?;
        out.layout_completion = layout.layout_completion;
        Ok(out)
    }
    pub async fn concurrent_two_step_extract(
        &self,
        i: Vec<VlmImageInput>,
        p: VlmBatchPriority,
        q: Vec<String>,
        image_analysis: Option<bool>,
    ) -> VlmResult<Vec<VlmExtractResult>> {
        self.aio_concurrent_two_step_extract(
            i,
            p,
            q,
            self.default_batch_semaphore(),
            image_analysis,
        )
        .await
    }
    pub async fn aio_concurrent_two_step_extract(
        &self,
        i: Vec<VlmImageInput>,
        p: VlmBatchPriority,
        q: Vec<String>,
        sem: VlmSemaphore,
        image_analysis: Option<bool>,
    ) -> VlmResult<Vec<VlmExtractResult>> {
        let sem = sem.or_else(|| self.default_batch_semaphore());
        let ps = priority_for(i.len(), p, self.preprocessor.config.incremental_priority)?;
        try_join_all(i.into_iter().zip(ps).map(|(image, priority)| {
            self.aio_two_step_extract(image, priority, sem.clone(), q.clone(), image_analysis)
        }))
        .await
    }
    pub async fn stepping_two_step_extract(
        &self,
        i: Vec<VlmImageInput>,
        p: VlmBatchPriority,
        q: Vec<String>,
        image_analysis: Option<bool>,
    ) -> VlmResult<Vec<VlmExtractResult>> {
        self.aio_stepping_two_step_extract(i, p, q, self.default_batch_semaphore(), image_analysis)
            .await
    }
    pub async fn aio_stepping_two_step_extract(
        &self,
        i: Vec<VlmImageInput>,
        p: VlmBatchPriority,
        q: Vec<String>,
        sem: VlmSemaphore,
        image_analysis: Option<bool>,
    ) -> VlmResult<Vec<VlmExtractResult>> {
        let sem = sem.or_else(|| self.default_batch_semaphore());
        let priorities = priority_for(i.len(), p, self.preprocessor.config.incremental_priority)?;
        let mut admitted = Vec::with_capacity(i.len());
        for image in i {
            admitted.push(self.admit_semantic_image(image).await?);
        }
        let layouts = try_join_all(
            admitted
                .iter()
                .cloned()
                .zip(priorities.iter().copied())
                .map(|(image, priority)| self.layout_admitted_raw(image, priority, sem.clone())),
        )
        .await?;
        let completions: Vec<_> = layouts
            .iter()
            .map(|layout| layout.layout_completion.clone())
            .collect();
        let pages = layouts.into_iter().map(|layout| layout.blocks).collect();
        let mut extracted = self
            .batch_extract_flat_inner(admitted, pages, priorities, &q, image_analysis, sem, true)
            .await?;
        for (result, completion) in extracted.iter_mut().zip(completions) {
            result.layout_completion = completion;
        }
        Ok(extracted)
    }
    pub async fn batch_two_step_extract(
        &self,
        images: Vec<VlmImageInput>,
        p: VlmBatchPriority,
        q: Vec<String>,
        image_analysis: Option<bool>,
    ) -> VlmResult<Vec<VlmExtractResult>> {
        self.concurrent_two_step_extract(images, p, q, image_analysis)
            .await
    }
    pub async fn aio_batch_two_step_extract(
        &self,
        i: Vec<VlmImageInput>,
        p: VlmBatchPriority,
        q: Vec<String>,
        sem: VlmSemaphore,
        image_analysis: Option<bool>,
    ) -> VlmResult<Vec<VlmExtractResult>> {
        let sem = sem.or_else(|| self.default_batch_semaphore());
        self.aio_concurrent_two_step_extract(i, p, q, sem, image_analysis)
            .await
    }
    pub async fn extract_with_layout(
        &self,
        i: VlmImageInput,
        blocks: Vec<VlmLayoutBlock>,
        p: VlmPriority,
        q: Vec<String>,
        image_analysis: Option<bool>,
    ) -> VlmResult<VlmExtractResult> {
        self.extract(
            i,
            blocks,
            p,
            &q,
            image_analysis,
            self.default_batch_semaphore(),
        )
        .await
    }
    #[allow(clippy::too_many_arguments)]
    async fn batch_extract_flat(
        &self,
        images: Vec<VlmImageInput>,
        pages: Vec<Vec<VlmLayoutBlock>>,
        priorities: Vec<VlmPriority>,
        prompts: &[String],
        image_analysis: Option<bool>,
        semaphore: VlmSemaphore,
    ) -> VlmResult<Vec<VlmExtractResult>> {
        self.batch_extract_flat_inner(
            images,
            pages,
            priorities,
            prompts,
            image_analysis,
            semaphore,
            false,
        )
        .await
    }
    #[allow(clippy::too_many_arguments)]
    async fn batch_extract_flat_inner(
        &self,
        images: Vec<VlmImageInput>,
        mut pages: Vec<Vec<VlmLayoutBlock>>,
        priorities: Vec<VlmPriority>,
        prompts: &[String],
        image_analysis: Option<bool>,
        semaphore: VlmSemaphore,
        admitted: bool,
    ) -> VlmResult<Vec<VlmExtractResult>> {
        let mut requests = Vec::new();
        let mut locations = Vec::new();
        for (page_index, ((image, blocks), priority)) in images
            .into_iter()
            .zip(pages.iter_mut())
            .zip(priorities)
            .enumerate()
        {
            let image = if admitted {
                image
            } else {
                self.admit_semantic_image(image).await?
            };
            let image = self
                .http
                .decode_admitted_image(image)
                .await?
                .ok_or_else(|| {
                    VlmError::InvalidImageInput("semantic operations require an image".into())
                })?;
            let preprocessor = self.preprocessor.clone();
            let page_blocks = std::mem::take(blocks);
            let prompts = prompts.to_vec();
            let max_pixels = self.http.max_decoded_pixels();
            let (next_blocks, prepared) = self
                .image_blocking(move || {
                    let mut blocks = page_blocks;
                    let prepared = preprocessor.prepare_for_extract_limited(
                        &image,
                        &mut blocks,
                        &prompts,
                        image_analysis,
                        usize::MAX,
                        max_pixels,
                    )?;
                    Ok((blocks, prepared))
                })
                .await?;
            *blocks = next_blocks;
            for (((image, prompt), sampling), block_index) in prepared
                .images
                .into_iter()
                .zip(prepared.prompts)
                .zip(prepared.sampling)
                .zip(prepared.block_indices)
            {
                requests.push(self.request(
                    VlmImageInput::Bytes {
                        data: image.data,
                        media_type: Some(image.media_type),
                    },
                    prompt,
                    sampling,
                    priority,
                ));
                locations.push((page_index, block_index));
            }
        }
        let responses = self.http.aio_batch_predict(requests, semaphore).await?;
        if responses.len() != locations.len() {
            return Err(protocol("extract", "response/request length mismatch"));
        }
        for ((page_index, block_index), response) in locations.into_iter().zip(responses) {
            let mut block = from_vlm(pages[page_index][block_index].clone());
            vlm_postprocess::clean_block(&mut block, response);
            pages[page_index][block_index] = to_vlm(block);
        }
        pages
            .into_iter()
            .map(|blocks| {
                Ok(VlmExtractResult {
                    blocks: self.preprocessor.post_process(blocks)?,
                    layout_completion: None,
                })
            })
            .collect()
    }
    pub async fn batch_extract_with_layout(
        &self,
        images: Vec<VlmImageInput>,
        blocks: Vec<Vec<VlmLayoutBlock>>,
        p: VlmBatchPriority,
        q: Vec<String>,
        image_analysis: Option<bool>,
    ) -> VlmResult<Vec<VlmExtractResult>> {
        if images.len() != blocks.len() {
            return Err(protocol("extract", "image/layout length mismatch"));
        }
        let ps = priority_for(
            images.len(),
            p,
            self.preprocessor.config.incremental_priority,
        )?;
        self.batch_extract_flat(
            images,
            blocks,
            ps,
            &q,
            image_analysis,
            self.default_batch_semaphore(),
        )
        .await
    }
    #[allow(clippy::too_many_arguments)]
    pub async fn aio_extract_with_layout(
        &self,
        i: VlmImageInput,
        b: Vec<VlmLayoutBlock>,
        p: VlmPriority,
        sem: VlmSemaphore,
        q: Vec<String>,
        image_analysis: Option<bool>,
    ) -> VlmResult<VlmExtractResult> {
        self.extract(
            i,
            b,
            p,
            &q,
            image_analysis,
            sem.or_else(|| self.default_batch_semaphore()),
        )
        .await
    }
    #[allow(clippy::too_many_arguments)]
    pub async fn aio_batch_extract_with_layout(
        &self,
        i: Vec<VlmImageInput>,
        b: Vec<Vec<VlmLayoutBlock>>,
        p: VlmBatchPriority,
        sem: VlmSemaphore,
        q: Vec<String>,
        image_analysis: Option<bool>,
    ) -> VlmResult<Vec<VlmExtractResult>> {
        if i.len() != b.len() {
            return Err(protocol("extract", "image/layout length mismatch"));
        }
        let sem = sem.or_else(|| self.default_batch_semaphore());
        let ps = priority_for(i.len(), p, self.preprocessor.config.incremental_priority)?;
        self.batch_extract_flat(i, b, ps, &q, image_analysis, sem)
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        Json, Router, extract::State, http::StatusCode, response::IntoResponse, routing::post,
    };
    use serde_json::{Map, json};
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    use tokio::{
        net::TcpListener,
        sync::{Barrier, Mutex, Notify},
        time::{Duration, sleep, timeout},
    };

    #[derive(Clone)]
    struct MockState {
        phases: Arc<Mutex<Vec<String>>>,
        active: Arc<AtomicUsize>,
        peak: Arc<AtomicUsize>,
        layout_active: Arc<AtomicUsize>,
        layout_peak: Arc<AtomicUsize>,
        requests: Arc<AtomicUsize>,
    }

    async fn mock_client(state: MockState) -> MinerUVlmClient {
        mock_client_with(state, VlmHttpConfig::default(), MinerUVlmConfig::default()).await
    }

    async fn mock_client_with(
        state: MockState,
        mut http: VlmHttpConfig,
        config: MinerUVlmConfig,
    ) -> MinerUVlmClient {
        let app = Router::new()
            .route("/v1/chat/completions", post(mock_chat))
            .with_state(state);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        http.server_url = Some(format!("http://{address}").parse().unwrap());
        http.model_name = Some("mock".into());
        http.skip_model_name_checking = true;
        http.max_retries = 0;
        http.max_concurrency = 2;
        MinerUVlmClient::connect(http, config).await.unwrap()
    }

    async fn mock_chat(
        State(state): State<MockState>,
        Json(request): Json<serde_json::Value>,
    ) -> Json<serde_json::Value> {
        state.requests.fetch_add(1, Ordering::SeqCst);
        let active = state.active.fetch_add(1, Ordering::SeqCst) + 1;
        state.peak.fetch_max(active, Ordering::SeqCst);
        let layout = request.to_string().contains("Layout Detection");
        if layout {
            let active = state.layout_active.fetch_add(1, Ordering::SeqCst) + 1;
            state.layout_peak.fetch_max(active, Ordering::SeqCst);
        }
        state
            .phases
            .lock()
            .await
            .push(if layout { "layout" } else { "extract" }.into());
        sleep(Duration::from_millis(10)).await;
        if layout {
            state.layout_active.fetch_sub(1, Ordering::SeqCst);
        }
        state.active.fetch_sub(1, Ordering::SeqCst);
        let content = if layout {
            if let Some(priority) = request.get("priority").and_then(serde_json::Value::as_i64) {
                format!(
                    "<|box_start|>{priority} 0 {} 1<|box_end|><|ref_start|>text<|ref_end|>",
                    priority + 1
                )
            } else {
                include_str!("../tests/fixtures/vlm/layout.txt").into()
            }
        } else if let Some(priority) = request.get("priority").and_then(serde_json::Value::as_i64) {
            format!("recognized-{priority}")
        } else {
            "recognized".into()
        };
        Json(json!({"choices":[{"finish_reason":"stop","message":{"content":content}}]}))
    }

    #[derive(Clone)]
    struct DeletePaths(Arc<Vec<std::path::PathBuf>>);

    async fn delete_paths_chat(
        State(state): State<DeletePaths>,
        Json(request): Json<serde_json::Value>,
    ) -> Json<serde_json::Value> {
        let layout = request.to_string().contains("Layout Detection");
        if layout {
            for path in state.0.iter() {
                let _ = std::fs::remove_file(path);
            }
        }
        let content = if layout {
            "<|box_start|>0 0 1000 1000<|box_end|><|ref_start|>text<|ref_end|>"
        } else {
            "recognized"
        };
        Json(json!({"choices":[{"finish_reason":"stop","message":{"content":content}}]}))
    }

    async fn delete_paths_client(paths: Vec<std::path::PathBuf>) -> MinerUVlmClient {
        let app = Router::new()
            .route("/v1/chat/completions", post(delete_paths_chat))
            .with_state(DeletePaths(Arc::new(paths)));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
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

    #[derive(Clone)]
    struct WindowState {
        first_two: Arc<Barrier>,
        release: Arc<Notify>,
        active: Arc<AtomicUsize>,
        peak: Arc<AtomicUsize>,
        layouts: Arc<AtomicUsize>,
        ignored: usize,
    }

    async fn window_client(state: WindowState) -> MinerUVlmClient {
        let app = Router::new()
            .route("/v1/chat/completions", post(window_chat))
            .with_state(state);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        MinerUVlmClient::connect(
            VlmHttpConfig {
                server_url: Some(format!("http://{address}").parse().unwrap()),
                model_name: Some("mock".into()),
                skip_model_name_checking: true,
                max_retries: 0,
                max_concurrency: 2,
                ..Default::default()
            },
            MinerUVlmConfig::default(),
        )
        .await
        .unwrap()
    }

    async fn window_chat(
        State(state): State<WindowState>,
        Json(request): Json<serde_json::Value>,
    ) -> Json<serde_json::Value> {
        let active = state.active.fetch_add(1, Ordering::SeqCst) + 1;
        state.peak.fetch_max(active, Ordering::SeqCst);
        let layout = request.to_string().contains("Layout Detection");
        let content = if layout {
            let admission = state.layouts.fetch_add(1, Ordering::SeqCst);
            if admission < 2 {
                let released = state.release.notified();
                tokio::pin!(released);
                let _ = released.as_mut().enable();
                state.first_two.wait().await;
                released.await;
            }
            let (left, right) = if let Some(priority) =
                request.get("priority").and_then(serde_json::Value::as_i64)
            {
                (priority, priority + 1)
            } else {
                let data_url = request["messages"][1]["content"][0]["image_url"]["url"]
                    .as_str()
                    .unwrap();
                let bytes = STANDARD
                    .decode(data_url.rsplit(',').next().unwrap())
                    .unwrap();
                let page = image::load_from_memory(&bytes)
                    .unwrap()
                    .to_rgb8()
                    .get_pixel(0, 0)[0] as i64;
                ((page + 1) * 100, (page + 1) * 100 + 50)
            };
            format!(
                "<|box_start|>{left} 0 {right} 1000<|box_end|><|ref_start|>text<|ref_end|><|rotate_up|>"
            )
        } else {
            "  raw semantic  ".into()
        };
        state.active.fetch_sub(1, Ordering::SeqCst);
        Json(
            json!({"ignored":"x".repeat(state.ignored),"choices":[{"finish_reason":"stop","message":{"content":content}}]}),
        )
    }

    fn window_state() -> WindowState {
        WindowState {
            // Two handlers plus the test make admission deterministic.
            first_two: Arc::new(Barrier::new(3)),
            release: Arc::new(Notify::new()),
            active: Arc::new(AtomicUsize::new(0)),
            peak: Arc::new(AtomicUsize::new(0)),
            layouts: Arc::new(AtomicUsize::new(0)),
            ignored: 0,
        }
    }

    #[derive(Clone)]
    struct FailWindowState {
        entered: Arc<Barrier>,
        pending: Arc<Notify>,
        failures: Arc<AtomicUsize>,
    }

    async fn fail_window_client(state: FailWindowState) -> MinerUVlmClient {
        let app = Router::new()
            .route("/v1/chat/completions", post(fail_window_chat))
            .with_state(state);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        MinerUVlmClient::connect(
            VlmHttpConfig {
                server_url: Some(format!("http://{address}").parse().unwrap()),
                model_name: Some("mock".into()),
                skip_model_name_checking: true,
                max_retries: 0,
                max_concurrency: 2,
                ..Default::default()
            },
            MinerUVlmConfig::default(),
        )
        .await
        .unwrap()
    }

    async fn fail_window_chat(
        State(state): State<FailWindowState>,
        Json(_request): Json<serde_json::Value>,
    ) -> axum::response::Response {
        state.entered.wait().await;
        if state.failures.fetch_add(1, Ordering::SeqCst) == 0 {
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
        state.pending.notified().await;
        Json(json!({"choices":[]})).into_response()
    }

    fn mock_state() -> MockState {
        MockState {
            phases: Arc::new(Mutex::new(vec![])),
            active: Arc::new(AtomicUsize::new(0)),
            peak: Arc::new(AtomicUsize::new(0)),
            layout_active: Arc::new(AtomicUsize::new(0)),
            layout_peak: Arc::new(AtomicUsize::new(0)),
            requests: Arc::new(AtomicUsize::new(0)),
        }
    }

    fn image_input() -> VlmImageInput {
        sized_image_input(32, 32)
    }

    fn sized_image_input(width: u32, height: u32) -> VlmImageInput {
        let mut bytes = vec![];
        DynamicImage::new_rgb8(width, height)
            .write_to(&mut Cursor::new(&mut bytes), ImageFormat::Png)
            .unwrap();
        VlmImageInput::Bytes {
            data: bytes.into(),
            media_type: Some("image/png".into()),
        }
    }

    #[tokio::test]
    async fn semantic_remote_url_is_rejected_before_request() {
        let state = mock_state();
        let client = mock_client(state.clone()).await;
        let result = client
            .layout_detect(
                VlmImageInput::RemoteUrl("https://example.com/image.png".parse().unwrap()),
                None,
            )
            .await;
        assert!(matches!(result, Err(VlmError::InvalidImageInput(_))));
        assert_eq!(state.requests.load(Ordering::SeqCst), 0);
    }

    fn block(kind: &str) -> VlmLayoutBlock {
        VlmLayoutBlock {
            block_type: kind.into(),
            bbox: NormalizedBbox::new(0., 0., 1., 1.).unwrap(),
            angle: None,
            content: None,
            merge_prev: Some(true),
            metadata: Map::from_iter([(String::from("custom"), json!("kept"))]),
        }
    }

    #[test]
    fn layout_type_round_trip_preserves_metadata_and_merge_state() {
        let original = block(BlockKind::TEXT);
        let round_trip = to_vlm(from_vlm(original.clone()));
        assert_eq!(round_trip.block_type, original.block_type);
        assert_eq!(round_trip.merge_prev, original.merge_prev);
        assert_eq!(round_trip.metadata, original.metadata);
    }

    #[test]
    fn official_snapshot_preserves_raw_reply_and_caller_metadata() {
        let mut raw = block(BlockKind::TEXT);
        raw.angle = Some(Rotation::Deg0);
        raw.content = Some("  raw text  ".into());
        raw.merge_prev = None;
        raw.metadata.extend(Map::from_iter([
            ("sub_type".into(), json!("paragraph")),
            ("_caller_key".into(), json!("kept")),
            ("type".into(), json!("reserved")),
            ("_skip_asset".into(), json!(true)),
            ("_table_image_token_map".into(), json!({})),
            ("_absorbed_by_table".into(), json!(0)),
            (COVERED_IMAGE_CAPTION.into(), json!(true)),
        ]));
        let snapshot = official_snapshot_block(raw.clone()).unwrap();
        assert_eq!(snapshot.content.as_deref(), Some("  raw text  "));
        assert_eq!(snapshot.merge_prev, Some(false));
        assert_eq!(snapshot.sub_type.as_deref(), Some("paragraph"));
        assert_eq!(snapshot.extra["_caller_key"], "kept");
        assert!(!snapshot.extra.contains_key("type"));
        assert!(!snapshot.extra.contains_key("_skip_asset"));
        assert!(!snapshot.extra.contains_key("_table_image_token_map"));
        assert!(!snapshot.extra.contains_key("_absorbed_by_table"));
        assert!(!snapshot.extra.contains_key(COVERED_IMAGE_CAPTION));

        let mut cleaned = from_vlm(raw);
        vlm_postprocess::clean_block(&mut cleaned, "  raw text  ".into());
        assert_ne!(snapshot.content, cleaned.content);

        let mut nontext = block(BlockKind::TABLE);
        nontext.angle = Some(Rotation::Deg0);
        assert_eq!(official_snapshot_block(nontext).unwrap().merge_prev, None);
    }

    #[test]
    fn official_snapshot_requires_angle() {
        assert!(matches!(
            official_snapshot_block(block(BlockKind::TEXT)),
            Err(VlmError::Protocol { .. })
        ));
    }

    #[test]
    fn prepares_layout_and_parses_fixture() {
        let pre = MinerUVlmPreprocessor {
            config: MinerUVlmConfig::default(),
        };
        let prepared = pre
            .prepare_for_layout(DynamicImage::new_rgb8(10, 20))
            .unwrap();
        assert_eq!(prepared.image.media_type, "image/png");
        assert!(!prepared.image.data.is_empty());
        let parsed = pre
            .parse_layout_output(include_str!("../tests/fixtures/vlm/layout.txt"))
            .unwrap();
        assert_eq!(parsed.len(), 4);
        assert_eq!(parsed[1].block_type, BlockKind::TABLE);
    }

    #[test]
    fn capped_preprocessing_rejects_derived_allocations() {
        let pre = MinerUVlmPreprocessor {
            config: MinerUVlmConfig::default(),
        };
        let resized = pre.resize_by_need(DynamicImage::new_rgb8(13, 13)).unwrap();
        assert_eq!((resized.width(), resized.height()), (29, 29));
        assert!(matches!(
            pre.resize_by_need_capped(DynamicImage::new_rgb8(13, 13), 800),
            Err(VlmError::LimitExceeded {
                resource: "image pixels",
                limit: 800,
                actual: 841,
            })
        ));
        let mut blocks = vec![block(BlockKind::TABLE)];
        let candidate = SemanticCandidate {
            original_index: 0,
            block: from_vlm(blocks[0].clone()),
            absorbed: vec![],
        };
        assert!(matches!(
            pre.encode_semantic_candidate_capped(
                &RgbImage::new(13, 13),
                &mut blocks,
                &candidate,
                800,
            ),
            Err(VlmError::LimitExceeded {
                resource: "image pixels",
                limit: 800,
                actual: 841,
            })
        ));
        assert!(matches!(
            pre.resize_by_need_capped(DynamicImage::new_rgb8(10_000, 1), 10_000),
            Err(VlmError::LimitExceeded {
                resource: "image pixels",
                limit: 10_000,
                actual: 2_000_000,
            })
        ));

        let pre = MinerUVlmPreprocessor {
            config: MinerUVlmConfig {
                layout_image_size: (u32::MAX, u32::MAX),
                ..Default::default()
            },
        };
        assert!(matches!(
            pre.prepare_for_layout_capped(DynamicImage::new_rgb8(1, 1), 100),
            Err(VlmError::LimitExceeded {
                resource: "image pixels",
                limit: 100,
                ..
            })
        ));
    }

    #[tokio::test]
    async fn semantic_layout_route_enforces_preprocessing_pixel_cap_before_request() {
        let state = mock_state();
        let client = mock_client_with(
            state.clone(),
            VlmHttpConfig {
                max_decoded_pixels: 100,
                ..Default::default()
            },
            MinerUVlmConfig {
                layout_image_size: (11, 10),
                ..Default::default()
            },
        )
        .await;
        assert!(matches!(
            client.layout_detect(sized_image_input(1, 1), None).await,
            Err(VlmError::LimitExceeded {
                resource: "image pixels",
                limit: 100,
                actual: 110,
            })
        ));
        assert_eq!(state.requests.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn layout_parse_rejects_nonempty_garbage_but_keeps_valid_blocks() {
        let pre = MinerUVlmPreprocessor {
            config: MinerUVlmConfig::default(),
        };
        assert!(matches!(
            pre.parse_layout_output("garbage"),
            Err(VlmError::Protocol { .. })
        ));
        let parsed = pre
            .parse_layout_output("<|box_start|>0 0 0 1<|box_end|><|ref_start|>text<|ref_end|><|box_start|>1 1 2 2<|box_end|><|ref_start|>title<|ref_end|>")
            .unwrap();
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].block_type, BlockKind::TITLE);
    }

    #[test]
    fn prepare_extract_filters_images_and_keeps_table() {
        let pre = MinerUVlmPreprocessor {
            config: MinerUVlmConfig::default(),
        };
        let mut blocks = vec![
            block(BlockKind::TABLE),
            block(BlockKind::IMAGE),
            block(BlockKind::LIST),
        ];
        let prepared = pre
            .prepare_for_extract(&DynamicImage::new_rgb8(32, 32), &mut blocks, &[], None)
            .unwrap();
        assert_eq!(prepared.block_indices, vec![0]);
        assert_eq!(prepared.prompts, vec!["\nTable Recognition:"]);
    }

    #[test]
    fn partial_prompt_and_sampling_maps_resolve_independently() {
        let exact = SamplingParams {
            temperature: Some(0.7),
            ..Default::default()
        };
        let fallback = SamplingParams {
            temperature: Some(0.2),
            ..Default::default()
        };
        let pre = MinerUVlmPreprocessor {
            config: MinerUVlmConfig {
                prompts: std::collections::BTreeMap::from([
                    (BlockKind::TEXT.into(), "text prompt".into()),
                    ("[default]".into(), "fallback prompt".into()),
                ]),
                sampling_params: std::collections::BTreeMap::from([
                    (BlockKind::EQUATION.into(), exact.clone()),
                    ("[default]".into(), fallback.clone()),
                ]),
                ..Default::default()
            },
        };
        assert_eq!(pre.prompt_for(BlockKind::EQUATION), "fallback prompt");
        assert_eq!(pre.sampling_for(BlockKind::EQUATION), Some(exact));
        assert_eq!(pre.prompt_for(BlockKind::TEXT), "text prompt");
        assert_eq!(pre.sampling_for(BlockKind::TEXT), Some(fallback.clone()));
        assert_eq!(pre.prompt_for("missing"), "fallback prompt");
        assert_eq!(pre.sampling_for("missing"), Some(fallback.clone()));
        assert_eq!(pre.prompt_for("[layout]"), "fallback prompt");
        assert_eq!(pre.sampling_for("[layout]"), Some(fallback));
    }

    #[test]
    fn empty_exact_prompt_falls_back_to_default() {
        let pre = MinerUVlmPreprocessor {
            config: MinerUVlmConfig {
                prompts: std::collections::BTreeMap::from([
                    (BlockKind::TEXT.into(), " \n ".into()),
                    ("[default]".into(), "fallback prompt".into()),
                ]),
                ..Default::default()
            },
        };
        assert_eq!(pre.prompt_for(BlockKind::TEXT), "fallback prompt");
    }

    #[test]
    fn semantic_crop_rotates_90_counter_clockwise_and_270_clockwise() {
        let mut image = RgbImage::new(2, 3);
        image.put_pixel(0, 0, image::Rgb([1, 0, 0]));
        image.put_pixel(1, 0, image::Rgb([2, 0, 0]));
        image.put_pixel(0, 2, image::Rgb([3, 0, 0]));
        image.put_pixel(1, 2, image::Rgb([4, 0, 0]));
        let bbox = NormalizedBbox::new(0., 0., 1., 1.).unwrap();
        let ccw = semantic_crop(&image, bbox, Some(Rotation::Deg90), u64::MAX).unwrap();
        let cw = semantic_crop(&image, bbox, Some(Rotation::Deg270), u64::MAX).unwrap();
        assert_eq!((ccw.width(), ccw.height()), (3, 2));
        assert_eq!(ccw.get_pixel(0, 1), &image::Rgb([1, 0, 0]));
        assert_eq!(ccw.get_pixel(2, 1), &image::Rgb([3, 0, 0]));
        assert_eq!(ccw.get_pixel(0, 0), &image::Rgb([2, 0, 0]));
        assert_eq!(ccw.get_pixel(2, 0), &image::Rgb([4, 0, 0]));
        assert_eq!(cw.get_pixel(2, 0), &image::Rgb([1, 0, 0]));
        assert_eq!(cw.get_pixel(2, 1), &image::Rgb([2, 0, 0]));
        assert_eq!(cw.get_pixel(0, 0), &image::Rgb([3, 0, 0]));
        assert_eq!(cw.get_pixel(0, 1), &image::Rgb([4, 0, 0]));
    }

    #[test]
    fn expands_incremental_priorities() {
        assert_eq!(
            priority_for(3, VlmBatchPriority::All(None), true).unwrap(),
            vec![Some(0), Some(1), Some(2)]
        );
        assert!(priority_for(2, VlmBatchPriority::PerItem(vec![None]), false).is_err());
    }

    #[test]
    fn post_process_keeps_caller_metadata_and_drops_containers() {
        let config = MinerUVlmConfig {
            abandon_list: true,
            abandon_paratext: false,
            handle_equation_block: false,
            ..Default::default()
        };
        let pre = MinerUVlmPreprocessor { config };
        let mut text = block(BlockKind::TEXT);
        text.metadata.insert("_caller_key".into(), json!("kept"));
        let paratext = block(BlockKind::HEADER);
        let equation_block = block(BlockKind::EQUATION_BLOCK);
        let list = block(BlockKind::LIST);
        let output = pre
            .post_process(vec![text, paratext, equation_block, list])
            .unwrap();
        assert_eq!(output.len(), 2);
        assert_eq!(output[0].metadata["_caller_key"], "kept");
        assert_eq!(output[1].block_type, BlockKind::HEADER);
    }

    #[test]
    fn full_post_process_without_equation_handler_still_normalizes() {
        let pre = MinerUVlmPreprocessor {
            config: MinerUVlmConfig {
                handle_equation_block: false,
                ..Default::default()
            },
        };
        let mut text = block(BlockKind::TEXT);
        text.content = Some("  text  ".into());
        let mut item = block(BlockKind::LIST_ITEM);
        item.content = Some("- item".into());
        let mut equation = block(BlockKind::EQUATION);
        equation.content = Some(r"\[x\]".into());
        let output = pre
            .post_process(vec![text, item, equation, block(BlockKind::EQUATION_BLOCK)])
            .unwrap();
        assert_eq!(
            output
                .iter()
                .map(|block| block.block_type.as_str())
                .collect::<Vec<_>>(),
            vec![BlockKind::TEXT, BlockKind::TEXT, BlockKind::EQUATION]
        );
        assert_eq!(output[0].content.as_deref(), Some("text"));
        assert_eq!(output[1].content.as_deref(), Some("item"));
        assert_eq!(output[2].content.as_deref(), Some("x"));
    }

    #[test]
    fn full_post_process_preserves_or_drops_lists_by_config() {
        let blocks = || {
            let mut list = block(BlockKind::LIST);
            list.metadata.insert("list_metadata".into(), json!("kept"));
            let mut item = block(BlockKind::LIST_ITEM);
            item.content = Some("- item".into());
            vec![block(BlockKind::TEXT), list, item]
        };
        let keep = MinerUVlmPreprocessor {
            config: MinerUVlmConfig {
                abandon_list: false,
                ..Default::default()
            },
        }
        .post_process(blocks())
        .unwrap();
        assert_eq!(
            keep.iter()
                .map(|block| block.block_type.as_str())
                .collect::<Vec<_>>(),
            vec![BlockKind::TEXT, BlockKind::LIST, BlockKind::TEXT]
        );
        assert_eq!(keep[1].metadata["list_metadata"], "kept");

        let drop = MinerUVlmPreprocessor {
            config: MinerUVlmConfig {
                abandon_list: true,
                ..Default::default()
            },
        }
        .post_process(blocks())
        .unwrap();
        assert_eq!(
            drop.iter()
                .map(|block| block.block_type.as_str())
                .collect::<Vec<_>>(),
            vec![BlockKind::TEXT, BlockKind::TEXT]
        );
    }

    #[test]
    fn prepare_extract_suppresses_known_titles_and_removes_covered_captions() {
        let pre = MinerUVlmPreprocessor {
            config: MinerUVlmConfig::default(),
        };
        let mut blocks = vec![
            block(BlockKind::TITLE),
            block(BlockKind::IMAGE),
            block(BlockKind::IMAGE_CAPTION),
        ];
        let prepared = pre
            .prepare_for_extract(
                &DynamicImage::new_rgb8(32, 32),
                &mut blocks,
                &[BlockKind::TITLE.into()],
                Some(true),
            )
            .unwrap();
        assert_eq!(prepared.block_indices, vec![1]);
        assert!(!prepared.block_indices.contains(&2));
        assert_eq!(blocks[2].metadata[COVERED_IMAGE_CAPTION], true);
        let output = pre.post_process(blocks).unwrap();
        assert!(
            !output
                .iter()
                .any(|block| block.block_type == BlockKind::IMAGE_CAPTION)
        );
    }

    #[test]
    fn simple_post_process_retains_equation_containers_lists_and_paratext() {
        let pre = MinerUVlmPreprocessor {
            config: MinerUVlmConfig {
                simple_post_process: true,
                abandon_paratext: true,
                abandon_list: true,
                ..Default::default()
            },
        };
        let output = pre
            .post_process(vec![
                block(BlockKind::EQUATION_BLOCK),
                block(BlockKind::HEADER),
                block(BlockKind::LIST),
            ])
            .unwrap();
        assert_eq!(
            output
                .iter()
                .map(|block| block.block_type.as_str())
                .collect::<Vec<_>>(),
            vec![
                BlockKind::EQUATION_BLOCK,
                BlockKind::HEADER,
                BlockKind::LIST
            ]
        );
    }

    #[test]
    fn simple_post_process_converts_list_items_to_text() {
        let pre = MinerUVlmPreprocessor {
            config: MinerUVlmConfig {
                simple_post_process: true,
                ..Default::default()
            },
        };
        let mut item = block(BlockKind::LIST_ITEM);
        item.content = Some("- item".into());
        let output = pre.post_process(vec![item]).unwrap();
        assert_eq!(output[0].block_type, BlockKind::TEXT);
        assert_eq!(output[0].content.as_deref(), Some("item"));
    }

    #[test]
    fn equation_handler_sees_container_before_full_mode_removes_it() {
        let pre = MinerUVlmPreprocessor {
            config: MinerUVlmConfig {
                handle_equation_block: true,
                ..Default::default()
            },
        };
        let mut first = block(BlockKind::EQUATION);
        first.content = Some(r"\(x\)".into());
        let mut second = block(BlockKind::EQUATION);
        second.content = Some(r"\(y\)".into());
        let output = pre
            .post_process(vec![block(BlockKind::EQUATION_BLOCK), first, second])
            .unwrap();
        assert_eq!(output.len(), 1);
        assert_eq!(output[0].block_type, BlockKind::EQUATION);
        assert_eq!(
            output[0].content.as_deref(),
            Some(r"\begin{array}{l} x \\ y \end{array}")
        );
    }

    #[tokio::test]
    async fn axum_stepping_finishes_layout_phase_before_flattened_extraction() {
        let state = mock_state();
        let client = mock_client(state.clone()).await;
        let output = client
            .stepping_two_step_extract(
                vec![image_input(), image_input()],
                VlmBatchPriority::All(None),
                vec![],
                None,
            )
            .await
            .unwrap();
        let phases = state.phases.lock().await.clone();
        assert_eq!(&phases[..2], ["layout", "layout"]);
        assert!(phases[2..].iter().all(|phase| phase == "extract"));
        assert_eq!(output.len(), 2);
        assert!(output.iter().all(|page| page.layout_completion.is_some()));
    }

    #[tokio::test]
    async fn two_step_and_stepping_batch_admit_paths_before_reuse() {
        fn write_png(path: &std::path::Path) {
            let VlmImageInput::Bytes { data, .. } = sized_image_input(2, 2) else {
                unreachable!()
            };
            std::fs::write(path, data).unwrap();
        }

        let single_dir = tempfile::tempdir().unwrap();
        let single = single_dir.path().join("single.png");
        write_png(&single);
        let client = delete_paths_client(vec![single.clone()]).await;
        let output = client
            .two_step_extract(VlmImageInput::Path(single.clone()), None, vec![], None)
            .await
            .unwrap();
        assert!(!single.exists());
        assert_eq!(output.blocks[0].content.as_deref(), Some("recognized"));

        let batch_dir = tempfile::tempdir().unwrap();
        let paths = [
            batch_dir.path().join("first.png"),
            batch_dir.path().join("second.png"),
        ];
        for path in &paths {
            write_png(path);
        }
        let client = delete_paths_client(paths.to_vec()).await;
        let output = client
            .stepping_two_step_extract(
                paths.iter().cloned().map(VlmImageInput::Path).collect(),
                VlmBatchPriority::All(None),
                vec![],
                None,
            )
            .await
            .unwrap();
        assert!(paths.iter().all(|path| !path.exists()));
        assert_eq!(output.len(), 2);
        assert!(
            output
                .iter()
                .all(|page| page.blocks[0].content.as_deref() == Some("recognized"))
        );
    }

    #[tokio::test]
    async fn concurrent_two_step_uses_configured_semaphore_and_keeps_order() {
        let state = window_state();
        let client = window_client(state.clone()).await;
        let task = tokio::spawn(async move {
            client
                .concurrent_two_step_extract(
                    vec![image_input(), image_input(), image_input()],
                    VlmBatchPriority::PerItem(vec![Some(30), Some(10), Some(20)]),
                    vec![],
                    None,
                )
                .await
        });
        timeout(Duration::from_secs(5), state.first_two.wait())
            .await
            .unwrap();
        assert_eq!(state.peak.load(Ordering::SeqCst), 2);
        assert_eq!(state.layouts.load(Ordering::SeqCst), 2);
        state.release.notify_waiters();
        let output = timeout(Duration::from_secs(2), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(state.peak.load(Ordering::SeqCst), 2);
        assert_eq!(
            output
                .iter()
                .map(|page| page.blocks[0].bbox.left)
                .collect::<Vec<_>>(),
            vec![0.03, 0.01, 0.02]
        );
    }

    #[tokio::test]
    async fn axum_aio_batch_content_uses_caller_semaphore_and_keeps_order() {
        let state = mock_state();
        let client = mock_client(state.clone()).await;
        let semaphore = Arc::new(tokio::sync::Semaphore::new(1));
        let output = client
            .aio_batch_content_extract(
                vec![image_input(), image_input()],
                vec!["text".into(), "text".into()],
                VlmBatchPriority::All(None),
                Some(semaphore),
            )
            .await
            .unwrap();
        assert_eq!(state.peak.load(Ordering::SeqCst), 1);
        assert_eq!(
            output,
            vec![Some("recognized".into()), Some("recognized".into())]
        );
    }

    #[tokio::test]
    async fn axum_aio_batch_layout_uses_caller_semaphore_and_keeps_order() {
        let state = mock_state();
        let client = mock_client(state.clone()).await;
        let output = client
            .aio_batch_layout_detect(
                vec![image_input(), image_input(), image_input()],
                VlmBatchPriority::PerItem(vec![Some(30), Some(10), Some(20)]),
                Some(Arc::new(tokio::sync::Semaphore::new(1))),
            )
            .await
            .unwrap();
        assert_eq!(state.peak.load(Ordering::SeqCst), 1);
        assert_eq!(
            output
                .iter()
                .map(|page| page.blocks[0].bbox.left)
                .collect::<Vec<_>>(),
            vec![0.03, 0.01, 0.02]
        );
    }

    #[tokio::test]
    async fn axum_batch_layout_uses_configured_shared_semaphore() {
        let state = window_state();
        let client = window_client(state.clone()).await;
        let task = tokio::spawn(async move {
            client
                .batch_layout_detect(
                    vec![image_input(), image_input(), image_input()],
                    VlmBatchPriority::PerItem(vec![Some(30), Some(10), Some(20)]),
                )
                .await
        });
        timeout(Duration::from_secs(5), state.first_two.wait())
            .await
            .unwrap();
        assert_eq!(state.peak.load(Ordering::SeqCst), 2);
        state.release.notify_waiters();
        let output = timeout(Duration::from_secs(2), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(
            output
                .iter()
                .map(|page| page.blocks[0].bbox.left)
                .collect::<Vec<_>>(),
            vec![0.03, 0.01, 0.02]
        );
    }

    #[tokio::test]
    async fn http_batch_uses_permits_released_after_creation_and_keeps_order() {
        let state = window_state();
        let client = window_client(state.clone()).await;
        let held = client
            .layout_semaphore
            .clone()
            .acquire_owned()
            .await
            .unwrap();
        let requests = [30, 10, 20]
            .into_iter()
            .map(|priority| {
                client.request(
                    image_input(),
                    "Layout Detection".into(),
                    None,
                    Some(priority),
                )
            })
            .collect();
        let http = client.http.clone();
        let semaphore = client.layout_semaphore.clone();
        let task =
            tokio::spawn(async move { http.aio_batch_predict(requests, Some(semaphore)).await });
        timeout(Duration::from_secs(2), async {
            while state.layouts.load(Ordering::SeqCst) != 1 {
                sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .unwrap();
        drop(held);
        timeout(Duration::from_secs(2), state.first_two.wait())
            .await
            .unwrap();
        state.release.notify_waiters();
        let output = timeout(Duration::from_secs(2), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(output[0].contains(">30 0 31 "));
        assert!(output[1].contains(">10 0 11 "));
        assert!(output[2].contains(">20 0 21 "));
    }

    #[tokio::test]
    async fn axum_default_aio_layout_and_content_batches_share_configured_semaphore_and_order() {
        let state = mock_state();
        let client = mock_client(state.clone()).await;
        let (layouts, content) = tokio::join!(
            client.aio_batch_layout_detect(
                vec![image_input(), image_input(), image_input()],
                VlmBatchPriority::PerItem(vec![Some(30), Some(10), Some(20)]),
                None,
            ),
            client.aio_batch_content_extract(
                vec![image_input(), image_input(), image_input()],
                vec!["text".into(), "text".into(), "text".into()],
                VlmBatchPriority::PerItem(vec![Some(3), Some(1), Some(2)]),
                None,
            ),
        );
        assert!(state.peak.load(Ordering::SeqCst) <= 2);
        assert_eq!(
            layouts
                .unwrap()
                .iter()
                .map(|page| page.blocks[0].bbox.left)
                .collect::<Vec<_>>(),
            vec![0.03, 0.01, 0.02]
        );
        assert_eq!(
            content.unwrap(),
            vec![
                Some("recognized-3".into()),
                Some("recognized-1".into()),
                Some("recognized-2".into()),
            ]
        );
    }

    #[tokio::test]
    async fn single_semantic_operations_share_the_configured_semaphore() {
        let state = mock_state();
        let client = mock_client(state.clone()).await;
        let (layout, content, two_step, external, aio_layout, aio_content, aio_external) = tokio::join!(
            client.layout_detect(image_input(), None),
            client.content_extract(image_input(), BlockKind::TEXT.into(), None),
            client.two_step_extract(image_input(), None, vec![], None),
            client.extract_with_layout(
                image_input(),
                vec![block(BlockKind::TEXT)],
                None,
                vec![],
                None,
            ),
            client.aio_layout_detect(image_input(), None, None),
            client.aio_content_extract(image_input(), BlockKind::TEXT.into(), None, None),
            client.aio_extract_with_layout(
                image_input(),
                vec![block(BlockKind::TEXT)],
                None,
                None,
                vec![],
                None,
            ),
        );
        assert!(state.peak.load(Ordering::SeqCst) <= 2);
        assert!(!layout.unwrap().blocks.is_empty());
        assert_eq!(content.unwrap(), Some("recognized".into()));
        assert!(!two_step.unwrap().blocks.is_empty());
        assert_eq!(
            external.unwrap().blocks[0].content.as_deref(),
            Some("recognized")
        );
        assert!(!aio_layout.unwrap().blocks.is_empty());
        assert_eq!(aio_content.unwrap(), Some("recognized".into()));
        assert_eq!(
            aio_external.unwrap().blocks[0].content.as_deref(),
            Some("recognized")
        );
    }

    #[tokio::test]
    async fn axum_caller_layout_image_analysis_false_suppresses_visuals() {
        let state = mock_state();
        let client = mock_client(state.clone()).await;
        let output = client
            .extract_with_layout(
                image_input(),
                vec![block(BlockKind::IMAGE)],
                None,
                vec![],
                Some(false),
            )
            .await
            .unwrap();
        assert_eq!(state.requests.load(Ordering::SeqCst), 0);
        assert_eq!(output.blocks[0].block_type, BlockKind::IMAGE);
        assert!(output.blocks[0].content.is_none());
    }

    #[tokio::test]
    async fn batch_extract_with_layout_keeps_item_and_priority_order() {
        let state = mock_state();
        let client = mock_client(state.clone()).await;
        let output = client
            .batch_extract_with_layout(
                vec![image_input(), image_input()],
                vec![vec![block(BlockKind::TEXT)], vec![block(BlockKind::TEXT)]],
                VlmBatchPriority::PerItem(vec![Some(20), Some(10)]),
                vec![],
                None,
            )
            .await
            .unwrap();
        assert_eq!(state.requests.load(Ordering::SeqCst), 2);
        assert_eq!(
            output[0].blocks[0].content.as_deref(),
            Some("recognized-20")
        );
        assert_eq!(
            output[1].blocks[0].content.as_deref(),
            Some("recognized-10")
        );
    }

    #[tokio::test]
    async fn axum_external_layout_preserves_caller_underscore_metadata() {
        let state = mock_state();
        let client = mock_client(state).await;
        let mut layout = block(BlockKind::TEXT);
        layout.metadata.insert("_caller_key".into(), json!("kept"));
        layout.metadata.insert(POST_PROCESS_ORDER.into(), json!(42));
        let output = client
            .extract_with_layout(image_input(), vec![layout], None, vec![], None)
            .await
            .unwrap();
        assert_eq!(output.blocks[0].metadata["_caller_key"], "kept");
        assert_eq!(output.blocks[0].metadata[POST_PROCESS_ORDER], 42);
        assert_eq!(output.blocks[0].content.as_deref(), Some("recognized"));
    }

    #[test]
    fn official_layout_parse_stops_at_its_block_cap() {
        let pre = MinerUVlmPreprocessor {
            config: MinerUVlmConfig::default(),
        };
        let layout = "<|box_start|>0 0 1 1<|box_end|><|ref_start|>text<|ref_end|><|box_start|>2 2 3 3<|box_end|><|ref_start|>title<|ref_end|>";
        assert!(matches!(
            pre.parse_layout_output_capped(layout, 1),
            Err(VlmError::LimitExceeded {
                resource: "layout blocks",
                limit: 1,
                actual: 2
            })
        ));
    }

    #[test]
    fn official_extraction_stops_before_an_extra_crop() {
        let pre = MinerUVlmPreprocessor {
            config: MinerUVlmConfig::default(),
        };
        assert!(matches!(
            pre.prepare_for_extract_capped(
                &DynamicImage::new_rgb8(32, 32),
                &mut [block(BlockKind::TEXT), block(BlockKind::TEXT)],
                &[],
                Some(false),
                1,
            ),
            Err(VlmError::LimitExceeded {
                resource: "semantic requests per page",
                limit: 1,
                actual: 2
            })
        ));
    }

    #[test]
    fn official_extraction_suppresses_before_selecting_semantic_work() {
        let pre = MinerUVlmPreprocessor {
            config: MinerUVlmConfig::default(),
        };
        let mut blocks = [block(BlockKind::TABLE), block(BlockKind::TEXT)];
        let prepared = pre
            .prepare_for_extract_capped(
                &DynamicImage::new_rgb8(32, 32),
                &mut blocks,
                &[BlockKind::TABLE.into()],
                Some(false),
                1,
            )
            .unwrap();
        assert_eq!(prepared.block_indices, [1]);
    }

    #[test]
    fn semantic_candidates_defer_table_crop_encoding_until_admitted() {
        let pre = MinerUVlmPreprocessor {
            config: MinerUVlmConfig::default(),
        };
        let mut blocks = [block(BlockKind::TABLE), block(BlockKind::IMAGE)];
        let candidates = pre
            .semantic_candidates(&mut blocks, &[], Some(false), 1)
            .unwrap();
        assert_eq!(candidates.len(), 1);
        assert!(!blocks[0].metadata.contains_key("_table_image_token_map"));
        pre.encode_semantic_candidate_capped(
            &RgbImage::new(32, 32),
            &mut blocks,
            &candidates[0],
            u64::MAX,
        )
        .unwrap();
        assert!(blocks[0].metadata.contains_key("_table_image_token_map"));
    }

    #[tokio::test]
    async fn official_rgb_path_applies_flags_batches_and_byte_accounting() {
        let requests = Arc::new(AtomicUsize::new(0));
        let app = Router::new().route(
            "/v1/chat/completions",
            post({
                let requests = requests.clone();
                move |Json(request): Json<serde_json::Value>| {
                    let requests = requests.clone();
                    async move {
                        requests.fetch_add(1, Ordering::SeqCst);
                        let content = if request.to_string().contains("Layout Detection") {
                            "<|box_start|>0 0 1000 1000<|box_end|><|ref_start|>text<|ref_end|><|rotate_up|><|box_start|>1 2 3 4<|box_end|><|ref_start|>table<|ref_end|><|rotate_right|><|box_start|>5 6 7 8<|box_end|><|ref_start|>text<|ref_end|><|rotate_down|>"
                        } else {
                            "recognized"
                        };
                        Json(json!({"choices":[{"finish_reason":"stop","message":{"content":content}}]}))
                    }
                }
            }),
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
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
        let image = Arc::new(RgbImage::new(32, 32));
        let mut pages = client
            .official_two_step_snapshot_window(
                vec![Arc::clone(&image)],
                false,
                false,
                false,
                8,
                2,
                1,
                1 << 20,
                1 << 20,
                1 << 20,
                1 << 20,
                Instant::now() + Duration::from_secs(5),
            )
            .await
            .unwrap();
        assert_eq!(pages.len(), 1);
        let (snapshot, _, raw, encoded) = pages.pop().unwrap();
        assert_eq!(requests.load(Ordering::SeqCst), 3);
        assert!(snapshot[1].content.is_none());
        assert!(raw > 0 && encoded > 0);
        assert_eq!(Arc::strong_count(&image), 1);
    }

    #[tokio::test]
    async fn snapshot_window_uses_request_permits_and_keeps_page_order() {
        let state = window_state();
        let client = window_client(state.clone()).await;
        let owners = (0..3)
            .map(|n| Arc::new(RgbImage::from_pixel(8, 8, image::Rgb([n, 0, 0]))))
            .collect::<Vec<_>>();
        let pages = owners.iter().map(Arc::clone).collect::<Vec<_>>();
        let task = tokio::spawn({
            let client = client.clone();
            async move {
                client
                    .official_two_step_snapshot_window(
                        pages,
                        false,
                        true,
                        true,
                        4,
                        2,
                        2,
                        1 << 20,
                        1 << 20,
                        1 << 20,
                        1 << 20,
                        Instant::now() + Duration::from_secs(10),
                    )
                    .await
            }
        });
        timeout(Duration::from_secs(5), state.first_two.wait())
            .await
            .unwrap();
        assert_eq!(state.peak.load(Ordering::SeqCst), 2);
        assert_eq!(state.layouts.load(Ordering::SeqCst), 2);
        state.release.notify_waiters();
        let pages = timeout(Duration::from_secs(2), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(state.peak.load(Ordering::SeqCst), 2);
        assert_eq!(
            pages
                .iter()
                .map(|page| page.0[0].bbox.unwrap().left)
                .collect::<Vec<_>>(),
            vec![0.1, 0.2, 0.3]
        );
        assert_eq!(pages[0].0[0].content.as_deref(), Some("  raw semantic  "));
        assert_eq!(pages[0].1[0].content.as_deref(), Some("raw semantic"));
        assert!(owners.iter().all(|image| Arc::strong_count(image) == 1));
    }

    #[tokio::test]
    #[ignore = "full VLM snapshot-window resource integration e2e"]
    async fn snapshot_window_shares_encoded_budget_and_keeps_exact_local_caps() {
        let state = window_state();
        state.layouts.store(2, Ordering::SeqCst); // no admission hold in this limit test
        let client = window_client(state).await;
        let image = Arc::new(RgbImage::from_pixel(8, 8, image::Rgb([7, 0, 0])));
        let layout_bytes = client
            .preprocessor
            .prepare_rgb_for_layout_capped(&image, u64::MAX)
            .unwrap()
            .image
            .data
            .len();
        let pages = vec![Arc::clone(&image), Arc::clone(&image)];
        assert!(matches!(
            client
                .official_two_step_snapshot_window(
                    pages.iter().map(Arc::clone).collect(),
                    false,
                    true,
                    true,
                    4,
                    2,
                    2,
                    layout_bytes - 1,
                    1 << 20,
                    1 << 20,
                    1 << 20,
                    Instant::now() + Duration::from_secs(2)
                )
                .await,
            Err(VlmError::LimitExceeded {
                resource: "encoded request bytes",
                ..
            })
        ));
        assert!(matches!(
            client
                .official_two_step_snapshot_window(
                    pages.iter().map(Arc::clone).collect(),
                    false,
                    true,
                    true,
                    4,
                    2,
                    2,
                    1 << 20,
                    layout_bytes - 1,
                    1 << 20,
                    1 << 20,
                    Instant::now() + Duration::from_secs(2)
                )
                .await,
            Err(VlmError::LimitExceeded {
                resource: "encoded batch bytes",
                ..
            })
        ));
        assert!(matches!(
            client
                .official_two_step_snapshot_window(
                    pages.iter().map(Arc::clone).collect(),
                    false,
                    true,
                    true,
                    4,
                    2,
                    2,
                    1 << 20,
                    1 << 20,
                    layout_bytes * 2 - 1,
                    1 << 20,
                    Instant::now() + Duration::from_secs(2)
                )
                .await,
            Err(VlmError::LimitExceeded {
                resource: "encoded document bytes",
                ..
            })
        ));
        assert_eq!(Arc::strong_count(&image), 3);
    }

    #[tokio::test]
    async fn snapshot_window_counts_complete_ignored_json_against_shared_raw_budget() {
        let mut state = window_state();
        state.ignored = 4096;
        let body = serde_json::to_vec(&json!({"ignored":"x".repeat(state.ignored),"choices":[{"finish_reason":"stop","message":{"content":"<|box_start|>0 0 1 1<|box_end|><|ref_start|>text<|ref_end|><|rotate_up|>"}}]})).unwrap();
        let client = window_client(state.clone()).await;
        let owners = vec![Arc::new(RgbImage::new(8, 8)), Arc::new(RgbImage::new(8, 8))];
        let pages = owners.iter().map(Arc::clone).collect::<Vec<_>>();
        let task = tokio::spawn({
            let client = client.clone();
            async move {
                client
                    .official_two_step_snapshot_window(
                        pages,
                        false,
                        true,
                        true,
                        4,
                        2,
                        2,
                        1 << 20,
                        1 << 20,
                        1 << 20,
                        body.len() * 2 - 1,
                        Instant::now() + Duration::from_secs(10),
                    )
                    .await
            }
        });
        timeout(Duration::from_secs(5), state.first_two.wait())
            .await
            .unwrap();
        state.release.notify_waiters();
        assert!(matches!(
            timeout(Duration::from_secs(10), task)
                .await
                .unwrap()
                .unwrap(),
            Err(VlmError::LimitExceeded { .. })
        ));
        assert!(owners.iter().all(|image| Arc::strong_count(image) == 1));
    }

    #[tokio::test]
    async fn snapshot_window_drops_pending_sibling_after_http_failure() {
        let state = FailWindowState {
            // Both handlers and the test cross together before one handler fails.
            entered: Arc::new(Barrier::new(3)),
            pending: Arc::new(Notify::new()),
            failures: Arc::new(AtomicUsize::new(0)),
        };
        let client = fail_window_client(state.clone()).await;
        let owners = vec![
            Arc::new(RgbImage::from_pixel(8, 8, image::Rgb([255, 0, 0]))),
            Arc::new(RgbImage::from_pixel(8, 8, image::Rgb([0, 0, 0]))),
        ];
        let pages = owners.iter().map(Arc::clone).collect::<Vec<_>>();
        let task = tokio::spawn(async move {
            client
                .official_two_step_snapshot_window(
                    pages,
                    false,
                    true,
                    true,
                    4,
                    2,
                    2,
                    1 << 20,
                    1 << 20,
                    1 << 20,
                    1 << 20,
                    Instant::now() + Duration::from_secs(2),
                )
                .await
        });
        timeout(Duration::from_secs(2), state.entered.wait())
            .await
            .unwrap();
        assert!(matches!(
            timeout(Duration::from_secs(1), task)
                .await
                .unwrap()
                .unwrap(),
            Err(VlmError::Http { status: 500, .. })
        ));
        assert!(owners.iter().all(|image| Arc::strong_count(image) == 1));
        state.pending.notify_one();
    }
}
