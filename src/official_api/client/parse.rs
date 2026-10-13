use super::*;

impl OfficialApiClient {
    pub async fn parse(
        &self,
        input: OfficialInput,
        options: &OfficialRemoteParseOptions,
    ) -> OfficialApiResult<OfficialParseResult> {
        let deadline = tokio::time::Instant::now() + self.timeout;
        let job = tokio::time::timeout_at(
            deadline,
            self.create_job(std::slice::from_ref(&input), options),
        )
        .await
        .map_err(|_| OfficialApiError::Transport("operation timed out".into()))??;
        let id = job
            .get("job_id")
            .and_then(Value::as_str)
            .ok_or_else(|| OfficialApiError::Protocol("job response omitted job_id".into()))?;
        let state = self.wait_job_until(id, deadline).await?;
        let file = state
            .pointer("/files/0")
            .ok_or_else(|| OfficialApiError::Protocol("job omitted file result".into()))?;
        if file.get("status").and_then(Value::as_str) != Some("completed") {
            return Err(remote_file_error(file));
        }
        let result = self.collect_result(state, deadline, OUTPUT_MAX).await;
        if matches!(&result, Err(OfficialApiError::Transport(message)) if message == "operation timed out")
        {
            let _ = tokio::time::timeout(Duration::from_secs(2), self.cancel_job(id)).await;
        }
        result
    }
    pub async fn parse_batch(
        &self,
        inputs: Vec<OfficialInput>,
        options: &OfficialRemoteParseOptions,
    ) -> Vec<OfficialApiResult<OfficialParseResult>> {
        let deadline = tokio::time::Instant::now() + self.timeout;
        let job = match tokio::time::timeout_at(deadline, self.create_job(&inputs, options)).await {
            Ok(Ok(value)) => value,
            Ok(Err(error)) => return (0..inputs.len()).map(|_| Err(error.clone())).collect(),
            Err(_) => {
                return (0..inputs.len())
                    .map(|_| Err(OfficialApiError::Transport("operation timed out".into())))
                    .collect();
            }
        };
        let Some(id) = job.get("job_id").and_then(Value::as_str) else {
            return (0..inputs.len())
                .map(|_| {
                    Err(OfficialApiError::Protocol(
                        "job response omitted job_id".into(),
                    ))
                })
                .collect();
        };
        let state = match self.wait_job_until(id, deadline).await {
            Ok(value) => value,
            Err(error) => return (0..inputs.len()).map(|_| Err(error.clone())).collect(),
        };
        let files = state
            .get("files")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let mut results = Vec::with_capacity(inputs.len());
        let mut retained = 0usize;
        for index in 0..inputs.len() {
            let Some(file) = files.get(index) else {
                results.push(Err(OfficialApiError::Protocol(
                    "job omitted file result".into(),
                )));
                continue;
            };
            if !file
                .get("status")
                .and_then(Value::as_str)
                .is_some_and(|s| s == "completed")
            {
                results.push(Err(remote_file_error(file)));
                continue;
            }
            let mut one = state.clone();
            one["files"] = json!([file]);
            let available = match remaining_budget(OUTPUT_MAX, retained) {
                Ok(available) => available,
                Err(error) => {
                    results.push(Err(error));
                    continue;
                }
            };
            match self.collect_result(one, deadline, available).await {
                Ok(result) => {
                    retained +=
                        serde_json::to_vec(&result.metadata).map_or(usize::MAX, |v| v.len());
                    retained = retained.saturating_add(
                        result
                            .artifacts
                            .iter()
                            .map(|a| {
                                serde_json::to_vec(&a.metadata).map_or(usize::MAX, |v| v.len())
                            })
                            .sum::<usize>(),
                    );
                    retained = retained.saturating_add(
                        result
                            .artifacts
                            .iter()
                            .map(|a| a.bytes.len())
                            .sum::<usize>(),
                    );
                    results.push(Ok(result));
                }
                Err(error) => {
                    if matches!(&error, OfficialApiError::Transport(message) if message == "operation timed out")
                    {
                        let _ =
                            tokio::time::timeout(Duration::from_secs(2), self.cancel_job(id)).await;
                    }
                    results.push(Err(error));
                }
            }
        }
        results
    }
    async fn collect_result(
        &self,
        state: Value,
        deadline: tokio::time::Instant,
        budget: usize,
    ) -> OfficialApiResult<OfficialParseResult> {
        let outputs = state
            .pointer("/files/0/output_files")
            .and_then(Value::as_object)
            .ok_or_else(|| OfficialApiError::Protocol("completed job omitted outputs".into()))?;
        let metadata = result_metadata(&state);
        let metadata_size = serde_json::to_vec(&metadata)
            .map_err(|_| OfficialApiError::TooLarge)?
            .len();
        let output_metadata_size = outputs.values().filter(|value| !value.is_null()).try_fold(
            0usize,
            |total, value| {
                total
                    .checked_add(
                        serde_json::to_vec(value)
                            .map_err(|_| OfficialApiError::TooLarge)?
                            .len(),
                    )
                    .ok_or(OfficialApiError::TooLarge)
            },
        )?;
        let budget = budget
            .checked_sub(
                metadata_size
                    .checked_add(output_metadata_size)
                    .ok_or(OfficialApiError::TooLarge)?,
            )
            .ok_or(OfficialApiError::TooLarge)?;
        let mut artifacts = Vec::new();
        let mut total = 0usize;
        for (kind, value) in outputs {
            if value.is_null() {
                continue;
            }
            if !["markdown", "middle_json", "structured_content", "zip"].contains(&kind.as_str()) {
                return Err(OfficialApiError::Protocol(
                    "unsupported artifact kind".into(),
                ));
            }
            if !value.is_object() {
                return Err(OfficialApiError::Protocol(
                    "malformed artifact reference".into(),
                ));
            }
            let file_id = value
                .get("file_id")
                .and_then(Value::as_str)
                .ok_or_else(|| OfficialApiError::Protocol("artifact omitted file_id".into()))?;
            if file_id.trim().is_empty() {
                return Err(OfficialApiError::Protocol(
                    "artifact has empty file_id".into(),
                ));
            }
            if value.get("bytes").and_then(Value::as_u64).is_none() {
                return Err(OfficialApiError::Protocol(
                    "artifact omitted byte count".into(),
                ));
            }
            let remaining = budget
                .checked_sub(total)
                .ok_or(OfficialApiError::TooLarge)?;
            if value
                .get("bytes")
                .and_then(Value::as_u64)
                .is_some_and(|bytes| bytes > remaining as u64)
            {
                return Err(OfficialApiError::TooLarge);
            }
            let bytes =
                tokio::time::timeout_at(deadline, self.download_file_limited(file_id, remaining))
                    .await
                    .map_err(|_| OfficialApiError::Transport("operation timed out".into()))??;
            total = total
                .checked_add(bytes.len())
                .ok_or(OfficialApiError::TooLarge)?;
            if kind == "zip" {
                validate_zip(&bytes)?;
            }
            artifacts.push(OfficialArtifact {
                kind: kind.clone(),
                file_id: file_id.into(),
                bytes,
                metadata: value.clone(),
            });
        }
        Ok(OfficialParseResult {
            metadata,
            artifacts,
        })
    }
    async fn wait_job_until(
        &self,
        id: &str,
        end: tokio::time::Instant,
    ) -> OfficialApiResult<Value> {
        loop {
            let state = match tokio::time::timeout_at(end, self.get_job(id)).await {
                Ok(result) => result?,
                Err(_) => {
                    let _ = tokio::time::timeout(Duration::from_secs(2), self.cancel_job(id)).await;
                    return Err(OfficialApiError::Transport("operation timed out".into()));
                }
            };
            match state.get("status").and_then(Value::as_str) {
                Some("completed" | "partial") => return Ok(state),
                Some("failed") => return Ok(state),
                Some("canceled") => {
                    return Err(OfficialApiError::Protocol(
                        state
                            .pointer("/error/message")
                            .and_then(Value::as_str)
                            .unwrap_or("job failed")
                            .into(),
                    ));
                }
                Some("queued" | "running") => {
                    tokio::time::sleep_until(
                        (tokio::time::Instant::now() + Duration::from_millis(250)).min(end),
                    )
                    .await
                }
                _ => return Err(OfficialApiError::Protocol("unknown job state".into())),
            }
            if tokio::time::Instant::now() >= end {
                let _ = tokio::time::timeout(Duration::from_secs(2), self.cancel_job(id)).await;
                return Err(OfficialApiError::Transport("operation timed out".into()));
            }
        }
    }
}
