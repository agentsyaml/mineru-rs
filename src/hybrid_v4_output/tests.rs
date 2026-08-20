use super::*;

fn bundle(root: &Path, middle: &str) -> PathBuf {
    let bundle = root.join("bundle");
    std::fs::create_dir(&bundle).unwrap();
    std::fs::write(bundle.join("markdown.md"), "# document\n").unwrap();
    std::fs::write(bundle.join("middle_json.json"), middle).unwrap();
    std::fs::write(bundle.join("content_list.json"), "[]").unwrap();
    std::fs::write(bundle.join("structured_content.json"), "{}").unwrap();
    bundle
}

const VALID_MIDDLE: &str = r#"{"schema_version":"1.0","pages":[{}],"_backend":"hybrid"}"#;

#[test]
fn accepts_v4_shape_and_replaces_atomically() {
    let temp = tempfile::tempdir().unwrap();
    let first = bundle(temp.path(), VALID_MIDDLE);
    std::fs::create_dir(first.join("images")).unwrap();
    std::fs::write(first.join("images/chart.png"), b"png").unwrap();
    std::fs::write(first.join("model_output.json"), "[{}]").unwrap();
    validate_and_publish(&first, temp.path(), "document", 1024).unwrap();
    assert_eq!(
        std::fs::read_to_string(temp.path().join("document/hybrid-v4/markdown.md")).unwrap(),
        "# document\n"
    );
    assert_eq!(
        std::fs::read(temp.path().join("document/hybrid-v4/images/chart.png")).unwrap(),
        b"png"
    );
    assert!(
        temp.path()
            .join("document/hybrid-v4/model_output.json")
            .is_file()
    );
    let second_root = tempfile::tempdir().unwrap();
    let second = bundle(second_root.path(), VALID_MIDDLE);
    std::fs::write(second.join("markdown.md"), "# replacement\n").unwrap();
    validate_and_publish(&second, temp.path(), "document", 1024).unwrap();
    assert_eq!(
        std::fs::read_to_string(temp.path().join("document/hybrid-v4/markdown.md")).unwrap(),
        "# replacement\n"
    );
}

#[test]
fn rejects_schema_backend_empty_pages_unknown_and_cap() {
    for middle in [
        r#"{"schema_version":"0.9","pages":[{}],"_backend":"hybrid"}"#,
        r#"{"schema_version":"1.0","pages":[{}],"_backend":"vlm"}"#,
        r#"{"schema_version":"1.0","pages":[],"_backend":"hybrid"}"#,
    ] {
        let temp = tempfile::tempdir().unwrap();
        let input = bundle(temp.path(), middle);
        assert!(validate_bundle(&input, 1024).is_err(), "{middle}");
    }
    let temp = tempfile::tempdir().unwrap();
    let input = bundle(temp.path(), VALID_MIDDLE);
    std::fs::write(input.join("unknown.txt"), b"x").unwrap();
    assert!(validate_bundle(&input, 1024).is_err());
    let cap_root = tempfile::tempdir().unwrap();
    let cap_input = bundle(cap_root.path(), VALID_MIDDLE);
    let total: u64 = std::fs::read_dir(&cap_input)
        .unwrap()
        .map(|entry| entry.unwrap().metadata().unwrap().len())
        .sum();
    assert!(validate_bundle(&cap_input, total - 1).is_err());
}

#[test]
fn rejects_invalid_text_and_json_without_publishing() {
    let cases = [
        ("markdown.md", vec![0xff, 0xfe]),
        ("content_list.json", b"{".to_vec()),
        ("structured_content.json", b"{".to_vec()),
        ("model_output.json", b"{".to_vec()),
    ];
    for (name, contents) in cases {
        let temp = tempfile::tempdir().unwrap();
        let input = bundle(temp.path(), VALID_MIDDLE);
        std::fs::write(input.join(name), contents).unwrap();
        assert!(
            validate_and_publish(&input, temp.path(), "document", 1024).is_err(),
            "{name}"
        );
        assert!(!temp.path().join("document/hybrid-v4").exists(), "{name}");
    }
}

#[test]
fn validation_failure_preserves_existing_output() {
    let temp = tempfile::tempdir().unwrap();
    let old = bundle(temp.path(), VALID_MIDDLE);
    validate_and_publish(&old, temp.path(), "document", 1024).unwrap();

    let replacement_root = tempfile::tempdir().unwrap();
    let replacement = bundle(
        replacement_root.path(),
        r#"{"schema_version":"0.9","pages":[{}],"_backend":"hybrid"}"#,
    );
    assert!(validate_and_publish(&replacement, temp.path(), "document", 1024).is_err());
    assert_eq!(
        std::fs::read_to_string(temp.path().join("document/hybrid-v4/markdown.md")).unwrap(),
        "# document\n"
    );
}

#[test]
fn rejects_unsafe_portable_names() {
    assert!(!portable_name("../escape"));
    assert!(!portable_name("name\\escape"));
    assert!(!portable_name("CON"));
}

#[test]
fn rejects_unsafe_image_entries() {
    let temp = tempfile::tempdir().unwrap();
    let input = bundle(temp.path(), VALID_MIDDLE);
    std::fs::create_dir(input.join("images")).unwrap();
    std::fs::write(input.join("images/name\\escape"), b"x").unwrap();
    assert!(validate_bundle(&input, 1024).is_err());
}

#[cfg(unix)]
#[test]
fn opened_bundle_handles_survive_a_symlink_swap() {
    use std::os::unix::fs::symlink;

    let temp = tempfile::tempdir().unwrap();
    let input = bundle(temp.path(), VALID_MIDDLE);
    let files = validate_bundle(&input, 1024).unwrap();
    let markdown = input.join("markdown.md");
    let original = input.join("markdown.original");
    let outside = temp.path().join("outside");
    std::fs::rename(&markdown, &original).unwrap();
    std::fs::write(&outside, b"attacker content\n").unwrap();
    symlink(&outside, &markdown).unwrap();

    let stage_root_path = temp.path().join("stage-root");
    let stage_root = crate::official_output::open_or_create_root(&stage_root_path).unwrap();
    stage_root.create_dir("stage").unwrap();
    let stage = crate::official_output::open_child_nofollow(stage_root, "stage").unwrap();
    copy_bundle(files, &stage, 1024).unwrap();
    assert_eq!(
        std::fs::read_to_string(stage_root_path.join("stage/markdown.md")).unwrap(),
        "# document\n"
    );
}

#[cfg(unix)]
#[test]
fn rejects_image_symlinks_without_touching_output() {
    use std::os::unix::fs::symlink;
    let temp = tempfile::tempdir().unwrap();
    let input = bundle(temp.path(), VALID_MIDDLE);
    std::fs::create_dir(input.join("images")).unwrap();
    let outside = temp.path().join("outside");
    std::fs::write(&outside, b"secret").unwrap();
    symlink(&outside, input.join("images/asset.png")).unwrap();
    assert!(validate_and_publish(&input, temp.path(), "document", 1024).is_err());
    assert!(!temp.path().join("document/hybrid-v4").exists());
}
