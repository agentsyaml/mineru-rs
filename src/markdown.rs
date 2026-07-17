use crate::{BlockKind, PageResult};

pub(crate) fn from_pages(pages: &[PageResult]) -> String {
    let mut output = String::new();
    for page in pages {
        for block in &page.blocks {
            if matches!(
                block.kind.as_str(),
                BlockKind::HEADER
                    | BlockKind::FOOTER
                    | BlockKind::PAGE_NUMBER
                    | BlockKind::ASIDE_TEXT
                    | BlockKind::PAGE_FOOTNOTE
                    | BlockKind::TABLE_CAPTION
                    | BlockKind::IMAGE_CAPTION
                    | BlockKind::CODE_CAPTION
                    | BlockKind::TABLE_FOOTNOTE
                    | BlockKind::IMAGE_FOOTNOTE
            ) {
                continue;
            }
            let content = block
                .content
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .or_else(|| block.metadata.get("asset_path").map(|_| ""));
            let Some(content) = content else {
                continue;
            };
            let text = match block.kind.as_str() {
                BlockKind::TITLE => format!("{} {content}", "#".repeat(title_level(block))),
                BlockKind::EQUATION | BlockKind::EQUATION_BLOCK => equation(content),
                BlockKind::TABLE => content.to_owned(),
                BlockKind::IMAGE | BlockKind::IMAGE_BLOCK | BlockKind::CHART => {
                    let path = block
                        .metadata
                        .get("asset_path")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    if path.is_empty() {
                        content.to_owned()
                    } else {
                        let alt = block
                            .metadata
                            .get("caption")
                            .and_then(|v| v.as_str())
                            .unwrap_or(content);
                        format!("![{alt}]({path})")
                    }
                }
                _ => content.to_owned(),
            };
            if block.kind.as_str() == BlockKind::TABLE
                && block.merge_previous
                && block
                    .metadata
                    .get("cell_merge")
                    .is_some_and(crate::document_postprocess::valid_cell_merge)
                && !output.is_empty()
            {
                output.push('\n');
                output.push_str(&text);
            } else if block.merge_previous && !output.is_empty() {
                let previous = output.chars().last().unwrap_or(' ');
                let first = text.chars().next().unwrap_or(' ');
                if !is_cjk(previous) || !is_cjk(first) {
                    output.push(' ');
                }
                output.push_str(&text);
            } else {
                if !output.is_empty() {
                    output.push_str("\n\n");
                }
                output.push_str(&text);
            }
        }
    }
    output
}

fn equation(content: &str) -> String {
    let content = content.trim();
    if content.starts_with("$$") && content.ends_with("$$") && content.len() >= 4 {
        content.to_owned()
    } else {
        format!("$$\n{content}\n$$")
    }
}

fn title_level(block: &crate::ContentBlock) -> usize {
    block
        .metadata
        .get("title_level")
        .and_then(|v| v.as_u64())
        .unwrap_or(1)
        .clamp(1, 6) as usize
}

fn is_cjk(c: char) -> bool {
    matches!(c as u32, 0x3000..=0x303f | 0x3400..=0x4dbf | 0x4e00..=0x9fff | 0xf900..=0xfaff)
}

#[cfg(test)]
mod tests {
    use super::from_pages;
    use crate::{BlockKind, ContentBlock, NormalizedBbox, PageResult};
    use serde_json::Map;

    #[test]
    fn joins_chinese_continuations_without_space() {
        let block = |content: &str, merge_previous| ContentBlock {
            kind: BlockKind::new(BlockKind::TEXT),
            bbox: NormalizedBbox::new(0.0, 0.0, 1.0, 1.0).unwrap(),
            angle: None,
            content: Some(content.into()),
            merge_previous,
            metadata: Map::new(),
        };
        assert_eq!(
            from_pages(&[PageResult {
                page_index: 0,
                page_size: [1.0, 1.0],
                blocks: vec![block("你好", false), block("世界", true)]
            }]),
            "你好世界"
        );
    }
}
