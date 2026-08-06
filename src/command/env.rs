use crate::{OfficialPdfOptions, VlmHttpConfig};
use std::{ffi::OsString, time::Duration};

/// Canonical default for the official page-admission semaphore.
const DEFAULT_PAGE_CONCURRENCY: usize = 4;

/// Typed, crate-private CLI/environment override set for core operational policy.
///
/// Every field is `None` when the source did not configure the knob. Fields are applied with
/// strict validation: malformed, non-finite, zero-where-invalid, overflow, or platform
/// unrepresentable values fail resolution before any network or output work.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CoreOverrides {
    pub processing_window_size: Option<usize>,
    pub page_concurrency: Option<usize>,
    pub render_workers: Option<usize>,
    pub render_timeout: Option<Duration>,
    pub max_pdf_bytes: Option<usize>,
    pub max_pages: Option<usize>,
    pub max_page_pixels: Option<u64>,
    pub max_rendered_image_bytes: Option<usize>,
    pub max_in_flight_image_bytes: Option<usize>,
    pub max_raw_output_bytes: Option<usize>,
    pub max_layout_blocks_per_page: Option<usize>,
    pub max_semantic_requests_per_page: Option<usize>,
    pub batch_size: Option<usize>,
    pub max_encoded_request_bytes: Option<usize>,
    pub max_encoded_batch_bytes: Option<usize>,
    pub max_total_asset_bytes: Option<usize>,
    pub max_staged_text_bytes: Option<usize>,
    pub total_deadline: Option<Duration>,
    pub formula: Option<bool>,
    pub table: Option<bool>,
    pub image_analysis: Option<bool>,
    pub vlm_debug: Option<bool>,
    pub http_max_concurrency: Option<usize>,
    pub http_timeout: Option<Duration>,
    pub connect_timeout: Option<Duration>,
    pub http_max_keepalive_connections: Option<usize>,
    pub http_keepalive_expiry: Option<Duration>,
    pub http_max_retries: Option<usize>,
    pub http_retry_backoff_factor: Option<f32>,
    pub max_remote_image_bytes: Option<usize>,
    pub max_decoded_pixels: Option<u64>,
    pub max_images_per_request: Option<usize>,
    pub max_redirects: Option<usize>,
    pub http_max_response_bytes: Option<usize>,
}

/// Resolved core policy: route options, official page concurrency, and HTTP transport config.
#[derive(Clone, Debug)]
pub struct ResolvedCore {
    pub route: OfficialPdfOptions,
    pub page_concurrency: usize,
    pub http: VlmHttpConfig,
}

/// Strictly parses the frozen core environment into typed overrides.
pub fn parse_core_overrides(
    lookup: &impl Fn(&str) -> Option<OsString>,
) -> Result<CoreOverrides, String> {
    Ok(CoreOverrides {
        processing_window_size: positive_usize(
            lookup("MINERU_PROCESSING_WINDOW_SIZE"),
            "MINERU_PROCESSING_WINDOW_SIZE",
        )?,
        page_concurrency: positive_usize(
            lookup("MINERU_OFFICIAL_PAGE_CONCURRENCY"),
            "MINERU_OFFICIAL_PAGE_CONCURRENCY",
        )?,
        render_workers: positive_usize(
            lookup("MINERU_PDF_RENDER_THREADS"),
            "MINERU_PDF_RENDER_THREADS",
        )?,
        render_timeout: positive_seconds(
            lookup("MINERU_PDF_RENDER_TIMEOUT"),
            "MINERU_PDF_RENDER_TIMEOUT",
        )?,
        max_pdf_bytes: positive_usize(lookup("MINERU_MAX_PDF_BYTES"), "MINERU_MAX_PDF_BYTES")?,
        max_pages: positive_usize(lookup("MINERU_MAX_PAGES"), "MINERU_MAX_PAGES")?,
        max_page_pixels: positive_u64(lookup("MINERU_MAX_PAGE_PIXELS"), "MINERU_MAX_PAGE_PIXELS")?,
        max_rendered_image_bytes: positive_usize(
            lookup("MINERU_MAX_RENDERED_IMAGE_BYTES"),
            "MINERU_MAX_RENDERED_IMAGE_BYTES",
        )?,
        max_in_flight_image_bytes: positive_usize(
            lookup("MINERU_MAX_IN_FLIGHT_IMAGE_BYTES"),
            "MINERU_MAX_IN_FLIGHT_IMAGE_BYTES",
        )?,
        max_raw_output_bytes: positive_usize(
            lookup("MINERU_MAX_RAW_OUTPUT_BYTES"),
            "MINERU_MAX_RAW_OUTPUT_BYTES",
        )?,
        max_layout_blocks_per_page: positive_usize(
            lookup("MINERU_MAX_LAYOUT_BLOCKS_PER_PAGE"),
            "MINERU_MAX_LAYOUT_BLOCKS_PER_PAGE",
        )?,
        max_semantic_requests_per_page: positive_usize(
            lookup("MINERU_MAX_SEMANTIC_REQUESTS_PER_PAGE"),
            "MINERU_MAX_SEMANTIC_REQUESTS_PER_PAGE",
        )?,
        batch_size: positive_usize(lookup("MINERU_BATCH_SIZE"), "MINERU_BATCH_SIZE")?,
        max_encoded_request_bytes: positive_usize(
            lookup("MINERU_MAX_ENCODED_REQUEST_BYTES"),
            "MINERU_MAX_ENCODED_REQUEST_BYTES",
        )?,
        max_encoded_batch_bytes: positive_usize(
            lookup("MINERU_MAX_ENCODED_BATCH_BYTES"),
            "MINERU_MAX_ENCODED_BATCH_BYTES",
        )?,
        max_total_asset_bytes: positive_usize(
            lookup("MINERU_MAX_TOTAL_ASSET_BYTES"),
            "MINERU_MAX_TOTAL_ASSET_BYTES",
        )?,
        max_staged_text_bytes: positive_usize(
            lookup("MINERU_MAX_STAGED_TEXT_BYTES"),
            "MINERU_MAX_STAGED_TEXT_BYTES",
        )?,
        total_deadline: positive_seconds(
            lookup("MINERU_TOTAL_DEADLINE_SECONDS"),
            "MINERU_TOTAL_DEADLINE_SECONDS",
        )?,
        formula: strict_bool(lookup("MINERU_FORMULA_ENABLE"), "MINERU_FORMULA_ENABLE")?,
        table: strict_bool(lookup("MINERU_TABLE_ENABLE"), "MINERU_TABLE_ENABLE")?,
        image_analysis: strict_bool(
            lookup("MINERU_IMAGE_ANALYSIS_ENABLE"),
            "MINERU_IMAGE_ANALYSIS_ENABLE",
        )?,
        vlm_debug: strict_bool(lookup("MINERU_VL_DEBUG_ENABLE"), "MINERU_VL_DEBUG_ENABLE")?,
        http_max_concurrency: positive_usize(
            lookup("MINERU_VLM_HTTP_CONCURRENCY"),
            "MINERU_VLM_HTTP_CONCURRENCY",
        )?,
        http_timeout: positive_seconds(
            lookup("MINERU_VLM_HTTP_TIMEOUT"),
            "MINERU_VLM_HTTP_TIMEOUT",
        )?,
        connect_timeout: positive_seconds(
            lookup("MINERU_VLM_CONNECT_TIMEOUT"),
            "MINERU_VLM_CONNECT_TIMEOUT",
        )?,
        http_max_keepalive_connections: positive_usize(
            lookup("MINERU_VLM_HTTP_MAX_KEEPALIVE_CONNECTIONS"),
            "MINERU_VLM_HTTP_MAX_KEEPALIVE_CONNECTIONS",
        )?,
        http_keepalive_expiry: positive_seconds(
            lookup("MINERU_VLM_HTTP_KEEPALIVE_EXPIRY"),
            "MINERU_VLM_HTTP_KEEPALIVE_EXPIRY",
        )?,
        http_max_retries: nonnegative_usize(
            lookup("MINERU_VLM_HTTP_MAX_RETRIES"),
            "MINERU_VLM_HTTP_MAX_RETRIES",
        )?,
        http_retry_backoff_factor: finite_nonnegative_f32(
            lookup("MINERU_VLM_HTTP_RETRY_BACKOFF_FACTOR"),
            "MINERU_VLM_HTTP_RETRY_BACKOFF_FACTOR",
        )?,
        max_remote_image_bytes: positive_usize(
            lookup("MINERU_VLM_MAX_IMAGE_BYTES"),
            "MINERU_VLM_MAX_IMAGE_BYTES",
        )?,
        max_decoded_pixels: positive_u64(
            lookup("MINERU_VLM_MAX_DECODED_PIXELS"),
            "MINERU_VLM_MAX_DECODED_PIXELS",
        )?,
        max_images_per_request: positive_usize(
            lookup("MINERU_VLM_MAX_IMAGES_PER_REQUEST"),
            "MINERU_VLM_MAX_IMAGES_PER_REQUEST",
        )?,
        max_redirects: nonnegative_usize(
            lookup("MINERU_VLM_MAX_REDIRECTS"),
            "MINERU_VLM_MAX_REDIRECTS",
        )?,
        http_max_response_bytes: positive_usize(
            lookup("MINERU_VLM_HTTP_MAX_RESPONSE_BYTES"),
            "MINERU_VLM_HTTP_MAX_RESPONSE_BYTES",
        )?,
    })
}

/// Resolves core policy with precedence compiled default -> frozen environment -> explicit CLI.
///
/// Environment values are parsed strictly (errors on malformed, non-finite, zero-where-invalid,
/// overflow, and platform-unrepresentable values). CLI overrides are applied last.
pub fn resolve_core(
    lookup: impl Fn(&str) -> Option<OsString>,
    cli: &CoreOverrides,
) -> Result<ResolvedCore, String> {
    let environment = parse_core_overrides(&lookup)?;
    let mut route = OfficialPdfOptions::default();
    let mut page_concurrency = DEFAULT_PAGE_CONCURRENCY;
    let mut http =
        VlmHttpConfig::from_env(|name| lookup(name).and_then(|value| value.into_string().ok()));
    apply_core(&environment, &mut route, &mut page_concurrency, &mut http)?;
    apply_core(cli, &mut route, &mut page_concurrency, &mut http)?;
    Ok(ResolvedCore {
        route,
        page_concurrency,
        http,
    })
}

fn apply_core(
    overrides: &CoreOverrides,
    route: &mut OfficialPdfOptions,
    page_concurrency: &mut usize,
    http: &mut VlmHttpConfig,
) -> Result<(), String> {
    if let Some(value) = overrides.processing_window_size {
        route.processing_window_size = value;
    }
    if let Some(value) = overrides.page_concurrency {
        if value > tokio::sync::Semaphore::MAX_PERMITS {
            return Err("page concurrency exceeds the tokio semaphore capacity".into());
        }
        *page_concurrency = value;
    }
    if let Some(value) = overrides.render_workers {
        route.render_workers = value;
    }
    if let Some(value) = overrides.render_timeout {
        route.render_timeout = value;
    }
    if let Some(value) = overrides.max_pdf_bytes {
        route.max_pdf_bytes = value;
    }
    if let Some(value) = overrides.max_pages {
        route.max_pages = value;
    }
    if let Some(value) = overrides.max_page_pixels {
        route.max_page_pixels = value;
    }
    if let Some(value) = overrides.max_rendered_image_bytes {
        route.max_rendered_image_bytes = value;
    }
    if let Some(value) = overrides.max_in_flight_image_bytes {
        route.max_in_flight_image_bytes = value;
    }
    if let Some(value) = overrides.max_raw_output_bytes {
        route.max_raw_output_bytes = value;
    }
    if let Some(value) = overrides.max_layout_blocks_per_page {
        route.max_layout_blocks_per_page = value;
    }
    if let Some(value) = overrides.max_semantic_requests_per_page {
        route.max_semantic_requests_per_page = value;
    }
    if let Some(value) = overrides.batch_size {
        if value == 0 {
            return Err("batch size must be greater than zero".into());
        }
        route.max_requests_per_batch = value;
    }
    if let Some(value) = overrides.max_encoded_request_bytes {
        route.max_encoded_request_bytes = value;
    }
    if let Some(value) = overrides.max_encoded_batch_bytes {
        route.max_encoded_batch_bytes = value;
    }
    if let Some(value) = overrides.max_total_asset_bytes {
        route.max_total_asset_bytes = value;
    }
    if let Some(value) = overrides.max_staged_text_bytes {
        route.max_staged_text_bytes = value;
    }
    if let Some(value) = overrides.total_deadline {
        route.total_deadline = value;
    }
    if let Some(value) = overrides.formula {
        route.formula_enable = value;
    }
    if let Some(value) = overrides.table {
        route.table_enable = value;
    }
    if let Some(value) = overrides.image_analysis {
        route.image_analysis = value;
    }
    if let Some(value) = overrides.vlm_debug {
        http.debug = value;
    }
    if let Some(value) = overrides.http_max_concurrency {
        http.max_concurrency = value;
    }
    if let Some(value) = overrides.http_timeout {
        http.http_timeout = value;
    }
    if let Some(value) = overrides.connect_timeout {
        http.connect_timeout = value;
    }
    if let Some(value) = overrides.http_max_keepalive_connections {
        http.max_keepalive_connections = value;
    }
    if let Some(value) = overrides.http_keepalive_expiry {
        http.keepalive_expiry = value;
    }
    if let Some(value) = overrides.http_max_retries {
        http.max_retries = value;
    }
    if let Some(value) = overrides.http_retry_backoff_factor {
        http.retry_backoff_factor = value;
    }
    if let Some(value) = overrides.max_remote_image_bytes {
        http.max_image_bytes = value;
    }
    if let Some(value) = overrides.max_decoded_pixels {
        http.max_decoded_pixels = value;
    }
    if let Some(value) = overrides.max_images_per_request {
        http.max_images_per_request = value;
    }
    if let Some(value) = overrides.max_redirects {
        http.max_redirects = value;
    }
    if let Some(value) = overrides.http_max_response_bytes {
        http.max_response_bytes = value;
    }
    route
        .validate()
        .map_err(|_| "invalid official PDF options after configuration".to_owned())?;
    validate_http(http)?;
    Ok(())
}

fn validate_http(http: &VlmHttpConfig) -> Result<(), String> {
    if http.max_concurrency == 0 || http.max_concurrency > tokio::sync::Semaphore::MAX_PERMITS {
        return Err(
            "http max concurrency must be greater than zero and at most the tokio semaphore capacity"
                .into(),
        );
    }
    if http.http_timeout.is_zero() {
        return Err("http timeout must be greater than zero".into());
    }
    if http.connect_timeout.is_zero() {
        return Err("connect timeout must be greater than zero".into());
    }
    if http.max_keepalive_connections == 0 {
        return Err("http max keepalive connections must be greater than zero".into());
    }
    if http.keepalive_expiry.is_zero() {
        return Err("http keepalive expiry must be greater than zero".into());
    }
    if http.max_image_bytes == 0 {
        return Err("max remote image bytes must be greater than zero".into());
    }
    if http.max_decoded_pixels == 0 {
        return Err("max decoded pixels must be greater than zero".into());
    }
    if http.max_images_per_request == 0 {
        return Err("max images per request must be greater than zero".into());
    }
    if http.max_response_bytes == 0 {
        return Err("http max response bytes must be greater than zero".into());
    }
    Ok(())
}

/// Strict unsigned decimal lexer: rejects signs, empty input, embedded separators without a
/// preceding digit, and u64 overflow.
pub(crate) fn strict_decimal(value: &OsString, name: &str) -> Result<u64, String> {
    let value = value
        .to_str()
        .ok_or_else(|| format!("{name} must be unsigned decimal"))?;
    let value = value.trim();
    if value.is_empty() || value.starts_with('+') || value.starts_with('-') {
        return Err(format!("{name} must be unsigned decimal"));
    }
    let mut number = 0u64;
    let mut previous_digit = false;
    for byte in value.bytes() {
        match byte {
            b'0'..=b'9' => {
                number = number
                    .checked_mul(10)
                    .and_then(|value| value.checked_add((byte - b'0') as u64))
                    .ok_or_else(|| format!("{name} overflows u64"))?;
                previous_digit = true;
            }
            b'_' if previous_digit => previous_digit = false,
            _ => return Err(format!("{name} must be unsigned decimal")),
        }
    }
    if !previous_digit {
        return Err(format!("{name} must be unsigned decimal"));
    }
    Ok(number)
}

pub(crate) fn positive_usize(value: Option<OsString>, name: &str) -> Result<Option<usize>, String> {
    let Some(value) = value else { return Ok(None) };
    let number = strict_decimal(&value, name)?;
    if number == 0 {
        return Err(format!("{name} must be greater than zero"));
    }
    usize::try_from(number)
        .map(Some)
        .map_err(|_| format!("{name} exceeds platform usize maximum"))
}

/// Strict boolean lexer: case-insensitive `true`/`false` (trimmed). Every other value,
/// including empty, `1`, `yes`, `on`, or non-UTF-8, fails rather than silently becoming false.
pub(crate) fn strict_bool(value: Option<OsString>, name: &str) -> Result<Option<bool>, String> {
    let Some(value) = value else { return Ok(None) };
    let text = value
        .to_str()
        .ok_or_else(|| format!("{name} must be true or false"))?
        .trim();
    match text.to_ascii_lowercase().as_str() {
        "true" => Ok(Some(true)),
        "false" => Ok(Some(false)),
        _ => Err(format!("{name} must be true or false")),
    }
}

pub(crate) fn nonnegative_usize(
    value: Option<OsString>,
    name: &str,
) -> Result<Option<usize>, String> {
    let Some(value) = value else { return Ok(None) };
    let number = strict_decimal(&value, name)?;
    usize::try_from(number)
        .map(Some)
        .map_err(|_| format!("{name} exceeds platform usize maximum"))
}

pub(crate) fn positive_u64(value: Option<OsString>, name: &str) -> Result<Option<u64>, String> {
    let Some(value) = value else { return Ok(None) };
    let number = strict_decimal(&value, name)?;
    if number == 0 {
        return Err(format!("{name} must be greater than zero"));
    }
    Ok(Some(number))
}

pub(crate) fn positive_u32(value: Option<OsString>, name: &str) -> Result<Option<u32>, String> {
    let Some(value) = value else { return Ok(None) };
    let number = strict_decimal(&value, name)?;
    if number == 0 {
        return Err(format!("{name} must be greater than zero"));
    }
    u32::try_from(number)
        .map(Some)
        .map_err(|_| format!("{name} exceeds the platform process-limit maximum"))
}

pub(crate) fn positive_seconds(
    value: Option<OsString>,
    name: &str,
) -> Result<Option<Duration>, String> {
    let Some(value) = value else { return Ok(None) };
    let number = strict_decimal(&value, name)?;
    if number == 0 {
        return Err(format!("{name} must be greater than zero"));
    }
    Ok(Some(Duration::from_secs(number)))
}

pub(crate) fn finite_nonnegative_f32(
    value: Option<OsString>,
    name: &str,
) -> Result<Option<f32>, String> {
    let Some(value) = value else { return Ok(None) };
    let text = value
        .to_str()
        .ok_or_else(|| format!("{name} must be a finite non-negative decimal"))?;
    let number: f32 = text
        .trim()
        .parse()
        .map_err(|_| format!("{name} must be a finite non-negative decimal"))?;
    if !number.is_finite() || number < 0.0 {
        return Err(format!("{name} must be a finite non-negative decimal"));
    }
    Ok(Some(number))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lookup_map<'a>(values: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<OsString> + 'a {
        move |name| {
            values
                .iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| OsString::from(*value))
        }
    }

    #[test]
    fn parse_core_overrides_is_strict_about_boundaries() {
        let lookup = lookup_map(&[
            ("MINERU_PROCESSING_WINDOW_SIZE", "1_024"),
            ("MINERU_OFFICIAL_PAGE_CONCURRENCY", "9"),
            ("MINERU_PDF_RENDER_THREADS", "16"),
            ("MINERU_PDF_RENDER_TIMEOUT", "600"),
            ("MINERU_MAX_PDF_BYTES", "18446744073709551615"),
            ("MINERU_MAX_PAGES", "999999"),
            ("MINERU_MAX_PAGE_PIXELS", "42"),
            ("MINERU_MAX_RENDERED_IMAGE_BYTES", "64"),
            ("MINERU_MAX_IN_FLIGHT_IMAGE_BYTES", "128"),
            ("MINERU_MAX_RAW_OUTPUT_BYTES", "256"),
            ("MINERU_MAX_LAYOUT_BLOCKS_PER_PAGE", "512"),
            ("MINERU_MAX_SEMANTIC_REQUESTS_PER_PAGE", "1024"),
            ("MINERU_BATCH_SIZE", "32"),
            ("MINERU_MAX_ENCODED_REQUEST_BYTES", "2048"),
            ("MINERU_MAX_ENCODED_BATCH_BYTES", "4096"),
            ("MINERU_MAX_TOTAL_ASSET_BYTES", "8192"),
            ("MINERU_MAX_STAGED_TEXT_BYTES", "16384"),
            ("MINERU_TOTAL_DEADLINE_SECONDS", "86400"),
            ("MINERU_FORMULA_ENABLE", "false"),
            ("MINERU_TABLE_ENABLE", "True"),
            ("MINERU_IMAGE_ANALYSIS_ENABLE", "false"),
            ("MINERU_VL_DEBUG_ENABLE", "TRUE"),
            ("MINERU_VLM_HTTP_CONCURRENCY", "16"),
            ("MINERU_VLM_HTTP_TIMEOUT", "30"),
            ("MINERU_VLM_CONNECT_TIMEOUT", "5"),
            ("MINERU_VLM_HTTP_MAX_KEEPALIVE_CONNECTIONS", "8"),
            ("MINERU_VLM_HTTP_KEEPALIVE_EXPIRY", "15"),
            ("MINERU_VLM_HTTP_MAX_RETRIES", "0"),
            ("MINERU_VLM_HTTP_RETRY_BACKOFF_FACTOR", "0.25"),
            ("MINERU_VLM_MAX_IMAGE_BYTES", "1048576"),
            ("MINERU_VLM_MAX_DECODED_PIXELS", "100000000"),
            ("MINERU_VLM_MAX_IMAGES_PER_REQUEST", "16"),
            ("MINERU_VLM_MAX_REDIRECTS", "0"),
            ("MINERU_VLM_HTTP_MAX_RESPONSE_BYTES", "10485760"),
        ]);
        let overrides = parse_core_overrides(&lookup).unwrap();
        assert_eq!(overrides.processing_window_size, Some(1024));
        assert_eq!(overrides.page_concurrency, Some(9));
        assert_eq!(overrides.render_workers, Some(16));
        assert_eq!(overrides.render_timeout, Some(Duration::from_secs(600)));
        assert_eq!(overrides.max_pdf_bytes, Some(usize::MAX));
        assert_eq!(overrides.max_pages, Some(999_999));
        assert_eq!(overrides.max_page_pixels, Some(42));
        assert_eq!(overrides.max_rendered_image_bytes, Some(64));
        assert_eq!(overrides.max_in_flight_image_bytes, Some(128));
        assert_eq!(overrides.max_raw_output_bytes, Some(256));
        assert_eq!(overrides.max_layout_blocks_per_page, Some(512));
        assert_eq!(overrides.max_semantic_requests_per_page, Some(1024));
        assert_eq!(overrides.batch_size, Some(32));
        assert_eq!(overrides.max_encoded_request_bytes, Some(2048));
        assert_eq!(overrides.max_encoded_batch_bytes, Some(4096));
        assert_eq!(overrides.max_total_asset_bytes, Some(8192));
        assert_eq!(overrides.max_staged_text_bytes, Some(16384));
        assert_eq!(overrides.total_deadline, Some(Duration::from_secs(86400)));
        assert_eq!(overrides.formula, Some(false));
        assert_eq!(overrides.table, Some(true));
        assert_eq!(overrides.image_analysis, Some(false));
        assert_eq!(overrides.vlm_debug, Some(true));
        assert_eq!(overrides.http_max_concurrency, Some(16));
        assert_eq!(overrides.http_timeout, Some(Duration::from_secs(30)));
        assert_eq!(overrides.connect_timeout, Some(Duration::from_secs(5)));
        assert_eq!(overrides.http_max_keepalive_connections, Some(8));
        assert_eq!(
            overrides.http_keepalive_expiry,
            Some(Duration::from_secs(15))
        );
        assert_eq!(overrides.http_max_retries, Some(0));
        assert_eq!(overrides.http_retry_backoff_factor, Some(0.25));
        assert_eq!(overrides.max_remote_image_bytes, Some(1_048_576));
        assert_eq!(overrides.max_decoded_pixels, Some(100_000_000));
        assert_eq!(overrides.max_images_per_request, Some(16));
        assert_eq!(overrides.max_redirects, Some(0));
        assert_eq!(overrides.http_max_response_bytes, Some(10_485_760));
    }

    #[test]
    fn parse_core_overrides_rejects_malformed_and_non_finite_values() {
        for (name, value) in [
            ("MINERU_PROCESSING_WINDOW_SIZE", "0"),
            ("MINERU_OFFICIAL_PAGE_CONCURRENCY", "bad"),
            ("MINERU_PDF_RENDER_THREADS", "-1"),
            ("MINERU_PDF_RENDER_TIMEOUT", "1e3"),
            ("MINERU_MAX_PDF_BYTES", "18446744073709551616"),
            ("MINERU_MAX_PAGES", "1__0"),
            ("MINERU_MAX_PAGE_PIXELS", "0"),
            ("MINERU_MAX_RENDERED_IMAGE_BYTES", ""),
            ("MINERU_MAX_IN_FLIGHT_IMAGE_BYTES", "  "),
            ("MINERU_MAX_RAW_OUTPUT_BYTES", "+5"),
            ("MINERU_MAX_LAYOUT_BLOCKS_PER_PAGE", "1.5"),
            ("MINERU_MAX_SEMANTIC_REQUESTS_PER_PAGE", "text"),
            ("MINERU_BATCH_SIZE", "0"),
            ("MINERU_MAX_ENCODED_REQUEST_BYTES", "0"),
            ("MINERU_MAX_ENCODED_BATCH_BYTES", "0"),
            ("MINERU_MAX_TOTAL_ASSET_BYTES", "0"),
            ("MINERU_MAX_STAGED_TEXT_BYTES", "0"),
            ("MINERU_TOTAL_DEADLINE_SECONDS", "0"),
            ("MINERU_FORMULA_ENABLE", "1"),
            ("MINERU_TABLE_ENABLE", "yes"),
            ("MINERU_IMAGE_ANALYSIS_ENABLE", ""),
            ("MINERU_VL_DEBUG_ENABLE", "on"),
            ("MINERU_VLM_HTTP_CONCURRENCY", "0"),
            ("MINERU_VLM_HTTP_TIMEOUT", "0"),
            ("MINERU_VLM_CONNECT_TIMEOUT", "0"),
            ("MINERU_VLM_HTTP_MAX_KEEPALIVE_CONNECTIONS", "0"),
            ("MINERU_VLM_HTTP_KEEPALIVE_EXPIRY", "0"),
            ("MINERU_VLM_HTTP_RETRY_BACKOFF_FACTOR", "NaN"),
            ("MINERU_VLM_HTTP_RETRY_BACKOFF_FACTOR", "inf"),
            ("MINERU_VLM_HTTP_RETRY_BACKOFF_FACTOR", "-0.5"),
            ("MINERU_VLM_MAX_IMAGE_BYTES", "0"),
            ("MINERU_VLM_MAX_DECODED_PIXELS", "0"),
            ("MINERU_VLM_MAX_IMAGES_PER_REQUEST", "0"),
            ("MINERU_VLM_HTTP_MAX_RESPONSE_BYTES", "0"),
        ] {
            let entry = [(name, value)];
            let lookup = lookup_map(&entry);
            assert!(
                parse_core_overrides(&lookup).is_err(),
                "{name}={value} must be rejected"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn parse_core_overrides_rejects_non_utf8() {
        use std::os::unix::ffi::OsStringExt;
        for name in [
            "MINERU_MAX_PAGES",
            "MINERU_FORMULA_ENABLE",
            "MINERU_VL_DEBUG_ENABLE",
        ] {
            let lookup =
                |candidate: &str| (candidate == name).then(|| OsString::from_vec(vec![0xff]));
            assert!(parse_core_overrides(&lookup).is_err(), "{name}");
        }
    }

    #[test]
    fn resolve_core_prefers_cli_over_environment_over_defaults() {
        let env = lookup_map(&[
            ("MINERU_PROCESSING_WINDOW_SIZE", "8"),
            ("MINERU_OFFICIAL_PAGE_CONCURRENCY", "16"),
            ("MINERU_PDF_RENDER_THREADS", "5"),
            ("MINERU_PDF_RENDER_TIMEOUT", "120"),
            ("MINERU_BATCH_SIZE", "4"),
            ("MINERU_VLM_HTTP_CONCURRENCY", "32"),
            ("MINERU_VLM_HTTP_TIMEOUT", "90"),
            ("MINERU_VLM_HTTP_RETRY_BACKOFF_FACTOR", "0.75"),
            ("MINERU_MAX_LAYOUT_BLOCKS_PER_PAGE", "300"),
        ]);
        let cli = CoreOverrides {
            processing_window_size: Some(16),
            page_concurrency: Some(24),
            render_workers: Some(6),
            render_timeout: Some(Duration::from_secs(240)),
            batch_size: Some(8),
            http_max_concurrency: Some(64),
            http_timeout: Some(Duration::from_secs(180)),
            http_retry_backoff_factor: Some(0.125),
            ..Default::default()
        };
        let resolved = resolve_core(&env, &cli).unwrap();
        assert_eq!(resolved.route.processing_window_size, 16);
        assert_eq!(resolved.page_concurrency, 24);
        assert_eq!(resolved.route.render_workers, 6);
        assert_eq!(resolved.route.render_timeout, Duration::from_secs(240));
        assert_eq!(resolved.route.max_requests_per_batch, 8);
        assert_eq!(resolved.http.max_concurrency, 64);
        assert_eq!(resolved.http.http_timeout, Duration::from_secs(180));
        assert_eq!(resolved.http.retry_backoff_factor, 0.125);
        // Environment still feeds the knobs the CLI did not configure.
        assert_eq!(resolved.route.max_layout_blocks_per_page, 300);
        assert_eq!(resolved.http.max_keepalive_connections, 20);
        assert_eq!(resolved.route.max_pdf_bytes, 512 * 1024 * 1024);
    }

    #[test]
    fn resolve_core_rejects_malformed_environment_even_without_cli() {
        let env = lookup_map(&[("MINERU_PDF_RENDER_TIMEOUT", "1e3")]);
        let error = resolve_core(&env, &CoreOverrides::default()).unwrap_err();
        assert!(error.contains("MINERU_PDF_RENDER_TIMEOUT"), "{error}");
    }

    /// Renders one resolved knob as its source string for table comparisons.
    fn render_knob(resolved: &ResolvedCore, name: &str) -> String {
        let route = &resolved.route;
        match name {
            "MINERU_PROCESSING_WINDOW_SIZE" => route.processing_window_size.to_string(),
            "MINERU_OFFICIAL_PAGE_CONCURRENCY" => resolved.page_concurrency.to_string(),
            "MINERU_PDF_RENDER_THREADS" => route.render_workers.to_string(),
            "MINERU_PDF_RENDER_TIMEOUT" => route.render_timeout.as_secs().to_string(),
            "MINERU_MAX_PDF_BYTES" => route.max_pdf_bytes.to_string(),
            "MINERU_MAX_PAGES" => route.max_pages.to_string(),
            "MINERU_MAX_PAGE_PIXELS" => route.max_page_pixels.to_string(),
            "MINERU_MAX_RENDERED_IMAGE_BYTES" => route.max_rendered_image_bytes.to_string(),
            "MINERU_MAX_IN_FLIGHT_IMAGE_BYTES" => route.max_in_flight_image_bytes.to_string(),
            "MINERU_MAX_RAW_OUTPUT_BYTES" => route.max_raw_output_bytes.to_string(),
            "MINERU_MAX_LAYOUT_BLOCKS_PER_PAGE" => route.max_layout_blocks_per_page.to_string(),
            "MINERU_MAX_SEMANTIC_REQUESTS_PER_PAGE" => {
                route.max_semantic_requests_per_page.to_string()
            }
            "MINERU_BATCH_SIZE" => route.max_requests_per_batch.to_string(),
            "MINERU_MAX_ENCODED_REQUEST_BYTES" => route.max_encoded_request_bytes.to_string(),
            "MINERU_MAX_ENCODED_BATCH_BYTES" => route.max_encoded_batch_bytes.to_string(),
            "MINERU_MAX_TOTAL_ASSET_BYTES" => route.max_total_asset_bytes.to_string(),
            "MINERU_MAX_STAGED_TEXT_BYTES" => route.max_staged_text_bytes.to_string(),
            "MINERU_TOTAL_DEADLINE_SECONDS" => route.total_deadline.as_secs().to_string(),
            "MINERU_VLM_HTTP_CONCURRENCY" => resolved.http.max_concurrency.to_string(),
            "MINERU_VLM_HTTP_TIMEOUT" => resolved.http.http_timeout.as_secs().to_string(),
            "MINERU_VLM_CONNECT_TIMEOUT" => resolved.http.connect_timeout.as_secs().to_string(),
            "MINERU_VLM_HTTP_MAX_KEEPALIVE_CONNECTIONS" => {
                resolved.http.max_keepalive_connections.to_string()
            }
            "MINERU_VLM_HTTP_KEEPALIVE_EXPIRY" => {
                resolved.http.keepalive_expiry.as_secs().to_string()
            }
            "MINERU_VLM_HTTP_MAX_RETRIES" => resolved.http.max_retries.to_string(),
            "MINERU_VLM_HTTP_RETRY_BACKOFF_FACTOR" => {
                resolved.http.retry_backoff_factor.to_string()
            }
            "MINERU_VLM_MAX_IMAGE_BYTES" => resolved.http.max_image_bytes.to_string(),
            "MINERU_VLM_MAX_DECODED_PIXELS" => resolved.http.max_decoded_pixels.to_string(),
            "MINERU_VLM_MAX_IMAGES_PER_REQUEST" => resolved.http.max_images_per_request.to_string(),
            "MINERU_VLM_MAX_REDIRECTS" => resolved.http.max_redirects.to_string(),
            "MINERU_VLM_HTTP_MAX_RESPONSE_BYTES" => resolved.http.max_response_bytes.to_string(),
            _ => unreachable!("unexpected knob {name}"),
        }
    }

    /// One table-driven test proving precedence `compiled default -> frozen environment ->
    /// explicit CLI` and strict malformed-env failure for every newly introduced knob.
    #[test]
    fn every_core_knob_obeys_default_env_cli_precedence_and_strictness() {
        const TABLE: &[(&str, &str, &str, &str)] = &[
            ("MINERU_PROCESSING_WINDOW_SIZE", "64", "8", "16"),
            ("MINERU_OFFICIAL_PAGE_CONCURRENCY", "4", "9", "24"),
            ("MINERU_PDF_RENDER_THREADS", "3", "5", "6"),
            ("MINERU_PDF_RENDER_TIMEOUT", "300", "120", "240"),
            ("MINERU_MAX_PDF_BYTES", "536870912", "700", "900"),
            ("MINERU_MAX_PAGES", "10000", "999", "1001"),
            ("MINERU_MAX_PAGE_PIXELS", "100000000", "42", "43"),
            ("MINERU_MAX_RENDERED_IMAGE_BYTES", "67108864", "44", "45"),
            ("MINERU_MAX_IN_FLIGHT_IMAGE_BYTES", "134217728", "46", "47"),
            ("MINERU_MAX_RAW_OUTPUT_BYTES", "134217728", "48", "49"),
            ("MINERU_MAX_LAYOUT_BLOCKS_PER_PAGE", "256", "300", "512"),
            ("MINERU_MAX_SEMANTIC_REQUESTS_PER_PAGE", "128", "129", "130"),
            ("MINERU_BATCH_SIZE", "32", "4", "8"),
            ("MINERU_MAX_ENCODED_REQUEST_BYTES", "16777216", "131", "132"),
            ("MINERU_MAX_ENCODED_BATCH_BYTES", "67108864", "133", "134"),
            ("MINERU_MAX_TOTAL_ASSET_BYTES", "1073741824", "135", "136"),
            ("MINERU_MAX_STAGED_TEXT_BYTES", "268435456", "137", "138"),
            ("MINERU_TOTAL_DEADLINE_SECONDS", "86400", "3600", "7200"),
            ("MINERU_VLM_HTTP_CONCURRENCY", "100", "32", "64"),
            ("MINERU_VLM_HTTP_TIMEOUT", "600", "90", "180"),
            ("MINERU_VLM_CONNECT_TIMEOUT", "10", "11", "12"),
            (
                "MINERU_VLM_HTTP_MAX_KEEPALIVE_CONNECTIONS",
                "20",
                "21",
                "22",
            ),
            ("MINERU_VLM_HTTP_KEEPALIVE_EXPIRY", "5", "15", "25"),
            ("MINERU_VLM_HTTP_MAX_RETRIES", "3", "0", "1"),
            (
                "MINERU_VLM_HTTP_RETRY_BACKOFF_FACTOR",
                "0.5",
                "0.75",
                "0.125",
            ),
            ("MINERU_VLM_MAX_IMAGE_BYTES", "33554432", "139", "140"),
            ("MINERU_VLM_MAX_DECODED_PIXELS", "100000000", "141", "142"),
            ("MINERU_VLM_MAX_IMAGES_PER_REQUEST", "64", "16", "32"),
            ("MINERU_VLM_MAX_REDIRECTS", "3", "0", "2"),
            (
                "MINERU_VLM_HTTP_MAX_RESPONSE_BYTES",
                "10485760",
                "143",
                "144",
            ),
        ];
        for (name, default, env_value, cli_value) in TABLE {
            let env_entry = [(*name, *env_value)];
            let cli_entry = [(*name, *cli_value)];
            let bad_entry = [(*name, "bad")];
            let env_only = lookup_map(&env_entry);
            let cli_only = lookup_map(&cli_entry);
            let cli = parse_core_overrides(&cli_only).unwrap();
            // Compiled default wins when nothing is configured.
            assert_eq!(
                render_knob(
                    &resolve_core(|_| None, &CoreOverrides::default()).unwrap(),
                    name
                ),
                *default,
                "{name} default"
            );
            // Frozen environment wins over the compiled default.
            assert_eq!(
                render_knob(
                    &resolve_core(&env_only, &CoreOverrides::default()).unwrap(),
                    name
                ),
                *env_value,
                "{name} environment"
            );
            // Explicit CLI wins over the frozen environment.
            assert_eq!(
                render_knob(&resolve_core(&env_only, &cli).unwrap(), name),
                *cli_value,
                "{name} CLI over environment"
            );
            // Malformed environment values fail before any work.
            let malformed = lookup_map(&bad_entry);
            let error = resolve_core(&malformed, &CoreOverrides::default()).unwrap_err();
            assert!(error.contains(name), "{name}: {error}");
        }
    }

    #[test]
    fn resolve_core_rejects_zero_batch_and_zero_concurrency() {
        for overrides in [
            CoreOverrides {
                batch_size: Some(0),
                ..Default::default()
            },
            CoreOverrides {
                http_max_concurrency: Some(0),
                ..Default::default()
            },
        ] {
            assert!(resolve_core(|_| None, &overrides).is_err());
        }
    }

    #[test]
    fn resolve_core_accepts_values_above_old_ceilings_without_allocation() {
        let cli = CoreOverrides {
            render_workers: Some(1024),
            render_timeout: Some(Duration::from_secs(u64::MAX)),
            processing_window_size: Some(usize::MAX),
            max_pdf_bytes: Some(usize::MAX),
            max_pages: Some(usize::MAX),
            max_page_pixels: Some(u64::MAX),
            max_rendered_image_bytes: Some(usize::MAX),
            max_in_flight_image_bytes: Some(usize::MAX),
            max_raw_output_bytes: Some(usize::MAX),
            max_layout_blocks_per_page: Some(usize::MAX),
            max_semantic_requests_per_page: Some(usize::MAX),
            batch_size: Some(usize::MAX),
            max_encoded_request_bytes: Some(usize::MAX),
            max_encoded_batch_bytes: Some(usize::MAX),
            max_total_asset_bytes: Some(usize::MAX),
            max_staged_text_bytes: Some(usize::MAX),
            total_deadline: Some(Duration::from_secs(u64::MAX)),
            http_max_concurrency: Some(tokio::sync::Semaphore::MAX_PERMITS),
            http_timeout: Some(Duration::from_secs(u64::MAX)),
            connect_timeout: Some(Duration::from_secs(u64::MAX)),
            http_max_keepalive_connections: Some(usize::MAX),
            http_keepalive_expiry: Some(Duration::from_secs(u64::MAX)),
            http_max_retries: Some(usize::MAX),
            http_retry_backoff_factor: Some(f32::MAX),
            max_remote_image_bytes: Some(usize::MAX),
            max_decoded_pixels: Some(u64::MAX),
            max_images_per_request: Some(usize::MAX),
            max_redirects: Some(usize::MAX),
            http_max_response_bytes: Some(usize::MAX),
            ..Default::default()
        };
        let resolved = resolve_core(|_| None, &cli).unwrap();
        assert_eq!(resolved.route.processing_window_size, usize::MAX);
        assert_eq!(resolved.route.render_workers, 1024);
        assert_eq!(
            resolved.http.max_concurrency,
            tokio::sync::Semaphore::MAX_PERMITS
        );
        // Anything above the tokio semaphore capacity is a capacity error, not a silent clamp.
        assert!(
            resolve_core(
                |_| None,
                &CoreOverrides {
                    http_max_concurrency: Some(tokio::sync::Semaphore::MAX_PERMITS + 1),
                    ..Default::default()
                }
            )
            .is_err()
        );
        // Explicit page concurrency above the semaphore capacity likewise fails at resolution.
        assert!(
            resolve_core(
                |_| None,
                &CoreOverrides {
                    page_concurrency: Some(usize::MAX),
                    ..Default::default()
                }
            )
            .is_err()
        );
        assert!(
            resolve_core(
                |_| None,
                &CoreOverrides {
                    page_concurrency: Some(tokio::sync::Semaphore::MAX_PERMITS),
                    ..Default::default()
                }
            )
            .is_ok()
        );
    }

    #[test]
    fn boolean_core_knobs_obey_default_env_cli_precedence_and_strictness() {
        for (env_name, cli_value) in [
            ("MINERU_FORMULA_ENABLE", true),
            ("MINERU_TABLE_ENABLE", false),
            ("MINERU_IMAGE_ANALYSIS_ENABLE", true),
            ("MINERU_VL_DEBUG_ENABLE", true),
        ] {
            let env_entry = [(env_name, if cli_value { "true" } else { "false" })];
            let bad_entry = [(env_name, " yes")];
            let env_only = lookup_map(&env_entry);
            let cli = parse_core_overrides(&lookup_map(&[(
                env_name,
                if cli_value { "false" } else { "true" },
            )]))
            .unwrap();
            // Compiled default applies when nothing is configured.
            let default = resolve_core(|_| None, &CoreOverrides::default()).unwrap();
            // Environment beats the compiled default; CLI beats the environment.
            let env_resolved = resolve_core(&env_only, &CoreOverrides::default()).unwrap();
            let cli_resolved = resolve_core(&env_only, &cli).unwrap();
            let route = |resolved: &ResolvedCore| resolved.route.clone();
            let boolean_of = |resolved: &ResolvedCore| match env_name {
                "MINERU_FORMULA_ENABLE" => route(resolved).formula_enable,
                "MINERU_TABLE_ENABLE" => route(resolved).table_enable,
                "MINERU_IMAGE_ANALYSIS_ENABLE" => route(resolved).image_analysis,
                _ => resolved.http.debug,
            };
            match env_name {
                "MINERU_FORMULA_ENABLE" => assert!(route(&default).formula_enable),
                "MINERU_TABLE_ENABLE" => assert!(route(&default).table_enable),
                "MINERU_IMAGE_ANALYSIS_ENABLE" => assert!(route(&default).image_analysis),
                _ => assert!(!default.http.debug),
            }
            assert_eq!(
                boolean_of(&env_resolved),
                cli_value,
                "{env_name} environment"
            );
            assert_eq!(boolean_of(&cli_resolved), !cli_value, "{env_name} CLI");
            // Malformed environment values fail before any work instead of silently being false.
            let malformed = lookup_map(&bad_entry);
            let error = resolve_core(&malformed, &CoreOverrides::default()).unwrap_err();
            assert!(error.contains(env_name), "{env_name}: {error}");
        }
    }
}
