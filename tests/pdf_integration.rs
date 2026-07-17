use axum::{
    Json, Router,
    extract::State,
    routing::{get, post},
};
use mineru::{ClientConfig, MinerUClient, ParseOptions, PdfInput, write_outputs};
use serde_json::{Value, json};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

#[derive(Clone, Default)]
struct Requests(Arc<AtomicUsize>, Arc<AtomicUsize>);

#[tokio::test]
async fn parses_minimal_fixture_against_local_openai_mock() {
    async fn models(State(state): State<Requests>) -> Json<Value> {
        state.0.fetch_add(1, Ordering::Relaxed);
        Json(json!({"data":[{"id":"test-model","owned_by":"test"}]}))
    }
    async fn completions(State(state): State<Requests>) -> Json<Value> {
        state.1.fetch_add(1, Ordering::Relaxed);
        Json(json!({"choices":[{"finish_reason":"stop","message":{"content":""}}]}))
    }

    let requests = Requests::default();
    let app = Router::new()
        .route("/v1/models", get(models))
        .route("/v1/chat/completions", post(completions))
        .with_state(requests.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let client =
        MinerUClient::new(ClientConfig::new(format!("http://{address}"), "test-model").unwrap())
            .unwrap();
    assert_eq!(client.check_model().await.unwrap().id, "test-model");
    let document = client
        .parse_pdf(
            PdfInput::Path("tests/fixtures/pdf/minimal.pdf".into()),
            ParseOptions::default(),
        )
        .await
        .unwrap();
    assert!(document.assets.iter().any(|asset| {
        asset.kind == mineru::AssetKind::Other("layout_preview".into())
            && asset.relative_path == std::path::Path::new("minimal_layout.pdf")
    }));
    assert_eq!(document.pages.len(), 1);
    assert!(document.pages[0].blocks.is_empty());
    assert_eq!(document.assets.len(), 1);
    let output = tempfile::tempdir().unwrap();
    let manifest = write_outputs(&document, output.path()).unwrap();
    assert!(manifest.markdown.exists());
    assert!(output.path().join("minimal_layout.pdf").exists());
    assert_eq!(requests.0.load(Ordering::Relaxed), 1);
    assert_eq!(requests.1.load(Ordering::Relaxed), 1);
}
