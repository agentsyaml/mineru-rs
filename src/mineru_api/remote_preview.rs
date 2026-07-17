use super::archive;
use crate::{
    BlockKind, ContentBlock, DocumentKind, NormalizedBbox, OfficeWorkers, OfficialPdfOptions,
    PageResult, ProgressCallback, ProgressEvent, RasterWorkers,
};
use bytes::Bytes;
use serde_json::Value;
use std::{
    path::{Path, PathBuf},
    time::Instant,
};

pub(super) struct Artifacts {
    pub(super) middle: Vec<u8>,
    pub(super) origin: Vec<u8>,
}
fn paths(stem: &str, kind: DocumentKind) -> Result<(PathBuf, PathBuf, PathBuf), String> {
    if crate::canonical_stem(stem).ok().as_deref() != Some(stem) {
        return Err("invalid preview stem".into());
    }
    let mode = if kind.is_office() { "office" } else { "vlm" };
    let dir = PathBuf::from(stem).join(mode);
    Ok((
        dir.join(format!("{stem}_middle.json")),
        dir.join(format!("{stem}_origin.{}", kind.suffix())),
        dir.join(format!("{stem}_layout.pdf")),
    ))
}
pub(super) fn read_artifacts(
    root: &Path,
    stem: &str,
    kind: DocumentKind,
    route: &OfficialPdfOptions,
) -> Result<Artifacts, String> {
    let (middle, origin, _) = paths(stem, kind)?;
    Ok(Artifacts {
        middle: archive::read_relative_capped(root, &middle, route.max_staged_text_bytes)?,
        origin: archive::read_relative_capped(root, &origin, route.max_pdf_bytes)?,
    })
}
pub(super) fn generate_and_publish(
    root: &Path,
    stem: &str,
    kind: DocumentKind,
    source_pdf: &[u8],
    middle: &[u8],
    route: &OfficialPdfOptions,
    deadline: Instant,
) -> Result<PathBuf, String> {
    if Instant::now() >= deadline {
        return Err("preview deadline expired".into());
    }
    let pages = parse_middle(middle, route)?;
    let asset = crate::preview::generate_until(
        source_pdf,
        &pages,
        stem,
        &crate::official_route::route_limits(route),
        route.max_total_asset_bytes,
        deadline,
    )
    .map_err(|_| "preview generation failed")?;
    if Instant::now() >= deadline {
        return Err("preview deadline expired".into());
    }
    let (_, _, layout) = paths(stem, kind)?;
    archive::write_relative_atomic(root, &layout, &asset.data)?;
    Ok(root.join(layout))
}
pub(super) async fn prepare_and_publish_downloaded(
    root: &Path,
    stem: &str,
    kind: DocumentKind,
    route: &OfficialPdfOptions,
    office_workers: &OfficeWorkers,
    raster_workers: &RasterWorkers,
    events: Option<ProgressCallback>,
) -> Result<PathBuf, String> {
    let deadline = Instant::now()
        .checked_add(route.total_deadline)
        .ok_or("preview deadline expired")?;
    let root = root.to_path_buf();
    let stem = stem.to_owned();
    let route = route.clone();
    let artifacts = tokio::task::spawn_blocking({
        let root = root.clone();
        let stem = stem.clone();
        let route = route.clone();
        move || read_artifacts(&root, &stem, kind, &route)
    })
    .await
    .map_err(|_| "preview worker stopped")?
    .map_err(|_| "preview artifact read failed")?;
    let source_pdf = if kind == DocumentKind::Pdf {
        artifacts.origin
    } else {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .filter(|remaining| !remaining.is_zero())
            .ok_or("preview deadline expired")?;
        let (prepared, warning) = crate::input_prepare::prepare_with_warning(
            Bytes::from(artifacts.origin),
            kind,
            &route,
            office_workers,
            raster_workers,
            remaining,
        )
        .await?;
        if let Some(message) = warning {
            crate::progress_events::emit(
                &events,
                ProgressEvent::OfficeWarning {
                    document: stem.clone(),
                    message,
                },
            );
        }
        prepared.bytes.to_vec()
    };
    tokio::task::spawn_blocking(move || {
        generate_and_publish(
            &root,
            &stem,
            kind,
            &source_pdf,
            &artifacts.middle,
            &route,
            deadline,
        )
    })
    .await
    .map_err(|_| "preview worker stopped")?
}
fn parse_middle(bytes: &[u8], route: &OfficialPdfOptions) -> Result<Vec<PageResult>, String> {
    let pages = serde_json::from_slice::<Value>(bytes)
        .ok()
        .and_then(|v| v.get("pdf_info").and_then(Value::as_array).cloned())
        .filter(|v| !v.is_empty() && v.len() <= route.max_pages)
        .ok_or("invalid preview middle JSON")?;
    let mut out = Vec::new();
    let mut previous = None;
    let mut total_blocks = 0usize;
    let max_total = route
        .max_pages
        .checked_mul(route.max_layout_blocks_per_page)
        .ok_or("invalid preview limits")?;
    for page in pages {
        let object = page.as_object().ok_or("invalid preview middle JSON")?;
        let index = object
            .get("page_idx")
            .and_then(Value::as_u64)
            .and_then(|v| usize::try_from(v).ok())
            .ok_or("invalid preview middle JSON")?;
        if previous.is_some_and(|last| index <= last) {
            return Err("invalid preview middle JSON".into());
        }
        previous = Some(index);
        let size = object
            .get("page_size")
            .and_then(Value::as_array)
            .filter(|v| v.len() == 2)
            .ok_or("invalid preview middle JSON")?;
        let number = |v: &Value| {
            v.as_f64()
                .filter(|v| v.is_finite() && *v > 0. && *v <= f32::MAX as f64)
                .map(|v| v as f32)
                .filter(|v| v.is_finite() && *v > 0.)
                .ok_or("invalid preview middle JSON")
        };
        let page_size = [number(&size[0])?, number(&size[1])?];
        let preproc = object
            .get("preproc_blocks")
            .and_then(Value::as_array)
            .ok_or("invalid preview middle JSON")?;
        let discarded = object
            .get("discarded_blocks")
            .and_then(Value::as_array)
            .ok_or("invalid preview middle JSON")?;
        let count = preproc
            .len()
            .checked_add(discarded.len())
            .ok_or("invalid preview middle JSON")?;
        if count > route.max_layout_blocks_per_page {
            return Err("invalid preview middle JSON".into());
        }
        total_blocks = total_blocks
            .checked_add(count)
            .filter(|n| *n <= max_total)
            .ok_or("invalid preview middle JSON")?;
        let mut blocks = Vec::with_capacity(count);
        for block in preproc.iter().chain(discarded) {
            blocks.push(parse_block(block, page_size)?);
        }
        out.push(PageResult {
            page_index: index,
            page_size,
            blocks,
        });
    }
    Ok(out)
}
fn parse_block(value: &Value, page_size: [f32; 2]) -> Result<ContentBlock, String> {
    let object = value.as_object().ok_or("invalid preview middle JSON")?;
    let kind = object
        .get("type")
        .and_then(Value::as_str)
        .filter(|v| !v.is_empty() && v.len() <= 128)
        .ok_or("invalid preview middle JSON")?;
    let bbox = object
        .get("bbox")
        .and_then(Value::as_array)
        .filter(|v| v.len() == 4)
        .ok_or("invalid preview middle JSON")?;
    let n = |v: &Value| {
        v.as_f64()
            .filter(|v| v.is_finite())
            .ok_or("invalid preview middle JSON")
    };
    let [x0, y0, x1, y1] = [n(&bbox[0])?, n(&bbox[1])?, n(&bbox[2])?, n(&bbox[3])?];
    if !(0. <= x0
        && x0 <= x1
        && x1 <= page_size[0] as f64
        && 0. <= y0
        && y0 <= y1
        && y1 <= page_size[1] as f64)
    {
        return Err("invalid preview middle JSON".into());
    }
    let normalized = [
        (x0 / page_size[0] as f64) as f32,
        (y0 / page_size[1] as f64) as f32,
        (x1 / page_size[0] as f64) as f32,
        (y1 / page_size[1] as f64) as f32,
    ];
    if normalized.iter().any(|v| !v.is_finite()) {
        return Err("invalid preview middle JSON".into());
    }
    Ok(ContentBlock {
        kind: BlockKind::new(kind),
        bbox: NormalizedBbox::new(normalized[0], normalized[1], normalized[2], normalized[3])
            .map_err(|_| "invalid preview middle JSON")?,
        angle: None,
        content: None,
        merge_previous: false,
        metadata: Default::default(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    fn route() -> OfficialPdfOptions {
        OfficialPdfOptions {
            max_pages: 1,
            max_layout_blocks_per_page: 1,
            ..Default::default()
        }
    }
    fn middle() -> Vec<u8> {
        br#"{"pdf_info":[{"page_idx":0,"page_size":[200,100],"preproc_blocks":[{"type":"text","bbox":[20,10,150,80]}],"discarded_blocks":[]}]}"#.to_vec()
    }
    fn write_artifacts(root: &Path, stem: &str, kind: DocumentKind, origin: &[u8]) {
        let (middle_path, origin_path, _) = paths(stem, kind).unwrap();
        std::fs::create_dir_all(root.join(middle_path.parent().unwrap())).unwrap();
        std::fs::write(root.join(middle_path), middle()).unwrap();
        std::fs::write(root.join(origin_path), origin).unwrap();
    }
    fn docx() -> Vec<u8> {
        use std::io::Write;
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let options = zip::write::SimpleFileOptions::default();
        zip.start_file("_rels/.rels", options).unwrap();
        zip.write_all(br#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="word/document.xml"/></Relationships>"#).unwrap();
        zip.start_file("[Content_Types].xml", options).unwrap();
        zip.write_all(br#"<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/></Types>"#).unwrap();
        zip.finish().unwrap().into_inner()
    }
    #[test]
    fn strict_middle_shape_and_caps() {
        let pages = parse_middle(&middle(), &route()).unwrap();
        assert_eq!(pages.len(), 1);
        assert_eq!(
            pages[0].blocks[0].bbox,
            NormalizedBbox::new(0.1, 0.1, 0.75, 0.8).unwrap()
        );
        for value in [br#"{}"#.as_slice(), br#"{"pdf_info":[]}"#, br#"{"pdf_info":[{"page_index":0,"page_size":[1,1],"preproc_blocks":[],"discarded_blocks":[]}]}"#, br#"{"pdf_info":[{"page_idx":0,"page_size":[0,1],"preproc_blocks":[],"discarded_blocks":[]}]}"#] { assert!(parse_middle(value, &route()).is_err()); }
        let mut over = route();
        over.max_layout_blocks_per_page = 0;
        assert!(parse_middle(&middle(), &over).is_err());
    }
    #[test]
    fn strict_page_block_and_bbox_boundaries() {
        let one = middle();
        let mut r = route();
        assert!(parse_middle(&one, &r).is_ok());
        let two = br#"{"pdf_info":[{"page_idx":0,"page_size":[1,1],"preproc_blocks":[],"discarded_blocks":[]},{"page_idx":1,"page_size":[1,1],"preproc_blocks":[],"discarded_blocks":[]}]}"#;
        assert!(parse_middle(two, &r).is_err());
        r.max_layout_blocks_per_page = 2;
        let split = br#"{"pdf_info":[{"page_idx":0,"page_size":[1,1],"preproc_blocks":[{"type":"text","bbox":[0,0,1,1]}],"discarded_blocks":[{"type":"text","bbox":[0,0,1,1]}]}]}"#;
        assert!(parse_middle(split, &r).is_ok());
        let over = br#"{"pdf_info":[{"page_idx":0,"page_size":[1,1],"preproc_blocks":[{"type":"text","bbox":[0,0,1,1]},{"type":"text","bbox":[0,0,1,1]}],"discarded_blocks":[{"type":"bad","bbox":null}]}]}"#;
        assert!(parse_middle(over, &r).is_err());
        for bbox in [
            r#"{"left":0,"top":0,"right":1,"bottom":1}"#,
            "[0,0,1]",
            "[0,0,\"x\",1]",
            "[0,0,1e999,1]",
            "[-1,0,1,1]",
            "[1,0,0,1]",
            "[0,1,1,0]",
            "[0,0,0,1]",
            "[0,0,1,0]",
            "[0,0,2,1]",
            "[0,0,1,2]",
        ] {
            let value = format!(
                r#"{{"pdf_info":[{{"page_idx":0,"page_size":[1,1],"preproc_blocks":[{{"type":"text","bbox":{bbox}}}],"discarded_blocks":[]}}]}}"#
            );
            assert!(parse_middle(value.as_bytes(), &route()).is_err(), "{bbox}");
        }
        let bad_size = br#"{"pdf_info":[{"page_idx":0,"page_size":[1e-100,1],"preproc_blocks":[],"discarded_blocks":[]}]}"#;
        assert!(parse_middle(bad_size, &route()).is_err());
    }
    #[test]
    fn artifact_reads_are_capped_and_safe() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("doc/vlm");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("doc_middle.json"), middle()).unwrap();
        std::fs::write(dir.join("doc_origin.pdf"), b"x").unwrap();
        let mut r = route();
        r.max_staged_text_bytes = middle().len();
        r.max_pdf_bytes = 1;
        assert!(read_artifacts(root.path(), "doc", DocumentKind::Pdf, &r).is_ok());
        r.max_pdf_bytes = 0;
        assert!(read_artifacts(root.path(), "doc", DocumentKind::Pdf, &r).is_err());
        r.max_pdf_bytes = 1;
        r.max_staged_text_bytes = middle().len() - 1;
        assert!(read_artifacts(root.path(), "doc", DocumentKind::Pdf, &r).is_err());
    }
    #[test]
    fn generates_pdf_and_office_mode_layouts() {
        let root = tempfile::tempdir().unwrap();
        let source = include_bytes!("../../tests/fixtures/pdf/minimal.pdf");
        let route = route();
        for (stem, kind, mode) in [
            ("doc", DocumentKind::Pdf, "vlm"),
            ("office", DocumentKind::Docx, "office"),
        ] {
            let dir = root.path().join(stem).join(mode);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join(format!("{stem}_middle.json")), middle()).unwrap();
            std::fs::write(dir.join(format!("{stem}_origin.{}", kind.suffix())), source).unwrap();
            let artifacts = read_artifacts(root.path(), stem, kind, &route).unwrap();
            let path = generate_and_publish(
                root.path(),
                stem,
                kind,
                &artifacts.origin,
                &artifacts.middle,
                &route,
                Instant::now() + std::time::Duration::from_secs(2),
            )
            .unwrap();
            assert_eq!(path, dir.join(format!("{stem}_layout.pdf")));
            assert_eq!(lopdf::Document::load(&path).unwrap().get_pages().len(), 1);
            assert!(!std::fs::read_dir(&dir).unwrap().any(|entry| {
                entry
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".mineru-extract-")
            }));
        }
        assert!(!root.path().join("office/vlm/office_layout.pdf").exists());
    }
    #[tokio::test]
    async fn publishes_downloaded_image_origin() {
        let root = tempfile::tempdir().unwrap();
        let mut png = Vec::new();
        image::DynamicImage::new_rgba8(1, 1)
            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();
        write_artifacts(root.path(), "img", DocumentKind::Png, &png);
        let office = OfficeWorkers::with_test_executable(std::env::current_exe().unwrap());
        let raster = RasterWorkers::default();
        let path = prepare_and_publish_downloaded(
            root.path(),
            "img",
            DocumentKind::Png,
            &route(),
            &office,
            &raster,
            None,
        )
        .await
        .unwrap();
        assert_eq!(lopdf::Document::load(path).unwrap().get_pages().len(), 1);
        office.drain().await;
        raster.drain().await;
    }
    #[tokio::test]
    async fn publishes_downloaded_office_origin_and_emits_warning() {
        let root = tempfile::tempdir().unwrap();
        write_artifacts(root.path(), "office", DocumentKind::Docx, &docx());
        let events = Arc::new(Mutex::new(Vec::new()));
        let callback = {
            let events = events.clone();
            Arc::new(move |event| events.lock().unwrap().push(event)) as ProgressCallback
        };
        let office = OfficeWorkers::with_test_executable(std::env::current_exe().unwrap());
        let raster = RasterWorkers::default();
        let path = prepare_and_publish_downloaded(
            root.path(),
            "office",
            DocumentKind::Docx,
            &route(),
            &office,
            &raster,
            Some(callback),
        )
        .await
        .unwrap();
        assert_eq!(lopdf::Document::load(path).unwrap().get_pages().len(), 1);
        assert_eq!(
            *events.lock().unwrap(),
            vec![ProgressEvent::OfficeWarning {
                document: "office".into(),
                message: "simultaneous stderr\\n".into(),
            }]
        );
        office.drain().await;
        raster.drain().await;
    }
    #[tokio::test]
    async fn expired_or_bad_downloaded_artifacts_do_not_publish() {
        let root = tempfile::tempdir().unwrap();
        write_artifacts(root.path(), "bad", DocumentKind::Png, b"not a PNG");
        let office = OfficeWorkers::with_test_executable(std::env::current_exe().unwrap());
        let raster = RasterWorkers::default();
        let mut expired = route();
        expired.total_deadline = std::time::Duration::ZERO;
        assert_eq!(
            prepare_and_publish_downloaded(
                root.path(),
                "bad",
                DocumentKind::Png,
                &expired,
                &office,
                &raster,
                None
            )
            .await
            .unwrap_err(),
            "preview deadline expired"
        );
        assert_eq!(
            prepare_and_publish_downloaded(
                root.path(),
                "bad",
                DocumentKind::Png,
                &route(),
                &office,
                &raster,
                None
            )
            .await
            .unwrap_err(),
            "invalid image"
        );
        assert!(!root.path().join("bad/vlm/bad_layout.pdf").exists());
        office.drain().await;
        raster.drain().await;
    }
    #[cfg(unix)]
    #[test]
    fn artifact_symlinks_are_rejected() {
        use std::os::unix::fs::symlink;
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let dir = root.path().join("doc/vlm");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(outside.path().join("middle"), middle()).unwrap();
        std::fs::write(outside.path().join("origin"), b"x").unwrap();
        symlink(outside.path().join("middle"), dir.join("doc_middle.json")).unwrap();
        std::fs::write(dir.join("doc_origin.pdf"), b"x").unwrap();
        assert!(read_artifacts(root.path(), "doc", DocumentKind::Pdf, &route()).is_err());
        std::fs::remove_file(dir.join("doc_middle.json")).unwrap();
        std::fs::write(dir.join("doc_middle.json"), middle()).unwrap();
        std::fs::remove_file(dir.join("doc_origin.pdf")).unwrap();
        symlink(outside.path().join("origin"), dir.join("doc_origin.pdf")).unwrap();
        assert!(read_artifacts(root.path(), "doc", DocumentKind::Pdf, &route()).is_err());
        assert_eq!(std::fs::read(outside.path().join("origin")).unwrap(), b"x");
    }
}
