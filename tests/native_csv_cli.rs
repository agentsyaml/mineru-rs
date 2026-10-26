use std::{fs, process::Command};

#[test]
fn csv_cli_parse_and_render_work_with_unusable_python() {
    let temp = tempfile::tempdir().unwrap();
    let input = temp.path().join("scores.csv");
    let output = temp.path().join("parsed");
    let python = temp.path().join("missing-python");
    fs::write(&input, "name,score\nAda,99\n").unwrap();
    let parsed = Command::new(env!("CARGO_BIN_EXE_mineru"))
        .args(["official", "parse"])
        .arg(&input)
        .args(["--tier", "flash", "-o"])
        .arg(&output)
        .arg("--python")
        .arg(&python)
        .output()
        .unwrap();
    assert!(
        parsed.status.success(),
        "{}",
        String::from_utf8_lossy(&parsed.stderr)
    );
    let bundle = output.join("scores/hybrid-v4");
    for name in [
        "markdown.md",
        "model_output.json",
        "middle_json.json",
        "structured_content.json",
    ] {
        assert!(bundle.join(name).is_file(), "missing {name}");
    }
    assert!(
        fs::read_to_string(bundle.join("markdown.md"))
            .unwrap()
            .contains("Ada")
    );

    let rendered = temp.path().join("restored.md");
    let restore = Command::new(env!("CARGO_BIN_EXE_mineru"))
        .args(["official", "render"])
        .arg(bundle.join("middle_json.json"))
        .args(["--target", "markdown", "--output"])
        .arg(&rendered)
        .arg("--python")
        .arg(&python)
        .output()
        .unwrap();
    assert!(
        restore.status.success(),
        "{}",
        String::from_utf8_lossy(&restore.stderr)
    );
    assert!(fs::read_to_string(rendered).unwrap().contains("Ada"));
}
