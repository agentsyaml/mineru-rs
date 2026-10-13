use crate::{
    official::{OfficialError, OfficialErrorKind, OfficialResult, OfficialRuntime},
    official_worker::{OfficialRequest, OfficialWorker},
};
use bytes::Bytes;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{collections::BTreeMap, path::PathBuf, time::Instant};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum OfficialRenderFormat {
    Markdown,
    Html,
    Latex,
    Docx,
    Epub,
    StructuredContent,
    ContentList,
    ContentListV2,
    Pdf,
}
impl OfficialRenderFormat {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Markdown => "markdown",
            Self::Html => "html",
            Self::Latex => "latex",
            Self::Docx => "docx",
            Self::Epub => "epub",
            Self::StructuredContent => "structured_content",
            Self::ContentList => "content_list",
            Self::ContentListV2 => "content_list_v2",
            Self::Pdf => "pdf",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum OfficialRenderMode {
    #[default]
    Default,
    Full,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum OfficialPdfLayout {
    #[default]
    Auto,
    Original,
    Reflow,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, rename_all = "snake_case")]
#[non_exhaustive]
pub struct OfficialLatexDelimiter {
    pub left: String,
    pub right: String,
}
impl OfficialLatexDelimiter {
    pub fn new(left: impl Into<String>, right: impl Into<String>) -> Self {
        Self {
            left: left.into(),
            right: right.into(),
        }
    }
}
impl Default for OfficialLatexDelimiter {
    fn default() -> Self {
        Self {
            left: "$".into(),
            right: "$".into(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, rename_all = "snake_case")]
#[non_exhaustive]
pub struct OfficialLatexDelimiters {
    pub display: OfficialLatexDelimiter,
    pub inline: OfficialLatexDelimiter,
}
impl OfficialLatexDelimiters {
    pub fn new(display: OfficialLatexDelimiter, inline: OfficialLatexDelimiter) -> Self {
        Self { display, inline }
    }
}
impl Default for OfficialLatexDelimiters {
    fn default() -> Self {
        Self {
            display: OfficialLatexDelimiter {
                left: "$$".into(),
                right: "$$".into(),
            },
            inline: OfficialLatexDelimiter::default(),
        }
    }
}

/// Closed union of upstream render dataclass fields; incompatible fields fail before worker launch.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "snake_case")]
#[non_exhaustive]
pub struct OfficialRenderOptions {
    pub mode: Option<OfficialRenderMode>,
    pub asset_base_url: Option<String>,
    pub standalone: Option<bool>,
    pub document_title: Option<String>,
    pub asset_base_path: Option<String>,
    pub title: Option<String>,
    pub authors: Option<Vec<String>>,
    pub language: Option<String>,
    pub identifier: Option<String>,
    pub modified_at: Option<String>,
    pub layout: Option<OfficialPdfLayout>,
    pub latex_delimiters: Option<OfficialLatexDelimiters>,
}

impl OfficialRenderOptions {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn with_mode(mut self, value: OfficialRenderMode) -> Self {
        self.mode = Some(value);
        self
    }
    pub fn with_asset_base_url(mut self, value: impl Into<String>) -> Self {
        self.asset_base_url = Some(value.into());
        self
    }
    pub fn with_standalone(mut self, value: bool) -> Self {
        self.standalone = Some(value);
        self
    }
    pub fn with_document_title(mut self, value: impl Into<String>) -> Self {
        self.document_title = Some(value.into());
        self
    }
    pub fn with_asset_base_path(mut self, value: impl Into<String>) -> Self {
        self.asset_base_path = Some(value.into());
        self
    }
    pub fn with_title(mut self, value: impl Into<String>) -> Self {
        self.title = Some(value.into());
        self
    }
    pub fn with_authors(mut self, value: Vec<String>) -> Self {
        self.authors = Some(value);
        self
    }
    pub fn with_language(mut self, value: impl Into<String>) -> Self {
        self.language = Some(value.into());
        self
    }
    pub fn with_identifier(mut self, value: impl Into<String>) -> Self {
        self.identifier = Some(value.into());
        self
    }
    pub fn with_modified_at(mut self, value: impl Into<String>) -> Self {
        self.modified_at = Some(value.into());
        self
    }
    pub fn with_pdf_layout(mut self, value: OfficialPdfLayout) -> Self {
        self.layout = Some(value);
        self
    }
    pub fn with_latex_delimiters(mut self, value: OfficialLatexDelimiters) -> Self {
        self.latex_delimiters = Some(value);
        self
    }

    pub(crate) fn validate(&self, format: OfficialRenderFormat) -> Result<(), OfficialError> {
        let allowed: &[&str] = match format {
            OfficialRenderFormat::Markdown => &["mode", "asset_base_url", "latex_delimiters"],
            OfficialRenderFormat::Html => {
                &["mode", "asset_base_url", "standalone", "document_title"]
            }
            OfficialRenderFormat::Latex => &["asset_base_path", "document_title"],
            OfficialRenderFormat::Docx => &[],
            OfficialRenderFormat::Epub => {
                &["title", "authors", "language", "identifier", "modified_at"]
            }
            OfficialRenderFormat::StructuredContent
            | OfficialRenderFormat::ContentList
            | OfficialRenderFormat::ContentListV2 => &["asset_base_url", "latex_delimiters"],
            OfficialRenderFormat::Pdf => &["document_title", "layout"],
        };
        let values = serde_json::to_value(self)
            .map_err(|e| OfficialError::new(OfficialErrorKind::InvalidOptions, e.to_string()))?;
        if let Some(object) = values.as_object()
            && object
                .iter()
                .any(|(key, value)| !value.is_null() && !allowed.contains(&key.as_str()))
        {
            return Err(OfficialError::new(
                OfficialErrorKind::InvalidOptions,
                "render option is not valid for this format",
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Debug)]
#[non_exhaustive]
pub enum OfficialRendered {
    Text {
        content: String,
        sidecars: BTreeMap<String, Bytes>,
    },
    Json {
        value: Value,
        sidecars: BTreeMap<String, Bytes>,
    },
    Binary {
        bytes: Bytes,
        sidecars: BTreeMap<String, Bytes>,
    },
}

pub(crate) async fn serialize(
    result: &OfficialResult,
    runtime: &OfficialRuntime,
    skip_defaults: bool,
) -> Result<Value, OfficialError> {
    let response = operation(
        runtime,
        "serialize",
        json!({"middle_json":result.middle,"skip_defaults":skip_defaults,"json":false}),
        &result.assets,
    )
    .await?;
    Ok(response.get("value").cloned().unwrap_or(Value::Null))
}

pub(crate) async fn serialize_json(
    result: &OfficialResult,
    runtime: &OfficialRuntime,
    skip_defaults: bool,
) -> Result<String, OfficialError> {
    let response = operation(
        runtime,
        "serialize",
        json!({"middle_json":result.middle,"skip_defaults":skip_defaults,"json":true}),
        &result.assets,
    )
    .await?;
    response
        .get("value")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| {
            OfficialError::new(
                OfficialErrorKind::Upstream,
                "worker returned invalid JSON serialization",
            )
        })
}

pub(crate) async fn restore(
    runtime: &OfficialRuntime,
    middle: Value,
    assets: BTreeMap<String, Bytes>,
    json_input: bool,
) -> Result<OfficialResult, OfficialError> {
    let payload = json!({"middle_json":middle,"json_input":json_input,"assets":assets.keys().collect::<Vec<_>>()});
    let bundle = operation_bundle(runtime, "restore", payload, &assets).await?;
    OfficialResult::from_bundle(bundle.path(), crate::official::runtime::RETAINED_RESULT_CAP)
}

impl OfficialResult {
    pub async fn render(
        &self,
        runtime: &OfficialRuntime,
        format: OfficialRenderFormat,
        options: OfficialRenderOptions,
    ) -> Result<OfficialRendered, OfficialError> {
        options.validate(format)?;
        let payload = json!({"middle_json":self.middle,"format":format.as_str(),"options":options,"assets":self.assets.keys().collect::<Vec<_>>()});
        let bundle = operation_bundle(runtime, "render", payload, &self.assets).await?;
        let root = bundle.path();
        let cap = crate::official::runtime::RETAINED_RESULT_CAP;
        let mut total = 0usize;
        let response: Value =
            serde_json::from_slice(&read_artifact(root, "render.json", &mut total, cap)?)
                .map_err(|e| error(e.to_string()))?;
        let sidecars = read_sidecars(root, &mut total, cap)?;
        match response.get("kind").and_then(Value::as_str) {
            Some("text") => {
                let content = response["content"]
                    .as_str()
                    .ok_or_else(|| error("render text missing"))?
                    .to_owned();
                Ok(OfficialRendered::Text { content, sidecars })
            }
            Some("json") => {
                let value = response["content"].clone();
                Ok(OfficialRendered::Json { value, sidecars })
            }
            Some("binary") => {
                let bytes = read_artifact(root, "render.bin", &mut total, cap)?;
                Ok(OfficialRendered::Binary {
                    bytes: Bytes::from(bytes),
                    sidecars,
                })
            }
            _ => Err(error("worker returned an invalid render kind")),
        }
    }
}

async fn operation(
    runtime: &OfficialRuntime,
    name: &str,
    payload: Value,
    assets: &BTreeMap<String, Bytes>,
) -> Result<Value, OfficialError> {
    let bundle = operation_bundle(runtime, name, payload, assets).await?;
    let mut total = 0;
    serde_json::from_slice(&read_artifact(
        bundle.path(),
        "operation.json",
        &mut total,
        crate::official::runtime::RETAINED_RESULT_CAP,
    )?)
    .map_err(|e| error(e.to_string()))
}

async fn operation_bundle(
    runtime: &OfficialRuntime,
    name: &str,
    payload: Value,
    assets: &BTreeMap<String, Bytes>,
) -> Result<crate::official_worker::OfficialBundle, OfficialError> {
    let bytes = serde_json::to_vec(&payload).map_err(|e| error(e.to_string()))?;
    let asset_bytes = assets
        .values()
        .try_fold(0usize, |sum, asset| sum.checked_add(asset.len()))
        .ok_or_else(limit_error)?;
    if bytes
        .len()
        .checked_add(asset_bytes)
        .is_none_or(|total| total > crate::official::runtime::RETAINED_RESULT_CAP)
    {
        return Err(limit_error());
    }
    let mut request = OfficialRequest::new(
        "flash".into(),
        "auto".into(),
        true,
        None,
        None,
        None,
        None,
        runtime.model_dir.clone(),
        runtime.config.clone(),
        crate::official::runtime::RETAINED_RESULT_CAP as u64,
        PathBuf::new(),
        PathBuf::new(),
    );
    request.operation = Some(name.into());
    let files = assets
        .iter()
        .map(|(path, data)| (path.as_str(), data.as_ref()))
        .collect::<Vec<_>>();
    let deadline = Instant::now()
        .checked_add(runtime.timeout)
        .ok_or_else(|| error("runtime timeout overflows"))?;
    OfficialWorker::new(runtime.python.clone())
        .map_err(error)?
        .run_with_files(&bytes, "json", request, deadline, &files)
        .await
        .map_err(error)
}

fn read_artifact(
    root: &std::path::Path,
    name: &str,
    total: &mut usize,
    cap: usize,
) -> Result<Vec<u8>, OfficialError> {
    let remaining = cap.saturating_sub(*total);
    let bytes = crate::hybrid_v4_output::read_bundle_file_nofollow(
        root,
        std::path::Path::new(name),
        remaining as u64,
    )
    .map_err(error)?;
    *total = total.checked_add(bytes.len()).ok_or_else(limit_error)?;
    Ok(bytes)
}

fn read_sidecars(
    root: &std::path::Path,
    total: &mut usize,
    cap: usize,
) -> Result<BTreeMap<String, Bytes>, OfficialError> {
    let images = root.join("images");
    let metadata = match std::fs::symlink_metadata(&images) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
        Err(error) => {
            return Err(OfficialError::new(
                OfficialErrorKind::Transport,
                error.to_string(),
            ));
        }
    };
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(error("worker returned an unsafe sidecar directory"));
    }
    let mut assets = BTreeMap::new();
    super::result_assets::collect_assets(root, &images, total, cap, &mut assets)?;
    Ok(assets)
}
fn error(message: impl AsRef<str>) -> OfficialError {
    crate::official::error::classify(message.as_ref().to_owned())
}
fn limit_error() -> OfficialError {
    OfficialError::new(
        OfficialErrorKind::LimitExceeded,
        "rendered result exceeds retained byte limit",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn response_and_sidecars_share_one_read_budget() {
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join("render.json"), b"12345").unwrap();
        fs::create_dir(root.path().join("images")).unwrap();
        fs::write(root.path().join("images/a.png"), b"1234").unwrap();
        let mut total = 0;
        read_artifact(root.path(), "render.json", &mut total, 8).unwrap();
        assert!(read_sidecars(root.path(), &mut total, 8).is_err());
    }

    #[test]
    fn binary_artifact_uses_only_the_budget_left_after_response_and_sidecars() {
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join("render.json"), b"{} ").unwrap();
        fs::create_dir(root.path().join("images")).unwrap();
        fs::write(root.path().join("images/a.png"), b"12").unwrap();
        let mut total = 0;
        read_artifact(root.path(), "render.json", &mut total, 8).unwrap();
        read_sidecars(root.path(), &mut total, 8).unwrap();
        fs::write(root.path().join("render.bin"), b"1234").unwrap();
        assert!(read_artifact(root.path(), "render.bin", &mut total, 8).is_err());
    }
}
