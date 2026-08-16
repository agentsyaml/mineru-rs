//! Hand-constructed minimal legacy-format fixtures, committed to `tests/fixtures/legacy/`.
//!
//! Every sample is built here from first principles (no external office suite) so the bytes are
//! reproducible: OLE compound files via `cfb`, BIFF8 workbook records for `.xls`, a legacy FIB +
//! piece table for `.doc`, PPT record atoms, ODF/EPUB zip packages, plain text for RTF/CSV.
use std::io::{Cursor, Write};
use zip::{CompressionMethod, ZipWriter, write::SimpleFileOptions};

fn ole(streams: &[(&str, &[u8])]) -> Vec<u8> {
    let mut compound = cfb::CompoundFile::create(Cursor::new(Vec::new())).unwrap();
    for (name, bytes) in streams {
        let mut stream = compound.create_stream(*name).unwrap();
        stream.write_all(bytes).unwrap();
    }
    compound.into_inner().into_inner()
}

/// Minimal legacy Word: an OLE2 `WordDocument` stream whose FIB (`wIdent` 0xA5EC, no complex
/// piece table) places the GBK-encoded text directly behind `fcMin`.
pub fn doc() -> Vec<u8> {
    let text = "Legacy DOC fixture 中文测试\r";
    let encoded = encoding_rs::GBK.encode(text).0.into_owned();
    let fc_min = 0x200usize;
    let fc_mac = fc_min + encoded.len();
    let mut fib = vec![0u8; fc_min];
    fib[0..2].copy_from_slice(&0xA5ECu16.to_le_bytes());
    fib[6..8].copy_from_slice(&0x0804u16.to_le_bytes()); // lid: Simplified Chinese -> GBK
    fib[0x0A..0x0C].copy_from_slice(&0u16.to_le_bytes()); // flags: no encryption, 0Table
    fib[0x18..0x1C].copy_from_slice(&(fc_min as u32).to_le_bytes());
    fib[0x1C..0x20].copy_from_slice(&(fc_mac as u32).to_le_bytes());
    fib[0x4C..0x50].copy_from_slice(&(encoded.len() as u32).to_le_bytes()); // ccpText
    fib[0x1A2..0x1A6].copy_from_slice(&0u32.to_le_bytes()); // fcClx: legacy single piece
    fib[0x1A6..0x1AA].copy_from_slice(&0u32.to_le_bytes()); // lcbClx
    fib.extend_from_slice(&encoded);
    ole(&[("WordDocument", &fib)])
}

/// Minimal legacy PowerPoint: a record stream with one title text shape. No `Current User`
/// stream, so anydoc falls back to its raw-order recovery path.
pub fn ppt() -> Vec<u8> {
    let text = "Legacy PPT fixture 中文测试";
    let mut stream = Vec::new();
    let mut record = |rec_type: u16, body: &[u8]| {
        stream.extend_from_slice(&0u16.to_le_bytes()); // ver_inst: atom
        stream.extend_from_slice(&rec_type.to_le_bytes());
        stream.extend_from_slice(&(body.len() as u32).to_le_bytes());
        stream.extend_from_slice(body);
    };
    record(0x0F9F, &[0x00]); // TextHeaderAtom, tx_type 0 (title)
    let units: Vec<u8> = text.encode_utf16().flat_map(|u| u.to_le_bytes()).collect();
    record(0x0FA0, &units); // TextCharsAtom, UTF-16LE
    ole(&[("PowerPoint Document", &stream)])
}

/// Minimal legacy Excel: an OLE2 `Workbook` stream holding a single-sheet BIFF8 workbook with
/// one label cell (UTF-16LE string, no code page record so calamine decodes UTF-16 natively).
pub fn xls() -> Vec<u8> {
    let name = "Sheet1";
    let text = "Legacy XLS fixture";
    let mut workbook = Vec::new();
    let mut record = |rec_type: u16, body: &[u8]| {
        workbook.extend_from_slice(&rec_type.to_le_bytes());
        workbook.extend_from_slice(&(body.len() as u16).to_le_bytes());
        workbook.extend_from_slice(body);
    };
    let bof = |dt: u16| {
        let mut body = Vec::new();
        body.extend_from_slice(&0x0600u16.to_le_bytes()); // BIFF8
        body.extend_from_slice(&dt.to_le_bytes());
        body.extend_from_slice(&[0u8; 12]);
        body
    };
    record(0x0809, &bof(0x0005)); // globals BOF
    let name_units: Vec<u8> = name.encode_utf16().flat_map(|u| u.to_le_bytes()).collect();
    let mut boundsheet = Vec::new();
    let sheet_offset = 20 + 4 + (6 + 2 + name_units.len()) + 4; // BOF + BoundSheet8 + EOF
    boundsheet.extend_from_slice(&(sheet_offset as u32).to_le_bytes());
    boundsheet.extend_from_slice(&[0x00, 0x00]); // visible, worksheet
    boundsheet.push(name.len() as u8);
    boundsheet.push(0x01); // fHighByte: UTF-16LE name
    boundsheet.extend_from_slice(&name_units);
    record(0x0085, &boundsheet);
    record(0x000A, &[]); // globals EOF
    record(0x0809, &bof(0x0010)); // worksheet BOF
    let mut dimensions = Vec::new();
    dimensions.extend_from_slice(&0u32.to_le_bytes()); // rwMic
    dimensions.extend_from_slice(&1u32.to_le_bytes()); // rwMac
    dimensions.extend_from_slice(&0u16.to_le_bytes()); // colMic
    dimensions.extend_from_slice(&1u16.to_le_bytes()); // colMac
    dimensions.extend_from_slice(&0u16.to_le_bytes());
    record(0x0200, &dimensions);
    let text_units: Vec<u8> = text.encode_utf16().flat_map(|u| u.to_le_bytes()).collect();
    let mut label = Vec::new();
    label.extend_from_slice(&0u16.to_le_bytes()); // rw
    label.extend_from_slice(&0u16.to_le_bytes()); // col
    label.extend_from_slice(&0u16.to_le_bytes()); // ixfe
    label.extend_from_slice(&(text.encode_utf16().count() as u16).to_le_bytes()); // cch
    label.push(0x01); // fHighByte: UTF-16LE text
    label.extend_from_slice(&text_units);
    record(0x0204, &label);
    record(0x000A, &[]); // sheet EOF
    ole(&[("Workbook", &workbook)])
}

pub fn rtf() -> Vec<u8> {
    b"{\\rtf1\\ansi Legacy RTF fixture\\par}".to_vec()
}

pub fn csv() -> Vec<u8> {
    "a,b,c\n中文,2,3\n".as_bytes().to_vec()
}

fn zip(parts: &[(&str, &[u8])]) -> Vec<u8> {
    let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
    let options = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
    for (name, bytes) in parts {
        writer.start_file(*name, options).unwrap();
        writer.write_all(bytes).unwrap();
    }
    writer.finish().unwrap().into_inner()
}

pub fn odt() -> Vec<u8> {
    zip(&[
        ("mimetype", b"application/vnd.oasis.opendocument.text"),
        (
            "content.xml",
            r#"<?xml version="1.0" encoding="UTF-8"?><office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0"><office:body><office:text><text:p>Legacy ODT fixture 中文</text:p></office:text></office:body></office:document-content>"#.as_bytes(),
        ),
    ])
}

pub fn ods() -> Vec<u8> {
    zip(&[
        ("mimetype", b"application/vnd.oasis.opendocument.spreadsheet"),
        (
            "content.xml",
            br#"<?xml version="1.0" encoding="UTF-8"?><office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0"><office:body><office:spreadsheet><table:table table:name="Sheet1"><table:table-row><table:table-cell><text:p>Legacy ODS fixture</text:p></table:table-cell></table:table-row></table:table></office:spreadsheet></office:body></office:document-content>"#,
        ),
    ])
}

pub fn odp() -> Vec<u8> {
    zip(&[
        ("mimetype", b"application/vnd.oasis.opendocument.presentation"),
        (
            "content.xml",
            br#"<?xml version="1.0" encoding="UTF-8"?><office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0" xmlns:draw="urn:oasis:names:tc:opendocument:xmlns:drawing:1.0" xmlns:presentation="urn:oasis:names:tc:opendocument:xmlns:presentation:1.0"><office:body><office:presentation><draw:page draw:name="Slide1"><draw:frame presentation:class="title"><draw:text-box><text:p>Legacy ODP fixture</text:p></draw:text-box></draw:frame></draw:page></office:presentation></office:body></office:document-content>"#,
        ),
    ])
}

pub fn epub() -> Vec<u8> {
    zip(&[
        ("mimetype", b"application/epub+zip"),
        (
            "META-INF/container.xml",
            br#"<?xml version="1.0"?><container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container"><rootfiles><rootfile full-path="content.opf" media-type="application/oebps-package+xml"/></rootfiles></container>"#,
        ),
        (
            "content.opf",
            br#"<?xml version="1.0"?><package xmlns="http://www.idpf.org/2007/opf" version="3.0"><metadata xmlns:dc="http://purl.org/dc/elements/1.1/"><dc:title>Legacy EPUB fixture</dc:title></metadata><manifest><item id="c1" href="chapter1.xhtml" media-type="application/xhtml+xml"/></manifest><spine><itemref idref="c1"/></spine></package>"#,
        ),
        (
            "chapter1.xhtml",
            r#"<html xmlns="http://www.w3.org/1999/xhtml"><head><title>Legacy EPUB fixture</title></head><body><p>Legacy EPUB fixture 中文</p></body></html>"#.as_bytes(),
        ),
    ])
}

pub struct LegacyFixture {
    pub name: &'static str,
    pub kind: &'static str,
    pub bytes: Vec<u8>,
    /// Expected markdown substrings the conversion must contain.
    pub expected: &'static [&'static str],
}

pub fn all() -> Vec<LegacyFixture> {
    vec![
        LegacyFixture {
            name: "minimal",
            kind: "doc",
            bytes: doc(),
            expected: &["Legacy DOC fixture", "中文测试"],
        },
        LegacyFixture {
            name: "minimal",
            kind: "ppt",
            bytes: ppt(),
            expected: &["Legacy PPT fixture", "中文测试"],
        },
        LegacyFixture {
            name: "minimal",
            kind: "xls",
            bytes: xls(),
            expected: &["Legacy XLS fixture"],
        },
        LegacyFixture {
            name: "minimal",
            kind: "rtf",
            bytes: rtf(),
            expected: &["Legacy RTF fixture"],
        },
        LegacyFixture {
            name: "minimal",
            kind: "csv",
            bytes: csv(),
            expected: &["中文", "3"],
        },
        LegacyFixture {
            name: "minimal",
            kind: "odt",
            bytes: odt(),
            expected: &["Legacy ODT fixture", "中文"],
        },
        LegacyFixture {
            name: "minimal",
            kind: "ods",
            bytes: ods(),
            expected: &["Legacy ODS fixture"],
        },
        LegacyFixture {
            name: "minimal",
            kind: "odp",
            bytes: odp(),
            expected: &["Legacy ODP fixture"],
        },
        LegacyFixture {
            name: "minimal",
            kind: "epub",
            bytes: epub(),
            expected: &["Legacy EPUB fixture", "中文"],
        },
    ]
}

/// One-shot regeneration of the committed binary fixtures under `tests/fixtures/legacy/`.
/// Run with `cargo test --test legacy_office_convert_helper regenerate_committed_legacy_fixtures -- --ignored`; normal runs skip it.
#[cfg(test)]
#[test]
#[ignore = "rewrites committed fixture binaries"]
fn regenerate_committed_legacy_fixtures() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/legacy");
    std::fs::create_dir_all(&root).unwrap();
    for fixture in all() {
        let path = root.join(format!("{}.{}", fixture.name, fixture.kind));
        std::fs::write(&path, &fixture.bytes).unwrap();
        println!("wrote {}", path.display());
    }
}

/// The committed fixture binaries must stay in lockstep with the in-memory generators; any drift
/// (an edited fixture or a stale regenerator) is caught here so the committed files never
/// silently diverge from what the tests actually exercise.
#[cfg(test)]
#[test]
fn committed_legacy_fixtures_match_generators() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/legacy");
    for fixture in all() {
        let path = root.join(format!("{}.{}", fixture.name, fixture.kind));
        let committed = std::fs::read(&path)
            .unwrap_or_else(|_| panic!("missing committed fixture {}", path.display()));
        assert_eq!(
            committed,
            fixture.bytes,
            "committed fixture {} drifted from the generator; run regenerate_committed_legacy_fixtures",
            path.display()
        );
    }
}
