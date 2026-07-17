use std::{
    panic::{AssertUnwindSafe, catch_unwind},
    sync::Arc,
};

pub type ProgressCallback = Arc<dyn Fn(ProgressEvent) + Send + Sync + 'static>;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProgressEvent {
    ServerStarted {
        address: String,
    },
    ServerStopped,
    RequestAccepted {
        label: String,
    },
    RequestRejected {
        message: String,
    },
    RequestCompleted {
        label: String,
    },
    RequestFailed {
        label: String,
        message: String,
    },
    DocumentStarted {
        document: String,
    },
    DocumentPrepared {
        document: String,
    },
    DocumentPageCompleted {
        document: String,
        page_index: usize,
        completed: usize,
        total: usize,
    },
    DocumentCompleted {
        document: String,
    },
    DocumentFailed {
        document: String,
        message: String,
    },
    OfficeWarning {
        document: String,
        message: String,
    },
    ApiSubmitted {
        label: String,
    },
    ApiPending {
        label: String,
        queued_ahead: Option<i64>,
    },
    ApiProcessing {
        label: String,
    },
    ApiDownloading {
        label: String,
    },
    ApiExtracting {
        label: String,
    },
    ApiWarning {
        label: String,
        message: String,
    },
    ApiCompleted {
        label: String,
    },
    ApiFailed {
        label: String,
        message: String,
    },
}

pub(crate) fn emit(callback: &Option<ProgressCallback>, event: ProgressEvent) {
    if let Some(callback) = callback {
        let _ = catch_unwind(AssertUnwindSafe(|| callback(event)));
    }
}

#[doc(hidden)]
pub fn sanitize_event_text(raw: &str, cap: usize) -> String {
    let redacted = crate::error::sanitize_vlm_error_bytes(raw.as_bytes(), raw.len());
    let mut escaped = String::new();
    for character in redacted.chars() {
        match character {
            '\r' => escaped.push_str("\\r"),
            '\n' => escaped.push_str("\\n"),
            '\t' => escaped.push_str("\\t"),
            '\0' => escaped.push_str("\\0"),
            c if c.is_control() => escaped.push_str(&format!("\\u{{{:X}}}", c as u32)),
            c => escaped.push(c),
        }
    }
    if escaped.len() <= cap {
        return escaped;
    }
    const MARKER: &str = " [truncated]";
    let limit = if cap >= MARKER.len() {
        cap - MARKER.len()
    } else {
        cap
    };
    let end = escaped
        .char_indices()
        .map(|(i, c)| i + c.len_utf8())
        .take_while(|&i| i <= limit)
        .last()
        .unwrap_or(0);
    escaped.truncate(end);
    if cap >= MARKER.len() {
        escaped.push_str(MARKER);
    }
    escaped
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn sanitizes_and_caps_events() {
        let text = sanitize_event_text("Bearer secret https://example.test/a\n\t\0€", 200);
        assert!(
            !text.contains("secret")
                && !text.contains("example.test")
                && text.contains("\\n\\t\\0")
        );
        assert!(!text.chars().any(char::is_control));
        assert!(sanitize_event_text("€€", 1).is_empty());
        assert_eq!(sanitize_event_text("x", 0), "");
    }
    #[test]
    fn callback_panics_are_ignored() {
        emit(
            &Some(Arc::new(|_| panic!("nope"))),
            ProgressEvent::ServerStopped,
        );
    }
}
