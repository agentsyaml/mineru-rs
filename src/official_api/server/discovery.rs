use super::{service::ServiceInner, store::Entry};
use crate::official_api::wire::{ListResponse, WireError};
use axum::{
    Json, Router,
    extract::{Path, State},
    http::StatusCode,
    routing::get,
};
use serde_json::{Value, json};
use std::sync::Arc;

pub(super) fn router(state: Arc<ServiceInner>) -> Router {
    Router::new()
        .route("/v1/health", get(health))
        .route("/v1/tiers", get(tiers))
        .route("/v1/models", get(models))
        .route("/v1/models/{id}", get(model))
        .route("/v1/usage", get(usage))
        .with_state(state)
}
async fn health() -> Json<crate::official_api::wire::Health> {
    Json(crate::official_api::wire::Health::default())
}
async fn tiers() -> Json<ListResponse<Value>> {
    let data = ["flash", "basic", "standard", "advanced"].into_iter().map(|id| json!({
        "id":id,"description":"Supported by the configured local parser.","current_model":null
    })).collect();
    Json(ListResponse {
        object: "list",
        first_id: Some("flash".into()),
        last_id: Some("advanced".into()),
        has_more: false,
        data,
    })
}
async fn models() -> Json<ListResponse<Value>> {
    Json(ListResponse {
        object: "list",
        data: Vec::new(),
        first_id: None,
        last_id: None,
        has_more: false,
    })
}
async fn model(Path(_id): Path<String>) -> Result<Json<Value>, WireError> {
    Err(not_found())
}
async fn usage(State(s): State<Arc<ServiceInner>>) -> Json<Value> {
    let jobs = s.jobs_created.load(std::sync::atomic::Ordering::Acquire);
    let files = s
        .store
        .inner
        .entries
        .lock()
        .map(|v| v.values().filter(|e| matches!(e, Entry::File(_))).count())
        .unwrap_or_default();
    Json(
        json!({"object":"usage","access_level":"self-hosted","billing_period":{"start":null,"end":null},
        "current":{"pages_processed":s.pages_processed.load(std::sync::atomic::Ordering::Acquire),"files_processed":s.files_processed.load(std::sync::atomic::Ordering::Acquire),"jobs_created":jobs},
        "limits":{"max_pages_per_file":1000,"max_file_size_bytes":s.store.file_cap(),"max_files_per_job":100,
            "max_concurrent_jobs":s.store.max_concurrent(),"max_file_retention_days":s.store.retention_secs().div_ceil(86400)},
        "counters":{"visible_files":files,"input_bytes":s.store.inner.used.load(std::sync::atomic::Ordering::Acquire),
            "artifact_bytes":s.store.inner.artifact_used.load(std::sync::atomic::Ordering::Acquire)}}),
    )
}
fn not_found() -> WireError {
    WireError {
        status: StatusCode::NOT_FOUND,
        code: "not_found",
        message: "The requested model was not found.",
        param: Some("model"),
    }
}
