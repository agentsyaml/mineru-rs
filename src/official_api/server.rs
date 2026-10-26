mod artifacts;
mod discovery;
mod files;
mod ingestion;
mod jobs;
mod service;
mod store;
mod submission;
pub(super) mod tasks;
mod uploads;

use axum::{
    Router,
    routing::{get, post, put},
};
pub use service::OfficialService;
pub use store::OfficialFileStore;

pub fn official_files_router(store: OfficialFileStore) -> Router {
    let body_cap = store.body_cap();
    Router::new()
        .route("/v1/uploads", post(uploads::create))
        .route("/v1/uploads/{id}", get(uploads::get))
        .route("/v1/uploads/{id}/content", put(uploads::put))
        .route("/v1/uploads/{id}/complete", post(uploads::complete))
        .route("/v1/uploads/{id}/cancel", post(uploads::cancel))
        .route("/v1/files", get(files::list))
        .route("/v1/files/{id}", get(files::get).delete(files::delete))
        .route("/v1/files/{id}/content", get(files::content))
        .layer(axum::extract::DefaultBodyLimit::max(body_cap))
        .with_state(store)
}

#[cfg(test)]
#[path = "server_tests.rs"]
mod tests;
