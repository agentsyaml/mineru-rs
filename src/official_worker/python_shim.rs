use std::{
    io::Write,
    path::{Path, PathBuf},
};
use tempfile::TempDir;

pub(crate) const PYTHON_SHIM: &str = include_str!("../../python/mineru_official_worker.py");
pub(crate) const PYTHON_WORKER_FILES: &[(&str, &str)] = &[
    ("mineru_official_worker.py", PYTHON_SHIM),
    (
        "mineru_official_worker_contract.py",
        include_str!("../../python/mineru_official_worker_contract.py"),
    ),
    (
        "mineru_official_worker_protocol.py",
        include_str!("../../python/mineru_official_worker_protocol.py"),
    ),
    (
        "mineru_official_worker_bundle.py",
        include_str!("../../python/mineru_official_worker_bundle.py"),
    ),
    (
        "mineru_official_worker_operations.py",
        include_str!("../../python/mineru_official_worker_operations.py"),
    ),
    (
        "mineru_official_worker_render.py",
        include_str!("../../python/mineru_official_worker_render.py"),
    ),
];

pub(super) struct PythonShim {
    _temporary: TempDir,
    path: PathBuf,
}

impl PythonShim {
    pub(super) fn new() -> Result<Self, String> {
        let temporary = tempfile::tempdir()
            .map_err(|error| format!("official worker Python shim tempdir: {error}"))?;
        let path = temporary.path().join("mineru_official_worker.py");
        for &(name, source) in PYTHON_WORKER_FILES {
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(temporary.path().join(name))
                .map_err(|error| format!("official worker source create: {error}"))?;
            file.write_all(source.as_bytes())
                .and_then(|()| file.flush())
                .map_err(|error| format!("official worker source write: {error}"))?;
        }
        Ok(Self {
            _temporary: temporary,
            path,
        })
    }

    pub(super) fn path(&self) -> &Path {
        &self.path
    }
}
