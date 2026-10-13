use super::*;

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
