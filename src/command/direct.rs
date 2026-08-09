//! Shared direct VLM runner for the canonical command.
use crate::{
    MinerUVlmClient, MinerUVlmConfig, OfficeWorkers, OfficialPdfOptions, ProgressCallback,
    ProgressEvent, RasterWorkers, VlmHeader, VlmHttpConfig, canonical_stem,
    input_prepare::{DocumentKind, prepare_with_warning_and_ooxml},
};
#[cfg(unix)]
use cap_fs_ext::MetadataExt;
use cap_fs_ext::{DirExt, FollowSymlinks, OpenOptionsFollowExt};
use cap_std::{
    ambient_authority,
    fs::{Dir, OpenOptions},
};
use std::{
    ffi::OsString,
    io::Read,
    panic::{AssertUnwindSafe, catch_unwind},
    path::{Component, Path, PathBuf},
    sync::Arc,
    time::Instant,
};

pub(super) type WarningCallback = Arc<dyn Fn(&str, &str) + Send + Sync + 'static>;
type DirectError = Box<dyn std::error::Error + Send + Sync + 'static>;

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

fn cleanup_warning_callback(
    warnings: &Option<WarningCallback>,
) -> Option<crate::official_route::CleanupWarningCallback> {
    warnings.as_ref().map(|warnings| {
        let warnings = Arc::clone(warnings);
        Arc::new(move || {
            emit_warning(
                &Some(Arc::clone(&warnings)),
                "official output cleanup",
                "published output cleanup failed",
            );
        }) as crate::official_route::CleanupWarningCallback
    })
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
    /// `Some` forces the boolean from a surface that owns it (the legacy `--no-*` flags).
    /// `None` means the value was already resolved with strict default -> env -> CLI precedence
    /// through `CoreOverrides` and must not be re-applied.
    pub no_formula: Option<bool>,
    pub no_table: Option<bool>,
    pub no_image_analysis: Option<bool>,
    pub document_limits: crate::DocumentLimitPolicy,
}

/// Caps each resident route budget at the aggregate document policy budget.
///
/// A resident budget that was explicitly configured (via `CoreOverrides`) must never be silently
/// reduced by the derivation; that unresolvable inversion is an error. Reducing only the compiled
/// default to the derived policy budget remains the legitimate aggregate derivation.
fn apply_document_limits(
    route: &mut OfficialPdfOptions,
    limits: crate::DocumentLimitPolicy,
    core: &super::env::CoreOverrides,
) -> Result<(), DirectError> {
    route.max_encoded_document_bytes = cap_resident(
        route.max_encoded_document_bytes,
        // No CoreOverrides knob owns the resident encoded-document budget; the policy value IS
        // the operator's explicit input, so capping the compiled default is always legitimate.
        false,
        limits.max_encoded_document_bytes,
        "MINERU_MAX_ENCODED_DOCUMENT_BYTES",
    )?;
    route.max_raw_output_bytes = cap_resident(
        route.max_raw_output_bytes,
        core.max_raw_output_bytes.is_some(),
        limits.raw_output_bytes,
        "MINERU_MAX_RAW_OUTPUT_BYTES",
    )?;
    route.max_total_asset_bytes = cap_resident(
        route.max_total_asset_bytes,
        core.max_total_asset_bytes.is_some(),
        limits.asset_total_bytes,
        "MINERU_MAX_TOTAL_ASSET_BYTES",
    )?;
    route.max_staged_text_bytes = cap_resident(
        route.max_staged_text_bytes,
        core.max_staged_text_bytes.is_some(),
        limits.staged_text_bytes,
        "MINERU_MAX_STAGED_TEXT_BYTES",
    )?;
    Ok(())
}

/// Checked aggregate derivation: a budget that cannot be represented on this platform never
/// caps the (already `usize`) resident value; an explicit resident value that the derivation
/// would shrink fails loudly instead of being silently reduced.
fn cap_resident(
    resident: usize,
    resident_explicit: bool,
    budget: u64,
    name: &str,
) -> Result<usize, DirectError> {
    let Ok(budget) = usize::try_from(budget) else {
        return Ok(resident);
    };
    if resident > budget {
        if resident_explicit {
            return Err(err(format!(
                "{name}={resident} exceeds the derived document budget {budget}; raise the output budget or lower {name}"
            )));
        }
        return Ok(budget);
    }
    Ok(resident)
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
#[cfg(unix)]
fn absolute(path: &Path) -> Result<PathBuf, DirectError> {
    let path = if path.is_absolute() {
        path.to_owned()
    } else {
        std::env::current_dir()?.join(path)
    };
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            Component::RootDir => out.push(c.as_os_str()),
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
#[cfg(windows)]
fn absolute(path: &Path) -> Result<PathBuf, DirectError> {
    use std::path::Prefix;

    if let Some(Component::Prefix(prefix)) = path.components().next() {
        match prefix.kind() {
            Prefix::Disk(_) | Prefix::UNC(_, _) if !path.is_absolute() => {
                return Err(err("drive-relative paths are unsupported"));
            }
            Prefix::Disk(_) | Prefix::UNC(_, _) => {}
            _ => return Err(err("device namespace paths are unsupported")),
        }
    }
    let path = if path.is_absolute() {
        path.to_owned()
    } else {
        std::env::current_dir()?.join(path)
    };
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            Component::Prefix(prefix) => match prefix.kind() {
                Prefix::Disk(_) | Prefix::UNC(_, _) => out.push(c.as_os_str()),
                _ => return Err(err("device namespace paths are unsupported")),
            },
            Component::RootDir => out.push(c.as_os_str()),
            Component::Normal(x) => out.push(x),
            Component::CurDir => {}
            Component::ParentDir => return Err(err("paths must not contain parent traversal")),
        }
    }
    if !out.is_absolute() {
        return Err(err("unsupported path"));
    }
    Ok(out)
}
#[cfg(not(any(unix, windows)))]
fn absolute(_path: &Path) -> Result<PathBuf, DirectError> {
    Err(err("direct paths are unsupported on this platform"))
}

#[cfg(unix)]
fn anchor_and_names(path: &Path) -> Result<(PathBuf, Vec<OsString>), DirectError> {
    let mut components = path.components();
    let Some(Component::RootDir) = components.next() else {
        return Err(err("path is not absolute"));
    };
    let names = components
        .map(|c| match c {
            Component::Normal(name) => Ok(name.to_owned()),
            _ => Err(err("invalid absolute path")),
        })
        .collect::<Result<_, _>>()?;
    Ok((PathBuf::from("/"), names))
}
#[cfg(windows)]
fn anchor_and_names(path: &Path) -> Result<(PathBuf, Vec<OsString>), DirectError> {
    use std::path::Prefix;

    let mut components = path.components();
    let Some(Component::Prefix(prefix)) = components.next() else {
        return Err(err("path has no filesystem prefix"));
    };
    match prefix.kind() {
        Prefix::Disk(_) | Prefix::UNC(_, _) => {}
        _ => return Err(err("device namespace paths are unsupported")),
    }
    let mut anchor = PathBuf::from(prefix.as_os_str());
    match (prefix.kind(), components.next()) {
        (Prefix::Disk(_), Some(root @ Component::RootDir))
        | (Prefix::UNC(_, _), Some(root @ Component::RootDir)) => {
            anchor.push(root.as_os_str());
        }
        (Prefix::UNC(_, _), None) => {}
        (Prefix::Disk(_), _) => return Err(err("drive-relative paths are unsupported")),
        _ => return Err(err("invalid absolute path")),
    }
    let names = components
        .map(|c| match c {
            Component::Normal(name) => Ok(name.to_owned()),
            _ => Err(err("invalid absolute path")),
        })
        .collect::<Result<_, _>>()?;
    Ok((anchor, names))
}
#[cfg(not(any(unix, windows)))]
fn anchor_and_names(_path: &Path) -> Result<(PathBuf, Vec<OsString>), DirectError> {
    Err(err("direct paths are unsupported on this platform"))
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
    let (anchor, names) = anchor_and_names(path)?;
    let mut dir = Dir::open_ambient_dir(anchor, ambient_authority())?;
    for name in names {
        dir = dir.open_dir_nofollow(name)?;
    }
    Ok(dir)
}
#[derive(Debug)]
struct Snapshot {
    bytes: bytes::Bytes,
}

fn snapshot(path: &Path, cap: usize, max_input_bytes: u64) -> Result<Snapshot, DirectError> {
    let path = absolute(path)?;
    let (anchor, names) = anchor_and_names(&path)?;
    let (leaf, parents) = names
        .split_last()
        .ok_or_else(|| err("input path has no file"))?;
    let mut dir = Dir::open_ambient_dir(anchor, ambient_authority())?;
    for p in parents {
        dir = dir.open_dir_nofollow(p)?;
    }
    let mut options = OpenOptions::new();
    options.read(true);
    options.follow(FollowSymlinks::No);
    let mut file = dir.open_with(leaf, &options)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(err("input is not a regular file"));
    }
    if metadata.len() > max_input_bytes {
        return Err(err(format!(
            "input exceeds configured limit of {max_input_bytes} bytes"
        )));
    }
    if metadata.len() > cap as u64 {
        return Err(err("input exceeds resident preparation limit"));
    }
    let mut data = Vec::with_capacity(cap.min(1024 * 1024));
    copy_capped(&mut file, &mut data, cap, max_input_bytes)?;
    Ok(Snapshot { bytes: data.into() })
}

fn copy_capped(
    input: &mut impl Read,
    output: &mut Vec<u8>,
    cap: usize,
    source_cap: u64,
) -> Result<(), DirectError> {
    let mut total = 0u64;
    let mut buffer = [0; 64 * 1024];
    loop {
        let read = input.read(&mut buffer)?;
        if read == 0 {
            return Ok(());
        }
        total = total.saturating_add(read as u64);
        if total > source_cap {
            return Err(err(format!(
                "input exceeds configured limit of {source_cap} bytes"
            )));
        }
        if output.len().saturating_add(read) > cap {
            return Err(err("input exceeds maximum input size"));
        }
        output.extend_from_slice(&buffer[..read]);
    }
}
#[cfg(unix)]
fn same_dir(a: &Dir, b: &Dir) -> std::io::Result<bool> {
    let a = a.dir_metadata()?;
    let b = b.dir_metadata()?;
    Ok(a.dev() == b.dev() && a.ino() == b.ino())
}
#[cfg(windows)]
fn same_dir(a: &Dir, b: &Dir) -> std::io::Result<bool> {
    use std::{
        mem::{MaybeUninit, size_of},
        os::windows::io::AsRawHandle,
    };
    use windows_sys::Win32::{
        Foundation::HANDLE,
        Storage::FileSystem::{FILE_ID_INFO, FileIdInfo, GetFileInformationByHandleEx},
    };

    fn identity(dir: &Dir) -> std::io::Result<(u64, [u8; 16])> {
        let mut info = MaybeUninit::<FILE_ID_INFO>::uninit();
        // SAFETY: `info` is sized for FileIdInfo and initialized only on API success.
        let ok = unsafe {
            GetFileInformationByHandleEx(
                dir.as_raw_handle() as HANDLE,
                FileIdInfo,
                info.as_mut_ptr().cast(),
                size_of::<FILE_ID_INFO>() as u32,
            )
        };
        if ok == 0 {
            return Err(std::io::Error::last_os_error());
        }
        // SAFETY: a successful call initialized the complete FILE_ID_INFO buffer.
        let info = unsafe { info.assume_init() };
        Ok((info.VolumeSerialNumber, info.FileId.Identifier))
    }

    Ok(identity(a)? == identity(b)?)
}
#[cfg(not(any(unix, windows)))]
fn same_dir(_a: &Dir, _b: &Dir) -> std::io::Result<bool> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "directory identity is unsupported on this platform",
    ))
}
fn output_chain(
    root: &Path,
    stem: &str,
    input: Option<&Dir>,
    target: &str,
) -> Result<PathBuf, DirectError> {
    let root = absolute(root)?;
    let (anchor, mut names) = anchor_and_names(&root)?;
    let mut current = Dir::open_ambient_dir(anchor, ambient_authority())?;
    if let Some(input) = input {
        if same_dir(&current, input)? {
            return Err(err("output directory must not be inside input directory"));
        }
    }
    let mut exists = true;
    names.extend([stem.into(), target.into()]);
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
                        if same_dir(&next, input)? {
                            return Err(err("output directory must not be inside input directory"));
                        }
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
fn config(
    options: &DirectOptions,
    env: &super::Environment,
    mut http: VlmHttpConfig,
) -> Result<VlmHttpConfig, DirectError> {
    let server = clean(options.base_url.clone(), options.server_option_label)?;
    let model = clean(options.model.clone(), "--model")?;
    let key = clean(
        options
            .api_key
            .clone()
            .or_else(|| env.string("MINERU_VL_API_KEY")),
        "--api-key",
    )?;
    if let Some(server) = server {
        http.server_url = Some(server.parse()?);
        http.invalid_server_url = false;
    }
    if let Some(model) = model {
        http.model_name = Some(model);
    }
    http.model_name = http
        .model_name
        .map(|m| m.trim().to_owned())
        .filter(|m| !m.is_empty());
    http.skip_model_name_checking = http.model_name.is_some();
    if let Some(key) = key {
        http.headers
            .push(VlmHeader::new("Authorization", format!("Bearer {key}"))?);
    }
    Ok(http)
}

/// Resolves the strict core policy (compiled default -> frozen environment -> CLI). The formula,
/// table, and image-analysis booleans resolve through `CoreOverrides`; the legacy `--no-*` flags
/// force their values only when the owning surface actually provided them.
fn resolved_route(
    options: &DirectOptions,
    env: &super::Environment,
    overrides: &super::RunOverrides,
) -> Result<super::env::ResolvedCore, DirectError> {
    let mut resolved =
        super::env::resolve_core(|name| env.os(name), &overrides.core).map_err(err)?;
    resolved.route.start_page = options.page_start.unwrap_or(0);
    resolved.route.end_page = options.page_end;
    if let Some(no_formula) = options.no_formula {
        resolved.route.formula_enable = !no_formula;
    }
    if let Some(no_table) = options.no_table {
        resolved.route.table_enable = !no_table;
    }
    if let Some(no_image_analysis) = options.no_image_analysis {
        resolved.route.image_analysis = !no_image_analysis;
    }
    Ok(resolved)
}

pub(super) async fn run_with_scoped_events(
    options: DirectOptions,
    office_workers: OfficeWorkers,
    env: super::Environment,
    overrides: super::RunOverrides,
    service: super::service::ResolvedService,
    events: Option<super::CommandCallback>,
    warnings: Option<WarningCallback>,
) -> Result<(), DirectError> {
    let raster_workers = RasterWorkers::default();
    let result = run_inner(
        &options,
        &office_workers,
        &raster_workers,
        None,
        events,
        warnings,
        &env,
        &overrides,
        &service,
    )
    .await;
    office_workers.drain().await;
    raster_workers.drain().await;
    result
}

async fn run_inner(
    options: &DirectOptions,
    office_workers: &OfficeWorkers,
    raster_workers: &RasterWorkers,
    events: Option<ProgressCallback>,
    command_events: Option<super::CommandCallback>,
    warnings: Option<WarningCallback>,
    env: &super::Environment,
    overrides: &super::RunOverrides,
    service: &super::service::ResolvedService,
) -> Result<(), DirectError> {
    let mut resolved = resolved_route(options, env, overrides)?;
    apply_document_limits(
        &mut resolved.route,
        options.document_limits,
        &overrides.core,
    )?;
    let totals =
        crate::document_limits::OfficialDocumentTotals::from_policy(options.document_limits);
    let route = resolved.route;
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
        emit_warning(&warnings, "unsupported input", &path.display().to_string());
    }
    let http = config(options, env, resolved.http)?;
    let page_concurrency = crate::official_route::OfficialPageConcurrency::new(
        resolved.page_concurrency,
        route.processing_window_size,
        http.max_concurrency,
    )
    .map_err(|error| err(error.to_string()))?;
    let client = MinerUVlmClient::connect(http, MinerUVlmConfig::default()).await?;
    for (candidate_id, path, kind, stem) in &candidates {
        let task_events = document_events(&command_events, &events, *candidate_id);
        emit_event(
            &task_events,
            ProgressEvent::DocumentStarted {
                document: stem.clone(),
            },
        );
        let cleanup_warning = cleanup_warning_callback(&warnings);
        let result = async {
            let deadline = Instant::now()
                .checked_add(route.total_deadline)
                .ok_or_else(|| err("input deadline overflow"))?;
            let snapshot = snapshot(
                path,
                route.max_pdf_bytes,
                options.document_limits.max_input_bytes,
            )?;
            let bytes = snapshot.bytes;
            let target = if kind.is_office() { "office" } else { "vlm" };
            let root = output_chain(&output, stem, input_dir.as_ref(), target)?;
            let remaining = deadline
                .checked_duration_since(Instant::now())
                .filter(|d| !d.is_zero())
                .ok_or_else(|| err("input deadline expired"))?;
            let (prepared, warning) = prepare_with_warning_and_ooxml(
                bytes,
                *kind,
                &route,
                office_workers,
                raster_workers,
                remaining,
                service.ooxml,
            )
            .await
            .map_err(err)?;
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
            let mut route = route.clone();
            if !kind.supports_page_range() {
                route.start_page = 0;
                route.end_page = None;
            }
            route.total_deadline = deadline
                .checked_duration_since(Instant::now())
                .filter(|d| !d.is_zero())
                .ok_or_else(|| err("input deadline expired"))?;
            client
                .parse_and_write_prepared_pdf_with_totals_and_page_concurrency(
                    prepared,
                    route,
                    &root,
                    stem,
                    task_events.clone(),
                    cleanup_warning,
                    totals,
                    page_concurrency.clone(),
                )
                .await
                .map_err(|e| -> DirectError { Box::new(e) })
        }
        .await;
        if let Err(e) = result {
            emit_event(
                &task_events,
                ProgressEvent::DocumentFailed {
                    document: stem.clone(),
                    message: e.to_string(),
                },
            );
            return Err(e);
        }
        emit_event(
            &task_events,
            ProgressEvent::DocumentCompleted {
                document: stem.clone(),
            },
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::ffi::OsString;

    fn snapshot_pdf(path: &Path, cap: usize, max: u64) -> Result<Snapshot, DirectError> {
        snapshot(path, cap, max)
    }

    fn test_options() -> DirectOptions {
        DirectOptions {
            input: PathBuf::new(),
            output: PathBuf::new(),
            base_url: None,
            server_option_label: "--url",
            model: None,
            api_key: None,
            page_start: None,
            page_end: None,
            no_formula: None,
            no_table: None,
            no_image_analysis: None,
            document_limits: crate::DocumentLimitPolicy::defaults(),
        }
    }

    #[test]
    fn boolean_route_resolution_is_strict_default_env_cli() {
        // Strict env beats the compiled default; the legacy `--no-*` force only when present.
        let env_values = HashMap::from([
            ("MINERU_FORMULA_ENABLE", OsString::from("false")),
            ("MINERU_TABLE_ENABLE", OsString::from("TrUe")),
        ]);
        let env = super::super::Environment::from_values(env_values);
        let options = DirectOptions {
            no_formula: None,
            no_table: None,
            no_image_analysis: None,
            ..test_options()
        };
        let resolved = resolved_route(&options, &env, &super::super::RunOverrides::default())
            .expect("strict booleans resolve");
        assert!(!resolved.route.formula_enable);
        assert!(resolved.route.table_enable);
        assert!(resolved.route.image_analysis);

        // An explicit legacy `--no-*` value is the surface's CLI and wins over env.
        let forced = DirectOptions {
            no_formula: Some(true),
            ..test_options()
        };
        let resolved = resolved_route(&forced, &env, &super::super::RunOverrides::default())
            .expect("strict booleans resolve");
        assert!(!resolved.route.formula_enable);
    }

    #[cfg(unix)]
    #[test]
    fn boolean_route_env_rejects_non_utf8_values() {
        use std::os::unix::ffi::OsStringExt;

        let env = super::super::Environment::from_values(HashMap::from([(
            "MINERU_FORMULA_ENABLE",
            OsString::from_vec(vec![0xff]),
        )]));
        let options = DirectOptions {
            no_formula: None,
            ..test_options()
        };
        let error = resolved_route(&options, &env, &super::super::RunOverrides::default())
            .expect_err("non-UTF-8 boolean fails before work");
        assert!(
            error.to_string().contains("MINERU_FORMULA_ENABLE"),
            "{error}"
        );
    }

    #[test]
    fn strict_core_resolution_errors_before_work() {
        // Malformed or zero-where-invalid frozen environment values fail resolution instead of
        // falling back silently.
        let env = super::super::Environment::from_values(HashMap::from([(
            "MINERU_PDF_RENDER_TIMEOUT",
            OsString::from("1e3"),
        )]));
        let error =
            super::super::env::resolve_core(|name| env.os(name), &Default::default()).unwrap_err();
        assert!(error.contains("MINERU_PDF_RENDER_TIMEOUT"), "{error}");
        let env = super::super::Environment::from_values(HashMap::from([(
            "MINERU_PROCESSING_WINDOW_SIZE",
            OsString::from("0"),
        )]));
        assert!(super::super::env::resolve_core(|name| env.os(name), &Default::default()).is_err());
    }

    #[test]
    fn document_limits_derive_resident_caps_but_never_shrink_explicit_values() {
        // A small output budget derives small resident caps.
        let policy = crate::DocumentLimitPolicy::new(4, 4, 1024).unwrap();
        // (raw = staged = 1024/4 = 256, assets = 1024, encoded = 4)

        // Compiled defaults are legitimately capped by the aggregate derivation.
        let mut route = OfficialPdfOptions::default();
        let core = super::super::env::CoreOverrides::default();
        apply_document_limits(&mut route, policy, &core).unwrap();
        assert_eq!(route.max_raw_output_bytes, 256);
        assert_eq!(route.max_staged_text_bytes, 256);
        assert_eq!(route.max_total_asset_bytes, 1024);
        assert_eq!(route.max_encoded_document_bytes, 4);
        assert_eq!(
            route.max_encoded_document_bytes,
            policy.max_encoded_document_bytes as usize
        );

        // An explicit resident value the derivation would shrink is an error, not a silent min().
        // The route already carries the applied explicit value, mirroring the resolved flow.
        let core = super::super::env::CoreOverrides {
            max_raw_output_bytes: Some(1 << 20),
            ..Default::default()
        };
        let route = OfficialPdfOptions {
            max_raw_output_bytes: 1 << 20,
            ..OfficialPdfOptions::default()
        };
        let error = apply_document_limits(&mut route.clone(), policy, &core).unwrap_err();
        assert!(
            error.to_string().contains("MINERU_MAX_RAW_OUTPUT_BYTES"),
            "{error}"
        );

        // An explicit value that fits the derived budget is preserved exactly.
        let core = super::super::env::CoreOverrides {
            max_staged_text_bytes: Some(128),
            ..Default::default()
        };
        let mut route = OfficialPdfOptions {
            max_staged_text_bytes: 128,
            ..OfficialPdfOptions::default()
        };
        apply_document_limits(&mut route, policy, &core).unwrap();
        assert_eq!(route.max_staged_text_bytes, 128);
    }

    #[test]
    fn snapshot_uses_an_overflow_probe() {
        let temp = tempfile::tempdir().unwrap();
        let pdf = temp.path().join("large.pdf");
        std::fs::write(&pdf, b"12345").unwrap();
        assert!(
            snapshot_pdf(&pdf, 4, 4)
                .unwrap_err()
                .to_string()
                .contains("exceeds")
        );
        assert_eq!(snapshot_pdf(&pdf, 5, 5).unwrap().bytes.as_ref(), b"12345");
    }

    #[test]
    fn oversized_pdf_is_rejected_from_the_opened_descriptor() {
        use lopdf::{Document, Object, Stream, dictionary};

        let temp = tempfile::tempdir().unwrap();
        let pdf = temp.path().join("large.pdf");
        let mut doc = Document::with_version("1.5");
        let page = doc.add_object(dictionary! {"Type" => "Page", "MediaBox" => vec![0.into(), 0.into(), 1.into(), 1.into()]});
        let pages = doc.new_object_id();
        doc.objects.insert(
            pages,
            Object::Dictionary(
                dictionary! {"Type" => "Pages", "Kids" => vec![page.into()], "Count" => 1},
            ),
        );
        doc.get_object_mut(page)
            .unwrap()
            .as_dict_mut()
            .unwrap()
            .set("Parent", pages);
        let catalog = doc.add_object(dictionary! {"Type" => "Catalog", "Pages" => pages});
        doc.trailer.set("Root", catalog);
        doc.add_object(Stream::new(dictionary! {}, vec![b'x'; 4096]));
        let mut source = Vec::new();
        doc.save_to(&mut source).unwrap();
        std::fs::write(&pdf, source).unwrap();

        assert!(snapshot(&pdf, 1024, 8192).is_err());
    }

    #[test]
    fn non_pdf_over_resident_cap_is_rejected_before_reading() {
        let temp = tempfile::tempdir().unwrap();
        let png = temp.path().join("large.png");
        std::fs::write(&png, [0; 5]).unwrap();
        assert!(snapshot(&png, 4, 10).is_err());
    }

    #[test]
    fn directory_identity_distinguishes_handles() {
        let temp = tempfile::tempdir().unwrap();
        let sibling = temp.path().join("sibling");
        std::fs::create_dir(&sibling).unwrap();
        let first = open_dir(&absolute(temp.path()).unwrap()).unwrap();
        let second = open_dir(&absolute(temp.path()).unwrap()).unwrap();
        let sibling = open_dir(&absolute(&sibling).unwrap()).unwrap();
        assert!(same_dir(&first, &second).unwrap());
        assert!(!same_dir(&first, &sibling).unwrap());
    }

    #[test]
    fn output_chain_rejects_equal_or_nested_input_but_allows_outside() {
        let temp = tempfile::tempdir().unwrap();
        let input = temp.path().join("input");
        let nested = input.join("nested");
        let output = temp.path().join("output");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::create_dir(&output).unwrap();
        let input_dir = open_dir(&absolute(&input).unwrap()).unwrap();

        assert!(output_chain(&output, "a", Some(&input_dir), "vlm").is_ok());
        assert!(output_chain(&input, "a", Some(&input_dir), "vlm").is_err());
        assert!(output_chain(&nested, "a", Some(&input_dir), "vlm").is_err());
    }

    #[test]
    fn output_chain_rejects_filesystem_anchor_input_before_traversal() {
        let temp = tempfile::tempdir().unwrap();
        let path = absolute(temp.path()).unwrap();
        let (anchor, _) = anchor_and_names(&path).unwrap();
        let anchor_dir = open_dir(&anchor).unwrap();
        assert!(output_chain(&path, "a", Some(&anchor_dir), "vlm").is_err());
    }

    #[cfg(windows)]
    #[test]
    fn windows_paths_accept_filesystem_roots_only() {
        assert!(absolute(Path::new(r"C:\input\file.pdf")).is_ok());
        assert!(absolute(Path::new(r"\\server\share\input\file.pdf")).is_ok());
        assert!(
            anchor_and_names(Path::new(r"\\server\share"))
                .unwrap()
                .1
                .is_empty()
        );
        assert!(absolute(Path::new(r"C:input\file.pdf")).is_err());
        assert!(absolute(Path::new(r"\\?\C:\input\file.pdf")).is_err());
        assert!(absolute(Path::new(r"\\.\device")).is_err());
    }

    #[cfg(windows)]
    #[test]
    fn output_chain_rejects_junction_redirect_into_input() {
        let temp = tempfile::tempdir().unwrap();
        let input = temp.path().join("input");
        let output = temp.path().join("output");
        let junction = output.join("redirect");
        std::fs::create_dir(&input).unwrap();
        std::fs::create_dir(&output).unwrap();
        let status = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(&junction)
            .arg(&input)
            .status()
            .unwrap();
        assert!(status.success(), "mklink /J failed: {status}");

        let rejected = output_chain(&junction, "a", None, "vlm").is_err();
        std::fs::remove_dir(&junction).unwrap();
        assert!(rejected);
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
        assert!(snapshot_pdf(&leaf, 10, 10).is_err());

        let output = temp.path().join("out");
        std::fs::create_dir(&output).unwrap();
        symlink(&input, output.join("a")).unwrap();
        assert!(output_chain(&output, "a", None, "vlm").is_err());
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

    #[test]
    fn cleanup_failure_warning_is_generic_and_emitted_once() {
        use std::sync::Mutex;

        let warnings = Arc::new(Mutex::new(Vec::new()));
        let callback = {
            let warnings = Arc::clone(&warnings);
            Arc::new(move |source: &str, message: &str| {
                warnings
                    .lock()
                    .unwrap()
                    .push((source.to_owned(), message.to_owned()))
            }) as WarningCallback
        };
        cleanup_warning_callback(&Some(callback)).unwrap()();
        assert_eq!(
            *warnings.lock().unwrap(),
            vec![(
                "official output cleanup".into(),
                "published output cleanup failed".into()
            )]
        );
    }

    #[tokio::test]
    async fn callback_runner_stops_after_first_preparation_failure() {
        use super::super::{CommandEvent, CommandScope, DocumentId};
        use std::sync::Mutex;

        let input = tempfile::tempdir().unwrap();
        let output = tempfile::tempdir().unwrap();
        std::fs::write(input.path().join("a.png"), b"not a PNG").unwrap();
        std::fs::write(input.path().join("b.png"), b"also not a PNG").unwrap();
        let events = Arc::new(Mutex::new(Vec::new()));
        let event_callback = {
            let events = Arc::clone(&events);
            Arc::new(move |event| events.lock().unwrap().push(event))
                as super::super::CommandCallback
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
        let result = run_with_scoped_events(
            DirectOptions {
                input: input.path().to_owned(),
                output: output.path().to_owned(),
                base_url: Some("http://127.0.0.1:1".into()),
                server_option_label: "--url",
                model: Some("mock".into()),
                api_key: None,
                page_start: None,
                page_end: None,
                no_formula: None,
                no_table: None,
                no_image_analysis: None,
                document_limits: crate::DocumentLimitPolicy::defaults(),
            },
            OfficeWorkers::with_executable(std::env::current_exe().unwrap()),
            super::super::Environment::process(),
            super::super::RunOverrides::default(),
            super::super::service::resolve_service(
                &(|name| std::env::var_os(name)),
                &super::super::service::ServiceOverrides::default(),
                crate::DocumentLimitPolicy::defaults(),
            )
            .unwrap(),
            Some(event_callback),
            Some(warning_callback),
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
        // The first document fails at preparation; the runner stops before starting the second.
        assert!(matches!(
            events[1],
            CommandEvent::Progress {
                scope: CommandScope::Document(DocumentId(1)),
                event: ProgressEvent::DocumentStarted { .. },
            }
        ));
        assert!(matches!(
            events[2],
            CommandEvent::Progress {
                scope: CommandScope::Document(DocumentId(1)),
                event: ProgressEvent::DocumentFailed { .. },
            }
        ));
        assert!(!events.iter().any(|event| matches!(
            event,
            CommandEvent::Progress {
                scope: CommandScope::Document(DocumentId(2)),
                ..
            }
        )));
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
                no_formula: None,
                no_table: None,
                no_image_analysis: None,
                document_limits: crate::DocumentLimitPolicy::defaults(),
            },
            OfficeWorkers::with_executable(std::env::current_exe().unwrap()),
            super::super::Environment::process(),
            super::super::RunOverrides::default(),
            super::super::service::resolve_service(
                &(|name| std::env::var_os(name)),
                &super::super::service::ServiceOverrides::default(),
                crate::DocumentLimitPolicy::defaults(),
            )
            .unwrap(),
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
