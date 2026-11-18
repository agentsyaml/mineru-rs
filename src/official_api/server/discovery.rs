use super::service::ServiceInner;
use crate::official_api::wire::{
    Health, ModelInfo, ModelListResponse, Tier, TierInfo, TierListResponse, WireError, code,
};
use axum::{
    Json, Router,
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::get,
};
use serde_json::json;
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
async fn health() -> Json<Health> {
    Json(Health::default())
}
async fn tiers() -> Json<TierListResponse> {
    let current_model = Some(virtual_model_id());
    let data = [Tier::Flash, Tier::Basic, Tier::Standard, Tier::Advanced]
        .into_iter()
        .map(|id| TierInfo {
            id,
            description: crate::official_api::wire::tier_description(id),
            current_model: current_model.clone(),
        })
        .collect();
    Json(TierListResponse {
        object: "list",
        data,
    })
}
async fn models() -> Json<ModelListResponse> {
    Json(ModelListResponse {
        object: "list",
        data: vec![model_info(&virtual_model_id())],
    })
}
async fn model(Path(id): Path<String>) -> Response {
    if id == virtual_model_id() {
        return Json(model_info(&id)).into_response();
    }
    let error = WireError::new(
        StatusCode::NOT_FOUND,
        code::MODEL_NOT_FOUND,
        format!("Model '{id}' not found"),
        None,
    );
    error.into_response()
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
fn model_info(id: &str) -> ModelInfo {
    ModelInfo {
        id: id.into(),
        object: "model",
        created: model_registered_at(),
        owned_by: "mineru",
        description: Some(
            "MinerU virtual model. PDF/image quality is selected per request via tier; \
             native documents always use flash. this id identifies the server version, \
             not a specific engine."
                .into(),
        ),
    }
}
async fn usage(State(s): State<Arc<ServiceInner>>) -> Json<serde_json::Value> {
    let jobs = s.jobs_created.load(std::sync::atomic::Ordering::Acquire);
    let files = s
        .store
        .inner
        .files
        .lock()
        .map(|v| v.len())
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
