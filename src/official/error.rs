use serde::{Deserialize, Serialize};
use std::{error::Error, fmt};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum OfficialErrorKind {
    InvalidOptions,
    InvalidInput,
    UnsupportedOperation,
    LimitExceeded,
    RuntimeUnavailable,
    Upstream,
    Deadline,
    Cancelled,
    Transport,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct OfficialError {
    pub kind: OfficialErrorKind,
    pub message: String,
}

impl OfficialError {
    pub(crate) fn new(kind: OfficialErrorKind, message: impl AsRef<str>) -> Self {
        Self {
            kind,
            message: redact_secret(message.as_ref(), 4096),
        }
    }
}

impl fmt::Display for OfficialError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}
impl Error for OfficialError {}

pub(crate) fn classify(message: String) -> OfficialError {
    let lower = message.to_ascii_lowercase();
    let kind = if lower.contains("deadline") || lower.contains("timed out") {
        OfficialErrorKind::Deadline
    } else if lower.contains("cancel") {
        OfficialErrorKind::Cancelled
    } else if lower.contains("exceed") || lower.contains("limit") || lower.contains("cap") {
        OfficialErrorKind::LimitExceeded
    } else if lower.contains("unavailable") || lower.contains("spawn") {
        OfficialErrorKind::RuntimeUnavailable
    } else if lower.contains("unsupported operation") {
        OfficialErrorKind::UnsupportedOperation
    } else if lower.contains("invalid")
        || lower.contains("must be")
        || lower.contains("unsupported input")
        || lower.contains("file type")
        || lower.contains("input is empty")
        || lower.contains("missing materialized")
    {
        OfficialErrorKind::InvalidInput
    } else if lower.contains("protocol") || lower.contains("pipe") || lower.contains("stdout") {
        OfficialErrorKind::Transport
    } else {
        OfficialErrorKind::Upstream
    };
    OfficialError::new(kind, message)
}

pub(crate) fn redact_secret(value: &str, cap: usize) -> String {
    let mut text = value.to_owned();
    for key in ["api_key", "api-key", "token", "authorization"] {
        let lower = text.to_ascii_lowercase();
        let Some(start) = lower.find(key) else {
            continue;
        };
        let rest = &text[start..];
        let Some(split) = rest.find(['=', ':']) else {
            continue;
        };
        let begin = start + split + 1;
        let end = text[begin..]
            .find(|c: char| c.is_whitespace() || c == ',' || c == '}')
            .map_or(text.len(), |n| begin + n);
        text.replace_range(begin..end, "[redacted]");
    }
    let mut cursor = 0;
    while let Some(scheme) = text[cursor..].find("://").map(|n| cursor + n) {
        let authority = scheme + 3;
        let end = text[authority..]
            .find(['/', '?', '#', '\n', '\r', ' '])
            .map_or(text.len(), |n| authority + n);
        if let Some(at) = text[authority..end].rfind('@').map(|n| authority + n) {
            text.replace_range(authority..at, "[redacted]");
            cursor = authority + 10;
        } else {
            cursor = end.saturating_add(1);
        }
        if cursor >= text.len() {
            break;
        }
    }
    text.chars().take(cap).collect()
}
