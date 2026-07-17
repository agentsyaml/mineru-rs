use bytes::Bytes;
use mineru::{
    Asset, AssetKind, Document, ModelBlock, ModelOutput, NormalizedBbox, OfficialDocument,
    Rotation, write_official_outputs,
};
use serde_json::{Map, Value, json};
use std::{fs, path::PathBuf};

fn bbox(a: f32, b: f32, c: f32, d: f32) -> NormalizedBbox {
    NormalizedBbox::new(a, b, c, d).unwrap()
}
fn asset(kind: AssetKind, path: &str, media: &str, bytes: &'static [u8]) -> Asset {
    Asset {
        kind,
        relative_path: PathBuf::from(path),
        media_type: media.into(),
        data: Bytes::from_static(bytes),
        md5: String::new(),
    }
}
fn document(model_output: ModelOutput) -> OfficialDocument {
    OfficialDocument {
        document: Document {
            markdown: "# exact\n".into(),
            middle_json: json!({"middle": [true]}),
            content_list: json!(["one"]),
            assets: vec![
                asset(
                    AssetKind::Image,
                    "images/picture.png",
                    "image/png",
                    b"image",
                ),
                asset(
                    AssetKind::Other("layout_preview".into()),
                    "ignored",
                    "application/pdf",
                    b"pdf",
                ),
            ],
            ..Default::default()
        },
        model_output,
        content_list_v2: json!({"v": 2}),
        diagnostics: vec![],
    }
}

#[test]
fn public_writer_emits_official_hierarchy_and_pinned_model_wire() {
    let mut extra = Map::new();
    extra.insert("extra_null".into(), Value::Null);
    let model = vec![vec![
        ModelBlock {
            block_type: "text".into(),
            bbox: Some(bbox(0., 0., 1., 1.)),
            angle: Some(Rotation::Deg0),
            content: Some("text".into()),
            merge_prev: Some(false),
            extra,
            ..Default::default()
        },
        ModelBlock {
            block_type: "image".into(),
            bbox: Some(bbox(0.1, 0.2, 0.3, 0.4)),
            angle: Some(Rotation::Deg90),
            sub_type: Some("figure".into()),
            ..Default::default()
        },
        ModelBlock {
            block_type: "title".into(),
            bbox: Some(bbox(0.2, 0.2, 0.8, 0.8)),
            angle: Some(Rotation::Deg180),
            content: Some("optional omitted".into()),
            ..Default::default()
        },
    ]];
    let temp = tempfile::tempdir().unwrap();
    let manifest = write_official_outputs(temp.path(), "a bad/pdf", &document(model)).unwrap();
    assert_eq!(manifest.stem, "a_bad_pdf");
    let vlm = temp.path().join("a_bad_pdf/vlm");
    for name in [
        "a_bad_pdf_middle.json",
        "a_bad_pdf_model.json",
        "a_bad_pdf_content_list.json",
        "a_bad_pdf_content_list_v2.json",
        "a_bad_pdf.md",
        "a_bad_pdf_layout.pdf",
    ] {
        assert!(vlm.join(name).is_file(), "{name}");
    }
    assert_eq!(fs::read(vlm.join("images/picture.png")).unwrap(), b"image");
    assert_eq!(fs::read(vlm.join("a_bad_pdf_layout.pdf")).unwrap(), b"pdf");
    assert!(!vlm.join("a_bad_pdf_origin.pdf").exists());
    assert!(
        !fs::read_dir(temp.path().join("a_bad_pdf"))
            .unwrap()
            .any(|entry| entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".vlm-staging-parent-"))
    );
    let actual: Value =
        serde_json::from_slice(&fs::read(vlm.join("a_bad_pdf_model.json")).unwrap()).unwrap();
    let expected: Value =
        serde_json::from_str(include_str!("fixtures/vlm/model_output_wire.json")).unwrap();
    assert_eq!(actual, expected);
}

#[test]
fn validation_happens_before_replacing_target() {
    let temp = tempfile::tempdir().unwrap();
    let old = temp.path().join("document/vlm");
    fs::create_dir_all(&old).unwrap();
    fs::write(old.join("stale"), b"old").unwrap();
    let mut doc = document(vec![vec![ModelBlock {
        block_type: "text".into(),
        bbox: None,
        angle: Some(Rotation::Deg0),
        ..Default::default()
    }]]);
    assert!(write_official_outputs(temp.path(), "", &doc).is_err());
    assert_eq!(fs::read(old.join("stale")).unwrap(), b"old");
    doc.model_output = vec![];
    write_official_outputs(temp.path(), "", &doc).unwrap();
    assert!(!old.join("stale").exists());
}

#[test]
fn rejects_invalid_assets_and_preview_counts() {
    for path in [
        "../x",
        "images/../x",
        "images\\x",
        "images//x",
        "images/CON.txt",
        "images/x. ",
        "images",
        "",
    ] {
        let mut doc = document(vec![]);
        doc.document.assets[0].relative_path = PathBuf::from(path);
        assert!(
            write_official_outputs(tempfile::tempdir().unwrap().path(), "x", &doc).is_err(),
            "{path}"
        );
    }
    let mut missing = document(vec![]);
    missing.document.assets.pop();
    assert!(write_official_outputs(tempfile::tempdir().unwrap().path(), "x", &missing).is_err());
    let mut multiple = document(vec![]);
    multiple.document.assets.push(asset(
        AssetKind::Other("layout_preview".into()),
        "x",
        "application/pdf",
        b"two",
    ));
    assert!(write_official_outputs(tempfile::tempdir().unwrap().path(), "x", &multiple).is_err());
}

#[test]
fn rejects_portable_path_aliases_without_touching_existing_output() {
    let temp = tempfile::tempdir().unwrap();
    let old = temp.path().join("x/vlm");
    fs::create_dir_all(&old).unwrap();
    fs::write(old.join("stale"), b"old").unwrap();

    for path in ["images\\..\\a.png", "images/NUL.pdf"] {
        let mut doc = document(vec![]);
        doc.document.assets[0].relative_path = PathBuf::from(path);
        assert!(
            write_official_outputs(temp.path(), "x", &doc).is_err(),
            "{path}"
        );
        assert_eq!(fs::read(old.join("stale")).unwrap(), b"old");
    }

    let mut duplicate = document(vec![]);
    duplicate.document.assets.push(asset(
        AssetKind::Image,
        "images/PICTURE.png",
        "image/png",
        b"duplicate",
    ));
    assert!(write_official_outputs(temp.path(), "x", &duplicate).is_err());
    assert_eq!(fs::read(old.join("stale")).unwrap(), b"old");
}

#[cfg(unix)]
#[test]
fn rejects_symlinked_output_root_and_stem_before_staging() {
    use std::os::unix::fs::symlink;

    let temp = tempfile::tempdir().unwrap();
    let real = temp.path().join("real");
    fs::create_dir(&real).unwrap();
    let root_link = temp.path().join("root-link");
    symlink(&real, &root_link).unwrap();
    assert!(write_official_outputs(&root_link, "document", &document(vec![])).is_err());

    let root = temp.path().join("root");
    fs::create_dir(&root).unwrap();
    symlink(&real, root.join("document")).unwrap();
    assert!(write_official_outputs(&root, "document", &document(vec![])).is_err());
}

#[test]
fn writer_supports_nested_and_absolute_roots() {
    let temp = tempfile::tempdir().unwrap();
    let nested = temp.path().join("new/nested/root");
    let manifest = write_official_outputs(&nested, "document", &document(vec![])).unwrap();
    assert_eq!(manifest.vlm_dir, nested.join("document/vlm"));
    assert!(manifest.vlm_dir.join("document.md").is_file());
}
