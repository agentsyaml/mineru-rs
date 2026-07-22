use clap::Parser;
use mineru::{
    BearerToken, ClientConfig, MinerUClient, PageRange, ParseOptions, PdfInput, write_outputs,
};
use std::path::PathBuf;

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
    #[arg(long)]
    no_formula: bool,
    #[arg(long)]
    no_table: bool,
    #[arg(long)]
    no_image_analysis: bool,
    /// Rust-only: use the bounded transactional direct route and official-shaped output.
    #[arg(long)]
    official_output: bool,
    /// Rust-only document grouping for --official-output; not MinerU's page window.
    #[arg(long, requires = "official_output", default_value_t = 1)]
    batch_size: usize,
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
            batch_size: cli.batch_size,
        };
        return mineru::command::run_legacy_direct(options)
            .await
            .map_err(Into::into);
    }
    let base_url = cli.base_url.ok_or("--base-url is required")?;
    let model = cli.model.ok_or("--model is required")?;
    let mut config = ClientConfig::new(&base_url, &model)?;
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
            PdfInput::Path(cli.input),
            ParseOptions {
                page_range,
                formula: !cli.no_formula,
                table: !cli.no_table,
                image_analysis: !cli.no_image_analysis,
                max_new_tokens: None,
                allow_truncated: true,
            },
        )
        .await?;
    let output = write_outputs(&document, cli.output)?;
    eprintln!(
        "wrote {} and {}",
        output.markdown.display(),
        output.document_json.display()
    );
    Ok(())
}
