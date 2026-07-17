use crate::{error::sanitize_vlm_error_bytes, *};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use bytes::Bytes;
use futures_util::{StreamExt, stream};
use image::ImageReader;
use reqwest::{Client, StatusCode, header::CONTENT_TYPE, redirect::Policy};
use serde_json::{Value, json};
use std::{
    io::Cursor,
    net::{IpAddr, SocketAddr},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, SystemTime},
};
use tokio::{io::AsyncReadExt, net::lookup_host, sync::Semaphore};
use url::Url;

/// Shared, monotonic byte allowance for one official document window.
#[derive(Debug)]
pub(crate) struct ByteBudget {
    cap: usize,
    used: AtomicUsize,
}

impl ByteBudget {
    pub(crate) fn new(cap: usize) -> Self {
        Self {
            cap,
            used: AtomicUsize::new(0),
        }
    }

    pub(crate) fn cap(&self) -> usize {
        self.cap
    }

    pub(crate) fn charge(&self, bytes: usize, resource: &'static str) -> VlmResult<()> {
        let result = self
            .used
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |used| {
                used.checked_add(bytes).filter(|total| *total <= self.cap)
            });
        result.map(|_| ()).map_err(|used| VlmError::LimitExceeded {
            resource,
            limit: self.cap as u64,
            actual: used.saturating_add(bytes) as u64,
        })
    }
}

struct FailFastBatch {
    inner: VlmBatchCompletionStream,
    failed: bool,
}
impl futures_core::Stream for FailFastBatch {
    type Item = VlmResult<(usize, String)>;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        if self.failed {
            return std::task::Poll::Ready(None);
        }
        match std::pin::Pin::new(&mut self.inner).poll_next(cx) {
            std::task::Poll::Ready(Some(item)) => {
                if item.is_err() {
                    self.failed = true;
                }
                std::task::Poll::Ready(Some(item))
            }
            other => other,
        }
    }
}

#[derive(Clone)]
pub struct VlmHttpClient {
    config: Arc<VlmHttpConfig>,
    http: Client,
    base: Url,
    model: String,
}
impl std::fmt::Debug for VlmHttpClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VlmHttpClient")
            .field("configured", &true)
            .finish()
    }
}

impl VlmHttpClient {
    pub async fn connect(config: VlmHttpConfig) -> VlmResult<Self> {
        if config.invalid_server_url {
            return Err(VlmError::InvalidConfig("invalid server URL".into()));
        }
        let base = config.server_url.clone().ok_or_else(|| {
            VlmError::InvalidConfig("MINERU_VL_SERVER or server_url is required".into())
        })?;
        if !valid_server(&base) {
            return Err(VlmError::InvalidConfig(
                "server_url must be a safe HTTP(S) URL".into(),
            ));
        }
        let mut builder = Client::builder()
            .no_proxy()
            .redirect(Policy::none())
            .timeout(config.http_timeout)
            .connect_timeout(config.connect_timeout)
            .pool_max_idle_per_host(config.max_keepalive_connections)
            .pool_idle_timeout(config.keepalive_expiry);
        if let Some(n) = config.max_connections {
            builder = builder.pool_max_idle_per_host(n);
        }
        let http = builder.build().map_err(|e| transport("connect", &e))?;
        let client = Self {
            config: Arc::new(config),
            http,
            base,
            model: String::new(),
        };
        let requested = client
            .config
            .model_name
            .clone()
            .map(|name| name.trim().to_owned())
            .filter(|name| !name.is_empty());
        let model = if let Some(name) = requested {
            if !client.config.skip_model_name_checking
                && !client.models().await?.iter().any(|model| model == &name)
            {
                return Err(VlmError::InvalidConfig(
                    "configured model was not returned by /v1/models".into(),
                ));
            }
            name
        } else {
            let models = client.models().await?;
            if models.len() == 1 && !models[0].trim().is_empty() {
                models[0].clone()
            } else {
                return Err(VlmError::InvalidConfig(format!(
                    "model_name is required unless /v1/models returns exactly one model{}",
                    model_candidates(&models)
                )));
            }
        };
        Ok(Self { model, ..client })
    }
    pub async fn predict(&self, request: VlmRequest) -> VlmResult<String> {
        self.complete(request, false).await
    }
    #[allow(dead_code)] // retained for the current official-route snapshot page
    pub(crate) async fn predict_capped(
        &self,
        request: VlmRequest,
        cap: usize,
        deadline: tokio::time::Instant,
    ) -> VlmResult<(String, usize)> {
        self.predict_official_budgeted(request, cap, None, deadline)
            .await
    }
    pub(crate) async fn predict_official_budgeted(
        &self,
        request: VlmRequest,
        cap: usize,
        budget: Option<Arc<ByteBudget>>,
        deadline: tokio::time::Instant,
    ) -> VlmResult<(String, usize)> {
        let config = self.config.clone();
        let model = self.model.clone();
        if tokio::time::Instant::now() >= deadline {
            return Err(VlmError::Timeout {
                operation: "official PDF",
            });
        }
        let body = tokio::time::timeout_at(
            deadline,
            tokio::task::spawn_blocking(move || official_body(config, model, request)),
        )
        .await
        .map_err(|_| VlmError::Timeout {
            operation: "official PDF",
        })?
        .map_err(|_| VlmError::Transport {
            operation: "chat",
            message: "body worker failed".into(),
        })??;
        let body = json_body(body, Some(deadline)).await?;
        tokio::time::timeout_at(
            deadline,
            self.complete_limited(body, cap, budget, Some(deadline)),
        )
        .await
        .map_err(|_| VlmError::Timeout {
            operation: "official PDF",
        })?
    }
    pub async fn aio_predict(&self, request: VlmRequest) -> VlmResult<String> {
        self.predict(request).await
    }
    pub async fn batch_predict(&self, requests: Vec<VlmRequest>) -> VlmResult<Vec<String>> {
        self.aio_batch_predict(requests, None).await
    }
    pub async fn aio_batch_predict(
        &self,
        requests: Vec<VlmRequest>,
        semaphore: VlmSemaphore,
    ) -> VlmResult<Vec<String>> {
        let limit = semaphore
            .unwrap_or_else(|| Arc::new(Semaphore::new(self.config.max_concurrency.max(1))));
        let n = limit.available_permits().max(1);
        let mut jobs = stream::iter(requests.into_iter().enumerate().map(|(i, r)| {
            let me = self.clone();
            let l = limit.clone();
            async move {
                let _permit = l.acquire_owned().await.map_err(|_| VlmError::Transport {
                    operation: "batch",
                    message: "semaphore closed".into(),
                })?;
                Ok((i, me.predict(r).await?))
            }
        }))
        .buffer_unordered(n);
        let mut out = Vec::new();
        while let Some(item) = jobs.next().await {
            out.push(item?)
        }
        out.sort_by_key(|x| x.0);
        Ok(out.into_iter().map(|x| x.1).collect())
    }
    pub fn stream_predict(&self, request: VlmRequest) -> VlmResult<VlmSseStream> {
        let (tx, s) = VlmSseStream::channel();
        let me = self.clone();
        std::thread::spawn(move || {
            let r = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|_| VlmError::Transport {
                    operation: "stream",
                    message: "runtime failed".into(),
                })
                .and_then(|rt| rt.block_on(me.sse(request, tx.clone())));
            if let Err(e) = r {
                let _ = tx.send(Err(e));
            }
        });
        Ok(s)
    }
    pub fn stream_test(&self, request: VlmRequest) -> VlmResult<()> {
        for x in self.stream_predict(request)? {
            x?;
        }
        Ok(())
    }
    pub async fn aio_batch_predict_as_iter(
        &self,
        requests: Vec<VlmRequest>,
        semaphore: VlmSemaphore,
    ) -> VlmResult<VlmBatchCompletionStream> {
        let l = semaphore
            .unwrap_or_else(|| Arc::new(Semaphore::new(self.config.max_concurrency.max(1))));
        let width = l.available_permits().max(1);
        let client = self.clone();
        let producer = stream::iter(requests.into_iter().enumerate().map(move |(i, r)| {
            let me = client.clone();
            let p = l.clone();
            async move {
                let _permit = p.acquire_owned().await.map_err(|_| VlmError::Transport {
                    operation: "batch",
                    message: "semaphore closed".into(),
                })?;
                me.predict(r).await.map(|v| (i, v))
            }
        }))
        .buffer_unordered(width);
        // Keep the public stream type while scheduling only `width` requests at once.
        Ok(Box::pin(FailFastBatch {
            inner: Box::pin(producer),
            failed: false,
        }))
    }
    pub async fn predict_scored(&self, _: VlmRequest) -> VlmResult<VlmScoredOutput> {
        unsupported()
    }
    pub async fn batch_predict_scored(
        &self,
        _: Vec<VlmRequest>,
    ) -> VlmResult<Vec<VlmScoredOutput>> {
        unsupported()
    }
    pub async fn aio_predict_scored(&self, _: VlmRequest) -> VlmResult<VlmScoredOutput> {
        unsupported()
    }
    pub async fn aio_batch_predict_scored(
        &self,
        _: Vec<VlmRequest>,
        _: VlmSemaphore,
    ) -> VlmResult<Vec<VlmScoredOutput>> {
        unsupported()
    }
    pub async fn score(&self, _: VlmRequest, _: String) -> VlmResult<VlmScoredOutput> {
        unsupported()
    }
    pub async fn batch_score(
        &self,
        _: Vec<VlmRequest>,
        _: Vec<String>,
    ) -> VlmResult<Vec<VlmScoredOutput>> {
        unsupported()
    }
    pub async fn aio_score(&self, _: VlmRequest, _: String) -> VlmResult<VlmScoredOutput> {
        unsupported()
    }
    pub async fn aio_batch_score(
        &self,
        _: Vec<VlmRequest>,
        _: Vec<String>,
        _: VlmSemaphore,
    ) -> VlmResult<Vec<VlmScoredOutput>> {
        unsupported()
    }
    async fn models(&self) -> VlmResult<Vec<String>> {
        let v = self.send_json("models", self.url("models")?, None).await?;
        let mut models = v
            .get("data")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|x| x.get("id").and_then(Value::as_str))
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .map(str::to_owned)
            .collect::<Vec<_>>();
        models.sort_unstable();
        models.dedup();
        Ok(models)
    }
    async fn complete(&self, r: VlmRequest, streaming: bool) -> VlmResult<String> {
        let body = json_body(self.body(r, streaming).await?, None).await?;
        self.complete_limited(body, self.config.max_response_bytes, None, None)
            .await
            .map(|x| x.0)
    }
    async fn complete_limited(
        &self,
        body: Bytes,
        cap: usize,
        budget: Option<Arc<ByteBudget>>,
        deadline: Option<tokio::time::Instant>,
    ) -> VlmResult<(String, usize)> {
        let v = self
            .send_json_limited(
                "chat",
                self.url("chat/completions")?,
                Some(body),
                cap,
                budget,
                deadline,
            )
            .await?;
        let (v, bytes) = v;
        let text = if deadline.is_some() {
            let allow_truncated_content = self.config.allow_truncated_content;
            json_worker(deadline, "chat", move || {
                completion_text(v, allow_truncated_content)
            })
            .await?
        } else {
            completion_text(v, self.config.allow_truncated_content)?
        };
        Ok((text, bytes))
    }
    fn url(&self, suffix: &str) -> VlmResult<Url> {
        let mut u = self.base.clone();
        let prefix = if u.path().ends_with('/') && u.path() != "/" {
            u.path().trim_end_matches('/')
        } else {
            ""
        };
        u.set_path(&format!("{prefix}/v1/{suffix}"));
        Ok(u)
    }
    fn headers(&self, mut r: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        for h in &self.config.headers {
            if !h.name().eq_ignore_ascii_case("authorization") {
                r = r.header(h.name(), h.value());
            }
        }
        if let Some(a) = self.config.authorization() {
            r = r.header("authorization", a)
        }
        r
    }
    async fn send_json(&self, op: &'static str, url: Url, body: Option<Bytes>) -> VlmResult<Value> {
        self.send_json_limited(op, url, body, self.config.max_response_bytes, None, None)
            .await
            .map(|x| x.0)
    }
    async fn send_json_limited(
        &self,
        op: &'static str,
        url: Url,
        body: Option<Bytes>,
        cap: usize,
        budget: Option<Arc<ByteBudget>>,
        deadline: Option<tokio::time::Instant>,
    ) -> VlmResult<(Value, usize)> {
        let mut attempt = 0;
        loop {
            let r = if let Some(b) = &body {
                self.headers(
                    self.http
                        .post(url.clone())
                        .header(CONTENT_TYPE, "application/json")
                        .body(b.clone()),
                )
            } else {
                self.headers(self.http.get(url.clone()))
            }
            .send()
            .await;
            match r {
                Ok(r) => {
                    if r.status().is_redirection() {
                        return Err(VlmError::Redirect(op.into()));
                    }
                    if !r.status().is_success() {
                        let retry = retry_status(r.status());
                        let status = r.status().as_u16();
                        let wait = retry_after(&r);
                        let bytes = read_limited(r, self.config.max_diagnostic_bytes, "diagnostic")
                            .await
                            .unwrap_or_default();
                        if retry && attempt < self.config.max_retries {
                            retry_wait(attempt, self.config.retry_backoff_factor, wait).await;
                            attempt += 1;
                            continue;
                        }
                        return Err(VlmError::Http {
                            operation: op,
                            status,
                            body: sanitize_vlm_error_bytes(
                                &bytes,
                                self.config.max_diagnostic_bytes,
                            ),
                        });
                    }
                    let b = read_limited_budgeted(r, cap, "response", budget.as_deref()).await?;
                    let bytes = b.len();
                    return json_response(b, op, deadline)
                        .await
                        .map(|value| (value, bytes));
                }
                Err(e) => {
                    if retry_error(&e) && attempt < self.config.max_retries {
                        retry_wait(attempt, self.config.retry_backoff_factor, None).await;
                        attempt += 1
                    } else {
                        return Err(transport(op, &e));
                    }
                }
            }
        }
    }
    async fn body(&self, r: VlmRequest, streaming: bool) -> VlmResult<Value> {
        let VlmRequest {
            images: inputs,
            prompt,
            sampling,
            priority,
        } = r;
        if inputs.len() > self.config.max_images_per_request {
            return Err(VlmError::LimitExceeded {
                resource: "images",
                limit: self.config.max_images_per_request as u64,
                actual: inputs.len() as u64,
            });
        }
        let mut images = Vec::new();
        for input in inputs {
            if let Some(image) = self.image(input).await? {
                images.push(image);
            }
        }
        Ok(build_body(
            &self.config,
            &self.model,
            prompt,
            sampling,
            priority,
            streaming,
            images,
        ))
    }
    async fn image(&self, input: VlmImageInput) -> VlmResult<Option<(Vec<u8>, String)>> {
        let (bytes, hint) = match input {
            VlmImageInput::None => return Ok(None),
            VlmImageInput::Path(p) => {
                (read_image_file(p, self.config.max_image_bytes).await?, None)
            }
            VlmImageInput::Bytes { data, media_type } => {
                if data.len() > self.config.max_image_bytes {
                    return Err(limit(
                        "image bytes",
                        self.config.max_image_bytes,
                        data.len(),
                    ));
                }
                (data.to_vec(), media_type)
            }
            VlmImageInput::Base64 { data, media_type } => {
                base64_fits(&data, self.config.max_image_bytes)?;
                (
                    STANDARD
                        .decode(data)
                        .map_err(|_| VlmError::InvalidImageInput("invalid base64".into()))?,
                    media_type,
                )
            }
            VlmImageInput::DataUrl(s) => data_url(&s, self.config.max_image_bytes)?,
            VlmImageInput::RemoteUrl(u) => (self.remote(u).await?, None),
        };
        if bytes.len() > self.config.max_image_bytes {
            return Err(limit(
                "image bytes",
                self.config.max_image_bytes,
                bytes.len(),
            ));
        }
        inspect_image(bytes, hint, &self.config).map(Some)
    }
    async fn remote(&self, mut url: Url) -> VlmResult<Vec<u8>> {
        if !self.config.allow_remote_images {
            return Err(VlmError::InvalidImageInput(
                "remote images are disabled".into(),
            ));
        }
        for redirects in 0..=self.config.max_redirects {
            let addrs = remote_addrs(&url, self.config.allow_private_remote_images).await?;
            let host = url
                .host_str()
                .ok_or_else(|| VlmError::InvalidImageInput("remote URL missing host".into()))?;
            let client = Client::builder()
                .no_proxy()
                .redirect(Policy::none())
                .resolve_to_addrs(host, &addrs)
                .timeout(self.config.http_timeout)
                .build()
                .map_err(|e| transport("image", &e))?;
            let r = client
                .get(url.clone())
                .send()
                .await
                .map_err(|e| transport("image", &e))?;
            if r.status().is_redirection() {
                if !matches!(
                    r.status(),
                    StatusCode::MOVED_PERMANENTLY
                        | StatusCode::FOUND
                        | StatusCode::SEE_OTHER
                        | StatusCode::TEMPORARY_REDIRECT
                        | StatusCode::PERMANENT_REDIRECT
                ) || redirects == self.config.max_redirects
                {
                    return Err(VlmError::Redirect("image".into()));
                }
                let next = r
                    .headers()
                    .get(reqwest::header::LOCATION)
                    .and_then(|x| x.to_str().ok())
                    .ok_or_else(|| VlmError::Redirect("image".into()))?;
                url = url
                    .join(next)
                    .map_err(|_| VlmError::Redirect("image".into()))?;
                continue;
            }
            if !r.status().is_success() {
                return Err(VlmError::Http {
                    operation: "image",
                    status: r.status().as_u16(),
                    body: String::new(),
                });
            }
            return read_limited(r, self.config.max_image_bytes, "image bytes").await;
        }
        Err(VlmError::Redirect("image redirects exceeded".into()))
    }
    async fn sse(
        &self,
        r: VlmRequest,
        tx: std::sync::mpsc::Sender<VlmResult<String>>,
    ) -> VlmResult<()> {
        let body = json_body(self.body(r, true).await?, None).await?;
        let mut attempt = 0;
        let mut response = loop {
            match self
                .headers(
                    self.http
                        .post(self.url("chat/completions")?)
                        .header(CONTENT_TYPE, "application/json")
                        .body(body.clone()),
                )
                .send()
                .await
            {
                Ok(x) if x.status().is_success() => break x,
                Ok(x) => {
                    let retry = retry_status(x.status());
                    let wait = retry_after(&x);
                    if retry && attempt < self.config.max_retries {
                        retry_wait(attempt, self.config.retry_backoff_factor, wait).await;
                        attempt += 1;
                        continue;
                    }
                    return Err(VlmError::Http {
                        operation: "stream",
                        status: x.status().as_u16(),
                        body: sanitize_vlm_error_bytes(
                            &read_limited(x, self.config.max_diagnostic_bytes, "diagnostic")
                                .await
                                .unwrap_or_default(),
                            self.config.max_diagnostic_bytes,
                        ),
                    });
                }
                Err(e) if retry_error(&e) && attempt < self.config.max_retries => {
                    retry_wait(attempt, self.config.retry_backoff_factor, None).await;
                    attempt += 1
                }
                Err(e) => return Err(transport("stream", &e)),
            }
        };
        if response
            .content_length()
            .is_some_and(|n| n > self.config.max_response_bytes as u64)
        {
            return Err(VlmError::LimitExceeded {
                resource: "response",
                limit: self.config.max_response_bytes as u64,
                actual: response.content_length().unwrap_or(0),
            });
        }
        let mut wire = 0;
        let mut assembled = 0;
        let mut pending = Vec::new();
        let mut event = Vec::new();
        let mut done = false;
        let mut terminal = false;
        while let Some(c) = response
            .chunk()
            .await
            .map_err(|e| transport("stream", &e))?
        {
            wire += c.len();
            if wire > self.config.max_response_bytes {
                return Err(limit("response", self.config.max_response_bytes, wire));
            }
            pending.extend_from_slice(&c);
            while let Some(n) = pending.iter().position(|b| *b == b'\n') {
                let mut line = pending.drain(..=n).collect::<Vec<_>>();
                if line.last() == Some(&b'\n') {
                    line.pop();
                }
                if line.last() == Some(&b'\r') {
                    line.pop();
                }
                sse_line(
                    &line,
                    &mut event,
                    &tx,
                    &mut assembled,
                    self.config.max_response_bytes,
                    &mut done,
                    &mut terminal,
                    self.config.allow_truncated_content,
                )?;
                if done {
                    return Ok(());
                }
            }
        }
        if !pending.is_empty() {
            sse_line(
                &pending,
                &mut event,
                &tx,
                &mut assembled,
                self.config.max_response_bytes,
                &mut done,
                &mut terminal,
                self.config.allow_truncated_content,
            )?;
        }
        if !event.is_empty() {
            sse_event(
                &mut event,
                &tx,
                &mut assembled,
                self.config.max_response_bytes,
                &mut done,
                &mut terminal,
                self.config.allow_truncated_content,
            )?;
        }
        if done {
            Ok(())
        } else {
            Err(protocol("stream", "SSE stream ended without [DONE]"))
        }
    }
}
fn completion_text(mut response: Value, allow_truncated_content: bool) -> VlmResult<String> {
    if response.get("error").is_some()
        || response.get("object").and_then(Value::as_str) == Some("error")
    {
        return Err(VlmError::Protocol {
            operation: "chat",
            message: "error object in successful response".into(),
        });
    }
    let choice = response
        .get_mut("choices")
        .and_then(Value::as_array_mut)
        .and_then(|choices| choices.first_mut())
        .ok_or_else(|| protocol("chat", "missing choices"))?;
    let finish = choice
        .get("finish_reason")
        .and_then(Value::as_str)
        .ok_or_else(|| protocol("chat", "missing finish reason"))?;
    if finish != "stop" && !(finish == "length" && allow_truncated_content) {
        return Err(protocol("chat", "unexpected finish reason"));
    }
    let content = choice
        .get_mut("message")
        .and_then(|message| message.get_mut("content"))
        .ok_or_else(|| protocol("chat", "missing string content"))?;
    match std::mem::replace(content, Value::Null) {
        Value::String(text) => Ok(strip_end(text)),
        Value::Null => Ok(String::new()),
        _ => Err(protocol("chat", "missing string content")),
    }
}
fn strip_end(mut text: String) -> String {
    let token = std::env::var("MINERU_VLM_END_TOKEN").unwrap_or_else(|_| "<|im_end|>".into());
    if !token.is_empty() && text.ends_with(&token) {
        text.truncate(text.len() - token.len());
    }
    text
}
fn valid_server(u: &Url) -> bool {
    matches!(u.scheme(), "http" | "https")
        && u.host_str().is_some()
        && u.username().is_empty()
        && u.password().is_none()
        && u.query().is_none()
        && u.fragment().is_none()
}
fn model_candidates(models: &[String]) -> String {
    const MAX_BYTES: usize = 256;
    if models.is_empty() {
        return " (/v1/models returned no usable models)".into();
    }
    let mut out = " (candidates: ".to_owned();
    for (index, model) in models.iter().take(8).enumerate() {
        if index != 0 {
            out.push_str(", ");
        }
        for character in model.chars() {
            let character = if character.is_control() {
                '?'
            } else {
                character
            };
            if out.len() + character.len_utf8() + 1 >= MAX_BYTES {
                break;
            }
            out.push(character);
        }
        if out.len() + 1 >= MAX_BYTES {
            break;
        }
    }
    if models.len() > 8 && out.len() + 5 < MAX_BYTES {
        out.push_str(", ...");
    }
    out.push(')');
    out
}
fn overlay(mut a: SamplingParams, b: SamplingParams) -> SamplingParams {
    macro_rules! o {
        ($x:ident) => {
            if b.$x.is_some() {
                a.$x = b.$x
            }
        };
    }
    o!(temperature);
    o!(top_p);
    o!(top_k);
    o!(presence_penalty);
    o!(frequency_penalty);
    o!(repetition_penalty);
    o!(no_repeat_ngram_size);
    o!(max_new_tokens);
    a
}
fn put<T: serde::Serialize>(o: &mut serde_json::Map<String, Value>, k: &str, v: Option<T>) {
    if let Some(v) = v {
        o.insert(k.into(), json!(v));
    }
}
fn build_body(
    config: &VlmHttpConfig,
    model: &str,
    prompt: Option<String>,
    sampling: Option<SamplingParams>,
    priority: VlmPriority,
    streaming: bool,
    images: Vec<(Vec<u8>, String)>,
) -> Value {
    let images = images.into_iter().map(|(bytes, media)| {
        json!({"type":"image_url","image_url":{"url":format!("data:{media};base64,{}", STANDARD.encode(bytes))}})
    });
    let prompt = prompt
        .filter(|text| !text.is_empty())
        .or_else(|| config.prompt.clone())
        .filter(|text| !text.is_empty())
        .unwrap_or_else(|| "What is the text in the illustrate?".into());
    let mut content = Vec::new();
    if prompt.contains("<image>") {
        let mut rest = images;
        for part in prompt.splitn(rest.len() + 1, "<image>") {
            if !part.is_empty() {
                content.push(json!({"type":"text","text":part}));
            }
            if let Some(image) = rest.next() {
                content.push(image);
            }
        }
        content.extend(rest);
    } else if config.text_before_image {
        content.push(json!({"type":"text","text":prompt}));
        content.extend(images);
    } else {
        content.extend(images);
        content.push(json!({"type":"text","text":prompt}));
    }
    let mut messages = Vec::new();
    if let Some(system) = config
        .system_prompt
        .clone()
        .unwrap_or_else(|| "You are a helpful assistant.".into())
        .strip_prefix("")
        .filter(|text| !text.is_empty())
    {
        messages.push(json!({"role":"system","content":system}));
    }
    messages.push(json!({"role":"user","content":content}));
    let gpt = model.to_ascii_lowercase().starts_with("gpt");
    let mut body = json!({"model":model,"messages":messages});
    if !gpt {
        body["skip_special_tokens"] = json!(false);
    }
    if streaming {
        body["stream"] = json!(true);
    }
    let sampling = overlay(
        config.sampling_params.clone().unwrap_or_default(),
        sampling.unwrap_or_default(),
    );
    let values = body.as_object_mut().expect("json object");
    put(values, "temperature", sampling.temperature);
    put(values, "top_p", sampling.top_p);
    put(values, "presence_penalty", sampling.presence_penalty);
    put(values, "frequency_penalty", sampling.frequency_penalty);
    if !gpt {
        put(values, "top_k", sampling.top_k);
        put(values, "repetition_penalty", sampling.repetition_penalty);
    }
    if let Some(value) = sampling.no_repeat_ngram_size {
        values.insert(
            "vllm_xargs".into(),
            json!({"no_repeat_ngram_size":value,"debug":config.debug}),
        );
    }
    if let Some(value) = sampling.max_new_tokens {
        values.insert("max_completion_tokens".into(), json!(value));
        values.insert("max_tokens".into(), json!(value));
    }
    if let Some(value) = priority {
        values.insert("priority".into(), json!(value));
    }
    body
}

fn official_body(config: Arc<VlmHttpConfig>, model: String, r: VlmRequest) -> VlmResult<Value> {
    let VlmRequest {
        images: inputs,
        prompt,
        sampling,
        priority,
    } = r;
    if inputs.len() > config.max_images_per_request {
        return Err(limit("images", config.max_images_per_request, inputs.len()));
    }
    let mut images = Vec::new();
    for image in inputs {
        let (bytes, hint) = match image {
            VlmImageInput::Bytes { data, media_type } => (data.to_vec(), media_type),
            VlmImageInput::Base64 { data, media_type } => {
                base64_fits(&data, config.max_image_bytes)?;
                (
                    STANDARD
                        .decode(data)
                        .map_err(|_| VlmError::InvalidImageInput("invalid base64".into()))?,
                    media_type,
                )
            }
            VlmImageInput::DataUrl(data) => data_url(&data, config.max_image_bytes)?,
            VlmImageInput::None => continue,
            _ => {
                return Err(VlmError::InvalidImageInput(
                    "official request requires a local image".into(),
                ));
            }
        };
        if bytes.len() > config.max_image_bytes {
            return Err(limit("image bytes", config.max_image_bytes, bytes.len()));
        }
        images.push(inspect_image(bytes, hint, &config)?);
    }
    Ok(build_body(
        &config, &model, prompt, sampling, priority, false, images,
    ))
}

async fn json_body(body: Value, deadline: Option<tokio::time::Instant>) -> VlmResult<Bytes> {
    json_worker(deadline, "chat", move || {
        serde_json::to_vec(&body)
            .map(Bytes::from)
            .map_err(|_| protocol("chat", "request JSON serialization failed"))
    })
    .await
}

async fn json_response(
    body: Vec<u8>,
    operation: &'static str,
    deadline: Option<tokio::time::Instant>,
) -> VlmResult<Value> {
    json_worker(deadline, operation, move || {
        serde_json::from_slice(&body).map_err(|_| protocol(operation, "invalid JSON response"))
    })
    .await
}

async fn json_worker<T: Send + 'static>(
    deadline: Option<tokio::time::Instant>,
    operation: &'static str,
    job: impl FnOnce() -> VlmResult<T> + Send + 'static,
) -> VlmResult<T> {
    let worker = tokio::task::spawn_blocking(job);
    let result = if let Some(deadline) = deadline {
        tokio::time::timeout_at(deadline, worker)
            .await
            .map_err(|_| VlmError::Timeout {
                operation: "official PDF",
            })?
    } else {
        worker.await
    };
    result.map_err(|_| VlmError::Transport {
        operation,
        message: "JSON worker failed".into(),
    })?
}

fn inspect_image(
    bytes: Vec<u8>,
    hint: Option<String>,
    config: &VlmHttpConfig,
) -> VlmResult<(Vec<u8>, String)> {
    let reader = ImageReader::new(Cursor::new(&bytes))
        .with_guessed_format()
        .map_err(|_| VlmError::InvalidImageInput("unsupported image".into()))?;
    let format = reader
        .format()
        .ok_or_else(|| VlmError::InvalidImageInput("unsupported image".into()))?;
    let media =
        mime(format).ok_or_else(|| VlmError::InvalidImageInput("unsupported image".into()))?;
    if hint
        .as_deref()
        .is_some_and(|value| !value.eq_ignore_ascii_case(media))
    {
        return Err(VlmError::InvalidImageInput(
            "image media type mismatch".into(),
        ));
    }
    let (width, height) = reader
        .into_dimensions()
        .map_err(|_| VlmError::InvalidImageInput("invalid image".into()))?;
    let pixels = width as u64 * height as u64;
    if pixels > config.max_decoded_pixels {
        return Err(VlmError::LimitExceeded {
            resource: "image pixels",
            limit: config.max_decoded_pixels,
            actual: pixels,
        });
    }
    Ok((bytes, media.into()))
}
fn protocol(op: &'static str, msg: &str) -> VlmError {
    VlmError::Protocol {
        operation: op,
        message: msg.into(),
    }
}
fn limit(resource: &'static str, limit: usize, actual: usize) -> VlmError {
    VlmError::LimitExceeded {
        resource,
        limit: limit as u64,
        actual: actual as u64,
    }
}
fn mime(f: image::ImageFormat) -> Option<&'static str> {
    match f {
        image::ImageFormat::Jpeg => Some("image/jpeg"),
        image::ImageFormat::Png => Some("image/png"),
        image::ImageFormat::Gif => Some("image/gif"),
        image::ImageFormat::Bmp => Some("image/bmp"),
        image::ImageFormat::WebP => Some("image/webp"),
        image::ImageFormat::Tiff => Some("image/tiff"),
        _ => None,
    }
}
fn base64_fits(data: &str, cap: usize) -> VlmResult<()> {
    let estimate = (data
        .len()
        .checked_add(3)
        .ok_or_else(|| limit("image bytes", cap, usize::MAX))?
        / 4)
    .checked_mul(3)
    .ok_or_else(|| limit("image bytes", cap, usize::MAX))?;
    if estimate > cap {
        return Err(limit("image bytes", cap, estimate));
    }
    Ok(())
}
async fn read_image_file(path: std::path::PathBuf, cap: usize) -> VlmResult<Vec<u8>> {
    let metadata = tokio::fs::metadata(&path).await.map_err(|_| VlmError::Io {
        operation: "image",
        message: "read failed".into(),
    })?;
    if metadata.len() > cap as u64 {
        return Err(VlmError::LimitExceeded {
            resource: "image bytes",
            limit: cap as u64,
            actual: metadata.len(),
        });
    }
    let file = tokio::fs::File::open(path)
        .await
        .map_err(|_| VlmError::Io {
            operation: "image",
            message: "read failed".into(),
        })?;
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take(cap as u64 + 1)
        .read_to_end(&mut bytes)
        .await
        .map_err(|_| VlmError::Io {
            operation: "image",
            message: "read failed".into(),
        })?;
    if bytes.len() > cap {
        return Err(limit("image bytes", cap, bytes.len()));
    }
    Ok(bytes)
}
fn data_url(s: &str, cap: usize) -> VlmResult<(Vec<u8>, Option<String>)> {
    let (m, d) = s
        .strip_prefix("data:")
        .and_then(|x| x.split_once(','))
        .ok_or_else(|| VlmError::InvalidImageInput("invalid data URL".into()))?;
    let h = m
        .strip_suffix(";base64")
        .ok_or_else(|| VlmError::InvalidImageInput("data URL must be base64".into()))?;
    base64_fits(d, cap)?;
    Ok((
        STANDARD
            .decode(d)
            .map_err(|_| VlmError::InvalidImageInput("invalid data URL".into()))?,
        Some(h.into()),
    ))
}
async fn remote_addrs(u: &Url, allow_private: bool) -> VlmResult<Vec<SocketAddr>> {
    if !matches!(u.scheme(), "http" | "https") || !u.username().is_empty() || u.password().is_some()
    {
        return Err(VlmError::InvalidImageInput(
            "remote URL must be safe HTTP(S)".into(),
        ));
    }
    let host = u
        .host_str()
        .ok_or_else(|| VlmError::InvalidImageInput("remote URL missing host".into()))?;
    let port = u
        .port_or_known_default()
        .ok_or_else(|| VlmError::InvalidImageInput("remote URL invalid port".into()))?;
    let a: Vec<_> = lookup_host((host, port))
        .await
        .map_err(|_| VlmError::InvalidImageInput("remote host resolution failed".into()))?
        .collect();
    if a.is_empty() || (!allow_private && a.iter().any(|x| !global(x.ip()))) {
        return Err(VlmError::InvalidImageInput(
            "private remote URL rejected".into(),
        ));
    }
    Ok(a)
}
fn global(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(x) => {
            let [a, b, c, _] = x.octets();
            !(x.is_private()
                || x.is_loopback()
                || x.is_link_local()
                || x.is_unspecified()
                || x.is_multicast()
                || x.is_broadcast()
                || a == 0
                || a == 100 && (64..=127).contains(&b)
                || a == 192 && ((b == 0 && matches!(c, 0 | 2)) || b == 168 || (b == 88 && c == 99))
                || a == 198 && ((b == 18 || b == 19) || (b == 51 && c == 100))
                || a == 203 && b == 0 && c == 113
                || a >= 240)
        }
        IpAddr::V6(x) => {
            {
                let s = x.segments();
                // Conservative admission: only public-unicast space, minus IANA special allocations.
                (0x2000..=0x3fff).contains(&s[0])
                    && !(s[0] == 0x3fff && s[1] & 0xf000 == 0) // 3fff::/20
                    && !(s[0] == 0x2001 && (s[1] < 0x0200 || s[1] == 0x0db8)) // 2001::/23 IETF special-purpose range, including ORCHIDv2, plus documentation
                    && s[0] != 0x2002 // 6to4
                    && !(x.is_loopback()
                    || x.is_unspecified()
                    || x.is_multicast()
                    || (s[0] == 0 && s[1] == 0 && s[2] == 0 && s[3] == 0 && s[4] == 0 && s[5] == 0)
                    || x.segments()[0] & 0xffc0 == 0xfe80
                    || x.segments()[0] & 0xfe00 == 0xfc00
                    || x.segments()[0] & 0xffc0 == 0xfec0
                    || (s[0] == 0x0100 && s[1] == 0) // 100::/64 discard-only
                    || (s[0] == 0x0064 && s[1] == 0xff9b && s[2] == 1) // 64:ff9b:1::/48
                    || s[0] == 0x2002 // 6to4
                    )
            }
        }
    }
}
fn transport(op: &'static str, e: &reqwest::Error) -> VlmError {
    if e.is_timeout() {
        VlmError::Timeout { operation: op }
    } else {
        VlmError::Transport {
            operation: op,
            message: "request failed".into(),
        }
    }
}
fn retry_error(e: &reqwest::Error) -> bool {
    e.is_timeout() || e.is_connect() || e.is_request()
}
fn retry_status(s: StatusCode) -> bool {
    s == StatusCode::TOO_MANY_REQUESTS || s == StatusCode::REQUEST_TIMEOUT || s.is_server_error()
}
fn retry_after(r: &reqwest::Response) -> Option<Duration> {
    let s = r
        .headers()
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?;
    if let Ok(n) = s.parse::<u64>() {
        return Some(Duration::from_secs(n.min(60)));
    }
    httpdate::parse_http_date(s)
        .ok()
        .and_then(|t| t.duration_since(SystemTime::now()).ok())
        .map(|d| d.min(Duration::from_secs(60)))
}
async fn retry_wait(attempt: usize, f: f32, hint: Option<Duration>) {
    tokio::time::sleep(hint.unwrap_or_else(|| {
        Duration::from_secs_f32((f.max(0.) * 2f32.powi(attempt as i32)).min(60.))
    }))
    .await
}
async fn read_limited(
    mut r: reqwest::Response,
    cap: usize,
    resource: &'static str,
) -> VlmResult<Vec<u8>> {
    if r.content_length().is_some_and(|n| n > cap as u64) {
        return Err(VlmError::LimitExceeded {
            resource,
            limit: cap as u64,
            actual: r.content_length().unwrap_or(0),
        });
    }
    let mut out = Vec::new();
    while let Some(c) = r.chunk().await.map_err(|e| transport("response", &e))? {
        if out.len() + c.len() > cap {
            return Err(limit(resource, cap, out.len() + c.len()));
        }
        out.extend_from_slice(&c)
    }
    Ok(out)
}
async fn read_limited_budgeted(
    mut r: reqwest::Response,
    cap: usize,
    resource: &'static str,
    budget: Option<&ByteBudget>,
) -> VlmResult<Vec<u8>> {
    if r.content_length().is_some_and(|n| n > cap as u64) {
        return Err(VlmError::LimitExceeded {
            resource,
            limit: cap as u64,
            actual: r.content_length().unwrap_or(0),
        });
    }
    let mut out = Vec::new();
    while let Some(chunk) = r.chunk().await.map_err(|e| transport("response", &e))? {
        let actual = out.len().saturating_add(chunk.len());
        if actual > cap {
            return Err(limit(resource, cap, actual));
        }
        if let Some(budget) = budget {
            budget.charge(chunk.len(), resource)?;
        }
        out.extend_from_slice(&chunk);
    }
    Ok(out)
}
#[allow(clippy::too_many_arguments)] // parser state is deliberately local to the stream loop
fn sse_line(
    line: &[u8],
    event: &mut Vec<u8>,
    tx: &std::sync::mpsc::Sender<VlmResult<String>>,
    total: &mut usize,
    cap: usize,
    done: &mut bool,
    terminal: &mut bool,
    allow_truncated: bool,
) -> VlmResult<()> {
    if line.is_empty() {
        return sse_event(event, tx, total, cap, done, terminal, allow_truncated);
    }
    if let Some(d) = line.strip_prefix(b"data:") {
        if !event.is_empty() {
            event.push(b'\n')
        }
        event.extend_from_slice(d.strip_prefix(b" ").unwrap_or(d));
    }
    Ok(())
}
fn sse_event(
    event: &mut Vec<u8>,
    tx: &std::sync::mpsc::Sender<VlmResult<String>>,
    total: &mut usize,
    cap: usize,
    done: &mut bool,
    terminal: &mut bool,
    allow_truncated: bool,
) -> VlmResult<()> {
    if event.is_empty() {
        return Ok(());
    }
    let data = std::str::from_utf8(event).map_err(|_| protocol("stream", "invalid SSE UTF-8"))?;
    if data == "[DONE]" {
        if !*terminal {
            return Err(protocol("stream", "[DONE] before terminal completion"));
        }
        event.clear();
        *done = true;
        return Ok(());
    }
    if *terminal {
        return Err(protocol("stream", "data after terminal completion"));
    }
    let v: Value =
        serde_json::from_str(data).map_err(|_| protocol("stream", "invalid SSE JSON"))?;
    if v.get("error").is_some() || v.get("object").and_then(Value::as_str) == Some("error") {
        return Err(protocol("stream", "error object in successful response"));
    }
    let choice = v
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|choices| choices.first())
        .and_then(Value::as_object)
        .ok_or_else(|| protocol("stream", "missing choices"))?;
    if let Some(reason) = choice
        .get("finish_reason")
        .filter(|reason| !reason.is_null())
    {
        let reason = reason
            .as_str()
            .ok_or_else(|| protocol("stream", "invalid finish reason"))?;
        if reason != "stop" && !(reason == "length" && allow_truncated) {
            return Err(protocol("stream", "unexpected finish reason"));
        }
        *terminal = true;
    }
    if let Some(s) = choice
        .get("delta")
        .and_then(|delta| delta.get("content"))
        .and_then(Value::as_str)
    {
        *total += s.len();
        if *total > cap {
            return Err(limit("completion", cap, *total));
        }
        let _ = tx.send(Ok(s.into()));
    }
    event.clear();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{build_body, global, json_worker, model_candidates};
    use crate::{SamplingParams, VlmError, VlmHttpConfig};
    use std::{
        net::{IpAddr, Ipv4Addr},
        time::Duration,
    };

    #[test]
    fn remote_classifier_rejects_special_ranges() {
        for address in [
            "10.0.0.1",
            "100.64.0.1",
            "192.0.2.1",
            "198.18.0.1",
            "203.0.113.1",
            "::",
            "::1",
            "::ffff:192.0.2.1",
            "fc00::1",
            "fe80::1",
            "ff00::1",
            "100::1",
            "64:ff9b:1::1",
            "3fff::1",
            "5f00::1",
            "64:ff9b::1",
            "100:0:0:1::1",
            "2002::1",
            "2001:0::1",
            "2001:10::1",
            "2001:20::1",
            "2001:100::1",
            "2001:1ff:ffff::1",
            "2001:db8::1",
        ] {
            assert!(
                !global(address.parse::<IpAddr>().unwrap()),
                "accepted {address}"
            );
        }
        assert!(global(IpAddr::V4(Ipv4Addr::from([8; 4]))));
    }

    #[test]
    fn candidate_diagnostics_are_bounded_and_stable() {
        let candidates =
            model_candidates(&["alpha".into(), "beta\nignored".into(), "z".repeat(1024)]);
        assert!(candidates.contains("alpha, beta?ignored"));
        assert!(candidates.len() <= 257);
    }

    #[test]
    fn shared_body_builder_keeps_official_and_public_protocol_fields_aligned() {
        let body = build_body(
            &VlmHttpConfig {
                sampling_params: Some(SamplingParams {
                    top_k: Some(2),
                    max_new_tokens: Some(7),
                    ..Default::default()
                }),
                ..Default::default()
            },
            "model",
            Some("prompt".into()),
            None,
            Some(3),
            false,
            vec![],
        );
        assert_eq!(body["messages"][1]["content"][0]["text"], "prompt");
        assert_eq!(body["skip_special_tokens"], false);
        assert_eq!(body["top_k"], 2);
        assert_eq!(body["max_tokens"], 7);
        assert_eq!(body["priority"], 3);
    }

    #[tokio::test]
    async fn json_workers_honor_official_deadlines() {
        assert!(matches!(
            json_worker(
                Some(tokio::time::Instant::now() + Duration::from_millis(1)),
                "chat",
                || {
                    std::thread::sleep(Duration::from_millis(20));
                    Ok(())
                },
            )
            .await,
            Err(VlmError::Timeout {
                operation: "official PDF"
            })
        ));
    }
}
