use serde_json::{Value, json};
use std::{
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
    let job: Value = client.post(format!("{base}/v1/parse/jobs"))
        .json(&json!({"files":[{"source":{"type":"inline","name":"scores.csv","data":"bmFtZSxzY29yZQpBZGEsOTkK"}}],
            "tier":"flash", "output_formats":["markdown","middle_json","structured_content","zip"]}))
        .send().await.unwrap().error_for_status().unwrap().json().await.unwrap();
    let id = job["job_id"].as_str().unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    let state = loop {
        let status: Value = client
            .get(format!("{base}/v1/parse/jobs/{id}"))
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
            Some("failed" | "partial" | "canceled") => panic!("CSV job did not complete: {status}"),
            _ if tokio::time::Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(100)).await
            }
            _ => panic!("CSV job timed out"),
        }
    };
    let outputs = &state["files"][0]["output_files"];
    for format in ["markdown", "middle_json", "structured_content", "zip"] {
        let id = outputs[format]["file_id"].as_str().unwrap();
        let bytes = client
            .get(format!("{base}/v1/files/{id}/content"))
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .bytes()
            .await
            .unwrap();
        assert!(!bytes.is_empty(), "empty {format}");
        match format {
            "markdown" => assert!(std::str::from_utf8(&bytes).unwrap().contains("Ada")),
            "middle_json" | "structured_content" => {
                let _: Value = serde_json::from_slice(&bytes).unwrap();
            }
            "zip" => {
                let mut zip = zip::ZipArchive::new(std::io::Cursor::new(bytes)).unwrap();
                for name in ["markdown.md", "middle_json.json", "structured_content.json"] {
                    assert!(zip.by_name(name).is_ok(), "missing {name}");
                }
            }
            _ => unreachable!(),
        }
    }
}
