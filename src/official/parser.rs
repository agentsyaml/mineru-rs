use crate::{
    official::{
        OfficialBatchItem, OfficialBatchResult, OfficialError, OfficialErrorKind, OfficialInput,
        OfficialParseOptions, OfficialResult, OfficialRuntime, OfficialTier,
    },
    official_worker::{
        OfficialPersistentWorker, OfficialRequest, OfficialSessionConfig, OfficialWorker,
    },
};
use std::{
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Instant,
};

pub struct OfficialParser {
    runtime: Arc<OfficialRuntime>,
    persistent: Mutex<Option<Arc<OfficialPersistentWorker>>>,
}

struct CancelOnDrop {
    flag: Option<Arc<AtomicBool>>,
    abort: Option<tokio::task::AbortHandle>,
}

impl CancelOnDrop {
    fn new(flag: Arc<AtomicBool>) -> Self {
        Self {
            flag: Some(flag),
            abort: None,
        }
    }

    fn abort_on_drop<T>(&mut self, task: &tokio::task::JoinHandle<T>) {
        self.abort = Some(task.abort_handle());
    }

    fn disarm(&mut self) {
        self.flag = None;
        self.abort = None;
    }
}

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        if let Some(flag) = self.flag.take() {
            flag.store(true, Ordering::Relaxed);
        }
        if let Some(abort) = self.abort.take() {
            abort.abort();
        }
    }
}

impl OfficialParser {
    pub fn new(runtime: OfficialRuntime) -> Result<Self, OfficialError> {
        if runtime.timeout.is_zero() {
            return Err(OfficialError::new(
                OfficialErrorKind::InvalidOptions,
                "runtime timeout must be positive",
            ));
        }
        for path in [
            runtime.python.as_ref(),
            runtime.config.as_ref(),
            runtime.model_dir.as_ref(),
        ]
        .into_iter()
        .flatten()
        {
            if !path.is_absolute() {
                return Err(OfficialError::new(
                    OfficialErrorKind::InvalidOptions,
                    "official runtime paths must be absolute",
                ));
            }
        }
        Ok(Self {
            runtime: Arc::new(runtime),
            persistent: Mutex::new(None),
        })
    }

    pub async fn parse_rich(
        &self,
        input: OfficialInput,
        options: OfficialParseOptions,
    ) -> Result<OfficialResult, OfficialError> {
        self.parse_one(
            input,
            options,
            None,
            crate::official::runtime::RETAINED_RESULT_CAP,
        )
        .await
    }

    pub async fn parse_batch_rich(
        &self,
        items: Vec<OfficialBatchItem>,
    ) -> Result<Vec<OfficialBatchResult>, OfficialError> {
        if items.is_empty() {
            return Ok(Vec::new());
        }
        let mut aggregate = 0usize;
        let mut worker: Option<Arc<OfficialPersistentWorker>> = None;
        let mut results = Vec::with_capacity(items.len());
        for (index, item) in items.into_iter().enumerate() {
            let filename = item.input.filename().to_owned();
            let mut options = item.options;
            if options.tier.is_none() {
                options.tier = Some(if item.input.is_native() {
                    OfficialTier::Flash
                } else {
                    OfficialTier::Standard
                });
            }
            let remaining = crate::official::runtime::RETAINED_RESULT_CAP.saturating_sub(aggregate);
            let native = matches!(item.input.suffix().as_deref(), Ok("csv" | "tsv"));
            let worker_result = if native {
                Ok(None)
            } else if let Some(existing) = &worker {
                Ok(Some(existing.clone()))
            } else {
                self.persistent_worker().map(|started| {
                    worker = Some(started.clone());
                    Some(started)
                })
            };
            let result = if remaining == 0 {
                Err(OfficialError::new(
                    OfficialErrorKind::LimitExceeded,
                    "batch exceeds retained result byte limit",
                ))
            } else {
                match worker_result {
                    Err(error) => Err(error),
                    Ok(worker) => self
                        .parse_one(item.input, options, worker, remaining)
                        .await
                        .inspect(|result| {
                            aggregate = aggregate.saturating_add(result.retained_bytes())
                        }),
                }
            };
            results.push(OfficialBatchResult {
                index,
                filename,
                result,
            });
        }
        Ok(results)
    }

    pub async fn shutdown(&self) -> Result<(), OfficialError> {
        let worker = self
            .persistent
            .lock()
            .map_err(|_| {
                OfficialError::new(
                    OfficialErrorKind::RuntimeUnavailable,
                    "persistent worker state is poisoned",
                )
            })?
            .take();
        if let Some(worker) = worker {
            worker
                .shutdown()
                .await
                .map_err(|error| OfficialError::new(OfficialErrorKind::Transport, error))?;
        }
        Ok(())
    }

    async fn parse_one(
        &self,
        input: OfficialInput,
        options: OfficialParseOptions,
        persistent: Option<Arc<OfficialPersistentWorker>>,
        retained_cap: usize,
    ) -> Result<OfficialResult, OfficialError> {
        let suffix = input.suffix()?;
        options
            .validate(&suffix)
            .map_err(|error| OfficialError::new(OfficialErrorKind::InvalidOptions, error))?;
        if matches!(suffix.as_str(), "csv" | "tsv") {
            return self
                .parse_native_csv(input, suffix, &options, retained_cap)
                .await;
        }
        let deadline = Instant::now()
            .checked_add(self.runtime.timeout)
            .ok_or_else(|| {
                OfficialError::new(
                    OfficialErrorKind::InvalidOptions,
                    "runtime timeout overflows",
                )
            })?;
        let bytes = input
            .clone()
            .acquire_limited(crate::official::runtime::INPUT_BYTE_CAP, deadline)
            .await?;
        if bytes.is_empty() {
            return Err(OfficialError::new(
                OfficialErrorKind::InvalidInput,
                "input is empty",
            ));
        }
        let mut context = options.source_context.clone().unwrap_or_default();
        if options.source_context.is_none()
            && let Some((uri, root)) = input.path_context()
        {
            context.source_uri = Some(uri);
            context.local_resource_root = Some(root);
        }
        let mut request = OfficialRequest::new(
            options.tier.unwrap_or_default().as_str().into(),
            options.ocr_mode.unwrap_or_default().as_str().into(),
            options.image_analysis.unwrap_or(true),
            options.page_range.clone(),
            options.vlm_server_url.clone(),
            options.vlm_api_key.clone(),
            options.vlm_model.clone(),
            self.runtime.model_dir.clone(),
            self.runtime.config.clone(),
            crate::official::runtime::BUNDLE_BYTE_CAP,
            PathBuf::new(),
            PathBuf::new(),
        );
        request.source_context = Some(serde_json::to_value(context).map_err(|error| {
            OfficialError::new(OfficialErrorKind::InvalidOptions, error.to_string())
        })?);
        request.official_vlm_config = options
            .vlm_config
            .as_ref()
            .map(serde_json::to_value)
            .transpose()
            .map_err(|error| {
                OfficialError::new(OfficialErrorKind::InvalidOptions, error.to_string())
            })?;
        let bundle = if let Some(worker) = persistent {
            worker
                .run_owned(bytes.clone(), &suffix, request, deadline)
                .await
        } else if self.runtime.worker_mode == crate::command::OfficialWorkerMode::Persistent {
            self.persistent_worker()?
                .run_owned(bytes.clone(), &suffix, request, deadline)
                .await
        } else {
            OfficialWorker::new(self.runtime.python.clone())
                .map_err(|error| OfficialError::new(OfficialErrorKind::RuntimeUnavailable, error))?
                .run_owned(bytes, &suffix, request, deadline)
                .await
        }
        .map_err(crate::official::error::classify)?;
        OfficialResult::from_bundle(bundle.path(), retained_cap)
    }

    async fn parse_native_csv(
        &self,
        input: OfficialInput,
        suffix: String,
        options: &OfficialParseOptions,
        retained_cap: usize,
    ) -> Result<OfficialResult, OfficialError> {
        if options.vlm_config.is_some()
            || options.vlm_server_url.is_some()
            || options.vlm_api_key.is_some()
            || options.vlm_model.is_some()
            || options.source_context.as_ref().is_some_and(|context| {
                context.source_uri.is_some()
                    || context.local_resource_root.is_some()
                    || context.transport_encoding.is_some()
            })
        {
            return Err(OfficialError::new(
                OfficialErrorKind::UnsupportedOperation,
                "CSV parsing does not support VLM or source-context options",
            ));
        }
        let input_cap = crate::official::native::csv::MAX_BYTES.min(retained_cap);
        let suffix_for_worker = suffix;
        let deadline = Instant::now()
            .checked_add(self.runtime.timeout)
            .ok_or_else(|| {
                OfficialError::new(
                    OfficialErrorKind::InvalidOptions,
                    "runtime timeout overflows",
                )
            })?;
        let cancelled = Arc::new(AtomicBool::new(false));
        let mut cancel_guard = CancelOnDrop::new(cancelled.clone());
        let worker_cancelled = cancelled.clone();
        let task = tokio::task::spawn_blocking(move || {
            let bytes =
                input.read_limited_controlled(input_cap, &worker_cancelled, Some(deadline))?;
            if bytes.is_empty() {
                return Err(OfficialError::new(
                    OfficialErrorKind::InvalidInput,
                    "input is empty",
                ));
            }
            crate::official::native::csv::analyze_with_cancel(
                &bytes,
                &suffix_for_worker,
                Some(&worker_cancelled),
                retained_cap,
            )
        });
        cancel_guard.abort_on_drop(&task);
        let parsed =
            match tokio::time::timeout_at(tokio::time::Instant::from_std(deadline), task).await {
                Err(_) => {
                    return Err(OfficialError::new(
                        OfficialErrorKind::Deadline,
                        "CSV parse deadline exceeded",
                    ));
                }
                Ok(result) => result.map_err(|error| {
                    OfficialError::new(OfficialErrorKind::Upstream, error.to_string())
                })??,
            };
        cancel_guard.disarm();
        let result = OfficialResult {
            middle: parsed.middle,
            model: Some(parsed.model),
            markdown: parsed.markdown,
            structured: parsed.structured,
            assets: Default::default(),
            warnings: Vec::new(),
            metadata: crate::official::OfficialVersionMetadata {
                mineru_package: format!("mineru-rs/{}", env!("CARGO_PKG_VERSION")),
                schema_version: "2.0".into(),
                bundle_name: "native-csv".into(),
            },
        };
        if result.retained_bytes() > retained_cap {
            return Err(OfficialError::new(
                OfficialErrorKind::LimitExceeded,
                "result exceeds retained byte limit",
            ));
        }
        Ok(result)
    }

    fn persistent_worker(&self) -> Result<Arc<OfficialPersistentWorker>, OfficialError> {
        let mut slot = self.persistent.lock().map_err(|_| {
            OfficialError::new(
                OfficialErrorKind::RuntimeUnavailable,
                "persistent worker state is poisoned",
            )
        })?;
        if let Some(worker) = &*slot {
            return Ok(worker.clone());
        }
        let config = OfficialSessionConfig::for_official_sdk(
            self.runtime.model_dir.clone(),
            self.runtime.config.clone(),
        )
        .map_err(|error| OfficialError::new(OfficialErrorKind::InvalidOptions, error))?;
        let worker = Arc::new(
            OfficialPersistentWorker::new(self.runtime.python.clone(), config).map_err(
                |error| OfficialError::new(OfficialErrorKind::RuntimeUnavailable, error),
            )?,
        );
        *slot = Some(worker.clone());
        Ok(worker)
    }
}

#[cfg(test)]
mod cancellation_tests {
    use super::*;

    #[tokio::test]
    async fn dropping_parse_future_releases_its_blocking_worker() {
        let started = Arc::new(std::sync::Barrier::new(2));
        let stopped = Arc::new(std::sync::Barrier::new(2));
        let started_worker = started.clone();
        let stopped_worker = stopped.clone();
        let task = tokio::spawn(async move {
            let cancelled = Arc::new(AtomicBool::new(false));
            let _guard = CancelOnDrop::new(cancelled.clone());
            let worker_cancelled = cancelled.clone();
            tokio::task::spawn_blocking(move || {
                started_worker.wait();
                while !worker_cancelled.load(Ordering::Relaxed) {
                    std::thread::yield_now();
                }
                stopped_worker.wait();
            })
            .await
            .unwrap();
        });
        tokio::task::spawn_blocking(move || started.wait())
            .await
            .unwrap();
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            tokio::task::spawn_blocking(move || stopped.wait()),
        )
        .await
        .expect("dropped parse future did not cancel blocking work")
        .unwrap();
    }
}
