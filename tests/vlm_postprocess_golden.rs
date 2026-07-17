#[test]
fn vlm_golden_cases_are_documented() {
    let cases = include_str!("fixtures/vlm/postprocess_golden.txt");
    assert!(cases.contains("<fcel>"));
    assert!(cases.contains("<eq>x</eq>"));
}
