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
    concurrency: usize,
    image_bytes: usize,
    max_blocks_per_page: usize,
}
impl PageExtractor {
    pub(crate) fn new(config: &ClientConfig) -> Result<Self> {
        Ok(Self {
            openai: Arc::new(OpenAi::new(config)?),
            concurrency: config.request_concurrency,
            image_bytes: config.limits.max_in_flight_image_bytes,
            max_blocks_per_page: config.limits.max_blocks_per_page,
        })
    }
    pub(crate) async fn extract_page(
        &self,
        page_index: usize,
        page_size: [f32; 2],
        image: RgbImage,
        options: &ParseOptions,
    ) -> Result<PageResult> {
        let page = image_pipeline::page_png(&image)?;
        let layout_text = self
            .openai
            .completion(
                profile::LAYOUT_PROMPT,
                &[page],
                profile::LAYOUT_SAMPLING,
                options.max_new_tokens,
                options.allow_truncated,
            )
            .await?;
        let mut blocks = parse_page_blocks(&layout_text, self.max_blocks_per_page)?;
        let table_images = image_pipeline::build_table_image_map(&mut blocks);
        let page = Arc::new(image);
        let semaphore = Arc::new(Semaphore::new(self.concurrency));
        let bytes = Arc::new(Semaphore::new(self.image_bytes.min(u32::MAX as usize)));
        let max_image_bytes = self.image_bytes;
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
            let permit = semaphore.clone();
            let byte_permit = bytes.clone();
            let source = page.clone();
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
                let _permit = permit
                    .acquire_owned()
                    .await
                    .map_err(|_| Error::WorkerJoin("request semaphore closed".into()))?;
                let estimate = (source
                    .as_raw()
                    .len()
                    .min(u32::MAX as usize)
                    .min(max_image_bytes)) as u32;
                let _bytes = byte_permit
                    .acquire_many_owned(estimate.max(1))
                    .await
                    .map_err(|_| Error::WorkerJoin("image byte semaphore closed".into()))?;
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
                        .map(|text| (text, crop.1))
                }
                .await
                .map_err(|source| Error::Block {
                    page,
                    block: index,
                    source: Box::new(source),
                })?;
                Ok::<_, Error>((index, content.0, content.1))
            });
        }
        while let Some(job) = jobs.join_next().await {
            let (index, content, token_map) =
                job.map_err(|e| Error::WorkerJoin(e.to_string()))??;
            if let Some(map) = token_map {
                blocks[index].metadata.insert(
                    "_table_image_token_map".into(),
                    serde_json::Value::Object(map),
                );
            }
            vlm_postprocess::clean_block(&mut blocks[index], content);
        }
        vlm_postprocess::post_process(&mut blocks);
        Ok(PageResult {
            page_index,
            page_size,
            blocks,
        })
    }
    pub(crate) async fn merge_cross_page_tables(
        &self,
        pages: &mut [PageResult],
        _options: &ParseOptions,
    ) -> Result<()> {
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
            let answer = self
                .openai
                .completion(&prompt, &[], profile::RECOGNITION_SAMPLING, None, true)
                .await?;
            let Some(cells) = parse_cell_merge(&answer, expected_segments) else {
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
        Ok(())
    }
}

fn parse_page_blocks(
    layout_text: &str,
    max_blocks_per_page: usize,
) -> Result<Vec<crate::ContentBlock>> {
    layout::parse_layout(layout_text, max_blocks_per_page)
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
    use tokio::net::TcpListener;

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
    async fn rejects_excess_layout_blocks_before_block_work() {
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
        assert!(matches!(
            extractor
                .extract_page(7, [1., 1.], RgbImage::new(1, 1), &ParseOptions::default())
                .await,
            Err(Error::LimitExceeded {
                resource: "blocks per page",
                limit: 1,
                actual: 2
            })
        ));
    }
}
