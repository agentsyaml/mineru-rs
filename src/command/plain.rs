use std::{
    ffi::OsString,
    io::Write,
    sync::{Arc, Mutex, MutexGuard},
};

use super::{CommandCallback, CommandEvent};
use crate::{ProgressCallback, ProgressEvent, sanitize_event_text};

const TEXT_CAP: usize = 512;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LogLevel {
    Trace,
    Debug,
    Info,
    Success,
    Warning,
    Error,
    Critical,
}

impl LogLevel {
    pub fn parse(value: Option<OsString>) -> Result<Self, &'static str> {
        let value = match value {
            None => return Ok(Self::Info),
            Some(value) => value
                .into_string()
                .map_err(|_| "invalid MINERU_LOG_LEVEL")?,
        };
        match value.to_ascii_uppercase().as_str() {
            "TRACE" => Ok(Self::Trace),
            "DEBUG" => Ok(Self::Debug),
            "INFO" => Ok(Self::Info),
            "SUCCESS" => Ok(Self::Success),
            "WARNING" => Ok(Self::Warning),
            "ERROR" => Ok(Self::Error),
            "CRITICAL" => Ok(Self::Critical),
            _ => Err("invalid MINERU_LOG_LEVEL"),
        }
    }

    pub fn from_env() -> Result<Self, &'static str> {
        Self::parse(std::env::var_os("MINERU_LOG_LEVEL"))
    }

    pub(crate) fn admits(self, severity: Severity) -> bool {
        match self {
            Self::Trace | Self::Debug | Self::Info => true,
            Self::Success | Self::Warning => severity != Severity::Info,
            Self::Error => severity == Severity::Error,
            Self::Critical => false,
        }
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
pub(crate) enum Severity {
    Info,
    Warning,
    Error,
}

pub type WarningCallback = Arc<dyn Fn(&str, &str) + Send + Sync + 'static>;

pub struct EventSink<W: Write + Send> {
    state: Mutex<State<W>>,
    tty: bool,
    level: LogLevel,
}

struct State<W> {
    writer: W,
    finished: bool,
    displayed: bool,
    warnings: usize,
    last_warning: String,
}

impl<W: Write + Send + 'static> EventSink<W> {
    pub fn new(writer: W, tty: bool, level: LogLevel) -> Self {
        Self {
            state: Mutex::new(State {
                writer,
                finished: false,
                displayed: false,
                warnings: 0,
                last_warning: String::new(),
            }),
            tty,
            level,
        }
    }

    #[allow(dead_code)] // The canonical CLI wraps events to suppress duplicate failures.
    pub fn callback(self: &Arc<Self>) -> ProgressCallback {
        let sink = Arc::clone(self);
        Arc::new(move |event| sink.event(event))
    }

    pub fn warning_callback(self: &Arc<Self>) -> WarningCallback {
        let sink = Arc::clone(self);
        Arc::new(move |source, message| sink.warning(source, message))
    }

    pub(crate) fn command_callback(self: &Arc<Self>) -> CommandCallback {
        let sink = Arc::clone(self);
        Arc::new(move |event| {
            if let CommandEvent::Progress { event, .. } = event {
                sink.event(event);
            }
        })
    }

    pub fn event(&self, event: ProgressEvent) {
        let (severity, phrase, primary, detail, warning) = format_event(event);
        if !self.level.admits(severity) {
            return;
        }
        self.render(severity, phrase, primary, detail, warning);
    }

    pub fn warning(&self, source: &str, message: &str) {
        if self.level.admits(Severity::Warning) {
            self.render(
                Severity::Warning,
                "warning",
                clean(source.to_owned()),
                Some(clean(message.to_owned())),
                true,
            );
        }
    }

    pub fn fail(&self, message: &str) {
        if self.level.admits(Severity::Error) {
            self.render(
                Severity::Error,
                "failed",
                clean(message.to_owned()),
                None,
                false,
            );
        }
    }

    pub fn finish(&self) {
        let mut state = self.lock();
        if state.finished {
            return;
        }
        state.finished = true;
        if self.tty && state.displayed {
            let _ = state.writer.write_all(b"\n");
            let _ = state.writer.flush();
        }
    }

    fn render(
        &self,
        _severity: Severity,
        phrase: &str,
        primary: String,
        detail: Option<String>,
        warning: bool,
    ) {
        let mut state = self.lock();
        if state.finished {
            return;
        }
        if warning {
            state.warnings = state.warnings.saturating_add(1);
            state.last_warning = detail.as_deref().unwrap_or(&primary).to_owned();
        }
        let mut line = format!("{phrase}: {primary}");
        if let Some(detail) = detail {
            line.push_str(": ");
            line.push_str(&detail);
        }
        if self.tty {
            if state.warnings != 0 {
                line.push_str(&format!(
                    " | warnings={} last-warning={}",
                    state.warnings, state.last_warning
                ));
            }
            let _ = state.writer.write_all(format!("\r{line}\x1b[K").as_bytes());
            state.displayed = true;
        } else {
            let _ = state.writer.write_all(format!("{line}\n").as_bytes());
        }
        let _ = state.writer.flush();
    }

    fn lock(&self) -> MutexGuard<'_, State<W>> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

fn clean(value: String) -> String {
    sanitize_event_text(&value, TEXT_CAP)
}

fn format_event(event: ProgressEvent) -> (Severity, &'static str, String, Option<String>, bool) {
    use ProgressEvent::*;
    match event {
        ServerStarted { address } => (
            Severity::Info,
            "server started",
            clean(address),
            None,
            false,
        ),
        ServerStopped => (
            Severity::Info,
            "server stopped",
            "server".into(),
            None,
            false,
        ),
        RequestAccepted { label } => (
            Severity::Info,
            "request accepted",
            clean(label),
            None,
            false,
        ),
        RequestRejected { message } => (
            Severity::Error,
            "request rejected",
            clean(message),
            None,
            false,
        ),
        RequestCompleted { label } => (
            Severity::Info,
            "request completed",
            clean(label),
            None,
            false,
        ),
        RequestFailed { label, message } => (
            Severity::Error,
            "request failed",
            clean(label),
            Some(clean(message)),
            false,
        ),
        DocumentStarted { document } => (
            Severity::Info,
            "document started",
            clean(document),
            None,
            false,
        ),
        DocumentPrepared { document } => (
            Severity::Info,
            "document prepared",
            clean(document),
            None,
            false,
        ),
        DocumentPageCompleted {
            document,
            page_index,
            completed,
            total,
        } => (
            Severity::Info,
            "document page completed",
            clean(document),
            Some(format!("page={page_index} completed={completed}/{total}")),
            false,
        ),
        DocumentCompleted { document } => (
            Severity::Info,
            "document completed",
            clean(document),
            None,
            false,
        ),
        DocumentFailed { document, message } => (
            Severity::Error,
            "document failed",
            clean(document),
            Some(clean(message)),
            false,
        ),
        OfficeWarning { document, message } => (
            Severity::Warning,
            "office warning",
            clean(document),
            Some(clean(message)),
            true,
        ),
        ApiSubmitted { label } => (Severity::Info, "api submitted", clean(label), None, false),
        ApiPending {
            label,
            queued_ahead,
        } => (
            Severity::Info,
            "api pending",
            clean(label),
            Some(match queued_ahead {
                Some(value) => format!("queued-ahead={value}"),
                None => "queued-ahead=none".into(),
            }),
            false,
        ),
        ApiProcessing { label } => (Severity::Info, "api processing", clean(label), None, false),
        ApiDownloading { label } => (Severity::Info, "api downloading", clean(label), None, false),
        ApiExtracting { label } => (Severity::Info, "api extracting", clean(label), None, false),
        ApiWarning { label, message } => (
            Severity::Warning,
            "api warning",
            clean(label),
            Some(clean(message)),
            true,
        ),
        ApiCompleted { label } => (Severity::Info, "api completed", clean(label), None, false),
        ApiFailed { label, message } => (
            Severity::Error,
            "api failed",
            clean(label),
            Some(clean(message)),
            false,
        ),
    }
}

pub(crate) fn event_severity(event: &ProgressEvent) -> Severity {
    match event {
        ProgressEvent::RequestRejected { .. }
        | ProgressEvent::RequestFailed { .. }
        | ProgressEvent::DocumentFailed { .. }
        | ProgressEvent::ApiFailed { .. } => Severity::Error,
        ProgressEvent::OfficeWarning { .. } | ProgressEvent::ApiWarning { .. } => Severity::Warning,
        _ => Severity::Info,
    }
}

#[cfg(test)]
mod command_tests {
    use super::*;
    use crate::command::{CommandEvent, CommandScope, DocumentId};

    #[derive(Clone, Default)]
    struct Buffer(Arc<Mutex<Vec<u8>>>);
    impl Write for Buffer {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn command_lifecycle_is_silent_and_progress_is_plain() {
        let buffer = Buffer::default();
        let sink = Arc::new(EventSink::new(buffer.clone(), false, LogLevel::Info));
        let callback = sink.command_callback();
        callback(CommandEvent::RunPlanned {
            documents: 1,
            api_tasks: 0,
        });
        callback(CommandEvent::RunCompleted);
        callback(CommandEvent::RunFailed {
            message: "ignored".into(),
        });
        assert!(buffer.0.lock().unwrap().is_empty());
        callback(CommandEvent::Progress {
            scope: CommandScope::Document(DocumentId(1)),
            event: ProgressEvent::DocumentStarted {
                document: "doc".into(),
            },
        });
        assert_eq!(&*buffer.0.lock().unwrap(), b"document started: doc\n");
    }
}
