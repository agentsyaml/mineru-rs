use reqwest::{Method, StatusCode, redirect::Policy};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, fmt, time::Duration};
use url::Url;

#[path = "client/support.rs"]
mod support;
use support::{
    http_error, read_limited, remaining_budget, remote_file_error, result_metadata, segment,
    transport, validate_page_range, validate_zip,
};

const INPUT_MAX: usize = 16 * 1024 * 1024;
const JOB_INPUT_MAX: usize = 64 * 1024 * 1024;
const OUTPUT_MAX: usize = 64 * 1024 * 1024;
const JSON_MAX: usize = 2 * 1024 * 1024;
const MAX_FILES: usize = 100;

#[derive(Clone, Debug, thiserror::Error)]
pub enum OfficialApiError {
    #[error("invalid client configuration: {0}")]
    Invalid(String),
    #[error("official API request failed: {0}")]
    Transport(String),
    #[error("official API returned HTTP {status}: {message}")]
    Http { status: StatusCode, message: String },
    #[error("official API response exceeded the configured safety limit")]
    TooLarge,
    #[error("official API protocol error: {0}")]
    Protocol(String),
    #[error("remote file error {error_type}/{code}: {message}")]
    File {
        error_type: String,
        code: String,
        message: String,
    },
}

pub type OfficialApiResult<T> = Result<T, OfficialApiError>;

/// `TASK_OUTPUT_FORMATS`: the formats the 4.2.3 task API composes.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OfficialOutputFormat {
    Default,
    Docx,
    Html,
    Latex,
}

impl OfficialOutputFormat {
    fn as_str(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::Docx => "docx",
            Self::Html => "html",
            Self::Latex => "latex",
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct OfficialRemoteParseOptions {
    pub tier: Option<crate::official::OfficialTier>,
    pub ocr_mode: Option<crate::official::OfficialOcrMode>,
    pub output_formats: Vec<OfficialOutputFormat>,
}

#[derive(Clone)]
pub struct OfficialApiClient {
    base: Url,
    token: Option<String>,
    timeout: Duration,
    http: reqwest::Client,
}

impl fmt::Debug for OfficialApiClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OfficialApiClient")
            .field("base", &self.base)
            .field("token", &self.token.as_ref().map(|_| "[redacted]"))
            .field("timeout", &self.timeout)
            .finish()
    }
}

/// `TaskUploadLink` from the create response.
#[derive(Clone, Debug, Deserialize)]
struct UploadLink {
    url: String,
    #[serde(default)]
    headers: BTreeMap<String, String>,
}

#[derive(Clone, Debug, Deserialize)]
struct CreatedFile {
    file_id: String,
    #[serde(default)]
    upload: Option<UploadLink>,
}

#[derive(Clone, Debug, Deserialize)]
pub(crate) struct CreatedTask {
    pub(crate) task_id: String,
    files: Vec<CreatedFile>,
}

impl OfficialApiClient {
    pub fn new(base_url: &str) -> OfficialApiResult<Self> {
        let base = Url::parse(base_url)
            .map_err(|_| OfficialApiError::Invalid("base URL must be absolute HTTP(S)".into()))?;
        if !matches!(base.scheme(), "http" | "https")
            || base.host_str().is_none()
            || !base.username().is_empty()
            || base.password().is_some()
            || base.query().is_some()
            || base.fragment().is_some()
        {
            return Err(OfficialApiError::Invalid(
                "base URL must be HTTP(S), without userinfo, query, or fragment".into(),
            ));
        }
        let http = reqwest::Client::builder()
            .no_proxy()
            .redirect(Policy::none())
            .timeout(Duration::from_secs(600))
            .build()
            .map_err(transport)?;
        Ok(Self {
            base,
            token: None,
            timeout: Duration::from_secs(600),
            http,
        })
    }

    pub fn with_token(mut self, token: impl Into<String>) -> OfficialApiResult<Self> {
        let token = token.into();
        if token.trim().is_empty() || token.contains(['\r', '\n']) {
            return Err(OfficialApiError::Invalid(
                "token must be nonempty and contain no line breaks".into(),
            ));
        }
        self.token = Some(token);
        Ok(self)
    }

    pub fn with_timeout(mut self, timeout: Duration) -> OfficialApiResult<Self> {
        if timeout.is_zero() {
            return Err(OfficialApiError::Invalid("timeout must be positive".into()));
        }
        self.timeout = timeout;
        self.http = reqwest::Client::builder()
            .no_proxy()
            .redirect(Policy::none())
            .timeout(timeout)
            .build()
            .map_err(transport)?;
        Ok(self)
    }

    fn url(&self, path: &str) -> OfficialApiResult<Url> {
        let url = self
            .base
            .join(path)
            .map_err(|_| OfficialApiError::Invalid("invalid endpoint URL".into()))?;
        self.check_origin(&url)?;
        Ok(url)
    }
    fn check_origin(&self, url: &Url) -> OfficialApiResult<()> {
        if url.scheme() != self.base.scheme()
            || url.host_str() != self.base.host_str()
            || url.port_or_known_default() != self.base.port_or_known_default()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.fragment().is_some()
        {
            return Err(OfficialApiError::Protocol(
                "follow-up URL is not same-origin".into(),
            ));
        }
        Ok(())
    }
    fn request(&self, method: Method, url: Url) -> reqwest::RequestBuilder {
        let mut req = self.http.request(method, url).timeout(self.timeout);
        if let Some(token) = &self.token {
            req = req.bearer_auth(token);
        }
        req
    }

    async fn send_json(
        &self,
        method: Method,
        url: Url,
        body: Option<Value>,
    ) -> OfficialApiResult<Value> {
        let mut request = self.request(method, url);
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = request.send().await.map_err(transport)?;
        let status = response.status();
        let bytes = read_limited(response, JSON_MAX).await?;
        if !status.is_success() {
            return Err(http_error(status, &bytes));
        }
        serde_json::from_slice(&bytes)
            .map_err(|_| OfficialApiError::Protocol("invalid JSON response".into()))
    }

    /// `POST /v1/tasks` with the JSON manifest; returns the 202 receipt.
    pub(crate) async fn create_task(
        &self,
        files: &[OfficialInput],
        options: &OfficialRemoteParseOptions,
    ) -> OfficialApiResult<CreatedTask> {
        if files.is_empty() || files.len() > MAX_FILES {
            return Err(OfficialApiError::Invalid(
                "task must contain 1..100 files".into(),
            ));
        }
        let mut total = 0usize;
        let mut manifest = Vec::with_capacity(files.len());
        for file in files {
            total = total
                .checked_add(file.bytes.len())
                .ok_or(OfficialApiError::TooLarge)?;
            if file.bytes.is_empty() || file.bytes.len() > INPUT_MAX || total > JOB_INPUT_MAX {
                return Err(OfficialApiError::TooLarge);
            }
            if crate::official_api::wire::validate_file_name(&file.filename).is_err() {
                return Err(OfficialApiError::Invalid("invalid file_name".into()));
            }
            let mut item = json!({
                "file_name": file.filename,
                "bytes": file.bytes.len(),
                "sha256sum": sha256_hex(&file.bytes),
            });
            if let Some(range) = &file.page_range {
                validate_page_range(range)?;
                item["page_range"] = json!(range);
            }
            manifest.push(item);
        }
        let mut body = json!({"files": manifest});
        if !options.output_formats.is_empty() {
            body["output_formats"] = json!(
                options
                    .output_formats
                    .iter()
                    .map(|format| format.as_str())
                    .collect::<Vec<_>>()
            );
        }
        if let Some(tier) = options.tier {
            body["tier"] = json!(tier.as_str());
        }
        if let Some(ocr) = options.ocr_mode {
            body["ocr_mode"] = json!(ocr.as_str());
        }
        let value = self
            .send_json(Method::POST, self.url("/v1/tasks")?, Some(body))
            .await?;
        let created: CreatedTask = serde_json::from_value(value)
            .map_err(|_| OfficialApiError::Protocol("invalid create response".into()))?;
        if created.files.len() != files.len()
            || created.files.iter().any(|file| file.file_id.is_empty())
        {
            return Err(OfficialApiError::Protocol(
                "create response omitted task files".into(),
            ));
        }
        Ok(created)
    }

    /// `PUT` one file to the server-provided upload link.
    async fn upload_file(&self, link: &UploadLink, data: &[u8]) -> OfficialApiResult<()> {
        let target = self
            .base
            .join(&link.url)
            .map_err(|_| OfficialApiError::Protocol("invalid upload URL".into()))?;
        self.check_origin(&target)?;
        let mut put = self.request(Method::PUT, target).body(data.to_vec());
        for (name, value) in &link.headers {
            if !name.eq_ignore_ascii_case("content-type") {
                return Err(OfficialApiError::Protocol(
                    "unsafe upload response header".into(),
                ));
            }
            put = put.header("content-type", value);
        }
        let response = put.send().await.map_err(transport)?;
        if !response.status().is_success() {
            return Err(http_error(
                response.status(),
                &read_limited(response, JSON_MAX).await?,
            ));
        }
        Ok(())
    }

    pub async fn get_task(&self, id: &str) -> OfficialApiResult<Value> {
        self.send_json(
            Method::GET,
            self.url(&format!("/v1/tasks/{}", segment(id)))?,
            None,
        )
        .await
    }

    pub async fn cancel_task(&self, id: &str) -> OfficialApiResult<Value> {
        self.send_json(
            Method::POST,
            self.url(&format!("/v1/tasks/{}/cancel", segment(id)))?,
            None,
        )
        .await
    }

    pub async fn get_file(&self, id: &str) -> OfficialApiResult<Value> {
        self.send_json(
            Method::GET,
            self.url(&format!("/v1/files/{}", segment(id)))?,
            None,
        )
        .await
    }

    pub async fn download_file(&self, id: &str) -> OfficialApiResult<Vec<u8>> {
        self.download_file_limited(id, OUTPUT_MAX).await
    }

    async fn download_file_limited(&self, id: &str, limit: usize) -> OfficialApiResult<Vec<u8>> {
        let url = self.url(&format!("/v1/files/{}/content", segment(id)))?;
        self.download(url, limit).await
    }

    /// Follow a `result_url` from the task detail, keeping it same-origin.
    pub(crate) async fn download_url(&self, raw: &str, limit: usize) -> OfficialApiResult<Vec<u8>> {
        let url = self
            .base
            .join(raw)
            .map_err(|_| OfficialApiError::Protocol("invalid result URL".into()))?;
        self.check_origin(&url)?;
        self.download(url, limit).await
    }

    async fn download(&self, url: Url, limit: usize) -> OfficialApiResult<Vec<u8>> {
        let response = self
            .request(Method::GET, url)
            .send()
            .await
            .map_err(transport)?;
        let status = response.status();
        let data = read_limited(response, limit).await?;
        if !status.is_success() {
            return Err(http_error(status, &data));
        }
        Ok(data)
    }
}

fn sha256_hex(data: &[u8]) -> String {
    Sha256::digest(data)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[derive(Clone, Debug)]
pub struct OfficialInput {
    pub filename: String,
    pub bytes: Vec<u8>,
    pub page_range: Option<String>,
}
impl OfficialInput {
    pub fn new(filename: impl Into<String>, bytes: impl Into<Vec<u8>>) -> Self {
        Self {
            filename: filename.into(),
            bytes: bytes.into(),
            page_range: None,
        }
    }
    pub fn with_page_range(mut self, range: impl Into<String>) -> Self {
        self.page_range = Some(range.into());
        self
    }
}

#[derive(Clone, Debug)]
pub struct OfficialArtifact {
    pub kind: String,
    pub file_id: String,
    pub bytes: Vec<u8>,
    pub metadata: Value,
}
#[derive(Clone, Debug)]
pub struct OfficialParseResult {
    pub metadata: Value,
    pub artifacts: Vec<OfficialArtifact>,
}
impl OfficialParseResult {
    /// `default` results arrive as a zip; read its markdown member.
    pub fn markdown(&self) -> Option<String> {
        for artifact in &self.artifacts {
            if artifact.kind == "markdown" {
                return std::str::from_utf8(&artifact.bytes).ok().map(str::to_owned);
            }
            if artifact.kind == "zip"
                && let Some(text) = zip_member(&artifact.bytes, "markdown.md")
                    .and_then(|b| String::from_utf8(b).ok())
            {
                return Some(text);
            }
        }
        None
    }
    pub fn json(&self, kind: &str) -> Option<OfficialApiResult<Value>> {
        for artifact in &self.artifacts {
            let bytes = if artifact.kind == kind {
                Some(artifact.bytes.clone())
            } else if artifact.kind == "zip" {
                zip_member(&artifact.bytes, &format!("{kind}.json"))
            } else {
                None
            };
            if let Some(bytes) = bytes {
                return Some(serde_json::from_slice(&bytes).map_err(|_| {
                    OfficialApiError::Protocol("artifact contains invalid JSON".into())
                }));
            }
        }
        None
    }
}

fn zip_member(bytes: &[u8], name: &str) -> Option<Vec<u8>> {
    use std::io::Read;
    let mut archive = zip::ZipArchive::new(std::io::Cursor::new(bytes)).ok()?;
    let mut entry = archive.by_name(name).ok()?;
    let mut data = Vec::new();
    entry.read_to_end(&mut data).ok()?;
    Some(data)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn archive() -> Vec<u8> {
        let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        for (name, body) in [
            ("markdown.md", "# title"),
            ("middle_json.json", "{\"a\":1}"),
        ] {
            writer
                .start_file(name, zip::write::SimpleFileOptions::default())
                .unwrap();
            writer.write_all(body.as_bytes()).unwrap();
        }
        writer.finish().unwrap().into_inner()
    }

    #[test]
    fn reads_markdown_and_json_out_of_the_default_archive() {
        let result = OfficialParseResult {
            metadata: Value::Null,
            artifacts: vec![OfficialArtifact {
                kind: "zip".into(),
                file_id: "file_1".into(),
                bytes: archive(),
                metadata: Value::Null,
            }],
        };
        assert_eq!(result.markdown().as_deref(), Some("# title"));
        let json = result.json("middle_json").unwrap().unwrap();
        assert_eq!(json["a"], 1);
        assert!(result.json("absent").is_none());
    }
}

#[path = "client/parse.rs"]
mod parse;
