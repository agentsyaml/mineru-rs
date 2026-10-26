use super::*;

#[test]
fn quoted_multiline_table_matches_model_and_middle_shapes() {
    let result = analyze(b"name;value\r\n\"a;b\n<c>\";7\r\n", "csv").unwrap();
    assert!(
        result.middle["pages"][0]["blocks"][0]["content"][0]["content"]
            .as_str()
            .unwrap()
            .contains("a;b<br>&lt;c&gt;")
    );
    assert!(result.markdown.contains("| name | value |"));
    assert_eq!(result.middle["metadata"]["producer"]["name"], "mineru-rs");
    assert_eq!(result.model["schema"], "docvortex.model");
    assert_eq!(result.model["page_index_map"], json!([]));
    assert!(result.model["pages"][0][0]["content"].is_string());
    assert_eq!(
        result.model["pages"][0][0]["content"],
        result.middle["pages"][0]["blocks"][0]["content"][0]["content"]
    );
    assert_eq!(result.middle["pages"][0]["blocks"][0]["index"], 0);
    assert_eq!(
        result.middle["pages"][0]["blocks"][0]["content"][0]["index"],
        0
    );
    assert_eq!(
        result.structured["pages"][0]["blocks"][0]["captions"],
        json!([])
    );
    assert!(result.structured.get("schema").is_none());
}

#[test]
fn pinned_csv_fixture_produces_raw_model_html_and_postprocessed_middle() {
    let output = analyze(
        include_bytes!("../../../tests/fixtures/legacy/minimal.csv"),
        "csv",
    )
    .unwrap();
    let model_block = &output.model["pages"][0][0];
    let html = model_block["content"].as_str().unwrap();
    assert_eq!(model_block["type"], "table");
    assert_eq!(output.model["page_index_map"], json!([]));
    assert!(output.middle["is_full_document"].as_bool().unwrap());
    assert!(html.starts_with("<table>\n  <tr>\n    <th>a</th>"));
    assert_eq!(
        output.middle["pages"][0]["blocks"][0]["content"][0]["type"],
        "table_body"
    );
    assert_eq!(output.middle["pages"][0]["blocks"][0]["index"], 0);
    assert_eq!(
        output.middle["pages"][0]["blocks"][0]["content"][0]["index"],
        0
    );
    assert_eq!(
        output.middle["pages"][0]["blocks"][0]["content"][0]["content"],
        html
    );
}

#[test]
fn delimiter_sniff_skips_single_column_modes_and_samples_long_files() {
    let ragged = analyze(b"a,b\n1,2,3\n4,5,6", "csv").unwrap();
    let html = ragged.middle["pages"][0]["blocks"][0]["content"][0]["content"]
        .as_str()
        .unwrap();
    assert!(html.contains("<td>1</td>") && html.contains("<td>3</td>"));
    let long = (0..24)
        .map(|n| format!("{n};{}", n + 1))
        .collect::<Vec<_>>()
        .join("\n");
    let output = analyze(long.as_bytes(), "csv").unwrap();
    assert!(
        output.middle["pages"][0]["blocks"][0]["content"][0]["content"]
            .as_str()
            .unwrap()
            .contains("<td>24</td>")
    );
}

#[test]
fn quotes_inside_unquoted_fields_are_data_but_closed_quotes_remain_strict() {
    let result = analyze(b"name,value\nfoo\"bar,2", "csv").unwrap();
    let html = result.model["pages"][0][0]["content"].as_str().unwrap();
    assert!(html.contains("foo&quot;bar"));
    assert_eq!(
        analyze(b"name,value\n\"closed\"junk,2", "csv")
            .unwrap_err()
            .kind,
        OfficialErrorKind::InvalidInput
    );
    assert_eq!(
        analyze(b"name,value\n\"unfinished,2", "csv")
            .unwrap_err()
            .kind,
        OfficialErrorKind::InvalidInput
    );
}

#[test]
fn c1_controls_are_preserved_while_disallowed_c0_controls_are_replaced() {
    let input = "head\n\"a\u{85}b\u{9f}c\0d\u{000b}e\"";
    let result = analyze(input.as_bytes(), "csv").unwrap();
    let html = result.model["pages"][0][0]["content"].as_str().unwrap();
    let expected = "a\u{85}b\u{9f}c\u{fffd}d\u{fffd}e";
    assert!(html.contains(expected));
    assert!(result.markdown.contains(expected));
    assert!(
        result.structured["pages"][0]["blocks"][0]["content"]
            .as_str()
            .unwrap()
            .contains(expected)
    );
}

#[test]
fn unicode_directive_numeric_data_controls_and_empty_tables_are_handled() {
    let (text, delimiter) = directive("€é\nx,y");
    assert_eq!(text, "€é\nx,y");
    assert_eq!(delimiter, None);
    let numeric = analyze(b"1,2\n3,4", "csv").unwrap();
    let html = numeric.middle["pages"][0]["blocks"][0]["content"][0]["content"]
        .as_str()
        .unwrap();
    assert!(html.contains("<td>1</td>") && !html.contains("<th>"));
    let control = analyze(b"head\n\"a\0b\r\nc\"", "csv").unwrap();
    assert!(control.markdown.contains("a\u{fffd}b<br>c"));
    assert!(!control.markdown.contains("&lt;br&gt;"));
    let whitespace = analyze(b"head\n\"  a\t b\x0c c\x0b d  \"", "csv").unwrap();
    assert!(whitespace.markdown.contains("a b\u{fffd} c\u{fffd} d"));
    let empty = analyze(b"", "csv").unwrap();
    assert_eq!(empty.middle["pages"][0]["blocks"], json!([]));
    assert_eq!(empty.structured["pages"][0]["blocks"], json!([]));
}

#[test]
fn header_inference_uses_unicode_folding_and_date_shapes() {
    for input in ["ß,SS\n1,2", "ﬀ,ff\n1,2"] {
        let output = analyze(input.as_bytes(), "csv").unwrap();
        assert!(
            !output.model["pages"][0][0]["content"]
                .as_str()
                .unwrap()
                .contains("<th>")
        );
    }
    let output = analyze(b"x,amount,match\n1234-t,1,match\n5678-t,2,match", "csv").unwrap();
    let html = output.model["pages"][0][0]["content"].as_str().unwrap();
    assert!(!html.contains("<th>"));

    let output = analyze(b"x,amount,match\n1/2/3,1,match\n4/5/6,2,match", "csv").unwrap();
    let html = output.model["pages"][0][0]["content"].as_str().unwrap();
    assert!(html.contains("<th>x</th>"));
}

#[test]
fn numeric_inference_accepts_python_unicode_decimal_float_tokens() {
    for (first, second) in [("١2", "٣4"), ("١2e2", "٣4e2"), ("١2%", "٣4%")] {
        let input = format!("x,amount,match\n{first},1,match\n{second},2,match");
        let output = analyze(input.as_bytes(), "csv").unwrap();
        let html = output.model["pages"][0][0]["content"].as_str().unwrap();
        assert!(html.contains("<th>x</th>"), "{input}");
    }

    for (first, second) in [("١x2", "٣x4"), ("١٢", "٣٤")] {
        let input = format!("x,amount,match\n{first},1,match\n{second},2,match");
        let output = analyze(input.as_bytes(), "csv").unwrap();
        let html = output.model["pages"][0][0]["content"].as_str().unwrap();
        assert!(!html.contains("<th>"), "{input}");
    }
}

#[test]
fn restore_requires_matching_nonnegative_table_indices() {
    let result = analyze(b"head\nvalue", "csv").unwrap();
    let valid = result.middle;
    let cancelled = AtomicBool::new(false);
    for (path, value) in [
        ("/pages/0/blocks/0/index", None),
        ("/pages/0/blocks/0/content/0/index", None),
        ("/pages/0/blocks/0/index", Some(json!(-1))),
        ("/pages/0/blocks/0/content/0/index", Some(json!(1))),
    ] {
        let mut invalid = valid.clone();
        if let Some(value) = value {
            invalid.pointer_mut(path).unwrap().clone_from(&value);
        } else {
            let parent = path.rsplit_once('/').unwrap().0;
            let key = path.rsplit('/').next().unwrap();
            invalid
                .pointer_mut(parent)
                .unwrap()
                .as_object_mut()
                .unwrap()
                .remove(key);
        }
        assert_eq!(
            csv_restore::restore(invalid, Default::default(), &cancelled)
                .unwrap_err()
                .kind,
            OfficialErrorKind::InvalidInput
        );
    }
}

#[test]
fn output_expansion_budget_and_invalid_encodings_fail_explicitly() {
    assert_eq!(
        analyze_with_cancel(&[b'<'; 128], "csv", None, 1_000)
            .unwrap_err()
            .kind,
        OfficialErrorKind::LimitExceeded
    );
    assert_eq!(
        analyze(b"a,\"unfinished", "csv").unwrap_err().kind,
        OfficialErrorKind::InvalidInput
    );
    assert_eq!(
        analyze(&[0xff, 0xfe, b'a', 0], "csv").unwrap_err().kind,
        OfficialErrorKind::UnsupportedOperation
    );
    assert_eq!(
        analyze(b"left\tright\n1\t2", "tsv").unwrap().middle["pages"][0]["blocks"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
}

#[tokio::test]
async fn dropping_restore_future_releases_its_blocking_worker() {
    use std::sync::{
        Arc, Barrier,
        atomic::{AtomicBool, Ordering},
    };

    let started = Arc::new(Barrier::new(2));
    let stopped = Arc::new(Barrier::new(2));
    let started_worker = started.clone();
    let stopped_worker = stopped.clone();
    let task = tokio::spawn(async move {
        let cancelled = Arc::new(AtomicBool::new(false));
        let _guard = RestoreCancelGuard(Some(cancelled.clone()));
        let worker_cancelled = cancelled.clone();
        tokio::task::spawn_blocking(move || {
            started_worker.wait();
            while !worker_cancelled.load(Ordering::Relaxed) {
                std::thread::yield_now();
            }
            stopped_worker.wait();
        })
        .await
        .unwrap();
    });
    tokio::task::spawn_blocking(move || started.wait())
        .await
        .unwrap();
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    tokio::time::timeout(
        std::time::Duration::from_secs(2),
        tokio::task::spawn_blocking(move || stopped.wait()),
    )
    .await
    .expect("dropped restore future did not cancel blocking work")
    .unwrap();
}
