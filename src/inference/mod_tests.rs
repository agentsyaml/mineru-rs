use super::*;
#[test]
fn preprocessing_is_rgb_chw_and_normalized() {
    let image =
        DynamicImage::ImageRgb8(image::RgbImage::from_pixel(2, 1, image::Rgb([255, 0, 128])));
    let (tensor, scale) = preprocess(&image).unwrap();
    assert_eq!(tensor.len(), 3 * 800 * 800);
    assert_eq!(scale, (800.0, 400.0));
    assert!(tensor[0] > 0.99 && tensor[800 * 800] < 0.01);
}

#[test]
fn uniform_800x800_images_survive_preprocess_exactly() {
    for color in [0u8, 255] {
        let image = DynamicImage::ImageRgb8(image::RgbImage::from_pixel(
            800,
            800,
            image::Rgb([color; 3]),
        ));
        let (tensor, _) = preprocess(&image).unwrap();
        let expected = f32::from(color) * (1.0f32 / 255.0);
        assert!(tensor.iter().all(|&v| v == expected));
    }
}

#[test]
fn normalization_matches_official_reciprocal_multiplication() {
    // Upstream multiplies by the f32 reciprocal, which differs from division
    // for 126 of the 256 byte values; assert exact bits, not tolerance.
    let image = DynamicImage::ImageRgb8(image::RgbImage::from_pixel(
        800,
        800,
        image::Rgb([3, 6, 128]),
    ));
    let (tensor, _) = preprocess(&image).unwrap();
    let plane = 800 * 800;
    for (channel, byte) in [3u8, 6, 128].into_iter().enumerate() {
        let expected = f32::from(byte) * (1.0f32 / 255.0);
        assert_eq!(
            tensor[channel * plane].to_bits(),
            expected.to_bits(),
            "channel {channel} byte {byte}"
        );
    }
    assert_ne!(
        (3u8 as f32 / 255.0).to_bits(),
        (3u8 as f32 * (1.0f32 / 255.0)).to_bits()
    );
}

#[test]
fn preprocess_scale_is_800_over_source_dimensions() {
    let image = DynamicImage::ImageRgb8(image::RgbImage::from_pixel(640, 320, image::Rgb([10; 3])));
    let (_, scale) = preprocess(&image).unwrap();
    assert_eq!(scale, (800.0 / 320.0, 800.0 / 640.0));
}

fn tensor_input(ty: ort::tensor::TensorElementType, dims: &[i64]) -> ort::value::ValueType {
    ort::value::ValueType::Tensor {
        ty,
        shape: ort::tensor::Shape::new(dims.to_vec()),
        dimension_symbols: ort::tensor::SymbolicDimensions::new(Vec::<String>::new()),
    }
}

#[test]
fn validate_input_accepts_only_float32_declared_shapes() {
    assert!(
        validate_input(
            "image",
            &tensor_input(ort::tensor::TensorElementType::Float32, &[1, 3, 800, 800])
        )
        .is_ok()
    );
    assert!(
        validate_input(
            "scale_factor",
            &tensor_input(ort::tensor::TensorElementType::Float32, &[1, 2])
        )
        .is_ok()
    );
    // The pinned model declares static dims; dynamic (-1) is rejected too.
    assert!(
        validate_input(
            "image",
            &tensor_input(ort::tensor::TensorElementType::Float32, &[-1, 3, 800, 800])
        )
        .is_err()
    );
    for bad in [
        ort::tensor::TensorElementType::Float16,
        ort::tensor::TensorElementType::Bfloat16,
        ort::tensor::TensorElementType::Int32,
    ] {
        assert!(validate_input("image", &tensor_input(bad, &[1, 3, 800, 800])).is_err());
        assert!(validate_input("scale_factor", &tensor_input(bad, &[1, 2])).is_err());
    }
    assert!(
        validate_input(
            "image",
            &tensor_input(ort::tensor::TensorElementType::Float32, &[1, 3])
        )
        .is_err()
    );
    assert!(
        validate_input(
            "image",
            &tensor_input(ort::tensor::TensorElementType::Float32, &[2, 3, 800, 800])
        )
        .is_err()
    );
    assert!(
        validate_input(
            "scale_factor",
            &tensor_input(ort::tensor::TensorElementType::Float32, &[1, 2, 3])
        )
        .is_err()
    );
    assert!(
        validate_input(
            "image",
            &ort::value::ValueType::Sequence(Box::new(tensor_input(
                ort::tensor::TensorElementType::Float32,
                &[1, 3, 800, 800]
            )))
        )
        .is_err()
    );
}

#[test]
fn extreme_aspect_ratios_are_rejected_before_resize_allocation() {
    assert!(validate_image_dimensions(1_000_000, 40).is_err());
    assert!(validate_image_dimensions(40, 1_000_000).is_err());
}
#[test]
fn runtime_path_failure_is_reported_without_panicking() {
    let error = LayoutSession::load("model.onnx", "relative-runtime.dylib")
        .err()
        .unwrap();
    assert!(matches!(error, InferenceError::Invalid(_)));
}

#[test]
fn existing_non_library_runtime_is_reported_without_panicking() {
    let file = tempfile::NamedTempFile::new().unwrap();
    let error = LayoutSession::load("model.onnx", file.path())
        .err()
        .unwrap();
    assert!(matches!(error, InferenceError::Runtime(_)));
}

#[test]
fn output_rows_validate_score_coordinates_and_class() {
    let row = [21.0, 0.9, 2.0, 3.0, 90.0, 30.0, 1.0, 1.0];
    assert_eq!(validate_rows(&row, 1).unwrap(), vec![row]);
    for bad in [
        [21.0, 1.1, 2.0, 3.0, 90.0, 30.0, 1.0, 1.0],
        [25.0, 0.9, 2.0, 3.0, 90.0, 30.0, 1.0, 1.0],
        [21.0, 0.9, 2.0, 3.0, f32::NAN, 30.0, 1.0, 1.0],
    ] {
        assert!(validate_rows(&bad, 1).is_err());
    }
    assert!(validate_rows(&row, 2).is_err());
}

#[test]
fn malformed_output_shapes_are_rejected_without_indexing_panics() {
    assert!(validate_output_shapes(&[], &[], &[1], &[0]).is_err());
    assert!(validate_output_shapes(&[0, 8], &[], &[1], &[]).is_err());
    assert!(validate_output_shapes(&[1, 8], &[1.0; 8], &[1], &[0]).is_err());
    assert_eq!(
        validate_output_shapes(&[0, 8], &[], &[1], &[0]).unwrap(),
        Vec::<[f32; 8]>::new()
    );
}
