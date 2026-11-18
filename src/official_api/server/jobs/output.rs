use super::*;
use crate::official_api::wire::{
    FileObjectModel, FileParseInfo, FilePurpose, TaskFileState, TaskStatus,
};

/// Compose the requested 4.2.3 formats and publish one downloadable file under
/// the task file identity the client already knows.
pub(crate) async fn publish_result(
    s: &ServiceInner,
    shared: &Arc<tokio::sync::Mutex<TaskRecord>>,
    index: usize,
    result: &crate::official::OfficialResult,
) -> Result<(), ()> {
    let (file_id, file_name, formats, tier, metadata, budget, expires) = {
        let task = shared.lock().await;
        let file = task.files.get(index).ok_or(())?;
        (
            file.file_id.clone(),
            file.file_name.clone(),
            task.options.output_formats.clone(),
            file.tier,
            task._metadata.clone(),
            task._artifact_budget.clone(),
            OfficialFileStore::now() + s.store.retention_secs() as i64,
        )
    };
    let artifact = super::super::artifacts::render(
        s.parser.runtime(),
        result,
        &formats,
        s.store.zip_cap() as usize,
    )
    .await
    .map_err(|_| ())?;
    let now = OfficialFileStore::now();
    let object = FileObjectModel {
        id: file_id.clone(),
        object: FileObjectModel::OBJECT,
        bytes: artifact.bytes.len() as u64,
        created_at: now,
        expires_at: Some(expires),
        file_name: format!("{}.{}", safe_stem(&file_name), artifact.suffix),
        purpose: FilePurpose::ParseOutput,
        sha256sum: Some(sha256(&artifact.bytes)),
    };
    {
        let mut task = shared.lock().await;
        let canceled = task.status == TaskStatus::Canceled || s.stopping.load(Ordering::Acquire);
        if canceled {
            return Err(());
        }
        s.store
            .publish_artifact_batch(vec![(object, artifact.bytes)], metadata, budget)
            .map_err(|_| ())?;
        let task = &mut *task;
        let file = task.files.get_mut(index).ok_or(())?;
        file.parse = Some(FileParseInfo {
            tier,
            duration_ms: elapsed_ms(file.started_at.as_deref()),
            parser_version: Some(crate::official_worker::PACKAGE_VERSION.into()),
        });
        file.status = TaskFileState::Completed;
        file.finished_at = Some(super::super::store::timestamp());
        file.data = None;
        s.store.aggregate(task);
    }
    Ok(())
}

fn elapsed_ms(started: Option<&str>) -> Option<u64> {
    let started =
        time::OffsetDateTime::parse(started?, &time::format_description::well_known::Rfc3339)
            .ok()?;
    let elapsed = time::OffsetDateTime::now_utc() - started;
    u64::try_from(elapsed.whole_milliseconds()).ok()
}

fn safe_stem(name: &str) -> String {
    let stem = name.rsplit_once('.').map_or(name, |v| v.0);
    let safe: String = stem
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .take(100)
        .collect();
    if safe.is_empty() {
        "result".into()
    } else {
        safe
    }
}

fn sha256(data: &[u8]) -> String {
    use sha2::Digest;
    sha2::Sha256::digest(data)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}
