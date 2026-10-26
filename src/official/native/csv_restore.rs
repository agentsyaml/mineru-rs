use super::{MAX_COLUMNS, MAX_GRID, MAX_RENDERED, csv_render};
use crate::official::{OfficialError, OfficialErrorKind, OfficialResult, OfficialVersionMetadata};
use bytes::Bytes;
use serde_json::Value;
use std::{
    collections::BTreeMap,
    sync::atomic::{AtomicBool, Ordering},
};

pub(super) fn restore(
    mut middle: Value,
    assets: BTreeMap<String, Bytes>,
    cancelled: &AtomicBool,
) -> Result<OfficialResult, OfficialError> {
    if !assets.is_empty() || !super::is_native_middle(&middle) {
        return Err(invalid(
            "native CSV restore requires its own producer and no assets",
        ));
    }
    if let Some(object) = middle.as_object_mut() {
        object
            .entry("schema")
            .or_insert_with(|| Value::String("docvortex.middle".into()));
        object
            .entry("schema_version")
            .or_insert_with(|| Value::String("2.0".into()));
        object
            .entry("extensions")
            .or_insert_with(|| Value::Object(Default::default()));
    }
    if json_size(&middle, cancelled)? > crate::official::runtime::RETAINED_RESULT_CAP {
        return Err(limit());
    }
    check_keys(
        &middle,
        &[
            "schema",
            "schema_version",
            "metadata",
            "pages",
            "is_full_document",
            "extensions",
        ],
    )?;
    check_keys(&middle["metadata"], &["file_suffix", "producer"])?;
    check_keys(&middle["metadata"]["producer"], &["name", "version"])?;
    if middle
        .pointer("/metadata/producer/version")
        .and_then(Value::as_str)
        .is_none_or(str::is_empty)
        || middle["extensions"]
            .as_object()
            .is_some_and(|extensions| !extensions.is_empty())
    {
        return Err(invalid("native CSV middle JSON has unsupported metadata"));
    }
    let _suffix = middle
        .pointer("/metadata/file_suffix")
        .and_then(Value::as_str)
        .filter(|suffix| matches!(*suffix, "csv" | "tsv"))
        .ok_or_else(|| invalid("native CSV middle JSON has an invalid suffix"))?
        .to_owned();
    if middle
        .get("schema")
        .is_some_and(|v| v != "docvortex.middle")
        || middle.get("schema_version").is_some_and(|v| v != "2.0")
        || middle["is_full_document"] != true
        || middle.get("extensions").is_some_and(|extensions| {
            extensions
                .as_object()
                .is_none_or(|object| !object.is_empty())
        })
        || middle["pages"]
            .as_array()
            .is_none_or(|pages| pages.len() != 1)
        || middle["pages"][0]["page_idx"] != 0
    {
        return Err(invalid("native CSV middle JSON does not match schema 2.0"));
    }
    check_keys(&middle["pages"][0], &["page_idx", "blocks"])?;
    let blocks = middle["pages"][0]["blocks"]
        .as_array()
        .ok_or_else(|| invalid("native CSV page blocks are missing"))?;
    let rows = match blocks.as_slice() {
        [] => Vec::new(),
        [table] if table["type"] == "table" => {
            check_keys(table, &["type", "index", "content"])?;
            let table_index = block_index(table)?;
            let content = table["content"]
                .as_array()
                .filter(|items| items.len() == 1)
                .ok_or_else(|| invalid("native CSV table body is invalid"))?;
            if content[0]["type"] != "table_body" {
                return Err(invalid("native CSV table body is invalid"));
            }
            check_keys(&content[0], &["type", "index", "content"])?;
            if block_index(&content[0])? != table_index {
                return Err(invalid("native CSV table body index must match its parent"));
            }
            let markup = content[0]["content"]
                .as_str()
                .ok_or_else(|| invalid("native CSV table content is missing"))?;
            let table_budget =
                (crate::official::runtime::RETAINED_RESULT_CAP / 5).min(MAX_RENDERED);
            if markup.len() > table_budget {
                return Err(limit());
            }
            parse_table(markup, cancelled)?
        }
        _ => return Err(invalid("native CSV document has unsupported blocks")),
    };
    let width = rows.iter().map(Vec::len).max().unwrap_or(0);
    let header = !rows.is_empty()
        && middle["pages"][0]["blocks"][0]["content"][0]["content"]
            .as_str()
            .is_some_and(|html| html.contains("<th>"));
    let markdown = csv_render::render_markdown(
        &rows,
        width,
        header,
        Some(cancelled),
        (crate::official::runtime::RETAINED_RESULT_CAP / 5).min(MAX_RENDERED),
    )?;
    if markdown.len() > MAX_RENDERED {
        return Err(limit());
    }
    let structured = csv_render::structured_content(&middle, &markdown);
    let result = OfficialResult {
        middle,
        model: None,
        markdown,
        structured,
        assets,
        warnings: Vec::new(),
        metadata: OfficialVersionMetadata {
            mineru_package: format!("mineru-rs/{}", env!("CARGO_PKG_VERSION")),
            schema_version: "2.0".into(),
            bundle_name: "native-csv".into(),
        },
    };
    if result.retained_bytes() > crate::official::runtime::RETAINED_RESULT_CAP {
        return Err(limit());
    }
    Ok(result)
}

fn parse_table(html: &str, cancelled: &AtomicBool) -> Result<Vec<Vec<String>>, OfficialError> {
    let mut rows = Vec::new();
    let mut row = Vec::new();
    let mut cell = None;
    let mut cell_tag = None;
    let mut table_open = false;
    let mut row_open = false;
    let mut table_closed = false;
    let mut cell_count = 0usize;
    let mut max_width = 0usize;
    let mut iterations = 0usize;
    let mut rest = html;
    while !rest.is_empty() {
        iterations += 1;
        if iterations % 1024 == 0 {
            check_cancel(cancelled)?;
        }
        if let Some(pos) = rest.find('<') {
            if cell.is_some() {
                append_text(cell.as_mut().unwrap(), &rest[..pos], cancelled)?;
            } else if !rest[..pos].trim().is_empty() {
                return Err(invalid("invalid table whitespace"));
            }
            rest = &rest[pos..];
            let end = rest
                .find('>')
                .ok_or_else(|| invalid("malformed table markup"))?;
            let tag = &rest[1..end];
            match tag {
                "table" if !table_open && !table_closed => table_open = true,
                "/table" if table_open && !row_open && cell.is_none() => {
                    table_open = false;
                    table_closed = true;
                }
                "tr" if table_open && !row_open && cell.is_none() => {
                    row_open = true;
                    row.clear();
                }
                "/tr" if row_open && cell.is_none() => {
                    rows.push(std::mem::take(&mut row));
                    row_open = false;
                    max_width = max_width.max(rows.last().map_or(0, Vec::len));
                    if rows.len().saturating_mul(max_width) > MAX_GRID {
                        return Err(limit());
                    }
                }
                "td" | "th" if row_open && cell.is_none() => {
                    if row.len() >= MAX_COLUMNS {
                        return Err(limit());
                    }
                    cell = Some(String::new());
                    cell_tag = Some(tag);
                }
                "/td" if row_open && cell_tag == Some("td") => {
                    row.push(cell.take().unwrap());
                    cell_count += 1;
                }
                "/th" if row_open && cell_tag == Some("th") => {
                    row.push(cell.take().unwrap());
                    cell_count += 1;
                }
                "br" if cell.is_some() => cell.as_mut().unwrap().push('\n'),
                _ => return Err(invalid("unsupported native CSV table markup")),
            }
            if cell.is_none() {
                cell_tag = None;
            }
            rest = &rest[end + 1..];
        } else {
            if cell.is_some() {
                append_text(cell.as_mut().unwrap(), rest, cancelled)?;
            } else if !rest.trim().is_empty() {
                return Err(invalid("invalid table trailing data"));
            }
            rest = "";
        }
        if rows.len() > 1_048_576 || cell_count > MAX_GRID {
            return Err(limit());
        }
    }
    if !table_closed
        || table_open
        || row_open
        || cell.is_some()
        || rows.iter().any(|r| r.len() > MAX_COLUMNS)
    {
        return Err(invalid("incomplete or oversized native CSV table"));
    }
    Ok(rows)
}

fn append_text(
    target: &mut String,
    text: &str,
    cancelled: &AtomicBool,
) -> Result<(), OfficialError> {
    let mut rest = text;
    while let Some(start) = rest.find('&') {
        check_cancel(cancelled)?;
        target.push_str(&rest[..start]);
        let end = rest[start..]
            .find(';')
            .ok_or_else(|| invalid("invalid HTML entity"))?
            + start;
        target.push(match &rest[start..=end] {
            "&amp;" => '&',
            "&lt;" => '<',
            "&gt;" => '>',
            "&quot;" => '"',
            "&#x27;" => '\'',
            "&#xfffd;" | "&#xFFFD;" => '\u{fffd}',
            _ => return Err(invalid("unsupported HTML entity")),
        });
        rest = &rest[end + 1..];
    }
    target.push_str(rest);
    Ok(())
}

fn block_index(value: &Value) -> Result<u64, OfficialError> {
    value
        .get("index")
        .and_then(Value::as_u64)
        .ok_or_else(|| invalid("native CSV table blocks require a nonnegative index"))
}

fn check_keys(value: &Value, allowed: &[&str]) -> Result<(), OfficialError> {
    if value
        .as_object()
        .is_none_or(|object| object.keys().any(|key| !allowed.contains(&key.as_str())))
    {
        return Err(invalid(
            "native CSV middle JSON contains unsupported fields",
        ));
    }
    Ok(())
}

fn json_size(value: &Value, cancelled: &AtomicBool) -> Result<usize, OfficialError> {
    check_cancel(cancelled)?;
    match value {
        Value::Null => Ok(4),
        Value::Bool(value) => Ok(if *value { 4 } else { 5 }),
        Value::Number(value) => Ok(value.to_string().len()),
        Value::String(value) => Ok(json_string_size(value, cancelled)?),
        Value::Array(items) => {
            let mut size = 2usize;
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    size = size.saturating_add(1);
                }
                size = size.saturating_add(json_size(item, cancelled)?);
            }
            Ok(size)
        }
        Value::Object(items) => {
            let mut size = 2usize;
            for (i, (key, item)) in items.iter().enumerate() {
                if i > 0 {
                    size = size.saturating_add(1);
                }
                size = size
                    .saturating_add(json_string_size(key, cancelled)?)
                    .saturating_add(1)
                    .saturating_add(json_size(item, cancelled)?);
            }
            Ok(size)
        }
    }
}

fn json_string_size(value: &str, cancelled: &AtomicBool) -> Result<usize, OfficialError> {
    let mut size = 2usize;
    for (i, ch) in value.chars().enumerate() {
        if i % 8192 == 0 {
            check_cancel(cancelled)?;
        }
        size = size.saturating_add(match ch {
            '"' | '\\' | '\n' | '\r' | '\t' => 2,
            c if c <= '\u{1f}' => 6,
            c => c.len_utf8(),
        });
    }
    Ok(size)
}

fn check_cancel(cancelled: &AtomicBool) -> Result<(), OfficialError> {
    if cancelled.load(Ordering::Relaxed) {
        Err(OfficialError::new(
            OfficialErrorKind::Cancelled,
            "CSV restore cancelled",
        ))
    } else {
        Ok(())
    }
}

fn invalid(message: &str) -> OfficialError {
    OfficialError::new(OfficialErrorKind::InvalidInput, message)
}
fn limit() -> OfficialError {
    OfficialError::new(
        OfficialErrorKind::LimitExceeded,
        "native CSV result exceeds retained byte limit",
    )
}
