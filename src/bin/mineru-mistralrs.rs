use clap::Parser;
use mineru::{
    MinerUVlmClient, MinerUVlmConfig, MistralRsConfig, OfficialPdfOptions, PdfInput, canonical_stem,
};
use std::path::{Path, PathBuf};

#[derive(Debug, Parser)]
#[command(about = "Parse a PDF with the local MinerU mistral.rs backend")]
struct Cli {
    /// Input PDF.
    input: PathBuf,
    /// Local MinerU model directory; takes priority over download.
    #[arg(long)]
    model_path: Option<PathBuf>,
    /// Allow downloading the model from Hugging Face when --model-path is absent.
    #[arg(long, default_value_t = true, default_missing_value = "true", num_args = 0..=1, require_equals = true)]
    allow_download: bool,
    #[arg(long, default_value = "output")]
    output: PathBuf,
    #[arg(long)]
    page_start: Option<usize>,
    #[arg(long)]
    page_end: Option<usize>,
    #[arg(long)]
    no_formula: bool,
    #[arg(long)]
    no_table: bool,
    #[arg(long)]
    no_image_analysis: bool,
}

#[tokio::main]
async fn main() {
    if let Err(error) = run(Cli::parse()).await {
        eprintln!("mineru-mistralrs: {error}");
        std::process::exit(1);
    }
}

async fn run(cli: Cli) -> Result<(), Box<dyn std::error::Error>> {
    validate_input(&cli.input)?;
    let model = MistralRsConfig::from_parts(cli.model_path, cli.allow_download)?;
    let options = OfficialPdfOptions {
        start_page: cli.page_start.unwrap_or(0),
        end_page: cli.page_end,
        formula_enable: !cli.no_formula,
        table_enable: !cli.no_table,
        image_analysis: !cli.no_image_analysis,
        max_requests_per_batch: 1,
        ..OfficialPdfOptions::default()
    };
    options.validate()?;
    let stem = canonical_stem(
        cli.input
            .file_stem()
            .and_then(|stem| stem.to_str())
            .ok_or("input filename must be valid Unicode")?,
    )?;
    let output = MinerUVlmClient::connect_mistralrs(model, MinerUVlmConfig::default())
        .await?
        .parse_and_write_official_pdf(PdfInput::Path(cli.input), options, &cli.output, &stem)
        .await?;
    eprintln!("wrote {}", output.vlm_dir.display());
    Ok(())
}

/// Fail before any model configuration/download work when the input PDF is
/// unusable: it must exist, be a regular file, and be readable.
fn validate_input(path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let metadata = std::fs::metadata(path)
        .map_err(|error| format!("cannot access input {}: {error}", path.display()))?;
    if !metadata.is_file() {
        return Err(format!("input {} is not a regular file", path.display()).into());
    }
    // metadata() succeeds even for unreadable regular files (e.g. mode 0o000 on
    // Unix); opening probes actual readability.
    std::fs::File::open(path)
        .map_err(|error| format!("cannot read input {}: {error}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn missing_input_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("missing.pdf");
        let error = validate_input(&missing).unwrap_err().to_string();
        assert!(error.contains("missing.pdf"), "{error}");
    }

    #[test]
    fn directory_input_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let error = validate_input(dir.path()).unwrap_err().to_string();
        assert!(error.contains("not a regular file"), "{error}");
    }

    #[test]
    fn regular_file_input_is_accepted() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("ok.pdf");
        fs::write(&file, b"%PDF").unwrap();
        validate_input(&file).unwrap();
    }
}
