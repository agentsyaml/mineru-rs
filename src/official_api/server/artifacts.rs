use crate::official::{OfficialError, OfficialErrorKind, OfficialResult};
use bytes::Bytes;
use std::{
    collections::BTreeMap,
    io::{self, Seek, SeekFrom, Write},
};

const JOB_CAP: usize = 64 * 1024 * 1024;
pub(super) fn render(
    result: &OfficialResult,
    format: crate::official_api::wire::OutputFormat,
    cap: usize,
) -> Result<Vec<u8>, OfficialError> {
    use crate::official_api::wire::OutputFormat::*;
    match format {
        Markdown => bounded(result.markdown().as_bytes(), cap),
        MiddleJson => json(result.middle_json(), cap),
        StructuredContent => json(result.structured_content(), cap),
        Zip => archive(result, cap.min(JOB_CAP)),
    }
}
fn json(value: &serde_json::Value, cap: usize) -> Result<Vec<u8>, OfficialError> {
    let mut out = Capped::new(cap);
    serde_json::to_writer(&mut out, value).map_err(|_| limit_error())?;
    Ok(out.data)
}
fn bounded(data: &[u8], cap: usize) -> Result<Vec<u8>, OfficialError> {
    if data.len() > cap {
        return Err(limit_error());
    }
    Ok(data.to_vec())
}
struct Bundle {
    files: BTreeMap<String, Bytes>,
    total: usize,
    cap: usize,
}
impl Bundle {
    fn insert(&mut self, path: &str, bytes: &[u8]) -> Result<(), OfficialError> {
        if !self.files.contains_key(path) && self.files.len() >= 4096 {
            return Err(limit_error());
        }
        if bytes.len() > self.cap.saturating_sub(self.total) {
            return Err(limit_error());
        }
        self.insert_owned(path, Bytes::copy_from_slice(bytes))
    }
    fn insert_json(&mut self, path: &str, value: &serde_json::Value) -> Result<(), OfficialError> {
        let mut out = Capped::new(self.cap.saturating_sub(self.total));
        serde_json::to_writer(&mut out, value).map_err(|_| limit_error())?;
        self.insert_owned(path, Bytes::from(out.data))
    }
    fn insert_owned(&mut self, path: &str, bytes: Bytes) -> Result<(), OfficialError> {
        if !self.files.contains_key(path) && self.files.len() >= 4096 {
            return Err(limit_error());
        }
        self.total = self
            .total
            .checked_add(bytes.len())
            .filter(|v| *v <= self.cap)
            .ok_or_else(limit_error)?;
        self.files.insert(path.into(), bytes);
        Ok(())
    }
}
fn archive(result: &OfficialResult, cap: usize) -> Result<Vec<u8>, OfficialError> {
    let mut bundle = Bundle {
        files: BTreeMap::new(),
        total: 0,
        cap: JOB_CAP,
    };
    bundle.insert("markdown.md", result.markdown().as_bytes())?;
    bundle.insert_json("middle_json.json", result.middle_json())?;
    bundle.insert_json("structured_content.json", result.structured_content())?;
    if let Some(model) = result.model_json() {
        bundle.insert_json("model_output.json", model)?;
    }
    for (path, bytes) in result.assets() {
        bundle.insert_owned(path, bytes.clone())?;
    }
    let sink = Capped::new(cap);
    let mut zip = zip::ZipWriter::new(sink);
    for (name, bytes) in bundle.files {
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
