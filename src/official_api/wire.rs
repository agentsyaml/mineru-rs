//! Strict wire DTOs for the official MinerU 4.2.3 task API.
//!
//! The upstream contract is published as Pydantic models in
//! `mineru/tasks/contracts.py`, `mineru/task_types.py`, and
//! `mineru/parser/api_server.py`. Every payload here mirrors one of them and is
//! emitted with the same `exclude_none` semantics: absent optionals are omitted,
//! zero values and empty containers are kept.

use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde::{Deserialize, Serialize};
use std::fmt;

/// `MAX_TASK_FILES` in `mineru/tasks/contracts.py`.
pub const MAX_TASK_FILES: usize = 100;
/// `TASK_UPLOAD_TTL` in `mineru/tasks/service.py`.
pub const TASK_UPLOAD_TTL_SECS: u64 = 3600;
/// Upper bound for the `x-mineru-task-lease-seconds` request header.
pub const MAX_LEASE_SECS: u64 = 3600;
/// `TaskFileInput.file_name` maximum length.
pub const MAX_FILE_NAME: usize = 1024;
/// `TASK_OUTPUT_FORMATS` in `mineru/task_types.py`.
pub const TASK_OUTPUT_FORMATS: [&str; 4] = ["default", "docx", "html", "latex"];
/// `HealthFeatures.sources`.
pub const SOURCES: [&str; 2] = ["upload", "url"];

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TaskStatus {
    WaitingFile,
    Pending,
    Running,
    Completed,
    Partial,
    Failed,
    Canceled,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TaskFileState {
    WaitingFile,
    Pending,
    Running,
    Completed,
    Failed,
    Canceled,
}

impl TaskFileState {
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Canceled)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TaskOutputFormat {
    Default,
    Docx,
    Html,
    Latex,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FilePurpose {
    Parse,
    ParseOutput,
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

#[derive(Clone, Debug, Serialize)]
pub struct ErrorDetail {
    pub code: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub param: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct ErrorResponse {
    pub error: ErrorDetail,
}

#[derive(Clone, Debug)]
pub struct WireError {
    pub status: StatusCode,
    pub code: &'static str,
    pub message: String,
    pub param: Option<String>,
}

pub mod code {
    pub const INVALID_REQUEST: &str = "invalid_request";
    pub const INTERNAL_ERROR: &str = "internal_error";
    pub const API_ERROR: &str = "api_error";
    pub const MODEL_NOT_FOUND: &str = "model_not_found";
    pub const UPLOAD_SIZE_MISMATCH: &str = "upload_size_mismatch";
    pub const FILE_HASH_MISMATCH: &str = "file_hash_mismatch";
    pub const TASK_ALREADY_TERMINAL: &str = "task_already_terminal";
    pub const UPLOAD_INTERRUPTED: &str = "upload_interrupted";
    pub const REQUEST_INTERRUPTED: &str = "request_interrupted";
    pub const QUALITY_TIER_UNAVAILABLE: &str = "quality_tier_unavailable";
    pub const TASK_NOT_FOUND: &str = "task_not_found";
    pub const UPLOAD_NOT_FOUND: &str = "upload_not_found";
    pub const PAGE_RANGE_INVALID: &str = "page_range_invalid";
}

impl WireError {
    pub fn new(
        status: StatusCode,
        code: &'static str,
        message: impl Into<String>,
        param: Option<String>,
    ) -> Self {
        Self {
            status,
            code,
            message: message.into(),
            param,
        }
    }

    /// The standard `invalid_task_request` constructor: 400 `invalid_request`.
    pub fn invalid(message: impl Into<String>, param: impl Into<String>) -> Self {
        Self::new(
            StatusCode::BAD_REQUEST,
            code::INVALID_REQUEST,
            message,
            Some(param.into()),
        )
    }

    pub fn invalid_unattributed(message: impl Into<String>) -> Self {
        Self::new(
            StatusCode::BAD_REQUEST,
            code::INVALID_REQUEST,
            message,
            None,
        )
    }

    /// Unknown or foreign task id: upstream answers 404 `task_not_found`.
    pub fn task_not_found(task_id: &str) -> Self {
        Self::new(
            StatusCode::NOT_FOUND,
            code::TASK_NOT_FOUND,
            format!("Task {task_id} not found"),
            None,
        )
    }

    /// Unknown `file_id` in a task upload: upstream answers 404 `upload_not_found`.
    pub fn upload_not_found() -> Self {
        Self::new(
            StatusCode::NOT_FOUND,
            code::UPLOAD_NOT_FOUND,
            "Unknown file_id",
            None,
        )
    }

    pub fn internal() -> Self {
        Self::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            code::INTERNAL_ERROR,
            "Internal server error",
            None,
        )
    }

    pub fn too_large() -> Self {
        Self::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            code::INVALID_REQUEST,
            "The request exceeds the allowed size.",
            None,
        )
    }
}

impl fmt::Display for WireError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for WireError {}

impl IntoResponse for WireError {
    fn into_response(self) -> Response {
        let body = ErrorResponse {
            error: ErrorDetail {
                code: self.code.into(),
                message: self.message,
                param: self.param,
            },
        };
        (self.status, Json(body)).into_response()
    }
}

/// Mirrors `validate_parse_filename`: a logical basename, never a path.
pub fn validate_file_name(name: &str) -> Result<(), WireError> {
    let fail = || WireError::invalid("Invalid file_name.", "file_name");
    if name.is_empty() || name.len() > MAX_FILE_NAME {
        return Err(fail());
    }
    if name == "." || name == ".." {
        return Err(fail());
    }
    if name
        .chars()
        .any(|c| matches!(c, '/' | '\\' | ':') || (c.is_control() && c != '\t'))
    {
        return Err(fail());
    }
    Ok(())
}

pub fn validate_hash(hash: &str) -> Result<(), WireError> {
    if hash.len() != 64
        || !hash
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err(WireError::invalid(
            "Expected a lowercase SHA-256 hex digest.",
            "sha256sum",
        ));
    }
    Ok(())
}

pub(crate) fn validate_page_range(value: &str) -> Result<(), WireError> {
    let raw = value;
    let value = value.trim();
    // Upstream's default is the empty string, meaning every page.
    if value.is_empty() || value == "all" {
        return Ok(());
    }
    for part in value.split(',') {
        let endpoints: Vec<_> = part.split('-').map(str::trim).collect();
        if endpoints.is_empty() || endpoints.len() > 2 {
            return Err(range_error(
                raw,
                "expected a page, an inclusive range, or 'all'",
            ));
        }
        let first = page_endpoint(endpoints[0], raw)?;
        let last = endpoints
            .get(1)
            .map(|v| page_endpoint(v, raw))
            .transpose()?;
        if let Some((relative, end)) = last {
            let (start_relative, start) = first;
            let invalid_order = match (start_relative, relative) {
                (true, true) => start < end,
                (false, false) => start > end,
                _ => false,
            };
            if invalid_order {
                return Err(range_error(raw, "reversed ranges are not supported"));
            }
        }
    }
    Ok(())
}

/// Upstream `InvalidRequestError('page_range_invalid', ...)`.
fn range_error(raw: &str, reason: &str) -> WireError {
    WireError::new(
        StatusCode::BAD_REQUEST,
        code::PAGE_RANGE_INVALID,
        format!("Invalid page range '{raw}': {reason}"),
        Some("page_range".to_string()),
    )
}

fn page_endpoint(value: &str, raw: &str) -> Result<(bool, u32), WireError> {
    let (relative, digits) = value
        .strip_prefix('r')
        .map_or((false, value), |v| (true, v));
    if digits.is_empty() || digits.starts_with('0') || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return Err(range_error(
            raw,
            "expected a page, an inclusive range, or 'all'",
        ));
    }
    Ok((
        relative,
        digits
            .parse()
            .map_err(|_| range_error(raw, "expected a page, an inclusive range, or 'all'"))?,
    ))
}
mod task;

pub use task::*;

// ---------------------------------------------------------------------------
// Discovery and file objects
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Serialize)]
pub struct HealthFeatures {
    pub task_output_formats: Vec<&'static str>,
    pub sources: Vec<&'static str>,
}

impl Default for HealthFeatures {
    fn default() -> Self {
        Self {
            task_output_formats: TASK_OUTPUT_FORMATS.to_vec(),
            sources: SOURCES.to_vec(),
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct HealthRuntime {
    pub device: &'static str,
}

#[derive(Clone, Debug, Serialize)]
pub struct Health {
    pub status: &'static str,
    pub version: &'static str,
    pub features: HealthFeatures,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub runtime: Option<HealthRuntime>,
}

impl Default for Health {
    fn default() -> Self {
        Self {
            status: "ok",
            version: crate::official_worker::PACKAGE_VERSION,
            features: HealthFeatures::default(),
            runtime: Some(HealthRuntime { device: "cpu" }),
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct ModelInfo {
    pub id: String,
    pub object: &'static str,
    pub created: i64,
    pub owned_by: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct ModelListResponse {
    pub object: &'static str,
    pub data: Vec<ModelInfo>,
}

#[derive(Clone, Debug, Serialize)]
pub struct TierInfo {
    pub id: Tier,
    pub description: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current_model: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct TierListResponse {
    pub object: &'static str,
    pub data: Vec<TierInfo>,
}

/// `FileObjectModel`.
#[derive(Clone, Debug, Serialize)]
pub struct FileObjectModel {
    pub id: String,
    pub object: &'static str,
    pub bytes: u64,
    pub created_at: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<i64>,
    pub file_name: String,
    pub purpose: FilePurpose,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sha256sum: Option<String>,
}

impl FileObjectModel {
    pub const OBJECT: &'static str = "file";
}

/// The tier descriptions upstream publishes verbatim.
pub fn tier_description(tier: Tier) -> &'static str {
    match tier {
        Tier::Flash => "Fast local text extraction.",
        Tier::Basic => "Basic parsing with local lightweight models.",
        Tier::Standard => "Standard parsing for most documents.",
        Tier::Advanced => "Advanced parsing for difficult documents.",
    }
}

#[cfg(test)]
#[path = "wire_tests.rs"]
mod tests;
