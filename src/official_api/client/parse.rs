use super::*;

impl OfficialApiClient {
    pub async fn parse(
        &self,
        input: OfficialInput,
        options: &OfficialRemoteParseOptions,
    ) -> OfficialApiResult<OfficialParseResult> {
        let deadline = tokio::time::Instant::now() + self.timeout;
        let task = match tokio::time::timeout_at(
            deadline,
            self.create_task(std::slice::from_ref(&input), options),
        )
        .await
        {
            Ok(result) => result?,
            Err(_) => return Err(timed_out()),
        };
        let id = task.task_id.clone();
        let results = self.execute(task, vec![input], deadline).await;
        let result = results
            .into_iter()
            .next()
            .unwrap_or_else(|| Err(OfficialApiError::Protocol("task omitted result".into())));
        if matches!(&result, Err(OfficialApiError::Transport(_))) {
            let _ = tokio::time::timeout(Duration::from_secs(2), self.cancel_task(&id)).await;
        }
        result
    }

    pub async fn parse_batch(
        &self,
        inputs: Vec<OfficialInput>,
        options: &OfficialRemoteParseOptions,
    ) -> Vec<OfficialApiResult<OfficialParseResult>> {
        let deadline = tokio::time::Instant::now() + self.timeout;
        let task = match tokio::time::timeout_at(deadline, self.create_task(&inputs, options)).await
        {
            Ok(Ok(task)) => task,
            Ok(Err(error)) => return (0..inputs.len()).map(|_| Err(error.clone())).collect(),
            Err(_) => {
                return (0..inputs.len()).map(|_| Err(timed_out())).collect();
            }
        };
        self.execute(task, inputs, deadline).await
    }

    /// Upload every file, wait for the aggregate to settle, then download each
    /// completed file's `result_url` within one shared byte budget.
    async fn execute(
        &self,
        task: CreatedTask,
        inputs: Vec<OfficialInput>,
        deadline: tokio::time::Instant,
    ) -> Vec<OfficialApiResult<OfficialParseResult>> {
        let mut results: Vec<Option<OfficialApiResult<OfficialParseResult>>> =
            Vec::with_capacity(inputs.len());
        for (index, input) in inputs.iter().enumerate() {
            let Some(file) = task.files.get(index) else {
                results.push(Some(Err(OfficialApiError::Protocol(
                    "create response omitted task file".into(),
                ))));
                continue;
            };
            let Some(link) = file.upload.as_ref() else {
                results.push(Some(Err(OfficialApiError::Protocol(
                    "create response omitted upload link".into(),
                ))));
                continue;
            };
            match tokio::time::timeout_at(deadline, self.upload_file(link, &input.bytes)).await {
                Ok(Ok(())) => results.push(None),
                Ok(Err(error)) => results.push(Some(Err(error))),
                Err(_) => results.push(Some(Err(timed_out()))),
            }
        }
        let state = match tokio::time::timeout_at(deadline, self.wait_task_until(&task.task_id))
            .await
        {
            Ok(Ok(state)) => state,
            Ok(Err(error)) => {
                return results.iter().map(|_| Err(error.clone())).collect();
            }
            Err(_) => {
                let _ =
                    tokio::time::timeout(Duration::from_secs(2), self.cancel_task(&task.task_id))
                        .await;
                return results.iter().map(|_| Err(timed_out())).collect();
            }
        };
        let files = state
            .get("files")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let kind = artifact_kind(&state);
        let mut retained = 0usize;
        for (index, slot) in results.iter_mut().enumerate() {
            if slot.is_some() {
                continue;
            }
            let Some(file) = files.get(index) else {
                *slot = Some(Err(OfficialApiError::Protocol(
                    "task omitted file result".into(),
                )));
                continue;
            };
            if file.get("status").and_then(Value::as_str) != Some("completed") {
                *slot = Some(Err(remote_file_error(file)));
                continue;
            }
            let Some(raw) = file.get("result_url").and_then(Value::as_str) else {
                *slot = Some(Err(OfficialApiError::Protocol(
                    "completed file omitted result_url".into(),
                )));
                continue;
            };
            let available = match remaining_budget(OUTPUT_MAX, retained) {
                Ok(available) => available,
                Err(error) => {
                    *slot = Some(Err(error));
                    continue;
                }
            };
            let outcome =
                tokio::time::timeout_at(deadline, self.download_url(raw, available)).await;
            *slot = Some(match outcome {
                Ok(Ok(bytes)) => {
                    if kind == "zip"
                        && let Err(error) = validate_zip(&bytes)
                    {
                        Err(error)
                    } else {
                        retained = retained.saturating_add(bytes.len());
                        Ok(OfficialParseResult {
                            metadata: result_metadata(&state),
                            artifacts: vec![OfficialArtifact {
                                kind: kind.into(),
                                file_id: file
                                    .get("file_id")
                                    .and_then(Value::as_str)
                                    .unwrap_or_default()
                                    .into(),
                                bytes,
                                metadata: file.clone(),
                            }],
                        })
                    }
                }
                Ok(Err(error)) => Err(error),
                Err(_) => Err(timed_out()),
            });
        }
        results
            .into_iter()
            .map(|slot| {
                slot.unwrap_or_else(|| {
                    Err(OfficialApiError::Protocol(
                        "task omitted file result".into(),
                    ))
                })
            })
            .collect()
    }

    async fn wait_task_until(&self, id: &str) -> OfficialApiResult<Value> {
        loop {
            let state = self.get_task(id).await?;
            match state.get("status").and_then(Value::as_str) {
                Some("completed" | "partial" | "failed") => return Ok(state),
                Some("canceled") => {
                    return Err(OfficialApiError::Protocol(
                        state
                            .pointer("/error/message")
                            .and_then(Value::as_str)
                            .unwrap_or("task canceled")
                            .into(),
                    ));
                }
                Some("waiting-file" | "pending" | "running") => {
                    tokio::time::sleep(Duration::from_millis(250)).await;
                }
                _ => return Err(OfficialApiError::Protocol("unknown task state".into())),
            }
        }
    }
}

fn timed_out() -> OfficialApiError {
    OfficialApiError::Transport("operation timed out".into())
}

/// Mirror `build_task_output`: only a lone `docx` or `html` request yields a
/// direct document; every other selection arrives as `result.zip`. The task
/// detail's `file_name` is the *input* name, so it cannot identify the artifact.
fn artifact_kind(state: &Value) -> &'static str {
    match state.get("output_formats").and_then(Value::as_array) {
        Some(formats) if formats.len() == 1 => match formats[0].as_str() {
            Some("docx") => "docx",
            Some("html") => "html",
            _ => "zip",
        },
        _ => "zip",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn classifies_the_artifact_from_the_requested_formats() {
        assert_eq!(artifact_kind(&json!({"output_formats": ["docx"]})), "docx");
        assert_eq!(artifact_kind(&json!({"output_formats": ["html"]})), "html");
        assert_eq!(
            artifact_kind(&json!({"output_formats": ["default"]})),
            "zip"
        );
        assert_eq!(artifact_kind(&json!({"output_formats": ["latex"]})), "zip");
        assert_eq!(
            artifact_kind(&json!({"output_formats": ["default", "docx"]})),
            "zip"
        );
        assert_eq!(artifact_kind(&json!({})), "zip");
    }
}
