use crate::error::sanitize_vlm_error_bytes;
use cap_fs_ext::{DirExt, FollowSymlinks, OpenOptionsFollowExt};
use cap_primitives::fs::open_dir_nofollow;
use cap_std::{
    ambient_authority,
    fs::{Dir, OpenOptions},
};
use futures_util::StreamExt;
use reqwest::{Client, Response, StatusCode};
use std::{
    collections::{HashMap, HashSet},
    io::{Read, Write},
    path::{Component, Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};
use tempfile::NamedTempFile;
use tokio::io::AsyncWriteExt;
use zip::{
    ZipArchive,
    read::{ArchiveOffset, Config},
};

use super::zip_scan::{ScanLimits, scan};

const BODY_CAP: usize = 64 * 1024;
static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy, Debug)]
pub(super) struct ArchiveLimits {
    pub(super) max_entries: u64,
    pub(super) max_compressed_bytes: u64,
    pub(super) max_expanded_bytes: u64,
    pub(super) max_entry_bytes: u64,
    pub(super) max_ratio: u64,
}

impl Default for ArchiveLimits {
    fn default() -> Self {
        Self {
            max_entries: 100_000,
            max_compressed_bytes: 8 * 1024 * 1024 * 1024,
            max_expanded_bytes: 32 * 1024 * 1024 * 1024,
            max_entry_bytes: 8 * 1024 * 1024 * 1024,
            max_ratio: 1000,
        }
    }
}

impl ArchiveLimits {
    fn validate(self) -> Result<Self, String> {
        if self.max_entries == 0
            || self.max_compressed_bytes == 0
            || self.max_expanded_bytes == 0
            || self.max_entry_bytes == 0
            || self.max_ratio == 0
        {
            return Err("archive limits must be positive".into());
        }
        self.max_entry_bytes
            .checked_mul(self.max_ratio)
            .ok_or_else(|| "archive limits are invalid".to_string())?;
        Ok(self)
    }
}

#[derive(Debug)]
pub(super) struct DownloadedZip(NamedTempFile);
impl DownloadedZip {
    pub(super) fn path(&self) -> &Path {
        self.0.path()
    }
    pub(super) fn reopen(&self) -> std::io::Result<std::fs::File> {
        self.0.reopen()
    }

    /// Extracts a result archive into an existing output root without following links.
    pub(super) fn extract(&self, destination: &Path, limits: ArchiveLimits) -> Result<(), String> {
        self.extract_impl(destination, limits, CommitFault::default())
    }

    fn extract_impl(
        &self,
        destination: &Path,
        limits: ArchiveLimits,
        fault: CommitFault,
    ) -> Result<(), String> {
        let limits = limits.validate()?;
        let compressed = self
            .0
            .as_file()
            .metadata()
            .map_err(|_| "unable to inspect result archive")?
            .len();
        if compressed > limits.max_compressed_bytes {
            return Err("result archive exceeds compressed size limit".into());
        }
        let scanned = scan(
            &mut self.reopen().map_err(|_| "unable to open result archive")?,
            ScanLimits::production(limits.max_entries),
        )
        .map_err(|_| "invalid result archive")?;
        let mut zip = ZipArchive::with_config(
            Config {
                archive_offset: ArchiveOffset::Known(0),
            },
            self.reopen().map_err(|_| "unable to open result archive")?,
        )
        .map_err(|_| "invalid result archive")?;
        let count = u64::try_from(zip.len()).map_err(|_| "result archive has too many entries")?;
        if zip.offset() != 0
            || count != scanned.count
            || zip.central_directory_start() != scanned.central_start
        {
            return Err("result archive has too many entries".into());
        }

        let mut entries = Vec::with_capacity(zip.len());
        let mut raw_names = HashSet::new();
        let mut kinds = HashMap::new();
        let mut folded_bytes = 0u64;
        let mut expanded = 0u64;
        for index in 0..zip.len() {
            let file = zip.by_index(index).map_err(|_| "invalid result archive")?;
            if !raw_names.insert(file.name_raw().to_vec()) {
                return Err("result archive has duplicate paths".into());
            }
            let path = archive_path(file.name())?;
            let directory = file.is_dir();
            if let Some(mode) = file.unix_mode() {
                let kind = mode & 0o170000;
                if kind != 0 && kind != 0o100000 && kind != 0o040000 {
                    return Err("result archive contains a symlink or special entry".into());
                }
                if directory != (kind == 0o040000) && kind != 0 {
                    return Err("result archive entry type is invalid".into());
                }
            }
            let folded_key = path.to_string_lossy().to_lowercase();
            folded_bytes = folded_bytes
                .checked_add(u64::try_from(folded_key.len()).map_err(|_| "invalid result archive")?)
                .filter(|v| *v <= 32 * 1024 * 1024)
                .ok_or_else(|| "invalid result archive".to_string())?;
            if folded_key.len() > 4 * 1024 || kinds.insert(folded_key, directory).is_some() {
                return Err("result archive has duplicate paths".into());
            }
            let size = file.size();
            let packed = file.compressed_size();
            if size > limits.max_entry_bytes {
                return Err("result archive entry exceeds expanded size limit".into());
            }
            expanded = expanded
                .checked_add(size)
                .filter(|v| *v <= limits.max_expanded_bytes)
                .ok_or_else(|| "result archive exceeds expanded size limit".to_string())?;
            if ratio_exceeded(size, packed, limits.max_ratio)? {
                return Err("result archive exceeds expansion ratio limit".into());
            }
            entries.push((path, directory, size, packed));
        }

        let mut folded: Vec<_> = kinds.keys().map(String::as_str).collect();
        folded.sort_unstable();
        for (index, key) in folded.iter().enumerate() {
            for (slash, _) in key.match_indices('/') {
                if kinds.get(&key[..slash]) == Some(&false) {
                    return Err("result archive has file-directory conflicts".into());
                }
            }
            if kinds.get(*key) == Some(&false)
                && folded.get(index + 1).is_some_and(|next| {
                    next.starts_with(key) && next.as_bytes().get(key.len()) == Some(&b'/')
                })
            {
                return Err("result archive has file-directory conflicts".into());
            }
        }

        validate_contents(self, &entries, limits, compressed)?;
        let root = output_root(destination)?;
        let transaction = ExtractionTransaction::new(&root, &entries)?;
        let mut total = 0u64;
        let mut buffer = [0u8; 64 * 1024];
        let result = (|| {
            for (index, (path, directory, expected, _)) in entries.iter().enumerate() {
                if *directory {
                    ensure_directory(&transaction.stage, path)?;
                    continue;
                }
                let (parent, leaf) = split_parent(&transaction.stage, path)?;
                write_temp_file(
                    &parent,
                    leaf,
                    |output| {
                        let mut input =
                            zip.by_index(index).map_err(|_| "invalid result archive")?;
                        let mut actual = 0u64;
                        loop {
                            let read = input
                                .read(&mut buffer)
                                .map_err(|_| "invalid result archive")?;
                            if read == 0 {
                                break;
                            }
                            let bytes = u64::try_from(read)
                                .map_err(|_| "result archive exceeds expanded size limit")?;
                            actual = actual
                                .checked_add(bytes)
                                .filter(|v| *v <= limits.max_entry_bytes)
                                .ok_or_else(|| {
                                    "result archive entry exceeds expanded size limit".to_string()
                                })?;
                            total = total
                                .checked_add(bytes)
                                .filter(|v| *v <= limits.max_expanded_bytes)
                                .ok_or_else(|| {
                                    "result archive exceeds expanded size limit".to_string()
                                })?;
                            if ratio_exceeded(total, compressed, limits.max_ratio)? {
                                return Err("result archive exceeds expansion ratio limit".into());
                            }
                            output
                                .write_all(&buffer[..read])
                                .map_err(|_| "unable to write extracted file")?;
                        }
                        if actual != *expected {
                            return Err("result archive entry size does not match metadata".into());
                        }
                        output
                            .flush()
                            .map_err(|_| "unable to write extracted file")?;
                        Ok(())
                    },
                    publish_file,
                )?;
            }
            for (path, directory, _, _) in &entries {
                preflight_destination(&root, path, *directory)?;
            }
            commit_staged_overlay(
                &root,
                &transaction.stage,
                &transaction.backup,
                &entries,
                fault,
            )
        })();
        if !result
            .as_ref()
            .is_err_and(|message| message.contains("partial publication"))
        {
            transaction.cleanup();
        }
        result
    }

    #[cfg(test)]
    fn extract_with_commit_fault(
        &self,
        destination: &Path,
        limits: ArchiveLimits,
        fail_after: usize,
        fail_restore_a: bool,
    ) -> Result<(), String> {
        self.extract_impl(
            destination,
            limits,
            CommitFault {
                fail_after: Some(fail_after),
                fail_restore_a,
            },
        )
    }
}

#[derive(Clone, Copy, Default)]
struct CommitFault {
    #[cfg(test)]
    fail_after: Option<usize>,
    #[cfg(test)]
    fail_restore_a: bool,
}

struct ExtractionTransaction {
    root: Dir,
    name: std::ffi::OsString,
    stage: Dir,
    backup: Dir,
}

impl ExtractionTransaction {
    fn new(root: &Dir, entries: &[(PathBuf, bool, u64, u64)]) -> Result<Self, String> {
        let cleanup_root = root.try_clone().map_err(|_| "unable to open output root")?;
        let reserved: HashSet<_> = entries
            .iter()
            .filter_map(|(path, _, _, _)| path.components().next())
            .filter_map(|component| match component {
                Component::Normal(name) => Some(name.to_string_lossy().to_lowercase()),
                _ => None,
            })
            .collect();
        for _ in 0..reserved.len().saturating_add(32) {
            let name = std::ffi::OsString::from(format!(
                ".mineru-archive-{}-{}",
                std::process::id(),
                TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
            ));
            if reserved.contains(&name.to_string_lossy().to_lowercase()) {
                continue;
            }
            #[cfg(unix)]
            let create_result = {
                use cap_std::fs::DirBuilderExt;
                let mut builder = cap_std::fs::DirBuilder::new();
                builder.mode(0o700);
                root.create_dir_with(&name, &builder)
            };
            #[cfg(not(unix))]
            let create_result = root.create_dir(&name);
            match create_result {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(_) => return Err("unable to safely create extraction transaction".into()),
            }
            let transaction = match root.open_dir_nofollow(&name) {
                Ok(transaction) => transaction,
                Err(_) => {
                    let _ = root.remove_dir(&name);
                    return Err("unable to safely create extraction transaction".into());
                }
            };
            let directories = (|| {
                transaction
                    .create_dir("stage")
                    .map_err(|_| "unable to safely create extraction transaction")?;
                transaction
                    .create_dir("backup")
                    .map_err(|_| "unable to safely create extraction transaction")?;
                let stage = transaction
                    .open_dir_nofollow("stage")
                    .map_err(|_| "unable to safely create extraction transaction")?;
                let backup = transaction
                    .open_dir_nofollow("backup")
                    .map_err(|_| "unable to safely create extraction transaction")?;
                Ok::<_, String>((stage, backup))
            })();
            drop(transaction);
            return match directories {
                Ok((stage, backup)) => Ok(Self {
                    root: cleanup_root,
                    name,
                    stage,
                    backup,
                }),
                Err(error) => {
                    let _ = root.remove_dir_all(&name);
                    Err(error)
                }
            };
        }
        Err("unable to safely create extraction transaction".into())
    }

    fn cleanup(self) {
        let Self {
            root,
            name,
            stage,
            backup,
        } = self;
        drop(stage);
        drop(backup);
        let _ = root.remove_dir_all(name);
    }
}

struct CommitState<'a> {
    root: &'a Dir,
    backup: &'a Dir,
    installed: Vec<PathBuf>,
    backed_up: Vec<PathBuf>,
    created_dirs: Vec<PathBuf>,
    publications: usize,
    fault: CommitFault,
}

impl CommitState<'_> {
    fn published(&mut self) -> Result<(), String> {
        self.publications += 1;
        #[cfg(test)]
        if self.fault.fail_after == Some(self.publications) {
            return Err("unable to publish extracted file".into());
        }
        Ok(())
    }

    fn ensure_directory(&mut self, path: &Path) -> Result<Dir, String> {
        let mut dir = self
            .root
            .try_clone()
            .map_err(|_| "unable to open output root")?;
        let mut current = PathBuf::new();
        for part in path.components() {
            let Component::Normal(name) = part else {
                return Err("result archive has an unsafe path".into());
            };
            current.push(name);
            match dir.symlink_metadata(name) {
                Ok(meta) if meta.is_dir() && !meta.file_type().is_symlink() => {}
                Ok(_) => return Err("output path contains a symlink or non-directory".into()),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    dir.create_dir(name)
                        .map_err(|_| "unable to create output directory")?;
                    self.created_dirs.push(current.clone());
                    self.published()?;
                }
                Err(_) => return Err("unable to inspect output path".into()),
            }
            dir = dir
                .open_dir_nofollow(name)
                .map_err(|_| "output path contains a symlink or non-directory")?;
        }
        Ok(dir)
    }

    fn install(&mut self, stage: &Dir, path: &Path) -> Result<(), String> {
        let leaf = path
            .file_name()
            .ok_or_else(|| "result archive has an unsafe path".to_string())?;
        let parent_path = path.parent().unwrap_or(Path::new(""));
        let destination_parent = self.ensure_directory(parent_path)?;
        let staged_parent = ensure_directory(stage, parent_path)?;
        match destination_parent.symlink_metadata(leaf) {
            Ok(meta) if meta.is_dir() || (!meta.is_file() && !meta.file_type().is_symlink()) => {
                return Err("output path contains a directory or special file".into());
            }
            Ok(_) => {
                let backup_parent = ensure_directory(self.backup, parent_path)
                    .map_err(|_| "unable to prepare publication rollback")?;
                destination_parent
                    .rename(leaf, &backup_parent, leaf)
                    .map_err(|_| "unable to prepare publication rollback")?;
                self.backed_up.push(path.to_path_buf());
                self.published()?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return Err("unable to inspect output path".into()),
        }
        staged_parent
            .rename(leaf, &destination_parent, leaf)
            .map_err(|_| "unable to publish extracted file")?;
        self.installed.push(path.to_path_buf());
        self.published()
    }

    fn rollback(&mut self) -> bool {
        let mut succeeded = true;
        for path in self.installed.iter().rev() {
            succeeded &= remove_relative_file(self.root, path).is_ok();
        }
        for path in self.backed_up.iter().rev() {
            #[cfg(test)]
            let restore = if self.fault.fail_restore_a && path == Path::new("a") {
                Err(())
            } else {
                restore_relative_file(self.backup, self.root, path)
            };
            #[cfg(not(test))]
            let restore = restore_relative_file(self.backup, self.root, path);
            succeeded &= restore.is_ok();
        }
        for path in self.created_dirs.iter().rev() {
            succeeded &= remove_relative_directory(self.root, path).is_ok();
        }
        succeeded
    }
}

fn commit_staged_overlay(
    root: &Dir,
    stage: &Dir,
    backup: &Dir,
    entries: &[(PathBuf, bool, u64, u64)],
    fault: CommitFault,
) -> Result<(), String> {
    let mut state = CommitState {
        root,
        backup,
        installed: Vec::new(),
        backed_up: Vec::new(),
        created_dirs: Vec::new(),
        publications: 0,
        fault,
    };
    let result = (|| {
        for (path, directory, _, _) in entries {
            if *directory {
                state.ensure_directory(path)?;
            } else {
                state.install(stage, path)?;
            }
        }
        Ok(())
    })();
    match result {
        Ok(()) => Ok(()),
        Err(error) if state.rollback() => Err(error),
        Err(_) => Err("partial publication: unable to fully roll back result archive".into()),
    }
}

fn remove_relative_file(root: &Dir, path: &Path) -> Result<(), ()> {
    let (parts, leaf) = relative_parts(path).map_err(|_| ())?;
    existing_parent(root, &parts)
        .map_err(|_| ())?
        .remove_file(leaf)
        .map_err(|_| ())
}

fn restore_relative_file(backup: &Dir, root: &Dir, path: &Path) -> Result<(), ()> {
    let (parts, leaf) = relative_parts(path).map_err(|_| ())?;
    let backup_parent = existing_parent(backup, &parts).map_err(|_| ())?;
    let destination_parent = existing_parent(root, &parts).map_err(|_| ())?;
    backup_parent
        .rename(leaf, &destination_parent, leaf)
        .map_err(|_| ())
}

fn remove_relative_directory(root: &Dir, path: &Path) -> Result<(), ()> {
    let (parts, leaf) = relative_parts(path).map_err(|_| ())?;
    existing_parent(root, &parts)
        .map_err(|_| ())?
        .remove_dir(leaf)
        .map_err(|_| ())
}

fn ratio_exceeded(expanded: u64, compressed: u64, max_ratio: u64) -> Result<bool, String> {
    if expanded == 0 {
        return Ok(false);
    }
    if compressed == 0 {
        return Ok(true);
    }
    Ok(expanded
        > compressed
            .checked_mul(max_ratio)
            .ok_or_else(|| "archive limits are invalid".to_string())?)
}

fn validate_contents(
    archive: &DownloadedZip,
    entries: &[(PathBuf, bool, u64, u64)],
    limits: ArchiveLimits,
    compressed: u64,
) -> Result<(), String> {
    let mut zip = ZipArchive::with_config(
        Config {
            archive_offset: ArchiveOffset::Known(0),
        },
        archive
            .reopen()
            .map_err(|_| "unable to open result archive")?,
    )
    .map_err(|_| "invalid result archive")?;
    let mut total = 0u64;
    let mut buffer = [0u8; 64 * 1024];
    for (index, (_, directory, expected, packed)) in entries.iter().enumerate() {
        if *directory {
            continue;
        }
        let mut input = zip.by_index(index).map_err(|_| "invalid result archive")?;
        let mut actual = 0u64;
        loop {
            let read = input
                .read(&mut buffer)
                .map_err(|_| "invalid result archive")?;
            if read == 0 {
                break;
            }
            let bytes =
                u64::try_from(read).map_err(|_| "result archive exceeds expanded size limit")?;
            actual = actual
                .checked_add(bytes)
                .filter(|v| *v <= limits.max_entry_bytes)
                .ok_or_else(|| "result archive entry exceeds expanded size limit".to_string())?;
            total = total
                .checked_add(bytes)
                .filter(|v| *v <= limits.max_expanded_bytes)
                .ok_or_else(|| "result archive exceeds expanded size limit".to_string())?;
            std::io::sink()
                .write_all(&buffer[..read])
                .map_err(|_| "unable to validate result archive")?;
        }
        if actual != *expected {
            return Err("result archive entry size does not match metadata".into());
        }
        if ratio_exceeded(actual, *packed, limits.max_ratio)? {
            return Err("result archive exceeds expansion ratio limit".into());
        }
        // Defense in depth: actual metadata and per-entry checks imply this bound.
        if ratio_exceeded(total, compressed, limits.max_ratio)? {
            return Err("result archive exceeds expansion ratio limit".into());
        }
    }
    Ok(())
}

fn archive_path(name: &str) -> Result<PathBuf, String> {
    if name.is_empty() || name.contains('\\') || name.bytes().any(|b| b < 0x20 || b == 0x7f) {
        return Err("result archive has an unsafe path".into());
    }
    let segments: Vec<_> = name.split('/').collect();
    if segments
        .iter()
        .any(|segment| *segment == "." || *segment == "..")
        || segments.iter().enumerate().any(|(index, segment)| {
            segment.is_empty()
                && (index != segments.len() - 1 || !name.ends_with('/') || segments.len() < 2)
        })
    {
        return Err("result archive has an unsafe path".into());
    }
    let path = Path::new(name);
    if path.is_absolute() || matches!(path.components().next(), Some(Component::Prefix(_))) {
        return Err("result archive has an unsafe path".into());
    }
    let mut result = PathBuf::new();
    for component in path.components() {
        let Component::Normal(part) = component else {
            return Err("result archive has an unsafe path".into());
        };
        let text = part
            .to_str()
            .ok_or_else(|| "result archive has an unsafe path".to_string())?;
        let upper = text.split('.').next().unwrap_or(text).to_ascii_uppercase();
        if text.contains(['<', '>', ':', '"', '|', '?', '*'])
            || text.ends_with(['.', ' '])
            || matches!(
                upper.as_str(),
                "CON"
                    | "PRN"
                    | "AUX"
                    | "NUL"
                    | "COM1"
                    | "COM2"
                    | "COM3"
                    | "COM4"
                    | "COM5"
                    | "COM6"
                    | "COM7"
                    | "COM8"
                    | "COM9"
                    | "LPT1"
                    | "LPT2"
                    | "LPT3"
                    | "LPT4"
                    | "LPT5"
                    | "LPT6"
                    | "LPT7"
                    | "LPT8"
                    | "LPT9"
            )
        {
            return Err("result archive has an unsafe path".into());
        }
        result.push(part);
    }
    if result.as_os_str().is_empty() {
        Err("result archive has an unsafe path".into())
    } else {
        Ok(result)
    }
}

fn create_temp_file(parent: &Dir) -> Result<(std::ffi::OsString, cap_std::fs::File), String> {
    for _ in 0..32 {
        let name = std::ffi::OsString::from(format!(
            ".mineru-extract-{}-{}",
            std::process::id(),
            TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let mut options = OpenOptions::new();
        options
            .write(true)
            .create_new(true)
            .follow(FollowSymlinks::No);
        match parent.open_with(&name, &options) {
            Ok(file) => return Ok((name, file)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(_) => return Err("unable to safely create extracted file".into()),
        }
    }
    Err("unable to safely create extracted file".into())
}

fn write_temp_file(
    parent: &Dir,
    leaf: &std::ffi::OsStr,
    writer: impl FnOnce(&mut cap_std::fs::File) -> Result<(), String>,
    publisher: impl FnOnce(&Dir, &std::ffi::OsStr, &std::ffi::OsStr) -> Result<(), String>,
) -> Result<(), String> {
    let (temp, mut output) = create_temp_file(parent)?;
    let write_result = writer(&mut output).and_then(|_| {
        output
            .flush()
            .map_err(|_| "unable to write extracted file".to_string())
    });
    drop(output);
    let result = write_result.and_then(|_| publisher(parent, &temp, leaf));
    if result.is_err() {
        let _ = parent.remove_file(&temp);
    }
    result
}

fn publish_file(
    parent: &Dir,
    temp: &std::ffi::OsStr,
    leaf: &std::ffi::OsStr,
) -> Result<(), String> {
    match parent.symlink_metadata(leaf) {
        Ok(meta) if meta.is_dir() || (!meta.is_file() && !meta.file_type().is_symlink()) => {
            return Err("output path contains a directory or special file".into());
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(_) => return Err("unable to inspect output path".into()),
    }
    parent
        .rename(temp, parent, leaf)
        .map_err(|_| "unable to publish extracted file".into())
}

fn open_output_root(path: &Path, create: bool) -> Result<Dir, String> {
    let components: Vec<_> = path.components().collect();
    if components
        .iter()
        .any(|c| matches!(c, Component::ParentDir | Component::Prefix(_)))
    {
        return Err("output root is unsafe".into());
    }
    let mut root = Dir::open_ambient_dir(
        if path.is_absolute() { "/" } else { "." },
        ambient_authority(),
    )
    .map_err(|_| "unable to open output root")?;
    let alias = cfg!(target_os = "macos")
        && matches!(components.as_slice(), [Component::RootDir, Component::Normal(x), ..] if *x == "tmp" || *x == "var");
    if alias {
        root = Dir::from_std_file(
            open_dir_nofollow(&root.into_std_file(), Path::new("private"))
                .map_err(|_| "unable to open output root")?,
        );
    }
    for component in components {
        if let Component::Normal(name) = component {
            match root.symlink_metadata(name) {
                Ok(meta) if meta.is_dir() && !meta.file_type().is_symlink() => {}
                Ok(_) => return Err("output root contains a symlink or non-directory".into()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound && create => root
                    .create_dir(name)
                    .map_err(|_| "unable to create output root")?,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    return Err("output root is missing".into());
                }
                Err(_) => return Err("unable to inspect output root".into()),
            }
            root = root
                .open_dir_nofollow(name)
                .map_err(|_| "unable to open output root")?;
        }
    }
    Ok(root)
}
fn output_root(path: &Path) -> Result<Dir, String> {
    open_output_root(path, true)
}

/// Creates and validates the root without exposing the capability outside this module.
pub(super) fn preflight_output_root(path: &Path) -> Result<(), String> {
    drop(output_root(path)?);
    Ok(())
}

fn relative_parts(path: &Path) -> Result<(Vec<&std::ffi::OsStr>, &std::ffi::OsStr), String> {
    let mut parts = Vec::new();
    for component in path.components() {
        match component {
            Component::Normal(part) if !part.is_empty() => parts.push(part),
            _ => return Err("relative path is unsafe".into()),
        }
    }
    let leaf = parts.pop().ok_or("relative path is unsafe")?;
    Ok((parts, leaf))
}

fn existing_parent(root: &Dir, parts: &[&std::ffi::OsStr]) -> Result<Dir, String> {
    let mut dir = root.try_clone().map_err(|_| "unable to open output root")?;
    for part in parts {
        match dir.symlink_metadata(part) {
            Ok(meta) if meta.is_dir() && !meta.file_type().is_symlink() => {}
            Ok(_) => return Err("output path contains a symlink or non-directory".into()),
            Err(_) => return Err("output path is missing".into()),
        }
        dir = dir
            .open_dir_nofollow(part)
            .map_err(|_| "unable to open output path")?;
    }
    Ok(dir)
}

pub(super) fn read_relative_capped(
    root: &Path,
    relative: &Path,
    cap: usize,
) -> Result<Vec<u8>, String> {
    let root = open_output_root(root, false)?;
    let (parts, leaf) = relative_parts(relative)?;
    let parent = existing_parent(&root, &parts)?;
    match parent.symlink_metadata(leaf) {
        Ok(meta) if meta.is_file() && !meta.file_type().is_symlink() => {}
        Ok(_) => return Err("output path is not a regular file".into()),
        Err(_) => return Err("output path is missing".into()),
    }
    let mut options = OpenOptions::new();
    options.read(true).follow(FollowSymlinks::No);
    let mut file = parent
        .open_with(leaf, &options)
        .map_err(|_| "unable to open output file")?;
    let mut out = Vec::with_capacity(cap.min(8192));
    let mut buffer = [0u8; 8192];
    while out.len() < cap {
        let amount = buffer.len().min(cap - out.len());
        let read = file
            .read(&mut buffer[..amount])
            .map_err(|_| "unable to read output file")?;
        if read == 0 {
            return Ok(out);
        }
        out.extend_from_slice(&buffer[..read]);
    }
    let mut probe = [0u8; 1];
    if file
        .read(&mut probe)
        .map_err(|_| "unable to read output file")?
        != 0
    {
        return Err("output file exceeds size limit".into());
    }
    Ok(out)
}

pub(super) fn write_relative_atomic(
    root: &Path,
    relative: &Path,
    bytes: &[u8],
) -> Result<(), String> {
    let root = open_output_root(root, false)?;
    let (parts, leaf) = relative_parts(relative)?;
    let parent = existing_parent(&root, &parts)?;
    write_temp_file(
        &parent,
        leaf,
        |file| {
            file.write_all(bytes)
                .map_err(|_| "unable to write output file".into())
        },
        publish_file,
    )
}

fn split_parent<'a>(root: &Dir, path: &'a Path) -> Result<(Dir, &'a std::ffi::OsStr), String> {
    let leaf = path
        .file_name()
        .ok_or_else(|| "result archive has an unsafe path".to_string())?;
    let parent = path.parent().unwrap_or(Path::new(""));
    Ok((ensure_directory(root, parent)?, leaf))
}
fn ensure_directory(root: &Dir, path: &Path) -> Result<Dir, String> {
    let mut dir = root.try_clone().map_err(|_| "unable to open output root")?;
    for part in path.components() {
        let Component::Normal(name) = part else {
            return Err("result archive has an unsafe path".into());
        };
        match dir.symlink_metadata(name) {
            Ok(meta) if meta.is_dir() && !meta.file_type().is_symlink() => {}
            Ok(_) => return Err("output path contains a symlink or non-directory".into()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => dir
                .create_dir(name)
                .map_err(|_| "unable to create output directory")?,
            Err(_) => return Err("unable to inspect output path".into()),
        }
        dir = dir
            .open_dir_nofollow(name)
            .map_err(|_| "output path contains a symlink or non-directory")?;
    }
    Ok(dir)
}
fn preflight_destination(root: &Dir, path: &Path, directory: bool) -> Result<(), String> {
    let leaf = path
        .file_name()
        .ok_or_else(|| "result archive has an unsafe path".to_string())?;
    let mut dir = root.try_clone().map_err(|_| "unable to open output root")?;
    for part in path.parent().unwrap_or(Path::new("")).components() {
        let Component::Normal(name) = part else {
            return Err("result archive has an unsafe path".into());
        };
        match dir.symlink_metadata(name) {
            Ok(meta) if meta.is_dir() && !meta.file_type().is_symlink() => {
                dir = dir
                    .open_dir_nofollow(name)
                    .map_err(|_| "output path contains a symlink")?
            }
            Ok(_) => return Err("output path contains a symlink or non-directory".into()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(_) => return Err("unable to inspect output path".into()),
        }
    }
    match dir.symlink_metadata(leaf) {
        Ok(meta)
            if meta.file_type().is_symlink()
                || (directory && !meta.is_dir())
                || (!directory && !meta.is_file()) =>
        {
            Err("output path contains a symlink or special file".into())
        }
        Ok(_) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(_) => Err("unable to inspect output path".into()),
    }
}

pub(super) async fn download(
    client: &Client,
    result_url: &str,
    task: &str,
    timeout: Duration,
    limits: ArchiveLimits,
) -> Result<DownloadedZip, String> {
    let limits = limits.validate()?;
    let task = sanitize_vlm_error_bytes(
        serde_json::to_string(task).unwrap_or_default().as_bytes(),
        BODY_CAP,
    );
    let response = tokio::time::timeout(timeout, client.get(result_url).send())
        .await
        .map_err(|_| format!("{task} result download timed out"))?
        .map_err(|_| format!("{task} result download failed"))?;
    if response.status() != StatusCode::OK {
        return Err(http_error(&task, response, timeout).await);
    }
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if !content_type
        .to_ascii_lowercase()
        .contains("application/zip")
    {
        return Err(format!(
            "{task} result download has unexpected Content-Type"
        ));
    }
    let temp = NamedTempFile::new().map_err(|_| "unable to create result archive".to_string())?;
    let mut file = tokio::fs::File::from_std(
        temp.reopen()
            .map_err(|_| "unable to open result archive".to_string())?,
    );
    let mut stream = response.bytes_stream();
    let mut total = 0_u64;
    while let Some(chunk) = tokio::time::timeout(timeout, stream.next())
        .await
        .map_err(|_| format!("{task} result download timed out"))?
    {
        let chunk = match chunk {
            Ok(chunk) => chunk,
            Err(_) => return Err(format!("{task} result download body failed")),
        };
        total = total
            .checked_add(
                u64::try_from(chunk.len())
                    .map_err(|_| "result archive exceeds compressed size limit")?,
            )
            .filter(|size| *size <= limits.max_compressed_bytes)
            .ok_or_else(|| "result archive exceeds compressed size limit".to_string())?;
        file.write_all(&chunk)
            .await
            .map_err(|_| "unable to write result archive".to_string())?;
    }
    file.flush()
        .await
        .map_err(|_| "unable to write result archive".to_string())?;
    Ok(DownloadedZip(temp))
}

async fn http_error(task: &str, response: Response, timeout: Duration) -> String {
    let status = response.status();
    let mut stream = response.bytes_stream();
    let mut body = Vec::new();
    loop {
        let next = tokio::time::timeout(timeout, stream.next())
            .await
            .map_err(|_| format!("{task} result download timed out"));
        let chunk = match next {
            Ok(Some(chunk)) => chunk,
            Ok(None) => break,
            Err(error) => return error,
        };
        let chunk = match chunk {
            Ok(chunk) => chunk,
            Err(_) => return format!("{task} result download body failed"),
        };
        let remain = BODY_CAP.saturating_sub(body.len());
        body.extend_from_slice(&chunk[..chunk.len().min(remain)]);
    }
    format!(
        "{task} result download HTTP {status}: {}",
        sanitize_vlm_error_bytes(&body, BODY_CAP)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    #[cfg(unix)]
    use std::os::unix::{
        fs::{FileTypeExt, symlink},
        net::UnixListener,
    };
    use zip::{CompressionMethod, ZipWriter, write::SimpleFileOptions};

    fn limits() -> ArchiveLimits {
        ArchiveLimits {
            max_entries: 8,
            max_compressed_bytes: 1024,
            max_expanded_bytes: 64,
            max_entry_bytes: 32,
            max_ratio: 100,
        }
    }
    fn archive(entries: &[(&str, &[u8], CompressionMethod)]) -> DownloadedZip {
        let temp = NamedTempFile::new().unwrap();
        let file = temp.reopen().unwrap();
        let mut writer = ZipWriter::new(file);
        for (name, bytes, method) in entries {
            writer
                .start_file(
                    *name,
                    SimpleFileOptions::default().compression_method(*method),
                )
                .unwrap();
            writer.write_all(bytes).unwrap();
        }
        writer.finish().unwrap();
        DownloadedZip(temp)
    }

    fn metadata(archive: &DownloadedZip) -> (u64, u64, u64) {
        let length = std::fs::metadata(archive.path()).unwrap().len();
        let mut zip = ZipArchive::new(archive.reopen().unwrap()).unwrap();
        let file = zip.by_index(0).unwrap();
        (length, file.size(), file.compressed_size())
    }

    fn sentinel_output() -> tempfile::TempDir {
        let output = tempfile::tempdir().unwrap();
        std::fs::write(output.path().join("sentinel"), "old").unwrap();
        output
    }

    fn assert_sentinel_unchanged(archive: &DownloadedZip, output: &tempfile::TempDir) {
        assert!(archive.extract(output.path(), limits()).is_err());
        assert_eq!(
            std::fs::read(output.path().join("sentinel")).unwrap(),
            b"old"
        );
    }

    fn transaction_artifacts(path: &Path) -> Vec<std::ffi::OsString> {
        std::fs::read_dir(path)
            .unwrap()
            .filter_map(|entry| {
                let name = entry.unwrap().file_name();
                name.to_string_lossy()
                    .starts_with(".mineru-archive-")
                    .then_some(name)
            })
            .collect()
    }

    fn patch_u32(bytes: &mut [u8], signature: &[u8; 4], offset: usize, value: u32) {
        let position = bytes
            .windows(4)
            .position(|window| window == signature)
            .unwrap();
        bytes[position + offset..position + offset + 4].copy_from_slice(&value.to_le_bytes());
    }

    #[test]
    fn extracts_directory_stored_deflated_unicode_and_overwrites() {
        let temp = NamedTempFile::new().unwrap();
        let file = temp.reopen().unwrap();
        let mut writer = ZipWriter::new(file);
        writer
            .add_directory(
                "nested/",
                SimpleFileOptions::default().unix_permissions(0o040755),
            )
            .unwrap();
        writer
            .start_file(
                "nested/é.txt",
                SimpleFileOptions::default().compression_method(CompressionMethod::Stored),
            )
            .unwrap();
        writer.write_all(b"stored").unwrap();
        writer
            .start_file(
                "new.txt",
                SimpleFileOptions::default().compression_method(CompressionMethod::Deflated),
            )
            .unwrap();
        writer.write_all(b"deflated").unwrap();
        writer.finish().unwrap();
        let archive = DownloadedZip(temp);
        let output = tempfile::tempdir().unwrap();
        std::fs::create_dir(output.path().join("nested")).unwrap();
        std::fs::write(output.path().join("new.txt"), "old").unwrap();
        archive.extract(output.path(), limits()).unwrap();
        assert_eq!(
            std::fs::read(output.path().join("nested/é.txt")).unwrap(),
            b"stored"
        );
        assert_eq!(
            std::fs::read(output.path().join("new.txt")).unwrap(),
            b"deflated"
        );
        assert!(transaction_artifacts(output.path()).is_empty());
    }

    #[test]
    fn ordered_archives_overlay_duplicate_files_and_merge_directories() {
        let output = tempfile::tempdir().unwrap();
        archive(&[
            ("merged/a", b"first", CompressionMethod::Stored),
            ("merged/keep", b"keep", CompressionMethod::Stored),
        ])
        .extract(output.path(), limits())
        .unwrap();
        archive(&[
            ("merged/a", b"second", CompressionMethod::Stored),
            ("merged/b", b"new", CompressionMethod::Stored),
        ])
        .extract(output.path(), limits())
        .unwrap();

        assert_eq!(
            std::fs::read(output.path().join("merged/a")).unwrap(),
            b"second"
        );
        assert_eq!(
            std::fs::read(output.path().join("merged/b")).unwrap(),
            b"new"
        );
        assert_eq!(
            std::fs::read(output.path().join("merged/keep")).unwrap(),
            b"keep"
        );
        assert!(transaction_artifacts(output.path()).is_empty());
    }

    #[test]
    fn commit_failure_rolls_back_the_complete_overlay() {
        let output = tempfile::tempdir().unwrap();
        std::fs::write(output.path().join("a"), b"old").unwrap();
        std::fs::write(output.path().join("keep"), b"unrelated").unwrap();
        let candidate = archive(&[
            ("nested/deeper/c", b"nested", CompressionMethod::Stored),
            ("a", b"new", CompressionMethod::Stored),
            ("b", b"new", CompressionMethod::Stored),
        ]);

        // Publications 1-3 create nested/deeper and install c; 4-5 back up/install a;
        // publication 6 installs b and then injects the commit failure.
        assert_eq!(
            candidate
                .extract_with_commit_fault(output.path(), limits(), 6, false)
                .unwrap_err(),
            "unable to publish extracted file"
        );
        assert_eq!(std::fs::read(output.path().join("a")).unwrap(), b"old");
        assert!(!output.path().join("b").exists());
        assert_eq!(
            std::fs::read(output.path().join("keep")).unwrap(),
            b"unrelated"
        );
        assert!(!output.path().join("nested").exists());
        assert!(transaction_artifacts(output.path()).is_empty());
    }

    #[test]
    fn rollback_failure_reports_partial_publication() {
        let output = tempfile::tempdir().unwrap();
        std::fs::write(output.path().join("a"), b"old").unwrap();
        std::fs::write(output.path().join("keep"), b"unrelated").unwrap();
        let error = archive(&[
            ("nested/deeper/c", b"nested", CompressionMethod::Stored),
            ("a", b"new", CompressionMethod::Stored),
            ("b", b"new", CompressionMethod::Stored),
        ])
        // The same six publications complete; rollback removes b, a, and c, then the
        // injected restore-a failure occurs before best-effort directory cleanup.
        .extract_with_commit_fault(output.path(), limits(), 6, true)
        .unwrap_err();
        assert_eq!(
            error,
            "partial publication: unable to fully roll back result archive"
        );
        assert!(!output.path().join("a").exists());
        assert!(!output.path().join("b").exists());
        assert!(!output.path().join("nested").exists());
        assert_eq!(
            std::fs::read(output.path().join("keep")).unwrap(),
            b"unrelated"
        );
        let artifacts = transaction_artifacts(output.path());
        assert_eq!(artifacts.len(), 1);
        assert_eq!(
            std::fs::read(output.path().join(&artifacts[0]).join("backup/a")).unwrap(),
            b"old"
        );
    }

    #[test]
    fn extracts_to_a_missing_relative_destination_root() {
        let archive = archive(&[("file", b"contents", CompressionMethod::Stored)]);
        let holder = tempfile::Builder::new()
            .prefix("archive-relative-")
            .tempdir_in(".")
            .unwrap();
        let relative = PathBuf::from(holder.path().file_name().unwrap());
        drop(holder);
        archive.extract(&relative, limits()).unwrap();
        assert_eq!(std::fs::read(relative.join("file")).unwrap(), b"contents");
        std::fs::remove_dir_all(relative).unwrap();
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn extracts_through_private_tmp_alias() {
        let output = tempfile::tempdir_in("/tmp").unwrap();
        archive(&[("file", b"contents", CompressionMethod::Stored)])
            .extract(output.path(), limits())
            .unwrap();
        assert_eq!(
            std::fs::read(output.path().join("file")).unwrap(),
            b"contents"
        );
    }

    #[cfg(unix)]
    #[test]
    fn preflight_rejects_hostile_destinations_before_writing() {
        let hostile = |name: &str| {
            archive(&[
                ("sentinel", b"new", CompressionMethod::Stored),
                (name, b"hostile", CompressionMethod::Stored),
            ])
        };

        let output = sentinel_output();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("file"), "outside").unwrap();
        symlink(outside.path(), output.path().join("link")).unwrap();
        assert_sentinel_unchanged(&hostile("link/file"), &output);
        assert_eq!(
            std::fs::read(outside.path().join("file")).unwrap(),
            b"outside"
        );

        let output = sentinel_output();
        let outside = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(outside.path(), "outside").unwrap();
        symlink(outside.path(), output.path().join("link")).unwrap();
        assert_sentinel_unchanged(&hostile("link"), &output);
        assert_eq!(std::fs::read(outside.path()).unwrap(), b"outside");

        let parent = tempfile::tempdir().unwrap();
        let output = parent.path().join("output");
        std::fs::create_dir(&output).unwrap();
        std::fs::write(output.join("sentinel"), "old").unwrap();
        let alias = parent.path().join("alias");
        symlink(&output, &alias).unwrap();
        assert!(hostile("hostile").extract(&alias, limits()).is_err());
        assert_eq!(std::fs::read(output.join("sentinel")).unwrap(), b"old");

        let output = sentinel_output();
        std::fs::write(output.path().join("parent"), "regular").unwrap();
        assert_sentinel_unchanged(&hostile("parent/child"), &output);

        for name in ["socket", "socket/child"] {
            let output = sentinel_output();
            let socket = output.path().join("socket");
            let _listener = UnixListener::bind(&socket).unwrap();
            assert_sentinel_unchanged(&hostile(name), &output);
            assert!(
                std::fs::symlink_metadata(socket)
                    .unwrap()
                    .file_type()
                    .is_socket()
            );
        }
    }

    #[test]
    fn preflight_rejects_file_directory_collisions_before_writing() {
        for (name, directory) in [("regular/", true), ("directory", false)] {
            let output = sentinel_output();
            if directory {
                std::fs::write(output.path().join("regular"), "regular").unwrap();
            } else {
                std::fs::create_dir(output.path().join("directory")).unwrap();
            }
            let temp = NamedTempFile::new().unwrap();
            let mut writer = ZipWriter::new(temp.reopen().unwrap());
            writer
                .start_file("sentinel", SimpleFileOptions::default())
                .unwrap();
            writer.write_all(b"new").unwrap();
            if directory {
                writer
                    .add_directory(name, SimpleFileOptions::default())
                    .unwrap();
            } else {
                writer
                    .start_file(name, SimpleFileOptions::default())
                    .unwrap();
                writer.write_all(b"hostile").unwrap();
            }
            writer.finish().unwrap();
            assert_sentinel_unchanged(&DownloadedZip(temp), &output);
        }
    }

    #[test]
    fn raw_path_conflicts_and_separators_preserve_sentinel() {
        for (case, entries) in [
            vec![
                ("File", b"x".as_slice(), CompressionMethod::Stored),
                ("file", b"y".as_slice(), CompressionMethod::Stored),
            ],
            vec![
                ("file", b"x".as_slice(), CompressionMethod::Stored),
                ("file/child", b"y".as_slice(), CompressionMethod::Stored),
            ],
            vec![
                ("file/child", b"x".as_slice(), CompressionMethod::Stored),
                ("file", b"y".as_slice(), CompressionMethod::Stored),
            ],
            vec![
                ("File", b"x".as_slice(), CompressionMethod::Stored),
                ("file/child", b"y".as_slice(), CompressionMethod::Stored),
            ],
            vec![
                ("file/child", b"x".as_slice(), CompressionMethod::Stored),
                ("File", b"y".as_slice(), CompressionMethod::Stored),
            ],
            vec![
                ("sentinel", b"new".as_slice(), CompressionMethod::Stored),
                ("a//b", b"x".as_slice(), CompressionMethod::Stored),
            ],
            vec![
                ("sentinel", b"new".as_slice(), CompressionMethod::Stored),
                ("a///", b"x".as_slice(), CompressionMethod::Stored),
            ],
        ]
        .into_iter()
        .enumerate()
        {
            let output = sentinel_output();
            let candidate = archive(&entries);
            assert!(
                candidate.extract(output.path(), limits()).is_err(),
                "case {case}"
            );
            assert_eq!(
                std::fs::read(output.path().join("sentinel")).unwrap(),
                b"old"
            );
        }
    }

    #[test]
    fn hostile_names_preserve_sentinel() {
        let mut names = vec![
            "/absolute",
            "../parent",
            "./leading",
            "a/./interior",
            "a/../interior",
            "trailing/.",
            "trailing/..",
            "a//b",
            "a\\b",
            "C:drive",
            "\\\\server",
            "control\x1b",
            "trailing.",
            "trailing ",
            "CON",
            "PRN",
            "AUX",
            "NUL",
        ];
        names.extend([
            "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8", "COM9", "LPT1", "LPT2",
            "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
        ]);
        for name in names {
            let output = sentinel_output();
            assert_sentinel_unchanged(
                &archive(&[
                    ("sentinel", b"new", CompressionMethod::Stored),
                    (name, b"bad", CompressionMethod::Stored),
                ]),
                &output,
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn overwriting_hard_link_replaces_it_without_mutating_peer() {
        let output = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let peer = outside.path().join("peer");
        std::fs::write(&peer, "old").unwrap();
        std::fs::hard_link(&peer, output.path().join("file")).unwrap();
        archive(&[("file", b"new", CompressionMethod::Stored)])
            .extract(output.path(), limits())
            .unwrap();
        assert_eq!(std::fs::read(output.path().join("file")).unwrap(), b"new");
        assert_eq!(std::fs::read(peer).unwrap(), b"old");
    }

    #[cfg(unix)]
    #[test]
    fn publish_replaces_raced_link_without_following_it() {
        let output = tempfile::tempdir().unwrap();
        let root = output_root(output.path()).unwrap();
        let outside = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(outside.path(), "outside").unwrap();
        let (temp, mut file) = create_temp_file(&root).unwrap();
        file.write_all(b"new").unwrap();
        drop(file);
        symlink(outside.path(), output.path().join("file")).unwrap();
        publish_file(&root, &temp, std::ffi::OsStr::new("file")).unwrap();
        assert_eq!(std::fs::read(output.path().join("file")).unwrap(), b"new");
        assert_eq!(std::fs::read(outside.path()).unwrap(), b"outside");
    }

    #[test]
    fn write_temp_file_removes_temp_on_writer_and_publish_errors() {
        let output = tempfile::tempdir().unwrap();
        let root = output_root(output.path()).unwrap();
        for publisher in [false, true] {
            let result = write_temp_file(
                &root,
                std::ffi::OsStr::new("file"),
                |file| {
                    file.write_all(b"partial").unwrap();
                    if publisher {
                        Ok(())
                    } else {
                        Err("writer failed".into())
                    }
                },
                |_, _, _| Err("publish failed".into()),
            );
            assert!(result.is_err());
            assert!(std::fs::read_dir(output.path()).unwrap().all(|entry| {
                !entry
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".mineru-extract-")
            }));
        }
    }

    #[test]
    fn relative_capability_read_and_write_are_bounded_and_atomic() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("nested")).unwrap();
        std::fs::write(root.path().join("nested/input"), b"abc").unwrap();
        assert_eq!(
            read_relative_capped(root.path(), Path::new("nested/input"), 3).unwrap(),
            b"abc"
        );
        assert!(read_relative_capped(root.path(), Path::new("nested/input"), 2).is_err());
        assert!(read_relative_capped(root.path(), Path::new("nested/input"), 0).is_err());
        std::fs::write(root.path().join("nested/empty"), b"").unwrap();
        assert_eq!(
            read_relative_capped(root.path(), Path::new("nested/empty"), 0).unwrap(),
            b""
        );
        assert!(read_relative_capped(root.path(), Path::new("missing/file"), 1).is_err());
        assert!(!root.path().join("missing").exists());
        write_relative_atomic(root.path(), Path::new("nested/output"), b"one").unwrap();
        write_relative_atomic(root.path(), Path::new("nested/output"), b"two").unwrap();
        assert_eq!(
            std::fs::read(root.path().join("nested/output")).unwrap(),
            b"two"
        );
        assert!(
            !std::fs::read_dir(root.path().join("nested"))
                .unwrap()
                .any(|entry| entry
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".mineru-extract-"))
        );
    }

    #[test]
    fn relative_capabilities_never_create_roots_or_accept_unsafe_paths() {
        let holder = tempfile::tempdir().unwrap();
        let missing = holder.path().join("missing-root");
        for path in [
            Path::new("/absolute"),
            Path::new("../parent"),
            Path::new("."),
            Path::new(""),
        ] {
            assert!(read_relative_capped(holder.path(), path, 1).is_err());
            assert!(write_relative_atomic(holder.path(), path, b"x").is_err());
        }
        assert!(read_relative_capped(&missing, Path::new("x"), 1).is_err());
        assert!(write_relative_atomic(&missing, Path::new("x"), b"x").is_err());
        assert!(!missing.exists());
    }

    #[cfg(unix)]
    #[test]
    fn relative_capabilities_reject_links_and_replace_final_link() {
        use std::os::unix::fs::symlink;
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("dir")).unwrap();
        std::fs::write(outside.path().join("target"), b"outside").unwrap();
        symlink(outside.path(), root.path().join("linkdir")).unwrap();
        symlink(outside.path().join("target"), root.path().join("dir/link")).unwrap();
        assert!(read_relative_capped(root.path(), Path::new("linkdir/target"), 10).is_err());
        assert!(read_relative_capped(root.path(), Path::new("dir/link"), 10).is_err());
        write_relative_atomic(root.path(), Path::new("dir/link"), b"replacement").unwrap();
        assert_eq!(
            std::fs::read(outside.path().join("target")).unwrap(),
            b"outside"
        );
        assert_eq!(
            std::fs::read(root.path().join("dir/link")).unwrap(),
            b"replacement"
        );
        std::fs::create_dir(root.path().join("dir/folder")).unwrap();
        assert!(write_relative_atomic(root.path(), Path::new("dir/folder"), b"x").is_err());
        let _socket =
            std::os::unix::net::UnixListener::bind(root.path().join("dir/socket")).unwrap();
        assert!(write_relative_atomic(root.path(), Path::new("dir/socket"), b"x").is_err());
        assert!(
            !std::fs::read_dir(root.path().join("dir"))
                .unwrap()
                .any(|entry| entry
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".mineru-extract-"))
        );
    }

    #[test]
    fn rejects_symlink_and_special_unix_entry_types_before_writing() {
        for mode in [0o120777, 0o010644] {
            let temp = NamedTempFile::new().unwrap();
            let mut writer = ZipWriter::new(temp.reopen().unwrap());
            writer
                .start_file("sentinel", SimpleFileOptions::default())
                .unwrap();
            writer.write_all(b"new").unwrap();
            writer
                .start_file("bad", SimpleFileOptions::default().unix_permissions(mode))
                .unwrap();
            writer.write_all(b"bad").unwrap();
            writer.finish().unwrap();
            let mut bytes = std::fs::read(temp.path()).unwrap();
            patch_u32(&mut bytes, b"PK\x01\x02", 38, mode << 16);
            std::fs::write(temp.path(), bytes).unwrap();
            let output = sentinel_output();
            assert_sentinel_unchanged(&DownloadedZip(temp), &output);
        }
    }

    #[test]
    fn max_entries_boundary_and_plus_one() {
        let entries = [
            ("one", b"a".as_slice(), CompressionMethod::Stored),
            ("two", b"b".as_slice(), CompressionMethod::Stored),
        ];
        let mut exact = limits();
        exact.max_entries = 2;
        assert!(
            archive(&entries)
                .extract(tempfile::tempdir().unwrap().path(), exact)
                .is_ok()
        );
        let mut over = exact;
        over.max_entries = 1;
        assert!(
            archive(&entries)
                .extract(tempfile::tempdir().unwrap().path(), over)
                .is_err()
        );
    }

    #[test]
    fn max_entry_bytes_boundary_and_plus_one() {
        let archive = archive(&[("file", b"abc", CompressionMethod::Stored)]);
        let mut exact = limits();
        exact.max_entry_bytes = 3;
        assert!(
            archive
                .extract(tempfile::tempdir().unwrap().path(), exact)
                .is_ok()
        );
        exact.max_entry_bytes = 2;
        assert!(
            archive
                .extract(tempfile::tempdir().unwrap().path(), exact)
                .is_err()
        );
    }

    #[test]
    fn max_expanded_bytes_boundary_and_plus_one() {
        let archive = archive(&[
            ("one", b"abc".as_slice(), CompressionMethod::Stored),
            ("two", b"de".as_slice(), CompressionMethod::Stored),
        ]);
        let mut exact = limits();
        exact.max_expanded_bytes = 5;
        assert!(
            archive
                .extract(tempfile::tempdir().unwrap().path(), exact)
                .is_ok()
        );
        exact.max_expanded_bytes = 4;
        assert!(
            archive
                .extract(tempfile::tempdir().unwrap().path(), exact)
                .is_err()
        );
    }

    #[test]
    fn archive_limit_failure_publishes_nothing() {
        let output = sentinel_output();
        let candidate = archive(&[
            ("sentinel", b"new", CompressionMethod::Stored),
            ("new", b"too large", CompressionMethod::Stored),
        ]);
        let mut limited = limits();
        limited.max_expanded_bytes = 3;

        assert!(candidate.extract(output.path(), limited).is_err());
        assert_eq!(
            std::fs::read(output.path().join("sentinel")).unwrap(),
            b"old"
        );
        assert!(!output.path().join("new").exists());
        assert!(transaction_artifacts(output.path()).is_empty());
    }

    #[test]
    fn max_compressed_bytes_boundary_and_plus_one() {
        let archive = archive(&[("file", b"abc", CompressionMethod::Stored)]);
        let mut exact = limits();
        exact.max_compressed_bytes = std::fs::metadata(archive.path()).unwrap().len();
        assert!(
            archive
                .extract(tempfile::tempdir().unwrap().path(), exact)
                .is_ok()
        );
        exact.max_compressed_bytes -= 1;
        assert!(
            archive
                .extract(tempfile::tempdir().unwrap().path(), exact)
                .is_err()
        );
    }

    #[test]
    fn per_entry_ratio_boundary_and_plus_one() {
        let archive = archive(&[("file", &[b'a'; 4096], CompressionMethod::Deflated)]);
        let (_, size, packed) = metadata(&archive);
        let mut exact = limits();
        exact.max_entry_bytes = size;
        exact.max_expanded_bytes = size;
        exact.max_ratio = size.div_ceil(packed);
        let result = archive.extract(tempfile::tempdir().unwrap().path(), exact);
        assert!(result.is_ok(), "{result:?}");
        exact.max_ratio -= 1;
        assert!(
            archive
                .extract(tempfile::tempdir().unwrap().path(), exact)
                .is_err()
        );
    }

    #[test]
    fn ratio_helper_has_exact_boundary_and_plus_one() {
        // Aggregate checks are defense in depth: per-entry checks imply them.
        assert!(!ratio_exceeded(100, 10, 10).unwrap());
        assert!(ratio_exceeded(101, 10, 10).unwrap());
        assert!(ratio_exceeded(1, 0, 10).unwrap());
        assert!(ratio_exceeded(0, 0, 10).unwrap() == false);
    }

    #[test]
    fn truncated_archive_fails_before_sentinel_is_overwritten() {
        let archive = archive(&[("sentinel", b"replacement", CompressionMethod::Stored)]);
        let mut bytes = std::fs::read(archive.path()).unwrap();
        bytes.truncate(bytes.len() - 10);
        std::fs::write(archive.path(), bytes).unwrap();
        let output = sentinel_output();
        assert_sentinel_unchanged(&archive, &output);
    }

    #[test]
    fn scanner_rejection_precedes_destination_creation() {
        let archive = archive(&[("file", b"contents", CompressionMethod::Stored)]);
        let mut bytes = std::fs::read(archive.path()).unwrap();
        bytes.splice(0..0, b"prefix".iter().copied());
        std::fs::write(archive.path(), bytes).unwrap();
        let parent = tempfile::tempdir().unwrap();
        let missing = parent.path().join("missing");
        assert!(archive.extract(&missing, limits()).is_err());
        assert!(!missing.exists());
        let output = sentinel_output();
        assert_sentinel_unchanged(&archive, &output);
    }

    #[test]
    fn corrupt_payload_fails_before_sentinel_is_overwritten() {
        let archive = archive(&[("sentinel", b"replacement", CompressionMethod::Stored)]);
        let mut bytes = std::fs::read(archive.path()).unwrap();
        let position = bytes.windows(11).position(|v| v == b"replacement").unwrap();
        bytes[position] ^= 1;
        std::fs::write(archive.path(), bytes).unwrap();
        let output = sentinel_output();
        assert_sentinel_unchanged(&archive, &output);
    }

    #[test]
    fn forged_size_metadata_fails_before_sentinel_is_overwritten() {
        let archive = archive(&[("sentinel", b"replacement", CompressionMethod::Stored)]);
        let mut bytes = std::fs::read(archive.path()).unwrap();
        patch_u32(&mut bytes, b"PK\x01\x02", 24, 1);
        std::fs::write(archive.path(), bytes).unwrap();
        let output = sentinel_output();
        assert_sentinel_unchanged(&archive, &output);
    }

    #[test]
    fn invalid_or_overflowing_limits_return_errors_without_panicking() {
        let archive = archive(&[("file", b"x", CompressionMethod::Stored)]);
        for invalid in [
            ArchiveLimits {
                max_entries: 0,
                ..limits()
            },
            ArchiveLimits {
                max_compressed_bytes: 0,
                ..limits()
            },
            ArchiveLimits {
                max_expanded_bytes: 0,
                ..limits()
            },
            ArchiveLimits {
                max_entry_bytes: 0,
                ..limits()
            },
            ArchiveLimits {
                max_ratio: 0,
                ..limits()
            },
            ArchiveLimits {
                max_entry_bytes: u64::MAX,
                max_ratio: 2,
                ..limits()
            },
        ] {
            assert!(
                archive
                    .extract(tempfile::tempdir().unwrap().path(), invalid)
                    .is_err()
            );
        }
    }
}
