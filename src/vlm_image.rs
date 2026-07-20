use crate::{VlmError, VlmHttpConfig, VlmImageInput, VlmResult};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use image::ImageReader;
use std::io::{Cursor, Read};

pub(crate) type AdmittedImage = (Vec<u8>, String);

pub(crate) async fn admit_local(
    input: VlmImageInput,
    config: std::sync::Arc<VlmHttpConfig>,
) -> VlmResult<Option<AdmittedImage>> {
    image_worker(move || admit_local_blocking(input, &config)).await
}

pub(crate) async fn decode_local(
    input: VlmImageInput,
    config: std::sync::Arc<VlmHttpConfig>,
) -> VlmResult<Option<image::DynamicImage>> {
    image_worker(move || {
        let Some((bytes, _)) = admit_local_blocking(input, &config)? else {
            return Ok(None);
        };
        image::load_from_memory(&bytes)
            .map(Some)
            .map_err(|_| VlmError::InvalidImageInput("invalid image".into()))
    })
    .await
}

pub(crate) async fn admit_bytes(
    bytes: Vec<u8>,
    hint: Option<String>,
    config: std::sync::Arc<VlmHttpConfig>,
) -> VlmResult<AdmittedImage> {
    image_worker(move || inspect(bytes, hint, &config)).await
}

pub(crate) fn admit_local_blocking(
    input: VlmImageInput,
    config: &VlmHttpConfig,
) -> VlmResult<Option<AdmittedImage>> {
    let (bytes, hint) = match input {
        VlmImageInput::None => return Ok(None),
        VlmImageInput::Path(path) => (read_path(path, config.max_image_bytes)?, None),
        VlmImageInput::Bytes { data, media_type } => {
            check_bytes(data.len(), config.max_image_bytes)?;
            (data.to_vec(), media_type)
        }
        VlmImageInput::Base64 { data, media_type } => {
            encoded_fits(data.len(), config.max_image_bytes)?;
            let bytes = STANDARD
                .decode(data)
                .map_err(|_| VlmError::InvalidImageInput("invalid base64".into()))?;
            check_bytes(bytes.len(), config.max_image_bytes)?;
            (bytes, media_type)
        }
        VlmImageInput::DataUrl(data) => data_url(&data, config.max_image_bytes)?,
        VlmImageInput::RemoteUrl(_) => {
            return Err(VlmError::InvalidImageInput("local image required".into()));
        }
    };
    inspect(bytes, hint, config).map(Some)
}

async fn image_worker<T: Send + 'static>(
    job: impl FnOnce() -> VlmResult<T> + Send + 'static,
) -> VlmResult<T> {
    tokio::task::spawn_blocking(job)
        .await
        .map_err(|_| VlmError::Transport {
            operation: "image",
            message: "image worker failed".into(),
        })?
}

fn read_path(path: std::path::PathBuf, cap: usize) -> VlmResult<Vec<u8>> {
    let metadata = std::fs::metadata(&path).map_err(|_| VlmError::Io {
        operation: "image",
        message: "read failed".into(),
    })?;
    if metadata.len() > cap as u64 {
        return Err(limit(cap, metadata.len()));
    }
    let file = std::fs::File::open(path).map_err(|_| VlmError::Io {
        operation: "image",
        message: "read failed".into(),
    })?;
    let mut bytes = Vec::with_capacity(metadata.len().min(cap as u64) as usize);
    file.take(cap.saturating_add(1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| VlmError::Io {
            operation: "image",
            message: "read failed".into(),
        })?;
    check_bytes(bytes.len(), cap)?;
    Ok(bytes)
}

fn data_url(data: &str, cap: usize) -> VlmResult<(Vec<u8>, Option<String>)> {
    let (media, encoded) = data
        .strip_prefix("data:")
        .and_then(|value| value.split_once(','))
        .ok_or_else(|| VlmError::InvalidImageInput("invalid data URL".into()))?;
    let media = media
        .strip_suffix(";base64")
        .ok_or_else(|| VlmError::InvalidImageInput("data URL must be base64".into()))?;
    let media = supported_media(media)
        .ok_or_else(|| VlmError::InvalidImageInput("unsupported image media type".into()))?;
    encoded_fits(encoded.len(), cap)?;
    let bytes = STANDARD
        .decode(encoded)
        .map_err(|_| VlmError::InvalidImageInput("invalid data URL".into()))?;
    check_bytes(bytes.len(), cap)?;
    Ok((bytes, Some(media.into())))
}

fn supported_media(media: &str) -> Option<&'static str> {
    [
        "image/jpeg",
        "image/png",
        "image/gif",
        "image/bmp",
        "image/webp",
        "image/tiff",
    ]
    .into_iter()
    .find(|supported| media.eq_ignore_ascii_case(supported))
}

fn encoded_fits(encoded_len: usize, cap: usize) -> VlmResult<()> {
    let encoded_cap = cap
        .checked_add(2)
        .and_then(|value| value.checked_div(3))
        .and_then(|value| value.checked_mul(4))
        .unwrap_or(usize::MAX);
    if encoded_len > encoded_cap {
        return Err(limit(cap, decoded_upper_bound(encoded_len)));
    }
    Ok(())
}

fn decoded_upper_bound(encoded_len: usize) -> u64 {
    encoded_len
        .checked_add(3)
        .and_then(|value| value.checked_div(4))
        .and_then(|value| value.checked_mul(3))
        .unwrap_or(usize::MAX) as u64
}

fn check_bytes(actual: usize, cap: usize) -> VlmResult<()> {
    if actual > cap {
        return Err(limit(cap, actual as u64));
    }
    Ok(())
}

fn inspect(
    bytes: Vec<u8>,
    hint: Option<String>,
    config: &VlmHttpConfig,
) -> VlmResult<AdmittedImage> {
    check_bytes(bytes.len(), config.max_image_bytes)?;
    let reader = ImageReader::new(Cursor::new(&bytes))
        .with_guessed_format()
        .map_err(|_| VlmError::InvalidImageInput("unsupported image".into()))?;
    let format = reader
        .format()
        .ok_or_else(|| VlmError::InvalidImageInput("unsupported image".into()))?;
    let media =
        mime(format).ok_or_else(|| VlmError::InvalidImageInput("unsupported image".into()))?;
    if hint
        .as_deref()
        .is_some_and(|value| !value.eq_ignore_ascii_case(media))
    {
        return Err(VlmError::InvalidImageInput(
            "image media type mismatch".into(),
        ));
    }
    let (width, height) = reader
        .into_dimensions()
        .map_err(|_| VlmError::InvalidImageInput("invalid image".into()))?;
    let pixels =
        u64::from(width)
            .checked_mul(u64::from(height))
            .ok_or(VlmError::LimitExceeded {
                resource: "image pixels",
                limit: config.max_decoded_pixels,
                actual: u64::MAX,
            })?;
    if pixels > config.max_decoded_pixels {
        return Err(VlmError::LimitExceeded {
            resource: "image pixels",
            limit: config.max_decoded_pixels,
            actual: pixels,
        });
    }
    Ok((bytes, media.into()))
}

fn mime(format: image::ImageFormat) -> Option<&'static str> {
    match format {
        image::ImageFormat::Jpeg => Some("image/jpeg"),
        image::ImageFormat::Png => Some("image/png"),
        image::ImageFormat::Gif => Some("image/gif"),
        image::ImageFormat::Bmp => Some("image/bmp"),
        image::ImageFormat::WebP => Some("image/webp"),
        image::ImageFormat::Tiff => Some("image/tiff"),
        _ => None,
    }
}

fn limit(cap: usize, actual: u64) -> VlmError {
    VlmError::LimitExceeded {
        resource: "image bytes",
        limit: cap as u64,
        actual,
    }
}
