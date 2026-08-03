//! Shared API for the MinerU VLM client.

mod client;
pub mod command;
mod config;
mod document_limits;
mod document_postprocess;
mod error;
mod extractor;
mod image_pipeline;
#[doc(hidden)]
pub mod input_prepare;
mod layout;
mod markdown;
mod middle_json;
mod mineru_api;
mod office_workers;
mod official_builders;
mod official_output;
mod official_route;
mod openai;
mod output;
mod pdf;
mod pipeline;
mod preview;
mod profile;
mod progress_events;
mod types;
mod vlm_client;
mod vlm_config;
mod vlm_http;
mod vlm_image;
mod vlm_postprocess;
mod vlm_types;

#[doc(hidden)]
pub mod vlm_api;

pub use client::MinerUClient;
pub use command::{RunContext, RunError, RunOptions, RunReport, run, run_with_context};
pub use config::{BearerToken, ClientConfig, Limits, Timeouts};
#[doc(hidden)]
pub use document_limits::{DocumentLimitOverrides, DocumentLimitPolicy};
pub use error::{Error, ErrorContext, Result, VlmError, VlmResult};
#[doc(hidden)]
pub use input_prepare::DocumentKind;
#[doc(hidden)]
pub use input_prepare::RasterWorkers;
#[doc(hidden)]
pub use mineru_api::ooxml::preflight_ooxml_bytes;
#[doc(hidden)]
pub use mineru_api::{
    RemoteApiDocument, RemoteApiEnv, RemoteApiFailure, RemoteApiOptions, normalize_remote_language,
    parse_remote_api_env, selected_document_pages,
};
#[doc(hidden)]
pub use office_workers::{OfficeConvertError, OfficeWorkers};
pub use official_output::canonical_stem;
pub use output::write_outputs;
#[doc(hidden)]
pub use progress_events::{ProgressCallback, ProgressEvent, sanitize_event_text};
pub use types::{
    Asset, AssetKind, BlockKind, ContentBlock, Document, ModelInfo, NormalizedBbox, OutputManifest,
    PageRange, PageResult, ParseOptions, PdfInput, Rotation,
};
pub use vlm_client::{MinerUVlmClient, MinerUVlmPreprocessor};
pub use vlm_config::{MinerUVlmConfig, SamplingParams, VlmHeader, VlmHttpConfig};
pub use vlm_http::VlmHttpClient;
pub use vlm_types::{
    ModelBlock, ModelOutput, OfficialOutputManifest, OfficialPdfOptions, VlmBatchCompletionStream,
    VlmBatchPriority, VlmCompletion, VlmEncodedImage, VlmExtractResult, VlmImageInput,
    VlmLayoutBlock, VlmPreparedExtraction, VlmPreparedLayout, VlmPriority, VlmRequest,
    VlmSemaphore, VlmSseStream,
};
// Internal legacy modules still refer to these through crate::. They are not public API.
use vlm_types::*;
