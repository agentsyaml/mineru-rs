use std::process::{Command, Output};

fn cli(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_mineru"))
        .args(args)
        .output()
        .unwrap()
}

#[test]
fn official_help_routes_and_legacy_help_stays_legacy() {
    let top = cli(&["official", "--help"]);
    assert!(top.status.success());
    assert!(String::from_utf8_lossy(&top.stdout).contains("parse"));
    for command in ["parse", "render"] {
        let out = cli(&["official", command, "--help"]);
        assert!(out.status.success());
        assert!(String::from_utf8_lossy(&out.stdout).contains("--python"));
    }
    let legacy = cli(&["--help"]);
    assert!(legacy.status.success());
    let help = String::from_utf8_lossy(&legacy.stdout);
    assert!(help.contains("--backend"));
    assert!(help.contains("--api-url"));
    assert!(!help.contains("Usage: mineru <COMMAND>"));
}

#[test]
fn invalid_render_target_exits_before_upstream() {
    let out = cli(&[
        "official",
        "render",
        "missing.json",
        "--output",
        "out",
        "--target",
        "wrong",
    ]);
    assert!(!out.status.success());
}

#[test]
fn parse_and_render_routes_reach_inputs_without_spawning_upstream() {
    let parse = cli(&["official", "parse", "missing.pdf", "--output", "unused"]);
    assert!(!parse.status.success());
    assert!(String::from_utf8_lossy(&parse.stderr).contains("could not read official input path"));
    let render = cli(&[
        "official",
        "render",
        "missing.json",
        "--output",
        "unused",
        "--target",
        "markdown",
    ]);
    assert!(!render.status.success());
    let stderr = String::from_utf8_lossy(&render.stderr);
    assert!(stderr.contains("No such file") || stderr.contains("not found"));
}
