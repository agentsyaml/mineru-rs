use crate::official::{
    OfficialInput, OfficialOcrMode, OfficialParseOptions, OfficialParser, OfficialRenderFormat,
    OfficialRenderOptions, OfficialRendered, OfficialResult, OfficialRuntime, OfficialTier,
    OfficialWorkerMode,
};
use bytes::Bytes;
use clap::{Parser, Subcommand};
#[path = "official_cli_publish.rs"]
mod official_cli_publish;
use official_cli_publish::publish_rendered;
use serde_json::Value;
use std::{
    collections::BTreeMap,
    fs::OpenOptions,
    io::Read,
    path::{Path, PathBuf},
    time::Duration,
};

#[derive(Parser)]
#[command(name = "mineru official", disable_version_flag = true)]
struct Cli {
    #[command(subcommand)]
    command: Action,
}
#[derive(Subcommand)]
enum Action {
    Parse(Parse),
    Render(Render),
}
#[derive(Parser)]
struct RuntimeArgs {
    #[arg(long)]
    python: Option<PathBuf>,
    #[arg(long)]
    config: Option<PathBuf>,
    #[arg(long)]
    model_dir: Option<PathBuf>,
    #[arg(long, value_parser=["per-document","persistent"], default_value="per-document")]
    mode: String,
    #[arg(long, default_value_t = 86400)]
    deadline: u64,
}
#[derive(Parser)]
struct Parse {
    input: PathBuf,
    #[arg(short, long)]
    output: PathBuf,
    #[arg(long, value_parser=["flash","basic","standard","advanced"],default_value="standard")]
    tier: String,
    #[arg(long,value_parser=["auto","txt","ocr"],default_value="auto")]
    ocr: String,
    #[arg(long,action=clap::ArgAction::Set,default_value_t=true)]
    image_analysis: bool,
    #[arg(long)]
    page_range: Option<String>,
    #[arg(long)]
    source_context: Option<String>,
    #[arg(long)]
    vlm_config: Option<String>,
    #[command(flatten)]
    runtime: RuntimeArgs,
}
#[derive(Parser)]
struct Render {
    middle_json: PathBuf,
    #[arg(long)]
    assets: Option<PathBuf>,
    #[arg(long)]
    output: PathBuf,
    #[arg(long,value_parser=["markdown","html","latex","docx","epub","structured_content","content_list","content_list_v2","pdf"])]
    target: String,
    #[arg(long)]
    options: Option<String>,
    #[command(flatten)]
    runtime: RuntimeArgs,
}
fn runtime(args: &RuntimeArgs) -> Result<OfficialRuntime, String> {
    if args.deadline == 0 {
        return Err("--deadline must be positive".into());
    }
    let mut r = OfficialRuntime::new()
        .with_worker_mode(match args.mode.as_str() {
            "per-document" => OfficialWorkerMode::PerDocument,
            "persistent" => OfficialWorkerMode::Persistent,
            _ => return Err("invalid --mode".into()),
        })
        .with_timeout(Duration::from_secs(args.deadline));
    if let Some(p) = &args.python {
        r = r.with_python(p.clone())
    }
    if let Some(p) = &args.config {
        r = r.with_config(p.clone())
    }
    if let Some(p) = &args.model_dir {
        r = r.with_model_dir(p.clone())
    }
    Ok(r)
}
pub(super) async fn run(argv: Vec<std::ffi::OsString>) -> i32 {
    let args = std::iter::once(std::ffi::OsString::from("mineru")).chain(argv.into_iter().skip(1));
    let cli = match Cli::try_parse_from(args) {
        Ok(v) => v,
        Err(e) => {
            let code = if e.use_stderr() { 2 } else { 0 };
            return e.print().map(|_| code).unwrap_or(2);
        }
    };
    let result = match cli.command {
        Action::Parse(c) => parse(c).await,
        Action::Render(c) => render(c).await,
    };
    if let Err(e) = result {
        eprintln!("error: {e}");
        1
    } else {
        0
    }
}
async fn parse(c: Parse) -> Result<(), String> {
    let r = runtime(&c.runtime)?;
    let options = parse_options(&c)?;
    let parser = OfficialParser::new(r).map_err(|e| e.to_string())?;
    let stem = c
        .input
        .file_stem()
        .and_then(|s| s.to_str())
        .ok_or("input needs a valid filename")?;
    parser
        .parse_rich(OfficialInput::path(&c.input), options)
        .await
        .map_err(|e| e.to_string())?
        .save(&c.output, stem)
        .map_err(|e| e.to_string())
}
fn parse_options(c: &Parse) -> Result<OfficialParseOptions, String> {
    let mut options = OfficialParseOptions::new()
        .with_tier(match c.tier.as_str() {
            "flash" => OfficialTier::Flash,
            "basic" => OfficialTier::Basic,
            "standard" => OfficialTier::Standard,
            _ => OfficialTier::Advanced,
        })
        .with_ocr_mode(match c.ocr.as_str() {
            "auto" => OfficialOcrMode::Auto,
            "txt" => OfficialOcrMode::Txt,
            _ => OfficialOcrMode::Ocr,
        })
        .with_image_analysis(c.image_analysis);
    if let Some(v) = &c.page_range {
        options = options.with_page_range(v.clone())
    }
    if let Some(v) = &c.source_context {
        options = options.with_source_context(
            serde_json::from_str(v).map_err(|e| format!("invalid source context JSON: {e}"))?,
        )
    }
    if let Some(v) = &c.vlm_config {
        options = options.with_vlm_config(
            serde_json::from_str(v).map_err(|e| format!("invalid VLM config JSON: {e}"))?,
        )
    }
    let suffix = c
        .input
        .extension()
        .and_then(|v| v.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    options.validate(&suffix).map_err(|e| e.to_string())?;
    Ok(options)
}
async fn render(c: Render) -> Result<(), String> {
    let r = runtime(&c.runtime)?;
    let middle: Value =
        serde_json::from_slice(&read_bounded_file(&c.middle_json, 64 * 1024 * 1024)?)
            .map_err(|e| e.to_string())?;
    let assets = match c.assets {
        Some(root) => load_assets(&root)?,
        None => BTreeMap::new(),
    };
    let opts = match c.options {
        Some(v) => render_options(&v)?,
        None => OfficialRenderOptions::new(),
    };
    let format = match c.target.as_str() {
        "markdown" => OfficialRenderFormat::Markdown,
        "html" => OfficialRenderFormat::Html,
        "latex" => OfficialRenderFormat::Latex,
        "docx" => OfficialRenderFormat::Docx,
        "epub" => OfficialRenderFormat::Epub,
        "structured_content" => OfficialRenderFormat::StructuredContent,
        "content_list" => OfficialRenderFormat::ContentList,
        "content_list_v2" => OfficialRenderFormat::ContentListV2,
        _ => OfficialRenderFormat::Pdf,
    };
    opts.validate(format).map_err(|e| e.to_string())?;
    let result = OfficialResult::from_dict(&r, middle, assets)
        .await
        .map_err(|e| e.to_string())?;
    let rendered = result
        .render(&r, format, opts)
        .await
        .map_err(|e| e.to_string())?;
    let (data, sidecars) = match rendered {
        OfficialRendered::Text { content, sidecars } => (content.into_bytes(), sidecars),
        OfficialRendered::Json { value, sidecars } => (
            serde_json::to_vec_pretty(&value).map_err(|e| e.to_string())?,
            sidecars,
        ),
        OfficialRendered::Binary { bytes, sidecars } => (bytes.to_vec(), sidecars),
    };
    publish_rendered(&c.output, &data, sidecars)?;
    Ok(())
}
fn render_options(text: &str) -> Result<OfficialRenderOptions, String> {
    let value: Value =
        serde_json::from_str(text).map_err(|e| format!("invalid options JSON: {e}"))?;
    const KEYS: &[&str] = &[
        "mode",
        "asset_base_url",
        "standalone",
        "document_title",
        "asset_base_path",
        "title",
        "authors",
        "language",
        "identifier",
        "modified_at",
        "layout",
        "latex_delimiters",
    ];
    if value
        .as_object()
        .is_none_or(|o| o.keys().any(|k| !KEYS.contains(&k.as_str())))
    {
        return Err("unknown render option".into());
    }
    serde_json::from_value(value).map_err(|e| format!("invalid options JSON: {e}"))
}
fn read_bounded_file(path: &Path, limit: u64) -> Result<Vec<u8>, String> {
    let meta = std::fs::symlink_metadata(path).map_err(|e| e.to_string())?;
    if !meta.is_file() || meta.file_type().is_symlink() || meta.len() > limit {
        return Err("file is not a bounded regular file".into());
    }
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    let file = options.open(path).map_err(|e| e.to_string())?;
    if !file.metadata().map_err(|e| e.to_string())?.is_file() {
        return Err("file is not a bounded regular file".into());
    }
    let mut bytes = Vec::new();
    file.take(limit.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;
    if bytes.len() as u64 > limit {
        return Err("file is not a bounded regular file".into());
    }
    Ok(bytes)
}
fn load_assets(root: &Path) -> Result<BTreeMap<String, Bytes>, String> {
    fn walk(
        base: &Path,
        dir: &Path,
        out: &mut BTreeMap<String, Bytes>,
        total: &mut u64,
    ) -> Result<(), String> {
        for entry in std::fs::read_dir(dir).map_err(|e| e.to_string())? {
            let e = entry.map_err(|e| e.to_string())?;
            let ty = e.file_type().map_err(|e| e.to_string())?;
            if ty.is_symlink() {
                return Err("asset directory contains a symlink".into());
            }
            if ty.is_dir() {
                walk(base, &e.path(), out, total)?
            } else if ty.is_file() {
                let rel = e
                    .path()
                    .strip_prefix(base)
                    .map_err(|e| e.to_string())?
                    .to_string_lossy()
                    .replace('\\', "/");
                let remaining = (64 * 1024 * 1024u64).saturating_sub(*total);
                let bytes = read_bounded_file(&e.path(), remaining)?;
                *total += bytes.len() as u64;
                out.insert(rel, Bytes::from(bytes));
            } else {
                return Err("unsupported asset entry".into());
            }
        }
        Ok(())
    }
    let meta = std::fs::symlink_metadata(root).map_err(|e| e.to_string())?;
    if !meta.is_dir() || meta.file_type().is_symlink() {
        return Err("assets must be a real directory".into());
    }
    let mut result = BTreeMap::new();
    walk(root, root, &mut result, &mut 0)?;
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn parse_args(extra: &[&str]) -> Parse {
        let args = std::iter::once("mineru")
            .chain(["parse", "input.docx", "-o", "out"])
            .chain(extra.iter().copied());
        match Cli::try_parse_from(args).unwrap().command {
            Action::Parse(value) => value,
            _ => panic!("wrong command"),
        }
    }
    #[test]
    fn pdf_defaults_and_full_page_expression_are_preserved() {
        let args = std::iter::once("mineru").chain([
            "parse",
            "input.pdf",
            "-o",
            "out",
            "--page-range",
            "1,3-5,9",
        ]);
        let defaults = match Cli::try_parse_from(args).unwrap().command {
            Action::Parse(v) => v,
            _ => panic!("wrong command"),
        };
        let options = parse_options(&defaults).unwrap();
        assert_eq!(options.tier, Some(OfficialTier::Standard));
        assert_eq!(options.ocr_mode, Some(OfficialOcrMode::Auto));
        assert_eq!(options.image_analysis, Some(true));
        assert_eq!(options.page_range.as_deref(), Some("1,3-5,9"));
    }
    #[test]
    fn native_document_requires_explicit_flash() {
        let defaults = parse_args(&[]);
        assert!(parse_options(&defaults).is_err());
        let explicit = parse_args(&["--tier", "flash"]);
        let options = parse_options(&explicit).unwrap();
        assert_eq!(options.tier, Some(OfficialTier::Flash));
    }
    #[test]
    fn runtime_flags_are_named_and_resolved() {
        let args = parse_args(&[
            "--python",
            "/py",
            "--config",
            "/cfg",
            "--model-dir",
            "/model",
            "--mode",
            "persistent",
            "--deadline",
            "17",
        ]);
        let runtime = runtime(&args.runtime).unwrap();
        assert_eq!(runtime.python.as_deref(), Some(Path::new("/py")));
        assert_eq!(runtime.config.as_deref(), Some(Path::new("/cfg")));
        assert_eq!(runtime.model_dir.as_deref(), Some(Path::new("/model")));
        assert_eq!(runtime.worker_mode, OfficialWorkerMode::Persistent);
        assert_eq!(runtime.timeout, Duration::from_secs(17));
    }
    #[test]
    fn malformed_json_and_unknown_render_options_fail_before_runtime() {
        let args = parse_args(&["--vlm-config", "{"]);
        assert!(parse_options(&args).is_err());
        assert!(render_options(r#"{"unknown":true}"#).is_err());
        assert!(
            Cli::try_parse_from([
                "mineru",
                "render",
                "middle.json",
                "--output",
                "x",
                "--target",
                "bogus"
            ])
            .is_err()
        );
    }
    #[test]
    fn empty_render_options_are_valid_for_all_targets() {
        let options = OfficialRenderOptions::new();
        for format in [
            OfficialRenderFormat::Markdown,
            OfficialRenderFormat::Html,
            OfficialRenderFormat::Latex,
            OfficialRenderFormat::Docx,
            OfficialRenderFormat::Epub,
            OfficialRenderFormat::StructuredContent,
            OfficialRenderFormat::ContentList,
            OfficialRenderFormat::ContentListV2,
            OfficialRenderFormat::Pdf,
        ] {
            assert!(options.validate(format).is_ok());
        }
    }
    #[test]
    fn assets_reject_traversal_and_symlinks() {
        let temp = tempfile::tempdir().unwrap();
        assert!(load_assets(&temp.path().join("../outside")).is_err());
        let outside = temp.path().join("outside");
        std::fs::write(&outside, b"x").unwrap();
        let assets = temp.path().join("assets");
        std::fs::create_dir(&assets).unwrap();
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&outside, assets.join("escape")).unwrap();
            assert!(load_assets(&assets).is_err());
        }
    }
}
