//! Normalization of a validated `CreateTaskRequest` into task-level state.
//!
//! Mirrors upstream `_read_submission`: deduplicate `output_formats` while
//! preserving order and default each file's `page_range` from the task value.

use crate::official_api::wire::{
    CreateTaskRequest, TaskFileInput, TaskOutputFormat, UploadTaskOptions,
};

pub(in crate::official_api::server) struct Submission {
    pub options: UploadTaskOptions,
    pub files: Vec<TaskFileInput>,
}

/// `list(dict.fromkeys(...))`: stable deduplication.
pub(in crate::official_api::server) fn normalize(req: CreateTaskRequest) -> Submission {
    let CreateTaskRequest {
        tier,
        ocr_mode,
        page_range,
        output_formats,
        files,
    } = req;
    let mut formats: Vec<TaskOutputFormat> = Vec::new();
    for format in output_formats {
        if !formats.contains(&format) {
            formats.push(format);
        }
    }
    let options = UploadTaskOptions {
        tier,
        ocr_mode,
        page_range: normalize_page_range(&page_range),
        output_formats: formats,
    };
    let files = files
        .into_iter()
        .map(|mut file| {
            let raw = file
                .page_range
                .take()
                .unwrap_or_else(|| options.page_range.clone());
            file.page_range = Some(normalize_page_range(&raw));
            file
        })
        .collect();
    Submission { options, files }
}

/// `normalize_page_range_input`: trim, merge absolute ranges, keep `all`.
pub(in crate::official_api::server) fn normalize_page_range(raw: &str) -> String {
    let value = raw.trim();
    if value.is_empty() {
        return String::new();
    }
    if value == "all" {
        return value.into();
    }
    let parts: Vec<&str> = value.split(',').map(str::trim).collect();
    let absolute = parts.iter().all(|part| {
        let (start, end) = part.split_once('-').unwrap_or((part, part));
        !start.trim().starts_with('r') && !end.trim().starts_with('r')
    });
    if !absolute {
        return parts.join(",");
    }
    let mut ranges: Vec<(u32, u32)> = parts
        .iter()
        .filter_map(|part| {
            let (start, end) = part.split_once('-').unwrap_or((part, part));
            let start = start.trim().parse::<u32>().ok()?;
            let end = end.trim().parse::<u32>().ok()?;
            Some((start, end))
        })
        .collect();
    ranges.sort_unstable();
    let mut merged: Vec<(u32, u32)> = Vec::with_capacity(ranges.len());
    for (start, end) in ranges {
        match merged.last_mut() {
            Some(last) if start <= last.1.saturating_add(1) => last.1 = last.1.max(end),
            _ => merged.push((start, end)),
        }
    }
    merged
        .into_iter()
        .map(|(start, end)| {
            if start == end {
                start.to_string()
            } else {
                format!("{start}-{end}")
            }
        })
        .collect::<Vec<_>>()
        .join(",")
}
