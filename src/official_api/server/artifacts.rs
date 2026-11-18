//! Task output composition for the 4.2.3 formats.
//!
//! Mirrors upstream `build_task_output`: `default` materializes the three text
//! artifacts, `docx`/`html`/`latex` render documents, a single docx-or-html
//! format is returned directly, and every other combination is one result.zip.

use crate::official::{
    OfficialError, OfficialErrorKind, OfficialRenderFormat, OfficialRenderOptions,
    OfficialRendered, OfficialResult, OfficialRuntime,
};
use crate::official_api::wire::TaskOutputFormat;
use bytes::Bytes;
use std::io::{self, Seek, SeekFrom, Write};

const JOB_CAP: usize = 64 * 1024 * 1024;

pub(super) struct Artifact {
    /// Suffix of the single downloadable result file.
    pub suffix: &'static str,
    pub bytes: Vec<u8>,
}

pub(super) async fn render(
    runtime: &OfficialRuntime,
    result: &OfficialResult,
    formats: &[TaskOutputFormat],
    cap: usize,
) -> Result<Artifact, OfficialError> {
    use TaskOutputFormat::{Default, Docx, Html, Latex};
    let has = |format: TaskOutputFormat| formats.contains(&format);
    let mut entries: Vec<(&'static str, Vec<u8>)> = Vec::new();
    if has(Default) {
        entries.push(("markdown.md", bounded_str(result.markdown(), cap)?));
        entries.push(("middle_json.json", json(result.middle_json(), cap)?));
        entries.push((
            "structured_content.json",
            json(result.structured_content(), cap)?,
        ));
    }
    if has(Docx) {
        let bytes = rendered(runtime, result, OfficialRenderFormat::Docx, cap).await?;
        entries.push(("document.docx", bytes));
    }
    if has(Html) {
        let html = rendered_text(runtime, result, OfficialRenderFormat::Html, cap).await?;
        let html = embed_images(&html, result.assets())?;
        entries.push(("document.html", html.into_bytes()));
    }
    if has(Latex) {
        let latex = rendered_text(runtime, result, OfficialRenderFormat::Latex, cap).await?;
        entries.push(("document.tex", latex.into_bytes()));
    }
    if formats.len() == 1 {
        let suffix = match formats[0] {
            Docx => Some("docx"),
            Html => Some("html"),
            _ => None,
        };
        if let Some(suffix) = suffix {
            let (_, bytes) = entries.pop().ok_or_else(limit_error)?;
            return Ok(Artifact { suffix, bytes });
        }
    }
    let bytes = archive(
        entries,
        result.assets(),
        has(Default) || has(Latex),
        cap.min(JOB_CAP),
    )?;
    Ok(Artifact {
        suffix: "zip",
        bytes,
    })
}

async fn rendered(
    runtime: &OfficialRuntime,
    result: &OfficialResult,
    format: OfficialRenderFormat,
    cap: usize,
) -> Result<Vec<u8>, OfficialError> {
    match result
        .render(runtime, format, OfficialRenderOptions::new())
        .await?
    {
        OfficialRendered::Binary { bytes, .. } => bounded(&bytes, cap),
        OfficialRendered::Text { content, .. } => bounded_str(&content, cap),
        OfficialRendered::Json { value, .. } => json(&value, cap),
    }
}

async fn rendered_text(
    runtime: &OfficialRuntime,
    result: &OfficialResult,
    format: OfficialRenderFormat,
    cap: usize,
) -> Result<String, OfficialError> {
    match result
        .render(runtime, format, OfficialRenderOptions::new())
        .await?
    {
        OfficialRendered::Text { content, .. } => {
            if content.len() > cap {
                return Err(limit_error());
            }
            Ok(content)
        }
        _ => Err(OfficialError::new(
            OfficialErrorKind::Upstream,
            "worker returned no text render",
        )),
    }
}

fn bounded(data: &[u8], cap: usize) -> Result<Vec<u8>, OfficialError> {
    if data.len() > cap {
        return Err(limit_error());
    }
    Ok(data.to_vec())
}
fn bounded_str(data: &str, cap: usize) -> Result<Vec<u8>, OfficialError> {
    bounded(data.as_bytes(), cap)
}
fn json(value: &serde_json::Value, cap: usize) -> Result<Vec<u8>, OfficialError> {
    let mut out = Capped::new(cap);
    serde_json::to_writer(&mut out, value).map_err(|_| limit_error())?;
    Ok(out.data)
}

/// Inline sidecar images as data URIs so a standalone HTML file is one file.
///
/// shortcut: only `img src` attributes are rewritten; `<source>`/SVG `xlink:href`
/// references are left untouched. Upgrade when the worker emits those tags.
fn embed_images(
    html: &str,
    assets: &std::collections::BTreeMap<String, Bytes>,
) -> Result<String, OfficialError> {
    use base64::Engine;
    let mut out = String::with_capacity(html.len());
    let mut rest = html;
    while let Some(pos) = find_ascii_case_insensitive(rest, "<img") {
        out.push_str(&rest[..pos]);
        let end = rest[pos..]
            .find('>')
            .map(|i| pos + i)
            .ok_or_else(limit_error)?;
        let tag = &rest[pos..=end];
        let mut replaced = tag.to_owned();
        if let Some(src) = attribute(tag, "src") {
            let external = src.starts_with("data:")
                || src.starts_with("http://")
                || src.starts_with("https://")
                || src.starts_with("//");
            if !external {
                let key = src.strip_prefix("./").unwrap_or(&src);
                let data = assets
                    .get(key)
                    .or_else(|| assets.get(&decode_percent(key)))
                    .ok_or_else(|| {
                        OfficialError::new(
                            OfficialErrorKind::InvalidInput,
                            "missing materialized HTML image",
                        )
                    })?;
                let uri = format!(
                    "data:{};base64,{}",
                    image_mime(key),
                    base64::engine::general_purpose::STANDARD.encode(data.as_ref())
                );
                replaced = replaced.replace(&format!("src=\"{src}\""), &format!("src=\"{uri}\""));
            }
        }
        out.push_str(&replaced);
        rest = &rest[end + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

fn attribute<'a>(tag: &'a str, name: &str) -> Option<String> {
    let needle = format!("{name}=\"");
    let start = tag.find(&needle)? + needle.len();
    let end = tag[start..].find('"')? + start;
    Some(tag[start..end].to_owned())
}

fn find_ascii_case_insensitive(haystack: &str, needle: &str) -> Option<usize> {
    haystack
        .as_bytes()
        .windows(needle.len())
        .position(|window| window.eq_ignore_ascii_case(needle.as_bytes()))
}

fn decode_percent(value: &str) -> String {
    let mut out = Vec::with_capacity(value.len());
    let bytes = value.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%'
            && index + 2 < bytes.len()
            && let Ok(byte) = u8::from_str_radix(&value[index + 1..index + 3], 16)
        {
            out.push(byte);
            index += 3;
            continue;
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn image_mime(path: &str) -> &'static str {
    match path
        .rsplit('.')
        .next()
        .unwrap_or("")
        .to_ascii_lowercase()
        .as_str()
    {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "bmp" => "image/bmp",
        "webp" => "image/webp",
        "tif" | "tiff" => "image/tiff",
        "svg" => "image/svg+xml",
        _ => "application/octet-stream",
    }
}

fn archive(
    entries: Vec<(&'static str, Vec<u8>)>,
    assets: &std::collections::BTreeMap<String, Bytes>,
    include_images: bool,
    cap: usize,
) -> Result<Vec<u8>, OfficialError> {
    let mut bundle = Vec::with_capacity(entries.len() + assets.len() + 1);
    let mut total = 0usize;
    for (name, bytes) in entries {
        total = total
            .checked_add(bytes.len())
            .filter(|v| *v <= cap)
            .ok_or_else(limit_error)?;
        bundle.push((name.to_owned(), Bytes::from(bytes)));
    }
    if include_images {
        bundle.push(("images/".into(), Bytes::new()));
        for (path, bytes) in assets {
            total = total
                .checked_add(bytes.len())
                .filter(|v| *v <= cap)
                .ok_or_else(limit_error)?;
            bundle.push((path.clone(), bytes.clone()));
        }
    }
    if bundle.len() > 4096 {
        return Err(limit_error());
    }
    let sink = Capped::new(cap);
    let mut zip = zip::ZipWriter::new(sink);
    for (name, bytes) in bundle {
        zip.start_file(name, zip::write::SimpleFileOptions::default())
            .map_err(|_| limit_error())?;
        zip.write_all(&bytes).map_err(|_| limit_error())?;
    }
    Ok(zip.finish().map_err(|_| limit_error())?.data)
}

struct Capped {
    data: Vec<u8>,
    pos: usize,
    cap: usize,
}
impl Capped {
    fn new(cap: usize) -> Self {
        Self {
            data: Vec::new(),
            pos: 0,
            cap,
        }
    }
}
impl Write for Capped {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let end = self
            .pos
            .checked_add(buf.len())
            .filter(|v| *v <= self.cap)
            .ok_or_else(|| io::Error::other("output cap"))?;
        if end > self.data.len() {
            self.data.resize(end, 0);
        }
        self.data[self.pos..end].copy_from_slice(buf);
        self.pos = end;
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
impl Seek for Capped {
    fn seek(&mut self, from: SeekFrom) -> io::Result<u64> {
        let pos = match from {
            SeekFrom::Start(n) => n as i128,
            SeekFrom::Current(n) => self.pos as i128 + n as i128,
            SeekFrom::End(n) => self.data.len() as i128 + n as i128,
        };
        if pos < 0 || pos as usize > self.cap {
            return Err(io::Error::other("output cap"));
        }
        self.pos = pos as usize;
        Ok(self.pos as u64)
    }
}

fn limit_error() -> OfficialError {
    OfficialError::new(
        OfficialErrorKind::LimitExceeded,
        "result exceeds artifact limit",
    )
}
