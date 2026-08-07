use crate::{BlockKind, ContentBlock, Error, ErrorContext, NormalizedBbox, Result, Rotation};
use regex::Regex;
use serde_json::Map;

/// Parses MinerU's boxed layout stream.
pub(crate) fn parse_layout(input: &str, max_blocks: usize) -> Result<Vec<ContentBlock>> {
    let (blocks, truncated) = parse_blocks(input, max_blocks);
    if truncated {
        return Err(Error::LimitExceeded {
            resource: "blocks per page",
            limit: max_blocks as u64,
            actual: blocks.len().saturating_add(1) as u64,
        });
    }
    if blocks.is_empty() && !input.trim().is_empty() {
        return Err(Error::Protocol {
            context: ErrorContext {
                operation: Some("layout parse"),
                ..Default::default()
            },
            message: "no valid layout tokens".into(),
        });
    }
    Ok(blocks)
}

/// Tolerant variant for the direct pipeline: never fails. Oversized or malformed output
/// degrades to the blocks parsed so far plus a warning so a page is never aborted.
pub(crate) fn parse_layout_tolerant(
    input: &str,
    max_blocks: usize,
    warnings: &mut Vec<String>,
) -> Vec<ContentBlock> {
    let (blocks, truncated) = parse_blocks(input, max_blocks);
    if truncated {
        warnings.push(format!(
            "blocks per page limit {max_blocks} reached; remaining layout blocks truncated"
        ));
    } else if blocks.is_empty() && !input.trim().is_empty() {
        warnings.push("no valid layout tokens; page treated as empty".into());
    }
    blocks
}

fn parse_blocks(input: &str, max_blocks: usize) -> (Vec<ContentBlock>, bool) {
    let mut blocks = Vec::new();
    // Rust's regex engine has no look-ahead; matching each complete header is
    // equivalent because the trailing content is not part of ContentBlock.
    let layout = Regex::new(r"<\|box_start\|>([0-9]+)\s+([0-9]+)\s+([0-9]+)\s+([0-9]+)<\|box_end\|><\|ref_start\|>(\w+?)<\|ref_end\|>(?:(<\|rotate_(up|right|down|left)\|>))?")
        .expect("frozen layout regex is valid");

    let mut tokens = layout.captures_iter(input).peekable();
    while let Some(token) = tokens.next() {
        let [Ok(x1), Ok(y1), Ok(x2), Ok(y2)] =
            [1, 2, 3, 4].map(|index| token[index].parse::<f32>())
        else {
            continue;
        };
        let bbox_values = [x1, y1, x2, y2];
        let (left, right) = if bbox_values[0] <= bbox_values[2] {
            (bbox_values[0], bbox_values[2])
        } else {
            (bbox_values[2], bbox_values[0])
        };
        let (top, bottom) = if bbox_values[1] <= bbox_values[3] {
            (bbox_values[1], bbox_values[3])
        } else {
            (bbox_values[3], bbox_values[1])
        };
        let bbox =
            match NormalizedBbox::new(left / 1000.0, top / 1000.0, right / 1000.0, bottom / 1000.0)
            {
                Ok(v) => v,
                Err(_) => continue,
            };
        let raw_kind = &token[5];
        let kind = match raw_kind {
            BlockKind::UNKNOWN | "inline_formula" => continue,
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
            | BlockKind::EQUATION_BLOCK => raw_kind,
            _ => continue,
        };
        if blocks.len() >= max_blocks {
            return (blocks, true);
        }
        let end = token.get(0).unwrap().end();
        let next = tokens
            .peek()
            .and_then(|next| next.get(0))
            .map_or(input.len(), |next| next.start());
        let merge_previous = kind == BlockKind::TEXT && input[end..next].contains("txt_contd_tgt");
        let angle = match token.get(7).map(|rotation| rotation.as_str()) {
            Some("up") => Some(Rotation::Deg0),
            Some("right") => Some(Rotation::Deg90),
            Some("down") => Some(Rotation::Deg180),
            Some("left") => Some(Rotation::Deg270),
            _ => None,
        };
        blocks.push(ContentBlock {
            kind: BlockKind::new(kind),
            bbox,
            angle,
            content: None,
            merge_previous,
            metadata: Map::new(),
        });
    }
    (blocks, false)
}

#[cfg(test)]
mod tests {
    use super::{parse_layout, parse_layout_tolerant};
    use crate::{BlockKind, Rotation};

    #[test]
    fn parses_frozen_layout_format() {
        let blocks = parse_layout(include_str!("../tests/fixtures/vlm/layout.txt"), 3).unwrap();
        assert_eq!(blocks.len(), 3);
        assert!(blocks[0].merge_previous);
        assert_eq!(blocks[0].angle, Some(Rotation::Deg0));
        assert_eq!(blocks[1].kind.as_str(), BlockKind::TABLE);
        assert_eq!(blocks[1].angle, Some(Rotation::Deg90));
        assert_eq!(blocks[1].bbox.left, 0.001);
        assert_eq!(blocks[2].angle, Some(Rotation::Deg180));
        assert!(blocks.iter().all(|block| block.content.is_none()));
    }

    #[test]
    fn skips_bad_boxes_and_unsupported_kinds() {
        let blocks = parse_layout("malformed <|box_start|>0 0 1001 1000<|box_end|><|ref_start|>text<|ref_end|><|box_start|>1 1 2 2<|box_end|><|ref_start|>unknown<|ref_end|><|box_start|>1 1 2 2<|box_end|><|ref_start|>inline_formula<|ref_end|><|box_start|>1 1 2 2<|box_end|><|ref_start|>future_kind<|ref_end|><|box_start|>1 1 2 2<|box_end|><|ref_start|>title<|ref_end|><|rotate_left|>", 1).unwrap();
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].kind.as_str(), BlockKind::TITLE);
        assert_eq!(blocks[0].angle, Some(Rotation::Deg270));
    }

    #[test]
    fn unicode_digits_and_surrounding_text_do_not_panic() {
        let input = "中文😀<|box_start|>١ 0 2 2<|box_end|><|ref_start|>text<|ref_end|>后缀<|box_start|>1 1 2 2<|box_end|><|ref_start|>title<|ref_end|>";
        let blocks = parse_layout(input, 1).unwrap();
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].kind.as_str(), BlockKind::TITLE);
    }

    #[test]
    fn rejects_nonempty_input_without_valid_blocks() {
        assert!(
            parse_layout(
                "<|box_start|>0 0 0 1<|box_end|><|ref_start|>text<|ref_end|>",
                1
            )
            .is_err()
        );
    }

    #[test]
    fn limits_valid_blocks_after_skipping_invalid_entries() {
        let input = "<|box_start|>0 0 0 1<|box_end|><|ref_start|>text<|ref_end|><|box_start|>1 1 2 2<|box_end|><|ref_start|>title<|ref_end|><|box_start|>3 3 4 4<|box_end|><|ref_start|>text<|ref_end|>";
        assert!(matches!(
            parse_layout(input, 1),
            Err(crate::Error::LimitExceeded {
                resource: "blocks per page",
                limit: 1,
                actual: 2
            })
        ));
    }

    #[test]
    fn tolerant_parse_truncates_and_warns_on_oversized_and_empty_output() {
        let oversized = "<|box_start|>1 1 2 2<|box_end|><|ref_start|>text<|ref_end|><|box_start|>3 3 4 4<|box_end|><|ref_start|>title<|ref_end|>";
        let mut warnings = Vec::new();
        let blocks = parse_layout_tolerant(oversized, 1, &mut warnings);
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].kind.as_str(), BlockKind::TEXT);
        assert_eq!(
            warnings,
            vec!["blocks per page limit 1 reached; remaining layout blocks truncated".to_owned()]
        );

        let mut warnings = Vec::new();
        let blocks = parse_layout_tolerant(
            "<|box_start|>0 0 0 1<|box_end|><|ref_start|>text<|ref_end|>",
            1,
            &mut warnings,
        );
        assert!(blocks.is_empty());
        assert_eq!(
            warnings,
            vec!["no valid layout tokens; page treated as empty".to_owned()]
        );

        let mut warnings = Vec::new();
        let blocks = parse_layout_tolerant("", 1, &mut warnings);
        assert!(blocks.is_empty());
        assert!(warnings.is_empty());
    }
}
