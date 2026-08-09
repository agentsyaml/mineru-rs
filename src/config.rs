use crate::{Error, Result};
use std::{fmt, time::Duration};
use url::Url;

#[derive(Clone, PartialEq, Eq)]
pub struct BearerToken(String);
impl BearerToken {
    pub fn new(token: impl Into<String>) -> Result<Self> {
        let token = token.into().trim().to_owned();
        if token.is_empty() || token.chars().any(|c| c.is_control() || c.is_whitespace()) {
            return Err(Error::InvalidConfig(
                "bearer token must not be empty or contain whitespace/control characters".into(),
            ));
        }
        Ok(Self(token))
    }
}
impl fmt::Debug for BearerToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("BearerToken(REDACTED)")
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Limits {
    pub max_pdf_bytes: usize,
    pub max_total_asset_bytes: usize,
    pub max_pages: usize,
    pub max_page_pixels: u64,
    pub max_response_bytes: usize,
    pub max_rendered_image_bytes: usize,
    pub max_in_flight_image_bytes: usize,
    pub max_blocks_per_page: usize,
    pub page_window_size: usize,
}
impl Default for Limits {
    fn default() -> Self {
        Self {
            max_pdf_bytes: 512 * 1024 * 1024,
            max_total_asset_bytes: 1024 * 1024 * 1024,
            max_pages: 10_000,
            max_page_pixels: 100_000_000,
            max_response_bytes: 10 * 1024 * 1024,
            max_rendered_image_bytes: 64 * 1024 * 1024,
            max_in_flight_image_bytes: 128 * 1024 * 1024,
            max_blocks_per_page: 256,
            page_window_size: 64,
        }
    }
}
impl Limits {
    pub fn validate(&self) -> Result<()> {
        if self.max_pdf_bytes == 0
            || self.max_total_asset_bytes == 0
            || self.max_pages == 0
            || self.max_page_pixels == 0
            || self.max_response_bytes == 0
            || self.max_rendered_image_bytes == 0
            || self.max_in_flight_image_bytes == 0
            || self.max_blocks_per_page == 0
            || self.page_window_size == 0
        {
            return Err(Error::InvalidConfig(
                "all limits must be greater than zero".into(),
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Timeouts {
    pub connect: Duration,
    pub request: Duration,
    pub total: Duration,
}
impl Default for Timeouts {
    fn default() -> Self {
        Self {
            connect: Duration::from_secs(10),
            request: Duration::from_secs(600),
            total: Duration::from_secs(24 * 60 * 60),
        }
    }
}
impl Timeouts {
    pub fn validate(&self) -> Result<()> {
        if self.connect.is_zero()
            || self.request.is_zero()
            || self.total.is_zero()
            || self.request > self.total
        {
            return Err(Error::InvalidConfig(
                "timeouts must be non-zero and request must not exceed total".into(),
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct ClientConfig {
    pub base_url: Url,
    pub model: String,
    pub bearer_token: Option<BearerToken>,
    pub limits: Limits,
    pub timeouts: Timeouts,
    pub request_concurrency: usize,
    pub render_workers: usize,
}
impl ClientConfig {
    pub fn new(base_url: impl AsRef<str>, model: impl Into<String>) -> Result<Self> {
        let config = Self::new_with_resolution(base_url, model, |_| None)?;
        Ok(config)
    }

    /// Creates a config honoring the canonical runtime environment spellings for the legacy
    /// `Limits`/`Timeouts` (the same env names the canonical route resolves). Values follow the
    /// strict default -> frozen environment -> explicit CLI precedence of the core seam.
    pub fn from_env(base_url: impl AsRef<str>, model: impl Into<String>) -> Result<Self> {
        Self::new_with_resolution(base_url, model, |name| std::env::var_os(name))
    }

    fn new_with_resolution(
        base_url: impl AsRef<str>,
        model: impl Into<String>,
        env: impl Fn(&str) -> Option<std::ffi::OsString>,
    ) -> Result<Self> {
        let mut base_url = Url::parse(base_url.as_ref())
            .map_err(|e| Error::InvalidConfig(format!("invalid base URL: {e}")))?;
        if !matches!(base_url.scheme(), "http" | "https")
            || !base_url.username().is_empty()
            || base_url.password().is_some()
            || base_url.query().is_some()
            || base_url.fragment().is_some()
        {
            return Err(Error::InvalidConfig(
                "base URL must be http(s) without userinfo, query, or fragment".into(),
            ));
        }
        if !base_url.path().ends_with('/') {
            base_url.set_path(&format!("{}/", base_url.path()));
        }
        let model = model.into().trim().to_owned();
        if model.is_empty() {
            return Err(Error::InvalidConfig("model is required".into()));
        }
        let overrides = crate::command::env::parse_core_overrides(&env)
            .map_err(|error| Error::InvalidConfig(error))?;
        let (limits, timeouts) = crate::config::resolve_legacy_policy(&overrides);
        let config = Self {
            base_url,
            model,
            bearer_token: None,
            limits,
            timeouts,
            request_concurrency: overrides.http_max_concurrency.unwrap_or(100),
            render_workers: overrides.render_workers.unwrap_or(3),
        };
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<()> {
        self.limits.validate()?;
        self.timeouts.validate()?;
        if self.request_concurrency == 0 || self.render_workers == 0 {
            return Err(Error::InvalidConfig(
                "request concurrency and render workers must be greater than zero".into(),
            ));
        }
        Ok(())
    }
}

/// Maps the resolved core policy onto the legacy `Limits`/`Timeouts` shapes without duplicating
/// arbitrary maxima. Every field follows the same env spelling used by the canonical route.
pub(crate) fn resolve_legacy_policy(
    overrides: &crate::command::env::CoreOverrides,
) -> (Limits, Timeouts) {
    let defaults = Limits::default();
    let timeout_defaults = Timeouts::default();
    let limits = Limits {
        max_pdf_bytes: overrides.max_pdf_bytes.unwrap_or(defaults.max_pdf_bytes),
        max_total_asset_bytes: overrides
            .max_total_asset_bytes
            .unwrap_or(defaults.max_total_asset_bytes),
        max_pages: overrides.max_pages.unwrap_or(defaults.max_pages),
        max_page_pixels: overrides
            .max_page_pixels
            .unwrap_or(defaults.max_page_pixels),
        max_response_bytes: overrides
            .http_max_response_bytes
            .unwrap_or(defaults.max_response_bytes),
        max_rendered_image_bytes: overrides
            .max_rendered_image_bytes
            .unwrap_or(defaults.max_rendered_image_bytes),
        max_in_flight_image_bytes: overrides
            .max_in_flight_image_bytes
            .unwrap_or(defaults.max_in_flight_image_bytes),
        max_blocks_per_page: overrides
            .max_layout_blocks_per_page
            .unwrap_or(defaults.max_blocks_per_page),
        page_window_size: overrides
            .processing_window_size
            .unwrap_or(defaults.page_window_size),
    };
    let timeouts = Timeouts {
        connect: overrides
            .connect_timeout
            .unwrap_or(timeout_defaults.connect),
        request: overrides.http_timeout.unwrap_or(timeout_defaults.request),
        total: overrides.total_deadline.unwrap_or(timeout_defaults.total),
    };
    (limits, timeouts)
}

#[cfg(test)]
mod tests {
    use super::{BearerToken, ClientConfig, Limits, Timeouts};
    use std::time::Duration;

    #[test]
    fn defaults_use_long_document_profile() {
        let config = ClientConfig::new("https://example.test", "model").unwrap();
        assert_eq!(
            config.limits,
            Limits {
                max_pdf_bytes: 512 * 1024 * 1024,
                max_total_asset_bytes: 1024 * 1024 * 1024,
                max_pages: 10_000,
                max_page_pixels: 100_000_000,
                max_response_bytes: 10 * 1024 * 1024,
                max_rendered_image_bytes: 64 * 1024 * 1024,
                max_in_flight_image_bytes: 128 * 1024 * 1024,
                max_blocks_per_page: 256,
                page_window_size: 64,
            }
        );
        assert_eq!(
            config.timeouts,
            Timeouts {
                connect: Duration::from_secs(10),
                request: Duration::from_secs(600),
                total: Duration::from_secs(24 * 60 * 60),
            }
        );
        assert_eq!(config.request_concurrency, 100);
        assert_eq!(config.render_workers, 3);
    }

    #[test]
    fn validates_and_normalizes() {
        assert!(BearerToken::new("bad token").is_err());
        assert!(ClientConfig::new("https://user@example.test", "model").is_err());
        assert_eq!(
            ClientConfig::new("https://example.test/v1", "model")
                .unwrap()
                .base_url
                .as_str(),
            "https://example.test/v1/"
        );
    }

    #[test]
    fn rejects_zero_total_asset_bytes() {
        let limits = Limits {
            max_total_asset_bytes: 0,
            ..Limits::default()
        };
        assert!(limits.validate().is_err());
    }

    #[test]
    fn rejects_zero_blocks_per_page() {
        assert!(
            Limits {
                max_blocks_per_page: 0,
                ..Limits::default()
            }
            .validate()
            .is_err()
        );
    }
}
