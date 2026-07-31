#![cfg(feature = "office")]
use std::{
    io::Write,
    process::{Command, Stdio},
};
#[path = "support/office_fixtures.rs"]
mod office_fixtures;
use office_fixtures::{docx, pptx, pptx_two_slides, xlsx};

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
fn assert_pdf(format: &str, bytes: Vec<u8>) {
    let output = run(&[format], &bytes);
    assert!(
        output.status.success(),
        "{format}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stdout.starts_with(b"%PDF-"));
    assert!(output.stderr.len() <= 4096);
    assert!(
        !lopdf::Document::load_mem(&output.stdout)
            .unwrap()
            .get_pages()
            .is_empty()
    );
}

#[test]
#[ignore = "real Office helper process e2e"]
fn helper_converts_self_authored_docx_pptx_and_xlsx() {
    assert_pdf("docx", docx());
    assert_pdf("pptx", pptx());
    assert_pdf("xlsx", xlsx());
    let output = run(&["pptx"], &pptx_two_slides());
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        lopdf::Document::load_mem(&output.stdout)
            .unwrap()
            .get_pages()
            .len(),
        2
    );
}
#[test]
#[ignore = "real Office helper process e2e"]
fn helper_rejects_bad_arguments_and_oversized_input() {
    for args in [&[][..], &["docx", "extra"], &["bad"]] {
        let output = run(args, b"");
        assert!(!output.status.success());
        assert!(output.stdout.is_empty() && output.stderr.len() <= 4096);
    }
    let output = run(&["docx"], &vec![0; CAP + 1]);
    assert!(!output.status.success());
    assert!(output.stdout.is_empty() && output.stderr.len() <= 4096);
}
