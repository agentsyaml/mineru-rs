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
    sync::{Arc, Mutex},
    time::Instant,
};

pub struct OfficialParser {
    runtime: Arc<OfficialRuntime>,
    persistent: Mutex<Option<Arc<OfficialPersistentWorker>>>,
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
        let worker = self.persistent_worker()?;
        let mut aggregate = 0usize;
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
            let result = if remaining == 0 {
                Err(OfficialError::new(
                    OfficialErrorKind::LimitExceeded,
                    "batch exceeds retained result byte limit",
                ))
            } else {
                self.parse_one(item.input, options, Some(worker.clone()), remaining)
                    .await
                    .inspect(|result| {
                        aggregate = aggregate.saturating_add(result.retained_bytes());
                    })
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
        let bytes = input.read()?;
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
        let deadline = Instant::now()
            .checked_add(self.runtime.timeout)
            .ok_or_else(|| {
                OfficialError::new(
                    OfficialErrorKind::InvalidOptions,
                    "runtime timeout overflows",
                )
            })?;
        let bundle = if let Some(worker) = persistent {
            worker.run(&bytes, &suffix, request, deadline).await
        } else if self.runtime.worker_mode == crate::command::OfficialWorkerMode::Persistent {
            self.persistent_worker()?
                .run(&bytes, &suffix, request, deadline)
                .await
        } else {
            OfficialWorker::new(self.runtime.python.clone())
                .map_err(|error| OfficialError::new(OfficialErrorKind::RuntimeUnavailable, error))?
                .run(&bytes, &suffix, request, deadline)
                .await
        }
        .map_err(crate::official::error::classify)?;
        OfficialResult::from_bundle(bundle.path(), retained_cap)
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
