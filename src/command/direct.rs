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
    collections::HashSet,
    ffi::OsString,
    io::{Read, Write},
    panic::{AssertUnwindSafe, catch_unwind},
    path::{Component, Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
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
    let size = metadata.len();
    let label = path.display().to_string();
    if size > max_input_bytes {
        return Err(err(format!(
            "input \"{label}\" is {size} bytes; exceeds configured input limit of {max_input_bytes} bytes; raise with --max-input-bytes or MINERU_MAX_INPUT_BYTES"
        )));
    }
    if size > cap as u64 {
        return Err(err(format!(
            "input \"{label}\" is {size} bytes; exceeds resident preparation limit of {cap} bytes; raise with --max-pdf-bytes or MINERU_MAX_PDF_BYTES"
        )));
    }
    let mut data = Vec::with_capacity(cap.min(1024 * 1024));
    copy_capped(&mut file, &mut data, cap, max_input_bytes, &label)?;
    Ok(Snapshot { bytes: data.into() })
}

fn copy_capped(
    input: &mut impl Read,
    output: &mut Vec<u8>,
    cap: usize,
    source_cap: u64,
    label: &str,
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
                "input \"{label}\" is {total} bytes; exceeds configured input limit of {source_cap} bytes; raise with --max-input-bytes or MINERU_MAX_INPUT_BYTES"
            )));
        }
        if output.len().saturating_add(read) > cap {
            return Err(err(format!(
                "input \"{label}\" is {total} bytes; exceeds maximum input size of {cap} bytes; raise with --max-pdf-bytes or MINERU_MAX_PDF_BYTES"
            )));
        }
        output.extend_from_slice(&buffer[..read]);
    }
}

/// Returns the message for a document whose on-disk length trips any preparation limit, in the
/// same wording `snapshot` uses. The batch preflight is advisory only: `snapshot` stays the final
/// enforcer (TOCTOU-safe, no-follow open preserved), so the two must agree on the wording.
fn preflight_limit_message(
    label: &str,
    len: u64,
    kind: DocumentKind,
    max_input_bytes: u64,
    max_pdf_bytes: usize,
    ooxml_archive_bytes: u64,
    office_input_bytes: usize,
) -> Option<String> {
    if len > max_input_bytes {
        return Some(format!(
            "input \"{label}\" is {len} bytes; exceeds configured input limit of {max_input_bytes} bytes; raise with --max-input-bytes or MINERU_MAX_INPUT_BYTES"
        ));
    }
    if len > max_pdf_bytes as u64 {
        return Some(format!(
            "input \"{label}\" is {len} bytes; exceeds resident preparation limit of {max_pdf_bytes} bytes; raise with --max-pdf-bytes or MINERU_MAX_PDF_BYTES"
        ));
    }
    // The resident cap (`max_pdf_bytes`, checked above) applies to every kind: snapshot enforces
    // it for legacy and OOXML inputs too, with the PDF-named wording, so preflight announces it
    // for them as well. The OOXML archive limit below is OOXML-package-specific; legacy's
    // container-size gate here is the office input limit after it (the helper enforces it
    // TOCTOU-safe on the raw bytes it reads, so this stays advisory).
    if kind.is_office() && len > ooxml_archive_bytes {
        return Some(format!(
            "input \"{label}\" is {len} bytes; exceeds OOXML archive limit of {ooxml_archive_bytes} bytes; raise with --ooxml-archive-bytes or MINERU_OOXML_ARCHIVE_BYTES"
        ));
    }
    if (kind.is_office() || kind.is_legacy_office()) && len > office_input_bytes as u64 {
        return Some(format!(
            "input \"{label}\" is {len} bytes; exceeds office conversion input limit of {office_input_bytes} bytes; raise with --office-input-bytes or MINERU_OFFICE_INPUT_BYTES"
        ));
    }
    None
}

/// Feature gates for the office lanes. A build without the owning feature must reject the kind at
/// plan time instead of spawning the helper, which only knows the formats compiled into it and
/// would fail with a generic "invalid format" (or a misleading "unavailable" when the helper
/// binary is absent).
fn feature_unavailable_message(kind: DocumentKind) -> Option<&'static str> {
    if kind.is_legacy_office() && !cfg!(feature = "legacy-office") {
        Some("legacy office conversion is unavailable (build with --features legacy-office)")
    } else if kind.is_office() && !cfg!(feature = "office") {
        Some("office conversion is unavailable")
    } else {
        None
    }
}

/// Joins per-document failures into the batch summary: count, then the first 16 details.
fn format_failures(failures: &[(String, String)]) -> String {
    let details = failures
        .iter()
        .take(16)
        .map(|(stem, message)| format!("{stem}: {message}"))
        .collect::<Vec<_>>()
        .join("; ");
    let suffix = if failures.len() > 16 { "; ..." } else { "" };
    format!("{} document(s) failed: {details}{suffix}", failures.len())
}

/// The legacy-family lane: helper text extraction into `{root}/{stem}/office/{stem}.md`.
/// Image references in the markdown stay dangling — no image assets are extracted.
async fn extract_text_and_write(
    bytes: bytes::Bytes,
    kind: DocumentKind,
    root: &Path,
    stem: &str,
    office_workers: &OfficeWorkers,
    remaining: Duration,
    task_events: &Option<ProgressCallback>,
) -> Result<(), DirectError> {
    let format = legacy_format_name(kind)?;
    let (text, warning) = office_workers
        .convert_text_with_warning(format, bytes, remaining)
        .await
        .map_err(|e| err(e.to_string()))?;
    if let Some(message) = warning {
        emit_event(
            task_events,
            ProgressEvent::OfficeWarning {
                document: stem.to_owned(),
                message,
            },
        );
    }
    write_legacy_text(root, stem, &text)?;
    Ok(())
}

fn legacy_format_name(kind: DocumentKind) -> Result<&'static str, DirectError> {
    Ok(match kind {
        DocumentKind::Doc => "doc",
        DocumentKind::Ppt => "ppt",
        DocumentKind::Xls => "xls",
        DocumentKind::Odt => "odt",
        DocumentKind::Rtf => "rtf",
        DocumentKind::Epub => "epub",
        DocumentKind::Ods => "ods",
        DocumentKind::Odp => "odp",
        DocumentKind::Csv => "csv",
        _ => return Err(err("legacy office kind required")),
    })
}

/// Opens or creates `{root}/{stem}/office` with no-follow directory walks and writes
/// `{stem}.md` with the symlink-free open the snapshot path uses for reads.
fn write_legacy_text(root: &Path, stem: &str, text: &[u8]) -> Result<(), DirectError> {
    let (anchor, mut names) = anchor_and_names(root)?;
    names.extend([stem.into(), "office".into()]);
    let mut current = Dir::open_ambient_dir(anchor, ambient_authority())?;
    for name in &names {
        match current.symlink_metadata(name) {
            Ok(meta) => {
                if meta.file_type().is_symlink() || !meta.is_dir() {
                    return Err(err("output path component is not a directory"));
                }
                current = current.open_dir_nofollow(name)?;
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                current
                    .create_dir(name)
                    .map_err(|_| err("output directory creation failed"))?;
                current = current.open_dir_nofollow(name)?;
            }
            Err(e) => return Err(e.into()),
        }
    }
    let mut options = OpenOptions::new();
    options
        .write(true)
        .create(true)
        .truncate(true)
        .follow(FollowSymlinks::No);
    let mut file = current
        .open_with(format!("{stem}.md"), &options)
        .map_err(|_| err("output file creation failed"))?;
    file.write_all(text)
        .map_err(|_| err("output write failed"))?;
    file.flush().map_err(|_| err("output write failed"))?;
    Ok(())
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
            let target = if kind.is_office() || kind.is_legacy_office() {
                "office"
            } else {
                "vlm"
            };
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
    // Advisory batch preflight: announce every document that will be rejected before any parsing
    // starts — either by a preparation limit or by a missing office feature. `snapshot` remains
    // the final enforcer for limits; doomed documents are skipped in the main loop below so the
    // rest of the batch still runs.
    let mut failures: Vec<(String, String)> = Vec::new();
    let mut doomed: HashSet<PathBuf> = HashSet::new();
    for (candidate_id, path, kind, stem) in &candidates {
        let Ok(metadata) = std::fs::symlink_metadata(path) else {
            continue;
        };
        let Some(message) = feature_unavailable_message(*kind)
            .map(str::to_owned)
            .or_else(|| {
                preflight_limit_message(
                    &path.display().to_string(),
                    metadata.len(),
                    *kind,
                    options.document_limits.max_input_bytes,
                    route.max_pdf_bytes,
                    service.ooxml.archive_bytes,
                    service.office.input_bytes,
                )
            })
        else {
            continue;
        };
        let task_events = document_events(&command_events, &events, *candidate_id);
        emit_event(
            &task_events,
            ProgressEvent::DocumentFailed {
                document: stem.clone(),
                message: message.clone(),
            },
        );
        failures.push((stem.clone(), message));
        doomed.insert(path.clone());
    }
    if !doomed.is_empty() && doomed.len() == candidates.len() {
        return Err(err(format_failures(&failures)));
    }
    let http = config(options, env, resolved.http)?;
    let page_concurrency = crate::official_route::OfficialPageConcurrency::new(
        resolved.page_concurrency,
        route.processing_window_size,
    )
    .map_err(|error| err(error.to_string()))?;
    // Lazy VLM connection: a batch whose *surviving* candidates are all legacy formats never
    // touches a VLM server, so no connection is attempted (`mineru -i old.doc -o out` works
    // offline). Preflight-doomed candidates are skipped before the main loop and never reach
    // the client, so they must not count toward the decision either.
    let all_legacy = candidates
        .iter()
        .filter(|(_, path, _, _)| !doomed.contains(path))
        .all(|(_, _, kind, _)| kind.is_legacy_office());
    let client = if all_legacy {
        None
    } else {
        Some(
            MinerUVlmClient::connect(
                http,
                MinerUVlmConfig {
                    concurrency_model: resolved.concurrency_model,
                    ..Default::default()
                },
            )
            .await?,
        )
    };
    for (candidate_id, path, kind, stem) in &candidates {
        if doomed.contains(path) {
            continue;
        }
        let task_events = document_events(&command_events, &events, *candidate_id);
        emit_event(
            &task_events,
            ProgressEvent::DocumentStarted {
                document: stem.clone(),
            },
        );
        let cleanup_warning = cleanup_warning_callback(&warnings);
        let result: Result<(), DirectError> = async {
            let deadline = Instant::now()
                .checked_add(route.total_deadline)
                .ok_or_else(|| err("input deadline overflow"))?;
            let snapshot = snapshot(
                path,
                route.max_pdf_bytes,
                options.document_limits.max_input_bytes,
            )?;
            let bytes = snapshot.bytes;
            let target = if kind.is_office() || kind.is_legacy_office() {
                "office"
            } else {
                "vlm"
            };
            let root = output_chain(&output, stem, input_dir.as_ref(), target)?;
            let remaining = deadline
                .checked_duration_since(Instant::now())
                .filter(|d| !d.is_zero())
                .ok_or_else(|| err("input deadline expired"))?;
            if kind.is_legacy_office() {
                extract_text_and_write(
                    bytes,
                    *kind,
                    &root,
                    stem,
                    office_workers,
                    remaining,
                    &task_events,
                )
                .await?;
                emit_event(
                    &task_events,
                    ProgressEvent::DocumentPrepared {
                        document: stem.clone(),
                    },
                );
                return Ok(());
            }
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
            // Invariant: a non-legacy candidate reaching this loop implies the VLM client was
            // connected. `all_legacy` is computed over exactly the loop's surviving candidates
            // (the same doomed filter the loop applies), so do not skip candidates elsewhere.
            client
                .as_ref()
                .expect("a non-legacy candidate implies a connected VLM client")
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
                .map_err(|e| -> DirectError { Box::new(e) })?;
            Ok(())
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
            failures.push((stem.clone(), e.to_string()));
            continue;
        }
        emit_event(
            &task_events,
            ProgressEvent::DocumentCompleted {
                document: stem.clone(),
            },
        );
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(err(format_failures(&failures)))
    }
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
    fn snapshot_uses_an_overflow_probe_and_names_the_file_and_knob() {
        let temp = tempfile::tempdir().unwrap();
        let pdf = temp.path().join("large.pdf");
        std::fs::write(&pdf, b"12345").unwrap();
        // Over the configured input limit (checked first) names the input knob and the file.
        let max_input = snapshot_pdf(&pdf, 10, 4).unwrap_err().to_string();
        assert!(max_input.contains("large.pdf"), "{max_input}");
        assert!(max_input.contains("configured input limit"), "{max_input}");
        assert!(max_input.contains("--max-input-bytes"), "{max_input}");
        assert!(max_input.contains("MINERU_MAX_INPUT_BYTES"), "{max_input}");
        // Over the resident preparation cap names the resident knob and the file.
        let resident = snapshot_pdf(&pdf, 4, 10).unwrap_err().to_string();
        assert!(resident.contains("large.pdf"), "{resident}");
        assert!(
            resident.contains("resident preparation limit"),
            "{resident}"
        );
        assert!(resident.contains("--max-pdf-bytes"), "{resident}");
        assert!(resident.contains("MINERU_MAX_PDF_BYTES"), "{resident}");
        assert_eq!(snapshot_pdf(&pdf, 5, 5).unwrap().bytes.as_ref(), b"12345");
    }

    #[test]
    fn preflight_names_the_tripped_limit_and_respects_kind() {
        let plain = |len| preflight_limit_message("big", len, DocumentKind::Pdf, 100, 10, 100, 100);
        assert!(plain(101).unwrap().contains("--max-input-bytes"));
        let resident = plain(11).unwrap();
        assert!(resident.contains("--max-pdf-bytes"), "{resident}");
        assert!(
            resident.contains("resident preparation limit"),
            "{resident}"
        );
        assert!(plain(10).is_none());

        let office =
            |len| preflight_limit_message("big", len, DocumentKind::Docx, 100, 100, 5, 100);
        assert!(office(6).unwrap().contains("--ooxml-archive-bytes"));
        assert!(office(5).is_none());
        let office =
            |len| preflight_limit_message("big", len, DocumentKind::Xlsx, 100, 100, 100, 4);
        assert!(office(5).unwrap().contains("--office-input-bytes"));
        assert!(office(4).is_none());

        // The OOXML archive limit never trips legacy kinds (they are not archives); their gate
        // is the office input limit, reported with its own wording.
        let legacy = |len| preflight_limit_message("big", len, DocumentKind::Doc, 100, 100, 5, 100);
        assert!(
            legacy(6).is_none(),
            "archive limit must not bind legacy kinds"
        );
        let legacy = |len| preflight_limit_message("big", len, DocumentKind::Rtf, 100, 100, 100, 4);
        let message = legacy(5).unwrap();
        assert!(message.contains("--office-input-bytes"), "{message}");
        assert!(!message.contains("OOXML archive"), "{message}");

        // The resident cap applies to every kind, including legacy: preflight announces it (with
        // the PDF-named wording) exactly as snapshot enforces it, so a huge legacy input is never
        // a mid-run surprise.
        let legacy_resident =
            |len| preflight_limit_message("big", len, DocumentKind::Doc, 1000, 100, 1000, 1000);
        let resident_message = legacy_resident(150).unwrap();
        assert!(
            resident_message.contains("--max-pdf-bytes"),
            "{resident_message}"
        );
        assert!(legacy_resident(100).is_none());

        // The office-only limits never trip non-office kinds.
        assert!(preflight_limit_message("big", 50, DocumentKind::Png, 100, 100, 5, 4).is_none());
    }

    #[test]
    fn feature_unavailable_gate_tracks_compiled_features() {
        // The gate is a compile-time constant: a kind is only reported unavailable when the
        // build lacks the feature that owns its lane. This test runs under every feature set.
        assert_eq!(
            feature_unavailable_message(DocumentKind::Doc).is_some(),
            !cfg!(feature = "legacy-office")
        );
        assert_eq!(
            feature_unavailable_message(DocumentKind::Docx).is_some(),
            !cfg!(feature = "office")
        );
        assert!(feature_unavailable_message(DocumentKind::Pdf).is_none());
    }

    #[test]
    fn failure_summary_counts_and_caps_details_at_sixteen() {
        let failures: Vec<(String, String)> = (0..20)
            .map(|i| (format!("doc{i}"), format!("boom {i}")))
            .collect();
        let summary = format_failures(&failures);
        assert!(summary.starts_with("20 document(s) failed: "), "{summary}");
        assert!(summary.contains("doc0: boom 0"), "{summary}");
        assert!(summary.contains("doc15: boom 15"), "{summary}");
        assert!(!summary.contains("doc16"), "{summary}");
        assert!(summary.ends_with("; ..."), "{summary}");
        let one = format_failures(&[("a".into(), "bad".into())]);
        assert_eq!(one, "1 document(s) failed: a: bad");
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
    async fn callback_runner_preflights_oversized_documents_and_continues() {
        use super::super::{CommandEvent, CommandScope, DocumentId};
        use std::sync::Mutex;

        let input = tempfile::tempdir().unwrap();
        let output = tempfile::tempdir().unwrap();
        // `a` exceeds the configured input limit, `b` exceeds the resident cap (below the input
        // limit), and `c` is small enough to run but has invalid content.
        std::fs::write(input.path().join("a.png"), [b'x'; 12]).unwrap();
        std::fs::write(input.path().join("b.png"), [b'y'; 8]).unwrap();
        std::fs::write(input.path().join("c.png"), b"nope").unwrap();
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
        let env = super::super::Environment::from_values(HashMap::from([(
            "MINERU_MAX_PDF_BYTES",
            OsString::from("5"),
        )]));
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
                document_limits: crate::DocumentLimitPolicy::new(10, 10, 10).unwrap(),
            },
            OfficeWorkers::with_executable(std::env::current_exe().unwrap()),
            env,
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
        let summary = result.unwrap_err().to_string();
        assert!(summary.contains("3 document(s) failed"), "{summary}");
        assert!(summary.contains("--max-input-bytes"), "{summary}");
        assert!(summary.contains("--max-pdf-bytes"), "{summary}");
        assert!(summary.contains("c: invalid image"), "{summary}");
        let events = events.lock().unwrap();
        assert!(matches!(
            events[0],
            CommandEvent::RunPlanned {
                documents: 3,
                api_tasks: 0
            }
        ));
        // Both oversized documents are announced up front, before any document starts.
        assert!(matches!(
            events[1],
            CommandEvent::Progress {
                scope: CommandScope::Document(DocumentId(1)),
                event: ProgressEvent::DocumentFailed { .. },
            }
        ));
        assert!(matches!(
            events[2],
            CommandEvent::Progress {
                scope: CommandScope::Document(DocumentId(2)),
                event: ProgressEvent::DocumentFailed { .. },
            }
        ));
        // The in-limit document still runs and fails on its own; the batch did not abort.
        assert!(matches!(
            events[3],
            CommandEvent::Progress {
                scope: CommandScope::Document(DocumentId(3)),
                event: ProgressEvent::DocumentStarted { .. },
            }
        ));
        assert!(matches!(
            events[4],
            CommandEvent::Progress {
                scope: CommandScope::Document(DocumentId(3)),
                event: ProgressEvent::DocumentFailed { .. },
            }
        ));
        // The preflight-skipped documents never started.
        assert!(!events.iter().any(|event| matches!(
            event,
            CommandEvent::Progress {
                scope: CommandScope::Document(DocumentId(1) | DocumentId(2)),
                event: ProgressEvent::DocumentStarted { .. },
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
        // The second document fails but no longer aborts the batch; the run summarizes instead.
        let summary = result.unwrap_err().to_string();
        assert_eq!(
            summary, "1 document(s) failed: b: invalid image",
            "{summary}"
        );
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
