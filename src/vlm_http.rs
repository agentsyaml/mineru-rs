use crate::{error::sanitize_vlm_error_bytes, *};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use bytes::Bytes;
use futures_util::{StreamExt, stream};
use reqwest::{Client, StatusCode, header::CONTENT_TYPE, redirect::Policy};
use serde_json::{Value, json};
use std::{
    net::{IpAddr, SocketAddr},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, SystemTime},
};
use tokio::{net::lookup_host, sync::Semaphore};
use url::Url;

/// Shared, monotonic byte allowance for one official document.
#[derive(Debug)]
pub(crate) struct ByteBudget {
    cap: u64,
    used: AtomicU64,
}

impl ByteBudget {
    pub(crate) fn new(cap: u64) -> Self {
        Self {
            cap,
            used: AtomicU64::new(0),
        }
    }

    #[cfg(test)]
    pub(crate) fn remaining(&self) -> u64 {
        self.cap.saturating_sub(self.used.load(Ordering::Acquire))
    }

    pub(crate) fn charge(&self, bytes: u64, resource: &'static str) -> VlmResult<()> {
        let result = self
            .used
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |used| {
                used.checked_add(bytes).filter(|total| *total <= self.cap)
            });
        result.map(|_| ()).map_err(|used| VlmError::LimitExceeded {
            resource,
            limit: self.cap,
            actual: used.saturating_add(bytes),
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
    temperature_retry: bool,
    http: Client,
    base: Url,
    model: String,
    task_work_lease: TaskWorkLease,
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
        Self::connect_with_temperature_retry(config, false).await
    }

    pub async fn connect_with_temperature_retry(
        config: VlmHttpConfig,
        temperature_retry: bool,
    ) -> VlmResult<Self> {
        Self::connect_for_task_with_temperature_retry(
            config,
            temperature_retry,
            TaskWorkLease::default(),
        )
        .await
    }

    pub(crate) async fn connect_for_task_with_temperature_retry(
        config: VlmHttpConfig,
        temperature_retry: bool,
        task_work_lease: TaskWorkLease,
    ) -> VlmResult<Self> {
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
        let builder = Client::builder()
            .no_proxy()
            .redirect(Policy::none())
            .timeout(config.http_timeout)
            .connect_timeout(config.connect_timeout)
            .pool_max_idle_per_host(config.max_keepalive_connections)
            .pool_idle_timeout(config.keepalive_expiry);
        let http = builder.build().map_err(|e| transport("connect", &e))?;
        let client = Self {
            config: Arc::new(config),
            temperature_retry,
            http,
            base,
            model: String::new(),
            task_work_lease,
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
    #[cfg(test)]
    pub(crate) async fn predict_official_budgeted(
        &self,
        request: VlmRequest,
        cap: usize,
        budget: Option<Arc<ByteBudget>>,
        deadline: tokio::time::Instant,
    ) -> VlmResult<(String, usize, Vec<String>)> {
        self.predict_official_budgeted_with_stage(request, cap, budget, deadline, "official")
            .await
    }

    pub(crate) async fn predict_official_budgeted_with_stage(
        &self,
        request: VlmRequest,
        cap: usize,
        budget: Option<Arc<ByteBudget>>,
        deadline: tokio::time::Instant,
        quality_stage: &'static str,
    ) -> VlmResult<(String, usize, Vec<String>)> {
        let config = self.config.clone();
        let model = self.model.clone();
        if tokio::time::Instant::now() >= deadline {
            return Err(VlmError::Timeout {
                operation: "official PDF",
            });
        }
        let body = tokio::time::timeout_at(
            deadline,
            tokio::task::spawn_blocking(
                self.task_work_lease
                    .wrap(move || official_body(config, model, request)),
            ),
        )
        .await
        .map_err(|_| VlmError::Timeout {
            operation: "official PDF",
        })?
        .map_err(|_| VlmError::Transport {
            operation: "chat",
            message: "body worker failed".into(),
        })??;
        let body = json_body(body, Some(deadline), &self.task_work_lease).await?;
        tokio::time::timeout_at(
            deadline,
            self.complete_limited(body, cap, budget, Some(deadline), true, Some(quality_stage)),
        )
        .await
        .map_err(|_| VlmError::Timeout {
            operation: "official PDF",
        })?
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
        let n = requests.len().max(1);
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
    pub async fn aio_batch_predict_as_iter(
        &self,
        requests: Vec<VlmRequest>,
        semaphore: VlmSemaphore,
    ) -> VlmResult<VlmBatchCompletionStream> {
        let l = semaphore
            .unwrap_or_else(|| Arc::new(Semaphore::new(self.config.max_concurrency.max(1))));
        let width = requests.len().max(1);
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
        // The semaphore, not a snapshot of its permits, controls active HTTP work.
        Ok(Box::pin(FailFastBatch {
            inner: Box::pin(producer),
            failed: false,
        }))
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
        // Legacy transport path has no warning sink and must stay strict: malformed LLM replies
        // surface as protocol errors instead of silently empty strings. The official route
        // degrades them via `predict_official_budgeted`.
        let body = json_body(self.body(r, streaming).await?, None, &self.task_work_lease).await?;
        self.complete_limited(
            body,
            self.config.max_response_bytes,
            None,
            None,
            false,
            None,
        )
        .await
        .map(|x| x.0)
    }
    async fn complete_limited(
        &self,
        body: Bytes,
        cap: usize,
        budget: Option<Arc<ByteBudget>>,
        deadline: Option<tokio::time::Instant>,
        degrade: bool,
        quality_stage: Option<&'static str>,
    ) -> VlmResult<(String, usize, Vec<String>)> {
        let quality_enabled = degrade && self.temperature_retry && quality_stage.is_some();
        let base_temperature = if quality_enabled {
            let body = body.clone();
            Some(
                json_worker(deadline, "chat", &self.task_work_lease, move || {
                    Ok(body_temperature(&body))
                })
                .await?,
            )
        } else {
            None
        }
        .flatten();
        let mut request_body = body;
        let mut total_bytes: usize = 0;
        let mut quality_retries = 0;
        let mut warnings = Vec::new();
        loop {
            if let Some(deadline) = deadline
                && tokio::time::Instant::now() >= deadline
            {
                return Err(VlmError::Timeout {
                    operation: "official PDF",
                });
            }
            let mut transport_retries_used = 0;
            let (v, bytes) = self
                .send_json_limited(
                    "chat",
                    self.url("chat/completions")?,
                    Some(request_body.clone()),
                    cap,
                    budget.clone(),
                    deadline,
                    &mut transport_retries_used,
                )
                .await?;
            total_bytes = total_bytes.saturating_add(bytes);
            let (text, response_warnings) = if deadline.is_some() {
                let allow_truncated_content = self.config.allow_truncated_content;
                let end_token = self.config.end_token.clone();
                json_worker(deadline, "chat", &self.task_work_lease, move || {
                    let mut response_warnings = Vec::new();
                    let text = completion_text(
                        v,
                        allow_truncated_content,
                        &end_token,
                        &mut response_warnings,
                        degrade,
                    )?;
                    Ok((text, response_warnings))
                })
                .await?
            } else {
                let mut response_warnings = Vec::new();
                let text = completion_text(
                    v,
                    self.config.allow_truncated_content,
                    &self.config.end_token,
                    &mut response_warnings,
                    degrade,
                )?;
                (text, response_warnings)
            };
            warnings.extend(
                response_warnings
                    .into_iter()
                    .map(|warning| stage_warning(quality_stage, warning)),
            );
            let Some(reason) = quality_enabled
                .then(|| quality_failure(&text, quality_stage.expect("quality stage is present")))
                .flatten()
            else {
                return Ok((text, total_bytes, warnings));
            };
            let Some(base_temperature) = base_temperature else {
                return Ok((text, total_bytes, warnings));
            };
            let Some(next_temperature) = next_retry_temperature(base_temperature, quality_retries)
            else {
                return Ok((text, total_bytes, warnings));
            };
            if let Some(deadline) = deadline
                && tokio::time::Instant::now() >= deadline
            {
                return Err(VlmError::Timeout {
                    operation: "official PDF",
                });
            }
            let next_body = self
                .temperature_body(request_body.clone(), next_temperature, deadline)
                .await?;
            warnings.push(stage_warning(
                quality_stage,
                format!("temperature retry: temperature={next_temperature:.1} quality={reason}"),
            ));
            request_body = next_body;
            quality_retries = quality_retries.saturating_add(1);
        }
    }

    async fn temperature_body(
        &self,
        body: Bytes,
        temperature: f32,
        deadline: Option<tokio::time::Instant>,
    ) -> VlmResult<Bytes> {
        json_worker(deadline, "chat", &self.task_work_lease, move || {
            replace_temperature(body, temperature)
        })
        .await
    }
    fn url(&self, suffix: &str) -> VlmResult<Url> {
        let mut u = self.base.clone();
        let append_v1 = u
            .path_segments()
            .and_then(|segments| segments.filter(|segment| !segment.is_empty()).next_back())
            != Some("v1");
        let trailing_empty_segments = u
            .path_segments()
            .map(|segments| {
                segments
                    .rev()
                    .take_while(|segment| segment.is_empty())
                    .count()
            })
            .unwrap_or(0);
        let mut segments = u
            .path_segments_mut()
            .map_err(|_| VlmError::InvalidConfig("server_url must be a base URL".into()))?;
        for _ in 0..trailing_empty_segments {
            segments.pop_if_empty();
        }
        if append_v1 {
            segments.push("v1");
        }
        segments.extend(suffix.split('/').filter(|segment| !segment.is_empty()));
        drop(segments);
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
        let mut retries_used = 0;
        self.send_json_limited(
            op,
            url,
            body,
            self.config.max_response_bytes,
            None,
            None,
            &mut retries_used,
        )
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
        retries_used: &mut usize,
    ) -> VlmResult<(Value, usize)> {
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
                        if retry && *retries_used < self.config.max_retries {
                            retry_wait(*retries_used, self.config.retry_backoff_factor, wait).await;
                            *retries_used += 1;
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
                    return json_response(b, op, deadline, &self.task_work_lease)
                        .await
                        .map(|value| (value, bytes));
                }
                Err(e) => {
                    if retry_error(&e) && *retries_used < self.config.max_retries {
                        retry_wait(*retries_used, self.config.retry_backoff_factor, None).await;
                        *retries_used += 1
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
        let config = self.config.clone();
        let model = self.model.clone();
        json_worker(None, "chat", &self.task_work_lease, move || {
            Ok(build_body(
                &config, &model, prompt, sampling, priority, streaming, images,
            ))
        })
        .await
    }
    async fn image(&self, input: VlmImageInput) -> VlmResult<Option<(Bytes, String)>> {
        match input {
            VlmImageInput::RemoteUrl(url) => {
                let bytes = self.remote(url).await?;
                crate::vlm_image::admit_bytes_for_task(
                    bytes,
                    None,
                    self.config.clone(),
                    &self.task_work_lease,
                )
                .await
                .map(Some)
            }
            input => {
                crate::vlm_image::admit_local_for_task(
                    input,
                    self.config.clone(),
                    &self.task_work_lease,
                )
                .await
            }
        }
    }

    async fn remote(&self, mut url: Url) -> VlmResult<Vec<u8>> {
        if !self.config.allow_remote_images {
            return Err(VlmError::InvalidImageInput(
                "remote images are disabled".into(),
            ));
        }
        for redirects in 0..=self.config.max_redirects {
            let addrs = remote_addrs(
                &url,
                self.config.allow_private_remote_images,
                self.config.connect_timeout,
            )
            .await?;
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
        let body = json_body(self.body(r, true).await?, None, &self.task_work_lease).await?;
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
const TEMPERATURE_RETRY_STEP: f32 = 0.2;
const TEMPERATURE_RETRY_MAX: f32 = 1.0;
const MAX_TEMPERATURE_RETRIES: usize = 5;
// Retry-only sampling floors: widen restrictive values without inventing omitted fields or
// replacing server-specific unlimited top_k values (0 and negative values).
const TEMPERATURE_RETRY_MIN_TOP_K: i64 = 40;
const TEMPERATURE_RETRY_MIN_TOP_P: f64 = 0.9;
// This bounds only the repetition quality heuristic's UTF-8-safe prefix and scratch table;
// it does not change the response/HTTP byte cap. Repetition after this prefix may be missed.
const MAX_REPEATED_FRAGMENT_SCAN_BYTES: usize = 16 * 1024;
const MIN_REPEATED_FRAGMENT_CHARS: usize = 8;
const REPEATED_FRAGMENT_COUNT: usize = 3;
const MIN_REPEATED_TOKEN_CHARS: usize = 8;
const REPEATED_TOKEN_COUNT: usize = 6;
const MIN_REPEATED_LINE_CHARS: usize = 8;
const REPEATED_LINE_COUNT: usize = 4;

/// Conservative quality guard for official buffered completions. Short text is
/// deliberately exempt: only whitespace, replacement characters, non-whitespace controls, or
/// repetitions crossing the documented thresholds fail.
fn quality_failure(text: &str, stage: &str) -> Option<&'static str> {
    if stage != "layout" && text.trim().is_empty() {
        return Some("blank");
    }
    if text.contains('\u{FFFD}') {
        return Some("replacement-character");
    }
    if text
        .chars()
        .any(|character| character.is_control() && !matches!(character, '\n' | '\r' | '\t'))
    {
        return Some("control-character");
    }
    if repeated_whole_fragment(text) || repeated_tokens(text) || repeated_lines(text) {
        return Some("repetition");
    }
    None
}

fn repeated_whole_fragment(text: &str) -> bool {
    let trimmed = text.trim();
    let mut scan_len = trimmed.len().min(MAX_REPEATED_FRAGMENT_SCAN_BYTES);
    while !trimmed.is_char_boundary(scan_len) {
        scan_len -= 1;
    }
    let bytes = &trimmed.as_bytes()[..scan_len];
    if bytes.len() < MIN_REPEATED_FRAGMENT_CHARS * REPEATED_FRAGMENT_COUNT {
        return false;
    }
    let mut prefix = [0usize; MAX_REPEATED_FRAGMENT_SCAN_BYTES];
    for index in 1..bytes.len() {
        let mut length = prefix[index - 1];
        while length > 0 && bytes[index] != bytes[length] {
            length = prefix[length - 1];
        }
        if bytes[index] == bytes[length] {
            length += 1;
        }
        prefix[index] = length;
    }
    let period = bytes.len() - prefix[bytes.len() - 1];
    trimmed.is_char_boundary(period)
        && trimmed[..period].chars().count() >= MIN_REPEATED_FRAGMENT_CHARS
        && bytes.len() / period >= REPEATED_FRAGMENT_COUNT
}

fn stage_warning(stage: Option<&'static str>, warning: String) -> String {
    match stage {
        Some(stage) => format!("stage={stage} {warning}"),
        None => warning,
    }
}

fn repeated_tokens(text: &str) -> bool {
    let mut previous = None;
    let mut run = 0;
    for token in text.split_whitespace() {
        let long_enough = token.chars().count() >= MIN_REPEATED_TOKEN_CHARS;
        if long_enough && previous == Some(token) {
            run += 1;
        } else {
            run = usize::from(long_enough);
        }
        if run >= REPEATED_TOKEN_COUNT {
            return true;
        }
        previous = long_enough.then_some(token);
    }
    false
}

fn repeated_lines(text: &str) -> bool {
    let mut previous = None;
    let mut run = 0;
    for line in text.lines() {
        let line = line.trim();
        let long_enough = line.chars().count() >= MIN_REPEATED_LINE_CHARS;
        if long_enough && previous == Some(line) {
            run += 1;
        } else {
            run = usize::from(long_enough);
        }
        if run >= REPEATED_LINE_COUNT {
            return true;
        }
        previous = long_enough.then_some(line);
    }
    false
}

fn body_temperature(body: &Bytes) -> Option<f32> {
    serde_json::from_slice::<Value>(body)
        .ok()
        .and_then(|body| body.get("temperature").and_then(Value::as_f64))
        .map(|temperature| temperature as f32)
        .filter(|temperature| temperature.is_finite())
}

fn next_retry_temperature(base: f32, retries_used: usize) -> Option<f32> {
    if !(0.0..TEMPERATURE_RETRY_MAX).contains(&base) || retries_used >= MAX_TEMPERATURE_RETRIES {
        return None;
    }
    let current = (base + retries_used as f32 * TEMPERATURE_RETRY_STEP).min(TEMPERATURE_RETRY_MAX);
    if current >= TEMPERATURE_RETRY_MAX {
        return None;
    }
    let step = retries_used.saturating_add(1) as f32 * TEMPERATURE_RETRY_STEP;
    Some((base + step).min(TEMPERATURE_RETRY_MAX)).filter(|temperature| *temperature > current)
}

fn replace_temperature(body: Bytes, temperature: f32) -> VlmResult<Bytes> {
    let mut value: Value = serde_json::from_slice(&body)
        .map_err(|_| protocol("chat", "request JSON serialization failed"))?;
    value["temperature"] = json!(temperature);
    widen_sampling_for_retry(&mut value);
    serde_json::to_vec(&value)
        .map(Bytes::from)
        .map_err(|_| protocol("chat", "request JSON serialization failed"))
}

fn widen_sampling_for_retry(body: &mut Value) {
    if let Some(top_k) = body.get("top_k").and_then(Value::as_i64)
        && top_k > 0
        && top_k < TEMPERATURE_RETRY_MIN_TOP_K
    {
        body["top_k"] = json!(TEMPERATURE_RETRY_MIN_TOP_K);
    }
    if let Some(top_p) = body.get("top_p").and_then(Value::as_f64)
        && top_p < TEMPERATURE_RETRY_MIN_TOP_P
    {
        body["top_p"] = json!(TEMPERATURE_RETRY_MIN_TOP_P);
    }
}

fn completion_text(
    mut response: Value,
    allow_truncated_content: bool,
    end_token: &str,
    warnings: &mut Vec<String>,
    degrade: bool,
) -> VlmResult<String> {
    // `degrade` is true only on the official budgeted path (which has a warning sink). The
    // legacy predict/complete path stays strict so a malformed reply surfaces as a protocol
    // error instead of a silently empty string.
    if response.get("error").is_some()
        || response.get("object").and_then(Value::as_str) == Some("error")
    {
        let server_message = response
            .get("error")
            .and_then(|error| error.get("message"))
            .and_then(Value::as_str)
            .unwrap_or("no error message");
        if degrade {
            warnings.push(format!(
                "VLM returned an error object in a successful chat response: {server_message}"
            ));
            return Ok(String::new());
        }
        return Err(protocol("chat", "error object in successful response"));
    }
    // Number of tokens the model generated (from the response `usage` block),
    // surfaced in warnings when a reply is malformed or truncated.
    let generated_tokens = completion_tokens(&response);
    let Some(choice) = response
        .get_mut("choices")
        .and_then(Value::as_array_mut)
        .and_then(|choices| choices.first_mut())
    else {
        if degrade {
            warnings.push("VLM chat response is missing choices".into());
            return Ok(String::new());
        }
        return Err(protocol("chat", "missing choices"));
    };
    match choice.get("finish_reason").and_then(Value::as_str) {
        Some("stop") => {}
        Some(finish) if finish == "length" && allow_truncated_content => {}
        Some(finish) => {
            if degrade {
                warnings.push(format!(
                    "VLM chat finish reason is unexpected: {finish} (generated {generated_tokens} tokens); \
                     a length-limited reply is truncated: raise the server max_tokens or check the model for repetitive output"
                ));
            } else {
                return Err(protocol("chat", "unexpected finish reason"));
            }
        }
        None => {
            if degrade {
                warnings.push(format!(
                    "VLM chat response is missing finish_reason (generated {generated_tokens} tokens); \
                     the server did not return a standard OpenAI chat completion"
                ));
            } else {
                return Err(protocol("chat", "missing finish reason"));
            }
        }
    }
    let Some(content) = choice
        .get_mut("message")
        .and_then(|message| message.get_mut("content"))
    else {
        if degrade {
            warnings.push(format!(
                "VLM chat response has no content (generated {generated_tokens} tokens)"
            ));
            return Ok(String::new());
        }
        return Err(protocol("chat", "missing string content"));
    };
    match std::mem::replace(content, Value::Null) {
        Value::String(text) => Ok(strip_end(text, end_token)),
        Value::Null => {
            if degrade {
                warnings.push(format!(
                    "VLM chat response content is empty (generated {generated_tokens} tokens)"
                ));
            }
            Ok(String::new())
        }
        _ => {
            if degrade {
                warnings.push(format!(
                    "VLM chat response content is not a string (generated {generated_tokens} tokens)"
                ));
                Ok(String::new())
            } else {
                Err(protocol("chat", "missing string content"))
            }
        }
    }
}
/// Extracts the number of generated tokens from an OpenAI chat completion
/// response's `usage` block, for surfacing in truncation warnings.
fn completion_tokens(response: &Value) -> u64 {
    response
        .get("usage")
        .and_then(|usage| usage.get("completion_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0)
}
fn strip_end(mut text: String, token: &str) -> String {
    if !token.is_empty() && text.ends_with(token) {
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
    images: Vec<(Bytes, String)>,
) -> Value {
    let images = images.into_iter().map(|(bytes, media)| {
        let mut url =
            String::with_capacity("data:;base64,".len() + media.len() + bytes.len() * 4 / 3 + 3);
        url.push_str("data:");
        url.push_str(&media);
        url.push_str(";base64,");
        STANDARD.encode_string(&bytes, &mut url);
        json!({"type":"image_url","image_url":{"url":url}})
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
    let system = config
        .system_prompt
        .clone()
        .unwrap_or_else(|| "You are a helpful assistant.".into());
    if !system.is_empty() {
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
        match image {
            VlmImageInput::None => continue,
            VlmImageInput::Path(_) | VlmImageInput::RemoteUrl(_) => {
                return Err(VlmError::InvalidImageInput(
                    "official request requires a local image".into(),
                ));
            }
            image => {
                if let Some(image) = crate::vlm_image::admit_local_blocking(image, &config)? {
                    images.push(image);
                }
            }
        }
    }
    Ok(build_body(
        &config, &model, prompt, sampling, priority, false, images,
    ))
}

async fn json_body(
    body: Value,
    deadline: Option<tokio::time::Instant>,
    task_work_lease: &TaskWorkLease,
) -> VlmResult<Bytes> {
    json_worker(deadline, "chat", task_work_lease, move || {
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
    task_work_lease: &TaskWorkLease,
) -> VlmResult<Value> {
    json_worker(deadline, operation, task_work_lease, move || {
        serde_json::from_slice(&body).map_err(|_| protocol(operation, "invalid JSON response"))
    })
    .await
}

async fn json_worker<T: Send + 'static>(
    deadline: Option<tokio::time::Instant>,
    operation: &'static str,
    task_work_lease: &TaskWorkLease,
    job: impl FnOnce() -> VlmResult<T> + Send + 'static,
) -> VlmResult<T> {
    let worker = tokio::task::spawn_blocking(task_work_lease.wrap(job));
    let result = if let Some(deadline) = deadline {
        tokio::time::timeout_at(deadline, worker)
            .await
            .map_err(|_| VlmError::Timeout { operation })?
    } else {
        worker.await
    };
    result.map_err(|_| VlmError::Transport {
        operation,
        message: "JSON worker failed".into(),
    })?
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
async fn remote_addrs(
    u: &Url,
    allow_private: bool,
    connect_timeout: Duration,
) -> VlmResult<Vec<SocketAddr>> {
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
    let a: Vec<_> = tokio::time::timeout(connect_timeout, lookup_host((host, port)))
        .await
        .map_err(|_| VlmError::InvalidImageInput("remote host resolution timed out".into()))?
        .map_err(|e| VlmError::InvalidImageInput(format!("remote host resolution failed: {e}")))?
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
/// Ceiling for a server-provided `Retry-After` hint (seconds). The hint is server-controlled, not
/// operator-configurable, so it is capped to keep deadline-free paths (e.g. `/v1/models`
/// discovery) from stalling for hours on a hostile or buggy server. The operator's own
/// `retry_backoff` factor remains uncapped.
const RETRY_AFTER_HINT_CAP_SECS: u64 = 300;
fn retry_after_hint(value: &str) -> Option<Duration> {
    if let Ok(n) = value.parse::<u64>() {
        return Some(Duration::from_secs(n.min(RETRY_AFTER_HINT_CAP_SECS)));
    }
    httpdate::parse_http_date(value)
        .ok()
        .and_then(|t| t.duration_since(SystemTime::now()).ok())
        .map(|wait| wait.min(Duration::from_secs(RETRY_AFTER_HINT_CAP_SECS)))
}
fn retry_after(r: &reqwest::Response) -> Option<Duration> {
    let s = r
        .headers()
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?;
    retry_after_hint(s)
}
/// Exponential backoff from the configured factor with no fixed cap. The exponent is bounded to
/// keep the float arithmetic finite, and a non-finite/overflowing result maps to the largest
/// representable wait rather than panicking in `Duration::from_secs_f32`.
fn retry_backoff(attempt: usize, f: f32) -> Duration {
    let exponent = u32::try_from(attempt).unwrap_or(u32::MAX).min(128);
    let seconds = f.max(0.0) * 2f32.powi(exponent as i32);
    if seconds.is_finite() {
        Duration::try_from_secs_f32(seconds).unwrap_or(Duration::MAX)
    } else {
        Duration::MAX
    }
}
static RETRY_JITTER_COUNTER: AtomicU64 = AtomicU64::new(0);
/// Deterministic jitter multiplier in [0.5, 1.0). Every retry consumes a step of a global
/// counter, so requests failing in lockstep do not all sleep the same backoff duration.
fn retry_jitter() -> f32 {
    let step = RETRY_JITTER_COUNTER.fetch_add(1, Ordering::Relaxed);
    // Scramble the counter so early lockstep retries spread immediately instead of by ~0.1%.
    let spread = step.wrapping_mul(2654435761).rotate_left(17) % 500;
    (500 + spread) as f32 / 1000.0
}
async fn retry_wait(attempt: usize, f: f32, hint: Option<Duration>) {
    let backoff = match hint {
        // Server-provided `Retry-After` hints are already per-request and need no jitter.
        Some(wait) => wait,
        None => retry_backoff(attempt, f).mul_f32(retry_jitter()),
    };
    tokio::time::sleep(backoff).await
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
        let actual = out.len().saturating_add(c.len());
        if actual > cap {
            return Err(limit(resource, cap, actual));
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
            budget.charge(chunk.len() as u64, resource)?;
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
    use super::{
        ByteBudget, MAX_REPEATED_FRAGMENT_SCAN_BYTES, TEMPERATURE_RETRY_MIN_TOP_K,
        TEMPERATURE_RETRY_MIN_TOP_P, VlmHttpClient, build_body, completion_text, global,
        json_worker, model_candidates, next_retry_temperature, quality_failure,
        repeated_whole_fragment, replace_temperature, retry_after_hint, retry_backoff, strip_end,
    };
    use crate::{
        SamplingParams, TaskWorkLease, VlmError, VlmHttpConfig, VlmImageInput, VlmRequest,
        vlm_image::admit_local,
    };
    use axum::{Json, Router, http::StatusCode, routing::post};
    use base64::{Engine as _, engine::general_purpose::STANDARD};
    use bytes::Bytes;
    use image::{DynamicImage, ImageFormat};
    use reqwest::Client;
    use serde_json::{Value, json};
    use std::{
        io::Cursor,
        net::{IpAddr, Ipv4Addr},
        sync::{
            Arc, Mutex,
            atomic::{AtomicUsize, Ordering},
        },
        time::{Duration, SystemTime},
    };
    use url::Url;

    async fn official_test_client(body: String, max_response_bytes: usize) -> VlmHttpClient {
        let app = Router::new().route(
            "/v1/chat/completions",
            post(move || {
                let body = body.clone();
                async move { ([("content-type", "application/json")], body) }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        VlmHttpClient::connect(VlmHttpConfig {
            server_url: Some(format!("http://{address}").parse().unwrap()),
            model_name: Some("mock".into()),
            skip_model_name_checking: true,
            max_retries: 0,
            max_response_bytes,
            ..Default::default()
        })
        .await
        .unwrap()
    }

    async fn official_sequence_client(
        replies: Vec<Value>,
        temperature_retry: bool,
    ) -> (VlmHttpClient, Arc<Mutex<Vec<Value>>>) {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let remaining = Arc::new(Mutex::new(replies));
        let app = Router::new().route(
            "/v1/chat/completions",
            post({
                let requests = Arc::clone(&requests);
                let remaining = Arc::clone(&remaining);
                move |Json(request): Json<Value>| {
                    let requests = Arc::clone(&requests);
                    let remaining = Arc::clone(&remaining);
                    async move {
                        requests.lock().unwrap().push(request);
                        Json(remaining.lock().unwrap().remove(0))
                    }
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let client = VlmHttpClient::connect_with_temperature_retry(
            VlmHttpConfig {
                server_url: Some(format!("http://{address}").parse().unwrap()),
                model_name: Some("mock".into()),
                skip_model_name_checking: true,
                max_retries: 0,
                ..Default::default()
            },
            temperature_retry,
        )
        .await
        .unwrap();
        (client, requests)
    }

    fn official_request(temperature: f32) -> VlmRequest {
        official_request_with_sampling(SamplingParams {
            temperature: Some(temperature),
            top_p: Some(0.01),
            top_k: Some(1),
            presence_penalty: Some(0.0),
            frequency_penalty: Some(0.0),
            repetition_penalty: Some(1.0),
            no_repeat_ngram_size: Some(100),
            ..Default::default()
        })
    }

    fn official_request_with_sampling(sampling: SamplingParams) -> VlmRequest {
        VlmRequest {
            sampling: Some(sampling),
            ..Default::default()
        }
    }

    fn official_reply(text: &str) -> Value {
        json!({
            "choices": [{"finish_reason": "stop", "message": {"content": text}}]
        })
    }

    #[test]
    fn official_quality_guard_uses_conservative_fixed_thresholds() {
        assert_eq!(quality_failure(" \n\t\r", "semantic"), Some("blank"));
        assert_eq!(quality_failure(" \n\t\r", "layout"), None);
        assert_eq!(quality_failure("short", "semantic"), None);
        assert_eq!(quality_failure("valid\ntext\tvalue", "semantic"), None);
        assert_eq!(
            quality_failure("replacement\u{FFFD}", "semantic"),
            Some("replacement-character")
        );
        assert_eq!(
            quality_failure("clean\u{0001}text", "semantic"),
            Some("control-character")
        );

        // Repetition requires either three whole fragments of at least eight characters, six
        // identical tokens of at least eight characters, or four identical lines of at least eight
        // characters. Four ordinary repeated JSON fields stay below the token/line thresholds.
        assert_eq!(
            quality_failure("abcdefghabcdefghabcdefgh", "semantic"),
            Some("repetition")
        );
        assert_eq!(
            quality_failure(
                "repeatme repeatme repeatme repeatme repeatme repeatme",
                "semantic",
            ),
            Some("repetition")
        );
        assert_eq!(
            quality_failure("same line\nsame line\nsame line\nsame line", "semantic",),
            Some("repetition")
        );
        assert_eq!(
            quality_failure(
                r#"{"type":"text","type":"text","type":"text","type":"text"}"#,
                "semantic",
            ),
            None
        );
        assert_eq!(
            quality_failure("abcdefghabcdefghabcdefghabcd", "semantic"),
            Some("repetition")
        );
    }

    #[test]
    fn whole_fragment_quality_scan_is_bounded_to_a_fixed_prefix() {
        let repeated = "abcdefgh".repeat(MAX_REPEATED_FRAGMENT_SCAN_BYTES / "abcdefgh".len());
        let text = format!("{repeated}suffix outside the scan window");

        assert!(text.len() > MAX_REPEATED_FRAGMENT_SCAN_BYTES);
        assert!(repeated_whole_fragment(&text));
    }

    #[test]
    fn temperature_retry_sequence_is_fixed_and_capped() {
        let temperatures = (0..6)
            .filter_map(|retries| next_retry_temperature(0.0, retries))
            .collect::<Vec<_>>();
        assert_eq!(temperatures, [0.2, 0.4, 0.6, 0.8, 1.0]);
        assert_eq!(next_retry_temperature(0.0, 5), None);
        assert_eq!(next_retry_temperature(0.9, 0), Some(1.0));
        assert_eq!(next_retry_temperature(1.0, 0), None);
    }

    #[test]
    fn temperature_replacement_preserves_the_buffered_request() {
        let body = Bytes::from_static(br#"{"temperature":0.0,"messages":[]}"#);
        let replaced = replace_temperature(body.clone(), 0.2).unwrap();
        assert_eq!(
            serde_json::from_slice::<Value>(&body).unwrap()["temperature"],
            0.0
        );
        let temperature = serde_json::from_slice::<Value>(&replaced).unwrap()["temperature"]
            .as_f64()
            .unwrap();
        assert!((temperature - 0.2).abs() < 0.000_001);
    }

    #[test]
    fn retry_backoff_follows_the_configured_factor_without_a_fixed_cap() {
        // Default factor 0.5: exponential waits 0.5, 1, 2, 4 seconds.
        assert_eq!(retry_backoff(0, 0.5), Duration::from_millis(500));
        assert_eq!(retry_backoff(1, 0.5), Duration::from_secs(1));
        assert_eq!(retry_backoff(2, 0.5), Duration::from_secs(2));
        assert_eq!(retry_backoff(3, 0.5), Duration::from_secs(4));
        // The removed 60 s ceiling: a large configured factor yields waits above 60 s.
        assert!(retry_backoff(7, 2.0) > Duration::from_secs(60));
        // A zero factor waits immediately; extreme exponents stay overflow-safe.
        assert!(retry_backoff(0, 0.0).is_zero());
        assert_eq!(retry_backoff(usize::MAX, 0.5), Duration::MAX);
    }

    #[test]
    fn server_retry_after_hint_is_capped_while_operator_backoff_is_not() {
        // A hostile/buggy server hint is capped at 5 minutes.
        assert_eq!(retry_after_hint("3600"), Some(Duration::from_secs(300)));
        assert_eq!(retry_after_hint("120"), Some(Duration::from_secs(120)));
        // HTTP-date hints are capped the same way.
        let far_future = httpdate::fmt_http_date(SystemTime::now() + Duration::from_secs(7200));
        assert_eq!(
            retry_after_hint(&far_future),
            Some(Duration::from_secs(300))
        );
        // The operator factor remains uncapped above the server-hint ceiling.
        assert!(retry_backoff(8, 2.0) > Duration::from_secs(300));
    }

    #[test]
    fn byte_budget_reports_full_cap_and_cumulative_rejection() {
        let budget = ByteBudget::new(8);
        budget.charge(3, "encoded document bytes").unwrap();
        budget.charge(5, "encoded document bytes").unwrap();
        assert!(matches!(
            budget.charge(1, "encoded document bytes"),
            Err(VlmError::LimitExceeded {
                limit: 8,
                actual: 9,
                ..
            })
        ));

        let large = ByteBudget::new(8 * 1024 * 1024 * 1024);
        large
            .charge(4 * 1024 * 1024 * 1024, "encoded document bytes")
            .unwrap();
        assert_eq!(large.remaining(), 4 * 1024 * 1024 * 1024);
    }

    #[test]
    fn completion_text_degrades_malformed_replies_to_warnings_on_the_official_path() {
        let text = |value, allow| -> (String, Vec<String>) {
            let mut warnings = Vec::new();
            let text = completion_text(value, allow, "", &mut warnings, true).unwrap();
            (text, warnings)
        };
        // Error object: empty content, warning with the server message, no leak of crafted choices.
        let (out, warnings) = text(
            serde_json::json!({"error": {"message": "model overloaded"}}),
            false,
        );
        assert_eq!(out, "");
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("model overloaded"));
        // Missing choices.
        let (out, warnings) = text(serde_json::json!({"choices": []}), false);
        assert_eq!(out, "");
        assert!(warnings[0].contains("missing choices"));
        // Missing finish_reason keeps available content plus a warning with the token count.
        let (out, warnings) = text(
            serde_json::json!({"choices": [{"message": {"content": "x"}}], "usage": {"completion_tokens": 7}}),
            false,
        );
        assert_eq!(out, "x");
        assert!(warnings[0].contains("finish_reason"));
        assert!(warnings[0].contains("7"), "{}", warnings[0]);
        // Unexpected finish reason keeps the available (truncated) content.
        let (out, warnings) = text(
            serde_json::json!({"choices": [{"finish_reason": "content_filter", "message": {"content": "partial"}}]}),
            false,
        );
        assert_eq!(out, "partial");
        assert!(warnings[0].contains("content_filter"));
        // length without allow_truncated_content keeps the truncated content with a warning
        // naming the token count and the remedy...
        let (out, warnings) = text(
            serde_json::json!({"choices": [{"finish_reason": "length", "message": {"content": "cut"}}], "usage": {"completion_tokens": 1234}}),
            false,
        );
        assert_eq!(out, "cut");
        assert!(warnings[0].contains("length"));
        assert!(warnings[0].contains("1234"), "{}", warnings[0]);
        assert!(warnings[0].contains("max_tokens"), "{}", warnings[0]);
        // ...whereas the allowed length+allow_truncated_content case stays warning-free.
        let mut warnings = Vec::new();
        assert_eq!(
            completion_text(
                serde_json::json!({"choices": [{"finish_reason": "length", "message": {"content": "cut"}}]}),
                true,
                "",
                &mut warnings,
                true,
            )
            .unwrap(),
            "cut"
        );
        assert!(warnings.is_empty());
        // Non-string content: empty content plus a warning.
        let (out, warnings) = text(
            serde_json::json!({"choices": [{"finish_reason": "stop", "message": {"content": 7}}]}),
            false,
        );
        assert_eq!(out, "");
        assert!(warnings[0].contains("not a string"));
    }

    #[test]
    fn completion_text_stays_strict_on_the_legacy_path() {
        let mut warnings = Vec::new();
        // Each malformed reply is a protocol error, never a silent empty string.
        for (value, expected) in [
            (
                serde_json::json!({"error": "boom"}),
                "error object in successful response",
            ),
            (serde_json::json!({"choices": []}), "missing choices"),
            (
                serde_json::json!({"choices": [{"message": {"content": "x"}}]}),
                "missing finish reason",
            ),
            (
                serde_json::json!({"choices": [{"finish_reason": "content_filter", "message": {"content": ""}}]}),
                "unexpected finish reason",
            ),
            (
                serde_json::json!({"choices": [{"finish_reason": "stop"}]}),
                "missing string content",
            ),
            (
                serde_json::json!({"choices": [{"finish_reason": "stop", "message": {"content": 7}}]}),
                "missing string content",
            ),
        ] {
            assert!(matches!(
                completion_text(value, false, "", &mut warnings, false),
                Err(VlmError::Protocol { operation: "chat", message }) if message == expected
            ));
        }
        assert!(warnings.is_empty());
        // Valid and null-content replies still return content as before.
        assert_eq!(
            completion_text(
                serde_json::json!({"choices": [{"finish_reason": "stop", "message": {"content": "ok"}}]}),
                false,
                "",
                &mut warnings,
                false,
            )
            .unwrap(),
            "ok"
        );
        assert_eq!(
            completion_text(
                serde_json::json!({"choices": [{"finish_reason": "stop", "message": {"content": null}}]}),
                false,
                "",
                &mut warnings,
                false,
            )
            .unwrap(),
            ""
        );
        assert!(warnings.is_empty());
    }

    #[tokio::test]
    async fn official_budgeted_surfaces_collected_warnings() {
        let body =
            r#"{"choices":[{"finish_reason":"content_filter","message":{"content":"partial"}}]}"#
                .to_string();
        let client = official_test_client(body, 1024).await;
        let (text, _raw, warnings) = client
            .predict_official_budgeted(
                VlmRequest::default(),
                client.config.max_response_bytes,
                None,
                tokio::time::Instant::now() + Duration::from_secs(2),
            )
            .await
            .unwrap();
        assert_eq!(text, "partial");
        assert!(warnings.iter().any(|w| w.contains("content_filter")));
    }

    #[tokio::test]
    async fn official_temperature_retry_is_opt_in_and_keeps_the_first_body() {
        let (client, requests) =
            official_sequence_client(vec![official_reply("usable")], false).await;
        let (text, _, warnings) = client
            .predict_official_budgeted(
                official_request(0.0),
                1024,
                None,
                tokio::time::Instant::now() + Duration::from_secs(2),
            )
            .await
            .unwrap();
        assert_eq!(text, "usable");
        assert!(warnings.is_empty());
        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0]["temperature"], 0.0);
    }

    #[tokio::test]
    async fn official_temperature_retry_widens_sampling_only_on_retry() {
        let (client, requests) =
            official_sequence_client(vec![official_reply(""), official_reply("usable")], true)
                .await;
        client
            .predict_official_budgeted(
                official_request(0.0),
                1024,
                None,
                tokio::time::Instant::now() + Duration::from_secs(2),
            )
            .await
            .unwrap();
        {
            let requests = requests.lock().unwrap();
            assert_eq!(requests[0]["top_k"], 1);
            assert!((requests[0]["top_p"].as_f64().unwrap() - 0.01).abs() < 0.000_001);
            assert_eq!(requests[1]["top_k"], TEMPERATURE_RETRY_MIN_TOP_K);
            assert!(
                (requests[1]["top_p"].as_f64().unwrap() - TEMPERATURE_RETRY_MIN_TOP_P).abs()
                    < 0.000_001
            );
        }

        let (client, requests) =
            official_sequence_client(vec![official_reply(""), official_reply("usable")], true)
                .await;
        client
            .predict_official_budgeted(
                official_request_with_sampling(SamplingParams {
                    temperature: Some(0.0),
                    top_p: Some(0.95),
                    top_k: Some(80),
                    ..Default::default()
                }),
                1024,
                None,
                tokio::time::Instant::now() + Duration::from_secs(2),
            )
            .await
            .unwrap();
        let requests = requests.lock().unwrap();
        assert_eq!(requests[0]["top_k"], 80);
        assert_eq!(requests[1]["top_k"], 80);
        assert!((requests[0]["top_p"].as_f64().unwrap() - 0.95).abs() < 0.000_001);
        assert!((requests[1]["top_p"].as_f64().unwrap() - 0.95).abs() < 0.000_001);
    }

    #[test]
    fn temperature_retry_does_not_add_unlimited_sampling_fields() {
        let missing = replace_temperature(
            Bytes::from_static(br#"{"temperature":0.0,"messages":[]}"#),
            0.2,
        )
        .unwrap();
        let missing: Value = serde_json::from_slice(&missing).unwrap();
        assert!(missing.get("top_k").is_none());
        assert!(missing.get("top_p").is_none());

        let unlimited =
            replace_temperature(Bytes::from_static(br#"{"temperature":0.0,"top_k":0}"#), 0.2)
                .unwrap();
        let unlimited: Value = serde_json::from_slice(&unlimited).unwrap();
        assert_eq!(unlimited["top_k"], 0);
        assert!(unlimited.get("top_p").is_none());
    }

    #[tokio::test]
    async fn official_layout_blank_response_is_accepted_without_a_temperature_retry() {
        let (client, requests) = official_sequence_client(vec![official_reply("")], true).await;
        let (text, _, warnings) = client
            .predict_official_budgeted_with_stage(
                official_request(0.0),
                1024,
                None,
                tokio::time::Instant::now() + Duration::from_secs(2),
                "layout",
            )
            .await
            .unwrap();
        assert_eq!(text, "");
        assert!(warnings.is_empty());
        assert_eq!(requests.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn official_semantic_blank_response_still_retries_when_enabled() {
        let (client, requests) =
            official_sequence_client(vec![official_reply(""), official_reply("usable")], true)
                .await;
        let (text, _, warnings) = client
            .predict_official_budgeted_with_stage(
                official_request(0.0),
                1024,
                None,
                tokio::time::Instant::now() + Duration::from_secs(2),
                "semantic",
            )
            .await
            .unwrap();
        assert_eq!(text, "usable");
        assert_eq!(requests.lock().unwrap().len(), 2);
        assert!(
            warnings
                .iter()
                .any(|warning| warning.contains("stage=semantic"))
        );
    }

    #[tokio::test]
    async fn transport_retry_budget_resets_for_each_temperature_request() {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let attempts = Arc::new(AtomicUsize::new(0));
        let app = Router::new().route(
            "/v1/chat/completions",
            post({
                let requests = Arc::clone(&requests);
                let attempts = Arc::clone(&attempts);
                move |Json(request): Json<Value>| {
                    let requests = Arc::clone(&requests);
                    let attempts = Arc::clone(&attempts);
                    async move {
                        requests.lock().unwrap().push(request);
                        let attempt = attempts.fetch_add(1, Ordering::SeqCst);
                        let reply = if attempt == 1 {
                            official_reply("")
                        } else {
                            official_reply("usable")
                        };
                        if matches!(attempt, 0 | 2) {
                            (
                                StatusCode::SERVICE_UNAVAILABLE,
                                Json(json!({"error": "retry"})),
                            )
                        } else {
                            (StatusCode::OK, Json(reply))
                        }
                    }
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let client = VlmHttpClient::connect_with_temperature_retry(
            VlmHttpConfig {
                server_url: Some(format!("http://{address}").parse().unwrap()),
                model_name: Some("mock".into()),
                skip_model_name_checking: true,
                max_retries: 1,
                retry_backoff_factor: 0.0,
                ..Default::default()
            },
            true,
        )
        .await
        .unwrap();
        let (_, _, warnings) = client
            .predict_official_budgeted_with_stage(
                official_request(0.0),
                1024,
                None,
                tokio::time::Instant::now() + Duration::from_secs(5),
                "semantic",
            )
            .await
            .unwrap();
        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 4);
        assert_eq!(requests[0]["temperature"], 0.0);
        assert_eq!(requests[1]["temperature"], 0.0);
        assert!((requests[2]["temperature"].as_f64().unwrap() - 0.2).abs() < 0.000_001);
        assert!((requests[3]["temperature"].as_f64().unwrap() - 0.2).abs() < 0.000_001);
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("stage=semantic"));
    }

    #[tokio::test]
    async fn ordinary_predict_ignores_the_official_temperature_retry() {
        let (client, requests) =
            official_sequence_client(vec![official_reply(""), official_reply("")], true).await;
        assert_eq!(client.predict(official_request(0.0)).await.unwrap(), "");
        assert_eq!(
            client
                .batch_predict(vec![official_request(0.0)])
                .await
                .unwrap(),
            [""]
        );
        assert_eq!(requests.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn official_temperature_retry_warms_in_fixed_steps_and_stops_on_success() {
        let (client, requests) =
            official_sequence_client(vec![official_reply(""), official_reply("usable")], true)
                .await;
        let (_, _, warnings) = client
            .predict_official_budgeted(
                official_request(0.0),
                1024,
                None,
                tokio::time::Instant::now() + Duration::from_secs(2),
            )
            .await
            .unwrap();
        let requests = requests.lock().unwrap();
        let temperatures = requests
            .iter()
            .map(|request| request["temperature"].as_f64().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(temperatures.len(), 2);
        assert!((temperatures[0] - 0.0).abs() < 0.000_001);
        assert!((temperatures[1] - 0.2).abs() < 0.000_001);
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("stage=official"));
    }

    #[tokio::test]
    async fn official_temperature_retry_covers_quality_failures_and_temperature_boundaries() {
        for bad in [
            "",
            "replacement\u{FFFD}",
            "clean\u{0001}text",
            "abcdefghabcdefghabcdefgh",
        ] {
            let (client, requests) =
                official_sequence_client(vec![official_reply(bad), official_reply("usable")], true)
                    .await;
            client
                .predict_official_budgeted(
                    official_request(0.9),
                    1024,
                    None,
                    tokio::time::Instant::now() + Duration::from_secs(2),
                )
                .await
                .unwrap();
            let requests = requests.lock().unwrap();
            assert_eq!(requests.len(), 2);
            assert!((requests[0]["temperature"].as_f64().unwrap() - 0.9).abs() < 0.000_001);
            assert!((requests[1]["temperature"].as_f64().unwrap() - 1.0).abs() < 0.000_001);
        }

        let (client, requests) = official_sequence_client(vec![official_reply("")], true).await;
        client
            .predict_official_budgeted(
                official_request(1.0),
                1024,
                None,
                tokio::time::Instant::now() + Duration::from_secs(2),
            )
            .await
            .unwrap();
        assert_eq!(requests.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn official_temperature_retry_stops_at_one_after_all_failures() {
        let (client, requests) =
            official_sequence_client((0..6).map(|_| official_reply("")).collect(), true).await;
        client
            .predict_official_budgeted(
                official_request(0.0),
                1024,
                None,
                tokio::time::Instant::now() + Duration::from_secs(2),
            )
            .await
            .unwrap();
        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 6);
        assert_eq!(requests[5]["temperature"], 1.0);
    }

    #[tokio::test]
    async fn official_temperature_retry_does_not_start_after_deadline() {
        let hits = Arc::new(AtomicUsize::new(0));
        let app = Router::new().route(
            "/v1/chat/completions",
            post({
                let hits = Arc::clone(&hits);
                move || {
                    let hits = Arc::clone(&hits);
                    async move {
                        hits.fetch_add(1, Ordering::SeqCst);
                        tokio::time::sleep(Duration::from_millis(100)).await;
                        Json(official_reply(""))
                    }
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let client = VlmHttpClient::connect_with_temperature_retry(
            VlmHttpConfig {
                server_url: Some(format!("http://{address}").parse().unwrap()),
                model_name: Some("mock".into()),
                skip_model_name_checking: true,
                max_retries: 0,
                ..Default::default()
            },
            true,
        )
        .await
        .unwrap();
        assert!(matches!(
            client
                .predict_official_budgeted(
                    official_request(0.0),
                    1024,
                    None,
                    tokio::time::Instant::now() + Duration::from_millis(20),
                )
                .await,
            Err(VlmError::Timeout { .. })
        ));
        assert_eq!(hits.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn official_response_cap_and_shared_raw_budget_are_independent() {
        let body = serde_json::json!({"choices":[{"finish_reason":"stop","message":{"content":"x".repeat(128)}}]}).to_string();
        let oversized = official_test_client(body.clone(), body.len() - 1).await;
        let high_budget = Arc::new(ByteBudget::new(8 * 1024 * 1024 * 1024));
        assert!(matches!(
            oversized
                .predict_official_budgeted(
                    VlmRequest::default(),
                    oversized.config.max_response_bytes,
                    Some(high_budget),
                    tokio::time::Instant::now() + Duration::from_secs(2),
                )
                .await,
            Err(VlmError::LimitExceeded { resource: "response", limit, .. }) if limit == body.len() as u64 - 1
        ));

        let client = official_test_client(body.clone(), body.len()).await;
        let raw = Arc::new(ByteBudget::new(body.len() as u64 * 2 - 1));
        client
            .predict_official_budgeted(
                VlmRequest::default(),
                client.config.max_response_bytes,
                Some(Arc::clone(&raw)),
                tokio::time::Instant::now() + Duration::from_secs(2),
            )
            .await
            .unwrap();
        assert!(matches!(
            client
                .predict_official_budgeted(
                    VlmRequest::default(),
                    client.config.max_response_bytes,
                    Some(raw),
                    tokio::time::Instant::now() + Duration::from_secs(2),
                )
                .await,
            Err(VlmError::LimitExceeded { resource: "response", limit, actual })
                if limit == body.len() as u64 * 2 - 1 && actual == body.len() as u64 * 2
        ));
    }

    #[test]
    fn vlm_urls_preserve_base_paths_and_normalize_v1() {
        for (base, expected) in [
            ("https://example.com", "https://example.com/v1/models"),
            ("https://example.com/", "https://example.com/v1/models"),
            (
                "https://example.com/proxy",
                "https://example.com/proxy/v1/models",
            ),
            (
                "https://example.com/proxy/",
                "https://example.com/proxy/v1/models",
            ),
            (
                "https://example.com/proxy//",
                "https://example.com/proxy/v1/models",
            ),
            (
                "https://example.com/proxy////",
                "https://example.com/proxy/v1/models",
            ),
            ("https://example.com/v1", "https://example.com/v1/models"),
            ("https://example.com/v1/", "https://example.com/v1/models"),
            ("https://example.com/v1//", "https://example.com/v1/models"),
            (
                "https://example.com/proxy/v1",
                "https://example.com/proxy/v1/models",
            ),
            (
                "https://example.com/proxy/v1/",
                "https://example.com/proxy/v1/models",
            ),
            (
                "https://example.com/v10",
                "https://example.com/v10/v1/models",
            ),
        ] {
            let client = VlmHttpClient {
                config: Arc::new(VlmHttpConfig::default()),
                temperature_retry: false,
                http: Client::new(),
                base: Url::parse(base).unwrap(),
                model: String::new(),
                task_work_lease: TaskWorkLease::default(),
            };
            assert_eq!(client.url("models").unwrap().as_str(), expected, "{base}");
        }
    }

    #[test]
    fn vlm_urls_preserve_encoded_prefix_authority_and_query() {
        let client = VlmHttpClient {
            config: Arc::new(VlmHttpConfig::default()),
            temperature_retry: false,
            http: Client::new(),
            base: Url::parse("https://user:pass@example.com:8443/proxy%2Ftenant?token=a%2Fb")
                .unwrap(),
            model: String::new(),
            task_work_lease: TaskWorkLease::default(),
        };

        assert_eq!(
            client.url("chat/completions").unwrap().as_str(),
            "https://user:pass@example.com:8443/proxy%2Ftenant/v1/chat/completions?token=a%2Fb"
        );
    }

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
    fn strip_end_only_uses_the_supplied_config_token() {
        assert_eq!(strip_end("value-END".into(), "-END"), "value");
        assert_eq!(
            strip_end("value<|im_end|>".into(), "-END"),
            "value<|im_end|>"
        );
        assert_eq!(strip_end("value".into(), ""), "value");
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

    #[test]
    fn shared_body_builder_omits_vllm_only_fields_for_gpt_models() {
        // OpenAI-compatible endpoints reject unknown fields, so a gpt-prefixed model name
        // must not receive skip_special_tokens/top_k/repetition_penalty. The max_tokens
        // duplication is intentional: it covers both vLLM and OpenAI field namings.
        let body = build_body(
            &VlmHttpConfig {
                sampling_params: Some(SamplingParams {
                    top_k: Some(2),
                    repetition_penalty: Some(1.1),
                    max_new_tokens: Some(7),
                    ..Default::default()
                }),
                ..Default::default()
            },
            "gpt-4o-mini",
            Some("prompt".into()),
            None,
            None,
            false,
            vec![],
        );
        assert!(body.get("skip_special_tokens").is_none());
        assert!(body.get("top_k").is_none());
        assert!(body.get("repetition_penalty").is_none());
        assert_eq!(body["max_tokens"], 7);
        assert_eq!(body["max_completion_tokens"], 7);
    }

    #[tokio::test]
    async fn timed_out_json_worker_holds_task_lease_until_it_exits() {
        let gate = Arc::new(tokio::sync::Semaphore::new(1));
        let permit = gate.clone().acquire_owned().await.unwrap();
        let root = TaskWorkLease::from_permit(permit);
        let worker_lease = root.clone();
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let deadline = tokio::time::Instant::now() + Duration::from_millis(20);
        let task = tokio::spawn(async move {
            json_worker(Some(deadline), "chat", &worker_lease, move || {
                let _ = started_tx.send(());
                release_rx.recv().unwrap();
                Ok(())
            })
            .await
        });
        started_rx.await.unwrap();
        assert!(matches!(
            task.await.unwrap(),
            Err(VlmError::Timeout { operation: "chat" })
        ));
        drop(root);
        assert!(gate.clone().try_acquire_owned().is_err());
        release_tx.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if gate.clone().try_acquire_owned().is_ok() {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    }

    fn png(width: u32, height: u32) -> Vec<u8> {
        let mut bytes = Vec::new();
        DynamicImage::new_rgb8(width, height)
            .write_to(&mut Cursor::new(&mut bytes), ImageFormat::Png)
            .unwrap();
        bytes
    }

    #[tokio::test]
    async fn local_image_admission_rejects_oversized_path_and_encoded_payload() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("oversized.png");
        std::fs::write(&path, [0_u8; 5]).unwrap();
        let config = Arc::new(VlmHttpConfig {
            max_image_bytes: 4,
            ..Default::default()
        });
        assert!(matches!(
            admit_local(VlmImageInput::Path(path), config.clone()).await,
            Err(VlmError::LimitExceeded {
                resource: "image bytes",
                ..
            })
        ));
        assert!(matches!(
            admit_local(
                VlmImageInput::Base64 {
                    data: "A".repeat(9),
                    media_type: None,
                },
                config,
            )
            .await,
            Err(VlmError::LimitExceeded {
                resource: "image bytes",
                ..
            })
        ));
    }

    #[tokio::test]
    async fn local_image_admission_enforces_pixels_and_keeps_valid_bytes() {
        let bytes = png(2, 3);
        let config = Arc::new(VlmHttpConfig {
            max_image_bytes: bytes.len(),
            max_decoded_pixels: 5,
            ..Default::default()
        });
        assert!(matches!(
            admit_local(
                VlmImageInput::DataUrl(format!(
                    "data:image/png;base64,{}",
                    STANDARD.encode(&bytes)
                )),
                config,
            )
            .await,
            Err(VlmError::LimitExceeded {
                resource: "image pixels",
                limit: 5,
                actual: 6,
            })
        ));

        let shared = Bytes::from(bytes.clone());
        let shared_ptr = shared.as_ptr();
        let admitted = admit_local(
            VlmImageInput::Bytes {
                data: shared,
                media_type: Some("image/png".into()),
            },
            Arc::new(VlmHttpConfig {
                max_image_bytes: bytes.len(),
                max_decoded_pixels: 6,
                ..Default::default()
            }),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(admitted.0.as_ref(), bytes.as_slice());
        assert_eq!(admitted.0.as_ptr(), shared_ptr);
        assert_eq!(admitted.1, "image/png");
    }

    #[tokio::test]
    async fn data_url_rejects_media_mismatch_and_huge_header() {
        let bytes = png(1, 1);
        let config = Arc::new(VlmHttpConfig::default());
        assert!(matches!(
            admit_local(
                VlmImageInput::DataUrl(format!(
                    "data:image/jpeg;base64,{}",
                    STANDARD.encode(&bytes)
                )),
                config.clone(),
            )
            .await,
            Err(VlmError::InvalidImageInput(message)) if message == "image media type mismatch"
        ));
        assert!(matches!(
            admit_local(
                VlmImageInput::DataUrl(format!(
                    "data:image/png{};base64,AA==",
                    "x".repeat(1_000_000)
                )),
                config,
            )
            .await,
            Err(VlmError::InvalidImageInput(message)) if message == "unsupported image media type"
        ));
    }
}
