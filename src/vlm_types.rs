use crate::{NormalizedBbox, Rotation, SamplingParams, VlmError, VlmResult};
use bytes::Bytes;
use futures_core::Stream;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::{
    fmt,
    path::PathBuf,
    pin::Pin,
    sync::{Arc, mpsc},
    time::Duration,
};
#[derive(Debug, Clone)]
pub struct OfficialPdfOptions {
    pub start_page: usize,
    pub end_page: Option<usize>,
    pub formula_enable: bool,
    pub table_enable: bool,
    pub image_analysis: bool,
    pub render_workers: usize,
    pub processing_window_size: usize,
    pub render_timeout: Duration,
    pub max_pdf_bytes: usize,
    pub max_pages: usize,
    pub max_page_pixels: u64,
    pub max_rendered_image_bytes: usize,
    pub max_in_flight_image_bytes: usize,
    pub max_raw_output_bytes: usize,
    pub max_layout_blocks_per_page: usize,
    pub max_semantic_requests_per_page: usize,
    pub max_requests_per_batch: usize,
    pub max_encoded_request_bytes: usize,
    pub max_encoded_batch_bytes: usize,
    pub max_encoded_document_bytes: usize,
    pub max_total_asset_bytes: usize,
    pub max_staged_text_bytes: usize,
    pub total_deadline: Duration,
}
impl Default for OfficialPdfOptions {
    fn default() -> Self {
        Self {
            start_page: 0,
            end_page: None,
            formula_enable: true,
            table_enable: true,
            image_analysis: true,
            render_workers: 3,
            processing_window_size: 64,
            render_timeout: Duration::from_secs(300),
            max_pdf_bytes: 512 * 1024 * 1024,
            max_pages: 10_000,
            max_page_pixels: 100_000_000,
            max_rendered_image_bytes: 64 * 1024 * 1024,
            max_in_flight_image_bytes: 128 * 1024 * 1024,
            max_raw_output_bytes: 128 * 1024 * 1024,
            max_layout_blocks_per_page: 256,
            max_semantic_requests_per_page: 128,
            max_requests_per_batch: 32,
            max_encoded_request_bytes: 16 * 1024 * 1024,
            max_encoded_batch_bytes: 64 * 1024 * 1024,
            max_encoded_document_bytes: 256 * 1024 * 1024,
            max_total_asset_bytes: 1024 * 1024 * 1024,
            max_staged_text_bytes: 256 * 1024 * 1024,
            total_deadline: Duration::from_secs(24 * 60 * 60),
        }
    }
}
impl OfficialPdfOptions {
    pub fn validate(&self) -> VlmResult<()> {
        if self.end_page.is_some_and(|end| {
            self.start_page > end
                || end
                    .checked_sub(self.start_page)
                    .and_then(|count| count.checked_add(1))
                    .is_none_or(|count| count > self.max_pages)
        }) || self.render_workers == 0
            || self.processing_window_size == 0
            || self.render_timeout.is_zero()
            || self.max_pdf_bytes == 0
            || self.max_pages == 0
            || self.max_page_pixels == 0
            || self.max_rendered_image_bytes == 0
            || self.max_in_flight_image_bytes == 0
            || self.max_raw_output_bytes == 0
            || self.max_layout_blocks_per_page == 0
            || self.max_semantic_requests_per_page == 0
            || self.max_requests_per_batch == 0
            || self.max_encoded_request_bytes == 0
            || self.max_encoded_batch_bytes == 0
            || self.max_encoded_document_bytes == 0
            || self.max_total_asset_bytes == 0
            || self.max_staged_text_bytes == 0
            || self.total_deadline.is_zero()
        {
            return Err(VlmError::InvalidConfig(
                "invalid official PDF options".into(),
            ));
        }
        Ok(())
    }
}
pub type VlmPriority = Option<i32>;
#[derive(Debug, Clone)]
pub enum VlmBatchPriority {
    All(VlmPriority),
    PerItem(Vec<VlmPriority>),
}
pub type VlmSemaphore = Option<Arc<tokio::sync::Semaphore>>;
#[derive(Clone, Default)]
pub(crate) struct TaskWorkLease(Option<Arc<tokio::sync::OwnedSemaphorePermit>>);
impl TaskWorkLease {
    pub(crate) fn from_permit(permit: tokio::sync::OwnedSemaphorePermit) -> Self {
        Self(Some(Arc::new(permit)))
    }

    pub(crate) fn wrap<T, F>(&self, job: F) -> impl FnOnce() -> T + Send + 'static
    where
        T: Send + 'static,
        F: FnOnce() -> T + Send + 'static,
    {
        let lease = self.clone();
        move || lease.run(job)
    }

    fn run<T>(self, job: impl FnOnce() -> T) -> T {
        let Self(permit) = self;
        let result = job();
        drop(permit);
        result
    }
}
pub type VlmBatchCompletionStream =
    Pin<Box<dyn Stream<Item = VlmResult<(usize, String)>> + Send + Unpin>>;
pub struct VlmSseStream {
    inner: mpsc::Receiver<VlmResult<String>>,
}
impl VlmSseStream {
    pub(crate) fn channel() -> (mpsc::Sender<VlmResult<String>>, Self) {
        let (sender, receiver) = mpsc::channel();
        (sender, Self { inner: receiver })
    }
}
impl Iterator for VlmSseStream {
    type Item = VlmResult<String>;
    fn next(&mut self) -> Option<Self::Item> {
        self.inner.recv().ok()
    }
}
#[derive(Clone)]
pub enum VlmImageInput {
    Path(PathBuf),
    Bytes {
        data: Bytes,
        media_type: Option<String>,
    },
    DataUrl(String),
    Base64 {
        data: String,
        media_type: Option<String>,
    },
    RemoteUrl(url::Url),
    None,
}
impl fmt::Debug for VlmImageInput {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Path(_) => f
                .debug_struct("VlmImageInput::Path")
                .field("configured", &true)
                .finish(),
            Self::Bytes { data, media_type } => f
                .debug_struct("VlmImageInput::Bytes")
                .field("byte_len", &data.len())
                .field("media_type", media_type)
                .finish(),
            Self::DataUrl(data) => f
                .debug_struct("VlmImageInput::DataUrl")
                .field("data_url_len", &data.len())
                .finish(),
            Self::Base64 { data, media_type } => f
                .debug_struct("VlmImageInput::Base64")
                .field("base64_len", &data.len())
                .field("media_type", media_type)
                .finish(),
            Self::RemoteUrl(_) => f
                .debug_struct("VlmImageInput::RemoteUrl")
                .field("configured", &true)
                .finish(),
            Self::None => f.write_str("VlmImageInput::None"),
        }
    }
}
#[derive(Debug, Clone)]
pub struct VlmEncodedImage {
    pub data: Bytes,
    pub media_type: String,
    pub width: u32,
    pub height: u32,
}
#[derive(Clone, Default)]
pub struct VlmRequest {
    pub images: Vec<VlmImageInput>,
    pub prompt: Option<String>,
    pub sampling: Option<SamplingParams>,
    pub priority: VlmPriority,
}
impl fmt::Debug for VlmRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("VlmRequest")
            .field("images", &self.images)
            .field("prompt_configured", &self.prompt.is_some())
            .field("prompt_len", &self.prompt.as_ref().map(String::len))
            .field("sampling", &self.sampling)
            .field("priority", &self.priority)
            .finish()
    }
}
#[derive(Clone)]
pub struct VlmCompletion {
    pub text: String,
    pub finish_reason: String,
    pub request_id: Option<String>,
}
impl fmt::Debug for VlmCompletion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("VlmCompletion")
            .field("text_len", &self.text.len())
            .field("finish_reason", &self.finish_reason)
            .field("request_id_configured", &self.request_id.is_some())
            .finish()
    }
}
#[derive(Clone)]
pub struct VlmScoredOutput {
    pub text: String,
    pub token_ids: Vec<u32>,
    pub logprobs: Vec<f32>,
    pub perplexity: Option<f32>,
    pub min_logprob: Option<f32>,
    pub logprob_std: Option<f32>,
}
impl fmt::Debug for VlmScoredOutput {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("VlmScoredOutput")
            .field("text_len", &self.text.len())
            .field("token_count", &self.token_ids.len())
            .field("logprob_count", &self.logprobs.len())
            .field("perplexity", &self.perplexity)
            .field("min_logprob", &self.min_logprob)
            .field("logprob_std", &self.logprob_std)
            .finish()
    }
}
#[derive(Debug, Clone)]
pub struct VlmLayoutBlock {
    pub block_type: String,
    pub bbox: NormalizedBbox,
    pub angle: Option<Rotation>,
    pub content: Option<String>,
    pub merge_prev: Option<bool>,
    pub metadata: Map<String, Value>,
}
#[derive(Debug, Clone)]
pub struct VlmExtractResult {
    pub blocks: Vec<VlmLayoutBlock>,
    pub layout_completion: Option<VlmCompletion>,
}
#[derive(Debug, Clone)]
pub struct VlmPreparedLayout {
    pub image: VlmEncodedImage,
}
#[derive(Debug, Clone)]
pub struct VlmPreparedExtraction {
    pub images: Vec<VlmEncodedImage>,
    pub prompts: Vec<String>,
    pub sampling: Vec<Option<SamplingParams>>,
    pub block_indices: Vec<usize>,
}
#[allow(dead_code)]
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ModelBlock {
    pub block_type: String,
    pub bbox: Option<NormalizedBbox>,
    pub angle: Option<Rotation>,
    pub content: Option<String>,
    pub merge_prev: Option<bool>,
    pub sub_type: Option<String>,
    pub extra: Map<String, Value>,
}
pub type ModelOutput = Vec<Vec<ModelBlock>>;

pub(crate) fn model_output_wire(output: &ModelOutput) -> VlmResult<Value> {
    let mut pages = Vec::with_capacity(output.len());
    for page in output {
        let mut blocks = Vec::with_capacity(page.len());
        for block in page {
            if block.block_type.is_empty() {
                return model_error("model block type is required");
            }
            let bbox = block
                .bbox
                .ok_or_else(|| model_protocol("model block bbox is required"))?;
            let angle = block
                .angle
                .ok_or_else(|| model_protocol("model block angle is required"))?;
            if block.extra.keys().any(|key| {
                matches!(
                    key.as_str(),
                    "type" | "bbox" | "angle" | "content" | "merge_prev" | "sub_type"
                )
            }) {
                return model_error("model block extra collides with reserved key");
            }
            let mut value = block.extra.clone();
            value.insert("type".into(), Value::String(block.block_type.clone()));
            value.insert(
                "bbox".into(),
                serde_json::json!([bbox.left, bbox.top, bbox.right, bbox.bottom]),
            );
            value.insert(
                "angle".into(),
                Value::from(match angle {
                    Rotation::Deg0 => 0,
                    Rotation::Deg90 => 90,
                    Rotation::Deg180 => 180,
                    Rotation::Deg270 => 270,
                }),
            );
            value.insert(
                "content".into(),
                block
                    .content
                    .clone()
                    .map(Value::String)
                    .unwrap_or(Value::Null),
            );
            if let Some(merge_prev) = block.merge_prev {
                value.insert("merge_prev".into(), Value::Bool(merge_prev));
            }
            if let Some(sub_type) = &block.sub_type {
                value.insert("sub_type".into(), Value::String(sub_type.clone()));
            }
            blocks.push(Value::Object(value));
        }
        pages.push(Value::Array(blocks));
    }
    Ok(Value::Array(pages))
}
fn model_error<T>(message: &str) -> VlmResult<T> {
    Err(model_protocol(message))
}
fn model_protocol(message: &str) -> VlmError {
    VlmError::Protocol {
        operation: "official model serialization",
        message: message.into(),
    }
}
#[allow(dead_code)]
#[derive(Debug, Clone, Default)]
pub struct OfficialDocument {
    pub document: crate::Document,
    pub model_output: ModelOutput,
    pub content_list_v2: Value,
    pub diagnostics: Vec<String>,
}
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct OfficialOutputManifest {
    pub root: PathBuf,
    pub stem: String,
    pub vlm_dir: PathBuf,
}
#[allow(dead_code)]
impl OfficialDocument {
    pub fn output_stem(input: &crate::PdfInput) -> String {
        match input {
            crate::PdfInput::Path(path) => path
                .file_stem()
                .and_then(|s| s.to_str())
                .map(sanitize_stem)
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "document".into()),
            crate::PdfInput::Bytes(_) => "document".into(),
        }
    }
}
#[allow(dead_code)]
pub fn sanitize_stem(value: &str) -> String {
    value
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}
pub(crate) fn unsupported<T>() -> VlmResult<T> {
    Err(VlmError::Unsupported("scored/PPL is unavailable over HTTP"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn task_work_lease_releases_only_after_last_clone() {
        let gate = Arc::new(tokio::sync::Semaphore::new(1));
        let permit = gate.clone().acquire_owned().await.unwrap();
        let root = TaskWorkLease::from_permit(permit);
        let first = root.clone();
        let last = first.clone();

        drop(root);
        drop(first);
        assert!(gate.clone().try_acquire_owned().is_err());
        drop(last);
        assert!(gate.try_acquire_owned().is_ok());
    }

    #[test]
    fn image_request_admits_zero_or_many_inputs() {
        assert!(VlmRequest::default().images.is_empty());
        let request = VlmRequest {
            images: vec![
                VlmImageInput::None,
                VlmImageInput::Bytes {
                    data: Bytes::new(),
                    media_type: Some("image/png".into()),
                },
            ],
            ..Default::default()
        };
        assert_eq!(request.images.len(), 2);
    }

    #[test]
    fn sensitive_vlm_debug_output_is_redacted() {
        let image = VlmImageInput::Bytes {
            data: Bytes::from_static(b"image-bytes-secret-marker"),
            media_type: Some("image/png".into()),
        };
        let data_url =
            VlmImageInput::DataUrl("data:image/png;base64,data-url-secret-marker".into());
        let base64 = VlmImageInput::Base64 {
            data: "base64-secret-marker".into(),
            media_type: None,
        };
        let request = VlmRequest {
            images: vec![image, data_url, base64],
            prompt: Some("prompt-secret-marker".into()),
            ..Default::default()
        };
        let completion = VlmCompletion {
            text: "completion-secret-marker".into(),
            finish_reason: "stop".into(),
            request_id: Some("request-id".into()),
        };
        let scored = VlmScoredOutput {
            text: "scored-secret-marker".into(),
            token_ids: vec![424_242],
            logprobs: vec![123.456],
            perplexity: None,
            min_logprob: None,
            logprob_std: None,
        };

        for (debug, marker) in [
            (format!("{request:?}"), "image-bytes-secret-marker"),
            (format!("{request:?}"), "data-url-secret-marker"),
            (format!("{request:?}"), "base64-secret-marker"),
            (format!("{request:?}"), "prompt-secret-marker"),
            (format!("{completion:?}"), "completion-secret-marker"),
            (format!("{scored:?}"), "scored-secret-marker"),
            (format!("{scored:?}"), "424242"),
            (format!("{scored:?}"), "123.456"),
        ] {
            assert!(!debug.contains(marker), "{debug}");
        }
    }

    #[test]
    fn official_path_stem_is_safe() {
        assert_eq!(sanitize_stem("a bad/pdf"), "a_bad_pdf");
        assert_eq!(
            OfficialDocument::output_stem(&crate::PdfInput::Bytes(Bytes::new())),
            "document"
        );
    }

    #[test]
    fn model_wire_requires_geometry_and_rejects_reserved_extra() {
        let base = ModelBlock {
            block_type: "text".into(),
            bbox: Some(NormalizedBbox::new(0.0, 0.0, 1.0, 1.0).unwrap()),
            angle: Some(Rotation::Deg0),
            ..Default::default()
        };
        let mut missing_bbox = base.clone();
        missing_bbox.bbox = None;
        assert!(model_output_wire(&vec![vec![missing_bbox]]).is_err());
        let mut missing_angle = base.clone();
        missing_angle.angle = None;
        assert!(model_output_wire(&vec![vec![missing_angle]]).is_err());
        let mut collision = base;
        collision.extra.insert("type".into(), Value::Null);
        assert!(model_output_wire(&vec![vec![collision]]).is_err());
    }

    #[test]
    fn official_pdf_options_validate_table() {
        let cases = [
            (OfficialPdfOptions::default(), true),
            (
                OfficialPdfOptions {
                    start_page: 2,
                    end_page: Some(1),
                    ..Default::default()
                },
                false,
            ),
            (
                OfficialPdfOptions {
                    end_page: Some(10_000),
                    ..Default::default()
                },
                false,
            ),
            (
                OfficialPdfOptions {
                    render_workers: 4,
                    ..Default::default()
                },
                true,
            ),
            (
                OfficialPdfOptions {
                    render_workers: 0,
                    ..Default::default()
                },
                false,
            ),
            (
                OfficialPdfOptions {
                    processing_window_size: 0,
                    ..Default::default()
                },
                false,
            ),
            (
                OfficialPdfOptions {
                    render_timeout: Duration::ZERO,
                    ..Default::default()
                },
                false,
            ),
            (
                OfficialPdfOptions {
                    max_raw_output_bytes: 0,
                    ..Default::default()
                },
                false,
            ),
            (
                OfficialPdfOptions {
                    max_requests_per_batch: 0,
                    ..Default::default()
                },
                false,
            ),
            (
                OfficialPdfOptions {
                    max_pdf_bytes: 0,
                    ..Default::default()
                },
                false,
            ),
            (
                OfficialPdfOptions {
                    max_pages: 0,
                    ..Default::default()
                },
                false,
            ),
            (
                OfficialPdfOptions {
                    max_page_pixels: 0,
                    ..Default::default()
                },
                false,
            ),
            (
                OfficialPdfOptions {
                    max_rendered_image_bytes: 0,
                    ..Default::default()
                },
                false,
            ),
            (
                OfficialPdfOptions {
                    max_in_flight_image_bytes: 0,
                    ..Default::default()
                },
                false,
            ),
            (
                OfficialPdfOptions {
                    max_total_asset_bytes: 0,
                    ..Default::default()
                },
                false,
            ),
            (
                OfficialPdfOptions {
                    max_encoded_batch_bytes: 0,
                    ..Default::default()
                },
                false,
            ),
            (
                OfficialPdfOptions {
                    max_encoded_document_bytes: 0,
                    ..Default::default()
                },
                false,
            ),
            (
                OfficialPdfOptions {
                    max_staged_text_bytes: 0,
                    ..Default::default()
                },
                false,
            ),
            (
                OfficialPdfOptions {
                    total_deadline: Duration::ZERO,
                    ..Default::default()
                },
                false,
            ),
        ];
        for (options, valid) in cases {
            assert_eq!(options.validate().is_ok(), valid);
        }
    }
}
