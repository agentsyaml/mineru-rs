#![cfg(feature = "mistralrs")]

use std::process::Command;

fn bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_mineru-mistralrs"))
}

#[test]
fn help_and_missing_model_configuration_are_clear() {
    let help = bin().arg("--help").output().unwrap();
    assert!(help.status.success());
    let help = String::from_utf8_lossy(&help.stdout);
    assert!(help.contains("--model-path"), "{help}");
    assert!(help.contains("--allow-download"), "{help}");
    // Default is allow-download=true, visible in help, so the CLI never
    // requires the flag for the common download path.
    assert!(help.contains("[default: true]"), "{help}");
    // A real input file gets past the input check and reaches model config.
    let input = format!("{}/Cargo.toml", env!("CARGO_MANIFEST_DIR"));
    let output = bin()
        .arg(&input)
        .arg("--allow-download=false")
        .env_remove("MINERU_VL_MODEL_DIR")
        .env_remove("MINERU_VL_AUTO_DOWNLOAD")
        .env_remove("MINERU_VL_BACKEND")
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("--model-path is required when --allow-download=false"),
        "{stderr}"
    );
}

#[test]
fn missing_input_fails_before_model_configuration() {
    let output = bin()
        .arg("missing-input.pdf")
        .env_remove("MINERU_VL_MODEL_DIR")
        .env_remove("MINERU_VL_AUTO_DOWNLOAD")
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    // The input error must take priority: model configuration is never reached.
    assert!(stderr.contains("missing-input.pdf"), "{stderr}");
    assert!(!stderr.contains("--model-path"), "{stderr}");
}
