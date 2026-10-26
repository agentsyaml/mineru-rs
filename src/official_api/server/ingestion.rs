use super::{jobs::prepare_job, service::ServiceInner, submission::Submission, tasks};
use crate::official_api::wire::{JobCreate, JobResponse, WireError};
use axum::{
    Json,
    extract::{FromRequest, State},
    http::StatusCode,
};
use std::sync::Arc;

pub(super) async fn create_job(
    State(service): State<Arc<ServiceInner>>,
    request: axum::http::Request<axum::body::Body>,
) -> Result<(StatusCode, Json<JobResponse>), WireError> {
    create_job_with_timeout(State(service), request, std::time::Duration::from_secs(30)).await
}

pub(super) async fn create_job_with_timeout(
    State(service): State<Arc<ServiceInner>>,
    request: axum::http::Request<axum::body::Body>,
    deadline: std::time::Duration,
) -> Result<(StatusCode, Json<JobResponse>), WireError> {
    if service.stopping.load(std::sync::atomic::Ordering::Acquire) {
        return Err(WireError {
            status: StatusCode::SERVICE_UNAVAILABLE,
            code: "server_shutting_down",
            message: "The server is shutting down.",
            param: None,
        });
    }
    let _permit = tasks::ingest_permit(&service)?;
    let parsed = tokio::time::timeout(deadline, Json::<JobCreate>::from_request(request, &()))
        .await
        .map_err(|_| tasks::ingest_timeout())?
        .map_err(|_| WireError::bad("body", "Invalid job request."))?;
    prepare_job(&service, Submission::from_create(parsed.0))
}
