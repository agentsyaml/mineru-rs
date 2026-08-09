use crate::{ContentBlock, Error, NormalizedBbox, Result, Rotation};
use base64::{Engine, engine::general_purpose::STANDARD};
use image::{
    ExtendedColorType, ImageEncoder, Rgb, RgbImage,
    codecs::{
        jpeg::JpegEncoder,
        png::{CompressionType, FilterType as PngFilterType, PngEncoder},
    },
    imageops::FilterType,
};
use serde_json::{Map, Value};

pub(crate) fn min_edge_dimensions(width: u32, height: u32, target: u32) -> (u32, u32) {
    let scale = target as f32 / width.min(height).max(1) as f32;
    (
        (width as f32 * scale).ceil() as u32,
        (height as f32 * scale).ceil() as u32,
    )
}

pub(crate) fn crop(
    image: &RgbImage,
    bbox: NormalizedBbox,
    rotation: Option<Rotation>,
    _table: bool,
) -> RgbImage {
    let w = image.width();
    let h = image.height();
    let x = (bbox.left * w as f32).floor().clamp(0.0, w as f32 - 1.0) as u32;
    let y = (bbox.top * h as f32).floor().clamp(0.0, h as f32 - 1.0) as u32;
    let right = (bbox.right * w as f32)
        .ceil()
        .clamp((x + 1) as f32, w as f32) as u32;
    let bottom = (bbox.bottom * h as f32)
        .ceil()
        .clamp((y + 1) as f32, h as f32) as u32;
    let mut out = image::imageops::crop_imm(image, x, y, right - x, bottom - y).to_image();
    out = rotate(out, rotation);
    let edge = out.width().min(out.height());
    if edge < 28 {
        let (width, height) = min_edge_dimensions(out.width(), out.height(), 28);
        out = image::imageops::resize(&out, width, height, FilterType::Lanczos3);
    }
    if out.width().max(out.height()) as f32 / out.width().min(out.height()).max(1) as f32 > 50.0 {
        let side = out.width().max(out.height());
        let mut padded = RgbImage::from_pixel(side, side, image::Rgb([255, 255, 255]));
        image::imageops::overlay(
            &mut padded,
            &out,
            ((side - out.width()) / 2) as i64,
            ((side - out.height()) / 2) as i64,
        );
        out = padded;
    }
    out
}

fn raw_crop(image: &RgbImage, bbox: NormalizedBbox, rotation: Option<Rotation>) -> RgbImage {
    let w = image.width();
    let h = image.height();
    let x = (bbox.left * w as f32).floor().clamp(0.0, w as f32 - 1.0) as u32;
    let y = (bbox.top * h as f32).floor().clamp(0.0, h as f32 - 1.0) as u32;
    let right = (bbox.right * w as f32)
        .ceil()
        .clamp((x + 1) as f32, w as f32) as u32;
    let bottom = (bbox.bottom * h as f32)
        .ceil()
        .clamp((y + 1) as f32, h as f32) as u32;
    rotate(
        image::imageops::crop_imm(image, x, y, right - x, bottom - y).to_image(),
        rotation,
    )
}

fn rotate(image: RgbImage, rotation: Option<Rotation>) -> RgbImage {
    match rotation {
        Some(Rotation::Deg90) => image::imageops::rotate90(&image),
        Some(Rotation::Deg180) => image::imageops::rotate180(&image),
        Some(Rotation::Deg270) => image::imageops::rotate270(&image),
        _ => image,
    }
}
pub(crate) fn png_bytes(image: &RgbImage) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    PngEncoder::new_with_quality(&mut bytes, CompressionType::Fast, PngFilterType::Adaptive)
        .write_image(
            image.as_raw(),
            image.width(),
            image.height(),
            ExtendedColorType::Rgb8,
        )
        .map_err(|e| Error::Image(e.to_string()))?;
    Ok(bytes)
}

pub(crate) fn jpeg_data_url(image: &RgbImage) -> Result<String> {
    let mut bytes = Vec::new();
    JpegEncoder::new_with_quality(&mut bytes, 75)
        .write_image(
            image.as_raw(),
            image.width(),
            image.height(),
            ExtendedColorType::Rgb8,
        )
        .map_err(|e| Error::Image(e.to_string()))?;
    Ok(format!("data:image/jpeg;base64,{}", STANDARD.encode(bytes)))
}

pub(crate) const TABLE_IMAGE_TOKEN_LETTERS: &str = "ACDGHKTWXYZ";
pub(crate) const TABLE_IMAGE_TOKEN_NUMBERS: &str = "2345678";
fn token(index: usize) -> String {
    let alphabet = format!("{TABLE_IMAGE_TOKEN_LETTERS}{TABLE_IMAGE_TOKEN_NUMBERS}");
    let mut n = index;
    let mut out = [b'A'; 4];
    for c in out.iter_mut().rev() {
        *c = alphabet.as_bytes()[n % alphabet.len()];
        n /= alphabet.len();
    }
    format!("[{}]", String::from_utf8_lossy(&out))
}

fn scale_coordinate(value: u32, target: u32, source: u32, round_up: bool) -> u32 {
    let divisor = u64::from(source.max(1));
    let product = u64::from(value)
        .checked_mul(u64::from(target))
        .unwrap_or(u64::MAX);
    let scaled = if round_up {
        product.saturating_add(divisor - 1) / divisor
    } else {
        product / divisor
    };
    u32::try_from(scaled.min(u64::from(target))).unwrap_or(target)
}

pub(crate) fn mask_and_encode_table_image(
    page: &RgbImage,
    table: &ContentBlock,
    images: &[ContentBlock],
) -> Result<(RgbImage, Map<String, Value>)> {
    let raw_table = raw_crop(page, table.bbox, None);
    let mut masked = crop(page, table.bbox, table.angle, true);
    let mut map = Map::new();
    for (n, image) in images.iter().enumerate() {
        let value = token(n);
        let source = raw_crop(page, image.bbox, image.angle);
        map.insert(value.clone(), Value::String(jpeg_data_url(&source)?));
        let mut x0 = ((image.bbox.left - table.bbox.left) / (table.bbox.right - table.bbox.left)
            * raw_table.width() as f32)
            .floor()
            .max(0.) as u32;
        let mut y0 = ((image.bbox.top - table.bbox.top) / (table.bbox.bottom - table.bbox.top)
            * raw_table.height() as f32)
            .floor()
            .max(0.) as u32;
        let mut x1 = ((image.bbox.right - table.bbox.left) / (table.bbox.right - table.bbox.left)
            * raw_table.width() as f32)
            .ceil()
            .min(raw_table.width() as f32) as u32;
        let mut y1 = ((image.bbox.bottom - table.bbox.top) / (table.bbox.bottom - table.bbox.top)
            * raw_table.height() as f32)
            .ceil()
            .min(raw_table.height() as f32) as u32;
        (x0, y0, x1, y1) = match table.angle {
            Some(Rotation::Deg90) => (raw_table.height() - y1, x0, raw_table.height() - y0, x1),
            Some(Rotation::Deg180) => (
                raw_table.width() - x1,
                raw_table.height() - y1,
                raw_table.width() - x0,
                raw_table.height() - y0,
            ),
            Some(Rotation::Deg270) => (y0, raw_table.width() - x1, y1, raw_table.width() - x0),
            _ => (x0, y0, x1, y1),
        };
        let rotated_w = match table.angle {
            Some(Rotation::Deg90 | Rotation::Deg270) => raw_table.height(),
            _ => raw_table.width(),
        };
        let rotated_h = match table.angle {
            Some(Rotation::Deg90 | Rotation::Deg270) => raw_table.width(),
            _ => raw_table.height(),
        };
        x0 = scale_coordinate(x0, masked.width(), rotated_w, false);
        x1 = scale_coordinate(x1, masked.width(), rotated_w, true);
        y0 = scale_coordinate(y0, masked.height(), rotated_h, false);
        y1 = scale_coordinate(y1, masked.height(), rotated_h, true);
        if x1 <= x0 || y1 <= y0 {
            continue;
        }
        let mut sum = [0u64; 3];
        let mut count = 0u64;
        for p in raw_crop(page, image.bbox, None).pixels() {
            for (c, value) in sum.iter_mut().enumerate() {
                *value += p[c] as u64;
            }
            count += 1;
        }
        let avg = Rgb([
            (sum[0] / count.max(1)) as u8,
            (sum[1] / count.max(1)) as u8,
            (sum[2] / count.max(1)) as u8,
        ]);
        for y in y0..y1 {
            for x in x0..x1 {
                masked.put_pixel(x, y, avg);
            }
        }
        paint_token(&mut masked, x0, y0, x1, y1, &value);
    }
    Ok((masked, map))
}

fn paint_token(image: &mut RgbImage, x0: u32, y0: u32, x1: u32, y1: u32, text: &str) {
    const GLYPHS: &[(&str, [u8; 5])] = &[
        ("[", [6, 4, 4, 4, 6]),
        ("]", [6, 2, 2, 2, 6]),
        ("A", [2, 5, 7, 5, 5]),
        ("C", [3, 4, 4, 4, 3]),
        ("D", [6, 5, 5, 5, 6]),
        ("G", [3, 4, 7, 5, 3]),
        ("H", [5, 5, 7, 5, 5]),
        ("K", [5, 5, 6, 5, 5]),
        ("T", [7, 2, 2, 2, 2]),
        ("W", [5, 5, 5, 7, 5]),
        ("X", [5, 5, 2, 5, 5]),
        ("Y", [5, 5, 2, 2, 2]),
        ("Z", [7, 1, 2, 4, 7]),
        ("2", [6, 1, 2, 4, 7]),
        ("3", [6, 1, 2, 1, 6]),
        ("4", [5, 5, 7, 1, 1]),
        ("5", [7, 4, 6, 1, 6]),
        ("6", [3, 4, 6, 5, 2]),
        ("7", [7, 1, 2, 2, 2]),
        ("8", [2, 5, 2, 5, 2]),
    ];
    let scale = ((x1 - x0) / (text.len() as u32 * 4))
        .min((y1 - y0) / 6)
        .max(1);
    let width = text.len() as u32 * 4 * scale;
    let start_x = x0 + (x1 - x0).saturating_sub(width) / 2;
    let start_y = y0 + (y1 - y0).saturating_sub(5 * scale) / 2;
    for (i, ch) in text.chars().enumerate() {
        if let Some((_, rows)) = GLYPHS.iter().find(|(c, _)| c.as_bytes()[0] == ch as u8) {
            for (row, bits) in rows.iter().enumerate() {
                for col in 0..3 {
                    if *bits & (1 << (2 - col)) != 0 {
                        for dy in 0..scale {
                            for dx in 0..scale {
                                let x = start_x + i as u32 * 4 * scale + col * scale + dx;
                                let y = start_y + row as u32 * scale + dy;
                                if x < x1 && y < y1 {
                                    image.put_pixel(x, y, Rgb([0, 0, 0]));
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::BlockKind;
    use image::{DynamicImage, ImageFormat};
    use std::io::Cursor;

    fn legacy_bytes(image: &RgbImage, format: ImageFormat) -> Result<Vec<u8>> {
        let mut bytes = Vec::new();
        DynamicImage::ImageRgb8(image.clone())
            .write_to(&mut Cursor::new(&mut bytes), format)
            .map_err(|e| Error::Image(e.to_string()))?;
        Ok(bytes)
    }

    fn patterned_image(width: u32, height: u32) -> RgbImage {
        RgbImage::from_fn(width, height, |x, y| {
            Rgb([
                (x.wrapping_mul(17) + y.wrapping_mul(31)) as u8,
                (x.wrapping_mul(47) ^ y.wrapping_mul(13)) as u8,
                (x.wrapping_mul(7) + y.wrapping_mul(61) + 3) as u8,
            ])
        })
    }

    #[test]
    fn direct_png_matches_legacy_write_to() {
        for image in [
            patterned_image(1, 1),
            patterned_image(17, 11),
            patterned_image(1036, 1036),
        ] {
            assert_eq!(
                png_bytes(&image).unwrap(),
                legacy_bytes(&image, ImageFormat::Png).unwrap()
            );
        }
    }

    #[test]
    fn direct_jpeg_matches_legacy_write_to() {
        for image in [patterned_image(1, 1), patterned_image(17, 11)] {
            let bytes = STANDARD
                .decode(jpeg_data_url(&image).unwrap().rsplit(',').next().unwrap())
                .unwrap();
            assert_eq!(bytes, legacy_bytes(&image, ImageFormat::Jpeg).unwrap());
        }
    }

    #[test]
    fn png_bytes_produce_a_standard_png() {
        let image = RgbImage::from_pixel(2, 1, Rgb([1, 2, 3]));
        let bytes = png_bytes(&image).unwrap();
        assert_eq!(
            &bytes[..8],
            &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]
        );
    }

    #[test]
    fn table_coordinate_scaling_is_overflow_safe_and_compatible() {
        assert_eq!(scale_coordinate(69_999, 70_000, 70_000, false), 69_999);
        assert_eq!(scale_coordinate(69_999, 70_000, 70_000, true), 69_999);
        assert_eq!(scale_coordinate(1_399, 1_400, 1_400, true), 1_399);
        assert_eq!(scale_coordinate(1, 10, 3, false), 3);
        assert_eq!(scale_coordinate(1, 10, 3, true), 4);
        assert_eq!(scale_coordinate(u32::MAX, u32::MAX, 1, true), u32::MAX);
    }

    fn block(kind: &str, bbox: NormalizedBbox, angle: Option<Rotation>) -> ContentBlock {
        ContentBlock {
            kind: BlockKind::new(kind),
            bbox,
            angle,
            content: None,
            merge_previous: false,
            metadata: Map::new(),
        }
    }

    #[test]
    fn table_image_mask_draws_tokens_and_rotates() {
        let outer = block(
            BlockKind::TABLE,
            NormalizedBbox::new(0.1, 0.1, 0.9, 0.9).unwrap(),
            None,
        );
        let inner = block(
            BlockKind::TABLE,
            NormalizedBbox::new(0.2, 0.2, 0.8, 0.8).unwrap(),
            None,
        );
        let first = block(
            BlockKind::IMAGE,
            NormalizedBbox::new(0.3, 0.4, 0.5, 0.6).unwrap(),
            None,
        );
        let mut blocks = vec![outer, inner, first];

        let mut page = RgbImage::from_pixel(100, 100, Rgb([240, 240, 240]));
        for y in 40..60 {
            for x in 30..50 {
                page.put_pixel(x, y, Rgb([20, 80, 200]));
            }
        }
        for rotation in [
            None,
            Some(Rotation::Deg90),
            Some(Rotation::Deg180),
            Some(Rotation::Deg270),
        ] {
            let mut table = blocks[1].clone();
            table.angle = rotation;
            let (masked, tokens) =
                mask_and_encode_table_image(&page, &table, &[blocks[2].clone()]).unwrap();
            assert!(masked.width() > 0 && masked.height() > 0);
            assert!(
                tokens["[AAAA]"]
                    .as_str()
                    .unwrap()
                    .starts_with("data:image/jpeg;base64,")
            );
            assert!(masked.pixels().any(|pixel| *pixel == Rgb([0, 0, 0])));
        }
    }
}
