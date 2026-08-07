use crate::{
    ClientConfig, Error, ErrorContext, ModelInfo, Result, error::sanitize_vlm_error_bytes,
    profile::Sampling,
};
use reqwest::{Client, StatusCode};
use serde_json::{Value, json};
use std::time::Duration;

#[derive(Debug)]
pub(crate) struct OpenAi {
    client: Client,
    config: ClientConfig,
}

impl OpenAi {
    pub(crate) fn new(config: &ClientConfig) -> Result<Self> {
        config.validate()?;
        let client = Client::builder()
            .connect_timeout(config.timeouts.connect)
            .timeout(config.timeouts.request)
            .build()
            .map_err(|source| Error::Transport {
                context: ctx("openai client"),
                source,
            })?;
        Ok(Self {
            client,
            config: config.clone(),
        })
    }
    fn url(&self, endpoint: &str) -> Result<url::Url> {
        let path = self.config.base_url.path().trim_end_matches('/');
        let endpoint = if path.ends_with("/v1") {
            format!("../v1/{endpoint}")
        } else {
            format!("v1/{endpoint}")
        };
        self.config
            .base_url
            .join(&endpoint)
            .map_err(|e| Error::InvalidConfig(format!("OpenAI endpoint: {e}")))
    }
    async fn body(
        &self,
        response: reqwest::Response,
        operation: &'static str,
    ) -> Result<(StatusCode, Vec<u8>)> {
        let status = response.status();
        let limit = self.config.limits.max_response_bytes;
        let mut body = Vec::new();
        let mut response = response;
        while let Some(chunk) = response.chunk().await.map_err(|source| Error::Transport {
            context: ctx(operation),
            source,
        })? {
            let actual = body.len().saturating_add(chunk.len());
            if actual > limit {
                return Err(Error::LimitExceeded {
                    resource: "response bytes",
                    limit: limit as u64,
                    actual: actual as u64,
                });
            }
            body.extend_from_slice(&chunk);
        }
        Ok((status, body))
    }
    pub(crate) async fn models(&self) -> Result<ModelInfo> {
        let url = self.url("models")?;
        for attempt in 0..3 {
            let mut request = self.client.get(url.clone());
            if let Some(token) = &self.config.bearer_token {
                request = request.bearer_auth(token.expose());
            }
            match request.send().await {
                Ok(response) => {
                    let (status, body) = self.body(response, "models").await?;
                    if status.is_success() {
                        let value: Value = serde_json::from_slice(&body)?;
                        if let Some(model) = value["data"]
                            .as_array()
                            .and_then(|a| a.iter().find(|m| m["id"] == self.config.model))
                        {
                            return Ok(ModelInfo {
                                id: self.config.model.clone(),
                                owned_by: model["owned_by"].as_str().map(str::to_owned),
                            });
                        }
                        return Err(protocol(
                            "models",
                            "configured model is not returned by server",
                        ));
                    }
                    if !(status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error())
                        || attempt == 2
                    {
                        return Err(Error::Http {
                            status: status.as_u16(),
                            body: sanitize_error(&body),
                        });
                    }
                }
                Err(source) => {
                    if attempt == 2 {
                        return Err(Error::Transport {
                            context: ctx("models"),
                            source,
                        });
                    }
                }
            }
            tokio::time::sleep(Duration::from_millis(50 * (attempt + 1))).await;
        }
        unreachable!()
    }
    pub(crate) async fn completion(
        &self,
        prompt: &str,
        images: &[String],
        sampling: Sampling,
        max_tokens: Option<u32>,
        allow_length: bool,
    ) -> Result<(String, Vec<String>)> {
        let mut content = Vec::new();
        let mut images = images.iter();
        if prompt.contains("<image>") {
            let mut parts = prompt.split("<image>").peekable();
            while let Some(part) = parts.next() {
                content.push(json!({"type":"text","text":part}));
                if parts.peek().is_some()
                    && let Some(image) = images.next()
                {
                    content.push(json!({"type":"image_url","image_url":{"url":image}}));
                }
            }
        } else {
            for image in images {
                content.push(json!({"type":"image_url","image_url":{"url":image}}));
            }
            if !prompt.is_empty() {
                content.push(json!({"type":"text","text":prompt}));
            }
        }
        let gpt = self.config.model.starts_with("gpt");
        let mut payload = json!({"model":self.config.model,"messages":[{"role":"system","content":"You are a helpful assistant."},{"role":"user","content":content}],"temperature":sampling.temperature,"top_p":sampling.top_p,"presence_penalty":sampling.presence_penalty,"frequency_penalty":sampling.frequency_penalty,"vllm_xargs":{"no_repeat_ngram_size":sampling.no_repeat_ngram_size}});
        if !gpt {
            payload["top_k"] = json!(sampling.top_k);
            payload["repetition_penalty"] = json!(sampling.repetition_penalty);
            payload["skip_special_tokens"] = json!(false);
        }
        if let Some(n) = max_tokens {
            payload["max_completion_tokens"] = json!(n);
            payload["max_tokens"] = json!(n);
        }
        let url = self.url("chat/completions")?;
        for attempt in 0..3 {
            let mut request = self.client.post(url.clone()).json(&payload);
            if let Some(token) = &self.config.bearer_token {
                request = request.bearer_auth(token.expose());
            }
            match request.send().await {
                Ok(response) => {
                    let (status, body) = self.body(response, "chat completions").await?;
                    if status.is_success() {
                        let value: Value = serde_json::from_slice(&body)?;
                        if let Some(error) = value.get("error") {
                            return Err(protocol("chat completions", &server_error_message(error)));
                        }
                        let mut warnings = Vec::new();
                        let Some(choice) = value["choices"].as_array().and_then(|a| a.first())
                        else {
                            warnings.push("empty choices; treating the reply as empty".into());
                            return Ok((String::new(), sanitize_warnings(warnings)));
                        };
                        let finish = choice["finish_reason"].as_str().unwrap_or("");
                        if finish != "stop" && !(allow_length && finish == "length") {
                            warnings.push(format!(
                                "unexpected finish reason {finish}; keeping the reply"
                            ));
                        }
                        let content = match &choice["message"]["content"] {
                            Value::String(content) => content.clone(),
                            Value::Array(elements) => {
                                warnings.push(
                                    "message content is an array; concatenating its text parts"
                                        .into(),
                                );
                                elements
                                    .iter()
                                    .filter_map(|element| element["text"].as_str())
                                    .collect()
                            }
                            Value::Null => {
                                warnings.push("message content is null; treating as empty".into());
                                String::new()
                            }
                            _ => {
                                warnings.push(
                                    "message content is not a string; treating as empty".into(),
                                );
                                String::new()
                            }
                        };
                        let content = match content.strip_suffix("<|im_end|>") {
                            Some(stripped) => stripped.to_owned(),
                            None => content,
                        };
                        return Ok((content, sanitize_warnings(warnings)));
                    }
                    if !(status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error())
                        || attempt == 2
                    {
                        return Err(Error::Http {
                            status: status.as_u16(),
                            body: sanitize_error(&body),
                        });
                    }
                }
                Err(source) => {
                    if attempt == 2 {
                        return Err(Error::Transport {
                            context: ctx("chat completions"),
                            source,
                        });
                    }
                }
            }
            tokio::time::sleep(Duration::from_millis(50 * (attempt + 1))).await;
        }
        unreachable!()
    }
}
fn ctx(operation: &'static str) -> ErrorContext {
    ErrorContext {
        operation: Some(operation),
        ..Default::default()
    }
}
fn protocol(operation: &'static str, message: &str) -> Error {
    Error::Protocol {
        context: ctx(operation),
        message: message.into(),
    }
}
const ERROR_BODY_CAP: usize = 4096;
const TRUNCATED_SUFFIX: &str = " [truncated]";
const WARNING_CAP: usize = 512;

fn sanitize_error(body: &[u8]) -> String {
    sanitize_error_with_cap(body, ERROR_BODY_CAP)
}

fn sanitize_warnings(warnings: Vec<String>) -> Vec<String> {
    warnings
        .into_iter()
        .map(|warning| sanitize_vlm_error_bytes(warning.as_bytes(), WARNING_CAP))
        .collect()
}

fn sanitize_error_with_cap(body: &[u8], cap: usize) -> String {
    bound_diagnostic(sanitize_vlm_error_bytes(body, cap), cap)
}

fn bound_diagnostic(mut text: String, cap: usize) -> String {
    if text.len() <= cap {
        return text;
    }
    let mut end = cap - TRUNCATED_SUFFIX.len();
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text.truncate(end);
    text.push_str(TRUNCATED_SUFFIX);
    text
}

fn server_error_message(error: &Value) -> String {
    const PREFIX: &str = "server error: ";
    let message = sanitize_error_with_cap(
        error["message"].as_str().unwrap_or("unknown").as_bytes(),
        ERROR_BODY_CAP - PREFIX.len(),
    );
    format!("{PREFIX}{message}")
}

#[cfg(test)]
mod tests {
    use super::{
        ERROR_BODY_CAP, OpenAi, TRUNCATED_SUFFIX, WARNING_CAP, bound_diagnostic, protocol,
        sanitize_error, sanitize_warnings, server_error_message,
    };
    use crate::{ClientConfig, Error, profile::COMMON};
    use axum::{Json, Router, routing::post};
    use serde_json::{Value, json};
    use tokio::net::TcpListener;

    async fn mock_completion(body: Value) -> OpenAi {
        let app = Router::new().route(
            "/v1/chat/completions",
            post(move || async move { Json(body) }),
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let config = ClientConfig::new(format!("http://{address}"), "model").unwrap();
        OpenAi::new(&config).unwrap()
    }

    #[test]
    fn resolves_conventional_openai_endpoints() {
        for (base, models, completions) in [
            (
                "https://host/",
                "https://host/v1/models",
                "https://host/v1/chat/completions",
            ),
            (
                "https://host/v1/",
                "https://host/v1/models",
                "https://host/v1/chat/completions",
            ),
            (
                "https://host/proxy/",
                "https://host/proxy/v1/models",
                "https://host/proxy/v1/chat/completions",
            ),
            (
                "https://host/proxy/v1/",
                "https://host/proxy/v1/models",
                "https://host/proxy/v1/chat/completions",
            ),
        ] {
            let config = ClientConfig::new(base, "model").unwrap();
            let openai = OpenAi::new(&config).unwrap();
            assert_eq!(openai.url("models").unwrap().as_str(), models);
            assert_eq!(
                openai.url("chat/completions").unwrap().as_str(),
                completions
            );
        }
    }

    #[test]
    fn sanitizes_http_error_body_with_cap() {
        let raw = "Bearer s é ".repeat(ERROR_BODY_CAP);
        let error = Error::Http {
            status: 401,
            body: sanitize_error(raw.as_bytes()),
        };
        let Error::Http { body, .. } = error else {
            unreachable!()
        };

        assert!(!body.contains("Bearer s"), "leaked secret: {body}");
        assert!(body.contains("Bearer [REDACTED]"));
        assert_eq!(body.len(), ERROR_BODY_CAP);
        assert!(body.ends_with(TRUNCATED_SUFFIX));

        let unicode = bound_diagnostic(
            format!(
                "{}é{}",
                "x".repeat(ERROR_BODY_CAP - TRUNCATED_SUFFIX.len() - 1),
                "y".repeat(TRUNCATED_SUFFIX.len() * 2)
            ),
            ERROR_BODY_CAP,
        );
        assert_eq!(unicode.len(), ERROR_BODY_CAP - 1);
        assert!(!unicode.contains('\u{fffd}'));
        assert!(unicode.ends_with(TRUNCATED_SUFFIX));
    }

    #[test]
    fn sanitizes_successful_json_error_message_with_cap() {
        let error = json!({
            "message": "data:x,s ".repeat(ERROR_BODY_CAP)
        });
        let error = protocol("chat completions", &server_error_message(&error));
        let Error::Protocol { message, .. } = error else {
            unreachable!()
        };

        assert!(!message.contains("data:x,s"), "leaked data URL: {message}");
        assert!(message.contains("[REDACTED_DATA_URL]"));
        assert_eq!(message.len(), ERROR_BODY_CAP);
        assert!(message.ends_with(TRUNCATED_SUFFIX));
    }

    #[tokio::test]
    async fn empty_choices_degrade_to_empty_content_with_warning() {
        let openai = mock_completion(json!({"choices": []})).await;
        let (content, warnings) = openai
            .completion("prompt", &[], COMMON, None, true)
            .await
            .unwrap();
        assert_eq!(content, "");
        assert!(
            warnings.iter().any(|w| w.contains("empty choices")),
            "{warnings:?}"
        );
    }

    #[tokio::test]
    async fn array_content_is_concatenated_with_warning() {
        let openai = mock_completion(json!({
            "choices": [{
                "finish_reason": "stop",
                "message": {"content": [
                    {"type": "text", "text": "part-"},
                    {"type": "image_url", "image_url": {"url": "data:image/png;base64,secret"}},
                    {"type": "text", "text": "two"}
                ]}
            }]
        }))
        .await;
        let (content, warnings) = openai
            .completion("prompt", &[], COMMON, None, true)
            .await
            .unwrap();
        assert_eq!(content, "part-two");
        assert!(
            warnings.iter().any(|w| w.contains("concatenating")),
            "{warnings:?}"
        );
        assert!(!warnings.iter().any(|w| w.contains("secret")));
    }

    #[tokio::test]
    async fn content_filter_finish_keeps_content_with_warning() {
        let openai = mock_completion(json!({
            "choices": [{
                "finish_reason": "content_filter",
                "message": {"content": "partial reply"}
            }]
        }))
        .await;
        let (content, warnings) = openai
            .completion("prompt", &[], COMMON, None, false)
            .await
            .unwrap();
        assert_eq!(content, "partial reply");
        assert!(
            warnings.iter().any(|w| w.contains("content_filter")),
            "{warnings:?}"
        );
    }

    #[test]
    fn warning_sanitizer_bounds_and_redacts() {
        let raw = format!("Bearer s {} {}", "x".repeat(WARNING_CAP), "secret-tail");
        let warnings = sanitize_warnings(vec![raw]);
        assert_eq!(warnings.len(), 1);
        let warning = &warnings[0];
        assert!(!warning.contains("secret-tail"));
        assert!(!warning.contains("Bearer s"));
        assert!(warning.contains("Bearer [REDACTED]"));
        assert!(warning.ends_with(TRUNCATED_SUFFIX));
    }
}
