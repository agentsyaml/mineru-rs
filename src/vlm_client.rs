use crate::vlm_http::ByteBudget;
use crate::*;
#[cfg(test)]
use base64::{Engine as _, engine::general_purpose::STANDARD};
use futures_util::{StreamExt, future::try_join_all, stream::FuturesUnordered};
use image::{DynamicImage, ImageFormat, RgbImage, imageops::FilterType};
use serde_json::Map;
use std::sync::Arc;
use std::time::Instant;
use std::{future::Future, io::Cursor, pin::Pin};
use tokio::sync::Semaphore;

const COVERED_IMAGE_CAPTION: &str = "_covered_image_caption";
const POST_PROCESS_ORDER: &str = "mineru_post_process_order";

#[derive(Clone)]
struct SemanticCandidate {
    original_index: usize,
    block: ContentBlock,
    absorbed: Vec<ContentBlock>,
}

/// An encoded semantic candidate plus its table token map, as produced by the two-phase encoder.
type EncodedCandidateWithTokens = (
    VlmEncodedImage,
    String,
    Option<SamplingParams>,
    usize,
    Map<String, serde_json::Value>,
);

fn official_snapshot_block(block: VlmLayoutBlock) -> VlmResult<(ModelBlock, Option<String>)> {
    if block.block_type.is_empty() {
        return Err(protocol(
            "official model snapshot",
            "layout block type is required",
        ));
    }
    // A missing angle is one block's quirk, not a page-killing defect: treat it as unrotated
    // and warn. This mirrors the direct pipeline, where angle=None is a rotate no-op.
    let (angle, warning) = match block.angle {
        Some(angle) => (angle, None),
        None => (
            Rotation::Deg0,
            Some("layout block angle is missing; assuming rotation 0".into()),
        ),
    };
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
    Ok((
        ModelBlock {
            block_type: block.block_type.clone(),
            bbox: Some(block.bbox),
            angle: Some(angle),
            content: block.content,
            merge_prev: (block.block_type == BlockKind::TEXT)
                .then_some(block.merge_prev.unwrap_or(false)),
            sub_type,
            extra,
        },
        warning,
    ))
}

fn protocol(operation: &'static str, message: impl Into<String>) -> VlmError {
    VlmError::Protocol {
        operation,
        message: message.into(),
    }
}
fn check_lengths(
    images: usize,
    others: usize,
    operation: &'static str,
    message: &str,
) -> Result<(), VlmError> {
    if images != others {
        return Err(protocol(operation, message));
    }
    Ok(())
}
fn cap_warning(
    resource: &str,
    bytes: usize,
    cap: usize,
    cap_kind: &str,
    continuation: &str,
) -> String {
    format!(
        "encoded {resource} request bytes ({bytes}) exceed the {cap_kind} cap {cap}; continuing with {continuation}"
    )
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
        let (candidates, truncated) = self.semantic_candidates_truncated(
            blocks,
            prompts,
            image_analysis,
            max_semantic_requests,
        );
        if truncated {
            // The extract pipelines have no warnings channel, so keep the hard error there; the
            // official snapshot page calls the truncated variant and warns instead.
            return Err(VlmError::LimitExceeded {
                resource: "semantic requests per page",
                limit: max_semantic_requests as u64,
                actual: max_semantic_requests.saturating_add(1) as u64,
            });
        }
        Ok(candidates)
    }
    /// Truncating variant: never fails on the cap; returns the candidates that fit plus whether
    /// the cap was hit (so callers with a warnings channel can degrade instead of aborting).
    fn semantic_candidates_truncated(
        &self,
        blocks: &mut [VlmLayoutBlock],
        prompts: &[String],
        image_analysis: Option<bool>,
        max_semantic_requests: usize,
    ) -> (Vec<SemanticCandidate>, bool) {
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
        let mut truncated = false;
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
                truncated = true;
                break;
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
        (candidates, truncated)
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
        let (image, prompt, sampling, original_index, tokens) =
            self.encode_semantic_candidate_capped_with_tokens(page, candidate, max_pixels)?;
        if !tokens.is_empty() {
            blocks[original_index].metadata.insert(
                "_table_image_token_map".into(),
                serde_json::Value::Object(tokens),
            );
        }
        Ok((image, prompt, sampling, original_index))
    }
    /// Two-phase variant of the candidate encoder: encodes without touching `blocks`. The table
    /// token map is returned so the caller can backfill it once every parallel encode finished
    /// (parallel writes into the shared block storage would race).
    fn encode_semantic_candidate_capped_with_tokens(
        &self,
        page: &RgbImage,
        candidate: &SemanticCandidate,
        max_pixels: u64,
    ) -> VlmResult<EncodedCandidateWithTokens> {
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
            tokens,
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
}

// Finite compatibility default for the client-created official page semaphore. This is a
// default, not a clamp: commands pass their own resolved `OfficialPageConcurrency`, and
// `MINERU_OFFICIAL_PAGE_CONCURRENCY`/`--page-concurrency` accept any positive value. The page
// semaphore bounds only how many page pipelines run at once; HTTP admission is the request-level
// `layout_semaphore`'s job.
const OFFICIAL_PAGE_CONCURRENCY: usize = 64;

#[derive(Debug, Clone)]
enum VlmBackend {
    Http(VlmHttpClient),
}

impl VlmBackend {
    async fn predict_official_budgeted(
        &self,
        request: VlmRequest,
        cap: usize,
        budget: Option<Arc<ByteBudget>>,
        deadline: tokio::time::Instant,
    ) -> VlmResult<(String, usize, Vec<String>)> {
        let Self::Http(client) = self;
        client
            .predict_official_budgeted(request, cap, budget, deadline)
            .await
    }

    async fn aio_batch_predict(
        &self,
        requests: Vec<VlmRequest>,
        semaphore: VlmSemaphore,
    ) -> VlmResult<Vec<String>> {
        let Self::Http(client) = self;
        client.aio_batch_predict(requests, semaphore).await
    }
}

#[derive(Clone)]
pub struct MinerUVlmClient {
    backend: VlmBackend,
    image_config: Arc<VlmHttpConfig>,
    max_decoded_pixels: u64,
    task_work_lease: TaskWorkLease,
    preprocessor: MinerUVlmPreprocessor,
    layout_semaphore: Arc<Semaphore>,
    official_page_semaphore: Arc<Semaphore>,
    /// Request-level concurrency model selected by `MINERU_OFFICIAL_CONCURRENCY_MODEL`.
    /// `Classic` runs the single-slot pipeline; `TwoPhase` runs encode-all then request-all.
    concurrency_model: ConcurrencyModel,
    /// Global CPU cap for parallel semantic-candidate encoding (two-phase model).
    encode_cpu_semaphore: Arc<Semaphore>,
    /// Chunk size for the two-phase encode-all stage: equals the encode CPU semaphore's
    /// capacity so one chunk's encoders run fully in parallel without internal queueing.
    encode_cpu_parallelism: usize,
    #[cfg(test)]
    semantic_scheduler_hook: Option<SemanticSchedulerHook>,
}
#[cfg(test)]
#[derive(Clone)]
struct SemanticSchedulerHook {
    before_encode: Arc<dyn Fn(usize) + Send + Sync>,
    after_encode: Arc<dyn Fn(usize) + Send + Sync>,
    state: Arc<dyn Fn(usize, usize, usize) + Send + Sync>,
    completed: Arc<dyn Fn(usize) + Send + Sync>,
}
#[cfg(test)]
impl std::fmt::Debug for SemanticSchedulerHook {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("SemanticSchedulerHook")
    }
}
impl std::fmt::Debug for MinerUVlmClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MinerUVlmClient")
            .field("backend", &self.backend)
            .field("preprocessor", &self.preprocessor)
            .finish_non_exhaustive()
    }
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
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn parse_and_write_prepared_pdf_with_totals_and_page_concurrency(
        &self,
        prepared: crate::input_prepare::PreparedPdf,
        options: OfficialPdfOptions,
        output_root: &std::path::Path,
        stem: &str,
        events: Option<ProgressCallback>,
        cleanup_warning: Option<crate::official_route::CleanupWarningCallback>,
        totals: crate::document_limits::OfficialDocumentTotals,
        page_concurrency: crate::official_route::OfficialPageConcurrency,
    ) -> VlmResult<OfficialOutputManifest> {
        crate::official_route::parse_and_write_prepared_with_events_and_cleanup_warning_with_totals_and_page_concurrency(
            self, prepared, options, output_root, stem, events, cleanup_warning, totals, page_concurrency,
        ).await
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
        // Document-wide resident-encoded-bytes ledger created by the window function. Only the
        // two-phase encode-all stage charges it; the classic path ignores it entirely.
        resident_encoded_budget: Arc<ByteBudget>,
        deadline: Instant,
        page_semaphore: Arc<Semaphore>,
    ) -> VlmResult<(
        Vec<ModelBlock>,
        Vec<VlmLayoutBlock>,
        usize,
        usize,
        Vec<String>,
    )> {
        if max_requests_per_batch == 0 {
            return Err(VlmError::InvalidConfig(
                "semantic requests per batch must be greater than zero".into(),
            ));
        }
        if max_encoded_request_bytes > max_encoded_batch_bytes {
            return Err(VlmError::InvalidConfig(
                "encoded request bytes must not exceed encoded batch bytes".into(),
            ));
        }
        let _page_permit = tokio::time::timeout_at(
            tokio::time::Instant::from_std(deadline),
            page_semaphore.acquire_owned(),
        )
        .await
        .map_err(|_| VlmError::Timeout {
            operation: "official PDF",
        })?
        .map_err(|_| VlmError::Transport {
            operation: "official PDF",
            message: "page semaphore closed".into(),
        })?;
        let preprocessor = self.preprocessor.clone();
        let layout_image = Arc::clone(&image);
        let max_pixels = self.max_decoded_pixels;
        let prepared_layout = self
            .official_blocking(deadline, move || {
                preprocessor.prepare_rgb_for_layout_capped(&layout_image, max_pixels)
            })
            .await?;
        let mut warnings = Vec::new();
        let layout_bytes = prepared_layout.image.data.len();
        // A layout image alone at or over a per-request/batch cap is one page's problem, not the
        // document's: skip the layout request and degrade to an empty layout instead of failing
        // the whole document. The document-level encoded budget charge below stays a hard error.
        let mut skip_layout_request = false;
        if layout_bytes > max_encoded_request_bytes {
            warnings.push(cap_warning(
                "layout",
                layout_bytes,
                max_encoded_request_bytes,
                "per-request",
                "an empty layout",
            ));
            skip_layout_request = true;
        } else if layout_bytes > max_encoded_batch_bytes {
            warnings.push(cap_warning(
                "layout",
                layout_bytes,
                max_encoded_batch_bytes,
                "batch",
                "an empty layout",
            ));
            skip_layout_request = true;
        }
        encoded_budget.charge(layout_bytes as u64, "encoded document bytes")?;
        let mut encoded_bytes = layout_bytes;
        let mut raw_bytes = 0;
        let mut blocks;
        if skip_layout_request {
            blocks = Vec::new();
        } else {
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
                // A closed semaphore is an internal runtime failure, never LLM malformation: it
                // must not be classifiable as Protocol (which the tolerance arms would degrade).
                .map_err(|_| VlmError::Transport {
                    operation: "official PDF",
                    message: "layout semaphore closed".into(),
                })?;
            let (layout_text, layout_raw_bytes, layout_warnings) = self
                .backend
                .predict_official_budgeted(
                    layout_request,
                    self.image_config.max_response_bytes,
                    Some(raw_budget.clone()),
                    tokio::time::Instant::from_std(deadline),
                )
                .await?;
            drop(_permit);
            raw_bytes = layout_raw_bytes;
            warnings.extend(layout_warnings);
            let preprocessor = self.preprocessor.clone();
            blocks = match self
                .official_blocking(deadline, move || {
                    preprocessor.parse_layout_output_capped(&layout_text, max_layout_blocks)
                })
                .await
            {
                // Parse-class (Protocol) malformation of the LLM layout text and the per-page
                // layout block cap degrade to an empty layout. Service failures
                // (Http/Transport/Timeout) and document-level budget errors stay fatal so a
                // broken server is never masked.
                Err(error @ (VlmError::Protocol { .. } | VlmError::LimitExceeded { .. })) => {
                    warnings.push(format!(
                        "layout parse failed: {error}; continuing with an empty layout"
                    ));
                    Vec::new()
                }
                Err(error) => return Err(error),
                Ok(blocks) => blocks,
            };
        }
        let suppress = [
            (!formula_enable).then_some(BlockKind::EQUATION.into()),
            (!table_enable).then_some(BlockKind::TABLE.into()),
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();
        let preprocessor = self.preprocessor.clone();
        let (blocks, candidates, truncated) = self
            .official_blocking(deadline, move || {
                let (candidates, truncated) = preprocessor.semantic_candidates_truncated(
                    &mut blocks,
                    &suppress,
                    Some(image_analysis),
                    max_semantic_requests,
                );
                Ok((blocks, candidates, truncated))
            })
            .await?;
        // A page with more semantic candidates than allowed degrades to the first ones, never
        // fails the whole document; the blocks themselves are still emitted.
        if truncated {
            warnings.push(format!(
                "semantic requests exceed the per-page cap of {max_semantic_requests}; continuing with the first {max_semantic_requests} candidates"
            ));
        }
        let block_count = blocks.len();
        let page = image;
        if self.concurrency_model == ConcurrencyModel::TwoPhase {
            return self
                .official_two_step_semantic_two_phase(
                    blocks,
                    candidates,
                    page,
                    max_encoded_request_bytes,
                    max_encoded_batch_bytes,
                    max_requests_per_batch,
                    raw_budget,
                    encoded_budget,
                    resident_encoded_budget,
                    warnings,
                    raw_bytes,
                    encoded_bytes,
                    deadline,
                )
                .await;
        }
        type Encoder = Pin<
            Box<
                dyn Future<
                        Output = VlmResult<(
                            Vec<VlmLayoutBlock>,
                            VlmEncodedImage,
                            String,
                            Option<SamplingParams>,
                            usize,
                            usize,
                        )>,
                    > + Send,
            >,
        >;
        type Request = Pin<
            Box<
                dyn Future<Output = VlmResult<(usize, usize, String, usize, usize, Vec<String>)>>
                    + Send,
            >,
        >;
        let mut blocks = Some(blocks);
        let mut replies = vec![None; candidates.len()];
        let mut encoder: Option<Encoder> = None;
        let mut requests: FuturesUnordered<Request> = FuturesUnordered::new();
        let mut next_candidate = 0;
        let mut resident_bytes = 0usize;
        let mut encoder_reservation = 0usize;
        while next_candidate < candidates.len() || encoder.is_some() || !requests.is_empty() {
            if encoder.is_none()
                && next_candidate < candidates.len()
                && requests.len() < max_requests_per_batch
                && resident_bytes <= max_encoded_batch_bytes - max_encoded_request_bytes
            {
                let candidate = candidates[next_candidate].clone();
                let order = next_candidate;
                next_candidate += 1;
                encoder_reservation = max_encoded_request_bytes;
                resident_bytes += encoder_reservation;
                let client = self.clone();
                let page = page.clone();
                let cpu_semaphore = self.encode_cpu_semaphore.clone();
                let blocks_for_encode = blocks.take().expect("encoder owns blocks exclusively");
                let max_pixels = self.max_decoded_pixels;
                let preprocessor = client.preprocessor.clone();
                #[cfg(test)]
                let scheduler_hook = self.semantic_scheduler_hook.clone();
                #[cfg(test)]
                if let Some(hook) = &scheduler_hook {
                    (hook.state)(
                        resident_bytes.saturating_sub(encoder_reservation),
                        encoder_reservation,
                        requests.len(),
                    );
                }
                encoder = Some(Box::pin(async move {
                    let _permit = cpu_semaphore
                        .acquire_owned()
                        .await
                        .map_err(|_| VlmError::Transport {
                            operation: "official PDF",
                            message: "encode semaphore closed".into(),
                        })?;
                    client
                        .official_blocking(deadline, move || {
                            #[cfg(test)]
                            if let Some(hook) = &scheduler_hook {
                                (hook.before_encode)(order);
                            }
                            let mut blocks = blocks_for_encode;
                            let prepared = preprocessor.encode_semantic_candidate_capped(
                                &page,
                                &mut blocks,
                                &candidate,
                                max_pixels,
                            )?;
                            #[cfg(test)]
                            if let Some(hook) = &scheduler_hook {
                                (hook.after_encode)(order);
                            }
                            Ok((
                                blocks, prepared.0, prepared.1, prepared.2, prepared.3, order,
                            ))
                        })
                        .await
                }));
            }
            tokio::select! {
                encoded = async {
                    match encoder.as_mut() {
                        Some(encoder) => encoder.as_mut().await,
                        None => std::future::pending().await,
                    }
                } => {
                    let (next_blocks, image, prompt, sampling, block_index, order) = encoded?;
                    blocks = Some(next_blocks);
                    resident_bytes = resident_bytes.checked_sub(encoder_reservation).expect("encoder reservation held");
                    encoder_reservation = 0;
                    let bytes = image.data.len();
                    if order >= replies.len() || block_index >= block_count {
                        return Err(protocol("official PDF", "semantic reply index is invalid"));
                    }
                    if bytes > max_encoded_request_bytes {
                        // A single block that encodes over the per-request cap degrades to empty
                        // content for this block only; sibling blocks and the page continue
                        // (mirrors the Protocol arm below).
                        warnings.push(cap_warning(
                            "semantic",
                            bytes,
                            max_encoded_request_bytes,
                            "per-request",
                            "empty content",
                        ));
                        replies[order] = Some((block_index, String::new()));
                        encoder = None;
                        continue;
                    }
                    // Overflow in the resident-bytes ledger is an internal invariant break, never
                    // degrade: keep it fatal (the tolerance arms only ever see LLM malformation).
                    let resident_after = resident_bytes.checked_add(bytes).ok_or(
                        VlmError::LimitExceeded {
                            resource: "encoded batch bytes",
                            limit: max_encoded_batch_bytes as u64,
                            actual: u64::MAX,
                        },
                    )?;
                    if resident_after > max_encoded_batch_bytes {
                        warnings.push(cap_warning(
                            "semantic",
                            bytes,
                            max_encoded_batch_bytes,
                            "batch",
                            "empty content",
                        ));
                        replies[order] = Some((block_index, String::new()));
                        encoder = None;
                        continue;
                    }
                    resident_bytes = resident_after;
                    // The document-level encoded budget and the returned encoded-bytes accounting
                    // count only the retained candidates: per-request- or batch-degraded
                    // candidates never reach the server, so charging them could only push the
                    // document over its budget (mirrors the two-phase accounting).
                    encoded_budget.charge(bytes as u64, "encoded document bytes")?;
                    encoded_bytes = encoded_bytes.saturating_add(bytes);
                    let request = self.request(VlmImageInput::Bytes { data: image.data, media_type: Some(image.media_type) }, prompt, sampling, None);
                    let backend = self.backend.clone();
                    let max_response_bytes = self.image_config.max_response_bytes;
                    let semaphore = self.layout_semaphore.clone();
                    let raw_budget = raw_budget.clone();
                    requests.push(Box::pin(async move {
                        let _permit = semaphore.acquire_owned().await.map_err(|_| VlmError::Transport {
                            operation: "official PDF",
                            message: "layout semaphore closed".into(),
                        })?;
                        match backend.predict_official_budgeted(request, max_response_bytes, Some(raw_budget), tokio::time::Instant::from_std(deadline)).await {
                            // Only parse-class (Protocol) malformation of the LLM reply degrades
                            // to empty content for this block. Resource/configuration failures and
                            // service failures (Http/Transport/Timeout) stay fatal so a wrong key
                            // or dead server is never masked as an empty successful document.
                            Ok((reply, raw_bytes, warnings)) => {
                                Ok((order, block_index, reply, raw_bytes, bytes, warnings))
                            }
                            Err(error @ VlmError::Protocol { .. }) => Ok((
                                order,
                                block_index,
                                String::new(),
                                0,
                                bytes,
                                vec![format!(
                                    "semantic request failed: {error}; continuing with empty content"
                                )],
                            )),
                            Err(error) => Err(error),
                        }
                    }));
                    encoder = None;
                    #[cfg(test)]
                    if let Some(hook) = &self.semantic_scheduler_hook {
                        (hook.state)(
                            resident_bytes.saturating_sub(encoder_reservation),
                            encoder_reservation,
                            requests.len(),
                        );
                    }
                }
                completed = requests.next(), if !requests.is_empty() => {
                    let (order, block_index, reply, bytes, lease, request_warnings) =
                        completed.expect("nonempty request queue")?;
                    warnings.extend(request_warnings);
                    resident_bytes = resident_bytes.checked_sub(lease).expect("request lease held");
                    if order >= replies.len() || block_index >= block_count {
                        return Err(protocol("official PDF", "semantic reply index is invalid"));
                    }
                    raw_bytes = raw_bytes.saturating_add(bytes);
                    replies[order] = Some((block_index, reply));
                    #[cfg(test)]
                    if let Some(hook) = &self.semantic_scheduler_hook {
                        (hook.completed)(order);
                        (hook.state)(
                            resident_bytes.saturating_sub(encoder_reservation),
                            encoder_reservation,
                            requests.len(),
                        );
                    }
                }
            }
        }
        self.complete_official_semantic_page(
            blocks.expect("encoder completed before semantic completion"),
            replies,
            warnings,
            raw_bytes,
            encoded_bytes,
            deadline,
        )
        .await
    }

    /// Shared tail of the classic and two-phase semantic pipelines: fill reply content into the
    /// layout blocks in source order, snapshot them, and run the shared cleaner.
    async fn complete_official_semantic_page(
        &self,
        mut blocks: Vec<VlmLayoutBlock>,
        replies: Vec<Option<(usize, String)>>,
        mut warnings: Vec<String>,
        raw_bytes: usize,
        encoded_bytes: usize,
        deadline: Instant,
    ) -> VlmResult<(
        Vec<ModelBlock>,
        Vec<VlmLayoutBlock>,
        usize,
        usize,
        Vec<String>,
    )> {
        for reply in replies {
            let (index, reply) =
                reply.ok_or_else(|| protocol("official PDF", "semantic reply missing"))?;
            blocks[index].content = Some(reply);
        }
        let preprocessor = self.preprocessor.clone();
        let (snapshot, cleaned, snapshot_warnings) = self
            .official_blocking(deadline, move || {
                let mut snapshot = Vec::with_capacity(blocks.len());
                let mut snapshot_warnings = Vec::new();
                for block in &blocks {
                    let (snapshot_block, warning) = official_snapshot_block(block.clone())?;
                    snapshot_warnings.extend(warning);
                    snapshot.push(snapshot_block);
                }
                for block in &mut blocks {
                    if let Some(content) = block.content.clone() {
                        let mut native = from_vlm(block.clone());
                        vlm_postprocess::clean_block(&mut native, content);
                        *block = to_vlm(native);
                    }
                }
                Ok((
                    snapshot,
                    preprocessor.post_process(blocks)?,
                    snapshot_warnings,
                ))
            })
            .await?;
        warnings.extend(snapshot_warnings);
        Ok((snapshot, cleaned, raw_bytes, encoded_bytes, warnings))
    }

    /// Two-phase semantic pipeline (encode-all, then request-all) used when
    /// `MINERU_OFFICIAL_CONCURRENCY_MODEL=two-phase`. The branch gate in
    /// `official_two_step_snapshot_page_core` sends the page here instead of the classic
    /// single-slot pipeline; degradation, deadline, budget, order, and FailFast semantics match
    /// the classic path, only the concurrency shape differs.
    ///
    /// Memory characteristic: encode-all stages candidates in chunks of `encode_cpu_parallelism`
    /// (the encode CPU semaphore's capacity), retaining or degrading each chunk's results before
    /// the next chunk encodes, so a single page's encoded peak stays at `max_encoded_batch_bytes`
    /// plus one chunk. The document-wide resident-bytes ledger (`resident_encoded_budget`) is a
    /// defensive invariant sentinel that bounds cumulative retained bytes across the window
    /// (batch cap times window page count), not a page-residency cap.
    #[allow(clippy::too_many_arguments)]
    async fn official_two_step_semantic_two_phase(
        &self,
        mut blocks: Vec<VlmLayoutBlock>,
        candidates: Vec<SemanticCandidate>,
        page: Arc<RgbImage>,
        max_encoded_request_bytes: usize,
        max_encoded_batch_bytes: usize,
        max_requests_per_batch: usize,
        raw_budget: Arc<ByteBudget>,
        encoded_budget: Arc<ByteBudget>,
        resident_encoded_budget: Arc<ByteBudget>,
        mut warnings: Vec<String>,
        mut raw_bytes: usize,
        mut encoded_bytes: usize,
        deadline: Instant,
    ) -> VlmResult<(
        Vec<ModelBlock>,
        Vec<VlmLayoutBlock>,
        usize,
        usize,
        Vec<String>,
    )> {
        let block_count = blocks.len();
        let max_pixels = self.max_decoded_pixels;
        let mut replies = vec![None; candidates.len()];
        // Phase 1, encode-all in chunks: candidates encode in chunks of the encode CPU
        // semaphore's capacity so one chunk runs fully in parallel, then each chunk's results
        // are immediately retained or degraded (classic rules) before the next chunk encodes.
        // Peak residency is therefore <= max_encoded_batch_bytes plus one chunk. The encoders
        // never touch `blocks`; each returns its table token map so the map is backfilled
        // between chunks (no encoder runs concurrently with a backfill).
        //
        // Note: accounting timing differs slightly from classic — chunks before a later encode
        // failure may already have been charged to the document budgets, but the page returns
        // Err and the whole document aborts, so those charges are unobservable. Deliberately
        // accepted.
        type TwoPhaseEncoder =
            Pin<Box<dyn Future<Output = VlmResult<(EncodedCandidateWithTokens, usize)>> + Send>>;
        let chunk = self.encode_cpu_parallelism.max(1);
        let mut resident_bytes = 0usize;
        // Retained candidates in source order, drained by the request-all stage.
        let mut retained: Vec<(
            usize,
            VlmEncodedImage,
            String,
            Option<SamplingParams>,
            usize,
        )> = Vec::new();
        {
            let cpu_semaphore = self.encode_cpu_semaphore.clone();
            let mut chunk_start = 0;
            while chunk_start < candidates.len() {
                let chunk_end = (chunk_start + chunk).min(candidates.len());
                let mut encoders: FuturesUnordered<TwoPhaseEncoder> = FuturesUnordered::new();
                for (local_idx, candidate) in candidates[chunk_start..chunk_end].iter().enumerate()
                {
                    let order = chunk_start + local_idx;
                    let candidate = candidate.clone();
                    let page = page.clone();
                    let client = self.clone();
                    let preprocessor = client.preprocessor.clone();
                    let semaphore = cpu_semaphore.clone();
                    #[cfg(test)]
                    let scheduler_hook = self.semantic_scheduler_hook.clone();
                    encoders.push(Box::pin(async move {
                        let _permit =
                            semaphore
                                .acquire_owned()
                                .await
                                .map_err(|_| VlmError::Transport {
                                    operation: "official PDF",
                                    message: "encode semaphore closed".into(),
                                })?;
                        client
                            .official_blocking(deadline, move || {
                                #[cfg(test)]
                                if let Some(hook) = &scheduler_hook {
                                    (hook.before_encode)(order);
                                }
                                let prepared = preprocessor
                                    .encode_semantic_candidate_capped_with_tokens(
                                        &page, &candidate, max_pixels,
                                    )?;
                                #[cfg(test)]
                                if let Some(hook) = &scheduler_hook {
                                    (hook.after_encode)(order);
                                }
                                Ok((
                                    (prepared.0, prepared.1, prepared.2, prepared.3, prepared.4),
                                    order,
                                ))
                            })
                            .await
                    }));
                }
                let mut chunk_results: Vec<Option<EncodedCandidateWithTokens>> =
                    (0..(chunk_end - chunk_start)).map(|_| None).collect();
                while let Some(result) = encoders.next().await {
                    // FailFast: an encode failure is internal (never LLM malformation), so the
                    // first error aborts the page exactly like the classic single-slot encoder.
                    let (encoded, order) = result?;
                    chunk_results[order - chunk_start] = Some(encoded);
                }
                // Retain or degrade this chunk in source order with the classic page-local
                // accounting rules (order, token-map backfill, and warning text unchanged).
                for (local_idx, chunk_result) in chunk_results.into_iter().enumerate() {
                    let order = chunk_start + local_idx;
                    let (image, prompt, sampling, original_index, tokens) =
                        chunk_result.expect("two-phase chunk encode result present");
                    let bytes = image.data.len();
                    if order >= replies.len() || original_index >= block_count {
                        return Err(protocol("official PDF", "semantic reply index is invalid"));
                    }
                    if !tokens.is_empty() {
                        blocks[original_index].metadata.insert(
                            "_table_image_token_map".into(),
                            serde_json::Value::Object(tokens),
                        );
                    }
                    if bytes > max_encoded_request_bytes {
                        // A single block that encodes over the per-request cap degrades to empty
                        // content for this block only; sibling blocks and the page continue
                        // (mirrors the classic Protocol arm).
                        warnings.push(cap_warning(
                            "semantic",
                            bytes,
                            max_encoded_request_bytes,
                            "per-request",
                            "empty content",
                        ));
                        replies[order] = Some((original_index, String::new()));
                        continue;
                    }
                    // Overflow in the resident-bytes ledger is an internal invariant break, never
                    // degrade: keep it fatal (the tolerance arms only ever see LLM malformation).
                    let resident_after =
                        resident_bytes
                            .checked_add(bytes)
                            .ok_or(VlmError::LimitExceeded {
                                resource: "encoded batch bytes",
                                limit: max_encoded_batch_bytes as u64,
                                actual: u64::MAX,
                            })?;
                    if resident_after > max_encoded_batch_bytes {
                        warnings.push(cap_warning(
                            "semantic",
                            bytes,
                            max_encoded_batch_bytes,
                            "batch",
                            "empty content",
                        ));
                        replies[order] = Some((original_index, String::new()));
                        continue;
                    }
                    resident_bytes = resident_after;
                    // The document-level encoded budget and the returned encoded-bytes accounting
                    // count only the retained candidates: the bytes of per-request- or
                    // batch-degraded candidates never reach the server, so charging them could
                    // only push the document over its budget and fail the whole page — the
                    // failure mode classic avoids by never encoding admission-gated candidates.
                    // (Classic still charges per-request-degraded candidates as a historical
                    // quirk; two-phase is deliberately stricter and counts only what is actually
                    // requested.)
                    encoded_budget.charge(bytes as u64, "encoded document bytes")?;
                    encoded_bytes = encoded_bytes.saturating_add(bytes);
                    // Document-wide resident-encoded-bytes ledger created by the window function.
                    // It is a defensive invariant sentinel, not a binding control: per-page
                    // retained bytes stay <= the batch cap and every page in the window charges
                    // once, so the cumulative bound is batch x window pages and it can only trip
                    // on broken accounting — when it does, it fails the document exactly like the
                    // encoded budget.
                    resident_encoded_budget.charge(bytes as u64, "encoded resident bytes")?;
                    retained.push((order, image, prompt, sampling, original_index));
                }
                chunk_start = chunk_end;
            }
        }
        type TwoPhaseRequest = Pin<
            Box<
                dyn Future<Output = VlmResult<(usize, usize, String, usize, usize, Vec<String>)>>
                    + Send,
            >,
        >;
        let mut requests: FuturesUnordered<TwoPhaseRequest> = FuturesUnordered::new();
        let backend = self.backend.clone();
        let semaphore = self.layout_semaphore.clone();
        // Per-page request admission: `max_requests_per_batch` bounds how many of this page's
        // semantic requests may be in flight at once (the `--batch-size` knob's two-phase
        // semantics), nested under the global request-level `layout_semaphore`. Zero is rejected
        // by the page core before we get here; `.max(1)` guards the semaphore constructor.
        let batch_semaphore = Arc::new(Semaphore::new(max_requests_per_batch.max(1)));
        let max_response_bytes = self.image_config.max_response_bytes;
        for (order, image, prompt, sampling, original_index) in retained {
            let bytes = image.data.len();
            let request = self.request(
                VlmImageInput::Bytes {
                    data: image.data,
                    media_type: Some(image.media_type),
                },
                prompt,
                sampling,
                None,
            );
            let backend = backend.clone();
            let semaphore = semaphore.clone();
            let batch_semaphore = batch_semaphore.clone();
            let raw_budget = raw_budget.clone();
            requests.push(Box::pin(async move {
                let _batch_permit =
                    batch_semaphore
                        .acquire_owned()
                        .await
                        .map_err(|_| VlmError::Transport {
                            operation: "official PDF",
                            message: "semantic batch semaphore closed".into(),
                        })?;
                let _permit = semaphore
                    .acquire_owned()
                    .await
                    .map_err(|_| VlmError::Transport {
                        operation: "official PDF",
                        message: "layout semaphore closed".into(),
                    })?;
                match backend
                    .predict_official_budgeted(
                        request,
                        max_response_bytes,
                        Some(raw_budget),
                        tokio::time::Instant::from_std(deadline),
                    )
                    .await
                {
                    // Only parse-class (Protocol) malformation of the LLM reply degrades to empty
                    // content for this block. Resource/configuration failures and service
                    // failures (Http/Transport/Timeout) stay fatal so a wrong key or dead server
                    // is never masked as an empty successful document — same as classic.
                    Ok((reply, raw_bytes, warnings)) => {
                        Ok((order, original_index, reply, raw_bytes, bytes, warnings))
                    }
                    Err(error @ VlmError::Protocol { .. }) => Ok((
                        order,
                        original_index,
                        String::new(),
                        0,
                        bytes,
                        vec![format!(
                            "semantic request failed: {error}; continuing with empty content"
                        )],
                    )),
                    Err(error) => Err(error),
                }
            }));
        }
        // Phase 2, request-all: every retained candidate's HTTP request runs concurrently under
        // the request-level semaphore; replies are backfilled by order.
        while !requests.is_empty() {
            let (order, block_index, reply, bytes, lease, request_warnings) =
                requests.next().await.expect("nonempty request queue")?;
            warnings.extend(request_warnings);
            resident_bytes = resident_bytes
                .checked_sub(lease)
                .expect("request lease held");
            if order >= replies.len() || block_index >= block_count {
                return Err(protocol("official PDF", "semantic reply index is invalid"));
            }
            raw_bytes = raw_bytes.saturating_add(bytes);
            replies[order] = Some((block_index, reply));
            #[cfg(test)]
            if let Some(hook) = &self.semantic_scheduler_hook {
                (hook.completed)(order);
                (hook.state)(resident_bytes, 0, requests.len());
            }
        }
        self.complete_official_semantic_page(
            blocks,
            replies,
            warnings,
            raw_bytes,
            encoded_bytes,
            deadline,
        )
        .await
    }

    #[cfg(test)]
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
    ) -> VlmResult<
        Vec<(
            Vec<ModelBlock>,
            Vec<VlmLayoutBlock>,
            usize,
            usize,
            Vec<String>,
        )>,
    > {
        self.official_two_step_snapshot_window_with_budgets(
            images,
            image_analysis,
            formula_enable,
            table_enable,
            max_layout_blocks,
            max_semantic_requests,
            max_requests_per_batch,
            max_encoded_request_bytes,
            max_encoded_batch_bytes,
            Arc::new(ByteBudget::new(remaining_encoded_document_bytes as u64)),
            Arc::new(ByteBudget::new(remaining_raw_reply_bytes as u64)),
            deadline,
        )
        .await
    }

    #[cfg(test)]
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn official_two_step_snapshot_window_with_budgets(
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
        encoded_budget: Arc<ByteBudget>,
        raw_budget: Arc<ByteBudget>,
        deadline: Instant,
    ) -> VlmResult<
        Vec<(
            Vec<ModelBlock>,
            Vec<VlmLayoutBlock>,
            usize,
            usize,
            Vec<String>,
        )>,
    > {
        self.official_two_step_snapshot_window_with_budgets_and_page_semaphore(
            images,
            image_analysis,
            formula_enable,
            table_enable,
            max_layout_blocks,
            max_semantic_requests,
            max_requests_per_batch,
            max_encoded_request_bytes,
            max_encoded_batch_bytes,
            encoded_budget,
            raw_budget,
            deadline,
            self.official_page_semaphore.clone(),
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn official_two_step_snapshot_window_with_budgets_and_page_semaphore(
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
        encoded_budget: Arc<ByteBudget>,
        raw_budget: Arc<ByteBudget>,
        deadline: Instant,
        page_semaphore: Arc<Semaphore>,
    ) -> VlmResult<
        Vec<(
            Vec<ModelBlock>,
            Vec<VlmLayoutBlock>,
            usize,
            usize,
            Vec<String>,
        )>,
    > {
        // Document-wide resident-encoded-bytes ledger for the two-phase model: a defensive
        // invariant sentinel, not a binding control. Each page's encode-all stage holds up to
        // `max_encoded_batch_bytes` of encoded images while its request-all stage drains, every
        // page in this window charges the ledger once, so the cumulative bound is the batch cap
        // times the window's page count and it can only trip on broken accounting. Classic pages
        // receive the same Arc but never charge it, so the classic path is unchanged.
        let image_count = images.len().max(1);
        let resident_encoded_budget = Arc::new(ByteBudget::new(
            (max_encoded_batch_bytes as u64).saturating_mul(image_count as u64),
        ));
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
                raw_budget.clone(),
                encoded_budget.clone(),
                resident_encoded_budget.clone(),
                deadline,
                page_semaphore.clone(),
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
        // The tokio semaphore capacity is a legitimate representability bound. An impossible
        // public `max_concurrency` (zero, or above capacity) fails instead of being silently
        // min-clamped or dead-locking on a zero-permit semaphore.
        if http.max_concurrency == 0 || http.max_concurrency > Semaphore::MAX_PERMITS {
            return Err(VlmError::InvalidConfig(
                "max_concurrency must be greater than zero and at most the tokio semaphore capacity"
                    .into(),
            ));
        }
        let official_page_semaphore = Arc::new(Semaphore::new(OFFICIAL_PAGE_CONCURRENCY));
        let layout_semaphore = Arc::new(Semaphore::new(http.max_concurrency));
        let max_decoded_pixels = http.max_decoded_pixels;
        let concurrency_model = config.concurrency_model;
        // Both pipelines serialize CPU-bound encoding behind a global semaphore sized to the
        // machine's real parallelism: classic acquires one permit per encode, two-phase holds
        // one permit per in-flight encoder in its encode-all stage.
        let encode_cpu_semaphore = Arc::new(Semaphore::new(
            std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(4),
        ));
        let image_config = Arc::new(http.clone());
        Ok(Self {
            backend: VlmBackend::Http(
                VlmHttpClient::connect_for_task(http, task_work_lease.clone()).await?,
            ),
            image_config,
            max_decoded_pixels,
            task_work_lease,
            preprocessor: MinerUVlmPreprocessor { config },
            layout_semaphore,
            official_page_semaphore,
            concurrency_model,
            encode_cpu_semaphore,
            encode_cpu_parallelism,
            #[cfg(test)]
            semantic_scheduler_hook: None,
        })
    }
    #[cfg(test)]
    fn set_semantic_scheduler_hook(&mut self, hook: SemanticSchedulerHook) {
        self.semantic_scheduler_hook = Some(hook);
    }
    #[cfg(test)]
    fn set_encode_cpu_parallelism(&mut self, parallelism: usize) {
        self.encode_cpu_parallelism = parallelism.max(1);
    }
    pub(crate) fn task_work_lease(&self) -> TaskWorkLease {
        self.task_work_lease.clone()
    }
    pub(crate) fn official_response_cap(&self) -> usize {
        self.image_config.max_response_bytes
    }
    pub(crate) fn official_page_concurrency(
        &self,
    ) -> crate::official_route::OfficialPageConcurrency {
        crate::official_route::OfficialPageConcurrency::from_semaphore(
            self.official_page_semaphore.clone(),
        )
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
    async fn decode_local_image(&self, image: VlmImageInput) -> VlmResult<Option<DynamicImage>> {
        crate::vlm_image::decode_local_for_task(
            image,
            self.image_config.clone(),
            &self.task_work_lease,
        )
        .await
    }
    async fn decode_admitted_image(&self, image: VlmImageInput) -> VlmResult<Option<DynamicImage>> {
        self.decode_local_image(image).await
    }
    async fn admit_semantic_image(&self, image: VlmImageInput) -> VlmResult<VlmImageInput> {
        if matches!(image, VlmImageInput::RemoteUrl(_)) {
            return Err(VlmError::InvalidImageInput(
                "semantic operations require a local image".into(),
            ));
        }
        Ok(crate::vlm_image::admit_local_for_task(
            image,
            self.image_config.clone(),
            &self.task_work_lease,
        )
        .await?
        .map(|(data, media_type)| VlmImageInput::Bytes {
            data,
            media_type: Some(media_type),
        })
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
        let image = if let Some(image) = self.decode_admitted_image(image).await? {
            let preprocessor = self.preprocessor.clone();
            let max_pixels = self.max_decoded_pixels;
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
            .backend
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
        if let Some(decoded) = self.decode_admitted_image(image).await? {
            let preprocessor = self.preprocessor.clone();
            let prompts = not_extract_list.to_vec();
            let max_pixels = self.max_decoded_pixels;
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
            let responses = self.backend.aio_batch_predict(requests, semaphore).await?;
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
            "semantic operations require a local image".into(),
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
        let image = self.decode_local_image(i).await?.ok_or_else(|| {
            VlmError::InvalidImageInput("semantic operations require a local image".into())
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
        let max_pixels = self.max_decoded_pixels;
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
            .backend
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
        check_lengths(
            images.len(),
            prompts.len(),
            "content",
            "image/prompt length mismatch",
        )?;
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
        check_lengths(i.len(), q.len(), "content", "image/prompt length mismatch")?;
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
            let image = self.decode_admitted_image(image).await?.ok_or_else(|| {
                VlmError::InvalidImageInput("semantic operations require a local image".into())
            })?;
            let preprocessor = self.preprocessor.clone();
            let page_blocks = std::mem::take(blocks);
            let prompts = prompts.to_vec();
            let max_pixels = self.max_decoded_pixels;
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
        let responses = self.backend.aio_batch_predict(requests, semaphore).await?;
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
        check_lengths(
            images.len(),
            blocks.len(),
            "extract",
            "image/layout length mismatch",
        )?;
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
        mpsc as std_mpsc,
    };
    use tokio::{
        net::TcpListener,
        sync::{Barrier, Mutex, Notify, mpsc},
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
        third_layout: Arc<Notify>,
        active: Arc<AtomicUsize>,
        peak: Arc<AtomicUsize>,
        layouts: Arc<AtomicUsize>,
        admission_limit: usize,
        ignored: usize,
    }

    async fn window_client(state: WindowState) -> MinerUVlmClient {
        window_client_with_concurrency(state, 2).await
    }

    async fn window_client_with_layout(state: WindowState, layout: (u32, u32)) -> MinerUVlmClient {
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
            MinerUVlmConfig {
                layout_image_size: layout,
                ..Default::default()
            },
        )
        .await
        .unwrap()
    }

    async fn window_client_with_concurrency(
        state: WindowState,
        max_concurrency: usize,
    ) -> MinerUVlmClient {
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
                max_concurrency,
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
            if admission < state.admission_limit {
                let released = state.release.notified();
                tokio::pin!(released);
                let _ = released.as_mut().enable();
                state.first_two.wait().await;
                released.await;
            } else if admission == state.admission_limit {
                state.third_layout.notify_one();
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
            third_layout: Arc::new(Notify::new()),
            active: Arc::new(AtomicUsize::new(0)),
            peak: Arc::new(AtomicUsize::new(0)),
            layouts: Arc::new(AtomicUsize::new(0)),
            admission_limit: 2,
            ignored: 0,
        }
    }

    fn window_state_with_admission_limit(limit: usize) -> WindowState {
        WindowState {
            first_two: Arc::new(Barrier::new(limit + 1)),
            release: Arc::new(Notify::new()),
            third_layout: Arc::new(Notify::new()),
            active: Arc::new(AtomicUsize::new(0)),
            peak: Arc::new(AtomicUsize::new(0)),
            layouts: Arc::new(AtomicUsize::new(0)),
            admission_limit: limit,
            ignored: 0,
        }
    }

    #[derive(Clone)]
    struct FailWindowState {
        entered: Arc<Barrier>,
        pending: Arc<Notify>,
        later: Arc<Notify>,
        failures: Arc<AtomicUsize>,
    }

    async fn fail_window_client(state: FailWindowState) -> MinerUVlmClient {
        fail_window_client_with_concurrency(state, 2).await
    }

    async fn fail_window_client_with_concurrency(
        state: FailWindowState,
        max_concurrency: usize,
    ) -> MinerUVlmClient {
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
                max_concurrency,
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
        let admission = state.failures.fetch_add(1, Ordering::SeqCst);
        if admission < 2 {
            state.entered.wait().await;
            if admission == 0 {
                return StatusCode::INTERNAL_SERVER_ERROR.into_response();
            }
            state.pending.notified().await;
        } else {
            state.later.notify_one();
        }
        Json(json!({"choices":[{"finish_reason":"stop","message":{"content":""}}]})).into_response()
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

    #[derive(Clone)]
    struct PipelineState {
        arrivals: mpsc::UnboundedSender<usize>,
        semantic: Arc<AtomicUsize>,
        release: Vec<Arc<Notify>>,
        hold: Vec<bool>,
        fail: Option<usize>,
        active: Arc<AtomicUsize>,
        peak: Arc<AtomicUsize>,
    }

    async fn pipeline_chat(
        State(state): State<PipelineState>,
        Json(request): Json<serde_json::Value>,
    ) -> axum::response::Response {
        if request.to_string().contains("Layout Detection") {
            return Json(json!({"choices":[{"finish_reason":"stop","message":{"content":"<|box_start|>0 0 300 1000<|box_end|><|ref_start|>text<|ref_end|><|rotate_up|><|box_start|>300 0 600 1000<|box_end|><|ref_start|>text<|ref_end|><|rotate_up|><|box_start|>600 0 1000 1000<|box_end|><|ref_start|>text<|ref_end|><|rotate_up|>"}}]})).into_response();
        }
        let index = state.semantic.fetch_add(1, Ordering::SeqCst);
        let active = state.active.fetch_add(1, Ordering::SeqCst) + 1;
        state.peak.fetch_max(active, Ordering::SeqCst);
        let released = state.release[index].notified();
        tokio::pin!(released);
        let _ = released.as_mut().enable();
        let _ = state.arrivals.send(index);
        if state.hold[index] {
            released.await;
        }
        state.active.fetch_sub(1, Ordering::SeqCst);
        if state.fail == Some(index) {
            // Parse-class malformation (invalid JSON in a 200 reply) degrades to a warning on
            // the official path; a 5xx service failure would stay an error and abort instead.
            ([("content-type", "application/json")], "not-json-body").into_response()
        } else {
            // Key the reply on the candidate crop's top-left pixel (like order_chat), so the
            // content is bound to the candidate, not to the arrival order at this mock.
            let data_url = request["messages"][1]["content"][0]["image_url"]["url"]
                .as_str()
                .unwrap();
            let bytes = STANDARD
                .decode(data_url.rsplit(',').next().unwrap())
                .unwrap();
            let page = image::load_from_memory(&bytes).unwrap().to_rgb8();
            let pixel = page.get_pixel(0, 0)[0];
            Json(json!({"choices":[{"finish_reason":"stop","message":{"content":format!("reply-{pixel}")}}]})).into_response()
        }
    }

    async fn pipeline_client(
        hold: &[usize],
        fail: Option<usize>,
    ) -> (
        MinerUVlmClient,
        PipelineState,
        mpsc::UnboundedReceiver<usize>,
    ) {
        pipeline_client_with_model(hold, fail, ConcurrencyModel::Classic).await
    }

    async fn pipeline_client_with_model(
        hold: &[usize],
        fail: Option<usize>,
        concurrency_model: ConcurrencyModel,
    ) -> (
        MinerUVlmClient,
        PipelineState,
        mpsc::UnboundedReceiver<usize>,
    ) {
        let (arrivals, receiver) = mpsc::unbounded_channel();
        let state = PipelineState {
            arrivals,
            semantic: Arc::new(AtomicUsize::new(0)),
            release: (0..3).map(|_| Arc::new(Notify::new())).collect(),
            hold: (0..3).map(|index| hold.contains(&index)).collect(),
            fail,
            active: Arc::new(AtomicUsize::new(0)),
            peak: Arc::new(AtomicUsize::new(0)),
        };
        let app = Router::new()
            .route("/v1/chat/completions", post(pipeline_chat))
            .with_state(state.clone());
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let client = MinerUVlmClient::connect(
            VlmHttpConfig {
                server_url: Some(format!("http://{address}").parse().unwrap()),
                model_name: Some("mock".into()),
                skip_model_name_checking: true,
                max_retries: 0,
                max_concurrency: 2,
                ..Default::default()
            },
            MinerUVlmConfig {
                concurrency_model,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        (client, state, receiver)
    }

    async fn pipeline_page(
        client: MinerUVlmClient,
        count: usize,
        batch_bytes: usize,
    ) -> VlmResult<(
        Vec<ModelBlock>,
        Vec<VlmLayoutBlock>,
        usize,
        usize,
        Vec<String>,
    )> {
        pipeline_page_capped(client, 3, count, batch_bytes).await
    }

    async fn pipeline_page_capped(
        client: MinerUVlmClient,
        semantic_cap: usize,
        count: usize,
        batch_bytes: usize,
    ) -> VlmResult<(
        Vec<ModelBlock>,
        Vec<VlmLayoutBlock>,
        usize,
        usize,
        Vec<String>,
    )> {
        client
            .official_two_step_snapshot_window(
                vec![gradient_page()],
                false,
                true,
                true,
                8,
                semantic_cap,
                count,
                1 << 20,
                batch_bytes,
                1 << 20,
                1 << 20,
                Instant::now() + Duration::from_secs(10),
            )
            .await
            .map(|mut pages| pages.remove(0))
    }

    #[tokio::test]
    async fn semantic_pipeline_http_completes_while_encoder_owns_blocks() {
        let (mut client, state, mut arrivals) = pipeline_client(&[0], None).await;
        let (started_tx, mut started_rx) = mpsc::unbounded_channel();
        let (completed_tx, mut completed_rx) = mpsc::unbounded_channel();
        let (release_encoder_tx, release_encoder_rx) = std_mpsc::channel();
        let release_encoder_rx = Arc::new(std::sync::Mutex::new(Some(release_encoder_rx)));
        client.set_semantic_scheduler_hook(SemanticSchedulerHook {
            before_encode: Arc::new(move |order| {
                if order == 1 {
                    let _ = started_tx.send(order);
                    release_encoder_rx
                        .lock()
                        .unwrap()
                        .take()
                        .unwrap()
                        .recv()
                        .unwrap();
                }
            }),
            after_encode: Arc::new(|_| {}),
            state: Arc::new(|_, _, _| {}),
            completed: Arc::new(move |order| {
                let _ = completed_tx.send(order);
            }),
        });
        let task = tokio::spawn(pipeline_page(client, 2, 2 << 20));
        assert_eq!(
            timeout(Duration::from_secs(2), arrivals.recv())
                .await
                .unwrap(),
            Some(0)
        );
        assert_eq!(
            timeout(Duration::from_secs(2), started_rx.recv())
                .await
                .unwrap(),
            Some(1)
        );
        state.release[0].notify_one();
        assert_eq!(
            timeout(Duration::from_secs(2), completed_rx.recv())
                .await
                .unwrap(),
            Some(0)
        );
        release_encoder_tx.send(()).unwrap();
        let result = timeout(Duration::from_secs(2), task)
            .await
            .unwrap()
            .unwrap();
        assert!(result.is_ok(), "{result:?}");
    }

    #[tokio::test]
    async fn semantic_pipeline_replenishes_before_slow_sibling() {
        let (mut client, state, mut arrivals) = pipeline_client(&[0], None).await;
        let (started_tx, mut started_rx) = mpsc::unbounded_channel();
        let (release_encoder_tx, release_encoder_rx) = std_mpsc::channel();
        let release_encoder_rx = Arc::new(std::sync::Mutex::new(Some(release_encoder_rx)));
        client.set_semantic_scheduler_hook(SemanticSchedulerHook {
            before_encode: Arc::new(move |order| {
                if order == 1 {
                    let _ = started_tx.send(order);
                    release_encoder_rx
                        .lock()
                        .unwrap()
                        .take()
                        .unwrap()
                        .recv()
                        .unwrap();
                }
            }),
            after_encode: Arc::new(|_| {}),
            state: Arc::new(|_, _, _| {}),
            completed: Arc::new(|_| {}),
        });
        let task = tokio::spawn(pipeline_page(client, 2, 2 << 20));
        assert_eq!(
            timeout(Duration::from_secs(2), arrivals.recv())
                .await
                .unwrap(),
            Some(0)
        );
        assert_eq!(
            timeout(Duration::from_secs(2), started_rx.recv())
                .await
                .unwrap(),
            Some(1)
        );
        release_encoder_tx.send(()).unwrap();
        assert_eq!(
            timeout(Duration::from_secs(2), arrivals.recv())
                .await
                .unwrap(),
            Some(1)
        );
        assert_eq!(
            timeout(Duration::from_secs(2), arrivals.recv())
                .await
                .unwrap(),
            Some(2)
        );
        assert!(state.active.load(Ordering::SeqCst) >= 1);
        state.release[0].notify_one();
        assert!(
            timeout(Duration::from_secs(2), task)
                .await
                .unwrap()
                .unwrap()
                .is_ok()
        );
    }

    #[tokio::test]
    async fn semantic_pipeline_bounds_count_and_encoded_bytes() {
        let (mut client, state, mut arrivals) = pipeline_client(&[0, 1], None).await;
        let observations = Arc::new(std::sync::Mutex::new(vec![]));
        let observed = observations.clone();
        client.set_semantic_scheduler_hook(SemanticSchedulerHook {
            before_encode: Arc::new(|_| {}),
            after_encode: Arc::new(|_| {}),
            state: Arc::new(move |resident, reservation, requests| {
                observed
                    .lock()
                    .unwrap()
                    .push((resident, reservation, requests));
            }),
            completed: Arc::new(|_| {}),
        });
        let batch_cap = 2 << 20;
        let task = tokio::spawn(pipeline_page(client, 2, batch_cap));
        assert_eq!(
            timeout(Duration::from_secs(5), arrivals.recv())
                .await
                .unwrap(),
            Some(0)
        );
        assert_eq!(
            timeout(Duration::from_secs(5), arrivals.recv())
                .await
                .unwrap(),
            Some(1)
        );
        state.release[1].notify_one();
        assert_eq!(
            timeout(Duration::from_secs(5), arrivals.recv())
                .await
                .unwrap(),
            Some(2)
        );
        state.release[0].notify_one();
        assert!(
            timeout(Duration::from_secs(5), task)
                .await
                .unwrap()
                .unwrap()
                .is_ok()
        );
        let observations = observations.lock().unwrap();
        assert!(
            observations
                .iter()
                .all(
                    |(resident, reservation, requests)| resident.saturating_add(*reservation)
                        <= batch_cap
                        && *requests <= 2
                )
        );
        assert!(state.peak.load(Ordering::SeqCst) <= 2);
    }

    #[tokio::test]
    async fn semantic_pipeline_preserves_order() {
        let (mut client, state, mut arrivals) = pipeline_client(&[0], None).await;
        let (started_tx, mut started_rx) = mpsc::unbounded_channel();
        let (release_encoder_tx, release_encoder_rx) = std_mpsc::channel();
        let release_encoder_rx = Arc::new(std::sync::Mutex::new(Some(release_encoder_rx)));
        client.set_semantic_scheduler_hook(SemanticSchedulerHook {
            before_encode: Arc::new(move |order| {
                if order == 1 {
                    let _ = started_tx.send(order);
                    release_encoder_rx
                        .lock()
                        .unwrap()
                        .take()
                        .unwrap()
                        .recv()
                        .unwrap();
                }
            }),
            after_encode: Arc::new(|_| {}),
            state: Arc::new(|_, _, _| {}),
            completed: Arc::new(|_| {}),
        });
        let task = tokio::spawn(pipeline_page(client, 2, 2 << 20));
        assert_eq!(
            timeout(Duration::from_secs(2), arrivals.recv())
                .await
                .unwrap(),
            Some(0)
        );
        assert_eq!(
            timeout(Duration::from_secs(2), started_rx.recv())
                .await
                .unwrap(),
            Some(1)
        );
        release_encoder_tx.send(()).unwrap();
        assert_eq!(
            timeout(Duration::from_secs(2), arrivals.recv())
                .await
                .unwrap(),
            Some(1)
        );
        assert_eq!(
            timeout(Duration::from_secs(2), arrivals.recv())
                .await
                .unwrap(),
            Some(2)
        );
        state.release[0].notify_one();
        let (snapshot, _, _, _, _) = timeout(Duration::from_secs(2), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(
            snapshot
                .into_iter()
                .map(|block| block.content.unwrap())
                .collect::<Vec<_>>(),
            ["reply-0", "reply-9", "reply-19"]
        );
    }

    #[tokio::test]
    async fn semantic_pipeline_recovers_failed_requests_and_continues() {
        let (mut client, state, mut arrivals) = pipeline_client(&[0], Some(0)).await;
        let (started_tx, mut started_rx) = mpsc::unbounded_channel();
        let (release_encoder_tx, release_encoder_rx) = std_mpsc::channel();
        let release_encoder_rx = Arc::new(std::sync::Mutex::new(Some(release_encoder_rx)));
        let (encoded_tx, mut encoded_rx) = mpsc::unbounded_channel();
        client.set_semantic_scheduler_hook(SemanticSchedulerHook {
            before_encode: Arc::new(move |order| {
                let _ = started_tx.send(order);
                if order == 1 {
                    release_encoder_rx
                        .lock()
                        .unwrap()
                        .take()
                        .unwrap()
                        .recv()
                        .unwrap();
                }
            }),
            after_encode: Arc::new(move |order| {
                let _ = encoded_tx.send(order);
            }),
            state: Arc::new(|_, _, _| {}),
            completed: Arc::new(|_| {}),
        });
        let task = tokio::spawn(pipeline_page(client, 2, 2 << 20));
        assert_eq!(
            timeout(Duration::from_secs(2), arrivals.recv())
                .await
                .unwrap(),
            Some(0)
        );
        assert_eq!(
            timeout(Duration::from_secs(2), started_rx.recv())
                .await
                .unwrap(),
            Some(0)
        );
        assert_eq!(
            timeout(Duration::from_secs(2), started_rx.recv())
                .await
                .unwrap(),
            Some(1)
        );
        // Unblock the encoder before the failing reply arrives: the failed request must degrade
        // to a warning with empty content, not abort the pipeline.
        release_encoder_tx.send(()).unwrap();
        state.release[0].notify_one();
        let (snapshot, _, _, _, warnings) = timeout(Duration::from_secs(2), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(
            warnings
                .iter()
                .any(|w| w.contains("semantic request failed"))
        );
        assert_eq!(
            snapshot
                .into_iter()
                .map(|block| block.content.unwrap())
                .collect::<Vec<_>>(),
            ["", "reply-9", "reply-19"]
        );
        assert_eq!(
            timeout(Duration::from_secs(2), encoded_rx.recv())
                .await
                .unwrap(),
            Some(0)
        );
        assert_eq!(
            timeout(Duration::from_secs(2), encoded_rx.recv())
                .await
                .unwrap(),
            Some(1)
        );
        assert_eq!(state.active.load(Ordering::SeqCst), 0);
    }

    // Deterministic two-phase mock: semantic replies are keyed by the decoded crop's top-left
    // pixel, so reply content identifies the candidate regardless of request arrival order.
    #[derive(Clone)]
    struct OrderState {
        semantic: Arc<AtomicUsize>,
    }

    async fn order_chat(
        State(state): State<OrderState>,
        Json(request): Json<serde_json::Value>,
    ) -> Json<serde_json::Value> {
        let layout = request.to_string().contains("Layout Detection");
        let content = if layout {
            "<|box_start|>0 0 300 1000<|box_end|><|ref_start|>text<|ref_end|><|rotate_up|><|box_start|>300 0 600 1000<|box_end|><|ref_start|>text<|ref_end|><|rotate_up|><|box_start|>600 0 1000 1000<|box_end|><|ref_start|>text<|ref_end|><|rotate_up|>".into()
        } else {
            state.semantic.fetch_add(1, Ordering::SeqCst);
            let data_url = request["messages"][1]["content"][0]["image_url"]["url"]
                .as_str()
                .unwrap();
            let bytes = STANDARD
                .decode(data_url.rsplit(',').next().unwrap())
                .unwrap();
            let page = image::load_from_memory(&bytes).unwrap().to_rgb8();
            format!("reply-{}", page.get_pixel(0, 0)[0])
        };
        Json(json!({"choices":[{"finish_reason":"stop","message":{"content":content}}]}))
    }

    async fn order_client_with_model(
        concurrency_model: ConcurrencyModel,
    ) -> (MinerUVlmClient, Arc<AtomicUsize>) {
        let state = OrderState {
            semantic: Arc::new(AtomicUsize::new(0)),
        };
        let app = Router::new()
            .route("/v1/chat/completions", post(order_chat))
            .with_state(state.clone());
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let client = MinerUVlmClient::connect(
            VlmHttpConfig {
                server_url: Some(format!("http://{address}").parse().unwrap()),
                model_name: Some("mock".into()),
                skip_model_name_checking: true,
                max_retries: 0,
                max_concurrency: 2,
                ..Default::default()
            },
            MinerUVlmConfig {
                concurrency_model,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        (client, state.semantic)
    }

    fn gradient_page() -> Arc<RgbImage> {
        // Horizontal gradient: the three 300/1000-wide crops land on x = 0, 9, 19, so every
        // candidate's top-left pixel is distinct and rotation-independent (`rotate_up` is Deg0).
        Arc::new(RgbImage::from_fn(32, 32, |x, _| {
            image::Rgb([x as u8, 0, 0])
        }))
    }

    async fn order_page(
        client: MinerUVlmClient,
    ) -> VlmResult<(
        Vec<ModelBlock>,
        Vec<VlmLayoutBlock>,
        usize,
        usize,
        Vec<String>,
    )> {
        client
            .official_two_step_snapshot_window(
                vec![gradient_page()],
                false,
                true,
                true,
                8,
                3,
                3,
                1 << 20,
                1 << 20,
                1 << 20,
                1 << 20,
                Instant::now() + Duration::from_secs(10),
            )
            .await
            .map(|mut pages| pages.remove(0))
    }

    #[tokio::test]
    async fn two_phase_holds_all_requests_until_every_encode_finishes_and_backfills_by_order() {
        let (mut client, semantic) = order_client_with_model(ConcurrencyModel::TwoPhase).await;
        let (started_tx, mut started_rx) = mpsc::unbounded_channel();
        let (release_encoder_tx, release_encoder_rx) = std_mpsc::channel();
        let release_encoder_rx = Arc::new(std::sync::Mutex::new(Some(release_encoder_rx)));
        client.set_semantic_scheduler_hook(SemanticSchedulerHook {
            before_encode: Arc::new(move |order| {
                if order == 1 {
                    let _ = started_tx.send(order);
                    release_encoder_rx
                        .lock()
                        .unwrap()
                        .take()
                        .unwrap()
                        .recv()
                        .unwrap();
                }
            }),
            after_encode: Arc::new(|_| {}),
            state: Arc::new(|_, _, _| {}),
            completed: Arc::new(|_| {}),
        });
        let task = tokio::spawn(order_page(client));
        assert_eq!(
            timeout(Duration::from_secs(2), started_rx.recv())
                .await
                .unwrap(),
            Some(1)
        );
        // The two-phase pipeline sends no semantic request while any encode is still running
        // (the classic pipeline would already have sent candidate 0's request here).
        sleep(Duration::from_millis(200)).await;
        assert_eq!(semantic.load(Ordering::SeqCst), 0);
        release_encoder_tx.send(()).unwrap();
        let (snapshot, _, _, _, warnings) = timeout(Duration::from_secs(2), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(semantic.load(Ordering::SeqCst), 3);
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(
            snapshot
                .into_iter()
                .map(|block| block.content.unwrap())
                .collect::<Vec<_>>(),
            ["reply-0", "reply-9", "reply-19"]
        );
    }

    #[tokio::test]
    async fn two_phase_and_classic_produce_identical_snapshots() {
        let (classic, _) = order_client_with_model(ConcurrencyModel::Classic).await;
        let (two_phase, _) = order_client_with_model(ConcurrencyModel::TwoPhase).await;
        let classic = order_page(classic).await.unwrap();
        let two_phase = order_page(two_phase).await.unwrap();
        assert_eq!(
            classic
                .0
                .iter()
                .map(|block| (block.block_type.clone(), block.content.clone()))
                .collect::<Vec<_>>(),
            two_phase
                .0
                .iter()
                .map(|block| (block.block_type.clone(), block.content.clone()))
                .collect::<Vec<_>>()
        );
        assert_eq!(
            classic
                .1
                .iter()
                .map(|block| (block.block_type.clone(), block.content.clone()))
                .collect::<Vec<_>>(),
            two_phase
                .1
                .iter()
                .map(|block| (block.block_type.clone(), block.content.clone()))
                .collect::<Vec<_>>()
        );
        assert_eq!(classic.2, two_phase.2);
        assert_eq!(classic.3, two_phase.3);
        assert_eq!(classic.4, two_phase.4);
    }

    #[tokio::test]
    async fn two_phase_degrades_candidates_over_the_batch_cap_like_classic() {
        // The pipeline mock served by a tiny-layout client: layout bytes stay negligible while
        // the noisy full-size crops encode far more, so the batch cap is the binding constraint.
        let (arrivals, _receiver) = mpsc::unbounded_channel();
        let state = PipelineState {
            arrivals,
            semantic: Arc::new(AtomicUsize::new(0)),
            release: (0..16).map(|_| Arc::new(Notify::new())).collect(),
            hold: vec![false; 16],
            fail: None,
            active: Arc::new(AtomicUsize::new(0)),
            peak: Arc::new(AtomicUsize::new(0)),
        };
        let app = Router::new()
            .route("/v1/chat/completions", post(pipeline_chat))
            .with_state(state);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let client = MinerUVlmClient::connect(
            VlmHttpConfig {
                server_url: Some(format!("http://{address}").parse().unwrap()),
                model_name: Some("mock".into()),
                skip_model_name_checking: true,
                max_retries: 0,
                max_concurrency: 2,
                ..Default::default()
            },
            MinerUVlmConfig {
                concurrency_model: ConcurrencyModel::TwoPhase,
                layout_image_size: (11, 10),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let image = Arc::new(RgbImage::from_fn(600, 600, |x, y| {
            image::Rgb([
                ((x * 31 + y * 17) % 4) as u8,
                ((x * 17 + y * 31) % 4) as u8,
                ((x + y) % 4) as u8,
            ])
        }));
        // Deterministic mock crops let us derive each candidate's encoded size from probe runs,
        // then set a batch cap that admits exactly the first candidate.
        let page = |semantic_cap: usize, batch: usize| {
            let client = client.clone();
            let image = Arc::clone(&image);
            async move {
                client
                    .official_two_step_snapshot_window(
                        vec![image],
                        false,
                        true,
                        true,
                        8,
                        semantic_cap,
                        3,
                        batch,
                        batch,
                        1 << 20,
                        1 << 20,
                        Instant::now() + Duration::from_secs(10),
                    )
                    .await
                    .map(|mut pages| pages.remove(0))
            }
        };
        let layout_only = page(0, 1 << 20).await.unwrap();
        let one = page(1, 1 << 20).await.unwrap();
        let two = page(2, 1 << 20).await.unwrap();
        let first = one.3 - layout_only.3;
        let second = two.3 - one.3;
        // Both caps equal `first + second - 1`: the first candidate fits, the second pushes the
        // resident ledger over the batch cap and degrades to empty content, exactly like classic.
        let (snapshot, _, _, _, warnings) = page(2, first + second - 1).await.unwrap();
        // Only candidate 0's request is ever sent (its reply text is probe-count dependent, but
        // non-empty); candidate 1 degrades to empty content; the third layout block has no
        // candidate at the cap of two and stays content-less.
        assert!(
            snapshot[0]
                .content
                .as_deref()
                .is_some_and(|content| !content.is_empty())
        );
        assert_eq!(snapshot[1].content.as_deref(), Some(""));
        assert_eq!(snapshot[2].content.as_deref(), None);
        assert_eq!(
            warnings
                .iter()
                .filter(|w| w.contains("exceed the batch cap"))
                .count(),
            1,
            "{warnings:?}"
        );
    }

    #[tokio::test]
    async fn two_phase_batch_degraded_candidates_do_not_charge_document_encoded_budget() {
        // Same tiny-layout client as the degrade test: layout bytes stay negligible while the
        // noisy full-size crops encode far more.
        let (arrivals, _receiver) = mpsc::unbounded_channel();
        let state = PipelineState {
            arrivals,
            semantic: Arc::new(AtomicUsize::new(0)),
            release: (0..16).map(|_| Arc::new(Notify::new())).collect(),
            hold: vec![false; 16],
            fail: None,
            active: Arc::new(AtomicUsize::new(0)),
            peak: Arc::new(AtomicUsize::new(0)),
        };
        let app = Router::new()
            .route("/v1/chat/completions", post(pipeline_chat))
            .with_state(state);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let client = MinerUVlmClient::connect(
            VlmHttpConfig {
                server_url: Some(format!("http://{address}").parse().unwrap()),
                model_name: Some("mock".into()),
                skip_model_name_checking: true,
                max_retries: 0,
                max_concurrency: 8,
                ..Default::default()
            },
            MinerUVlmConfig {
                concurrency_model: ConcurrencyModel::TwoPhase,
                layout_image_size: (11, 10),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let image = Arc::new(RgbImage::from_fn(600, 600, |x, y| {
            image::Rgb([
                ((x * 31 + y * 17) % 4) as u8,
                ((x * 17 + y * 31) % 4) as u8,
                ((x + y) % 4) as u8,
            ])
        }));
        let run = |semantic_cap: usize, batch: usize, encoded_budget: u64| {
            let client = client.clone();
            let image = Arc::clone(&image);
            async move {
                client
                    .official_two_step_snapshot_window_with_budgets(
                        vec![image],
                        false,
                        true,
                        true,
                        8,
                        semantic_cap,
                        3,
                        batch,
                        batch,
                        Arc::new(ByteBudget::new(encoded_budget)),
                        Arc::new(ByteBudget::new(1 << 20)),
                        Instant::now() + Duration::from_secs(10),
                    )
                    .await
                    .map(|mut pages| pages.remove(0))
            }
        };
        let layout_only = run(0, 1 << 20, 1 << 20).await.unwrap();
        let one = run(1, 1 << 20, 1 << 20).await.unwrap();
        let two = run(2, 1 << 20, 1 << 20).await.unwrap();
        let layout = layout_only.3;
        let first = one.3 - layout_only.3;
        let second = two.3 - one.3;
        assert!(first > 0 && second > 0, "probe sizes must be nonzero");
        // A batch cap admitting exactly the first candidate batch-degrades the second. The
        // document encoded budget fits the retained bytes (layout + first) but not the bytes of
        // the encoded-then-degraded second candidate: degraded candidates never reach the
        // server, so they must not count against the document budget or fail the page.
        let batch = first + second - 1;
        let (snapshot, _, _, _, warnings) = run(2, batch, (layout + first) as u64)
            .await
            .expect("batch-degraded candidates must not fail the document encoded budget");
        assert!(
            snapshot[0]
                .content
                .as_deref()
                .is_some_and(|content| !content.is_empty())
        );
        assert_eq!(snapshot[1].content.as_deref(), Some(""));
        assert_eq!(
            warnings
                .iter()
                .filter(|w| w.contains("exceed the batch cap"))
                .count(),
            1,
            "{warnings:?}"
        );
        // The counter-case: retained candidates over the document budget still fail hard.
        let result = run(2, batch, (layout + first - 1) as u64).await;
        assert!(
            matches!(
                &result,
                Err(VlmError::LimitExceeded {
                    resource: "encoded document bytes",
                    ..
                })
            ),
            "{result:?}"
        );
    }

    #[tokio::test]
    async fn two_phase_document_resident_budget_fails_hard() {
        let (client, _state, _arrivals) =
            pipeline_client_with_model(&[], None, ConcurrencyModel::TwoPhase).await;
        let layout_only = pipeline_page_capped(client.clone(), 0, 3, 1 << 20)
            .await
            .unwrap();
        let one = pipeline_page_capped(client.clone(), 1, 3, 1 << 20)
            .await
            .unwrap();
        let first = one.3 - layout_only.3;
        // A document-resident budget that holds a single candidate: the second retained candidate
        // trips the ledger and fails the whole page — a hard error like the encoded budget.
        let result = client
            .official_two_step_snapshot_page_core(
                Arc::new(RgbImage::new(32, 32)),
                false,
                true,
                true,
                8,
                3,
                3,
                1 << 20,
                1 << 20,
                Arc::new(ByteBudget::new(1 << 20)),
                Arc::new(ByteBudget::new(1 << 20)),
                Arc::new(ByteBudget::new(first as u64)),
                Instant::now() + Duration::from_secs(10),
                client.official_page_semaphore.clone(),
            )
            .await;
        assert!(
            matches!(
                &result,
                Err(VlmError::LimitExceeded {
                    resource: "encoded resident bytes",
                    ..
                })
            ),
            "{result:?}"
        );
    }

    #[tokio::test]
    async fn two_phase_window_resident_budget_scales_with_window_pages_not_page_concurrency() {
        // A tiny-layout client keeps the layout image negligible so the per-page batch cap can be
        // tightened down to the retained candidate bytes without skipping the layout request.
        let (arrivals, _receiver) = mpsc::unbounded_channel();
        let state = PipelineState {
            arrivals,
            semantic: Arc::new(AtomicUsize::new(0)),
            release: (0..16).map(|_| Arc::new(Notify::new())).collect(),
            hold: vec![false; 16],
            fail: None,
            active: Arc::new(AtomicUsize::new(0)),
            peak: Arc::new(AtomicUsize::new(0)),
        };
        let app = Router::new()
            .route("/v1/chat/completions", post(pipeline_chat))
            .with_state(state);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let client = MinerUVlmClient::connect(
            VlmHttpConfig {
                server_url: Some(format!("http://{address}").parse().unwrap()),
                model_name: Some("mock".into()),
                skip_model_name_checking: true,
                max_retries: 0,
                max_concurrency: 8,
                ..Default::default()
            },
            MinerUVlmConfig {
                concurrency_model: ConcurrencyModel::TwoPhase,
                layout_image_size: (11, 10),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let image = Arc::new(RgbImage::from_fn(600, 600, |x, y| {
            image::Rgb([
                ((x * 31 + y * 17) % 4) as u8,
                ((x * 17 + y * 31) % 4) as u8,
                ((x + y) % 4) as u8,
            ])
        }));
        let probe = |semantic_cap: usize| {
            let client = client.clone();
            let image = Arc::clone(&image);
            async move {
                client
                    .official_two_step_snapshot_window(
                        vec![image],
                        false,
                        true,
                        true,
                        8,
                        semantic_cap,
                        3,
                        1 << 20,
                        1 << 20,
                        1 << 20,
                        1 << 20,
                        Instant::now() + Duration::from_secs(10),
                    )
                    .await
                    .map(|mut pages| pages.remove(0))
            }
        };
        let layout_only = probe(0).await.unwrap();
        let all = probe(3).await.unwrap();
        // Each page's retained encoded bytes; charged once per page against the window's
        // document-wide resident ledger.
        let page_charge = all.3 - layout_only.3;
        assert!(page_charge > 0, "probe pages must retain encoded bytes");
        assert!(
            layout_only.3 <= page_charge,
            "layout bytes ({}) must fit under the per-page batch cap so the layout request survives",
            layout_only.3
        );
        // A two-permit page semaphore with a four-page window: the resident cap must cover the
        // window's four cumulative page charges, not just the two pages running at once. A batch
        // cap of one page's charge leaves the old (permit-scaled) formula at 2x the window total.
        let permits = Arc::new(Semaphore::new(2));
        let result = client
            .official_two_step_snapshot_window_with_budgets_and_page_semaphore(
                (0..4).map(|_| Arc::clone(&image)).collect(),
                false,
                true,
                true,
                8,
                3,
                3,
                page_charge,
                page_charge,
                Arc::new(ByteBudget::new(1 << 22)),
                Arc::new(ByteBudget::new(1 << 22)),
                Instant::now() + Duration::from_secs(10),
                permits,
            )
            .await;
        let pages = result.expect("a four-page window must not trip the resident ledger");
        assert_eq!(pages.len(), 4);
        // The semantic phase really ran on every page (three blocks with content each).
        assert!(
            pages
                .iter()
                .all(|page| page.0.len() == 3 && page.0.iter().all(|block| block.content.is_some())),
            "{pages:?}"
        );
    }

    #[tokio::test]
    async fn two_phase_backfills_table_token_map_after_all_encodes() {
        let app = Router::new().route(
            "/v1/chat/completions",
            post(|Json(request): Json<serde_json::Value>| async move {
                let content = if request.to_string().contains("Layout Detection") {
                    "<|box_start|>0 0 1000 1000<|box_end|><|ref_start|>table<|ref_end|><|box_start|>100 100 200 200<|box_end|><|ref_start|>image<|ref_end|>"
                } else {
                    // [AAAA] is the token `mask_and_encode_table_image` inserts for the first
                    // absorbed image; the cleaner only resolves it when the map was backfilled.
                    "[AAAA]"
                };
                Json(json!({"choices":[{"finish_reason":"stop","message":{"content":content}}]}))
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
            MinerUVlmConfig {
                concurrency_model: ConcurrencyModel::TwoPhase,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let (_, cleaned, _, _, _) = client
            .official_two_step_snapshot_window(
                vec![Arc::new(RgbImage::new(32, 32))],
                false,
                true,
                true,
                8,
                2,
                2,
                1 << 20,
                1 << 20,
                1 << 20,
                1 << 20,
                Instant::now() + Duration::from_secs(5),
            )
            .await
            .unwrap()
            .pop()
            .unwrap();
        // post_process strips the map key itself after cleaning, so the map's effect is observed
        // through the resolved data URL in the table content.
        assert_eq!(cleaned[0].block_type, BlockKind::TABLE);
        assert!(
            cleaned[0]
                .content
                .as_deref()
                .is_some_and(|content| content.contains("data:image/jpeg")),
            "{:?}",
            cleaned[0].content
        );
    }

    #[tokio::test]
    async fn two_phase_limits_per_page_in_flight_semantic_requests_to_batch_cap() {
        // HTTP concurrency 8, batch cap 2, three retained candidates: without the per-page batch
        // semaphore all three requests would be dispatched at once; with it the third stays
        // parked until one of the first two completes.
        let (arrivals, mut receiver) = mpsc::unbounded_channel();
        let state = PipelineState {
            arrivals,
            semantic: Arc::new(AtomicUsize::new(0)),
            release: (0..3).map(|_| Arc::new(Notify::new())).collect(),
            hold: vec![true, true, true],
            fail: None,
            active: Arc::new(AtomicUsize::new(0)),
            peak: Arc::new(AtomicUsize::new(0)),
        };
        let app = Router::new()
            .route("/v1/chat/completions", post(pipeline_chat))
            .with_state(state.clone());
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let client = MinerUVlmClient::connect(
            VlmHttpConfig {
                server_url: Some(format!("http://{address}").parse().unwrap()),
                model_name: Some("mock".into()),
                skip_model_name_checking: true,
                max_retries: 0,
                max_concurrency: 8,
                ..Default::default()
            },
            MinerUVlmConfig {
                concurrency_model: ConcurrencyModel::TwoPhase,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let image = Arc::new(RgbImage::new(32, 32));
        let task = tokio::spawn({
            let client = client.clone();
            let image = Arc::clone(&image);
            async move {
                client
                    .official_two_step_snapshot_window(
                        vec![image],
                        false,
                        true,
                        true,
                        8,
                        3,
                        2,
                        1 << 20,
                        1 << 20,
                        1 << 20,
                        1 << 20,
                        Instant::now() + Duration::from_secs(10),
                    )
                    .await
                    .map(|mut pages| pages.remove(0))
            }
        });
        // The two batch permits are taken: the first two semantic requests arrive and park at
        // the server; the third stays parked on the per-page batch semaphore.
        assert_eq!(
            timeout(Duration::from_secs(2), receiver.recv())
                .await
                .unwrap(),
            Some(0)
        );
        assert_eq!(
            timeout(Duration::from_secs(2), receiver.recv())
                .await
                .unwrap(),
            Some(1)
        );
        assert!(
            timeout(Duration::from_millis(300), receiver.recv())
                .await
                .is_err(),
            "third semantic request dispatched while two are in flight"
        );
        assert!(state.peak.load(Ordering::SeqCst) <= 2);
        // Release the first request; the third acquires the freed batch permit and arrives.
        state.release[0].notify_waiters();
        assert_eq!(
            timeout(Duration::from_secs(2), receiver.recv())
                .await
                .unwrap(),
            Some(2)
        );
        state.release[1].notify_waiters();
        state.release[2].notify_waiters();
        timeout(Duration::from_secs(5), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(
            state.peak.load(Ordering::SeqCst) <= 2,
            "per-page batch cap of 2 exceeded"
        );
    }

    /// Tiny-layout two-phase client whose chunk size is fixed by the test. The tiny layout
    /// keeps layout bytes negligible so the per-page batch cap binds on candidate bytes only.
    async fn chunked_two_phase_client(chunk: usize) -> MinerUVlmClient {
        let (arrivals, _receiver) = mpsc::unbounded_channel();
        let state = PipelineState {
            arrivals,
            semantic: Arc::new(AtomicUsize::new(0)),
            release: (0..16).map(|_| Arc::new(Notify::new())).collect(),
            hold: vec![false; 16],
            fail: None,
            active: Arc::new(AtomicUsize::new(0)),
            peak: Arc::new(AtomicUsize::new(0)),
        };
        let app = Router::new()
            .route("/v1/chat/completions", post(pipeline_chat))
            .with_state(state);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let mut client = MinerUVlmClient::connect(
            VlmHttpConfig {
                server_url: Some(format!("http://{address}").parse().unwrap()),
                model_name: Some("mock".into()),
                skip_model_name_checking: true,
                max_retries: 0,
                max_concurrency: 8,
                ..Default::default()
            },
            MinerUVlmConfig {
                concurrency_model: ConcurrencyModel::TwoPhase,
                layout_image_size: (11, 10),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        client.set_encode_cpu_parallelism(chunk);
        client
    }

    #[tokio::test]
    async fn two_phase_chunked_encoding_holds_residency_at_the_batch_cap() {
        // Chunk size 1 with three candidates spans three chunks. A batch cap admitting only the
        // first candidate must still degrade the later chunks' candidates to empty content with
        // one batch-cap warning each — the chunked retain/degrade is the unchunked drain split
        // across chunk boundaries, so the resident ledger never accumulates past the cap.
        let client = chunked_two_phase_client(1).await;
        let image = Arc::new(RgbImage::from_fn(600, 600, |x, y| {
            image::Rgb([
                ((x * 31 + y * 17) % 4) as u8,
                ((x * 17 + y * 31) % 4) as u8,
                ((x + y) % 4) as u8,
            ])
        }));
        let page = |semantic_cap: usize, batch: usize| {
            let client = client.clone();
            let image = Arc::clone(&image);
            async move {
                client
                    .official_two_step_snapshot_window(
                        vec![image],
                        false,
                        true,
                        true,
                        8,
                        semantic_cap,
                        3,
                        batch,
                        batch,
                        1 << 20,
                        1 << 20,
                        Instant::now() + Duration::from_secs(10),
                    )
                    .await
                    .map(|mut pages| pages.remove(0))
            }
        };
        let layout_only = page(0, 1 << 20).await.unwrap();
        let one = page(1, 1 << 20).await.unwrap();
        let two = page(2, 1 << 20).await.unwrap();
        let three = page(3, 1 << 20).await.unwrap();
        let first = one.3 - layout_only.3;
        let second = two.3 - one.3;
        let third = three.3 - two.3;
        assert!(
            first > 0 && second > 0 && third > 0,
            "probe sizes must be nonzero"
        );
        // Batch cap admits exactly the first candidate (>= every single candidate's bytes, so
        // the per-request check passes) but not the first two combined: the second and third
        // candidates (later chunks) degrade to empty content via the batch cap, one
        // "exceed the batch cap" warning each.
        let (snapshot, _, _, _, warnings) = page(3, second.max(third)).await.unwrap();
        assert!(
            snapshot[0]
                .content
                .as_deref()
                .is_some_and(|content| !content.is_empty())
        );
        assert_eq!(snapshot[1].content.as_deref(), Some(""));
        assert_eq!(snapshot[2].content.as_deref(), Some(""));
        assert_eq!(
            warnings
                .iter()
                .filter(|w| w.contains("exceed the batch cap"))
                .count(),
            2,
            "{warnings:?}"
        );
    }

    #[tokio::test]
    async fn two_phase_chunk_boundary_accumulates_resident_bytes_identically() {
        // Same three candidates and a batch cap admitting all of them: chunk size 1 (three
        // chunks, resident_bytes carries across chunk boundaries) must yield results identical
        // to a single unchunked run (chunk size 3). This pins the cross-chunk resident_bytes
        // accumulation — no double-counting, no missed count, identical degradation.
        let run = |chunk: usize, semantic_cap: usize| {
            let client = chunked_two_phase_client(chunk);
            let image = Arc::new(RgbImage::from_fn(600, 600, |x, y| {
                image::Rgb([
                    ((x * 31 + y * 17) % 4) as u8,
                    ((x * 17 + y * 31) % 4) as u8,
                    ((x + y) % 4) as u8,
                ])
            }));
            async move {
                let client = client.await;
                client
                    .official_two_step_snapshot_window(
                        vec![image],
                        false,
                        true,
                        true,
                        8,
                        semantic_cap,
                        3,
                        1 << 20,
                        1 << 20,
                        1 << 20,
                        1 << 20,
                        Instant::now() + Duration::from_secs(10),
                    )
                    .await
                    .map(|mut pages| pages.remove(0))
            }
        };
        let chunked = run(1, 3).await.unwrap();
        let unchunked = run(3, 3).await.unwrap();
        assert_eq!(
            chunked
                .0
                .iter()
                .map(|block| (block.block_type.clone(), block.content.clone()))
                .collect::<Vec<_>>(),
            unchunked
                .0
                .iter()
                .map(|block| (block.block_type.clone(), block.content.clone()))
                .collect::<Vec<_>>()
        );
        assert_eq!(chunked.2, unchunked.2, "raw bytes must match");
        assert_eq!(chunked.3, unchunked.3, "encoded bytes must match");
        assert_eq!(chunked.4, unchunked.4, "warnings must match");
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
        let (snapshot, _) = official_snapshot_block(raw.clone()).unwrap();
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
        assert_eq!(official_snapshot_block(nontext).unwrap().0.merge_prev, None);
    }

    #[test]
    fn official_snapshot_missing_angle_defaults_to_unrotated() {
        let (snapshot, warning) = official_snapshot_block(block(BlockKind::TEXT)).unwrap();
        assert_eq!(snapshot.angle, Some(Rotation::Deg0));
        assert!(warning.is_some());
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
        let backend = client.backend.clone();
        let semaphore = client.layout_semaphore.clone();
        let task =
            tokio::spawn(async move { backend.aio_batch_predict(requests, Some(semaphore)).await });
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
        let (snapshot, _, raw, encoded, warnings) = pages.pop().unwrap();
        assert_eq!(requests.load(Ordering::SeqCst), 3);
        assert!(snapshot[1].content.is_none());
        assert!(raw > 0 && encoded > 0);
        assert!(warnings.is_empty());
        assert_eq!(Arc::strong_count(&image), 1);
    }

    #[tokio::test]
    async fn snapshot_windows_share_document_budgets() {
        let state = window_state();
        state.layouts.store(2, Ordering::SeqCst);
        let client = window_client(state).await;
        let image = Arc::new(RgbImage::new(8, 8));
        let deadline = Instant::now() + Duration::from_secs(5);
        let page_encoded = client
            .official_two_step_snapshot_window_with_budgets(
                vec![Arc::clone(&image)],
                false,
                true,
                true,
                4,
                2,
                2,
                1 << 20,
                1 << 20,
                Arc::new(ByteBudget::new(1 << 20)),
                Arc::new(ByteBudget::new(1 << 20)),
                deadline,
            )
            .await
            .unwrap()[0]
            .3 as u64;
        let encoded = Arc::new(ByteBudget::new(page_encoded * 2 - 1));
        let raw = Arc::new(ByteBudget::new(1 << 20));
        client
            .official_two_step_snapshot_window_with_budgets(
                vec![Arc::clone(&image)],
                false,
                true,
                true,
                4,
                2,
                2,
                1 << 20,
                1 << 20,
                Arc::clone(&encoded),
                Arc::clone(&raw),
                deadline,
            )
            .await
            .unwrap();
        let result = client
            .official_two_step_snapshot_window_with_budgets(
                vec![image],
                false,
                true,
                true,
                4,
                2,
                2,
                1 << 20,
                1 << 20,
                encoded,
                raw,
                deadline,
            )
            .await;
        assert!(
            matches!(
                &result,
                Err(VlmError::LimitExceeded {
                    resource: "encoded document bytes",
                    limit,
                    actual,
                }) if *limit == page_encoded * 2 - 1 && *actual == page_encoded * 2
            ),
            "{result:?}"
        );
    }

    #[tokio::test]
    async fn snapshot_window_caps_page_pipelines_and_keeps_page_order() {
        let state = window_state();
        let client = window_client_with_concurrency(state.clone(), 8).await;
        let owners = (0..5)
            .map(|n| Arc::new(RgbImage::from_pixel(8, 8, image::Rgb([n, 0, 0]))))
            .collect::<Vec<_>>();
        let pages = owners.iter().map(Arc::clone).collect::<Vec<_>>();
        let task = tokio::spawn({
            let client = client.clone();
            async move {
                client
                    .official_two_step_snapshot_window_with_budgets_and_page_semaphore(
                        pages,
                        false,
                        true,
                        true,
                        4,
                        2,
                        2,
                        1 << 20,
                        1 << 20,
                        Arc::new(ByteBudget::new(1 << 20)),
                        Arc::new(ByteBudget::new(1 << 20)),
                        Instant::now() + Duration::from_secs(10),
                        Arc::new(Semaphore::new(2)),
                    )
                    .await
            }
        });
        timeout(Duration::from_secs(5), state.first_two.wait())
            .await
            .unwrap();
        assert_eq!(state.peak.load(Ordering::SeqCst), 2);
        assert_eq!(state.layouts.load(Ordering::SeqCst), 2);
        assert!(
            timeout(Duration::from_millis(100), state.third_layout.notified())
                .await
                .is_err()
        );
        state.release.notify_waiters();
        let pages = timeout(Duration::from_secs(5), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(state.peak.load(Ordering::SeqCst), 2);
        assert_eq!(state.layouts.load(Ordering::SeqCst), 5);
        assert_eq!(
            pages
                .iter()
                .map(|page| page.0[0].bbox.unwrap().left)
                .collect::<Vec<_>>(),
            vec![0.1, 0.2, 0.3, 0.4, 0.5]
        );
        assert_eq!(pages[0].0[0].content.as_deref(), Some("  raw semantic  "));
        assert_eq!(pages[0].1[0].content.as_deref(), Some("raw semantic"));
        assert!(owners.iter().all(|image| Arc::strong_count(image) == 1));
    }

    #[tokio::test]
    async fn route_page_semaphore_honors_four_permits_and_bounds_the_next_page() {
        let state = window_state_with_admission_limit(4);
        let client = window_client_with_concurrency(state.clone(), 8).await;
        let pages = (0..5)
            .map(|n| Arc::new(RgbImage::from_pixel(8, 8, image::Rgb([n, 0, 0]))))
            .collect();
        let permits = Arc::new(Semaphore::new(4));
        let task = tokio::spawn({
            let client = client.clone();
            async move {
                client
                    .official_two_step_snapshot_window_with_budgets_and_page_semaphore(
                        pages,
                        false,
                        true,
                        true,
                        4,
                        2,
                        2,
                        1 << 20,
                        1 << 20,
                        Arc::new(ByteBudget::new(1 << 20)),
                        Arc::new(ByteBudget::new(1 << 20)),
                        Instant::now() + Duration::from_secs(10),
                        permits,
                    )
                    .await
            }
        });
        timeout(Duration::from_secs(5), state.first_two.wait())
            .await
            .unwrap();
        assert_eq!(state.layouts.load(Ordering::SeqCst), 4);
        assert!(
            timeout(Duration::from_millis(100), state.third_layout.notified())
                .await
                .is_err()
        );
        state.release.notify_waiters();
        timeout(Duration::from_secs(5), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(state.layouts.load(Ordering::SeqCst), 5);
    }

    #[tokio::test]
    async fn official_page_permit_acquisition_respects_deadline() {
        let state = mock_state();
        let client = mock_client(state.clone()).await;
        let permits = client.official_page_semaphore.available_permits();
        let held = client
            .official_page_semaphore
            .clone()
            .acquire_many_owned(permits as u32)
            .await
            .unwrap();
        assert_eq!(client.official_page_semaphore.available_permits(), 0);

        let result = timeout(
            Duration::from_secs(1),
            client.official_two_step_snapshot_page_core(
                Arc::new(RgbImage::new(8, 8)),
                false,
                true,
                true,
                4,
                2,
                2,
                1 << 20,
                1 << 20,
                Arc::new(ByteBudget::new(1 << 20)),
                Arc::new(ByteBudget::new(1 << 20)),
                Arc::new(ByteBudget::new(1 << 20)),
                Instant::now() + Duration::from_millis(20),
                client.official_page_semaphore.clone(),
            ),
        )
        .await
        .unwrap();
        assert!(matches!(
            result,
            Err(VlmError::Timeout {
                operation: "official PDF"
            })
        ));
        assert_eq!(state.requests.load(Ordering::SeqCst), 0);
        assert_eq!(client.official_page_semaphore.available_permits(), 0);
        drop(held);
        assert_eq!(client.official_page_semaphore.available_permits(), permits);
    }

    #[tokio::test]
    async fn snapshot_window_degrades_layout_bytes_over_encoded_request_cap() {
        let state = window_state();
        state.layouts.store(2, Ordering::SeqCst); // no admission hold in this degrade test
        let client = window_client_with_layout(state.clone(), (256, 256)).await;
        let image = Arc::new(RgbImage::new(256, 256));
        let layout_bytes = client
            .preprocessor
            .prepare_rgb_for_layout_capped(&image, u64::MAX)
            .unwrap()
            .image
            .data
            .len();
        let pages = client
            .official_two_step_snapshot_window(
                vec![image],
                false,
                true,
                true,
                4,
                2,
                2,
                layout_bytes - 1, // layout request alone exceeds the cap
                layout_bytes - 1,
                1 << 20,
                1 << 20,
                Instant::now() + Duration::from_secs(2),
            )
            .await
            .unwrap();
        let (snapshot, _, _, _, warnings) = &pages[0];
        assert!(snapshot.is_empty());
        assert!(
            warnings
                .iter()
                .any(|w| w.contains("continuing with an empty layout")),
            "{warnings:?}"
        );
    }

    #[tokio::test]
    async fn snapshot_window_degrades_layout_block_cap_overflow() {
        let state = window_state();
        state.layouts.store(2, Ordering::SeqCst);
        let client = window_client(state.clone()).await;
        let image = Arc::new(RgbImage::new(8, 8));
        let pages = client
            .official_two_step_snapshot_window(
                vec![image],
                false,
                true,
                true,
                0, // below the single block the mock layout returns
                2,
                2,
                1 << 20,
                1 << 20,
                1 << 20,
                1 << 20,
                Instant::now() + Duration::from_secs(2),
            )
            .await
            .unwrap();
        let (snapshot, _, _, _, warnings) = &pages[0];
        assert!(snapshot.is_empty());
        assert!(
            warnings
                .iter()
                .any(|w| w.contains("continuing with an empty layout")),
            "{warnings:?}"
        );
    }

    #[tokio::test]
    async fn snapshot_window_truncates_semantic_requests_over_cap() {
        let state = window_state();
        state.layouts.store(2, Ordering::SeqCst);
        let client = window_client(state.clone()).await;
        let image = Arc::new(RgbImage::new(8, 8));
        let pages = client
            .official_two_step_snapshot_window(
                vec![image],
                false,
                true,
                true,
                4,
                0, // drop all semantic candidates
                2,
                1 << 20,
                1 << 20,
                1 << 20,
                1 << 20,
                Instant::now() + Duration::from_secs(2),
            )
            .await
            .unwrap();
        let (snapshot, _, _, _, warnings) = &pages[0];
        assert_eq!(snapshot.len(), 1); // the layout block survives
        assert!(snapshot[0].content.is_none());
        assert!(
            warnings
                .iter()
                .any(|w| w.contains("semantic requests exceed the per-page cap")),
            "{warnings:?}"
        );
    }

    #[tokio::test]
    async fn snapshot_window_skips_semantic_request_encoded_over_cap() {
        let state = window_state();
        state.layouts.store(2, Ordering::SeqCst);
        // A tiny layout image keeps the layout bytes small while the semantic crop (taken from
        // the original, larger page image) encodes well above that size.
        let client = window_client_with_layout(state.clone(), (11, 10)).await;
        // Low-amplitude noise keeps the downscaled layout image (and thus the mock's bbox
        // anchor pixel) near black, while the full-size page crops encode much larger.
        let image = Arc::new(RgbImage::from_fn(600, 600, |x, y| {
            image::Rgb([
                ((x * 31 + y * 17) % 4) as u8,
                ((x * 17 + y * 31) % 4) as u8,
                ((x + y) % 4) as u8,
            ])
        }));
        let tiny_layout = client
            .preprocessor
            .prepare_rgb_for_layout_capped(&image, u64::MAX)
            .unwrap()
            .image
            .data
            .len();
        let pages = client
            .official_two_step_snapshot_window(
                vec![image],
                false,
                true,
                true,
                4,
                2,
                2,
                tiny_layout, // layout passes exactly; the semantic crop overflows it
                1 << 20,
                1 << 20,
                1 << 20,
                Instant::now() + Duration::from_secs(5),
            )
            .await
            .unwrap();
        let (snapshot, _, _, _, warnings) = &pages[0];
        assert_eq!(snapshot.len(), 1);
        assert_eq!(snapshot[0].content.as_deref(), Some(""));
        assert!(
            warnings
                .iter()
                .any(|w| w.contains("exceed the per-request cap")),
            "{warnings:?}"
        );
    }

    #[tokio::test]
    #[ignore = "full VLM snapshot-window resource integration e2e"]
    async fn snapshot_window_shares_encoded_budget_and_keeps_exact_local_caps() {
        let state = window_state();
        state.layouts.store(2, Ordering::SeqCst); // no admission hold in this limit test
        let client = window_client(state.clone()).await;
        let image = Arc::new(RgbImage::from_pixel(8, 8, image::Rgb([7, 0, 0])));
        let layout_bytes = client
            .preprocessor
            .prepare_rgb_for_layout_capped(&image, u64::MAX)
            .unwrap()
            .image
            .data
            .len();
        let pages = vec![Arc::clone(&image), Arc::clone(&image)];
        // 1. A page whose layout image alone exceeds the encoded-request cap degrades to an empty
        //    layout with a warning, never failing the document.
        let degraded = client
            .official_two_step_snapshot_window(
                pages.iter().map(Arc::clone).collect(),
                false,
                true,
                true,
                4,
                2,
                2,
                layout_bytes - 1,
                layout_bytes - 1,
                1 << 20,
                1 << 20,
                Instant::now() + Duration::from_secs(2),
            )
            .await
            .unwrap();
        let (snapshot, _, _, _, warnings) = &degraded[0];
        assert!(snapshot.is_empty());
        assert!(
            warnings
                .iter()
                .any(|w| w.contains("continuing with an empty layout")),
            "{warnings:?}"
        );
        // 2. An encoded semantic candidate exceeding the per-request cap degrades to empty content
        //    for that block with a warning. Low-amplitude noise keeps the layout bytes small while
        //    the full-size page crops encode above the per-request cap.
        let tiny = window_client_with_layout(state.clone(), (11, 10)).await;
        let noisy = Arc::new(RgbImage::from_fn(600, 600, |x, y| {
            image::Rgb([
                ((x * 31 + y * 17) % 4) as u8,
                ((x * 17 + y * 31) % 4) as u8,
                ((x + y) % 4) as u8,
            ])
        }));
        let tiny_layout = tiny
            .preprocessor
            .prepare_rgb_for_layout_capped(&noisy, u64::MAX)
            .unwrap()
            .image
            .data
            .len();
        let degraded = tiny
            .official_two_step_snapshot_window(
                vec![noisy],
                false,
                true,
                true,
                4,
                2,
                2,
                tiny_layout,
                1 << 20,
                1 << 20,
                1 << 20,
                Instant::now() + Duration::from_secs(2),
            )
            .await
            .unwrap();
        let (snapshot, _, _, _, warnings) = &degraded[0];
        assert_eq!(snapshot.len(), 1);
        assert_eq!(snapshot[0].content.as_deref(), Some(""));
        assert!(
            warnings
                .iter()
                .any(|w| w.contains("exceed the per-request cap")),
            "{warnings:?}"
        );
        // 3. A batch cap below the request cap is a configuration error (guard), never silently
        //    accepted: admission is gated on `batch - request` and candidate bytes never exceed
        //    the request cap, so the guard is the batch policy's only reachable rejection.
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
            Err(VlmError::InvalidConfig { .. })
        ));
        // 4. The shared encoded document budget is charged across the window.
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
    async fn failed_page_releases_pipeline_permit_for_later_page() {
        let state = FailWindowState {
            // Both initial handlers and the test cross together before one handler fails.
            entered: Arc::new(Barrier::new(3)),
            pending: Arc::new(Notify::new()),
            later: Arc::new(Notify::new()),
            failures: Arc::new(AtomicUsize::new(0)),
        };
        let client = fail_window_client_with_concurrency(state.clone(), 8).await;
        let owners = (0..3)
            .map(|n| Arc::new(RgbImage::from_pixel(8, 8, image::Rgb([n, 0, 0]))))
            .collect::<Vec<_>>();
        let pages = owners.iter().map(Arc::clone).collect::<Vec<_>>();
        let task = tokio::spawn(async move {
            let raw = Arc::new(ByteBudget::new(1 << 20));
            let encoded = Arc::new(ByteBudget::new(1 << 20));
            futures_util::future::join_all(pages.into_iter().map(|image| {
                client.official_two_step_snapshot_page_core(
                    image,
                    false,
                    true,
                    true,
                    4,
                    2,
                    2,
                    1 << 20,
                    1 << 20,
                    raw.clone(),
                    encoded.clone(),
                    Arc::new(ByteBudget::new(1 << 20)),
                    Instant::now() + Duration::from_secs(10),
                    client.official_page_semaphore.clone(),
                )
            }))
            .await
        });
        timeout(Duration::from_secs(5), state.entered.wait())
            .await
            .unwrap();
        timeout(Duration::from_secs(5), state.later.notified())
            .await
            .unwrap();
        state.pending.notify_one();
        let results = timeout(Duration::from_secs(5), task)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            results
                .iter()
                .filter(|result| matches!(result, Err(VlmError::Http { status: 500, .. })))
                .count(),
            1
        );
        assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 2);
        assert!(owners.iter().all(|image| Arc::strong_count(image) == 1));
    }

    #[tokio::test]
    async fn snapshot_window_drops_pending_sibling_after_http_failure() {
        let state = FailWindowState {
            // Both handlers and the test cross together before one handler fails.
            entered: Arc::new(Barrier::new(3)),
            pending: Arc::new(Notify::new()),
            later: Arc::new(Notify::new()),
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
                    Instant::now() + Duration::from_secs(10),
                )
                .await
        });
        timeout(Duration::from_secs(5), state.entered.wait())
            .await
            .unwrap();
        assert!(matches!(
            timeout(Duration::from_secs(5), task)
                .await
                .unwrap()
                .unwrap(),
            Err(VlmError::Http { status: 500, .. })
        ));
        assert!(owners.iter().all(|image| Arc::strong_count(image) == 1));
        state.pending.notify_one();
    }

    #[tokio::test]
    async fn official_semantic_batch_rejects_invalid_caps() {
        let client = mock_client(mock_state()).await;
        let image = Arc::new(RgbImage::new(32, 32));
        let deadline = Instant::now() + Duration::from_secs(2);
        let zero = client
            .official_two_step_snapshot_page_core(
                image.clone(),
                false,
                true,
                true,
                4,
                1,
                0,
                1,
                1,
                Arc::new(ByteBudget::new(1 << 20)),
                Arc::new(ByteBudget::new(1 << 20)),
                Arc::new(ByteBudget::new(1 << 20)),
                deadline,
                client.official_page_semaphore.clone(),
            )
            .await;
        assert!(matches!(zero, Err(VlmError::InvalidConfig(_))));
        let invalid = client
            .official_two_step_snapshot_page_core(
                image.clone(),
                false,
                true,
                true,
                4,
                1,
                1,
                2,
                1,
                Arc::new(ByteBudget::new(1 << 20)),
                Arc::new(ByteBudget::new(1 << 20)),
                Arc::new(ByteBudget::new(1 << 20)),
                deadline,
                client.official_page_semaphore.clone(),
            )
            .await;
        assert!(matches!(invalid, Err(VlmError::InvalidConfig(_))));
    }
}
