use super::*;
use std::path::{Path, PathBuf};

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

fn stage_validation(bundle: &Path, byte_cap: u64) -> Result<(), String> {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join("stage")).unwrap();
    let directory = crate::official_output::open_or_create_root(root.path()).unwrap();
    let stage = crate::official_output::open_child_nofollow(directory, "stage").unwrap();
    copy_bundle(bundle, &stage, byte_cap).and_then(|()| validate_staged(&stage, byte_cap))
}

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
        assert!(stage_validation(&input, 1024).is_err(), "{middle}");
    }
    let temp = tempfile::tempdir().unwrap();
    let input = bundle(temp.path(), VALID_MIDDLE);
    std::fs::write(input.join("unknown.txt"), b"x").unwrap();
    assert!(stage_validation(&input, 1024).is_err());
    let cap_root = tempfile::tempdir().unwrap();
    let cap_input = bundle(cap_root.path(), VALID_MIDDLE);
    let total: u64 = std::fs::read_dir(&cap_input)
        .unwrap()
        .map(|entry| entry.unwrap().metadata().unwrap().len())
        .sum();
    assert!(stage_validation(&cap_input, total - 1).is_err());
}

#[test]
fn rejects_entry_depth_component_and_path_caps() {
    let many_root = tempfile::tempdir().unwrap();
    let many = bundle(many_root.path(), VALID_MIDDLE);
    let images = many.join("images");
    std::fs::create_dir(&images).unwrap();
    for index in 0..(MAX_ENTRIES as usize - 4) {
        std::fs::write(images.join(format!("empty-{index}")), []).unwrap();
    }
    assert!(validate_and_publish(&many, many_root.path(), "document", u64::MAX).is_err());

    let deep_root = tempfile::tempdir().unwrap();
    let deep = bundle(deep_root.path(), VALID_MIDDLE);
    let mut current = deep.join("images");
    std::fs::create_dir(&current).unwrap();
    for index in 0..31 {
        current = current.join(format!("d{index}"));
        std::fs::create_dir(&current).unwrap();
    }
    std::fs::write(current.join("leaf.bin"), []).unwrap();
    assert!(validate_and_publish(&deep, deep_root.path(), "document", u64::MAX).is_err());

    let component = "x".repeat(usize::try_from(MAX_COMPONENT_BYTES).unwrap() + 1);
    assert!(RelativePath::root().child(&component).is_err());
    let component = "x".repeat(usize::try_from(MAX_COMPONENT_BYTES).unwrap());
    let mut path = RelativePath::root();
    for _ in 0..16 {
        path = path.child(&component).unwrap();
    }
    assert!(path.child("leaf").is_err());

    let mut budget = TreeState::default();
    budget.name_bytes = MAX_NAME_BUDGET;
    assert!(budget.admit(&RelativePath::root(), "x").is_err());
}

#[test]
fn rejects_oversized_text_and_json_before_dom_parsing() {
    for name in ["markdown.md", "content_list.json"] {
        let temp = tempfile::tempdir().unwrap();
        let input = bundle(temp.path(), VALID_MIDDLE);
        let file = std::fs::OpenOptions::new()
            .write(true)
            .open(input.join(name))
            .unwrap();
        file.set_len(MAX_RESIDENT_BYTES.checked_add(1).unwrap())
            .unwrap();
        assert!(
            validate_and_publish(
                &input,
                temp.path(),
                "document",
                MAX_RESIDENT_BYTES.checked_add(1024).unwrap()
            )
            .is_err(),
            "{name}"
        );
        assert!(!temp.path().join("document/hybrid-v4").exists());
    }
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
fn publication_failure_rolls_back_the_previous_output() {
    let temp = tempfile::tempdir().unwrap();
    let input = bundle(temp.path(), VALID_MIDDLE);
    validate_and_publish(&input, temp.path(), "document", 1024).unwrap();

    let root = crate::official_output::open_or_create_root(temp.path()).unwrap();
    let document = crate::official_output::open_child_nofollow(root, "document").unwrap();
    let (transaction_name, transaction, stage) = create_transaction(&document).unwrap();
    drop(stage);
    transaction.remove_dir("stage").unwrap();
    assert!(
        publish_transaction_inner(&document, &transaction, "stage", BUNDLE_NAME, false).is_err()
    );
    assert_eq!(
        std::fs::read_to_string(temp.path().join("document/hybrid-v4/markdown.md")).unwrap(),
        "# document\n"
    );
    cleanup_transaction(&document, &transaction_name, transaction).unwrap();
}

#[test]
fn rollback_failure_preserves_transaction_and_backup() {
    let temp = tempfile::tempdir().unwrap();
    let input = bundle(temp.path(), VALID_MIDDLE);
    validate_and_publish(&input, temp.path(), "document", 1024).unwrap();

    let root = crate::official_output::open_or_create_root(temp.path()).unwrap();
    let document = crate::official_output::open_child_nofollow(root, "document").unwrap();
    let (transaction_name, transaction, stage) = create_transaction(&document).unwrap();
    drop(stage);
    transaction.remove_dir("stage").unwrap();
    let transaction_path = temp.path().join("document").join(&transaction_name);
    let error = finish_transaction(
        &document,
        &transaction_name,
        transaction,
        &transaction_path,
        true,
    )
    .unwrap_err();

    assert!(error.contains("preserved"), "{error}");
    assert!(
        error.contains("restoring previous output failed"),
        "{error}"
    );
    assert_eq!(
        std::fs::read_to_string(transaction_path.join("backup/markdown.md")).unwrap(),
        "# document\n"
    );
    assert!(!transaction_path.join("stage").exists());
    assert!(temp.path().join("document/hybrid-v4").is_file());
    std::fs::remove_file(temp.path().join("document/hybrid-v4")).unwrap();
    std::fs::remove_dir_all(transaction_path).unwrap();
}

#[test]
fn rejects_unsafe_portable_names() {
    assert!(!portable_name("../escape"));
    assert!(!portable_name("name\\escape"));
    assert!(!portable_name("bad:name"));
    assert!(!portable_name("CON"));
}

#[test]
fn rejects_portable_case_collisions() {
    let mut state = TreeState::default();
    let root = RelativePath::root();
    state.admit(&root, "asset.png").unwrap();
    assert!(state.admit(&root, "ASSET.PNG").is_err());
}

#[test]
fn rejects_unsafe_image_entries() {
    let temp = tempfile::tempdir().unwrap();
    let input = bundle(temp.path(), VALID_MIDDLE);
    std::fs::create_dir(input.join("images")).unwrap();
    std::fs::write(input.join("images/name\\escape"), b"x").unwrap();
    assert!(stage_validation(&input, 1024).is_err());
}

#[test]
fn staged_snapshot_survives_same_size_source_mutation() {
    let temp = tempfile::tempdir().unwrap();
    let input = bundle(temp.path(), VALID_MIDDLE);
    let stage_root_path = temp.path().join("stage-root");
    let stage_root = crate::official_output::open_or_create_root(&stage_root_path).unwrap();
    stage_root.create_dir("stage").unwrap();
    let stage = crate::official_output::open_child_nofollow(stage_root, "stage").unwrap();
    copy_bundle(&input, &stage, 1024).unwrap();
    std::fs::write(input.join("markdown.md"), b"# changed\n").unwrap();
    validate_staged(&stage, 1024).unwrap();
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
