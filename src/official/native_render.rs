use super::{
    OfficialError, OfficialErrorKind, OfficialRenderFormat, OfficialRenderOptions,
    OfficialRendered, OfficialResult,
};
use serde_json::Value;
use std::collections::BTreeMap;

pub(super) fn serialize(result: &OfficialResult, _skip_defaults: bool) -> Value {
    result.middle.clone()
}

pub(super) fn render(
    result: &OfficialResult,
    format: OfficialRenderFormat,
    options: &OfficialRenderOptions,
) -> Result<OfficialRendered, OfficialError> {
    if options.mode.is_some()
        || options.asset_base_url.is_some()
        || options.standalone.is_some()
        || options.document_title.is_some()
        || options.asset_base_path.is_some()
        || options.title.is_some()
        || options.authors.is_some()
        || options.language.is_some()
        || options.identifier.is_some()
        || options.modified_at.is_some()
        || options.layout.is_some()
        || options.latex_delimiters.is_some()
    {
        return Err(unsupported("CSV rendering does not support render options"));
    }
    match format {
        OfficialRenderFormat::Markdown => Ok(OfficialRendered::Text {
            content: result.markdown.clone(),
            sidecars: BTreeMap::new(),
        }),
        OfficialRenderFormat::StructuredContent => Ok(OfficialRendered::Json {
            value: result.structured.clone(),
            sidecars: BTreeMap::new(),
        }),
        _ => Err(unsupported(
            "CSV supports markdown and structured_content rendering only",
        )),
    }
}

fn unsupported(message: &str) -> OfficialError {
    OfficialError::new(OfficialErrorKind::UnsupportedOperation, message)
}
