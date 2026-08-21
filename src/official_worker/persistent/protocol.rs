use serde::{Deserialize, Deserializer, de::Error as DeError};
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
    #[serde(rename = "type", default = "handshake_frame_type")]
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
    #[serde(rename = "type", default = "result_frame_type")]
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
    #[serde(rename = "type", default = "error_frame_type")]
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

#[derive(Debug)]
pub(super) enum PersistentFrame {
    Handshake(PersistentHandshakeFrame),
    Result(PersistentResultFrame),
    Error(PersistentErrorFrame),
}

fn handshake_frame_type() -> String {
    "handshake".into()
}

fn result_frame_type() -> String {
    "result".into()
}

fn error_frame_type() -> String {
    "error".into()
}

const HANDSHAKE_FRAME_ERROR: &str = "__official_persistent_handshake_frame_error__: ";
const RESULT_FRAME_ERROR: &str = "__official_persistent_result_frame_error__: ";
const ERROR_FRAME_ERROR: &str = "__official_persistent_error_frame_error__: ";

fn deserialize_handshake_frame<'de, D>(
    deserializer: D,
) -> Result<PersistentHandshakeFrame, D::Error>
where
    D: Deserializer<'de>,
{
    PersistentHandshakeFrame::deserialize(deserializer)
        .map_err(|error| D::Error::custom(format!("{HANDSHAKE_FRAME_ERROR}{error}")))
}

fn deserialize_result_frame<'de, D>(deserializer: D) -> Result<PersistentResultFrame, D::Error>
where
    D: Deserializer<'de>,
{
    PersistentResultFrame::deserialize(deserializer)
        .map_err(|error| D::Error::custom(format!("{RESULT_FRAME_ERROR}{error}")))
}

fn deserialize_error_frame<'de, D>(deserializer: D) -> Result<PersistentErrorFrame, D::Error>
where
    D: Deserializer<'de>,
{
    PersistentErrorFrame::deserialize(deserializer)
        .map_err(|error| D::Error::custom(format!("{ERROR_FRAME_ERROR}{error}")))
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
enum PersistentFrameEnvelope {
    #[serde(rename = "handshake", deserialize_with = "deserialize_handshake_frame")]
    Handshake { frame: PersistentHandshakeFrame },
    #[serde(rename = "result", deserialize_with = "deserialize_result_frame")]
    Result { frame: PersistentResultFrame },
    #[serde(rename = "error", deserialize_with = "deserialize_error_frame")]
    Error { frame: PersistentErrorFrame },
    #[serde(other)]
    Unsupported,
}

pub(super) fn parse_persistent_frame(bytes: &[u8]) -> Result<PersistentFrame, String> {
    let envelope = match serde_json::from_slice::<PersistentFrameEnvelope>(bytes) {
        Ok(envelope) => envelope,
        Err(error) if error.is_syntax() || error.is_eof() => {
            return Err(format!(
                "official persistent protocol JSON is invalid: {error}"
            ));
        }
        Err(error) => {
            let message = error.to_string();
            if message.contains("missing field `type`") || message.starts_with("invalid type") {
                return Err("official persistent protocol frame type is missing".into());
            }
            if let Some(detail) = message.strip_prefix(HANDSHAKE_FRAME_ERROR) {
                return Err(format!(
                    "official persistent handshake is invalid: {detail}"
                ));
            }
            if let Some(detail) = message.strip_prefix(RESULT_FRAME_ERROR) {
                return Err(format!("official persistent result is invalid: {detail}"));
            }
            if let Some(detail) = message.strip_prefix(ERROR_FRAME_ERROR) {
                return Err(format!(
                    "official persistent error frame is invalid: {detail}"
                ));
            }
            return Err(format!(
                "official persistent protocol frame is invalid: {message}"
            ));
        }
    };
    match envelope {
        PersistentFrameEnvelope::Handshake { frame } => Ok(PersistentFrame::Handshake(frame)),
        PersistentFrameEnvelope::Result { frame } => Ok(PersistentFrame::Result(frame)),
        PersistentFrameEnvelope::Error { frame } => Ok(PersistentFrame::Error(frame)),
        PersistentFrameEnvelope::Unsupported => {
            Err("official persistent protocol frame type is unsupported".into())
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn handshake_frame() -> Value {
        json!({
            "type": "handshake",
            "protocol": "mineru-rs-official-worker/2",
            "status": "ready",
            "package_version": "4.0.0a6",
            "schema_version": "1.0",
            "backend": "hybrid-http-client",
            "max_in_flight": 1,
            "capabilities": {"bundle_name": "hybrid-v4"},
        })
    }

    fn result_frame() -> Value {
        json!({
            "type": "result",
            "protocol": "mineru-rs-official-worker/2",
            "request_id": "request-1",
            "sequence": 1,
            "status": "ok",
            "package_version": "4.0.0a6",
            "schema_version": "1.0",
            "backend": "hybrid-http-client",
            "bundle_name": "hybrid-v4",
        })
    }

    fn error_frame() -> Value {
        json!({
            "type": "error",
            "protocol": "mineru-rs-official-worker/2",
            "status": "error",
            "package_version": "4.0.0a6",
            "schema_version": "1.0",
            "backend": "hybrid-http-client",
            "bundle_name": "hybrid-v4",
            "error": "startup failed",
        })
    }

    fn parse(value: Value) -> Result<PersistentFrame, String> {
        parse_persistent_frame(&serde_json::to_vec(&value).unwrap())
    }

    #[test]
    fn parses_handshake_result_and_error_frames() {
        assert!(
            matches!(parse(handshake_frame()), Ok(PersistentFrame::Handshake(frame))
            if frame.frame_type == "handshake" && frame.max_in_flight == 1)
        );
        assert!(
            matches!(parse(result_frame()), Ok(PersistentFrame::Result(frame))
            if frame.frame_type == "result" && frame.request_id == "request-1")
        );
        assert!(
            matches!(parse(error_frame()), Ok(PersistentFrame::Error(frame))
            if frame.frame_type == "error" && frame.error == "startup failed")
        );
    }

    #[test]
    fn rejects_unknown_type_and_unknown_field() {
        let mut unknown_type = handshake_frame();
        unknown_type["type"] = json!("notice");
        assert_eq!(
            parse(unknown_type).unwrap_err(),
            "official persistent protocol frame type is unsupported"
        );

        let mut unknown_field = handshake_frame();
        unknown_field["unexpected"] = json!(true);
        let error = parse(unknown_field).unwrap_err();
        assert!(
            error.contains("official persistent handshake is invalid: unknown field `unexpected`")
        );
    }

    #[test]
    fn rejects_missing_type_and_malformed_json() {
        let mut missing_type = handshake_frame();
        missing_type.as_object_mut().unwrap().remove("type");
        assert_eq!(
            parse(missing_type).unwrap_err(),
            "official persistent protocol frame type is missing"
        );

        let error = parse_persistent_frame(br#"{"type":"handshake""#).unwrap_err();
        assert!(error.starts_with("official persistent protocol JSON is invalid: "));
    }

    #[cfg(unix)]
    fn value_with_encoded_len(target: usize) -> Value {
        let empty = serde_json::to_vec(&json!({"payload": ""})).unwrap();
        assert!(target > empty.len());
        json!({"payload": "x".repeat(target - empty.len())})
    }

    #[cfg(unix)]
    async fn read_from_cat(bytes: &[u8]) -> Result<Vec<u8>, String> {
        use std::process::Stdio;
        use tokio::process::Command;

        let mut child = Command::new("cat")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut stdin = child.stdin.take().unwrap();
        stdin.write_all(bytes).await.unwrap();
        drop(stdin);
        let stdout = child.stdout.take().unwrap();
        let result = read_persistent_frame(&mut BufReader::new(stdout)).await;
        child.wait().await.unwrap();
        result
    }

    #[cfg(unix)]
    async fn write_to_cat(frame: &Value) -> Result<(), String> {
        use std::process::Stdio;
        use tokio::process::Command;

        let mut child = Command::new("cat")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .spawn()
            .unwrap();
        let mut stdin = child.stdin.take().unwrap();
        let result = write_persistent_frame(&mut stdin, frame).await;
        drop(stdin);
        child.wait().await.unwrap();
        result
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn accepts_the_cap_and_rejects_one_byte_over_it() {
        let exact = serde_json::to_vec(&value_with_encoded_len(REQUEST_CAP - 1)).unwrap();
        assert_eq!(exact.len() + 1, REQUEST_CAP);
        let mut exact_with_newline = exact.clone();
        exact_with_newline.push(b'\n');
        assert_eq!(
            read_from_cat(&exact_with_newline).await.unwrap(),
            exact_with_newline
        );
        assert!(
            write_to_cat(&value_with_encoded_len(REQUEST_CAP - 1))
                .await
                .is_ok()
        );

        let over = serde_json::to_vec(&value_with_encoded_len(REQUEST_CAP)).unwrap();
        assert_eq!(over.len() + 1, REQUEST_CAP + 1);
        let mut over_with_newline = over.clone();
        over_with_newline.push(b'\n');
        assert_eq!(
            read_from_cat(&over_with_newline).await.unwrap_err(),
            "official persistent frame exceeds its limit"
        );
        assert_eq!(
            write_to_cat(&value_with_encoded_len(REQUEST_CAP))
                .await
                .unwrap_err(),
            "official persistent frame exceeds its limit"
        );
    }
}
