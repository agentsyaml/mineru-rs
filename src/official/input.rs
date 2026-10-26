use crate::official::{OfficialError, OfficialErrorKind};
use bytes::Bytes;
use std::{
    fmt,
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
    time::Instant,
};

#[derive(Clone)]
#[non_exhaustive]
pub enum OfficialInput {
    Path(PathBuf),
    Bytes { filename: String, data: Bytes },
}
impl fmt::Debug for OfficialInput {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Path(_) => f.write_str("Path([redacted])"),
            Self::Bytes { filename, data } => f
                .debug_struct("Bytes")
                .field("filename", filename)
                .field("length", &data.len())
                .finish(),
        }
    }
}

impl OfficialInput {
    pub fn path(path: impl Into<PathBuf>) -> Self {
        Self::Path(path.into())
    }
    pub fn bytes(filename: impl Into<String>, data: impl Into<Bytes>) -> Self {
        Self::Bytes {
            filename: filename.into(),
            data: data.into(),
        }
    }

    pub(crate) fn filename(&self) -> &str {
        match self {
            Self::Path(path) => path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("input"),
            Self::Bytes { filename, .. } => filename,
        }
    }

    pub(crate) fn is_native(&self) -> bool {
        self.suffix().is_ok_and(|suffix| {
            !matches!(
                suffix.as_str(),
                "pdf" | "png" | "jpg" | "jpeg" | "webp" | "gif" | "bmp" | "tiff" | "jp2"
            )
        })
    }

    pub(crate) fn suffix(&self) -> Result<String, OfficialError> {
        let name = match self {
            Self::Path(path) => path.as_path(),
            Self::Bytes { filename, .. } => Path::new(filename),
        };
        let suffix = name
            .extension()
            .and_then(|value| value.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        if suffixes().contains(&suffix.as_str()) {
            Ok(suffix)
        } else {
            Err(OfficialError::new(
                OfficialErrorKind::InvalidInput,
                "unsupported official input format",
            ))
        }
    }

    #[cfg(test)]
    pub(crate) fn read_limited(&self, limit: usize) -> Result<Bytes, OfficialError> {
        self.read_limited_controlled(limit, &AtomicBool::new(false), None)
    }

    pub(crate) fn read_limited_controlled(
        &self,
        limit: usize,
        cancelled: &AtomicBool,
        deadline: Option<Instant>,
    ) -> Result<Bytes, OfficialError> {
        check_read(cancelled, deadline)?;
        match self {
            Self::Bytes { data, .. } if data.len() > limit => Err(OfficialError::new(
                OfficialErrorKind::LimitExceeded,
                "official input exceeds byte limit",
            )),
            Self::Bytes { data, .. } => Ok(data.clone()),
            Self::Path(path) => {
                use std::io::Read;
                check_read(cancelled, deadline)?;
                let path_metadata = std::fs::metadata(path).map_err(|_| {
                    OfficialError::new(
                        OfficialErrorKind::InvalidInput,
                        "could not read official input path",
                    )
                })?;
                if !path_metadata.is_file() {
                    return Err(OfficialError::new(
                        OfficialErrorKind::InvalidInput,
                        "official input path must be a regular file",
                    ));
                }
                check_read(cancelled, deadline)?;
                let file = std::fs::File::open(path).map_err(|_| {
                    OfficialError::new(
                        OfficialErrorKind::InvalidInput,
                        "could not read official input path",
                    )
                })?;
                let metadata = file
                    .metadata()
                    .map_err(|e| OfficialError::new(OfficialErrorKind::Transport, e.to_string()))?;
                if !metadata.is_file() {
                    return Err(OfficialError::new(
                        OfficialErrorKind::InvalidInput,
                        "official input path must be a regular file",
                    ));
                }
                if metadata.len() > limit as u64 {
                    return Err(OfficialError::new(
                        OfficialErrorKind::LimitExceeded,
                        "official input exceeds byte limit",
                    ));
                }
                check_read(cancelled, deadline)?;
                let mut bytes = Vec::with_capacity(limit.min(1024 * 1024));
                let mut file = file;
                let mut buffer = [0; 64 * 1024];
                loop {
                    check_read(cancelled, deadline)?;
                    let count = limit
                        .saturating_sub(bytes.len())
                        .saturating_add(1)
                        .min(buffer.len());
                    let read = file.read(&mut buffer[..count]).map_err(|e| {
                        OfficialError::new(OfficialErrorKind::Transport, e.to_string())
                    })?;
                    if read == 0 {
                        break;
                    }
                    bytes.extend_from_slice(&buffer[..read]);
                    if bytes.len() > limit {
                        break;
                    }
                }
                if bytes.len() > limit {
                    return Err(OfficialError::new(
                        OfficialErrorKind::LimitExceeded,
                        "official input exceeds byte limit",
                    ));
                }
                Ok(Bytes::from(bytes))
            }
        }
    }

    pub(crate) async fn acquire_limited(
        self,
        limit: usize,
        deadline: Instant,
    ) -> Result<Bytes, OfficialError> {
        self.acquire_limited_with(limit, deadline, || {}).await
    }

    #[cfg(test)]
    pub(crate) async fn acquire_limited_with_start(
        self,
        limit: usize,
        deadline: Instant,
        on_start: impl FnOnce() + Send + 'static,
    ) -> Result<Bytes, OfficialError> {
        self.acquire_limited_with(limit, deadline, on_start).await
    }

    async fn acquire_limited_with(
        self,
        limit: usize,
        deadline: Instant,
        on_start: impl FnOnce() + Send + 'static,
    ) -> Result<Bytes, OfficialError> {
        let cancelled = std::sync::Arc::new(AtomicBool::new(false));
        let worker_cancelled = cancelled.clone();
        let mut guard = InputReadCancel(Some(cancelled), None);
        let task = tokio::task::spawn_blocking(move || {
            on_start();
            self.read_limited_controlled(limit, &worker_cancelled, Some(deadline))
        });
        guard.1 = Some(task.abort_handle());
        let result = tokio::time::timeout_at(tokio::time::Instant::from_std(deadline), task).await;
        if result.is_ok() {
            guard.disarm();
        }
        result
            .map_err(|_| {
                OfficialError::new(
                    OfficialErrorKind::Deadline,
                    "official input acquisition deadline exceeded",
                )
            })?
            .map_err(|error| OfficialError::new(OfficialErrorKind::Upstream, error.to_string()))?
    }

    pub(crate) fn path_context(&self) -> Option<(String, PathBuf)> {
        let Self::Path(path) = self else { return None };
        let path = if path.is_absolute() {
            path.clone()
        } else {
            std::env::current_dir().ok()?.join(path)
        };
        let uri = url::Url::from_file_path(&path).ok()?.to_string();
        Some((uri, path.parent()?.to_owned()))
    }
}

struct InputReadCancel(
    Option<std::sync::Arc<AtomicBool>>,
    Option<tokio::task::AbortHandle>,
);

impl InputReadCancel {
    fn disarm(&mut self) {
        self.0 = None;
        self.1 = None;
    }
}

impl Drop for InputReadCancel {
    fn drop(&mut self) {
        if let Some(flag) = self.0.take() {
            flag.store(true, Ordering::Relaxed);
        }
        if let Some(abort) = self.1.take() {
            abort.abort();
        }
    }
}

fn check_read(cancelled: &AtomicBool, deadline: Option<Instant>) -> Result<(), OfficialError> {
    let (kind, message) = if cancelled.load(Ordering::Relaxed) {
        (
            OfficialErrorKind::Cancelled,
            "official input acquisition cancelled",
        )
    } else if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
        (
            OfficialErrorKind::Deadline,
            "official input acquisition deadline exceeded",
        )
    } else {
        return Ok(());
    };
    Err(OfficialError::new(kind, message))
}

pub(crate) fn suffixes() -> [&'static str; 28] {
    [
        "pdf", "png", "jpg", "jpeg", "webp", "gif", "bmp", "tiff", "jp2", "doc", "docx", "ppt",
        "pptx", "xls", "xlsx", "rtf", "odt", "ods", "odp", "html", "htm", "shtml", "mhtml", "mht",
        "csv", "tsv", "epub", "ofd",
    ]
}
