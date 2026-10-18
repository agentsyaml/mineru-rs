use clap::{Args, ValueEnum};
use mineru::{command::OfficialWorkerMode, official::OfficialRuntime};
use std::{path::PathBuf, time::Duration};

#[derive(Args, Default)]
pub(super) struct OfficialArgs {
    /// Enable the official MinerU4.0.12 upload/jobs API.
    #[arg(long)]
    official_api: bool,
    /// Trusted Python executable (absolute path to an existing file).
    #[arg(long)]
    official_python: Option<PathBuf>,
    /// Trusted parser configuration file (must be absolute).
    #[arg(long)]
    official_config: Option<PathBuf>,
    /// Trusted model directory (must be absolute).
    #[arg(long)]
    official_model_dir: Option<PathBuf>,
    /// Worker lifecycle when official API is enabled.
    #[arg(long, value_enum)]
    official_worker_mode: Option<WorkerMode>,
    /// Maximum seconds for each official parse (default: 600).
    #[arg(long)]
    official_timeout_secs: Option<u64>,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum WorkerMode {
    PerDocument,
    Persistent,
}

impl OfficialArgs {
    pub(super) fn runtime(&self) -> Result<Option<OfficialRuntime>, String> {
        let configured = self.official_python.is_some()
            || self.official_config.is_some()
            || self.official_model_dir.is_some()
            || self.official_worker_mode.is_some()
            || self.official_timeout_secs.is_some();
        if !self.official_api {
            return if configured {
                Err("official API options require --official-api".into())
            } else {
                Ok(None)
            };
        }
        let timeout = self.official_timeout_secs.unwrap_or(600);
        if timeout == 0 {
            return Err("--official-timeout-secs must be positive".into());
        }
        validate_python(self.official_python.as_deref())?;
        validate_absolute_file(self.official_config.as_deref(), "--official-config", true)?;
        validate_absolute_file(
            self.official_model_dir.as_deref(),
            "--official-model-dir",
            false,
        )?;
        let mode = match self.official_worker_mode.unwrap_or(WorkerMode::Persistent) {
            WorkerMode::PerDocument => OfficialWorkerMode::PerDocument,
            WorkerMode::Persistent => OfficialWorkerMode::Persistent,
        };
        let mut runtime = OfficialRuntime::new()
            .with_worker_mode(mode)
            .with_timeout(Duration::from_secs(timeout));
        if let Some(path) = &self.official_python {
            runtime = runtime.with_python(path.clone());
        }
        if let Some(path) = &self.official_config {
            runtime = runtime.with_config(path.clone());
        }
        if let Some(path) = &self.official_model_dir {
            runtime = runtime.with_model_dir(path.clone());
        }
        Ok(Some(runtime))
    }
}

fn validate_python(path: Option<&std::path::Path>) -> Result<(), String> {
    let Some(path) = path else {
        return Ok(());
    };
    if path.is_absolute() {
        return path
            .is_file()
            .then_some(())
            .ok_or_else(|| "--official-python must name an existing file".into());
    }
    Err("--official-python must be an absolute path".into())
}
fn validate_absolute_file(
    path: Option<&std::path::Path>,
    flag: &str,
    file: bool,
) -> Result<(), String> {
    let Some(path) = path else {
        return Ok(());
    };
    if !path.is_absolute() {
        return Err(format!("{flag} must be an absolute path"));
    }
    let valid = if file { path.is_file() } else { path.is_dir() };
    valid
        .then_some(())
        .ok_or_else(|| format!("{flag} does not name the required existing path"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[derive(Parser)]
    struct Probe {
        #[command(flatten)]
        official: OfficialArgs,
    }

    #[test]
    fn opt_in_defaults_and_orphan_options() {
        let disabled = Probe::try_parse_from(["probe"]).unwrap().official;
        assert!(disabled.runtime().unwrap().is_none());
        let orphan = Probe::try_parse_from(["probe", "--official-timeout-secs", "10"])
            .unwrap()
            .official;
        assert!(
            orphan
                .runtime()
                .unwrap_err()
                .contains("require --official-api")
        );
        let enabled = Probe::try_parse_from(["probe", "--official-api"])
            .unwrap()
            .official;
        let runtime = enabled.runtime().unwrap().unwrap();
        let debug = format!("{runtime:?}");
        assert!(debug.contains("Persistent") && debug.contains("600s"));
        let per_document = Probe::try_parse_from([
            "probe",
            "--official-api",
            "--official-worker-mode",
            "per-document",
        ])
        .unwrap()
        .official;
        assert!(format!("{:?}", per_document.runtime().unwrap().unwrap()).contains("PerDocument"));
        assert!(
            Probe::try_parse_from([
                "probe",
                "--official-api",
                "--official-worker-mode",
                "invalid"
            ])
            .is_err()
        );
    }

    #[test]
    fn rejects_bad_timeout_and_untrusted_relative_paths() {
        let timeout =
            Probe::try_parse_from(["probe", "--official-api", "--official-timeout-secs", "0"])
                .unwrap()
                .official;
        assert!(timeout.runtime().unwrap_err().contains("positive"));
        let config = Probe::try_parse_from([
            "probe",
            "--official-api",
            "--official-config",
            "relative.yaml",
        ])
        .unwrap()
        .official;
        assert!(config.runtime().unwrap_err().contains("absolute path"));
        assert!(validate_python(Some(std::path::Path::new("./python3"))).is_err());
        assert!(validate_python(Some(std::path::Path::new("python3"))).is_err());
    }

    #[test]
    fn validates_python_for_actual_runtime_construction() {
        let default = Probe::try_parse_from(["probe", "--official-api"])
            .unwrap()
            .official;
        assert!(default.runtime().unwrap().is_some());
        let executable = std::env::current_exe().unwrap();
        let explicit = Probe::try_parse_from([
            "probe".to_owned(),
            "--official-api".to_owned(),
            "--official-python".to_owned(),
            executable.to_string_lossy().into_owned(),
        ])
        .unwrap()
        .official;
        let runtime = explicit.runtime().unwrap().unwrap();
        mineru::official::OfficialParser::new(runtime).unwrap();
    }
}
