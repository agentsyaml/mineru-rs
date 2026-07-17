// Pipeline asset shaping is isolated from the VLM service in pipeline.rs unit tests.
// This target keeps the output contract executable without either external service.
use bytes::Bytes;
use mineru::{Asset, AssetKind, Document, write_outputs};
use std::path::PathBuf;

#[test]
fn shaped_asset_is_published_at_its_relative_path() {
    let temp = tempfile::tempdir().unwrap();
    let asset = Asset {
        kind: AssetKind::Image,
        relative_path: PathBuf::from("assets/0-0-image-12345678.png"),
        media_type: "image/png".into(),
        data: Bytes::from_static(b"png"),
        md5: "12345678".into(),
    };
    let document = Document {
        markdown: "![x](assets/0-0-image-12345678.png)".into(),
        assets: vec![asset],
        ..Default::default()
    };
    write_outputs(&document, temp.path()).unwrap();
    assert!(temp.path().join("assets/0-0-image-12345678.png").exists());
}
