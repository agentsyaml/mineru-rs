#[test]
fn layout_fixture_is_present_for_the_crate_private_parser() {
    assert!(include_str!("fixtures/vlm/layout.txt").contains("<|box_start|>"));
}
