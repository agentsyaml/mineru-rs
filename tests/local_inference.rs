#![cfg(feature = "local-inference")]

use image::ImageReader;
use mineru::{
    inference::{InferenceError, LayoutSession},
    models::{DownloadPolicy, ModelCache},
};

#[test]
fn runtime_initialization_child() {
    let Ok(mode) = std::env::var("MINERU_ORT_TEST_CHILD") else {
        return;
    };
    let runtime = std::env::var("MINERU_TEST_ORT_DYLIB").expect("set MINERU_TEST_ORT_DYLIB");
    if mode == "conflict" {
        let _uncommitted = ort::init_from("/missing/onnxruntime-library");
    }
    let error = LayoutSession::load("missing-model.onnx", runtime)
        .err()
        .expect("conflict must fail before model loading");
    match mode.as_str() {
        "conflict" => assert!(
            matches!(error, InferenceError::Runtime(message) if message.contains("incompatible global library"))
        ),
        "old" => assert!(
            matches!(error, InferenceError::Runtime(message) if message.contains("unsupported ONNX Runtime version") || message.contains("does not provide API"))
        ),
        _ => panic!("unknown child test mode"),
    }
}

#[test]
#[ignore = "requires MINERU_TEST_ORT_DYLIB"]
fn conflicting_ort_initializer_fails_closed_in_subprocess() {
    assert_child_mode("conflict");
}

#[test]
#[ignore = "requires MINERU_TEST_OLD_ORT_DYLIB"]
fn incompatible_ort_runtime_fails_closed_in_subprocess() {
    let old_runtime =
        std::env::var("MINERU_TEST_OLD_ORT_DYLIB").expect("set MINERU_TEST_OLD_ORT_DYLIB");
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "runtime_initialization_child"])
        .env("MINERU_ORT_TEST_CHILD", "old")
        .env("MINERU_TEST_ORT_DYLIB", old_runtime)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn assert_child_mode(mode: &str) {
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "runtime_initialization_child"])
        .env("MINERU_ORT_TEST_CHILD", mode)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[tokio::test]
#[ignore = "requires MINERU_TEST_ORT_DYLIB and a previously cached model"]
async fn optional_real_graph_smoke() {
    let runtime = std::env::var("MINERU_TEST_ORT_DYLIB").expect("set MINERU_TEST_ORT_DYLIB");
    let cache_dir = std::env::var("MINERU_TEST_MODEL_CACHE").expect("set MINERU_TEST_MODEL_CACHE");
    let cache = ModelCache::new(cache_dir, DownloadPolicy::Offline).unwrap();
    let model = cache.ensure_layout().await.unwrap();
    let missing_model = model.with_extension("missing.onnx");
    assert!(LayoutSession::load(&missing_model, &runtime).is_err());
    let mut session = LayoutSession::load(model, runtime).unwrap();
    let image = ImageReader::open("tests/fixtures/official/arxiv_2410.21169v5/vlm/images/cc9d646c918053bb628e661ed5772ce1ec4682952a90dc8e687eff8cb42f5df2.jpg").unwrap().decode().unwrap();
    eprintln!("ONNX I/O: {:?}", session.io_metadata());
    let rows = session.infer_raw(&image).unwrap();
    eprintln!("raw model rows: {}", rows.len());
    assert!(
        !rows.is_empty(),
        "fixture should produce at least one raw detection"
    );
}
