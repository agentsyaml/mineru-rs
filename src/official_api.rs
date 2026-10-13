//! Strict wire DTOs for the additive official-v1 HTTP API.

pub mod client;
mod server;
pub mod wire;

pub use client::{OfficialApiClient, OfficialRemoteParseOptions};
pub use wire::*;

pub use server::{OfficialFileStore, OfficialService, official_files_router};

pub fn health() -> Health {
    Health::default()
}
