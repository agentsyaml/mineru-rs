mod artifacts;
mod discovery;
mod files;
mod jobs;
mod service;
mod store;
mod submission;
pub(super) mod tasks;

use axum::{Router, routing::get};
pub use service::OfficialService;
pub use store::OfficialFileStore;

pub fn official_files_router(store: OfficialFileStore) -> Router {
    Router::new()
        .route("/v1/files/{id}", get(files::get))
        .route("/v1/files/{id}/content", get(files::content))
        .with_state(store)
}

#[cfg(test)]
#[path = "server_tests.rs"]
mod tests;
