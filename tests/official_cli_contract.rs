use serde_json::{Value, json};
use std::collections::HashSet;

const ENVS: &[&str] = &[
    "ASCEND_RT_VISIBLE_DEVICES",
    "CUDA_VISIBLE_DEVICES",
    "MINERU_API_ALLOW_PUBLIC_HTTP_CLIENT",
    "MINERU_API_DISABLE_ACCESS_LOG",
    "MINERU_API_ENABLE_FASTAPI_DOCS",
    "MINERU_API_ENABLE_VLM_PRELOAD",
    "MINERU_API_MAX_CONCURRENT_REQUESTS",
    "MINERU_API_OUTPUT_ROOT",
    "MINERU_API_PUBLIC_BIND_EXPOSED",
    "MINERU_API_SHUTDOWN_ON_STDIN_EOF",
    "MINERU_API_TASK_CLEANUP_INTERVAL_SECONDS",
    "MINERU_API_TASK_RETENTION_SECONDS",
    "MINERU_DEVICE_MODE",
    "MINERU_FORMULA_ENABLE",
    "MINERU_HYBRID_BATCH_RATIO",
    "MINERU_INTER_OP_NUM_THREADS",
    "MINERU_INTRA_OP_NUM_THREADS",
    "MINERU_LOCAL_API_LAUNCH_MODE",
    "MINERU_LOCAL_API_STARTUP_TIMEOUT_SECONDS",
    "MINERU_LOG_LEVEL",
    "MINERU_LMDEPLOY_BACKEND",
    "MINERU_LMDEPLOY_DEVICE",
    "MINERU_MODEL_SOURCE",
    "MINERU_OCR_DET_MASK_INLINE_FORMULA_ENABLE",
    "MINERU_PDF_RENDER_THREADS",
    "MINERU_PDF_RENDER_TIMEOUT",
    "MINERU_PROCESSING_WINDOW_SIZE",
    "MINERU_ROUTER_ALLOW_PUBLIC_HTTP_CLIENT",
    "MINERU_ROUTER_ENABLE_VLM_PRELOAD",
    "MINERU_ROUTER_LOCAL_GPUS",
    "MINERU_ROUTER_PUBLIC_BIND_EXPOSED",
    "MINERU_ROUTER_UPSTREAM_URLS_JSON",
    "MINERU_ROUTER_WORKER_ARGS_JSON",
    "MINERU_ROUTER_WORKER_HOST",
    "MINERU_SEAL_OCR_DEBUG",
    "MINERU_SEAL_OCR_DEBUG_DIR",
    "MINERU_TABLE_ENABLE",
    "MINERU_TASK_RESULT_DOWNLOAD_TIMEOUT_SECONDS",
    "MINERU_TASK_RESULT_TIMEOUT_SECONDS",
    "MINERU_TOOLS_CONFIG_JSON",
    "MINERU_VIRTUAL_VRAM_SIZE",
    "MINERU_VLLM_DEVICE",
    "MINERU_VLM_FORMULA_ENABLE",
    "MINERU_VLM_TABLE_ENABLE",
    "OMP_NUM_THREADS",
    "TM_LOG_LEVEL",
    "TOKENIZERS_PARALLELISM",
    "TORCH_CUDNN_V8_API_DISABLED",
    "VLLM_USE_V1",
    "FTLANG_CACHE",
    "MINERU_ENABLE_PIPELINE_INFERENCE_LOCKS",
    "MINERU_FORMULA_CH_SUPPORT",
    "MINERU_TABLE_MERGE_ENABLE",
    "MINERU_GRADIO_DEFAULT_LOCALE",
    "GRADIO_ALLOWED_PATHS",
    "PYTORCH_ENABLE_MPS_FALLBACK",
];

fn contract() -> Value {
    serde_json::from_str(include_str!(
        "fixtures/official/mineru_3.4.4_cli_contract.json"
    ))
    .unwrap()
}
fn row<'a>(env: &'a [Value], name: &str) -> &'a Value {
    env.iter().find(|r| r["name"] == name).unwrap()
}

#[test]
fn official_contract_is_exact_and_structured() {
    let c = contract();
    assert_eq!(
        c["source"]["commit"],
        "0dfc9460cd9ab693b9af60ae3fbffd7bc111b062"
    );
    let flags = c["flags"].as_array().unwrap();
    let flag_names: Vec<_> = flags
        .iter()
        .flat_map(|f| f["names"].as_array().unwrap())
        .map(|name| name.as_str().unwrap())
        .collect();
    let expected = HashSet::from([
        "-v",
        "--version",
        "-p",
        "--path",
        "-o",
        "--output",
        "--api-url",
        "-m",
        "--method",
        "-b",
        "--backend",
        "--effort",
        "-l",
        "--lang",
        "-u",
        "--url",
        "-s",
        "--start",
        "-e",
        "--end",
        "-f",
        "--formula",
        "-t",
        "--table",
        "--image-analysis",
        "--client-side-output-generation",
    ]);
    assert_eq!(flag_names.iter().copied().collect::<HashSet<_>>(), expected);
    assert_eq!(
        flag_names.len(),
        flag_names.iter().collect::<HashSet<_>>().len()
    );
    let fields: Vec<_> = flags.iter().map(|f| f["field"].as_str().unwrap()).collect();
    assert_eq!(fields.len(), fields.iter().collect::<HashSet<_>>().len());
    for flag in flags {
        for key in [
            "names",
            "field",
            "type",
            "default",
            "required",
            "choices",
            "applicability",
            "behavior",
            "source",
        ] {
            assert!(flag.get(key).is_some(), "{} lacks {key}", flag["field"]);
        }
    }
    let actual_flags: Vec<_> = flags.iter().map(|f| json!({
        "names": f["names"], "field": f["field"], "type": f["type"], "default": f["default"],
        "required": f["required"], "choices": f["choices"], "applicability": f["applicability"], "behavior": f["behavior"]
    })).collect();
    assert_eq!(
        actual_flags.as_slice(),
        json!([
            {"names":["-v","--version"],"field":"version","type":"version","default":"package version","required":false,"choices":null,"applicability":"command","behavior":"prints version and exits"},
            {"names":["-p","--path"],"field":"input_path","type":"existing path","default":null,"required":true,"choices":null,"applicability":"command","behavior":"file or one-level directory input"},
            {"names":["-o","--output"],"field":"output_dir","type":"path","default":null,"required":true,"choices":null,"applicability":"command","behavior":"output root"},
            {"names":["--api-url"],"field":"api_url","type":"string","default":null,"required":false,"choices":null,"applicability":"command","behavior":"existing API; absence launches a temporary API"},
            {"names":["-m","--method"],"field":"method","type":"choice","default":"auto","required":false,"choices":["auto","txt","ocr"],"applicability":"pipeline and hybrid","behavior":"PDF parsing method"},
            {"names":["-b","--backend"],"field":"backend","type":"normalized string","default":"hybrid-engine","required":false,"choices":["pipeline","vlm-engine","vlm-http-client","hybrid-engine","hybrid-http-client","vlm-auto-engine","hybrid-auto-engine"],"applicability":"command","behavior":"selects a public backend; legacy aliases normalize to canonical names"},
            {"names":["--effort"],"field":"effort","type":"choice","default":"medium","required":false,"choices":["medium","high"],"applicability":"hybrid","behavior":"hybrid effort"},
            {"names":["-l","--lang"],"field":"lang","type":"normalized string","default":"ch","required":false,"choices":"official OCR language table","applicability":"pipeline","behavior":"OCR language"},
            {"names":["-u","--url"],"field":"server_url","type":"string","default":null,"required":false,"choices":null,"applicability":"vlm-http-client and hybrid-http-client","behavior":"OpenAI-compatible VLM server URL"},
            {"names":["-s","--start"],"field":"start_page_id","type":"integer","default":0,"required":false,"choices":null,"applicability":"PDF","behavior":"zero-based first page"},
            {"names":["-e","--end"],"field":"end_page_id","type":"integer","default":null,"required":false,"choices":null,"applicability":"PDF","behavior":"zero-based final page; None serializes as 99999 for API tasks"},
            {"names":["-f","--formula"],"field":"formula_enable","type":"boolean value","default":true,"required":false,"choices":[true,false],"applicability":"backend-specific","behavior":"enables formula parsing; backend processing applies the value"},
            {"names":["-t","--table"],"field":"table_enable","type":"boolean value","default":true,"required":false,"choices":[true,false],"applicability":"backend-specific","behavior":"enables table parsing; backend processing applies the value"},
            {"names":["--image-analysis"],"field":"image_analysis","type":"boolean value","default":true,"required":false,"choices":[true,false],"applicability":"VLM and hybrid","behavior":"enables image/chart analysis; hybrid medium effort disables it"},
            {"names":["--client-side-output-generation"],"field":"client_side_output_generation","type":"boolean value","default":false,"required":false,"choices":[true,false],"applicability":"API task result handling","behavior":"rebuilds markdown/content lists locally"}
        ])
        .as_array()
        .unwrap()
        .as_slice()
    );

    let env = c["environment"].as_array().unwrap();
    let names: Vec<_> = env.iter().map(|r| r["name"].as_str().unwrap()).collect();
    assert_eq!(env.len(), 56);
    assert_eq!(
        names.iter().copied().collect::<HashSet<_>>(),
        ENVS.iter().copied().collect()
    );
    assert_eq!(names.len(), names.iter().collect::<HashSet<_>>().len());
    for r in env {
        for key in [
            "access",
            "default",
            "type",
            "invalid",
            "scope",
            "precedence",
            "source",
        ] {
            assert!(r.get(key).is_some(), "{} lacks {key}", r["name"]);
        }
    }

    for (name, access, default, source) in [
        (
            "FTLANG_CACHE",
            "both",
            json!("resource cache path"),
            "language.py:5-10",
        ),
        (
            "MINERU_ENABLE_PIPELINE_INFERENCE_LOCKS",
            "read",
            json!(false),
            "model_init.py:27-30",
        ),
        (
            "MINERU_FORMULA_CH_SUPPORT",
            "read",
            json!(false),
            "model_init.py:62-69",
        ),
        (
            "MINERU_TABLE_MERGE_ENABLE",
            "read",
            json!(true),
            "runtime_utils.py:19-26",
        ),
        (
            "MINERU_GRADIO_DEFAULT_LOCALE",
            "read",
            json!("zh"),
            "gradio_app.py:257-265",
        ),
        (
            "GRADIO_ALLOWED_PATHS",
            "read",
            json!("empty comma-list"),
            "gradio_app.py:893-904",
        ),
        (
            "PYTORCH_ENABLE_MPS_FALLBACK",
            "write",
            json!("1"),
            "pipeline_analyze.py:31",
        ),
    ] {
        let r = row(env, name);
        assert_eq!(r["access"], access);
        assert_eq!(r["default"], default);
        assert!(r["source"].as_str().unwrap().contains(source));
    }
    for name in [
        "MINERU_ROUTER_ALLOW_PUBLIC_HTTP_CLIENT",
        "MINERU_ROUTER_ENABLE_VLM_PRELOAD",
        "MINERU_ROUTER_LOCAL_GPUS",
        "MINERU_ROUTER_PUBLIC_BIND_EXPOSED",
        "MINERU_ROUTER_UPSTREAM_URLS_JSON",
        "MINERU_ROUTER_WORKER_ARGS_JSON",
        "MINERU_ROUTER_WORKER_HOST",
        "CUDA_VISIBLE_DEVICES",
        "ASCEND_RT_VISIBLE_DEVICES",
        "MINERU_MODEL_SOURCE",
    ] {
        assert_eq!(row(env, name)["access"], "both", "{name}");
    }
    assert!(
        row(env, "CUDA_VISIBLE_DEVICES")["source"]
            .as_str()
            .unwrap()
            .contains("422-426")
    );
    assert!(
        row(env, "MINERU_MODEL_SOURCE")["source"]
            .as_str()
            .unwrap()
            .contains("models_download.py:64,82-90")
    );
    for name in ["MINERU_VLM_FORMULA_ENABLE", "MINERU_VLM_TABLE_ENABLE"] {
        assert!(
            row(env, name)["source"]
                .as_str()
                .unwrap()
                .contains("common.py:731-749,826-844")
        );
    }
    for name in ["MINERU_INTER_OP_NUM_THREADS", "MINERU_INTRA_OP_NUM_THREADS"] {
        assert_eq!(
            row(env, name)["source"],
            "mineru/utils/os_env_config.py:5-8; mineru/model/table/rec/slanet_plus/table_structure.py:33-34; mineru/model/table/rec/unet_table/table_structure_unet.py:34-35"
        );
    }
    assert_eq!(
        row(env, "FTLANG_CACHE")["precedence"],
        "non-empty existing environment wins; absent or empty is replaced with the resource cache path"
    );
    assert_eq!(
        row(env, "MINERU_LOG_LEVEL")["scope"],
        "CLI/API/Gradio logging"
    );
    assert_eq!(
        row(env, "MINERU_LOG_LEVEL")["source"],
        "mineru/cli/client.py:59; mineru/cli/fast_api.py:74; mineru/cli/gradio_app.py:29"
    );
    assert_eq!(
        row(env, "MINERU_VIRTUAL_VRAM_SIZE")["invalid"],
        "invalid or nonpositive value falls back to auto"
    );
    assert_eq!(
        row(env, "MINERU_VIRTUAL_VRAM_SIZE")["source"],
        "mineru/utils/model_utils.py:218-231"
    );
    assert_eq!(
        row(env, "MINERU_VLM_FORMULA_ENABLE")["default"],
        "VLM: caller formula value; hybrid: true"
    );
    assert_eq!(
        row(env, "MINERU_VLM_FORMULA_ENABLE")["precedence"],
        "VLM writes caller formula value; hybrid forces true before backend reads"
    );
    assert_eq!(
        row(env, "MINERU_VLM_TABLE_ENABLE")["default"],
        "caller table value in VLM and hybrid"
    );
    assert_eq!(
        row(env, "MINERU_VLM_TABLE_ENABLE")["precedence"],
        "VLM and hybrid write caller table value before backend reads"
    );
    let env_flag_invalid = "case-insensitive exact 1|true|yes|on enables; no trimming; absent uses default; every other explicit value is false";
    for name in [
        "MINERU_API_ALLOW_PUBLIC_HTTP_CLIENT",
        "MINERU_API_DISABLE_ACCESS_LOG",
        "MINERU_API_ENABLE_FASTAPI_DOCS",
        "MINERU_API_ENABLE_VLM_PRELOAD",
        "MINERU_API_PUBLIC_BIND_EXPOSED",
        "MINERU_API_SHUTDOWN_ON_STDIN_EOF",
        "MINERU_ROUTER_ALLOW_PUBLIC_HTTP_CLIENT",
        "MINERU_ROUTER_ENABLE_VLM_PRELOAD",
        "MINERU_ROUTER_PUBLIC_BIND_EXPOSED",
    ] {
        assert_eq!(row(env, name)["invalid"], env_flag_invalid, "{name}");
    }
    for name in [
        "MINERU_API_ALLOW_PUBLIC_HTTP_CLIENT",
        "MINERU_API_DISABLE_ACCESS_LOG",
        "MINERU_API_ENABLE_FASTAPI_DOCS",
        "MINERU_API_ENABLE_VLM_PRELOAD",
        "MINERU_API_PUBLIC_BIND_EXPOSED",
        "MINERU_API_SHUTDOWN_ON_STDIN_EOF",
    ] {
        assert!(
            row(env, name)["source"]
                .as_str()
                .unwrap()
                .contains("100-104"),
            "{name}"
        );
    }
    let override_invalid = "case-insensitive exact true enables; no trimming; absent preserves caller boolean; every other explicit value is false";
    for name in [
        "MINERU_FORMULA_ENABLE",
        "MINERU_TABLE_ENABLE",
        "MINERU_OCR_DET_MASK_INLINE_FORMULA_ENABLE",
    ] {
        assert_eq!(row(env, name)["invalid"], override_invalid, "{name}");
        assert_eq!(
            row(env, name)["source"],
            "mineru/utils/config_reader.py:140-155",
            "{name}"
        );
    }
    let router_precedence = "module/settings read environment where applicable; router CLI/default resolved values are forcibly written at router.py:1617-1644, overriding process environment for child/runtime handoff";
    for name in [
        "MINERU_ROUTER_ALLOW_PUBLIC_HTTP_CLIENT",
        "MINERU_ROUTER_ENABLE_VLM_PRELOAD",
        "MINERU_ROUTER_LOCAL_GPUS",
        "MINERU_ROUTER_PUBLIC_BIND_EXPOSED",
        "MINERU_ROUTER_UPSTREAM_URLS_JSON",
        "MINERU_ROUTER_WORKER_ARGS_JSON",
        "MINERU_ROUTER_WORKER_HOST",
    ] {
        assert_eq!(row(env, name)["precedence"], router_precedence, "{name}");
        assert!(
            row(env, name)["source"]
                .as_str()
                .unwrap()
                .contains("1617-1644"),
            "{name}"
        );
    }
    assert_eq!(
        row(env, "MINERU_VLM_TABLE_ENABLE")["scope"],
        "VLM and hybrid result conversion"
    );
    assert!(
        row(env, "MINERU_VLM_TABLE_ENABLE")["source"]
            .as_str()
            .unwrap()
            .contains("hybrid_model_output_to_middle_json.py:266")
    );
    assert_eq!(
        row(env, "MINERU_MODEL_SOURCE")["type"],
        "normalized enum: huggingface|modelscope|local"
    );
    assert_eq!(
        row(env, "MINERU_MODEL_SOURCE")["invalid"],
        "explicit environment auto rejects; unsupported value/type warns then resolves auto and persists resolved source"
    );
    assert_eq!(
        row(env, "MINERU_MODEL_SOURCE")["precedence"],
        "environment > config > internal auto; resolved auto source persists; temporary write/restore applies for download command"
    );
    assert_eq!(
        row(env, "MINERU_SEAL_OCR_DEBUG")["invalid"],
        "case-insensitive exact 1|true|yes|on enables; no trimming; every other value is off"
    );
    assert_eq!(
        row(env, "MINERU_SEAL_OCR_DEBUG")["source"],
        "mineru/model/ocr/pytorch_paddle.py:122-124"
    );
    assert_eq!(
        row(env, "MINERU_HYBRID_BATCH_RATIO")["invalid"],
        "absent or empty uses VRAM heuristic; nonempty integer is returned unchanged including zero/negative; only integer parse failure falls back to VRAM heuristic"
    );
    assert_eq!(
        row(env, "MINERU_HYBRID_BATCH_RATIO")["source"],
        "mineru/backend/hybrid/hybrid_analyze.py:835-869"
    );
    assert_eq!(
        row(env, "MINERU_ENABLE_PIPELINE_INFERENCE_LOCKS")["invalid"],
        "case-insensitive exact true|1|yes enables; no trimming; every other value is false"
    );
    assert_eq!(
        row(env, "MINERU_FORMULA_CH_SUPPORT")["invalid"],
        "case-insensitive exact true|1|yes selects pp_formulanet_plus_m; false|0|no selects unimernet_small; no trimming; all other values warn and select unimernet_small"
    );
    assert_eq!(
        row(env, "MINERU_TABLE_MERGE_ENABLE")["invalid"],
        "case-insensitive exact true|1|yes merges; false|0|no skips; no trimming; all other values warn and skip"
    );
    for name in ["MINERU_VLM_FORMULA_ENABLE", "MINERU_VLM_TABLE_ENABLE"] {
        assert_eq!(
            row(env, name)["invalid"],
            "case-insensitive exact true enables; no trimming; every other value disables",
            "{name}"
        );
    }

    let cli = &c["cli_facts"];
    assert_eq!(cli["parser_context"]["ignore_unknown_options"], true);
    assert_eq!(cli["parser_context"]["allow_extra_args"], true);
    assert_eq!(
        cli["extra_args"]["stripped_options"],
        json!(["--host", "--port"])
    );
    assert_eq!(cli["page_validation"]["negative_start_or_end"], "error");
    assert_eq!(cli["ocr"]["aliases"]["en"], "ch");
    assert_eq!(cli["ocr"]["choices"].as_array().unwrap().len(), 12);
    for (fact, source) in [
        (
            "extra_args",
            "mineru/cli/client.py:1037-1218; mineru/cli/api_client.py:355-366,467-547,658-691",
        ),
        (
            "page_validation",
            "mineru/cli/client.py:927-930,535-541; mineru/utils/pdf_page_id.py:5-10",
        ),
        ("ocr", "mineru/utils/ocr_language.py:3-16,44-53,116-125"),
        (
            "input_discovery",
            "mineru/cli/client.py:535-581; mineru/utils/guess_suffix_or_lang.py:142-201; mineru/cli/common.py:42-47",
        ),
        ("duplicate_stems", "mineru/cli/common.py:89-168"),
        ("output_dirs", "mineru/cli/output_paths.py:5-58"),
    ] {
        assert_eq!(cli[fact]["source"], source, "{fact}");
    }
    assert_eq!(
        cli["input_discovery"]["directory"],
        "one-level sorted regular files"
    );
    assert_eq!(
        cli["input_discovery"]["classification"],
        "OOXML ZIP package inspection for docx/pptx/xlsx, then Magika content detection, then PDF-signature recovery when .pdf is classified ai/html"
    );
    assert_eq!(
        cli["input_discovery"]["page_accounting"],
        "PDF uses selected effective page count; image and Office inputs count as one"
    );
    assert_eq!(
        cli["input_discovery"]["source"],
        "mineru/cli/client.py:535-581; mineru/utils/guess_suffix_or_lang.py:142-201; mineru/cli/common.py:42-47"
    );
    assert_eq!(
        cli["duplicate_stems"]["collision"],
        "smallest collision-free _N suffix"
    );
    assert_eq!(
        cli["output_dirs"]["hybrid"],
        "<output>/<stem>/hybrid_<method>"
    );
    assert!(
        cli["backend_effects"]["source"]
            .as_str()
            .unwrap()
            .contains("common.py")
    );
}

#[test]
fn protocol_is_independently_implementable() {
    let c = contract();
    let p = &c["protocol"];
    assert_eq!(p["protocol_version"], 2);
    assert_eq!(
        p["protocol_version_source"],
        "mineru/cli/api_protocol.py:2-4"
    );
    let endpoints = p["endpoints"].as_array().unwrap();
    let actual: HashSet<_> = endpoints
        .iter()
        .map(|e| (e["method"].as_str().unwrap(), e["path"].as_str().unwrap()))
        .collect();
    assert_eq!(
        actual,
        HashSet::from([
            ("POST", "/file_parse"),
            ("POST", "/tasks"),
            ("GET", "/tasks/{task_id}"),
            ("GET", "/tasks/{task_id}/result"),
            ("GET", "/health")
        ])
    );
    assert_eq!(actual.len(), endpoints.len());
    assert_eq!(p["health"]["required"]["status"], "healthy");
    assert_eq!(p["health"]["required"]["protocol_version"], 2);
    assert_eq!(
        p["health"]["required"]["max_concurrent_requests"],
        "positive integer"
    );
    assert_eq!(p["health"]["required"]["processing_window_size"], "integer");
    assert_eq!(
        p["health"]["processing_window_normalization"],
        "max(1, processing_window_size)"
    );
    assert_eq!(p["multipart"]["required_files_field"], "files");
    assert_eq!(
        p["multipart"]["defaults"],
        json!({
            "lang_list": ["ch"], "backend": "hybrid-engine", "effort": "medium",
            "parse_method": "auto", "formula_enable": true, "table_enable": true,
            "image_analysis": true, "server_url": null, "return_md": true,
            "return_middle_json": false, "return_model_output": false,
            "return_content_list": false, "return_images": false,
            "response_format_zip": false, "return_original_file": false,
            "client_side_output_generation": false, "start_page_id": 0, "end_page_id": 99999
        })
    );
    assert_eq!(
        p["multipart"]["none_mapping"],
        json!({"end_page_id": 99999, "server_url": "omitted"})
    );
    assert_eq!(
        p["multipart"]["return_original_file"],
        "effective only when response_format_zip=true"
    );
    assert_eq!(
        p["multipart"]["client_side_output_generation_mutation"],
        json!({"return_md": false, "return_middle_json": true, "return_model_output": true, "return_content_list": false, "return_images": true})
    );
    assert_eq!(
        p["multipart"]["source"],
        "mineru/cli/api_request.py:92-254; mineru/cli/backend_options.py:11; mineru/cli/api_client.py:805-848"
    );
    assert_eq!(
        p["public_http_client_security"]["reject_when"],
        "publicly bound and not explicitly allowed, with backend ending -http-client or nonblank server_url after strip"
    );
    assert_eq!(p["public_http_client_security"]["status_code"], 400);
    assert_eq!(
        p["public_http_client_security"]["bypass_when"],
        "non-public bind or explicit allow"
    );
    assert_eq!(
        p["public_http_client_security"]["source"],
        "mineru/cli/api_request.py:214-225; mineru/cli/public_http_client_policy.py:27-37"
    );
    assert_eq!(
        p["canonical_client_submission"]["caller_selected"],
        json!([
            "lang",
            "backend",
            "method",
            "formula_enable",
            "table_enable",
            "image_analysis",
            "server_url",
            "start_page_id",
            "end_page_id",
            "effort",
            "client_side_output_generation"
        ])
    );
    assert_eq!(
        p["canonical_client_submission"]["mappings"],
        json!({"lang": "lang_list=[lang]", "method": "parse_method"})
    );
    assert_eq!(
        p["canonical_client_submission"]["always"],
        json!({"return_middle_json": true, "return_model_output": true, "return_images": true, "response_format_zip": true, "return_original_file": true, "client_side_output_generation": "caller-selected bool serialized"})
    );
    assert_eq!(
        p["canonical_client_submission"]["conditional"],
        json!({"return_md": "!client_side_output_generation", "return_content_list": "!client_side_output_generation"})
    );
    assert_eq!(
        p["canonical_client_submission"]["source"],
        "mineru/cli/client.py:667-702; mineru/cli/api_client.py:805-848"
    );
    assert_eq!(
        p["sync_zip"]["headers_only_when_zip"],
        json!([
            "X-MinerU-Task-Id",
            "X-MinerU-Task-Status",
            "X-MinerU-Task-Status-Url",
            "X-MinerU-Task-Result-Url"
        ])
    );
    assert_eq!(p["sync_zip"]["source"], "mineru/cli/fast_api.py:695-722");
    assert_eq!(
        p["task_payload"]["fields"],
        json!([
            "task_id",
            "status",
            "backend",
            "file_names",
            "created_at",
            "started_at",
            "completed_at",
            "error",
            "status_url",
            "result_url"
        ])
    );
    assert_eq!(
        p["task_payload"]["optional"],
        json!({"queued_ahead": "integer when supplied"})
    );
    assert_eq!(
        p["task_payload"]["source"],
        "mineru/cli/fast_api.py:173-196; mineru/cli/api_client.py:900-933"
    );
    assert_eq!(p["async_submission"]["method"], "POST");
    assert_eq!(p["async_submission"]["path"], "/tasks");
    assert_eq!(p["async_submission"]["status_code"], 202);
    assert_eq!(
        p["async_submission"]["response"],
        "status payload plus message=Task submitted successfully"
    );
    assert_eq!(
        p["async_submission"]["client_requires"],
        json!(["string task_id", "string status_url", "string result_url"])
    );
    assert_eq!(
        p["async_submission"]["file_names"],
        "tuple only for list[str], otherwise empty"
    );
    assert_eq!(
        p["async_submission"]["queued_ahead"],
        "non-integer becomes None"
    );
    assert_eq!(
        p["async_submission"]["source"],
        "mineru/cli/fast_api.py:685-692,1276-1293; mineru/cli/api_client.py:864-933"
    );
    assert_eq!(
        p["task_concurrency"]["formula"],
        "min(client, server, task_count)"
    );
    assert_eq!(p["task_concurrency"]["macos_server"], 1);
    assert_eq!(
        p["task_concurrency"]["local"],
        "temporary local API uses server health limit directly"
    );
    assert_eq!(
        p["task_concurrency"]["remote"],
        "explicit API uses min(client limit, server health limit)"
    );
    assert_eq!(
        p["task_concurrency"]["source"],
        "mineru/cli/client.py:950-984; mineru/cli/router.py:807-824"
    );
    assert_eq!(
        p["task_planning"]["pipeline"],
        json!([
            "sort by effective pages descending with original-order ties",
            "oversized document is a singleton",
            "place only in eligible existing bins",
            "choose least total pages then smallest bin index",
            "reindex after packing"
        ])
    );
    assert_eq!(p["task_planning"]["vlm_hybrid"], "one document per task");
    assert_eq!(p["task_planning"]["source"], "mineru/cli/client.py:609-664");
    assert_eq!(
        p["tty_live_renderer"]["enabled_when"],
        "explicit api_url and stderr is a TTY"
    );
    assert_eq!(
        p["tty_live_renderer"]["disabled_when"],
        "no explicit api_url or non-TTY stderr"
    );
    assert_eq!(
        p["tty_live_renderer"]["source"],
        "mineru/cli/client.py:292-301"
    );
    assert_eq!(p["status"]["active"], json!(["pending", "processing"]));
    assert_eq!(p["status"]["success"], "completed");
    assert_eq!(
        p["status"]["failure"],
        "any other returned status immediately errors"
    );
    assert_eq!(
        p["status"]["queued_ahead"],
        "retained only if integer while active; renderer shows ahead only while pending"
    );
    assert_eq!(
        p["status"]["source"],
        "mineru/cli/api_client.py:936-991; mineru/cli/client.py:292-301"
    );
    assert_eq!(
        p["zip_safety"]["reject"],
        json!([
            "absolute paths",
            ".. path components",
            "destination escapes"
        ])
    );
    assert_eq!(
        p["zip_safety"]["source"],
        "mineru/cli/api_client.py:994-1065"
    );
    assert_eq!(
        p["health"]["source"],
        "mineru/cli/api_client.py:729-758; mineru/cli/fast_api.py:1377-1392"
    );
    assert_eq!(p["processing_window"]["default_pages"], 64);
    assert_eq!(
        p["processing_window"]["invalid"],
        "invalid integer falls back; minimum 1"
    );
    assert_eq!(
        p["processing_window"]["operation"],
        "sequential contiguous windows within one document; each window performs one ordered batch extraction operation"
    );
    assert_eq!(
        p["processing_window"]["vlm"],
        "batch_two_step_extract per window"
    );
    assert_eq!(
        p["processing_window"]["hybrid"],
        "medium batch_extract_with_layout or high batch_two_step_extract per window"
    );
    assert_eq!(
        p["processing_window"]["distinct_from"],
        "model batch ratio and API task concurrency"
    );
    assert_eq!(
        p["processing_window"]["source"],
        "mineru/utils/config_reader.py:158-174; mineru/backend/vlm/vlm_analyze.py:446-500,546-599; mineru/backend/hybrid/hybrid_analyze.py:924-1060,1132-1275"
    );
}
