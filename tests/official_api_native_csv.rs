use serde_json::{Value, json};
use std::{
    io::Read,
    net::TcpListener,
    process::{Child, Command, Stdio},
    time::Duration,
};

struct Server(Child);
impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[tokio::test]
async fn official_api_csv_outputs_work_without_python_on_path() {
    let temp = tempfile::tempdir().unwrap();
    let empty_path = temp.path().join("empty-path");
    std::fs::create_dir(&empty_path).unwrap();
    let port = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let child = Command::new(env!("CARGO_BIN_EXE_mineru-api"))
        .args([
            "--official-api",
            "--host",
            "127.0.0.1",
            "--port",
            &port.to_string(),
        ])
        .env("PATH", &empty_path)
        .env_remove("MINERU_OFFICIAL_PYTHON")
        .env_remove("PYTHONHOME")
        .env_remove("PYTHONPATH")
        .env_remove("VIRTUAL_ENV")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let _server = Server(child);
    let base = format!("http://127.0.0.1:{port}");
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        if let Ok(Ok(response)) = tokio::time::timeout(
            Duration::from_secs(1),
            client.get(format!("{base}/v1/health")).send(),
        )
        .await
            && response.status().is_success()
        {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "API server failed to start"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let csv = b"name,score\nAda,99\n";
    let created: Value = client
        .post(format!("{base}/v1/tasks"))
        .json(&json!({
            "files": [{"file_name": "scores.csv", "bytes": csv.len(), "sha256sum": sha256(csv)}],
            "tier": "flash"
        }))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let task_id = created["task_id"].as_str().unwrap().to_owned();
    let file = &created["files"][0];
    let upload_url = link(&base, file["upload"]["url"].as_str().unwrap());
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

    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    let state = loop {
        let status: Value = client
            .get(format!("{base}/v1/tasks/{task_id}"))
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        match status["status"].as_str() {
            Some("completed") => break status,
            Some("failed" | "partial" | "canceled") => {
                panic!("CSV task did not complete: {status}")
            }
            _ if tokio::time::Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(100)).await
            }
            _ => panic!("CSV task timed out"),
        }
    };
    let file = &state["files"][0];
    assert_eq!(file["status"], "completed");
    let repeat: Value = client
        .put(link(
            &base,
            &format!(
                "/v1/tasks/{task_id}/uploads/{}",
                file["file_id"].as_str().unwrap()
            ),
        ))
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
    assert_eq!(repeat["status"], "completed");
    let result_url = file["result_url"].as_str().unwrap();
    let bytes = client
        .get(link(&base, result_url))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .bytes()
        .await
        .unwrap();
    assert!(!bytes.is_empty());
    let mut zip = zip::ZipArchive::new(std::io::Cursor::new(bytes)).unwrap();
    for name in ["markdown.md", "middle_json.json", "structured_content.json"] {
        let mut entry = zip
            .by_name(name)
            .unwrap_or_else(|_| panic!("missing {name}"));
        let mut text = String::new();
        entry.read_to_string(&mut text).unwrap();
        match name {
            "markdown.md" => assert!(text.contains("Ada")),
            _ => {
                let _: Value = serde_json::from_str(&text).unwrap();
            }
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
