#[cfg(test)]
mod tests {
    use crate::official_api::wire::*;
    use axum::response::IntoResponse;
    use serde_json::json;

    #[test]
    fn create_request_defaults_and_validation() {
        let request: CreateTaskRequest =
            serde_json::from_value(json!({"files":[{"file_name":"a.pdf","bytes":4}]})).unwrap();
        request.validate().unwrap();
        assert_eq!(request.page_range, "");
        assert_eq!(request.output_formats, vec![TaskOutputFormat::Default]);
        let deduped: CreateTaskRequest = serde_json::from_value(json!({
            "files": [{"file_name": "a.pdf", "bytes": 4}],
            "output_formats": ["docx", "default", "docx"]
        }))
        .unwrap();
        assert_eq!(
            deduped.normalized_formats(),
            vec![TaskOutputFormat::Docx, TaskOutputFormat::Default]
        );
        assert!(
            serde_json::from_value::<CreateTaskRequest>(
                json!({"files":[{"file_name":"a","bytes":1,"local_path":"/tmp/a"}]})
            )
            .is_err()
        );
        assert!(
            serde_json::from_value::<CreateTaskRequest>(
                json!({"files":[{"file_name":"a.pdf","bytes":1}],"unexpected":true})
            )
            .is_err()
        );
        assert!(
            serde_json::from_value::<CreateTaskRequest>(
                json!({"files":[{"file_name":"a.pdf","bytes":0}]})
            )
            .unwrap()
            .validate()
            .is_err()
        );
        assert!(
            serde_json::from_value::<CreateTaskRequest>(json!({"files":[{"file_name":"a.pdf"}]}))
                .unwrap()
                .validate()
                .is_err()
        );
        assert!(
            serde_json::from_value::<CreateTaskRequest>(
                json!({"files":[{"file_name":"a.pdf","url":"https://x","bytes":1}]})
            )
            .unwrap()
            .validate()
            .is_err()
        );
        assert!(
            serde_json::from_value::<CreateTaskRequest>(json!({"files":[]}))
                .unwrap()
                .validate()
                .is_err()
        );
        assert!(
            serde_json::from_value::<CreateTaskRequest>(
                json!({"files":[{"file_name":"a.pdf","bytes":1}],"output_formats":[]})
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
    }

    #[test]
    fn file_inputs_validate_names_hashes_and_sources() {
        let mut input: TaskFileInput = serde_json::from_value(json!({
            "file_name": "report.pdf", "bytes": 10
        }))
        .unwrap();
        input.validate(0).unwrap();
        input.file_name = "../escape.pdf".into();
        assert!(input.validate(0).is_err());
        input.file_name = "report.pdf".into();
        input.sha256sum = Some("A".repeat(64));
        assert!(input.validate(0).is_err());
        input.sha256sum = Some("a".repeat(64));
        input.validate(0).unwrap();
        input.page_range = Some("1-5".into());
        input.validate(0).unwrap();
        input.page_range = Some("r3-r1".into());
        input.validate(0).unwrap();
        input.page_range = Some("r1-r3".into());
        assert!(input.validate(0).is_err());
    }

    #[test]
    fn task_payloads_omit_absent_optionals_and_keep_zero_progress() {
        let created = CreateTaskResponse {
            task_id: "job_1".into(),
            status: TaskStatus::WaitingFile,
            files: vec![CreatedTaskFile {
                file_id: "file_1".into(),
                file_name: "a.pdf".into(),
                status: TaskFileState::WaitingFile,
                upload: None,
            }],
        };
        let value = serde_json::to_value(created).unwrap();
        assert_eq!(value["files"][0]["status"], "waiting-file");
        assert!(value["files"][0].get("upload").is_none());

        let detail = TaskDetailResponse {
            task_id: "job_1".into(),
            status: TaskStatus::Running,
            created_at: "2026-01-01T00:00:00Z".into(),
            started_at: None,
            finished_at: None,
            output_formats: vec![TaskOutputFormat::Default],
            progress: TaskProgress::default(),
            files: vec![TaskFileDetail {
                file_id: "file_1".into(),
                file_name: "a.pdf".into(),
                status: TaskFileState::Running,
                bytes: Some(4),
                url: None,
                upload: None,
                parse: None,
                result_url: None,
                error: None,
            }],
            error: None,
        };
        let value = serde_json::to_value(detail).unwrap();
        assert_eq!(value["progress"]["completed"], 0);
        assert_eq!(value["progress"]["failed"], 0);
        assert_eq!(value["progress"]["total"], 0);
        assert_eq!(value["created_at"], "2026-01-01T00:00:00Z");
        assert!(value.get("started_at").is_none());
    }

    #[test]
    fn health_and_error_envelope_are_official_v1() {
        let health = serde_json::to_value(Health::default()).unwrap();
        assert_eq!(health["status"], "ok");
        assert_eq!(health["version"], "4.2.3");
        assert_eq!(
            health["features"]["task_output_formats"],
            json!(["default", "docx", "html", "latex"])
        );
        assert_eq!(health["features"]["sources"], json!(["upload", "url"]));
        assert!(health.get("schema_version").is_none());

        let response = WireError::invalid("Invalid page range.", "page_range").into_response();
        assert_eq!(response.status(), axum::http::StatusCode::BAD_REQUEST);
        let response = WireError::too_large().into_response();
        assert_eq!(response.status(), axum::http::StatusCode::PAYLOAD_TOO_LARGE);

        let envelope = serde_json::to_value(ErrorResponse {
            error: ErrorDetail {
                code: "invalid_request".into(),
                message: "Invalid request".into(),
                param: None,
            },
        })
        .unwrap();
        assert!(envelope["error"].get("param").is_none());
        assert!(envelope["error"].get("type").is_none());
    }

    #[test]
    fn not_found_and_page_range_use_upstream_codes() {
        let task = WireError::task_not_found("job_1");
        assert_eq!(task.status, axum::http::StatusCode::NOT_FOUND);
        assert_eq!(task.code, "task_not_found");
        assert_eq!(task.message, "Task job_1 not found");

        let upload = WireError::upload_not_found();
        assert_eq!(upload.code, "upload_not_found");
        assert_eq!(upload.message, "Unknown file_id");

        let reversed = validate_page_range("5-2").unwrap_err();
        assert_eq!(reversed.code, "page_range_invalid");
        assert_eq!(reversed.param.as_deref(), Some("page_range"));
        assert_eq!(
            reversed.message,
            "Invalid page range '5-2': reversed ranges are not supported"
        );

        let malformed = validate_page_range("1,,2").unwrap_err();
        assert_eq!(malformed.code, "page_range_invalid");
        assert_eq!(
            malformed.message,
            "Invalid page range '1,,2': expected a page, an inclusive range, or 'all'"
        );
        assert!(validate_page_range("1-3,r1").is_ok());
    }
}
