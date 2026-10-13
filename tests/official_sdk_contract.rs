use bytes::Bytes;
use mineru::official::{
    HtmlSourceContext, OfficialBatchItem, OfficialInput, OfficialOcrMode, OfficialParseOptions,
    OfficialRuntime, OfficialTier, OfficialVlmConfig, OfficialWorkerMode,
};

#[test]
fn public_sdk_types_are_serde_compatible_and_secret_debug_is_redacted() {
    let options = OfficialParseOptions::new()
        .with_tier(OfficialTier::Flash)
        .with_ocr_mode(OfficialOcrMode::Ocr)
        .with_source_context(HtmlSourceContext::new())
        .with_vlm_config(OfficialVlmConfig::new().with_api_key("not-printable"));
    let json = serde_json::to_value(&options).unwrap();
    assert_eq!(json["tier"], "flash");
    assert_eq!(json["ocr_mode"], "ocr");
    assert!(!format!("{options:?}").contains("not-printable"));

    let input = OfficialInput::bytes("file.csv", Bytes::from_static(b"a,b\n1,2\n"));
    let batch = OfficialBatchItem::new(input).with_options(options);
    assert!(format!("{batch:?}").contains("file.csv"));
    let runtime = OfficialRuntime::default().with_worker_mode(OfficialWorkerMode::Persistent);
    assert!(!format!("{runtime:?}").contains("/"));
}
