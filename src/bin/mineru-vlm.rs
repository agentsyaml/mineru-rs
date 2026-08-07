use clap::Parser;
use mineru::{
    BearerToken, ClientConfig, MinerUClient, PageRange, ParseOptions, PdfInput, write_outputs,
};
use std::{io::Read, path::PathBuf};

#[derive(Debug, Parser)]
#[command(about = "Parse a PDF with a MinerU VLM service")]
struct Cli {
    input: PathBuf,
    #[arg(long)]
    base_url: Option<String>,
    #[arg(long)]
    model: Option<String>,
    #[arg(long, default_value = "output")]
    output: PathBuf,
    #[arg(long)]
    api_key: Option<String>,
    #[arg(long)]
    page_start: Option<u32>,
    #[arg(long)]
    page_end: Option<u32>,
    #[arg(long, action = clap::ArgAction::SetTrue)]
    no_formula: Option<bool>,
    #[arg(long, action = clap::ArgAction::SetTrue)]
    no_table: Option<bool>,
    #[arg(long, action = clap::ArgAction::SetTrue)]
    no_image_analysis: Option<bool>,
    /// Rust-only: use the bounded transactional direct route and official-shaped output.
    #[arg(long)]
    official_output: bool,
    /// Semantic inference request admission per page for --official-output (default 32).
    /// This is real inference batching, distinct from document grouping and the page window.
    #[arg(long, requires = "official_output")]
    batch_size: Option<usize>,
    #[arg(long)]
    max_input_bytes: Option<String>,
    #[arg(long, requires = "official_output")]
    max_encoded_document_bytes: Option<String>,
    #[arg(long)]
    max_output_bytes: Option<String>,
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    if let Err(error) = run(cli).await {
        eprintln!("mineru-vlm: {error}");
        std::process::exit(1);
    }
}

async fn run(cli: Cli) -> Result<(), Box<dyn std::error::Error>> {
    if !cli.official_output {
        // Route/service/server-owned frozen-environment knobs cannot act in ordinary mode;
        // reject them before any network or output work (mirrors canonical direct mode).
        if let Some(message) =
            mineru::command::legacy_ordinary_mode_error(|name| std::env::var_os(name))
        {
            return Err(message.into());
        }
        if std::env::var_os("MINERU_MAX_ENCODED_DOCUMENT_BYTES").is_some() {
            return Err(
                "MINERU_MAX_ENCODED_DOCUMENT_BYTES applies only to --official-output; configure the server for ordinary mode"
                    .into(),
            );
        }
    }
    let document_limits = mineru::DocumentLimitPolicy::resolve(
        &mineru::DocumentLimitOverrides {
            max_input_bytes: cli.max_input_bytes.clone(),
            max_encoded_document_bytes: cli.max_encoded_document_bytes.clone(),
            max_output_bytes: cli.max_output_bytes.clone(),
        },
        std::env::var_os,
    )?;
    if cli.official_output {
        let options = mineru::command::LegacyDirectOptions {
            input: cli.input,
            output: cli.output,
            base_url: cli.base_url,
            model: cli.model,
            api_key: cli.api_key,
            page_start: cli.page_start.map(|page| page as usize),
            page_end: cli.page_end.map(|page| page as usize),
            no_formula: cli.no_formula,
            no_table: cli.no_table,
            no_image_analysis: cli.no_image_analysis,
            // Absent means the official route's compiled default (32); an explicit value is the
            // real per-page semantic inference admission.
            batch_size: cli.batch_size.unwrap_or(32),
            document_limits,
        };
        return mineru::command::run_legacy_direct(options)
            .await
            .map_err(Into::into);
    }
    let base_url = cli.base_url.ok_or("--base-url is required")?;
    let model = cli.model.ok_or("--model is required")?;
    let mut config = ClientConfig::from_env(&base_url, &model)?;
    let input = read_input_exact(
        &cli.input,
        document_limits.max_input_bytes,
        config.limits.max_pdf_bytes,
    )?;
    let max_output = usize::try_from(document_limits.max_output_bytes).unwrap_or(usize::MAX);
    if config.limits.max_total_asset_bytes > max_output {
        if std::env::var_os("MINERU_MAX_TOTAL_ASSET_BYTES").is_some() {
            return Err(format!(
                "MINERU_MAX_TOTAL_ASSET_BYTES={} exceeds the derived document budget MINERU_MAX_OUTPUT_BYTES={}; raise the output budget or lower MINERU_MAX_TOTAL_ASSET_BYTES",
                config.limits.max_total_asset_bytes, document_limits.max_output_bytes
            )
            .into());
        }
        config.limits.max_total_asset_bytes = max_output;
    }
    config.bearer_token = cli
        .api_key
        .or_else(|| std::env::var("MINERU_VL_API_KEY").ok())
        .map(BearerToken::new)
        .transpose()?;
    let page_range = match (cli.page_start, cli.page_end) {
        (None, None) => None,
        (start, end) => Some(PageRange::new(start.unwrap_or(0), end)?),
    };
    let document = MinerUClient::new(config)?
        .parse_pdf(
            PdfInput::Bytes(input),
            ParseOptions {
                page_range,
                formula: !cli.no_formula.unwrap_or(false),
                table: !cli.no_table.unwrap_or(false),
                image_analysis: !cli.no_image_analysis.unwrap_or(false),
                max_new_tokens: None,
                allow_truncated: true,
            },
        )
        .await?;
    for warning in &document.warnings {
        eprintln!("warning: {warning}");
    }
    let output = write_outputs(&document, cli.output)?;
    eprintln!(
        "wrote {} and {}",
        output.markdown.display(),
        output.document_json.display()
    );
    Ok(())
}

fn read_input_exact(
    path: &std::path::Path,
    max_input: u64,
    resident_cap: usize,
) -> Result<bytes::Bytes, Box<dyn std::error::Error>> {
    let file = std::fs::File::open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err("input is not a regular file".into());
    }
    let length = metadata.len();
    if length > max_input {
        return Err(format!("input exceeds configured limit of {max_input} bytes").into());
    }
    if length > resident_cap as u64 {
        return Err("input exceeds resident parser limit".into());
    }
    read_open_file_exact(file, length, max_input, resident_cap)
}

fn read_open_file_exact(
    mut file: std::fs::File,
    length: u64,
    max_input: u64,
    resident_cap: usize,
) -> Result<bytes::Bytes, Box<dyn std::error::Error>> {
    if length > max_input {
        return Err(format!("input exceeds configured limit of {max_input} bytes").into());
    }
    if length > resident_cap as u64 {
        return Err("input exceeds resident parser limit".into());
    }
    let capacity = usize::try_from(length)?;
    let mut bytes = Vec::with_capacity(capacity);
    (&mut file).take(length).read_to_end(&mut bytes)?;
    if bytes.len() != capacity {
        return Err("input shrank during read".into());
    }
    let mut probe = [0; 1];
    if file.read(&mut probe)? != 0 {
        return Err("input grew during read".into());
    }
    Ok(bytes.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    #[test]
    fn exact_open_file_rejects_growth_and_shrink() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("input.pdf");
        std::fs::write(&path, b"abc").unwrap();
        let file = std::fs::File::open(&path).unwrap();
        assert_eq!(
            read_open_file_exact(file, 3, 3, 3).unwrap().as_ref(),
            b"abc"
        );

        let file = std::fs::File::open(&path).unwrap();
        std::fs::write(&path, b"abcd").unwrap();
        assert!(
            read_open_file_exact(file, 3, 4, 4)
                .unwrap_err()
                .to_string()
                .contains("grew")
        );

        let file = std::fs::File::open(&path).unwrap();
        std::fs::write(&path, b"a").unwrap();
        assert!(
            read_open_file_exact(file, 4, 4, 4)
                .unwrap_err()
                .to_string()
                .contains("shrank")
        );
    }
    #[test]
    fn controls_are_accepted_and_encoded_requires_official_output() {
        assert!(
            Cli::try_parse_from([
                "mineru-vlm",
                "a.pdf",
                "--max-input-bytes",
                "1",
                "--max-output-bytes",
                "2"
            ])
            .is_ok()
        );
        let error =
            Cli::try_parse_from(["mineru-vlm", "a.pdf", "--max-encoded-document-bytes", "1"])
                .unwrap_err();
        assert!(error.to_string().contains("--official-output"), "{error}");
        assert!(
            Cli::try_parse_from([
                "mineru-vlm",
                "a.pdf",
                "--official-output",
                "--max-encoded-document-bytes",
                "1"
            ])
            .is_ok()
        );
    }

    #[test]
    fn ordinary_mode_rejects_encoded_environment_in_a_scrubbed_process() {
        for value in ["8", "malformed"] {
            let output = Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "tests::ordinary_mode_encoded_environment_child",
                    "--nocapture",
                ])
                .env("MINERU_VLM_ENCODED_CHILD", "1")
                .env_remove("MINERU_MAX_INPUT_BYTES")
                .env_remove("MINERU_MAX_OUTPUT_BYTES")
                .env("MINERU_MAX_ENCODED_DOCUMENT_BYTES", value)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }

    #[tokio::test]
    async fn ordinary_mode_encoded_environment_child() {
        if std::env::var_os("MINERU_VLM_ENCODED_CHILD").is_none() {
            return;
        }
        let cli = Cli::try_parse_from([
            "mineru-vlm",
            "a.pdf",
            "--base-url",
            "http://127.0.0.1:1",
            "--model",
            "mock",
        ])
        .unwrap();
        assert!(
            run(cli)
                .await
                .unwrap_err()
                .to_string()
                .contains("only to --official-output")
        );
    }

    #[test]
    fn ordinary_mode_rejects_inert_route_and_service_env_before_network() {
        for (name, value) in [
            ("MINERU_OFFICIAL_PAGE_CONCURRENCY", "8"),
            ("MINERU_API_RECORD_CAP", "40"),
            ("MINERU_ARCHIVE_MAX_ENTRIES", "5"),
        ] {
            let output = Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "tests::ordinary_mode_inert_env_child",
                    "--nocapture",
                ])
                .env("MINERU_VLM_INERT_ENV_CHILD", "1")
                .env("MINERU_VLM_INERT_ENV_NAME", name)
                .env_remove("MINERU_MAX_INPUT_BYTES")
                .env_remove("MINERU_MAX_OUTPUT_BYTES")
                .env_remove("MINERU_MAX_ENCODED_DOCUMENT_BYTES")
                .env(name, value)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }

    #[tokio::test]
    async fn ordinary_mode_inert_env_child() {
        if std::env::var_os("MINERU_VLM_INERT_ENV_CHILD").is_none() {
            return;
        }
        let name = std::env::var("MINERU_VLM_INERT_ENV_NAME").unwrap();
        let cli = Cli::try_parse_from([
            "mineru-vlm",
            "a.pdf",
            "--base-url",
            "http://127.0.0.1:1",
            "--model",
            "mock",
        ])
        .unwrap();
        // The guard rejects the inert env before any network or output work, naming it clearly.
        let error = run(cli).await.unwrap_err().to_string();
        assert!(error.contains(&name), "{name}: {error}");
    }
}
