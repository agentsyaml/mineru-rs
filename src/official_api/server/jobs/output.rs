use super::*;

pub(crate) async fn publish_outputs(
    s: &ServiceInner,
    shared: &Arc<tokio::sync::Mutex<JobRecord>>,
    index: usize,
    result: &crate::official::OfficialResult,
    aggregate: &mut usize,
) -> Result<(), ()> {
    let (formats, metadata, budget, stem) = {
        let job = shared.lock().await;
        (
            job.response.output_formats.clone(),
            job._metadata.clone(),
            job._artifact_budget.clone(),
            safe_stem(&job.files[index].name),
        )
    };
    let mut rendered = Vec::new();
    let mut total = 0usize;
    for format in formats {
        let cap = (64 * 1024 * 1024usize)
            .saturating_sub(*aggregate + total)
            .min(s.store.zip_cap() as usize);
        let bytes = super::super::artifacts::render(result, format, cap).map_err(|_| ())?;
        total = total
            .checked_add(bytes.len())
            .filter(|n| n.saturating_add(*aggregate) <= 64 * 1024 * 1024)
            .ok_or(())?;
        rendered.push((format, bytes));
    }
    let now = OfficialFileStore::now();
    let expires = now + s.store.retention_secs() as i64;
    let mut artifacts = Vec::new();
    let mut refs = std::collections::BTreeMap::new();
    for (format, bytes) in rendered {
        let name = output_name(format);
        let file_id = s.store.next_id("file");
        let size = bytes.len() as u64;
        let object = FileObject {
            id: file_id.clone(),
            object: "file",
            bytes: size,
            created_at: now,
            expires_at: Some(expires),
            filename: format!("{stem}.{name}"),
            purpose: "parse".into(),
            sha256sum: Some(sha256(&bytes)),
        };
        artifacts.push((object, bytes));
        refs.insert(
            name,
            OutputReference {
                file_id: file_id.clone(),
                bytes: size,
            },
        );
    }
    #[cfg(test)]
    {
        let pause = s
            .publication_pause
            .lock()
            .ok()
            .and_then(|mut pause| pause.take());
        if let Some((ready, resume)) = pause {
            let _ = ready.send(());
            let _ = resume.await;
        }
    }
    let mut job = shared.lock().await;
    if job.cancel.load(Ordering::Acquire) || s.stopping.load(Ordering::Acquire) {
        return Err(());
    }
    s.store
        .publish_artifact_batch(artifacts, metadata, budget)
        .map_err(|_| ())?;
    *aggregate += total;
    job.response.files[index].output_files = Some(refs);
    job.response.files[index].parse = Some(serde_json::json!({"pages":result.pages().len()}));
    Ok(())
}
fn output_name(f: OutputFormat) -> String {
    match f {
        OutputFormat::Markdown => "markdown",
        OutputFormat::MiddleJson => "middle_json",
        OutputFormat::StructuredContent => "structured_content",
        OutputFormat::Zip => "zip",
    }
    .into()
}
fn safe_stem(name: &str) -> String {
    name.rsplit_once('.')
        .map_or(name, |v| v.0)
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .take(100)
        .collect()
}
fn sha256(data: &[u8]) -> String {
    use sha2::Digest;
    sha2::Sha256::digest(data)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}
