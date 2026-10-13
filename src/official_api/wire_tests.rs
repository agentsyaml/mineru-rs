#[cfg(test)]
mod tests {
    use crate::official_api::wire::*;
    use axum::response::IntoResponse;
    use serde_json::json;

    #[test]
    fn upload_wire_defaults_and_validation() {
        let upload: UploadCreate = serde_json::from_value(json!({
            "filename":"a.pdf", "bytes":4, "mime_type":"application/pdf"
        }))
        .unwrap();
        assert_eq!(upload.purpose, "parse");
        upload.validate().unwrap();
        let expiry: ExpiresAfter = serde_json::from_value(json!({"anchor":"created_at"})).unwrap();
        assert_eq!(expiry.seconds, 3600);
        assert!(
            serde_json::from_value::<UploadCreate>(json!({"filename":"a", "bytes":1})).is_err()
        );
        assert!(
            serde_json::from_value::<UploadCreate>(
                json!({"filename":"a", "bytes":1, "mime_type":"x", "local_path":"/tmp/a"})
            )
            .is_err()
        );
    }

    #[test]
    fn job_wire_is_closed_and_validates_sources() {
        let job: JobCreate = serde_json::from_value(json!({"files":[{
            "source":{"type":"inline", "name":"a.pdf", "data":"YQ=="}, "page_range":"1-3,5"
        }]}))
        .unwrap();
        assert_eq!(job.output_formats, vec![OutputFormat::Markdown]);
        assert!(matches!(job.ocr_mode, OcrMode::Auto));
        job.validate().unwrap();
        assert!(
            serde_json::from_value::<JobCreate>(
                json!({"files":[{"source":{"type":"url","url":"https://x"}}]})
            )
            .is_err()
        );
        assert!(serde_json::from_value::<JobCreate>(json!({"files":[],"runtime":"x"})).is_err());
        assert!(
            serde_json::from_value::<JobCreate>(
                json!({"files":[{"source":{"type":"file_id","file_id":"x"},"page_range":"0-2"}]})
            )
            .unwrap()
            .validate()
            .is_err()
        );
        for value in [
            "", " all ", "3-r1", "r3-5", "r1-r1", "1-1", "r3-r1", " 3 - r1 ",
        ] {
            assert!(validate_page_range(value).is_ok(), "{value}");
        }
        for value in ["r1-r3", "3-1", "r0", "+1", "01", "1,,2"] {
            assert!(validate_page_range(value).is_err(), "{value}");
        }
        assert!(validate_inline_base64("%%%=").is_err());
    }

    #[test]
    fn health_and_error_envelope_are_official_v1() {
        let health = serde_json::to_value(Health::default()).unwrap();
        assert_eq!(health["status"], "ok");
        assert_eq!(health["version"], "4.0.10");
        assert!(health.get("schema_version").is_none());
        let response = WireError::too_large().into_response();
        assert_eq!(response.status(), axum::http::StatusCode::PAYLOAD_TOO_LARGE);
    }
}
