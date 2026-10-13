use reqwest::{Method, StatusCode, redirect::Policy};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{fmt, time::Duration};
use url::Url;

#[path = "client/support.rs"]
mod support;
use support::{
    http_error, mime_for, read_limited, remaining_budget, remote_file_error, result_metadata,
    segment, transport, validate_page_range, validate_zip,
};

const INPUT_MAX: usize = 16 * 1024 * 1024;
const JOB_INPUT_MAX: usize = 64 * 1024 * 1024;
const OUTPUT_MAX: usize = 64 * 1024 * 1024;
const JSON_MAX: usize = 2 * 1024 * 1024;

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

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OfficialOutputFormat {
    Markdown,
    MiddleJson,
    StructuredContent,
    Zip,
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

    pub async fn create_upload(&self, filename: &str, bytes: &[u8]) -> OfficialApiResult<Value> {
        if bytes.is_empty() || bytes.len() > INPUT_MAX {
            return Err(OfficialApiError::Invalid("input must be 1..16 MiB".into()));
        }
        let hash = Sha256::digest(bytes)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        let mime = mime_for(filename);
        let created = self.send_json(Method::POST, self.url("/v1/uploads")?, Some(json!({"filename":filename,"bytes":bytes.len(),"mime_type":mime,"purpose":"parse","sha256sum":hash}))).await?;
        if created.get("status").and_then(Value::as_str) == Some("completed") {
            return Ok(created);
        }
        let raw = created
            .get("upload_url")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                OfficialApiError::Protocol("upload response omitted upload_url".into())
            })?;
        let target = self
            .base
            .join(raw)
            .map_err(|_| OfficialApiError::Protocol("invalid upload URL".into()))?;
        self.check_origin(&target)?;
        if created.get("upload_method").and_then(Value::as_str) != Some("PUT") {
            return Err(OfficialApiError::Protocol(
                "unsupported upload method".into(),
            ));
        }
        let mut put = self.request(Method::PUT, target).body(bytes.to_vec());
        if let Some(headers) = created.get("upload_headers").and_then(Value::as_object) {
            for (name, value) in headers {
                if !name.eq_ignore_ascii_case("content-type") {
                    return Err(OfficialApiError::Protocol(
                        "unsafe upload response header".into(),
                    ));
                }
                if let Some(value) = value.as_str() {
                    put = put.header("content-type", value);
                }
            }
        }
        let response = put.send().await.map_err(transport)?;
        if !response.status().is_success() {
            return Err(http_error(
                response.status(),
                &read_limited(response, JSON_MAX).await?,
            ));
        }
        let id = created
            .get("id")
            .and_then(Value::as_str)
            .ok_or_else(|| OfficialApiError::Protocol("upload response omitted id".into()))?;
        self.send_json(
            Method::POST,
            self.url(&format!("/v1/uploads/{}/complete", segment(id)))?,
            Some(json!({"sha256sum":hash})),
        )
        .await
    }

    pub async fn get_job(&self, id: &str) -> OfficialApiResult<Value> {
        self.send_json(
            Method::GET,
            self.url(&format!("/v1/parse/jobs/{}", segment(id)))?,
            None,
        )
        .await
    }
    pub async fn get_upload(&self, id: &str) -> OfficialApiResult<Value> {
        self.send_json(
            Method::GET,
            self.url(&format!("/v1/uploads/{}", segment(id)))?,
            None,
        )
        .await
    }
    pub async fn cancel_upload(&self, id: &str) -> OfficialApiResult<Value> {
        self.send_json(
            Method::POST,
            self.url(&format!("/v1/uploads/{}/cancel", segment(id)))?,
            None,
        )
        .await
    }
    pub async fn cancel_job(&self, id: &str) -> OfficialApiResult<Value> {
        self.send_json(
            Method::DELETE,
            self.url(&format!("/v1/parse/jobs/{}", segment(id)))?,
            None,
        )
        .await
    }

    pub async fn create_job(
        &self,
        files: &[OfficialInput],
        options: &OfficialRemoteParseOptions,
    ) -> OfficialApiResult<Value> {
        if files.is_empty() || files.len() > 100 {
            return Err(OfficialApiError::Invalid(
                "job must contain 1..100 files".into(),
            ));
        }
        let mut total = 0usize;
        for file in files {
            total = total
                .checked_add(file.bytes.len())
                .ok_or(OfficialApiError::TooLarge)?;
            if file.bytes.is_empty() || file.bytes.len() > INPUT_MAX || total > JOB_INPUT_MAX {
                return Err(OfficialApiError::TooLarge);
            }
            if file.filename.trim().is_empty() || file.filename.len() > 255 {
                return Err(OfficialApiError::Invalid("invalid filename".into()));
            }
            if let Some(range) = &file.page_range {
                validate_page_range(range)?;
            }
        }
        let formats: Vec<&str> = if options.output_formats.is_empty() {
            vec!["markdown"]
        } else {
            options
                .output_formats
                .iter()
                .map(|f| match f {
                    OfficialOutputFormat::Markdown => "markdown",
                    OfficialOutputFormat::MiddleJson => "middle_json",
                    OfficialOutputFormat::StructuredContent => "structured_content",
                    OfficialOutputFormat::Zip => "zip",
                })
                .collect()
        };
        let mut sources = Vec::with_capacity(files.len());
        for file in files {
            let upload = self.create_upload(&file.filename, &file.bytes).await?;
            let file_id = upload
                .pointer("/file/id")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    OfficialApiError::Protocol("completed upload omitted file.id".into())
                })?;
            let mut item = json!({"source":{"type":"file_id","file_id":file_id}});
            if let Some(range) = &file.page_range {
                item["page_range"] = json!(range);
            }
            sources.push(item);
        }
        let mut body = json!({"files":sources,"output_formats":formats});
        if let Some(tier) = options.tier {
            body["tier"] = json!(tier.as_str());
        }
        if let Some(ocr) = options.ocr_mode {
            body["ocr_mode"] = json!(ocr.as_str());
        }
        self.send_json(Method::POST, self.url("/v1/parse/jobs")?, Some(body))
            .await
    }
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
    pub fn markdown(&self) -> Option<&str> {
        self.artifacts
            .iter()
            .find(|a| a.kind == "markdown")
            .and_then(|a| std::str::from_utf8(&a.bytes).ok())
    }
    pub fn json(&self, kind: &str) -> Option<OfficialApiResult<Value>> {
        self.artifacts.iter().find(|a| a.kind == kind).map(|a| {
            serde_json::from_slice(&a.bytes)
                .map_err(|_| OfficialApiError::Protocol("artifact contains invalid JSON".into()))
        })
    }
}

impl OfficialApiClient {
    pub async fn list_jobs(&self, query: &[(&str, &str)]) -> OfficialApiResult<Value> {
        self.list("/v1/parse/jobs", query).await
    }
    pub async fn list_files(&self, query: &[(&str, &str)]) -> OfficialApiResult<Value> {
        self.list("/v1/files", query).await
    }
    async fn list(&self, path: &str, query: &[(&str, &str)]) -> OfficialApiResult<Value> {
        let mut url = self.url(path)?;
        url.query_pairs_mut().extend_pairs(query.iter().copied());
        self.send_json(Method::GET, url, None).await
    }
    pub async fn get_file(&self, id: &str) -> OfficialApiResult<Value> {
        self.send_json(
            Method::GET,
            self.url(&format!("/v1/files/{}", segment(id)))?,
            None,
        )
        .await
    }
    pub async fn delete_file(&self, id: &str) -> OfficialApiResult<Value> {
        self.send_json(
            Method::DELETE,
            self.url(&format!("/v1/files/{}", segment(id)))?,
            None,
        )
        .await
    }
    pub async fn download_file(&self, id: &str) -> OfficialApiResult<Vec<u8>> {
        self.download_file_limited(id, OUTPUT_MAX).await
    }
    async fn download_file_limited(&self, id: &str, limit: usize) -> OfficialApiResult<Vec<u8>> {
        let response = self
            .request(
                Method::GET,
                self.url(&format!("/v1/files/{}/content", segment(id)))?,
            )
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

#[path = "client/parse.rs"]
mod parse;
