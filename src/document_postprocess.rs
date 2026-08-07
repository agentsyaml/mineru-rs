use crate::{Document, PageResult, markdown, middle_json};

pub(crate) fn build(mut pages: Vec<PageResult>) -> Document {
    pages.sort_by_key(|page| page.page_index);
    for page in &mut pages {
        // The model's sequence is reading order; never replace it with geometric sorting.
        let mut last_title_level = 0;
        for block in &mut page.blocks {
            if block.kind.as_str() == crate::BlockKind::TITLE {
                let level = numbered_title_level(block.content.as_deref()).unwrap_or(
                    if last_title_level == 0 {
                        1
                    } else {
                        last_title_level
                    },
                );
                last_title_level = level;
                block.metadata.insert("title_level".into(), level.into());
            }
        }
        for index in 0..page.blocks.len() {
            if is_note(page.blocks[index].kind.as_str())
                && let Some(target) = nearest_compatible_asset(&page.blocks, index)
            {
                page.blocks[index]
                    .metadata
                    .insert("attached_to".into(), target.into());
                let note = page.blocks[index].content.clone().unwrap_or_default();
                if !note.trim().is_empty() {
                    let key = if page.blocks[index].kind.as_str().contains("caption") {
                        "caption"
                    } else {
                        "footnote"
                    };
                    let target = &mut page.blocks[target];
                    target.metadata.insert(key.into(), note.into());
                }
            }
        }
    }
    for index in 0..pages.len().saturating_sub(1) {
        let next = pages[index + 1]
            .blocks
            .iter_mut()
            .find(|block| block.kind.as_str() == crate::BlockKind::TABLE);
        if let Some(block) = next
            && let Some(cells) = block.metadata.remove("_cell_merge_from_previous")
            && valid_cell_merge(&cells)
        {
            block.metadata.insert("cell_merge".into(), cells);
            block
                .metadata
                .insert("cross_page_table".into(), true.into());
            block.merge_previous = true;
        }
    }
    Document {
        markdown: markdown::from_pages(&pages),
        middle_json: middle_json::from_pages(&pages),
        content_list: middle_json::content_list(&pages),
        pages,
        assets: Vec::new(),
        warnings: Vec::new(),
    }
}

pub(crate) fn valid_cell_merge(value: &serde_json::Value) -> bool {
    value
        .as_array()
        .is_some_and(|cells| cells.iter().all(|cell| cell.as_u64().is_some()))
}

fn numbered_title_level(text: Option<&str>) -> Option<u64> {
    let prefix = text?.split_whitespace().next()?;
    let parts = prefix.trim_end_matches('.').split('.').collect::<Vec<_>>();
    (!parts.is_empty() && parts.iter().all(|part| part.parse::<u32>().is_ok()))
        .then_some(parts.len().min(6) as u64)
}

fn is_note(kind: &str) -> bool {
    matches!(
        kind,
        crate::BlockKind::TABLE_CAPTION
            | crate::BlockKind::IMAGE_CAPTION
            | crate::BlockKind::CODE_CAPTION
            | crate::BlockKind::TABLE_FOOTNOTE
            | crate::BlockKind::IMAGE_FOOTNOTE
            | crate::BlockKind::PAGE_FOOTNOTE
    )
}

fn nearest_compatible_asset(blocks: &[crate::ContentBlock], index: usize) -> Option<usize> {
    let note_kind = blocks[index].kind.as_str();
    (0..blocks.len())
        .filter(|&candidate| compatible_note_target(note_kind, blocks[candidate].kind.as_str()))
        .min_by_key(|&candidate| (index.abs_diff(candidate), candidate > index))
}

fn compatible_note_target(note_kind: &str, target_kind: &str) -> bool {
    match note_kind {
        crate::BlockKind::IMAGE_CAPTION | crate::BlockKind::IMAGE_FOOTNOTE => matches!(
            target_kind,
            crate::BlockKind::IMAGE | crate::BlockKind::IMAGE_BLOCK | crate::BlockKind::CHART
        ),
        crate::BlockKind::TABLE_CAPTION | crate::BlockKind::TABLE_FOOTNOTE => {
            target_kind == crate::BlockKind::TABLE
        }
        crate::BlockKind::CODE_CAPTION => matches!(
            target_kind,
            crate::BlockKind::CODE | crate::BlockKind::ALGORITHM
        ),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::build;
    use crate::{BlockKind, ContentBlock, NormalizedBbox, PageResult};
    use serde_json::{Map, json};

    fn block(kind: &str, content: &str) -> ContentBlock {
        ContentBlock {
            kind: BlockKind::new(kind),
            bbox: NormalizedBbox::new(0.0, 0.0, 1.0, 1.0).unwrap(),
            angle: None,
            content: Some(content.into()),
            merge_previous: false,
            metadata: Map::new(),
        }
    }

    #[test]
    fn builds_ordered_public_document_with_assets_and_table_merge() {
        let mut table = block(BlockKind::TABLE, "<table><tr><td>a</td></tr></table>");
        table
            .metadata
            .insert("asset_path".into(), json!("assets/table.png"));
        table.metadata.insert("asset_md5".into(), json!("private"));
        table.metadata.insert("cell_merge".into(), json!([0, 1]));
        let mut image = block(BlockKind::IMAGE, "");
        image
            .metadata
            .insert("asset_path".into(), json!("assets/image.png"));
        let document = build(vec![
            PageResult {
                page_index: 1,
                page_size: [100.0, 200.0],
                blocks: vec![block(BlockKind::HEADER, "chrome"), table],
            },
            PageResult {
                page_index: 0,
                page_size: [100.0, 200.0],
                blocks: vec![
                    block(BlockKind::TITLE, "1. Intro"),
                    image,
                    block(BlockKind::IMAGE_CAPTION, "figure caption"),
                ],
            },
        ]);
        assert_eq!(document.pages[0].page_index, 0);
        assert!(
            document
                .markdown
                .contains("![figure caption](assets/image.png)")
        );
        assert!(!document.markdown.contains("chrome"));
        assert!(document.middle_json["pdf_info"][0]["para_blocks"][1]["asset_path"].is_string());
        assert!(document.middle_json.to_string().contains("cell_merge"));
        assert!(!document.middle_json.to_string().contains("asset_md5"));
    }

    #[test]
    fn note_skips_closer_incompatible_target() {
        let document = build(vec![PageResult {
            page_index: 0,
            page_size: [1.0, 1.0],
            blocks: vec![
                block(BlockKind::IMAGE, ""),
                block(BlockKind::TABLE, "table"),
                block(BlockKind::IMAGE_CAPTION, "image caption"),
            ],
        }]);

        assert_eq!(document.pages[0].blocks[2].metadata["attached_to"], 0);
        assert_eq!(
            document.pages[0].blocks[0].metadata["caption"],
            "image caption"
        );
        assert!(!document.pages[0].blocks[1].metadata.contains_key("caption"));
    }

    #[test]
    fn incompatible_and_page_footnotes_remain_unattached() {
        let document = build(vec![
            PageResult {
                page_index: 0,
                page_size: [1.0, 1.0],
                blocks: vec![
                    block(BlockKind::TABLE, "table"),
                    block(BlockKind::IMAGE_CAPTION, "orphan caption"),
                ],
            },
            PageResult {
                page_index: 1,
                page_size: [1.0, 1.0],
                blocks: vec![
                    block(BlockKind::IMAGE, ""),
                    block(BlockKind::PAGE_FOOTNOTE, "page footnote"),
                ],
            },
        ]);

        assert!(
            !document.pages[0].blocks[1]
                .metadata
                .contains_key("attached_to")
        );
        assert!(!document.pages[0].blocks[0].metadata.contains_key("caption"));
        assert!(
            !document.pages[1].blocks[1]
                .metadata
                .contains_key("attached_to")
        );
        assert!(
            !document.pages[1].blocks[0]
                .metadata
                .contains_key("footnote")
        );
    }

    #[test]
    fn consumes_integer_cell_merge_for_adjacent_tables() {
        let first = block(BlockKind::TABLE, "| a |\n| - |\n| 1 |");
        let mut second = block(BlockKind::TABLE, "| 2 |");
        second
            .metadata
            .insert("_cell_merge_from_previous".into(), json!([0, 1]));
        let document = build(vec![
            PageResult {
                page_index: 0,
                page_size: [1.0, 1.0],
                blocks: vec![first],
            },
            PageResult {
                page_index: 1,
                page_size: [1.0, 1.0],
                blocks: vec![second],
            },
        ]);

        assert_eq!(
            document.middle_json["pdf_info"][0]["preproc_blocks"][0]["cell_merge"],
            serde_json::Value::Null
        );
        assert_eq!(
            document.middle_json["pdf_info"][1]["para_blocks"][0]["cell_merge"],
            json!([0, 1])
        );
        assert_eq!(document.markdown, "| a |\n| - |\n| 1 |\n| 2 |");
        assert!(
            !document.pages[1].blocks[0]
                .metadata
                .contains_key("_cell_merge_from_previous")
        );
    }

    #[test]
    fn only_directional_metadata_marks_the_incoming_table_as_continuation() {
        let first = block(BlockKind::TABLE, "page zero");
        let mut second = block(BlockKind::TABLE, "page one");
        second.metadata.insert("cell_merge".into(), json!([0, 1]));
        let mut third = block(BlockKind::TABLE, "page two");
        third
            .metadata
            .insert("_cell_merge_from_previous".into(), json!([1, 0]));
        let document = build(vec![
            PageResult {
                page_index: 0,
                page_size: [1.0, 1.0],
                blocks: vec![first],
            },
            PageResult {
                page_index: 1,
                page_size: [1.0, 1.0],
                blocks: vec![second],
            },
            PageResult {
                page_index: 2,
                page_size: [1.0, 1.0],
                blocks: vec![third],
            },
        ]);

        assert!(!document.pages[1].blocks[0].merge_previous);
        assert!(document.pages[2].blocks[0].merge_previous);
        assert_eq!(document.markdown, "page zero\n\npage one\npage two");
    }

    #[test]
    fn keeps_distinct_incoming_and_outgoing_boundary_cells() {
        let first = block(BlockKind::TABLE, "page zero");
        let mut second = block(BlockKind::TABLE, "page one");
        second.metadata.insert("cell_merge".into(), json!([1, 0]));
        second
            .metadata
            .insert("_cell_merge_from_previous".into(), json!([0, 1]));
        let mut third = block(BlockKind::TABLE, "page two");
        third
            .metadata
            .insert("_cell_merge_from_previous".into(), json!([1, 0]));
        let document = build(vec![
            PageResult {
                page_index: 0,
                page_size: [1.0, 1.0],
                blocks: vec![first],
            },
            PageResult {
                page_index: 1,
                page_size: [1.0, 1.0],
                blocks: vec![second],
            },
            PageResult {
                page_index: 2,
                page_size: [1.0, 1.0],
                blocks: vec![third],
            },
        ]);

        assert_eq!(
            document.pages[1].blocks[0].metadata["cell_merge"],
            json!([0, 1])
        );
        assert_eq!(
            document.pages[2].blocks[0].metadata["cell_merge"],
            json!([1, 0])
        );
        assert!(document.pages[1].blocks[0].merge_previous);
        assert!(document.pages[2].blocks[0].merge_previous);
    }

    #[test]
    fn ignores_non_array_or_invalid_cell_merge() {
        for value in [json!(true), json!([-1]), json!([0, "1"])] {
            let mut first = block(BlockKind::TABLE, "first");
            first.metadata.insert("cell_merge".into(), value);
            let document = build(vec![
                PageResult {
                    page_index: 0,
                    page_size: [1.0, 1.0],
                    blocks: vec![first],
                },
                PageResult {
                    page_index: 1,
                    page_size: [1.0, 1.0],
                    blocks: vec![block(BlockKind::TABLE, "second")],
                },
            ]);
            assert_eq!(document.markdown, "first\n\nsecond");
            assert!(document.middle_json["pdf_info"][0]["para_blocks"][0]["cell_merge"].is_null());
            assert!(document.middle_json["pdf_info"][1]["para_blocks"][0]["cell_merge"].is_null());
        }
    }
}
