//! Task submission and inspection payloads.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

use super::{
    ErrorDetail, OcrMode, TaskFileState, TaskOutputFormat, TaskStatus, Tier, WireError,
    validate_file_name, validate_hash, validate_page_range,
};

/// `TaskFileInput`. `bytes` is a consistency check, not an upload cap.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskFileInput {
    pub file_name: String,
    #[serde(default)]
    pub bytes: Option<u64>,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub sha256sum: Option<String>,
    #[serde(default)]
    pub page_range: Option<String>,
}

impl TaskFileInput {
    pub fn validate(&self, index: usize) -> Result<(), WireError> {
        validate_file_name(&self.file_name)?;
        if self.url.is_none() && self.bytes.is_none() {
            return Err(WireError::invalid(
                "bytes is required when url is not supplied",
                format!("files.{index}.bytes"),
            ));
        }
        if self.bytes == Some(0) {
            return Err(WireError::invalid(
                "bytes must be greater than 0",
                format!("files.{index}.bytes"),
            ));
        }
        if self.url.is_some() && self.bytes.is_some() {
            return Err(WireError::invalid(
                "bytes and url are mutually exclusive",
                format!("files.{index}.bytes"),
            ));
        }
        if let Some(url) = &self.url
            && !(url.starts_with("http://") || url.starts_with("https://"))
        {
            return Err(WireError::invalid(
                "url must be an absolute HTTP URL",
                format!("files.{index}.url"),
            ));
        }
        if let Some(hash) = &self.sha256sum {
            validate_hash(hash)?;
        }
        if let Some(range) = &self.page_range {
            validate_page_range(range).map_err(|mut error| {
                error.param = Some(format!("files.{index}.page_range"));
                error
            })?;
        }
        Ok(())
    }
}

/// `UploadTaskOptions`, flattened into `CreateTaskRequest`.
#[derive(Clone, Debug, Deserialize)]
pub struct UploadTaskOptions {
    #[serde(default)]
    pub tier: Option<Tier>,
    #[serde(default)]
    pub ocr_mode: OcrMode,
    #[serde(default)]
    pub page_range: String,
    #[serde(default = "default_output_formats")]
    pub output_formats: Vec<TaskOutputFormat>,
}

pub fn default_output_formats() -> Vec<TaskOutputFormat> {
    vec![TaskOutputFormat::Default]
}

/// `CreateTaskRequest`: the option group plus one to `MAX_TASK_FILES` inputs.
///
/// The option fields are written out instead of flattened because serde
/// ignores `deny_unknown_fields` on a struct that also uses `flatten`, and
/// upstream rejects unknown keys with 400.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateTaskRequest {
    #[serde(default)]
    pub tier: Option<Tier>,
    #[serde(default)]
    pub ocr_mode: OcrMode,
    #[serde(default)]
    pub page_range: String,
    #[serde(default = "default_output_formats")]
    pub output_formats: Vec<TaskOutputFormat>,
    pub files: Vec<TaskFileInput>,
}

impl CreateTaskRequest {
    pub fn validate(&self) -> Result<(), WireError> {
        if self.files.is_empty() {
            return Err(WireError::invalid("files must not be empty", "files"));
        }
        if self.files.len() > super::MAX_TASK_FILES {
            return Err(WireError::invalid(
                format!("At most {} files are allowed", super::MAX_TASK_FILES),
                "files",
            ));
        }
        if self.output_formats.is_empty() {
            return Err(WireError::invalid(
                "output_formats must not be empty",
                "output_formats",
            ));
        }
        validate_page_range(&self.page_range)?;
        for (index, file) in self.files.iter().enumerate() {
            file.validate(index)?;
        }
        Ok(())
    }

    /// `list(dict.fromkeys(...))`: stable deduplication.
    pub fn normalized_formats(&self) -> Vec<TaskOutputFormat> {
        let mut formats: Vec<TaskOutputFormat> = Vec::new();
        for format in &self.output_formats {
            if !formats.contains(format) {
                formats.push(*format);
            }
        }
        formats
    }
}

/// `TaskUploadLink`.
#[derive(Clone, Debug, Serialize)]
pub struct TaskUploadLink {
    pub url: String,
    pub headers: BTreeMap<String, String>,
    pub expires_at: i64,
}

/// `CreatedTaskFile`: identity plus an optional upload link.
#[derive(Clone, Debug, Serialize)]
pub struct CreatedTaskFile {
    pub file_id: String,
    pub file_name: String,
    pub status: TaskFileState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub upload: Option<TaskUploadLink>,
}

/// `CreateTaskResponse`, returned with status 202.
#[derive(Clone, Debug, Serialize)]
pub struct CreateTaskResponse {
    pub task_id: String,
    pub status: TaskStatus,
    pub files: Vec<CreatedTaskFile>,
}

/// `UploadFileResponse`.
#[derive(Clone, Debug, Serialize)]
pub struct UploadFileResponse {
    pub task_id: String,
    pub file_id: String,
    pub status: TaskFileState,
}

/// `FileParseInfo`.
#[derive(Clone, Debug, Serialize)]
pub struct FileParseInfo {
    pub tier: Tier,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parser_version: Option<String>,
}

/// `TaskFileDetail`.
#[derive(Clone, Debug, Serialize)]
pub struct TaskFileDetail {
    pub file_id: String,
    pub file_name: String,
    pub status: TaskFileState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub upload: Option<TaskUploadLink>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parse: Option<FileParseInfo>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<ErrorDetail>,
}

/// `TaskProgress`. Zero counters are always serialized.
#[derive(Clone, Debug, Default, Serialize)]
pub struct TaskProgress {
    pub completed: u32,
    pub failed: u32,
    pub total: u32,
}

/// `TaskDetailResponse`.
#[derive(Clone, Debug, Serialize)]
pub struct TaskDetailResponse {
    pub task_id: String,
    pub status: TaskStatus,
    pub created_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub started_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<String>,
    pub output_formats: Vec<TaskOutputFormat>,
    pub progress: TaskProgress,
    pub files: Vec<TaskFileDetail>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<ErrorDetail>,
}

/// `CanceledTaskFile`. File level has no `partial` state.
#[derive(Clone, Debug, Serialize)]
pub struct CanceledTaskFile {
    pub file_id: String,
    pub file_name: String,
    pub status: TaskFileState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parse: Option<FileParseInfo>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<ErrorDetail>,
}

/// `CancelTaskResponse`.
#[derive(Clone, Debug, Serialize)]
pub struct CancelTaskResponse {
    pub task_id: String,
    pub status: &'static str,
    pub finished_at: String,
    pub files: Vec<CanceledTaskFile>,
}

impl CancelTaskResponse {
    pub const CANCELED: &'static str = "canceled";
}
