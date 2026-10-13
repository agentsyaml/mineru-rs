use super::{OUTPUT_MAX, OfficialApiError, OfficialApiResult};
use reqwest::{Response, StatusCode};
use serde_json::Value;
use std::{fmt, io::Read};

pub(super) async fn read_limited(response: Response, limit: usize) -> OfficialApiResult<Vec<u8>> {
    if response.content_length().is_some_and(|n| n > limit as u64) {
        return Err(OfficialApiError::TooLarge);
    }
    let mut stream = response.bytes_stream();
    let mut data = Vec::new();
    use futures_util::StreamExt;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(transport)?;
        if data.len().saturating_add(chunk.len()) > limit {
            return Err(OfficialApiError::TooLarge);
        }
        data.extend_from_slice(&chunk);
    }
    Ok(data)
}

pub(super) fn transport(_error: impl fmt::Display) -> OfficialApiError {
    OfficialApiError::Transport("network request failed".into())
}
pub(super) fn http_error(status: StatusCode, bytes: &[u8]) -> OfficialApiError {
    let message = serde_json::from_slice::<Value>(bytes)
        .ok()
        .and_then(|v| {
            v.pointer("/error/message")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .unwrap_or_else(|| "request rejected".into());
    OfficialApiError::Http { status, message }
}
pub(super) fn remote_file_error(file: &Value) -> OfficialApiError {
    let error = file.get("error").cloned().unwrap_or_default();
    OfficialApiError::File {
        error_type: error
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("remote_error")
            .into(),
        code: error
            .get("code")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .into(),
        message: error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("file parse failed")
            .into(),
    }
}
pub(super) fn result_metadata(state: &Value) -> Value {
    let mut metadata = serde_json::json!({});
    for key in [
        "job_id",
        "status",
        "created_at",
        "started_at",
        "finished_at",
        "tier",
        "output_formats",
        "access_level",
        "progress",
    ] {
        if let Some(value) = state.get(key) {
            metadata[key] = value.clone();
        }
    }
    metadata["files"] = serde_json::json!([state.pointer("/files/0")]);
    metadata
}
pub(super) fn mime_for(name: &str) -> &'static str {
    match name
        .rsplit('.')
        .next()
        .unwrap_or("")
        .to_ascii_lowercase()
        .as_str()
    {
        "pdf" => "application/pdf",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "zip" => "application/zip",
        "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        _ => "application/octet-stream",
    }
}
pub(super) fn segment(id: &str) -> String {
    url::form_urlencoded::byte_serialize(id.as_bytes()).collect()
}
pub(super) fn validate_page_range(value: &str) -> OfficialApiResult<()> {
    crate::official_api::wire::validate_page_range(value)
        .map_err(|error| OfficialApiError::Invalid(error.message.into()))?;
    Ok(())
}
pub(super) fn remaining_budget(limit: usize, used: usize) -> OfficialApiResult<usize> {
    limit.checked_sub(used).ok_or(OfficialApiError::TooLarge)
}

pub(super) fn validate_zip(bytes: &[u8]) -> OfficialApiResult<()> {
    validate_zip_with_limits(bytes, 4096, OUTPUT_MAX)
}
fn validate_zip_with_limits(
    bytes: &[u8],
    max_entries: usize,
    max_expanded: usize,
) -> OfficialApiResult<()> {
    let mut cursor = std::io::Cursor::new(bytes);
    let scanned = crate::mineru_api::zip_scan::scan(
        &mut cursor,
        crate::mineru_api::zip_scan::ScanLimits::production(max_entries as u64),
    )
    .map_err(|error| match error {
        crate::mineru_api::zip_scan::ScanError::Limit => OfficialApiError::TooLarge,
        crate::mineru_api::zip_scan::ScanError::Fallback => {
            OfficialApiError::Protocol("unverifiable ZIP artifact".into())
        }
    })?;
    if scanned.count > max_entries as u64 {
        return Err(OfficialApiError::TooLarge);
    }
    let mut archive = zip::ZipArchive::new(std::io::Cursor::new(bytes))
        .map_err(|_| OfficialApiError::Protocol("invalid ZIP artifact".into()))?;
    if archive.len() > max_entries {
        return Err(OfficialApiError::TooLarge);
    }
    let mut names = std::collections::HashSet::new();
    let mut expanded = 0usize;
    for index in 0..archive.len() {
        let mut entry = archive
            .by_index(index)
            .map_err(|_| OfficialApiError::Protocol("invalid ZIP entry".into()))?;
        let name = entry.name();
        let path = name.strip_suffix('/').unwrap_or(name);
        let components: Vec<_> = path.split('/').collect();
        if name.contains('\\')
            || name.starts_with('/')
            || components
                .iter()
                .any(|part| part.is_empty() || *part == "." || *part == "..")
            || components
                .first()
                .is_some_and(|part| part.as_bytes().get(1) == Some(&b':'))
            || !names.insert(path.to_owned())
        {
            return Err(OfficialApiError::Protocol(
                "unsafe or duplicate ZIP path".into(),
            ));
        }
        if entry
            .unix_mode()
            .is_some_and(|mode| mode & 0o170000 == 0o120000)
        {
            return Err(OfficialApiError::Protocol(
                "ZIP symlink is not permitted".into(),
            ));
        }
        let mut buffer = [0u8; 8192];
        loop {
            let count = entry
                .read(&mut buffer)
                .map_err(|_| OfficialApiError::Protocol("invalid ZIP data".into()))?;
            if count == 0 {
                break;
            }
            expanded = expanded
                .checked_add(count)
                .ok_or(OfficialApiError::TooLarge)?;
            if expanded > max_expanded {
                return Err(OfficialApiError::TooLarge);
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn uses_wire_page_range_grammar() {
        for value in ["", "all", " r3-r1, 5 ", "r1-r1", "1-3", "3-r1", "r3-5"] {
            assert!(validate_page_range(value).is_ok(), "{value}");
        }
        for value in ["r1-r3", "-1", "+1", "r0", "01", "1,,2"] {
            assert!(validate_page_range(value).is_err(), "{value}");
        }
    }
    #[test]
    fn batch_budget_rejects_aggregate_overflow() {
        assert_eq!(remaining_budget(10, 7).unwrap(), 3);
        assert!(remaining_budget(10, 11).is_err());
    }
    #[test]
    fn enforces_archive_path_and_expansion_limits() {
        use std::io::{Cursor, Write};
        fn archive(entries: &[(&str, &[u8], u32)]) -> Vec<u8> {
            let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
            for (name, bytes, mode) in entries {
                let options = zip::write::SimpleFileOptions::default().unix_permissions(*mode);
                writer.start_file(*name, options).unwrap();
                writer.write_all(bytes).unwrap();
            }
            writer.finish().unwrap().into_inner()
        }
        assert!(validate_zip(&archive(&[("valid.txt", b"ok", 0o100644)])).is_ok());
        assert!(validate_zip(&archive(&[("../x", b"x", 0o100644)])).is_err());
        assert!(validate_zip(&archive(&[("./x", b"x", 0o100644)])).is_err());
        assert!(validate_zip(&archive(&[("a", b"x", 0o100644), ("a/", b"x", 0o100644)])).is_err());
        let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
        writer
            .add_symlink("link", "target", zip::write::SimpleFileOptions::default())
            .unwrap();
        let bytes = writer.finish().unwrap().into_inner();
        let mut zip = zip::ZipArchive::new(Cursor::new(&bytes)).unwrap();
        assert_eq!(
            zip.by_index(0).unwrap().unix_mode().unwrap() & 0o170000,
            0o120000
        );
        assert!(validate_zip(&bytes).is_err());
        assert!(matches!(
            validate_zip_with_limits(
                &archive(&[("one", b"x", 0o100644), ("two", b"x", 0o100644)]),
                1,
                8
            ),
            Err(OfficialApiError::TooLarge)
        ));
        assert!(
            validate_zip_with_limits(&archive(&[("large", &[b'x'; 4096], 0o100644)]), 8, 1024)
                .is_err()
        );
    }
}
