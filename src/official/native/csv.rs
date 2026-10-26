use crate::official::{OfficialError, OfficialErrorKind};
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
#[path = "csv_infer.rs"]
mod csv_infer;
#[path = "csv_render.rs"]
mod csv_render;
#[path = "csv_restore.rs"]
mod csv_restore;
use csv_infer::infer_header;
use csv_render::{render_html, render_markdown, structured_content};
use serde_json::{Value, json};

pub(crate) const MAX_BYTES: usize = 200 * 1024 * 1024;
const MAX_ROWS: usize = 1_048_576;
const MAX_COLUMNS: usize = 16_384;
const MAX_GRID: usize = 250_000;
pub(super) const MAX_RENDERED: usize = 256 * 1024 * 1024;

#[derive(Debug)]
pub(crate) struct NativeCsvResult {
    pub model: Value,
    pub middle: Value,
    pub markdown: String,
    pub structured: Value,
}

#[cfg(test)]
pub(crate) fn analyze(bytes: &[u8], suffix: &str) -> Result<NativeCsvResult, OfficialError> {
    analyze_with_cancel(bytes, suffix, None, MAX_RENDERED)
}

pub(crate) fn analyze_with_cancel(
    bytes: &[u8],
    suffix: &str,
    cancelled: Option<&AtomicBool>,
    output_budget: usize,
) -> Result<NativeCsvResult, OfficialError> {
    if bytes.len() > MAX_BYTES {
        return Err(error(
            OfficialErrorKind::LimitExceeded,
            "CSV exceeds max_bytes",
        ));
    }
    if !matches!(suffix, "csv" | "tsv") {
        return Err(error(
            OfficialErrorKind::InvalidInput,
            "native CSV analyzer requires csv or tsv",
        ));
    }
    let text = decode(bytes)?;
    let (text, explicit) = directive(text);
    check_cancel(cancelled)?;
    let delimiter = match explicit {
        Some(delimiter) => delimiter,
        None => infer_delimiter(text, suffix, cancelled)?,
    };
    let rows = parse(text, delimiter, MAX_ROWS, false, cancelled)?;
    let width = rows.iter().map(Vec::len).max().unwrap_or(0);
    if width > MAX_COLUMNS || rows.len().saturating_mul(width) > MAX_GRID {
        return Err(error(
            OfficialErrorKind::LimitExceeded,
            "CSV exceeds max_grid_slots",
        ));
    }
    let header = infer_header(&rows);
    let output_budget = output_budget.min(MAX_RENDERED) / 5;
    let table = render_html(&rows, width, header, cancelled, output_budget)?;
    let model_blocks = if rows.is_empty() {
        Vec::new()
    } else {
        vec![json!({"type":"table","content":table})]
    };
    let middle_blocks = if rows.is_empty() {
        Vec::new()
    } else {
        vec![
            json!({"type":"table","index":0,"content":[{"type":"table_body","index":0,"content":table}]}),
        ]
    };
    let metadata = json!({"file_suffix":suffix,"producer":{"name":"mineru-rs","version":env!("CARGO_PKG_VERSION").to_owned()}});
    let model = json!({"schema":"docvortex.model","schema_version":"2.0","page_index_map":[],"pages":[model_blocks],"metadata":metadata,"extensions":{}});
    let middle = json!({
        "schema":"docvortex.middle","schema_version":"2.0",
        "metadata":metadata,
        "pages":[{"page_idx":0,"blocks":middle_blocks}],"is_full_document":true,"extensions":{}
    });
    let markdown = render_markdown(&rows, width, header, cancelled, output_budget)?;
    let structured = structured_content(&middle, &markdown);
    Ok(NativeCsvResult {
        model,
        middle,
        markdown,
        structured,
    })
}

pub(crate) fn is_native_middle(middle: &Value) -> bool {
    matches!(
        middle
            .pointer("/metadata/file_suffix")
            .and_then(Value::as_str),
        Some("csv" | "tsv")
    ) && middle
        .pointer("/metadata/producer/name")
        .and_then(Value::as_str)
        == Some("mineru-rs")
}

pub(crate) async fn restore_json(
    json: String,
    assets: std::collections::BTreeMap<String, bytes::Bytes>,
    timeout: Duration,
) -> Result<crate::official::OfficialResult, OfficialError> {
    bounded_restore(
        move |cancelled| {
            let middle: Value = serde_json::from_str(&json)
                .map_err(|e| error(OfficialErrorKind::InvalidInput, &e.to_string()))?;
            if !is_native_middle(&middle) {
                return Err(error(
                    OfficialErrorKind::InvalidInput,
                    "native CSV restore route changed",
                ));
            }
            csv_restore::restore(middle, assets, cancelled)
        },
        timeout,
    )
    .await
}

pub(crate) async fn restore_middle(
    middle: Value,
    assets: std::collections::BTreeMap<String, bytes::Bytes>,
    timeout: Duration,
) -> Result<crate::official::OfficialResult, OfficialError> {
    bounded_restore(
        move |cancelled| csv_restore::restore(middle, assets, cancelled),
        timeout,
    )
    .await
}

async fn bounded_restore(
    operation: impl FnOnce(&AtomicBool) -> Result<crate::official::OfficialResult, OfficialError>
    + Send
    + 'static,
    timeout: Duration,
) -> Result<crate::official::OfficialResult, OfficialError> {
    let cancelled = Arc::new(AtomicBool::new(false));
    let mut guard = RestoreCancelGuard(Some(cancelled.clone()));
    let task_flag = cancelled.clone();
    let task = tokio::task::spawn_blocking(move || operation(&task_flag));
    let result = match tokio::time::timeout(timeout, task).await {
        Ok(result) => result.map_err(|e| error(OfficialErrorKind::Upstream, &e.to_string()))?,
        Err(_) => {
            return Err(error(
                OfficialErrorKind::Deadline,
                "CSV restore deadline exceeded",
            ));
        }
    };
    guard.disarm();
    result
}

pub(super) struct RestoreCancelGuard(pub(super) Option<Arc<AtomicBool>>);

impl RestoreCancelGuard {
    pub(super) fn disarm(&mut self) {
        self.0 = None;
    }
}

impl Drop for RestoreCancelGuard {
    fn drop(&mut self) {
        if let Some(flag) = self.0.take() {
            flag.store(true, Ordering::Relaxed);
        }
    }
}

fn error(kind: OfficialErrorKind, message: &str) -> OfficialError {
    OfficialError::new(kind, message)
}

fn check_cancel(cancelled: Option<&AtomicBool>) -> Result<(), OfficialError> {
    if cancelled.is_some_and(|flag| flag.load(std::sync::atomic::Ordering::Relaxed)) {
        Err(error(OfficialErrorKind::Cancelled, "CSV parsing cancelled"))
    } else {
        Ok(())
    }
}

fn decode(bytes: &[u8]) -> Result<&str, OfficialError> {
    if bytes.starts_with(&[0xef, 0xbb, 0xbf]) {
        return std::str::from_utf8(&bytes[3..])
            .map_err(|_| error(OfficialErrorKind::InvalidInput, "CSV is not valid UTF-8"));
    }
    if bytes.starts_with(&[0xff, 0xfe, 0x00, 0x00]) || bytes.starts_with(&[0x00, 0x00, 0xfe, 0xff])
    {
        return Err(error(
            OfficialErrorKind::UnsupportedOperation,
            "UTF-32 CSV is not supported",
        ));
    }
    if bytes.starts_with(&[0xff, 0xfe]) || bytes.starts_with(&[0xfe, 0xff]) {
        return Err(error(
            OfficialErrorKind::UnsupportedOperation,
            "UTF-16 CSV is not supported yet",
        ));
    }
    std::str::from_utf8(bytes).map_err(|_| {
        error(
            OfficialErrorKind::UnsupportedOperation,
            "CSV encoding is unsupported; expected UTF-8",
        )
    })
}

fn directive(text: &str) -> (&str, Option<char>) {
    let line_end = text.find(['\r', '\n']).unwrap_or(text.len());
    let line = &text[..line_end];
    if let Some(value) = line.get(4..).filter(|_| line.len() == 5).filter(|_| {
        line.get(..4)
            .is_some_and(|p| p.eq_ignore_ascii_case("sep="))
    }) {
        let delim = value
            .chars()
            .next()
            .filter(|c| matches!(c, ',' | ';' | '\t' | '|'));
        if delim.is_some() {
            let next = if text[line_end..].starts_with("\r\n") {
                line_end + 2
            } else {
                line_end + 1
            };
            return (&text[next.min(text.len())..], delim);
        }
    }
    (text, None)
}

fn infer_delimiter(
    text: &str,
    suffix: &str,
    cancelled: Option<&AtomicBool>,
) -> Result<char, OfficialError> {
    let candidates = if suffix == "tsv" {
        ['\t', ',', ';', '|']
    } else {
        [',', ';', '\t', '|']
    };
    let mut best = (0usize, 0usize, 0usize);
    let mut chosen = candidates[0];
    for (priority, delimiter) in candidates.into_iter().enumerate() {
        check_cancel(cancelled)?;
        if let Ok(rows) = parse(text, delimiter, 20, true, cancelled) {
            let mut counts = std::collections::BTreeMap::new();
            for row in rows {
                *counts.entry(row.len().max(1)).or_insert(0usize) += 1;
            }
            let (width, frequency) = counts
                .into_iter()
                .max_by_key(|(width, count)| (*count, *width))
                .unwrap_or((1, 0));
            if width < 2 {
                continue;
            }
            let score = (frequency, width, 4 - priority);
            if score > best {
                chosen = delimiter;
                best = score;
            }
        }
    }
    check_cancel(cancelled)?;
    Ok(chosen)
}

fn parse(
    text: &str,
    delimiter: char,
    limit: usize,
    sample: bool,
    cancelled: Option<&AtomicBool>,
) -> Result<Vec<Vec<String>>, OfficialError> {
    let mut rows = Vec::new();
    let mut row = Vec::new();
    let mut field = String::new();
    let mut chars = text.chars().peekable();
    let mut quoted = false;
    let mut closed = false;
    let mut max_width = 0usize;
    let mut cell_count = 0usize;
    let mut checked = 0usize;
    while let Some(ch) = chars.next() {
        checked += 1;
        if checked % 8192 == 0 {
            check_cancel(cancelled)?;
        }
        if quoted {
            if ch == '"' {
                if chars.peek() == Some(&'"') {
                    chars.next();
                    field.push('"');
                } else {
                    quoted = false;
                    closed = true;
                }
            } else {
                field.push(ch);
            }
        } else if closed && ch != delimiter && ch != '\r' && ch != '\n' {
            return Err(error(
                OfficialErrorKind::InvalidInput,
                "malformed CSV: data after closing quote",
            ));
        } else if ch == delimiter {
            row.push(std::mem::take(&mut field));
            cell_count = cell_count.saturating_add(1);
            closed = false;
            if row.len() >= MAX_COLUMNS {
                return Err(error(
                    OfficialErrorKind::LimitExceeded,
                    "CSV exceeds max_columns",
                ));
            }
        } else if ch == '\r' || ch == '\n' {
            if ch == '\r' && chars.peek() == Some(&'\n') {
                chars.next();
            }
            row.push(std::mem::take(&mut field));
            cell_count = cell_count.saturating_add(1);
            rows.push(std::mem::take(&mut row));
            closed = false;
            max_width = max_width.max(rows.last().map_or(0, Vec::len));
            if cell_count > MAX_GRID || rows.len().saturating_mul(max_width) > MAX_GRID {
                return Err(error(
                    OfficialErrorKind::LimitExceeded,
                    "CSV exceeds max_grid_slots",
                ));
            }
            if rows.len() > limit {
                return Err(error(
                    OfficialErrorKind::LimitExceeded,
                    "CSV exceeds max_rows",
                ));
            }
            if sample && rows.len() == limit {
                return Ok(rows);
            }
        } else if ch == '"' && field.is_empty() && !closed {
            quoted = true;
        } else {
            field.push(ch);
        }
    }
    if quoted {
        return Err(error(
            OfficialErrorKind::InvalidInput,
            "malformed CSV: unterminated quoted field",
        ));
    }
    if !field.is_empty() || !row.is_empty() || closed {
        row.push(field);
        cell_count = cell_count.saturating_add(1);
        rows.push(row);
        max_width = max_width.max(rows.last().map_or(0, Vec::len));
        if cell_count > MAX_GRID || rows.len().saturating_mul(max_width) > MAX_GRID {
            return Err(error(
                OfficialErrorKind::LimitExceeded,
                "CSV exceeds max_grid_slots",
            ));
        }
    }
    if rows.len() > limit {
        return Err(error(
            OfficialErrorKind::LimitExceeded,
            "CSV exceeds max_rows",
        ));
    }
    Ok(rows)
}

#[cfg(test)]
#[path = "csv_tests.rs"]
mod tests;
