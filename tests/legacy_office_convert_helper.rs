#![cfg(feature = "legacy-office")]
//! Runs the `mineru-office-convert` helper against the hand-constructed legacy fixtures.
use std::{
    io::Write,
    process::{Command, Stdio},
    time::{Duration, Instant},
};
#[path = "support/legacy_fixtures.rs"]
mod legacy_fixtures;
use legacy_fixtures::{all, doc, rtf};

const CAP: usize = 32 * 1024 * 1024;

fn run(args: &[&str], input: &[u8]) -> std::process::Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_mineru-office-convert"))
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(input).unwrap();
    child.wait_with_output().unwrap()
}

#[test]
fn every_legacy_format_converts_to_clean_utf8_markdown() {
    for fixture in all() {
        let output = run(&[fixture.kind], &fixture.bytes);
        assert!(
            output.status.success(),
            "{}: {}",
            fixture.kind,
            String::from_utf8_lossy(&output.stderr)
        );
        let markdown =
            String::from_utf8(output.stdout.clone()).expect("markdown must be valid UTF-8");
        assert!(
            !markdown.contains('\u{fffd}'),
            "{}: replacement character leaked into output: {markdown:?}",
            fixture.kind
        );
        for expected in fixture.expected {
            assert!(
                markdown.contains(expected),
                "{}: missing {expected:?} in {markdown:?}",
                fixture.kind
            );
        }
        assert!(output.stderr.len() <= 4096, "{}", fixture.kind);
    }
}

#[test]
fn legacy_content_cross_validation_rejects_mismatches() {
    for (requested, bytes) in [("doc", rtf()), ("rtf", doc())] {
        let started = Instant::now();
        let output = run(&[requested], &bytes);
        assert!(started.elapsed() < Duration::from_secs(10));
        assert!(!output.status.success());
        assert!(output.stdout.is_empty() && output.stderr.len() <= 4096);
        assert_eq!(
            output.stderr,
            b"input format does not match requested format\n"
        );
    }
}

#[test]
fn bom_prefixed_rtf_converts() {
    // LibreOffice and some Word exports write a UTF-8 BOM before `{\rtf` for non-ASCII text;
    // the helper must strip it before detection and parsing.
    let mut bytes = b"\xef\xbb\xbf".to_vec();
    bytes.extend_from_slice(&rtf());
    let output = run(&["rtf"], &bytes);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let markdown = String::from_utf8(output.stdout).unwrap();
    assert!(markdown.contains("Legacy RTF fixture"), "{markdown:?}");
    assert!(!markdown.contains('\u{fffd}'), "{markdown:?}");
    assert!(output.stderr.len() <= 4096);
}

#[test]
fn csv_has_no_signature_and_is_accepted_as_declared() {
    let output = run(&["csv"], b"x,y\n1,2\n");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let markdown = String::from_utf8(output.stdout).unwrap();
    for expected in ["x", "y", "1", "2"] {
        assert!(
            markdown.contains(expected),
            "missing {expected:?} in {markdown:?}"
        );
    }
}

#[test]
fn helper_argument_and_input_errors_are_bounded() {
    for (args, expected) in [
        (
            &[][..],
            "usage: mineru-office-convert <docx|pptx|xlsx|doc|ppt|xls|odt|rtf|epub|ods|odp|csv>\n",
        ),
        (&["bad"][..], "invalid format\n"),
    ] {
        let output = run(args, b"");
        assert!(!output.status.success());
        assert!(output.stdout.is_empty() && output.stderr.len() <= 4096);
        assert_eq!(output.stderr, expected.as_bytes());
    }
    let output = run(&["csv"], &vec![0; CAP + 1]);
    assert!(!output.status.success());
    assert!(output.stdout.is_empty() && output.stderr.len() <= 4096);
    assert_eq!(output.stderr, b"input too large\n");
}
