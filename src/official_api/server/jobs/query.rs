use super::*;
use axum::extract::{Path, Query, rejection::QueryRejection};
use serde::Deserialize;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::official_api::server) struct JobQuery {
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    limit: Option<u16>,
    #[serde(default)]
    after: Option<String>,
    #[serde(default)]
    order: Option<ListOrder>,
    #[serde(default)]
    created_after: Option<String>,
}

pub(in crate::official_api::server) async fn list(
    State(s): State<Arc<ServiceInner>>,
    query: Result<Query<JobQuery>, QueryRejection>,
) -> Result<Json<ListResponse<JobListEntry>>, WireError> {
    let Query(query) = query.map_err(|_| WireError::bad("query", "Invalid job list query."))?;
    let limit = Pagination {
        after: query.after.clone(),
        limit: query.limit,
        order: query.order,
    }
    .validate(20, 100)? as usize;
    let statuses: Vec<_> = query
        .status
        .as_deref()
        .unwrap_or("")
        .split(',')
        .filter(|v| !v.is_empty())
        .collect();
    if statuses.iter().any(|s| {
        ![
            "queued",
            "running",
            "completed",
            "partial",
            "failed",
            "canceled",
        ]
        .contains(s)
    }) {
        return Err(WireError::bad("status", "Unsupported job status."));
    }
    let jobs = {
        let jobs = s.jobs.lock().map_err(|_| internal())?;
        jobs.iter()
            .map(|(id, job)| (id.clone(), job.clone()))
            .collect::<Vec<_>>()
    };
    let mut data = Vec::new();
    for (id, job) in jobs {
        let record = job.lock().await;
        let r = &record.response;
        if statuses.is_empty() || statuses.contains(&r.status.as_str()) {
            data.push(JobListEntry {
                job_id: id,
                status: r.status.clone(),
                created_at: r.created_at.clone(),
                file_count: r.files.len() as u32,
            });
        }
    }
    data.sort_by(|a, b| (&a.created_at, &a.job_id).cmp(&(&b.created_at, &b.job_id)));
    if matches!(query.order, Some(ListOrder::Desc)) {
        data.reverse();
    }
    if let Some(after) = query.after {
        if let Some(i) = data.iter().position(|v| v.job_id == after) {
            data.drain(..=i);
        }
    }
    if let Some(after) = query.created_after {
        data.retain(|v| v.created_at >= after);
    }
    let has_more = data.len() > limit;
    data.truncate(limit);
    let first_id = data.first().map(|v| v.job_id.clone());
    let last_id = data.last().map(|v| v.job_id.clone());
    Ok(Json(ListResponse {
        object: "list",
        data,
        first_id,
        last_id,
        has_more,
    }))
}

pub(in crate::official_api::server) async fn get(
    State(s): State<Arc<ServiceInner>>,
    Path(id): Path<String>,
) -> Result<Json<JobResponse>, WireError> {
    let record = s
        .jobs
        .lock()
        .map_err(|_| internal())?
        .get(&id)
        .cloned()
        .ok_or_else(not_found)?;
    Ok(Json(record.lock().await.response.clone()))
}

pub(in crate::official_api::server) async fn cancel(
    State(s): State<Arc<ServiceInner>>,
    Path(id): Path<String>,
) -> Result<Json<CancelResponse>, WireError> {
    let record = s
        .jobs
        .lock()
        .map_err(|_| internal())?
        .get(&id)
        .cloned()
        .ok_or_else(not_found)?;
    let mut record = record.lock().await;
    if !matches!(record.response.status.as_str(), "queued" | "running") {
        return Err(error(
            StatusCode::CONFLICT,
            "job_terminal",
            "Terminal jobs cannot be canceled.",
        ));
    }
    record.cancel.store(true, Ordering::Release);
    if record.response.status == "queued" {
        record.response.status = "canceled".into();
        record.response.finished_at = Some(timestamp());
        record._artifact_budget.shrink_to(0);
        record._metadata.shrink_to(1);
        for file in &mut record.response.files {
            file.status = "canceled".into();
        }
        super::update_progress(&mut record);
        record.files.clear();
        record.expiry = Some(OfficialFileStore::now() + s.store.retention_secs() as i64);
    }
    Ok(Json(CancelResponse {
        job_id: id,
        status: "canceled".into(),
        canceled_at: timestamp(),
    }))
}

#[cfg(test)]
#[path = "query_tests.rs"]
mod tests;
