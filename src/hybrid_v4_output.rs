//! Validation and atomic publication for official MinerU 4.0.0a6 Hybrid output.

use cap_fs_ext::{FollowSymlinks, OpenOptionsFollowExt};
use cap_std::fs::{Dir, File as CapFile, OpenOptions};
use serde_json::Value;
use std::{
    collections::HashSet,
    io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
};

pub(crate) const BUNDLE_NAME: &str = "hybrid-v4";
const SCHEMA_VERSION: &str = "1.0";

struct BundleFile {
    relative: PathBuf,
    size: u64,
    source: CapFile,
}

/// Recounts and validates the child bundle, then installs it as `{stem}/hybrid-v4`.
/// Validation happens before any public target is moved, and a failed rename restores the old
/// target through the same descriptor-relative parent.
pub(crate) fn validate_and_publish(
    bundle: &Path,
    output: &Path,
    stem: &str,
    byte_cap: u64,
) -> Result<(), String> {
    let files = validate_bundle(bundle, byte_cap)?;
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
    }
    let document =
        crate::official_output::open_child_nofollow(root, stem_name).map_err(|e| e.to_string())?;
    let stage_name = unique_name("hybrid-v4-stage");
    document
        .create_dir(&stage_name)
        .map_err(|e| e.to_string())?;
    let stage = crate::official_output::open_child_nofollow(
        document.try_clone().map_err(|e| e.to_string())?,
        &stage_name,
    )
    .map_err(|e| {
        let _ = document.remove_dir_all(&stage_name);
        e.to_string()
    })?;
    if let Err(error) = copy_bundle(files, &stage, byte_cap) {
        drop(stage);
        let _ = document.remove_dir_all(&stage_name);
        return Err(error);
    }
    drop(stage);

    let target_name = std::ffi::OsStr::new(BUNDLE_NAME);
    let target_exists = match document.symlink_metadata(target_name) {
        Ok(meta) if meta.file_type().is_symlink() => {
            let _ = document.remove_dir_all(&stage_name);
            return Err("hybrid-v4 output target is a symlink".into());
        }
        Ok(meta) if !meta.is_dir() => {
            let _ = document.remove_dir_all(&stage_name);
            return Err("hybrid-v4 output target is not a directory".into());
        }
        Ok(_) => true,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => {
            let _ = document.remove_dir_all(&stage_name);
            return Err(error.to_string());
        }
    };
    let backup_name = unique_name("hybrid-v4-backup");
    if target_exists && let Err(error) = document.rename(target_name, &document, &backup_name) {
        let _ = document.remove_dir_all(&stage_name);
        return Err(error.to_string());
    }
    if let Err(error) = document.rename(&stage_name, &document, target_name) {
        let _ = document.remove_dir_all(&stage_name);
        let install_error = error.to_string();
        if target_exists {
            return match document.rename(&backup_name, &document, target_name) {
                Ok(()) => Err(install_error),
                Err(rollback_error) => Err(format!(
                    "hybrid-v4 install failed: {install_error}; restoring previous output failed: {rollback_error}"
                )),
            };
        }
        return Err(install_error);
    }
    if target_exists {
        let _ = document.remove_dir_all(&backup_name);
    }
    Ok(())
}

fn open_bundle(bundle: &Path) -> Result<Dir, String> {
    let parent = bundle
        .parent()
        .ok_or_else(|| "hybrid-v4 bundle has no parent directory".to_owned())?;
    let name = bundle
        .file_name()
        .ok_or_else(|| "hybrid-v4 bundle has no directory name".to_owned())?;
    let parent = crate::official_output::open_or_create_root(parent).map_err(|e| e.to_string())?;
    crate::official_output::open_child_nofollow(parent, name)
        .map_err(|e| format!("hybrid-v4 bundle is not a regular directory: {e}"))
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

fn validate_bundle(bundle: &Path, byte_cap: u64) -> Result<Vec<BundleFile>, String> {
    if byte_cap == 0 {
        return Err("hybrid-v4 bundle byte cap must be positive".into());
    }
    let bundle = open_bundle(bundle)?;
    let mut files = Vec::new();
    let mut names = HashSet::new();
    let mut required = HashSet::from([
        "markdown.md".to_owned(),
        "middle_json.json".to_owned(),
        "content_list.json".to_owned(),
        "structured_content.json".to_owned(),
    ]);
    for entry in bundle.read_dir(".").map_err(|e| e.to_string())? {
        let entry = entry.map_err(|e| e.to_string())?;
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| "hybrid-v4 bundle has a non-UTF-8 name".to_owned())?;
        if name == "images" {
            let images = crate::official_output::open_child_nofollow(
                bundle.try_clone().map_err(|e| e.to_string())?,
                &name,
            )
            .map_err(|e| format!("hybrid-v4 images is not a regular directory: {e}"))?;
            walk_images(
                &images,
                Path::new("images"),
                &mut files,
                &mut names,
                byte_cap,
            )?;
        } else if matches!(
            name.as_str(),
            "markdown.md"
                | "middle_json.json"
                | "content_list.json"
                | "structured_content.json"
                | "model_output.json"
        ) {
            let mut source = open_file_nofollow(&bundle, std::ffi::OsStr::new(&name))?;
            let bytes = read_bounded(&mut source, byte_cap)?;
            if name == "markdown.md" {
                std::str::from_utf8(&bytes)
                    .map_err(|_| "hybrid-v4 markdown.md is not UTF-8".to_owned())?;
            } else {
                let value: Value = serde_json::from_slice(&bytes)
                    .map_err(|e| format!("hybrid-v4 {name} is invalid JSON: {e}"))?;
                validate_json(&name, &value)?;
            }
            add_file(
                PathBuf::from(&name),
                bytes.len() as u64,
                source,
                &mut files,
                &mut names,
                byte_cap,
            )?;
            required.remove(&name);
        } else {
            return Err(format!("unknown hybrid-v4 bundle entry: {name}"));
        }
    }
    if let Some(name) = required.into_iter().next() {
        return Err(format!("hybrid-v4 bundle is missing {name}"));
    }
    Ok(files)
}

fn walk_images(
    directory: &Dir,
    prefix: &Path,
    files: &mut Vec<BundleFile>,
    names: &mut HashSet<String>,
    byte_cap: u64,
) -> Result<(), String> {
    for entry in directory.read_dir(".").map_err(|e| e.to_string())? {
        let entry = entry.map_err(|e| e.to_string())?;
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| "hybrid-v4 image has a non-UTF-8 name".to_owned())?;
        if !portable_name(&name) {
            return Err(format!("unsafe hybrid-v4 image name: {name}"));
        }
        let relative = prefix.join(&name);
        let meta = directory
            .symlink_metadata(&name)
            .map_err(|e| e.to_string())?;
        if meta.file_type().is_symlink() {
            return Err(format!(
                "hybrid-v4 image is a symlink: {}",
                relative.display()
            ));
        }
        if meta.is_dir() {
            let child = crate::official_output::open_child_nofollow(
                directory.try_clone().map_err(|e| e.to_string())?,
                &name,
            )
            .map_err(|e| e.to_string())?;
            walk_images(&child, &relative, files, names, byte_cap)?;
        } else if meta.is_file() {
            let source = open_file_nofollow(directory, std::ffi::OsStr::new(&name))?;
            let size = source.metadata().map_err(|e| e.to_string())?.len();
            add_file(relative, size, source, files, names, byte_cap)?;
        } else {
            return Err(format!(
                "hybrid-v4 image is not a regular file: {}",
                relative.display()
            ));
        }
    }
    Ok(())
}

fn add_file(
    relative: PathBuf,
    size: u64,
    source: CapFile,
    files: &mut Vec<BundleFile>,
    names: &mut HashSet<String>,
    byte_cap: u64,
) -> Result<(), String> {
    let portable = relative
        .components()
        .map(|component| match component {
            std::path::Component::Normal(name) => name
                .to_str()
                .filter(|name| portable_name(name))
                .map(str::to_ascii_lowercase)
                .ok_or_else(|| "unsafe hybrid-v4 portable name".to_owned()),
            _ => Err("unsafe hybrid-v4 relative path".to_owned()),
        })
        .collect::<Result<Vec<_>, _>>()?
        .join("/");
    if !names.insert(portable) {
        return Err(format!(
            "duplicate hybrid-v4 portable name: {}",
            relative.display()
        ));
    }
    let current = files
        .iter()
        .try_fold(0u64, |total, file| total.checked_add(file.size));
    let total = current
        .and_then(|total| total.checked_add(size))
        .ok_or_else(|| "hybrid-v4 bundle byte count overflow".to_owned())?;
    if total > byte_cap {
        return Err(format!("hybrid-v4 bundle exceeds {byte_cap} bytes"));
    }
    files.push(BundleFile {
        relative,
        size,
        source,
    });
    Ok(())
}

fn validate_json(name: &str, value: &Value) -> Result<(), String> {
    if name == "middle_json.json" {
        let object = value
            .as_object()
            .ok_or_else(|| "hybrid-v4 middle_json.json is not an object".to_owned())?;
        if object.get("schema_version").and_then(Value::as_str) != Some(SCHEMA_VERSION) {
            return Err("hybrid-v4 schema is not 1.0".into());
        }
        if object.get("_backend").and_then(Value::as_str) != Some("hybrid") {
            return Err("hybrid-v4 backend is not hybrid".into());
        }
        let pages = object
            .get("pages")
            .and_then(Value::as_array)
            .ok_or_else(|| "hybrid-v4 middle_json.json has no pages".to_owned())?;
        if pages.is_empty() || pages.iter().any(Value::is_null) {
            return Err("hybrid-v4 bundle contains empty pages".into());
        }
    } else if name == "model_output.json" && value.as_array().is_some_and(Vec::is_empty) {
        return Err("hybrid-v4 model output has empty pages".into());
    } else if name == "structured_content.json"
        && (value.as_array().is_some_and(Vec::is_empty)
            || value
                .get("pages")
                .and_then(Value::as_array)
                .is_some_and(Vec::is_empty))
    {
        return Err("hybrid-v4 structured content has empty pages".into());
    }
    Ok(())
}

fn copy_bundle(files: Vec<BundleFile>, stage: &Dir, byte_cap: u64) -> Result<(), String> {
    let mut total = 0u64;
    for file in files {
        let BundleFile {
            relative,
            size: expected_size,
            mut source,
        } = file;
        let (parent, name) = destination_parent(stage, &relative)?;
        source.seek(SeekFrom::Start(0)).map_err(|e| e.to_string())?;
        let mut output = parent.create(name).map_err(|e| e.to_string())?;
        let mut buffer = [0u8; 64 * 1024];
        let mut size = 0u64;
        loop {
            let read = source.read(&mut buffer).map_err(|e| e.to_string())?;
            if read == 0 {
                break;
            }
            size = size
                .checked_add(read as u64)
                .ok_or_else(|| "hybrid-v4 file size overflow".to_owned())?;
            total = total
                .checked_add(read as u64)
                .ok_or_else(|| "hybrid-v4 bundle byte count overflow".to_owned())?;
            if size > expected_size || total > byte_cap {
                return Err("hybrid-v4 bundle changed or exceeds its byte cap".into());
            }
            std::io::Write::write_all(&mut output, &buffer[..read]).map_err(|e| e.to_string())?;
        }
        if size != expected_size {
            return Err("hybrid-v4 bundle changed while publishing".into());
        }
    }
    Ok(())
}

fn destination_parent(stage: &Dir, relative: &Path) -> Result<(Dir, std::ffi::OsString), String> {
    let mut components = relative.components();
    let filename = match components.next_back() {
        Some(std::path::Component::Normal(name)) => name.to_owned(),
        _ => return Err("invalid hybrid-v4 destination".into()),
    };
    let mut current = stage.try_clone().map_err(|e| e.to_string())?;
    for component in components {
        let std::path::Component::Normal(name) = component else {
            return Err("invalid hybrid-v4 destination".into());
        };
        match current.symlink_metadata(name) {
            Ok(meta) if meta.file_type().is_symlink() || !meta.is_dir() => {
                return Err("hybrid-v4 destination parent is not a directory".into());
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                current.create_dir(name).map_err(|e| e.to_string())?;
            }
            Err(error) => return Err(error.to_string()),
        }
        current = crate::official_output::open_child_nofollow(current, name)
            .map_err(|e| e.to_string())?;
    }
    Ok((current, filename))
}

fn read_bounded(input: &mut impl Read, cap: u64) -> Result<Vec<u8>, String> {
    let mut bytes = Vec::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = input.read(&mut buffer).map_err(|e| e.to_string())?;
        if read == 0 {
            return Ok(bytes);
        }
        if (bytes.len() as u64).saturating_add(read as u64) > cap {
            return Err(format!("hybrid-v4 bundle exceeds {cap} bytes"));
        }
        bytes.extend_from_slice(&buffer[..read]);
    }
}

fn portable_name(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && !name.ends_with(['.', ' '])
        && name
            .chars()
            .all(|c| !c.is_control() && c != '/' && c != '\\')
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

fn unique_name(prefix: &str) -> std::ffi::OsString {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    format!(
        ".{prefix}-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    )
    .into()
}

#[cfg(test)]
mod tests;
