//! Shared direct VLM runner for the canonical command and legacy CLI boundary.
use super::env::apply_route_env;
use crate::{
    MinerUVlmClient, MinerUVlmConfig, OfficeWorkers, OfficialPdfOptions, ProgressCallback,
    ProgressEvent, RasterWorkers, VlmHeader, VlmHttpConfig, canonical_stem,
    input_prepare::{DocumentKind, prepare_with_warning},
};
use cap_fs_ext::{DirExt, FollowSymlinks, OpenOptionsFollowExt};
use cap_std::{
    ambient_authority,
    fs::{Dir, OpenOptions},
};
use std::{
    io::{IsTerminal, Read, Write},
    panic::{AssertUnwindSafe, catch_unwind},
    path::{Component, Path, PathBuf},
    sync::Arc,
    time::Instant,
};

pub(super) type WarningCallback = Arc<dyn Fn(&str, &str) + Send + Sync + 'static>;
type DirectError = Box<dyn std::error::Error + Send + Sync + 'static>;

#[derive(Clone, Copy, Eq, PartialEq)]
enum DirectMode {
    LegacyOutput,
    CallbackOutput,
}

fn emit_event(callback: &Option<ProgressCallback>, event: ProgressEvent) {
    if let Some(callback) = callback {
        let _ = catch_unwind(AssertUnwindSafe(|| callback(event)));
    }
}

fn emit_warning(callback: &Option<WarningCallback>, source: &str, message: &str) {
    if let Some(callback) = callback {
        let _ = catch_unwind(AssertUnwindSafe(|| callback(source, message)));
    }
}

fn document_events(
    command_events: &Option<super::CommandCallback>,
    events: &Option<ProgressCallback>,
    document_id: usize,
) -> Option<ProgressCallback> {
    command_events
        .as_ref()
        .map(|callback| {
            super::scoped_progress(
                Some(Arc::clone(callback)),
                super::CommandScope::Document(super::DocumentId(document_id)),
            )
        })
        .or_else(|| events.clone())
}

#[derive(Debug)]
pub(super) struct DirectOptions {
    pub input: PathBuf,
    pub output: PathBuf,
    pub base_url: Option<String>,
    pub server_option_label: &'static str,
    pub model: Option<String>,
    pub api_key: Option<String>,
    pub page_start: Option<usize>,
    pub page_end: Option<usize>,
    pub no_formula: bool,
    pub no_table: bool,
    pub no_image_analysis: bool,
    pub batch_size: usize,
    pub canonical_mixed: bool,
}

fn err(s: impl Into<String>) -> DirectError {
    s.into().into()
}
fn clean(v: Option<String>, name: &str) -> Result<Option<String>, DirectError> {
    v.map(|v| {
        let v = v.trim().to_owned();
        if v.is_empty() || v.chars().any(char::is_control) {
            Err(err(format!(
                "{name} must be nonempty and contain no control characters"
            )))
        } else {
            Ok(v)
        }
    })
    .transpose()
}
fn absolute(path: &Path) -> Result<PathBuf, DirectError> {
    let path = if path.is_absolute() {
        path.to_owned()
    } else {
        std::env::current_dir()?.join(path)
    };
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            Component::RootDir => out.push("/"),
            Component::Normal(x) => out.push(x),
            Component::CurDir => {}
            Component::ParentDir => return Err(err("paths must not contain parent traversal")),
            Component::Prefix(_) => return Err(err("unsupported path")),
        }
    }
    #[cfg(target_os = "macos")]
    {
        if out.starts_with("/tmp") || out.starts_with("/var") {
            out = Path::new("/private").join(out.strip_prefix("/").unwrap());
        }
    }
    Ok(out)
}
fn kind_for(path: &Path) -> Option<DocumentKind> {
    DocumentKind::from_suffix(path.extension()?.to_str()?)
}
fn enumerate(
    path: &Path,
    inputs: &mut Vec<(PathBuf, DocumentKind)>,
    skipped: &mut Vec<PathBuf>,
) -> Result<(), DirectError> {
    let meta = std::fs::symlink_metadata(path)?;
    if meta.file_type().is_symlink() || !(meta.is_file() || meta.is_dir()) {
        return Err(err(format!(
            "unsupported symlink or special file: {}",
            path.display()
        )));
    }
    if meta.is_file() {
        let kind =
            kind_for(path).ok_or_else(|| err(format!("unsupported input: {}", path.display())))?;
        inputs.push((path.to_owned(), kind));
        return Ok(());
    }
    let mut entries: Vec<_> = std::fs::read_dir(path)?.collect::<Result<_, _>>()?;
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        let entry_path = entry.path();
        let meta = std::fs::symlink_metadata(&entry_path)?;
        if meta.is_file() {
            if let Some(kind) = kind_for(&entry_path) {
                inputs.push((entry_path, kind));
            } else {
                skipped.push(entry_path);
            }
        } else {
            skipped.push(entry_path);
        }
    }
    Ok(())
}

pub(super) fn discover_inputs(
    path: &Path,
) -> Result<(PathBuf, Vec<(PathBuf, DocumentKind)>, Vec<PathBuf>), DirectError> {
    let input = absolute(path)?;
    let mut inputs = Vec::new();
    let mut skipped = Vec::new();
    enumerate(&input, &mut inputs, &mut skipped)?;
    inputs.sort_by(|a, b| a.0.cmp(&b.0));
    if inputs.is_empty() {
        return Err(err("no supported inputs found"));
    }
    Ok((input, inputs, skipped))
}

pub(super) fn allocate_input_stems(
    inputs: &[(PathBuf, DocumentKind)],
) -> Result<Vec<String>, DirectError> {
    let raw_stems: Vec<_> = inputs
        .iter()
        .map(|(p, _)| {
            let stem = p
                .file_stem()
                .and_then(|x| x.to_str())
                .ok_or_else(|| err("non-UTF-8 input name"))?;
            canonical_stem(stem).map_err(|e| -> DirectError { Box::new(e) })
        })
        .collect::<Result<_, DirectError>>()?;
    Ok(crate::mineru_api::planning::unique_stems(&raw_stems))
}
fn open_dir(path: &Path) -> Result<Dir, DirectError> {
    let mut dir = Dir::open_ambient_dir("/", ambient_authority())?;
    for c in path.components().skip(1) {
        let Component::Normal(x) = c else {
            return Err(err("invalid directory path"));
        };
        dir = dir.open_dir_nofollow(x)?;
    }
    Ok(dir)
}
fn snapshot(path: &Path, cap: usize) -> Result<bytes::Bytes, DirectError> {
    let path = absolute(path)?;
    let names: Vec<_> = path
        .components()
        .skip(1)
        .map(|c| {
            if let Component::Normal(x) = c {
                Ok(x.to_owned())
            } else {
                Err(err("invalid input path"))
            }
        })
        .collect::<Result<_, _>>()?;
    let (leaf, parents) = names
        .split_last()
        .ok_or_else(|| err("input path has no file"))?;
    let mut dir = Dir::open_ambient_dir("/", ambient_authority())?;
    for p in parents {
        dir = dir.open_dir_nofollow(p)?;
    }
    let mut options = OpenOptions::new();
    options.read(true);
    options.follow(FollowSymlinks::No);
    let mut file = dir.open_with(leaf, &options)?;
    if !file.metadata()?.is_file() {
        return Err(err("input is not a regular file"));
    }
    let mut data = Vec::with_capacity(cap.min(1024 * 1024));
    (&mut file).take(cap as u64).read_to_end(&mut data)?;
    let mut probe = [0];
    if file.read(&mut probe)? != 0 {
        return Err(err("input exceeds maximum input size"));
    }
    Ok(data.into())
}
#[cfg(unix)]
fn same_dir(a: &Dir, b: &Dir) -> std::io::Result<bool> {
    use cap_primitives::fs::MetadataExt;
    let a = a.metadata(".")?;
    let b = b.metadata(".")?;
    Ok(a.dev() == b.dev() && a.ino() == b.ino())
}
fn output_chain(
    root: &Path,
    stem: &str,
    input: Option<&Dir>,
    target: &str,
) -> Result<PathBuf, DirectError> {
    let root = absolute(root)?;
    let mut current = Dir::open_ambient_dir("/", ambient_authority())?;
    let mut exists = true;
    let names: Vec<_> = root
        .components()
        .skip(1)
        .map(|c| {
            if let Component::Normal(x) = c {
                Ok(x.to_owned())
            } else {
                Err(err("invalid output path"))
            }
        })
        .chain([Ok(stem.into()), Ok(target.into())])
        .collect::<Result<_, _>>()?;
    for name in names {
        if exists {
            for entry in current.read_dir(".")? {
                let n = entry?.file_name();
                if n != name {
                    match (n.to_str(), name.to_str()) {
                        (Some(a), Some(b)) if a.to_lowercase() == b.to_lowercase() => {
                            return Err(err("output path has a case-insensitive alias"));
                        }
                        (None, _) | (_, None) => return Err(err("output path has an unsafe name")),
                        _ => {}
                    }
                }
            }
            match current.symlink_metadata(&name) {
                Ok(meta) => {
                    if meta.file_type().is_symlink() || !meta.is_dir() {
                        return Err(err("output path component is not a directory"));
                    }
                    let next = current.open_dir_nofollow(&name)?;
                    if let Some(input) = input {
                        #[cfg(unix)]
                        if same_dir(&next, input)? {
                            return Err(err("output directory must not be inside input directory"));
                        }
                        #[cfg(not(unix))]
                        return Err(err(
                            "directory input containment is unsupported on this platform",
                        ));
                    }
                    current = next;
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => exists = false,
                Err(e) => return Err(e.into()),
            }
        }
    }
    Ok(root)
}
fn config(options: &DirectOptions, env: &super::Environment) -> Result<VlmHttpConfig, DirectError> {
    let server = clean(options.base_url.clone(), options.server_option_label)?;
    let model = clean(options.model.clone(), "--model")?;
    let key = clean(
        options
            .api_key
            .clone()
            .or_else(|| env.string("MINERU_VL_API_KEY")),
        "--api-key",
    )?;
    let mut config = env.vlm_http_config();
    if let Some(server) = server {
        config.server_url = Some(server.parse()?);
        config.invalid_server_url = false;
    }
    if let Some(model) = model {
        config.model_name = Some(model);
    }
    config.model_name = config
        .model_name
        .map(|m| m.trim().to_owned())
        .filter(|m| !m.is_empty());
    config.skip_model_name_checking = config.model_name.is_some();
    if let Some(key) = key {
        config
            .headers
            .push(VlmHeader::new("Authorization", format!("Bearer {key}"))?);
    }
    Ok(config)
}

struct Progress<W: Write> {
    sink: W,
    tty: bool,
    failed: bool,
    batch: usize,
    total_batches: usize,
    count: usize,
    done: usize,
}
impl<W: Write> Progress<W> {
    fn new(sink: W, tty: bool) -> Self {
        Self {
            sink,
            tty,
            failed: false,
            batch: 0,
            total_batches: 0,
            count: 0,
            done: 0,
        }
    }
    fn say(&mut self, s: impl std::fmt::Display, final_state: bool) {
        if self.tty {
            let width = 20;
            let filled = if self.count == 0 {
                0
            } else {
                width * self.done / self.count
            };
            let _ = write!(
                self.sink,
                "\rbatch {}/{} [{}{}] {}/{}: {s}\x1b[K",
                self.batch,
                self.total_batches,
                "█".repeat(filled),
                "░".repeat(width - filled),
                self.done,
                self.count
            );
            if final_state {
                let _ = writeln!(self.sink);
            }
            let _ = self.sink.flush();
        } else {
            let _ = writeln!(self.sink, "{s}");
        }
    }
}

pub(super) async fn run_legacy(
    options: DirectOptions,
    env: super::Environment,
) -> Result<(), DirectError> {
    run_impl(
        options,
        None,
        None,
        None,
        DirectMode::LegacyOutput,
        None,
        env,
    )
    .await
}

#[allow(dead_code)]
pub(super) async fn run_with_events(
    options: DirectOptions,
    office_workers: OfficeWorkers,
    env: super::Environment,
    events: Option<ProgressCallback>,
    warnings: Option<WarningCallback>,
) -> Result<(), DirectError> {
    run_impl(
        options,
        events,
        None,
        warnings,
        DirectMode::CallbackOutput,
        Some(office_workers),
        env,
    )
    .await
}

pub(super) async fn run_with_scoped_events(
    options: DirectOptions,
    office_workers: OfficeWorkers,
    env: super::Environment,
    events: Option<super::CommandCallback>,
    warnings: Option<WarningCallback>,
) -> Result<(), DirectError> {
    run_impl(
        options,
        None,
        events,
        warnings,
        DirectMode::CallbackOutput,
        Some(office_workers),
        env,
    )
    .await
}

async fn run_impl(
    options: DirectOptions,
    events: Option<ProgressCallback>,
    command_events: Option<super::CommandCallback>,
    warnings: Option<WarningCallback>,
    mode: DirectMode,
    office_workers: Option<OfficeWorkers>,
    env: super::Environment,
) -> Result<(), DirectError> {
    if !options.canonical_mixed {
        return run_legacy_impl(options, &env).await;
    }
    let office_workers = office_workers.ok_or_else(|| err("office workers unavailable"))?;
    let raster_workers = RasterWorkers::default();
    let result = run_inner(
        &options,
        &office_workers,
        &raster_workers,
        events,
        command_events,
        warnings,
        mode,
        &env,
    )
    .await;
    office_workers.drain().await;
    raster_workers.drain().await;
    result
}

fn enumerate_legacy(
    path: &Path,
    pdfs: &mut Vec<PathBuf>,
    skipped: &mut Vec<PathBuf>,
) -> Result<(), DirectError> {
    let meta = std::fs::symlink_metadata(path)?;
    if meta.file_type().is_symlink() || !(meta.is_file() || meta.is_dir()) {
        return Err(err(format!(
            "unsupported symlink or special file: {}",
            path.display()
        )));
    }
    if meta.is_file() {
        if path
            .extension()
            .is_some_and(|x| x.eq_ignore_ascii_case("pdf"))
        {
            pdfs.push(path.to_owned());
        } else {
            skipped.push(path.to_owned());
        }
        return Ok(());
    }
    let mut entries: Vec<_> = std::fs::read_dir(path)?.collect::<Result<_, _>>()?;
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        enumerate_legacy(&entry.path(), pdfs, skipped)?;
    }
    Ok(())
}

async fn run_legacy_impl(
    options: DirectOptions,
    env: &super::Environment,
) -> Result<(), DirectError> {
    if options.batch_size == 0 {
        return Err(err("--batch-size must be greater than zero"));
    }
    let mut route = OfficialPdfOptions::default();
    route.start_page = options.page_start.unwrap_or(0);
    route.end_page = options.page_end;
    route.formula_enable = !options.no_formula;
    route.table_enable = !options.no_table;
    route.image_analysis = !options.no_image_analysis;
    if apply_route_env(&mut route, |name| env.os(name)) {
        eprintln!("warning: invalid MINERU_PROCESSING_WINDOW_SIZE; using 64");
    }
    route.validate()?;
    let input = absolute(&options.input)?;
    let output = absolute(&options.output)?;
    let mut pdfs = Vec::new();
    let mut skipped = Vec::new();
    enumerate_legacy(&input, &mut pdfs, &mut skipped)?;
    pdfs.sort();
    if pdfs.is_empty() {
        return Err(err("no PDF inputs found"));
    }
    let input_dir = std::fs::symlink_metadata(&input)?
        .is_dir()
        .then(|| open_dir(&input))
        .transpose()?;
    let mut planned = std::collections::BTreeSet::new();
    let candidates: Vec<_> = pdfs
        .into_iter()
        .map(|path| {
            let stem = canonical_stem(
                path.file_stem()
                    .and_then(|x| x.to_str())
                    .ok_or_else(|| err("non-UTF-8 input name"))?,
            )
            .map_err(|e| Box::new(e) as DirectError)?;
            if !planned.insert(format!("{}/vlm", stem.to_ascii_lowercase())) {
                return Err(err("duplicate output stem"));
            }
            output_chain(&output, &stem, input_dir.as_ref(), "vlm")?;
            Ok((path, stem))
        })
        .collect::<Result<_, DirectError>>()?;
    for path in skipped {
        eprintln!("skipped unsupported input: {}", path.display());
    }
    let client =
        MinerUVlmClient::connect(config(&options, env)?, MinerUVlmConfig::default()).await?;
    let total = candidates.len();
    let batches = total.div_ceil(options.batch_size);
    let mut progress = Progress::new(std::io::stderr(), std::io::stderr().is_terminal());
    let mut completed = 0;
    for (i, batch) in candidates.chunks(options.batch_size).enumerate() {
        progress.batch = i + 1;
        progress.total_batches = batches;
        progress.count = batch.len();
        progress.done = 0;
        progress.say(
            format_args!("batch {}/{}: {} document(s)", i + 1, batches, batch.len()),
            false,
        );
        for (path, stem) in batch {
            progress.say(
                format_args!(
                    "document {}/{}: processing {}",
                    completed + 1,
                    total,
                    path.display()
                ),
                false,
            );
            let result = async {
                let bytes = snapshot(path, route.max_pdf_bytes)?;
                let root = output_chain(&output, stem, input_dir.as_ref(), "vlm")?;
                client
                    .parse_and_write_official_pdf(
                        crate::PdfInput::Bytes(bytes),
                        route.clone(),
                        &root,
                        stem,
                    )
                    .await
                    .map_err(|e| Box::new(e) as DirectError)
            }
            .await;
            if let Err(error) = result {
                progress.failed = true;
                progress.say(format_args!("failed {}", path.display()), true);
                return Err(error);
            }
            completed += 1;
            progress.done += 1;
            progress.say(
                format_args!("document {completed}/{total}: completed {}", path.display()),
                false,
            );
        }
        if !progress.failed {
            progress.done = progress.count;
            progress.say(format_args!("batch {}/{}: completed", i + 1, batches), true);
        }
    }
    Ok(())
}

async fn run_inner(
    options: &DirectOptions,
    office_workers: &OfficeWorkers,
    raster_workers: &RasterWorkers,
    events: Option<ProgressCallback>,
    command_events: Option<super::CommandCallback>,
    warnings: Option<WarningCallback>,
    mode: DirectMode,
    env: &super::Environment,
) -> Result<(), DirectError> {
    if options.batch_size == 0 {
        return Err(err("--batch-size must be greater than zero"));
    }
    let mut route = OfficialPdfOptions::default();
    route.start_page = options.page_start.unwrap_or(0);
    route.end_page = options.page_end;
    route.formula_enable = !options.no_formula;
    route.table_enable = !options.no_table;
    route.image_analysis = !options.no_image_analysis;
    if apply_route_env(&mut route, |name| env.os(name)) {
        if mode == DirectMode::LegacyOutput {
            eprintln!("warning: invalid MINERU_PROCESSING_WINDOW_SIZE; using 64");
        } else {
            emit_warning(
                &warnings,
                "MINERU_PROCESSING_WINDOW_SIZE",
                "invalid value; using 64",
            );
        }
    }
    let input = absolute(&options.input)?;
    let output = absolute(&options.output)?;
    let (_, inputs, skipped) = discover_inputs(&input)?;
    let input_dir = if std::fs::symlink_metadata(&input)?.is_dir() {
        Some(open_dir(&input)?)
    } else {
        None
    };
    let mut validation = route.clone();
    if !inputs.iter().any(|(_, kind)| *kind == DocumentKind::Pdf) {
        validation.start_page = 0;
        validation.end_page = None;
    }
    validation.validate()?;
    let allocated = allocate_input_stems(&inputs)?;
    let candidates: Vec<_> = inputs
        .into_iter()
        .zip(allocated)
        .enumerate()
        .map(|(index, ((p, kind), stem))| {
            let target = if kind.is_office() { "office" } else { "vlm" };
            output_chain(&output, &stem, input_dir.as_ref(), target)?;
            Ok((index + 1, p, kind, stem))
        })
        .collect::<Result<_, DirectError>>()?;
    super::emit_command(
        &command_events,
        super::CommandEvent::RunPlanned {
            documents: candidates.len(),
            api_tasks: 0,
        },
    );
    for path in skipped {
        if mode == DirectMode::LegacyOutput {
            eprintln!("skipped unsupported input: {}", path.display());
        } else {
            emit_warning(&warnings, "unsupported input", &path.display().to_string());
        }
    }
    let client =
        MinerUVlmClient::connect(config(options, env)?, MinerUVlmConfig::default()).await?;
    let total = candidates.len();
    let batches = total.div_ceil(options.batch_size);
    let mut progress = Progress::new(std::io::stderr(), std::io::stderr().is_terminal());
    let mut completed = 0;
    for (i, batch) in candidates.chunks(options.batch_size).enumerate() {
        progress.batch = i + 1;
        progress.total_batches = batches;
        progress.count = batch.len();
        progress.done = 0;
        if mode == DirectMode::LegacyOutput {
            progress.say(
                format_args!("batch {}/{}: {} document(s)", i + 1, batches, batch.len()),
                false,
            );
        }
        for (candidate_id, path, kind, stem) in batch {
            let task_events = document_events(&command_events, &events, *candidate_id);
            if mode == DirectMode::LegacyOutput {
                progress.say(
                    format_args!(
                        "document {}/{}: processing {}",
                        completed + 1,
                        total,
                        path.display()
                    ),
                    false,
                );
            }
            if mode == DirectMode::CallbackOutput {
                emit_event(
                    &task_events,
                    ProgressEvent::DocumentStarted {
                        document: stem.clone(),
                    },
                );
            }
            let result = async {
                let deadline = Instant::now()
                    .checked_add(route.total_deadline)
                    .ok_or_else(|| err("input deadline overflow"))?;
                let bytes = snapshot(path, route.max_pdf_bytes)?;
                let target = if kind.is_office() { "office" } else { "vlm" };
                let root = output_chain(&output, stem, input_dir.as_ref(), target)?;
                let remaining = deadline
                    .checked_duration_since(Instant::now())
                    .filter(|d| !d.is_zero())
                    .ok_or_else(|| err("input deadline expired"))?;
                let (prepared, warning) = prepare_with_warning(
                    bytes,
                    *kind,
                    &route,
                    office_workers,
                    raster_workers,
                    remaining,
                )
                .await
                .map_err(err)?;
                if mode == DirectMode::CallbackOutput {
                    if let Some(message) = warning {
                        emit_event(
                            &task_events,
                            ProgressEvent::OfficeWarning {
                                document: stem.clone(),
                                message,
                            },
                        );
                    }
                    emit_event(
                        &task_events,
                        ProgressEvent::DocumentPrepared {
                            document: stem.clone(),
                        },
                    );
                }
                let mut route = route.clone();
                if !kind.supports_page_range() {
                    route.start_page = 0;
                    route.end_page = None;
                }
                route.total_deadline = deadline
                    .checked_duration_since(Instant::now())
                    .filter(|d| !d.is_zero())
                    .ok_or_else(|| err("input deadline expired"))?;
                if mode == DirectMode::CallbackOutput {
                    client
                        .parse_and_write_prepared_pdf_with_events(
                            prepared,
                            route,
                            &root,
                            stem,
                            task_events.clone(),
                        )
                        .await
                        .map_err(|e| -> DirectError { Box::new(e) })
                } else {
                    client
                        .parse_and_write_prepared_pdf(prepared, route, &root, stem)
                        .await
                        .map_err(|e| -> DirectError { Box::new(e) })
                }
            }
            .await;
            if let Err(e) = result {
                if mode == DirectMode::CallbackOutput {
                    emit_event(
                        &task_events,
                        ProgressEvent::DocumentFailed {
                            document: stem.clone(),
                            message: e.to_string(),
                        },
                    );
                }
                progress.failed = true;
                if mode == DirectMode::LegacyOutput {
                    progress.say(format_args!("failed {}", path.display()), true);
                }
                return Err(e);
            }
            completed += 1;
            progress.done += 1;
            if mode == DirectMode::CallbackOutput {
                emit_event(
                    &task_events,
                    ProgressEvent::DocumentCompleted {
                        document: stem.clone(),
                    },
                );
            } else {
                progress.say(
                    format_args!("document {completed}/{total}: completed {}", path.display()),
                    false,
                );
            }
        }
        if mode == DirectMode::LegacyOutput && !progress.failed {
            progress.done = progress.count;
            progress.say(format_args!("batch {}/{}: completed", i + 1, batches), true);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::env::{Decimal, decimal};
    use super::*;
    use std::collections::HashMap;
    use std::{ffi::OsString, time::Duration};

    #[test]
    fn decimal_matches_python_integer_lexing() {
        for (value, expected) in [
            ("  +001_024\u{2003}", Decimal::Positive(1024)),
            ("0", Decimal::NonPositive),
            ("-0", Decimal::NonPositive),
            ("-2", Decimal::NonPositive),
            ("-999999999999999999999999999999", Decimal::NonPositive),
            (
                "999999999999999999999999999999",
                Decimal::Positive(u64::MAX),
            ),
            ("", Decimal::Invalid),
            ("1.0", Decimal::Invalid),
            ("1e3", Decimal::Invalid),
            ("0x10", Decimal::Invalid),
            ("text", Decimal::Invalid),
            ("_1", Decimal::Invalid),
            ("1_", Decimal::Invalid),
            ("1__0", Decimal::Invalid),
            ("+", Decimal::Invalid),
            ("--1", Decimal::Invalid),
        ] {
            assert_eq!(
                decimal(&OsString::from(value), u64::MAX),
                expected,
                "{value:?}"
            );
        }
    }

    #[test]
    fn route_env_applies_numeric_and_boolean_overrides() {
        let values = HashMap::from([
            ("MINERU_PROCESSING_WINDOW_SIZE", OsString::from("0")),
            ("MINERU_PDF_RENDER_THREADS", OsString::from("+007")),
            (
                "MINERU_PDF_RENDER_TIMEOUT",
                OsString::from("999999999999999999999999999999"),
            ),
            ("MINERU_FORMULA_ENABLE", OsString::from("TrUe")),
            ("MINERU_TABLE_ENABLE", OsString::from(" yes")),
        ]);
        let mut route = OfficialPdfOptions::default();
        assert!(!apply_route_env(&mut route, |name| values
            .get(name)
            .cloned()));
        assert_eq!(route.processing_window_size, 1);
        assert_eq!(route.render_workers, 7);
        assert_eq!(route.render_timeout, Duration::from_secs(u64::MAX));
        assert!(route.formula_enable);
        assert!(!route.table_enable);
    }

    #[test]
    fn route_env_defaults_invalid_values_and_preserves_absent_booleans() {
        let values = HashMap::from([
            ("MINERU_PROCESSING_WINDOW_SIZE", OsString::from("1__0")),
            ("MINERU_PDF_RENDER_THREADS", OsString::from("-2")),
            ("MINERU_PDF_RENDER_TIMEOUT", OsString::from("1e3")),
            ("MINERU_FORMULA_ENABLE", OsString::from(" true ")),
            ("MINERU_TABLE_ENABLE", OsString::from("")),
        ]);
        let mut route = OfficialPdfOptions {
            formula_enable: false,
            table_enable: true,
            processing_window_size: 22,
            render_workers: 23,
            render_timeout: Duration::from_secs(24),
            ..Default::default()
        };
        assert!(apply_route_env(&mut route, |name| values
            .get(name)
            .cloned()));
        assert_eq!(route.processing_window_size, 64);
        assert_eq!(route.render_workers, 3);
        assert_eq!(route.render_timeout, Duration::from_secs(300));
        assert!(!route.formula_enable);
        assert!(!route.table_enable);

        let mut route = OfficialPdfOptions {
            formula_enable: false,
            table_enable: true,
            ..Default::default()
        };
        assert!(!apply_route_env(&mut route, |_| None));
        assert!(!route.formula_enable);
        assert!(route.table_enable);
    }

    #[test]
    fn route_env_boolean_values_require_exact_true() {
        for value in [" true", "true ", "1", "yes", ""] {
            let values = HashMap::from([("MINERU_FORMULA_ENABLE", OsString::from(value))]);
            let mut route = OfficialPdfOptions::default();
            assert!(!apply_route_env(&mut route, |name| values
                .get(name)
                .cloned()));
            assert!(!route.formula_enable, "{value:?}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn route_env_non_utf8_values_are_invalid_or_false() {
        use std::os::unix::ffi::OsStringExt;

        let values = HashMap::from([
            (
                "MINERU_PROCESSING_WINDOW_SIZE",
                OsString::from_vec(vec![0xff]),
            ),
            ("MINERU_PDF_RENDER_THREADS", OsString::from_vec(vec![0xff])),
            ("MINERU_PDF_RENDER_TIMEOUT", OsString::from_vec(vec![0xff])),
            ("MINERU_FORMULA_ENABLE", OsString::from_vec(vec![0xff])),
        ]);
        let mut route = OfficialPdfOptions::default();
        assert!(apply_route_env(&mut route, |name| values
            .get(name)
            .cloned()));
        assert_eq!(route.processing_window_size, 64);
        assert_eq!(route.render_workers, 3);
        assert_eq!(route.render_timeout, Duration::from_secs(300));
        assert!(!route.formula_enable);
    }

    #[test]
    fn snapshot_uses_an_overflow_probe() {
        let temp = tempfile::tempdir().unwrap();
        let pdf = temp.path().join("large.pdf");
        std::fs::write(&pdf, b"12345").unwrap();
        assert!(
            snapshot(&pdf, 4)
                .unwrap_err()
                .to_string()
                .contains("exceeds")
        );
        assert_eq!(snapshot(&pdf, 5).unwrap().as_ref(), b"12345");
    }

    #[cfg(unix)]
    #[test]
    fn nofollow_snapshot_and_output_chain_reject_links() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let input = temp.path().join("input");
        std::fs::create_dir(&input).unwrap();
        let target = input.join("target.pdf");
        std::fs::write(&target, b"x").unwrap();
        let leaf = input.join("leaf.pdf");
        std::fs::write(&leaf, b"x").unwrap();
        std::fs::remove_file(&leaf).unwrap();
        symlink(&target, &leaf).unwrap();
        assert!(snapshot(&leaf, 10).is_err());

        let input_dir = open_dir(&absolute(&input).unwrap()).unwrap();
        assert!(output_chain(&input.join("out"), "a", Some(&input_dir), "vlm").is_err());
        let output = temp.path().join("out");
        std::fs::create_dir(&output).unwrap();
        symlink(&input, output.join("a")).unwrap();
        assert!(output_chain(&output, "a", None, "vlm").is_err());
    }

    #[test]
    fn progress_tty_redraws_once_and_failure_cannot_succeed() {
        let mut tty = Progress::new(Vec::new(), true);
        tty.batch = 1;
        tty.total_batches = 1;
        tty.count = 2;
        tty.done = 1;
        tty.say("processing", false);
        tty.failed = true;
        tty.say("failed", true);
        let output = String::from_utf8(tty.sink).unwrap();
        assert!(
            output.contains("\r")
                && output.contains("\x1b[K")
                && output.contains("[██████████░░░░░░░░░░]")
        );
        assert_eq!(output.matches('\n').count(), 1);
        assert!(tty.failed);

        let mut plain = Progress::new(Vec::new(), false);
        plain.say("processing", false);
        plain.failed = true;
        plain.say("failed", true);
        let output = String::from_utf8(plain.sink).unwrap();
        assert_eq!(output, "processing\nfailed\n");
        assert!(!output.contains("completed"));
    }

    #[test]
    fn direct_callbacks_ignore_panics() {
        emit_event(
            &Some(Arc::new(|_| panic!("event callback"))),
            ProgressEvent::DocumentStarted {
                document: "doc".into(),
            },
        );
        emit_warning(
            &Some(Arc::new(|_, _| panic!("warning callback"))),
            "source",
            "message",
        );
        super::super::emit_command(
            &Some(Arc::new(|_| panic!("command callback"))),
            super::super::CommandEvent::RunCompleted,
        );
    }

    #[tokio::test]
    async fn callback_runner_stops_after_first_preparation_failure() {
        use std::sync::Mutex;

        let input = tempfile::tempdir().unwrap();
        let output = tempfile::tempdir().unwrap();
        std::fs::write(input.path().join("a.png"), b"not a PNG").unwrap();
        std::fs::write(input.path().join("b.png"), b"also not a PNG").unwrap();
        let events = Arc::new(Mutex::new(Vec::new()));
        let event_callback = {
            let events = Arc::clone(&events);
            Arc::new(move |event| events.lock().unwrap().push(event)) as ProgressCallback
        };
        let warnings = Arc::new(Mutex::new(Vec::new()));
        let warning_callback = {
            let warnings = Arc::clone(&warnings);
            Arc::new(move |source: &str, message: &str| {
                warnings
                    .lock()
                    .unwrap()
                    .push((source.to_owned(), message.to_owned()))
            }) as WarningCallback
        };
        let result = run_with_events(
            DirectOptions {
                input: input.path().to_owned(),
                output: output.path().to_owned(),
                base_url: Some("http://127.0.0.1:1".into()),
                server_option_label: "--url",
                model: Some("mock".into()),
                api_key: None,
                page_start: None,
                page_end: None,
                no_formula: false,
                no_table: false,
                no_image_analysis: false,
                batch_size: 1,
                canonical_mixed: true,
            },
            OfficeWorkers::with_executable(std::env::current_exe().unwrap()),
            super::super::Environment::process(),
            Some(event_callback),
            Some(warning_callback),
        )
        .await;
        assert_eq!(result.unwrap_err().to_string(), "invalid image");
        assert_eq!(
            *events.lock().unwrap(),
            vec![
                ProgressEvent::DocumentStarted {
                    document: "a".into(),
                },
                ProgressEvent::DocumentFailed {
                    document: "a".into(),
                    message: "invalid image".into(),
                },
            ]
        );
        assert!(warnings.lock().unwrap().is_empty());
    }

    #[test]
    fn document_callbacks_keep_page_events_on_their_stable_scope() {
        use super::super::{CommandEvent, CommandScope, DocumentId};
        use std::sync::Mutex;

        let commands = Arc::new(Mutex::new(Vec::new()));
        let callback = {
            let commands = Arc::clone(&commands);
            Arc::new(move |event| commands.lock().unwrap().push(event))
                as super::super::CommandCallback
        };
        let first = document_events(&Some(callback.clone()), &None, 1).unwrap();
        let second = document_events(&Some(callback), &None, 2).unwrap();
        first(ProgressEvent::DocumentStarted {
            document: "a".into(),
        });
        first(ProgressEvent::DocumentPageCompleted {
            document: "a".into(),
            page_index: 0,
            completed: 1,
            total: 1,
        });
        second(ProgressEvent::DocumentStarted {
            document: "b".into(),
        });
        let events = commands.lock().unwrap();
        assert!(matches!(
            events[0],
            CommandEvent::Progress {
                scope: CommandScope::Document(DocumentId(1)),
                ..
            }
        ));
        assert!(matches!(
            events[1],
            CommandEvent::Progress {
                scope: CommandScope::Document(DocumentId(1)),
                event: ProgressEvent::DocumentPageCompleted { .. }
            }
        ));
        assert!(matches!(
            events[2],
            CommandEvent::Progress {
                scope: CommandScope::Document(DocumentId(2)),
                ..
            }
        ));
    }

    #[tokio::test]
    async fn scoped_runner_keeps_second_id_after_first_success_then_failure() {
        use super::super::{CommandEvent, CommandScope, DocumentId};
        use axum::{Json, Router, routing::post};
        use serde_json::json;
        use std::io::Cursor;
        use std::sync::Mutex;

        let input = tempfile::tempdir().unwrap();
        let output = tempfile::tempdir().unwrap();
        let mut png = Vec::new();
        image::DynamicImage::new_rgb8(1, 1)
            .write_to(&mut Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();
        std::fs::write(input.path().join("a.png"), png).unwrap();
        std::fs::write(input.path().join("b.png"), b"also not a PNG").unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(
            axum::serve(
                listener,
                Router::new().route(
                    "/v1/chat/completions",
                    post(|| async {
                        Json(json!({"choices":[{"finish_reason":"stop","message":{"content":""}}]}))
                    }),
                ),
            )
            .into_future(),
        );
        let events = Arc::new(Mutex::new(Vec::new()));
        let callback = {
            let events = Arc::clone(&events);
            Arc::new(move |event| events.lock().unwrap().push(event))
                as super::super::CommandCallback
        };
        let result = run_with_scoped_events(
            DirectOptions {
                input: input.path().to_owned(),
                output: output.path().to_owned(),
                base_url: Some(base_url),
                server_option_label: "--url",
                model: Some("mock".into()),
                api_key: None,
                page_start: None,
                page_end: None,
                no_formula: false,
                no_table: false,
                no_image_analysis: false,
                batch_size: 1,
                canonical_mixed: true,
            },
            OfficeWorkers::with_executable(std::env::current_exe().unwrap()),
            super::super::Environment::process(),
            Some(callback),
            None,
        )
        .await;
        assert_eq!(result.unwrap_err().to_string(), "invalid image");
        let events = events.lock().unwrap();
        assert!(matches!(
            events[0],
            CommandEvent::RunPlanned {
                documents: 2,
                api_tasks: 0
            }
        ));
        let first_completed = events
            .iter()
            .position(|event| {
                matches!(
                    event,
                    CommandEvent::Progress {
                        scope: CommandScope::Document(DocumentId(1)),
                        event: ProgressEvent::DocumentCompleted { .. }
                    }
                )
            })
            .unwrap();
        let second_started = events
            .iter()
            .position(|event| {
                matches!(
                    event,
                    CommandEvent::Progress {
                        scope: CommandScope::Document(DocumentId(2)),
                        event: ProgressEvent::DocumentStarted { .. }
                    }
                )
            })
            .unwrap();
        let second_failed = events
            .iter()
            .position(|event| {
                matches!(
                    event,
                    CommandEvent::Progress {
                        scope: CommandScope::Document(DocumentId(2)),
                        event: ProgressEvent::DocumentFailed { .. }
                    }
                )
            })
            .unwrap();
        assert!(first_completed < second_started && second_started < second_failed);
        assert!(!events.iter().any(|event| matches!(
            event,
            CommandEvent::Progress {
                scope: CommandScope::Document(DocumentId(1)),
                event: ProgressEvent::DocumentFailed { .. }
            }
        )));
    }
}
