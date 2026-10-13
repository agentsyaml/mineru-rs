use super::*;
use axum::http::StatusCode;
use std::{net::SocketAddr, time::Duration};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

struct PolicyTestServer {
    address: SocketAddr,
    stop: oneshot::Sender<()>,
    task: tokio::task::JoinHandle<Result<(), std::io::Error>>,
    _root: TempDir,
}

async fn start(bind: &str, exposed: bool, allow_http: bool) -> PolicyTestServer {
    let root = tempfile::tempdir().unwrap();
    let listener = tokio::net::TcpListener::bind(bind).await.unwrap();
    let address = listener.local_addr().unwrap();
    let config = ServiceConfig::new(
        1,
        root.path().join("output"),
        OfficialPdfOptions::default(),
        None,
        None,
    )
    .unwrap()
    .public_policy(exposed, allow_http);
    let (stop, stopped) = oneshot::channel();
    let task = tokio::spawn(
        ServiceBuilder::new(config)
            .with_official_runtime(crate::official::OfficialRuntime::new())
            .serve(listener, async move {
                let _ = stopped.await;
            }),
    );
    PolicyTestServer {
        address,
        stop,
        task,
        _root: root,
    }
}

async fn raw_partial(address: SocketAddr, method: &str, path: &str, length: usize) -> String {
    let peer = SocketAddr::from(([127, 0, 0, 1], address.port()));
    let mut stream = tokio::net::TcpStream::connect(peer).await.unwrap();
    let request = format!(
        "{method} {path} HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {length}\r\nConnection: close\r\n\r\n"
    );
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut response = Vec::new();
    tokio::time::timeout(Duration::from_secs(2), stream.read_to_end(&mut response))
        .await
        .unwrap()
        .unwrap();
    String::from_utf8_lossy(&response).into_owned()
}

impl PolicyTestServer {
    async fn stop(self) {
        let _ = self.stop.send(());
        assert!(self.task.await.unwrap().is_ok());
    }
}

async fn complete_upload(client: &reqwest::Client, base: &str) {
    let bytes = b"name,value\nalpha,1\n";
    let upload: serde_json::Value = client.post(format!("{base}/v1/uploads"))
        .json(&serde_json::json!({"filename":"a.csv","bytes":bytes.len(),"mime_type":"text/csv","purpose":"parse"}))
        .send().await.unwrap().error_for_status().unwrap().json().await.unwrap();
    let id = upload["id"].as_str().unwrap();
    assert_eq!(
        client
            .put(format!("{base}{}", upload["upload_url"].as_str().unwrap()))
            .body(bytes.to_vec())
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::OK
    );
    assert_eq!(
        client
            .post(format!("{base}/v1/uploads/{id}/complete"))
            .json(&serde_json::json!({}))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::OK
    );
}

#[tokio::test]
async fn official_ingress_uses_bound_listener_public_client_policy() {
    let public = start("0.0.0.0:0", true, false).await;
    let base = format!("http://127.0.0.1:{}", public.address.port());
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    for (method, path) in [
        ("POST", "/v1/uploads"),
        ("PUT", "/v1/uploads/pending/content"),
        ("POST", "/v1/uploads/pending/complete"),
        ("POST", "/v1/parse/jobs"),
    ] {
        let response = raw_partial(public.address, method, path, 1_048_576).await;
        assert!(
            response.starts_with("HTTP/1.1 400"),
            "{method} {path}: {response}"
        );
        assert!(
            response.contains("\"public_policy\""),
            "{method} {path}: {response}"
        );
        assert!(response.contains("public HTTP-client requests are disabled"));
    }
    for path in [
        "/v1/health",
        "/v1/tiers",
        "/v1/files",
        "/v1/parse/jobs",
        "/v1/usage",
    ] {
        assert_eq!(
            client
                .get(format!("{base}{path}"))
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::OK
        );
    }
    let cancel = raw_partial(public.address, "POST", "/v1/uploads/unknown/cancel", 0).await;
    assert!(cancel.starts_with("HTTP/1.1 404"));
    assert!(!cancel.contains("public HTTP-client requests are disabled"));
    let cancel_job = raw_partial(public.address, "DELETE", "/v1/parse/jobs/unknown", 0).await;
    assert!(cancel_job.starts_with("HTTP/1.1 404"));
    assert!(!cancel_job.contains("public HTTP-client requests are disabled"));
    for path in ["/tasks", "/file_parse"] {
        let response = raw_partial(public.address, "POST", path, 1_048_576).await;
        assert!(response.starts_with("HTTP/1.1 400"));
        assert!(response.contains("{\"detail\":\"public HTTP-client requests are disabled\"}"));
    }
    let usage: serde_json::Value = client
        .get(format!("{base}/v1/usage"))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(usage["current"]["jobs_created"], 0);
    assert_eq!(usage["counters"]["visible_files"], 0);
    assert_eq!(usage["counters"]["input_bytes"], 0);
    public.stop().await;

    let opted_in = start("0.0.0.0:0", true, true).await;
    complete_upload(
        &client,
        &format!("http://127.0.0.1:{}", opted_in.address.port()),
    )
    .await;
    opted_in.stop().await;

    let loopback = start("127.0.0.1:0", false, false).await;
    complete_upload(&client, &format!("http://{}", loopback.address)).await;
    loopback.stop().await;
}
