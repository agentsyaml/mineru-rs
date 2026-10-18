use mineru::{
    official::{OfficialRuntime, OfficialWorkerMode},
    official_api::{OfficialFileStore, OfficialService},
};
use serde_json::{Value, json};
use std::{
    io::Cursor,
    path::PathBuf,
    sync::{Arc, atomic::AtomicU64},
    time::Duration,
};
use tokio::sync::Semaphore;

#[tokio::test]
async fn official_v1_upload_job_outputs_and_leases_with_real_worker() {
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
    assert_eq!(health["version"], "4.0.12");
    reject_unsafe_job(&client, &base).await;
    let csv = b"name,score\nAda,99\n";
    let upload: Value = client.post(format!("{base}/v1/uploads"))
        .json(&json!({"filename":"scores.csv","bytes":csv.len(),"mime_type":"text/csv","purpose":"parse"}))
        .send().await.unwrap().error_for_status().unwrap().json().await.unwrap();
    let upload_id = upload["id"].as_str().unwrap();
    assert_eq!(upload["upload_method"], "PUT");
    let put_url = format!("{base}{}", upload["upload_url"].as_str().unwrap());
    assert_eq!(
        client
            .put(put_url)
            .body(csv.to_vec())
            .send()
            .await
            .unwrap()
            .status(),
        reqwest::StatusCode::OK
    );
    let completed: Value = client
        .post(format!("{base}/v1/uploads/{upload_id}/complete"))
        .json(&json!({}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let file_id = completed["file"]["id"].as_str().unwrap().to_owned();
    assert_eq!(
        client
            .get(format!("{base}/v1/files/{file_id}/content"))
            .send()
            .await
            .unwrap()
            .status(),
        reqwest::StatusCode::FORBIDDEN
    );

    let job: Value = client.post(format!("{base}/v1/parse/jobs"))
        .json(&json!({"files":[{"source":{"type":"file_id","file_id":file_id}}],"tier":"flash","ocr_mode":"auto",
            "output_formats":["markdown","middle_json","structured_content","zip"]}))
        .send().await.unwrap().error_for_status().unwrap().json().await.unwrap();
    let job_id = job["job_id"].as_str().unwrap().to_owned();
    let queued: Value = client
        .post(format!("{base}/v1/parse/jobs"))
        .json(&json!({"files":[{"source":{"type":"inline","name":"queued.pdf","data":"YQ=="}}]}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let canceled: Value = client
        .delete(format!(
            "{base}/v1/parse/jobs/{}",
            queued["job_id"].as_str().unwrap()
        ))
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
            .delete(format!("{base}/v1/files/{file_id}"))
            .send()
            .await
            .unwrap()
            .status(),
        reqwest::StatusCode::OK
    );
    assert_eq!(
        client
            .get(format!("{base}/v1/files/{file_id}"))
            .send()
            .await
            .unwrap()
            .status(),
        reqwest::StatusCode::NOT_FOUND
    );
    gate.add_permits(1);
    let end = tokio::time::Instant::now() + Duration::from_secs(120);
    let result = loop {
        let response = client
            .get(format!("{base}/v1/parse/jobs/{job_id}"))
            .send()
            .await
            .unwrap();
        let state: Value = response.error_for_status().unwrap().json().await.unwrap();
        match state["status"].as_str() {
            Some("completed" | "partial" | "failed" | "canceled") => break state,
            _ if tokio::time::Instant::now() < end => {
                tokio::time::sleep(Duration::from_millis(200)).await
            }
            _ => panic!("official job did not finish before deadline"),
        }
    };
    assert_eq!(result["status"], "completed", "{result}");
    let outputs = result.pointer("/files/0/output_files").unwrap();
    for format in ["markdown", "middle_json", "structured_content", "zip"] {
        let id = outputs[format]["file_id"].as_str().unwrap();
        let response = client
            .get(format!("{base}/v1/files/{id}/content"))
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap();
        assert_eq!(
            response.headers()[reqwest::header::CONTENT_TYPE]
                .to_str()
                .unwrap(),
            "application/octet-stream"
        );
        assert_eq!(
            response.headers()["x-content-type-options"]
                .to_str()
                .unwrap(),
            "nosniff"
        );
        let bytes = response.bytes().await.unwrap();
        assert!(!bytes.is_empty(), "empty {format} output");
        if format == "markdown" {
            assert!(std::str::from_utf8(&bytes).unwrap().contains("Ada"));
        }
        if format == "middle_json" || format == "structured_content" {
            serde_json::from_slice::<Value>(&bytes).unwrap();
        }
        if format == "zip" {
            check_zip(&bytes);
        }
    }
    let _ = stop_tx.send(());
    server.await.unwrap();
    service.shutdown().await.unwrap();
}

async fn reject_unsafe_job(client: &reqwest::Client, base: &str) {
    for body in [
        json!({"files":[{"source":{"type":"url","url":"https://example.invalid/a.pdf"}}]}),
        json!({"files":[{"source":{"type":"local","path":"/etc/passwd"}}]}),
        json!({"files":[{"source":{"type":"inline","name":"a.pdf","data":"YQ=="}}],"runtime":{"python":"/tmp/python"}}),
    ] {
        let response = client
            .post(format!("{base}/v1/parse/jobs"))
            .json(&body)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::BAD_REQUEST);
        let error: Value = response.json().await.unwrap();
        assert!(error.get("error").is_some());
    }
}

fn check_zip(bytes: &[u8]) {
    let mut archive = zip::ZipArchive::new(Cursor::new(bytes)).unwrap();
    for name in ["markdown.md", "middle_json.json", "structured_content.json"] {
        assert!(
            archive.by_name(name).is_ok(),
            "missing self-contained ZIP member {name}"
        );
    }
}
