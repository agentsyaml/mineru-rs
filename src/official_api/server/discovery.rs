use super::{service::ServiceInner, store::Entry};
use crate::official_api::wire::{ApiError, ApiErrorBody};
use axum::{
    Json, Router,
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::get,
};
use serde_json::{Value, json};
use std::{
    sync::{Arc, OnceLock},
    time::{SystemTime, UNIX_EPOCH},
};

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
async fn tiers() -> Json<Value> {
    let current_model = virtual_model_id();
    let data: Vec<Value> = [
        ("flash", "Fast local text extraction."),
        ("basic", "Basic parsing with local lightweight models."),
        ("standard", "Standard parsing for most documents."),
        ("advanced", "Advanced parsing for difficult documents."),
    ]
    .into_iter()
    .map(|(id, description)| {
        json!({"id":id,"description":description,"current_model":current_model})
    })
    .collect();
    Json(json!({"object":"list","data":data}))
}
async fn models() -> Json<Value> {
    let id = virtual_model_id();
    Json(json!({"object":"list","data":[model_info(&id)]}))
}
async fn model(Path(id): Path<String>) -> Response {
    if id == virtual_model_id() {
        return Json(model_info(&id)).into_response();
    }
    (
        StatusCode::NOT_FOUND,
        Json(ApiError {
            error: ApiErrorBody {
                error_type: "invalid_request_error".into(),
                code: "model_not_found".into(),
                message: format!("Model '{id}' not found"),
                param: None,
            },
        }),
    )
        .into_response()
}
/// The single virtual model advertised by `/v1/models`. Its id tracks the
/// pinned MinerU package version; quality is chosen per request through `tier`.
fn virtual_model_id() -> String {
    format!("mineru-{}", crate::official_worker::PACKAGE_VERSION)
}
/// Stable per-process registration timestamp, matching upstream's virtual model.
fn model_registered_at() -> i64 {
    static REGISTERED_AT: OnceLock<i64> = OnceLock::new();
    *REGISTERED_AT.get_or_init(|| {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or_default()
    })
}
fn model_info(id: &str) -> Value {
    json!({
        "id":id,"object":"model","created":model_registered_at(),"owned_by":"mineru",
        "description":"MinerU virtual model. Quality is selected per request via tier (flash|basic|standard|advanced); this id identifies the server version, not a specific engine."
    })
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
