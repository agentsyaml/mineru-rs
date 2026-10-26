use super::*;
#[path = "input_tests.rs"]
mod input_tests;

#[tokio::test]
async fn empty_persistent_batch_does_not_resolve_or_spawn_python() {
    let runtime =
        OfficialRuntime::new().with_worker_mode(crate::command::OfficialWorkerMode::Persistent);
    let parser = OfficialParser::new(runtime).unwrap();
    assert!(
        parser
            .parse_batch_rich(Vec::new())
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn csv_parse_batch_render_and_restore_never_spawn_python() {
    let (runtime, _python_dir) = input_tests::runtime_with_missing_python();
    let runtime = runtime.with_worker_mode(crate::command::OfficialWorkerMode::Persistent);
    let parser = OfficialParser::new(runtime.clone()).unwrap();
    let input = OfficialInput::bytes(
        "table.csv",
        bytes::Bytes::from_static(b"name,score\nAda,99\n"),
    );
    assert_eq!(
        parser
            .parse_rich(input.clone(), OfficialParseOptions::new())
            .await
            .unwrap_err()
            .kind,
        OfficialErrorKind::InvalidOptions
    );
    let result = parser
        .parse_rich(
            input.clone(),
            OfficialParseOptions::new().with_tier(OfficialTier::Flash),
        )
        .await
        .unwrap();
    assert_eq!(result.version_metadata().bundle_name, "native-csv");
    let expected_body = "| name | score |\n| --- | --- |\n| Ada | 99 |";
    assert_eq!(result.markdown(), expected_body);
    assert_eq!(
        result.structured_content()["pages"][0]["blocks"][0]["content"],
        expected_body
    );
    let middle = result.to_dict(&runtime, true).await.unwrap();
    assert_eq!(middle["schema"], "docvortex.middle");
    assert_eq!(middle["schema_version"], "2.0");
    assert_eq!(middle["extensions"], serde_json::json!({}));
    let skipped_defaults_json: serde_json::Value =
        serde_json::from_str(&result.to_json(&runtime, true).await.unwrap()).unwrap();
    assert_eq!(skipped_defaults_json, middle);
    let restored = OfficialResult::from_dict(&runtime, middle.clone(), Default::default())
        .await
        .unwrap();
    let restored_full = restored.to_dict(&runtime, false).await.unwrap();
    assert_eq!(restored_full["schema"], "docvortex.middle");
    assert_eq!(restored_full["schema_version"], "2.0");
    assert_eq!(restored_full["extensions"], serde_json::json!({}));
    let output = tempfile::tempdir().unwrap();
    restored.save(output.path(), "roundtrip").unwrap();
    for (index, version) in ["0.3.0", "0.3.2"].into_iter().enumerate() {
        let mut legacy_middle = middle.clone();
        legacy_middle["metadata"]["producer"]["version"] = version.into();
        let from_dict =
            OfficialResult::from_dict(&runtime, legacy_middle.clone(), Default::default())
                .await
                .unwrap();
        assert_eq!(
            from_dict.to_dict(&runtime, false).await.unwrap()["metadata"]["producer"]["version"],
            version
        );
        from_dict
            .render(
                &runtime,
                OfficialRenderFormat::Markdown,
                OfficialRenderOptions::new(),
            )
            .await
            .unwrap();
        from_dict
            .save(output.path(), &format!("legacy-dict-{index}"))
            .unwrap();

        let legacy_json = serde_json::to_string(&legacy_middle).unwrap();
        let from_json = OfficialResult::from_json(&runtime, &legacy_json, Default::default())
            .await
            .unwrap();
        let serialized: serde_json::Value =
            serde_json::from_str(&from_json.to_json(&runtime, false).await.unwrap()).unwrap();
        assert_eq!(serialized["metadata"]["producer"]["version"], version);
        from_json
            .render(
                &runtime,
                OfficialRenderFormat::Markdown,
                OfficialRenderOptions::new(),
            )
            .await
            .unwrap();
        from_json
            .save(output.path(), &format!("legacy-json-{index}"))
            .unwrap();
    }
    for invalid_version in [
        serde_json::Value::Null,
        serde_json::json!(""),
        serde_json::json!(3),
    ] {
        let mut invalid_middle = middle.clone();
        invalid_middle["metadata"]["producer"]["version"] = invalid_version;
        assert_eq!(
            OfficialResult::from_dict(&runtime, invalid_middle, Default::default())
                .await
                .unwrap_err()
                .kind,
            OfficialErrorKind::InvalidInput
        );
    }
    let mut missing_version = middle.clone();
    missing_version["metadata"]["producer"]
        .as_object_mut()
        .unwrap()
        .remove("version");
    assert!(
        OfficialResult::from_dict(&runtime, missing_version, Default::default())
            .await
            .unwrap_err()
            .kind
            == OfficialErrorKind::InvalidInput
    );
    let mut incompatible_schema = middle.clone();
    incompatible_schema["schema_version"] = "3.0".into();
    assert_eq!(
        OfficialResult::from_dict(&runtime, incompatible_schema, Default::default())
            .await
            .unwrap_err()
            .kind,
        OfficialErrorKind::InvalidInput
    );
    let json = result.to_json(&runtime, false).await.unwrap();
    let restored_json = OfficialResult::from_json(&runtime, &json, Default::default())
        .await
        .unwrap();
    for restored in [restored, restored_json] {
        assert_eq!(restored.markdown(), result.markdown());
        assert_eq!(restored.structured_content(), result.structured_content());
    }
    let multiline = parser
        .parse_rich(
            OfficialInput::bytes(
                "multiline.csv",
                bytes::Bytes::from_static(b"head\n\"a\0b\r\nc\""),
            ),
            OfficialParseOptions::new().with_tier(OfficialTier::Flash),
        )
        .await
        .unwrap();
    let multiline_json = multiline.to_json(&runtime, false).await.unwrap();
    let multiline_restored =
        OfficialResult::from_json(&runtime, &multiline_json, Default::default())
            .await
            .unwrap();
    assert_eq!(multiline_restored.markdown(), multiline.markdown());
    let whitespace = parser
        .parse_rich(
            OfficialInput::bytes(
                "whitespace.csv",
                bytes::Bytes::from_static(b"head\n\"\x1ca\t b\x0c c\x0b d\x1f\""),
            ),
            OfficialParseOptions::new().with_tier(OfficialTier::Flash),
        )
        .await
        .unwrap();
    assert!(
        whitespace
            .markdown()
            .contains("\u{fffd}a b\u{fffd} c\u{fffd} d\u{fffd}")
    );
    let whitespace_json = whitespace.to_json(&runtime, false).await.unwrap();
    let whitespace_restored =
        OfficialResult::from_json(&runtime, &whitespace_json, Default::default())
            .await
            .unwrap();
    assert_eq!(whitespace_restored.markdown(), whitespace.markdown());
    let c1 = parser
        .parse_rich(
            OfficialInput::bytes(
                "c1.csv",
                bytes::Bytes::copy_from_slice("head\n\"a\u{85}b\u{9f}c\"".as_bytes()),
            ),
            OfficialParseOptions::new().with_tier(OfficialTier::Flash),
        )
        .await
        .unwrap();
    assert!(c1.markdown().contains("a\u{85}b\u{9f}c"));
    assert!(
        c1.model_json().unwrap()["pages"][0][0]["content"]
            .as_str()
            .unwrap()
            .contains("a\u{85}b\u{9f}c")
    );
    assert!(
        c1.middle_json()["pages"][0]["blocks"][0]["content"][0]["content"]
            .as_str()
            .unwrap()
            .contains("a\u{85}b\u{9f}c")
    );
    assert!(
        c1.structured_content()["pages"][0]["blocks"][0]["content"]
            .as_str()
            .unwrap()
            .contains("a\u{85}b\u{9f}c")
    );
    let c1_json = c1.to_json(&runtime, false).await.unwrap();
    let c1_restored = OfficialResult::from_json(&runtime, &c1_json, Default::default())
        .await
        .unwrap();
    assert_eq!(c1_restored.markdown(), c1.markdown());
    assert_eq!(c1_restored.structured_content(), c1.structured_content());
    assert!(matches!(
        result
            .render(
                &runtime,
                OfficialRenderFormat::Markdown,
                OfficialRenderOptions::new()
            )
            .await
            .unwrap(),
        OfficialRendered::Text { .. }
    ));
    assert!(matches!(
        result
            .render(
                &runtime,
                OfficialRenderFormat::StructuredContent,
                OfficialRenderOptions::new()
            )
            .await
            .unwrap(),
        OfficialRendered::Json { .. }
    ));
    assert_eq!(
        result
            .render(
                &runtime,
                OfficialRenderFormat::Html,
                OfficialRenderOptions::new()
            )
            .await
            .unwrap_err()
            .kind,
        OfficialErrorKind::UnsupportedOperation
    );
    assert_eq!(
        result
            .render(
                &runtime,
                OfficialRenderFormat::Markdown,
                OfficialRenderOptions::new().with_mode(OfficialRenderMode::Default),
            )
            .await
            .unwrap_err()
            .kind,
        OfficialErrorKind::UnsupportedOperation
    );
    let batch = parser
        .parse_batch_rich(vec![OfficialBatchItem::new(input)])
        .await
        .unwrap();
    assert!(batch[0].result.is_ok());
    assert!(
        parser
            .parse_batch_rich(Vec::new())
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        parser
            .parse_rich(
                OfficialInput::bytes(
                    "invalid.csv",
                    bytes::Bytes::from_static(&[0xff, 0xfe, b'a', 0]),
                ),
                OfficialParseOptions::new().with_tier(OfficialTier::Flash),
            )
            .await
            .unwrap_err()
            .kind,
        OfficialErrorKind::UnsupportedOperation
    );
    assert_eq!(
        OfficialInput::bytes("oversize.csv", bytes::Bytes::from_static(b"ab"))
            .read_limited(1)
            .unwrap_err()
            .kind,
        OfficialErrorKind::LimitExceeded
    );
    let directory = tempfile::tempdir().unwrap();
    assert_eq!(
        OfficialInput::path(directory.path())
            .read_limited(1024)
            .unwrap_err()
            .kind,
        OfficialErrorKind::InvalidInput
    );
    parser.shutdown().await.unwrap();
}

#[tokio::test]
async fn table_markdown_matches_structured_body_edge_and_break_semantics() {
    let (runtime, _python_dir) = input_tests::runtime_with_missing_python();
    let parser = OfficialParser::new(runtime.clone()).unwrap();
    let cases = [
        ("\u{2003}a\u{2003},b\n1,2", "| a | b |"),
        ("a\u{85},b\n1,2", "| a | b |"),
        ("a\u{85}b,b\n1,2", "| a\u{85}b | b |"),
        ("\"\na\n\",b\n1,2", "| <br>a<br> | b |"),
        ("<br>,b\n1,2", "| &lt;br> | b |"),
    ];
    for (index, (source, expected)) in cases.into_iter().enumerate() {
        let result = parser
            .parse_rich(
                OfficialInput::bytes(
                    format!("cell-{index}.csv"),
                    bytes::Bytes::copy_from_slice(source.as_bytes()),
                ),
                OfficialParseOptions::new().with_tier(OfficialTier::Flash),
            )
            .await
            .unwrap();
        assert!(
            result.markdown().contains(expected),
            "{}",
            result.markdown()
        );
        assert!(!result.markdown().ends_with('\n'));
        assert_eq!(
            result.structured_content()["pages"][0]["blocks"][0]["content"],
            result.markdown()
        );
        let json = result.to_json(&runtime, false).await.unwrap();
        let restored = OfficialResult::from_json(&runtime, &json, Default::default())
            .await
            .unwrap();
        assert_eq!(restored.markdown(), result.markdown());
        assert_eq!(restored.structured_content(), result.structured_content());
    }
}

#[test]
fn restore_metadata_router_skips_deep_unknown_fields_and_uses_last_duplicate() {
    let deep = (0..130).fold("0".to_owned(), |value, _| format!("[{value}]"));
    let html = format!(
        r#"{{"metadata":{{"file_suffix":"html","producer":{{"name":"mineru-rs"}}}},"extensions":{{"nested":{deep}}}}}"#
    );
    assert!(
        !native::restore_route::inspect_json(&html)
            .unwrap()
            .is_native_csv()
    );
    let csv = format!(
        r#"{{"metadata":{{"file_suffix":"csv","producer":{{"name":"mineru-rs"}}}},"extensions":{{"nested":{deep}}}}}"#
    );
    assert!(
        native::restore_route::inspect_json(&csv)
            .unwrap()
            .is_native_csv()
    );
    let duplicate = r#"{"metadata":{"file_suffix":"csv","producer":{"name":"mineru-rs"}},"metadata":{"file_suffix":"html","producer":{"name":"mineru-rs"}}}"#;
    assert!(
        !native::restore_route::inspect_json(duplicate)
            .unwrap()
            .is_native_csv()
    );
    let duplicate_suffix = r#"{"metadata":{"file_suffix":"csv","file_suffix":"html","producer":{"name":"mineru-rs"}}}"#;
    assert!(
        !native::restore_route::inspect_json(duplicate_suffix)
            .unwrap()
            .is_native_csv()
    );
}

#[tokio::test]
async fn deeply_nested_non_native_restore_reaches_the_worker_path() {
    let deep = (0..130).fold("0".to_owned(), |value, _| format!("[{value}]"));
    let json = format!(
        r#"{{"schema":"docvortex.middle","schema_version":"2.0","metadata":{{"file_suffix":"html","producer":{{"name":"mineru-rs","version":"fixture"}}}},"pages":[],"is_full_document":true,"extensions":{{"nested":{deep}}}}}"#
    );
    let (runtime, _python_dir) = input_tests::runtime_with_missing_python();
    assert_eq!(
        OfficialResult::from_json(&runtime, &json, Default::default())
            .await
            .unwrap_err()
            .kind,
        OfficialErrorKind::RuntimeUnavailable
    );
}

#[test]
fn accepts_exact_parseable_extension_set_and_native_flash_only() {
    assert_eq!(input::suffixes().len(), 28);
    for suffix in input::suffixes() {
        let input =
            OfficialInput::bytes(format!("input.{suffix}"), bytes::Bytes::from_static(b"x"));
        assert_eq!(input.suffix().unwrap(), suffix);
        let options = OfficialParseOptions::default();
        let valid = options.validate(suffix).is_ok();
        assert_eq!(
            valid,
            matches!(
                suffix,
                "pdf" | "png" | "jpg" | "jpeg" | "webp" | "gif" | "bmp" | "tiff" | "jp2"
            )
        );
    }
    let native = OfficialParseOptions {
        tier: Some(OfficialTier::Flash),
        ..Default::default()
    };
    assert!(native.validate("csv").is_ok());
}

#[test]
fn rejects_page_ranges_for_non_pdf_and_ambiguous_vlm_overrides() {
    let options = OfficialParseOptions {
        page_range: Some("3-r1".into()),
        ..Default::default()
    };
    assert!(options.validate("png").is_err());
    let conflict = OfficialParseOptions {
        vlm_config: Some(OfficialVlmConfig::default()),
        vlm_api_key: Some("key".into()),
        ..Default::default()
    };
    assert!(conflict.validate("pdf").is_err());
    let bad_engine = OfficialParseOptions {
        vlm_config: Some(OfficialVlmConfig {
            engine: "unknown".into(),
            ..Default::default()
        }),
        ..Default::default()
    };
    assert!(bad_engine.validate("pdf").is_err());
}

#[test]
fn render_contract_is_exact_and_rejects_cross_format_options() {
    use serde_json::json;
    let formats = [
        OfficialRenderFormat::Markdown,
        OfficialRenderFormat::Html,
        OfficialRenderFormat::Latex,
        OfficialRenderFormat::Docx,
        OfficialRenderFormat::Epub,
        OfficialRenderFormat::StructuredContent,
        OfficialRenderFormat::ContentList,
        OfficialRenderFormat::ContentListV2,
        OfficialRenderFormat::Pdf,
    ];
    assert_eq!(
        formats.map(OfficialRenderFormat::as_str),
        [
            "markdown",
            "html",
            "latex",
            "docx",
            "epub",
            "structured_content",
            "content_list",
            "content_list_v2",
            "pdf"
        ]
    );
    for format in formats {
        assert!(
            OfficialRenderOptions::new().validate(format).is_ok(),
            "empty {format:?} options"
        );
    }
    let options = OfficialRenderOptions::new().with_mode(OfficialRenderMode::Full);
    assert!(options.validate(OfficialRenderFormat::Markdown).is_ok());
    assert!(options.validate(OfficialRenderFormat::Pdf).is_err());
    let value = serde_json::to_value(
        OfficialRenderOptions::new().with_pdf_layout(OfficialPdfLayout::Original),
    )
    .unwrap();
    assert_eq!(value["layout"], json!("original"));
}
