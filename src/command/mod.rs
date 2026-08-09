//! Shared high-level command execution.

mod direct;
#[doc(hidden)]
pub mod env;
#[doc(hidden)]
pub mod plain;
mod rich;
#[doc(hidden)]
pub mod service;

use crate::{
    OfficeWorkers, ProgressCallback, ProgressEvent, RemoteApiDocument, RemoteApiOptions,
    normalize_remote_language, sanitize_event_text, selected_document_pages,
};
use clap::{ArgAction, CommandFactory, FromArgMatches, Parser, error::ErrorKind};
use std::{
    collections::BTreeMap,
    ffi::OsString,
    io::IsTerminal,
    panic::{AssertUnwindSafe, catch_unwind},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

const WARNING_CAP: usize = 64;
const TEXT_CAP: usize = 512;
const FAILURE_CAP: usize = 4096;
const ENV_NAMES: [&str; 92] = [
    "MINERU_LOG_LEVEL",
    "MINERU_PROCESSING_WINDOW_SIZE",
    "MINERU_OFFICIAL_PAGE_CONCURRENCY",
    "MINERU_PDF_RENDER_THREADS",
    "MINERU_PDF_RENDER_TIMEOUT",
    "MINERU_FORMULA_ENABLE",
    "MINERU_TABLE_ENABLE",
    "MINERU_IMAGE_ANALYSIS_ENABLE",
    "MINERU_API_MAX_CONCURRENT_REQUESTS",
    "MINERU_TASK_RESULT_TIMEOUT_SECONDS",
    "MINERU_TASK_RESULT_DOWNLOAD_TIMEOUT_SECONDS",
    "MINERU_API_TASK_RETENTION_SECONDS",
    "MINERU_API_TASK_CLEANUP_INTERVAL_SECONDS",
    "MINERU_API_RECORD_CAP",
    "MINERU_API_FILE_CAP",
    "MINERU_API_BODY_CAP",
    "MINERU_API_TEXT_CAP",
    "MINERU_API_TEXT_TOTAL_CAP",
    "MINERU_API_FORM_FIELDS_CAP",
    "MINERU_API_CONNECT_TIMEOUT_SECONDS",
    "MINERU_API_ACQUISITION_TIMEOUT_SECONDS",
    "MINERU_API_SEND_TIMEOUT_SECONDS",
    "MINERU_API_POLL_INTERVAL_SECONDS",
    "MINERU_ARCHIVE_MAX_ENTRIES",
    "MINERU_ARCHIVE_MAX_RATIO",
    "MINERU_ZIP_SCAN_CENTRAL_CAP",
    "MINERU_ZIP_SCAN_NAME_CAP",
    "MINERU_ZIP_SCAN_DEPTH_CAP",
    "MINERU_ZIP_SCAN_TOTAL_NAME_CAP",
    "MINERU_ZIP_SCAN_TOTAL_COMPONENT_CAP",
    "MINERU_OOXML_ARCHIVE_BYTES",
    "MINERU_OOXML_EXPANDED_BYTES",
    "MINERU_OOXML_XML_ENTRY_BYTES",
    "MINERU_OOXML_XML_TOTAL_BYTES",
    "MINERU_OOXML_RATIO",
    "MINERU_OOXML_XML_DEPTH",
    "MINERU_OOXML_XML_EVENTS",
    "MINERU_OOXML_XML_ATTRIBUTES",
    "MINERU_OOXML_XML_NAMESPACES",
    "MINERU_OFFICE_INPUT_BYTES",
    "MINERU_OFFICE_OUTPUT_BYTES",
    "MINERU_OFFICE_STDERR_BYTES",
    "MINERU_OFFICE_WALL_SECONDS",
    "MINERU_OFFICE_CPU_SECONDS",
    "MINERU_OFFICE_NOFILE",
    "MINERU_OFFICE_ADDRESS_SPACE_BYTES",
    "MINERU_OFFICE_ACTIVE_PROCESS_LIMIT",
    "MINERU_OFFICE_PROCESS_MEMORY_BYTES",
    "MINERU_OFFICE_JOB_MEMORY_BYTES",
    "MINERU_OFFICE_PROCESS_TIME_SECONDS",
    "MINERU_OFFICE_JOB_TIME_SECONDS",
    "MINERU_VL_SERVER",
    "MINERU_VL_MODEL_NAME",
    "MINERU_VL_API_KEY",
    "MINERU_VL_DEBUG_ENABLE",
    "MINERU_VLM_END_TOKEN",
    "MINERU_VLM_TEXT_BEFORE_IMAGE",
    "MINERU_VLM_ALLOW_TRUNCATED_CONTENT",
    "MINERU_VLM_ALLOW_REMOTE_IMAGES",
    "MINERU_VLM_ALLOW_PRIVATE_REMOTE_IMAGES",
    "MINERU_MAX_INPUT_BYTES",
    "MINERU_MAX_ENCODED_DOCUMENT_BYTES",
    "MINERU_MAX_OUTPUT_BYTES",
    "MINERU_MAX_PDF_BYTES",
    "MINERU_MAX_PAGES",
    "MINERU_MAX_PAGE_PIXELS",
    "MINERU_MAX_RENDERED_IMAGE_BYTES",
    "MINERU_MAX_IN_FLIGHT_IMAGE_BYTES",
    "MINERU_MAX_RAW_OUTPUT_BYTES",
    "MINERU_MAX_LAYOUT_BLOCKS_PER_PAGE",
    "MINERU_MAX_SEMANTIC_REQUESTS_PER_PAGE",
    "MINERU_BATCH_SIZE",
    "MINERU_MAX_ENCODED_REQUEST_BYTES",
    "MINERU_MAX_ENCODED_BATCH_BYTES",
    "MINERU_MAX_TOTAL_ASSET_BYTES",
    "MINERU_MAX_STAGED_TEXT_BYTES",
    "MINERU_TOTAL_DEADLINE_SECONDS",
    "MINERU_VLM_HTTP_CONCURRENCY",
    "MINERU_VLM_HTTP_TIMEOUT",
    "MINERU_VLM_CONNECT_TIMEOUT",
    "MINERU_VLM_HTTP_MAX_KEEPALIVE_CONNECTIONS",
    "MINERU_VLM_HTTP_KEEPALIVE_EXPIRY",
    "MINERU_VLM_HTTP_MAX_RETRIES",
    "MINERU_VLM_HTTP_RETRY_BACKOFF_FACTOR",
    "MINERU_VLM_MAX_IMAGE_BYTES",
    "MINERU_VLM_MAX_DECODED_PIXELS",
    "MINERU_VLM_MAX_IMAGES_PER_REQUEST",
    "MINERU_VLM_MAX_REDIRECTS",
    "MINERU_VLM_HTTP_MAX_RESPONSE_BYTES",
    "TERM",
    "CI",
    "NO_COLOR",
];

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) struct DocumentId(pub(crate) usize);

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) struct ApiTaskId(pub(crate) usize);

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) enum CommandScope {
    Document(DocumentId),
    ApiTask(ApiTaskId),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum CommandEvent {
    RunPlanned {
        documents: usize,
        api_tasks: usize,
    },
    Progress {
        scope: CommandScope,
        event: ProgressEvent,
    },
    RunCompleted,
    RunFailed {
        message: String,
    },
}

pub(crate) type CommandCallback = Arc<dyn Fn(CommandEvent) + Send + Sync + 'static>;

pub(crate) fn emit_command(callback: &Option<CommandCallback>, event: CommandEvent) {
    if let Some(callback) = callback {
        let _ = catch_unwind(AssertUnwindSafe(|| callback(event)));
    }
}

pub(crate) fn scoped_progress(
    callback: Option<CommandCallback>,
    scope: CommandScope,
) -> ProgressCallback {
    Arc::new(move |event| emit_command(&callback, CommandEvent::Progress { scope, event }))
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RunOptions {
    pub path: PathBuf,
    pub output: PathBuf,
    pub api_url: Option<String>,
    pub api_key: Option<String>,
    pub method: String,
    pub backend: String,
    pub effort: String,
    pub lang: String,
    pub url: Option<String>,
    pub start: usize,
    pub end: Option<usize>,
    pub formula: bool,
    pub table: bool,
    pub image_analysis: bool,
    pub client_side_output_generation: bool,
}

impl RunOptions {
    pub fn new(path: impl Into<PathBuf>, output: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            output: output.into(),
            api_url: None,
            api_key: None,
            method: "auto".into(),
            backend: "vlm-http-client".into(),
            effort: "medium".into(),
            lang: "ch".into(),
            url: None,
            start: 0,
            end: None,
            formula: true,
            table: true,
            image_analysis: true,
            client_side_output_generation: false,
        }
    }
}

/// Crate-private resolved CLI override seam. Public entry points (`run`, `run_with_context`)
/// use default overrides; the canonical CLI builds this from its Clap surface.
#[derive(Clone, Debug, Default)]
pub(crate) struct RunOverrides {
    pub document_limits: crate::DocumentLimitOverrides,
    pub core: env::CoreOverrides,
    pub service: service::ServiceOverrides,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct RunReport {
    pub warnings: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
#[error("{0}")]
pub struct RunError(String);

impl RunError {
    fn new(error: impl std::fmt::Display) -> Self {
        Self(sanitize_event_text(&error.to_string(), FAILURE_CAP))
    }
}

#[derive(Clone)]
pub(super) struct Environment(Arc<BTreeMap<&'static str, OsString>>);

impl Environment {
    fn process() -> Self {
        Self(Arc::new(
            ENV_NAMES
                .into_iter()
                .filter_map(|name| std::env::var_os(name).map(|value| (name, value)))
                .collect(),
        ))
    }

    pub(super) fn os(&self, name: &str) -> Option<OsString> {
        self.0.get(name).cloned()
    }

    pub(super) fn string(&self, name: &str) -> Option<String> {
        self.0.get(name)?.to_str().map(str::to_owned)
    }

    #[cfg(test)]
    pub(super) fn from_values(values: std::collections::HashMap<&'static str, OsString>) -> Self {
        Self(Arc::new(values.into_iter().collect()))
    }
}

/// Execution context for embeddings whose Office helper is not beside the calling program.
#[derive(Clone)]
pub struct RunContext {
    office_executable: PathBuf,
    environment: Environment,
    events: Option<ProgressCallback>,
    command_events: Option<CommandCallback>,
    warnings: Option<direct::WarningCallback>,
}

impl RunContext {
    /// Creates a context with an explicit Office helper executable.
    ///
    /// `path` must be absolute and point to a trusted `mineru-office-convert[.exe]`.
    pub fn with_office_executable(path: PathBuf) -> Result<Self, RunError> {
        if !path.is_absolute() {
            return Err(RunError::new("office helper path must be absolute"));
        }
        Ok(Self {
            office_executable: path,
            environment: Environment::process(),
            events: None,
            command_events: None,
            warnings: None,
        })
    }

    fn process_default() -> Result<Self, RunError> {
        let executable = std::env::current_exe().map_err(RunError::new)?;
        Self::with_office_executable(executable.with_file_name(if cfg!(windows) {
            "mineru-office-convert.exe"
        } else {
            "mineru-office-convert"
        }))
    }

    #[cfg(test)]
    fn office_workers(&self) -> OfficeWorkers {
        OfficeWorkers::with_executable(self.office_executable.clone())
    }

    fn office_workers_with_policy(
        &self,
        limits: service::OfficeLimits,
        ooxml: service::OoxmlLimits,
    ) -> OfficeWorkers {
        OfficeWorkers::with_executable_and_policy(self.office_executable.clone(), limits, ooxml)
    }

    fn with_output(mut self, events: CommandCallback, warnings: direct::WarningCallback) -> Self {
        self.command_events = Some(events);
        self.warnings = Some(warnings);
        self
    }
}

pub async fn run(options: RunOptions) -> Result<RunReport, RunError> {
    run_with_context(options, RunContext::process_default()?).await
}

/// Executes a high-level command using the explicit context.
pub async fn run_with_context(
    options: RunOptions,
    context: RunContext,
) -> Result<RunReport, RunError> {
    let collected = Arc::new(Mutex::new(WarningCollector::default()));
    let events = combined_command_events(
        context.events.clone(),
        context.command_events.clone(),
        Arc::clone(&collected),
    );
    let warnings = combined_warnings(context.warnings.clone(), Arc::clone(&collected));
    // The public entry treats `RunOptions` boolean fields as explicit inputs with the same
    // strict default -> frozen environment -> explicit precedence as the canonical CLI.
    let overrides = RunOverrides {
        core: env::CoreOverrides {
            formula: Some(options.formula),
            table: Some(options.table),
            image_analysis: Some(options.image_analysis),
            ..Default::default()
        },
        ..Default::default()
    };
    run_core(options, &context, overrides, events, warnings).await?;
    Ok(RunReport {
        warnings: collected
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .warnings
            .clone(),
    })
}

#[derive(Default)]
struct WarningCollector {
    warnings: Vec<String>,
    truncated: bool,
}

impl WarningCollector {
    fn push(&mut self, source: &str, message: &str) {
        if self.warnings.len() < WARNING_CAP {
            self.warnings.push(sanitize_event_text(
                &format!("{source}: {message}"),
                TEXT_CAP,
            ));
        } else if !self.truncated {
            self.truncated = true;
            self.warnings.push("warnings truncated".into());
        }
    }
}

fn combined_warnings(
    output: Option<direct::WarningCallback>,
    collected: Arc<Mutex<WarningCollector>>,
) -> direct::WarningCallback {
    Arc::new(move |source, message| {
        collected
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(source, message);
        if let Some(output) = &output {
            output(source, message);
        }
    })
}

fn combined_command_events(
    unscoped_output: Option<ProgressCallback>,
    command_output: Option<CommandCallback>,
    collected: Arc<Mutex<WarningCollector>>,
) -> CommandCallback {
    Arc::new(move |command_event| {
        if let CommandEvent::Progress { event, .. } = &command_event {
            match event {
                ProgressEvent::OfficeWarning { document, message } => collected
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .push(&format!("office warning: {document}"), message),
                ProgressEvent::VlmWarning { message } => collected
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .push("vlm warning", message),
                ProgressEvent::ApiWarning { label, message } => collected
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .push(&format!("api warning: {label}"), message),
                _ => {}
            }
            if let Some(output) = &unscoped_output {
                output(event.clone());
            }
        }
        if let Some(output) = &command_output {
            output(command_event);
        }
    })
}

async fn run_core(
    options: RunOptions,
    context: &RunContext,
    overrides: RunOverrides,
    events: CommandCallback,
    warnings: direct::WarningCallback,
) -> Result<(), RunError> {
    if options.api_url.is_some()
        && (overrides
            .document_limits
            .max_encoded_document_bytes
            .is_some()
            || context
                .environment
                .os("MINERU_MAX_ENCODED_DOCUMENT_BYTES")
                .is_some())
    {
        return Err(RunError::new(
            "max encoded document bytes cannot configure a remote server; configure the server",
        ));
    }
    if options.api_url.is_some()
        && let Some(message) = remote_local_transport_error(&overrides.core, &context.environment)
    {
        return Err(RunError::new(message));
    }
    if options.api_url.is_some()
        && let Some(message) =
            remote_local_service_transport_error(&overrides.service, &context.environment)
    {
        return Err(RunError::new(message));
    }
    let document_limits = crate::DocumentLimitPolicy::resolve(&overrides.document_limits, |name| {
        context.environment.os(name)
    })
    .map_err(RunError::new)?;
    let mut resolved = env::resolve_core(|name| context.environment.os(name), &overrides.core)
        .map_err(RunError::new)?;
    // Remote-only Phase-1B controls cannot act in direct mode; server request caps are owned by
    // the task-service CLI and cannot act from this client at all. No behaviorless configuration.
    if options.api_url.is_none()
        && let Some(message) = remote_only_service_error(&overrides.service, &context.environment)
    {
        return Err(RunError::new(message));
    }
    if let Some(message) = server_owned_error(&overrides.service, &context.environment) {
        return Err(RunError::new(message));
    }
    let service = service::resolve_service(
        &(|name| context.environment.os(name)),
        &overrides.service,
        document_limits,
    )
    .map_err(RunError::new)?;
    // Resolved local VLM transport booleans feed the HTTP config with the same strict
    // default -> environment -> CLI precedence as every other transport knob.
    resolved.http.text_before_image = service.vlm_text_before_image;
    resolved.http.allow_truncated_content = service.vlm_allow_truncated_content;
    resolved.http.allow_remote_images = service.vlm_allow_remote_images;
    resolved.http.allow_private_remote_images = service.vlm_allow_private_remote_images;
    let office_workers = context.office_workers_with_policy(service.office, service.ooxml);
    if !matches!(options.method.as_str(), "auto" | "txt" | "ocr") {
        return Err(RunError::new(format!(
            "unsupported method: {}",
            options.method
        )));
    }
    if options.backend != "vlm-http-client" {
        return Err(RunError::new(format!(
            "unsupported backend: {}",
            options.backend
        )));
    }
    if !matches!(options.effort.as_str(), "medium" | "high") {
        return Err(RunError::new(format!(
            "unsupported effort: {}",
            options.effort
        )));
    }
    let language = normalize_remote_language(&options.lang).map_err(RunError::new)?;
    if options.api_url.is_none()
        && options.end.is_some_and(|end| end < options.start)
        && has_pdf_input(&options.path)
    {
        return Err(RunError::new("--end must not be less than --start"));
    }
    if options.api_url.is_none()
        && let Some(message) = behaviorless_warning(&options)
    {
        warnings("ignored direct options", &message);
    }
    if let Some(api_url) = options.api_url.clone() {
        run_api(
            options,
            api_url,
            language,
            document_limits,
            resolved,
            service,
            office_workers,
            context,
            events,
            warnings,
        )
        .await
    } else {
        direct::run_with_scoped_events(
            direct::DirectOptions {
                input: options.path,
                output: options.output,
                base_url: options.url,
                server_option_label: "--url",
                model: None,
                api_key: options.api_key.clone(),
                page_start: Some(options.start),
                page_end: options.end,
                // Formula/table/image-analysis resolve through CoreOverrides with strict
                // default -> frozen environment -> explicit CLI precedence, so the canonical
                // runner does not re-apply option-derived booleans.
                no_formula: None,
                no_table: None,
                no_image_analysis: None,
                document_limits,
            },
            office_workers,
            context.environment.clone(),
            overrides,
            service,
            Some(events),
            Some(warnings),
        )
        .await
        .map_err(RunError::new)
    }
}

async fn run_api(
    options: RunOptions,
    api_url: String,
    language: String,
    document_limits: crate::DocumentLimitPolicy,
    resolved: env::ResolvedCore,
    service: service::ResolvedService,
    office_workers: OfficeWorkers,
    context: &RunContext,
    events: CommandCallback,
    warnings: direct::WarningCallback,
) -> Result<(), RunError> {
    if options.client_side_output_generation {
        return Err(RunError::new(
            "client-side output generation is unsupported",
        ));
    }
    // The frozen startup snapshot is the only timing/concurrency source for the remote API
    // client; no worker re-reads a drifting process environment after startup.
    let env = crate::RemoteApiEnv {
        max_concurrent_requests: service.remote_concurrency,
        result_timeout_seconds: service.task_result_timeout.as_secs_f64(),
        download_timeout_seconds: service.task_download_timeout.as_secs_f64(),
    };
    let mut route = resolved.route;
    route.start_page = options.start;
    route.end_page = options.end;
    // Formula/table/image-analysis arrive already resolved with strict default -> frozen
    // environment -> explicit CLI precedence from `resolve_core`; no late env fallback.
    let start =
        u64::try_from(options.start).map_err(|_| RunError::new("page start exceeds u64"))?;
    let end = options
        .end
        .map(|value| u64::try_from(value).map_err(|_| RunError::new("page end exceeds u64")))
        .transpose()?;
    let (_, inputs, skipped) = direct::discover_inputs(&options.path).map_err(RunError::new)?;
    let stems = direct::allocate_input_stems(&inputs).map_err(RunError::new)?;
    for path in skipped {
        warnings("unsupported input", &path.display().to_string());
    }
    let documents = inputs
        .into_iter()
        .zip(stems)
        .enumerate()
        .map(|(order, ((path, kind), stem))| {
            let effective_pages =
                selected_document_pages(&path, kind, start, end).map_err(RunError::new)?;
            Ok(RemoteApiDocument {
                path,
                kind,
                stem,
                effective_pages,
                order,
            })
        })
        .collect::<Result<Vec<_>, RunError>>()?;
    let remote_options = RemoteApiOptions {
        backend: options.backend,
        method: options.method,
        effort: options.effort,
        language,
        server_url: options
            .url
            .or_else(|| context.environment.string("MINERU_VL_SERVER")),
        api_key: options
            .api_key
            .or_else(|| context.environment.string("MINERU_VL_API_KEY")),
        start,
        end,
        formula: route.formula_enable,
        table: route.table_enable,
        image_analysis: route.image_analysis,
        client_side_output_generation: false,
        route,
    };
    let failures = crate::mineru_api::run_remote_api_documents_scoped_with_workers_and_policy(
        documents,
        options.output,
        api_url,
        remote_options,
        env,
        Some(events),
        office_workers,
        document_limits,
        resolved.http.max_response_bytes,
        service,
    )
    .await
    .map_err(RunError::new)?;
    if failures.is_empty() {
        Ok(())
    } else {
        Err(RunError::new(format_remote_failures(&failures)))
    }
}

fn format_remote_failures(failures: &[crate::RemoteApiFailure]) -> String {
    let details = failures
        .iter()
        .take(16)
        .map(|failure| {
            sanitize_event_text(
                &format!(
                    "task#{} [{}]: {}",
                    failure.task_index,
                    failure.document_stems.join(", "),
                    failure.message
                ),
                TEXT_CAP,
            )
        })
        .collect::<Vec<_>>()
        .join("; ");
    let suffix = (failures.len() > 16)
        .then_some("; additional failures truncated")
        .unwrap_or("");
    sanitize_event_text(
        &format!("{} API task(s) failed: {details}{suffix}", failures.len()),
        FAILURE_CAP,
    )
}

/// Local VLM transport controls accepted alongside a remote `--api-url` would silently do
/// nothing, because the remote server performs parsing and the client-side preview does not
/// consume these knobs. Reject them before any network or output work. `MINERU_VL_SERVER` is
/// exempt because `run_api` maps it to the submitted `server_url`, and the model/key credentials
/// are inert transport identity rather than behavior-loss configuration.
fn remote_local_transport_error(
    core: &env::CoreOverrides,
    environment: &Environment,
) -> Option<String> {
    let mut controls = Vec::new();
    let mut push = |flag: &str, env_name: &str, cli_set: bool| {
        if cli_set || environment.os(env_name).is_some() {
            controls.push(format!("{flag}/{env_name}"));
        }
    };
    push(
        "--page-concurrency",
        "MINERU_OFFICIAL_PAGE_CONCURRENCY",
        core.page_concurrency.is_some(),
    );
    push(
        "--processing-window-size",
        "MINERU_PROCESSING_WINDOW_SIZE",
        core.processing_window_size.is_some(),
    );
    push(
        "--render-workers",
        "MINERU_PDF_RENDER_THREADS",
        core.render_workers.is_some(),
    );
    push(
        "--render-timeout-seconds",
        "MINERU_PDF_RENDER_TIMEOUT",
        core.render_timeout.is_some(),
    );
    push(
        "--batch-size",
        "MINERU_BATCH_SIZE",
        core.batch_size.is_some(),
    );
    push(
        "--http-max-concurrency",
        "MINERU_VLM_HTTP_CONCURRENCY",
        core.http_max_concurrency.is_some(),
    );
    push(
        "--http-timeout-seconds",
        "MINERU_VLM_HTTP_TIMEOUT",
        core.http_timeout.is_some(),
    );
    push(
        "--connect-timeout-seconds",
        "MINERU_VLM_CONNECT_TIMEOUT",
        core.connect_timeout.is_some(),
    );
    push(
        "--http-max-keepalive-connections",
        "MINERU_VLM_HTTP_MAX_KEEPALIVE_CONNECTIONS",
        core.http_max_keepalive_connections.is_some(),
    );
    push(
        "--http-keepalive-expiry-seconds",
        "MINERU_VLM_HTTP_KEEPALIVE_EXPIRY",
        core.http_keepalive_expiry.is_some(),
    );
    push(
        "--http-max-retries",
        "MINERU_VLM_HTTP_MAX_RETRIES",
        core.http_max_retries.is_some(),
    );
    push(
        "--http-retry-backoff-factor",
        "MINERU_VLM_HTTP_RETRY_BACKOFF_FACTOR",
        core.http_retry_backoff_factor.is_some(),
    );
    push(
        "--max-remote-image-bytes",
        "MINERU_VLM_MAX_IMAGE_BYTES",
        core.max_remote_image_bytes.is_some(),
    );
    push(
        "--max-decoded-pixels",
        "MINERU_VLM_MAX_DECODED_PIXELS",
        core.max_decoded_pixels.is_some(),
    );
    push(
        "--max-images-per-request",
        "MINERU_VLM_MAX_IMAGES_PER_REQUEST",
        core.max_images_per_request.is_some(),
    );
    push(
        "--max-redirects",
        "MINERU_VLM_MAX_REDIRECTS",
        core.max_redirects.is_some(),
    );
    push(
        "--http-max-response-bytes",
        "MINERU_VLM_HTTP_MAX_RESPONSE_BYTES",
        core.http_max_response_bytes.is_some(),
    );
    push(
        "--vlm-debug",
        "MINERU_VL_DEBUG_ENABLE",
        core.vlm_debug.is_some(),
    );
    (!controls.is_empty()).then(|| {
        format!(
            "local VLM transport controls cannot configure a remote API server: {}",
            controls.join(", ")
        )
    })
}

/// Local VLM transport booleans resolved through `ServiceOverrides` share the same
/// remote-mode rejection as the core transport controls.
fn remote_local_service_transport_error(
    service: &service::ServiceOverrides,
    environment: &Environment,
) -> Option<String> {
    let mut controls = Vec::new();
    let mut push = |flag: &str, env_name: &str, cli_set: bool| {
        if cli_set || environment.os(env_name).is_some() {
            controls.push(format!("{flag}/{env_name}"));
        }
    };
    push(
        "--vlm-text-before-image",
        "MINERU_VLM_TEXT_BEFORE_IMAGE",
        service.vlm_text_before_image.is_some(),
    );
    push(
        "--vlm-allow-truncated-content",
        "MINERU_VLM_ALLOW_TRUNCATED_CONTENT",
        service.vlm_allow_truncated_content.is_some(),
    );
    push(
        "--vlm-allow-remote-images",
        "MINERU_VLM_ALLOW_REMOTE_IMAGES",
        service.vlm_allow_remote_images.is_some(),
    );
    push(
        "--vlm-allow-private-remote-images",
        "MINERU_VLM_ALLOW_PRIVATE_REMOTE_IMAGES",
        service.vlm_allow_private_remote_images.is_some(),
    );
    (!controls.is_empty()).then(|| {
        format!(
            "local VLM transport controls cannot configure a remote API server: {}",
            controls.join(", ")
        )
    })
}

/// Remote-only Phase-1B controls accepted alongside a direct run would silently do nothing,
/// because no remote API client, result archive, or task poller exists in direct mode. Reject
/// explicit/env remote-only controls before any output work; no behaviorless configuration.
pub(super) fn remote_only_service_error(
    service: &service::ServiceOverrides,
    environment: &Environment,
) -> Option<String> {
    let mut controls = Vec::new();
    let mut push = |flag: &str, env_name: &str, cli_set: bool| {
        if cli_set || environment.os(env_name).is_some() {
            controls.push(format!("{flag}/{env_name}"));
        }
    };
    push(
        "--api-max-concurrent-requests",
        "MINERU_API_MAX_CONCURRENT_REQUESTS",
        service.api_max_concurrent_requests.is_some(),
    );
    push(
        "--task-result-timeout-seconds",
        "MINERU_TASK_RESULT_TIMEOUT_SECONDS",
        service.task_result_timeout.is_some(),
    );
    push(
        "--task-result-download-timeout-seconds",
        "MINERU_TASK_RESULT_DOWNLOAD_TIMEOUT_SECONDS",
        service.task_download_timeout.is_some(),
    );
    push(
        "--api-connect-timeout-seconds",
        "MINERU_API_CONNECT_TIMEOUT_SECONDS",
        service.api_connect_timeout.is_some(),
    );
    push(
        "--api-acquisition-timeout-seconds",
        "MINERU_API_ACQUISITION_TIMEOUT_SECONDS",
        service.api_acquisition_timeout.is_some(),
    );
    push(
        "--api-send-timeout-seconds",
        "MINERU_API_SEND_TIMEOUT_SECONDS",
        service.api_send_timeout.is_some(),
    );
    push(
        "--api-poll-interval-seconds",
        "MINERU_API_POLL_INTERVAL_SECONDS",
        service.api_poll_interval.is_some(),
    );
    push(
        "--archive-max-entries",
        "MINERU_ARCHIVE_MAX_ENTRIES",
        service.archive_max_entries.is_some(),
    );
    push(
        "--archive-max-ratio",
        "MINERU_ARCHIVE_MAX_RATIO",
        service.archive_max_ratio.is_some(),
    );
    push(
        "--zip-scan-central-cap",
        "MINERU_ZIP_SCAN_CENTRAL_CAP",
        service.zip_central_cap.is_some(),
    );
    push(
        "--zip-scan-name-cap",
        "MINERU_ZIP_SCAN_NAME_CAP",
        service.zip_name_cap.is_some(),
    );
    push(
        "--zip-scan-depth-cap",
        "MINERU_ZIP_SCAN_DEPTH_CAP",
        service.zip_depth_cap.is_some(),
    );
    push(
        "--zip-scan-total-name-cap",
        "MINERU_ZIP_SCAN_TOTAL_NAME_CAP",
        service.zip_total_name_cap.is_some(),
    );
    push(
        "--zip-scan-total-component-cap",
        "MINERU_ZIP_SCAN_TOTAL_COMPONENT_CAP",
        service.zip_total_component_cap.is_some(),
    );
    (!controls.is_empty()).then(|| {
        format!(
            "remote API service controls cannot configure a direct run: {}",
            controls.join(", ")
        )
    })
}

/// Task-service request caps are owned by the `mineru-vlm-api` CLI. Explicit values reaching
/// this client could never act, so they are rejected before work.
pub(super) fn server_owned_error(
    service: &service::ServiceOverrides,
    environment: &Environment,
) -> Option<String> {
    let mut controls = Vec::new();
    let mut push = |env_name: &str, cli_set: bool| {
        if cli_set || environment.os(env_name).is_some() {
            controls.push(env_name.to_owned());
        }
    };
    push("MINERU_API_RECORD_CAP", service.server_record_cap.is_some());
    push("MINERU_API_FILE_CAP", service.server_file_cap.is_some());
    push("MINERU_API_BODY_CAP", service.server_body_cap.is_some());
    push("MINERU_API_TEXT_CAP", service.server_text_cap.is_some());
    push(
        "MINERU_API_TEXT_TOTAL_CAP",
        service.server_text_total_cap.is_some(),
    );
    push(
        "MINERU_API_FORM_FIELDS_CAP",
        service.server_form_fields_cap.is_some(),
    );
    push(
        "MINERU_API_TASK_RETENTION_SECONDS",
        service.task_retention.is_some(),
    );
    push(
        "MINERU_API_TASK_CLEANUP_INTERVAL_SECONDS",
        service.task_cleanup_interval.is_some(),
    );
    (!controls.is_empty()).then(|| {
        format!(
            "task-service request caps cannot configure this client; configure the server: {}",
            controls.join(", ")
        )
    })
}

fn behaviorless_warning(options: &RunOptions) -> Option<String> {
    let mut selected = Vec::new();
    if options.method != "auto" {
        selected.push(format!("method={}", options.method));
    }
    if options.effort != "medium" {
        selected.push(format!("effort={}", options.effort));
    }
    if options.lang != "ch" {
        selected.push(format!("lang={}", options.lang));
    }
    if options.client_side_output_generation {
        selected.push("client-side-output-generation=true".into());
    }
    (!selected.is_empty()).then(|| selected.join(", "))
}

fn has_pdf_input(path: &Path) -> bool {
    let is_pdf = |path: &Path| {
        path.extension()
            .is_some_and(|extension| extension.eq_ignore_ascii_case("pdf"))
    };
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return false;
    };
    if meta.is_file() {
        return is_pdf(path);
    }
    meta.is_dir()
        && std::fs::read_dir(path).is_ok_and(|entries| {
            entries.filter_map(Result::ok).any(|entry| {
                std::fs::symlink_metadata(entry.path()).is_ok_and(|meta| meta.is_file())
                    && is_pdf(&entry.path())
            })
        })
}

#[derive(Parser, Debug)]
#[command(
    about = "Parse PDF, image, and Office documents with the supported external VLM-HTTP subset (vlm-http-client only; no local engines).",
    version,
    disable_version_flag = true,
    after_help = "Environment:\n  MINERU_VL_SERVER      MinerU VLM service base URL, e.g. https://host/v1\n  MINERU_VL_MODEL_NAME  model id served by that endpoint\n  MINERU_VL_API_KEY     Bearer token; preferred over --api-key\n\nFull reference: docs/usage.en.md"
)]
pub struct Cli {
    #[arg(short = 'v', long, action = ArgAction::Version)]
    version: Option<bool>,
    #[arg(short = 'p', long)]
    path: PathBuf,
    #[arg(short = 'o', long)]
    output: PathBuf,
    #[arg(long)]
    api_url: Option<String>,
    #[arg(long)]
    api_key: Option<String>,
    #[arg(short = 'm', long, value_parser = ["auto", "txt", "ocr"], default_value = "auto")]
    method: String,
    #[arg(short = 'b', long, value_parser = ["vlm-http-client"], default_value = "vlm-http-client")]
    backend: String,
    #[arg(long, value_parser = ["medium", "high"], default_value = "medium")]
    effort: String,
    #[arg(short = 'l', long, value_parser = ["ch", "ch_server", "korean", "ta", "te", "ka", "th", "el", "arabic", "east_slavic", "cyrillic", "devanagari", "en", "japan", "chinese_cht", "latin"], default_value = "ch")]
    lang: String,
    #[arg(short = 'u', long)]
    url: Option<String>,
    #[arg(short = 's', long, default_value_t = 0, help = "Zero-based first page")]
    start: usize,
    #[arg(short = 'e', long, help = "Zero-based inclusive last page")]
    end: Option<usize>,
    #[arg(short = 'f', long, action = ArgAction::Set)]
    formula: Option<bool>,
    #[arg(short = 't', long, action = ArgAction::Set)]
    table: Option<bool>,
    #[arg(long, action = ArgAction::Set)]
    image_analysis: Option<bool>,
    #[arg(long, action = ArgAction::Set, default_value_t = false)]
    client_side_output_generation: bool,
    #[arg(long)]
    max_input_bytes: Option<String>,
    #[arg(long)]
    max_encoded_document_bytes: Option<String>,
    #[arg(long)]
    max_output_bytes: Option<String>,
    #[arg(long)]
    log_level: Option<String>,
    #[arg(long)]
    processing_window_size: Option<String>,
    #[arg(long)]
    page_concurrency: Option<String>,
    #[arg(long)]
    render_workers: Option<String>,
    #[arg(long)]
    render_timeout_seconds: Option<String>,
    #[arg(long)]
    max_pdf_bytes: Option<String>,
    #[arg(long)]
    max_pages: Option<String>,
    #[arg(long)]
    max_page_pixels: Option<String>,
    #[arg(long)]
    max_rendered_image_bytes: Option<String>,
    #[arg(long)]
    max_in_flight_image_bytes: Option<String>,
    #[arg(long)]
    max_raw_output_bytes: Option<String>,
    #[arg(long)]
    max_layout_blocks_per_page: Option<String>,
    #[arg(long)]
    max_semantic_requests_per_page: Option<String>,
    #[arg(long)]
    batch_size: Option<String>,
    #[arg(long)]
    max_encoded_request_bytes: Option<String>,
    #[arg(long)]
    max_encoded_batch_bytes: Option<String>,
    #[arg(long)]
    max_total_asset_bytes: Option<String>,
    #[arg(long)]
    max_staged_text_bytes: Option<String>,
    #[arg(long)]
    total_deadline_seconds: Option<String>,
    #[arg(long)]
    http_max_concurrency: Option<String>,
    #[arg(long)]
    http_timeout_seconds: Option<String>,
    #[arg(long)]
    connect_timeout_seconds: Option<String>,
    #[arg(long)]
    http_max_keepalive_connections: Option<String>,
    #[arg(long)]
    http_keepalive_expiry_seconds: Option<String>,
    #[arg(long)]
    http_max_retries: Option<String>,
    #[arg(long)]
    http_retry_backoff_factor: Option<String>,
    #[arg(long)]
    max_remote_image_bytes: Option<String>,
    #[arg(long)]
    max_decoded_pixels: Option<String>,
    #[arg(long)]
    max_images_per_request: Option<String>,
    #[arg(long)]
    max_redirects: Option<String>,
    #[arg(long)]
    http_max_response_bytes: Option<String>,
    #[arg(long)]
    vlm_debug: Option<bool>,
    #[arg(long, action = ArgAction::Set)]
    vlm_text_before_image: Option<bool>,
    #[arg(long, action = ArgAction::Set)]
    vlm_allow_truncated_content: Option<bool>,
    #[arg(long, action = ArgAction::Set)]
    vlm_allow_remote_images: Option<bool>,
    #[arg(long, action = ArgAction::Set)]
    vlm_allow_private_remote_images: Option<bool>,
    #[arg(long)]
    api_max_concurrent_requests: Option<String>,
    #[arg(long)]
    task_result_timeout_seconds: Option<String>,
    #[arg(long)]
    task_result_download_timeout_seconds: Option<String>,
    #[arg(long)]
    api_connect_timeout_seconds: Option<String>,
    #[arg(long)]
    api_acquisition_timeout_seconds: Option<String>,
    #[arg(long)]
    api_send_timeout_seconds: Option<String>,
    #[arg(long)]
    api_poll_interval_seconds: Option<String>,
    #[arg(long)]
    archive_max_entries: Option<String>,
    #[arg(long)]
    archive_max_ratio: Option<String>,
    #[arg(long)]
    zip_scan_central_cap: Option<String>,
    #[arg(long)]
    zip_scan_name_cap: Option<String>,
    #[arg(long)]
    zip_scan_depth_cap: Option<String>,
    #[arg(long)]
    zip_scan_total_name_cap: Option<String>,
    #[arg(long)]
    zip_scan_total_component_cap: Option<String>,
    #[arg(long)]
    ooxml_archive_bytes: Option<String>,
    #[arg(long)]
    ooxml_expanded_bytes: Option<String>,
    #[arg(long)]
    ooxml_xml_entry_bytes: Option<String>,
    #[arg(long)]
    ooxml_xml_total_bytes: Option<String>,
    #[arg(long)]
    ooxml_ratio: Option<String>,
    #[arg(long)]
    ooxml_xml_depth: Option<String>,
    #[arg(long)]
    ooxml_xml_events: Option<String>,
    #[arg(long)]
    ooxml_xml_attributes: Option<String>,
    #[arg(long)]
    ooxml_xml_namespaces: Option<String>,
    #[arg(long)]
    office_input_bytes: Option<String>,
    #[arg(long)]
    office_output_bytes: Option<String>,
    #[arg(long)]
    office_stderr_bytes: Option<String>,
    #[arg(long)]
    office_wall_seconds: Option<String>,
    #[arg(long)]
    office_cpu_seconds: Option<String>,
    #[arg(long)]
    office_nofile: Option<String>,
    #[arg(long)]
    office_address_space_bytes: Option<String>,
    #[arg(long)]
    office_active_process_limit: Option<String>,
    #[arg(long)]
    office_process_memory_bytes: Option<String>,
    #[arg(long)]
    office_job_memory_bytes: Option<String>,
    #[arg(long)]
    office_process_time_seconds: Option<String>,
    #[arg(long)]
    office_job_time_seconds: Option<String>,
}

/// uv-style layered help palette: bold bright-green usage line, bold cyan section
/// headers (Usage/Arguments/Options), bright-green flag names, italic gray
/// placeholders. Everything else keeps clap's default `Styles::styled()` palette
/// (e.g. bold red errors), so `error.print()` stays readable too.
fn cli_styles() -> clap::builder::Styles {
    use clap::builder::styling::{AnsiColor, Style, Styles};
    Styles::styled()
        .usage(
            Style::new()
                .bold()
                .fg_color(Some(AnsiColor::BrightGreen.into())),
        )
        .header(
            Style::new()
                .bold()
                .fg_color(Some(AnsiColor::BrightCyan.into())),
        )
        .literal(Style::new().fg_color(Some(AnsiColor::BrightGreen.into())))
        .placeholder(
            Style::new()
                .italic()
                .fg_color(Some(AnsiColor::BrightBlack.into())),
        )
}

/// The mineru CLI `clap::Command` with the uv-style help/error palette applied.
/// `ColorChoice` stays `Auto`: TTYs get color, piped output stays plain.
pub fn cli_command() -> clap::Command {
    Cli::command().styles(cli_styles())
}

impl From<Cli> for RunOptions {
    fn from(cli: Cli) -> Self {
        Self {
            path: cli.path,
            output: cli.output,
            api_url: cli.api_url,
            api_key: cli.api_key,
            method: cli.method,
            backend: cli.backend,
            effort: cli.effort,
            lang: cli.lang,
            url: cli.url,
            start: cli.start,
            end: cli.end,
            formula: cli.formula.unwrap_or(true),
            table: cli.table.unwrap_or(true),
            image_analysis: cli.image_analysis.unwrap_or(true),
            client_side_output_generation: cli.client_side_output_generation,
        }
    }
}

/// Run-relative elapsed clock shared by the plain and rich stderr renderers.
/// The stamp mirrors indicatif's `{elapsed_precise}` rendering (`HH:MM:SS`,
/// days-prefixed past 24h) so plain lines and rich bars stay consistent.
pub(crate) struct RunClock(Instant);

impl RunClock {
    pub(crate) fn start() -> Self {
        Self(Instant::now())
    }

    /// ANSI-free elapsed stamp since run start, e.g. `[+00:00:05]`.
    pub(crate) fn stamp(&self) -> String {
        format!("[+{}]", Self::render(self.0.elapsed()))
    }

    fn render(elapsed: Duration) -> String {
        let mut t = elapsed.as_secs();
        let seconds = t % 60;
        t /= 60;
        let minutes = t % 60;
        t /= 60;
        let hours = t % 24;
        t /= 24;
        if t > 0 {
            format!("{t}d {hours:02}:{minutes:02}:{seconds:02}")
        } else {
            format!("{hours:02}:{minutes:02}:{seconds:02}")
        }
    }
}

enum CliOutput {
    Plain(Arc<plain::EventSink<std::io::Stderr>>),
    Rich(Arc<rich::Renderer>),
}

impl CliOutput {
    fn new(level: plain::LogLevel, policy: rich::Policy) -> Self {
        if policy.rich {
            Self::Rich(rich::Renderer::stderr(level, policy.color))
        } else {
            Self::Plain(Arc::new(plain::EventSink::with_elapsed(
                std::io::stderr(),
                false,
                level,
            )))
        }
    }

    fn command_callback(&self) -> CommandCallback {
        match self {
            Self::Plain(sink) => sink.command_callback(),
            Self::Rich(renderer) => renderer.callback(),
        }
    }

    fn warning_callback(&self) -> direct::WarningCallback {
        match self {
            Self::Plain(sink) => sink.warning_callback(),
            Self::Rich(renderer) => renderer.warning_callback(),
        }
    }

    fn fail(&self, message: &str) {
        match self {
            Self::Plain(sink) => sink.fail(message),
            Self::Rich(renderer) => renderer.fail(message),
        }
    }

    fn finish(&self) {
        match self {
            Self::Plain(sink) => sink.finish(),
            Self::Rich(renderer) => renderer.finish(),
        }
    }
}

#[doc(hidden)]
pub async fn run_cli(argv: Vec<OsString>, context: RunContext) -> i32 {
    let args = std::iter::once(OsString::from("mineru")).chain(argv);
    // Same as `Cli::try_parse_from`, but on the styled command so help/version/errors
    // all render with the uv-style palette (ColorChoice::Auto: TTY-only coloring).
    let (options, overrides, cli_log_level) = match cli_command()
        .try_get_matches_from(args)
        .and_then(|matches| Cli::from_arg_matches(&matches))
    {
        Ok(cli) => {
            if cli
                .api_key
                .as_deref()
                .is_some_and(|key| !key.trim().is_empty())
            {
                eprintln!(
                    "warning: --api-key is visible in the process list and shell history; prefer MINERU_VL_API_KEY"
                );
            }
            let overrides = RunOverrides {
                document_limits: crate::DocumentLimitOverrides {
                    max_input_bytes: cli.max_input_bytes.clone(),
                    max_encoded_document_bytes: cli.max_encoded_document_bytes.clone(),
                    max_output_bytes: cli.max_output_bytes.clone(),
                },
                core: match cli_core_overrides(&cli) {
                    Ok(core) => core,
                    Err(error) => {
                        eprintln!("{error}");
                        return 1;
                    }
                },
                service: match cli_service_overrides(&cli) {
                    Ok(service) => service,
                    Err(error) => {
                        eprintln!("{error}");
                        return 1;
                    }
                },
            };
            let log_level = cli.log_level.clone();
            (RunOptions::from(cli), overrides, log_level)
        }
        Err(error) => {
            let code = if matches!(
                error.kind(),
                ErrorKind::DisplayHelp | ErrorKind::DisplayVersion
            ) {
                0
            } else {
                2
            };
            let _ = error.print();
            return code;
        }
    };
    // Explicit CLI wins over the frozen environment, which wins over the compiled default.
    let level_value = cli_log_level
        .map(OsString::from)
        .or_else(|| context.environment.os("MINERU_LOG_LEVEL"));
    let level = match plain::LogLevel::parse(level_value) {
        Ok(level) => level,
        Err(error) => {
            eprintln!("{error}");
            return 1;
        }
    };
    let policy = rich::Policy::select(std::io::stderr().is_terminal(), &context.environment);
    let output = CliOutput::new(level, policy);
    let typed_failure = Arc::new(AtomicBool::new(false));
    let events: CommandCallback = {
        let output = output.command_callback();
        let typed_failure = Arc::clone(&typed_failure);
        Arc::new(move |event| {
            if matches!(
                &event,
                CommandEvent::Progress {
                    event: ProgressEvent::DocumentFailed { .. } | ProgressEvent::ApiFailed { .. },
                    ..
                }
            ) {
                typed_failure.store(true, Ordering::Relaxed);
            }
            output(event);
        })
    };
    let context = context.with_output(Arc::clone(&events), output.warning_callback());
    let result = run_core(
        options,
        &context,
        overrides,
        events.clone(),
        output.warning_callback(),
    )
    .await;
    if let Err(error) = &result
        && !typed_failure.load(Ordering::Relaxed)
    {
        output.fail(&error.to_string());
    }
    events(if result.is_ok() {
        CommandEvent::RunCompleted
    } else {
        CommandEvent::RunFailed {
            message: result.as_ref().unwrap_err().to_string(),
        }
    });
    output.finish();
    if result.is_ok() { 0 } else { 1 }
}

/// Maps the canonical Clap surface onto typed core overrides. The flag-to-environment-name
/// correspondence is one-to-one; strict parsing produces errors before any work begins.
fn cli_core_overrides(cli: &Cli) -> Result<env::CoreOverrides, String> {
    let value = |name: &str| -> Option<OsString> {
        let flag: Option<&str> = match name {
            "MINERU_PROCESSING_WINDOW_SIZE" => cli.processing_window_size.as_deref(),
            "MINERU_OFFICIAL_PAGE_CONCURRENCY" => cli.page_concurrency.as_deref(),
            "MINERU_PDF_RENDER_THREADS" => cli.render_workers.as_deref(),
            "MINERU_PDF_RENDER_TIMEOUT" => cli.render_timeout_seconds.as_deref(),
            "MINERU_MAX_PDF_BYTES" => cli.max_pdf_bytes.as_deref(),
            "MINERU_MAX_PAGES" => cli.max_pages.as_deref(),
            "MINERU_MAX_PAGE_PIXELS" => cli.max_page_pixels.as_deref(),
            "MINERU_MAX_RENDERED_IMAGE_BYTES" => cli.max_rendered_image_bytes.as_deref(),
            "MINERU_MAX_IN_FLIGHT_IMAGE_BYTES" => cli.max_in_flight_image_bytes.as_deref(),
            "MINERU_MAX_RAW_OUTPUT_BYTES" => cli.max_raw_output_bytes.as_deref(),
            "MINERU_MAX_LAYOUT_BLOCKS_PER_PAGE" => cli.max_layout_blocks_per_page.as_deref(),
            "MINERU_MAX_SEMANTIC_REQUESTS_PER_PAGE" => {
                cli.max_semantic_requests_per_page.as_deref()
            }
            "MINERU_BATCH_SIZE" => cli.batch_size.as_deref(),
            "MINERU_MAX_ENCODED_REQUEST_BYTES" => cli.max_encoded_request_bytes.as_deref(),
            "MINERU_MAX_ENCODED_BATCH_BYTES" => cli.max_encoded_batch_bytes.as_deref(),
            "MINERU_MAX_TOTAL_ASSET_BYTES" => cli.max_total_asset_bytes.as_deref(),
            "MINERU_MAX_STAGED_TEXT_BYTES" => cli.max_staged_text_bytes.as_deref(),
            "MINERU_TOTAL_DEADLINE_SECONDS" => cli.total_deadline_seconds.as_deref(),
            "MINERU_VLM_HTTP_CONCURRENCY" => cli.http_max_concurrency.as_deref(),
            "MINERU_VLM_HTTP_TIMEOUT" => cli.http_timeout_seconds.as_deref(),
            "MINERU_VLM_CONNECT_TIMEOUT" => cli.connect_timeout_seconds.as_deref(),
            "MINERU_VLM_HTTP_MAX_KEEPALIVE_CONNECTIONS" => {
                cli.http_max_keepalive_connections.as_deref()
            }
            "MINERU_VLM_HTTP_KEEPALIVE_EXPIRY" => cli.http_keepalive_expiry_seconds.as_deref(),
            "MINERU_VLM_HTTP_MAX_RETRIES" => cli.http_max_retries.as_deref(),
            "MINERU_VLM_HTTP_RETRY_BACKOFF_FACTOR" => cli.http_retry_backoff_factor.as_deref(),
            "MINERU_VLM_MAX_IMAGE_BYTES" => cli.max_remote_image_bytes.as_deref(),
            "MINERU_VLM_MAX_DECODED_PIXELS" => cli.max_decoded_pixels.as_deref(),
            "MINERU_VLM_MAX_IMAGES_PER_REQUEST" => cli.max_images_per_request.as_deref(),
            "MINERU_VLM_MAX_REDIRECTS" => cli.max_redirects.as_deref(),
            "MINERU_VLM_HTTP_MAX_RESPONSE_BYTES" => cli.http_max_response_bytes.as_deref(),
            _ => return None,
        };
        flag.map(OsString::from)
    };
    let mut core = env::parse_core_overrides(&value)?;
    core.formula = cli.formula;
    core.table = cli.table;
    core.image_analysis = cli.image_analysis;
    core.vlm_debug = cli.vlm_debug;
    Ok(core)
}

/// Maps the canonical Clap surface onto typed service overrides. The flag-to-environment-name
/// correspondence is one-to-one; strict parsing produces errors before any work begins.
fn cli_service_overrides(cli: &Cli) -> Result<service::ServiceOverrides, String> {
    let value = |name: &str| -> Option<OsString> {
        let flag: Option<&str> = match name {
            "MINERU_API_MAX_CONCURRENT_REQUESTS" => cli.api_max_concurrent_requests.as_deref(),
            "MINERU_TASK_RESULT_TIMEOUT_SECONDS" => cli.task_result_timeout_seconds.as_deref(),
            "MINERU_TASK_RESULT_DOWNLOAD_TIMEOUT_SECONDS" => {
                cli.task_result_download_timeout_seconds.as_deref()
            }
            "MINERU_API_CONNECT_TIMEOUT_SECONDS" => cli.api_connect_timeout_seconds.as_deref(),
            "MINERU_API_ACQUISITION_TIMEOUT_SECONDS" => {
                cli.api_acquisition_timeout_seconds.as_deref()
            }
            "MINERU_API_SEND_TIMEOUT_SECONDS" => cli.api_send_timeout_seconds.as_deref(),
            "MINERU_API_POLL_INTERVAL_SECONDS" => cli.api_poll_interval_seconds.as_deref(),
            "MINERU_ARCHIVE_MAX_ENTRIES" => cli.archive_max_entries.as_deref(),
            "MINERU_ARCHIVE_MAX_RATIO" => cli.archive_max_ratio.as_deref(),
            "MINERU_ZIP_SCAN_CENTRAL_CAP" => cli.zip_scan_central_cap.as_deref(),
            "MINERU_ZIP_SCAN_NAME_CAP" => cli.zip_scan_name_cap.as_deref(),
            "MINERU_ZIP_SCAN_DEPTH_CAP" => cli.zip_scan_depth_cap.as_deref(),
            "MINERU_ZIP_SCAN_TOTAL_NAME_CAP" => cli.zip_scan_total_name_cap.as_deref(),
            "MINERU_ZIP_SCAN_TOTAL_COMPONENT_CAP" => cli.zip_scan_total_component_cap.as_deref(),
            "MINERU_OOXML_ARCHIVE_BYTES" => cli.ooxml_archive_bytes.as_deref(),
            "MINERU_OOXML_EXPANDED_BYTES" => cli.ooxml_expanded_bytes.as_deref(),
            "MINERU_OOXML_XML_ENTRY_BYTES" => cli.ooxml_xml_entry_bytes.as_deref(),
            "MINERU_OOXML_XML_TOTAL_BYTES" => cli.ooxml_xml_total_bytes.as_deref(),
            "MINERU_OOXML_RATIO" => cli.ooxml_ratio.as_deref(),
            "MINERU_OOXML_XML_DEPTH" => cli.ooxml_xml_depth.as_deref(),
            "MINERU_OOXML_XML_EVENTS" => cli.ooxml_xml_events.as_deref(),
            "MINERU_OOXML_XML_ATTRIBUTES" => cli.ooxml_xml_attributes.as_deref(),
            "MINERU_OOXML_XML_NAMESPACES" => cli.ooxml_xml_namespaces.as_deref(),
            "MINERU_OFFICE_INPUT_BYTES" => cli.office_input_bytes.as_deref(),
            "MINERU_OFFICE_OUTPUT_BYTES" => cli.office_output_bytes.as_deref(),
            "MINERU_OFFICE_STDERR_BYTES" => cli.office_stderr_bytes.as_deref(),
            "MINERU_OFFICE_WALL_SECONDS" => cli.office_wall_seconds.as_deref(),
            "MINERU_OFFICE_CPU_SECONDS" => cli.office_cpu_seconds.as_deref(),
            "MINERU_OFFICE_NOFILE" => cli.office_nofile.as_deref(),
            "MINERU_OFFICE_ADDRESS_SPACE_BYTES" => cli.office_address_space_bytes.as_deref(),
            "MINERU_OFFICE_ACTIVE_PROCESS_LIMIT" => cli.office_active_process_limit.as_deref(),
            "MINERU_OFFICE_PROCESS_MEMORY_BYTES" => cli.office_process_memory_bytes.as_deref(),
            "MINERU_OFFICE_JOB_MEMORY_BYTES" => cli.office_job_memory_bytes.as_deref(),
            "MINERU_OFFICE_PROCESS_TIME_SECONDS" => cli.office_process_time_seconds.as_deref(),
            "MINERU_OFFICE_JOB_TIME_SECONDS" => cli.office_job_time_seconds.as_deref(),
            "MINERU_API_RECORD_CAP" => None,
            "MINERU_API_FILE_CAP" => None,
            "MINERU_API_BODY_CAP" => None,
            "MINERU_API_TEXT_CAP" => None,
            "MINERU_API_TEXT_TOTAL_CAP" => None,
            "MINERU_API_FORM_FIELDS_CAP" => None,
            _ => return None,
        };
        flag.map(OsString::from)
    };
    let mut service = service::parse_service_overrides(&value)?;
    service.vlm_text_before_image = cli.vlm_text_before_image;
    service.vlm_allow_truncated_content = cli.vlm_allow_truncated_content;
    service.vlm_allow_remote_images = cli.vlm_allow_remote_images;
    service.vlm_allow_private_remote_images = cli.vlm_allow_private_remote_images;
    Ok(service)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        collections::HashMap,
        future::Future,
        process::Command,
        sync::atomic::{AtomicUsize, Ordering as AtomicOrdering},
        time::Duration,
    };

    #[test]
    fn run_clock_renders_indicatif_style_elapsed() {
        assert_eq!(RunClock::render(Duration::from_secs(0)), "00:00:00");
        assert_eq!(RunClock::render(Duration::from_secs(5)), "00:00:05");
        assert_eq!(RunClock::render(Duration::from_secs(65)), "00:01:05");
        assert_eq!(RunClock::render(Duration::from_secs(3661)), "01:01:01");
        assert_eq!(
            RunClock::render(Duration::from_secs(24 * 3600 + 61)),
            "1d 00:01:01"
        );
        let stamp = RunClock::start().stamp();
        assert!(stamp.starts_with("[+") && stamp.ends_with(']'));
        assert!(!stamp.contains('\x1b'));
    }

    #[test]
    fn run_error_sanitizes_secrets_controls_and_length_at_creation() {
        let error = RunError::new(format!(
            "invalid option: Bearer super-secret\n\t\0{}",
            "x".repeat(FAILURE_CAP * 2)
        ));
        let message = error.to_string();
        assert!(message.starts_with("invalid option: Bearer [REDACTED]"));
        assert!(!message.contains("super-secret"));
        assert!(!message.chars().any(char::is_control));
        assert!(message.contains("\\n\\t\\0"));
        assert!(message.len() <= FAILURE_CAP);
        assert!(message.ends_with(" [truncated]"));
    }

    fn assert_send_future<F, T>(_: F)
    where
        F: Future<Output = T> + Send + 'static,
        T: Send + 'static,
    {
    }

    #[test]
    fn binding_entry_futures_are_send_and_static() {
        let helper = std::env::current_dir().unwrap().join("unused-helper");
        let context = RunContext::with_office_executable(helper).unwrap();
        assert_send_future(run_with_context(
            RunOptions::new("unused-input", "unused-output"),
            context.clone(),
        ));
        assert_send_future(run_cli(Vec::new(), context));
    }

    #[test]
    fn cli_maps_all_run_options() {
        let cli = Cli::try_parse_from([
            "mineru",
            "-p",
            "a",
            "-o",
            "b",
            "--api-url",
            "http://api",
            "--api-key",
            "secret",
            "--method",
            "ocr",
            "--effort",
            "high",
            "--lang",
            "en",
            "--url",
            "http://model",
            "--start",
            "2",
            "--end",
            "4",
            "--formula",
            "false",
            "--table",
            "false",
            "--image-analysis",
            "false",
            "--client-side-output-generation",
            "true",
            "--max-input-bytes",
            "1_024",
            "--max-encoded-document-bytes",
            "2_048",
            "--max-output-bytes",
            "4_096",
        ])
        .unwrap();
        assert_eq!(cli.max_input_bytes.as_deref(), Some("1_024"));
        assert_eq!(cli.max_encoded_document_bytes.as_deref(), Some("2_048"));
        assert_eq!(cli.max_output_bytes.as_deref(), Some("4_096"));
        let options = RunOptions::from(cli);
        assert_eq!(options.path, PathBuf::from("a"));
        assert_eq!(options.output, PathBuf::from("b"));
        assert_eq!(options.api_url.as_deref(), Some("http://api"));
        assert_eq!(options.api_key.as_deref(), Some("secret"));
        assert_eq!(options.method, "ocr");
        assert_eq!(options.backend, "vlm-http-client");
        assert_eq!(options.effort, "high");
        assert_eq!(options.lang, "en");
        assert_eq!(options.url.as_deref(), Some("http://model"));
        assert_eq!((options.start, options.end), (2, Some(4)));
        assert!(!options.formula && !options.table && !options.image_analysis);
        assert!(options.client_side_output_generation);
    }

    #[test]
    fn context_requires_and_uses_absolute_helper() {
        assert!(RunContext::with_office_executable("relative".into()).is_err());
        let path = std::env::current_dir().unwrap().join("helper");
        let context = RunContext::with_office_executable(path.clone()).unwrap();
        assert_eq!(context.office_workers().executable(), &path);
    }

    #[test]
    fn environment_snapshot_is_frozen() {
        for mode in ["present", "absent"] {
            let output = Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "command::tests::environment_snapshot_child",
                    "--nocapture",
                ])
                .env("MINERU_ENV_SNAPSHOT_CHILD", mode)
                .env_remove("MINERU_VL_API_KEY")
                .env_remove("MINERU_VLM_END_TOKEN")
                .envs(
                    (mode == "present")
                        .then_some([
                            ("MINERU_VL_API_KEY", "snapshot-key"),
                            ("MINERU_VLM_END_TOKEN", "snapshot-end"),
                        ])
                        .into_iter()
                        .flatten(),
                )
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStringExt;
            let invalid = OsString::from_vec(vec![0xff]);
            let output = Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "command::tests::environment_snapshot_child",
                    "--nocapture",
                ])
                .env("MINERU_ENV_SNAPSHOT_CHILD", "non-utf8")
                .env("MINERU_VL_API_KEY", &invalid)
                .env("MINERU_VLM_END_TOKEN", &invalid)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }

    #[test]
    fn environment_snapshot_child() {
        let Ok(mode) = std::env::var("MINERU_ENV_SNAPSHOT_CHILD") else {
            return;
        };
        let original_key = std::env::var_os("MINERU_VL_API_KEY");
        let original_end = std::env::var_os("MINERU_VLM_END_TOKEN");
        let context = RunContext::with_office_executable(
            std::env::current_dir().unwrap().join("unused-helper"),
        )
        .unwrap();
        // SAFETY: this test only mutates env in an exact-filtered child test process.
        unsafe {
            std::env::set_var("MINERU_VL_API_KEY", "late-key");
            if mode == "present" {
                std::env::remove_var("MINERU_VLM_END_TOKEN");
            } else {
                std::env::set_var("MINERU_VLM_END_TOKEN", "late-end");
            }
        }
        // The snapshot is read before the late mutation, so the frozen values still surface.
        match mode.as_str() {
            "present" => {
                assert_eq!(
                    context.environment.string("MINERU_VL_API_KEY").as_deref(),
                    Some("snapshot-key")
                );
                assert_eq!(
                    context
                        .environment
                        .string("MINERU_VLM_END_TOKEN")
                        .as_deref(),
                    Some("snapshot-end")
                );
            }
            "absent" | "non-utf8" => {
                assert_eq!(context.environment.string("MINERU_VL_API_KEY"), None);
                assert_eq!(context.environment.string("MINERU_VLM_END_TOKEN"), None);
            }
            _ => panic!("unknown child mode"),
        }
        // SAFETY: restore the child process environment before the test exits.
        unsafe {
            match original_key {
                Some(value) => std::env::set_var("MINERU_VL_API_KEY", value),
                None => std::env::remove_var("MINERU_VL_API_KEY"),
            }
            match original_end {
                Some(value) => std::env::set_var("MINERU_VLM_END_TOKEN", value),
                None => std::env::remove_var("MINERU_VLM_END_TOKEN"),
            }
        }
    }

    #[test]
    fn remote_encoded_limit_is_rejected_in_a_scrubbed_process() {
        for mode in ["flag", "environment"] {
            let output = Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "command::tests::remote_encoded_limit_child",
                    "--nocapture",
                ])
                .env("MINERU_REMOTE_ENCODED_CHILD", mode)
                .env_remove("MINERU_MAX_INPUT_BYTES")
                .env_remove("MINERU_MAX_OUTPUT_BYTES")
                .env_remove("MINERU_MAX_ENCODED_DOCUMENT_BYTES")
                .envs(
                    (mode == "environment")
                        .then_some([("MINERU_MAX_ENCODED_DOCUMENT_BYTES", "malformed")])
                        .into_iter()
                        .flatten(),
                )
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            let messages = format!(
                "{}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(
                messages.contains("cannot configure a remote server"),
                "{messages}"
            );
        }
    }

    #[tokio::test]
    async fn remote_encoded_limit_child() {
        let Ok(mode) = std::env::var("MINERU_REMOTE_ENCODED_CHILD") else {
            return;
        };
        let context = RunContext::with_office_executable(
            std::env::current_dir().unwrap().join("unused-helper"),
        )
        .unwrap();
        let mut args = vec![
            "-p".into(),
            "missing.pdf".into(),
            "-o".into(),
            "output".into(),
            "--api-url".into(),
            "http://127.0.0.1:1".into(),
        ];
        if mode == "flag" {
            args.extend(["--max-encoded-document-bytes".into(), "8".into()]);
        }
        assert_eq!(run_cli(args, context).await, 1);
    }

    #[test]
    fn warnings_are_bounded_and_sanitized() {
        let mut collector = WarningCollector::default();
        for index in 0..=WARNING_CAP {
            collector.push(
                "api",
                &format!("{index} Bearer secret https://example.test/a\n"),
            );
        }
        assert_eq!(collector.warnings.len(), WARNING_CAP + 1);
        assert_eq!(collector.warnings.last().unwrap(), "warnings truncated");
        assert!(!collector.warnings.join(" ").contains("secret"));
        assert!(!collector.warnings.join(" ").contains("example.test"));
        assert!(
            collector
                .warnings
                .iter()
                .all(|warning| warning.len() <= TEXT_CAP)
        );
    }

    #[test]
    fn remote_failure_detail_is_bounded_and_sanitized() {
        let text = format_remote_failures(&[crate::RemoteApiFailure {
            task_index: 7,
            document_stems: vec!["doc".into()],
            message: "Bearer secret https://example.test/a\nfailed\n".repeat(1000),
        }]);
        assert!(text.contains("task#7 [doc]"));
        assert!(
            !text.contains("secret") && !text.contains("example.test"),
            "{text}"
        );
        assert!(text.len() <= FAILURE_CAP);
    }

    #[test]
    fn cli_requires_path_and_output_and_rejects_unknown_flags() {
        assert!(Cli::try_parse_from(["mineru"]).is_err());
        assert!(Cli::try_parse_from(["mineru", "-p", "a"]).is_err());
        assert!(Cli::try_parse_from(["mineru", "-o", "b"]).is_err());
        // The current surface rejects unknown spellings but accepts the real boolean options.
        assert!(Cli::try_parse_from(["mineru", "-p", "a", "-o", "b", "--bogus"]).is_err());
        assert!(
            Cli::try_parse_from(["mineru", "-p", "a", "-o", "b", "--formula", "false"]).is_ok()
        );
        assert!(
            Cli::try_parse_from(["mineru", "-p", "a", "-o", "b", "--vlm-debug", "true"]).is_ok()
        );
    }

    #[test]
    fn cli_core_overrides_maps_every_flag_and_applies_batch() {
        let cli = Cli::try_parse_from([
            "mineru",
            "-p",
            "a",
            "-o",
            "b",
            "--processing-window-size",
            "32",
            "--page-concurrency",
            "12",
            "--render-workers",
            "8",
            "--render-timeout-seconds",
            "120",
            "--max-pdf-bytes",
            "9",
            "--max-pages",
            "10",
            "--max-page-pixels",
            "11",
            "--max-rendered-image-bytes",
            "12",
            "--max-in-flight-image-bytes",
            "13",
            "--max-raw-output-bytes",
            "14",
            "--max-layout-blocks-per-page",
            "15",
            "--max-semantic-requests-per-page",
            "16",
            "--batch-size",
            "17",
            "--max-encoded-request-bytes",
            "18",
            "--max-encoded-batch-bytes",
            "19",
            "--max-total-asset-bytes",
            "20",
            "--max-staged-text-bytes",
            "21",
            "--total-deadline-seconds",
            "22",
            "--http-max-concurrency",
            "23",
            "--http-timeout-seconds",
            "24",
            "--connect-timeout-seconds",
            "25",
            "--http-max-keepalive-connections",
            "26",
            "--http-keepalive-expiry-seconds",
            "27",
            "--http-max-retries",
            "28",
            "--http-retry-backoff-factor",
            "0.5",
            "--max-remote-image-bytes",
            "29",
            "--max-decoded-pixels",
            "30",
            "--max-images-per-request",
            "31",
            "--max-redirects",
            "32",
            "--http-max-response-bytes",
            "33",
        ])
        .unwrap();
        let core = cli_core_overrides(&cli).unwrap();
        assert_eq!(core.processing_window_size, Some(32));
        assert_eq!(core.page_concurrency, Some(12));
        assert_eq!(core.render_workers, Some(8));
        assert_eq!(core.max_layout_blocks_per_page, Some(15));
        assert_eq!(core.max_semantic_requests_per_page, Some(16));
        assert_eq!(core.batch_size, Some(17));
        assert_eq!(core.http_max_concurrency, Some(23));
        assert_eq!(core.http_retry_backoff_factor, Some(0.5));
        assert_eq!(core.max_decoded_pixels, Some(30));
        // The batch flag genuinely feeds the inference admission field.
        let resolved = env::resolve_core(|_| None, &core).unwrap();
        assert_eq!(resolved.route.max_requests_per_batch, 17);
        // Malformed CLI values fail before any work.
        let cli = Cli::try_parse_from([
            "mineru",
            "-p",
            "a",
            "-o",
            "b",
            "--processing-window-size",
            "0",
        ])
        .unwrap();
        let error = cli_core_overrides(&cli).unwrap_err();
        assert!(error.contains("MINERU_PROCESSING_WINDOW_SIZE"), "{error}");
    }

    #[test]
    fn high_level_api_captures_no_stderr() {
        for mode in ["static", "warning"] {
            let output = Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "command::tests::high_level_stderr_child",
                    "--nocapture",
                ])
                .env("MINERU_STDERR_CHILD", mode)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert_eq!(output.stderr, b"", "mode={mode}");
        }
    }

    #[tokio::test]
    async fn high_level_stderr_child() {
        let Ok(mode) = std::env::var("MINERU_STDERR_CHILD") else {
            return;
        };
        let mut context = RunContext::with_office_executable(
            std::env::current_dir().unwrap().join("unused-helper"),
        )
        .unwrap();
        let warnings = Arc::new(AtomicUsize::new(0));
        if mode == "warning" {
            let warnings = Arc::clone(&warnings);
            context.warnings = Some(Arc::new(move |_, _| {
                warnings.fetch_add(1, AtomicOrdering::Relaxed);
            }));
        }
        let missing = tempfile::tempdir().unwrap().path().join("missing.pdf");
        let mut options = RunOptions::new(missing, "output");
        if mode == "static" {
            options.client_side_output_generation = true;
            options.api_url = Some("http://127.0.0.1:1".into());
            assert_eq!(
                run_with_context(options, context)
                    .await
                    .unwrap_err()
                    .to_string(),
                "client-side output generation is unsupported"
            );
        } else {
            options.method = "txt".into();
            assert!(run_with_context(options, context).await.is_err());
            assert_eq!(warnings.load(AtomicOrdering::Relaxed), 1);
        }
    }

    #[test]
    fn cli_service_overrides_maps_every_flag_and_applies_strictly() {
        let cli = Cli::try_parse_from([
            "mineru",
            "-p",
            "a",
            "-o",
            "b",
            "--vlm-text-before-image",
            "true",
            "--vlm-allow-remote-images",
            "false",
            "--api-max-concurrent-requests",
            "12",
            "--task-result-timeout-seconds",
            "13",
            "--task-result-download-timeout-seconds",
            "14",
            "--api-connect-timeout-seconds",
            "15",
            "--api-acquisition-timeout-seconds",
            "16",
            "--api-send-timeout-seconds",
            "17",
            "--api-poll-interval-seconds",
            "18",
            "--archive-max-entries",
            "19",
            "--archive-max-ratio",
            "20",
            "--zip-scan-central-cap",
            "21",
            "--zip-scan-name-cap",
            "22",
            "--zip-scan-depth-cap",
            "23",
            "--zip-scan-total-name-cap",
            "24",
            "--zip-scan-total-component-cap",
            "25",
            "--ooxml-archive-bytes",
            "26",
            "--ooxml-expanded-bytes",
            "27",
            "--ooxml-xml-entry-bytes",
            "28",
            "--ooxml-xml-total-bytes",
            "29",
            "--ooxml-ratio",
            "30",
            "--ooxml-xml-depth",
            "31",
            "--ooxml-xml-events",
            "32",
            "--ooxml-xml-attributes",
            "33",
            "--ooxml-xml-namespaces",
            "34",
            "--office-input-bytes",
            "35",
            "--office-output-bytes",
            "36",
            "--office-stderr-bytes",
            "37",
            "--office-wall-seconds",
            "38",
            "--office-cpu-seconds",
            "39",
            "--office-nofile",
            "40",
            "--office-address-space-bytes",
            "41",
            "--office-active-process-limit",
            "42",
            "--office-process-memory-bytes",
            "43",
            "--office-job-memory-bytes",
            "44",
            "--office-process-time-seconds",
            "45",
            "--office-job-time-seconds",
            "46",
        ])
        .unwrap();
        let service = cli_service_overrides(&cli).unwrap();
        assert_eq!(service.vlm_text_before_image, Some(true));
        assert_eq!(service.vlm_allow_remote_images, Some(false));
        assert_eq!(service.api_max_concurrent_requests, Some(12));
        assert_eq!(service.archive_max_entries, Some(19));
        assert_eq!(service.zip_depth_cap, Some(23));
        assert_eq!(service.ooxml_xml_events, Some(32));
        assert_eq!(service.office_input_bytes, Some(35));
        assert_eq!(service.office_active_process_limit, Some(42));
        assert_eq!(service.office_job_time_seconds, Some(46));
        // Strict malformed CLI values fail before any work.
        let cli =
            Cli::try_parse_from(["mineru", "-p", "a", "-o", "b", "--office-wall-seconds", "0"])
                .unwrap();
        let error = cli_service_overrides(&cli).unwrap_err();
        assert!(error.contains("MINERU_OFFICE_WALL_SECONDS"), "{error}");
    }

    #[test]
    fn remote_only_service_controls_are_rejected_in_direct_mode() {
        let environment = Environment::from_values(
            [("MINERU_TASK_RESULT_TIMEOUT_SECONDS", OsString::from("900"))]
                .into_iter()
                .collect(),
        );
        let message =
            remote_only_service_error(&service::ServiceOverrides::default(), &environment).unwrap();
        assert!(
            message.contains("MINERU_TASK_RESULT_TIMEOUT_SECONDS"),
            "{message}"
        );
        // CLI-set controls are rejected too.
        let message = remote_only_service_error(
            &service::ServiceOverrides {
                archive_max_entries: Some(5),
                ..Default::default()
            },
            &Environment::from_values(HashMap::new()),
        )
        .unwrap();
        assert!(message.contains("--archive-max-entries"), "{message}");
        // Remote-only controls are fine when the API URL is present (the caller checks first).
        assert!(
            remote_only_service_error(
                &service::ServiceOverrides {
                    api_send_timeout: Some(Duration::from_secs(1)),
                    ..Default::default()
                },
                &Environment::from_values(HashMap::new()),
            )
            .is_some()
        );
    }

    #[test]
    fn server_owned_caps_are_rejected_on_the_canonical_client() {
        let environment = Environment::from_values(
            [("MINERU_API_RECORD_CAP", OsString::from("40"))]
                .into_iter()
                .collect(),
        );
        let message =
            server_owned_error(&service::ServiceOverrides::default(), &environment).unwrap();
        assert!(message.contains("MINERU_API_RECORD_CAP"), "{message}");
        assert!(message.contains("configure the server"), "{message}");
    }

    #[test]
    fn local_transport_booleans_are_rejected_in_remote_mode() {
        let environment = Environment::from_values(
            [("MINERU_VLM_ALLOW_REMOTE_IMAGES", OsString::from("true"))]
                .into_iter()
                .collect(),
        );
        let message = remote_local_service_transport_error(
            &service::ServiceOverrides::default(),
            &environment,
        )
        .unwrap();
        assert!(
            message.contains("MINERU_VLM_ALLOW_REMOTE_IMAGES"),
            "{message}"
        );
    }

    /// Every flag/env in each of the four mode-applicability surfaces must be rejected, both when
    /// CLI-set and when present in the frozen environment. The same tables drive the exact-count
    /// assertion on the all-set message, so a knob added to a rejection function without a row
    /// here (or vice versa) fails the test instead of drifting.
    #[test]
    fn mode_rejection_covers_every_knob_in_each_surface() {
        let no_env = Environment::from_values(HashMap::new());

        let core_rows: &[(&str, &str, fn(&mut env::CoreOverrides))] = &[
            (
                "--page-concurrency",
                "MINERU_OFFICIAL_PAGE_CONCURRENCY",
                |c| c.page_concurrency = Some(4),
            ),
            (
                "--processing-window-size",
                "MINERU_PROCESSING_WINDOW_SIZE",
                |c| c.processing_window_size = Some(2),
            ),
            ("--render-workers", "MINERU_PDF_RENDER_THREADS", |c| {
                c.render_workers = Some(2)
            }),
            (
                "--render-timeout-seconds",
                "MINERU_PDF_RENDER_TIMEOUT",
                |c| c.render_timeout = Some(Duration::from_secs(2)),
            ),
            ("--batch-size", "MINERU_BATCH_SIZE", |c| {
                c.batch_size = Some(2)
            }),
            (
                "--http-max-concurrency",
                "MINERU_VLM_HTTP_CONCURRENCY",
                |c| c.http_max_concurrency = Some(2),
            ),
            ("--http-timeout-seconds", "MINERU_VLM_HTTP_TIMEOUT", |c| {
                c.http_timeout = Some(Duration::from_secs(2))
            }),
            (
                "--connect-timeout-seconds",
                "MINERU_VLM_CONNECT_TIMEOUT",
                |c| c.connect_timeout = Some(Duration::from_secs(2)),
            ),
            (
                "--http-max-keepalive-connections",
                "MINERU_VLM_HTTP_MAX_KEEPALIVE_CONNECTIONS",
                |c| c.http_max_keepalive_connections = Some(2),
            ),
            (
                "--http-keepalive-expiry-seconds",
                "MINERU_VLM_HTTP_KEEPALIVE_EXPIRY",
                |c| c.http_keepalive_expiry = Some(Duration::from_secs(2)),
            ),
            ("--http-max-retries", "MINERU_VLM_HTTP_MAX_RETRIES", |c| {
                c.http_max_retries = Some(2)
            }),
            (
                "--http-retry-backoff-factor",
                "MINERU_VLM_HTTP_RETRY_BACKOFF_FACTOR",
                |c| c.http_retry_backoff_factor = Some(0.5),
            ),
            (
                "--max-remote-image-bytes",
                "MINERU_VLM_MAX_IMAGE_BYTES",
                |c| c.max_remote_image_bytes = Some(1024),
            ),
            (
                "--max-decoded-pixels",
                "MINERU_VLM_MAX_DECODED_PIXELS",
                |c| c.max_decoded_pixels = Some(1024),
            ),
            (
                "--max-images-per-request",
                "MINERU_VLM_MAX_IMAGES_PER_REQUEST",
                |c| c.max_images_per_request = Some(2),
            ),
            ("--max-redirects", "MINERU_VLM_MAX_REDIRECTS", |c| {
                c.max_redirects = Some(2)
            }),
            (
                "--http-max-response-bytes",
                "MINERU_VLM_HTTP_MAX_RESPONSE_BYTES",
                |c| c.http_max_response_bytes = Some(1024),
            ),
            ("--vlm-debug", "MINERU_VL_DEBUG_ENABLE", |c| {
                c.vlm_debug = Some(true)
            }),
        ];
        assert_knob_rejection(core_rows, remote_local_transport_error);

        let service_transport_rows: &[(&str, &str, fn(&mut service::ServiceOverrides))] = &[
            (
                "--vlm-text-before-image",
                "MINERU_VLM_TEXT_BEFORE_IMAGE",
                |s| s.vlm_text_before_image = Some(true),
            ),
            (
                "--vlm-allow-truncated-content",
                "MINERU_VLM_ALLOW_TRUNCATED_CONTENT",
                |s| s.vlm_allow_truncated_content = Some(true),
            ),
            (
                "--vlm-allow-remote-images",
                "MINERU_VLM_ALLOW_REMOTE_IMAGES",
                |s| s.vlm_allow_remote_images = Some(true),
            ),
            (
                "--vlm-allow-private-remote-images",
                "MINERU_VLM_ALLOW_PRIVATE_REMOTE_IMAGES",
                |s| s.vlm_allow_private_remote_images = Some(true),
            ),
        ];
        assert_knob_rejection(service_transport_rows, remote_local_service_transport_error);

        let remote_only_rows: &[(&str, &str, fn(&mut service::ServiceOverrides))] = &[
            (
                "--api-max-concurrent-requests",
                "MINERU_API_MAX_CONCURRENT_REQUESTS",
                |s| s.api_max_concurrent_requests = Some(2),
            ),
            (
                "--task-result-timeout-seconds",
                "MINERU_TASK_RESULT_TIMEOUT_SECONDS",
                |s| s.task_result_timeout = Some(Duration::from_secs(2)),
            ),
            (
                "--task-result-download-timeout-seconds",
                "MINERU_TASK_RESULT_DOWNLOAD_TIMEOUT_SECONDS",
                |s| s.task_download_timeout = Some(Duration::from_secs(2)),
            ),
            (
                "--api-connect-timeout-seconds",
                "MINERU_API_CONNECT_TIMEOUT_SECONDS",
                |s| s.api_connect_timeout = Some(Duration::from_secs(2)),
            ),
            (
                "--api-acquisition-timeout-seconds",
                "MINERU_API_ACQUISITION_TIMEOUT_SECONDS",
                |s| s.api_acquisition_timeout = Some(Duration::from_secs(2)),
            ),
            (
                "--api-send-timeout-seconds",
                "MINERU_API_SEND_TIMEOUT_SECONDS",
                |s| s.api_send_timeout = Some(Duration::from_secs(2)),
            ),
            (
                "--api-poll-interval-seconds",
                "MINERU_API_POLL_INTERVAL_SECONDS",
                |s| s.api_poll_interval = Some(Duration::from_secs(2)),
            ),
            ("--archive-max-entries", "MINERU_ARCHIVE_MAX_ENTRIES", |s| {
                s.archive_max_entries = Some(2)
            }),
            ("--archive-max-ratio", "MINERU_ARCHIVE_MAX_RATIO", |s| {
                s.archive_max_ratio = Some(2)
            }),
            (
                "--zip-scan-central-cap",
                "MINERU_ZIP_SCAN_CENTRAL_CAP",
                |s| s.zip_central_cap = Some(2),
            ),
            ("--zip-scan-name-cap", "MINERU_ZIP_SCAN_NAME_CAP", |s| {
                s.zip_name_cap = Some(2)
            }),
            ("--zip-scan-depth-cap", "MINERU_ZIP_SCAN_DEPTH_CAP", |s| {
                s.zip_depth_cap = Some(2)
            }),
            (
                "--zip-scan-total-name-cap",
                "MINERU_ZIP_SCAN_TOTAL_NAME_CAP",
                |s| s.zip_total_name_cap = Some(2),
            ),
            (
                "--zip-scan-total-component-cap",
                "MINERU_ZIP_SCAN_TOTAL_COMPONENT_CAP",
                |s| s.zip_total_component_cap = Some(2),
            ),
        ];
        assert_knob_rejection(remote_only_rows, remote_only_service_error);

        let server_owned_rows: &[(&str, fn(&mut service::ServiceOverrides))] = &[
            ("MINERU_API_RECORD_CAP", |s| s.server_record_cap = Some(2)),
            ("MINERU_API_FILE_CAP", |s| s.server_file_cap = Some(2)),
            ("MINERU_API_BODY_CAP", |s| s.server_body_cap = Some(2)),
            ("MINERU_API_TEXT_CAP", |s| s.server_text_cap = Some(2)),
            ("MINERU_API_TEXT_TOTAL_CAP", |s| {
                s.server_text_total_cap = Some(2)
            }),
            ("MINERU_API_FORM_FIELDS_CAP", |s| {
                s.server_form_fields_cap = Some(2)
            }),
            ("MINERU_API_TASK_RETENTION_SECONDS", |s| {
                s.task_retention = Some(Duration::from_secs(2))
            }),
            ("MINERU_API_TASK_CLEANUP_INTERVAL_SECONDS", |s| {
                s.task_cleanup_interval = Some(Duration::from_secs(2))
            }),
        ];
        for (env_name, set) in server_owned_rows {
            let mut service = service::ServiceOverrides::default();
            set(&mut service);
            let message = server_owned_error(&service, &no_env)
                .unwrap_or_else(|| panic!("CLI-set {env_name} must be rejected"));
            assert!(message.contains(env_name), "{env_name}: {message}");
            let environment =
                Environment::from_values(HashMap::from([(*env_name, OsString::from("2"))]));
            let message = server_owned_error(&service::ServiceOverrides::default(), &environment)
                .unwrap_or_else(|| panic!("env {env_name} must be rejected"));
            assert!(message.contains(env_name), "{env_name}: {message}");
        }
        // Every server-owned knob at once lists the exact full set.
        let all_server = {
            let mut service = service::ServiceOverrides::default();
            for (_, set) in server_owned_rows {
                set(&mut service);
            }
            service
        };
        let message = server_owned_error(&all_server, &no_env).unwrap();
        assert_eq!(
            message.matches("MINERU_").count(),
            server_owned_rows.len(),
            "{message}"
        );
    }

    /// Runs the CLI-set and frozen-environment rejection checks for every (flag, env) row in a
    /// table, then verifies the all-set message reports the exact number of knobs so a knob added
    /// to a rejection function without a row here (or vice versa) fails the test.
    fn assert_knob_rejection<T, F>(rows: &[(&'static str, &'static str, fn(&mut T))], reject: F)
    where
        T: Default,
        F: Fn(&T, &Environment) -> Option<String>,
    {
        let no_env = Environment::from_values(HashMap::new());
        for (flag, env_name, set) in rows {
            let mut overrides = T::default();
            set(&mut overrides);
            let message = reject(&overrides, &no_env)
                .unwrap_or_else(|| panic!("CLI {flag} must be rejected"));
            assert!(message.contains(flag), "CLI {flag}: {message}");
            let environment =
                Environment::from_values(HashMap::from([(*env_name, OsString::from("2"))]));
            let message = reject(&T::default(), &environment)
                .unwrap_or_else(|| panic!("env {env_name} must be rejected"));
            assert!(message.contains(env_name), "env {env_name}: {message}");
        }
        let all = {
            let mut overrides = T::default();
            for (_, _, set) in rows {
                set(&mut overrides);
            }
            overrides
        };
        let message = reject(&all, &no_env).unwrap();
        assert_eq!(message.matches("MINERU_").count(), rows.len(), "{message}");
        let all_env = Environment::from_values(
            rows.iter()
                .map(|(_, env_name, _)| (*env_name, OsString::from("2")))
                .collect(),
        );
        let message = reject(&T::default(), &all_env).unwrap();
        assert_eq!(message.matches("MINERU_").count(), rows.len(), "{message}");
    }

    #[test]
    fn service_snapshot_reaches_office_workers_and_remote_runner() {
        // Resolve the service snapshot and confirm the office limits survive the child env.
        let service = service::resolve_service(
            &(|_| None),
            &service::ServiceOverrides::default(),
            crate::DocumentLimitPolicy::defaults(),
        )
        .unwrap();
        assert_eq!(service.office.input_bytes, 32 * 1024 * 1024);
        assert_eq!(service.ooxml.xml_events, 100_000);
        assert_eq!(service.task_result_timeout, Duration::from_secs(3600));
        let env = service.office.child_env();
        assert!(
            env.iter()
                .any(|(name, value)| name == service::OFFICE_INPUT_ENV
                    && value.to_str() == Some("33554432"))
        );
    }
}
