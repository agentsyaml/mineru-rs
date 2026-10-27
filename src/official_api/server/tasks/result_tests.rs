use super::*;
use crate::official_api::wire::{JobLinks, JobProgress, OutputFormat, Tier};
use std::io::{Cursor, Write};

fn payload() -> JobResponse {
    JobResponse {
        job_id: "test".into(),
        status: "completed".into(),
        created_at: "now".into(),
        started_at: None,
        finished_at: None,
        tier: Tier::Basic,
        output_formats: vec![OutputFormat::Zip],
        access_level: "private".into(),
        progress: JobProgress {
            total: 0,
            completed: 0,
            failed: 0,
        },
        files: vec![],
        links: JobLinks {
            self_url: "".into(),
            cancel: "".into(),
        },
    }
}

fn source(name: &str, body: &[u8]) -> Bytes {
    let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
    writer
        .start_file(name, zip::write::SimpleFileOptions::default())
        .unwrap();
    writer.write_all(body).unwrap();
    Bytes::from(writer.finish().unwrap().into_inner())
}

fn manifest_size(job: &JobResponse) -> u64 {
    serde_json::to_vec(&task_payload(job)).unwrap().len() as u64
}

#[test]
fn combined_zip_rejects_aggregate_output_and_expansion_limits() {
    let job = payload();
    let inputs = [(0, source("a", b"x")), (1, source("b", b"y"))];
    let expanded = manifest_size(&job) + 2;
    assert!(
        inputs
            .iter()
            .all(|(_, bytes)| bytes.len() < expanded as usize)
    );
    assert_eq!(
        combine_zips(&job, &inputs, expanded, 10).unwrap_err().code,
        "invalid_response"
    );
    assert_eq!(
        combine_zips(&job, &inputs, expanded - 1, 10)
            .unwrap_err()
            .code,
        "invalid_response"
    );
    assert_eq!(
        combine_zips(&job, &inputs, expanded + 1024, 2)
            .unwrap_err()
            .code,
        "invalid_response"
    );
}

#[test]
fn combined_zip_contains_manifest_and_indexed_paths() {
    let job = payload();
    let inputs = [(0, source("a.txt", b"one")), (2, source("b.txt", b"two"))];
    let bytes = combine_zips(&job, &inputs, 1 << 20, 10).unwrap();
    let mut archive = zip::ZipArchive::new(Cursor::new(bytes)).unwrap();
    assert!(archive.by_name("manifest.json").is_ok());
    assert_eq!(
        archive.by_name("0001/a.txt").unwrap().name().unwrap(),
        "0001/a.txt"
    );
    assert_eq!(
        archive.by_name("0003/b.txt").unwrap().name().unwrap(),
        "0003/b.txt"
    );
}

#[test]
fn bounded_cursor_rejects_finalization_writes_after_seek() {
    let mut writer = zip::ZipWriter::new(BoundedCursor::new(64));
    writer
        .start_file("x", zip::write::SimpleFileOptions::default())
        .unwrap();
    writer.write_all(b"x").unwrap();
    assert!(writer.finish().is_err());
}
