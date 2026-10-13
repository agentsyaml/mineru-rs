use super::{NativeAsset, NativeInput, NativeRuntimeOptions};
use mineru::official::{
    OfficialError, OfficialErrorKind, OfficialInput, OfficialParseOptions, OfficialRenderFormat,
    OfficialRenderOptions, OfficialRuntime,
};
use napi::{Error, Result, bindgen_prelude::Buffer};
use serde_json::Value;
use std::{collections::BTreeMap, path::PathBuf, time::Duration};

pub(super) fn error_kind(kind: OfficialErrorKind) -> &'static str {
    match kind {
        OfficialErrorKind::InvalidOptions => "invalid_options",
        OfficialErrorKind::InvalidInput => "invalid_input",
        OfficialErrorKind::UnsupportedOperation => "unsupported_operation",
        OfficialErrorKind::LimitExceeded => "limit_exceeded",
        OfficialErrorKind::RuntimeUnavailable => "runtime_unavailable",
        OfficialErrorKind::Upstream => "upstream",
        OfficialErrorKind::Deadline => "deadline",
        OfficialErrorKind::Cancelled => "cancelled",
        OfficialErrorKind::Transport => "transport",
        _ => "upstream",
    }
}

pub(super) fn official_error(error: OfficialError) -> Error {
    Error::from_reason(format!(
        "{}: {}",
        error_kind(error.kind),
        safe_error_message(error.kind)
    ))
}

pub(super) fn safe_error_message(kind: OfficialErrorKind) -> &'static str {
    match kind {
        OfficialErrorKind::InvalidOptions => "invalid options",
        OfficialErrorKind::InvalidInput => "invalid input",
        OfficialErrorKind::UnsupportedOperation => "unsupported operation",
        OfficialErrorKind::LimitExceeded => "configured resource limit exceeded",
        OfficialErrorKind::RuntimeUnavailable => "official runtime unavailable",
        OfficialErrorKind::Upstream => "official parser operation failed",
        OfficialErrorKind::Deadline => "official operation deadline exceeded",
        OfficialErrorKind::Cancelled => "official operation cancelled",
        OfficialErrorKind::Transport => "official worker transport failed",
        _ => "official operation failed",
    }
}

pub(super) fn runtime(options: Option<NativeRuntimeOptions>) -> Result<OfficialRuntime> {
    let Some(options) = options else {
        return Ok(OfficialRuntime::new());
    };
    let mut runtime = OfficialRuntime::new();
    if let Some(path) = options.python {
        runtime = runtime.with_python(PathBuf::from(path));
    }
    if let Some(path) = options.config {
        runtime = runtime.with_config(PathBuf::from(path));
    }
    if let Some(path) = options.model_dir {
        runtime = runtime.with_model_dir(PathBuf::from(path));
    }
    if let Some(mode) = options.worker_mode {
        runtime = runtime.with_worker_mode(match mode.as_str() {
            "per-document" => mineru::command::OfficialWorkerMode::PerDocument,
            "persistent" => mineru::command::OfficialWorkerMode::Persistent,
            _ => {
                return Err(Error::from_reason(
                    "invalid_options: workerMode must be per-document or persistent",
                ));
            }
        });
    }
    if let Some(timeout) = options.timeout_ms {
        runtime = runtime.with_timeout(Duration::from_millis(u64::from(timeout)));
    }
    Ok(runtime)
}

pub(super) fn input(value: NativeInput) -> Result<OfficialInput> {
    match (value.path, value.filename, value.data) {
        (Some(path), None, None) => Ok(OfficialInput::path(path)),
        (None, Some(filename), Some(data)) => Ok(OfficialInput::bytes(filename, data.to_vec())),
        _ => Err(Error::from_reason(
            "invalid_input: provide a path or both filename and data",
        )),
    }
}

pub(super) fn parse_options(source: Option<String>) -> Result<OfficialParseOptions> {
    let Some(source) = source else {
        return Ok(OfficialParseOptions::new());
    };
    let value: Value = serde_json::from_str(&source)
        .map_err(|_| Error::from_reason("invalid_options: parse options must be JSON"))?;
    let object = value
        .as_object()
        .ok_or_else(|| Error::from_reason("invalid_options: parse options must be an object"))?;
    const ALLOWED: &[&str] = &[
        "tier",
        "ocr_mode",
        "image_analysis",
        "page_range",
        "source_context",
        "vlm_config",
    ];
    if object.keys().any(|key| !ALLOWED.contains(&key.as_str())) {
        return Err(Error::from_reason("invalid_options: unknown parse option"));
    }
    for (field, allowed) in [
        (
            "source_context",
            &["source_uri", "local_resource_root", "transport_encoding"][..],
        ),
        (
            "vlm_config",
            &[
                "engine",
                "server_url",
                "api_key",
                "model",
                "http_timeout",
                "max_concurrency",
            ][..],
        ),
    ] {
        if let Some(nested) = object.get(field).and_then(Value::as_object)
            && nested.keys().any(|key| !allowed.contains(&key.as_str()))
        {
            return Err(Error::from_reason(format!(
                "invalid_options: unknown {field} field"
            )));
        }
    }
    serde_json::from_value(value)
        .map_err(|_| Error::from_reason("invalid_options: invalid parse option value"))
}

pub(super) fn assets(values: Vec<NativeAsset>) -> Result<BTreeMap<String, bytes::Bytes>> {
    let mut assets = BTreeMap::new();
    for asset in values {
        if assets
            .insert(asset.path, bytes::Bytes::from(asset.data.to_vec()))
            .is_some()
        {
            return Err(Error::from_reason("invalid_input: duplicate asset path"));
        }
    }
    Ok(assets)
}

pub(super) fn render_format(value: &str) -> Result<OfficialRenderFormat> {
    match value {
        "markdown" => Ok(OfficialRenderFormat::Markdown),
        "html" => Ok(OfficialRenderFormat::Html),
        "latex" => Ok(OfficialRenderFormat::Latex),
        "docx" => Ok(OfficialRenderFormat::Docx),
        "epub" => Ok(OfficialRenderFormat::Epub),
        "structured_content" => Ok(OfficialRenderFormat::StructuredContent),
        "content_list" => Ok(OfficialRenderFormat::ContentList),
        "content_list_v2" => Ok(OfficialRenderFormat::ContentListV2),
        "pdf" => Ok(OfficialRenderFormat::Pdf),
        _ => Err(Error::from_reason(
            "invalid_options: unsupported render format",
        )),
    }
}

pub(super) fn render_options(source: Option<String>) -> Result<OfficialRenderOptions> {
    let Some(source) = source else {
        return Ok(OfficialRenderOptions::new());
    };
    let value: Value = serde_json::from_str(&source)
        .map_err(|_| Error::from_reason("invalid_options: render options must be JSON"))?;
    let object = value
        .as_object()
        .ok_or_else(|| Error::from_reason("invalid_options: render options must be an object"))?;
    const ALLOWED: &[&str] = &[
        "mode",
        "asset_base_url",
        "standalone",
        "document_title",
        "asset_base_path",
        "title",
        "authors",
        "language",
        "identifier",
        "modified_at",
        "layout",
        "latex_delimiters",
    ];
    if object.keys().any(|key| !ALLOWED.contains(&key.as_str())) {
        return Err(Error::from_reason("invalid_options: unknown render option"));
    }
    serde_json::from_value(value)
        .map_err(|_| Error::from_reason("invalid_options: invalid render option value"))
}

pub(super) fn asset_files(values: BTreeMap<String, bytes::Bytes>) -> Vec<NativeAsset> {
    values
        .into_iter()
        .map(|(path, data)| NativeAsset {
            path,
            data: Buffer::from(data.to_vec()),
        })
        .collect()
}
