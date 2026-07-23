//! Shared high-level command execution.

mod direct;
#[doc(hidden)]
pub mod env;
#[doc(hidden)]
pub mod plain;
mod rich;

use crate::{
    OfficeWorkers, OfficialPdfOptions, ProgressCallback, ProgressEvent, RemoteApiDocument,
    RemoteApiOptions, normalize_remote_language, parse_remote_api_env, sanitize_event_text,
    selected_document_pages,
};
use clap::{ArgAction, Parser, error::ErrorKind};
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
};

const WARNING_CAP: usize = 64;
const TEXT_CAP: usize = 512;
const FAILURE_CAP: usize = 4096;
const ENV_NAMES: [&str; 17] = [
    "MINERU_LOG_LEVEL",
    "MINERU_PROCESSING_WINDOW_SIZE",
    "MINERU_PDF_RENDER_THREADS",
    "MINERU_PDF_RENDER_TIMEOUT",
    "MINERU_FORMULA_ENABLE",
    "MINERU_TABLE_ENABLE",
    "MINERU_API_MAX_CONCURRENT_REQUESTS",
    "MINERU_TASK_RESULT_TIMEOUT_SECONDS",
    "MINERU_TASK_RESULT_DOWNLOAD_TIMEOUT_SECONDS",
    "MINERU_VL_SERVER",
    "MINERU_VL_MODEL_NAME",
    "MINERU_VL_API_KEY",
    "MINERU_VL_DEBUG_ENABLE",
    "MINERU_VLM_END_TOKEN",
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

    fn vlm_http_config(&self) -> crate::VlmHttpConfig {
        crate::VlmHttpConfig::from_env(|name| self.string(name))
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

    fn office_workers(&self) -> OfficeWorkers {
        OfficeWorkers::with_executable(self.office_executable.clone())
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
    run_core(options, &context, events, warnings).await?;
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
    events: CommandCallback,
    warnings: direct::WarningCallback,
) -> Result<(), RunError> {
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
        run_api(options, api_url, language, context, events, warnings).await
    } else {
        direct::run_with_scoped_events(
            direct::DirectOptions {
                input: options.path,
                output: options.output,
                base_url: options.url,
                server_option_label: "--url",
                model: None,
                api_key: None,
                page_start: Some(options.start),
                page_end: options.end,
                no_formula: !options.formula,
                no_table: !options.table,
                no_image_analysis: !options.image_analysis,
                batch_size: 1,
                canonical_mixed: true,
            },
            context.office_workers(),
            context.environment.clone(),
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
    context: &RunContext,
    events: CommandCallback,
    warnings: direct::WarningCallback,
) -> Result<(), RunError> {
    if options.client_side_output_generation {
        return Err(RunError::new(
            "client-side output generation is unsupported",
        ));
    }
    let env =
        parse_remote_api_env(|name| context.environment.string(name)).map_err(RunError::new)?;
    let mut route = OfficialPdfOptions::default();
    route.start_page = options.start;
    route.end_page = options.end;
    route.formula_enable = options.formula;
    route.table_enable = options.table;
    route.image_analysis = options.image_analysis;
    if env::apply_route_env(&mut route, |name| context.environment.os(name)) {
        warnings("MINERU_PROCESSING_WINDOW_SIZE", "invalid value; using 64");
    }
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
        server_url: options.url,
        start,
        end,
        formula: route.formula_enable,
        table: route.table_enable,
        image_analysis: route.image_analysis,
        client_side_output_generation: false,
        route,
    };
    let failures = crate::mineru_api::run_remote_api_documents_scoped_with_workers(
        documents,
        options.output,
        api_url,
        remote_options,
        env,
        Some(events),
        context.office_workers(),
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
    disable_version_flag = true
)]
struct Cli {
    #[arg(short = 'v', long, action = ArgAction::Version)]
    version: Option<bool>,
    #[arg(short = 'p', long)]
    path: PathBuf,
    #[arg(short = 'o', long)]
    output: PathBuf,
    #[arg(long)]
    api_url: Option<String>,
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
    #[arg(short = 'f', long, action = ArgAction::Set, default_value_t = true)]
    formula: bool,
    #[arg(short = 't', long, action = ArgAction::Set, default_value_t = true)]
    table: bool,
    #[arg(long, action = ArgAction::Set, default_value_t = true)]
    image_analysis: bool,
    #[arg(long, action = ArgAction::Set, default_value_t = false)]
    client_side_output_generation: bool,
}

impl From<Cli> for RunOptions {
    fn from(cli: Cli) -> Self {
        Self {
            path: cli.path,
            output: cli.output,
            api_url: cli.api_url,
            method: cli.method,
            backend: cli.backend,
            effort: cli.effort,
            lang: cli.lang,
            url: cli.url,
            start: cli.start,
            end: cli.end,
            formula: cli.formula,
            table: cli.table,
            image_analysis: cli.image_analysis,
            client_side_output_generation: cli.client_side_output_generation,
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
            Self::Plain(Arc::new(plain::EventSink::new(
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
    let options = match Cli::try_parse_from(args) {
        Ok(cli) => RunOptions::from(cli),
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
    let level = match plain::LogLevel::parse(context.environment.os("MINERU_LOG_LEVEL")) {
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
    let result = run_with_context(options, context).await;
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

#[doc(hidden)]
#[derive(Clone, Debug)]
pub struct LegacyDirectOptions {
    pub input: PathBuf,
    pub output: PathBuf,
    pub base_url: Option<String>,
    pub model: Option<String>,
    pub api_key: Option<String>,
    pub page_start: Option<usize>,
    pub page_end: Option<usize>,
    pub no_formula: bool,
    pub no_table: bool,
    pub no_image_analysis: bool,
    pub batch_size: usize,
}

#[doc(hidden)]
pub async fn run_legacy_direct(options: LegacyDirectOptions) -> Result<(), RunError> {
    direct::run_legacy(
        direct::DirectOptions {
            input: options.input,
            output: options.output,
            base_url: options.base_url,
            server_option_label: "--base-url",
            model: options.model,
            api_key: options.api_key,
            page_start: options.page_start,
            page_end: options.page_end,
            no_formula: options.no_formula,
            no_table: options.no_table,
            no_image_analysis: options.no_image_analysis,
            batch_size: options.batch_size,
            canonical_mixed: false,
        },
        Environment::process(),
    )
    .await
    .map_err(RunError::new)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        future::Future,
        process::Command,
        sync::atomic::{AtomicUsize, Ordering as AtomicOrdering},
    };

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
        let options = RunOptions::from(
            Cli::try_parse_from([
                "mineru",
                "-p",
                "a",
                "-o",
                "b",
                "--api-url",
                "http://api",
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
            ])
            .unwrap(),
        );
        assert_eq!(options.path, PathBuf::from("a"));
        assert_eq!(options.output, PathBuf::from("b"));
        assert_eq!(options.api_url.as_deref(), Some("http://api"));
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
        let config = context.environment.vlm_http_config();
        match mode.as_str() {
            "present" => {
                assert_eq!(
                    config.authorization().as_deref(),
                    Some("Bearer snapshot-key")
                );
                assert_eq!(config.end_token, "snapshot-end");
            }
            "absent" | "non-utf8" => {
                assert_eq!(config.authorization(), None);
                assert_eq!(config.end_token, "<|im_end|>");
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
    fn parser_contract_rejects_old_and_missing_options() {
        assert!(Cli::try_parse_from(["mineru"]).is_err());
        for option in [
            "--server-url",
            "--model",
            "--api-key",
            "--batch-size",
            "--start-page",
            "--end-page",
            "--no-formula",
            "--no-table",
            "--no-image-analysis",
            "--log-level",
        ] {
            assert!(Cli::try_parse_from(["mineru", "-p", "a", "-o", "b", option, "x"]).is_err());
        }
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
}
