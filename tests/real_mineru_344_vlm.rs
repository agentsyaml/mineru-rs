mod common;

use common::{pinned_venv, reference_env, required, run};
use lopdf::{Document, Object};
use serde_json::Value;
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
    time::Duration,
};

const BBOX_TOLERANCE: f64 = 0.02;

#[test]
#[ignore = "requires MINERU_RUN_REFERENCE=1, configured reference server, and a caller-provided multi-page PDF"]
fn compares_mineru_344_vlm_semantics() {
    if std::env::var("MINERU_RUN_REFERENCE").ok().as_deref() != Some("1") {
        eprintln!("skipping reference comparison: set MINERU_RUN_REFERENCE=1");
        return;
    }
    let (url, model, token) = reference_env();
    let input = PathBuf::from(required("MINERU_REFERENCE_INPUT"));
    assert!(
        input.is_file()
            && input
                .extension()
                .is_some_and(|e| e.eq_ignore_ascii_case("pdf")),
        "MINERU_REFERENCE_INPUT must be an existing PDF"
    );
    let source = Document::load(&input).expect("MINERU_REFERENCE_INPUT is not a readable PDF");
    assert!(
        source.get_pages().len() >= 2,
        "MINERU_REFERENCE_INPUT must have at least two pages"
    );
    let stem = input
        .file_stem()
        .and_then(|s| s.to_str())
        .expect("input PDF has no UTF-8 stem");
    let temp = tempfile::tempdir().unwrap();
    let python = pinned_venv(temp.path(), token.as_deref());
    let rust = temp.path().join("rust");
    let upstream = temp.path().join("upstream");
    let mut ours = Command::new(env!("CARGO_BIN_EXE_mineru-vlm"));
    ours.arg(&input)
        .args(["--base-url", &url, "--model", &model, "--output"])
        .arg(&rust);
    if let Some(token) = &token {
        ours.env("MINERU_VL_API_KEY", token);
    }
    run(
        &mut ours,
        "Rust CLI",
        Duration::from_secs(600),
        token.as_deref(),
    );
    let mut reference = Command::new(python.with_file_name("mineru"));
    reference
        .arg("-p")
        .arg(&input)
        .args(["-o"])
        .arg(&upstream)
        .args(["-b", "vlm-http-client", "-u", &url])
        .env("MINERU_VL_MODEL_NAME", &model);
    if let Some(token) = &token {
        reference.env("MINERU_VL_API_KEY", token);
    }
    run(
        &mut reference,
        "MinerU 3.4.4",
        Duration::from_secs(600),
        token.as_deref(),
    );

    let py = upstream.join(stem).join("vlm");
    let py_middle = py.join(format!("{stem}_middle.json"));
    let py_content = py.join(format!("{stem}_content_list.json"));
    let rust_middle = rust.join("middle.json");
    let rust_content = rust.join("content_list.json");
    for path in [
        &py_middle,
        &py_content,
        &py.join(format!("{stem}.md")),
        &py.join(format!("{stem}_layout.pdf")),
        &rust_middle,
        &rust_content,
        &rust.join("document.md"),
        &rust.join(format!("{stem}_layout.pdf")),
    ] {
        assert!(
            path.is_file(),
            "required output artifact is missing: {}",
            path.display()
        );
    }
    let ours_middle = read_json(&rust_middle);
    let py_middle_json = read_json(&py_middle);
    let ours_pages = ours_middle["pdf_info"]
        .as_array()
        .expect("Rust middle.json pdf_info missing");
    let py_pages = py_middle_json
        .pointer("/pdf_info")
        .and_then(Value::as_array)
        .expect("Python middle.json pdf_info missing");
    assert_eq!(
        source.get_pages().len(),
        ours_pages.len(),
        "source/Rust page count mismatch"
    );
    assert_eq!(
        source.get_pages().len(),
        py_pages.len(),
        "source/Python page count mismatch"
    );
    for (i, (a, b)) in ours_pages.iter().zip(py_pages).enumerate() {
        compare_blocks(i, a, b);
    }
    assert_eq!(
        normalize(fs::read_to_string(rust.join("document.md")).unwrap()),
        normalize(fs::read_to_string(py.join(format!("{stem}.md"))).unwrap()),
        "normalized Markdown mismatch"
    );
    assert_eq!(
        content_projection(&read_json(&rust_content)),
        content_projection(&read_json(&py_content)),
        "content-list projection mismatch"
    );
    assert_assets(&rust, &[&read_json(&rust_content), &ours_middle], "Rust");
    assert_assets(&py, &[&read_json(&py_content), &py_middle_json], "Python");
    assert_preview(&source, &rust.join(format!("{stem}_layout.pdf")), "Rust");
    assert_preview(&source, &py.join(format!("{stem}_layout.pdf")), "Python");
}

fn read_json(path: &Path) -> Value {
    serde_json::from_slice(
        &fs::read(path).unwrap_or_else(|e| panic!("missing {}: {e}", path.display())),
    )
    .unwrap_or_else(|e| panic!("invalid JSON {}: {e}", path.display()))
}
fn normalize(s: String) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}
fn blocks(page: &Value) -> &[Value] {
    page["preproc_blocks"]
        .as_array()
        .expect("middle.json page preproc_blocks missing")
}
fn compare_blocks(page: usize, rust: &Value, python: &Value) {
    let a = blocks(rust);
    let b = blocks(python);
    assert_eq!(a.len(), b.len(), "page {page} block count mismatch");
    for (ordinal, (a, b)) in a.iter().zip(b).enumerate() {
        assert_eq!(
            a["kind"].as_str().or_else(|| a["type"].as_str()),
            b["kind"].as_str().or_else(|| b["type"].as_str()),
            "page {page} block {ordinal} kind/type mismatch"
        );
        let ab = rust_bbox(a);
        let bb = python_bbox(b, python);
        assert!(
            ab.iter()
                .zip(bb)
                .all(|(x, y)| (x - y).abs() <= BBOX_TOLERANCE),
            "page {page} block {ordinal} bbox mismatch (tolerance {BBOX_TOLERANCE}): {ab:?} != {bb:?}"
        );
    }
}
fn number(v: &Value, what: &str) -> f64 {
    v.as_f64()
        .unwrap_or_else(|| panic!("invalid {what}: expected number"))
}
fn rust_bbox(block: &Value) -> [f64; 4] {
    let b = block["bbox"]
        .as_object()
        .expect("invalid Rust bbox: expected {left,top,right,bottom}");
    [
        number(&b["left"], "Rust bbox.left"),
        number(&b["top"], "Rust bbox.top"),
        number(&b["right"], "Rust bbox.right"),
        number(&b["bottom"], "Rust bbox.bottom"),
    ]
}
fn python_bbox(block: &Value, page: &Value) -> [f64; 4] {
    let b = block["bbox"]
        .as_array()
        .expect("invalid Python bbox: expected [left, top, right, bottom]");
    assert_eq!(b.len(), 4, "invalid Python bbox: expected four values");
    let mut out = [0, 1, 2, 3].map(|i| number(&b[i], "Python bbox"));
    if out.iter().any(|v| v.abs() > 1.0) {
        let size = page["page_size"]
            .as_array()
            .or_else(|| page["page_bbox"].as_array())
            .expect("Python page-unit bbox requires page_size/page_bbox");
        assert!(
            size.len() >= 4 || size.len() == 2,
            "invalid Python page size"
        );
        let (w, h) = if size.len() == 2 {
            (
                number(&size[0], "page width"),
                number(&size[1], "page height"),
            )
        } else {
            (
                number(&size[2], "page width") - number(&size[0], "page left"),
                number(&size[3], "page height") - number(&size[1], "page top"),
            )
        };
        assert!(w > 0.0 && h > 0.0, "invalid Python page dimensions");
        out = [out[0] / w, out[1] / h, out[2] / w, out[3] / h];
    }
    out
}
// Stable content-list projection: preserves list order and item type plus text/content only; metadata is deliberately excluded.
fn content_projection(value: &Value) -> Vec<String> {
    value
        .as_array()
        .expect("content-list must be an array")
        .iter()
        .map(|item| {
            let kind = item
                .get("type")
                .or_else(|| item.get("kind"))
                .and_then(Value::as_str)
                .unwrap_or("");
            let text = item
                .get("text")
                .or_else(|| item.get("content").filter(|v| v.is_string()))
                .and_then(Value::as_str)
                .map(|s| normalize(s.to_owned()))
                .unwrap_or_default();
            format!("{kind}:{text}")
        })
        .collect()
}
fn assert_assets(root: &Path, values: &[&Value], name: &str) {
    for path in values.iter().flat_map(|value| asset_paths(value)) {
        assert!(
            root.join(&path).is_file(),
            "{name} referenced asset missing: {path}"
        );
    }
}
fn asset_paths(v: &Value) -> Vec<String> {
    let mut out = Vec::new();
    fn walk(v: &Value, out: &mut Vec<String>) {
        match v {
            Value::Array(a) => {
                for x in a {
                    walk(x, out)
                }
            }
            Value::Object(o) => {
                for (k, x) in o {
                    if (k == "asset_path" || k == "relative_path" || k.ends_with("_path"))
                        && x.is_string()
                    {
                        out.push(x.as_str().unwrap().to_owned());
                    } else {
                        walk(x, out)
                    }
                }
            }
            _ => {}
        }
    }
    walk(v, &mut out);
    out
}

fn effective_page_value(doc: &Document, mut id: lopdf::ObjectId, key: &[u8]) -> Option<Object> {
    loop {
        let page = doc.get_object(id).unwrap().as_dict().unwrap();
        if let Ok(value) = page.get(key) {
            return Some(value.clone());
        }
        id = page.get(b"Parent").ok()?.as_reference().ok()?;
    }
}

fn assert_preview(source: &Document, path: &Path, name: &str) {
    let preview = Document::load(path).unwrap_or_else(|e| panic!("invalid {name} preview: {e}"));
    assert_eq!(
        source.get_pages().len(),
        preview.get_pages().len(),
        "{name} preview page count mismatch"
    );
    for n in 1..=source.get_pages().len() as u32 {
        let box_of = |d: &Document| {
            effective_page_value(d, d.get_pages()[&n], b"CropBox")
                .or_else(|| effective_page_value(d, d.get_pages()[&n], b"MediaBox"))
                .expect("preview/source page box missing")
                .as_array()
                .unwrap()
                .iter()
                .map(|x| x.as_i64().unwrap())
                .collect::<Vec<_>>()
        };
        assert_eq!(
            box_of(source),
            box_of(&preview),
            "{name} preview page {n} box mismatch"
        );
    }
}
