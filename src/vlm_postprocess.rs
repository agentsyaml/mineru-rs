use crate::{BlockKind, ContentBlock};
use regex::Regex;
use serde_json::Value;

const TABLE_FORMULA: &str = r"(?s)\x5c\((.*?)\x5c\)|\x5c\[(.*?)\x5c\]";

pub(crate) fn clean_block(block: &mut ContentBlock, response: String) {
    let mut text = response.replace("<|im_end|>", "");
    if matches!(
        block.kind.as_str(),
        BlockKind::IMAGE | BlockKind::CHART | BlockKind::IMAGE_BLOCK
    ) && let Some(output) = parse_vlm_class_output(&text)
    {
        text = apply_vlm_class_output(block, output);
    }

    let marker = text.trim();
    if matches!(
        block.kind.as_str(),
        BlockKind::IMAGE | BlockKind::CHART | BlockKind::IMAGE_BLOCK
    ) {
        if is_pure_table(marker) {
            block.kind = BlockKind::new(BlockKind::TABLE);
        } else if is_pure_formula(marker) {
            block.kind = BlockKind::new(BlockKind::EQUATION);
        } else if block.kind.as_str() == BlockKind::IMAGE_BLOCK && !marker.is_empty() {
            block.kind = BlockKind::new(BlockKind::IMAGE);
        }
    }
    if block.kind.as_str() == BlockKind::EQUATION_BLOCK && is_pure_formula(marker) {
        block.kind = BlockKind::new(BlockKind::EQUATION);
    }

    // Tokens are data, not display text: resolve them before table escaping and OTSL conversion.
    if block.kind.as_str() == BlockKind::TABLE {
        text = replace_table_images(block, text);
        text = otsl_html(&text);
        text = formulas(&text);
    } else if block.kind.as_str() == BlockKind::EQUATION {
        text = normalize_equation(&text);
    }

    text = text.trim().to_owned();
    if block.kind.as_str() == BlockKind::LIST_ITEM {
        block.kind = BlockKind::new(BlockKind::TEXT);
        text = text.trim_start_matches(['-', '*', '•', ' ']).to_owned();
    }
    if block.kind.as_str() == BlockKind::TEXT {
        text = clean_text(&text);
    }
    if text == "[Non-Text]" {
        text.clear();
    }
    block.content = (!text.is_empty()).then_some(text);
}

#[derive(Default)]
struct VlmClassOutput {
    class_name: String,
    sub_class: Option<String>,
    caption: Option<String>,
    content: Option<String>,
}

fn parse_vlm_class_output(value: &str) -> Option<VlmClassOutput> {
    let class_name = tagged_field(value, "class")?;
    let class_name = compact_whitespace(&class_name);
    if class_name.is_empty() {
        return None;
    }
    Some(VlmClassOutput {
        class_name,
        sub_class: field(value, "sub_class"),
        caption: field(value, "caption"),
        content: field(value, "content"),
    })
}

fn field(value: &str, name: &str) -> Option<String> {
    tagged_field(value, name).or_else(|| line_field(value, name))
}

fn tagged_field(value: &str, name: &str) -> Option<String> {
    let name = if name == "sub_class" {
        r"sub[\s_-]*class"
    } else {
        name
    };
    let start = Regex::new(&format!(r"(?i)<\|\s*{name}\s*_(?:start|begin)\s*\|>"))
        .expect("fixed VLM field regex is valid");
    let end = Regex::new(&format!(r"(?i)<\|\s*{name}\s*_(?:end|stop)\s*\|>"))
        .expect("fixed VLM field regex is valid");
    if let Some(start) = start.find(value) {
        let body = &value[start.end()..];
        if let Some(end) = end.find(body) {
            return Some(body[..end.start()].trim().to_owned());
        }
        // A truncated response can still provide the last content field safely.
        if name == "content" {
            return Some(body.trim().to_owned());
        }
    }

    let xml = Regex::new(&format!(r"(?is)<\s*{name}\s*>\s*(.*?)\s*</\s*{name}\s*>"))
        .expect("fixed VLM XML field regex is valid");
    xml.captures(value)
        .and_then(|captures| captures.get(1))
        .map(|body| body.as_str().trim().to_owned())
}

fn line_field(value: &str, name: &str) -> Option<String> {
    let name = if name == "sub_class" {
        r"sub[\s_-]*class"
    } else {
        name
    };
    let line = Regex::new(&format!(
        r"(?im)^[ \t]*(?:[-*][ \t]+)?{name}[ \t]*(?::|=)[ \t]*(.+?)[ \t]*$"
    ))
    .expect("fixed VLM line field regex is valid");
    line.captures(value)
        .and_then(|captures| captures.get(1))
        .map(|body| body.as_str().trim().to_owned())
}

fn compact_whitespace(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn normalized_class(value: &str) -> String {
    compact_whitespace(value)
        .to_ascii_lowercase()
        .replace([' ', '-'], "_")
}

fn apply_vlm_class_output(block: &mut ContentBlock, output: VlmClassOutput) -> String {
    if let Some(caption) = output
        .caption
        .as_ref()
        .filter(|caption| !caption.is_empty())
    {
        block
            .metadata
            .insert("caption".into(), Value::String(caption.clone()));
    }

    let class_name = normalized_class(&output.class_name);
    match class_name.as_str() {
        "pure_table" | "table" => block.kind = BlockKind::new(BlockKind::TABLE),
        "pure_formula" | "formula" | "equation" => block.kind = BlockKind::new(BlockKind::EQUATION),
        "chart" => {
            block.kind = BlockKind::new(BlockKind::CHART);
            if let Some(sub_class) = output.sub_class.filter(|sub_class| !sub_class.is_empty()) {
                block.metadata.insert(
                    "sub_type".into(),
                    Value::String(compact_whitespace(&sub_class)),
                );
            }
        }
        _ => block.kind = BlockKind::new(BlockKind::IMAGE),
    }

    output
        .content
        .filter(|content| !content.is_empty())
        .or(output.caption)
        .unwrap_or_default()
}

fn is_pure_table(value: &str) -> bool {
    let value = value.trim();
    value.to_ascii_lowercase().starts_with("<table")
        && value.to_ascii_lowercase().ends_with("</table>")
}

fn is_pure_formula(value: &str) -> bool {
    outer_equation_delimiter(value.trim()).is_some()
}

/// Final page cleanup; kept separate so extraction can wire it in after all jobs finish.
pub(crate) fn post_process(blocks: &mut Vec<ContentBlock>) {
    for block in blocks.iter_mut() {
        if let Some(content) = block.content.clone() {
            clean_block(block, content);
        }
    }
    let remove_containers = resolve_containers(blocks);
    let mut index = 0;
    blocks.retain(|block| {
        let remove_container = remove_containers[index];
        index += 1;
        !(remove_container
            || block.metadata.contains_key("_absorbed_by_table")
            || block.kind.as_str() == "paratext"
            || block.kind.as_str() == BlockKind::LIST && block.content.is_none())
    });
    for block in blocks {
        block.metadata.retain(|key, _| !key.starts_with('_'));
    }
}

fn resolve_containers(blocks: &mut [ContentBlock]) -> Vec<bool> {
    let mut remove = vec![false; blocks.len()];
    for index in 0..blocks.len() {
        let kind = blocks[index].kind.as_str();
        if kind == BlockKind::EQUATION_BLOCK {
            let children: Vec<_> = blocks
                .iter()
                .enumerate()
                .filter(|(child_index, child)| {
                    *child_index != index
                        && child.kind.as_str() == BlockKind::EQUATION
                        && child
                            .content
                            .as_deref()
                            .is_some_and(|content| !content.trim().is_empty())
                        && covers(&blocks[index], child)
                })
                .map(|(child_index, _)| child_index)
                .collect();
            match children.len() {
                0 => {}
                1 => remove[index] = true,
                _ => {
                    let contents = children
                        .iter()
                        .filter_map(|child_index| blocks[*child_index].content.as_deref())
                        .map(str::to_owned)
                        .collect::<Vec<_>>();
                    blocks[index].kind = BlockKind::new(BlockKind::EQUATION);
                    blocks[index].content = Some(format!(
                        r"\begin{{array}}{{l}} {} \end{{array}}",
                        contents.join(r" \\ ")
                    ));
                    for child_index in children {
                        remove[child_index] = true;
                    }
                }
            }
        } else if kind == BlockKind::IMAGE_BLOCK {
            let has_child = blocks.iter().enumerate().any(|(child_index, child)| {
                child_index != index
                    && matches!(
                        child.kind.as_str(),
                        BlockKind::IMAGE
                            | BlockKind::CHART
                            | BlockKind::TABLE
                            | BlockKind::EQUATION
                    )
                    && (child
                        .content
                        .as_deref()
                        .is_some_and(|content| !content.trim().is_empty())
                        || child.metadata.contains_key("asset_path"))
                    && covers(&blocks[index], child)
            });
            if has_child {
                remove[index] = true;
            } else if blocks[index].content.is_none() {
                block_as_image(&mut blocks[index]);
            }
        } else if kind == BlockKind::LIST {
            let has_child = blocks.iter().enumerate().any(|(child_index, child)| {
                child_index != index
                    && child.kind.as_str() == BlockKind::TEXT
                    && child
                        .content
                        .as_deref()
                        .is_some_and(|content| !content.trim().is_empty())
                    && covers(&blocks[index], child)
            });
            if has_child {
                remove[index] = true;
            }
        }
    }
    remove
}

fn block_as_image(block: &mut ContentBlock) {
    block.kind = BlockKind::new(BlockKind::IMAGE);
}

fn covers(parent: &ContentBlock, child: &ContentBlock) -> bool {
    let left = parent.bbox.left.max(child.bbox.left);
    let top = parent.bbox.top.max(child.bbox.top);
    let right = parent.bbox.right.min(child.bbox.right);
    let bottom = parent.bbox.bottom.min(child.bbox.bottom);
    let overlap = (right - left).max(0.0) * (bottom - top).max(0.0);
    let child_area = (child.bbox.right - child.bbox.left) * (child.bbox.bottom - child.bbox.top);
    child_area > 0.0 && overlap / child_area > 0.9
}

fn replace_table_images(block: &ContentBlock, mut value: String) -> String {
    if block.kind.as_str() != BlockKind::TABLE {
        return value;
    }
    if let Some(map) = block
        .metadata
        .get("_table_image_token_map")
        .and_then(|v| v.as_object())
    {
        for (token, url) in map {
            if let Some(url) = url.as_str() {
                value = value.replace(token, &format!(r#"<img src="{}"/>"#, escape_attr(url)));
            }
        }
    }
    value
}

fn formulas(value: &str) -> String {
    Regex::new(TABLE_FORMULA)
        .expect("fixed table formula regex is valid")
        .replace_all(value, |captures: &regex::Captures<'_>| {
            format!(
                "<eq>{}</eq>",
                captures
                    .get(1)
                    .or_else(|| captures.get(2))
                    .expect("formula match has a body")
                    .as_str()
                    .trim()
            )
        })
        .into_owned()
}

fn outer_equation_delimiter(value: &str) -> Option<&str> {
    for (open, close) in [("$$", "$$"), (r"\[", r"\]"), (r"\(", r"\)")] {
        if value.len() >= open.len() + close.len()
            && value.starts_with(open)
            && value.ends_with(close)
        {
            return Some(&value[open.len()..value.len() - close.len()]);
        }
    }
    None
}

fn normalize_equation(value: &str) -> String {
    let mut value = value.trim();
    while let Some(inner) = outer_equation_delimiter(value) {
        value = inner.trim();
    }
    let value = normalize_left_right(value);
    remove_unmatched_braces(&value)
}

fn normalize_left_right(value: &str) -> String {
    let begin = Regex::new(r"\\begin\s*\{")
        .expect("fixed equation begin regex is valid")
        .find_iter(value)
        .count();
    let end = Regex::new(r"\\end\s*\{")
        .expect("fixed equation end regex is valid")
        .find_iter(value)
        .count();
    if begin != end {
        return value.to_owned();
    }

    let commands =
        Regex::new(r"\\(?:left|right)\b").expect("fixed equation command regex is valid");
    let mut output = String::new();
    let mut last = 0;
    let mut open: usize = 0;
    for command in commands.find_iter(value) {
        output.push_str(&value[last..command.start()]);
        if command.as_str() == r"\right" && open == 0 {
            output.push_str(r"\left.");
        }
        output.push_str(command.as_str());
        if command.as_str() == r"\left" {
            open += 1;
        } else {
            open = open.saturating_sub(1);
        }
        last = command.end();
    }
    output.push_str(&value[last..]);
    output.push_str(&r"\right.".repeat(open));
    output
}

fn remove_unmatched_braces(value: &str) -> String {
    let mut remove = vec![false; value.len()];
    let mut opens = Vec::new();
    let mut slashes = 0;
    for (index, character) in value.char_indices() {
        if character == '\\' {
            slashes += 1;
            continue;
        }
        let escaped = slashes % 2 == 1;
        slashes = 0;
        match character {
            '{' if !escaped => opens.push(index),
            '}' if !escaped => {
                if opens.pop().is_none() {
                    remove[index] = true;
                }
            }
            _ => {}
        }
    }
    for index in opens {
        remove[index] = true;
    }
    value
        .char_indices()
        .filter_map(|(index, character)| (!remove[index]).then_some(character))
        .collect()
}

fn clean_text(value: &str) -> String {
    let value = display_to_inline(value);
    let value = fix_inline_macro_spacing(&value);
    move_blank_underscores(&value)
}

fn display_to_inline(value: &str) -> String {
    let Some(start) = value.find(r"\[") else {
        return value.to_owned();
    };
    let body = &value[start + 2..];
    let Some(end) = body.find(r"\]") else {
        return value.to_owned();
    };
    if body[end + 2..].contains(r"\]") || value[start + 2..].contains(r"\[") {
        return value.to_owned();
    }
    let inner = body[..end].trim();
    let range_only = !inner.is_empty()
        && inner.chars().all(|character| {
            character.is_ascii_digit() || character.is_whitespace() || ",.-".contains(character)
        });
    if inner.is_empty()
        || range_only
        || inner.contains(['\n', '\r', '&', '$'])
        || [r"\\", r"\begin", r"\end", r"\tag", r"\label"]
            .iter()
            .any(|needle| inner.contains(needle))
    {
        return value.to_owned();
    }
    format!(r"{}\({}\){}", &value[..start], inner, &body[end + 2..])
}

fn fix_inline_macro_spacing(value: &str) -> String {
    let inline = Regex::new(r"(?s)\\\((.*?)\\\)").expect("fixed inline math regex is valid");
    let macro_suffix = Regex::new(r"\\(?:cong|to|times|subset|in)([A-Za-z])([^A-Za-z]|$)")
        .expect("fixed macro spacing regex is valid");
    inline
        .replace_all(value, |captures: &regex::Captures<'_>| {
            let inner = captures.get(1).expect("inline math has a body").as_str();
            let spaced = macro_suffix.replace_all(inner, |suffix: &regex::Captures<'_>| {
                let matched = suffix.get(0).expect("macro suffix has a match").as_str();
                let letter = suffix.get(1).expect("macro suffix has a letter").as_str();
                let boundary = suffix.get(2).expect("macro suffix has a boundary").as_str();
                let complete = &matched[..matched.len() - boundary.len()];
                if matches!(complete, r"\top" | r"\int" | r"\inf") {
                    matched.to_owned()
                } else {
                    format!(
                        "{} {letter}{boundary}",
                        &complete[..complete.len() - letter.len()]
                    )
                }
            });
            format!(r"\({spaced}\)")
        })
        .into_owned()
}

fn move_blank_underscores(value: &str) -> String {
    let inline = Regex::new(r"(?s)\\\((.*?)\\\)").expect("fixed inline math regex is valid");
    let blank = Regex::new(r"_{3,}").expect("fixed blank regex is valid");
    inline
        .replace_all(value, |captures: &regex::Captures<'_>| {
            let inner = captures.get(1).expect("inline math has a body").as_str();
            let blanks: Vec<_> = blank
                .find_iter(inner)
                .filter(|found| {
                    inner[..found.start()]
                        .chars()
                        .next_back()
                        .is_none_or(char::is_whitespace)
                        && inner[found.end()..]
                            .chars()
                            .next()
                            .is_none_or(char::is_whitespace)
                })
                .map(|found| (found.start(), found.end()))
                .collect();
            if blanks.is_empty()
                || inner[..blanks[0].0].trim().is_empty()
                || inner[blanks.last().expect("nonempty blanks").1..]
                    .trim()
                    .is_empty()
            {
                return captures
                    .get(0)
                    .expect("inline math has a match")
                    .as_str()
                    .to_owned();
            }

            let mut pieces = Vec::new();
            let mut last = 0;
            for (start, end) in blanks {
                if !inner[last..start].trim().is_empty() {
                    pieces.push(format!(r"\({}\)", inner[last..start].trim()));
                }
                pieces.push(inner[start..end].to_owned());
                last = end;
            }
            if !inner[last..].trim().is_empty() {
                pieces.push(format!(r"\({}\)", inner[last..].trim()));
            }
            pieces.join(" ")
        })
        .into_owned()
}

fn otsl_html(value: &str) -> String {
    if !["<nl>", "<fcel>", "<ecel>", "<lcel>", "<ucel>", "<xcel>"]
        .iter()
        .any(|token| value.contains(token))
    {
        return value.to_owned();
    }
    let token = Regex::new(r"(?i)<(nl|fcel|ecel|lcel|ucel|xcel)>")
        .expect("fixed OTSL token regex is valid");
    let mut rows: Vec<Vec<Cell>> = vec![Vec::new()];
    let mut last = 0;
    let mut current: Option<(usize, usize)> = None;
    for hit in token.captures_iter(value) {
        let matched = hit.get(0).expect("OTSL token has a full match");
        let before = value[last..matched.start()].trim();
        if let Some((row, column)) = current.take() {
            rows[row][column].text.push_str(before);
        } else if !before.is_empty() && rows.last().expect("OTSL has a row").is_empty() {
            rows.last_mut()
                .expect("OTSL has a row")
                .push(Cell::new(before.to_owned()));
        }

        match hit[1].to_ascii_lowercase().as_str() {
            "nl" => rows.push(Vec::new()),
            "fcel" => {
                let row = rows.len() - 1;
                let column = rows[row].len();
                rows[row].push(Cell::new(String::new()));
                current = Some((row, column));
            }
            "ecel" => rows
                .last_mut()
                .expect("OTSL has a row")
                .push(Cell::new(String::new())),
            "lcel" => extend_left(&mut rows),
            "ucel" => extend_up(&mut rows),
            "xcel" => extend_both(&mut rows),
            _ => unreachable!("OTSL regex only matches known tokens"),
        }
        last = matched.end();
    }
    if let Some((row, column)) = current {
        rows[row][column].text.push_str(value[last..].trim());
    }
    rows.retain(|row| !row.is_empty());
    if rows.is_empty() {
        return value.to_owned();
    }
    let body = rows
        .into_iter()
        .map(|row| {
            format!(
                "<tr>{}</tr>",
                row.into_iter().map(Cell::html).collect::<String>()
            )
        })
        .collect::<String>();
    format!("<table>{body}</table>")
}

#[derive(Default)]
struct Cell {
    text: String,
    colspan: usize,
    rowspan: usize,
    hidden: bool,
    anchor: Option<(usize, usize)>,
}
impl Cell {
    fn new(text: String) -> Self {
        Self {
            text,
            colspan: 1,
            rowspan: 1,
            hidden: false,
            anchor: None,
        }
    }
    fn hidden(anchor: Option<(usize, usize)>) -> Self {
        Self {
            hidden: true,
            anchor,
            ..Self::default()
        }
    }
    fn html(self) -> String {
        if self.hidden {
            return String::new();
        }
        let col = if self.colspan > 1 {
            format!(r#" colspan="{}""#, self.colspan)
        } else {
            String::new()
        };
        let row = if self.rowspan > 1 {
            format!(r#" rowspan="{}""#, self.rowspan)
        } else {
            String::new()
        };
        format!("<td{col}{row}>{}</td>", escape_cell(&self.text))
    }
}

fn owner_at(rows: &[Vec<Cell>], row: usize, column: usize) -> Option<(usize, usize)> {
    let cell = rows.get(row)?.get(column)?;
    if cell.hidden {
        cell.anchor
    } else {
        Some((row, column))
    }
}

fn extend_colspan(rows: &mut [Vec<Cell>], anchor: (usize, usize)) {
    rows[anchor.0][anchor.1].colspan += 1;
}

fn extend_rowspan(rows: &mut [Vec<Cell>], anchor: (usize, usize)) {
    rows[anchor.0][anchor.1].rowspan += 1;
}

fn extend_left(rows: &mut [Vec<Cell>]) {
    let row = rows.len() - 1;
    let column = rows[row].len();
    let anchor = column
        .checked_sub(1)
        .and_then(|column| owner_at(rows, row, column));
    if let Some(anchor) = anchor {
        extend_colspan(rows, anchor);
    }
    rows[row].push(Cell::hidden(anchor));
}

fn extend_up(rows: &mut [Vec<Cell>]) {
    let row = rows.len() - 1;
    let column = rows[row].len();
    let anchor = row
        .checked_sub(1)
        .and_then(|row| owner_at(rows, row, column));
    if let Some(anchor) = anchor {
        extend_rowspan(rows, anchor);
    }
    rows[row].push(Cell::hidden(anchor));
}

fn extend_both(rows: &mut [Vec<Cell>]) {
    let row = rows.len() - 1;
    let column = rows[row].len();
    let left = column
        .checked_sub(1)
        .and_then(|column| owner_at(rows, row, column));
    let up = row
        .checked_sub(1)
        .and_then(|row| owner_at(rows, row, column));
    let anchor = match (left, up) {
        (Some(left), Some(up)) if left == up => Some(left),
        (Some(left), None) => {
            extend_colspan(rows, left);
            Some(left)
        }
        (None, Some(up)) => {
            extend_rowspan(rows, up);
            Some(up)
        }
        (Some(_), Some(up)) => Some(up),
        (None, None) => None,
    };
    rows[row].push(Cell::hidden(anchor));
}

fn escape_attr(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
}

fn escape_cell(value: &str) -> String {
    let image = Regex::new(r#"<img src="data:[^"]*"/>"#).expect("fixed image tag regex is valid");
    let mut out = String::new();
    let mut at = 0;
    for found in image.find_iter(value) {
        out.push_str(&escape_attr(&value[at..found.start()]));
        out.push_str(found.as_str());
        at = found.end();
    }
    out.push_str(&escape_attr(&value[at..]));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::NormalizedBbox;
    use serde_json::{Map, Value};

    fn block(kind: &str, content: &str) -> ContentBlock {
        ContentBlock {
            kind: BlockKind::new(kind),
            bbox: NormalizedBbox::new(0., 0., 1., 1.).unwrap(),
            angle: None,
            content: Some(content.into()),
            merge_previous: false,
            metadata: Map::new(),
        }
    }

    fn boxed(
        kind: &str,
        content: Option<&str>,
        left: f32,
        top: f32,
        right: f32,
        bottom: f32,
    ) -> ContentBlock {
        ContentBlock {
            kind: BlockKind::new(kind),
            bbox: NormalizedBbox::new(left, top, right, bottom).unwrap(),
            angle: None,
            content: content.map(str::to_owned),
            merge_previous: false,
            metadata: Map::new(),
        }
    }

    #[test]
    fn class_output_variants_keep_their_semantics() {
        let mut table = block(
            BlockKind::IMAGE,
            "<|class_start|>\npure_table\n<|class_end|><|content_start|><fcel>A<lcel><|content_end|>",
        );
        let response = table.content.clone().unwrap();
        clean_block(&mut table, response);
        assert_eq!(table.kind.as_str(), BlockKind::TABLE);
        assert_eq!(
            table.content.as_deref(),
            Some("<table><tr><td colspan=\"2\">A</td></tr></table>")
        );

        let mut formula = block(
            BlockKind::IMAGE,
            "<|class_start|>pure_formula<|class_end|><|content_start|>\\[x\\]<|content_end|>",
        );
        let response = formula.content.clone().unwrap();
        clean_block(&mut formula, response);
        assert_eq!(formula.kind.as_str(), BlockKind::EQUATION);
        assert_eq!(formula.content.as_deref(), Some("x"));

        let mut chart = block(
            BlockKind::CHART,
            "<|CLASS_START|>chart<|CLASS_END|>\nsub-class: Bar Chart\n<content>January: 4</content>",
        );
        let response = chart.content.clone().unwrap();
        clean_block(&mut chart, response);
        assert_eq!(chart.kind.as_str(), BlockKind::CHART);
        assert_eq!(chart.content.as_deref(), Some("January: 4"));
        assert_eq!(chart.metadata["sub_type"], "Bar Chart");

        let mut image = block(
            BlockKind::IMAGE,
            "<|class_start|>natural_image<|class_end|>\ncaption: A red kite\n<content>Bird\nabove water</content>",
        );
        let response = image.content.clone().unwrap();
        clean_block(&mut image, response);
        assert_eq!(image.kind.as_str(), BlockKind::IMAGE);
        assert_eq!(image.content.as_deref(), Some("Bird\nabove water"));
        assert_eq!(image.metadata["caption"], "A red kite");
    }

    #[test]
    fn otsl_images_and_table_formula_are_normalized() {
        let mut b = block(
            BlockKind::TABLE,
            "<fcel>A & B<lcel><nl><fcel>[ABCD] \\(x\\)",
        );
        b.metadata.insert(
            "_table_image_token_map".into(),
            serde_json::json!({"[ABCD]":"data:image/png;base64,x"}),
        );
        let response = b.content.clone().unwrap();
        clean_block(&mut b, response);
        assert_eq!(
            b.content.unwrap(),
            "<table><tr><td colspan=\"2\">A &amp; B</td></tr><tr><td><img src=\"data:image/png;base64,x\"/> <eq>x</eq></td></tr></table>"
        );
    }

    #[test]
    fn otsl_hidden_cells_produce_real_row_and_col_spans() {
        assert_eq!(
            otsl_html("<fcel>A<lcel><nl><ucel><xcel>"),
            "<table><tr><td colspan=\"2\" rowspan=\"2\">A</td></tr><tr></tr></table>"
        );
    }

    #[test]
    fn table_formula_regex_matches_literal_latex_delimiters() {
        assert_eq!(formulas(r"\(x\) \[x\]"), "<eq>x</eq> <eq>x</eq>");
    }

    #[test]
    fn equations_and_text_are_cleaned_without_changing_ambiguous_math() {
        assert_eq!(normalize_equation(r"$$\left(x\right]$$"), r"\left(x\right]");
        assert_eq!(normalize_equation(r"\[\left(x\]"), r"\left(x\right.");
        assert_eq!(normalize_equation(r"\frac{a}{b"), r"\frac{a}b");
        assert_eq!(
            clean_text(r"Use \[\timesX ___ = y\] now."),
            r"Use \(\times X\) ___ \(= y\) now."
        );
        assert_eq!(clean_text(r"Range \[1-3\] stays."), r"Range \[1-3\] stays.");
        assert_eq!(
            clean_text(r"Keep \(\int\) intact."),
            r"Keep \(\int\) intact."
        );
        assert_eq!(
            clean_text(r"中文😀 \(甲 ___ 乙🚀\) 结束"),
            r"中文😀 \(甲\) ___ \(乙🚀\) 结束"
        );
    }

    #[test]
    fn containers_follow_real_children_without_promoting_empty_equation_blocks() {
        let mut blocks = vec![
            boxed(BlockKind::EQUATION_BLOCK, None, 0., 0., 1., 1.),
            boxed(BlockKind::EQUATION, Some(r"\(a\)"), 0., 0., 0.4, 1.),
            boxed(BlockKind::EQUATION, Some(r"\(b\)"), 0.5, 0., 1., 1.),
            boxed(BlockKind::EQUATION_BLOCK, None, 0., 0., 0.1, 0.1),
        ];
        post_process(&mut blocks);
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0].kind.as_str(), BlockKind::EQUATION);
        assert_eq!(
            blocks[0].content.as_deref(),
            Some(r"\begin{array}{l} a \\ b \end{array}")
        );
        assert_eq!(blocks[1].kind.as_str(), BlockKind::EQUATION_BLOCK);
    }

    #[test]
    fn page_removes_absorbed_and_keeps_semantic_equation_blocks() {
        let mut absorbed = block(BlockKind::IMAGE, "gone");
        absorbed
            .metadata
            .insert("_absorbed_by_table".into(), Value::from(0));
        let mut blocks = vec![
            block(BlockKind::EQUATION_BLOCK, r"\(x\)"),
            block(BlockKind::LIST_ITEM, "- item"),
            absorbed,
        ];
        post_process(&mut blocks);
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0].kind.as_str(), BlockKind::EQUATION);
        assert_eq!(blocks[0].content.as_deref(), Some("x"));
        assert_eq!(blocks[1].kind.as_str(), BlockKind::TEXT);
    }
}
