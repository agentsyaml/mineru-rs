use crate::{Document, OutputManifest, Result};
use std::{
    collections::HashSet,
    fs,
    path::{Component, Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

static STAGE_COUNTER: AtomicU64 = AtomicU64::new(0);

pub fn write_outputs(document: &Document, directory: impl AsRef<Path>) -> Result<OutputManifest> {
    write_outputs_with(
        document,
        directory.as_ref(),
        |staging, target| fs::rename(staging, target),
        remove_path,
    )
}

fn write_outputs_with(
    document: &Document,
    directory: &Path,
    install_stage: impl FnOnce(&Path, &Path) -> std::io::Result<()>,
    cleanup_backup: impl FnOnce(&Path) -> std::io::Result<()>,
) -> Result<OutputManifest> {
    validate_assets(document)?;
    let parent = directory.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;
    let staging = create_staging(parent)?;
    let result = write_staged(document, &staging);
    if let Err(error) = result {
        let _ = fs::remove_dir_all(&staging);
        return Err(error);
    }
    if !directory.exists() {
        if let Err(error) = fs::rename(&staging, directory) {
            let _ = fs::remove_dir_all(&staging);
            return Err(error.into());
        }
        return Ok(manifest(document, directory));
    }

    let backup = unique_sibling_path(parent, "backup");
    if let Err(error) = fs::rename(directory, &backup) {
        let _ = fs::remove_dir_all(&staging);
        return Err(error.into());
    }
    if let Err(error) = install_stage(&staging, directory) {
        let restore = fs::rename(&backup, directory);
        let _ = fs::remove_dir_all(&staging);
        return Err(restore.err().unwrap_or(error).into());
    }
    // Successful staged install is the commit point; cleanup cannot revoke it.
    let _ = cleanup_backup(&backup);
    Ok(manifest(document, directory))
}

fn remove_path(path: &Path) -> std::io::Result<()> {
    if fs::symlink_metadata(path)?.is_dir() {
        fs::remove_dir_all(path)
    } else {
        fs::remove_file(path)
    }
}

fn create_staging(parent: &Path) -> Result<PathBuf> {
    loop {
        let path = unique_sibling_path(parent, "stage");
        match fs::create_dir(&path) {
            Ok(()) => return Ok(path),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error.into()),
        }
    }
}

fn unique_sibling_path(parent: &Path, kind: &str) -> PathBuf {
    parent.join(format!(
        ".mineru-{kind}-{}-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos(),
        STAGE_COUNTER.fetch_add(1, Ordering::Relaxed),
    ))
}

fn manifest(document: &Document, directory: &Path) -> OutputManifest {
    let document_json = directory.join("document.json");
    let markdown = directory.join("document.md");
    let middle_json = directory.join("middle.json");
    let content_list = directory.join("content_list.json");
    OutputManifest {
        document_json,
        markdown,
        middle_json,
        content_list,
        assets: document
            .assets
            .iter()
            .map(|asset| asset.relative_path.clone())
            .collect(),
    }
}

fn write_staged(document: &Document, directory: &Path) -> Result<()> {
    atomic_write(
        &directory.join("document.json"),
        &serde_json::to_vec_pretty(document)?,
    )?;
    atomic_write(&directory.join("document.md"), document.markdown.as_bytes())?;
    atomic_write(
        &directory.join("middle.json"),
        &serde_json::to_vec_pretty(&document.middle_json)?,
    )?;
    atomic_write(
        &directory.join("content_list.json"),
        &serde_json::to_vec_pretty(&document.content_list)?,
    )?;
    for asset in &document.assets {
        let path = directory.join(&asset.relative_path);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        atomic_write(&path, &asset.data)?;
    }
    Ok(())
}

fn validate_assets(document: &Document) -> Result<()> {
    let mut paths = [
        "document.json",
        "document.md",
        "middle.json",
        "content_list.json",
    ]
    .into_iter()
    .map(PathBuf::from)
    .collect::<HashSet<_>>();
    for asset in &document.assets {
        if asset.relative_path.as_os_str().is_empty()
            || asset.relative_path.is_absolute()
            || asset.relative_path.components().any(|c| {
                matches!(
                    c,
                    Component::ParentDir | Component::RootDir | Component::Prefix(_)
                )
            })
            || !paths.insert(normalize_relative_path(&asset.relative_path))
        {
            return Err(crate::Error::InvalidInput(
                "asset paths must be unique relative paths".into(),
            ));
        }
    }
    Ok(())
}

fn normalize_relative_path(path: &Path) -> PathBuf {
    path.components()
        .filter_map(|component| match component {
            Component::Normal(part) => Some(part),
            Component::CurDir => None,
            _ => None,
        })
        .collect()
}

fn atomic_write(path: &Path, data: &[u8]) -> Result<()> {
    let temporary = path.with_extension(format!(
        "{}.tmp",
        path.extension().and_then(|x| x.to_str()).unwrap_or("")
    ));
    fs::write(&temporary, data)?;
    fs::rename(temporary, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{remove_path, write_outputs, write_outputs_with};
    use crate::{Asset, AssetKind, Document};
    use bytes::Bytes;
    use std::{cell::RefCell, fs, io, path::Path, path::PathBuf};

    fn private_artifacts(parent: &Path) -> Vec<PathBuf> {
        let mut artifacts = fs::read_dir(parent)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| {
                path.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with(".mineru-")
            })
            .collect::<Vec<_>>();
        artifacts.sort();
        artifacts
    }

    fn replacement_document() -> Document {
        Document {
            markdown: "# new".into(),
            assets: vec![Asset {
                kind: AssetKind::Image,
                relative_path: PathBuf::from("assets/image.png"),
                media_type: "image/png".into(),
                data: Bytes::from_static(b"new image"),
                md5: "md5".into(),
            }],
            ..Default::default()
        }
    }

    #[test]
    fn writes_manifest_outputs_and_asset_data() {
        let directory = tempfile::tempdir().unwrap();
        let document = Document {
            markdown: "# document".into(),
            assets: vec![Asset {
                kind: AssetKind::Image,
                relative_path: PathBuf::from("assets/image.png"),
                media_type: "image/png".into(),
                data: Bytes::from_static(b"image"),
                md5: "md5".into(),
            }],
            ..Default::default()
        };

        let manifest = write_outputs(&document, directory.path()).unwrap();
        assert!(manifest.document_json.exists());
        assert!(manifest.markdown.exists());
        assert!(manifest.middle_json.exists());
        assert!(manifest.content_list.exists());
        assert_eq!(manifest.assets, [PathBuf::from("assets/image.png")]);
        assert_eq!(
            fs::read(directory.path().join("assets/image.png")).unwrap(),
            b"image"
        );
    }

    #[test]
    fn rejects_asset_traversal() {
        let directory = tempfile::tempdir().unwrap();
        let document = Document {
            assets: vec![Asset {
                kind: AssetKind::Image,
                relative_path: PathBuf::from("assets/../escape.png"),
                media_type: "image/png".into(),
                data: Bytes::new(),
                md5: String::new(),
            }],
            ..Default::default()
        };
        assert!(write_outputs(&document, directory.path()).is_err());
    }

    #[test]
    fn restores_existing_target_when_staged_install_fails() {
        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join("output");
        fs::create_dir_all(target.join("nested")).unwrap();
        fs::write(target.join("old.txt"), b"old").unwrap();
        fs::write(target.join("nested/only-old.txt"), b"nested old").unwrap();

        let result = write_outputs_with(
            &replacement_document(),
            &target,
            |staging, destination| {
                assert_eq!(destination, target);
                assert!(!target.exists());
                assert!(staging.join("document.md").exists());
                Err(io::Error::other("injected staged-install failure"))
            },
            remove_path,
        );

        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("injected staged-install failure")
        );
        assert_eq!(fs::read(target.join("old.txt")).unwrap(), b"old");
        assert_eq!(
            fs::read(target.join("nested/only-old.txt")).unwrap(),
            b"nested old"
        );
        assert_eq!(fs::read_dir(&target).unwrap().count(), 2);
        assert_eq!(fs::read_dir(target.join("nested")).unwrap().count(), 1);
        assert!(!target.join("document.md").exists());
        assert!(!target.join("assets/image.png").exists());
        assert!(private_artifacts(temp.path()).is_empty());
    }

    #[test]
    fn cleanup_failure_after_commit_still_returns_installed_manifest() {
        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join("output");
        fs::create_dir(&target).unwrap();
        fs::write(target.join("old.txt"), b"old").unwrap();
        let retained_backup = RefCell::new(None);

        let manifest = write_outputs_with(
            &replacement_document(),
            &target,
            |staging, target| fs::rename(staging, target),
            |backup| {
                assert_eq!(
                    fs::read_to_string(target.join("document.md")).unwrap(),
                    "# new"
                );
                *retained_backup.borrow_mut() = Some(backup.to_owned());
                Err(io::Error::other("injected backup-cleanup failure"))
            },
        )
        .unwrap();

        assert_eq!(manifest.document_json, target.join("document.json"));
        assert_eq!(manifest.markdown, target.join("document.md"));
        assert_eq!(manifest.middle_json, target.join("middle.json"));
        assert_eq!(manifest.content_list, target.join("content_list.json"));
        assert_eq!(fs::read_to_string(&manifest.markdown).unwrap(), "# new");
        assert!(manifest.document_json.exists());
        assert!(manifest.middle_json.exists());
        assert!(manifest.content_list.exists());
        assert_eq!(
            fs::read(target.join(&manifest.assets[0])).unwrap(),
            b"new image"
        );
        assert!(!target.join("old.txt").exists());

        let backup = retained_backup.into_inner().unwrap();
        assert_eq!(private_artifacts(temp.path()), [backup.clone()]);
        assert_eq!(fs::read(backup.join("old.txt")).unwrap(), b"old");
        remove_path(&backup).unwrap();
        assert!(private_artifacts(temp.path()).is_empty());
    }

    #[test]
    fn ordinary_replacement_removes_stage_and_backup() {
        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join("output");
        fs::create_dir(&target).unwrap();
        fs::write(target.join("old.txt"), b"old").unwrap();

        write_outputs(&replacement_document(), &target).unwrap();

        assert_eq!(
            fs::read_to_string(target.join("document.md")).unwrap(),
            "# new"
        );
        assert!(!target.join("old.txt").exists());
        assert!(private_artifacts(temp.path()).is_empty());

        remove_path(&target).unwrap();
        fs::write(&target, b"old file").unwrap();
        write_outputs(&replacement_document(), &target).unwrap();
        assert!(target.is_dir());
        assert!(private_artifacts(temp.path()).is_empty());
    }
}
