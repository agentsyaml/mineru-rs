use super::{
    service::{JobRecord, PreparedFile, ServiceInner},
    store::OfficialFileStore,
    submission::{InputSource, Submission, decoded_size},
    tasks::result_helpers::internal,
};
use crate::{
    official::{
        HtmlSourceContext, OfficialInput, OfficialOcrMode, OfficialParseOptions, OfficialTier,
    },
    official_api::wire::*,
};
use axum::{Json, extract::State, http::StatusCode};
use base64::Engine;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

pub(super) mod output;
#[cfg(test)]
mod progress_tests;
mod query;
mod tier;
use output::publish_outputs;
pub(super) use query::{cancel, get, list};

#[cfg(test)]
pub(super) async fn create(
    State(s): State<Arc<ServiceInner>>,
    req: Result<Json<JobCreate>, axum::extract::rejection::JsonRejection>,
) -> Result<(StatusCode, Json<JobResponse>), WireError> {
    if s.stopping.load(Ordering::Acquire) {
        return Err(error(
            StatusCode::SERVICE_UNAVAILABLE,
            "server_shutting_down",
            "The server is shutting down.",
        ));
    }
    let Json(req) = req.map_err(|_| WireError::bad("body", "Invalid job request."))?;
    create_submission(&s, req)
}

#[cfg(test)]
fn create_submission(
    s: &ServiceInner,
    req: JobCreate,
) -> Result<(StatusCode, Json<JobResponse>), WireError> {
    prepare_job(s, Submission::from_create(req))
}

/// Shared admission pipeline for every submission route: stopping check,
/// slot/metadata/artifact reservation, per-file input charging, validation,
/// and record commit. Returns the stored `JobResponse`.
pub(super) fn prepare_job(
    s: &ServiceInner,
    req: Submission,
) -> Result<(StatusCode, Json<JobResponse>), WireError> {
    if s.stopping.load(Ordering::Acquire) {
        return Err(error(
            StatusCode::SERVICE_UNAVAILABLE,
            "server_shutting_down",
            "The server is shutting down.",
        ));
    }
    req.validate()?;
    let slot = s
        .store
        .shared_slots()
        .clone()
        .try_acquire_owned()
        .map_err(|_| {
            error(
                StatusCode::TOO_MANY_REQUESTS,
                "capacity_exceeded",
                "Job capacity is exhausted.",
            )
        })?;
    let metadata = s
        .store
        .reserve_metadata(1 + req.files.len() * req.output_formats.len())
        .ok_or_else(|| {
            error(
                StatusCode::TOO_MANY_REQUESTS,
                "capacity_exceeded",
                "Metadata capacity is exhausted.",
            )
        })?;
    let artifact_budget = s.store.artifact_budget(64 * 1024 * 1024).ok_or_else(|| {
        error(
            StatusCode::TOO_MANY_REQUESTS,
            "capacity_exceeded",
            "Artifact storage capacity is exhausted.",
        )
    })?;
    let mut total = 0u64;
    for file in &req.files {
        let size = match &file.source {
            InputSource::FileId { file_id } => {
                let object = s.store.file(file_id).ok_or_else(not_found)?;
                if object.purpose != "parse" {
                    return Err(WireError::bad("file_id", "File purpose is not parse."));
                }
                if object.bytes > s.store.file_cap() {
                    return Err(WireError::too_large());
                }
                object.bytes
            }
            InputSource::Inline { data, .. } => {
                let decoded = decoded_size(data);
                if decoded > crate::official_api::wire::INLINE_MAX_BYTES as u64
                    || decoded > s.store.file_cap()
                {
                    return Err(WireError::too_large());
                }
                decoded
            }
            InputSource::Raw { bytes, .. } => {
                // Size limit comes from the configured store cap, not the
                // upstream hardcoded 200 MiB literal.
                if bytes.is_empty() || bytes.len() as u64 > s.store.file_cap() {
                    return Err(if bytes.is_empty() {
                        WireError::bad("files", "Empty files are not supported.")
                    } else {
                        WireError::too_large()
                    });
                }
                bytes.len() as u64
            }
        };
        total = total
            .checked_add(size)
            .filter(|v| *v <= 64 * 1024 * 1024)
            .ok_or_else(WireError::too_large)?;
    }
    let mut files = Vec::with_capacity(req.files.len());
    for file in &req.files {
        let (id, name, bytes) = match &file.source {
            InputSource::FileId { file_id } => {
                let (object, bytes) = s.store.input_lease(file_id).ok_or_else(not_found)?;
                if object.purpose != "parse" {
                    return Err(WireError::bad("file_id", "File purpose is not parse."));
                }
                (Some(file_id.clone()), object.filename, bytes)
            }
            InputSource::Inline { name, data } => {
                let decoded_size = (data.len() / 4 * 3).saturating_sub(
                    data.as_bytes()
                        .iter()
                        .rev()
                        .take_while(|b| **b == b'=')
                        .count(),
                );
                let reservation =
                    s.store
                        .input_reservation(decoded_size as u64)
                        .ok_or_else(|| {
                            error(
                                StatusCode::TOO_MANY_REQUESTS,
                                "capacity_exceeded",
                                "Input storage capacity is exhausted.",
                            )
                        })?;
                let decoded = base64::engine::general_purpose::STANDARD
                    .decode(data)
                    .map_err(|_| WireError::bad("data", "Expected valid base64 data."))?;
                (None, name.clone(), s.store.charged(decoded, reservation))
            }
            InputSource::Raw { name, bytes } => {
                let reservation =
                    s.store
                        .input_reservation(bytes.len() as u64)
                        .ok_or_else(|| {
                            error(
                                StatusCode::TOO_MANY_REQUESTS,
                                "capacity_exceeded",
                                "Input storage capacity is exhausted.",
                            )
                        })?;
                (
                    None,
                    name.clone(),
                    s.store.charged(bytes.to_vec(), reservation),
                )
            }
        };
        let tier = tier::requested_tier(req.tier, &name);
        if tier::is_native(&name) && tier != OfficialTier::Flash {
            return Err(WireError::bad(
                "tier",
                "Native formats require the flash tier.",
            ));
        }
        let range = file
            .page_range
            .clone()
            .filter(|v| !v.trim().is_empty() && v.trim() != "all");
        files.push(PreparedFile {
            file_id: id,
            name,
            data: bytes,
            page_range: range,
            tier,
        });
    }
    let id = s.store.next_id("job");
    let created = timestamp();
    let tier = req.tier.unwrap_or_else(|| {
        if files.iter().any(|f| f.tier == OfficialTier::Flash) {
            Tier::Flash
        } else {
            Tier::Standard
        }
    });
    let response = JobResponse {
        job_id: id.clone(),
        status: "queued".into(),
        created_at: created,
        started_at: None,
        finished_at: None,
        tier,
        output_formats: req.output_formats.clone(),
        access_level: "self-hosted".into(),
        progress: JobProgress {
            total: files.len() as u32,
            completed: 0,
            failed: 0,
        },
        files: files
            .iter()
            .map(|f| JobFileResponse {
                file_id: f.file_id.clone(),
                name: f.name.clone(),
                page_range: f.page_range.clone().unwrap_or_default(),
                status: "queued".into(),
                parse: None,
                output_files: None,
                error: None,
            })
            .collect(),
        links: JobLinks {
            self_url: format!("/v1/parse/jobs/{id}"),
            cancel: format!("/v1/parse/jobs/{id}"),
        },
    };
    let record = JobRecord {
        response: response.clone(),
        files,
        cancel: Arc::new(AtomicBool::new(false)),
        _slot: slot,
        _metadata: metadata,
        _artifact_budget: artifact_budget,
        expiry: None,
        ocr_mode: match req.ocr_mode {
            OcrMode::Auto => OfficialOcrMode::Auto,
            OcrMode::Txt => OfficialOcrMode::Txt,
            OcrMode::Ocr => OfficialOcrMode::Ocr,
        },
    };
    commit_job(s, id, record)?;
    Ok((StatusCode::ACCEPTED, Json(response)))
}

fn commit_job(s: &ServiceInner, id: String, record: JobRecord) -> Result<(), WireError> {
    let _admission = s.store.inner.admission.lock().map_err(|_| internal())?;
    if s.stopping.load(Ordering::Acquire) || !s.store.inner.accepting.load(Ordering::Acquire) {
        return Err(error(
            StatusCode::SERVICE_UNAVAILABLE,
            "server_shutting_down",
            "The server is shutting down.",
        ));
    }
    #[cfg(test)]
    {
        let pause = s.commit_pause.lock().map_err(|_| internal())?.take();
        if let Some((ready, resume)) = pause {
            let _ = ready.send(());
            let _ = resume.recv();
        }
    }
    let mut jobs = s.jobs.lock().map_err(|_| internal())?;
    let mut queue = s.queue.lock().map_err(|_| internal())?;
    jobs.insert(id.clone(), Arc::new(tokio::sync::Mutex::new(record)));
    queue.push_back(id);
    s.jobs_created.fetch_add(1, Ordering::Relaxed);
    s.notify.notify_one();
    Ok(())
}

pub(super) async fn run_job(s: Arc<ServiceInner>, id: String) {
    let shared = match s.jobs.lock().ok().and_then(|j| j.get(&id).cloned()) {
        Some(v) => v,
        None => return,
    };
    let (files, canceled) = {
        let mut job = shared.lock().await;
        if job.response.status != "queued" {
            return;
        }
        if s.stopping.load(Ordering::Acquire) {
            job.cancel.store(true, Ordering::Release);
        }
        job.response.status = "running".into();
        job.response.started_at = Some(timestamp());
        (job.files.clone(), job.cancel.clone())
    };
    let mut successes = 0u32;
    let mut failures = 0u32;
    let mut aggregate = 0usize;
    for (index, file) in files.iter().enumerate() {
        if canceled.load(Ordering::Acquire) || s.stopping.load(Ordering::Acquire) {
            break;
        }
        shared.lock().await.response.files[index].status = "running".into();
        let permit = loop {
            let notified = s.cancel_notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if canceled.load(Ordering::Acquire) || s.stopping.load(Ordering::Acquire) {
                break None;
            }
            tokio::select! {
                permit = s.store.shared_gate().acquire() => break Some(permit),
                _ = &mut notified => {}
            }
        };
        let Some(permit) = permit else { break };
        let permit = match permit {
            Ok(v) => v,
            Err(_) => {
                failures += 1;
                mark_file(&shared, index, "failed", Some("runtime unavailable")).await;
                continue;
            }
        };
        if canceled.load(Ordering::Acquire) || s.stopping.load(Ordering::Acquire) {
            drop(permit);
            break;
        }
        let input = OfficialInput::bytes(file.name.clone(), file.data.clone());
        let ocr = shared.lock().await.ocr_mode;
        let mut options = OfficialParseOptions::new()
            .with_tier(file.tier)
            .with_ocr_mode(ocr)
            .with_source_context(HtmlSourceContext::new());
        if let Some(range) = &file.page_range {
            options = options.with_page_range(range.clone());
        }
        let parsed = s.parser.parse_rich(input, options).await;
        drop(permit);
        if canceled.load(Ordering::Acquire) || s.stopping.load(Ordering::Acquire) {
            mark_file(&shared, index, "canceled", None).await;
            break;
        }
        match parsed {
            Ok(result) if result.pages().len() > 1000 => {
                failures += 1;
                mark_file(&shared, index, "failed", Some("page limit exceeded")).await;
            }
            Ok(result) => {
                match publish_outputs(&s, &shared, index, &result, &mut aggregate).await {
                    Ok(()) => {
                        successes += 1;
                        s.files_processed.fetch_add(1, Ordering::Relaxed);
                        s.pages_processed
                            .fetch_add(result.pages().len() as u64, Ordering::Relaxed);
                        mark_file(&shared, index, "completed", None).await;
                    }
                    Err(_) => {
                        if record_publication_failure(&s, &shared, index, &mut failures).await {
                            break;
                        }
                    }
                }
            }
            Err(_) => {
                failures += 1;
                mark_file(&shared, index, "failed", Some("document parsing failed")).await;
            }
        }
    }
    finalize_job(&s, &shared, successes, failures).await;
}

pub(crate) async fn publication_was_canceled(
    s: &ServiceInner,
    shared: &Arc<tokio::sync::Mutex<JobRecord>>,
) -> bool {
    let job = shared.lock().await;
    job.cancel.load(Ordering::Acquire) || s.stopping.load(Ordering::Acquire)
}

pub(crate) async fn record_publication_failure(
    s: &ServiceInner,
    shared: &Arc<tokio::sync::Mutex<JobRecord>>,
    index: usize,
    failures: &mut u32,
) -> bool {
    if publication_was_canceled(s, shared).await {
        return true;
    }
    *failures += 1;
    mark_file(shared, index, "failed", Some("artifact generation failed")).await;
    false
}

pub(crate) async fn finalize_job(
    s: &ServiceInner,
    shared: &Arc<tokio::sync::Mutex<JobRecord>>,
    successes: u32,
    failures: u32,
) {
    let mut job = shared.lock().await;
    for file in &mut job.response.files {
        if matches!(file.status.as_str(), "queued" | "running") {
            file.status = "canceled".into();
        }
    }
    update_progress(&mut job);
    job.response.finished_at = Some(timestamp());
    job.response.status =
        if job.cancel.load(Ordering::Acquire) || s.stopping.load(Ordering::Acquire) {
            "canceled".into()
        } else if failures == 0 {
            "completed".into()
        } else if successes == 0 {
            "failed".into()
        } else {
            "partial".into()
        };
    job._artifact_budget.shrink_to(0);
    job._metadata.shrink_to(1);
    job.files.clear();
    job.expiry = Some(OfficialFileStore::now() + s.store.retention_secs() as i64);
}

async fn mark_file(
    shared: &Arc<tokio::sync::Mutex<JobRecord>>,
    index: usize,
    status: &str,
    message: Option<&str>,
) {
    let mut job = shared.lock().await;
    let file = &mut job.response.files[index];
    file.status = status.into();
    if let Some(message) = message {
        file.error = Some(ApiErrorBody {
            error_type: "server_error".into(),
            code: "parse_failed".into(),
            message: message.into(),
            param: None,
        });
    }
    update_progress(&mut job);
}

pub(super) fn update_progress(job: &mut JobRecord) {
    job.response.progress.completed = job
        .response
        .files
        .iter()
        .filter(|file| file.status == "completed")
        .count() as u32;
    job.response.progress.failed = job
        .response
        .files
        .iter()
        .filter(|file| file.status == "failed")
        .count() as u32;
}
fn timestamp() -> String {
    time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default()
}
fn error(status: StatusCode, code: &'static str, message: &'static str) -> WireError {
    WireError {
        status,
        code,
        message,
        param: None,
    }
}
fn not_found() -> WireError {
    error(
        StatusCode::NOT_FOUND,
        "not_found",
        "The requested resource was not found.",
    )
}
