use mineru::{
    official::{OfficialRuntime, OfficialWorkerMode},
    official_api::{OfficialFileStore, OfficialService},
};
use serde_json::{Value, json};
use std::{
    io::{Cursor, Read},
    path::PathBuf,
    sync::{Arc, atomic::AtomicU64},
    time::Duration,
};
use tokio::sync::Semaphore;

#[tokio::test]
async fn official_v1_task_upload_results_and_leases_with_real_worker() {
    let Some(python) = std::env::var_os("MINERU_OFFICIAL_PYTHON").map(PathBuf::from) else {
        eprintln!("skipping official-v1 real-worker proof: MINERU_OFFICIAL_PYTHON is unset");
        return;
    };
    let slots = Arc::new(Semaphore::new(4));
    let gate = Arc::new(Semaphore::new(0));
    let store = OfficialFileStore::new(slots, gate.clone(), Duration::from_secs(3600))
        .unwrap()
        .with_shared_ids(Arc::new(AtomicU64::new(1)))
        .with_transport_limits(16 * 1024 * 1024, 2 * 1024 * 1024, 64 * 1024 * 1024);
    let runtime = OfficialRuntime::new()
        .with_python(python)
        .with_worker_mode(OfficialWorkerMode::Persistent)
        .with_timeout(Duration::from_secs(120));
    let service = OfficialService::new(runtime, store).unwrap();
    service.start().unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel();
    let router = service.router();
    let server = tokio::spawn(async move {
        axum::serve(listener, router)
            .with_graceful_shutdown(async {
                let _ = stop_rx.await;
            })
            .await
            .unwrap();
    });
    let client = reqwest::Client::new();
    let health: Value = client
        .get(format!("{base}/v1/health"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(health["version"], "4.2.3");
    reject_unsafe_task(&client, &base).await;
    let csv = b"name,score\nAda,99\n";
    let created: Value = client
        .post(format!("{base}/v1/tasks"))
        .json(
            &json!({"files":[{"file_name":"scores.csv","bytes":csv.len(),"sha256sum":sha256(csv)}],
            "tier":"flash"}),
        )
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let task_id = created["task_id"].as_str().unwrap().to_owned();
    let file_id = created["files"][0]["file_id"].as_str().unwrap().to_owned();
    let upload_url = link(
        &base,
        created["files"][0]["upload"]["url"].as_str().unwrap(),
    );
    assert_eq!(created["files"][0]["status"], "waiting-file");
    let uploaded: Value = client
        .put(upload_url)
        .header("content-type", "application/octet-stream")
        .body(csv.to_vec())
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(uploaded["status"], "pending");

    let queued: Value = client
        .post(format!("{base}/v1/tasks"))
        .json(&json!({"files":[{"file_name":"queued.pdf","bytes":1}]}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let queued_id = queued["task_id"].as_str().unwrap().to_owned();
    let canceled: Value = client
        .post(format!("{base}/v1/tasks/{queued_id}/cancel"))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(canceled["status"], "canceled");
    assert_eq!(
        client
            .post(format!("{base}/v1/tasks/{queued_id}/cancel"))
            .send()
            .await
            .unwrap()
            .status(),
        reqwest::StatusCode::CONFLICT
    );

    gate.add_permits(1);
    let end = tokio::time::Instant::now() + Duration::from_secs(120);
    let state = loop {
        let response = client
            .get(format!("{base}/v1/tasks/{task_id}"))
            .send()
            .await
            .unwrap();
        let state: Value = response.error_for_status().unwrap().json().await.unwrap();
        match state["status"].as_str() {
            Some("completed" | "partial" | "failed" | "canceled") => break state,
            _ if tokio::time::Instant::now() < end => {
                tokio::time::sleep(Duration::from_millis(200)).await
            }
            _ => panic!("official task did not finish before deadline"),
        }
    };
    assert_eq!(state["status"], "completed", "{state}");
    let file = &state["files"][0];
    assert_eq!(file["status"], "completed");
    assert_eq!(file["parse"]["tier"], "flash");
    assert_eq!(file["parse"]["parser_version"], "4.2.3");
    let result_url = file["result_url"].as_str().unwrap();
    let response = client
        .get(link(&base, result_url))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    assert_eq!(
        response.headers()[reqwest::header::CONTENT_TYPE]
            .to_str()
            .unwrap(),
        "application/zip"
    );
    assert_eq!(
        response.headers()["x-content-type-options"]
            .to_str()
            .unwrap(),
        "nosniff"
    );
    let bytes = response.bytes().await.unwrap();
    check_zip(&bytes);
    let object: Value = client
        .get(format!("{base}/v1/files/{file_id}"))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(object["purpose"], "parse_output");
    assert_eq!(object["file_name"], "scores.zip");
    let _ = stop_tx.send(());
    server.await.unwrap();
    service.shutdown().await.unwrap();
}

async fn reject_unsafe_task(client: &reqwest::Client, base: &str) {
    for body in [
        json!({"files":[{"file_name":"a.pdf","url":"https://example.invalid/a.pdf","bytes":1}]}),
        json!({"files":[{"file_name":"a.pdf","bytes":1,"local_path":"/etc/passwd"}]}),
        json!({"files":[{"file_name":"a.pdf","bytes":1}],"runtime":{"python":"/tmp/python"}}),
        json!({"files":[]}),
        json!({"files":[{"file_name":"../a.pdf","bytes":1}]}),
    ] {
        let response = client
            .post(format!("{base}/v1/tasks"))
            .json(&body)
            .send()
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            reqwest::StatusCode::BAD_REQUEST,
            "{body}"
        );
        let error: Value = response.json().await.unwrap();
        assert_eq!(error["error"]["code"], "invalid_request");
        assert!(error["error"].get("type").is_none());
    }
    let response = client
        .post(format!("{base}/v1/tasks?tier=flash"))
        .json(&json!({"files":[{"file_name":"a.pdf","bytes":1}]}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::BAD_REQUEST);
    let response = client
        .post(format!("{base}/v1/tasks"))
        .header("content-type", "multipart/form-data")
        .body("x")
        .send()
        .await
        .unwrap();
    assert_eq!(
        response.status(),
        reqwest::StatusCode::UNSUPPORTED_MEDIA_TYPE
    );
}

fn check_zip(bytes: &[u8]) {
    let mut archive = zip::ZipArchive::new(Cursor::new(bytes)).unwrap();
    for name in ["markdown.md", "middle_json.json", "structured_content.json"] {
        let mut entry = archive
            .by_name(name)
            .unwrap_or_else(|_| panic!("missing self-contained ZIP member {name}"));
        let mut text = String::new();
        entry.read_to_string(&mut text).unwrap();
        if name == "markdown.md" {
            assert!(text.contains("Ada"));
        } else {
            let _: Value = serde_json::from_str(&text).unwrap();
        }
    }
}

fn sha256(data: &[u8]) -> String {
    use sha2::Digest;
    sha2::Sha256::digest(data)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}
fn link(base: &str, raw: &str) -> String {
    if raw.starts_with("http://") || raw.starts_with("https://") {
        raw.to_owned()
    } else {
        format!("{base}{raw}")
    }
}
