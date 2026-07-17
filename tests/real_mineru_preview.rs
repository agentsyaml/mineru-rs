mod common;

use common::{pinned_venv, reference_env, run};
use lopdf::{Document, Object};
use std::{path::Path, process::Command, time::Duration};

type Overlay = (usize, String, Vec<[(f32, f32); 4]>);

fn effective_page_value(doc: &Document, mut id: lopdf::ObjectId, key: &[u8]) -> Option<Object> {
    loop {
        let page = doc.get_object(id).unwrap().as_dict().unwrap();
        if let Ok(value) = page.get(key) {
            return Some(value.clone());
        }
        id = page.get(b"Parent").ok()?.as_reference().ok()?;
    }
}

fn page_box(doc: &Document, page: lopdf::ObjectId) -> [i64; 4] {
    let values = effective_page_value(doc, page, b"CropBox")
        .or_else(|| effective_page_value(doc, page, b"MediaBox"))
        .unwrap()
        .as_array()
        .unwrap()
        .clone();
    [0, 1, 2, 3].map(|i| values[i].as_i64().unwrap())
}

fn rotation(doc: &Document, page: lopdf::ObjectId) -> i64 {
    effective_page_value(doc, page, b"Rotate")
        .and_then(|value| value.as_i64().ok())
        .unwrap_or(0)
        .rem_euclid(360)
}

fn stream_content(doc: &Document, object: &Object) -> Vec<String> {
    match object {
        Object::Reference(id) => stream_content(doc, doc.get_object(*id).unwrap()),
        Object::Array(objects) => objects
            .iter()
            .flat_map(|o| stream_content(doc, o))
            .collect(),
        Object::Stream(stream) => vec![
            String::from_utf8_lossy(
                &stream
                    .decompressed_content()
                    .unwrap_or_else(|_| stream.content.clone()),
            )
            .into_owned(),
        ],
        _ => Vec::new(),
    }
}

fn fill_polygons(content: &str) -> Vec<[(f32, f32); 4]> {
    let tokens: Vec<_> = content.split_whitespace().collect();
    let paths = tokens
        .windows(14)
        .filter_map(|part| {
            (part[2] == "m"
                && part[5] == "l"
                && part[8] == "l"
                && part[11] == "l"
                && part[12] == "h"
                && part[13] == "f")
                .then(|| {
                    Some([
                        (part[0].parse().ok()?, part[1].parse().ok()?),
                        (part[3].parse().ok()?, part[4].parse().ok()?),
                        (part[6].parse().ok()?, part[7].parse().ok()?),
                        (part[9].parse().ok()?, part[10].parse().ok()?),
                    ])
                })
                .flatten()
        })
        .collect::<Vec<_>>();
    let rectangles = tokens.windows(5).filter_map(|part| {
        (part[4] == "re")
            .then(|| {
                let (x, y, width, height) = (
                    part[0].parse::<f32>().ok()?,
                    part[1].parse::<f32>().ok()?,
                    part[2].parse::<f32>().ok()?,
                    part[3].parse::<f32>().ok()?,
                );
                Some([
                    (x, y),
                    (x + width, y),
                    (x + width, y + height),
                    (x, y + height),
                ])
            })
            .flatten()
    });
    paths.into_iter().chain(rectangles).collect()
}

fn colors(content: &str) -> Vec<[f32; 3]> {
    content
        .lines()
        .filter_map(|line| {
            let parts: Vec<_> = line.split_whitespace().collect();
            if parts.len() != 4 || parts[3] != "rg" {
                return None;
            }
            Some([
                parts[0].parse().ok()?,
                parts[1].parse().ok()?,
                parts[2].parse().ok()?,
            ])
        })
        .collect()
}

fn overlays(doc: &Document) -> Vec<Overlay> {
    doc.get_pages()
        .values()
        .enumerate()
        .filter_map(|(index, id)| {
            let page = doc.get_object(*id).unwrap().as_dict().unwrap();
            let text = stream_content(doc, page.get(b"Contents").unwrap()).join("\n");
            let polygons = fill_polygons(&text);
            (!polygons.is_empty() && text.contains(" Tj")).then_some((index, text, polygons))
        })
        .collect()
}

fn has_alpha(doc: &Document) -> bool {
    doc.get_pages().values().any(|page| {
        let page = doc.get_object(*page).unwrap().as_dict().unwrap();
        let resources = page.get(b"Resources").unwrap().as_dict().unwrap();
        let states = resources.get(b"ExtGState").unwrap().as_dict().unwrap();
        states.iter().any(|(_, state)| {
            state
                .as_dict()
                .ok()
                .and_then(|s| s.get(b"ca").ok())
                .and_then(|alpha| alpha.as_f32().ok())
                .is_some_and(|alpha| (alpha - 0.3).abs() < f32::EPSILON)
        })
    })
}

fn assert_page_parity(source: &Document, preview: &Document) {
    assert_eq!(source.get_pages().len(), 1);
    assert_eq!(preview.get_pages().len(), source.get_pages().len());
    for number in 1..=source.get_pages().len() as u32 {
        let source_page = source.get_pages()[&number];
        let preview_page = preview.get_pages()[&number];
        assert_eq!(
            page_box(source, source_page),
            page_box(preview, preview_page)
        );
        assert_eq!(
            rotation(source, source_page),
            rotation(preview, preview_page)
        );
    }
}

#[test]
#[ignore = "requires MINERU_RUN_REFERENCE=1 and a configured external MinerU reference server"]
fn compares_preview_semantics_with_mineru_3_4_4() {
    if std::env::var("MINERU_RUN_REFERENCE").ok().as_deref() != Some("1") {
        eprintln!("skipping reference comparison: set MINERU_RUN_REFERENCE=1");
        return;
    }
    let (url, model, token) = reference_env();
    let temp = tempfile::tempdir().unwrap();
    let python = pinned_venv(temp.path(), token.as_deref());
    let input = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/pdf/minimal.pdf");
    let rust = temp.path().join("rust");
    let upstream = temp.path().join("upstream");
    let mut ours = Command::new(env!("CARGO_BIN_EXE_mineru-vlm"));
    ours.args([
        input.as_os_str(),
        "--base-url".as_ref(),
        url.as_ref(),
        "--model".as_ref(),
        model.as_ref(),
        "--output".as_ref(),
        rust.as_os_str(),
    ]);
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
        .args(["-b", "vlm-http-client", "-u", &url]);
    reference.env("MINERU_VL_MODEL_NAME", &model);
    if let Some(token) = &token {
        reference.env("MINERU_VL_API_KEY", token);
    }
    run(
        &mut reference,
        "MinerU 3.4.4",
        Duration::from_secs(600),
        token.as_deref(),
    );
    let source = Document::load(&input).unwrap();
    let ours = Document::load(rust.join("minimal_layout.pdf")).unwrap();
    let reference_path = upstream
        .join("minimal")
        .join("vlm")
        .join("minimal_layout.pdf");
    assert!(
        reference_path.is_file(),
        "expected upstream VLM preview is missing"
    );
    let reference = Document::load(&reference_path).expect("invalid upstream preview PDF");
    assert_page_parity(&source, &ours);
    assert_page_parity(&source, &reference);
    let ours_overlays = overlays(&ours);
    let reference_overlays = overlays(&reference);
    assert!(
        !ours_overlays.is_empty(),
        "Rust preview overlay geometry missing"
    );
    assert!(
        !reference_overlays.is_empty(),
        "upstream preview overlay geometry missing"
    );
    assert!(
        ours_overlays.iter().any(|(index, _, _)| *index == 0),
        "Rust page 1 overlay geometry missing"
    );
    assert!(
        reference_overlays.iter().any(|(index, _, _)| *index == 0),
        "upstream page 1 overlay geometry missing"
    );
    assert!(
        ours_overlays
            .iter()
            .any(|(_, text, _)| text.contains("(1)")),
        "Rust preview label missing"
    );
    assert!(
        reference_overlays
            .iter()
            .any(|(_, text, _)| text.contains("(1)")),
        "upstream preview label missing"
    );
    assert!(has_alpha(&ours), "Rust alpha /ca 0.3 missing");
    assert!(has_alpha(&reference), "upstream alpha /ca 0.3 missing");
    let palette = [
        [0.6, 0.0, 0.298],
        [0.4, 0.4, 1.0],
        [0.0, 1.0, 0.0],
        [0.4, 0.0, 0.8],
        [0.8, 0.8, 0.0],
        [0.6, 1.0, 0.2],
        [0.4, 0.698, 1.0],
        [1.0, 0.0, 0.0],
    ];
    let ours_colors: Vec<_> = ours_overlays
        .iter()
        .flat_map(|(_, text, _)| colors(text))
        .collect();
    let upstream_colors: Vec<_> = reference_overlays
        .iter()
        .flat_map(|(_, text, _)| colors(text))
        .collect();
    for (name, preview_colors) in [("Rust", &ours_colors), ("upstream", &upstream_colors)] {
        assert!(!preview_colors.is_empty(), "{name} fill palette missing");
        assert!(
            preview_colors
                .iter()
                .all(|color| palette.iter().any(|expected| color
                    .iter()
                    .zip(expected)
                    .all(|(a, b)| (a - b).abs() < 0.01))),
            "unexpected {name} normalized palette"
        );
    }
    for (doc, overlays) in [(&ours, &ours_overlays), (&reference, &reference_overlays)] {
        for (index, _, polygons) in overlays {
            let crop = page_box(doc, doc.get_pages()[&((*index + 1) as u32)]);
            assert!(
                polygons
                    .iter()
                    .flatten()
                    .all(|(x, y)| (crop[0] as f32..=crop[2] as f32).contains(x)
                        && (crop[1] as f32..=crop[3] as f32).contains(y)),
                "overlay outside CropBox"
            );
        }
    }
}
