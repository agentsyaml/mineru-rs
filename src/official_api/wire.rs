use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, fmt};

pub const INLINE_MAX_BYTES: usize = 1_048_576;
pub const UPLOAD_MAX_BYTES: u64 = 16_777_216;
pub const OUTPUT_FORMATS: [&str; 4] = ["markdown", "middle_json", "structured_content", "zip"];
pub const SOURCES: [&str; 2] = ["file_id", "inline"];

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UploadCreate {
    pub filename: String,
    pub bytes: u64,
    pub mime_type: String,
    #[serde(default = "parse_purpose")]
    pub purpose: String,
    #[serde(default)]
    pub sha256sum: Option<String>,
    #[serde(default)]
    pub expires_after: Option<ExpiresAfter>,
}
fn parse_purpose() -> String {
    "parse".into()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExpiresAfter {
    pub anchor: Anchor,
    #[serde(default = "default_expiry")]
    pub seconds: u32,
}
fn default_expiry() -> u32 {
    3600
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Anchor {
    CreatedAt,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Completion {
    #[serde(default)]
    pub sha256sum: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct UploadResponse {
    pub id: String,
    pub object: &'static str,
    pub bytes: u64,
    pub created_at: i64,
    pub expires_at: i64,
    pub filename: String,
    pub purpose: String,
    pub mime_type: String,
    pub sha256sum: Option<String>,
    pub status: String,
    pub upload_url: Option<String>,
    pub upload_method: Option<&'static str>,
    pub upload_headers: BTreeMap<String, String>,
    pub file: Option<FileObject>,
}
pub type UploadCreated = UploadResponse;

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JobCreate {
    pub files: Vec<JobFile>,
    #[serde(default)]
    pub tier: Option<Tier>,
    #[serde(default)]
    pub ocr_mode: OcrMode,
    #[serde(default = "default_outputs")]
    pub output_formats: Vec<OutputFormat>,
}
fn default_outputs() -> Vec<OutputFormat> {
    vec![OutputFormat::Markdown]
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OutputFormat {
    Markdown,
    MiddleJson,
    StructuredContent,
    Zip,
}
#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Tier {
    Flash,
    Basic,
    Standard,
    Advanced,
}
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum OcrMode {
    #[default]
    Auto,
    Txt,
    Ocr,
}
#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "type", deny_unknown_fields)]
pub enum JobSource {
    #[serde(rename = "file_id")]
    FileId { file_id: String },
    #[serde(rename = "inline")]
    Inline { name: String, data: String },
}
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JobFile {
    pub source: JobSource,
    #[serde(default)]
    pub page_range: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct FileObject {
    pub id: String,
    pub object: &'static str,
    pub bytes: u64,
    pub created_at: i64,
    pub expires_at: Option<i64>,
    pub filename: String,
    pub purpose: String,
    pub sha256sum: Option<String>,
}
#[derive(Clone, Debug, Serialize)]
pub struct FileDeletion {
    pub id: String,
    pub object: &'static str,
    pub deleted: bool,
}
#[derive(Clone, Debug, Serialize)]
pub struct ListResponse<T> {
    pub object: &'static str,
    pub data: Vec<T>,
    pub first_id: Option<String>,
    pub last_id: Option<String>,
    pub has_more: bool,
}
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Pagination {
    #[serde(default)]
    pub after: Option<String>,
    #[serde(default)]
    pub limit: Option<u16>,
    #[serde(default)]
    pub order: Option<ListOrder>,
}
#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ListOrder {
    Asc,
    Desc,
}

#[derive(Clone, Debug, Serialize)]
pub struct OutputReference {
    pub file_id: String,
    pub bytes: u64,
}
#[derive(Clone, Debug, Serialize)]
pub struct JobFileResponse {
    pub file_id: Option<String>,
    pub name: String,
    pub page_range: String,
    pub status: String,
    pub parse: Option<serde_json::Value>,
    pub output_files: Option<BTreeMap<String, OutputReference>>,
    pub error: Option<ApiErrorBody>,
}
#[derive(Clone, Debug, Serialize)]
pub struct JobProgress {
    pub total: u32,
    pub completed: u32,
    pub failed: u32,
}
#[derive(Clone, Debug, Serialize)]
pub struct JobLinks {
    #[serde(rename = "self")]
    pub self_url: String,
    pub cancel: String,
}
#[derive(Clone, Debug, Serialize)]
pub struct JobResponse {
    pub job_id: String,
    pub status: String,
    pub created_at: String,
    pub started_at: Option<String>,
    pub finished_at: Option<String>,
    pub tier: Tier,
    pub output_formats: Vec<OutputFormat>,
    pub access_level: String,
    pub progress: JobProgress,
    pub files: Vec<JobFileResponse>,
    pub links: JobLinks,
}
#[derive(Clone, Debug, Serialize)]
pub struct JobListEntry {
    pub job_id: String,
    pub status: String,
    pub created_at: String,
    pub file_count: u32,
}
#[derive(Clone, Debug, Serialize)]
pub struct CancelResponse {
    pub job_id: String,
    pub status: String,
    pub canceled_at: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct Health {
    pub status: &'static str,
    pub version: &'static str,
    pub features: HealthFeatures,
}
#[derive(Clone, Debug, Serialize)]
pub struct HealthFeatures {
    pub webhook: bool,
    pub output_formats: Vec<&'static str>,
    pub sources: Vec<&'static str>,
}
impl Default for Health {
    fn default() -> Self {
        Self {
            status: "ok",
            version: "4.0.10",
            features: HealthFeatures {
                webhook: false,
                output_formats: OUTPUT_FORMATS.to_vec(),
                sources: SOURCES.to_vec(),
            },
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct ApiErrorBody {
    #[serde(rename = "type")]
    pub error_type: String,
    pub code: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub param: Option<String>,
}
#[derive(Clone, Debug, Serialize)]
pub struct ApiError {
    pub error: ApiErrorBody,
}
#[derive(Clone, Debug)]
pub struct WireError {
    pub status: StatusCode,
    pub code: &'static str,
    pub message: &'static str,
    pub param: Option<&'static str>,
}
impl WireError {
    pub fn bad(param: &'static str, message: &'static str) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            code: "invalid_request_error",
            message,
            param: Some(param),
        }
    }
    pub fn too_large() -> Self {
        Self {
            status: StatusCode::PAYLOAD_TOO_LARGE,
            code: "file_too_large",
            message: "The request exceeds the allowed size.",
            param: None,
        }
    }
}
impl fmt::Display for WireError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.message)
    }
}
impl std::error::Error for WireError {}
impl IntoResponse for WireError {
    fn into_response(self) -> Response {
        let body = ApiError {
            error: ApiErrorBody {
                error_type: "invalid_request_error".into(),
                code: self.code.into(),
                message: self.message.into(),
                param: self.param.map(str::to_owned),
            },
        };
        (self.status, Json(body)).into_response()
    }
}

impl UploadCreate {
    pub fn validate(&self) -> Result<(), WireError> {
        if self.filename.trim().is_empty() || self.filename.len() > 255 {
            return Err(WireError::bad("filename", "A valid filename is required."));
        }
        if self.mime_type.trim().is_empty() || self.mime_type.len() > 255 {
            return Err(WireError::bad(
                "mime_type",
                "A valid MIME type is required.",
            ));
        }
        if self.bytes == 0 {
            return Err(WireError::bad("bytes", "Bytes must be positive."));
        }
        if self.bytes > UPLOAD_MAX_BYTES {
            return Err(WireError::too_large());
        }
        if !matches!(self.purpose.as_str(), "parse" | "input_image") {
            return Err(WireError::bad("purpose", "Unsupported upload purpose."));
        }
        if let Some(hash) = &self.sha256sum {
            validate_hash(hash)?;
        }
        if let Some(expiry) = &self.expires_after {
            if expiry.anchor != Anchor::CreatedAt || !(3600..=2592000).contains(&expiry.seconds) {
                return Err(WireError::bad(
                    "expires_after",
                    "Unsupported upload expiry.",
                ));
            }
        }
        Ok(())
    }
}
fn validate_hash(hash: &str) -> Result<(), WireError> {
    if hash.len() != 64
        || !hash
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err(WireError::bad(
            "sha256sum",
            "Expected a lowercase SHA-256 hex digest.",
        ));
    }
    Ok(())
}

impl JobCreate {
    pub fn validate(&self) -> Result<(), WireError> {
        if self.files.is_empty() || self.files.len() > 100 {
            return Err(WireError::bad("files", "Expected between 1 and 100 files."));
        }
        if self.output_formats.is_empty() {
            return Err(WireError::bad(
                "output_formats",
                "At least one output format is required.",
            ));
        }
        let mut formats = std::collections::BTreeSet::new();
        if self
            .output_formats
            .iter()
            .any(|format| !formats.insert(*format as u8))
        {
            return Err(WireError::bad(
                "output_formats",
                "Output formats must be unique.",
            ));
        }
        for file in &self.files {
            if let Some(range) = &file.page_range {
                validate_page_range(range)?;
            }
            match &file.source {
                JobSource::FileId { file_id } if file_id.trim().is_empty() => {
                    return Err(WireError::bad("file_id", "A file identifier is required."));
                }
                JobSource::Inline { name, data } => {
                    if name.trim().is_empty() || name.len() > 255 {
                        return Err(WireError::bad("name", "A valid filename is required."));
                    }
                    validate_inline_base64(data)?;
                }
                _ => {}
            }
        }
        Ok(())
    }
}
pub(crate) fn validate_page_range(value: &str) -> Result<(), WireError> {
    let value = value.trim();
    if value.is_empty() || value == "all" {
        return Ok(());
    }
    for part in value.split(',') {
        let endpoints: Vec<_> = part.split('-').map(str::trim).collect();
        if endpoints.is_empty() || endpoints.len() > 2 {
            return Err(WireError::bad("page_range", "Invalid page range."));
        }
        let first = page_endpoint(endpoints[0])?;
        let last = endpoints.get(1).map(|v| page_endpoint(v)).transpose()?;
        if let Some((relative, end)) = last {
            let (start_relative, start) = first;
            let invalid_order = match (start_relative, relative) {
                (true, true) => start < end,
                (false, false) => start > end,
                _ => false,
            };
            if invalid_order {
                return Err(WireError::bad("page_range", "Invalid page range."));
            }
        }
    }
    Ok(())
}
fn page_endpoint(value: &str) -> Result<(bool, u32), WireError> {
    let (relative, digits) = value
        .strip_prefix('r')
        .map_or((false, value), |v| (true, v));
    if digits.is_empty() || digits.starts_with('0') || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return Err(WireError::bad("page_range", "Invalid page range."));
    }
    Ok((
        relative,
        digits
            .parse()
            .map_err(|_| WireError::bad("page_range", "Invalid page range."))?,
    ))
}
fn validate_inline_base64(data: &str) -> Result<(), WireError> {
    if data.len() > INLINE_MAX_BYTES.div_ceil(3) * 4 {
        return Err(WireError::too_large());
    }
    let bytes = data.as_bytes();
    let padding = bytes.iter().rev().take_while(|b| **b == b'=').count();
    if bytes.len() % 4 != 0
        || padding > 2
        || bytes[..bytes.len().saturating_sub(padding)]
            .iter()
            .any(|b| !b.is_ascii_alphanumeric() && !matches!(*b, b'+' | b'/'))
        || bytes[bytes.len().saturating_sub(padding)..]
            .iter()
            .any(|b| *b != b'=')
        || bytes.len() / 4 * 3 - padding > INLINE_MAX_BYTES
    {
        return Err(WireError::bad(
            "data",
            "Expected valid bounded base64 data.",
        ));
    }
    Ok(())
}
impl Pagination {
    pub fn validate(&self, default: u16, max: u16) -> Result<u16, WireError> {
        let limit = self.limit.unwrap_or(default);
        if limit == 0 || limit > max {
            return Err(WireError::bad("limit", "Pagination limit is out of range."));
        }
        Ok(limit)
    }
}

#[cfg(test)]
#[path = "wire_tests.rs"]
mod tests;
