use super::zip_scan::{ScanError, ScanLimits, scan};
use quick_xml::{
    Reader, XmlVersion,
    encoding::DecodingReader,
    events::{BytesDecl, BytesStart, Event},
    name::ResolveResult,
    reader::NsReader,
};
use std::{
    collections::HashMap,
    fs::File,
    io::{Cursor, Read},
    path::Path,
};
use zip::{
    ZipArchive,
    read::{ArchiveOffset, Config},
};

const ARCHIVE_CAP: u64 = 512 * 1024 * 1024;
const TOTAL_CAP: u64 = 2 * 1024 * 1024 * 1024;
const XML_CAP: u64 = 8 * 1024 * 1024;
const RATIO_CAP: u64 = 500;
const RELS: &str = "_rels/.rels";
const TYPES: &str = "[Content_Types].xml";
const REL_NS: &[u8] = b"http://schemas.openxmlformats.org/package/2006/relationships";
const TYPE_NS: &[u8] = b"http://schemas.openxmlformats.org/package/2006/content-types";
const OFFICE: &str =
    "http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument";

pub(super) fn detect(path: &Path) -> Result<Option<&'static str>, String> {
    detect_with(path, Limits::default())
}

/// Detect an OOXML package from the exact bounded bytes that will be consumed.
pub(crate) fn detect_bytes(bytes: &[u8]) -> Result<Option<&'static str>, String> {
    detect_reader(Cursor::new(bytes), bytes.len() as u64, Limits::default())
}

#[derive(Clone, Copy)]
struct Limits {
    archive: u64,
    total: u64,
    xml: u64,
    ratio: u64,
    scan: ScanLimits,
}
impl Default for Limits {
    fn default() -> Self {
        Self {
            archive: ARCHIVE_CAP,
            total: TOTAL_CAP,
            xml: XML_CAP,
            ratio: RATIO_CAP,
            scan: ScanLimits::production(10_000),
        }
    }
}

fn detect_with(path: &Path, limits: Limits) -> Result<Option<&'static str>, String> {
    if limits.archive == 0 || limits.total == 0 || limits.xml == 0 || limits.ratio == 0 {
        return Err("OOXML limits are invalid".into());
    }
    let file = match File::open(path) {
        Ok(file) => file,
        Err(_) => return Ok(None),
    };
    let size = match file.metadata() {
        Ok(metadata) => metadata.len(),
        Err(_) => return Ok(None),
    };
    if size > limits.archive {
        return Err("OOXML archive exceeds size limit".into());
    }
    detect_reader(file, size, limits)
}

fn detect_reader<R: Read + std::io::Seek>(
    mut file: R,
    size: u64,
    limits: Limits,
) -> Result<Option<&'static str>, String> {
    if limits.archive == 0 || limits.total == 0 || limits.xml == 0 || limits.ratio == 0 {
        return Err("OOXML limits are invalid".into());
    }
    if size > limits.archive {
        return Err("OOXML archive exceeds size limit".into());
    }
    let scanned = match scan(&mut file, limits.scan) {
        Ok(v) => v,
        Err(ScanError::Fallback) => return Ok(None),
        Err(ScanError::Limit) => return Err("OOXML archive exceeds scan limit".into()),
    };
    let mut zip = match ZipArchive::with_config(
        Config {
            archive_offset: ArchiveOffset::Known(0),
        },
        file,
    ) {
        Ok(v) => v,
        Err(_) => return Ok(None),
    };
    if zip.offset() != 0
        || u64::try_from(zip.len()).ok() != Some(scanned.count)
        || zip.central_directory_start() != scanned.central_start
    {
        return Ok(None);
    }
    let mut total = 0u64;
    let mut rel = None;
    let mut types = None;
    for i in 0..zip.len() {
        let entry = match zip.by_index(i) {
            Ok(v) => v,
            Err(_) => return Ok(None),
        };
        let mode = entry.unix_mode().unwrap_or(0) & 0o170000;
        if mode != 0 && mode != 0o100000 && mode != 0o040000 {
            return Err("OOXML archive contains a symlink or special entry".into());
        }
        total = total
            .checked_add(entry.size())
            .filter(|v| *v <= limits.total)
            .ok_or("OOXML archive exceeds expanded size limit")?;
        match entry.name() {
            RELS if rel.replace(i).is_some() => return Ok(None),
            TYPES if types.replace(i).is_some() => return Ok(None),
            _ => {}
        }
    }
    let (rel, types) = match (rel, types) {
        (Some(a), Some(b)) => (a, b),
        _ => return Ok(None),
    };
    let relationships = match read_xml(&mut zip, rel, limits) {
        Ok(v) => v,
        Err(ReadXml::Fallback) => return Ok(None),
        Err(ReadXml::Limit) => return Err("OOXML XML exceeds limit".into()),
    };
    let overrides = match read_xml(&mut zip, types, limits) {
        Ok(v) => v,
        Err(ReadXml::Fallback) => return Ok(None),
        Err(ReadXml::Limit) => return Err("OOXML XML exceeds limit".into()),
    };
    let targets = parse_relationships(&relationships).ok();
    let overrides = parse_overrides(&overrides).ok();
    let (Some(targets), Some(overrides)) = (targets, overrides) else {
        return Ok(None);
    };
    Ok(targets.into_iter().find_map(|target| match overrides.get(&target)?.as_str() {
        "application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml" => Some("docx"),
        "application/vnd.openxmlformats-officedocument.presentationml.presentation.main+xml" => Some("pptx"),
        "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml" => Some("xlsx"),
        _ => None,
    }))
}

enum ReadXml {
    Fallback,
    Limit,
}
fn read_xml<R: Read + std::io::Seek>(
    zip: &mut ZipArchive<R>,
    index: usize,
    limits: Limits,
) -> Result<Vec<u8>, ReadXml> {
    let mut entry = zip.by_index(index).map_err(|_| ReadXml::Fallback)?;
    let declared = entry.size();
    let packed = entry.compressed_size();
    if declared > limits.xml {
        return Err(ReadXml::Limit);
    }
    if declared > 0
        && (packed == 0
            || packed
                .checked_mul(limits.ratio)
                .is_none_or(|limit| declared > limit))
    {
        return Err(ReadXml::Limit);
    }
    let mut out = Vec::with_capacity(usize::try_from(declared).map_err(|_| ReadXml::Limit)?);
    let mut chunk = [0u8; 8192];
    loop {
        let n = entry.read(&mut chunk).map_err(|_| ReadXml::Fallback)?;
        if n == 0 {
            break;
        };
        let read_len = u64::try_from(n).map_err(|_| ReadXml::Limit)?;
        if u64::try_from(out.len())
            .ok()
            .and_then(|written| written.checked_add(read_len))
            .filter(|n| *n <= limits.xml)
            .is_none()
        {
            return Err(ReadXml::Limit);
        }
        out.extend_from_slice(&chunk[..n]);
    }
    if u64::try_from(out.len()).ok() != Some(declared) {
        return Err(ReadXml::Fallback);
    }
    Ok(out)
}

fn parse_relationships(bytes: &[u8]) -> Result<Vec<String>, ()> {
    let mut reader = xml_reader(bytes)?;
    let utf8_decoder = NsReader::from_reader(Cursor::new(b"".as_slice())).decoder();
    let mut buf = Vec::new();
    let mut out = Vec::new();
    let mut events = 0usize;
    let mut version = XmlVersion::Implicit1_0;
    let mut depth = 0usize;
    let mut root_seen = false;
    let mut declaration_seen = false;
    let mut first_event = true;
    loop {
        buf.clear();
        let (ns, event) = reader.read_resolved_event_into(&mut buf).map_err(|_| ())?;
        if matches!(ns, ResolveResult::Unknown(_)) {
            return Err(());
        }
        let allowed_ns = matches!(ns, ResolveResult::Unbound)
            || matches!(ns, ResolveResult::Bound(n) if n.as_ref() == REL_NS);
        events += 1;
        if events > 100_000 {
            return Err(());
        }
        if let Event::Decl(decl) = &event {
            if !first_event || declaration_seen || root_seen {
                return Err(());
            }
            declaration_seen = true;
            version = decl.xml_version().map_err(|_| ())?;
            validate_declaration(decl, bytes)?;
        }
        first_event = false;
        if let Event::Start(e) | Event::Empty(e) = &event {
            validate_attributes(&reader, e)?;
        }
        let child = depth == 1;
        match &event {
            Event::DocType(_) => return Err(()),
            Event::CData(_) | Event::GeneralRef(_) if depth == 0 => return Err(()),
            Event::GeneralRef(reference) if !valid_general_ref(reference.as_ref(), version) => {
                return Err(());
            }
            Event::Start(_) => {
                if depth == 0 {
                    if root_seen {
                        return Err(());
                    }
                    root_seen = true;
                }
                depth += 1;
            }
            Event::Empty(_) => {
                if depth == 0 {
                    if root_seen {
                        return Err(());
                    }
                    root_seen = true;
                }
            }
            Event::End(_) => depth = depth.checked_sub(1).ok_or(())?,
            Event::Text(text) if depth == 0 && !is_ascii_whitespace_text(text) => {
                return Err(());
            }
            _ => {}
        }
        match event {
            Event::Start(e) | Event::Empty(e)
                if child && e.local_name().as_ref() == b"Relationship" && allowed_ns =>
            {
                let mut typ = None;
                let mut target = None;
                let mut external = false;
                for a in e.attributes() {
                    let a = a.map_err(|_| ())?;
                    if a.key.as_namespace_binding().is_some() {
                        continue;
                    }
                    let (attribute_ns, _) = reader.resolver().resolve_attribute(a.key);
                    if matches!(attribute_ns, ResolveResult::Unknown(_)) {
                        return Err(());
                    }
                    if !matches!(attribute_ns, ResolveResult::Unbound) {
                        continue;
                    }
                    match a.key.as_ref() {
                        b"Type" => {
                            typ = Some(
                                a.decoded_and_normalized_value(version, utf8_decoder)
                                    .map_err(|_| ())?
                                    .into_owned(),
                            )
                        }
                        b"Target" => {
                            target = Some(
                                a.decoded_and_normalized_value(version, utf8_decoder)
                                    .map_err(|_| ())?
                                    .into_owned(),
                            )
                        }
                        b"TargetMode" => {
                            external = a
                                .decoded_and_normalized_value(version, utf8_decoder)
                                .map_err(|_| ())?
                                .as_ref()
                                == "External"
                        }
                        _ => {}
                    }
                }
                if !external && typ.as_deref() == Some(OFFICE) {
                    if let Some(t) = target {
                        let t = normalize(t);
                        if !t.is_empty() {
                            out.push(t);
                        }
                    }
                }
            }
            Event::Eof => return (root_seen && depth == 0).then_some(out).ok_or(()),
            _ => {}
        }
    }
}
fn parse_overrides(bytes: &[u8]) -> Result<HashMap<String, String>, ()> {
    let mut reader = xml_reader(bytes)?;
    let utf8_decoder = NsReader::from_reader(Cursor::new(b"".as_slice())).decoder();
    let mut buf = Vec::new();
    let mut out = HashMap::new();
    let mut events = 0usize;
    let mut version = XmlVersion::Implicit1_0;
    let mut depth = 0usize;
    let mut root_seen = false;
    let mut declaration_seen = false;
    let mut first_event = true;
    loop {
        buf.clear();
        let (ns, event) = reader.read_resolved_event_into(&mut buf).map_err(|_| ())?;
        if matches!(ns, ResolveResult::Unknown(_)) {
            return Err(());
        }
        let allowed_ns = matches!(ns, ResolveResult::Unbound)
            || matches!(ns, ResolveResult::Bound(n) if n.as_ref() == TYPE_NS);
        events += 1;
        if events > 100_000 {
            return Err(());
        }
        if let Event::Decl(decl) = &event {
            if !first_event || declaration_seen || root_seen {
                return Err(());
            }
            declaration_seen = true;
            version = decl.xml_version().map_err(|_| ())?;
            validate_declaration(decl, bytes)?;
        }
        first_event = false;
        if let Event::Start(e) | Event::Empty(e) = &event {
            validate_attributes(&reader, e)?;
        }
        let child = depth == 1;
        match &event {
            Event::DocType(_) => return Err(()),
            Event::CData(_) | Event::GeneralRef(_) if depth == 0 => return Err(()),
            Event::GeneralRef(reference) if !valid_general_ref(reference.as_ref(), version) => {
                return Err(());
            }
            Event::Start(_) => {
                if depth == 0 {
                    if root_seen {
                        return Err(());
                    }
                    root_seen = true;
                }
                depth += 1;
            }
            Event::Empty(_) => {
                if depth == 0 {
                    if root_seen {
                        return Err(());
                    }
                    root_seen = true;
                }
            }
            Event::End(_) => depth = depth.checked_sub(1).ok_or(())?,
            Event::Text(text) if depth == 0 && !is_ascii_whitespace_text(text) => {
                return Err(());
            }
            _ => {}
        }
        match event {
            Event::Start(e) | Event::Empty(e)
                if child && e.local_name().as_ref() == b"Override" && allowed_ns =>
            {
                let mut part = None;
                let mut content = None;
                for a in e.attributes() {
                    let a = a.map_err(|_| ())?;
                    if a.key.as_namespace_binding().is_some() {
                        continue;
                    }
                    let (attribute_ns, _) = reader.resolver().resolve_attribute(a.key);
                    if matches!(attribute_ns, ResolveResult::Unknown(_)) {
                        return Err(());
                    }
                    if !matches!(attribute_ns, ResolveResult::Unbound) {
                        continue;
                    }
                    match a.key.as_ref() {
                        b"PartName" => {
                            part = Some(
                                a.decoded_and_normalized_value(version, utf8_decoder)
                                    .map_err(|_| ())?
                                    .into_owned(),
                            )
                        }
                        b"ContentType" => {
                            content = Some(
                                a.decoded_and_normalized_value(version, utf8_decoder)
                                    .map_err(|_| ())?
                                    .into_owned(),
                            )
                        }
                        _ => {}
                    }
                }
                if let (Some(part), Some(content)) = (part, content) {
                    let part = normalize(part);
                    if !part.is_empty() && !content.is_empty() {
                        out.insert(part, content);
                    }
                }
            }
            Event::Eof => return (root_seen && depth == 0).then_some(out).ok_or(()),
            _ => {}
        }
    }
}
fn xml_reader(bytes: &[u8]) -> Result<NsReader<DecodingReader<Cursor<&[u8]>>>, ()> {
    let mut decoding = DecodingReader::new(Cursor::new(bytes));
    if bytes.starts_with(b"<?xml") {
        let mut raw = Reader::from_reader(Cursor::new(bytes));
        let mut buf = Vec::new();
        let Event::Decl(decl) = raw.read_event_into(&mut buf).map_err(|_| ())? else {
            return Err(());
        };
        decl.xml_version().map_err(|_| ())?;
        validate_declaration(&decl, bytes)?;
        if let Some(encoding) = decl.encoding() {
            encoding.map_err(|_| ())?;
            let encoder = decl.encoder().ok_or(())?;
            if !encoder.is_ascii_compatible() {
                return Err(());
            }
            decoding.set_encoding(encoder);
        }
    }
    Ok(NsReader::from_reader(decoding))
}
fn validate_attributes(
    reader: &NsReader<DecodingReader<Cursor<&[u8]>>>,
    element: &quick_xml::events::BytesStart<'_>,
) -> Result<(), ()> {
    for attribute in element.attributes() {
        let attribute = attribute.map_err(|_| ())?;
        if attribute.key.as_namespace_binding().is_none()
            && matches!(
                reader.resolver().resolve_attribute(attribute.key).0,
                ResolveResult::Unknown(_)
            )
        {
            return Err(());
        }
    }
    Ok(())
}
fn valid_general_ref(reference: &[u8], version: XmlVersion) -> bool {
    if matches!(reference, b"lt" | b"gt" | b"amp" | b"apos" | b"quot") {
        return true;
    }
    let value = match reference.strip_prefix(b"#x") {
        Some(hex) if !hex.is_empty() && hex.iter().all(u8::is_ascii_hexdigit) => {
            u32::from_str_radix(std::str::from_utf8(hex).unwrap_or(""), 16)
        }
        None if reference.starts_with(b"#")
            && reference.len() > 1
            && reference[1..].iter().all(u8::is_ascii_digit) =>
        {
            std::str::from_utf8(&reference[1..]).unwrap_or("").parse()
        }
        _ => return false,
    };
    let Ok(value) = value else { return false };
    if matches!(version, XmlVersion::Explicit1_1) {
        matches!(value, 0x1..=0xd7ff | 0xe000..=0xfffd | 0x10000..=0x10ffff)
    } else {
        matches!(value, 0x9 | 0xa | 0xd | 0x20..=0xd7ff | 0xe000..=0xfffd | 0x10000..=0x10ffff)
    }
}
fn validate_declaration(decl: &BytesDecl<'_>, bytes: &[u8]) -> Result<(), ()> {
    let content = std::str::from_utf8(decl.as_ref()).map_err(|_| ())?;
    let start = BytesStart::from_content(content, 3);
    let mut attributes = start.attributes();
    let version = attributes.next().ok_or(())?.map_err(|_| ())?;
    if version.key.as_ref() != b"version" || !matches!(version.value.as_ref(), b"1.0" | b"1.1") {
        return Err(());
    }
    let mut encoding_seen = false;
    let mut standalone_seen = false;
    for attribute in attributes {
        let attribute = attribute.map_err(|_| ())?;
        match attribute.key.as_ref() {
            b"encoding" if !encoding_seen && !standalone_seen => {
                let value = attribute.value.as_ref();
                let Some((first, rest)) = value.split_first() else {
                    return Err(());
                };
                if !first.is_ascii_alphabetic()
                    || !rest.iter().all(|byte| {
                        byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-')
                    })
                {
                    return Err(());
                }
                encoding_seen = true;
            }
            b"standalone" if !standalone_seen => {
                if !matches!(attribute.value.as_ref(), b"yes" | b"no") {
                    return Err(());
                }
                standalone_seen = true;
            }
            _ => return Err(()),
        }
    }
    let Some(encoding) = decl.encoding() else {
        return Ok(());
    };
    let encoding = encoding.map_err(|_| ())?;
    let encoding = encoding.as_ref();
    if decl.encoder().is_none() {
        return Err(());
    }
    let eq = |name: &[u8]| encoding.eq_ignore_ascii_case(name);
    let utf8 = eq(b"UTF-8") || eq(b"UTF8");
    let utf16 = eq(b"UTF-16");
    let le = eq(b"UTF-16LE");
    let be = eq(b"UTF-16BE");
    let prefix = &bytes[..bytes.len().min(4)];
    let detected_le = bytes.starts_with(b"\xff\xfe") || prefix.starts_with(b"<\0?\0");
    let detected_be = bytes.starts_with(b"\xfe\xff") || prefix.starts_with(b"\0<\0?");
    if bytes.starts_with(b"\xef\xbb\xbf") && !utf8 {
        return Err(());
    }
    if (detected_le && !(utf16 || le)) || (detected_be && !(utf16 || be)) {
        return Err(());
    }
    if !detected_le && !detected_be && (utf16 || le || be) {
        return Err(());
    }
    Ok(())
}
fn is_ascii_whitespace_text(text: &quick_xml::events::BytesText<'_>) -> bool {
    let text: &[u8] = text.as_ref();
    text.iter()
        .all(|byte| matches!(byte, b' ' | b'\t' | b'\r' | b'\n'))
}
fn normalize(value: String) -> String {
    value.replace('\\', "/").trim_start_matches('/').into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use zip::{CompressionMethod, ZipWriter, write::SimpleFileOptions};

    const DOCX: &str =
        "application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml";
    const PPTX: &str =
        "application/vnd.openxmlformats-officedocument.presentationml.presentation.main+xml";
    const XLSX: &str = "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml";

    fn bytes(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut central = Vec::new();
        for (name, data) in entries {
            let local = out.len() as u32;
            let crc = crc32(data);
            out.extend(b"PK\x03\x04");
            out.extend([20, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
            out.extend(crc.to_le_bytes());
            out.extend((data.len() as u32).to_le_bytes());
            out.extend((data.len() as u32).to_le_bytes());
            out.extend((name.len() as u16).to_le_bytes());
            out.extend(0u16.to_le_bytes());
            out.extend(name.as_bytes());
            out.extend(*data);
            central.extend(b"PK\x01\x02");
            central.extend([20, 0, 20, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
            central.extend(crc.to_le_bytes());
            central.extend((data.len() as u32).to_le_bytes());
            central.extend((data.len() as u32).to_le_bytes());
            central.extend((name.len() as u16).to_le_bytes());
            central.extend([0; 12]);
            central.extend(local.to_le_bytes());
            central.extend(name.as_bytes());
        }
        let start = out.len() as u32;
        let size = central.len() as u32;
        out.extend(central);
        out.extend(b"PK\x05\x06");
        out.extend([0; 4]);
        out.extend((entries.len() as u16).to_le_bytes());
        out.extend((entries.len() as u16).to_le_bytes());
        out.extend(size.to_le_bytes());
        out.extend(start.to_le_bytes());
        out.extend(0u16.to_le_bytes());
        out
    }
    fn crc32(bytes: &[u8]) -> u32 {
        bytes.iter().fold(!0u32, |crc, b| {
            (0..8).fold(crc ^ *b as u32, |crc, _| {
                (crc >> 1) ^ (0xedb8_8320 & 0u32.wrapping_sub(crc & 1))
            })
        }) ^ !0
    }
    fn detect_bytes(bytes: &[u8]) -> Option<&'static str> {
        detect_with_bytes(bytes, Limits::default()).unwrap()
    }
    fn detect_with_bytes(bytes: &[u8], limits: Limits) -> Result<Option<&'static str>, String> {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(bytes).unwrap();
        detect_with(file.path(), limits)
    }
    fn limits(bytes: &[u8]) -> Limits {
        Limits {
            archive: bytes.len() as u64,
            total: u64::MAX,
            xml: u64::MAX,
            ratio: RATIO_CAP,
            ..Limits::default()
        }
    }
    fn xml(kind: &str, bare: bool) -> (String, String) {
        let content = match kind {
            "docx" => DOCX,
            "pptx" => PPTX,
            _ => XLSX,
        };
        let (rr, r, end) = if bare {
            ("Relationships", "Relationship", "Relationships")
        } else {
            (
                "Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\"",
                "Relationship",
                "Relationships",
            )
        };
        let tt = if bare {
            "Types"
        } else {
            "Types xmlns=\"http://schemas.openxmlformats.org/package/2006/content-types\""
        };
        (
            format!("<{rr}><{r} Type=\"{OFFICE}\" Target=\"word/document.xml\"/></{end}>"),
            format!(
                "<{tt}><Override PartName=\"/word/document.xml\" ContentType=\"{content}\"/></Types>"
            ),
        )
    }
    fn package(rels: impl AsRef<[u8]>, types: impl AsRef<[u8]>) -> Vec<u8> {
        bytes(&[(RELS, rels.as_ref()), (TYPES, types.as_ref())])
    }
    fn deflated_package(rels: &[u8], types: &[u8]) -> Vec<u8> {
        let mut zip = ZipWriter::new(Cursor::new(Vec::new()));
        let options = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
        for (name, data) in [(RELS, rels), (TYPES, types)] {
            zip.start_file(name, options).unwrap();
            zip.write_all(data).unwrap();
        }
        zip.finish().unwrap().into_inner()
    }
    fn xml_ratio(bytes: &[u8]) -> u64 {
        let mut zip = ZipArchive::new(Cursor::new(bytes)).unwrap();
        [RELS, TYPES]
            .into_iter()
            .map(|name| {
                let entry = zip.by_name(name).unwrap();
                entry.size().div_ceil(entry.compressed_size())
            })
            .max()
            .unwrap()
    }
    fn assert_kind(kind: &str) {
        let (r, t) = xml(kind, false);
        assert_eq!(detect_bytes(&package(r, t)), Some(kind));
    }
    fn utf16(s: &str, be: bool) -> Vec<u8> {
        let mut out = if be {
            vec![0xfe, 0xff]
        } else {
            vec![0xff, 0xfe]
        };
        for c in s.encode_utf16() {
            out.extend(if be { c.to_be_bytes() } else { c.to_le_bytes() });
        }
        out
    }
    fn utf16_no_bom(s: &str, be: bool) -> Vec<u8> {
        s.encode_utf16()
            .flat_map(|c| if be { c.to_be_bytes() } else { c.to_le_bytes() })
            .collect()
    }
    fn windows_1252(s: &str) -> Vec<u8> {
        s.chars()
            .map(|c| if c == 'é' { 0xe9 } else { c as u8 })
            .collect()
    }
    fn central(bytes: &[u8], name: &str) -> usize {
        bytes
            .windows(4)
            .enumerate()
            .find_map(|(i, sig)| {
                (sig == b"PK\x01\x02" && bytes[i + 46..].starts_with(name.as_bytes())).then_some(i)
            })
            .unwrap()
    }
    fn patch16(bytes: &mut [u8], at: usize, value: u16) {
        bytes[at..at + 2].copy_from_slice(&value.to_le_bytes());
    }
    fn patch32(bytes: &mut [u8], at: usize, value: u32) {
        bytes[at..at + 4].copy_from_slice(&value.to_le_bytes());
    }
    fn patch_central32(bytes: &mut [u8], name: &str, at: usize, value: u32) {
        patch32(bytes, central(bytes, name) + at, value);
    }
    fn patch_entry(bytes: &mut [u8], name: &str, central_at: usize, local_at: usize, value: u16) {
        let c = central(bytes, name);
        patch16(bytes, c + central_at, value);
        let local = u32::from_le_bytes(bytes[c + 42..c + 46].try_into().unwrap()) as usize;
        patch16(bytes, local + local_at, value);
    }
    fn corrupt_payload(bytes: &mut [u8], name: &str) {
        let c = central(bytes, name);
        let local = u32::from_le_bytes(bytes[c + 42..c + 46].try_into().unwrap()) as usize;
        let data = local
            + 30
            + u16::from_le_bytes(bytes[local + 26..local + 28].try_into().unwrap()) as usize
            + u16::from_le_bytes(bytes[local + 28..local + 30].try_into().unwrap()) as usize;
        bytes[data] ^= 1;
    }

    #[test]
    fn detects_namespaced_docx() {
        assert_kind("docx");
    }
    #[test]
    fn detects_namespaced_pptx() {
        assert_kind("pptx");
    }
    #[test]
    fn detects_namespaced_xlsx() {
        assert_kind("xlsx");
    }
    #[test]
    fn accepts_bare_child_tags() {
        let (r, t) = xml("docx", true);
        assert_eq!(detect_bytes(&package(r, t)), Some("docx"));
    }
    #[test]
    fn first_mapped_relationship_wins() {
        let r = format!(
            "<Relationships><Relationship Type=\"{OFFICE}\" Target=\"missing.xml\"/><Relationship Type=\"{OFFICE}\" Target=\"word/document.xml\"/></Relationships>"
        );
        let t = format!(
            "<Types><Override PartName=\"word/document.xml\" ContentType=\"{DOCX}\"/></Types>"
        );
        assert_eq!(detect_bytes(&package(r, t)), Some("docx"));
    }
    #[test]
    fn last_duplicate_override_wins() {
        let r = format!(
            "<Relationships><Relationship Type=\"{OFFICE}\" Target=\"word/document.xml\"/></Relationships>"
        );
        let t = format!(
            "<Types><Override PartName=\"word/document.xml\" ContentType=\"{DOCX}\"/><Override PartName=\"word/document.xml\" ContentType=\"{PPTX}\"/></Types>"
        );
        assert_eq!(detect_bytes(&package(r, t)), Some("pptx"));
    }
    #[test]
    fn normalizes_targets_and_part_names() {
        let r = format!(
            "<Relationships><Relationship Type=\"{OFFICE}\" Target=\"\\\\word\\document.xml\"/></Relationships>"
        );
        let t = format!(
            "<Types><Override PartName=\"///word/document.xml\" ContentType=\"{DOCX}\"/></Types>"
        );
        assert_eq!(detect_bytes(&package(r, t)), Some("docx"));
    }
    #[test]
    fn ignores_exact_external_relationship() {
        let r = format!(
            "<Relationships><Relationship Type=\"{OFFICE}\" TargetMode=\"External\" Target=\"bad\"/><Relationship Type=\"{OFFICE}\" Target=\"word/document.xml\"/></Relationships>"
        );
        let t = format!(
            "<Types><Override PartName=\"word/document.xml\" ContentType=\"{DOCX}\"/></Types>"
        );
        assert_eq!(detect_bytes(&package(r, t)), Some("docx"));
    }
    #[test]
    fn rejects_wrong_namespace_type_content_and_match() {
        let r = format!(
            "<Relationships xmlns=\"wrong\"><Relationship Type=\"{OFFICE}\" Target=\"word/document.xml\"/></Relationships>"
        );
        let t = format!(
            "<Types><Override PartName=\"word/document.xml\" ContentType=\"{DOCX}\"/></Types>"
        );
        assert_eq!(detect_bytes(&package(r, t)), None);
        let r = "<Relationships><Relationship Type=\"wrong\" Target=\"word/document.xml\"/></Relationships>";
        let t = format!(
            "<Types><Override PartName=\"word/document.xml\" ContentType=\"{DOCX}\"/></Types>"
        );
        assert_eq!(detect_bytes(&package(r, t)), None);
        let (r, _) = xml("docx", true);
        assert_eq!(
            detect_bytes(&package(
                r,
                "<Types><Override PartName=\"word/document.xml\" ContentType=\"wrong\"/></Types>"
            )),
            None
        );
    }
    #[test]
    fn detects_utf8_package() {
        let (r, t) = xml("xlsx", false);
        let r = format!("<?xml version=\"1.0\" encoding=\"UTF-8\"?>{r}");
        assert_eq!(detect_bytes(&package(r, t)), Some("xlsx"));
    }
    #[test]
    fn detects_utf16le_package() {
        let (r, t) = xml("xlsx", false);
        assert_eq!(
            detect_bytes(&package(
                utf16(
                    &format!("<?xml version=\"1.0\" encoding=\"UTF-16\"?>{r}"),
                    false
                ),
                utf16(
                    &format!("<?xml version=\"1.0\" encoding=\"UTF-16\"?>{t}"),
                    false
                )
            )),
            Some("xlsx")
        );
    }
    #[test]
    fn detects_utf16be_package() {
        let (r, t) = xml("xlsx", false);
        assert_eq!(
            detect_bytes(&package(
                utf16(
                    &format!("<?xml version=\"1.0\" encoding=\"UTF-16\"?>{r}"),
                    true
                ),
                utf16(
                    &format!("<?xml version=\"1.0\" encoding=\"UTF-16\"?>{t}"),
                    true
                )
            )),
            Some("xlsx")
        );
    }
    #[test]
    fn falls_back_for_open_failure() {
        assert_eq!(
            detect_with(Path::new("/definitely/not/an/ooxml"), Limits::default()).unwrap(),
            None
        );
    }
    #[test]
    fn falls_back_for_non_zip() {
        assert_eq!(detect_bytes(b"not a zip"), None);
    }
    #[test]
    fn falls_back_for_missing_relationships() {
        let (r, _) = xml("docx", true);
        assert_eq!(detect_bytes(&bytes(&[(RELS, r.as_bytes())])), None);
    }
    #[test]
    fn falls_back_for_missing_content_types() {
        let (_, t) = xml("docx", true);
        assert_eq!(detect_bytes(&bytes(&[(TYPES, t.as_bytes())])), None);
    }
    #[test]
    fn falls_back_for_duplicate_relationships() {
        let (r, t) = xml("docx", true);
        assert_eq!(
            detect_bytes(&bytes(&[
                (RELS, r.as_bytes()),
                (RELS, r.as_bytes()),
                (TYPES, t.as_bytes())
            ])),
            None
        );
    }
    #[test]
    fn falls_back_for_duplicate_content_types() {
        let (r, t) = xml("docx", true);
        assert_eq!(
            detect_bytes(&bytes(&[
                (RELS, r.as_bytes()),
                (TYPES, t.as_bytes()),
                (TYPES, t.as_bytes())
            ])),
            None
        );
    }
    #[test]
    fn falls_back_for_malformed_relationships() {
        assert_eq!(detect_bytes(&package(b"<Relationships", b"<Types/>")), None);
    }
    #[test]
    fn falls_back_for_malformed_content_types() {
        assert_eq!(detect_bytes(&package(b"<Relationships/>", b"<Types")), None);
    }
    #[test]
    fn falls_back_for_relationship_payload_corruption() {
        let (r, t) = xml("docx", true);
        let mut b = package(r, t);
        corrupt_payload(&mut b, RELS);
        assert_eq!(detect_bytes(&b), None);
    }
    #[test]
    fn falls_back_for_content_types_payload_corruption() {
        let (r, t) = xml("docx", true);
        let mut b = package(r, t);
        corrupt_payload(&mut b, TYPES);
        assert_eq!(detect_bytes(&b), None);
    }
    #[test]
    fn scanner_falls_back_for_unsupported_compression() {
        let (r, t) = xml("docx", true);
        let mut b = package(&r, &t);
        patch_entry(&mut b, RELS, 10, 8, 9);
        assert_eq!(detect_bytes(&b), None);
    }
    #[test]
    fn scanner_falls_back_for_encryption() {
        let (r, t) = xml("docx", true);
        let mut b = package(&r, &t);
        patch_entry(&mut b, RELS, 8, 6, 1);
        assert_eq!(detect_bytes(&b), None);
    }
    #[test]
    fn scanner_falls_back_for_data_descriptor() {
        let (r, t) = xml("docx", true);
        let mut b = package(&r, &t);
        patch_entry(&mut b, RELS, 8, 6, 8);
        assert_eq!(detect_bytes(&b), None);
    }
    #[test]
    fn no_matching_relationship_is_none() {
        assert_eq!(
            detect_bytes(&package(b"<Relationships/>", b"<Types/>")),
            None
        );
    }
    #[test]
    fn no_matching_override_is_none() {
        let r = format!(
            "<Relationships><Relationship Type=\"{OFFICE}\" Target=\"missing.xml\"/></Relationships>"
        );
        let t = format!(
            "<Types><Override PartName=\"word/document.xml\" ContentType=\"{DOCX}\"/></Types>"
        );
        assert_eq!(detect_bytes(&package(r, t)), None);
    }
    #[test]
    fn unrelated_valid_zip_is_none() {
        assert_eq!(detect_bytes(&bytes(&[("hello.txt", b"hello")])), None);
    }

    #[test]
    fn archive_limit_boundary_is_hard_error() {
        let (r, t) = xml("docx", true);
        let b = package(r, t);
        assert_eq!(detect_with_bytes(&b, limits(&b)), Ok(Some("docx")));
        let mut l = limits(&b);
        l.archive -= 1;
        assert!(detect_with_bytes(&b, l).is_err());
    }
    #[test]
    fn zero_limits_are_hard_errors() {
        let (r, t) = xml("docx", true);
        let b = package(r, t);
        for limit in [
            Limits {
                archive: 0,
                ..limits(&b)
            },
            Limits {
                total: 0,
                ..limits(&b)
            },
            Limits {
                xml: 0,
                ..limits(&b)
            },
            Limits {
                ratio: 0,
                ..limits(&b)
            },
            Limits {
                scan: ScanLimits::production(0),
                ..limits(&b)
            },
        ] {
            assert!(detect_with_bytes(&b, limit).is_err());
        }
    }
    #[test]
    fn scan_limits_map_to_hard_errors() {
        let (r, t) = xml("docx", true);
        let b = package(r, t);
        let mut exact = limits(&b);
        exact.scan.max_entries = 2;
        assert_eq!(detect_with_bytes(&b, exact), Ok(Some("docx")));
        exact.scan.max_entries = 1;
        assert!(detect_with_bytes(&b, exact).is_err());
        let mut named = limits(&b);
        named.scan.name_cap = RELS.len() - 1;
        assert!(detect_with_bytes(&b, named).is_err());
    }
    #[test]
    fn total_limit_counts_every_declared_entry() {
        let (r, t) = xml("docx", true);
        let b = bytes(&[
            (RELS, r.as_bytes()),
            (TYPES, t.as_bytes()),
            ("junk", b"payload"),
        ]);
        let total = (r.len() + t.len() + 7) as u64;
        let mut l = limits(&b);
        l.total = total;
        assert_eq!(detect_with_bytes(&b, l), Ok(Some("docx")));
        l.total -= 1;
        assert!(detect_with_bytes(&b, l).is_err());
    }
    #[test]
    fn xml_declared_and_actual_limits_are_hard_errors() {
        let (r, t) = xml("docx", true);
        let b = package(&r, &t);
        let mut l = limits(&b);
        l.xml = r.len() as u64;
        assert_eq!(detect_with_bytes(&b, l), Ok(Some("docx")));
        l.xml -= 1;
        assert!(detect_with_bytes(&b, l).is_err());

        let b = package(b"<Relationships/>", &t);
        let mut l = limits(&b);
        l.xml = t.len() as u64;
        assert_eq!(detect_with_bytes(&b, l), Ok(None));
        l.xml -= 1;
        assert!(detect_with_bytes(&b, l).is_err());

        let mut forged = b.clone();
        patch_central32(&mut forged, RELS, 24, 1);
        let mut l = limits(&forged);
        l.xml = 1;
        assert!(detect_with_bytes(&forged, l).is_err());
    }
    #[test]
    fn xml_ratio_limits_are_checked_and_hard_errors() {
        let (r, t) = xml("docx", true);
        let b = deflated_package(r.as_bytes(), t.as_bytes());
        let ratio = xml_ratio(&b);
        let mut l = limits(&b);
        l.ratio = ratio;
        assert_eq!(detect_with_bytes(&b, l), Ok(Some("docx")));
        l.ratio -= 1;
        assert!(detect_with_bytes(&b, l).is_err());

        let mut zero_packed = b;
        patch_central32(&mut zero_packed, RELS, 20, 0);
        assert!(detect_with_bytes(&zero_packed, limits(&zero_packed)).is_err());
    }
    #[test]
    fn symlink_and_special_modes_are_hard_errors() {
        let (r, t) = xml("docx", true);
        for mode in [0o120000, 0o140000] {
            let mut b = package(&r, &t);
            let c = central(&b, RELS);
            b[c + 5] = 3;
            patch32(&mut b, c + 38, mode << 16);
            assert!(detect_with_bytes(&b, limits(&b)).is_err());
        }
    }
    #[test]
    fn unrelated_corruption_is_not_read_but_is_counted() {
        let (r, t) = xml("docx", true);
        let mut b = bytes(&[
            (RELS, r.as_bytes()),
            (TYPES, t.as_bytes()),
            ("junk", b"payload"),
        ]);
        corrupt_payload(&mut b, "junk");
        assert_eq!(detect_with_bytes(&b, limits(&b)), Ok(Some("docx")));
        let mut l = limits(&b);
        l.total = (r.len() + t.len() + 6) as u64;
        assert!(detect_with_bytes(&b, l).is_err());
    }
    #[test]
    fn production_scan_entry_boundary_is_hard_error() {
        let (r, t) = xml("docx", true);
        let mut entries = vec![(RELS, r.as_bytes()), (TYPES, t.as_bytes())];
        let names: Vec<String> = (0..9_998).map(|i| format!("x{i}")).collect();
        entries.extend(names.iter().map(|name| (name.as_str(), b"".as_slice())));
        let b = bytes(&entries);
        assert_eq!(detect_with_bytes(&b, limits(&b)), Ok(Some("docx")));
        let mut entries = entries;
        entries.push(("one-more", b""));
        let b = bytes(&entries);
        assert!(detect_with_bytes(&b, limits(&b)).is_err());
    }
    #[test]
    fn detects_utf16_without_bom() {
        let (r, t) = xml("docx", false);
        for be in [false, true] {
            let r = utf16_no_bom(
                &format!("<?xml version=\"1.0\" encoding=\"UTF-16\"?>{r}"),
                be,
            );
            let t = utf16_no_bom(
                &format!("<?xml version=\"1.0\" encoding=\"UTF-16\"?>{t}"),
                be,
            );
            assert_eq!(detect_bytes(&package(r, t)), Some("docx"));
        }
    }
    #[test]
    fn detects_windows_1252_after_declaration() {
        let r = format!(
            "<?xml version=\"1.0\" encoding=\"windows-1252\"?><Relationships><Relationship Type=\"{OFFICE}\" Target=\"word/café.xml\"/></Relationships>"
        );
        let t = format!(
            "<?xml version=\"1.0\" encoding=\"windows-1252\"?><Types><Override PartName=\"word/café.xml\" ContentType=\"{DOCX}\"/></Types>"
        );
        assert_eq!(
            detect_bytes(&package(windows_1252(&r), windows_1252(&t))),
            Some("docx")
        );
    }
    #[test]
    fn invalid_or_conflicting_declarations_fall_back() {
        let (r, t) = xml("docx", true);
        for bad in [
            b"<?xml version=\"wat\"?><Relationships/>".as_slice(),
            b"<?xml version=\"1.0\" encoding=\"unknown\"?><Relationships/>".as_slice(),
            b"\xef\xbb\xbf<?xml version=\"1.0\" encoding=\"windows-1252\"?><Relationships/>"
                .as_slice(),
            &utf16(
                "<?xml version=\"1.0\" encoding=\"UTF-8\"?><Relationships/>",
                false,
            ),
            &utf16_no_bom(
                "<?xml version=\"1.0\" encoding=\"UTF-16BE\"?><Relationships/>",
                false,
            ),
        ] {
            assert_eq!(detect_bytes(&package(bad, t.as_bytes())), None);
        }
        assert_eq!(
            detect_bytes(&package(
                r,
                b"<?xml version=\"1.0\" encoding=\"unknown\"?><Types/>"
            )),
            None
        );
        for encoding in ["UTF-16", "UTF-16LE", "UTF-16BE"] {
            let r = format!("<?xml version=\"1.0\" encoding=\"{encoding}\"?><Relationships/>");
            assert_eq!(detect_bytes(&package(r, t.as_bytes())), None);
        }
    }
    #[test]
    fn declaration_grammar_is_enforced() {
        let (_, t) = xml("docx", true);
        for declaration in [
            "<?xml version=\"1.0\" unknown=\"x\"?>",
            "<?xml version=\"1.0\" standalone=\"maybe\"?>",
            "<?xml version=\"1.0\" version=\"1.0\"?>",
            "<?xml version=\"1.0\" encoding=\"UTF-8\" encoding=\"UTF-8\"?>",
            "<?xml version=\"1.0\" standalone=\"yes\" standalone=\"no\"?>",
            "<?xml encoding=\"UTF-8\" version=\"1.0\"?>",
            "<?xml version=\"1.0\" standalone=\"yes\" encoding=\"UTF-8\"?>",
        ] {
            assert_eq!(
                detect_bytes(&package(format!("{declaration}<Relationships/>"), &t)),
                None
            );
        }
        for declaration in [
            "<?xml version=\"1.0\" standalone=\"yes\"?>",
            "<?xml version=\"1.1\" standalone=\"no\"?>",
            "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>",
        ] {
            let (r, t) = xml("docx", true);
            assert_eq!(
                detect_bytes(&package(
                    format!("{declaration}{r}"),
                    format!("{declaration}{t}")
                )),
                Some("docx")
            );
        }
    }
    #[test]
    fn xml11_normalizes_literal_nel_in_attributes() {
        let r = format!(
            "<?xml version=\"1.1\"?><Relationships><Relationship Type=\"{OFFICE}\" Target=\"word\u{85}document.xml\"/></Relationships>"
        );
        let t = format!(
            "<?xml version=\"1.1\"?><Types><Override PartName=\"word document.xml\" ContentType=\"{DOCX}\"/></Types>"
        );
        assert_eq!(detect_bytes(&package(r, t)), Some("docx"));
    }
    #[test]
    fn late_and_repeated_declarations_fall_back() {
        let (_, t) = xml("docx", true);
        for r in [
            "<?xml version=\"1.0\"?><?xml version=\"1.0\"?><Relationships/>",
            "<Relationships/><?xml version=\"1.0\"?>",
        ] {
            assert_eq!(detect_bytes(&package(r, t.as_bytes())), None);
        }
    }
    #[test]
    fn detects_overlong_windows_1252_declaration() {
        let declaration = format!(
            "<?xml version=\"1.0\"{}encoding=\"windows-1252\"?>",
            " ".repeat(64)
        );
        let r = format!(
            "{declaration}<Relationships><Relationship Type=\"{OFFICE}\" Target=\"word/café.xml\"/></Relationships>"
        );
        let t = format!(
            "{declaration}<Types><Override PartName=\"word/café.xml\" ContentType=\"{DOCX}\"/></Types>"
        );
        assert_eq!(
            detect_bytes(&package(windows_1252(&r), windows_1252(&t))),
            Some("docx")
        );
    }
    #[test]
    fn declaration_must_be_first_emitted_event() {
        let (_, t) = xml("docx", true);
        for prefix in [" \t", "<!--before-->", "<?before value?>"] {
            let r = format!("{prefix}<?xml version=\"1.0\"?><Relationships/>");
            assert_eq!(detect_bytes(&package(r, t.as_bytes())), None);
        }
    }
    #[test]
    fn allows_misc_around_root_but_rejects_top_level_content() {
        let (r, t) = xml("docx", true);
        assert_eq!(
            detect_bytes(&package(
                format!(" \t<!--before--><?pi x?>{r}<?after x?><!--after-->\r\n"),
                format!(" \t<!--before--><?pi x?>{t}<?after x?><!--after-->\r\n")
            )),
            Some("docx")
        );
        for r in [
            "<![CDATA[x]]><Relationships/>",
            "&unresolved;<Relationships/>",
        ] {
            assert_eq!(detect_bytes(&package(r, b"<Types/>")), None);
        }
    }
    #[test]
    fn attribute_namespaces_are_resolved() {
        let (r, t) = xml("docx", true);
        assert_eq!(
            detect_bytes(&package(r.replace("/>", " bad:extra=\"x\"/>"), &t)),
            None
        );
        assert_eq!(
            detect_bytes(&package(&r, t.replace("/>", " bad:extra=\"x\"/>"))),
            None
        );
        assert_eq!(
            detect_bytes(&package(
                r.replace("<Relationships", "<Relationships xmlns:bad=\"urn:bad\"")
                    .replace("/>", " bad:extra=\"x\"/>"),
                t.replace("<Types", "<Types xmlns:bad=\"urn:bad\"")
                    .replace("/>", " bad:extra=\"x\"/>")
            )),
            Some("docx")
        );
    }
    #[test]
    fn attributes_are_validated_on_every_element() {
        let (r, t) = xml("docx", true);
        for (r, t) in [
            (
                r.replacen("<Relationships", "<Relationships bad:extra=\"x\"", 1),
                t.clone(),
            ),
            (r.clone(), t.replacen("<Types", "<Types bad:extra=\"x\"", 1)),
            (
                r.replacen(
                    "</Relationships>",
                    "<x bad:extra=\"x\"/></Relationships>",
                    1,
                ),
                t.clone(),
            ),
            (
                r.clone(),
                t.replacen("</Types>", "<x bad:extra=\"x\"/></Types>", 1),
            ),
        ] {
            assert_eq!(detect_bytes(&package(r, t)), None);
        }
        let r = r
            .replacen(
                "<Relationships",
                "<Relationships xmlns:bad=\"urn:bad\" bad:root=\"x\"",
                1,
            )
            .replace("/>", " bad:candidate=\"x\"/><x bad:child=\"x\"/>");
        let t = t
            .replacen("<Types", "<Types xmlns:bad=\"urn:bad\" bad:root=\"x\"", 1)
            .replace("/>", " bad:candidate=\"x\"/><x bad:child=\"x\"/>");
        assert_eq!(detect_bytes(&package(r, t)), Some("docx"));
    }
    #[test]
    fn only_xml_s_is_allowed_outside_root() {
        let (r, t) = xml("docx", true);
        assert_eq!(
            detect_bytes(&package(format!(" \t\r\n{r} \t\r\n"), &t)),
            Some("docx")
        );
        for byte in ['\u{b}', '\u{c}'] {
            assert_eq!(detect_bytes(&package(format!("{byte}{r}"), &t)), None);
            assert_eq!(detect_bytes(&package(format!("{r}{byte}"), &t)), None);
        }
    }
    #[test]
    fn general_references_are_limited_without_dtd() {
        let (r, t) = xml("docx", true);
        for r in [
            r.replacen("</Relationships>", "<x>&unknown;</x></Relationships>", 1),
            r.replacen("</Relationships>", "&unknown;</Relationships>", 1),
        ] {
            assert_eq!(detect_bytes(&package(r, &t)), None);
        }
        assert_eq!(
            detect_bytes(&package(
                r.replacen(
                    "</Relationships>",
                    "<x>&lt;&gt;&amp;&apos;&quot;&#65;&#x41;</x></Relationships>",
                    1
                ),
                t.replacen(
                    "</Types>",
                    "<x>&lt;&gt;&amp;&apos;&quot;&#65;&#x41;</x></Types>",
                    1
                )
            )),
            Some("docx")
        );
        for entity in ["&#;", "&#x;", "&#x110000;", "&#xD800;", "&#0;"] {
            let r = r.replacen(
                "</Relationships>",
                &format!("<x>{entity}</x></Relationships>"),
                1,
            );
            assert_eq!(detect_bytes(&package(r, &t)), None);
        }
        for version in ["1.0", "1.1"] {
            for entity in ["&#x110000;", "&#xD800;", "&#0;"] {
                let r = r.replacen(
                    "</Relationships>",
                    &format!("<x>{entity}</x></Relationships>"),
                    1,
                );
                let r = format!("<?xml version=\"{version}\"?>{r}");
                assert_eq!(detect_bytes(&package(r, &t)), None);
            }
        }
        let r10 = format!(
            "<?xml version=\"1.0\"?>{}",
            r.replacen("</Relationships>", "<x>&#1;</x></Relationships>", 1,)
        );
        let r11 = r10.replacen("version=\"1.0\"", "version=\"1.1\"", 1);
        assert_eq!(detect_bytes(&package(r10, &t)), None);
        assert_eq!(detect_bytes(&package(r11, &t)), Some("docx"));
        assert_eq!(
            detect_bytes(&package(
                &r,
                t.replacen("</Types>", "<x>&unknown;</x></Types>", 1)
            )),
            None
        );
    }
    #[test]
    fn only_immediate_children_are_matched_and_xml_must_be_single_root() {
        let (r, t) = xml("docx", true);
        assert_eq!(
            detect_bytes(&package(
                format!("<Relationship Type=\"{OFFICE}\" Target=\"word/document.xml\"/>"),
                &t
            )),
            None
        );
        assert_eq!(
            detect_bytes(&package(
                format!("<Relationships><x>{r}</x></Relationships>"),
                &t
            )),
            None
        );
        assert_eq!(
            detect_bytes(&package(
                &r,
                format!("<Override PartName=\"word/document.xml\" ContentType=\"{DOCX}\"/>")
            )),
            None
        );
        assert_eq!(
            detect_bytes(&package(&r, format!("<Types><x>{t}</x></Types>"))),
            None
        );
        assert_eq!(detect_bytes(&package(r, t)), Some("docx"));
        let (r, t) = xml("docx", true);
        assert_eq!(detect_bytes(&package(format!("{r}{r}"), &t)), None);
        assert_eq!(detect_bytes(&package(format!("{r}junk"), t)), None);
        assert_eq!(
            detect_bytes(&package(
                b"<!DOCTYPE Relationships><Relationships/>",
                b"<Types/>"
            )),
            None
        );
        assert_eq!(
            detect_bytes(&package(b"<Relationships/>", b"<!DOCTYPE Types><Types/>")),
            None
        );
    }
    #[test]
    fn duplicate_and_empty_paths_follow_detection_rules() {
        let r = format!(
            "<Relationships><Relationship Type=\"{OFFICE}\" Target=\"/\"/><Relationship Type=\"{OFFICE}\" Target=\"word/document.xml\"/></Relationships>"
        );
        let t = format!(
            "<Types><Override PartName=\"/\" ContentType=\"{DOCX}\"/><Override PartName=\"word/document.xml\" ContentType=\"{DOCX}\"/><Override PartName=\"word/document.xml\" ContentType=\"unknown\"/></Types>"
        );
        assert_eq!(detect_bytes(&package(r, t)), None);
        let (r, _) = xml("docx", true);
        let t = format!(
            "<Types><Override PartName=\"word/document.xml\" ContentType=\"{DOCX}\"/><Override PartName=\"word/document.xml\" ContentType=\"\"/></Types>"
        );
        assert_eq!(detect_bytes(&package(r, t)), Some("docx"));
    }
}
