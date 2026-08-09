use crate::vlm_http::ByteBudget;
use crate::{SamplingParams, VlmError, VlmImageInput, VlmRequest, VlmResult};
use mistralrs::{
    Model, MultimodalLoaderType, MultimodalMessages, MultimodalModelBuilder, RequestBuilder,
    TextMessageRole,
};
use serde::Deserialize;
use std::{env, ffi::OsString, fs, path::PathBuf, sync::Arc};

pub const MINERU_MODEL_ID: &str = "opendatalab/MinerU2.5-2509-1.2B";
pub const MINERU_MODEL_REVISION: &str = "1aa090b41282e64fadd79c10572221f91ec21924";

/// Upper bound for a sanitized mistralrs error chain rendered into a VlmError.
const MISTRALRS_ERROR_CAP: usize = 4096;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MistralRsModelSource {
    Local(PathBuf),
    Download {
        model_id: &'static str,
        revision: &'static str,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MistralRsConfig {
    pub source: MistralRsModelSource,
    /// Human-facing name of the local-model source for validation errors:
    /// "MINERU_VL_MODEL_DIR" (env) or "model path" (CLI --model-path).
    source_name: &'static str,
}

impl MistralRsConfig {
    pub fn from_env() -> VlmResult<Self> {
        Self::from_env_with(|name| env::var_os(name))
    }

    fn from_env_with(get: impl Fn(&str) -> Option<OsString>) -> VlmResult<Self> {
        let source = if let Some(dir) = get("MINERU_VL_MODEL_DIR") {
            let dir = PathBuf::from(dir);
            validate_model_dir(&dir, "MINERU_VL_MODEL_DIR")?;
            MistralRsModelSource::Local(dir)
        } else if enabled(&get, "MINERU_VL_AUTO_DOWNLOAD")? {
            MistralRsModelSource::Download {
                model_id: MINERU_MODEL_ID,
                revision: MINERU_MODEL_REVISION,
            }
        } else {
            return Err(VlmError::InvalidConfig(
                "MINERU_VL_MODEL_DIR is required unless MINERU_VL_AUTO_DOWNLOAD=true".into(),
            ));
        };
        Ok(Self {
            source,
            source_name: "MINERU_VL_MODEL_DIR",
        })
    }

    /// Resolve the model source from explicit CLI parts. A local model path
    /// always wins; otherwise download is used only when `allow_download` is
    /// true, mirroring `from_env_with`'s precedence.
    pub fn from_parts(model_path: Option<PathBuf>, allow_download: bool) -> VlmResult<Self> {
        let source = if let Some(dir) = model_path {
            validate_model_dir(&dir, "model path")?;
            MistralRsModelSource::Local(dir)
        } else if allow_download {
            MistralRsModelSource::Download {
                model_id: MINERU_MODEL_ID,
                revision: MINERU_MODEL_REVISION,
            }
        } else {
            return Err(VlmError::InvalidConfig(
                "--model-path is required when --allow-download=false".into(),
            ));
        };
        Ok(Self {
            source,
            source_name: "model path",
        })
    }
}

#[derive(Clone)]
pub(crate) struct MistralRsClient {
    model: Arc<Model>,
    serial: Arc<tokio::sync::Mutex<()>>,
    response_cap: usize,
}

impl std::fmt::Debug for MistralRsClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MistralRsClient").finish_non_exhaustive()
    }
}

impl MistralRsClient {
    pub(crate) async fn connect(config: MistralRsConfig, response_cap: usize) -> VlmResult<Self> {
        let model_id = builder_source(&config.source, config.source_name)?;
        let mut builder = MultimodalModelBuilder::new(model_id)
            .with_loader_type(MultimodalLoaderType::Qwen2VL)
            .with_max_num_seqs(1);
        if let MistralRsModelSource::Download { revision, .. } = config.source {
            builder = builder.with_hf_revision(revision);
        }
        #[cfg(not(any(feature = "mistralrs-cuda", feature = "mistralrs-metal")))]
        {
            builder = builder.with_force_cpu();
            // Single forced CPU device: the SDK's default auto device mapping
            // is meaningless here and gates on system-wide available memory
            // (sysinfo), refusing to load on memory-pressured machines even
            // though the ~2.4GB model fits. Load all layers directly on CPU.
            // This only applies to plain CPU builds; CUDA and Metal builds get
            // the SDK's native device selection instead of forced CPU.
            builder = builder.with_device_mapping(mistralrs::core::DeviceMapSetting::Map(
                mistralrs::core::DeviceMapMetadata::dummy(),
            ));
        }
        let model = builder.build().await.map_err(|error| {
            let boxed: Box<dyn std::error::Error + Send + Sync> = error.into();
            mistralrs_transport_error("mistralrs model build", boxed.as_ref(), None)
        })?;
        Ok(Self {
            model: Arc::new(model),
            serial: Arc::new(tokio::sync::Mutex::new(())),
            response_cap,
        })
    }

    pub(crate) async fn aio_batch_predict(
        &self,
        requests: Vec<VlmRequest>,
        semaphore: crate::VlmSemaphore,
    ) -> VlmResult<Vec<String>> {
        let mut replies = Vec::with_capacity(requests.len());
        for request in requests {
            let _permit = match &semaphore {
                Some(semaphore) => Some(semaphore.clone().acquire_owned().await.map_err(|_| {
                    VlmError::Transport {
                        operation: "mistralrs batch",
                        message: "semaphore closed".into(),
                    }
                })?),
                None => None,
            };
            replies.push(self.predict(request, self.response_cap).await?);
        }
        Ok(replies)
    }

    pub(crate) async fn predict_official_budgeted(
        &self,
        request: VlmRequest,
        cap: usize,
        budget: Option<Arc<ByteBudget>>,
        deadline: tokio::time::Instant,
    ) -> VlmResult<(String, usize)> {
        let text = tokio::time::timeout_at(deadline, self.predict(request, cap))
            .await
            .map_err(|_| VlmError::Timeout {
                operation: "official PDF",
            })??;
        let bytes = text.len();
        if let Some(budget) = budget {
            budget.charge(bytes as u64, "raw reply bytes")?;
        }
        Ok((text, bytes))
    }

    async fn predict(&self, request: VlmRequest, cap: usize) -> VlmResult<String> {
        let request = request_builder(request)?;
        let _serial = self.serial.lock().await;
        let response = self
            .model
            .send_chat_request(request)
            .await
            .map_err(|error| {
                let inner = error.source_inner().map(|e| e as &dyn std::error::Error);
                mistralrs_transport_error("mistralrs inference", &error, inner)
            })?;
        let content = response
            .choices
            .first()
            .and_then(|choice| choice.message.content.clone())
            .filter(|content| !content.is_empty())
            .ok_or_else(|| VlmError::Protocol {
                operation: "mistralrs inference",
                message: "response has no choice content".into(),
            })?;
        if content.len() > cap {
            return Err(VlmError::LimitExceeded {
                resource: "raw reply bytes",
                limit: cap as u64,
                actual: content.len() as u64,
            });
        }
        Ok(content)
    }
}

/// Maps a mistralrs build/inference error into a bounded, sanitized Transport
/// error. Walks the `std::error::Error` source chain so the root cause
/// survives, appending only causes not already rendered by an enclosing
/// Display (thiserror variants embed their boxed inner; anyhow does not), then
/// runs the chain through `sanitize_vlm_error_bytes` (cap
/// `MISTRALRS_ERROR_CAP`) so HF_TOKEN, Authorization and query secrets never
/// reach the CLI. `inner` is the SDK boxed inner (`Error::source_inner()`);
/// mistralrs 0.8.1's thiserror enum does not surface it via `source()`, while
/// the anyhow build error is boxed so its `source()` chain walks directly.
fn mistralrs_transport_error(
    operation: &'static str,
    error: &dyn std::error::Error,
    inner: Option<&dyn std::error::Error>,
) -> VlmError {
    let mut chain = error.to_string();
    let mut source = inner.or_else(|| error.source());
    while let Some(cause) = source {
        let rendered = cause.to_string();
        if !chain.contains(&rendered) {
            chain.push_str(": ");
            chain.push_str(&rendered);
        }
        source = cause.source();
    }
    VlmError::Transport {
        operation,
        message: crate::error::sanitize_vlm_error_bytes(chain.as_bytes(), MISTRALRS_ERROR_CAP),
    }
}

fn builder_source(source: &MistralRsModelSource, source_name: &str) -> VlmResult<String> {
    match source {
        MistralRsModelSource::Local(path) => path.to_str().map(str::to_owned).ok_or_else(|| {
            VlmError::InvalidConfig(format!(
                "{source_name} must be valid Unicode for mistral.rs"
            ))
        }),
        MistralRsModelSource::Download { model_id, .. } => Ok((*model_id).into()),
    }
}

fn request_builder(request: VlmRequest) -> VlmResult<RequestBuilder> {
    let images = request
        .images
        .into_iter()
        .map(decode_image)
        .collect::<VlmResult<Vec<_>>>()?;
    if images.is_empty() {
        return Err(VlmError::InvalidImageInput(
            "mistralrs requests require an image".into(),
        ));
    }
    // mistral.rs emits model-specific image markers for add_image_message.
    let prompt = request.prompt.unwrap_or_default().replace("<image>", "");
    let messages = MultimodalMessages::new()
        .add_message(TextMessageRole::System, "You are a helpful assistant.")
        .add_image_message(TextMessageRole::User, prompt, images);
    Ok(apply_sampling(
        RequestBuilder::from(messages),
        request.sampling,
    ))
}

fn decode_image(input: VlmImageInput) -> VlmResult<image::DynamicImage> {
    match input {
        VlmImageInput::Bytes { data, .. } => image::load_from_memory(&data)
            .map_err(|_| VlmError::InvalidImageInput("invalid image".into())),
        VlmImageInput::RemoteUrl(_) => Err(VlmError::InvalidImageInput(
            "mistralrs requests require a local image".into(),
        )),
        VlmImageInput::None => Err(VlmError::InvalidImageInput(
            "mistralrs requests require an image".into(),
        )),
        _ => Err(VlmError::InvalidImageInput(
            "mistralrs image was not admitted".into(),
        )),
    }
}

fn apply_sampling(mut builder: RequestBuilder, sampling: Option<SamplingParams>) -> RequestBuilder {
    let Some(sampling) = sampling else {
        return builder;
    };
    if let Some(value) = sampling.temperature {
        builder = builder.set_sampler_temperature(value.into());
    }
    if let Some(value) = sampling.top_p {
        builder = builder.set_sampler_topp(value.into());
    }
    if let Some(value) = sampling.top_k.filter(|value| *value >= 0) {
        builder = builder.set_sampler_topk(value as usize);
    }
    if let Some(value) = sampling.presence_penalty {
        builder = builder.set_sampler_presence_penalty(value);
    }
    if let Some(value) = sampling.frequency_penalty {
        builder = builder.set_sampler_frequency_penalty(value);
    }
    // Guard against runaway decoding when no explicit cap was given.
    builder = builder.set_sampler_max_len(sampling.max_new_tokens.unwrap_or(512) as usize);
    // ponytail: mistral.rs 0.8.1 has no repetition-penalty/no-repeat-ngram setters; ignore until a native mapping exists.
    builder
}

#[derive(Deserialize)]
struct ModelConfig {
    architectures: Vec<String>,
    model_type: String,
}

pub fn validate_model_dir(dir: &std::path::Path, source_name: &str) -> VlmResult<()> {
    if !dir.is_dir() {
        return Err(VlmError::InvalidConfig(format!(
            "{source_name} is not a directory: {}",
            dir.display()
        )));
    }
    let config_path = dir.join("config.json");
    let config: ModelConfig = serde_json::from_slice(&fs::read(&config_path).map_err(|error| {
        VlmError::InvalidConfig(format!("cannot read {}: {error}", config_path.display()))
    })?)
    .map_err(|error| {
        VlmError::InvalidConfig(format!("invalid {}: {error}", config_path.display()))
    })?;
    if config.model_type != "qwen2_vl"
        || !config
            .architectures
            .iter()
            .any(|name| name == "Qwen2VLForConditionalGeneration")
    {
        return Err(VlmError::InvalidConfig(format!(
            "{source_name} is not a Qwen2-VL MinerU model"
        )));
    }
    for name in ["tokenizer.json", "preprocessor_config.json"] {
        let path = dir.join(name);
        if !path.is_file() {
            return Err(VlmError::InvalidConfig(format!(
                "missing {}",
                path.display()
            )));
        }
    }
    let weights = dir.join("model.safetensors");
    if fs::metadata(&weights)
        .map(|metadata| metadata.len())
        .unwrap_or(0)
        == 0
    {
        return Err(VlmError::InvalidConfig(format!(
            "missing or empty {}",
            weights.display()
        )));
    }
    Ok(())
}

fn enabled(get: &impl Fn(&str) -> Option<OsString>, name: &str) -> VlmResult<bool> {
    let Some(value) = get(name) else {
        return Ok(false);
    };
    let value = value.into_string().map_err(|_| {
        VlmError::InvalidConfig(format!("{name} must be valid Unicode and true or false"))
    })?;
    match value.as_str() {
        "false" | "0" => Ok(false),
        "true" | "1" => Ok(true),
        value => Err(VlmError::InvalidConfig(format!(
            "{name} must be true or false, got {value:?}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use mistralrs::RequestLike;
    use std::{collections::BTreeMap, fs};

    fn env(values: &[(&str, &str)]) -> impl Fn(&str) -> Option<OsString> {
        let values = values
            .iter()
            .map(|(key, value)| ((*key).to_owned(), OsString::from(value)))
            .collect::<BTreeMap<_, _>>();
        move |key| values.get(key).cloned()
    }

    #[test]
    fn download_requires_explicit_opt_in() {
        assert!(MistralRsConfig::from_env_with(env(&[])).is_err());
    }

    #[derive(Debug)]
    struct MistralrsTestError {
        message: String,
        source: Option<Box<dyn std::error::Error + Send + Sync>>,
    }

    impl MistralrsTestError {
        fn new(message: impl Into<String>) -> Self {
            Self {
                message: message.into(),
                source: None,
            }
        }

        fn with_source(mut self, source: impl std::error::Error + Send + Sync + 'static) -> Self {
            self.source = Some(Box::new(source));
            self
        }
    }

    impl std::fmt::Display for MistralrsTestError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str(&self.message)
        }
    }

    impl std::error::Error for MistralrsTestError {
        fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
            self.source
                .as_ref()
                .map(|e| e.as_ref() as &(dyn std::error::Error + 'static))
        }
    }

    #[test]
    fn mistralrs_build_error_keeps_root_cause_and_redacts_secrets() {
        let secret = "hf_mistralrs_secret_12345";
        let root = MistralrsTestError::new("root cause: safetensors blob missing");
        // The build path boxes an anyhow chain, whose `source()` walks directly.
        let error = MistralrsTestError::new(format!(
            "download failed: Authorization: Bearer {secret}, HF_TOKEN={secret}"
        ))
        .with_source(root);

        let message = mistralrs_transport_error("mistralrs model build", &error, None).to_string();

        assert!(!message.contains(secret), "leaked {secret}: {message}");
        assert!(message.contains("download failed"), "{message}");
        assert!(message.contains("safetensors blob missing"), "{message}");
    }

    #[test]
    fn mistralrs_inference_error_redacts_tokens_and_url_secrets() {
        let secret = "hf_inference_secret_67890";
        let inner = MistralrsTestError::new(format!(
            "response decode failed: token={secret} at https://hf.example/model?token={secret}"
        ));
        let error = mistralrs::error::Error::Inference(Box::new(inner));

        // The SDK surfaces its boxed inner through `source_inner()`, not
        // `std::error::Error::source()`, matching the real inference path.
        let inner = error.source_inner().map(|e| e as &dyn std::error::Error);
        let message = mistralrs_transport_error("mistralrs inference", &error, inner).to_string();

        assert!(!message.contains(secret), "leaked {secret}: {message}");
        assert!(message.contains("response decode failed"), "{message}");
    }

    #[test]
    fn local_model_is_strict_and_download_is_explicit() {
        let temp = tempfile::tempdir().unwrap();
        let error = MistralRsConfig::from_env_with(env(&[
            ("MINERU_VL_MODEL_DIR", temp.path().to_str().unwrap()),
            ("MINERU_VL_AUTO_DOWNLOAD", "true"),
        ]))
        .unwrap_err();
        assert!(error.to_string().contains("cannot read"));
        assert!(matches!(
            MistralRsConfig::from_env_with(env(&[("MINERU_VL_AUTO_DOWNLOAD", "true"),]))
                .unwrap()
                .source,
            MistralRsModelSource::Download { .. }
        ));
    }

    #[test]
    fn validates_complete_local_model_directory() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(
            temp.path().join("config.json"),
            r#"{"architectures":["Qwen2VLForConditionalGeneration"],"model_type":"qwen2_vl"}"#,
        )
        .unwrap();
        fs::write(temp.path().join("tokenizer.json"), "{}").unwrap();
        fs::write(temp.path().join("preprocessor_config.json"), "{}").unwrap();
        fs::write(temp.path().join("model.safetensors"), "weights").unwrap();
        validate_model_dir(temp.path(), "MINERU_VL_MODEL_DIR").unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn non_unicode_local_path_is_not_discarded_for_download() {
        use std::os::unix::ffi::OsStringExt;

        let path = OsString::from_vec(b"/definitely-not-a-model-\xff".to_vec());
        let error = MistralRsConfig::from_env_with(|name| match name {
            "MINERU_VL_MODEL_DIR" => Some(path.clone()),
            "MINERU_VL_AUTO_DOWNLOAD" => Some(OsString::from("true")),
            _ => None,
        })
        .unwrap_err();
        assert!(error.to_string().contains("not a directory"));
    }

    #[cfg(unix)]
    #[test]
    fn non_unicode_boolean_is_a_configuration_error() {
        use std::os::unix::ffi::OsStringExt;

        let error = MistralRsConfig::from_env_with(|name| {
            (name == "MINERU_VL_AUTO_DOWNLOAD").then(|| OsString::from_vec(vec![0xff]))
        })
        .unwrap_err();
        assert!(error.to_string().contains("valid Unicode"));
    }

    fn image() -> VlmImageInput {
        let mut bytes = Vec::new();
        image::DynamicImage::new_rgb8(1, 1)
            .write_to(
                &mut std::io::Cursor::new(&mut bytes),
                image::ImageFormat::Png,
            )
            .unwrap();
        VlmImageInput::Bytes {
            data: Bytes::from(bytes),
            media_type: Some("image/png".into()),
        }
    }

    #[test]
    fn request_adapter_preserves_image_order_and_maps_supported_sampling() {
        let mut request = request_builder(VlmRequest {
            images: vec![image(), image()],
            prompt: Some("<image> layout".into()),
            sampling: Some(SamplingParams {
                temperature: Some(0.2),
                top_p: Some(0.8),
                max_new_tokens: Some(16),
                ..Default::default()
            }),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(request.images_ref().len(), 2);
        assert!(!format!("{:?}", request.take_messages()).contains("<image>"));
        let sampling = request.take_sampling_params();
        assert!((sampling.temperature.unwrap() - 0.2).abs() < 1e-6);
        assert!((sampling.top_p.unwrap() - 0.8).abs() < 1e-6);
        assert_eq!(sampling.max_len, Some(16));
    }

    #[test]
    fn request_adapter_defaults_max_len_to_512_when_not_explicit() {
        let mut request = request_builder(VlmRequest {
            images: vec![image()],
            sampling: Some(SamplingParams {
                temperature: Some(0.2),
                ..Default::default()
            }),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(request.take_sampling_params().max_len, Some(512));
    }

    #[test]
    fn request_adapter_rejects_unadmitted_images() {
        for image in [
            VlmImageInput::None,
            VlmImageInput::RemoteUrl("https://example.test".parse().unwrap()),
        ] {
            assert!(
                request_builder(VlmRequest {
                    images: vec![image],
                    ..Default::default()
                })
                .is_err()
            );
        }
    }

    #[test]
    fn builder_source_uses_the_fixed_download_model() {
        assert_eq!(
            builder_source(
                &MistralRsModelSource::Download {
                    model_id: MINERU_MODEL_ID,
                    revision: MINERU_MODEL_REVISION,
                },
                "model path"
            )
            .unwrap(),
            MINERU_MODEL_ID
        );
    }

    #[cfg(unix)]
    #[test]
    fn builder_source_rejects_non_unicode_local_path() {
        use std::os::unix::ffi::OsStringExt;

        let error = builder_source(
            &MistralRsModelSource::Local(PathBuf::from(OsString::from_vec(vec![0xff]))),
            "model path",
        )
        .unwrap_err();
        assert!(error.to_string().contains("model path"));
    }

    #[test]
    fn from_parts_prefers_local_model_path() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(
            temp.path().join("config.json"),
            r#"{"architectures":["Qwen2VLForConditionalGeneration"],"model_type":"qwen2_vl"}"#,
        )
        .unwrap();
        fs::write(temp.path().join("tokenizer.json"), "{}").unwrap();
        fs::write(temp.path().join("preprocessor_config.json"), "{}").unwrap();
        fs::write(temp.path().join("model.safetensors"), "weights").unwrap();

        // A local path wins even when download would otherwise be allowed.
        let config = MistralRsConfig::from_parts(Some(temp.path().to_path_buf()), true).unwrap();
        assert_eq!(
            config.source,
            MistralRsModelSource::Local(temp.path().to_path_buf())
        );
    }

    #[test]
    fn from_parts_downloads_by_default_when_no_path() {
        let config = MistralRsConfig::from_parts(None, true).unwrap();
        assert!(matches!(
            config.source,
            MistralRsModelSource::Download { .. }
        ));
    }

    #[test]
    fn from_parts_names_both_fixes_when_no_path_and_no_download() {
        let error = MistralRsConfig::from_parts(None, false).unwrap_err();
        let message = error.to_string();
        assert!(message.contains("--model-path"), "{message}");
        assert!(message.contains("--allow-download=false"), "{message}");
    }

    #[test]
    fn from_parts_uses_model_path_label_in_validation_errors() {
        let temp = tempfile::tempdir().unwrap();
        let error =
            MistralRsConfig::from_parts(Some(temp.path().join("absent")), false).unwrap_err();
        let message = error.to_string();
        assert!(message.contains("model path"), "{message}");
        assert!(!message.contains("MINERU_VL_MODEL_DIR"), "{message}");
    }

    /// Real-model gate for the vendored mistralrs-core special-token patch.
    ///
    /// Requires weights on disk — never downloads. Feeds the page through the
    /// production `prepare_for_layout` pipeline (1036x1036 PNG), asserts the
    /// raw reply keeps `<|box_start|>` and parses to >=1 block with legal
    /// normalized bboxes, then re-predicts on the same loaded client and
    /// asserts the greedy layout reply is byte-for-byte deterministic.
    /// Run explicitly with:
    ///
    ///   MINERU_VL_MODEL_DIR=/path/to/MinerU2.5-2509-1.2B \
    ///   MINERU_VL_SMOKE_IMAGE=/path/to/page.png \
    ///   cargo test --release --features mistralrs --lib \
    ///     mistralrs_client::tests::real_model_layout_reply_preserves_special_tokens \
    ///     -- --ignored --exact
    #[tokio::test]
    #[ignore = "requires local MinerU weights + image (MINERU_VL_MODEL_DIR, MINERU_VL_SMOKE_IMAGE)"]
    async fn real_model_layout_reply_preserves_special_tokens() {
        let model_dir = env::var_os("MINERU_VL_MODEL_DIR").unwrap_or_else(|| {
            panic!(
                "skipped: set MINERU_VL_MODEL_DIR to a local Qwen2-VL MinerU model \
                 directory (this gate never downloads the 2.3 GB weights)"
            )
        });
        let image_path = env::var_os("MINERU_VL_SMOKE_IMAGE").unwrap_or_else(|| {
            panic!(
                "skipped: set MINERU_VL_SMOKE_IMAGE to a local page image to render \
                 the layout reply"
            )
        });
        let config = MistralRsConfig {
            source: MistralRsModelSource::Local(PathBuf::from(model_dir)),
            source_name: "MINERU_VL_MODEL_DIR",
        };
        let data = fs::read(&image_path).unwrap_or_else(|error| {
            panic!(
                "cannot read MINERU_VL_SMOKE_IMAGE {}: {error}",
                image_path.to_string_lossy()
            )
        });
        let data = Bytes::from(data);
        // Production feeds layout detection the prepared 1036x1036 PNG, not the
        // raw page scan; feeding the raw JPEG makes the model reply in table
        // protocol instead of boxed layout (smoke-only, keep faithful).
        let preprocessor = crate::MinerUVlmPreprocessor {
            config: crate::MinerUVlmConfig::default(),
        };
        let prepared = preprocessor
            .prepare_for_layout(image::load_from_memory(&data).unwrap())
            .expect("prepare_for_layout");
        // Greedy layout sampling mirrors the production "[layout]" config.
        let request = || VlmRequest {
            images: vec![VlmImageInput::Bytes {
                data: prepared.image.data.clone(),
                media_type: Some(prepared.image.media_type.clone()),
            }],
            prompt: Some("\nLayout Detection:".into()),
            sampling: Some(SamplingParams {
                temperature: Some(0.0),
                top_p: Some(0.01),
                top_k: Some(1),
                ..Default::default()
            }),
            ..Default::default()
        };
        let client = MistralRsClient::connect(config, 1 << 20).await.unwrap();
        let reply = client.predict(request(), 1 << 20).await.unwrap();
        assert!(
            reply.contains("<|box_start|>"),
            "layout reply lost MinerU special tokens; got: {reply}"
        );

        // Parse the raw reply through the production preprocessor and require
        // every block to carry a legal normalized bbox.
        let blocks = preprocessor
            .parse_layout_output(&reply)
            .expect("raw layout reply must parse through the production parser");
        assert!(
            !blocks.is_empty(),
            "layout reply parsed to zero blocks; got: {reply}"
        );
        for block in &blocks {
            let bbox = block.bbox;
            crate::NormalizedBbox::new(bbox.left, bbox.top, bbox.right, bbox.bottom)
                .expect("block bbox must be a legal normalized coordinate");
        }

        // Determinism: same loaded client, same page, same greedy sampling must
        // reproduce the raw reply byte-for-byte, and the repeat must parse too.
        let reply_again = client.predict(request(), 1 << 20).await.unwrap();
        assert_eq!(
            reply, reply_again,
            "greedy layout sampling must be deterministic on the same client"
        );
        let blocks_again = preprocessor
            .parse_layout_output(&reply_again)
            .expect("second raw layout reply must also parse");
        assert!(
            !blocks_again.is_empty(),
            "second layout reply parsed to zero blocks"
        );
    }
}
