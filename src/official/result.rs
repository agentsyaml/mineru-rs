use crate::official::{OfficialDataWriter, OfficialError, OfficialErrorKind, OfficialRuntime};
use bytes::Bytes;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{collections::BTreeMap, fmt, path::Path};

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct OfficialVersionMetadata {
    pub mineru_package: String,
    pub schema_version: String,
    pub bundle_name: String,
}

#[derive(Clone)]
#[non_exhaustive]
pub struct OfficialResult {
    pub(crate) middle: Value,
    pub(crate) model: Option<Value>,
    pub(crate) markdown: String,
    pub(crate) structured: Value,
    pub(crate) assets: BTreeMap<String, Bytes>,
    pub(crate) warnings: Vec<String>,
    pub(crate) metadata: OfficialVersionMetadata,
}

impl fmt::Debug for OfficialResult {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OfficialResult")
            .field("metadata", &self.metadata)
            .field("pages", &self.pages().len())
            .field("markdown_bytes", &self.markdown.len())
            .field("asset_count", &self.assets.len())
            .field("warning_count", &self.warnings.len())
            .field("model_output_present", &self.model.is_some())
            .finish()
    }
}

impl OfficialResult {
    pub fn middle_json(&self) -> &Value {
        &self.middle
    }
    pub fn model_json(&self) -> Option<&Value> {
        self.model.as_ref()
    }
    pub fn markdown(&self) -> &str {
        &self.markdown
    }
    pub fn structured_content(&self) -> &Value {
        &self.structured
    }
    pub fn pages(&self) -> &[Value] {
        self.middle
            .get("pages")
            .and_then(Value::as_array)
            .map_or(&[], Vec::as_slice)
    }
    pub fn assets(&self) -> &BTreeMap<String, Bytes> {
        &self.assets
    }
    pub fn warnings(&self) -> &[String] {
        &self.warnings
    }
    pub fn version_metadata(&self) -> &OfficialVersionMetadata {
        &self.metadata
    }
    pub fn export_pages(&self) -> Vec<Value> {
        self.pages().to_vec()
    }
    pub(crate) fn retained_bytes(&self) -> usize {
        [
            self.markdown.len(),
            self.structured.to_string().len(),
            self.middle.to_string().len(),
            self.model
                .as_ref()
                .map_or(0, |value| value.to_string().len()),
            self.assets
                .values()
                .map(Bytes::len)
                .fold(0usize, usize::saturating_add),
        ]
        .into_iter()
        .fold(0usize, usize::saturating_add)
    }

    pub async fn to_dict(
        &self,
        runtime: &OfficialRuntime,
        skip_defaults: bool,
    ) -> Result<Value, OfficialError> {
        crate::official::render::serialize(self, runtime, skip_defaults).await
    }
    pub async fn to_json(
        &self,
        runtime: &OfficialRuntime,
        skip_defaults: bool,
    ) -> Result<String, OfficialError> {
        crate::official::render::serialize_json(self, runtime, skip_defaults).await
    }

    pub fn save_with_writer(
        &self,
        writer: &mut impl OfficialDataWriter,
    ) -> Result<(), OfficialError> {
        self.validate_owned_assets()?;
        writer.write_string("markdown.md", &self.markdown)?;
        let middle = serde_json::to_string_pretty(&self.middle)
            .map_err(|error| OfficialError::new(OfficialErrorKind::Upstream, error.to_string()))?;
        let structured = serde_json::to_string_pretty(&self.structured)
            .map_err(|error| OfficialError::new(OfficialErrorKind::Upstream, error.to_string()))?;
        writer.write_string("middle_json.json", &middle)?;
        writer.write_string("structured_content.json", &structured)?;
        if let Some(model) = &self.model {
            let json = serde_json::to_string_pretty(model).map_err(|error| {
                OfficialError::new(OfficialErrorKind::Upstream, error.to_string())
            })?;
            writer.write_string("model_output.json", &json)?;
        }
        for (path, bytes) in &self.assets {
            writer.write(path, bytes)?;
        }
        Ok(())
    }

    pub fn save(&self, root: impl AsRef<Path>, stem: &str) -> Result<(), OfficialError> {
        self.validate_owned_assets()?;
        let canonical = crate::official_output::canonical_stem(stem).map_err(|error| {
            OfficialError::new(OfficialErrorKind::InvalidInput, error.to_string())
        })?;
        if canonical != stem {
            return Err(OfficialError::new(
                OfficialErrorKind::InvalidInput,
                "stem is not a safe output name",
            ));
        }
        let temporary = tempfile::tempdir()
            .map_err(|error| OfficialError::new(OfficialErrorKind::Transport, error.to_string()))?;
        let mut writer = super::result_assets::LocalBundleWriter::new(temporary.path().to_owned());
        self.save_with_writer(&mut writer)?;
        crate::hybrid_v4_output::validate_and_publish(
            temporary.path(),
            root.as_ref(),
            stem,
            crate::official::runtime::BUNDLE_BYTE_CAP,
        )
        .map_err(|error| OfficialError::new(OfficialErrorKind::InvalidInput, error))
    }

    pub async fn from_dict(
        runtime: &OfficialRuntime,
        middle: Value,
        assets: BTreeMap<String, Bytes>,
    ) -> Result<Self, OfficialError> {
        crate::official::render::restore(runtime, middle, assets, false).await
    }
    pub async fn from_json(
        runtime: &OfficialRuntime,
        json: &str,
        assets: BTreeMap<String, Bytes>,
    ) -> Result<Self, OfficialError> {
        crate::official::render::restore(runtime, Value::String(json.to_owned()), assets, true)
            .await
    }

    pub(crate) fn from_bundle(root: &Path, retained_cap: usize) -> Result<Self, OfficialError> {
        let retained_cap = retained_cap.min(crate::official::runtime::RETAINED_RESULT_CAP);
        let validated = tempfile::tempdir()
            .map_err(|error| OfficialError::new(OfficialErrorKind::Transport, error.to_string()))?;
        crate::hybrid_v4_output::validate_and_publish(
            root,
            validated.path(),
            "result",
            retained_cap as u64,
        )
        .map_err(|error| OfficialError::new(OfficialErrorKind::InvalidInput, error))?;
        let root = validated.path().join("result/hybrid-v4");
        let mut total = 0usize;
        let read = |name: &str, total: &mut usize| -> Result<Vec<u8>, OfficialError> {
            let remaining = retained_cap.saturating_sub(*total);
            let bytes = crate::hybrid_v4_output::read_bundle_file_nofollow(
                &root,
                Path::new(name),
                remaining as u64,
            )
            .map_err(crate::official::error::classify)?;
            *total = total.checked_add(bytes.len()).ok_or_else(|| {
                OfficialError::new(
                    OfficialErrorKind::LimitExceeded,
                    "result exceeds retained byte limit",
                )
            })?;
            Ok(bytes)
        };
        let markdown = String::from_utf8(read("markdown.md", &mut total)?).map_err(|error| {
            OfficialError::new(OfficialErrorKind::InvalidInput, error.to_string())
        })?;
        let middle: Value = serde_json::from_slice(&read("middle_json.json", &mut total)?)
            .map_err(|error| {
                OfficialError::new(OfficialErrorKind::InvalidInput, error.to_string())
            })?;
        let structured: Value =
            serde_json::from_slice(&read("structured_content.json", &mut total)?).map_err(
                |error| OfficialError::new(OfficialErrorKind::InvalidInput, error.to_string()),
            )?;
        let model_path = root.join("model_output.json");
        let model = if model_path.exists() {
            Some(
                serde_json::from_slice(&read("model_output.json", &mut total)?).map_err(
                    |error| OfficialError::new(OfficialErrorKind::InvalidInput, error.to_string()),
                )?,
            )
        } else {
            None
        };
        let mut assets = BTreeMap::new();
        super::result_assets::collect_assets(&root, &root, &mut total, retained_cap, &mut assets)?;
        if total > retained_cap {
            return Err(OfficialError::new(
                OfficialErrorKind::LimitExceeded,
                "result exceeds retained byte limit",
            ));
        }
        Ok(Self {
            middle,
            model,
            markdown,
            structured,
            assets,
            warnings: Vec::new(),
            metadata: OfficialVersionMetadata {
                mineru_package: "4.0.12".into(),
                schema_version: "2.0".into(),
                bundle_name: "hybrid-v4".into(),
            },
        })
    }

    fn validate_owned_assets(&self) -> Result<(), OfficialError> {
        super::result_assets::validate_owned_assets(self)
    }
}
