use crate::{Error, Result};
use bytes::Bytes;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub enum PdfInput {
    Path(PathBuf),
    Bytes(Bytes),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ParseOptions {
    pub page_range: Option<PageRange>,
    pub formula: bool,
    pub table: bool,
    pub image_analysis: bool,
    pub max_new_tokens: Option<u32>,
    pub allow_truncated: bool,
}
impl Default for ParseOptions {
    fn default() -> Self {
        Self {
            page_range: None,
            formula: true,
            table: true,
            image_analysis: true,
            max_new_tokens: None,
            allow_truncated: true,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PageRange {
    pub start: u32,
    pub end: Option<u32>,
}
impl PageRange {
    pub fn new(start: u32, end: Option<u32>) -> Result<Self> {
        let range = Self { start, end };
        range.validate()?;
        Ok(range)
    }
    pub fn validate(&self) -> Result<()> {
        if self.end.is_some_and(|end| self.start > end) {
            Err(Error::InvalidInput(
                "page range start must not exceed end".into(),
            ))
        } else {
            Ok(())
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Document {
    pub pages: Vec<PageResult>,
    pub markdown: String,
    pub middle_json: Value,
    pub content_list: Value,
    pub assets: Vec<Asset>,
    /// Recoverable per-page/per-block failures surfaced during direct parsing. Never fatal;
    /// consumers should display these as warnings, not abort the document.
    #[serde(default)]
    pub warnings: Vec<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PageResult {
    pub page_index: usize,
    pub page_size: [f32; 2],
    pub blocks: Vec<ContentBlock>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContentBlock {
    pub kind: BlockKind,
    pub bbox: NormalizedBbox,
    pub angle: Option<Rotation>,
    pub content: Option<String>,
    pub merge_previous: bool,
    #[serde(default)]
    pub(crate) metadata: Map<String, Value>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct BlockKind(String);
impl BlockKind {
    pub const TEXT: &str = "text";
    pub const TITLE: &str = "title";
    pub const TABLE: &str = "table";
    pub const EQUATION: &str = "equation";
    pub const FORMULA_NUMBER: &str = "formula_number";
    pub const CODE: &str = "code";
    pub const ALGORITHM: &str = "algorithm";
    pub const ASIDE_TEXT: &str = "aside_text";
    pub const REF_TEXT: &str = "ref_text";
    pub const INDEX: &str = "index";
    pub const PHONETIC: &str = "phonetic";
    pub const LIST_ITEM: &str = "list_item";
    pub const TABLE_CAPTION: &str = "table_caption";
    pub const IMAGE_CAPTION: &str = "image_caption";
    pub const CODE_CAPTION: &str = "code_caption";
    pub const TABLE_FOOTNOTE: &str = "table_footnote";
    pub const IMAGE_FOOTNOTE: &str = "image_footnote";
    pub const HEADER: &str = "header";
    pub const FOOTER: &str = "footer";
    pub const PAGE_NUMBER: &str = "page_number";
    pub const PAGE_FOOTNOTE: &str = "page_footnote";
    pub const IMAGE: &str = "image";
    pub const CHART: &str = "chart";
    pub const LIST: &str = "list";
    pub const IMAGE_BLOCK: &str = "image_block";
    pub const EQUATION_BLOCK: &str = "equation_block";
    pub const UNKNOWN: &str = "unknown";

    pub fn new(kind: impl Into<String>) -> Self {
        Self(kind.into())
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum Rotation {
    #[default]
    Deg0,
    Deg90,
    Deg180,
    Deg270,
}
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct NormalizedBbox {
    pub left: f32,
    pub top: f32,
    pub right: f32,
    pub bottom: f32,
}
impl NormalizedBbox {
    pub fn new(left: f32, top: f32, right: f32, bottom: f32) -> Result<Self> {
        if [left, top, right, bottom]
            .iter()
            .any(|v| !v.is_finite() || !(0.0..=1.0).contains(v))
            || left >= right
            || top >= bottom
        {
            return Err(Error::InvalidInput(
                "bbox must be finite, normalized, and non-empty".into(),
            ));
        }
        Ok(Self {
            left,
            top,
            right,
            bottom,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum AssetKind {
    Image,
    Table,
    Equation,
    Chart,
    Other(String),
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Asset {
    pub kind: AssetKind,
    pub relative_path: PathBuf,
    pub media_type: String,
    #[serde(skip, default)]
    pub data: Bytes,
    pub md5: String,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelInfo {
    pub id: String,
    pub owned_by: Option<String>,
}
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutputManifest {
    pub document_json: PathBuf,
    pub markdown: PathBuf,
    pub middle_json: PathBuf,
    pub content_list: PathBuf,
    pub assets: Vec<PathBuf>,
}

#[cfg(test)]
mod tests {
    use super::{BlockKind, Document, NormalizedBbox, PageRange, ParseOptions};

    #[test]
    fn rejects_invalid_ranges_and_boxes() {
        assert!(PageRange::new(2, Some(1)).is_err());
        assert!(NormalizedBbox::new(0.0, 0.0, 0.0, 1.0).is_err());
    }

    #[test]
    fn defaults_and_block_kinds_preserve_protocol_values() {
        let options: ParseOptions = serde_json::from_str("{}").unwrap();
        assert!(
            options.formula && options.table && options.image_analysis && options.allow_truncated
        );
        let kind: BlockKind = serde_json::from_str("\"future_kind\"").unwrap();
        assert_eq!(kind.as_str(), "future_kind");
        assert_eq!(
            serde_json::to_string(&BlockKind::new(BlockKind::TEXT)).unwrap(),
            "\"text\""
        );
    }

    #[test]
    fn document_round_trips_without_warnings_field() {
        let value = serde_json::json!({
            "pages": [],
            "markdown": "# empty",
            "middle_json": {},
            "content_list": [],
            "assets": []
        });
        let document: Document = serde_json::from_value(value).unwrap();
        assert!(document.warnings.is_empty());
        let round_tripped: Document =
            serde_json::from_value(serde_json::to_value(&document).unwrap()).unwrap();
        assert_eq!(round_tripped.warnings, document.warnings);
        assert_eq!(round_tripped.markdown, "# empty");
    }
}
