//! Shared direct VLM runner for the canonical command.
use crate::{
    ConcurrencyModel, MinerUVlmClient, MinerUVlmConfig, OfficeWorkers, OfficialPdfOptions,
    ProgressCallback, ProgressEvent, RasterWorkers, VlmHeader, VlmHttpConfig, canonical_stem,
    input_prepare::{DocumentKind, prepare_with_warning_and_ooxml},
    official_worker::{
        OfficialPersistentWorker, OfficialRequest, OfficialSessionConfig, OfficialWorker,
    },
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
    io::Read,
    panic::{AssertUnwindSafe, catch_unwind},
    path::{Component, Path, PathBuf},
    sync::Arc,
    time::Instant,
};
#[cfg(feature = "legacy-office")]
use std::{
    io::Write,
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
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
    pub local_backend: bool,
    pub method: String,
    pub lang: String,
    pub base_url: Option<String>,
    pub server_option_label: &'static str,
    pub model: Option<String>,
    pub api_key: Option<String>,
    pub official_hybrid: bool,
    /// An explicit CLI/environment override; `None` selects after batch preflight.
    pub official_worker_mode: Option<super::OfficialWorkerMode>,
    pub effort: String,
    pub model_stack: String,
    pub model_stack_explicit: bool,
    pub official_python: Option<PathBuf>,
    pub official_model_dir: Option<PathBuf>,
    pub official_config: Option<PathBuf>,
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
        Some(
            "legacy office conversion is unavailable (build with --features legacy-office, or first convert the file with Microsoft Office or LibreOffice to DOCX, XLSX, or PPTX)",
        )
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

#[cfg(feature = "legacy-office")]
async fn extract_text_and_write_local(
    bytes: bytes::Bytes,
    kind: DocumentKind,
    root: &Path,
    stem: &str,
    office_workers: &OfficeWorkers,
    remaining: Duration,
) -> Result<(), DirectError> {
    let text = office_workers
        .convert_text(kind.suffix(), bytes, remaining)
        .await
        .map_err(|error| err(error.to_string()))?;
    write_legacy_text(root, stem, &text)
}

#[cfg(feature = "legacy-office")]
fn native_output_bytes(
    route: &OfficialPdfOptions,
    service: &super::service::ResolvedService,
    document_limits: crate::DocumentLimitPolicy,
) -> usize {
    route
        .max_staged_text_bytes
        .min(service.office.output_bytes)
        .min(usize::try_from(document_limits.max_output_bytes).unwrap_or(usize::MAX))
}

#[cfg(feature = "legacy-office")]
static TEXT_TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

#[cfg(feature = "legacy-office")]
fn create_text_temp_file(current: &Dir) -> Result<(OsString, cap_std::fs::File), DirectError> {
    for _ in 0..32 {
        let name = OsString::from(format!(
            ".mineru-text-{}-{}",
            std::process::id(),
            TEXT_TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let mut options = OpenOptions::new();
        options
            .write(true)
            .create_new(true)
            .follow(FollowSymlinks::No);
        #[cfg(windows)]
        {
            use cap_std::fs::OpenOptionsExt;
            use windows_sys::Win32::{Foundation::GENERIC_WRITE, Storage::FileSystem::DELETE};
            options.access_mode(GENERIC_WRITE | DELETE);
        }
        match current.open_with(&name, &options) {
            Ok(file) => return Ok((name, file)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(_) => return Err(err("output file creation failed")),
        }
    }
    Err(err("output file creation failed"))
}

#[cfg(all(feature = "legacy-office", windows))]
fn replace_text_temp_file(
    current: &Dir,
    file: &cap_std::fs::File,
    target: &std::ffi::OsStr,
) -> std::io::Result<()> {
    use std::{
        mem::size_of,
        os::windows::{ffi::OsStrExt, io::AsRawHandle},
        ptr::copy_nonoverlapping,
    };
    use windows_sys::Win32::{
        Foundation::HANDLE,
        Storage::FileSystem::{FILE_RENAME_INFO, FileRenameInfo, SetFileInformationByHandle},
    };

    let target: Vec<u16> = target.encode_wide().collect();
    let name_bytes = target
        .len()
        .checked_mul(size_of::<u16>())
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "target too long"))?;
    let name_offset = std::mem::offset_of!(FILE_RENAME_INFO, FileName);
    let size = name_offset
        .checked_add(name_bytes)
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "target too long"))?;
    let size_u32 = u32::try_from(size)
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "target too long"))?;
    let mut info = vec![0u64; size.div_ceil(size_of::<u64>())];

    // SAFETY: `info` is aligned for FILE_RENAME_INFO and large enough for its variable name.
    let info = info.as_mut_ptr().cast::<FILE_RENAME_INFO>();
    unsafe {
        (*info).Anonymous.ReplaceIfExists = true;
        (*info).RootDirectory = current.as_raw_handle() as HANDLE;
        (*info).FileNameLength = u32::try_from(name_bytes).map_err(|_| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "target too long")
        })?;
        copy_nonoverlapping(target.as_ptr(), (*info).FileName.as_mut_ptr(), target.len());
        if SetFileInformationByHandle(
            file.as_raw_handle() as HANDLE,
            FileRenameInfo,
            info.cast(),
            size_u32,
        ) == 0
        {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(())
        }
    }
}

/// Opens or creates `{root}/{stem}/office` with no-follow directory walks and atomically replaces
/// `{stem}.md` with the symlink-free descriptor-relative publication used for local text output.
#[cfg(feature = "legacy-office")]
fn write_text_profile(
    root: &Path,
    stem: &str,
    profile: &str,
    text: &[u8],
) -> Result<(), DirectError> {
    let (anchor, mut names) = anchor_and_names(root)?;
    names.extend([stem.into(), profile.into()]);
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
    let target = OsString::from(format!("{stem}.md"));
    match current.symlink_metadata(&target) {
        Ok(meta) if meta.file_type().is_symlink() || !meta.is_file() => {
            return Err(err("output file creation failed"));
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(_) => return Err(err("output file creation failed")),
    }

    let (temporary, mut file) = create_text_temp_file(&current)?;
    let write_result = file
        .write_all(text)
        .and_then(|_| file.flush())
        .map_err(|_| err("output write failed"));
    #[cfg(windows)]
    let result = write_result.and_then(|()| {
        replace_text_temp_file(&current, &file, &target)
            .map_err(|_| err("output file replacement failed"))
    });
    #[cfg(windows)]
    drop(file);
    #[cfg(not(windows))]
    let result = write_result.and_then(|()| {
        drop(file);
        current
            .rename(&temporary, &current, &target)
            .map_err(|_| err("output file replacement failed"))
    });
    if result.is_err() {
        let _ = current.remove_file(&temporary);
    }
    result
}

#[cfg(feature = "legacy-office")]
fn write_legacy_text(root: &Path, stem: &str, text: &[u8]) -> Result<(), DirectError> {
    write_text_profile(root, stem, "office", text)
}

#[cfg(feature = "legacy-office")]
fn write_native_text(root: &Path, stem: &str, text: &[u8]) -> Result<(), DirectError> {
    write_text_profile(root, stem, "native", text)
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

fn output_target(options: &DirectOptions, kind: DocumentKind) -> &'static str {
    if options.official_hybrid {
        "hybrid-v4"
    } else if options.local_backend && kind == DocumentKind::Pdf {
        "native"
    } else if kind.is_office() || (options.local_backend && kind.is_legacy_office()) {
        "office"
    } else {
        "vlm"
    }
}

fn official_image_or_pdf(kind: DocumentKind) -> bool {
    matches!(
        kind,
        DocumentKind::Pdf
            | DocumentKind::Png
            | DocumentKind::Jpeg
            | DocumentKind::Jpg
            | DocumentKind::Jp2
            | DocumentKind::Webp
            | DocumentKind::Gif
            | DocumentKind::Bmp
            | DocumentKind::Tiff
    )
}

fn validate_official_hybrid_input(
    kind: DocumentKind,
    route: &OfficialPdfOptions,
    bytes: &[u8],
) -> Result<(), DirectError> {
    if kind == DocumentKind::Pdf {
        validate_official_page_selection(kind, route, bytes)
    } else {
        crate::input_prepare::preflight_image(bytes, kind, route).map_err(err)
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_official_hybrid_documents(
    options: &DirectOptions,
    route: &OfficialPdfOptions,
    config: OfficialHybridConfig,
    worker_mode: Option<super::OfficialWorkerMode>,
    candidates: &[(usize, PathBuf, DocumentKind, String)],
    doomed: &HashSet<PathBuf>,
    output: &Path,
    command_events: &Option<super::CommandCallback>,
    events: &Option<ProgressCallback>,
    failures: &mut Vec<(String, String)>,
) -> Result<(), DirectError> {
    let mut doomed = doomed.clone();
    for (candidate_id, path, kind, stem) in candidates {
        if doomed.contains(path) {
            continue;
        }
        let preflight = snapshot(
            path,
            route.max_pdf_bytes,
            options.document_limits.max_input_bytes,
        )
        .and_then(|snapshot| validate_official_hybrid_input(*kind, route, &snapshot.bytes));
        if let Err(error) = preflight {
            let message = error.to_string();
            let task_events = document_events(command_events, events, *candidate_id);
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
    }
    if doomed.len() == candidates.len() {
        return Err(err(format_failures(failures)));
    }

    // Automatic selection must use the fully validated runnable set, not just size/feature gates.
    let runnable = candidates
        .iter()
        .filter(|(_, path, _, _)| !doomed.contains(path))
        .count();
    let worker_mode = worker_mode.unwrap_or_else(|| {
        if runnable == 1 {
            super::OfficialWorkerMode::PerDocument
        } else {
            super::OfficialWorkerMode::Persistent
        }
    });
    let persistent_worker = if worker_mode == super::OfficialWorkerMode::Persistent {
        let session = OfficialSessionConfig::new(
            config.model_stack.clone(),
            config.model_dir.clone(),
            config.config.clone(),
            config.api_key.clone(),
            config.model_name.clone(),
        )
        .map_err(err)?;
        Some(OfficialPersistentWorker::new(config.python.clone(), session).map_err(err)?)
    } else {
        None
    };
    for (candidate_id, path, kind, stem) in candidates {
        if doomed.contains(path) {
            continue;
        }
        let task_events = document_events(command_events, events, *candidate_id);
        emit_event(
            &task_events,
            ProgressEvent::DocumentStarted {
                document: stem.clone(),
            },
        );
        let result: Result<(), DirectError> = async {
            let deadline = Instant::now()
                .checked_add(route.total_deadline)
                .ok_or_else(|| err("input deadline overflow"))?;
            let bytes = snapshot(
                path,
                route.max_pdf_bytes,
                options.document_limits.max_input_bytes,
            )?
            .bytes;
            validate_official_hybrid_input(*kind, route, &bytes)?;
            emit_event(
                &task_events,
                ProgressEvent::DocumentPrepared {
                    document: stem.clone(),
                },
            );
            let page_range = if *kind == DocumentKind::Pdf
                && (route.start_page != 0 || route.end_page.is_some())
            {
                Some(official_page_range(*kind, route)?.expect("selected official page range"))
            } else {
                None
            };
            let request = OfficialRequest::new(
                "hybrid-http-client".into(),
                options.effort.clone(),
                config.server_url.clone(),
                options.method.clone(),
                options.lang.clone(),
                route.image_analysis,
                page_range,
                config.model_stack.clone(),
                config.model_dir.clone(),
                config.config.clone(),
                config.api_key.clone(),
                config.model_name.clone(),
                options.document_limits.max_output_bytes,
            );
            let bundle = if let Some(worker) = persistent_worker.as_ref() {
                worker.run(&bytes, kind.suffix(), request, deadline).await?
            } else {
                let worker = OfficialWorker::new(config.python.clone()).map_err(err)?;
                worker.run(&bytes, kind.suffix(), request, deadline).await?
            };
            crate::hybrid_v4_output::validate_and_publish(
                bundle.path(),
                output,
                stem,
                options.document_limits.max_output_bytes,
            )?;
            Ok(())
        }
        .await;
        if let Err(error) = result {
            let message = error.to_string();
            emit_event(
                &task_events,
                ProgressEvent::DocumentFailed {
                    document: stem.clone(),
                    message: message.clone(),
                },
            );
            failures.push((stem.clone(), message));
            continue;
        }
        emit_event(
            &task_events,
            ProgressEvent::DocumentCompleted {
                document: stem.clone(),
            },
        );
    }
    if let Some(worker) = persistent_worker.as_ref() {
        worker.shutdown().await.map_err(err)?;
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(err(format_failures(failures)))
    }
}

fn validate_official_page_selection(
    kind: DocumentKind,
    route: &OfficialPdfOptions,
    bytes: &[u8],
) -> Result<(), DirectError> {
    if kind != DocumentKind::Pdf {
        return Ok(());
    }
    let document = lopdf::Document::load_mem(bytes).map_err(|_| err("invalid official PDF"))?;
    if document.is_encrypted() || document.was_encrypted() {
        return Err(err("encrypted PDFs are unsupported"));
    }
    let pages = document.get_pages().len();
    if pages == 0 {
        return Err(err("official PDF has no pages"));
    }
    let selected = match route.end_page {
        Some(end) => {
            if route.start_page > end {
                return Err(err("invalid official page range"));
            }
            if route.start_page >= pages || end >= pages {
                return Err(err(format!(
                    "official page range {}~{} is outside PDF with {pages} page(s)",
                    route.start_page, end
                )));
            }
            end.checked_sub(route.start_page)
                .and_then(|count| count.checked_add(1))
                .ok_or_else(|| err("invalid official page range"))?
        }
        None => {
            if route.start_page >= pages {
                return Err(err(format!(
                    "official page start {} is outside PDF with {pages} page(s)",
                    route.start_page
                )));
            }
            pages - route.start_page
        }
    };
    if selected > route.max_pages {
        return Err(Box::new(crate::VlmError::LimitExceeded {
            resource: "pages",
            limit: route.max_pages as u64,
            actual: selected as u64,
        }));
    }
    Ok(())
}

fn official_page_range(
    kind: DocumentKind,
    route: &OfficialPdfOptions,
) -> Result<Option<String>, DirectError> {
    if kind != DocumentKind::Pdf || (route.start_page == 0 && route.end_page.is_none()) {
        return Ok(None);
    }
    let start = route
        .start_page
        .checked_add(1)
        .ok_or_else(|| err("official page start exceeds usize"))?;
    let Some(end) = route.end_page else {
        return Ok(Some(format!("{start}~-1")));
    };
    let end = end
        .checked_add(1)
        .ok_or_else(|| err("official page end exceeds usize"))?;
    if start == end {
        Ok(Some(start.to_string()))
    } else {
        Ok(Some(format!("{start}~{end}")))
    }
}

#[cfg(feature = "legacy-office")]
fn legacy_message_once(message: String, recommendation_emitted: &mut bool) -> String {
    let recommendation = crate::legacy_office::LEGACY_PDF_RECOMMENDATION;
    if message.contains(recommendation) {
        if !*recommendation_emitted {
            *recommendation_emitted = true;
            return message;
        }
        return message
            .replace(&format!("; {recommendation}."), "")
            .replace(&format!("; {recommendation}"), "");
    }
    if !*recommendation_emitted {
        *recommendation_emitted = true;
        format!("{message}; {recommendation}")
    } else {
        message
    }
}

fn config_inputs(
    options: &DirectOptions,
    env: &super::Environment,
) -> Result<(Option<url::Url>, Option<String>), DirectError> {
    let server = clean(options.base_url.clone(), options.server_option_label)?
        .map(|server| server.parse())
        .transpose()?;
    let key = clean(
        options
            .api_key
            .clone()
            .or_else(|| env.string("MINERU_VL_API_KEY")),
        "--api-key",
    )?;
    Ok((server, key))
}

struct OfficialHybridConfig {
    python: Option<PathBuf>,
    model_stack: String,
    model_dir: Option<PathBuf>,
    config: Option<PathBuf>,
    server_url: Option<String>,
    api_key: Option<String>,
    model_name: Option<String>,
}

fn official_text(env: &super::Environment, name: &str) -> Result<Option<String>, DirectError> {
    let Some(value) = env.os(name) else {
        return Ok(None);
    };
    value
        .into_string()
        .map(Some)
        .map_err(|_| err(format!("{name} must be valid UTF-8")))
}

fn official_path(
    explicit: Option<&Path>,
    environment: Option<String>,
    name: &str,
) -> Result<Option<PathBuf>, DirectError> {
    let path = explicit
        .map(Path::to_owned)
        .or_else(|| environment.map(PathBuf::from));
    let Some(path) = path else { return Ok(None) };
    if !path.is_absolute() || path.to_string_lossy().chars().any(char::is_control) {
        return Err(err(format!(
            "{name} path must be absolute and contain no controls"
        )));
    }
    Ok(Some(path))
}

fn resolve_official_hybrid(
    options: &DirectOptions,
    env: &super::Environment,
) -> Result<OfficialHybridConfig, DirectError> {
    let environment_stack = official_text(env, "MINERU_MODEL_STACK")?;
    let model_stack = if options.model_stack_explicit || options.model_stack != "auto" {
        options.model_stack.clone()
    } else {
        environment_stack.unwrap_or_else(|| "auto".into())
    };
    if !matches!(model_stack.as_str(), "auto" | "light" | "full") {
        return Err(err("model_stack must be auto, light, or full"));
    }
    let python = official_path(
        options.official_python.as_deref(),
        official_text(env, "MINERU_OFFICIAL_PYTHON")?,
        "official Python executable",
    )?;
    let model_dir = official_path(
        options.official_model_dir.as_deref(),
        official_text(env, "MINERU_MODEL_BASE_DIR")?,
        "official model directory",
    )?;
    let config = official_path(
        options.official_config.as_deref(),
        official_text(env, "MINERU_CONFIG")?,
        "official config",
    )?;
    let server_url = clean(
        options
            .base_url
            .clone()
            .or(official_text(env, "MINERU_VL_SERVER")?),
        "--url",
    )?;
    if matches!(options.effort.as_str(), "high" | "xhigh") {
        super::validate_hybrid_server_url(server_url.as_deref()).map_err(err)?;
    }
    Ok(OfficialHybridConfig {
        python,
        model_stack,
        model_dir,
        config,
        server_url,
        api_key: clean(
            options
                .api_key
                .clone()
                .or(official_text(env, "MINERU_VL_API_KEY")?),
            "--api-key",
        )?,
        model_name: clean(
            official_text(env, "MINERU_VL_MODEL_NAME")?,
            "MINERU_VL_MODEL_NAME",
        )?,
    })
}

fn config(
    options: &DirectOptions,
    env: &super::Environment,
    mut http: VlmHttpConfig,
) -> Result<VlmHttpConfig, DirectError> {
    let (server, key) = config_inputs(options, env)?;
    let model = clean(options.model.clone(), "--model")?;
    if let Some(server) = server {
        http.server_url = Some(server);
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

async fn ensure_vlm_client(
    client: &mut Option<MinerUVlmClient>,
    client_http: &mut Option<VlmHttpConfig>,
    options: &DirectOptions,
    env: &super::Environment,
    concurrency_model: ConcurrencyModel,
    temperature_retry: bool,
) -> Result<(), DirectError> {
    if client.is_some() {
        return Ok(());
    }
    let http = config(
        options,
        env,
        client_http
            .take()
            .ok_or_else(|| err("VLM client configuration was already consumed"))?,
    )?;
    let connected = MinerUVlmClient::connect_with_temperature_retry(
        http,
        MinerUVlmConfig {
            concurrency_model,
            ..Default::default()
        },
        temperature_retry,
    )
    .await
    .map_err(|error| -> DirectError { Box::new(error) })?;
    *client = Some(connected);
    Ok(())
}

/// Resolves the strict core policy (compiled default -> frozen environment -> CLI). The formula,
/// table, and image-analysis booleans resolve through `CoreOverrides`; the legacy `--no-*` flags
/// force their values only when the owning surface actually provided them.
fn resolved_route(
    options: &DirectOptions,
    env: &super::Environment,
    overrides: &super::RunOverrides,
) -> Result<super::env::ResolvedCore, DirectError> {
    let mut resolved = if options.local_backend {
        super::env::resolve_core(
            |name| {
                (!super::local_vlm_environment_name(name))
                    .then(|| env.os(name))
                    .flatten()
            },
            &super::local_core_overrides(&overrides.core),
        )
    } else {
        super::env::resolve_core(|name| env.os(name), &overrides.core)
    }
    .map_err(err)?;
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
    let temperature_retry = if options.local_backend {
        false
    } else {
        super::env::resolve_temperature_retry(&|name| env.os(name), &overrides.core).map_err(err)?
    };
    apply_document_limits(
        &mut resolved.route,
        options.document_limits,
        &overrides.core,
    )?;
    let totals =
        crate::document_limits::OfficialDocumentTotals::from_policy(options.document_limits);
    let route = resolved.route;
    let official_config = options
        .official_hybrid
        .then(|| resolve_official_hybrid(options, env))
        .transpose()?;
    let input = absolute(&options.input)?;
    let output = absolute(&options.output)?;
    let (_, inputs, skipped) = discover_inputs(&input)?;
    if options.official_hybrid
        && let Some((path, kind)) = inputs
            .iter()
            .find(|(_, kind)| !official_image_or_pdf(*kind))
    {
        return Err(err(format!(
            "direct Hybrid accepts only PDF and official image inputs; {} ({}) is unsupported",
            path.display(),
            kind.suffix()
        )));
    }
    if options.local_backend
        && let Some((path, kind)) = inputs
            .iter()
            .find(|(_, kind)| !kind.is_legacy_office() && *kind != DocumentKind::Pdf)
    {
        return Err(err(format!(
            "backend=local supports AnyDoc legacy Office formats and native text PDFs; input \"{}\" ({}) is unsupported (images and OOXML inputs require a VLM backend)",
            path.display(),
            kind.suffix()
        )));
    }
    if options.local_backend
        && inputs.iter().any(|(_, kind)| *kind == DocumentKind::Pdf)
        && (route.start_page != 0 || route.end_page.is_some())
    {
        return Err(err(
            "backend=local native PDF Markdown does not support --start/--end page selection",
        ));
    }
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
            output_chain(
                &output,
                &stem,
                input_dir.as_ref(),
                output_target(options, kind),
            )?;
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
    #[cfg(feature = "legacy-office")]
    let mut legacy_recommendation_emitted = false;
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
        #[cfg(feature = "legacy-office")]
        let message = if kind.is_legacy_office()
            && message.contains(crate::legacy_office::LEGACY_PDF_RECOMMENDATION)
        {
            legacy_message_once(message, &mut legacy_recommendation_emitted)
        } else {
            message
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
    if options.official_hybrid {
        return run_official_hybrid_documents(
            options,
            &route,
            official_config.expect("official Hybrid configuration"),
            options.official_worker_mode,
            &candidates,
            &doomed,
            &output,
            &command_events,
            &events,
            &mut failures,
        )
        .await;
    }
    let page_concurrency = crate::official_route::OfficialPageConcurrency::new(
        resolved.page_concurrency,
        route.processing_window_size,
    )
    .map_err(|error| err(error.to_string()))?;
    // The client is lazy so a legacy conversion failure is handled before any VLM discovery or
    // request. Only the client is retained across documents; each prepared document is consumed by
    // the route immediately, keeping resident memory bounded to one.
    let mut client = None;
    let mut client_http = Some(resolved.http);
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
            let root = output_chain(
                &output,
                stem,
                input_dir.as_ref(),
                output_target(options, *kind),
            )?;
            let deadline = Instant::now()
                .checked_add(route.total_deadline)
                .ok_or_else(|| err("input deadline overflow"))?;
            let bytes = snapshot(
                path,
                route.max_pdf_bytes,
                options.document_limits.max_input_bytes,
            )?
            .bytes;
            let remaining = deadline
                .checked_duration_since(Instant::now())
                .filter(|d| !d.is_zero())
                .ok_or_else(|| err("input deadline expired"))?;
            if options.local_backend && *kind == DocumentKind::Pdf {
                #[cfg(feature = "legacy-office")]
                {
                    let markdown = office_workers
                        .convert_native_pdf(
                            bytes,
                            route.max_pages,
                            native_output_bytes(&route, service, options.document_limits),
                            remaining,
                        )
                        .await
                        .map_err(|error| err(error.to_string()))?;
                    write_native_text(&root, stem, &markdown)?;
                    emit_event(
                        &task_events,
                        ProgressEvent::DocumentPrepared {
                            document: stem.clone(),
                        },
                    );
                    return Ok(());
                }
                #[cfg(not(feature = "legacy-office"))]
                return Err(err(
                    "backend=local native PDF Markdown requires the legacy-office feature (AnyDoc PDF support)",
                ));
            }
            if kind.is_legacy_office() {
                if options.local_backend {
                    #[cfg(feature = "legacy-office")]
                    extract_text_and_write_local(
                        bytes,
                        *kind,
                        &root,
                        stem,
                        office_workers,
                        remaining,
                    )
                    .await?;
                    #[cfg(not(feature = "legacy-office"))]
                    return Err(err(
                        "backend=local requires the legacy-office feature for AnyDoc parsing",
                    ));
                    #[cfg(feature = "legacy-office")]
                    {
                        emit_event(
                            &task_events,
                            ProgressEvent::DocumentPrepared {
                                document: stem.clone(),
                            },
                        );
                        return Ok(());
                    }
                } else {
                    #[cfg(feature = "legacy-office")]
                    {
                        let (prepared, warning) =
                            crate::input_prepare::prepare_legacy_with_warning(
                                bytes,
                                *kind,
                                &route,
                                office_workers,
                                remaining,
                            )
                            .await
                            .map_err(err)?;
                        if let Some(message) = warning {
                            emit_event(
                                &task_events,
                                ProgressEvent::OfficeWarning {
                                    document: stem.clone(),
                                    message: legacy_message_once(
                                        message,
                                        &mut legacy_recommendation_emitted,
                                    ),
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
                        route.start_page = 0;
                        route.end_page = None;
                        route.total_deadline = deadline
                            .checked_duration_since(Instant::now())
                            .filter(|d| !d.is_zero())
                            .ok_or_else(|| err("input deadline expired"))?;
                        ensure_vlm_client(
                            &mut client,
                            &mut client_http,
                            options,
                            env,
                            resolved.concurrency_model,
                            temperature_retry,
                        )
                        .await?;
                        client
                            .as_ref()
                            .expect("a non-local legacy candidate implies a connected VLM client")
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
                        return Ok(());
                    }
                    #[cfg(not(feature = "legacy-office"))]
                    return Err(err(
                        "legacy PDF conversion requires the legacy-office feature",
                    ));
                }
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
            ensure_vlm_client(
                &mut client,
                &mut client_http,
                options,
                env,
                resolved.concurrency_model,
                temperature_retry,
            )
            .await?;
            client
                .as_ref()
                .expect("a VLM candidate implies a connected VLM client")
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
            let message = e.to_string();
            #[cfg(feature = "legacy-office")]
            let message = {
                let mut message = message;
                if !options.local_backend
                    && kind.is_legacy_office()
                    && message.starts_with("legacy best-effort PDF conversion failed")
                {
                    message = legacy_message_once(message, &mut legacy_recommendation_emitted);
                }
                message
            };
            // Client discovery/configuration historically failed before document events. Keep
            // that fail-fast behavior, while conversion errors remain document-scoped so later
            // documents can still run.
            if !options.local_backend && client.is_none() && client_http.is_none() {
                return Err(e);
            }
            emit_event(
                &task_events,
                ProgressEvent::DocumentFailed {
                    document: stem.clone(),
                    message: message.clone(),
                },
            );
            failures.push((stem.clone(), message));
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
            local_backend: false,
            method: "auto".into(),
            lang: "ch".into(),
            base_url: None,
            server_option_label: "--url",
            model: None,
            api_key: None,
            official_hybrid: false,
            official_worker_mode: None,
            effort: "medium".into(),
            model_stack: "auto".into(),
            model_stack_explicit: false,
            official_python: None,
            official_model_dir: None,
            official_config: None,
            page_start: None,
            page_end: None,
            no_formula: None,
            no_table: None,
            no_image_analysis: None,
            document_limits: crate::DocumentLimitPolicy::defaults(),
        }
    }

    fn pdf_with_pages(count: usize) -> Vec<u8> {
        use lopdf::{Document, Object, dictionary};

        let mut document = Document::with_version("1.5");
        let pages = document.new_object_id();
        let page_ids: Vec<_> = (0..count)
            .map(|_| {
                let page = document.new_object_id();
                document.objects.insert(
                    page,
                    Object::Dictionary(dictionary! {
                        "Type" => "Page",
                        "Parent" => pages,
                        "MediaBox" => vec![0.into(), 0.into(), 1.into(), 1.into()],
                    }),
                );
                page
            })
            .collect();
        document.objects.insert(
            pages,
            Object::Dictionary(dictionary! {
                "Type" => "Pages",
                "Kids" => page_ids.iter().copied().map(Object::Reference).collect::<Vec<_>>(),
                "Count" => count as i64,
            }),
        );
        let catalog = document.add_object(dictionary! { "Type" => "Catalog", "Pages" => pages });
        document.trailer.set("Root", catalog);
        let mut bytes = Vec::new();
        document.save_to(&mut bytes).unwrap();
        bytes
    }

    #[test]
    fn official_pdf_page_validation_uses_zero_based_inclusive_bounds() {
        let bytes = pdf_with_pages(3);
        let mut route = OfficialPdfOptions::default();

        route.start_page = 0;
        route.end_page = Some(0);
        assert!(validate_official_page_selection(DocumentKind::Pdf, &route, &bytes).is_ok());
        route.start_page = 2;
        route.end_page = Some(2);
        assert!(validate_official_page_selection(DocumentKind::Pdf, &route, &bytes).is_ok());

        route.start_page = 0;
        route.end_page = Some(3);
        assert!(validate_official_page_selection(DocumentKind::Pdf, &route, &bytes).is_err());
        route.start_page = 3;
        route.end_page = None;
        assert!(validate_official_page_selection(DocumentKind::Pdf, &route, &bytes).is_err());

        route.start_page = 2;
        route.end_page = Some(1);
        assert!(validate_official_page_selection(DocumentKind::Pdf, &route, &bytes).is_err());

        route.start_page = 0;
        route.end_page = Some(2);
        route.max_pages = 2;
        let error = validate_official_page_selection(DocumentKind::Pdf, &route, &bytes)
            .unwrap_err()
            .to_string();
        assert!(error.contains("limit exceeded for pages"), "{error}");

        let empty = pdf_with_pages(0);
        route.max_pages = 3;
        assert!(validate_official_page_selection(DocumentKind::Pdf, &route, &empty).is_err());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn official_hybrid_page_rejection_does_not_spawn_python() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().unwrap();
        let pdf = temp.path().join("one.pdf");
        std::fs::write(&pdf, pdf_with_pages(1)).unwrap();
        let marker = temp.path().join("spawned");
        let python = temp.path().join("worker.sh");
        std::fs::write(
            &python,
            format!("#!/bin/sh\nprintf spawned > \"{}\"\n", marker.display()),
        )
        .unwrap();
        let mut permissions = std::fs::metadata(&python).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&python, permissions).unwrap();

        let mut route = OfficialPdfOptions::default();
        route.start_page = 1;
        route.end_page = Some(1);
        let candidates = vec![(1, pdf, DocumentKind::Pdf, "one".into())];
        let mut failures = Vec::new();
        let error = run_official_hybrid_documents(
            &test_options(),
            &route,
            OfficialHybridConfig {
                python: Some(python),
                model_stack: "auto".into(),
                model_dir: None,
                config: None,
                server_url: None,
                api_key: None,
                model_name: None,
            },
            Some(super::super::OfficialWorkerMode::PerDocument),
            &candidates,
            &HashSet::new(),
            temp.path(),
            &None,
            &None,
            &mut failures,
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(error.contains("outside PDF"), "{error}");
        assert_eq!(failures.len(), 1);
        assert!(!marker.exists());
    }

    #[tokio::test]
    async fn official_hybrid_truncated_image_fails_before_worker_creation() {
        use std::io::Cursor;

        let temp = tempfile::tempdir().unwrap();
        let image = temp.path().join("truncated.png");
        let mut bytes = Vec::new();
        image::DynamicImage::new_rgb8(2, 3)
            .write_to(&mut Cursor::new(&mut bytes), image::ImageFormat::Png)
            .unwrap();
        bytes.truncate(bytes.len() / 2);
        std::fs::write(&image, bytes).unwrap();

        let candidates = vec![(1, image, DocumentKind::Png, "truncated".into())];
        let mut failures = Vec::new();
        let error = run_official_hybrid_documents(
            &test_options(),
            &OfficialPdfOptions::default(),
            OfficialHybridConfig {
                python: Some("/definitely/missing/python".into()),
                model_stack: "auto".into(),
                model_dir: None,
                config: None,
                server_url: None,
                api_key: None,
                model_name: None,
            },
            Some(super::super::OfficialWorkerMode::PerDocument),
            &candidates,
            &HashSet::new(),
            temp.path(),
            &None,
            &None,
            &mut failures,
        )
        .await
        .unwrap_err()
        .to_string();
        assert_eq!(error, "1 document(s) failed: truncated: invalid image");
        assert_eq!(failures.len(), 1);
    }

    #[test]
    fn official_page_ranges_use_official_open_ended_syntax() {
        let mut route = OfficialPdfOptions::default();
        assert_eq!(
            official_page_range(DocumentKind::Pdf, &route).unwrap(),
            None
        );

        route.start_page = 2;
        assert_eq!(
            official_page_range(DocumentKind::Pdf, &route).unwrap(),
            Some("3~-1".into())
        );

        route.end_page = Some(4);
        assert_eq!(
            official_page_range(DocumentKind::Pdf, &route).unwrap(),
            Some("3~5".into())
        );
    }

    #[tokio::test]
    async fn official_open_ended_page_limit_rejects_before_worker_dispatch() {
        use lopdf::{Document, Object, dictionary};

        let temp = tempfile::tempdir().unwrap();
        let pdf = temp.path().join("multi.pdf");
        let mut document = Document::with_version("1.5");
        let pages = document.new_object_id();
        let page_ids: Vec<_> = (0..2)
            .map(|_| {
                let page = document.new_object_id();
                document.objects.insert(
                    page,
                    Object::Dictionary(dictionary! {
                        "Type" => "Page",
                        "Parent" => pages,
                        "MediaBox" => vec![0.into(), 0.into(), 1.into(), 1.into()],
                    }),
                );
                page
            })
            .collect();
        document.objects.insert(
            pages,
            Object::Dictionary(dictionary! {
                "Type" => "Pages",
                "Kids" => page_ids.iter().copied().map(Object::Reference).collect::<Vec<_>>(),
                "Count" => 2,
            }),
        );
        let catalog = document.add_object(dictionary! { "Type" => "Catalog", "Pages" => pages });
        document.trailer.set("Root", catalog);
        let mut bytes = Vec::new();
        document.save_to(&mut bytes).unwrap();
        std::fs::write(&pdf, bytes).unwrap();

        let mut route = OfficialPdfOptions::default();
        route.max_pages = 1;
        let candidates = vec![(1, pdf, DocumentKind::Pdf, "multi".into())];
        let mut failures = Vec::new();
        let error = run_official_hybrid_documents(
            &test_options(),
            &route,
            OfficialHybridConfig {
                python: Some("/definitely/missing/python".into()),
                model_stack: "auto".into(),
                model_dir: None,
                config: None,
                server_url: None,
                api_key: None,
                model_name: None,
            },
            Some(super::super::OfficialWorkerMode::PerDocument),
            &candidates,
            &HashSet::new(),
            temp.path(),
            &None,
            &None,
            &mut failures,
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(error.contains("limit exceeded for pages: 2 > 1"), "{error}");
        assert_eq!(failures.len(), 1);
    }

    #[test]
    fn explicit_auto_model_stack_overrides_environment() {
        let env = super::super::Environment::from_values(HashMap::from([(
            "MINERU_MODEL_STACK",
            OsString::from("full"),
        )]));
        let mut options = test_options();
        options.model_stack_explicit = true;
        assert_eq!(
            resolve_official_hybrid(&options, &env).unwrap().model_stack,
            "auto"
        );

        options.model_stack_explicit = false;
        assert_eq!(
            resolve_official_hybrid(&options, &env).unwrap().model_stack,
            "full"
        );
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

    #[cfg(feature = "legacy-office")]
    #[test]
    fn local_text_profile_replaces_markdown_without_leaving_temps() {
        let temp = tempfile::tempdir().unwrap();
        let root = absolute(temp.path()).unwrap();
        write_text_profile(&root, "doc", "office", b"old").unwrap();
        write_text_profile(&root, "doc", "office", b"new").unwrap();
        assert_eq!(
            std::fs::read(root.join("doc/office/doc.md")).unwrap(),
            b"new"
        );
        assert!(
            !std::fs::read_dir(root.join("doc/office"))
                .unwrap()
                .map(Result::unwrap)
                .any(|entry| entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".mineru-text-"))
        );
    }

    #[cfg(all(feature = "legacy-office", windows))]
    #[test]
    fn windows_text_profile_replaces_an_existing_target() {
        let temp = tempfile::tempdir().unwrap();
        let root = absolute(temp.path()).unwrap();
        write_text_profile(&root, "doc", "office", b"old").unwrap();
        write_text_profile(&root, "doc", "office", b"new").unwrap();
        assert_eq!(
            std::fs::read(root.join("doc/office/doc.md")).unwrap(),
            b"new"
        );
    }

    #[cfg(all(feature = "legacy-office", unix))]
    #[test]
    fn local_text_write_failure_preserves_previous_markdown() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().unwrap();
        let root = absolute(temp.path()).unwrap();
        write_text_profile(&root, "doc", "office", b"old").unwrap();
        let profile = root.join("doc/office");
        let mut permissions = std::fs::metadata(&profile).unwrap().permissions();
        permissions.set_mode(0o500);
        std::fs::set_permissions(&profile, permissions).unwrap();
        let result = write_text_profile(&root, "doc", "office", b"new");
        let mut restore = std::fs::metadata(&profile).unwrap().permissions();
        restore.set_mode(0o700);
        std::fs::set_permissions(&profile, restore).unwrap();

        if result.is_err() {
            assert_eq!(std::fs::read(profile.join("doc.md")).unwrap(), b"old");
        } else {
            // Privileged test runners may bypass the directory permission used to force the
            // temporary-file write failure; the successful atomic path is covered above.
            assert_eq!(std::fs::read(profile.join("doc.md")).unwrap(), b"new");
        }
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
                local_backend: false,
                method: "auto".into(),
                lang: "ch".into(),
                base_url: Some("http://127.0.0.1:1".into()),
                server_option_label: "--url",
                model: Some("mock".into()),
                api_key: None,
                official_hybrid: false,
                official_worker_mode: None,
                effort: "medium".into(),
                model_stack: "auto".into(),
                model_stack_explicit: false,
                official_python: None,
                official_model_dir: None,
                official_config: None,
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
                local_backend: false,
                method: "auto".into(),
                lang: "ch".into(),
                base_url: Some(base_url),
                server_option_label: "--url",
                model: Some("mock".into()),
                api_key: None,
                official_hybrid: false,
                official_worker_mode: None,
                effort: "medium".into(),
                model_stack: "auto".into(),
                model_stack_explicit: false,
                official_python: None,
                official_model_dir: None,
                official_config: None,
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
