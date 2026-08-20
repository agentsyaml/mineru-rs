use serde::Deserialize;
use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{ChildStdin, ChildStdout},
};

use super::super::{
    OfficialRequest, OfficialSessionConfig, PERSISTENT_INPUT_FORMATS, PERSISTENT_MODEL_STACKS,
    PERSISTENT_PROTOCOL, REQUEST_CAP,
};

pub(super) fn persistent_capabilities() -> Value {
    json!({
        "efforts": super::super::PERSISTENT_EFFORTS,
        "model_stacks": PERSISTENT_MODEL_STACKS,
        "input_formats": PERSISTENT_INPUT_FORMATS,
        "bundle_name": crate::hybrid_v4_output::BUNDLE_NAME,
        "cancellation": "process-terminate",
    })
}

pub(super) fn validate_persistent_request(request: &OfficialRequest) -> Result<(), String> {
    if request.request_id.is_empty() {
        return Err("official persistent request id is empty".into());
    }
    if !super::super::PERSISTENT_EFFORTS.contains(&request.effort.as_str()) {
        return Err("official persistent effort is unsupported".into());
    }
    if request.effort != "medium"
        && !request
            .server_url
            .as_deref()
            .is_some_and(|url| url.starts_with("http://") || url.starts_with("https://"))
    {
        return Err("official persistent high/xhigh requests require an HTTP(S) server_url".into());
    }
    if request.method.is_empty() || request.lang.is_empty() {
        return Err("official persistent method and lang are required".into());
    }
    if request.page_range.as_deref().is_some_and(str::is_empty) {
        return Err("official persistent page_range must be nonempty".into());
    }
    if request.max_bundle_bytes == 0 {
        return Err("official persistent max_bundle_bytes must be positive".into());
    }
    if request.bundle_name != crate::hybrid_v4_output::BUNDLE_NAME {
        return Err("official persistent bundle name mismatch".into());
    }
    Ok(())
}

pub(super) fn persistent_start_frame(config: &OfficialSessionConfig) -> Value {
    json!({
        "type": "start",
        "protocol": PERSISTENT_PROTOCOL,
        "package_version": config.package_version,
        "schema_version": config.schema_version,
        "backend": config.backend,
        "model_stack": config.model_stack,
        "model_base_dir": config.model_base_dir,
        "config": config.config,
        "vl_api_key": config.vl_api_key,
        "vl_model_name": config.vl_model_name,
        "capabilities": persistent_capabilities(),
    })
}

pub(super) fn persistent_request_frame(
    request: &OfficialRequest,
    config: &OfficialSessionConfig,
    sequence: u64,
) -> Value {
    let mut frame = json!({
        "type": "request",
        "protocol": PERSISTENT_PROTOCOL,
        "request_id": request.request_id,
        "sequence": sequence,
        "package_version": config.package_version,
        "schema_version": config.schema_version,
        "backend": config.backend,
        "effort": request.effort,
        "server_url": request.server_url,
        "method": request.method,
        "lang": request.lang,
        "image_analysis": request.image_analysis,
        "bundle_name": crate::hybrid_v4_output::BUNDLE_NAME,
        "input_path": request.input_path,
        "bundle_path": request.bundle_path,
        "max_bundle_bytes": request.max_bundle_bytes,
    });
    if let Some(page_range) = &request.page_range {
        frame["page_range"] = Value::String(page_range.clone());
    }
    frame
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct PersistentHandshakeFrame {
    #[serde(rename = "type")]
    pub(super) frame_type: String,
    pub(super) protocol: String,
    pub(super) status: String,
    pub(super) package_version: String,
    pub(super) schema_version: String,
    pub(super) backend: String,
    pub(super) max_in_flight: u32,
    pub(super) capabilities: Value,
    pub(super) diagnostic: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct PersistentResultFrame {
    #[serde(rename = "type")]
    pub(super) frame_type: String,
    pub(super) protocol: String,
    pub(super) request_id: String,
    pub(super) sequence: u64,
    pub(super) status: String,
    pub(super) package_version: String,
    pub(super) schema_version: String,
    pub(super) backend: String,
    pub(super) bundle_name: String,
    pub(super) error: Option<String>,
    pub(super) diagnostic: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct PersistentErrorFrame {
    #[serde(rename = "type")]
    pub(super) frame_type: String,
    pub(super) protocol: String,
    pub(super) status: String,
    pub(super) package_version: String,
    pub(super) schema_version: String,
    pub(super) backend: String,
    pub(super) bundle_name: String,
    pub(super) error: String,
    pub(super) diagnostic: Option<String>,
}

pub(super) enum PersistentFrame {
    Handshake(PersistentHandshakeFrame),
    Result(PersistentResultFrame),
    Error(PersistentErrorFrame),
}

pub(super) fn parse_persistent_frame(bytes: &[u8]) -> Result<PersistentFrame, String> {
    let value: Value = serde_json::from_slice(bytes)
        .map_err(|error| format!("official persistent protocol JSON is invalid: {error}"))?;
    let frame_type = value
        .get("type")
        .and_then(Value::as_str)
        .ok_or_else(|| "official persistent protocol frame type is missing".to_owned())?;
    match frame_type {
        "handshake" => serde_json::from_value(value)
            .map(PersistentFrame::Handshake)
            .map_err(|error| format!("official persistent handshake is invalid: {error}")),
        "result" => serde_json::from_value(value)
            .map(PersistentFrame::Result)
            .map_err(|error| format!("official persistent result is invalid: {error}")),
        "error" => serde_json::from_value(value)
            .map(PersistentFrame::Error)
            .map_err(|error| format!("official persistent error frame is invalid: {error}")),
        _ => Err("official persistent protocol frame type is unsupported".into()),
    }
}

pub(super) fn validate_persistent_error(
    frame: &PersistentErrorFrame,
    config: &OfficialSessionConfig,
) -> Result<(), String> {
    if frame.frame_type != "error"
        || frame.protocol != PERSISTENT_PROTOCOL
        || frame.status != "error"
        || frame.package_version != config.package_version
        || frame.schema_version != config.schema_version
        || frame.backend != config.backend
        || frame.bundle_name != crate::hybrid_v4_output::BUNDLE_NAME
    {
        return Err("official persistent error frame mismatch".into());
    }
    Ok(())
}

pub(super) async fn write_persistent_frame(
    stdin: &mut ChildStdin,
    frame: &Value,
) -> Result<(), String> {
    let mut encoded = serde_json::to_vec(frame)
        .map_err(|error| format!("official persistent frame encode failed: {error}"))?;
    encoded.push(b'\n');
    if encoded.len() > REQUEST_CAP {
        return Err("official persistent frame exceeds its limit".into());
    }
    stdin
        .write_all(&encoded)
        .await
        .map_err(|error| format!("official persistent stdin failed: {error}"))?;
    stdin
        .flush()
        .await
        .map_err(|error| format!("official persistent stdin flush failed: {error}"))
}

pub(super) async fn read_persistent_frame(
    stdout: &mut BufReader<ChildStdout>,
) -> Result<Vec<u8>, String> {
    let mut frame = Vec::new();
    loop {
        let available = stdout
            .fill_buf()
            .await
            .map_err(|error| format!("official persistent stdout failed: {error}"))?;
        if available.is_empty() {
            if frame.is_empty() {
                return Err("official persistent worker stdout EOF".into());
            }
            return Err("official persistent frame has no newline".into());
        }
        if let Some(newline) = available.iter().position(|byte| *byte == b'\n') {
            let size = newline + 1;
            if frame.len() + size > REQUEST_CAP {
                return Err("official persistent frame exceeds its limit".into());
            }
            frame.extend_from_slice(&available[..size]);
            stdout.consume(size);
            return Ok(frame);
        }
        if frame.len() + available.len() > REQUEST_CAP {
            return Err("official persistent frame exceeds its limit".into());
        }
        frame.extend_from_slice(available);
        let size = available.len();
        stdout.consume(size);
    }
}
