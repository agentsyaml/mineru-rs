use axum::{Json, Router, extract::State, routing::post};
use mineru::{ClientConfig, MinerUClient, ParseOptions, PdfInput};
use serde_json::{Value, json};
use std::{path::PathBuf, sync::Arc};
use tokio::{net::TcpListener, sync::Mutex};

const CONTRACT: &str = include_str!("fixtures/vlm/mineru_3.4.4_vl_utils_1.0.5_contract.json");

#[tokio::test]
async fn mineru_344_vlm_http_requests_match_reviewed_fixture() {
    let expected: Value = serde_json::from_str(CONTRACT).unwrap();
    let received = Arc::new(Mutex::new(Vec::<Value>::new()));
    let app = Router::new()
        .route("/v1/chat/completions", post(mock_completion))
        .with_state(received.clone());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let config = ClientConfig::new(
        format!("http://{address}"),
        expected["model"].as_str().unwrap(),
    )
    .unwrap();
    MinerUClient::new(config)
        .unwrap()
        .parse_pdf(
            PdfInput::Path(
                PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/pdf/minimal.pdf"),
            ),
            ParseOptions::default(),
        )
        .await
        .unwrap();

    let received = received.lock().await;
    let requests = expected["requests"].as_array().unwrap();
    assert_eq!(received.len(), requests.len());
    for expected_request in requests {
        let prompt = expected_request["prompt"].as_str().unwrap();
        let actual = received
            .iter()
            .find(|request| request["messages"][1]["content"][1]["text"] == prompt)
            .unwrap_or_else(|| panic!("missing request for {prompt:?}"));
        assert_eq!(actual["model"], expected["model"]);
        assert_eq!(
            actual["messages"][0],
            json!({"role":"system", "content": expected["system"]})
        );
        assert_eq!(actual["messages"][1]["role"], "user");
        let content = actual["messages"][1]["content"].as_array().unwrap();
        assert_eq!(content[0]["type"], expected["image_content"]["type"]);
        assert!(
            content[0]["image_url"]["url"]
                .as_str()
                .unwrap()
                .starts_with(
                    expected["image_content"]["data_url_prefix"]
                        .as_str()
                        .unwrap()
                )
        );
        assert_eq!(
            content[1],
            json!({"type":"text", "text": expected_request["prompt"]})
        );
        for field in [
            "temperature",
            "top_p",
            "top_k",
            "presence_penalty",
            "frequency_penalty",
            "repetition_penalty",
        ] {
            let actual = actual[field].as_f64().unwrap();
            let expected = expected_request[field].as_f64().unwrap();
            assert!(
                (actual - expected).abs() < 0.000_001,
                "{field}: {actual} != {expected}"
            );
        }
        assert_eq!(
            actual["skip_special_tokens"],
            expected["skip_special_tokens"]
        );
        assert_eq!(
            actual["vllm_xargs"]["no_repeat_ngram_size"],
            expected_request["no_repeat_ngram_size"]
        );
        for field in expected["default_max_token_fields"].as_array().unwrap() {
            assert!(
                actual.get(field.as_str().unwrap()).is_none(),
                "{field} must be absent by default"
            );
        }
    }
}

async fn mock_completion(
    State(received): State<Arc<Mutex<Vec<Value>>>>,
    Json(request): Json<Value>,
) -> Json<Value> {
    let prompt = request["messages"][1]["content"][1]["text"]
        .as_str()
        .unwrap()
        .to_owned();
    received.lock().await.push(request);
    Json(
        json!({"choices":[{"finish_reason":"length","message":{"content": match prompt.as_str() {
            "\nLayout Detection:" => "<|box_start|>0 0 450 200<|box_end|><|ref_start|>text<|ref_end|><|box_start|>500 0 999 200<|box_end|><|ref_start|>table<|ref_end|><|box_start|>0 250 450 500<|box_end|><|ref_start|>equation<|ref_end|><|box_start|>500 250 999 500<|box_end|><|ref_start|>image<|ref_end|>",
            "\nTable Recognition:" => "<table><tr><td>fixture table</td></tr></table>",
            "\nFormula Recognition:" => "x = 1",
            "\nImage Analysis:" => "fixture image",
            "\nText Recognition:" => "fixture text",
            _ => unreachable!("unexpected prompt: {prompt:?}"),
        }}}]}),
    )
}

#[test]
fn client_configuration_rejects_non_http_urls() {
    assert!(ClientConfig::new("ftp://example.test", "model").is_err());
}
