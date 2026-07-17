use std::io;
use std::sync::OnceLock;

use regex::Regex;
use thiserror::Error;
pub type Result<T, E = Error> = std::result::Result<T, E>;
pub type VlmResult<T> = std::result::Result<T, VlmError>;
#[derive(Debug, Error)]
pub enum VlmError {
    #[error("unsupported: {0}")]
    Unsupported(&'static str),
    #[error("invalid VLM configuration: {0}")]
    InvalidConfig(String),
    #[error("invalid VLM header: {0}")]
    InvalidHeader(String),
    #[error("invalid VLM image input: {0}")]
    InvalidImageInput(String),
    #[error("invalid input: {0}")]
    InvalidInput(String),
    #[error("PDF error: {0}")]
    Pdf(String),
    #[error("redirect rejected: {0}")]
    Redirect(String),
    #[error("transport error during {operation}: {message}")]
    Transport {
        operation: &'static str,
        message: String,
    },
    #[error("HTTP {status} during {operation}: {body}")]
    Http {
        operation: &'static str,
        status: u16,
        body: String,
    },
    #[error("timeout during {operation}")]
    Timeout { operation: &'static str },
    #[error("protocol error during {operation}: {message}")]
    Protocol {
        operation: &'static str,
        message: String,
    },
    #[error("limit exceeded for {resource}: {actual} > {limit}")]
    LimitExceeded {
        resource: &'static str,
        limit: u64,
        actual: u64,
    },
    #[error("I/O error during {operation}: {message}")]
    Io {
        operation: &'static str,
        message: String,
    },
}

/// Produces a bounded, safe-to-log representation of VLM transport data.
#[allow(dead_code)] // Used by VLM transport implementations as they are added.
pub(crate) fn sanitize_vlm_error_bytes(raw: &[u8], cap: usize) -> String {
    static DATA_URL: OnceLock<Regex> = OnceLock::new();
    static QUOTED_AUTHORIZATION: OnceLock<Regex> = OnceLock::new();
    static AUTHORIZATION: OnceLock<Regex> = OnceLock::new();
    static BEARER: OnceLock<Regex> = OnceLock::new();
    static HTTP_URL: OnceLock<Regex> = OnceLock::new();
    static SECRET_VALUE: OnceLock<Regex> = OnceLock::new();

    let truncated = raw.len() > cap;
    let mut text = String::from_utf8_lossy(&raw[..raw.len().min(cap)]).into_owned();
    text = DATA_URL
        .get_or_init(|| Regex::new(r"(?i)\bdata:[^\s,]+,[^\s]*").unwrap())
        .replace_all(&text, "[REDACTED_DATA_URL]")
        .into_owned();
    text = QUOTED_AUTHORIZATION
        .get_or_init(|| {
            Regex::new(
                r#"(?i)([\"']authorization[\"']\s*[:=]\s*)(?:\"(?:\\.|[^\"\\])*\"|'(?:\\.|[^'\\])*')"#,
            )
            .unwrap()
        })
        .replace_all(&text, "$1[REDACTED]")
        .into_owned();
    text = AUTHORIZATION
        .get_or_init(|| Regex::new(r"(?i)\bauthorization\s*[:=]\s*(?:bearer\s+)?[^\s,;]+").unwrap())
        .replace_all(&text, "Authorization: [REDACTED]")
        .into_owned();
    text = BEARER
        .get_or_init(|| Regex::new(r"(?i)\bbearer\s+[^\s,;]+").unwrap())
        .replace_all(&text, "Bearer [REDACTED]")
        .into_owned();
    text = SECRET_VALUE
        .get_or_init(|| Regex::new(r#"(?i)(?:\"|'|\b)(authorization|cookie|x-api-key|api[-_]?key|access_token|refresh_token|client_secret|token|secret|password|credential)(?:\"|')?(\s*[=:]\s*)(?:\"[^\"]*\"|'[^']*'|[^\s,;}&]+)"#).unwrap())
        .replace_all(&text, "$1$2[REDACTED]")
        .into_owned();
    text = HTTP_URL
        .get_or_init(|| Regex::new(r#"(?i)\bhttps?://[^\s"'<>]+"#).unwrap())
        .replace_all(&text, "[REDACTED_URL]")
        .into_owned();
    if truncated {
        text.push_str(" [truncated]");
    }
    text
}
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ErrorContext {
    pub operation: Option<&'static str>,
    pub status: Option<u16>,
    pub limit: Option<&'static str>,
    pub page: Option<usize>,
    pub block: Option<usize>,
}
#[derive(Debug, Error)]
pub enum Error {
    #[error("invalid configuration: {0}")]
    InvalidConfig(String),
    #[error("invalid input: {0}")]
    InvalidInput(String),
    #[error("{context:?}: transport error: {source}")]
    Transport {
        context: ErrorContext,
        #[source]
        source: reqwest::Error,
    },
    #[error("HTTP {status}: {body}")]
    Http { status: u16, body: String },
    #[error("timeout during {operation}")]
    Timeout { operation: &'static str },
    #[error("PDF error: {0}")]
    Pdf(String),
    #[error("{context:?}: protocol error: {message}")]
    Protocol {
        context: ErrorContext,
        message: String,
    },
    #[error("limit exceeded for {resource}: {actual} > {limit}")]
    LimitExceeded {
        resource: &'static str,
        limit: u64,
        actual: u64,
    },
    #[error("image error: {0}")]
    Image(String),
    #[error("worker join error: {0}")]
    WorkerJoin(String),
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),
    #[error("serialization error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("page {page}: {source}")]
    Page {
        page: usize,
        #[source]
        source: Box<Error>,
    },
    #[error("page {page}, block {block}: {source}")]
    Block {
        page: usize,
        block: usize,
        #[source]
        source: Box<Error>,
    },
}

#[cfg(test)]
mod tests {
    use super::{VlmError, sanitize_vlm_error_bytes};

    #[test]
    fn sanitizer_redacts_sensitive_markers() {
        let raw = b"https://example.test/path?key=https-secret http://example.test/path?key=http-secret Authorization: Bearer auth-secret bearer bearer-secret data:image/png;base64,image-secret";
        let sanitized = sanitize_vlm_error_bytes(raw, raw.len());

        for secret in [
            "https-secret",
            "http-secret",
            "auth-secret",
            "bearer-secret",
            "data:image/png;base64,image-secret",
        ] {
            assert!(!sanitized.contains(secret), "leaked {secret}: {sanitized}");
        }
        assert!(sanitized.contains("[REDACTED_URL]"));
        assert!(sanitized.contains("[REDACTED]"));
        assert!(sanitized.contains("[REDACTED_DATA_URL]"));
    }

    #[test]
    fn sanitizer_redacts_quoted_json_secrets() {
        let sanitized = sanitize_vlm_error_bytes(
            br#"{"token":"json-secret","password": "also-secret","api-key":"k1","access_token":"k2","refresh_token":k3,"client_secret":'k4'}"#,
            1024,
        );
        for secret in ["json-secret", "also-secret", "k1", "k2", "k3", "k4"] {
            assert!(!sanitized.contains(secret), "leaked {secret}: {sanitized}");
        }
    }

    #[test]
    fn sanitized_vlm_error_is_safe_in_display_and_debug() {
        let secret = "display-debug-secret";
        let body = sanitize_vlm_error_bytes(
            format!("https://example.test/{secret} Bearer {secret}").as_bytes(),
            1024,
        );
        let error = VlmError::Http {
            operation: "request",
            status: 401,
            body,
        };

        assert!(!error.to_string().contains(secret));
        assert!(!format!("{error:?}").contains(secret));
    }

    #[test]
    fn sanitizer_redacts_quoted_json_authorization_values() {
        for raw in [
            br#"{"authorization":"Bearer json-auth-secret"}"#.as_slice(),
            br#"{"AUTHORIZATION": "bearer lowercase-secret"}"#.as_slice(),
            br#"{'authorization':'Bearer colon-secret'}"#.as_slice(),
            br#"{\"authorization\":\"Bearer prefix\\\"tail-secret\"}"#.as_slice(),
        ] {
            let body = sanitize_vlm_error_bytes(raw, 1024);
            let error = VlmError::Http {
                operation: "request",
                status: 401,
                body,
            };
            for secret in [
                "json-auth-secret",
                "lowercase-secret",
                "colon-secret",
                "tail-secret",
            ] {
                assert!(!error.to_string().contains(secret));
                assert!(!format!("{error:?}").contains(secret));
            }
        }
    }

    #[test]
    fn sanitizer_caps_bytes_before_decoding_without_tail_leak() {
        let raw = b"safe-prefix-\xFFraw-tail-secret";
        let sanitized = sanitize_vlm_error_bytes(raw, 12);

        assert_eq!(sanitized, "safe-prefix- [truncated]");
        assert!(!sanitized.contains("raw-tail-secret"));
    }
}
