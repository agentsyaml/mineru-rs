// Query-string parsing for the task ingestion endpoints: a strict allowlist
// shared by `/v1/tasks` and `/v1/file_parse`.
use crate::official_api::wire::WireError;

/// Query-string options for non-JSON submissions (multipart / octet-stream).
#[derive(Default)]
pub(in crate::official_api::server) struct IngestQuery {
    pub tier: Option<String>,
    pub ocr_mode: Option<String>,
    pub page_range: Option<String>,
    pub output_formats: Vec<String>,
    pub filename: Option<String>,
    pub sha256sum: Option<String>,
    pub response_format: Option<String>,
    pub wait_timeout: Option<String>,
}

const QUERY_KEYS: [&str; 8] = [
    "tier",
    "ocr_mode",
    "page_range",
    "output_formats",
    "filename",
    "sha256sum",
    "response_format",
    "wait_timeout",
];

/// Parse the query string, rejecting unknown keys.
pub(in crate::official_api::server) fn parse_query(
    query: Option<&str>,
) -> Result<IngestQuery, WireError> {
    let mut parsed = IngestQuery::default();
    for pair in query.unwrap_or("").split('&').filter(|p| !p.is_empty()) {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        let key = percent_decode(key);
        if !QUERY_KEYS.contains(&key.as_str()) {
            return Err(WireError::bad("query", "Unsupported query parameter."));
        }
        let value = percent_decode(value);
        match key.as_str() {
            "tier" => parsed.tier = Some(value),
            "ocr_mode" => parsed.ocr_mode = Some(value),
            "page_range" => parsed.page_range = Some(value),
            "filename" => parsed.filename = Some(value),
            "sha256sum" => parsed.sha256sum = Some(value),
            "response_format" => parsed.response_format = Some(value),
            "wait_timeout" => parsed.wait_timeout = Some(value),
            "output_formats" => parsed.output_formats.push(value),
            _ => unreachable!("allowlist checked above"),
        }
    }
    Ok(parsed)
}

pub(in crate::official_api::server) fn percent_decode(value: &str) -> String {
    let mut out = Vec::with_capacity(value.len());
    let bytes = value.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'%' if index + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[index + 1..index + 3]).unwrap_or("");
                match u8::from_str_radix(hex, 16) {
                    Ok(byte) => {
                        out.push(byte);
                        index += 3;
                    }
                    Err(_) => {
                        out.push(bytes[index]);
                        index += 1;
                    }
                }
            }
            b'+' => {
                out.push(b' ');
                index += 1;
            }
            byte => {
                out.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}
