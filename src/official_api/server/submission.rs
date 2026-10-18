use crate::official_api::wire::*;

pub(in crate::official_api::server) fn decoded_size(data: &str) -> u64 {
    (data.len() / 4 * 3).saturating_sub(
        data.as_bytes()
            .iter()
            .rev()
            .take_while(|b| **b == b'=')
            .count(),
    ) as u64
}

#[derive(Clone, Debug)]
pub(in crate::official_api::server) enum InputSource {
    FileId {
        file_id: String,
    },
    Inline {
        name: String,
        data: String,
    },
    /// Raw bytes from multipart fields or octet-stream bodies.
    Raw {
        name: String,
        bytes: bytes::Bytes,
    },
}

#[derive(Clone, Debug)]
pub(in crate::official_api::server) struct SubmittedFile {
    pub source: InputSource,
    pub page_range: Option<String>,
}

/// Transport-independent submission shared by `/v1/parse/jobs` and the
/// MinerU 4.x convenience `/v1/tasks` endpoints.
#[derive(Clone, Debug)]
pub(in crate::official_api::server) struct Submission {
    pub files: Vec<SubmittedFile>,
    pub tier: Option<Tier>,
    pub ocr_mode: OcrMode,
    pub output_formats: Vec<OutputFormat>,
}

impl Submission {
    pub(in crate::official_api::server) fn from_create(req: JobCreate) -> Self {
        Self {
            files: req
                .files
                .into_iter()
                .map(|file| SubmittedFile {
                    source: match file.source {
                        JobSource::FileId { file_id } => InputSource::FileId { file_id },
                        JobSource::Inline { name, data } => InputSource::Inline { name, data },
                    },
                    page_range: file.page_range,
                })
                .collect(),
            tier: req.tier,
            ocr_mode: req.ocr_mode,
            output_formats: req.output_formats,
        }
    }

    pub(in crate::official_api::server) fn validate(&self) -> Result<(), WireError> {
        if self.files.is_empty() || self.files.len() > 100 {
            return Err(WireError::bad("files", "Expected between 1 and 100 files."));
        }
        if self.output_formats.is_empty() {
            return Err(WireError::bad(
                "output_formats",
                "At least one output format is required.",
            ));
        }
        let mut formats = std::collections::BTreeSet::new();
        if self
            .output_formats
            .iter()
            .any(|format| !formats.insert(*format as u8))
        {
            return Err(WireError::bad(
                "output_formats",
                "Output formats must be unique.",
            ));
        }
        for file in &self.files {
            if let Some(range) = &file.page_range {
                validate_page_range(range)?;
            }
            match &file.source {
                InputSource::FileId { file_id } if file_id.trim().is_empty() => {
                    return Err(WireError::bad("file_id", "A file identifier is required."));
                }
                InputSource::FileId { .. } => {}
                InputSource::Inline { name, data } => {
                    if name.trim().is_empty() || name.len() > 255 {
                        return Err(WireError::bad("name", "A valid filename is required."));
                    }
                    validate_inline_base64(data)?;
                }
                InputSource::Raw { name, .. } => {
                    if name.trim().is_empty() || name.len() > 255 {
                        return Err(WireError::bad("name", "A valid filename is required."));
                    }
                }
            }
        }
        Ok(())
    }
}
