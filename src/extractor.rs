use crate::{
    ClientConfig, Error, PageResult, ParseOptions, Result, image_pipeline, layout, openai::OpenAi,
    profile, vlm_postprocess,
};
use image::RgbImage;
use regex::Regex;
use serde_json::Value;
use std::sync::Arc;
use tokio::{sync::Semaphore, task::JoinSet};

#[derive(Debug, Clone)]
pub(crate) struct PageExtractor {
    openai: Arc<OpenAi>,
    request_gate: Arc<Semaphore>,
    image_byte_gate: Arc<Semaphore>,
    max_image_bytes: usize,
    max_blocks_per_page: usize,
}
impl PageExtractor {
    pub(crate) fn new(config: &ClientConfig) -> Result<Self> {
        Ok(Self {
            openai: Arc::new(OpenAi::new(config)?),
            request_gate: Arc::new(Semaphore::new(config.request_concurrency)),
            image_byte_gate: Arc::new(Semaphore::new(
                config
                    .limits
                    .max_in_flight_image_bytes
                    .min(u32::MAX as usize),
            )),
            max_image_bytes: config.limits.max_in_flight_image_bytes,
            max_blocks_per_page: config.limits.max_blocks_per_page,
        })
    }
    pub(crate) async fn extract_page(
        &self,
        page_index: usize,
        page_size: [f32; 2],
        image: Arc<RgbImage>,
        options: &ParseOptions,
    ) -> Result<(PageResult, Vec<String>)> {
        let page = image_pipeline::page_png(&image)?;
        let _request = self
            .request_gate
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| Error::WorkerJoin("request semaphore closed".into()))?;
        let (layout_text, completion_warnings) = self
            .openai
            .completion(
                profile::LAYOUT_PROMPT,
                &[page],
                profile::LAYOUT_SAMPLING,
                options.max_new_tokens,
                options.allow_truncated,
            )
            .await?;
        drop(_request);
        let mut warnings: Vec<String> = completion_warnings
            .into_iter()
            .map(|warning| format!("page {page_index}: {warning}"))
            .collect();
        let mut blocks = parse_page_blocks(&layout_text, self.max_blocks_per_page, &mut warnings);
        let table_images = image_pipeline::build_table_image_map(&mut blocks);
        let max_image_bytes = self.max_image_bytes;
        let mut jobs = JoinSet::new();
        for (index, block) in blocks.iter().enumerate() {
            if block
                .metadata
                .get("_skip_asset")
                .and_then(serde_json::Value::as_bool)
                == Some(true)
            {
                continue;
            }
            if matches!(
                block.kind.as_str(),
                crate::BlockKind::LIST
                    | crate::BlockKind::IMAGE_BLOCK
                    | crate::BlockKind::EQUATION_BLOCK
            ) {
                continue;
            }
            if (block.kind.as_str() == crate::BlockKind::TABLE && !options.table)
                || (block.kind.as_str() == crate::BlockKind::EQUATION && !options.formula)
                || (matches!(
                    block.kind.as_str(),
                    crate::BlockKind::IMAGE | crate::BlockKind::CHART
                ) && !options.image_analysis)
            {
                continue;
            }
            let client = self.openai.clone();
            let permit = self.request_gate.clone();
            let byte_permit = self.image_byte_gate.clone();
            let source = image.clone();
            let bbox = block.bbox;
            let angle = block.angle;
            let table = block.kind.as_str() == crate::BlockKind::TABLE;
            let absorbed: Vec<_> = table_images
                .iter()
                .find(|(table_index, _)| *table_index == index)
                .map(|(_, ids)| ids.iter().map(|i| blocks[*i].clone()).collect())
                .unwrap_or_default();
            let table_block = block.clone();
            let kind = block.kind.as_str().to_owned();
            let max = options.max_new_tokens;
            let allow = options.allow_truncated;
            let page = page_index;
            let sampling = if table {
                profile::TABLE_SAMPLING
            } else {
                profile::RECOGNITION_SAMPLING
            };
            let prompt: &'static str = match kind.as_str() {
                crate::BlockKind::TABLE => profile::TABLE_PROMPT,
                crate::BlockKind::EQUATION => profile::EQUATION_PROMPT,
                crate::BlockKind::IMAGE | crate::BlockKind::CHART => profile::IMAGE_PROMPT,
                _ => profile::TEXT_PROMPT,
            };
            jobs.spawn(async move {
                // A failed block degrades to a warning: the page keeps the block with its
                // content absent and parsing continues with the remaining blocks.
                let _permit = permit
                    .acquire_owned()
                    .await
                    .map_err(|_| format!("page {page} block {index}: request semaphore closed"))?;
                let estimate = (source
                    .as_raw()
                    .len()
                    .min(u32::MAX as usize)
                    .min(max_image_bytes)) as u32;
                let _bytes = byte_permit
                    .acquire_many_owned(estimate.max(1))
                    .await
                    .map_err(|_| {
                        format!("page {page} block {index}: image byte semaphore closed")
                    })?;
                let content = async {
                    let crop = if table {
                        let (crop, map) = image_pipeline::mask_and_encode_table_image(
                            &source,
                            &table_block,
                            &absorbed,
                        )?;
                        // Map travels with the result because blocks cannot be borrowed by jobs.
                        (crop, Some(map))
                    } else {
                        (image_pipeline::crop(&source, bbox, angle, table), None)
                    };
                    let data = image_pipeline::data_url(&crop.0)?;
                    client
                        .completion(prompt, &[data], sampling, max, allow)
                        .await
                        .map(|(text, warnings)| (text, crop.1, warnings))
                }
                .await
                .map_err(|source| format!("page {page} block {index}: {source}"))?;
                Ok::<_, String>((index, content.0, content.1, content.2))
            });
        }
        while let Some(job) = jobs.join_next().await {
            match job.map_err(|e| Error::WorkerJoin(e.to_string()))? {
                Ok((index, content, token_map, block_warnings)) => {
                    warnings.extend(
                        block_warnings
                            .into_iter()
                            .map(|warning| format!("page {page_index} block {index}: {warning}")),
                    );
                    if let Some(map) = token_map {
                        blocks[index].metadata.insert(
                            "_table_image_token_map".into(),
                            serde_json::Value::Object(map),
                        );
                    }
                    vlm_postprocess::clean_block(&mut blocks[index], content);
                }
                Err(warning) => warnings.push(warning),
            }
        }
        vlm_postprocess::post_process(&mut blocks);
        Ok((
            PageResult {
                page_index,
                page_size,
                blocks,
            },
            warnings,
        ))
    }
    pub(crate) async fn merge_cross_page_tables(
        &self,
        pages: &mut [PageResult],
        _options: &ParseOptions,
    ) -> Vec<String> {
        let mut warnings = Vec::new();
        for (page, last_index, first_index) in table_candidates(pages) {
            let (left, right) = pages.split_at_mut(page + 1);
            let left_table = &mut left[page].blocks[last_index];
            let right_table = &mut right[0].blocks[first_index];
            let (Some(a), Some(b)) = (&left_table.content, &right_table.content) else {
                continue;
            };
            let (Some(left_shape), Some(right_shape)) = (table_shape(a), table_shape(b)) else {
                continue;
            };
            if left_shape.columns != right_shape.columns
                || left_shape.last_segments != right_shape.first_segments
                || !left_shape.last_has_content
                || !right_shape.first_has_content
            {
                continue;
            }
            let expected_segments = left_shape.last_segments.len();
            let prompt = cell_merge_prompt(a, b, expected_segments);
            let answer = match self
                .openai
                .completion(&prompt, &[], profile::RECOGNITION_SAMPLING, None, true)
                .await
            {
                Ok((answer, completion_warnings)) => {
                    warnings.extend(
                        completion_warnings
                            .into_iter()
                            .map(|warning| format!("page {page}: {warning}")),
                    );
                    answer
                }
                Err(error) => {
                    warnings.push(format!(
                        "page {page}: cross-page table merge failed: {error}"
                    ));
                    continue;
                }
            };
            let Some(cells) = parse_cell_merge(&answer, expected_segments) else {
                warnings.push(format!(
                    "page {page}: cross-page table merge reply was not parseable; tables kept separate"
                ));
                continue;
            };
            left_table
                .metadata
                .insert("cell_merge".into(), cells.clone());
            right_table
                .metadata
                .insert("cell_merge".into(), cells.clone());
            right_table
                .metadata
                .insert("_cell_merge_from_previous".into(), cells);
        }
        warnings
    }
}

fn parse_page_blocks(
    layout_text: &str,
    max_blocks_per_page: usize,
    warnings: &mut Vec<String>,
) -> Vec<crate::ContentBlock> {
    layout::parse_layout_tolerant(layout_text, max_blocks_per_page, warnings)
}

#[derive(Debug)]
struct TableShape {
    columns: usize,
    first_segments: Vec<usize>,
    last_segments: Vec<usize>,
    first_has_content: bool,
    last_has_content: bool,
}

fn table_candidates(pages: &[PageResult]) -> Vec<(usize, usize, usize)> {
    pages
        .windows(2)
        .enumerate()
        .filter_map(|(page, pair)| {
            let left = &pair[0].blocks;
            let right = &pair[1].blocks;
            let last = left
                .iter()
                .rposition(|b| b.kind.as_str() == crate::BlockKind::TABLE)?;
            let first = right
                .iter()
                .position(|b| b.kind.as_str() == crate::BlockKind::TABLE)?;
            (left[last + 1..]
                .iter()
                .chain(&right[..first])
                .all(|b| allowed_between_tables(b.kind.as_str())))
            .then_some((page, last, first))
        })
        .collect()
}

fn allowed_between_tables(kind: &str) -> bool {
    matches!(
        kind,
        crate::BlockKind::TABLE_CAPTION
            | crate::BlockKind::TABLE_FOOTNOTE
            | crate::BlockKind::IMAGE_CAPTION
            | crate::BlockKind::IMAGE_FOOTNOTE
            | crate::BlockKind::HEADER
            | crate::BlockKind::FOOTER
            | crate::BlockKind::PAGE_NUMBER
            | crate::BlockKind::PAGE_FOOTNOTE
    )
}

fn table_shape(html: &str) -> Option<TableShape> {
    let rows = Regex::new(r"(?is)<tr\b[^>]*>(.*?)</tr>").unwrap();
    let cells = Regex::new(r"(?is)<t[dh]\b([^>]*)>(.*?)</t[dh]>").unwrap();
    let tags = Regex::new(r"(?is)<[^>]+>").unwrap();
    let colspan = Regex::new(r#"(?i)\bcolspan\s*=\s*[\"']?(\d+)"#).unwrap();
    let rowspan = Regex::new(r#"(?i)\browspan\s*=\s*[\"']?(\d+)"#).unwrap();
    let mut active: Vec<Option<(usize, usize)>> = Vec::new();
    let mut content = Vec::new();
    let mut parsed = Vec::new();
    for row in rows.captures_iter(html) {
        let mut rendered: Vec<Option<usize>> =
            active.iter().map(|cell| cell.map(|(id, _)| id)).collect();
        for cell in cells.captures_iter(&row[1]) {
            let width = colspan
                .captures(&cell[1])
                .and_then(|m| m[1].parse().ok())
                .filter(|&n| n > 0)
                .unwrap_or(1);
            let height = rowspan
                .captures(&cell[1])
                .and_then(|m| m[1].parse().ok())
                .filter(|&n| n > 0)
                .unwrap_or(1);
            let start = (0..)
                .find(|&i| (i..i + width).all(|j| rendered.get(j).is_none_or(Option::is_none)))?;
            if rendered.len() < start + width {
                rendered.resize(start + width, None);
            }
            if active.len() < start + width {
                active.resize(start + width, None);
            }
            let id = content.len();
            content.push(!tags.replace_all(&cell[2], "").trim().is_empty());
            for column in start..start + width {
                rendered[column] = Some(id);
                if height > 1 {
                    active[column] = Some((id, height));
                }
            }
        }
        if rendered.iter().all(Option::is_none) {
            continue;
        }
        parsed.push(rendered);
        for cell in &mut active {
            if let Some((_, remaining)) = cell {
                *remaining -= 1;
                if *remaining == 0 {
                    *cell = None;
                }
            }
        }
    }
    let columns = parsed.iter().map(Vec::len).max()?;
    (columns > 0 && parsed.iter().all(|row| row.len() == columns)).then(|| {
        let segments = |row: &[Option<usize>]| -> Option<(Vec<usize>, bool)> {
            let mut widths = Vec::new();
            let mut has_content = false;
            let mut index = 0;
            while index < row.len() {
                let id = row[index]?;
                let end = (index + 1..row.len())
                    .find(|&i| row[i] != Some(id))
                    .unwrap_or(row.len());
                widths.push(end - index);
                has_content |= content[id];
                index = end;
            }
            Some((widths, has_content))
        };
        let (first_segments, first_has_content) = segments(parsed.first().unwrap())?;
        let (last_segments, last_has_content) = segments(parsed.last().unwrap())?;
        Some(TableShape {
            columns,
            first_segments,
            last_segments,
            first_has_content,
            last_has_content,
        })
    })?
}

fn cell_merge_prompt(left: &str, right: &str, segments: usize) -> String {
    format!(
        "Identify continuation cells between these adjacent table segments. Their boundary has {segments} ordered rendered cell segments. Return only a JSON integer array with exactly {segments} entries: 1 when that segment continues across pages, otherwise 0; no prose, object, or booleans. LEFT SEGMENT:\n{left}\nRIGHT SEGMENT:\n{right}"
    )
}

fn parse_cell_merge(answer: &str, segments: usize) -> Option<Value> {
    let Value::Array(cells) = serde_json::from_str::<Value>(answer.trim()).ok()? else {
        return None;
    };
    (cells.len() == segments
        && cells
            .iter()
            .all(|cell| matches!(cell, Value::Number(n) if n.as_u64().is_some_and(|n| n <= 1))))
    .then_some(Value::Array(cells))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{BlockKind, ClientConfig, ContentBlock, Limits, NormalizedBbox};
    use axum::{Json, Router, routing::post};
    use serde_json::{Map, json};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::{net::TcpListener, time::Duration};

    fn block(kind: &str, content: Option<&str>) -> ContentBlock {
        ContentBlock {
            kind: BlockKind::new(kind),
            bbox: NormalizedBbox {
                left: 0.,
                top: 0.,
                right: 1.,
                bottom: 1.,
            },
            angle: None,
            content: content.map(str::to_owned),
            merge_previous: false,
            metadata: Map::new(),
        }
    }
    fn page(blocks: Vec<ContentBlock>) -> PageResult {
        PageResult {
            page_index: 0,
            page_size: [1., 1.],
            blocks,
        }
    }

    #[test]
    fn candidates_cover_pages_one_to_two_and_two_to_three() {
        let pages = vec![
            page(vec![block(BlockKind::TABLE, None)]),
            page(vec![
                block(BlockKind::HEADER, None),
                block(BlockKind::TABLE, None),
            ]),
            page(vec![block(BlockKind::TABLE, None)]),
        ];
        assert_eq!(table_candidates(&pages), vec![(0, 0, 1), (1, 1, 0)]);
    }
    #[test]
    fn candidates_reject_intervening_text() {
        assert!(
            table_candidates(&[
                page(vec![
                    block(BlockKind::TABLE, None),
                    block(BlockKind::TEXT, None)
                ]),
                page(vec![block(BlockKind::TABLE, None)])
            ])
            .is_empty()
        );
    }
    #[test]
    fn structure_and_response_validation_reject_incompatible_data() {
        assert!(table_shape("<table><tr><td>a</td><td>b</td></tr></table>").is_some());
        assert!(
            table_shape("<table><tr><td>a</td></tr><tr><td>b</td><td>c</td></tr></table>")
                .is_none()
        );
        let shape = table_shape(
            "<table><tr><th rowspan=\"2\">H</th><th>A</th></tr><tr><td>B</td></tr></table>",
        )
        .unwrap();
        assert_eq!(shape.first_segments, vec![1, 1]);
        assert_eq!(shape.last_segments, vec![1, 1]);
        assert_eq!(
            parse_cell_merge("[0, 1]", 2),
            Some(serde_json::json!([0, 1]))
        );
        assert_eq!(
            parse_cell_merge("[0, 1, 0]", 3),
            Some(serde_json::json!([0, 1, 0]))
        );
        assert!(parse_cell_merge("[0]", 2).is_none());
        assert!(parse_cell_merge("{\"cells\":[0]}", 2).is_none());
        assert!(parse_cell_merge("[true]", 2).is_none());
        assert!(parse_cell_merge("[2]", 1).is_none());
        assert!(parse_cell_merge("[-1]", 1).is_none());
    }

    #[tokio::test]
    async fn excess_layout_blocks_are_truncated_to_warnings_before_block_work() {
        let layout = "<|box_start|>1 1 2 2<|box_end|><|ref_start|>text<|ref_end|><|box_start|>3 3 4 4<|box_end|><|ref_start|>title<|ref_end|>";
        let app = Router::new().route(
            "/v1/chat/completions",
            post(move || async move {
                Json(json!({"choices":[{"finish_reason":"stop","message":{"content":layout}}]}))
            }),
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let mut config = ClientConfig::new(format!("http://{address}"), "model").unwrap();
        config.limits = Limits {
            max_blocks_per_page: 1,
            ..Limits::default()
        };
        let extractor = PageExtractor::new(&config).unwrap();
        let (result, warnings) = extractor
            .extract_page(
                7,
                [1., 1.],
                Arc::new(RgbImage::new(1, 1)),
                &ParseOptions::default(),
            )
            .await
            .unwrap();
        assert_eq!(result.blocks.len(), 1);
        assert!(
            warnings
                .iter()
                .any(|w| w.contains("blocks per page limit 1 reached")),
            "{warnings:?}"
        );
    }

    #[tokio::test]
    async fn failing_block_call_keeps_block_and_continues_page() {
        use axum::{http::StatusCode, response::IntoResponse};
        use std::sync::atomic::AtomicUsize;
        let calls = Arc::new(AtomicUsize::new(0));
        let app = Router::new().route(
            "/v1/chat/completions",
            post({
                let calls = calls.clone();
                move || {
                    let calls = calls.clone();
                    async move {
                        if calls.fetch_add(1, Ordering::SeqCst) == 0 {
                            Json(json!({"choices":[{"finish_reason":"stop","message":{"content":"<|box_start|>0 0 1000 1000<|box_end|><|ref_start|>text<|ref_end|>"}}]}))
                                .into_response()
                        } else {
                            (StatusCode::INTERNAL_SERVER_ERROR, "block service failure")
                                .into_response()
                        }
                    }
                }
            }),
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let config = ClientConfig::new(format!("http://{address}"), "model").unwrap();
        let extractor = PageExtractor::new(&config).unwrap();
        let (result, warnings) = extractor
            .extract_page(
                3,
                [1., 1.],
                Arc::new(RgbImage::new(1, 1)),
                &ParseOptions::default(),
            )
            .await
            .unwrap();
        assert_eq!(result.blocks.len(), 1);
        assert!(
            result.blocks[0].content.is_none(),
            "block must stay content-less on failure"
        );
        assert!(
            warnings.iter().any(|w| w.contains("page 3 block 0")),
            "{warnings:?}"
        );
    }

    #[tokio::test]
    async fn window_pages_share_request_limit_and_restore_order() {
        let active = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let app = Router::new().route(
            "/v1/chat/completions",
            post({
                let active = active.clone();
                let peak = peak.clone();
                move || {
                    let active = active.clone();
                    let peak = peak.clone();
                    async move {
                        let current = active.fetch_add(1, Ordering::SeqCst) + 1;
                        peak.fetch_max(current, Ordering::SeqCst);
                        tokio::time::sleep(Duration::from_millis(20)).await;
                        active.fetch_sub(1, Ordering::SeqCst);
                        Json(json!({"choices":[{"finish_reason":"stop","message":{"content":"<|box_start|>0 0 1000 1000<|box_end|><|ref_start|>text<|ref_end|>"}}]}))
                    }
                }
            }),
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let mut config = ClientConfig::new(format!("http://{address}"), "model").unwrap();
        config.request_concurrency = 2;
        let extractor = PageExtractor::new(&config).unwrap();
        let mut jobs = JoinSet::new();
        for index in [2, 0, 1] {
            let extractor = extractor.clone();
            jobs.spawn(async move {
                extractor
                    .extract_page(
                        index,
                        [1., 1.],
                        Arc::new(RgbImage::new(1, 1)),
                        &ParseOptions::default(),
                    )
                    .await
                    .unwrap()
                    .0
            });
        }
        let mut pages = Vec::new();
        while let Some(page) = jobs.join_next().await {
            pages.push(page.unwrap());
        }
        pages.sort_unstable_by_key(|page| page.page_index);

        assert_eq!(
            pages.iter().map(|page| page.page_index).collect::<Vec<_>>(),
            vec![0, 1, 2]
        );
        assert_eq!(peak.load(Ordering::SeqCst), 2);
    }
}
