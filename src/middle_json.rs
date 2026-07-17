use crate::PageResult;
use serde_json::{Value, json};

/// A stable, public representation of the model result.  Private extraction
/// metadata deliberately stays in the pipeline.
pub(crate) fn from_pages(pages: &[PageResult]) -> Value {
    json!({ "pdf_info": pages.iter().map(|page| {
        let semantic = |block: &crate::ContentBlock| !is_discarded(block);
        json!({
            "page_index": page.page_index,
            "page_size": page.page_size,
            "preproc_blocks": page.blocks.iter().filter(|block| semantic(block)).map(block_json).collect::<Vec<_>>(),
            "para_blocks": para_blocks(page),
            "discarded_blocks": page.blocks.iter().filter(|block| is_discarded(block)).map(block_json).collect::<Vec<_>>(),
        })
    }).collect::<Vec<_>>() })
}

fn block_json(block: &crate::ContentBlock) -> Value {
    let content = block.content.as_deref().unwrap_or("");
    json!({
        "type": block.kind.as_str(),
        "bbox": block.bbox,
        "content": content,
        "lines": lines(content),
        "merge_previous": block.merge_previous,
        "asset_path": block.metadata.get("asset_path").and_then(Value::as_str),
        "cell_merge": block.metadata.get("cell_merge").filter(|v| crate::document_postprocess::valid_cell_merge(v)),
        "caption": block.metadata.get("caption").and_then(Value::as_str),
        "footnote": block.metadata.get("footnote").and_then(Value::as_str),
    })
}

fn lines(content: &str) -> Vec<Value> {
    content
        .lines()
        .map(|text| json!({ "spans": [{ "content": text }] }))
        .collect()
}

fn para_blocks(page: &PageResult) -> Vec<Value> {
    let mut blocks: Vec<Value> = Vec::new();
    for block in page
        .blocks
        .iter()
        .filter(|block| !is_discarded(block) && !is_note(block))
    {
        let value = block_json(block);
        if block.merge_previous
            && matches!(
                block.kind.as_str(),
                crate::BlockKind::TEXT | crate::BlockKind::LIST_ITEM
            )
            && let Some(previous) = blocks.last_mut()
            && previous["type"] == block.kind.as_str()
        {
            let separator = if previous["content"].as_str().unwrap_or("").is_empty() {
                ""
            } else {
                " "
            };
            let combined = format!(
                "{}{}{}",
                previous["content"].as_str().unwrap_or(""),
                separator,
                block.content.as_deref().unwrap_or("")
            );
            previous["content"] = combined.clone().into();
            previous["lines"] = json!(lines(&combined));
        } else {
            blocks.push(value);
        }
    }
    blocks
}

fn is_discarded(block: &crate::ContentBlock) -> bool {
    matches!(
        block.kind.as_str(),
        crate::BlockKind::HEADER
            | crate::BlockKind::FOOTER
            | crate::BlockKind::PAGE_NUMBER
            | crate::BlockKind::ASIDE_TEXT
            | crate::BlockKind::PAGE_FOOTNOTE
    )
}

fn is_note(block: &crate::ContentBlock) -> bool {
    matches!(
        block.kind.as_str(),
        crate::BlockKind::TABLE_CAPTION
            | crate::BlockKind::IMAGE_CAPTION
            | crate::BlockKind::CODE_CAPTION
            | crate::BlockKind::TABLE_FOOTNOTE
            | crate::BlockKind::IMAGE_FOOTNOTE
    )
}

pub(crate) fn content_list(pages: &[PageResult]) -> Value {
    Value::Array(
        pages
            .iter()
            .flat_map(|page| {
                page.blocks
                    .iter()
                    .filter(|block| !is_discarded(block) && !is_note(block))
                    .enumerate()
                    .map(move |(block_index, block)| {
                        json!({
                            "page_index": page.page_index,
                            "block_index": block_index,
                            "type": content_type(block.kind.as_str()),
                            "content": block.content,
                            "bbox": block.bbox,
                            "asset_path": block.metadata.get("asset_path"),
                        })
                    })
            })
            .collect(),
    )
}

fn content_type(kind: &str) -> &str {
    match kind {
        crate::BlockKind::IMAGE_BLOCK | crate::BlockKind::CHART => "image",
        crate::BlockKind::EQUATION_BLOCK => "equation",
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::from_pages;
    use crate::{BlockKind, ContentBlock, NormalizedBbox, PageResult};
    use serde_json::Map;

    #[test]
    fn pdf_info_is_a_single_page_array_with_text_spans() {
        let page = PageResult {
            page_index: 0,
            page_size: [100.0, 200.0],
            blocks: vec![ContentBlock {
                kind: BlockKind::new(BlockKind::TEXT),
                bbox: NormalizedBbox::new(0.0, 0.0, 1.0, 1.0).unwrap(),
                angle: None,
                content: Some("one\ntwo".into()),
                merge_previous: false,
                metadata: Map::new(),
            }],
        };
        let output = from_pages(&[page]);
        assert!(output["pdf_info"].is_array());
        assert!(output["pdf_info"][0]["page_index"].is_number());
        assert_eq!(
            output["pdf_info"][0]["para_blocks"][0]["lines"]
                .as_array()
                .unwrap()
                .len(),
            2
        );
    }
}
