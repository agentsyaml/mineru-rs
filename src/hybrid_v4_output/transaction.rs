//! Stage transaction lifecycle for hybrid-v4 bundle publication.
use super::{BUNDLE_NAME, open_child};
#[cfg(test)]
use cap_fs_ext::{FollowSymlinks, OpenOptionsFollowExt};
use cap_std::fs::Dir;
#[cfg(test)]
use cap_std::fs::OpenOptions;
use std::path::Path;

pub(super) fn create_transaction(document: &Dir) -> Result<(std::ffi::OsString, Dir, Dir), String> {
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

pub(super) fn cleanup_transaction(
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

pub(super) fn cleanup_failure(message: String, cleanup: Result<(), String>, path: &Path) -> String {
    match cleanup {
        Ok(()) => message,
        Err(cleanup) => format!(
            "{message}; hybrid-v4 transaction cleanup failed at {}: {cleanup}",
            path.display()
        ),
    }
}

type PublishFailure = (String, bool);

pub(crate) fn publish_transaction_inner(
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

pub(super) fn finish_transaction(
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
