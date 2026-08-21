//! Validation and atomic publication for official MinerU 4.0.0a6 Hybrid output.
use cap_fs_ext::{FollowSymlinks, OpenOptionsFollowExt};
use cap_std::fs::{Dir, File as CapFile, OpenOptions};
use serde_json::Value;
use std::{
    collections::HashSet,
    io::{Read, Write},
    path::Path,
};
pub(crate) const BUNDLE_NAME: &str = "hybrid-v4";
const SCHEMA_VERSION: &str = "1.0";
const MAX_ENTRIES: u64 = 8_192;
const MAX_DEPTH: u32 = 32;
const MAX_COMPONENT_BYTES: u64 = 255;
const MAX_RELATIVE_PATH_BYTES: u64 = 4_096;
const MAX_NAME_BUDGET: u64 = 32 * 1024 * 1024;
const MAX_RESIDENT_BYTES: u64 = 64 * 1024 * 1024;
const COPY_BUFFER_BYTES: usize = 64 * 1024;
pub(crate) fn validate_and_publish(
    bundle: &Path,
    output: &Path,
    stem: &str,
    byte_cap: u64,
) -> Result<(), String> {
    if byte_cap == 0 {
        return Err("hybrid-v4 bundle byte cap must be positive".into());
    }
    let root = crate::official_output::open_or_create_root(output).map_err(|e| e.to_string())?;
    let stem_name = std::ffi::OsStr::new(stem);
    match root.symlink_metadata(stem_name) {
        Ok(meta) if meta.file_type().is_symlink() => {
            return Err("hybrid-v4 document stem is a symlink".into());
        }
        Ok(meta) if !meta.is_dir() => {
            return Err("hybrid-v4 document stem is not a directory".into());
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            root.create_dir(stem_name).map_err(|e| e.to_string())?;
        }
        Err(error) => return Err(error.to_string()),
    };
    let document = open_child(&root, stem_name)?;
    let (transaction_name, transaction, stage) = create_transaction(&document)?;
    let transaction_path = output.join(stem).join(&transaction_name);
    if let Err(error) =
        copy_bundle(bundle, &stage, byte_cap).and_then(|()| validate_staged(&stage, byte_cap))
    {
        drop(stage);
        return Err(cleanup_failure(
            error,
            cleanup_transaction(&document, &transaction_name, transaction),
            &transaction_path,
        ));
    }
    drop(stage);
    finish_transaction(
        &document,
        &transaction_name,
        transaction,
        &transaction_path,
        false,
    )
}
fn create_transaction(document: &Dir) -> Result<(std::ffi::OsString, Dir, Dir), String> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    for _ in 0..64 {
        #[rustfmt::skip]
        let serial = NEXT.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |value| value.checked_add(1)).map_err(|_| "hybrid-v4 stage counter overflow".to_owned())?;
        let transaction_name: std::ffi::OsString =
            format!(".hybrid-v4-stage-{}-{serial}", std::process::id()).into();
        match create_private_dir(document, &transaction_name) {
            Ok(()) => match open_transaction(document, &transaction_name) {
                Ok((transaction, stage)) => return Ok((transaction_name, transaction, stage)),
                Err(error) => {
                    return match document.remove_dir_all(&transaction_name) {
                        Ok(()) => Err(error),
                        Err(cleanup) => Err(format!(
                            "{error}; hybrid-v4 transaction cleanup failed: {cleanup}"
                        )),
                    };
                }
            },
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error.to_string()),
        }
    }
    Err("unable to create a collision-safe hybrid-v4 stage".into())
}
fn open_transaction(document: &Dir, name: &std::ffi::OsStr) -> Result<(Dir, Dir), String> {
    let transaction = open_child(document, name)?;
    transaction.create_dir("stage").map_err(|e| e.to_string())?;
    let stage = open_child(&transaction, "stage")?;
    Ok((transaction, stage))
}
#[cfg(unix)]
#[rustfmt::skip]
fn create_private_dir(parent: &Dir, name: &std::ffi::OsStr) -> std::io::Result<()> { use cap_std::fs::DirBuilderExt; let mut builder = cap_std::fs::DirBuilder::new(); builder.mode(0o700); parent.create_dir_with(name, &builder) }
#[cfg(not(unix))]
#[rustfmt::skip]
fn create_private_dir(parent: &Dir, name: &std::ffi::OsStr) -> std::io::Result<()> { parent.create_dir(name) }
fn remove_private_entry(parent: &Dir, name: &str) -> Result<(), String> {
    match parent.remove_dir_all(name) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.to_string()),
    }
}
fn cleanup_stage_only(transaction: &Dir) -> Result<(), String> {
    remove_private_entry(transaction, "stage")
}
fn cleanup_transaction(
    document: &Dir,
    name: &std::ffi::OsStr,
    transaction: Dir,
) -> Result<(), String> {
    let mut errors = Vec::new();
    for entry in ["stage", "backup"] {
        if let Err(error) = remove_private_entry(&transaction, entry) {
            errors.push(format!("{entry}: {error}"));
        }
    }
    drop(transaction);
    if let Err(error) = document.remove_dir(name)
        && error.kind() != std::io::ErrorKind::NotFound
    {
        errors.push(format!("transaction: {error}"));
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("; "))
    }
}
fn cleanup_failure(message: String, cleanup: Result<(), String>, path: &Path) -> String {
    match cleanup {
        Ok(()) => message,
        Err(cleanup) => format!(
            "{message}; hybrid-v4 transaction cleanup failed at {}: {cleanup}",
            path.display()
        ),
    }
}
type PublishFailure = (String, bool);
fn publish_transaction_inner(
    document: &Dir,
    transaction: &Dir,
    stage_name: &str,
    target_name: &str,
    _force_rollback_failure: bool,
) -> Result<(), PublishFailure> {
    let target_exists = match document.symlink_metadata(target_name) {
        Ok(meta) if meta.file_type().is_symlink() => {
            return Err(("hybrid-v4 output target is a symlink".into(), false));
        }
        Ok(meta) if !meta.is_dir() => {
            return Err(("hybrid-v4 output target is not a directory".into(), false));
        }
        Ok(_) => true,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => return Err((error.to_string(), false)),
    };
    let backup_name = std::ffi::OsStr::new("backup");
    if target_exists {
        document
            .rename(target_name, transaction, backup_name)
            .map_err(|e| (e.to_string(), false))?;
        #[cfg(test)]
        if _force_rollback_failure {
            let mut options = OpenOptions::new();
            options
                .write(true)
                .create_new(true)
                .follow(FollowSymlinks::No);
            let _conflict = document
                .open_with(target_name, &options)
                .expect("rollback-failure conflict target");
        }
    }
    if let Err(error) = transaction.rename(stage_name, document, target_name) {
        let install_error = error.to_string();
        if target_exists {
            return match transaction.rename(backup_name, document, target_name) {
                Ok(()) => Err((install_error, false)),
                Err(rollback_error) => Err((
                    format!(
                        "hybrid-v4 install failed: {install_error}; restoring previous output failed: {rollback_error}"
                    ),
                    true,
                )),
            };
        }
        return Err((install_error, false));
    }
    Ok(())
}
fn finish_transaction(
    document: &Dir,
    name: &std::ffi::OsStr,
    transaction: Dir,
    transaction_path: &Path,
    force_rollback_failure: bool,
) -> Result<(), String> {
    match publish_transaction_inner(
        document,
        &transaction,
        "stage",
        BUNDLE_NAME,
        force_rollback_failure,
    ) {
        Ok(()) => match cleanup_transaction(document, name, transaction) {
            Ok(()) => Ok(()),
            Err(cleanup) => Err(format!(
                "hybrid-v4 published successfully; transaction cleanup failed at {}: {cleanup}",
                transaction_path.display()
            )),
        },
        Err((message, true)) => {
            let stage_cleanup = match cleanup_stage_only(&transaction) {
                Ok(()) => "staging directory cleaned".to_owned(),
                Err(error) => format!("staging cleanup failed: {error}"),
            };
            drop(transaction);
            Err(format!(
                "{message}; transaction and backup preserved at {} ({stage_cleanup})",
                transaction_path.display()
            ))
        }
        Err((message, false)) => Err(cleanup_failure(
            message,
            cleanup_transaction(document, name, transaction),
            transaction_path,
        )),
    }
}
fn open_bundle(bundle: &Path) -> Result<Dir, String> {
    let parent = bundle
        .parent()
        .ok_or_else(|| "hybrid-v4 bundle has no parent directory".to_owned())?;
    let name = bundle
        .file_name()
        .ok_or_else(|| "hybrid-v4 bundle has no directory name".to_owned())?;
    let parent = crate::official_output::open_or_create_root(parent).map_err(|e| e.to_string())?;
    open_child(&parent, name)
        .map_err(|e| format!("hybrid-v4 bundle is not a regular directory: {e}"))
}
fn open_child(directory: &Dir, name: impl AsRef<Path>) -> Result<Dir, String> {
    crate::official_output::open_child_nofollow(
        directory.try_clone().map_err(|e| e.to_string())?,
        name,
    )
    .map_err(|e| e.to_string())
}
fn open_file_nofollow(directory: &Dir, name: &std::ffi::OsStr) -> Result<CapFile, String> {
    let mut options = OpenOptions::new();
    options.read(true).follow(FollowSymlinks::No);
    let file = directory
        .open_with(name, &options)
        .map_err(|e| e.to_string())?;
    if !file.metadata().map_err(|e| e.to_string())?.is_file() {
        return Err("hybrid-v4 bundle entry is not a regular file".into());
    }
    Ok(file)
}
#[derive(Clone)]
struct RelativePath {
    depth: u32,
    bytes: u64,
    portable: String,
}
impl RelativePath {
    fn root() -> Self {
        Self {
            depth: 0,
            bytes: 0,
            portable: String::new(),
        }
    }
    fn child(&self, name: &str) -> Result<Self, String> {
        if !portable_name(name) {
            return Err(format!("unsafe hybrid-v4 name: {name}"));
        }
        let component_bytes = u64::try_from(name.len())
            .map_err(|_| "hybrid-v4 component size overflow".to_owned())?;
        let depth = self
            .depth
            .checked_add(1)
            .ok_or_else(|| "hybrid-v4 path depth overflow".to_owned())?;
        let bytes = self
            .bytes
            .checked_add(component_bytes)
            .and_then(|bytes| bytes.checked_add(if self.depth > 0 { 1 } else { 0 }))
            .ok_or_else(|| "hybrid-v4 relative path size overflow".to_owned())?;
        if component_bytes > MAX_COMPONENT_BYTES
            || depth > MAX_DEPTH
            || bytes > MAX_RELATIVE_PATH_BYTES
        {
            return Err("hybrid-v4 path component/depth/length cap exceeded".into());
        }
        let lower = name.to_lowercase();
        let portable = if self.portable.is_empty() {
            lower
        } else {
            format!("{}/{}", self.portable, lower)
        };
        Ok(Self {
            depth,
            bytes,
            portable,
        })
    }
}
#[derive(Default)]
struct TreeState {
    entries: u64,
    name_bytes: u64,
    data_bytes: u64,
    names: HashSet<String>,
}

impl TreeState {
    fn admit(&mut self, parent: &RelativePath, name: &str) -> Result<RelativePath, String> {
        let relative = parent.child(name)?;
        let entries = self
            .entries
            .checked_add(1)
            .ok_or_else(|| "hybrid-v4 entry count overflow".to_owned())?;
        let name_bytes = self
            .name_bytes
            .checked_add(relative.bytes)
            .ok_or_else(|| "hybrid-v4 relative-name budget overflow".to_owned())?;
        if entries > MAX_ENTRIES || name_bytes > MAX_NAME_BUDGET {
            return Err("hybrid-v4 entry/name cap exceeded".into());
        }
        self.names
            .try_reserve(1)
            .map_err(|_| "hybrid-v4 portable-name set allocation failed".to_owned())?;
        if !self.names.insert(relative.portable.clone()) {
            return Err(format!(
                "duplicate hybrid-v4 portable name: {}",
                relative.portable
            ));
        }
        self.entries = entries;
        self.name_bytes = name_bytes;
        Ok(relative)
    }

    fn charge_data(&mut self, amount: u64, byte_cap: u64) -> Result<(), String> {
        self.data_bytes = self
            .data_bytes
            .checked_add(amount)
            .ok_or_else(|| "hybrid-v4 bundle byte count overflow".to_owned())?;
        if self.data_bytes > byte_cap {
            return Err(format!("hybrid-v4 bundle exceeds {byte_cap} bytes"));
        }
        Ok(())
    }
}
fn copy_bundle(bundle: &Path, stage: &Dir, byte_cap: u64) -> Result<(), String> {
    let source = open_bundle(bundle)?;
    let mut state = TreeState::default();
    walk_directory(
        &source,
        stage,
        &RelativePath::root(),
        &mut state,
        byte_cap,
        true,
    )
}
fn walk_directory(
    source: &Dir,
    destination: &Dir,
    parent: &RelativePath,
    state: &mut TreeState,
    byte_cap: u64,
    root: bool,
) -> Result<(), String> {
    for entry in source.read_dir(".").map_err(|e| e.to_string())? {
        let entry = entry.map_err(|e| e.to_string())?;
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| "hybrid-v4 bundle has a non-UTF-8 name".to_owned())?;
        let relative = state.admit(parent, &name)?;
        let meta = source.symlink_metadata(&name).map_err(|e| e.to_string())?;
        if meta.file_type().is_symlink() {
            return Err(format!("hybrid-v4 bundle entry is a symlink: {name}"));
        }
        if meta.is_dir() {
            if root && name != "images" {
                return Err(format!("unknown hybrid-v4 bundle entry: {name}"));
            }
            let source_child = open_child(source, &name)?;
            let destination_child = create_destination_dir(destination, &name)?;
            walk_directory(
                &source_child,
                &destination_child,
                &relative,
                state,
                byte_cap,
                false,
            )?;
        } else if meta.is_file() {
            if root && !known_file(&name) {
                return Err(format!("unknown hybrid-v4 bundle entry: {name}"));
            }
            copy_file(source, destination, &name, state, byte_cap)?;
        } else {
            return Err(format!(
                "hybrid-v4 bundle entry is not a regular file: {}",
                relative.portable
            ));
        }
    }
    Ok(())
}
fn create_destination_dir(parent: &Dir, name: &str) -> Result<Dir, String> {
    parent.create_dir(name).map_err(|error| {
        if error.kind() == std::io::ErrorKind::AlreadyExists {
            "hybrid-v4 destination directory collision".to_owned()
        } else {
            error.to_string()
        }
    })?;
    open_child(parent, name)
}
fn copy_file(
    source_directory: &Dir,
    destination_directory: &Dir,
    name: &str,
    state: &mut TreeState,
    byte_cap: u64,
) -> Result<(), String> {
    let mut source = open_file_nofollow(source_directory, std::ffi::OsStr::new(name))?;
    let mut options = OpenOptions::new();
    options
        .write(true)
        .create_new(true)
        .follow(FollowSymlinks::No);
    let mut destination = destination_directory
        .open_with(name, &options)
        .map_err(|e| e.to_string())?;
    let mut buffer = [0u8; COPY_BUFFER_BYTES];
    loop {
        let read = source.read(&mut buffer).map_err(|e| e.to_string())?;
        if read == 0 {
            break;
        }
        state.charge_data(
            u64::try_from(read).map_err(|_| "hybrid-v4 read size overflow".to_owned())?,
            byte_cap,
        )?;
        destination
            .write_all(&buffer[..read])
            .map_err(|e| e.to_string())?;
    }
    destination.flush().map_err(|e| e.to_string())?;
    Ok(())
}
fn validate_staged(stage: &Dir, byte_cap: u64) -> Result<(), String> {
    for name in [
        "markdown.md",
        "middle_json.json",
        "content_list.json",
        "structured_content.json",
    ] {
        validate_staged_file(stage, name, byte_cap)?;
    }
    match stage.symlink_metadata("model_output.json") {
        Ok(meta) if meta.file_type().is_symlink() => {
            return Err("hybrid-v4 staged model_output.json is a symlink".into());
        }
        Ok(_) => validate_staged_file(stage, "model_output.json", byte_cap)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.to_string()),
    }
    Ok(()) // ponytail: copy phase owns the bounded structure; reopen only text/JSON.
}
fn known_file(name: &str) -> bool {
    matches!(
        name,
        "markdown.md"
            | "middle_json.json"
            | "content_list.json"
            | "structured_content.json"
            | "model_output.json"
    )
}
fn validate_staged_file(stage: &Dir, name: &str, byte_cap: u64) -> Result<(), String> {
    let mut file = open_file_nofollow(stage, std::ffi::OsStr::new(name))?;
    let size = file.metadata().map_err(|e| e.to_string())?.len();
    validate_known_file(name, &mut file, size, byte_cap)
}
fn validate_known_file(
    name: &str,
    file: &mut CapFile,
    expected_size: u64,
    byte_cap: u64,
) -> Result<(), String> {
    let resident_cap = byte_cap.min(MAX_RESIDENT_BYTES);
    if expected_size > resident_cap {
        return Err(format!(
            "hybrid-v4 {name} exceeds resident validation limit {resident_cap} bytes"
        ));
    }
    let bytes = read_bounded(file, resident_cap)?;
    let actual_size =
        u64::try_from(bytes.len()).map_err(|_| "hybrid-v4 staged file size overflow".to_owned())?;
    if actual_size != expected_size {
        return Err(format!("hybrid-v4 staged {name} changed while validating"));
    }
    if name == "markdown.md" {
        std::str::from_utf8(&bytes).map_err(|_| "hybrid-v4 markdown.md is not UTF-8".to_owned())?;
    } else {
        let value: Value = serde_json::from_slice(&bytes)
            .map_err(|e| format!("hybrid-v4 {name} is invalid JSON: {e}"))?;
        validate_json(name, &value)?;
    }
    Ok(())
}
fn validate_json(name: &str, value: &Value) -> Result<(), String> {
    match name {
        "middle_json.json" => {
            let object = value
                .as_object()
                .ok_or_else(|| "hybrid-v4 middle_json.json is not an object".to_owned())?;
            if object.get("schema_version").and_then(Value::as_str) != Some(SCHEMA_VERSION)
                || object.get("_backend").and_then(Value::as_str) != Some("hybrid")
            {
                return Err("hybrid-v4 schema/backend is invalid".into());
            }
            let pages = object
                .get("pages")
                .and_then(Value::as_array)
                .ok_or_else(|| "hybrid-v4 middle_json.json has no pages".to_owned())?;
            if pages.is_empty() || pages.iter().any(Value::is_null) {
                return Err("hybrid-v4 bundle contains empty pages".into());
            }
        }
        "model_output.json" if value.as_array().is_some_and(Vec::is_empty) => {
            return Err("hybrid-v4 model output has empty pages".into());
        }
        "structured_content.json"
            if value.as_array().is_some_and(Vec::is_empty)
                || value
                    .get("pages")
                    .and_then(Value::as_array)
                    .is_some_and(Vec::is_empty) =>
        {
            return Err("hybrid-v4 structured content has empty pages".into());
        }
        _ => {}
    }
    Ok(())
}
fn read_bounded(input: &mut impl Read, cap: u64) -> Result<Vec<u8>, String> {
    let mut bytes = Vec::new();
    let mut buffer = [0u8; COPY_BUFFER_BYTES];
    loop {
        let read = input.read(&mut buffer).map_err(|e| e.to_string())?;
        if read == 0 {
            return Ok(bytes);
        }
        let read = u64::try_from(read).map_err(|_| "hybrid-v4 read size overflow".to_owned())?;
        let current = u64::try_from(bytes.len())
            .map_err(|_| "hybrid-v4 resident buffer size overflow".to_owned())?;
        let next = current
            .checked_add(read)
            .ok_or_else(|| "hybrid-v4 resident buffer size overflow".to_owned())?;
        if next > cap {
            return Err(format!("hybrid-v4 bundle exceeds {cap} bytes"));
        }
        let read = usize::try_from(read).map_err(|_| "hybrid-v4 read size overflow".to_owned())?;
        bytes
            .try_reserve(read)
            .map_err(|_| "hybrid-v4 resident buffer allocation failed".to_owned())?;
        bytes.extend_from_slice(&buffer[..read]);
    }
}
fn portable_name(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && !name.ends_with(['.', ' '])
        && name.chars().all(|c| {
            !c.is_control() && !matches!(c, '/' | '\\' | '<' | '>' | ':' | '"' | '|' | '?' | '*')
        })
        && !windows_device_name(name)
}
fn windows_device_name(name: &str) -> bool {
    let base = name.split('.').next().unwrap_or_default();
    base.eq_ignore_ascii_case("con")
        || base.eq_ignore_ascii_case("prn")
        || base.eq_ignore_ascii_case("aux")
        || base.eq_ignore_ascii_case("nul")
        || (base.len() == 4
            && (base.as_bytes()[..3].eq_ignore_ascii_case(b"com")
                || base.as_bytes()[..3].eq_ignore_ascii_case(b"lpt"))
            && matches!(base.as_bytes()[3], b'1'..=b'9'))
}
#[cfg(test)]
mod tests;
