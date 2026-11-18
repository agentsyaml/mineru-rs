//! Response assembly for the task surface, mirroring `tasks/http.py`.

use super::super::store::{OfficialFileStore, TaskFile, TaskRecord};
use crate::official_api::wire::{
    CancelTaskResponse, CanceledTaskFile, CreateTaskResponse, CreatedTaskFile, FileParseInfo,
    TaskDetailResponse, TaskFileDetail, TaskFileState, TaskProgress, TaskStatus, TaskUploadLink,
};
use axum::http::{HeaderMap, header};
use std::collections::BTreeMap;
use tokio::sync::Mutex as AsyncMutex;

/// Absolute base derived from the request; empty when no `Host` is present.
pub(in crate::official_api::server) fn base_url(headers: &HeaderMap) -> String {
    let Some(host) = headers
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
    else {
        return String::new();
    };
    let scheme = headers
        .get("x-forwarded-proto")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("http");
    format!("{scheme}://{host}")
}

pub(in crate::official_api::server) fn status_str(status: TaskStatus) -> &'static str {
    match status {
        TaskStatus::WaitingFile => "waiting-file",
        TaskStatus::Pending => "pending",
        TaskStatus::Running => "running",
        TaskStatus::Completed => "completed",
        TaskStatus::Partial => "partial",
        TaskStatus::Failed => "failed",
        TaskStatus::Canceled => "canceled",
    }
}

fn upload_link(task: &TaskRecord, file: &TaskFile, base: &str) -> Option<TaskUploadLink> {
    if !file.upload_available(task, OfficialFileStore::now()) {
        return None;
    }
    Some(TaskUploadLink {
        url: format!("{base}/v1/tasks/{}/uploads/{}", task.id, file.file_id),
        headers: BTreeMap::from([(
            "Content-Type".to_owned(),
            "application/octet-stream".to_owned(),
        )]),
        expires_at: task.expires_at,
    })
}

fn result_url(file: &TaskFile, base: &str) -> Option<String> {
    (file.status == TaskFileState::Completed)
        .then(|| format!("{base}/v1/files/{}/content", file.file_id))
}

fn parse_info(file: &TaskFile) -> Option<FileParseInfo> {
    if file.status != TaskFileState::Completed {
        return None;
    }
    file.parse.clone()
}

pub(in crate::official_api::server) async fn created(
    shared: &AsyncMutex<TaskRecord>,
    base: &str,
) -> CreateTaskResponse {
    let task = shared.lock().await;
    CreateTaskResponse {
        task_id: task.id.clone(),
        status: task.status,
        files: task
            .files
            .iter()
            .map(|file| CreatedTaskFile {
                file_id: file.file_id.clone(),
                file_name: file.file_name.clone(),
                status: file.status,
                upload: upload_link(&task, file, base),
            })
            .collect(),
    }
}

pub(in crate::official_api::server) fn detail(task: &TaskRecord, base: &str) -> TaskDetailResponse {
    let mut completed = 0u32;
    let mut failed = 0u32;
    for file in &task.files {
        match file.status {
            TaskFileState::Completed => completed += 1,
            TaskFileState::Failed => failed += 1,
            _ => {}
        }
    }
    TaskDetailResponse {
        task_id: task.id.clone(),
        status: task.status,
        created_at: task.created_at.clone(),
        started_at: task
            .files
            .iter()
            .filter_map(|file| file.started_at.as_deref())
            .min_by_key(|value| {
                time::OffsetDateTime::parse(value, &time::format_description::well_known::Rfc3339)
                    .ok()
            })
            .map(str::to_owned),
        finished_at: task.finished_at.clone(),
        output_formats: task.options.output_formats.clone(),
        progress: TaskProgress {
            completed,
            failed,
            total: task.files.len() as u32,
        },
        files: task
            .files
            .iter()
            .map(|file| TaskFileDetail {
                file_id: file.file_id.clone(),
                file_name: file.file_name.clone(),
                status: file.status,
                bytes: file.bytes(),
                url: file.url.clone(),
                upload: upload_link(task, file, base),
                parse: parse_info(file),
                result_url: result_url(file, base),
                error: file.error.clone(),
            })
            .collect(),
        error: task.error.clone(),
    }
}

pub(in crate::official_api::server) fn canceled(
    task: &TaskRecord,
    base: &str,
) -> CancelTaskResponse {
    CancelTaskResponse {
        task_id: task.id.clone(),
        status: CancelTaskResponse::CANCELED,
        finished_at: task.finished_at.clone().unwrap_or_default(),
        files: task
            .files
            .iter()
            .filter(|file| file.status.is_terminal())
            .map(|file| CanceledTaskFile {
                file_id: file.file_id.clone(),
                file_name: file.file_name.clone(),
                status: file.status,
                parse: parse_info(file),
                result_url: result_url(file, base),
                error: file.error.clone(),
            })
            .collect(),
    }
}
