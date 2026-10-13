use bytes::Bytes;
use std::{
    collections::BTreeMap,
    io::Write,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

static STAGE_ID: AtomicU64 = AtomicU64::new(0);

pub(super) fn publish_rendered(
    output: &Path,
    main: &[u8],
    sidecars: BTreeMap<String, Bytes>,
) -> Result<(), String> {
    prepare_render(output, main, sidecars)?.commit()
}

fn prepare_render(
    output: &Path,
    main: &[u8],
    sidecars: BTreeMap<String, Bytes>,
) -> Result<PreparedRender, String> {
    let parent = output.parent().unwrap_or(Path::new("."));
    let target = output.file_name().ok_or("output needs a filename")?;
    let root = crate::official_output::open_or_create_root(parent).map_err(|e| e.to_string())?;
    let paths = sidecars
        .keys()
        .map(PathBuf::from)
        .collect::<std::collections::BTreeSet<_>>();
    for name in sidecars.keys() {
        let rel = Path::new(name);
        if rel.is_absolute()
            || rel
                .components()
                .any(|c| !matches!(c, std::path::Component::Normal(_)))
            || rel.components().count() < 2
            || rel
                .components()
                .next()
                .is_none_or(|c| c.as_os_str() != "images")
        {
            return Err("unsafe rendered sidecar path".into());
        }
        if rel
            .ancestors()
            .skip(1)
            .any(|p| !p.as_os_str().is_empty() && paths.contains(p))
        {
            return Err("overlapping rendered sidecar paths".into());
        }
    }
    if target == "images" {
        return Err("render output overlaps a sidecar path".into());
    }
    absent(&root, Path::new(target), "render output already exists")?;
    if !sidecars.is_empty() {
        absent(
            &root,
            Path::new("images"),
            "render sidecar destination already exists",
        )?;
    }
    let name = format!(
        ".mineru-render-{}-{}",
        std::process::id(),
        STAGE_ID.fetch_add(1, Ordering::Relaxed)
    );
    root.create_dir(&name).map_err(|e| e.to_string())?;
    let stage = match open_stage(&root, &name) {
        Ok(stage) => stage,
        Err(error) => {
            let _ = root.remove_dir_all(&name);
            return Err(error);
        }
    };
    let prepared = (|| {
        stage
            .create("rendered")
            .map_err(|e| e.to_string())?
            .write_all(main)
            .map_err(|e| e.to_string())?;
        for (path, bytes) in &sidecars {
            write_staged(&stage, Path::new(path), bytes)?;
        }
        Ok::<_, String>(())
    })();
    if let Err(error) = prepared {
        drop(stage);
        let _ = root.remove_dir_all(&name);
        return Err(error);
    }
    drop(stage);
    let stage = match open_stage(&root, &name) {
        Ok(stage) => stage,
        Err(error) => {
            let _ = root.remove_dir_all(&name);
            return Err(error);
        }
    };
    Ok(PreparedRender {
        root,
        target: target.to_owned(),
        name,
        stage: Some(stage),
        assets: sidecars.keys().map(PathBuf::from).collect(),
        owns_images: false,
    })
}

struct PreparedRender {
    root: cap_std::fs::Dir,
    target: std::ffi::OsString,
    name: String,
    stage: Option<cap_std::fs::Dir>,
    assets: Vec<PathBuf>,
    owns_images: bool,
}

impl PreparedRender {
    fn commit(mut self) -> Result<(), String> {
        if !self.assets.is_empty() {
            self.root.create_dir("images").map_err(|e| e.to_string())?;
            self.owns_images = true;
            let images = crate::official_output::open_child_nofollow(
                self.root.try_clone().map_err(|e| e.to_string())?,
                "images",
            )
            .map_err(|e| e.to_string())?;
            let stage = self.stage.as_ref().expect("prepared stage");
            for path in &self.assets {
                let mut parent = images.try_clone().map_err(|e| e.to_string())?;
                let mut parts = path.components().skip(1).peekable();
                while let Some(part) = parts.next() {
                    if parts.peek().is_some() {
                        let name = part.as_os_str();
                        if parent.symlink_metadata(name).is_err() {
                            parent.create_dir(name).map_err(|e| e.to_string())?;
                        }
                        parent = crate::official_output::open_child_nofollow(parent, name)
                            .map_err(|e| e.to_string())?;
                    } else {
                        stage
                            .hard_link(path, &parent, part.as_os_str())
                            .map_err(|e| e.to_string())?;
                    }
                }
            }
        }
        let stage = self.stage.as_ref().expect("prepared stage");
        stage
            .hard_link("rendered", &self.root, &self.target)
            .map_err(|e| e.to_string())?;
        self.owns_images = false;
        self.stage.take();
        self.root
            .remove_dir_all(&self.name)
            .map_err(|e| e.to_string())
    }
}

impl Drop for PreparedRender {
    fn drop(&mut self) {
        self.stage.take();
        if self.owns_images {
            let _ = self.root.remove_dir_all("images");
        }
        let _ = self.root.remove_dir_all(&self.name);
    }
}

fn absent(root: &cap_std::fs::Dir, path: &Path, message: &str) -> Result<(), String> {
    match root.symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Ok(_) => Err(message.into()),
        Err(error) => Err(error.to_string()),
    }
}

fn open_stage(root: &cap_std::fs::Dir, name: &str) -> Result<cap_std::fs::Dir, String> {
    crate::official_output::open_child_nofollow(root.try_clone().map_err(|e| e.to_string())?, name)
        .map_err(|e| e.to_string())
}

fn write_staged(stage: &cap_std::fs::Dir, path: &Path, bytes: &[u8]) -> Result<(), String> {
    let mut parts = path.components().peekable();
    let mut dir = stage.try_clone().map_err(|e| e.to_string())?;
    while let Some(part) = parts.next() {
        if parts.peek().is_some() {
            let name = part.as_os_str();
            if dir.symlink_metadata(name).is_err() {
                dir.create_dir(name).map_err(|e| e.to_string())?;
            }
            dir = crate::official_output::open_child_nofollow(dir, name)
                .map_err(|e| e.to_string())?;
        } else {
            dir.create(part.as_os_str())
                .map_err(|e| e.to_string())?
                .write_all(bytes)
                .map_err(|e| e.to_string())?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn asset(name: &str) -> BTreeMap<String, Bytes> {
        BTreeMap::from([(name.into(), Bytes::from_static(b"asset-new"))])
    }
    #[test]
    fn collisions_leave_both_existing_artifacts_unchanged() {
        let temp = tempfile::tempdir().unwrap();
        let main = temp.path().join("render.md");
        std::fs::write(&main, b"main-old").unwrap();
        std::fs::create_dir(temp.path().join("images")).unwrap();
        std::fs::write(temp.path().join("images/same.png"), b"asset-old").unwrap();
        assert!(publish_rendered(&main, b"main-new", asset("images/same.png")).is_err());
        assert_eq!(std::fs::read(main).unwrap(), b"main-old");
        assert_eq!(
            std::fs::read(temp.path().join("images/same.png")).unwrap(),
            b"asset-old"
        );
        assert_eq!(
            std::fs::read_dir(temp.path().join("images"))
                .unwrap()
                .count(),
            1
        );
    }
    #[test]
    fn success_publishes_main_and_asset_together() {
        let temp = tempfile::tempdir().unwrap();
        let main = temp.path().join("render.bin");
        publish_rendered(&main, b"binary-render", asset("images/page.png")).unwrap();
        assert_eq!(std::fs::read(main).unwrap(), b"binary-render");
        assert_eq!(
            std::fs::read(temp.path().join("images/page.png")).unwrap(),
            b"asset-new"
        );
    }
    #[test]
    fn competing_prepared_publishers_cannot_replace_or_remove_winner() {
        use std::sync::{Arc, Barrier};
        let temp = tempfile::tempdir().unwrap();
        let output = temp.path().join("same.bin");
        let first = prepare_render(&output, b"first", asset("images/item.png")).unwrap();
        let second = prepare_render(
            &output,
            b"second",
            BTreeMap::from([(
                "images/item.png".into(),
                Bytes::from_static(b"second-asset"),
            )]),
        )
        .unwrap();
        let barrier = Arc::new(Barrier::new(2));
        let (a, b) = std::thread::scope(|scope| {
            let x = Arc::clone(&barrier);
            let a = scope.spawn(move || {
                x.wait();
                first.commit()
            });
            let b = scope.spawn(move || {
                barrier.wait();
                second.commit()
            });
            (a.join().unwrap(), b.join().unwrap())
        });
        assert_ne!(a.is_ok(), b.is_ok());
        let main = std::fs::read(output).unwrap();
        let asset = std::fs::read(temp.path().join("images/item.png")).unwrap();
        assert!(
            (main.as_slice() == b"first" && asset.as_slice() == b"asset-new")
                || (main.as_slice() == b"second" && asset.as_slice() == b"second-asset")
        );
    }
    #[test]
    fn unsafe_paths_and_symlink_destinations_are_rejected() {
        let temp = tempfile::tempdir().unwrap();
        assert!(
            publish_rendered(&temp.path().join("safe"), b"x", asset("images/../escape")).is_err()
        );
        assert!(publish_rendered(&temp.path().join("safe"), b"x", asset("")).is_err());
        #[cfg(unix)]
        {
            let outside = temp.path().join("outside");
            std::fs::write(&outside, b"sentinel").unwrap();
            std::os::unix::fs::symlink(&outside, temp.path().join("main-link")).unwrap();
            assert!(
                publish_rendered(&temp.path().join("main-link"), b"replace", BTreeMap::new())
                    .is_err()
            );
            std::os::unix::fs::symlink(temp.path(), temp.path().join("parent-link")).unwrap();
            assert!(
                publish_rendered(
                    &temp.path().join("parent-link/escape"),
                    b"x",
                    BTreeMap::new()
                )
                .is_err()
            );
            assert!(!temp.path().join("escape").exists());
            assert_eq!(std::fs::read(outside).unwrap(), b"sentinel");
        }
    }
}
