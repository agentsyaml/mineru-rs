use crate::official::{OfficialError, OfficialErrorKind};
use serde_json::{Value, json};
use std::sync::atomic::{AtomicBool, Ordering};

pub(super) fn render_html(
    rows: &[Vec<String>],
    width: usize,
    header: bool,
    cancelled: Option<&AtomicBool>,
    limit: usize,
) -> Result<String, OfficialError> {
    let mut out = String::new();
    push_bounded(&mut out, "<table>", limit)?;
    for (index, row) in rows.iter().enumerate() {
        push_bounded(&mut out, "\n  <tr>", limit)?;
        let tag = if header && index == 0 { "th" } else { "td" };
        for col in 0..width {
            check_cancel(cancelled)?;
            let cell = row.get(col).map_or("", String::as_str);
            push_bounded(&mut out, "\n    <", limit)?;
            push_bounded(&mut out, tag, limit)?;
            push_bounded(&mut out, ">", limit)?;
            let mut chars = cell.chars().peekable();
            while let Some(ch) = chars.next() {
                check_cancel(cancelled)?;
                let encoded = if ch == '\r' {
                    if chars.peek() == Some(&'\n') {
                        chars.next();
                    }
                    "<br>"
                } else if ch == '\n' {
                    "<br>"
                } else if ch == '&' {
                    "&amp;"
                } else if ch == '<' {
                    "&lt;"
                } else if ch == '>' {
                    "&gt;"
                } else if ch == '"' {
                    "&quot;"
                } else if ch == '\'' {
                    "&#x27;"
                } else if disallowed_control(ch) {
                    "\u{fffd}"
                } else {
                    push_char_bounded(&mut out, ch, limit)?;
                    continue;
                };
                push_bounded(&mut out, encoded, limit)?;
            }
            push_bounded(&mut out, "</", limit)?;
            push_bounded(&mut out, tag, limit)?;
            push_bounded(&mut out, ">", limit)?;
        }
        push_bounded(&mut out, "\n  </tr>", limit)?;
    }
    push_bounded(&mut out, "\n</table>", limit)?;
    Ok(out)
}

pub(super) fn render_markdown(
    rows: &[Vec<String>],
    width: usize,
    _header: bool,
    cancelled: Option<&AtomicBool>,
    limit: usize,
) -> Result<String, OfficialError> {
    if rows.is_empty() || width == 0 {
        return Ok(String::new());
    }
    let mut out = String::new();
    for (index, row) in rows.iter().enumerate() {
        if index == 1 {
            push_separator(&mut out, width, limit)?;
        }
        push_bounded(&mut out, "|", limit)?;
        for column in 0..width {
            check_cancel(cancelled)?;
            push_bounded(&mut out, " ", limit)?;
            markdown_cell(
                &mut out,
                row.get(column).map_or("", String::as_str),
                cancelled,
                limit,
            )?;
            push_bounded(&mut out, " |", limit)?;
        }
        push_bounded(&mut out, "\n", limit)?;
    }
    if rows.len() == 1 {
        push_separator(&mut out, width, limit)?;
    }
    // Document-level rendering strips the generated final row newline.
    out.pop();
    Ok(out)
}

fn markdown_cell(
    out: &mut String,
    value: &str,
    cancelled: Option<&AtomicBool>,
    limit: usize,
) -> Result<(), OfficialError> {
    let value = trim_cell_edges(value);
    let mut chars = value.chars().peekable();
    let mut line_has_text = false;
    let mut pending_space = false;
    while let Some(ch) = chars.next() {
        check_cancel(cancelled)?;
        if ch == '\r' || ch == '\n' {
            if ch == '\r' && chars.peek() == Some(&'\n') {
                chars.next();
            }
            push_bounded(out, "<br>", limit)?;
            line_has_text = false;
            pending_space = false;
        } else if matches!(ch, ' ' | '\t') {
            pending_space = line_has_text;
        } else {
            if pending_space {
                push_bounded(out, " ", limit)?;
            }
            pending_space = false;
            let encoded = if ch == '&' {
                "&amp;"
            } else if ch == '<' {
                "&lt;"
            } else if ch == '\\' {
                "\\\\"
            } else if ch == '|' {
                "\\|"
            } else if disallowed_control(ch) {
                "\u{fffd}"
            } else {
                push_char_bounded(out, ch, limit)?;
                line_has_text = true;
                continue;
            };
            push_bounded(out, encoded, limit)?;
            line_has_text = true;
        }
    }
    Ok(())
}

fn trim_cell_edges(value: &str) -> &str {
    let is_edge_space = |ch: char| {
        // CR/LF are rendered as <br>; only characters that remain edge whitespace are trimmed.
        ch != '\r' && ch != '\n' && ch.is_whitespace() && !disallowed_control(ch)
    };
    let start = value
        .char_indices()
        .find_map(|(index, ch)| (!is_edge_space(ch)).then_some(index))
        .unwrap_or(value.len());
    let end = value
        .char_indices()
        .rev()
        .find_map(|(index, ch)| (!is_edge_space(ch)).then_some(index + ch.len_utf8()))
        .unwrap_or(start);
    &value[start..end]
}

fn disallowed_control(ch: char) -> bool {
    matches!(ch as u32, 0x00..=0x08 | 0x0b | 0x0c | 0x0e..=0x1f | 0x7f)
}

fn push_separator(out: &mut String, width: usize, limit: usize) -> Result<(), OfficialError> {
    push_bounded(out, "|", limit)?;
    for _ in 0..width {
        push_bounded(out, " --- |", limit)?;
    }
    push_bounded(out, "\n", limit)
}

fn push_bounded(out: &mut String, value: &str, limit: usize) -> Result<(), OfficialError> {
    if out
        .len()
        .checked_add(value.len())
        .is_none_or(|length| length > limit)
    {
        return Err(OfficialError::new(
            OfficialErrorKind::LimitExceeded,
            "CSV exceeds rendered output budget",
        ));
    }
    out.push_str(value);
    Ok(())
}

fn push_char_bounded(out: &mut String, value: char, limit: usize) -> Result<(), OfficialError> {
    if out
        .len()
        .checked_add(value.len_utf8())
        .is_none_or(|length| length > limit)
    {
        return Err(OfficialError::new(
            OfficialErrorKind::LimitExceeded,
            "CSV exceeds rendered output budget",
        ));
    }
    out.push(value);
    Ok(())
}

fn check_cancel(cancelled: Option<&AtomicBool>) -> Result<(), OfficialError> {
    if cancelled.is_some_and(|flag| flag.load(Ordering::Relaxed)) {
        Err(OfficialError::new(
            OfficialErrorKind::Cancelled,
            "CSV parsing cancelled",
        ))
    } else {
        Ok(())
    }
}

pub(super) fn structured_content(middle: &Value, markdown: &str) -> Value {
    let mut structured = middle.clone();
    if let Some(object) = structured.as_object_mut() {
        object.remove("schema");
        object.remove("schema_version");
        object.remove("extensions");
    }
    if let Some(block) = structured
        .get_mut("pages")
        .and_then(Value::as_array_mut)
        .and_then(|pages| pages.first_mut())
        .and_then(|page| page.get_mut("blocks"))
        .and_then(Value::as_array_mut)
        .and_then(|blocks| blocks.first_mut())
        .and_then(Value::as_object_mut)
    {
        block.remove("index");
        block.remove("bbox");
        block.insert("content".into(), Value::String(markdown.to_owned()));
        block.insert("captions".into(), json!([]));
        block.insert("footnotes".into(), json!([]));
    }
    structured
}
