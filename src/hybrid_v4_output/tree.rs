//! Tree copy, staging, and JSON validation for hybrid-v4 bundles.
use super::{SCHEMA_VERSION, open_child};
use cap_fs_ext::{FollowSymlinks, OpenOptionsFollowExt};
use cap_std::fs::{Dir, File as CapFile, OpenOptions};
use serde_json::Value;
use std::{
    collections::HashSet,
    io::{Read, Write},
    path::Path,
};

pub(crate) const MAX_COMPONENT_BYTES: u64 = 255;
pub(crate) const MAX_DEPTH: u32 = 32;
const MAX_RELATIVE_PATH_BYTES: u64 = 4_096;
pub(crate) const MAX_NAME_BUDGET: u64 = 32 * 1024 * 1024;
pub(crate) const MAX_RESIDENT_BYTES: u64 = 64 * 1024 * 1024;
pub(crate) const MAX_ENTRIES: u64 = 8_192;
const COPY_BUFFER_BYTES: usize = 64 * 1024;

#[derive(Clone)]
pub(super) struct RelativePath {
    depth: u32,
    bytes: u64,
    portable: String,
}

impl RelativePath {
    pub(super) fn root() -> Self {
        Self {
            depth: 0,
            bytes: 0,
            portable: String::new(),
        }
    }

    pub(super) fn child(&self, name: &str) -> Result<Self, String> {
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
pub(crate) struct TreeState {
    entries: u64,
    pub(crate) name_bytes: u64,
    data_bytes: u64,
    names: HashSet<String>,
}

impl TreeState {
    pub(super) fn admit(
        &mut self,
        parent: &RelativePath,
        name: &str,
    ) -> Result<RelativePath, String> {
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

pub(super) fn copy_bundle(bundle: &Path, stage: &Dir, byte_cap: u64) -> Result<(), String> {
    let source = super::open_bundle(bundle)?;
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
    let mut source = super::open_file_nofollow(source_directory, std::ffi::OsStr::new(name))?;
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

fn known_file(name: &str) -> bool {
    matches!(
        name,
        "markdown.md" | "middle_json.json" | "structured_content.json" | "model_output.json"
    )
}

pub(super) fn validate_staged(stage: &Dir, byte_cap: u64) -> Result<(), String> {
    for name in ["markdown.md", "middle_json.json", "structured_content.json"] {
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

fn validate_staged_file(stage: &Dir, name: &str, byte_cap: u64) -> Result<(), String> {
    let mut file = super::open_file_nofollow(stage, std::ffi::OsStr::new(name))?;
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
            if object.get("schema").and_then(Value::as_str) != Some("docvortex.middle")
                || object.get("schema_version").and_then(Value::as_str) != Some(SCHEMA_VERSION)
            {
                return Err("hybrid-v4 middle_json schema is invalid".into());
            }
            let pages = object
                .get("pages")
                .and_then(Value::as_array)
                .ok_or_else(|| "hybrid-v4 middle_json.json has no pages".to_owned())?;
            // Upstream can legitimately emit a document with zero retained
            // pages, so only reject malformed entries, not an empty list.
            if pages.iter().any(Value::is_null) {
                return Err("hybrid-v4 bundle contains null pages".into());
            }
        }
        // Both remaining documents are objects upstream; an empty array or any
        // other shape is malformed. Their `pages` may legitimately be empty.
        "model_output.json" if !value.is_object() => {
            return Err("hybrid-v4 model output is not an object".into());
        }
        "structured_content.json" if !value.is_object() => {
            return Err("hybrid-v4 structured content is not an object".into());
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

pub(crate) fn portable_name(name: &str) -> bool {
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
