use crate::{VlmError, VlmResult};
use std::{env, fmt, time::Duration};
use url::Url;

#[derive(Clone, PartialEq, Eq)]
pub struct VlmHeader {
    name: String,
    value: String,
}
impl VlmHeader {
    pub fn new(name: impl Into<String>, value: impl Into<String>) -> VlmResult<Self> {
        let name = name.into();
        let value = value.into();
        if name.is_empty() || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') {
            return Err(VlmError::InvalidHeader("invalid header name".into()));
        }
        if value.contains(['\r', '\n'])
            || matches!(
                name.to_ascii_lowercase().as_str(),
                "host" | "content-length" | "content-type"
            )
        {
            return Err(VlmError::InvalidHeader("reserved or invalid header".into()));
        }
        Ok(Self { name, value })
    }
    pub fn name(&self) -> &str {
        &self.name
    }
    pub fn value(&self) -> &str {
        &self.value
    }
}
impl fmt::Debug for VlmHeader {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("VlmHeader")
            .field("name", &self.name)
            .field("value", &"REDACTED")
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct SamplingParams {
    pub temperature: Option<f32>,
    pub top_p: Option<f32>,
    pub top_k: Option<i32>,
    pub presence_penalty: Option<f32>,
    pub frequency_penalty: Option<f32>,
    pub repetition_penalty: Option<f32>,
    pub no_repeat_ngram_size: Option<i32>,
    pub max_new_tokens: Option<u32>,
}
#[derive(Clone)]
pub struct VlmHttpConfig {
    pub model_name: Option<String>,
    pub server_url: Option<Url>,
    /// Set only when MINERU_VL_SERVER was present but could not be parsed.
    pub invalid_server_url: bool,
    pub headers: Vec<VlmHeader>,
    pub prompt: Option<String>,
    pub system_prompt: Option<String>,
    pub sampling_params: Option<SamplingParams>,
    pub text_before_image: bool,
    pub allow_truncated_content: bool,
    pub max_concurrency: usize,
    pub http_timeout: Duration,
    pub connect_timeout: Duration,
    pub max_connections: Option<usize>,
    pub max_keepalive_connections: usize,
    pub keepalive_expiry: Duration,
    pub debug: bool,
    pub max_retries: usize,
    pub retry_backoff_factor: f32,
    pub skip_model_name_checking: bool,
    pub max_image_bytes: usize,
    pub max_decoded_pixels: u64,
    pub max_images_per_request: usize,
    pub allow_remote_images: bool,
    pub allow_private_remote_images: bool,
    pub max_redirects: usize,
    pub max_diagnostic_bytes: usize,
    pub max_response_bytes: usize,
}
impl fmt::Debug for VlmHttpConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("VlmHttpConfig")
            .field("model_name", &self.model_name)
            .field("server_url_configured", &self.server_url.is_some())
            .field("headers", &self.headers)
            .finish_non_exhaustive()
    }
}
impl Default for VlmHttpConfig {
    fn default() -> Self {
        Self {
            model_name: env_nonempty("MINERU_VL_MODEL_NAME"),
            server_url: env_nonempty("MINERU_VL_SERVER").and_then(|s| Url::parse(&s).ok()),
            invalid_server_url: env_nonempty("MINERU_VL_SERVER")
                .is_some_and(|s| Url::parse(&s).is_err()),
            headers: vec![],
            prompt: None,
            system_prompt: None,
            sampling_params: None,
            text_before_image: false,
            allow_truncated_content: false,
            max_concurrency: 100,
            http_timeout: Duration::from_secs(600),
            connect_timeout: Duration::from_secs(10),
            max_connections: None,
            max_keepalive_connections: 20,
            keepalive_expiry: Duration::from_secs(5),
            debug: env_debug().unwrap_or(false),
            max_retries: 3,
            retry_backoff_factor: 0.5,
            skip_model_name_checking: false,
            max_image_bytes: 32 * 1024 * 1024,
            max_decoded_pixels: 100_000_000,
            max_images_per_request: 64,
            allow_remote_images: false,
            allow_private_remote_images: false,
            max_redirects: 3,
            max_diagnostic_bytes: 64 * 1024,
            max_response_bytes: 10 * 1024 * 1024,
        }
    }
}
impl VlmHttpConfig {
    pub fn authorization(&self) -> Option<String> {
        self.headers
            .iter()
            .find(|h| h.name.eq_ignore_ascii_case("authorization"))
            .map(|h| h.value.clone())
            .or_else(|| env_nonempty("MINERU_VL_API_KEY").map(|v| format!("Bearer {v}")))
    }
}
fn env_nonempty(name: &str) -> Option<String> {
    env::var(name)
        .ok()
        .map(|v| v.trim().to_owned())
        .filter(|v| !v.is_empty())
}
fn env_debug() -> Option<bool> {
    env::var("MINERU_VL_DEBUG_ENABLE")
        .ok()
        .and_then(|v| match v.to_ascii_lowercase().as_str() {
            "1" | "true" | "yes" => Some(true),
            "0" | "false" | "no" => Some(false),
            _ => None,
        })
}

#[derive(Debug, Clone)]
pub struct MinerUVlmConfig {
    pub prompts: std::collections::BTreeMap<String, String>,
    pub sampling_params: std::collections::BTreeMap<String, SamplingParams>,
    pub layout_image_size: (u32, u32),
    pub min_image_edge: u32,
    pub max_image_edge_ratio: u32,
    pub simple_post_process: bool,
    pub handle_equation_block: bool,
    pub abandon_list: bool,
    pub abandon_paratext: bool,
    pub image_analysis: bool,
    pub incremental_priority: bool,
    pub enable_table_formula_eq_wrap: bool,
    pub enable_cross_page_table_merge: bool,
}
impl Default for MinerUVlmConfig {
    fn default() -> Self {
        let base_sampling = SamplingParams {
            temperature: Some(0.0),
            top_p: Some(0.01),
            top_k: Some(1),
            presence_penalty: Some(0.0),
            frequency_penalty: Some(0.0),
            repetition_penalty: Some(1.0),
            no_repeat_ngram_size: Some(100),
            max_new_tokens: None,
        };
        let mut sampling_params = std::collections::BTreeMap::from([
            ("[layout]".into(), base_sampling.clone()),
            ("table".into(), base_sampling.clone()),
        ]);
        for key in [
            "equation",
            "image",
            "chart",
            "[default]",
            "[cross_page_table_merge]",
        ] {
            sampling_params.insert(
                key.into(),
                SamplingParams {
                    presence_penalty: Some(1.0),
                    frequency_penalty: Some(0.05),
                    ..base_sampling.clone()
                },
            );
        }
        sampling_params.insert(
            "table".into(),
            SamplingParams {
                presence_penalty: Some(1.0),
                frequency_penalty: Some(0.005),
                ..base_sampling
            },
        );
        Self {
            prompts: std::collections::BTreeMap::from([
                ("table".into(), "\nTable Recognition:".into()),
                ("equation".into(), "\nFormula Recognition:".into()),
                ("image".into(), "\nImage Analysis:".into()),
                ("chart".into(), "\nImage Analysis:".into()),
                ("[default]".into(), "\nText Recognition:".into()),
                ("[layout]".into(), "\nLayout Detection:".into()),
                ("[cross_page_table_merge]".into(), "".into()),
            ]),
            sampling_params,
            layout_image_size: (1036, 1036),
            min_image_edge: 28,
            max_image_edge_ratio: 50,
            simple_post_process: false,
            handle_equation_block: true,
            abandon_list: false,
            abandon_paratext: false,
            image_analysis: false,
            incremental_priority: false,
            enable_table_formula_eq_wrap: false,
            enable_cross_page_table_merge: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{LazyLock, Mutex};

    static ENV_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

    #[test]
    fn defaults_freeze_raw_model_state() {
        let config = VlmHttpConfig::default();
        assert_eq!(config.max_concurrency, 100);
        assert_eq!(config.http_timeout, Duration::from_secs(600));
        assert_eq!(config.max_response_bytes, 10 * 1024 * 1024);
        assert!(!config.skip_model_name_checking);
    }

    #[test]
    fn mineru_defaults_match_upstream_phase_zero_skeleton() {
        let config = MinerUVlmConfig::default();
        assert_eq!(
            config.prompts,
            std::collections::BTreeMap::from([
                ("table".into(), "\nTable Recognition:".into()),
                ("equation".into(), "\nFormula Recognition:".into()),
                ("image".into(), "\nImage Analysis:".into()),
                ("chart".into(), "\nImage Analysis:".into()),
                ("[default]".into(), "\nText Recognition:".into()),
                ("[layout]".into(), "\nLayout Detection:".into()),
                ("[cross_page_table_merge]".into(), "".into()),
            ])
        );
        assert_eq!(config.sampling_params.len(), 7);
        let table = &config.sampling_params["table"];
        assert_eq!(table.presence_penalty, Some(1.0));
        assert_eq!(table.frequency_penalty, Some(0.005));
        let equation = &config.sampling_params["equation"];
        assert_eq!(equation.presence_penalty, Some(1.0));
        assert_eq!(equation.frequency_penalty, Some(0.05));
        let layout = &config.sampling_params["[layout]"];
        assert_eq!(layout.presence_penalty, Some(0.0));
        assert_eq!(layout.frequency_penalty, Some(0.0));
        for profile in config.sampling_params.values() {
            assert_eq!(profile.temperature, Some(0.0));
            assert_eq!(profile.top_p, Some(0.01));
            assert_eq!(profile.top_k, Some(1));
            assert_eq!(profile.repetition_penalty, Some(1.0));
            assert_eq!(profile.no_repeat_ngram_size, Some(100));
            assert_eq!(profile.max_new_tokens, None);
        }
        assert!(!config.image_analysis);
        assert!(!config.enable_table_formula_eq_wrap);
        assert!(!config.enable_cross_page_table_merge);
    }

    #[test]
    fn headers_validate_and_redact() {
        let header = VlmHeader::new("X-Secret", "super-secret").unwrap();
        assert!(!format!("{header:?}").contains("super-secret"));
        assert!(VlmHeader::new("Host", "example.test").is_err());
        assert!(VlmHeader::new("X-Test", "bad\nvalue").is_err());
    }

    #[test]
    fn http_config_debug_redacts_server_url() {
        let config = VlmHttpConfig {
            server_url: Some(
                Url::parse("https://user:secret@example.test/api?token=secret").unwrap(),
            ),
            ..Default::default()
        };

        let debug = format!("{config:?}");
        assert!(debug.contains("server_url_configured: true"));
        assert!(!debug.contains("user"));
        assert!(!debug.contains("secret"));
        assert!(!debug.contains("example.test"));
    }

    #[test]
    fn authorization_header_takes_precedence_and_debug_redacts_secrets() {
        let _guard = ENV_LOCK.lock().unwrap();
        let previous = env::var_os("MINERU_VL_API_KEY");
        // SAFETY: the lock serializes this test's process-wide environment mutation.
        unsafe { env::set_var("MINERU_VL_API_KEY", "environment-secret") };

        let config = VlmHttpConfig {
            headers: vec![VlmHeader::new("Authorization", "Bearer supplied-token").unwrap()],
            ..Default::default()
        };
        assert_eq!(
            config.authorization().as_deref(),
            Some("Bearer supplied-token")
        );
        let debug = format!("{config:?}");
        assert!(!debug.contains("supplied-token"));
        assert!(!debug.contains("environment-secret"));
        assert_eq!(
            VlmHttpConfig::default().authorization().as_deref(),
            Some("Bearer environment-secret")
        );

        // SAFETY: restore the process-wide environment before releasing the lock.
        unsafe {
            if let Some(value) = previous {
                env::set_var("MINERU_VL_API_KEY", value);
            } else {
                env::remove_var("MINERU_VL_API_KEY");
            }
        }
    }
}
