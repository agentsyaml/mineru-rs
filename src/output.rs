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
    let directory = directory.as_ref();
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
    if let Err(error) = fs::rename(&staging, directory) {
        let restore = fs::rename(&backup, directory);
        let _ = fs::remove_dir_all(&staging);
        return Err(restore.err().unwrap_or(error).into());
    }
    fs::remove_dir_all(backup)?;
    Ok(manifest(document, directory))
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
    use super::write_outputs;
    use crate::{Asset, AssetKind, Document};
    use bytes::Bytes;
    use std::{fs, path::PathBuf};

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
}
