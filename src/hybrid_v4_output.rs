//! Validation and atomic publication for official MinerU 4.0.4 Hybrid output bundles.
pub(crate) const BUNDLE_NAME: &str = "hybrid-v4";
const SCHEMA_VERSION: &str = "2.0";

mod transaction;
mod tree;

use cap_fs_ext::{FollowSymlinks, OpenOptionsFollowExt};
use cap_std::fs::{Dir, File as CapFile, OpenOptions};
use std::path::Path;
use transaction::{cleanup_failure, cleanup_transaction, finish_transaction};
use tree::{copy_bundle, validate_staged};

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
    let (transaction_name, transaction, stage) = transaction::create_transaction(&document)?;
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

#[cfg(test)]
mod tests;
