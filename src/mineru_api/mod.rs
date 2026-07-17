//! Internal support for the official MinerU API client.
//!
//! Transport, discovery, archive extraction, and CLI wiring intentionally live in later phases.
#![allow(dead_code)] // P2A defines the private domain consumed by later P2 phases.

mod archive;
#[cfg(feature = "internal-mineru-api-client")]
mod classifier;
#[cfg(feature = "internal-mineru-api-client")]
mod discovery;
mod http;
pub(crate) mod ooxml;
mod planning;
mod remote_preview;
pub(crate) use planning::unique_stems;
mod runner;
mod zip_scan;

use crate::{ProgressCallback, input_prepare::DocumentKind};
use serde_json::Value;
use std::path::PathBuf;

#[doc(hidden)]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RemoteApiDocument {
    pub path: PathBuf,
    pub kind: DocumentKind,
    pub stem: String,
    pub effective_pages: usize,
    pub order: usize,
}
#[doc(hidden)]
#[derive(Clone, Debug)]
pub struct RemoteApiOptions {
    pub backend: String,
    pub method: String,
    pub effort: String,
    pub language: String,
    pub server_url: Option<String>,
    pub start: u64,
    pub end: Option<u64>,
    pub formula: bool,
    pub table: bool,
    pub image_analysis: bool,
    pub client_side_output_generation: bool,
    pub route: crate::OfficialPdfOptions,
}
impl Default for RemoteApiOptions {
    fn default() -> Self {
        Self {
            backend: "vlm-http-client".into(),
            method: "auto".into(),
            effort: "medium".into(),
            language: "ch".into(),
            server_url: None,
            start: 0,
            end: None,
            formula: true,
            table: true,
            image_analysis: true,
            client_side_output_generation: false,
            route: crate::OfficialPdfOptions::default(),
        }
    }
}
#[doc(hidden)]
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RemoteApiEnv {
    pub max_concurrent_requests: usize,
    pub result_timeout_seconds: f64,
    pub download_timeout_seconds: f64,
}
#[doc(hidden)]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RemoteApiFailure {
    pub task_index: usize,
    pub document_stems: Vec<String>,
    pub message: String,
}

#[doc(hidden)]
pub fn normalize_remote_language(value: &str) -> Result<String, String> {
    match value {
        "en" | "japan" | "chinese_cht" | "latin" => Ok("ch".into()),
        "ch" | "ch_server" | "korean" | "ta" | "te" | "ka" | "th" | "el" | "arabic"
        | "east_slavic" | "cyrillic" | "devanagari" => Ok(value.into()),
        _ => Err(format!("unsupported language: {value}")),
    }
}
#[doc(hidden)]
pub fn parse_remote_api_env(get: impl Fn(&str) -> Option<String>) -> Result<RemoteApiEnv, String> {
    parse_remote_env(get).map(Into::into)
}
#[doc(hidden)]
pub async fn run_remote_api_documents(
    documents: Vec<RemoteApiDocument>,
    output: PathBuf,
    api_url: String,
    options: RemoteApiOptions,
    env: RemoteApiEnv,
    events: Option<ProgressCallback>,
) -> Result<Vec<RemoteApiFailure>, String> {
    runner::run_documents(documents, &output, &api_url, options, env, events).await
}
#[doc(hidden)]
pub fn selected_document_pages(
    path: &std::path::Path,
    kind: DocumentKind,
    start: u64,
    end: Option<u64>,
) -> Result<usize, String> {
    planning::selected_pages_for_path(path, kind == DocumentKind::Pdf, start, end)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Backend {
    Pipeline,
    VlmEngine,
    VlmHttpClient,
    HybridEngine,
    HybridHttpClient,
}

impl Backend {
    pub(crate) fn parse(value: &str) -> Result<Self, String> {
        match value {
            "pipeline" => Ok(Self::Pipeline),
            "vlm-engine" | "vlm-auto-engine" => Ok(Self::VlmEngine),
            "vlm-http-client" => Ok(Self::VlmHttpClient),
            "hybrid-engine" | "hybrid-auto-engine" => Ok(Self::HybridEngine),
            "hybrid-http-client" => Ok(Self::HybridHttpClient),
            _ => Err(format!("unsupported backend: {value}")),
        }
    }

    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Pipeline => "pipeline",
            Self::VlmEngine => "vlm-engine",
            Self::VlmHttpClient => "vlm-http-client",
            Self::HybridEngine => "hybrid-engine",
            Self::HybridHttpClient => "hybrid-http-client",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ParseMethod {
    Auto,
    Txt,
    Ocr,
}
impl ParseMethod {
    pub(crate) fn parse(value: &str) -> Result<Self, String> {
        match value {
            "auto" => Ok(Self::Auto),
            "txt" => Ok(Self::Txt),
            "ocr" => Ok(Self::Ocr),
            _ => Err(format!("unsupported method: {value}")),
        }
    }
    const fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Txt => "txt",
            Self::Ocr => "ocr",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Effort {
    Medium,
    High,
}
impl Effort {
    pub(crate) fn parse(value: &str) -> Result<Self, String> {
        match value {
            "medium" => Ok(Self::Medium),
            "high" => Ok(Self::High),
            _ => Err(format!("unsupported effort: {value}")),
        }
    }
    const fn as_str(self) -> &'static str {
        match self {
            Self::Medium => "medium",
            Self::High => "high",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RemoteOptions {
    pub(crate) lang: String,
    pub(crate) backend: Backend,
    pub(crate) method: ParseMethod,
    pub(crate) effort: Effort,
    pub(crate) formula: bool,
    pub(crate) table: bool,
    pub(crate) image_analysis: bool,
    pub(crate) server_url: Option<String>,
    pub(crate) start: u64,
    pub(crate) end: Option<u64>,
    pub(crate) client_side: bool,
}
impl Default for RemoteOptions {
    fn default() -> Self {
        Self {
            lang: "ch".into(),
            backend: Backend::HybridEngine,
            method: ParseMethod::Auto,
            effort: Effort::Medium,
            formula: true,
            table: true,
            image_analysis: true,
            server_url: None,
            start: 0,
            end: None,
            client_side: false,
        }
    }
}

/// Ordered form pairs preserve repeated multipart fields and their canonical order.
pub(crate) fn request_form(options: &RemoteOptions) -> Vec<(String, String)> {
    let mut form = vec![
        ("lang_list".into(), options.lang.clone()),
        ("backend".into(), options.backend.as_str().into()),
        ("effort".into(), options.effort.as_str().into()),
        ("parse_method".into(), options.method.as_str().into()),
        ("formula_enable".into(), options.formula.to_string()),
        ("table_enable".into(), options.table.to_string()),
        ("image_analysis".into(), options.image_analysis.to_string()),
    ];
    form.extend([
        ("return_md".into(), (!options.client_side).to_string()),
        ("return_middle_json".into(), "true".into()),
        ("return_model_output".into(), "true".into()),
        (
            "return_content_list".into(),
            (!options.client_side).to_string(),
        ),
        ("return_images".into(), "true".into()),
        ("response_format_zip".into(), "true".into()),
        ("return_original_file".into(), "true".into()),
        (
            "client_side_output_generation".into(),
            options.client_side.to_string(),
        ),
        ("start_page_id".into(), options.start.to_string()),
        (
            "end_page_id".into(),
            options.end.unwrap_or(99_999).to_string(),
        ),
    ]);
    if let Some(url) = options.server_url.as_deref().filter(|v| !v.is_empty()) {
        form.push(("server_url".into(), url.into()));
    }
    form
}

pub(crate) fn normalize_api_url(url: &str) -> String {
    url.trim_end_matches('/').into()
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ServerHealth {
    pub(crate) base_url: String,
    pub(crate) max_concurrent_requests: usize,
    pub(crate) processing_window_size: usize,
}
pub(crate) fn validate_health(base_url: &str, value: &Value) -> Result<ServerHealth, String> {
    let object = value
        .as_object()
        .ok_or("health payload must be an object")?;
    if object.get("status").and_then(Value::as_str) != Some("healthy") {
        return Err("server is not healthy".into());
    }
    let integer = |name: &str| {
        object
            .get(name)
            .and_then(Value::as_i64)
            .ok_or_else(|| format!("health {name} must be an integer"))
    };
    if integer("protocol_version")? != 2 {
        return Err("unsupported protocol version".into());
    }
    let max = integer("max_concurrent_requests")?;
    if max <= 0 {
        return Err("health max_concurrent_requests must be positive".into());
    }
    let processing_window_size = usize::try_from(integer("processing_window_size")?.max(1))
        .map_err(|_| "health processing_window_size is too large")?;
    Ok(ServerHealth {
        base_url: base_url.into(),
        max_concurrent_requests: usize::try_from(max)
            .map_err(|_| "health max_concurrent_requests is too large")?,
        processing_window_size,
    })
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct InputDocument {
    pub(crate) path: PathBuf,
    pub(crate) suffix: String,
    pub(crate) stem: String,
    pub(crate) effective_pages: usize,
    pub(crate) order: usize,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PlannedTask {
    pub(crate) index: usize,
    pub(crate) documents: Vec<InputDocument>,
    pub(crate) total_pages: usize,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SubmitResponse {
    pub(crate) task_id: String,
    pub(crate) status_url: String,
    pub(crate) result_url: String,
    pub(crate) file_names: Vec<String>,
    pub(crate) queued_ahead: Option<i64>,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct StatusSnapshot {
    pub(crate) status: String,
    pub(crate) queued_ahead: Option<i64>,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct TaskFailure {
    pub(crate) task_index: usize,
    pub(crate) document_stems: Vec<String>,
    pub(crate) message: String,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct RemoteEnv {
    pub(crate) max_concurrent_requests: usize,
    pub(crate) result_timeout_seconds: f64,
    pub(crate) download_timeout_seconds: f64,
}
impl From<RemoteApiEnv> for RemoteEnv {
    fn from(v: RemoteApiEnv) -> Self {
        Self {
            max_concurrent_requests: v.max_concurrent_requests,
            result_timeout_seconds: v.result_timeout_seconds,
            download_timeout_seconds: v.download_timeout_seconds,
        }
    }
}
impl From<RemoteEnv> for RemoteApiEnv {
    fn from(v: RemoteEnv) -> Self {
        Self {
            max_concurrent_requests: v.max_concurrent_requests,
            result_timeout_seconds: v.result_timeout_seconds,
            download_timeout_seconds: v.download_timeout_seconds,
        }
    }
}
pub(crate) fn parse_remote_env(get: impl Fn(&str) -> Option<String>) -> Result<RemoteEnv, String> {
    let max = match get("MINERU_API_MAX_CONCURRENT_REQUESTS") {
        None => 3,
        Some(value) => value
            .trim()
            .parse::<usize>()
            .ok()
            .filter(|v| *v > 0)
            .ok_or("MINERU_API_MAX_CONCURRENT_REQUESTS must be positive")?,
    };
    let timeout = |name: &str, default: f64| {
        get(name)
            .and_then(|v| v.trim().parse::<f64>().ok())
            .filter(|v| v.is_finite() && *v >= 1.0)
            .unwrap_or(default)
    };
    Ok(RemoteEnv {
        max_concurrent_requests: max,
        result_timeout_seconds: timeout("MINERU_TASK_RESULT_TIMEOUT_SECONDS", 3600.0),
        download_timeout_seconds: timeout("MINERU_TASK_RESULT_DOWNLOAD_TIMEOUT_SECONDS", 600.0),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{OfficialPdfOptions, VlmHttpConfig};
    use axum::{
        Json, Router,
        response::{IntoResponse, Response},
        routing::post,
    };
    use serde_json::json;
    use std::{fs, time::Duration};
    use tokio::sync::oneshot;

    #[tokio::test]
    async fn p3b_private_client_composes_with_vlm_api_service() {
        async fn chat(Json(request): Json<serde_json::Value>) -> Response {
            let content = if request.to_string().contains("Layout Detection") {
                "<|box_start|>0 0 1000 1000<|box_end|><|ref_start|>text<|ref_end|><|rotate_up|>"
            } else {
                "recognized"
            };
            Json(json!({"choices":[{"finish_reason":"stop","message":{"content":content}}]}))
                .into_response()
        }

        let model_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let model_url = format!("http://{}", model_listener.local_addr().unwrap());
        let (model_stop, model_stopped) = oneshot::channel();
        let model_task = tokio::spawn(async move {
            axum::serve(
                model_listener,
                Router::new().route("/v1/chat/completions", post(chat)),
            )
            .with_graceful_shutdown(async move {
                let _ = model_stopped.await;
            })
            .await
        });

        let root = tempfile::tempdir().unwrap();
        let service_output = root.path().join("tasks");
        let service_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let service_url = format!("http://{}", service_listener.local_addr().unwrap());
        let (service_stop, service_stopped) = oneshot::channel();
        let service_config = crate::vlm_api::ServiceConfig::new(
            1,
            service_output.clone(),
            OfficialPdfOptions::default(),
            None,
            None,
        )
        .unwrap()
        .test_http(VlmHttpConfig {
            model_name: Some("mock".into()),
            skip_model_name_checking: true,
            max_retries: 0,
            http_timeout: Duration::from_secs(5),
            connect_timeout: Duration::from_secs(1),
            ..Default::default()
        });
        let service_task = tokio::spawn(crate::vlm_api::serve(
            service_listener,
            service_config,
            async move {
                let _ = service_stopped.await;
            },
        ));

        let input = root.path().join("input.pdf");
        fs::copy(
            concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/fixtures/pdf/minimal.pdf"
            ),
            &input,
        )
        .unwrap();
        let options = RemoteOptions {
            backend: Backend::VlmHttpClient,
            server_url: Some(model_url),
            end: Some(0),
            ..Default::default()
        };
        let document = InputDocument {
            path: input,
            suffix: "pdf".into(),
            stem: "input".into(),
            effective_pages: 1,
            order: 0,
        };
        let client = http::MineruApiClient::new(&service_url).unwrap();
        let health = tokio::time::timeout(Duration::from_secs(5), client.health())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            (
                health.max_concurrent_requests,
                health.processing_window_size
            ),
            (1, 64)
        );
        let submitted =
            tokio::time::timeout(Duration::from_secs(5), client.submit(&options, &[document]))
                .await
                .unwrap()
                .unwrap();
        let env = RemoteEnv {
            max_concurrent_requests: health.max_concurrent_requests,
            result_timeout_seconds: 10.0,
            download_timeout_seconds: 10.0,
        };
        tokio::time::timeout(
            Duration::from_secs(12),
            client.poll(&submitted.status_url, env, None),
        )
        .await
        .unwrap()
        .unwrap();
        let archive = tokio::time::timeout(
            Duration::from_secs(12),
            client.download_result_zip(
                &submitted.result_url,
                &submitted.task_id,
                env,
                archive::ArchiveLimits::default(),
            ),
        )
        .await
        .unwrap()
        .unwrap();
        let extracted = root.path().join("extracted");
        archive
            .extract(&extracted, archive::ArchiveLimits::default())
            .unwrap();

        assert_eq!(
            fs::read_dir(&extracted)
                .unwrap()
                .map(|entry| entry.unwrap().file_name())
                .collect::<Vec<_>>(),
            ["input"]
        );
        let vlm = extracted.join("input/vlm");
        let mut files = fs::read_dir(&vlm)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect::<Vec<_>>();
        files.sort();
        assert_eq!(
            files,
            [
                "input.md",
                "input_content_list.json",
                "input_content_list_v2.json",
                "input_middle.json",
                "input_model.json",
                "input_origin.pdf"
            ]
        );
        assert!(
            !["layout", "staging", "upload", "partial"]
                .iter()
                .any(|name| extracted.join(name).exists())
        );
        assert!(!fs::read(vlm.join("input.md")).unwrap().is_empty());
        for name in [
            "input_middle.json",
            "input_model.json",
            "input_content_list.json",
            "input_content_list_v2.json",
        ] {
            let _: serde_json::Value =
                serde_json::from_slice(&fs::read(vlm.join(name)).unwrap()).unwrap();
        }
        assert_eq!(
            lopdf::Document::load(vlm.join("input_origin.pdf"))
                .unwrap()
                .get_pages()
                .len(),
            1
        );

        service_stop.send(()).unwrap();
        assert!(
            tokio::time::timeout(Duration::from_secs(5), service_task)
                .await
                .unwrap()
                .unwrap()
                .is_ok()
        );
        assert_eq!(fs::read_dir(&service_output).unwrap().count(), 0);
        model_stop.send(()).unwrap();
        assert!(
            tokio::time::timeout(Duration::from_secs(5), model_task)
                .await
                .unwrap()
                .unwrap()
                .is_ok()
        );
    }

    #[test]
    fn domain_form_health_and_env_are_exact() {
        for (name, backend) in [
            ("pipeline", Backend::Pipeline),
            ("vlm-engine", Backend::VlmEngine),
            ("vlm-http-client", Backend::VlmHttpClient),
            ("hybrid-engine", Backend::HybridEngine),
            ("hybrid-http-client", Backend::HybridHttpClient),
            ("vlm-auto-engine", Backend::VlmEngine),
            ("hybrid-auto-engine", Backend::HybridEngine),
        ] {
            assert_eq!(Backend::parse(name), Ok(backend));
        }
        assert_eq!(Backend::Pipeline.as_str(), "pipeline");
        assert_eq!(Backend::VlmEngine.as_str(), "vlm-engine");
        assert_eq!(Backend::VlmHttpClient.as_str(), "vlm-http-client");
        assert_eq!(Backend::HybridEngine.as_str(), "hybrid-engine");
        assert_eq!(Backend::HybridHttpClient.as_str(), "hybrid-http-client");
        assert!(Backend::parse("no").is_err());
        for (name, method) in [
            ("auto", ParseMethod::Auto),
            ("txt", ParseMethod::Txt),
            ("ocr", ParseMethod::Ocr),
        ] {
            assert_eq!(ParseMethod::parse(name), Ok(method));
        }
        assert_eq!(ParseMethod::Auto.as_str(), "auto");
        assert_eq!(ParseMethod::Txt.as_str(), "txt");
        assert_eq!(ParseMethod::Ocr.as_str(), "ocr");
        assert!(ParseMethod::parse("x").is_err());
        assert_eq!(Effort::parse("medium"), Ok(Effort::Medium));
        assert_eq!(Effort::parse("high"), Ok(Effort::High));
        assert_eq!(Effort::Medium.as_str(), "medium");
        assert_eq!(Effort::High.as_str(), "high");
        assert!(Effort::parse("low").is_err());
        assert_eq!(
            RemoteOptions::default(),
            RemoteOptions {
                lang: "ch".into(),
                backend: Backend::HybridEngine,
                method: ParseMethod::Auto,
                effort: Effort::Medium,
                formula: true,
                table: true,
                image_analysis: true,
                server_url: None,
                start: 0,
                end: None,
                client_side: false
            }
        );
        let mut o = RemoteOptions {
            lang: "en".into(),
            server_url: Some(" ".into()),
            client_side: false,
            ..Default::default()
        };
        assert_eq!(normalize_api_url("https://api.test///"), "https://api.test");
        assert_eq!(
            request_form(&o),
            vec![
                ("lang_list", "en"),
                ("backend", "hybrid-engine"),
                ("effort", "medium"),
                ("parse_method", "auto"),
                ("formula_enable", "true"),
                ("table_enable", "true"),
                ("image_analysis", "true"),
                ("return_md", "true"),
                ("return_middle_json", "true"),
                ("return_model_output", "true"),
                ("return_content_list", "true"),
                ("return_images", "true"),
                ("response_format_zip", "true"),
                ("return_original_file", "true"),
                ("client_side_output_generation", "false"),
                ("start_page_id", "0"),
                ("end_page_id", "99999"),
                ("server_url", " ")
            ]
            .into_iter()
            .map(|(a, b)| (a.into(), b.into()))
            .collect::<Vec<_>>()
        );
        o.server_url = None;
        o.client_side = true;
        let true_form = request_form(&o);
        assert_eq!(
            true_form
                .iter()
                .map(|(k, v)| (k.as_str(), v.as_str()))
                .collect::<Vec<_>>(),
            vec![
                ("lang_list", "en"),
                ("backend", "hybrid-engine"),
                ("effort", "medium"),
                ("parse_method", "auto"),
                ("formula_enable", "true"),
                ("table_enable", "true"),
                ("image_analysis", "true"),
                ("return_md", "false"),
                ("return_middle_json", "true"),
                ("return_model_output", "true"),
                ("return_content_list", "false"),
                ("return_images", "true"),
                ("response_format_zip", "true"),
                ("return_original_file", "true"),
                ("client_side_output_generation", "true"),
                ("start_page_id", "0"),
                ("end_page_id", "99999")
            ]
        );
        let health = validate_health("https://api.test", &json!({"status":"healthy","protocol_version":2,"max_concurrent_requests":2,"processing_window_size":0})).unwrap();
        assert_eq!(health.base_url, "https://api.test");
        assert_eq!(health.processing_window_size, 1);
        assert_eq!(validate_health("x", &json!({"status":"healthy","protocol_version":2,"max_concurrent_requests":2,"processing_window_size":-3})).unwrap().processing_window_size, 1);
        for bad in [
            json!({}),
            json!({"status":"healthy","protocol_version":true,"max_concurrent_requests":2,"processing_window_size":1}),
            json!({"status":"healthy","protocol_version":2,"max_concurrent_requests":0,"processing_window_size":1}),
            json!({"status":"healthy","protocol_version":2,"max_concurrent_requests":2,"processing_window_size":true}),
        ] {
            assert!(validate_health("x", &bad).is_err());
        }
        assert_eq!(
            parse_remote_env(|_| None).unwrap(),
            RemoteEnv {
                max_concurrent_requests: 3,
                result_timeout_seconds: 3600.,
                download_timeout_seconds: 600.
            }
        );
        assert!(
            parse_remote_env(|n| (n == "MINERU_API_MAX_CONCURRENT_REQUESTS").then(|| "0".into()))
                .is_err()
        );
        assert!(
            parse_remote_env(|n| (n == "MINERU_API_MAX_CONCURRENT_REQUESTS").then(|| "bad".into()))
                .is_err()
        );
        let env = parse_remote_env(|n| {
            Some(
                match n {
                    "MINERU_API_MAX_CONCURRENT_REQUESTS" => " 4 ",
                    "MINERU_TASK_RESULT_TIMEOUT_SECONDS" => " 1.5 ",
                    _ => " 2.5 ",
                }
                .into(),
            )
        })
        .unwrap();
        assert_eq!(
            (
                env.max_concurrent_requests,
                env.result_timeout_seconds,
                env.download_timeout_seconds
            ),
            (4, 1.5, 2.5)
        );
        let fallback = parse_remote_env(|n| {
            Some(
                match n {
                    "MINERU_API_MAX_CONCURRENT_REQUESTS" => "4",
                    "MINERU_TASK_RESULT_TIMEOUT_SECONDS" => "0.5",
                    _ => "nan",
                }
                .into(),
            )
        })
        .unwrap();
        assert_eq!(
            (
                fallback.result_timeout_seconds,
                fallback.download_timeout_seconds
            ),
            (3600., 600.)
        );
    }
}
