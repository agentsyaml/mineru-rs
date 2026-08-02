#![cfg(feature = "office")]
use std::{
    io::{Cursor, Read, Write},
    process::{Command, Stdio},
    time::{Duration, Instant},
};
#[path = "support/office_fixtures.rs"]
mod office_fixtures;
use office_fixtures::{docx, pptx, pptx_two_slides, xlsx};
use zip::{CompressionMethod, ZipArchive, ZipWriter, write::SimpleFileOptions};

const CAP: usize = 32 * 1024 * 1024;

fn run(args: &[&str], input: &[u8]) -> std::process::Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_mineru-office-convert"))
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(input).unwrap();
    child.wait_with_output().unwrap()
}
fn assert_pdf(format: &str, bytes: Vec<u8>) {
    let output = run(&[format], &bytes);
    assert!(
        output.status.success(),
        "{format}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stdout.starts_with(b"%PDF-"));
    assert!(output.stderr.len() <= 4096);
    assert!(
        !lopdf::Document::load_mem(&output.stdout)
            .unwrap()
            .get_pages()
            .is_empty()
    );
}

fn hostile_docx(attributes: &str) -> Vec<u8> {
    let mut zip = ZipWriter::new(Cursor::new(Vec::new()));
    let options = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
    let document = format!("<document {attributes}/>");
    for (name, contents) in [
        (
            "[Content_Types].xml",
            r#"<?xml version="1.0"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/></Types>"#,
        ),
        (
            "_rels/.rels",
            r#"<?xml version="1.0"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="word/document.xml"/></Relationships>"#,
        ),
        ("word/document.xml", document.as_str()),
    ] {
        zip.start_file(name, options).unwrap();
        zip.write_all(contents.as_bytes()).unwrap();
    }
    zip.finish().unwrap().into_inner()
}

fn assert_preflight_rejects(format: &str, bytes: Vec<u8>) {
    let started = Instant::now();
    let output = run(&[format], &bytes);
    assert!(started.elapsed() < Duration::from_secs(10));
    assert!(!output.status.success());
    assert!(output.stdout.is_empty() && output.stderr.len() <= 4096);
    assert_eq!(
        output.stderr,
        b"input format does not match requested format\n"
    );
}

fn pptx_with_chart(
    relationship: &str,
    target: &str,
    chart: &[u8],
    chart_options: SimpleFileOptions,
    mixed_case: bool,
) -> Vec<u8> {
    let mut source = ZipArchive::new(Cursor::new(pptx())).unwrap();
    let mut zip = ZipWriter::new(Cursor::new(Vec::new()));
    let options = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
    for index in 0..source.len() {
        let mut entry = source.by_index(index).unwrap();
        let mut name = entry.name().to_owned();
        let mut contents = Vec::new();
        entry.read_to_end(&mut contents).unwrap();
        if mixed_case {
            name = match name.as_str() {
                "ppt/slides/slide1.xml" => "ppt/Slides/Slide1.XML".into(),
                "ppt/slides/_rels/slide1.xml.rels" => "ppt/Slides/_rels/Slide1.XML.rels".into(),
                _ => name,
            };
            if name == "ppt/presentation.xml" {
                contents = String::from_utf8(contents)
                    .unwrap()
                    .replace("slides/slide1.xml", "Slides/Slide1.XML")
                    .into_bytes();
            } else if name == "[Content_Types].xml" {
                contents = String::from_utf8(contents)
                    .unwrap()
                    .replace("/ppt/slides/slide1.xml", "/ppt/Slides/Slide1.XML")
                    .into_bytes();
            }
        }
        if name.ends_with("slide1.xml.rels") || name.ends_with("Slide1.XML.rels") {
            let relationships = String::from_utf8(contents).unwrap();
            contents = relationships
                .replace(
                    "</Relationships>",
                    &format!("{relationship}</Relationships>"),
                )
                .into_bytes();
        }
        zip.start_file(name, options).unwrap();
        zip.write_all(&contents).unwrap();
    }
    zip.start_file(target, chart_options).unwrap();
    zip.write_all(chart).unwrap();
    zip.finish().unwrap().into_inner()
}

fn chart_relationship(target: &str, extra: &str) -> String {
    format!(
        r#"<Relationship Id="chart" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/chart" Target="{target}"{extra}/>"#
    )
}

fn junk_chart() -> Vec<u8> {
    format!("junk<chart {}/>", "a=\"x\" ".repeat(257)).into_bytes()
}

fn rebuilt_fixture(
    base: Vec<u8>,
    replacements: &[(&str, Vec<u8>)],
    additions: &[(&str, &[u8])],
) -> Vec<u8> {
    let mut source = ZipArchive::new(Cursor::new(base)).unwrap();
    let mut zip = ZipWriter::new(Cursor::new(Vec::new()));
    let options = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
    for index in 0..source.len() {
        let mut entry = source.by_index(index).unwrap();
        let name = entry.name().to_owned();
        let mut contents = Vec::new();
        entry.read_to_end(&mut contents).unwrap();
        if let Some((_, replacement)) = replacements.iter().find(|(path, _)| *path == name) {
            contents = replacement.clone();
        }
        zip.start_file(name, options).unwrap();
        zip.write_all(&contents).unwrap();
    }
    for (name, contents) in additions {
        zip.start_file(name, options).unwrap();
        zip.write_all(contents).unwrap();
    }
    zip.finish().unwrap().into_inner()
}

fn docx_with_header(header: &[u8]) -> Vec<u8> {
    let mut source = ZipArchive::new(Cursor::new(docx())).unwrap();
    let mut document = String::new();
    source
        .by_name("word/document.xml")
        .unwrap()
        .read_to_string(&mut document)
        .unwrap();
    let mut relationships = String::new();
    source
        .by_name("word/_rels/document.xml.rels")
        .unwrap()
        .read_to_string(&mut relationships)
        .unwrap();
    let document = document
        .replace(
            "xmlns:w=\"http://schemas.openxmlformats.org/wordprocessingml/2006/main\"",
            "xmlns:w=\"http://schemas.openxmlformats.org/wordprocessingml/2006/main\" xmlns:r=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships\"",
        )
        .replace(
            "<w:sectPr/>",
            "<w:sectPr><w:headerReference w:type=\"default\" r:id=\"rId2\"/></w:sectPr>",
        );
    let relationships = relationships.replace(
        "</Relationships>",
        "<Relationship Id=\"rId2\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/header\" Target=\"header.bin\"/></Relationships>",
    );
    rebuilt_fixture(
        docx(),
        &[
            ("word/document.xml", document.into_bytes()),
            ("word/_rels/document.xml.rels", relationships.into_bytes()),
        ],
        &[("word/header.bin", header)],
    )
}

fn xlsx_with_sheet(sheet: &[u8]) -> Vec<u8> {
    let relationships = r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet.bin"/><Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles" Target="styles.xml"/></Relationships>"#;
    rebuilt_fixture(
        xlsx(),
        &[(
            "xl/_rels/workbook.xml.rels",
            relationships.as_bytes().to_vec(),
        )],
        &[("xl/worksheets/sheet.bin", sheet)],
    )
}

fn xlsx_with_drawing(drawing: &[u8], chart: &[u8]) -> Vec<u8> {
    let mut source = ZipArchive::new(Cursor::new(xlsx())).unwrap();
    let mut worksheet = String::new();
    source
        .by_name("xl/worksheets/sheet1.xml")
        .unwrap()
        .read_to_string(&mut worksheet)
        .unwrap();
    let worksheet = worksheet
        .replace(
            "xmlns=\"http://schemas.openxmlformats.org/spreadsheetml/2006/main\"",
            "xmlns=\"http://schemas.openxmlformats.org/spreadsheetml/2006/main\" xmlns:r=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships\"",
        )
        .replace("</worksheet>", "<drawing r:id=\"rId1\"/></worksheet>");
    rebuilt_fixture(
        xlsx(),
        &[("xl/worksheets/sheet1.xml", worksheet.into_bytes())],
        &[
            (
                "xl/worksheets/_rels/sheet1.xml.rels",
                br#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/drawing" Target="../drawings/drawing.bin"/></Relationships>"#,
            ),
            ("xl/drawings/drawing.bin", drawing),
            (
                "xl/drawings/_rels/drawing.bin.rels",
                br#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/chart" Target="../charts/chart.bin"/><Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/image" Target="../media/image.bin"/></Relationships>"#,
            ),
            ("xl/charts/chart.bin", chart),
            ("xl/media/image.bin", b"ordinary binary media"),
        ],
    )
}

const XLSX_DRAWING: &[u8] = br#"<xdr:wsDr xmlns:xdr="http://schemas.openxmlformats.org/drawingml/2006/spreadsheetDrawing" xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:c="http://schemas.openxmlformats.org/drawingml/2006/chart" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><xdr:twoCellAnchor><xdr:from><xdr:col>0</xdr:col><xdr:colOff>0</xdr:colOff><xdr:row>0</xdr:row><xdr:rowOff>0</xdr:rowOff></xdr:from><xdr:to><xdr:col>1</xdr:col><xdr:colOff>0</xdr:colOff><xdr:row>1</xdr:row><xdr:rowOff>0</xdr:rowOff></xdr:to><xdr:graphicFrame><xdr:nvGraphicFramePr><xdr:cNvPr id="1" name="Chart"/><xdr:cNvGraphicFramePr/></xdr:nvGraphicFramePr><xdr:xfrm/><a:graphic><a:graphicData uri="http://schemas.openxmlformats.org/drawingml/2006/chart"><c:chart r:id="rId1"/></a:graphicData></a:graphic></xdr:graphicFrame><xdr:clientData/></xdr:twoCellAnchor></xdr:wsDr>"#;
const CHART_XML: &[u8] = br#"<c:chartSpace xmlns:c="http://schemas.openxmlformats.org/drawingml/2006/chart"><c:chart><c:plotArea><c:layout/></c:plotArea></c:chart></c:chartSpace>"#;

#[test]
fn helper_argument_errors_run_after_containment_setup() {
    for (args, expected) in [
        (&[][..], "usage: mineru-office-convert <docx|pptx|xlsx>\n"),
        (&["bad"][..], "invalid format\n"),
    ] {
        let output = run(args, b"");
        assert!(!output.status.success());
        assert!(output.stdout.is_empty() && output.stderr.len() <= 4096);
        assert_eq!(output.stderr, expected.as_bytes());
    }
}

#[test]
fn helper_preflight_rejects_257_attributes_in_document_xml() {
    assert_preflight_rejects("docx", hostile_docx(&"a=\"x\" ".repeat(257)));
}

#[test]
fn helper_preflight_rejects_257_namespaces_in_document_xml() {
    assert_preflight_rejects(
        "docx",
        hostile_docx(
            &(0..257)
                .map(|index| format!("xmlns:n{index}=\"urn:{index}\""))
                .collect::<Vec<_>>()
                .join(" "),
        ),
    );
}

#[test]
fn helper_converts_relationship_selected_docx_header_bin() {
    assert_pdf(
        "docx",
        docx_with_header(
            br#"<w:hdr xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:p><w:r><w:t>Header</w:t></w:r></w:p></w:hdr>"#,
        ),
    );
}

#[test]
fn helper_preflight_rejects_relationship_selected_docx_header_bin() {
    assert_preflight_rejects("docx", docx_with_header(&junk_chart()));
}

#[test]
fn helper_converts_relationship_selected_xlsx_sheet_bin() {
    let mut source = ZipArchive::new(Cursor::new(xlsx())).unwrap();
    let mut sheet = Vec::new();
    source
        .by_name("xl/worksheets/sheet1.xml")
        .unwrap()
        .read_to_end(&mut sheet)
        .unwrap();
    assert_pdf("xlsx", xlsx_with_sheet(&sheet));
}

#[test]
fn helper_preflight_rejects_relationship_selected_xlsx_sheet_bin() {
    assert_preflight_rejects("xlsx", xlsx_with_sheet(&junk_chart()));
}

#[test]
fn helper_converts_relationship_selected_xlsx_drawing_and_chart_bins() {
    assert_pdf("xlsx", xlsx_with_drawing(XLSX_DRAWING, CHART_XML));
}

#[test]
fn helper_preflight_rejects_relationship_selected_xlsx_drawing_bin() {
    assert_preflight_rejects("xlsx", xlsx_with_drawing(&junk_chart(), CHART_XML));
}

#[test]
fn helper_preflight_rejects_relationship_selected_xlsx_chart_bin() {
    assert_preflight_rejects("xlsx", xlsx_with_drawing(XLSX_DRAWING, &junk_chart()));
}

#[test]
fn helper_converts_arbitrary_suffix_chart_target() {
    let chart = br#"<c:chartSpace xmlns:c="http://schemas.openxmlformats.org/drawingml/2006/chart"><c:chart><c:plotArea><c:layout/></c:plotArea></c:chart></c:chartSpace>"#;
    assert_pdf(
        "pptx",
        pptx_with_chart(
            &chart_relationship("../charts/chart.bin", ""),
            "ppt/charts/chart.bin",
            chart,
            SimpleFileOptions::default(),
            false,
        ),
    );
}

#[test]
fn helper_preflight_rejects_junk_prefixed_chart_bin() {
    let chart = junk_chart();
    assert_preflight_rejects(
        "pptx",
        pptx_with_chart(
            &chart_relationship("../charts/chart.bin", ""),
            "ppt/charts/chart.bin",
            &chart,
            SimpleFileOptions::default(),
            false,
        ),
    );
}

#[test]
fn helper_preflight_rejects_external_chart_target() {
    let chart = junk_chart();
    assert_preflight_rejects(
        "pptx",
        pptx_with_chart(
            &chart_relationship("../charts/chart.bin", r#" TargetMode="External""#),
            "ppt/charts/chart.bin",
            &chart,
            SimpleFileOptions::default(),
            false,
        ),
    );
}

#[test]
fn helper_preflight_rejects_chart_target_without_type() {
    let chart = junk_chart();
    assert_preflight_rejects(
        "pptx",
        pptx_with_chart(
            r#"<Relationship Id="chart" Target="../charts/chart.bin"/>"#,
            "ppt/charts/chart.bin",
            &chart,
            SimpleFileOptions::default(),
            false,
        ),
    );
}

#[test]
fn helper_preflight_rejects_mixed_case_chart_target() {
    let chart = junk_chart();
    assert_preflight_rejects(
        "pptx",
        pptx_with_chart(
            &chart_relationship("../Charts/Chart.BIN", ""),
            "ppt/Charts/Chart.BIN",
            &chart,
            SimpleFileOptions::default(),
            true,
        ),
    );
}

#[test]
fn helper_preflight_rejects_root_escaping_chart_target() {
    let chart = junk_chart();
    assert_preflight_rejects(
        "pptx",
        pptx_with_chart(
            &chart_relationship("../../../../ppt/charts/chart.bin", ""),
            "ppt/charts/chart.bin",
            &chart,
            SimpleFileOptions::default(),
            false,
        ),
    );
}

#[test]
fn helper_preflight_rejects_oversized_whitespace_chart_bin() {
    let mut chart = vec![b' '; 8 * 1024 * 1024 + 1];
    chart.extend_from_slice(
        format!(
            "<chart {}/>",
            (0..257)
                .map(|index| format!("xmlns:n{index}=\"urn:{index}\""))
                .collect::<Vec<_>>()
                .join(" "),
        )
        .as_bytes(),
    );
    assert!(chart.len() < CAP);
    assert_preflight_rejects(
        "pptx",
        pptx_with_chart(
            &chart_relationship("../charts/chart.bin", ""),
            "ppt/charts/chart.bin",
            &chart,
            SimpleFileOptions::default().compression_method(CompressionMethod::Stored),
            false,
        ),
    );
}

#[test]
#[ignore = "real Office helper process e2e"]
fn helper_converts_self_authored_docx_pptx_and_xlsx() {
    assert_pdf("docx", docx());
    assert_pdf("pptx", pptx());
    assert_pdf("xlsx", xlsx());
    let output = run(&["pptx"], &pptx_two_slides());
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        lopdf::Document::load_mem(&output.stdout)
            .unwrap()
            .get_pages()
            .len(),
        2
    );
}
#[test]
#[ignore = "real Office helper process e2e"]
fn helper_rejects_bad_arguments_and_oversized_input() {
    for args in [&[][..], &["docx", "extra"], &["bad"]] {
        let output = run(args, b"");
        assert!(!output.status.success());
        assert!(output.stdout.is_empty() && output.stderr.len() <= 4096);
    }
    let output = run(&["docx"], &vec![0; CAP + 1]);
    assert!(!output.status.success());
    assert!(output.stdout.is_empty() && output.stderr.len() <= 4096);
}
