use serde::{Deserialize, Serialize};
use std::{fmt, path::PathBuf};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum OfficialTier {
    Flash,
    Basic,
    Standard,
    Advanced,
}
impl OfficialTier {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Flash => "flash",
            Self::Basic => "basic",
            Self::Standard => "standard",
            Self::Advanced => "advanced",
        }
    }
}
impl Default for OfficialTier {
    fn default() -> Self {
        Self::Standard
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum OfficialOcrMode {
    Auto,
    Txt,
    Ocr,
}
impl OfficialOcrMode {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Txt => "txt",
            Self::Ocr => "ocr",
        }
    }
}
impl Default for OfficialOcrMode {
    fn default() -> Self {
        Self::Auto
    }
}

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "snake_case")]
#[non_exhaustive]
pub struct HtmlSourceContext {
    pub source_uri: Option<String>,
    pub local_resource_root: Option<PathBuf>,
    pub transport_encoding: Option<String>,
}
impl HtmlSourceContext {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn with_source_uri(mut self, value: impl Into<String>) -> Self {
        self.source_uri = Some(value.into());
        self
    }
    pub fn with_local_resource_root(mut self, value: impl Into<PathBuf>) -> Self {
        self.local_resource_root = Some(value.into());
        self
    }
    pub fn with_transport_encoding(mut self, value: impl Into<String>) -> Self {
        self.transport_encoding = Some(value.into());
        self
    }
}
impl fmt::Debug for HtmlSourceContext {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HtmlSourceContext")
            .field(
                "source_uri",
                &self
                    .source_uri
                    .as_ref()
                    .map(|value| crate::official::error::redact_secret(value, 1024)),
            )
            .field(
                "local_resource_root",
                &self.local_resource_root.as_ref().map(|_| "[trusted root]"),
            )
            .field("transport_encoding", &self.transport_encoding)
            .finish()
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "snake_case")]
#[non_exhaustive]
pub struct OfficialVlmConfig {
    pub engine: String,
    pub server_url: String,
    pub api_key: String,
    pub model: String,
    pub http_timeout: u64,
    pub max_concurrency: usize,
}
impl Default for OfficialVlmConfig {
    fn default() -> Self {
        Self {
            engine: "auto".into(),
            server_url: String::new(),
            api_key: String::new(),
            model: String::new(),
            http_timeout: 600,
            max_concurrency: 100,
        }
    }
}
impl OfficialVlmConfig {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn with_engine(mut self, value: impl Into<String>) -> Self {
        self.engine = value.into();
        self
    }
    pub fn with_server_url(mut self, value: impl Into<String>) -> Self {
        self.server_url = value.into();
        self
    }
    pub fn with_api_key(mut self, value: impl Into<String>) -> Self {
        self.api_key = value.into();
        self
    }
    pub fn with_model(mut self, value: impl Into<String>) -> Self {
        self.model = value.into();
        self
    }
    pub fn with_http_timeout(mut self, value: u64) -> Self {
        self.http_timeout = value;
        self
    }
    pub fn with_max_concurrency(mut self, value: usize) -> Self {
        self.max_concurrency = value;
        self
    }
}
impl fmt::Debug for OfficialVlmConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OfficialVlmConfig")
            .field("engine", &self.engine)
            .field(
                "server_url",
                &crate::official::error::redact_secret(&self.server_url, 1024),
            )
            .field(
                "api_key",
                &(!self.api_key.is_empty()).then_some("[redacted]"),
            )
            .field("model", &self.model)
            .field("http_timeout", &self.http_timeout)
            .field("max_concurrency", &self.max_concurrency)
            .finish()
    }
}

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "snake_case")]
#[non_exhaustive]
pub struct OfficialParseOptions {
    pub tier: Option<OfficialTier>,
    pub ocr_mode: Option<OfficialOcrMode>,
    pub image_analysis: Option<bool>,
    pub page_range: Option<String>,
    pub source_context: Option<HtmlSourceContext>,
    pub vlm_config: Option<OfficialVlmConfig>,
    pub vlm_server_url: Option<String>,
    pub vlm_api_key: Option<String>,
    pub vlm_model: Option<String>,
}
impl fmt::Debug for OfficialParseOptions {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OfficialParseOptions")
            .field("tier", &self.tier)
            .field("ocr_mode", &self.ocr_mode)
            .field("image_analysis", &self.image_analysis)
            .field("page_range", &self.page_range)
            .field("source_context", &self.source_context)
            .field("vlm_config", &self.vlm_config)
            .field(
                "vlm_server_url",
                &self
                    .vlm_server_url
                    .as_ref()
                    .map(|value| crate::official::error::redact_secret(value, 1024)),
            )
            .field(
                "vlm_api_key",
                &self.vlm_api_key.as_ref().map(|_| "[redacted]"),
            )
            .field("vlm_model", &self.vlm_model)
            .finish()
    }
}

impl OfficialParseOptions {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn with_tier(mut self, value: OfficialTier) -> Self {
        self.tier = Some(value);
        self
    }
    pub fn with_ocr_mode(mut self, value: OfficialOcrMode) -> Self {
        self.ocr_mode = Some(value);
        self
    }
    pub fn with_image_analysis(mut self, value: bool) -> Self {
        self.image_analysis = Some(value);
        self
    }
    pub fn with_page_range(mut self, value: impl Into<String>) -> Self {
        self.page_range = Some(value.into());
        self
    }
    pub fn with_source_context(mut self, value: HtmlSourceContext) -> Self {
        self.source_context = Some(value);
        self
    }
    pub fn with_vlm_config(mut self, value: OfficialVlmConfig) -> Self {
        self.vlm_config = Some(value);
        self
    }
    pub fn with_vlm_server_url(mut self, value: impl Into<String>) -> Self {
        self.vlm_server_url = Some(value.into());
        self
    }
    pub fn with_vlm_api_key(mut self, value: impl Into<String>) -> Self {
        self.vlm_api_key = Some(value.into());
        self
    }
    pub fn with_vlm_model(mut self, value: impl Into<String>) -> Self {
        self.vlm_model = Some(value.into());
        self
    }

    pub(crate) fn validate(&self, suffix: &str) -> Result<(), String> {
        let tier = self.tier.unwrap_or_default();
        if !crate::official::input::suffixes().contains(&suffix) {
            return Err("unsupported official input format".into());
        }
        let native = !matches!(
            suffix,
            "pdf" | "png" | "jpg" | "jpeg" | "webp" | "gif" | "bmp" | "tiff" | "jp2"
        );
        if native && tier != OfficialTier::Flash {
            return Err("native document formats require the flash tier".into());
        }
        if self.page_range.as_deref().is_some_and(str::is_empty) {
            return Err("page_range must be nonempty".into());
        }
        if suffix != "pdf" && self.page_range.is_some() {
            return Err("page ranges are supported for PDF input only".into());
        }
        if self.vlm_config.is_some()
            && (self.vlm_server_url.is_some()
                || self.vlm_api_key.is_some()
                || self.vlm_model.is_some())
        {
            return Err("vlm_config replacement cannot be combined with legacy VLM aliases".into());
        }
        if let Some(config) = &self.vlm_config {
            if !["auto", "llama-cpp", "vllm", "lmdeploy", "mlx"].contains(&config.engine.as_str()) {
                return Err("unsupported VLM engine".into());
            }
            if config.http_timeout == 0 || config.max_concurrency == 0 {
                return Err("VLM timeout and concurrency must be positive".into());
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{OfficialParseOptions, OfficialVlmConfig};

    #[test]
    fn debug_redacts_vlm_url_secrets_and_preserves_useful_fields() {
        let options = OfficialParseOptions::new()
            .with_vlm_server_url(
                "https://user:password@vlm.example/v1?api_key=query-secret&token=token-secret",
            )
            .with_page_range("1-3")
            .with_vlm_model("public-model");
        let debug = format!("{options:?}");

        for secret in ["user", "password", "query-secret", "token-secret"] {
            assert!(!debug.contains(secret), "{secret} leaked in {debug}");
        }
        assert!(debug.contains("vlm_server_url: Some"));
        assert!(debug.contains("page_range: Some(\"1-3\")"));
        assert!(debug.contains("vlm_model: Some(\"public-model\")"));
    }

    #[test]
    fn debug_keeps_config_url_redaction() {
        let options = OfficialParseOptions::new().with_vlm_config(
            OfficialVlmConfig::new()
                .with_server_url("https://user:password@vlm.example/v1?api_key=config-secret"),
        );
        let debug = format!("{options:?}");

        for secret in ["user", "password", "config-secret"] {
            assert!(!debug.contains(secret), "{secret} leaked in {debug}");
        }
    }
}
