use std::{
    path::{Path, PathBuf},
    sync::{Mutex, OnceLock},
};

use image::{DynamicImage, GenericImageView, imageops::FilterType};
use ort::{session::Session, value::Tensor};
use thiserror::Error;

const SIDE: u32 = 800;
const MAX_PIXELS: u64 = 40_000_000;
const MAX_RESIZE_TEMP_BYTES: u64 = 256 * 1024 * 1024;
const MAX_BOXES: usize = 100_000;
static RUNTIME: OnceLock<PathBuf> = OnceLock::new();
static RUNTIME_ATTEMPT: OnceLock<PathBuf> = OnceLock::new();
static RUNTIME_INIT: Mutex<()> = Mutex::new(());

#[derive(Debug, Error)]
pub enum InferenceError {
    #[error("invalid inference input: {0}")]
    Invalid(String),
    #[error("ONNX Runtime initialization failed: {0}")]
    Runtime(String),
}

pub struct LayoutSession {
    session: Session,
}

impl LayoutSession {
    pub fn io_metadata(&self) -> (Vec<String>, Vec<String>) {
        (
            self.session
                .inputs
                .iter()
                .map(|input| format!("{}: {:?}", input.name, input.input_type))
                .collect(),
            self.session
                .outputs
                .iter()
                .map(|output| format!("{}: {:?}", output.name, output.output_type))
                .collect(),
        )
    }

    /// Loads trusted model and runtime files. Both the graph and shared library can execute native code;
    /// inference is not sandboxed and does not bound allocations made inside ONNX Runtime.
    pub fn load(
        model_path: impl AsRef<Path>,
        runtime_path: impl AsRef<Path>,
    ) -> Result<Self, InferenceError> {
        let runtime_path = runtime_path.as_ref();
        if !runtime_path.is_absolute() || !runtime_path.is_file() {
            return Err(InferenceError::Invalid(
                "runtime path must be an existing absolute file".into(),
            ));
        }
        if runtime_path.to_str().is_none() {
            return Err(InferenceError::Invalid(
                "runtime path is not valid UTF-8".into(),
            ));
        }
        let runtime_path = runtime_path
            .canonicalize()
            .map_err(|e| InferenceError::Invalid(e.to_string()))?;
        let runtime_path_str = runtime_path.to_str().ok_or_else(|| {
            InferenceError::Invalid("canonical runtime path is not valid UTF-8".into())
        })?;
        let _guard = RUNTIME_INIT.lock().map_err(|_| {
            InferenceError::Runtime("runtime initialization lock was poisoned".into())
        })?;
        match RUNTIME.get() {
            Some(path) if path != &runtime_path => {
                return Err(InferenceError::Invalid(
                    "ONNX Runtime was already initialized from a different path".into(),
                ));
            }
            Some(_) => (),
            None => {
                if RUNTIME_ATTEMPT
                    .get()
                    .is_some_and(|path| path != &runtime_path)
                {
                    return Err(InferenceError::Invalid(
                        "ONNX Runtime initialization was attempted with a different path".into(),
                    ));
                }
                let verified_runtime = validate_runtime_library(&runtime_path)?;
                let _ = RUNTIME_ATTEMPT.set(runtime_path.clone());
                let builder = ort::init_from(runtime_path_str);
                let active_api = std::panic::catch_unwind(std::panic::AssertUnwindSafe(ort::api))
                    .map_err(|_| {
                    InferenceError::Runtime(
                        "ONNX Runtime selected an incompatible global library".into(),
                    )
                })?;
                if !std::ptr::eq(active_api as *const _, verified_runtime.api) {
                    return Err(InferenceError::Runtime(
                        "ONNX Runtime global API belongs to a different library than requested"
                            .into(),
                    ));
                }
                let initialized = builder
                    .commit()
                    .map_err(|e| InferenceError::Runtime(e.to_string()))?;
                if !initialized {
                    return Err(InferenceError::Runtime(
                        "ONNX Runtime environment was already initialized outside LayoutSession"
                            .into(),
                    ));
                }
                let _ = RUNTIME.set(runtime_path.clone());
            }
        }
        let session = Session::builder()
            .and_then(|b| b.commit_from_file(model_path))
            .map_err(|e| InferenceError::Runtime(e.to_string()))?;
        Ok(Self { session })
    }

    /// Returns raw model rows. This does not apply PaddleX filtering or layout postprocessing.
    pub fn infer_raw(&mut self, image: &DynamicImage) -> Result<Vec<[f32; 8]>, InferenceError> {
        let (w, h) = image.dimensions();
        validate_image_dimensions(w, h)?;
        let (input, scale) = preprocess(image)?;
        let mut inputs = vec![(
            "image",
            Tensor::from_array(([1usize, 3, 800, 800], input)).map_err(rt_error)?,
        )];
        for metadata in &self.session.inputs {
            let expected_rank = if metadata.name == "image" { 4 } else { 2 };
            let (kind, shape) = match &metadata.input_type {
                ort::value::ValueType::Tensor { ty, shape, .. } => (ty, shape),
                _ => {
                    return Err(InferenceError::Invalid(format!(
                        "input {} is not a tensor",
                        metadata.name
                    )));
                }
            };
            let dims: &[i64] = shape;
            if *kind != ort::tensor::TensorElementType::Float32 || dims.len() != expected_rank {
                return Err(InferenceError::Invalid(format!(
                    "input {} has unsupported dtype or rank",
                    metadata.name
                )));
            }
            let expected = if metadata.name == "image" {
                &[1, 3, 800, 800][..]
            } else {
                &[1, 2][..]
            };
            if dims
                .iter()
                .zip(expected)
                .any(|(actual, wanted)| *actual >= 0 && actual != wanted)
            {
                return Err(InferenceError::Invalid(format!(
                    "input {} has an incompatible shape",
                    metadata.name
                )));
            }
            match metadata.name.as_str() {
                "image" => (),
                "im_shape" => inputs.push((
                    "im_shape",
                    Tensor::from_array(([1usize, 2], vec![800f32, 800.])).map_err(rt_error)?,
                )),
                "scale_factor" => inputs.push((
                    "scale_factor",
                    Tensor::from_array(([1usize, 2], vec![scale.0, scale.1])).map_err(rt_error)?,
                )),
                name => {
                    return Err(InferenceError::Invalid(format!(
                        "unsupported model input {name}"
                    )));
                }
            }
        }
        if !self.session.inputs.iter().any(|i| i.name == "image") {
            return Err(InferenceError::Invalid("model has no image input".into()));
        }
        let outputs = self.session.run(inputs).map_err(rt_error)?;
        decode_outputs(&outputs)
    }
}

struct VerifiedRuntime {
    _library: libloading::Library,
    api: *const ort::sys::OrtApi,
}

fn validate_runtime_library(path: &Path) -> Result<VerifiedRuntime, InferenceError> {
    let library = unsafe { libloading::Library::new(path) }.map_err(rt_error)?;
    let api = unsafe {
        let entry = library
            .get::<unsafe extern "system" fn() -> *const ort::sys::OrtApiBase>(b"OrtGetApiBase\0")
            .map_err(rt_error)?;
        let base = entry();
        if base.is_null() {
            return Err(InferenceError::Runtime(
                "OrtGetApiBase returned null".into(),
            ));
        }
        let version = ((*base).GetVersionString)();
        if version.is_null() {
            return Err(InferenceError::Runtime(
                "ONNX Runtime returned a null version".into(),
            ));
        }
        let version = std::ffi::CStr::from_ptr(version)
            .to_str()
            .map_err(rt_error)?;
        let mut parts = version.split('.');
        let major = parts.next().and_then(|part| part.parse::<u32>().ok());
        let minor = parts.next().and_then(|part| part.parse::<u32>().ok());
        if major != Some(1) || minor.is_none_or(|minor| minor < ort::MINOR_VERSION) {
            return Err(InferenceError::Runtime(format!(
                "unsupported ONNX Runtime version {version}"
            )));
        }
        let api = ((*base).GetApi)(ort::sys::ORT_API_VERSION);
        if api.is_null() {
            return Err(InferenceError::Runtime(format!(
                "ONNX Runtime {version} does not provide API {}",
                ort::sys::ORT_API_VERSION
            )));
        }
        api
    };
    Ok(VerifiedRuntime {
        _library: library,
        api,
    })
}

fn preprocess(image: &DynamicImage) -> Result<(Vec<f32>, (f32, f32)), InferenceError> {
    let (w, h) = image.dimensions();
    validate_image_dimensions(w, h)?;
    let rgb = image.to_rgb8();
    // Catmull-Rom is cubic, but is not pixel-identical to Pillow's BICUBIC kernel.
    let resized = image::imageops::resize(&rgb, SIDE, SIDE, FilterType::CatmullRom);
    let mut chw = vec![0f32; (3 * SIDE * SIDE) as usize];
    for (i, pixel) in resized.pixels().enumerate() {
        for c in 0..3 {
            chw[c * (SIDE * SIDE) as usize + i] = f32::from(pixel[c]) / 255.0;
        }
    }
    Ok((chw, (SIDE as f32 / h as f32, SIDE as f32 / w as f32)))
}

fn validate_image_dimensions(w: u32, h: u32) -> Result<(), InferenceError> {
    if w == 0 || h == 0 || u64::from(w) * u64::from(h) > MAX_PIXELS {
        return Err(InferenceError::Invalid(
            "image dimensions exceed the supported limit".into(),
        ));
    }
    let resize_temporary = u64::from(w.max(h))
        .checked_mul(u64::from(SIDE))
        .and_then(|pixels| pixels.checked_mul(16));
    if resize_temporary.is_none_or(|bytes| bytes > MAX_RESIZE_TEMP_BYTES) {
        return Err(InferenceError::Invalid(
            "image resize temporary exceeds memory bound".into(),
        ));
    }
    Ok(())
}

fn decode_outputs(
    outputs: &ort::session::SessionOutputs<'_>,
) -> Result<Vec<[f32; 8]>, InferenceError> {
    if outputs.len() != 2 {
        return Err(InferenceError::Invalid(
            "expected boxes and count outputs".into(),
        ));
    }
    let (shape, rows) = outputs[0].try_extract_tensor::<f32>().map_err(rt_error)?;
    let (counts_shape, counts) = outputs[1].try_extract_tensor::<i32>().map_err(rt_error)?;
    validate_output_shapes(shape, rows, counts_shape, counts)
}

fn validate_output_shapes(
    box_shape: &[i64],
    rows: &[f32],
    count_shape: &[i64],
    counts: &[i32],
) -> Result<Vec<[f32; 8]>, InferenceError> {
    if box_shape.len() != 2
        || box_shape[0] < 0
        || box_shape[1] != 8
        || count_shape != [1]
        || counts.len() != 1
    {
        return Err(InferenceError::Invalid(
            "unexpected box/count output shape".into(),
        ));
    }
    let count = usize::try_from(counts[0])
        .map_err(|_| InferenceError::Invalid("negative output count".into()))?;
    let expected = usize::try_from(box_shape[0])
        .ok()
        .and_then(|n| n.checked_mul(8));
    if count > MAX_BOXES || count != box_shape[0] as usize || expected != Some(rows.len()) {
        return Err(InferenceError::Invalid(
            "box count or output buffer length is invalid".into(),
        ));
    }
    validate_rows(rows, count)
}

fn validate_rows(rows: &[f32], count: usize) -> Result<Vec<[f32; 8]>, InferenceError> {
    if rows.len() != count.saturating_mul(8) || count > MAX_BOXES {
        return Err(InferenceError::Invalid(
            "box data length does not match output count".into(),
        ));
    }
    let mut decoded = Vec::with_capacity(count);
    for row in rows.chunks_exact(8) {
        let mut box_row = [0f32; 8];
        box_row.copy_from_slice(row);
        if box_row.iter().any(|x| !x.is_finite())
            || box_row[0] < 0.0
            || box_row[0] > 24.0
            || box_row[0].fract() != 0.0
            || !(0.0..=1.0).contains(&box_row[1])
        {
            return Err(InferenceError::Invalid(format!(
                "invalid raw box values: {box_row:?}"
            )));
        }
        decoded.push(box_row);
    }
    Ok(decoded)
}

fn rt_error(error: impl std::fmt::Display) -> InferenceError {
    InferenceError::Runtime(error.to_string())
}

#[cfg(test)]
mod tests {
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
}
