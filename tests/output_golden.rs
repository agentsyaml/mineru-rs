use bytes::Bytes;
use mineru::{Asset, AssetKind, Document, write_outputs};
use serde_json::json;
use std::{fs, path::PathBuf};

fn asset(path: &str) -> Asset {
    Asset {
        kind: AssetKind::Image,
        relative_path: PathBuf::from(path),
        media_type: "image/png".into(),
        data: Bytes::from_static(b"asset"),
        md5: "md5".into(),
    }
}

#[test]
fn invalid_and_duplicate_assets_do_not_publish() {
    let temp = tempfile::tempdir().unwrap();
    let absent = temp.path().join("absent");
    assert!(
        write_outputs(
            &Document {
                assets: vec![asset("../escape.png")],
                ..Default::default()
            },
            &absent
        )
        .is_err()
    );
    assert!(!absent.exists());

    let existing = temp.path().join("existing");
    fs::create_dir(&existing).unwrap();
    fs::write(existing.join("keep"), b"keep").unwrap();
    assert!(
        write_outputs(
            &Document {
                assets: vec![asset("assets/a.png"), asset("assets/./a.png")],
                ..Default::default()
            },
            &existing
        )
        .is_err()
    );
    assert_eq!(fs::read(existing.join("keep")).unwrap(), b"keep");
    assert!(!existing.join("assets").exists());
}

#[test]
fn publishes_all_outputs_and_assets() {
    let temp = tempfile::tempdir().unwrap();
    let output = temp.path().join("output");
    fs::create_dir(&output).unwrap();
    fs::write(output.join("stale.txt"), b"old").unwrap();
    fs::create_dir(output.join("stale")).unwrap();
    fs::write(output.join("stale/file.txt"), b"old").unwrap();
    let document = Document {
        markdown: "# ok".into(),
        assets: vec![asset("assets/a.png")],
        ..Default::default()
    };
    let manifest = write_outputs(&document, &output).unwrap();
    assert!(manifest.document_json.exists());
    assert!(manifest.markdown.exists());
    assert!(manifest.middle_json.exists());
    assert!(manifest.content_list.exists());
    assert_eq!(fs::read(output.join("assets/a.png")).unwrap(), b"asset");
    assert!(!output.join("stale.txt").exists());
    assert!(!output.join("stale").exists());
}

#[test]
fn publishes_mineru_pdf_info_and_markdown_asset_reference() {
    let temp = tempfile::tempdir().unwrap();
    let output = temp.path().join("output");
    let document = Document {
        markdown: "![figure](assets/0-1-image-deadbeef.png)".into(),
        middle_json: json!({"pdf_info": [{"page_index": 0, "page_size": [100, 200], "preproc_blocks": [], "para_blocks": [], "discarded_blocks": []}]}),
        assets: vec![asset("assets/0-1-image-deadbeef.png")],
        ..Default::default()
    };
    let manifest = write_outputs(&document, &output).unwrap();
    let middle: serde_json::Value =
        serde_json::from_slice(&fs::read(manifest.middle_json).unwrap()).unwrap();
    assert!(middle["pdf_info"].is_array());
    assert!(middle["pdf_info"][0]["para_blocks"].is_array());
    assert!(
        fs::read_to_string(manifest.markdown)
            .unwrap()
            .contains("assets/0-1-image-deadbeef.png")
    );
}
