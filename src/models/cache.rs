use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use reqwest::{StatusCode, Url, header::LOCATION};
use sha2::{Digest, Sha256};
use tempfile::NamedTempFile;
use thiserror::Error;
use tokio::time::timeout;

const REPO: &str = "opendatalab/MinerU-4_models_onnx";
const REVISION: &str = "358310b4f64b95f9fefc372ad899356e4111f376";
const FILE: &str = "Layout/PP-DocLayoutV2/inference.onnx";
const SIZE: u64 = 213_963_712;
const SHA256: &str = "cd540dc296ff3115fe78efa65b68501e6a8dc74b198acc9834a725ffaa095aac";
const CONFIG: &str = "Layout/PP-DocLayoutV2/inference.yml";
const CONFIG_SIZE: u64 = 1_482;
const CONFIG_SHA256: &str = "93100e0fb2cd76b9d246a853deea85e191d8333b2cfca7be76e82e4c4ebfa819";
const MAX_REDIRECTS: usize = 4;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DownloadPolicy {
    Auto,
    Offline,
}

#[derive(Clone, Copy, Debug)]
pub struct ModelArtifact {
    pub path: &'static str,
    pub size: u64,
    pub sha256: &'static str,
}

#[derive(Clone, Copy, Debug)]
pub struct ModelManifest {
    pub repository: &'static str,
    pub revision: &'static str,
    pub layout: ModelArtifact,
    pub layout_config: ModelArtifact,
}

pub const fn manifest() -> ModelManifest {
    ModelManifest {
        repository: REPO,
        revision: REVISION,
        layout: ModelArtifact {
            path: FILE,
            size: SIZE,
            sha256: SHA256,
        },
        layout_config: ModelArtifact {
            path: CONFIG,
            size: CONFIG_SIZE,
            sha256: CONFIG_SHA256,
        },
    }
}

#[derive(Debug, Error)]
pub enum ModelError {
    #[error("model cache I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("model download failed: {0}")]
    Http(String),
    #[error("model download timed out")]
    Timeout,
    #[error("model download rejected: {0}")]
    Invalid(String),
    #[error("model is not available in offline cache")]
    Offline,
}

impl From<reqwest::Error> for ModelError {
    fn from(error: reqwest::Error) -> Self {
        Self::Http(error.without_url().to_string())
    }
}

/// Content-addressed cache; callers must keep the configured root user-trusted.
#[derive(Clone, Debug)]
pub struct ModelCache {
    root: PathBuf,
    policy: DownloadPolicy,
    client: reqwest::Client,
}

impl ModelCache {
    pub fn new(root: impl Into<PathBuf>, policy: DownloadPolicy) -> Result<Self, ModelError> {
        Ok(Self {
            root: root.into(),
            policy,
            client: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(Duration::from_secs(30 * 60))
                .build()?,
        })
    }

    pub fn layout_model_path(&self) -> PathBuf {
        artifact_path(&self.root, SHA256, FILE)
    }

    pub fn layout_config_path(&self) -> PathBuf {
        artifact_path(&self.root, CONFIG_SHA256, CONFIG)
    }

    pub async fn verify_layout(&self) -> Result<PathBuf, ModelError> {
        self.verify_artifact(self.layout_model_path(), SIZE, SHA256)
            .await?;
        self.verify_artifact(self.layout_config_path(), CONFIG_SIZE, CONFIG_SHA256)
            .await?;
        Ok(self.layout_model_path())
    }

    async fn verify_artifact(
        &self,
        path: PathBuf,
        size: u64,
        hash: &str,
    ) -> Result<(), ModelError> {
        if verified_file(&path, size, hash).await? {
            Ok(())
        } else {
            Err(ModelError::Invalid(
                "cached artifact failed size or SHA-256 verification".into(),
            ))
        }
    }

    pub async fn ensure_layout(&self) -> Result<PathBuf, ModelError> {
        let graph = Spec::pinned(FILE, SIZE, SHA256);
        let config = Spec::pinned(CONFIG, CONFIG_SIZE, CONFIG_SHA256);
        let config_path = artifact_path(&self.root, CONFIG_SHA256, CONFIG);
        self.ensure_artifact(config, config_path).await?;
        let graph_path = artifact_path(&self.root, SHA256, FILE);
        self.ensure_artifact(graph, graph_path.clone()).await?;
        Ok(graph_path)
    }

    async fn ensure_artifact(&self, spec: Spec, destination: PathBuf) -> Result<(), ModelError> {
        if verified_file(&destination, spec.size, spec.sha256).await? {
            return Ok(());
        }
        if self.policy == DownloadPolicy::Offline {
            return Err(ModelError::Offline);
        }
        let parent = destination
            .parent()
            .expect("fixed artifact path has a parent");
        tokio::fs::create_dir_all(parent).await?;
        if tokio::fs::symlink_metadata(parent)
            .await?
            .file_type()
            .is_symlink()
        {
            return Err(ModelError::Invalid(
                "model cache directory is a symlink".into(),
            ));
        }
        let mut temp = NamedTempFile::new_in(parent)?;
        timeout(
            Duration::from_secs(30 * 60),
            self.download(&mut temp, spec.clone()),
        )
        .await
        .map_err(|_| ModelError::Timeout)??;
        if verified_file(&destination, spec.size, spec.sha256).await? {
            return Ok(());
        }
        // Replacing a concurrent winner is safe: both candidates have the manifest digest.
        temp.persist(&destination).map_err(|error| error.error)?;
        self.verify_artifact(destination, spec.size, spec.sha256)
            .await
    }

    pub async fn preload_layout(&self) -> Result<PathBuf, ModelError> {
        self.ensure_layout().await
    }

    async fn download(&self, temp: &mut NamedTempFile, spec: Spec) -> Result<(), ModelError> {
        let mut url = spec.url.clone();
        let mut redirects = 0;
        loop {
            check_url(&url)?;
            let response = self.client.get(url.clone()).send().await?;
            if response.status().is_redirection() {
                if redirects == MAX_REDIRECTS {
                    return Err(ModelError::Invalid("too many model redirects".into()));
                }
                let target = response
                    .headers()
                    .get(LOCATION)
                    .and_then(|v| v.to_str().ok())
                    .ok_or_else(|| ModelError::Invalid("invalid redirect".into()))?;
                url = url
                    .join(target)
                    .map_err(|e| ModelError::Invalid(e.to_string()))?;
                check_url(&url)?;
                redirects += 1;
                continue;
            }
            if response.status() != StatusCode::OK {
                return Err(ModelError::Invalid(format!(
                    "model server returned {}",
                    response.status()
                )));
            }
            if response.content_length().is_some_and(|n| n != spec.size) {
                return Err(ModelError::Invalid(
                    "model response has incorrect length".into(),
                ));
            }
            let mut stream = response.bytes_stream();
            // tempfile::reopen permits delete sharing on Windows; cancellation drops this handle before cleanup.
            let mut output = tokio::fs::File::from_std(temp.reopen()?);
            let mut size = 0u64;
            use futures_util::StreamExt;
            while let Some(chunk) = stream.next().await {
                let chunk = chunk?;
                size = size.saturating_add(chunk.len() as u64);
                if size > spec.size {
                    return Err(ModelError::Invalid(
                        "model response exceeds pinned size".into(),
                    ));
                }
                tokio::io::AsyncWriteExt::write_all(&mut output, &chunk).await?;
            }
            output.sync_all().await?;
            drop(output);
            if !verified_file(temp.path(), spec.size, spec.sha256).await? {
                return Err(ModelError::Invalid(
                    "download failed pinned size or SHA-256 verification".into(),
                ));
            }
            return Ok(());
        }
    }
}

#[derive(Clone)]
struct Spec {
    size: u64,
    sha256: &'static str,
    url: Url,
}

impl Spec {
    fn pinned(path: &'static str, size: u64, sha256: &'static str) -> Self {
        Self {
            size,
            sha256,
            url: source_url(path).expect("pinned artifact URL"),
        }
    }

    #[cfg(test)]
    fn fixture(size: u64, sha256: &'static str, url: Url) -> Self {
        Self { size, sha256, url }
    }
}

fn source_url(path: &str) -> Result<Url, ModelError> {
    Url::parse(&format!(
        "https://huggingface.co/{REPO}/resolve/{REVISION}/{path}"
    ))
    .map_err(|e| ModelError::Invalid(e.to_string()))
}

fn artifact_path(root: &Path, hash: &str, path: &str) -> PathBuf {
    root.join("models")
        .join(REPO.replace('/', "--"))
        .join(REVISION)
        .join(hash)
        .join(Path::new(path).file_name().expect("manifest file name"))
}

fn check_url(url: &Url) -> Result<(), ModelError> {
    let host = url.host_str().unwrap_or_default();
    let allowed = [
        "huggingface.co",
        "cdn-lfs.huggingface.co",
        "cdn-lfs-us-1.huggingface.co",
        "cdn-lfs-eu-1.huggingface.co",
        "us.aws.cdn.hf.co",
        "eu.aws.cdn.hf.co",
        "ap.aws.cdn.hf.co",
        "cas-bridge.xethub.hf.co",
        "transfer.xethub.hf.co",
    ];
    #[cfg(test)]
    if host == "127.0.0.1" && url.scheme() == "http" {
        return Ok(());
    }
    if url.scheme() != "https"
        || url.username() != ""
        || url.password().is_some()
        || (!allowed.contains(&host))
        || url.port().is_some_and(|port| port != 443)
        || url.fragment().is_some()
    {
        return Err(ModelError::Invalid(
            "model URL is not an approved HTTPS host".into(),
        ));
    }
    Ok(())
}

async fn verified_file(
    path: &Path,
    expected_size: u64,
    expected_hash: &str,
) -> Result<bool, ModelError> {
    let metadata = match tokio::fs::symlink_metadata(path).await {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e.into()),
    };
    if metadata.file_type().is_symlink() {
        return Err(ModelError::Invalid("model cache path is a symlink".into()));
    }
    if !metadata.is_file() || metadata.len() != expected_size {
        return Ok(false);
    }
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    let file = options.open(path)?;
    if !file.metadata()?.is_file() {
        return Ok(false);
    }
    let mut file = tokio::fs::File::from_std(file);
    let mut hash = Sha256::new();
    let mut size = 0u64;
    let mut buffer = vec![0; 64 * 1024];
    use tokio::io::AsyncReadExt;
    let mut bounded = (&mut file).take(expected_size.saturating_add(1));
    loop {
        let count = tokio::io::AsyncReadExt::read(&mut bounded, &mut buffer).await?;
        if count == 0 {
            break;
        }
        size += count as u64;
        hash.update(&buffer[..count]);
    }
    Ok(size == expected_size && hex_digest(&hash.finalize()) == expected_hash)
}

fn hex_digest(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut result = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        write!(&mut result, "{byte:02x}").expect("writing to String cannot fail");
    }
    result
}

#[cfg(test)]
#[path = "cache_tests.rs"]
mod tests;
