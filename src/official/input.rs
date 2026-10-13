use crate::official::{OfficialError, OfficialErrorKind};
use bytes::Bytes;
use std::{
    fmt,
    path::{Path, PathBuf},
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

    pub(crate) fn read(&self) -> Result<Bytes, OfficialError> {
        match self {
            Self::Path(path) => std::fs::read(path).map(Bytes::from).map_err(|_| {
                OfficialError::new(
                    OfficialErrorKind::InvalidInput,
                    "could not read official input path",
                )
            }),
            Self::Bytes { data, .. } if data.is_empty() => Err(OfficialError::new(
                OfficialErrorKind::InvalidInput,
                "input is empty",
            )),
            Self::Bytes { data, .. } => Ok(data.clone()),
        }
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

pub(crate) fn suffixes() -> [&'static str; 28] {
    [
        "pdf", "png", "jpg", "jpeg", "webp", "gif", "bmp", "tiff", "jp2", "doc", "docx", "ppt",
        "pptx", "xls", "xlsx", "rtf", "odt", "ods", "odp", "html", "htm", "shtml", "mhtml", "mht",
        "csv", "tsv", "epub", "ofd",
    ]
}
