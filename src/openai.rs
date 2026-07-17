use crate::{ClientConfig, Error, ErrorContext, ModelInfo, Result, profile::Sampling};
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
                            body: truncate(&body),
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
    ) -> Result<String> {
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
                            return Err(protocol(
                                "chat completions",
                                &format!(
                                    "server error: {}",
                                    error["message"].as_str().unwrap_or("unknown")
                                ),
                            ));
                        }
                        let choice = value["choices"]
                            .as_array()
                            .and_then(|a| a.first())
                            .ok_or_else(|| protocol("chat completions", "empty choices"))?;
                        let finish = choice["finish_reason"].as_str().unwrap_or("");
                        if finish != "stop" && !(allow_length && finish == "length") {
                            return Err(protocol("chat completions", "unexpected finish reason"));
                        }
                        let content = match &choice["message"]["content"] {
                            Value::Null => "",
                            Value::String(content) => content,
                            _ => {
                                return Err(protocol(
                                    "chat completions",
                                    "message content is not a string",
                                ));
                            }
                        };
                        return Ok(content
                            .strip_suffix("<|im_end|>")
                            .unwrap_or(content)
                            .to_owned());
                    }
                    if !(status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error())
                        || attempt == 2
                    {
                        return Err(Error::Http {
                            status: status.as_u16(),
                            body: truncate(&body),
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
fn truncate(body: &[u8]) -> String {
    String::from_utf8_lossy(&body[..body.len().min(4096)]).into_owned()
}

#[cfg(test)]
mod tests {
    use super::OpenAi;
    use crate::ClientConfig;

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
}
