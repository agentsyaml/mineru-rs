use clap::{ArgAction, Parser};
use mineru::{
    OfficialPdfOptions, ProgressCallback, ProgressEvent, RemoteApiDocument, RemoteApiOptions,
    normalize_remote_language, parse_remote_api_env, run_remote_api_documents,
    selected_document_pages,
};
use std::{
    io::IsTerminal,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

#[path = "support/event_sink.rs"]
mod event_sink;
mod support;

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

fn err(s: impl Into<String>) -> Box<dyn std::error::Error> {
    s.into().into()
}

fn behaviorless_warning(cli: &Cli) -> Option<String> {
    let mut selected = Vec::new();
    if cli.method != "auto" {
        selected.push(format!("method={}", cli.method));
    }
    if cli.effort != "medium" {
        selected.push(format!("effort={}", cli.effort));
    }
    if cli.lang != "ch" {
        selected.push(format!("lang={}", cli.lang));
    }
    if cli.client_side_output_generation {
        selected.push("client-side-output-generation=true".into());
    }
    (!selected.is_empty()).then(|| selected.join(", "))
}

fn has_pdf_input(path: &std::path::Path) -> bool {
    let is_pdf = |path: &std::path::Path| {
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

async fn run(cli: Cli, level: event_sink::LogLevel) -> Result<(), Box<dyn std::error::Error>> {
    let language = normalize_remote_language(&cli.lang).map_err(err)?;
    let stderr = std::io::stderr();
    let sink = Arc::new(event_sink::EventSink::new(
        stderr,
        std::io::stderr().is_terminal(),
        level,
    ));
    if cli.api_url.is_none()
        && cli.end.is_some_and(|end| end < cli.start)
        && has_pdf_input(&cli.path)
    {
        sink.fail("--end must not be less than --start");
        sink.finish();
        return Err(err("--end must not be less than --start"));
    }
    if cli.api_url.is_none()
        && let Some(message) = behaviorless_warning(&cli)
    {
        sink.warning("ignored direct options", &message);
    }
    let typed_failure = Arc::new(AtomicBool::new(false));
    let events: ProgressCallback = {
        let sink = Arc::clone(&sink);
        let typed_failure = Arc::clone(&typed_failure);
        Arc::new(move |event| {
            if matches!(
                event,
                ProgressEvent::DocumentFailed { .. } | ProgressEvent::ApiFailed { .. }
            ) {
                typed_failure.store(true, Ordering::Relaxed);
            }
            sink.event(event);
        })
    };
    let result = if let Some(api_url) = cli.api_url.clone() {
        run_api(&cli, api_url, language, events.clone(), &sink).await
    } else {
        support::direct_vlm::run_with_events(
            support::direct_vlm::DirectOptions {
                input: cli.path,
                output: cli.output,
                base_url: cli.url,
                server_option_label: "--url",
                model: None,
                api_key: None,
                page_start: Some(cli.start),
                page_end: cli.end,
                no_formula: !cli.formula,
                no_table: !cli.table,
                no_image_analysis: !cli.image_analysis,
                batch_size: 1,
                canonical_mixed: true,
            },
            Some(events),
            Some(sink.warning_callback()),
        )
        .await
    };
    if let Err(error) = &result {
        if !typed_failure.load(Ordering::Relaxed) {
            sink.fail(&error.to_string());
        }
    }
    sink.finish();
    result
}

async fn run_api(
    cli: &Cli,
    api_url: String,
    language: String,
    events: ProgressCallback,
    sink: &Arc<event_sink::EventSink<std::io::Stderr>>,
) -> Result<(), Box<dyn std::error::Error>> {
    if cli.client_side_output_generation {
        return Err(err("client-side output generation is unsupported"));
    }
    let env = parse_remote_api_env(|name| std::env::var(name).ok()).map_err(err)?;
    let mut route = OfficialPdfOptions::default();
    route.start_page = cli.start;
    route.end_page = cli.end;
    route.formula_enable = cli.formula;
    route.table_enable = cli.table;
    route.image_analysis = cli.image_analysis;
    if support::official_env::apply_route_env(&mut route, |name| std::env::var_os(name)) {
        sink.warning("MINERU_PROCESSING_WINDOW_SIZE", "invalid value; using 64");
    }
    let start = u64::try_from(cli.start).map_err(|_| err("page start exceeds u64"))?;
    let end = cli
        .end
        .map(|value| u64::try_from(value).map_err(|_| err("page end exceeds u64")))
        .transpose()?;
    let (_, inputs, skipped) = support::direct_vlm::discover_inputs(&cli.path)?;
    let stems = support::direct_vlm::allocate_input_stems(&inputs)?;
    for path in skipped {
        sink.warning("unsupported input", &path.display().to_string());
    }
    let documents = inputs
        .into_iter()
        .zip(stems)
        .enumerate()
        .map(|(order, ((path, kind), stem))| {
            let effective_pages = selected_document_pages(&path, kind, start, end).map_err(err)?;
            Ok(RemoteApiDocument {
                path,
                kind,
                stem,
                effective_pages,
                order,
            })
        })
        .collect::<Result<Vec<_>, Box<dyn std::error::Error>>>()?;
    let options = RemoteApiOptions {
        backend: cli.backend.clone(),
        method: cli.method.clone(),
        effort: cli.effort.clone(),
        language,
        server_url: cli.url.clone(),
        start,
        end,
        formula: route.formula_enable,
        table: route.table_enable,
        image_analysis: route.image_analysis,
        client_side_output_generation: false,
        route,
    };
    let failures = run_remote_api_documents(
        documents,
        cli.output.clone(),
        api_url,
        options,
        env,
        Some(events),
    )
    .await
    .map_err(err)?;
    if failures.is_empty() {
        Ok(())
    } else {
        Err(err(format!("{} API task(s) failed", failures.len())))
    }
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    let level = match event_sink::LogLevel::from_env() {
        Ok(level) => level,
        Err(error) => {
            eprintln!("{error}");
            std::process::exit(1);
        }
    };
    if run(cli, level).await.is_err() {
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parser_maps_canonical_surface() {
        let cli = Cli::try_parse_from([
            "mineru",
            "-p",
            "a.pdf",
            "-o",
            "out",
            "-s",
            "2",
            "-e",
            "4",
            "-f",
            "false",
            "-t",
            "false",
            "--image-analysis",
            "false",
        ])
        .unwrap();
        assert_eq!((cli.start, cli.end), (2, Some(4)));
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
        let api = Cli::try_parse_from(["mineru", "-p", "a", "-o", "b", "--api-url", "http://api"])
            .unwrap();
        assert_eq!(api.api_url.as_deref(), Some("http://api"));
        let cli = Cli::try_parse_from([
            "mineru",
            "-p",
            "a",
            "-o",
            "b",
            "--formula",
            "false",
            "--table",
            "true",
            "--image-analysis",
            "false",
            "--client-side-output-generation",
            "true",
        ])
        .unwrap();
        assert!(
            !cli.formula && cli.table && !cli.image_analysis && cli.client_side_output_generation
        );
        assert!(Cli::try_parse_from(["mineru", "-p", "a", "-o", "b", "--formula"]).is_err());
    }
}
