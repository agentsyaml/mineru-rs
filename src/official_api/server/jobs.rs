//! Task execution pipeline: one runner pass parses every file of a task that
//! is ready, publishing each result under its reserved file identity.
use super::{
    service::ServiceInner,
    store::{OfficialFileStore, TaskRecord, task_terminal},
};
use crate::{
    official::{
        HtmlSourceContext, OfficialInput, OfficialOcrMode, OfficialParseOptions, OfficialTier,
    },
    official_api::wire::{ErrorDetail, OcrMode, TaskFileState, TaskStatus},
};
use bytes::Bytes;
use std::sync::{Arc, atomic::Ordering};
use tokio::sync::OwnedSemaphorePermit;

pub(super) mod output;
mod tier;
mod url_source;

pub(super) use tier::{file_tier, resolve as resolve_tier};

struct Runnable {
    index: usize,
    name: String,
    tier: OfficialTier,
    page_range: Option<String>,
    ocr: OfficialOcrMode,
    data: Option<Bytes>,
}

/// Run one task to its current extent: every waiting upload or URL source is
/// parsed, then the aggregate status is recomputed. Duplicate queue entries
/// return immediately; the active pass rescans after each file.
pub(super) async fn run_task(s: Arc<ServiceInner>, id: String) {
    let Some(shared) = s.store.lookup(&id) else {
        return;
    };
    {
        let mut task = shared.lock().await;
        if task.running {
            return;
        }
        task.running = true;
    }
    loop {
        let job = {
            let mut task = shared.lock().await;
            s.store.expire_uploads(&mut task);
            if s.stopping.load(Ordering::Acquire) || task.status == TaskStatus::Canceled {
                cancel_task(&s, &mut task);
                None
            } else {
                match next_ready(&task) {
                    Some(index) => Some(start_file(&mut task, index)),
                    None => None,
                }
            }
        };
        let Some(mut job) = job else { break };
        if job.data.is_none() {
            if url_source::download(&s, &shared, job.index).await.is_err() {
                continue;
            }
            let mut task = shared.lock().await;
            if task.status == TaskStatus::Canceled || s.stopping.load(Ordering::Acquire) {
                cancel_task(&s, &mut task);
                break;
            }
            job = start_file(&mut task, job.index);
            if job.data.is_none() {
                continue;
            }
        }
        let Some(data) = job.data.take() else {
            continue;
        };
        if s.stopping.load(Ordering::Acquire) || canceled(&s, &shared).await {
            let mut task = shared.lock().await;
            cancel_task(&s, &mut task);
            break;
        }
        let Some(permit) = acquire_gate(&s, &shared).await else {
            let mut task = shared.lock().await;
            cancel_task(&s, &mut task);
            break;
        };
        {
            let mut task = shared.lock().await;
            if let Some(file) = task.files.get_mut(job.index)
                && file.status == TaskFileState::Pending
            {
                file.status = TaskFileState::Running;
                file.started_at = file
                    .started_at
                    .clone()
                    .or_else(|| Some(super::store::timestamp()));
            }
        }
        let input = OfficialInput::bytes(job.name.clone(), data);
        let mut options = OfficialParseOptions::new()
            .with_tier(job.tier)
            .with_ocr_mode(job.ocr)
            .with_source_context(HtmlSourceContext::new());
        if let Some(range) = &job.page_range {
            options = options.with_page_range(range.clone());
        }
        let parsed = s.parser.parse_rich(input, options).await;
        drop(permit);
        match parsed {
            Ok(result) if result.pages().len() > 1000 => {
                fail(&s, &shared, job.index, "page limit exceeded").await;
            }
            Ok(result) => {
                if output::publish_result(&s, &shared, job.index, &result)
                    .await
                    .is_err()
                {
                    fail(&s, &shared, job.index, "artifact generation failed").await;
                } else {
                    s.files_processed.fetch_add(1, Ordering::Relaxed);
                    s.pages_processed
                        .fetch_add(result.pages().len() as u64, Ordering::Relaxed);
                }
            }
            Err(_) => fail(&s, &shared, job.index, "document parsing failed").await,
        }
    }
    let mut task = shared.lock().await;
    task.running = false;
    s.store.aggregate(&mut task);
    if task_terminal(task.status) {
        task._artifact_budget.shrink_to(0);
        task._metadata.shrink_to(1);
    }
}

fn next_ready(task: &TaskRecord) -> Option<usize> {
    task.files.iter().position(|file| match file.status {
        TaskFileState::Pending => !file.submitted,
        TaskFileState::WaitingFile => file.url.is_some() && !file.submitted,
        _ => false,
    })
}

fn start_file(task: &mut TaskRecord, index: usize) -> Runnable {
    let (name, tier, page_range) = {
        let file = &task.files[index];
        (
            file.file_name.clone(),
            tier::requested_tier(file.tier, &file.file_name),
            page_range_for(&file.page_range),
        )
    };
    let file = &mut task.files[index];
    if file.data.is_some() {
        file.upload_active = false;
        file.submitted = true;
    } else {
        file.upload_active = true;
    }
    Runnable {
        index,
        name,
        tier,
        page_range,
        ocr: match task.options.ocr_mode {
            OcrMode::Auto => OfficialOcrMode::Auto,
            OcrMode::Txt => OfficialOcrMode::Txt,
            OcrMode::Ocr => OfficialOcrMode::Ocr,
        },
        data: file.data.clone(),
    }
}

/// Upstream forwards the normalized range to every input format, not only PDFs.
fn page_range_for(range: &str) -> Option<String> {
    let trimmed = range.trim();
    if trimmed.is_empty() || trimmed == "all" {
        return None;
    }
    Some(trimmed.to_owned())
}

async fn acquire_gate(
    s: &ServiceInner,
    shared: &Arc<tokio::sync::Mutex<TaskRecord>>,
) -> Option<OwnedSemaphorePermit> {
    loop {
        let notified = s.cancel_notify.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        if s.stopping.load(Ordering::Acquire) || canceled(s, shared).await {
            return None;
        }
        tokio::select! {
            permit = s.store.shared_gate().clone().acquire_owned() => return permit.ok(),
            _ = &mut notified => {}
        }
    }
}

async fn canceled(s: &ServiceInner, shared: &Arc<tokio::sync::Mutex<TaskRecord>>) -> bool {
    s.stopping.load(Ordering::Acquire) || shared.lock().await.status == TaskStatus::Canceled
}

async fn fail(
    s: &ServiceInner,
    shared: &Arc<tokio::sync::Mutex<TaskRecord>>,
    index: usize,
    message: &str,
) {
    fail_with(s, shared, index, "parse_failed", message).await;
}

pub(super) async fn fail_with(
    s: &ServiceInner,
    shared: &Arc<tokio::sync::Mutex<TaskRecord>>,
    index: usize,
    code: &str,
    message: &str,
) {
    let mut task = shared.lock().await;
    if task.status == TaskStatus::Canceled {
        return;
    }
    if let Some(file) = task.files.get_mut(index)
        && !file.status.is_terminal()
    {
        file.status = TaskFileState::Failed;
        file.finished_at = Some(super::store::timestamp());
        file.data = None;
        file.error = Some(ErrorDetail {
            code: code.into(),
            message: message.into(),
            param: None,
        });
    }
    s.store.aggregate(&mut task);
}

/// Cancel every unfinished file, keeping completed results downloadable.
pub(super) fn cancel_task(s: &ServiceInner, task: &mut TaskRecord) {
    task.status = TaskStatus::Canceled;
    task.finished_at = Some(super::store::timestamp());
    task.retention_at = Some(OfficialFileStore::now() + s.store.retention_secs() as i64);
    for file in &mut task.files {
        if !file.status.is_terminal() {
            file.status = TaskFileState::Canceled;
            file.data = None;
        }
    }
    task._artifact_budget.shrink_to(0);
    task._metadata.shrink_to(1);
}
