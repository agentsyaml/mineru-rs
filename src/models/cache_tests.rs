use super::*;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};

const BODY: &[u8] = b"test model";
const HASH: &str = "e6a8036e9cb9f8c9f52ffd985c4d6f0498d94e6919f4c67b6423da0176e549fa";

#[test]
fn pinned_source_url_uses_hugging_face_host_and_exact_revision_path() {
    let url = source_url(FILE).unwrap();
    assert_eq!(url.host_str(), Some("huggingface.co"));
    assert_eq!(url.path(), format!("/{REPO}/resolve/{REVISION}/{FILE}"));
}

#[tokio::test]
async fn redirects_reject_credentials_hosts_ports_and_downgrades() {
    let targets = [
        "https://user:secret@huggingface.co/model",
        "https://untrusted.invalid/model",
        "https://huggingface.co:8443/model",
        "http://untrusted.invalid/model",
    ];
    for target in targets {
        let dir = tempfile::tempdir().unwrap();
        let cache = ModelCache::new(dir.path(), DownloadPolicy::Auto).unwrap();
        let spec = Spec::fixture(10, HASH, redirect_fixture(target.into(), 1).await);
        let result = cache
            .ensure_artifact(spec, artifact_path(dir.path(), HASH, "redirect.bin"))
            .await;
        assert!(
            matches!(result, Err(ModelError::Invalid(_))),
            "target was accepted: {target}"
        );
    }
}

#[tokio::test]
async fn redirect_hops_are_bounded() {
    let dir = tempfile::tempdir().unwrap();
    let cache = ModelCache::new(dir.path(), DownloadPolicy::Auto).unwrap();
    let target = "http://{address}/redirect".to_string();
    let url = redirect_fixture(target, MAX_REDIRECTS + 1).await;
    let spec = Spec::fixture(10, HASH, url);
    let result = cache
        .ensure_artifact(spec, artifact_path(dir.path(), HASH, "hops.bin"))
        .await;
    assert!(matches!(result, Err(ModelError::Invalid(message)) if message.contains("too many")));
}

async fn fixture(body: &'static [u8], declared: usize, requests: usize) -> Url {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        for _ in 0..requests {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0; 1024];
            let _ = socket.read(&mut request).await;
            socket
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {declared}\r\nConnection: close\r\n\r\n"
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
            socket.write_all(body).await.unwrap();
        }
    });
    Url::parse(&format!("http://{address}/fixture")).unwrap()
}

async fn redirect_fixture(location: String, requests: usize) -> Url {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let location = location.replace("{address}", &address.to_string());
    tokio::spawn(async move {
        for _ in 0..requests {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0; 1024];
            let _ = socket.read(&mut request).await;
            socket.write_all(format!("HTTP/1.1 302 Found\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").as_bytes()).await.unwrap();
        }
    });
    Url::parse(&format!("http://{address}/redirect")).unwrap()
}

#[tokio::test]
async fn ensure_reuses_verified_cache_and_replaces_corruption() {
    let dir = tempfile::tempdir().unwrap();
    let cache = ModelCache::new(dir.path(), DownloadPolicy::Auto).unwrap();
    let spec = Spec::fixture(BODY.len() as u64, HASH, fixture(BODY, BODY.len(), 2).await);
    let path = artifact_path(dir.path(), HASH, "fixture.bin");
    cache
        .ensure_artifact(spec.clone(), path.clone())
        .await
        .unwrap();
    assert_eq!(tokio::fs::read(&path).await.unwrap(), BODY);
    tokio::fs::write(&path, b"bad").await.unwrap();
    cache.ensure_artifact(spec, path.clone()).await.unwrap();
    assert_eq!(tokio::fs::read(&path).await.unwrap(), BODY);
}

#[tokio::test]
async fn offline_does_not_contact_network_and_rejects_missing_entry() {
    let dir = tempfile::tempdir().unwrap();
    let cache = ModelCache::new(dir.path(), DownloadPolicy::Offline).unwrap();
    let spec = Spec::fixture(BODY.len() as u64, HASH, fixture(BODY, BODY.len(), 1).await);
    let path = artifact_path(dir.path(), HASH, "fixture.bin");
    assert!(matches!(
        cache.ensure_artifact(spec, path).await,
        Err(ModelError::Offline)
    ));
    assert!(
        tokio::fs::read_dir(dir.path())
            .await
            .unwrap()
            .next_entry()
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn wrong_hash_truncation_and_symlink_are_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let cache = ModelCache::new(dir.path(), DownloadPolicy::Auto).unwrap();
    let bad = Spec::fixture(BODY.len() as u64, "00", fixture(BODY, BODY.len(), 1).await);
    let bad_path = artifact_path(dir.path(), "00", "fixture.bin");
    assert!(cache.ensure_artifact(bad, bad_path.clone()).await.is_err());
    assert!(!tokio::fs::try_exists(&bad_path).await.unwrap());

    let mut signed_url = fixture(b"short", 10, 1).await;
    signed_url.set_query(Some("X-Amz-Signature=very-secret"));
    let truncated = Spec::fixture(10, HASH, signed_url);
    let error = cache
        .ensure_artifact(truncated, artifact_path(dir.path(), HASH, "truncate.bin"))
        .await
        .unwrap_err();
    assert!(!error.to_string().contains("very-secret"));
    assert!(!format!("{error:?}").contains("very-secret"));
    #[cfg(unix)]
    {
        let path = artifact_path(dir.path(), HASH, "link.bin");
        tokio::fs::create_dir_all(path.parent().unwrap())
            .await
            .unwrap();
        std::os::unix::fs::symlink(dir.path().join("outside"), &path).unwrap();
        assert!(verified_file(&path, BODY.len() as u64, HASH).await.is_err());
        assert!(!path.to_string_lossy().contains(".."));
    }
}

#[tokio::test]
async fn concurrent_ensure_publishes_only_verified_content() {
    let dir = tempfile::tempdir().unwrap();
    let cache = ModelCache::new(dir.path(), DownloadPolicy::Auto).unwrap();
    let spec = Spec::fixture(BODY.len() as u64, HASH, fixture(BODY, BODY.len(), 2).await);
    let path = artifact_path(dir.path(), HASH, "fixture.bin");
    let (first, second) = tokio::join!(
        cache.ensure_artifact(spec.clone(), path.clone()),
        cache.ensure_artifact(spec, path.clone())
    );
    first.unwrap();
    second.unwrap();
    assert!(verified_file(&path, BODY.len() as u64, HASH).await.unwrap());
}

#[tokio::test]
async fn cancellation_drops_unpublished_temporary_file() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut request = [0; 1024];
        let _ = socket.read(&mut request).await;
        socket
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\n\r\n")
            .await
            .unwrap();
        socket.write_all(b"partial").await.unwrap();
        let _ = ready_tx.send(());
        std::future::pending::<()>().await;
    });
    let dir = tempfile::tempdir().unwrap();
    let cache = ModelCache::new(dir.path(), DownloadPolicy::Auto).unwrap();
    let spec = Spec::fixture(
        10,
        HASH,
        Url::parse(&format!("http://{address}/slow")).unwrap(),
    );
    let path = artifact_path(dir.path(), HASH, "fixture.bin");
    let task_path = path.clone();
    let task = tokio::spawn(async move { cache.ensure_artifact(spec, task_path).await });
    ready_rx.await.unwrap();
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let mut entries = tokio::fs::read_dir(path.parent().unwrap()).await.unwrap();
            if let Some(entry) = entries.next_entry().await.unwrap()
                && entry.metadata().await.unwrap().len() >= 7
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("download wrote partial bytes before cancellation");
    task.abort();
    let _ = task.await;
    server.abort();
    assert!(!tokio::fs::try_exists(&path).await.unwrap());
    assert!(
        tokio::fs::read_dir(path.parent().unwrap())
            .await
            .unwrap()
            .next_entry()
            .await
            .unwrap()
            .is_none()
    );
}
