use crate::command::OfficialWorkerMode;
use std::{fmt, path::PathBuf, time::Duration};

pub(crate) const RETAINED_RESULT_CAP: usize = 64 * 1024 * 1024;
pub(crate) const INPUT_BYTE_CAP: usize = 200 * 1024 * 1024;
pub(crate) const BUNDLE_BYTE_CAP: u64 = 1024 * 1024 * 1024;

#[derive(Clone)]
#[non_exhaustive]
pub struct OfficialRuntime {
    pub(crate) python: Option<PathBuf>,
    pub(crate) config: Option<PathBuf>,
    pub(crate) model_dir: Option<PathBuf>,
    pub(crate) worker_mode: OfficialWorkerMode,
    pub(crate) timeout: Duration,
}
impl fmt::Debug for OfficialRuntime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OfficialRuntime")
            .field("python", &self.python.as_ref().map(|_| "[configured]"))
            .field("config", &self.config.as_ref().map(|_| "[configured]"))
            .field(
                "model_dir",
                &self.model_dir.as_ref().map(|_| "[configured]"),
            )
            .field("worker_mode", &self.worker_mode)
            .field("timeout", &self.timeout)
            .finish()
    }
}

impl Default for OfficialRuntime {
    fn default() -> Self {
        Self::new()
    }
}

impl OfficialRuntime {
    pub fn new() -> Self {
        Self {
            python: None,
            config: None,
            model_dir: None,
            worker_mode: OfficialWorkerMode::PerDocument,
            timeout: Duration::from_secs(24 * 60 * 60),
        }
    }

    pub fn with_python(mut self, path: PathBuf) -> Self {
        self.python = Some(path);
        self
    }
    pub fn with_config(mut self, path: PathBuf) -> Self {
        self.config = Some(path);
        self
    }
    pub fn with_model_dir(mut self, path: PathBuf) -> Self {
        self.model_dir = Some(path);
        self
    }
    pub fn with_worker_mode(mut self, mode: OfficialWorkerMode) -> Self {
        self.worker_mode = mode;
        self
    }
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }
}
