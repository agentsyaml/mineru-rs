use std::process::{Command, Output};

fn cli(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_mineru"))
        .args(args)
        .output()
        .unwrap()
}

#[test]
fn show_and_help_are_local_and_report_pinned_identity() {
    let show = cli(&["models", "show"]);
    assert!(show.status.success());
    let output = String::from_utf8_lossy(&show.stdout);
    assert!(output.contains("opendatalab/MinerU-4_models_onnx"));
    assert!(output.contains("358310b4f64b95f9fefc372ad899356e4111f376"));
    assert!(output.contains("does not implement inference"));

    let help = cli(&["models", "--help"]);
    assert!(help.status.success());
    assert!(String::from_utf8_lossy(&help.stdout).contains("download"));
}

#[test]
fn cache_directory_is_required_and_offline_verify_does_not_download() {
    let missing = cli(&["models", "verify"]);
    assert!(!missing.status.success());
    assert!(String::from_utf8_lossy(&missing.stderr).contains("--cache-dir"));

    let temp = tempfile::tempdir().unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_mineru"))
        .args(["models", "verify", "--cache-dir"])
        .arg(temp.path())
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("cached artifact failed"));
}

#[test]
fn model_help_and_show_do_not_change_legacy_version() {
    let version = cli(&["--version"]);
    assert!(version.status.success());
    assert!(String::from_utf8_lossy(&version.stdout).contains(env!("CARGO_PKG_VERSION")));
    let help = cli(&["models", "download", "--help"]);
    assert!(help.status.success());
    assert!(String::from_utf8_lossy(&help.stdout).contains("--cache-dir"));
}
