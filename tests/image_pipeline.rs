#[test]
fn normalized_boxes_keep_geometry_contract() {
    assert!(mineru::NormalizedBbox::new(0.1, 0.1, 0.9, 0.9).is_ok());
}
