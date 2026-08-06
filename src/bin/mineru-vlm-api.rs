use clap::Parser;
use mineru::command::env::CoreOverrides;
use mineru::command::plain::{EventSink, LogLevel};
use mineru::command::service::{self, ServiceOverrides};
use mineru::vlm_api::ServiceConfig;
use mineru::{DocumentLimitOverrides, DocumentLimitPolicy};
use std::sync::Arc;
use std::{
    ffi::OsString,
    future::{Future, pending},
    io::{IsTerminal, Read, stderr},
    net::IpAddr,
    path::PathBuf,
    process::ExitCode,
};

#[derive(Parser)]
#[command(about = "MinerU mixed vlm-http-client task service")]
struct Args {
    #[arg(long, default_value = "127.0.0.1")]
    host: IpAddr,
    #[arg(long, default_value_t = 8000)]
    port: u16,
    #[arg(long)]
    output_root: Option<PathBuf>,
    #[arg(long)]
    concurrency: Option<String>,
    #[arg(long)]
    shutdown_on_stdin_eof: bool,
    /// Explicitly allow binding a non-loopback address (default: only loopback is permitted).
    #[arg(long, action = clap::ArgAction::Set)]
    public_bind_exposed: Option<bool>,
    /// Explicitly allow VLM HTTP requests from non-loopback clients on a public listener.
    #[arg(long, action = clap::ArgAction::Set)]
    allow_public_http_client: Option<bool>,
    #[arg(long)]
    max_input_bytes: Option<String>,
    #[arg(long)]
    max_encoded_document_bytes: Option<String>,
    #[arg(long)]
    max_output_bytes: Option<String>,
    #[arg(long)]
    official_page_concurrency: Option<String>,
    #[arg(long)]
    processing_window_size: Option<String>,
    #[arg(long)]
    render_workers: Option<String>,
    #[arg(long)]
    render_timeout_seconds: Option<String>,
    #[arg(long, action = clap::ArgAction::Set)]
    formula: Option<bool>,
    #[arg(long, action = clap::ArgAction::Set)]
    table: Option<bool>,
    #[arg(long, action = clap::ArgAction::Set)]
    image_analysis: Option<bool>,
    /// VLM HTTP client debug flag resolved once at startup (frozen env `MINERU_VL_DEBUG_ENABLE`).
    #[arg(long, action = clap::ArgAction::Set)]
    vlm_debug: Option<bool>,
    /// VLM transport text-before-image (frozen env `MINERU_VLM_TEXT_BEFORE_IMAGE`).
    #[arg(long, action = clap::ArgAction::Set)]
    vlm_text_before_image: Option<bool>,
    /// VLM transport truncated-content allowance (frozen env `MINERU_VLM_ALLOW_TRUNCATED_CONTENT`).
    #[arg(long, action = clap::ArgAction::Set)]
    vlm_allow_truncated_content: Option<bool>,
    /// VLM transport remote-image allowance (frozen env `MINERU_VLM_ALLOW_REMOTE_IMAGES`).
    #[arg(long, action = clap::ArgAction::Set)]
    vlm_allow_remote_images: Option<bool>,
    /// VLM transport private-remote-image allowance (frozen env `MINERU_VLM_ALLOW_PRIVATE_REMOTE_IMAGES`).
    #[arg(long, action = clap::ArgAction::Set)]
    vlm_allow_private_remote_images: Option<bool>,
    #[arg(long)]
    task_retention_seconds: Option<String>,
    #[arg(long)]
    task_cleanup_interval_seconds: Option<String>,
    #[arg(long)]
    record_cap: Option<String>,
    #[arg(long)]
    file_cap: Option<String>,
    #[arg(long)]
    body_cap: Option<String>,
    #[arg(long)]
    text_cap: Option<String>,
    #[arg(long)]
    text_total_cap: Option<String>,
    #[arg(long)]
    form_fields_cap: Option<String>,
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
}

fn main() -> ExitCode {
    let args = Args::parse();
    let level = match LogLevel::from_env() {
        Ok(level) => level,
        Err(error) => {
            eprintln!("{error}");
            return ExitCode::FAILURE;
        }
    };
    let is_tty = stderr().is_terminal();
    let sink = Arc::new(EventSink::new(stderr(), is_tty, level));
    let result = (|| -> Result<(), Box<dyn std::error::Error>> {
        let env = startup_config(&args)?;
        if !args.host.is_loopback() && !env.public_bind_exposed {
            return Err("--host must be a loopback IP address".into());
        }
        let config = ServiceConfig::new(
            env.concurrency,
            env.output_root,
            env.route,
            env.formula,
            env.table,
        )?
        .official_page_concurrency(env.official_page_concurrency)?
        .document_limits(env.document_limits)
        .http_config(env.http)
        .image_analysis(env.image_analysis)
        .public_policy(env.public_bind_exposed, env.allow_public_http_client)
        .task_lifecycle(
            env.service.task_retention,
            env.service.task_cleanup_interval,
        )?
        .service_policy(env.service)
        .progress_callback(sink.callback());
        Ok(tokio::runtime::Runtime::new()?.block_on(async move {
            let listener = tokio::net::TcpListener::bind((args.host, args.port)).await?;
            mineru::vlm_api::serve(listener, config, shutdown(env.shutdown_on_stdin_eof)).await
        })?)
    })();
    if let Err(error) = &result {
        sink.fail(&error.to_string());
    }
    sink.finish();
    if result.is_ok() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

async fn ctrl_c_shutdown() {
    if tokio::signal::ctrl_c().await.is_err() {
        pending::<()>().await;
    }
}

#[cfg(unix)]
async fn sigterm_shutdown() {
    let Ok(mut signal) = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
    else {
        pending::<()>().await;
        return;
    };
    if signal.recv().await.is_none() {
        pending::<()>().await;
    }
}

async fn process_shutdown() {
    #[cfg(unix)]
    tokio::select! {
        () = ctrl_c_shutdown() => (),
        () = sigterm_shutdown() => (),
    }
    #[cfg(not(unix))]
    ctrl_c_shutdown().await;
}

fn read_until_eof_or_error(reader: &mut impl Read) {
    let mut bytes = [0; 1024];
    while reader.read(&mut bytes).is_ok_and(|count| count != 0) {}
}

fn stdin_watcher(enabled: bool) -> Option<tokio::sync::oneshot::Receiver<()>> {
    enabled.then(|| {
        let (sender, receiver) = tokio::sync::oneshot::channel();
        let _ = std::thread::Builder::new().spawn(move || {
            let mut stdin = std::io::stdin().lock();
            read_until_eof_or_error(&mut stdin);
            let _ = sender.send(());
        });
        receiver
    })
}

async fn stdin_shutdown(receiver: tokio::sync::oneshot::Receiver<()>) {
    if receiver.await.is_err() {
        pending::<()>().await;
    }
}

fn shutdown(enabled: bool) -> impl Future<Output = ()> {
    let stdin = stdin_watcher(enabled);
    async move {
        tokio::select! {
            () = process_shutdown() => (),
            () = async { match stdin { Some(receiver) => stdin_shutdown(receiver).await, None => pending::<()>().await } } => (),
        }
    }
}

struct StartupEnv {
    output_root: PathBuf,
    concurrency: usize,
    official_page_concurrency: usize,
    public_bind_exposed: bool,
    allow_public_http_client: bool,
    shutdown_on_stdin_eof: bool,
    route: mineru::OfficialPdfOptions,
    http: mineru::VlmHttpConfig,
    formula: Option<bool>,
    table: Option<bool>,
    image_analysis: Option<bool>,
    document_limits: DocumentLimitPolicy,
    service: service::ResolvedService,
}

/// Environment names the task service consumes. Client-only controls (task result/download
/// timing, API transport timing, result-archive entries/ratio) are not read here; no worker may
/// re-read a drifting process environment after startup. The VLM transport identity names are
/// consumed as the frozen base `VlmHttpConfig` resolved at startup.
const CONSUMED_NAMES: [&str; 84] = [
    "MINERU_API_OUTPUT_ROOT",
    "MINERU_API_MAX_CONCURRENT_REQUESTS",
    "MINERU_API_PUBLIC_BIND_EXPOSED",
    "MINERU_API_ALLOW_PUBLIC_HTTP_CLIENT",
    "MINERU_API_SHUTDOWN_ON_STDIN_EOF",
    "MINERU_OFFICIAL_PAGE_CONCURRENCY",
    "MINERU_PROCESSING_WINDOW_SIZE",
    "MINERU_PDF_RENDER_THREADS",
    "MINERU_PDF_RENDER_TIMEOUT",
    "MINERU_FORMULA_ENABLE",
    "MINERU_TABLE_ENABLE",
    "MINERU_IMAGE_ANALYSIS_ENABLE",
    "MINERU_VL_DEBUG_ENABLE",
    "MINERU_VL_SERVER",
    "MINERU_VL_MODEL_NAME",
    "MINERU_VL_API_KEY",
    "MINERU_VLM_END_TOKEN",
    "MINERU_VLM_TEXT_BEFORE_IMAGE",
    "MINERU_VLM_ALLOW_TRUNCATED_CONTENT",
    "MINERU_VLM_ALLOW_REMOTE_IMAGES",
    "MINERU_VLM_ALLOW_PRIVATE_REMOTE_IMAGES",
    // Core route knobs resolved at startup and carried into every task's inference.
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
    // VLM transport knobs resolved into the frozen base `VlmHttpConfig` at startup.
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
    "MINERU_MAX_INPUT_BYTES",
    "MINERU_MAX_ENCODED_DOCUMENT_BYTES",
    "MINERU_MAX_OUTPUT_BYTES",
    "MINERU_API_TASK_RETENTION_SECONDS",
    "MINERU_API_TASK_CLEANUP_INTERVAL_SECONDS",
    "MINERU_API_RECORD_CAP",
    "MINERU_API_FILE_CAP",
    "MINERU_API_BODY_CAP",
    "MINERU_API_TEXT_CAP",
    "MINERU_API_TEXT_TOTAL_CAP",
    "MINERU_API_FORM_FIELDS_CAP",
    "MINERU_ZIP_SCAN_CENTRAL_CAP",
    "MINERU_ZIP_SCAN_NAME_CAP",
    "MINERU_ZIP_SCAN_DEPTH_CAP",
    "MINERU_ZIP_SCAN_TOTAL_NAME_CAP",
    "MINERU_ZIP_SCAN_TOTAL_COMPONENT_CAP",
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
    "MINERU_OOXML_ARCHIVE_BYTES",
    "MINERU_OOXML_EXPANDED_BYTES",
    "MINERU_OOXML_XML_ENTRY_BYTES",
    "MINERU_OOXML_XML_TOTAL_BYTES",
    "MINERU_OOXML_RATIO",
    "MINERU_OOXML_XML_DEPTH",
    "MINERU_OOXML_XML_EVENTS",
    "MINERU_OOXML_XML_ATTRIBUTES",
    "MINERU_OOXML_XML_NAMESPACES",
];

fn api_flag(value: Option<&OsString>) -> bool {
    value
        .and_then(|v| v.to_str())
        .is_some_and(|v| matches!(v.to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on"))
}

/// Frozen process environment snapshot: read exactly once, then every resolution reads the map.
fn snapshot_process_env() -> std::collections::HashMap<OsString, OsString> {
    std::env::vars_os().collect()
}

/// Resolves the frozen core + service policy with the owning CLI's explicit overrides.
fn startup_config(args: &Args) -> Result<StartupEnv, String> {
    let frozen = snapshot_process_env();
    let (service_cli, service_masked) = server_service_overrides(args)?;
    let (core_cli, core_masked) = server_core_overrides(args)?;
    let overridden = {
        let mut names = std::collections::HashSet::new();
        names.extend(service_masked.iter().copied());
        names.extend(core_masked.iter().copied());
        names
    };
    let lookup = |name: &str| {
        if overridden.contains(name) || !CONSUMED_NAMES.contains(&name) {
            None
        } else {
            frozen.get(std::ffi::OsStr::new(name)).cloned()
        }
    };
    let document_limits = DocumentLimitPolicy::resolve(
        &DocumentLimitOverrides {
            max_input_bytes: args.max_input_bytes.clone(),
            max_encoded_document_bytes: args.max_encoded_document_bytes.clone(),
            max_output_bytes: args.max_output_bytes.clone(),
        },
        &lookup,
    )?;
    let core = mineru::command::env::resolve_core(&lookup, &core_cli)?;
    let service = service::resolve_service(&lookup, &service_cli, document_limits)?;
    Ok(StartupEnv {
        output_root: args
            .output_root
            .clone()
            .or_else(|| {
                frozen
                    .get(&OsString::from("MINERU_API_OUTPUT_ROOT"))
                    .cloned()
                    .map(Into::into)
            })
            .unwrap_or_else(|| PathBuf::from("./output")),
        concurrency: service.remote_concurrency,
        official_page_concurrency: core.page_concurrency,
        public_bind_exposed: args
            .public_bind_exposed
            .or_else(|| {
                frozen
                    .get(&OsString::from("MINERU_API_PUBLIC_BIND_EXPOSED"))
                    .map(|value| api_flag(Some(value)))
            })
            .unwrap_or(false),
        allow_public_http_client: args
            .allow_public_http_client
            .or_else(|| {
                frozen
                    .get(&OsString::from("MINERU_API_ALLOW_PUBLIC_HTTP_CLIENT"))
                    .map(|value| api_flag(Some(value)))
            })
            .unwrap_or(false),
        shutdown_on_stdin_eof: api_flag(
            frozen.get(&OsString::from("MINERU_API_SHUTDOWN_ON_STDIN_EOF")),
        ) || args.shutdown_on_stdin_eof,
        route: core.route,
        http: core.http,
        formula: env_or_cli_bool(&frozen, core_cli.formula, "MINERU_FORMULA_ENABLE")?,
        table: env_or_cli_bool(&frozen, core_cli.table, "MINERU_TABLE_ENABLE")?,
        image_analysis: env_or_cli_bool(
            &frozen,
            core_cli.image_analysis,
            "MINERU_IMAGE_ANALYSIS_ENABLE",
        )?,
        document_limits,
        service,
    })
}

/// Strict boolean resolution for the operator pins: an explicit CLI value wins; otherwise the
/// frozen environment must be exactly `true` or `false` (mirroring `env::strict_bool`). A
/// malformed or non-boolean value fails before work instead of silently dropping the pin.
fn env_or_cli_bool(
    frozen: &std::collections::HashMap<OsString, OsString>,
    cli: Option<bool>,
    name: &'static str,
) -> Result<Option<bool>, String> {
    match cli {
        Some(value) => Ok(Some(value)),
        None => match frozen.get(&OsString::from(name)) {
            Some(value) => strict_flag(value, name).map(Some),
            None => Ok(None),
        },
    }
}

fn strict_flag(value: &OsString, name: &str) -> Result<bool, String> {
    let text = value
        .to_str()
        .ok_or_else(|| format!("{name} must be true or false"))?
        .trim();
    match text.to_ascii_lowercase().as_str() {
        "true" => Ok(true),
        "false" => Ok(false),
        _ => Err(format!("{name} must be true or false")),
    }
}

/// Explicit CLI overrides for the service policy (server-owned knobs only).
fn server_service_overrides(args: &Args) -> Result<(ServiceOverrides, Vec<&'static str>), String> {
    let mut cli = ServiceOverrides::default();
    let set = |name: &'static str, value: Option<&String>| value.map(|v| (name, v.clone()));
    let mut values = Vec::new();
    for value in [
        set(
            "MINERU_API_MAX_CONCURRENT_REQUESTS",
            args.concurrency.as_ref(),
        ),
        set(
            "MINERU_API_TASK_RETENTION_SECONDS",
            args.task_retention_seconds.as_ref(),
        ),
        set(
            "MINERU_API_TASK_CLEANUP_INTERVAL_SECONDS",
            args.task_cleanup_interval_seconds.as_ref(),
        ),
        set("MINERU_API_RECORD_CAP", args.record_cap.as_ref()),
        set("MINERU_API_FILE_CAP", args.file_cap.as_ref()),
        set("MINERU_API_BODY_CAP", args.body_cap.as_ref()),
        set("MINERU_API_TEXT_CAP", args.text_cap.as_ref()),
        set("MINERU_API_TEXT_TOTAL_CAP", args.text_total_cap.as_ref()),
        set("MINERU_API_FORM_FIELDS_CAP", args.form_fields_cap.as_ref()),
        set(
            "MINERU_OFFICE_INPUT_BYTES",
            args.office_input_bytes.as_ref(),
        ),
        set(
            "MINERU_OFFICE_OUTPUT_BYTES",
            args.office_output_bytes.as_ref(),
        ),
        set(
            "MINERU_OFFICE_STDERR_BYTES",
            args.office_stderr_bytes.as_ref(),
        ),
        set(
            "MINERU_OFFICE_WALL_SECONDS",
            args.office_wall_seconds.as_ref(),
        ),
        set(
            "MINERU_OFFICE_CPU_SECONDS",
            args.office_cpu_seconds.as_ref(),
        ),
        set("MINERU_OFFICE_NOFILE", args.office_nofile.as_ref()),
        set(
            "MINERU_OFFICE_ADDRESS_SPACE_BYTES",
            args.office_address_space_bytes.as_ref(),
        ),
        set(
            "MINERU_OFFICE_ACTIVE_PROCESS_LIMIT",
            args.office_active_process_limit.as_ref(),
        ),
        set(
            "MINERU_OFFICE_PROCESS_MEMORY_BYTES",
            args.office_process_memory_bytes.as_ref(),
        ),
        set(
            "MINERU_OFFICE_JOB_MEMORY_BYTES",
            args.office_job_memory_bytes.as_ref(),
        ),
        set(
            "MINERU_OFFICE_PROCESS_TIME_SECONDS",
            args.office_process_time_seconds.as_ref(),
        ),
        set(
            "MINERU_OFFICE_JOB_TIME_SECONDS",
            args.office_job_time_seconds.as_ref(),
        ),
        set(
            "MINERU_OOXML_ARCHIVE_BYTES",
            args.ooxml_archive_bytes.as_ref(),
        ),
        set(
            "MINERU_OOXML_EXPANDED_BYTES",
            args.ooxml_expanded_bytes.as_ref(),
        ),
        set(
            "MINERU_OOXML_XML_ENTRY_BYTES",
            args.ooxml_xml_entry_bytes.as_ref(),
        ),
        set(
            "MINERU_OOXML_XML_TOTAL_BYTES",
            args.ooxml_xml_total_bytes.as_ref(),
        ),
        set("MINERU_OOXML_RATIO", args.ooxml_ratio.as_ref()),
        set("MINERU_OOXML_XML_DEPTH", args.ooxml_xml_depth.as_ref()),
        set("MINERU_OOXML_XML_EVENTS", args.ooxml_xml_events.as_ref()),
        set(
            "MINERU_OOXML_XML_ATTRIBUTES",
            args.ooxml_xml_attributes.as_ref(),
        ),
        set(
            "MINERU_OOXML_XML_NAMESPACES",
            args.ooxml_xml_namespaces.as_ref(),
        ),
    ] {
        if let Some((name, value)) = value {
            values.push((name, OsString::from(value)));
        }
    }
    // Strict boolean transport knobs: an explicit flag wins over the frozen environment with the
    // same precedence as every other server knob. The names are masked when set so the
    // environment cannot leak through the frozen snapshot.
    for (name, value) in [
        ("MINERU_VLM_TEXT_BEFORE_IMAGE", args.vlm_text_before_image),
        (
            "MINERU_VLM_ALLOW_TRUNCATED_CONTENT",
            args.vlm_allow_truncated_content,
        ),
        (
            "MINERU_VLM_ALLOW_REMOTE_IMAGES",
            args.vlm_allow_remote_images,
        ),
        (
            "MINERU_VLM_ALLOW_PRIVATE_REMOTE_IMAGES",
            args.vlm_allow_private_remote_images,
        ),
    ] {
        if let Some(value) = value {
            values.push((name, OsString::from(value.to_string())));
        }
    }
    let parsed = service::parse_service_overrides(&|name| {
        values
            .iter()
            .find(|(key, _)| *key == name)
            .map(|(_, value)| value.clone())
    })?;
    cli.vlm_text_before_image = parsed.vlm_text_before_image;
    cli.vlm_allow_truncated_content = parsed.vlm_allow_truncated_content;
    cli.vlm_allow_remote_images = parsed.vlm_allow_remote_images;
    cli.vlm_allow_private_remote_images = parsed.vlm_allow_private_remote_images;
    cli.api_max_concurrent_requests = parsed.api_max_concurrent_requests;
    cli.task_retention = parsed.task_retention;
    cli.task_cleanup_interval = parsed.task_cleanup_interval;
    cli.server_record_cap = parsed.server_record_cap;
    cli.server_file_cap = parsed.server_file_cap;
    cli.server_body_cap = parsed.server_body_cap;
    cli.server_text_cap = parsed.server_text_cap;
    cli.server_text_total_cap = parsed.server_text_total_cap;
    cli.server_form_fields_cap = parsed.server_form_fields_cap;
    cli.office_input_bytes = parsed.office_input_bytes;
    cli.office_output_bytes = parsed.office_output_bytes;
    cli.office_stderr_bytes = parsed.office_stderr_bytes;
    cli.office_wall_seconds = parsed.office_wall_seconds;
    cli.office_cpu_seconds = parsed.office_cpu_seconds;
    cli.office_nofile = parsed.office_nofile;
    cli.office_address_space_bytes = parsed.office_address_space_bytes;
    cli.office_active_process_limit = parsed.office_active_process_limit;
    cli.office_process_memory_bytes = parsed.office_process_memory_bytes;
    cli.office_job_memory_bytes = parsed.office_job_memory_bytes;
    cli.office_process_time_seconds = parsed.office_process_time_seconds;
    cli.office_job_time_seconds = parsed.office_job_time_seconds;
    cli.ooxml_archive_bytes = parsed.ooxml_archive_bytes;
    cli.ooxml_expanded_bytes = parsed.ooxml_expanded_bytes;
    cli.ooxml_xml_entry_bytes = parsed.ooxml_xml_entry_bytes;
    cli.ooxml_xml_total_bytes = parsed.ooxml_xml_total_bytes;
    cli.ooxml_ratio = parsed.ooxml_ratio;
    cli.ooxml_xml_depth = parsed.ooxml_xml_depth;
    cli.ooxml_xml_events = parsed.ooxml_xml_events;
    cli.ooxml_xml_attributes = parsed.ooxml_xml_attributes;
    cli.ooxml_xml_namespaces = parsed.ooxml_xml_namespaces;
    let masked = values.iter().map(|(name, _)| *name).collect();
    Ok((cli, masked))
}

/// Explicit CLI overrides for the core route policy (server-consumed knobs only).
fn server_core_overrides(args: &Args) -> Result<(CoreOverrides, Vec<&'static str>), String> {
    let mut values = Vec::new();
    let set = |name: &'static str, value: Option<&String>| value.map(|v| (name, v.clone()));
    for value in [
        set(
            "MINERU_OFFICIAL_PAGE_CONCURRENCY",
            args.official_page_concurrency.as_ref(),
        ),
        set(
            "MINERU_PROCESSING_WINDOW_SIZE",
            args.processing_window_size.as_ref(),
        ),
        set("MINERU_PDF_RENDER_THREADS", args.render_workers.as_ref()),
        set(
            "MINERU_PDF_RENDER_TIMEOUT",
            args.render_timeout_seconds.as_ref(),
        ),
    ] {
        if let Some((name, value)) = value {
            values.push((name, OsString::from(value)));
        }
    }
    let mut cli = mineru::command::env::parse_core_overrides(&|name| {
        values
            .iter()
            .find(|(key, _)| *key == name)
            .map(|(_, value)| value.clone())
    })?;
    cli.formula = args.formula;
    cli.table = args.table;
    cli.image_analysis = args.image_analysis;
    cli.vlm_debug = args.vlm_debug;
    if args.formula.is_some() {
        values.push(("MINERU_FORMULA_ENABLE", OsString::new()));
    }
    if args.table.is_some() {
        values.push(("MINERU_TABLE_ENABLE", OsString::new()));
    }
    if args.image_analysis.is_some() {
        values.push(("MINERU_IMAGE_ANALYSIS_ENABLE", OsString::new()));
    }
    if args.vlm_debug.is_some() {
        values.push(("MINERU_VL_DEBUG_ENABLE", OsString::new()));
    }
    let masked = values.iter().map(|(name, _)| *name).collect();
    Ok((cli, masked))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn args(values: &[&str]) -> Args {
        Args::try_parse_from(std::iter::once("mineru-vlm-api").chain(values.iter().copied()))
            .unwrap()
    }

    /// Runs startup_config in a scrubbed child-free test by mutating the process environment
    /// under a mutex; every test in this module uses the lock.
    fn configured(values: &[(&str, &str)], cli: &Args) -> Result<StartupEnv, String> {
        use std::sync::{LazyLock, Mutex};
        static LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));
        let _guard = LOCK.lock().unwrap();
        let mut saved = Vec::new();
        for (name, value) in values {
            let key = OsString::from(*name);
            saved.push((key.clone(), std::env::var_os(name)));
            // SAFETY: serialized by the mutex in this single-threaded test process.
            unsafe { std::env::set_var(key, value) };
        }
        let result = startup_config(cli);
        for (key, value) in saved {
            // SAFETY: restoring under the same mutex.
            match value {
                Some(value) => unsafe { std::env::set_var(key, value) },
                None => unsafe { std::env::remove_var(key) },
            }
        }
        result
    }

    #[test]
    fn cli_service_values_override_env_and_omissions_preserve_it() {
        let cli = args(&[
            "--output-root",
            "cli-output",
            "--concurrency",
            "7",
            "--shutdown-on-stdin-eof",
            "--record-cap",
            "40",
        ]);
        let env = configured(
            &[
                ("MINERU_API_OUTPUT_ROOT", "env-output"),
                (
                    "MINERU_API_MAX_CONCURRENT_REQUESTS",
                    "invalid-but-overridden",
                ),
            ],
            &cli,
        )
        .unwrap();
        assert_eq!(env.output_root, PathBuf::from("cli-output"));
        assert_eq!(env.concurrency, 7);
        assert!(env.shutdown_on_stdin_eof);
        assert_eq!(env.service.server.record_cap, 40);

        let cli = args(&[]);
        let env = configured(
            &[
                ("MINERU_API_OUTPUT_ROOT", "env-output"),
                ("MINERU_API_MAX_CONCURRENT_REQUESTS", "5"),
                ("MINERU_API_SHUTDOWN_ON_STDIN_EOF", "true"),
            ],
            &cli,
        )
        .unwrap();
        assert_eq!(env.output_root, PathBuf::from("env-output"));
        assert_eq!(env.concurrency, 5);
        assert!(env.shutdown_on_stdin_eof);
    }

    #[test]
    fn concurrency_zero_rejected_at_startup_config() {
        let cli = args(&["--concurrency", "0"]);
        assert!(configured(&[], &cli).is_err());
        let cli = args(&["--concurrency", "7"]);
        assert_eq!(configured(&[], &cli).unwrap().concurrency, 7);
    }

    #[test]
    fn document_limit_controls_are_accepted_and_override_environment() {
        let cli = args(&[
            "--max-input-bytes",
            "11",
            "--max-encoded-document-bytes",
            "12",
            "--max-output-bytes",
            "13",
        ]);
        let env = configured(
            &[
                ("MINERU_MAX_INPUT_BYTES", "bad"),
                ("MINERU_MAX_ENCODED_DOCUMENT_BYTES", "bad"),
                ("MINERU_MAX_OUTPUT_BYTES", "bad"),
            ],
            &cli,
        )
        .unwrap();
        assert_eq!(
            (
                env.document_limits.max_input_bytes,
                env.document_limits.max_encoded_document_bytes,
                env.document_limits.max_output_bytes
            ),
            (11, 12, 13)
        );
    }

    #[test]
    fn defaults_and_parsers() {
        let env = configured(&[], &args(&[])).unwrap();
        assert_eq!(env.output_root, PathBuf::from("./output"));
        assert_eq!(env.concurrency, 3);
        assert_eq!(env.official_page_concurrency, 4);
        assert_eq!(env.service.task_retention, Duration::from_secs(86400));
        assert_eq!(env.service.task_cleanup_interval, Duration::from_secs(300));
        assert_eq!(env.service.server.record_cap, 32);
        assert!(
            !env.public_bind_exposed && !env.allow_public_http_client && !env.shutdown_on_stdin_eof
        );
        let env = configured(
            &[
                ("MINERU_API_OUTPUT_ROOT", ""),
                ("MINERU_API_MAX_CONCURRENT_REQUESTS", "1_024"),
                ("MINERU_API_PUBLIC_BIND_EXPOSED", "YES"),
                ("MINERU_API_ALLOW_PUBLIC_HTTP_CLIENT", "on"),
                ("MINERU_API_SHUTDOWN_ON_STDIN_EOF", "1"),
                ("MINERU_OFFICIAL_PAGE_CONCURRENCY", "9"),
                ("MINERU_PROCESSING_WINDOW_SIZE", "7"),
                ("MINERU_PDF_RENDER_THREADS", "8"),
                ("MINERU_PDF_RENDER_TIMEOUT", "9"),
                ("MINERU_FORMULA_ENABLE", "TRUE"),
                ("MINERU_TABLE_ENABLE", "false"),
                ("MINERU_API_RECORD_CAP", "40"),
            ],
            &args(&[]),
        )
        .unwrap();
        assert_eq!(env.output_root, PathBuf::new());
        assert_eq!(env.concurrency, 1024);
        // Explicit page concurrency above the removed 1..=8 ceiling is accepted.
        assert_eq!(env.official_page_concurrency, 9);
        assert_eq!(env.service.server.record_cap, 40);
        assert!(
            env.public_bind_exposed && env.allow_public_http_client && env.shutdown_on_stdin_eof
        );
        assert_eq!(env.route.processing_window_size, 7);
        assert_eq!(env.route.render_workers, 8);
        assert_eq!(env.route.render_timeout, Duration::from_secs(9));
        assert_eq!(env.route.formula_enable, true);
        assert_eq!(env.route.table_enable, false);
    }

    #[test]
    fn newly_consumed_core_and_transport_env_knobs_reach_startup_config() {
        let env = configured(
            &[
                ("MINERU_MAX_PDF_BYTES", "1000"),
                ("MINERU_MAX_PAGES", "11"),
                ("MINERU_MAX_PAGE_PIXELS", "12"),
                ("MINERU_MAX_RENDERED_IMAGE_BYTES", "13"),
                ("MINERU_MAX_IN_FLIGHT_IMAGE_BYTES", "14"),
                ("MINERU_MAX_RAW_OUTPUT_BYTES", "15"),
                ("MINERU_MAX_LAYOUT_BLOCKS_PER_PAGE", "16"),
                ("MINERU_MAX_SEMANTIC_REQUESTS_PER_PAGE", "17"),
                ("MINERU_BATCH_SIZE", "18"),
                ("MINERU_MAX_ENCODED_REQUEST_BYTES", "19"),
                ("MINERU_MAX_ENCODED_BATCH_BYTES", "20"),
                ("MINERU_MAX_TOTAL_ASSET_BYTES", "21"),
                ("MINERU_MAX_STAGED_TEXT_BYTES", "22"),
                ("MINERU_TOTAL_DEADLINE_SECONDS", "23"),
                ("MINERU_VLM_HTTP_CONCURRENCY", "24"),
                ("MINERU_VLM_HTTP_TIMEOUT", "25"),
                ("MINERU_VLM_CONNECT_TIMEOUT", "26"),
                ("MINERU_VLM_HTTP_MAX_KEEPALIVE_CONNECTIONS", "27"),
                ("MINERU_VLM_HTTP_KEEPALIVE_EXPIRY", "28"),
                ("MINERU_VLM_HTTP_MAX_RETRIES", "29"),
                ("MINERU_VLM_HTTP_RETRY_BACKOFF_FACTOR", "0.5"),
                ("MINERU_VLM_MAX_IMAGE_BYTES", "30"),
                ("MINERU_VLM_MAX_DECODED_PIXELS", "31"),
                ("MINERU_VLM_MAX_IMAGES_PER_REQUEST", "32"),
                ("MINERU_VLM_MAX_REDIRECTS", "33"),
                ("MINERU_VLM_HTTP_MAX_RESPONSE_BYTES", "34"),
            ],
            &args(&[]),
        )
        .unwrap();
        assert_eq!(env.route.max_pdf_bytes, 1000);
        assert_eq!(env.route.max_pages, 11);
        assert_eq!(env.route.max_page_pixels, 12);
        assert_eq!(env.route.max_rendered_image_bytes, 13);
        assert_eq!(env.route.max_in_flight_image_bytes, 14);
        assert_eq!(env.route.max_raw_output_bytes, 15);
        assert_eq!(env.route.max_layout_blocks_per_page, 16);
        assert_eq!(env.route.max_semantic_requests_per_page, 17);
        assert_eq!(env.route.max_requests_per_batch, 18);
        assert_eq!(env.route.max_encoded_request_bytes, 19);
        assert_eq!(env.route.max_encoded_batch_bytes, 20);
        assert_eq!(env.route.max_total_asset_bytes, 21);
        assert_eq!(env.route.max_staged_text_bytes, 22);
        assert_eq!(env.route.total_deadline, Duration::from_secs(23));
        assert_eq!(env.http.max_concurrency, 24);
        assert_eq!(env.http.http_timeout, Duration::from_secs(25));
        assert_eq!(env.http.connect_timeout, Duration::from_secs(26));
        assert_eq!(env.http.max_keepalive_connections, 27);
        assert_eq!(env.http.keepalive_expiry, Duration::from_secs(28));
        assert_eq!(env.http.max_retries, 29);
        assert_eq!(env.http.retry_backoff_factor, 0.5);
        assert_eq!(env.http.max_image_bytes, 30);
        assert_eq!(env.http.max_decoded_pixels, 31);
        assert_eq!(env.http.max_images_per_request, 32);
        assert_eq!(env.http.max_redirects, 33);
        assert_eq!(env.http.max_response_bytes, 34);
    }

    #[test]
    fn malformed_startup_values_fail_strictly() {
        for (name, value) in [
            ("MINERU_API_MAX_CONCURRENT_REQUESTS", "bad"),
            ("MINERU_PDF_RENDER_TIMEOUT", "1e3"),
            ("MINERU_API_RECORD_CAP", "0"),
            ("MINERU_OFFICE_WALL_SECONDS", "0"),
            ("MINERU_OOXML_XML_DEPTH", "0"),
            ("MINERU_FORMULA_ENABLE", "yes"),
        ] {
            let result = configured(&[(name, value)], &args(&[]));
            assert!(result.is_err(), "{name}={value}");
        }
    }

    #[test]
    fn formula_table_pins_are_symmetric_for_true_and_false() {
        // `false` must pin just like `true`: the operator-resolved value wins over the
        // per-request form fields. Malformed env values fail before work.
        let env = configured(
            &[
                ("MINERU_FORMULA_ENABLE", "false"),
                ("MINERU_TABLE_ENABLE", "FALSE"),
                ("MINERU_IMAGE_ANALYSIS_ENABLE", "true"),
            ],
            &args(&[]),
        )
        .unwrap();
        assert_eq!(env.formula, Some(false));
        assert_eq!(env.table, Some(false));
        assert_eq!(env.image_analysis, Some(true));
        for (name, value) in [
            ("MINERU_FORMULA_ENABLE", "yes"),
            ("MINERU_TABLE_ENABLE", "1"),
            ("MINERU_IMAGE_ANALYSIS_ENABLE", ""),
        ] {
            let result = configured(&[(name, value)], &args(&[]));
            assert!(result.is_err(), "{name}={value}");
        }
    }

    #[test]
    fn vlm_transport_boolean_flags_override_the_frozen_environment() {
        let cli = args(&[
            "--vlm-debug=false",
            "--vlm-text-before-image=false",
            "--vlm-allow-truncated-content=false",
            "--vlm-allow-remote-images=false",
            "--vlm-allow-private-remote-images=false",
        ]);
        let env = configured(
            &[
                ("MINERU_VL_DEBUG_ENABLE", "true"),
                ("MINERU_VLM_TEXT_BEFORE_IMAGE", "true"),
                ("MINERU_VLM_ALLOW_TRUNCATED_CONTENT", "true"),
                ("MINERU_VLM_ALLOW_REMOTE_IMAGES", "true"),
                ("MINERU_VLM_ALLOW_PRIVATE_REMOTE_IMAGES", "true"),
            ],
            &cli,
        )
        .unwrap();
        assert!(!env.http.debug);
        assert!(!env.service.vlm_text_before_image);
        assert!(!env.service.vlm_allow_truncated_content);
        assert!(!env.service.vlm_allow_remote_images);
        assert!(!env.service.vlm_allow_private_remote_images);

        // Omitted flags fall back to the frozen environment spelling (strict true/false).
        let env = configured(
            &[
                ("MINERU_VL_DEBUG_ENABLE", "true"),
                ("MINERU_VLM_TEXT_BEFORE_IMAGE", "false"),
                ("MINERU_VLM_ALLOW_REMOTE_IMAGES", "false"),
            ],
            &args(&[]),
        )
        .unwrap();
        assert!(env.http.debug);
        assert!(!env.service.vlm_text_before_image);
        assert!(!env.service.vlm_allow_remote_images);

        // Strict boolean flags: a bare flag or a malformed value is a CLI parse error.
        assert!(Args::try_parse_from(["mineru-vlm-api", "--vlm-debug"]).is_err());
        assert!(
            Args::try_parse_from(["mineru-vlm-api", "--vlm-allow-remote-images", "1"]).is_err()
        );
        // Malformed environment values fail before work.
        assert!(configured(&[("MINERU_VLM_TEXT_BEFORE_IMAGE", "yes")], &args(&[])).is_err());
    }

    #[test]
    fn vlm_transport_boolean_flags_appear_in_help() {
        let help = <Args as clap::CommandFactory>::command()
            .render_help()
            .to_string();
        for flag in [
            "--vlm-debug",
            "--vlm-text-before-image",
            "--vlm-allow-truncated-content",
            "--vlm-allow-remote-images",
            "--vlm-allow-private-remote-images",
        ] {
            assert!(help.contains(flag), "{flag}: {help}");
        }
    }

    #[test]
    fn client_only_env_names_are_never_read() {
        // Task result/download timing and archive caps are client-only; the service ignores them
        // even when malformed.
        let env = configured(
            &[
                ("MINERU_TASK_RESULT_TIMEOUT_SECONDS", "not-a-number"),
                ("MINERU_TASK_RESULT_DOWNLOAD_TIMEOUT_SECONDS", "0"),
                ("MINERU_ARCHIVE_MAX_ENTRIES", "0"),
                ("MINERU_API_CONNECT_TIMEOUT_SECONDS", "bad"),
            ],
            &args(&[]),
        )
        .unwrap();
        assert_eq!(env.service.remote_concurrency, 3);
    }

    #[test]
    fn startup_snapshot_carries_vlm_transport_identity_into_the_frozen_http_config() {
        // The worker's base VlmHttpConfig is resolved once at startup from the frozen snapshot,
        // credentials included; it is never re-read from the ambient environment per task.
        let env = configured(
            &[
                ("MINERU_VL_SERVER", "http://vlm.internal:9000/"),
                ("MINERU_VL_MODEL_NAME", "frozen-model"),
                ("MINERU_VL_API_KEY", "frozen-key"),
                ("MINERU_VLM_END_TOKEN", "frozen-end"),
            ],
            &args(&[]),
        )
        .unwrap();
        assert_eq!(
            env.http.server_url.as_ref().map(|u| u.as_str()),
            Some("http://vlm.internal:9000/")
        );
        assert_eq!(env.http.model_name.as_deref(), Some("frozen-model"));
        assert_eq!(env.http.api_key.as_deref(), Some("frozen-key"));
        assert_eq!(env.http.end_token, "frozen-end");
    }

    #[test]
    fn public_bind_flags_are_strict_and_override_environment() {
        // Explicit CLI values override the frozen environment, matching the default -> env ->
        // explicit CLI precedence of every other server knob.
        let cli = args(&[
            "--public-bind-exposed=false",
            "--allow-public-http-client=false",
        ]);
        let env = configured(
            &[
                ("MINERU_API_PUBLIC_BIND_EXPOSED", "true"),
                ("MINERU_API_ALLOW_PUBLIC_HTTP_CLIENT", "on"),
            ],
            &cli,
        )
        .unwrap();
        assert!(!env.public_bind_exposed && !env.allow_public_http_client);

        let cli = args(&[
            "--public-bind-exposed",
            "true",
            "--allow-public-http-client",
            "true",
        ]);
        let env = configured(
            &[
                ("MINERU_API_PUBLIC_BIND_EXPOSED", "false"),
                ("MINERU_API_ALLOW_PUBLIC_HTTP_CLIENT", "false"),
            ],
            &cli,
        )
        .unwrap();
        assert!(env.public_bind_exposed && env.allow_public_http_client);

        // Omitted flags fall back to the frozen environment spelling.
        let env = configured(
            &[
                ("MINERU_API_PUBLIC_BIND_EXPOSED", "YES"),
                ("MINERU_API_ALLOW_PUBLIC_HTTP_CLIENT", "1"),
            ],
            &args(&[]),
        )
        .unwrap();
        assert!(env.public_bind_exposed && env.allow_public_http_client);

        // Strict boolean parsing: a bare flag or a malformed value is a CLI parse error.
        assert!(Args::try_parse_from(["mineru-vlm-api", "--public-bind-exposed"]).is_err());
        assert!(Args::try_parse_from(["mineru-vlm-api", "--public-bind-exposed", "1"]).is_err());
    }

    #[test]
    fn public_bind_flags_appear_in_help() {
        let help = <Args as clap::CommandFactory>::command()
            .render_help()
            .to_string();
        assert!(help.contains("--public-bind-exposed"), "{help}");
        assert!(help.contains("--allow-public-http-client"), "{help}");
    }
}
