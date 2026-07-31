//! Private, fixture-shaped official VLM artifact construction.
#![allow(dead_code)] // Stage 3 wires this private builder into the PDF path.

use crate::{Asset, AssetKind, ModelBlock, ModelOutput, VlmError, VlmResult};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use bytes::Bytes;
use image::{ColorType, ImageEncoder, RgbImage, codecs::jpeg::JpegEncoder};
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet, HashSet},
    io::{self, Write},
    path::PathBuf,
    sync::OnceLock,
};

pub(crate) struct OfficialBuildPage {
    pub slice_page_idx: usize,
    pub page_size_points: [f32; 2],
    pub render_scale: f32,
    pub rgb: RgbImage,
    pub snapshot: Vec<ModelBlock>,
}

pub(crate) struct OfficialBuildArtifacts {
    pub model_output: ModelOutput,
    pub middle_json: Value,
    pub content_list: Value,
    pub content_list_v2: Value,
    pub markdown: String,
    pub assets: Vec<Asset>,
}

/// A page after its pixel-backed assets have been extracted.  These records are
/// serialized by the route, so document canonicalization need not retain RGB pages.
#[derive(Serialize, Deserialize)]
pub(crate) struct OfficialPreparedPage {
    slice_page_idx: usize,
    page_size_points: [f32; 2],
    snapshot: Vec<ModelBlock>,
    preproc: Vec<Node>,
    discarded: Vec<Block>,
}

pub(crate) struct OfficialPreparedArtifacts {
    pub page: OfficialPreparedPage,
    pub assets: Vec<Asset>,
}

#[derive(Clone, Serialize, Deserialize)]
enum Span {
    Text(String),
    InlineEquation(String),
    InterlineEquation(String),
    Table(String),
    Image(Option<String>),
    Chart(Option<String>),
}

#[derive(Clone, Serialize, Deserialize)]
struct Line {
    bbox: [i32; 4],
    spans: Vec<Span>,
}

#[derive(Clone, Serialize, Deserialize)]
struct Block {
    kind: String,
    bbox: [i32; 4],
    angle: i32,
    lines: Vec<Line>,
    index: usize,
    merge_prev: Option<bool>,
    sub_type: Option<String>,
    guess_lang: Option<String>,
    image_path: Option<String>,
    cell_merge: Option<Value>,
    cross_page: bool,
    lines_deleted: bool,
}

#[derive(Clone, Serialize, Deserialize)]
enum Node {
    Leaf(Block),
    List {
        marker: Block,
        items: Vec<Block>,
    },
    Visual {
        kind: String,
        body: Block,
        captions: Vec<Block>,
        footnotes: Vec<Block>,
        sub_type: Option<String>,
        cell_merge: Option<Value>,
        sub_images: Vec<Value>,
    },
    Tombstone {
        kind: String,
        bbox: [i32; 4],
        angle: i32,
        index: usize,
        sub_type: Option<String>,
    },
}

fn err<T>(message: impl Into<String>) -> VlmResult<T> {
    Err(VlmError::Protocol {
        operation: "official builders",
        message: message.into(),
    })
}

fn sha(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn json_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(value) => *value,
        Value::Number(value) => value.as_f64().is_some_and(|value| value != 0.0),
        Value::String(value) => !value.is_empty(),
        Value::Array(value) => !value.is_empty(),
        Value::Object(value) => !value.is_empty(),
    }
}

fn angle(block: &ModelBlock) -> VlmResult<i32> {
    match block.angle.ok_or_else(|| VlmError::Protocol {
        operation: "official builders",
        message: "snapshot angle is required".into(),
    })? {
        crate::Rotation::Deg0 => Ok(0),
        crate::Rotation::Deg90 => Ok(90),
        crate::Rotation::Deg180 => Ok(180),
        crate::Rotation::Deg270 => Ok(270),
    }
}

fn point_bbox(block: &ModelBlock, size: [f32; 2]) -> VlmResult<[i32; 4]> {
    let bbox = block.bbox.ok_or_else(|| VlmError::Protocol {
        operation: "official builders",
        message: "snapshot bbox is required".into(),
    })?;
    let coordinates = [bbox.left, bbox.top, bbox.right, bbox.bottom];
    if coordinates
        .iter()
        .any(|coordinate| !coordinate.is_finite() || !(0.0..=1.0).contains(coordinate))
    {
        return err("snapshot bbox must be finite and normalized");
    }

    let mut point = [0; 4];
    for (target, (coordinate, dimension)) in point.iter_mut().zip(
        coordinates
            .into_iter()
            .zip([size[0], size[1], size[0], size[1]]),
    ) {
        let value = (coordinate * dimension).trunc();
        if !value.is_finite() || value < i32::MIN as f32 || value > i32::MAX as f32 {
            return err("snapshot geometry is outside point-coordinate range");
        }
        *target = value as i32;
    }
    if point[2] < point[0] {
        point.swap(0, 2);
    }
    if point[3] < point[1] {
        point.swap(1, 3);
    }
    if point[0] == point[2] || point[1] == point[3] {
        return err("empty snapshot geometry");
    }
    Ok(point)
}

fn expected_pixels(points: f32, scale: f32) -> VlmResult<u32> {
    let pixels = (points * scale).ceil();
    if !pixels.is_finite() || pixels <= 0.0 || pixels > u32::MAX as f32 {
        return err("invalid rendered page dimensions");
    }
    Ok(pixels as u32)
}

fn validate_page(page: &OfficialBuildPage, previous_index: Option<usize>) -> VlmResult<()> {
    if previous_index.is_some_and(|index| page.slice_page_idx <= index)
        || !page.render_scale.is_finite()
        || page.render_scale <= 0.0
        || page
            .page_size_points
            .iter()
            .any(|value| !value.is_finite() || *value <= 0.0)
    {
        return err("invalid official build page");
    }
    let expected_width = expected_pixels(page.page_size_points[0], page.render_scale)?;
    let expected_height = expected_pixels(page.page_size_points[1], page.render_scale)?;
    if page.rgb.width() != expected_width || page.rgb.height() != expected_height {
        return err("RGB snapshot does not match page points and render scale");
    }
    Ok(())
}

fn crop(
    page: &OfficialBuildPage,
    page_rgb_md5: &md5::Digest,
    tag: &str,
    kind: AssetKind,
    point: [i32; 4],
    assets: &mut BTreeMap<String, Asset>,
    max_asset_bytes: usize,
) -> VlmResult<String> {
    let clamp = |value: f32, maximum: u32, ceil: bool| {
        let value = if ceil { value.ceil() } else { value.floor() };
        value.clamp(0.0, maximum as f32) as u32
    };
    let x0 = clamp(point[0] as f32 * page.render_scale, page.rgb.width(), false);
    let y0 = clamp(
        point[1] as f32 * page.render_scale,
        page.rgb.height(),
        false,
    );
    let x1 = clamp(point[2] as f32 * page.render_scale, page.rgb.width(), true);
    let y1 = clamp(point[3] as f32 * page.render_scale, page.rgb.height(), true);
    if x0 >= x1 || y0 >= y1 {
        return err("empty crop");
    }

    let seed = format!(
        "{}/{:X}_{}_{}_{}_{}_{}",
        tag.to_ascii_lowercase(),
        page_rgb_md5,
        page.slice_page_idx,
        point[0],
        point[1],
        point[2],
        point[3]
    );
    let basename = format!("{}.jpg", sha(seed.as_bytes()));
    let path = format!("images/{basename}");

    if assets.contains_key(&path) {
        return Ok(basename);
    }
    let used = asset_bytes(assets)?;
    let remaining = max_asset_bytes.saturating_sub(used);
    let pixels = (x1 - x0) as usize * (y1 - y0) as usize;
    let raw_bytes = pixels
        .checked_mul(3)
        .ok_or_else(|| VlmError::LimitExceeded {
            resource: "total asset bytes",
            limit: max_asset_bytes as u64,
            actual: u64::MAX,
        })?;
    if raw_bytes > remaining {
        return Err(VlmError::LimitExceeded {
            resource: "total asset bytes",
            limit: max_asset_bytes as u64,
            actual: used.saturating_add(raw_bytes) as u64,
        });
    }
    let image = image::imageops::crop_imm(&page.rgb, x0, y0, x1 - x0, y1 - y0).to_image();
    let data = encode_jpeg(&image, remaining)?;
    assets.insert(
        path.clone(),
        Asset {
            kind,
            relative_path: PathBuf::from(path),
            media_type: "image/jpeg".into(),
            md5: format!("{:x}", md5::compute(&data)),
            data: Bytes::from(data),
        },
    );
    Ok(basename)
}

fn inline_images(
    html: &str,
    assets: &mut BTreeMap<String, Asset>,
    max_asset_bytes: usize,
) -> VlmResult<String> {
    if html.is_empty() || !html.contains("base64,") {
        return Ok(html.into());
    }
    // Python's re.sub is deliberately not tag/boundary scoped.
    let source = Regex::new(r#"src="(data:image/[^"]+)""#).expect("constant regex");
    let data_uri = Regex::new(r"(?s)data:image/([^;]+);base64,(.+)").expect("constant regex");
    let mut replacements = BTreeMap::new();

    for capture in source.captures_iter(html) {
        let uri_match = capture.get(1).expect("data URI capture");
        let uri = uri_match.as_str();
        let Some(parts) = data_uri.captures(uri) else {
            continue;
        };
        let declared = parts[1].to_ascii_lowercase();
        let mut extension = match declared
            .split_once('+')
            .map_or(declared.as_str(), |(kind, _)| kind)
        {
            "jpeg" => "jpg",
            kind => kind,
        }
        .to_owned();
        // Python's b64decode is deliberately permissive here.  Strip what it
        // ignores, then leave malformed padding/forms untouched on failure.
        let encoded: String = parts[2]
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '/' | '='))
            .collect();
        let used = asset_bytes(assets)?;
        let remaining = max_asset_bytes.saturating_sub(used);
        let upper = base64_upper_bound(encoded.len()).ok_or_else(|| VlmError::LimitExceeded {
            resource: "total asset bytes",
            limit: max_asset_bytes as u64,
            actual: u64::MAX,
        })?;
        if upper > remaining {
            return Err(VlmError::LimitExceeded {
                resource: "total asset bytes",
                limit: max_asset_bytes as u64,
                actual: used.saturating_add(upper) as u64,
            });
        }
        // Python's base64.b64decode(validate=False) accepts an empty payload
        // after discarding non-base64 bytes (notably `%%%`).
        let Ok(mut data) = STANDARD.decode(&encoded) else {
            continue;
        };
        let mut asset_media_type = format!("image/{declared}");
        let mut path_seed = uri.to_owned();
        if matches!(declared.as_str(), "wmf" | "emf" | "x-wmf" | "x-emf") {
            // ponytail: non-Windows builds use one deterministic JPEG rather than
            // carrying a Windows metafile renderer.
            data = vector_placeholder().to_vec();
            extension = "jpg".into();
            asset_media_type = "image/jpeg".into();
            path_seed = format!("data:image/jpeg;base64,{}", STANDARD.encode(&data));
        }
        let basename = format!("{}.{}", sha(path_seed.as_bytes()), extension);
        let path = format!("images/{basename}");
        if let Some(existing) = assets.get(&path) {
            if existing.data.as_ref() != data {
                return err("inline asset collision");
            }
        } else {
            assets.insert(
                path.clone(),
                Asset {
                    kind: AssetKind::Image,
                    relative_path: PathBuf::from(path),
                    media_type: asset_media_type,
                    md5: format!("{:x}", md5::compute(&data)),
                    data: Bytes::from(data),
                },
            );
        }
        replacements.insert(uri.to_owned(), basename);
    }
    let output = source
        .replace_all(html, |capture: &regex::Captures<'_>| {
            replacements.get(&capture[1]).map_or_else(
                || capture[0].to_owned(),
                |basename| format!("src=\"{basename}\""),
            )
        })
        .into_owned();
    Ok(output)
}

fn base64_upper_bound(len: usize) -> Option<usize> {
    len.checked_add(3)?.checked_div(4)?.checked_mul(3)
}

fn asset_bytes(assets: &BTreeMap<String, Asset>) -> VlmResult<usize> {
    assets.values().try_fold(0usize, |used, asset| {
        used.checked_add(asset.data.len())
            .ok_or_else(|| VlmError::LimitExceeded {
                resource: "total asset bytes",
                limit: usize::MAX as u64,
                actual: u64::MAX,
            })
    })
}

fn encode_jpeg(image: &RgbImage, cap: usize) -> VlmResult<Vec<u8>> {
    let mut output = CappedWriter::new(cap);
    JpegEncoder::new(&mut output)
        .write_image(
            image.as_raw(),
            image.width(),
            image.height(),
            ColorType::Rgb8.into(),
        )
        .map_err(|error| {
            if output.attempted > cap {
                VlmError::LimitExceeded {
                    resource: "total asset bytes",
                    limit: cap as u64,
                    actual: output.attempted as u64,
                }
            } else {
                VlmError::Protocol {
                    operation: "official builders",
                    message: error.to_string(),
                }
            }
        })?;
    Ok(output.data)
}

struct CappedWriter {
    data: Vec<u8>,
    cap: usize,
    attempted: usize,
}

impl CappedWriter {
    fn new(cap: usize) -> Self {
        Self {
            data: Vec::new(),
            cap,
            attempted: 0,
        }
    }
}

impl Write for CappedWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.attempted = self.attempted.saturating_add(bytes.len());
        if self.data.len().saturating_add(bytes.len()) > self.cap {
            return Err(io::Error::other("output limit"));
        }
        self.data.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn vector_placeholder() -> &'static [u8] {
    static JPEG: OnceLock<Vec<u8>> = OnceLock::new();
    JPEG.get_or_init(|| {
        encode_jpeg(
            &RgbImage::from_pixel(320, 180, image::Rgb([240, 240, 240])),
            320 * 180 * 3,
        )
        .expect("memory JPEG encoding")
    })
}

fn output_html(html: &str) -> String {
    let source = Regex::new(r#"(?i)(src\s*=\s*[\"'])([^\"']+)([\"'])"#).expect("constant regex");
    let html = Regex::new(r"(?s)<eq>(.*?)</eq>")
        .expect("constant regex")
        .replace_all(html, |capture: &regex::Captures<'_>| {
            format!(" ${}$ ", html_unescape(&capture[1]))
        })
        .into_owned();
    source
        .replace_all(&html, |capture: &regex::Captures<'_>| {
            let value = &capture[2];
            if value.to_ascii_lowercase().starts_with("data:") {
                capture[0].to_owned()
            } else {
                format!("{}images/{}{}", &capture[1], value, &capture[3])
            }
        })
        .into_owned()
}

fn html_unescape(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    let bytes = value.as_bytes();
    let mut copied = 0;
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] != b'&' {
            index += value[index..]
                .chars()
                .next()
                .expect("valid UTF-8")
                .len_utf8();
            continue;
        }
        let Some((end, decoded)) = html_reference(&value[index..]) else {
            index += 1;
            continue;
        };
        output.push_str(&value[copied..index]);
        output.push_str(&decoded);
        index += end;
        copied = index;
    }
    if copied == 0 {
        value.into()
    } else {
        output.push_str(&value[copied..]);
        output
    }
}

fn html_reference(value: &str) -> Option<(usize, String)> {
    let bytes = value.as_bytes();
    debug_assert_eq!(bytes.first(), Some(&b'&'));
    if bytes.get(1) == Some(&b'#') {
        let (radix, mut index) = if matches!(bytes.get(2), Some(b'x' | b'X')) {
            (16, 3)
        } else {
            (10, 2)
        };
        let digits = index;
        while bytes.get(index).is_some_and(|byte| {
            if radix == 16 {
                byte.is_ascii_hexdigit()
            } else {
                byte.is_ascii_digit()
            }
        }) {
            index += 1;
        }
        if index == digits {
            return None;
        }
        let number = u32::from_str_radix(&value[digits..index], radix).unwrap_or(u32::MAX);
        if bytes.get(index) == Some(&b';') {
            index += 1;
        }
        return Some((index, python_numeric_reference(number)));
    }

    let mut end = 1;
    while end < bytes.len()
        && end <= 32
        && !matches!(
            bytes[end],
            b'\t' | b'\n' | 0x0c | b' ' | b'<' | b'&' | b'#' | b';'
        )
    {
        end += 1;
    }
    if end == 1 {
        return None;
    }
    let semicolon = bytes.get(end) == Some(&b';');
    if semicolon {
        end += 1;
    }
    let name = &value[1..end - usize::from(semicolon)];
    if semicolon {
        return named_entity(name).map(|decoded| (end, decoded.into()));
    }
    for prefix_len in (1..=name.len()).rev() {
        let prefix = &name[..prefix_len];
        if legacy_entity(prefix)
            && let Some(decoded) = named_entity(prefix)
        {
            return Some((1 + name.len(), format!("{decoded}{}", &name[prefix_len..])));
        }
    }
    None
}

fn named_entity(name: &str) -> Option<&'static str> {
    // html-escape carries the WHATWG names but its generated table loses these
    // multi-codepoint values. Keep the source table's complete replacements.
    let combined = match name {
        "acE" => Some("\u{223e}\u{0333}"),
        "bne" => Some("=\u{20e5}"),
        "bnequiv" => Some("\u{2261}\u{20e5}"),
        "caps" => Some("\u{2229}\u{fe00}"),
        "cups" => Some("\u{222a}\u{fe00}"),
        "fjlig" => Some("fj"),
        "gesl" => Some("\u{22db}\u{fe00}"),
        "gvertneqq" | "gvnE" => Some("\u{2269}\u{fe00}"),
        "lates" => Some("\u{2aad}\u{fe00}"),
        "lesg" => Some("\u{22da}\u{fe00}"),
        "lvertneqq" | "lvnE" => Some("\u{2268}\u{fe00}"),
        "nang" => Some("\u{2220}\u{20d2}"),
        "napE" => Some("\u{2a70}\u{0338}"),
        "napid" => Some("\u{224b}\u{0338}"),
        "nbump" | "NotHumpDownHump" => Some("\u{224e}\u{0338}"),
        "nbumpe" | "NotHumpEqual" => Some("\u{224f}\u{0338}"),
        "ncongdot" => Some("\u{2a6d}\u{0338}"),
        "nedot" => Some("\u{2250}\u{0338}"),
        "nesim" | "NotEqualTilde" => Some("\u{2242}\u{0338}"),
        "ngE" | "ngeqq" | "NotGreaterFullEqual" => Some("\u{2267}\u{0338}"),
        "ngeqslant" | "nges" | "NotGreaterSlantEqual" => Some("\u{2a7e}\u{0338}"),
        "nGg" => Some("\u{22d9}\u{0338}"),
        "nGt" => Some("\u{226b}\u{20d2}"),
        "nGtv" | "NotGreaterGreater" => Some("\u{226b}\u{0338}"),
        "nlE" | "nleqq" => Some("\u{2266}\u{0338}"),
        "nleqslant" | "nles" | "NotLessSlantEqual" => Some("\u{2a7d}\u{0338}"),
        "nLl" => Some("\u{22d8}\u{0338}"),
        "nLt" => Some("\u{226a}\u{20d2}"),
        "nLtv" | "NotLessLess" => Some("\u{226a}\u{0338}"),
        "notindot" => Some("\u{22f5}\u{0338}"),
        "notinE" => Some("\u{22f9}\u{0338}"),
        "NotLeftTriangleBar" => Some("\u{29cf}\u{0338}"),
        "NotNestedGreaterGreater" => Some("\u{2aa2}\u{0338}"),
        "NotNestedLessLess" => Some("\u{2aa1}\u{0338}"),
        "NotPrecedesEqual" | "npre" | "npreceq" => Some("\u{2aaf}\u{0338}"),
        "NotRightTriangleBar" => Some("\u{29d0}\u{0338}"),
        "NotSquareSubset" => Some("\u{228f}\u{0338}"),
        "NotSquareSuperset" => Some("\u{2290}\u{0338}"),
        "NotSubset" | "nsubset" | "vnsub" => Some("\u{2282}\u{20d2}"),
        "NotSucceedsEqual" | "nsce" | "nsucceq" => Some("\u{2ab0}\u{0338}"),
        "NotSucceedsTilde" => Some("\u{227f}\u{0338}"),
        "NotSuperset" | "nsupset" | "vnsup" => Some("\u{2283}\u{20d2}"),
        "nparsl" => Some("\u{2afd}\u{20e5}"),
        "npart" => Some("\u{2202}\u{0338}"),
        "nrarrc" => Some("\u{2933}\u{0338}"),
        "nrarrw" => Some("\u{219d}\u{0338}"),
        "nsubE" | "nsubseteqq" => Some("\u{2ac5}\u{0338}"),
        "nsupE" | "nsupseteqq" => Some("\u{2ac6}\u{0338}"),
        "nvap" => Some("\u{224d}\u{20d2}"),
        "nvge" => Some("\u{2265}\u{20d2}"),
        "nvgt" => Some(">\u{20d2}"),
        "nvle" => Some("\u{2264}\u{20d2}"),
        "nvlt" => Some("<\u{20d2}"),
        "nvltrie" => Some("\u{22b4}\u{20d2}"),
        "nvrtrie" => Some("\u{22b5}\u{20d2}"),
        "nvsim" => Some("\u{223c}\u{20d2}"),
        "race" => Some("\u{223d}\u{0331}"),
        "smtes" => Some("\u{2aac}\u{fe00}"),
        "sqcaps" => Some("\u{2293}\u{fe00}"),
        "sqcups" => Some("\u{2294}\u{fe00}"),
        "ThickSpace" => Some("\u{205f}\u{200a}"),
        "varsubsetneq" | "vsubne" => Some("\u{228a}\u{fe00}"),
        "varsubsetneqq" | "vsubnE" => Some("\u{2acb}\u{fe00}"),
        "varsupsetneq" | "vsupne" => Some("\u{228b}\u{fe00}"),
        "varsupsetneqq" | "vsupnE" => Some("\u{2acc}\u{fe00}"),
        _ => None,
    };
    if combined.is_some() {
        return combined;
    }
    html_escape::NAMED_ENTITIES
        .binary_search_by(|(candidate, _)| candidate.cmp(&name.as_bytes()))
        .ok()
        .map(|index| html_escape::NAMED_ENTITIES[index].1)
}

fn legacy_entity(name: &str) -> bool {
    const LEGACY: &[&str] = &[
        "AElig", "AMP", "Aacute", "Acirc", "Agrave", "Aring", "Atilde", "Auml", "COPY", "Ccedil",
        "ETH", "Eacute", "Ecirc", "Egrave", "Euml", "GT", "Iacute", "Icirc", "Igrave", "Iuml",
        "LT", "Ntilde", "Oacute", "Ocirc", "Ograve", "Oslash", "Otilde", "Ouml", "QUOT", "REG",
        "THORN", "Uacute", "Ucirc", "Ugrave", "Uuml", "Yacute", "aacute", "acirc", "acute",
        "aelig", "agrave", "amp", "aring", "atilde", "auml", "brvbar", "ccedil", "cedil", "cent",
        "copy", "curren", "deg", "divide", "eacute", "ecirc", "egrave", "eth", "euml", "frac12",
        "frac14", "frac34", "gt", "iacute", "icirc", "iexcl", "igrave", "iquest", "iuml", "laquo",
        "lt", "macr", "micro", "middot", "nbsp", "not", "ntilde", "oacute", "ocirc", "ograve",
        "ordf", "ordm", "oslash", "otilde", "ouml", "para", "plusmn", "pound", "quot", "raquo",
        "reg", "sect", "shy", "sup1", "sup2", "sup3", "szlig", "thorn", "times", "uacute", "ucirc",
        "ugrave", "uml", "uuml", "yacute", "yen", "yuml",
    ];
    LEGACY.binary_search(&name).is_ok()
}

fn python_numeric_reference(number: u32) -> String {
    let c1 = [
        (0x80, '€'),
        (0x81, '\u{81}'),
        (0x82, '‚'),
        (0x83, 'ƒ'),
        (0x84, '„'),
        (0x85, '…'),
        (0x86, '†'),
        (0x87, '‡'),
        (0x88, 'ˆ'),
        (0x89, '‰'),
        (0x8a, 'Š'),
        (0x8b, '‹'),
        (0x8c, 'Œ'),
        (0x8d, '\u{8d}'),
        (0x8e, 'Ž'),
        (0x8f, '\u{8f}'),
        (0x90, '\u{90}'),
        (0x91, '‘'),
        (0x92, '’'),
        (0x93, '“'),
        (0x94, '”'),
        (0x95, '•'),
        (0x96, '–'),
        (0x97, '—'),
        (0x98, '˜'),
        (0x99, '™'),
        (0x9a, 'š'),
        (0x9b, '›'),
        (0x9c, 'œ'),
        (0x9d, '\u{9d}'),
        (0x9e, 'ž'),
        (0x9f, 'Ÿ'),
    ];
    if let Some((_, character)) = c1.iter().find(|(code, _)| *code == number) {
        return character.to_string();
    }
    if number == 0 || (0xd800..=0xdfff).contains(&number) || number > 0x10ffff {
        return "�".into();
    }
    if matches!(number, 1..=8 | 11 | 14..=31 | 127..=159)
        || (0xfdd0..=0xfdef).contains(&number)
        || number & 0xffff == 0xfffe
        || number & 0xffff == 0xffff
    {
        return String::new();
    }
    char::from_u32(number)
        .expect("checked Unicode scalar")
        .to_string()
}

fn clean_text_content(content: &str) -> String {
    if content.matches("\\[").count() == content.matches("\\]").count() && content.contains("\\[") {
        Regex::new(r"\\\[(.*?)\\\]")
            .expect("constant regex")
            .replace_all(content, "[$1]")
            .into_owned()
    } else {
        content.into()
    }
}

fn text_spans(content: String) -> Vec<Span> {
    let content = clean_text_content(&content);
    if content.matches("\\(").count() != content.matches("\\)").count() || !content.contains("\\(")
    {
        return vec![Span::Text(content)];
    }
    let expression = Regex::new(r"\\\((.+?)\\\)").expect("constant regex");
    let mut spans = Vec::new();
    let mut end = 0;
    for matched in expression.captures_iter(&content) {
        let whole = matched.get(0).expect("whole match");
        let before = &content[end..whole.start()];
        if !before.trim().is_empty() {
            spans.push(Span::Text(before.into()));
        }
        spans.push(Span::InlineEquation(matched[1].trim().into()));
        end = whole.end();
    }
    let after = &content[end..];
    if !after.trim().is_empty() {
        spans.push(Span::Text(after.into()));
    }
    if spans.is_empty() {
        vec![Span::Text(content)]
    } else {
        spans
    }
}

fn equation_content(content: &str) -> String {
    let content = content.trim();
    content
        .strip_prefix("\\[")
        .and_then(|content| content.strip_suffix("\\]"))
        .unwrap_or(content)
        .trim_end()
        .into()
}

fn code_content(content: &str) -> String {
    let lines: Vec<_> = content.lines().collect();
    let start = usize::from(lines.first().is_some_and(|line| line.starts_with("```")));
    let end = lines.len() - usize::from(lines.last().is_some_and(|line| line.trim().eq("```")));
    lines
        .get(start..end.max(start))
        .unwrap_or_default()
        .join("\n")
        .trim()
        .into()
}

fn raw_block(
    page: &OfficialBuildPage,
    page_rgb_md5: Option<&md5::Digest>,
    raw: &ModelBlock,
    index: usize,
    assets: &mut BTreeMap<String, Asset>,
    max_asset_bytes: usize,
) -> VlmResult<Block> {
    let bbox = point_bbox(raw, page.page_size_points)?;
    let angle = angle(raw)?;
    let source_type = raw.block_type.as_str();
    let mut block = Block {
        kind: String::new(),
        bbox,
        angle,
        lines: Vec::new(),
        index,
        merge_prev: None,
        sub_type: None,
        guess_lang: None,
        image_path: None,
        cell_merge: None,
        cross_page: false,
        lines_deleted: false,
    };

    match source_type {
        "text" | "title" | "ref_text" | "phonetic" | "header" | "footer" | "page_number"
        | "aside_text" | "page_footnote" | "formula_number" | "index" | "list_item" => {
            block.kind = match source_type {
                "formula_number" | "index" | "list_item" => "text",
                _ => source_type,
            }
            .into();
            block.lines = vec![Line {
                bbox,
                spans: text_spans(raw.content.clone().unwrap_or_default()),
            }];
            if source_type == "title" {
                let content = block
                    .lines
                    .first_mut()
                    .and_then(|line| line.spans.first_mut());
                if let Some(Span::Text(content)) = content {
                    *content = content.split_whitespace().collect::<Vec<_>>().join(" ");
                }
            }
            if source_type == "text" {
                block.merge_prev = raw.merge_prev;
            }
        }
        "list" => {
            if raw
                .content
                .as_deref()
                .is_some_and(|content| !content.is_empty())
            {
                return err("list marker content must be null or empty");
            }
            block.kind = "list".into();
        }
        "table_caption" | "image_caption" | "chart_caption" | "code_caption" | "caption" => {
            block.kind = "caption".into();
            block.lines = vec![Line {
                bbox,
                spans: text_spans(raw.content.clone().unwrap_or_default()),
            }];
        }
        "table_footnote" | "image_footnote" | "chart_footnote" | "code_footnote" | "footnote" => {
            block.kind = "footnote".into();
            block.lines = vec![Line {
                bbox,
                spans: text_spans(raw.content.clone().unwrap_or_default()),
            }];
        }
        "table" => {
            block.kind = "table_body".into();
            let html = inline_images(
                raw.content.as_deref().unwrap_or_default(),
                assets,
                max_asset_bytes,
            )?;
            block.lines = vec![Line {
                bbox,
                spans: vec![Span::Table(html)],
            }];
            block.image_path = Some(crop(
                page,
                page_rgb_md5.expect("visual page has an RGB digest"),
                "table",
                AssetKind::Table,
                bbox,
                assets,
                max_asset_bytes,
            )?);
            block.cell_merge = raw
                .extra
                .get("cell_merge")
                .filter(|value| json_truthy(value))
                .cloned();
        }
        "image" | "image_block" => {
            block.kind = if source_type == "image" {
                "image_body"
            } else {
                "image_block_body"
            }
            .into();
            block.lines = vec![Line {
                bbox,
                spans: vec![Span::Image(raw.content.clone())],
            }];
            block.image_path = Some(crop(
                page,
                page_rgb_md5.expect("visual page has an RGB digest"),
                "image",
                AssetKind::Image,
                bbox,
                assets,
                max_asset_bytes,
            )?);
            if source_type == "image" {
                block.sub_type = raw.sub_type.clone().filter(|value| !value.is_empty());
            }
        }
        "chart" => {
            block.kind = "chart_body".into();
            block.lines = vec![Line {
                bbox,
                spans: vec![Span::Chart(raw.content.clone())],
            }];
            block.image_path = Some(crop(
                page,
                page_rgb_md5.expect("visual page has an RGB digest"),
                "chart",
                AssetKind::Chart,
                bbox,
                assets,
                max_asset_bytes,
            )?);
            block.sub_type = raw.sub_type.clone().filter(|value| !value.is_empty());
        }
        "equation" => {
            block.kind = "interline_equation".into();
            block.lines = vec![Line {
                bbox,
                spans: raw
                    .content
                    .as_deref()
                    .map(equation_content)
                    .map(Span::InterlineEquation)
                    .into_iter()
                    .collect(),
            }];
            block.image_path = Some(crop(
                page,
                page_rgb_md5.expect("visual page has an RGB digest"),
                "interline_equation",
                AssetKind::Equation,
                bbox,
                assets,
                max_asset_bytes,
            )?);
        }
        "equation_block" => {
            // Layout containers carry geometry, not recognition text.
            block.kind = "equation_block".into();
        }
        "code" | "algorithm" => {
            block.kind = "code_body".into();
            let content = code_content(raw.content.as_deref().unwrap_or_default());
            let spans = text_spans(content);
            block.sub_type = Some(
                if source_type == "code"
                    && spans
                        .iter()
                        .any(|span| matches!(span, Span::InlineEquation(_)))
                {
                    "algorithm"
                } else {
                    source_type
                }
                .into(),
            );
            block.guess_lang = raw
                .extra
                .get("guess_lang")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .map(str::to_owned);
            block.lines = vec![Line { bbox, spans }];
        }
        _ => return err(format!("unknown raw type: {source_type}")),
    }
    Ok(block)
}

fn is_chrome(kind: &str) -> bool {
    matches!(
        kind,
        "header" | "footer" | "page_number" | "aside_text" | "page_footnote"
    )
}

fn is_visual_body(kind: &str) -> bool {
    matches!(
        kind,
        "image_body" | "image_block_body" | "table_body" | "chart_body" | "code_body"
    )
}

fn visual_kind(kind: &str) -> &'static str {
    match kind {
        "table_body" => "table",
        "chart_body" => "chart",
        "code_body" => "code",
        _ => "image",
    }
}

fn overlap_ratio(inner: [i32; 4], outer: [i32; 4]) -> f64 {
    let left = inner[0].max(outer[0]);
    let top = inner[1].max(outer[1]);
    let right = inner[2].min(outer[2]);
    let bottom = inner[3].min(outer[3]);
    let overlap = (right - left).max(0) as f64 * (bottom - top).max(0) as f64;
    let area = (inner[2] - inner[0]) as f64 * (inner[3] - inner[1]) as f64;
    if area == 0.0 { 0.0 } else { overlap / area }
}

fn overlaps(first: [i32; 4], second: [i32; 4]) -> bool {
    first[0] < second[2] && first[2] > second[0] && first[1] < second[3] && first[3] > second[1]
}

fn outside_visual_gap(between: [i32; 4], child: [i32; 4], parent: [i32; 4]) -> bool {
    let gap = if child[3] <= parent[1] {
        Some((child[3], parent[1]))
    } else if parent[3] <= child[1] {
        Some((parent[3], child[1]))
    } else {
        None
    };
    gap.is_some_and(|(top, bottom)| {
        !overlaps(between, child)
            && !overlaps(between, parent)
            && (between[1] >= bottom || between[3] <= top)
    })
}

fn visual_neighbor(child: &Block, parent: &Block, blocks: &[Block]) -> bool {
    if child.kind == "footnote" && child.index < parent.index {
        return false;
    }
    let allowed = if child.kind == "caption" {
        ["caption"].as_slice()
    } else {
        ["caption", "footnote"].as_slice()
    };
    let start = child.index.min(parent.index);
    let end = child.index.max(parent.index);
    for between in blocks
        .iter()
        .filter(|block| block.index > start && block.index < end)
    {
        if allowed.contains(&between.kind.as_str())
            || outside_visual_gap(between.bbox, child.bbox, parent.bbox)
        {
            continue;
        }
        return false;
    }
    true
}

fn bbox_distance(first: [i32; 4], second: [i32; 4]) -> f64 {
    let dx = if first[2] < second[0] {
        (second[0] - first[2]) as f64
    } else if second[2] < first[0] {
        (first[0] - second[2]) as f64
    } else {
        0.0
    };
    let dy = if first[3] < second[1] {
        (second[1] - first[3]) as f64
    } else if second[3] < first[1] {
        (first[1] - second[3]) as f64
    } else {
        0.0
    };
    (dx * dx + dy * dy).sqrt()
}

fn center_distance(first: [i32; 4], second: [i32; 4]) -> f64 {
    let x = (first[0] + first[2] - second[0] - second[2]) as f64;
    let y = (first[1] + first[3] - second[1] - second[3]) as f64;
    (x * x + y * y).sqrt()
}

fn effective_index_distance(child: &Block, parent: &Block, blocks: &[Block]) -> usize {
    let start = child.index.min(parent.index);
    let end = child.index.max(parent.index);
    let matching_children = blocks
        .iter()
        .filter(|block| block.index > start && block.index < end && block.kind == child.kind)
        .count();
    end - start - matching_children
}

fn best_visual_parent<'a>(
    child: &Block,
    parents: &'a [Block],
    blocks: &[Block],
) -> Option<&'a Block> {
    let candidates: Vec<_> = parents
        .iter()
        .filter(|parent| visual_neighbor(child, parent, blocks))
        .collect();
    let minimum = candidates
        .iter()
        .map(|parent| effective_index_distance(child, parent, blocks))
        .min()?;
    let closest: Vec<_> = candidates
        .into_iter()
        .filter(|parent| effective_index_distance(child, parent, blocks) == minimum)
        .collect();
    if closest.len() == 1 {
        return closest.into_iter().next();
    }
    let nearest = closest
        .iter()
        .map(|parent| bbox_distance(child.bbox, parent.bbox))
        .fold((f64::INFINITY, f64::NEG_INFINITY), |(low, high), value| {
            (low.min(value), high.max(value))
        });
    if nearest.1 - nearest.0 > 2.0 {
        return closest.into_iter().min_by(|left, right| {
            bbox_distance(child.bbox, left.bbox)
                .total_cmp(&bbox_distance(child.bbox, right.bbox))
                .then_with(|| left.index.cmp(&right.index))
        });
    }
    if child.kind == "caption"
        && closest
            .iter()
            .all(|parent| visual_kind(&parent.kind) == "table")
    {
        return closest.into_iter().max_by_key(|parent| parent.index);
    }
    if child.kind == "footnote" {
        return closest.into_iter().min_by_key(|parent| parent.index);
    }
    closest.into_iter().min_by(|left, right| {
        center_distance(child.bbox, left.bbox)
            .total_cmp(&center_distance(child.bbox, right.bbox))
            .then_with(|| left.index.cmp(&right.index))
    })
}

fn node_index(node: &Node) -> usize {
    match node {
        Node::Leaf(block) => block.index,
        Node::List { marker, .. } => marker.index,
        Node::Visual { body, .. } => body.index,
        Node::Tombstone { index, .. } => *index,
    }
}

fn span_value(span: &Span, bbox: [i32; 4], image_path: Option<&str>) -> Value {
    let mut value = json!({"bbox":bbox});
    match span {
        Span::Text(content) => {
            value["type"] = json!("text");
            value["content"] = json!(content);
        }
        Span::InlineEquation(content) => {
            value["type"] = json!("inline_equation");
            value["content"] = json!(content);
        }
        Span::InterlineEquation(content) => {
            value["type"] = json!("interline_equation");
            value["content"] = json!(content);
        }
        Span::Table(html) => {
            value["type"] = json!("table");
            value["html"] = json!(html);
        }
        Span::Image(content) => {
            value["type"] = json!("image");
            if let Some(content) = content {
                value["content"] = json!(content);
            }
        }
        Span::Chart(content) => {
            value["type"] = json!("chart");
            if let Some(content) = content {
                value["content"] = json!(content);
            }
        }
    }
    if matches!(
        span,
        Span::InterlineEquation(_) | Span::Table(_) | Span::Image(_) | Span::Chart(_)
    ) && let Some(image_path) = image_path
    {
        value["image_path"] = json!(image_path);
    }
    value
}

fn block_value(block: &Block, kind: Option<&str>) -> Value {
    let lines: Vec<_> = block
        .lines
        .iter()
        .map(|line| {
            json!({
                "bbox":line.bbox,
                "spans":line.spans.iter().map(|span| { let mut value = span_value(span, line.bbox, block.image_path.as_deref()); if block.cross_page { value["cross_page"] = json!(true); } value }).collect::<Vec<_>>(),
            })
        })
        .collect();
    let mut value = json!({
        "bbox":block.bbox,
        "type":kind.unwrap_or(&block.kind),
        "angle":block.angle,
        "lines":lines,
        "index":block.index,
    });
    if block.kind == "text"
        && let Some(merge_prev) = block.merge_prev
    {
        value["merge_prev"] = json!(merge_prev);
    }
    if block.lines_deleted {
        value["lines_deleted"] = json!(true);
    }
    if block.kind == "table_body" && block.cell_merge.as_ref().is_some_and(json_truthy) {
        value["cell_merge"] = block.cell_merge.clone().expect("checked truthy cell merge");
    }
    value
}

fn node_value(node: &Node) -> Value {
    match node {
        Node::Leaf(block) => block_value(block, None),
        Node::List { marker, items } => json!({
            "bbox":marker.bbox,
            "type":"list",
            "angle":marker.angle,
            "index":marker.index,
            "blocks":items.iter().map(|item| block_value(item, None)).collect::<Vec<_>>(),
            "sub_type":marker.sub_type,
        }),
        Node::Visual {
            kind,
            body,
            captions,
            footnotes,
            sub_type,
            cell_merge,
            sub_images,
        } => {
            let mut children = Vec::new();
            let body_kind = format!("{kind}_body");
            children.push(block_value(body, Some(&body_kind)));
            for caption in captions {
                children.push(block_value(caption, Some(&format!("{kind}_caption"))));
            }
            for footnote in footnotes {
                children.push(block_value(footnote, Some(&format!("{kind}_footnote"))));
            }
            children.sort_by_key(|child| child["index"].as_u64());
            let mut value = json!({
                "type":kind,
                "bbox":body.bbox,
                "blocks":children,
                "index":body.index,
            });
            if matches!(kind.as_str(), "image" | "chart")
                && body.kind != "image_block_body"
                && sub_type.as_deref().is_some_and(|value| !value.is_empty())
            {
                value["sub_type"] = json!(sub_type);
            }
            if kind == "table" && cell_merge.is_some() {
                value["cell_merge"] = cell_merge.clone().unwrap_or(Value::Null);
            }
            if kind == "image" && !sub_images.is_empty() {
                value["sub_images"] = json!(sub_images);
            }
            value
        }
        Node::Tombstone {
            kind,
            bbox,
            angle,
            index,
            sub_type,
        } => {
            let mut value = json!({
                "type":kind,
                "bbox":bbox,
                "index":index,
                "blocks":[],
                "lines_deleted":true,
            });
            if kind == "list" {
                value["angle"] = json!(angle);
                value["sub_type"] = json!(sub_type);
            }
            value
        }
    }
}

fn node_bbox(node: &Node) -> [i32; 4] {
    match node {
        Node::Leaf(block) => block.bbox,
        Node::List { marker, .. } => marker.bbox,
        Node::Visual { body, .. } => body.bbox,
        Node::Tombstone { bbox, .. } => *bbox,
    }
}

fn normalized(bbox: [i32; 4], size: [f32; 2]) -> [i32; 4] {
    [
        (bbox[0] as f32 * 1000.0 / size[0]) as i32,
        (bbox[1] as f32 * 1000.0 / size[1]) as i32,
        (bbox[2] as f32 * 1000.0 / size[0]) as i32,
        (bbox[3] as f32 * 1000.0 / size[1]) as i32,
    ]
}

fn render_block(block: &Block, formula_enable: bool) -> String {
    merge_para_with_text(block, formula_enable, true, false)
}

fn render_list_item(block: &Block) -> String {
    merge_para_with_text(block, true, true, true)
}

fn markdown_text(content: &str) -> String {
    let normalized: String = content
        .chars()
        .map(|c| match c {
            '\u{ff10}'..='\u{ff19}' | '\u{ff21}'..='\u{ff3a}' | '\u{ff41}'..='\u{ff5a}' => {
                char::from_u32(c as u32 - 0xfee0).expect("full-width ASCII")
            }
            _ => c,
        })
        .collect();
    let mut output = String::new();
    let mut backslashes = 0;
    for c in normalized.chars() {
        if matches!(c, '*' | '_' | '`' | '~' | '$') && backslashes % 2 == 0 {
            output.push('\\');
        }
        output.push(c);
        backslashes = if c == '\\' { backslashes + 1 } else { 0 };
    }
    output
}

fn is_cjk(character: char) -> bool {
    matches!(character,
        '\u{3400}'..='\u{4dbf}' | '\u{4e00}'..='\u{9fff}' | '\u{f900}'..='\u{faff}' |
        '\u{3040}'..='\u{309f}' | '\u{30a0}'..='\u{30ff}' | '\u{ac00}'..='\u{d7af}'
    )
}

fn has_following_joinable_span(block: &Block, line_idx: usize, span_idx: usize) -> bool {
    block.lines[line_idx..]
        .iter()
        .enumerate()
        .flat_map(|(offset, line)| line.spans[if offset == 0 { span_idx + 1 } else { 0 }..].iter())
        .any(|span| match span {
            Span::Text(content) => !normalize_full_width(content).trim().is_empty(),
            Span::InlineEquation(content) => !content.trim().is_empty(),
            _ => false,
        })
}

fn escape_leading_block_marker(value: &mut String) {
    let prefix = value
        .char_indices()
        .take_while(|(_, character)| matches!(character, ' ' | '\t'))
        .take(4)
        .last()
        .map_or(0, |(index, character)| index + character.len_utf8());
    if value[..prefix].chars().count() > 3 {
        return;
    }
    let rest = &value[prefix..];
    let marker_len = if rest.starts_with('+') || rest.starts_with('-') {
        1
    } else {
        rest.chars()
            .take_while(|character| *character == '#')
            .count()
    };
    if marker_len > 0 && marker_len <= 6 && rest[marker_len..].starts_with([' ', '\t']) {
        value.insert(prefix, '\\');
    }
}

fn merge_para_with_text(
    block: &Block,
    formula_enable: bool,
    escape_block_marker: bool,
    is_list_child: bool,
) -> String {
    let escape_markdown_text = block.kind != "code_body";
    let aggregate_text: String = block
        .lines
        .iter()
        .flat_map(|line| &line.spans)
        .filter_map(|span| match span {
            Span::Text(content) => Some(normalize_full_width(content)),
            _ => None,
        })
        .collect();
    // ponytail: fastText compatibility ceiling; replace with the bundled source model when exact language classification is required.
    let cjk_block = aggregate_text.chars().any(is_cjk);
    let mut output = String::new();
    for (line_idx, line) in block.lines.iter().enumerate() {
        for (span_idx, span) in line.spans.iter().enumerate() {
            let mut value = match span {
                Span::Text(content) => {
                    let content = normalize_full_width(content);
                    if escape_markdown_text {
                        markdown_text(&content)
                    } else {
                        content
                    }
                }
                Span::InlineEquation(content) => format!("${content}$"),
                Span::InterlineEquation(content) if formula_enable => {
                    format!("\n$$\n{content}\n$$\n")
                }
                Span::InterlineEquation(_) => block
                    .image_path
                    .as_ref()
                    .map(|path| format!("![](images/{path})"))
                    .unwrap_or_default(),
                _ => String::new(),
            };
            value = value.trim().into();
            if value.is_empty() {
                continue;
            }
            let is_last_span = span_idx + 1 == line.spans.len();
            let following = has_following_joinable_span(block, line_idx, span_idx);
            let hyphenated_line_end = matches!(span, Span::Text(_))
                && span_idx + 1 == line.spans.len()
                && value.ends_with(['-', '\u{00ad}', '\u{2010}', '\u{2011}', '\u{2043}'])
                && value
                    [..value.len() - value.chars().last().expect("ends with hyphen").len_utf8()]
                    .split_whitespace()
                    .last()
                    .is_some_and(|word| word.bytes().all(|byte| byte.is_ascii_alphabetic()));
            if cjk_block {
                output.push_str(&value);
                if following && (!is_last_span || matches!(span, Span::InlineEquation(_))) {
                    output.push(' ');
                }
            } else if hyphenated_line_end {
                let next_starts_lowercase = block
                    .lines
                    .get(line_idx + 1)
                    .and_then(|line| line.spans.first())
                    .and_then(|span| match span {
                        Span::Text(content) if !normalize_full_width(content).is_empty() => {
                            normalize_full_width(content).chars().next()
                        }
                        _ => None,
                    })
                    .is_some_and(|character| character.is_ascii_lowercase());
                if next_starts_lowercase {
                    value.pop();
                }
                output.push_str(&value);
            } else {
                output.push_str(&value);
                if following {
                    output.push(' ');
                }
            }
        }
    }
    if escape_block_marker && !is_list_child {
        escape_leading_block_marker(&mut output);
    }
    output
}

fn normalize_full_width(content: &str) -> String {
    content
        .chars()
        .map(|c| match c {
            '\u{ff10}'..='\u{ff19}' | '\u{ff21}'..='\u{ff3a}' | '\u{ff41}'..='\u{ff5a}' => {
                char::from_u32(c as u32 - 0xfee0).expect("full-width ASCII")
            }
            _ => c,
        })
        .collect()
}

fn v2_spans(block: &Block) -> Vec<Value> {
    let mut output: Vec<Value> = Vec::new();
    let mut usable = Vec::new();
    for line in &block.lines {
        for span in &line.spans {
            match span {
                Span::Text(content) if !content.trim().is_empty() => {
                    usable.push(("text", content.clone()))
                }
                Span::InlineEquation(content) if !content.trim().is_empty() => {
                    usable.push(("equation_inline", content.clone()))
                }
                _ => {}
            }
        }
    }
    for (index, (kind, mut content)) in usable.into_iter().enumerate() {
        if kind == "text" && index + 1 < usable_len(block) {
            content.push(' ');
        }
        if block.kind == "phonetic" && kind == "text" {
            output.push(json!({"type":"phonetic","content":content}));
        } else if let Some(previous) = output
            .last_mut()
            .filter(|previous| previous["type"] == kind && kind == "text")
        {
            let previous_content = previous["content"].as_str().unwrap_or_default().to_owned();
            previous["content"] = json!(format!("{previous_content}{content}"));
        } else {
            output.push(json!({"type":kind,"content":content}));
        }
    }
    output
}

fn usable_len(block: &Block) -> usize {
    block
        .lines
        .iter()
        .flat_map(|line| &line.spans)
        .filter(|span| match span {
            Span::Text(content) | Span::InlineEquation(content) => !content.trim().is_empty(),
            _ => false,
        })
        .count()
}

fn body_data(node: &Node) -> (&Block, Option<&str>, Option<&str>) {
    let Node::Visual { body, .. } = node else {
        unreachable!("visual body requested for a non-visual node")
    };
    let mut content = None;
    let mut html = None;
    for line in &body.lines {
        for span in &line.spans {
            match span {
                Span::Table(value) => html = Some(value.as_str()),
                Span::Image(Some(value)) | Span::Chart(Some(value)) => {
                    content = Some(value.as_str())
                }
                Span::Text(value) => content = Some(value.as_str()),
                _ => {}
            }
        }
    }
    (body, content, html)
}

fn visual_children(node: &Node) -> (&[Block], &[Block]) {
    match node {
        Node::Visual {
            captions,
            footnotes,
            ..
        } => (captions, footnotes),
        _ => unreachable!("visual children requested for a non-visual node"),
    }
}

fn flat_node(node: &Node, page: &OfficialBuildPage) -> Value {
    let bbox = normalized(node_bbox(node), page.page_size_points);
    let mut value = match node {
        Node::Leaf(block) if block.kind == "title" => {
            json!({"type":"text","text":render_block(block, true),"text_level":1})
        }
        Node::Leaf(block) if block.kind == "interline_equation" => {
            json!({"type":"equation","text":render_block(block, true),"text_format":"latex"})
        }
        Node::Leaf(block) => json!({"type":block.kind,"text":render_block(block, true)}),
        Node::List { marker, items } => json!({
            "type":"list",
            "sub_type":marker.sub_type.clone().unwrap_or_default(),
            "list_items":items.iter().map(render_list_item).collect::<Vec<_>>(),
        }),
        Node::Visual { kind, .. } if kind == "table" => {
            let (body, _, html) = body_data(node);
            let (captions, footnotes) = visual_children(node);
            let mut table = json!({
                "type":"table",
                "img_path":body.image_path.as_ref().map(|path| format!("images/{path}")).unwrap_or_default(),
                "table_caption":captions.iter().map(|caption| render_block(caption, true)).collect::<Vec<_>>(),
                "table_footnote":footnotes.iter().map(|footnote| render_block(footnote, true)).collect::<Vec<_>>(),
            });
            if let Some(html) = html.filter(|html| !html.is_empty()) {
                table["table_body"] = json!(output_html(html));
            }
            table
        }
        Node::Visual { kind, sub_type, .. } if kind == "image" || kind == "chart" => {
            let (body, content, _) = body_data(node);
            let (captions, footnotes) = visual_children(node);
            let caption_key = format!("{kind}_caption");
            let footnote_key = format!("{kind}_footnote");
            let mut visual = json!({
                "type":kind,
                "img_path":body.image_path.as_ref().map(|path| format!("images/{path}")).unwrap_or_default(),
                "content":content.unwrap_or_default(),
            });
            visual[caption_key] = json!(
                captions
                    .iter()
                    .map(|caption| render_block(caption, true))
                    .collect::<Vec<_>>()
            );
            visual[footnote_key] = json!(
                footnotes
                    .iter()
                    .map(|footnote| render_block(footnote, true))
                    .collect::<Vec<_>>()
            );
            if sub_type.as_deref().is_some_and(|value| !value.is_empty()) {
                visual["sub_type"] = json!(sub_type);
            }
            visual
        }
        Node::Visual { sub_type, .. } => {
            let (body, _, _) = body_data(node);
            let (captions, _) = visual_children(node);
            json!({
                "type":"code",
                "sub_type":sub_type.clone().unwrap_or_else(|| "code".into()),
                "code_body":code_markdown(body, sub_type.as_deref()),
                "code_caption":captions.iter().map(|caption| render_block(caption, true)).collect::<Vec<_>>(),
            })
        }
        Node::Tombstone { kind, sub_type, .. } if kind == "list" => json!({
            "type":"list",
            "sub_type":sub_type.clone().unwrap_or_default(),
            "list_items":[],
        }),
        Node::Tombstone { kind, .. } => json!({
            "type":kind,
            "img_path":"",
            "table_caption":[],
            "table_footnote":[],
        }),
    };
    value["bbox"] = json!(bbox);
    value["page_idx"] = json!(page.slice_page_idx);
    value
}

fn code_markdown(body: &Block, sub_type: Option<&str>) -> String {
    if sub_type == Some("algorithm") {
        render_algorithm_html_from_lines(body)
    } else {
        let content = merge_para_with_text(body, true, true, false);
        format!(
            "```{}\n{content}\n```",
            body.guess_lang.as_deref().unwrap_or("txt")
        )
    }
}

fn render_algorithm_html_from_lines(body: &Block) -> String {
    let mut inner = String::new();
    let mut previous_was_inline_equation = false;
    for line in &body.lines {
        for span in &line.spans {
            match span {
                Span::Text(content) => inner.push_str(&html_escape(&normalize_full_width(content))),
                Span::InlineEquation(content) => {
                    if previous_was_inline_equation {
                        inner.push(' ');
                    }
                    inner.push('$');
                    inner.push_str(&html_escape(content));
                    inner.push('$');
                }
                _ => {}
            }
            previous_was_inline_equation = matches!(span, Span::InlineEquation(_));
        }
    }
    if inner.trim().is_empty() {
        String::new()
    } else {
        format!(
            "<div class=\"mineru-algorithm\" style=\"white-space: pre-wrap; font-family:monospace;\">\n{inner}\n</div>"
        )
    }
}

fn html_escape(content: &str) -> String {
    content
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn v2_node(node: &Node, page: &OfficialBuildPage) -> Value {
    let mut value = match node {
        Node::Leaf(block) if block.kind == "title" => json!({
            "type":"title",
            "content":{"title_content":v2_spans(block),"level":1},
        }),
        Node::Leaf(block) if block.kind == "interline_equation" => json!({
            "type":"equation_interline",
            "content":{
                "math_content":equation_math(block),
                "math_type":"latex",
                "image_source":{"path":block.image_path.as_ref().map(|path| format!("images/{path}")).unwrap_or_default()},
            },
        }),
        Node::Leaf(block) if block.kind == "ref_text" => json!({
            "type":"list",
            "content":{"list_type":"reference_list","list_items":[{"item_type":"text","item_content":v2_spans(block)}]},
        }),
        Node::Leaf(block) if is_chrome(&block.kind) => {
            let output_type = match block.kind.as_str() {
                "header" => "page_header",
                "footer" => "page_footer",
                "aside_text" => "page_aside_text",
                "page_number" => "page_number",
                "page_footnote" => "page_footnote",
                _ => unreachable!(),
            };
            let mut content = json!({});
            content[format!("{output_type}_content")] = json!(v2_spans(block));
            json!({"type":output_type,"content":content})
        }
        Node::Leaf(block) => json!({
            "type":"paragraph",
            "content":{"paragraph_content":v2_spans(block)},
        }),
        Node::List { marker, items } => json!({
            "type":"list",
            "content":{
                "list_type":if marker.sub_type.as_deref() == Some("ref_text") { "reference_list" } else { "text_list" },
                "list_items":items.iter().map(|item| json!({"item_type":"text","item_content":v2_spans(item)})).collect::<Vec<_>>(),
            },
        }),
        Node::Visual { kind, .. } if kind == "table" => {
            let (body, _, html) = body_data(node);
            let (captions, footnotes) = visual_children(node);
            let html = output_html(html.unwrap_or_default());
            let nest = if html.matches("<table").count() > 1 {
                2
            } else {
                1
            };
            json!({
                "type":"table",
                "content":{
                    "image_source":{"path":body.image_path.as_ref().map(|path| format!("images/{path}")).unwrap_or_default()},
                    "table_caption":captions.iter().flat_map(v2_spans).collect::<Vec<_>>(),
                    "table_footnote":footnotes.iter().flat_map(v2_spans).collect::<Vec<_>>(),
                    "html":html,
                    "table_type":if html.contains("colspan") || html.contains("rowspan") || nest > 1 { "complex_table" } else { "simple_table" },
                    "table_nest_level":nest,
                },
            })
        }
        Node::Visual { kind, sub_type, .. } if kind == "image" || kind == "chart" => {
            let (body, content, _) = body_data(node);
            let (captions, footnotes) = visual_children(node);
            let caption_key = format!("{kind}_caption");
            let footnote_key = format!("{kind}_footnote");
            let mut content_map = json!({
                "image_source":{"path":body.image_path.as_ref().map(|path| format!("images/{path}")).unwrap_or_default()},
                "content":content.unwrap_or_default(),
            });
            content_map[caption_key] =
                json!(captions.iter().flat_map(v2_spans).collect::<Vec<_>>());
            content_map[footnote_key] =
                json!(footnotes.iter().flat_map(v2_spans).collect::<Vec<_>>());
            let mut visual = json!({
                "type":kind,
                "content":content_map,
            });
            if sub_type.as_deref().is_some_and(|value| !value.is_empty()) {
                visual["sub_type"] = json!(sub_type);
            }
            visual
        }
        Node::Visual { sub_type, .. } => {
            let (body, _, _) = body_data(node);
            let (captions, _) = visual_children(node);
            if sub_type.as_deref() == Some("algorithm") {
                json!({"type":"algorithm","content":{"algorithm_caption":captions.iter().flat_map(v2_spans).collect::<Vec<_>>(),"algorithm_content":v2_spans(body)}})
            } else {
                json!({"type":"code","content":{"code_caption":captions.iter().flat_map(v2_spans).collect::<Vec<_>>(),"code_content":v2_spans(body),"code_language":"txt"}})
            }
        }
        Node::Tombstone { kind, sub_type, .. } if kind == "list" => {
            json!({"type":"list","content":{"list_type":if sub_type.as_deref() == Some("ref_text") { "reference_list" } else { "text_list" },"list_items":[]}})
        }
        Node::Tombstone { kind, .. } => json!({
            "type":kind,
            "content":{
                "image_source":{"path":""},
                "table_caption":[],
                "table_footnote":[],
                "html":"",
                "table_type":"simple_table",
                "table_nest_level":1,
            },
        }),
    };
    value["bbox"] = json!(normalized(node_bbox(node), page.page_size_points));
    value
}

fn equation_math(block: &Block) -> &str {
    block
        .lines
        .iter()
        .flat_map(|line| &line.spans)
        .find_map(|span| match span {
            Span::InterlineEquation(content) => Some(content.as_str()),
            _ => None,
        })
        .unwrap_or_default()
}

fn markdown_node(node: &Node, formula_enable: bool, table_enable: bool) -> String {
    match node {
        Node::Leaf(block) if block.kind == "title" => format!("# {}", render_block(block, true)),
        Node::Leaf(block)
            if matches!(
                block.kind.as_str(),
                "text" | "ref_text" | "phonetic" | "interline_equation"
            ) =>
        {
            render_block(block, formula_enable)
        }
        Node::List { items, .. } => items
            .iter()
            .map(render_list_item)
            .filter(|item| !item.is_empty())
            .collect::<Vec<_>>()
            .join("  \n"),
        Node::Visual { kind, .. } => {
            let (body, _, html) = body_data(node);
            let (captions, footnotes) = visual_children(node);
            let mut parts: Vec<(usize, String, bool)> = captions
                .iter()
                .map(|caption| (caption.index, render_block(caption, true), false))
                .filter(|(_, content, _)| !content.is_empty())
                .collect();
            let body_content = match kind.as_str() {
                "table" if table_enable => html.map(output_html).unwrap_or_default(),
                "table" => body
                    .image_path
                    .as_ref()
                    .map(|path| format!("![](images/{path})"))
                    .unwrap_or_default(),
                "image" | "chart" => {
                    let image = body
                        .image_path
                        .as_ref()
                        .map(|path| format!("![](images/{path})"));
                    let text = body_data(node).1.unwrap_or_default();
                    let summary = match node {
                        Node::Visual {
                            sub_type: Some(sub_type),
                            ..
                        } => sub_type.as_str(),
                        _ if kind == "chart" => "chart content",
                        _ => "image content",
                    };
                    let details = (!text.trim().is_empty()).then(|| {
                        format!("<details>\n<summary>{summary}</summary>\n\n{text}\n</details>")
                    });
                    [image, details]
                        .into_iter()
                        .flatten()
                        .collect::<Vec<_>>()
                        .join("\n\n")
                }
                _ => code_markdown(
                    body,
                    match node {
                        Node::Visual { sub_type, .. } => sub_type.as_deref(),
                        _ => None,
                    },
                ),
            };
            if !body_content.is_empty() {
                parts.push((body.index, body_content, kind == "table" && table_enable));
            }
            parts.extend(
                footnotes
                    .iter()
                    .map(|footnote| (footnote.index, render_block(footnote, true), false))
                    .filter(|(_, content, _)| !content.is_empty()),
            );
            parts.sort_by_key(|(index, _, _)| *index);
            let mut output = String::new();
            let mut prior_html = false;
            for (_, content, is_html) in parts {
                if !output.is_empty() {
                    output.push_str(if prior_html || is_html {
                        "\n\n"
                    } else {
                        "  \n"
                    });
                }
                output.push_str(&content);
                prior_html = is_html;
            }
            output
        }
        Node::Tombstone { .. } => String::new(),
        _ => String::new(),
    }
}

fn can_merge_text(current: &Block, previous: &Block) -> bool {
    let Some(first) = current.lines.first() else {
        return false;
    };
    let Some(last) = previous.lines.last() else {
        return false;
    };
    let first_content = render_block(current, true);
    let last_content = render_block(previous, true);
    let current_width = current.bbox[2] - current.bbox[0];
    let previous_width = previous.bbox[2] - previous.bbox[0];
    !first.spans.is_empty()
        && !last.spans.is_empty()
        && !first_content.is_empty()
        && !last_content.is_empty()
        && !last_content.ends_with([
            '.', '!', '?', '。', '！', '？', ')', '）', '"', '”', ':', '：', ';', '；',
        ])
        && !first_content
            .starts_with(|character: char| character.is_ascii_digit() || character.is_uppercase())
        && current_width > 0
        && previous_width > 0
        && (current_width - previous_width).abs() < current_width.min(previous_width)
        && current.bbox[1] < previous.bbox[3]
}

fn merge_para_text(nodes: &mut [Node]) {
    for current_index in (0..nodes.len()).rev() {
        let current = match &nodes[current_index] {
            Node::Leaf(block) if block.kind == "text" && block.merge_prev == Some(true) => block,
            _ => continue,
        };
        let mut previous_index = None;
        for index in (0..current_index).rev() {
            match &nodes[index] {
                Node::Visual { .. } => continue,
                Node::Leaf(block) if block.kind == "text" => {
                    if can_merge_text(current, block) {
                        previous_index = Some(index);
                    }
                    break;
                }
                _ => break,
            }
        }
        let Some(previous_index) = previous_index else {
            continue;
        };
        let (before, current_and_after) = nodes.split_at_mut(current_index);
        let Node::Leaf(previous) = &mut before[previous_index] else {
            unreachable!();
        };
        let Node::Leaf(current) = &mut current_and_after[0] else {
            unreachable!();
        };
        previous.lines.append(&mut current.lines);
    }
}

fn block_text(block: &Block) -> String {
    render_block(block, true)
}

fn near_above(child: &Block, parent: &Block) -> bool {
    let width = (parent.bbox[2] - parent.bbox[0]).max(1);
    let height = (child.bbox[3] - child.bbox[1]).max(1);
    child.bbox[3] <= parent.bbox[1]
        && parent.bbox[1] - child.bbox[3] <= (height * 3 / 2).max(12)
        && child.bbox[2] >= parent.bbox[0] - (width * 3 / 100).max(12)
        && child.bbox[0] <= parent.bbox[2] + (width * 3 / 100).max(12)
}

fn caption_fallbacks(blocks: &mut [Block]) {
    // Pinned fallbacks run before visual parent selection; they only reclassify
    // geometrically adjacent text fragments and never discard a block.
    for index in 1..blocks.len().saturating_sub(1) {
        if blocks[index].kind != "text" || blocks[index - 1].kind != "caption" {
            continue;
        }
        if is_visual_body(&blocks[index + 1].kind)
            && near_above(&blocks[index], &blocks[index + 1])
            && blocks[index - 1].bbox[1] < blocks[index].bbox[3]
            && blocks[index - 1].bbox[3] > blocks[index].bbox[1]
        {
            blocks[index].kind = "caption".into();
            blocks[index].merge_prev = None;
        }
    }
    for table_index in 0..blocks.len() {
        if blocks[table_index].kind != "table_body" {
            continue;
        }
        let mut saw_caption = false;
        for index in (0..table_index).rev() {
            if !near_above(&blocks[index], &blocks[table_index]) {
                break;
            }
            if blocks[index].kind == "caption" {
                saw_caption = true;
            } else if saw_caption
                && matches!(blocks[index].kind.as_str(), "text" | "footnote")
                && blocks[index].lines.len() <= 1
            {
                blocks[index].kind = "caption".into();
                blocks[index].merge_prev = None;
            }
        }
        let first_effective = blocks.iter().position(|block| !is_chrome(&block.kind));
        if let Some(first) = first_effective
            && first < table_index
            && blocks[first].kind == "text"
            && near_above(&blocks[first], &blocks[table_index])
            && matches!(block_text(&blocks[first]).to_ascii_lowercase().as_str(), text if text.contains("continued") || text.contains("continuation") || text.contains("续表"))
        {
            blocks[first].kind = "caption".into();
            blocks[first].merge_prev = None;
        }
    }
}

fn node_is_empty(node: &Node) -> bool {
    match node {
        Node::Leaf(block) => block.lines.iter().all(|line| line.spans.is_empty()),
        Node::List { items, .. } => items.is_empty(),
        Node::Visual { .. } => false,
        Node::Tombstone { .. } => true,
    }
}

fn is_ref_list(node: &Node) -> bool {
    matches!(node, Node::List { marker, items } if !items.is_empty() && marker.sub_type.as_deref() == Some("ref_text"))
}

fn table_html_mut(node: &mut Node) -> Option<&mut String> {
    let Node::Visual { kind, body, .. } = node else {
        return None;
    };
    if kind != "table" {
        return None;
    }
    body.lines
        .iter_mut()
        .flat_map(|line| &mut line.spans)
        .find_map(|span| match span {
            Span::Table(html) => Some(html),
            _ => None,
        })
}

fn continuation_caption(node: &Node) -> bool {
    let Node::Visual { kind, captions, .. } = node else {
        return false;
    };
    kind == "table"
        && captions.iter().any(|caption| {
            let text = block_text(caption).to_ascii_lowercase();
            is_continuation_text(&text)
        })
}

fn is_continuation_text(text: &str) -> bool {
    let text: String = text
        .trim()
        .chars()
        .map(|c| match c {
            '！'..='～' => char::from_u32(c as u32 - 0xfee0).unwrap(),
            '　' => ' ',
            _ => c,
        })
        .collect::<String>()
        .to_ascii_lowercase();
    [
        "(续)",
        "(续表)",
        "(续上表)",
        "(continued)",
        "(cont.)",
        "(cont’d)",
        "(…continued)",
        "continued",
        "续表",
    ]
    .iter()
    .any(|marker| {
        text.ends_with(marker)
            && (*marker != "continued"
                || text.len() == marker.len()
                || !text[..text.len() - marker.len()]
                    .chars()
                    .last()
                    .is_some_and(char::is_alphabetic))
    }) || text.contains("(continued)")
}

fn table_children(node: &Node) -> Option<(&[Block], &[Block])> {
    match node {
        Node::Visual {
            kind,
            captions,
            footnotes,
            ..
        } if kind == "table" => Some((captions, footnotes)),
        _ => None,
    }
}

fn last_surviving_node(paras: &[Vec<Node>], before_page: usize) -> Option<(usize, usize)> {
    (0..before_page).rev().find_map(|page| {
        paras[page]
            .iter()
            .rposition(|node| !node_is_empty(node))
            .map(|index| (page, index))
    })
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum HtmlTagKind {
    Open,
    Close,
    Other,
}

#[derive(Clone)]
struct HtmlTag {
    start: usize,
    end: usize,
    kind: HtmlTagKind,
    name: String,
    self_closing: bool,
}

fn html_tags(html: &str) -> Option<Vec<HtmlTag>> {
    let bytes = html.as_bytes();
    let mut tags = Vec::new();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] != b'<' {
            index += html[index..].chars().next()?.len_utf8();
            continue;
        }
        let start = index;
        if bytes.get(index + 1..index + 4) == Some(b"!--") {
            let end = html[index + 4..].find("-->")? + index + 7;
            tags.push(HtmlTag {
                start,
                end,
                kind: HtmlTagKind::Other,
                name: String::new(),
                self_closing: true,
            });
            index = end;
            continue;
        }
        let mut end = index + 1;
        let mut quote = None;
        while end < bytes.len() {
            match (quote, bytes[end]) {
                (Some(delimiter), byte) if byte == delimiter => quote = None,
                (None, b'\'' | b'\"') => quote = Some(bytes[end]),
                (None, b'>') => break,
                _ => {}
            }
            end += 1;
        }
        if end == bytes.len() {
            return None;
        }
        let mut cursor = index + 1;
        while bytes.get(cursor).is_some_and(u8::is_ascii_whitespace) {
            cursor += 1;
        }
        if matches!(bytes.get(cursor), Some(b'!' | b'?')) {
            tags.push(HtmlTag {
                start,
                end: end + 1,
                kind: HtmlTagKind::Other,
                name: String::new(),
                self_closing: true,
            });
            index = end + 1;
            continue;
        }
        let close = bytes.get(cursor) == Some(&b'/');
        cursor += usize::from(close);
        while bytes.get(cursor).is_some_and(u8::is_ascii_whitespace) {
            cursor += 1;
        }
        let name_start = cursor;
        while bytes
            .get(cursor)
            .is_some_and(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b':' | b'-' | b'_'))
        {
            cursor += 1;
        }
        if cursor == name_start {
            return None;
        }
        let name = html[name_start..cursor].to_ascii_lowercase();
        let self_closing = !close && (is_void_tag(&name) || html[..end].trim_end().ends_with('/'));
        tags.push(HtmlTag {
            start,
            end: end + 1,
            kind: if close {
                HtmlTagKind::Close
            } else {
                HtmlTagKind::Open
            },
            name,
            self_closing,
        });
        index = end + 1;
    }
    Some(tags)
}

fn is_void_tag(name: &str) -> bool {
    matches!(
        name,
        "area"
            | "base"
            | "br"
            | "col"
            | "embed"
            | "hr"
            | "img"
            | "input"
            | "link"
            | "meta"
            | "param"
            | "source"
            | "track"
            | "wbr"
    )
}

#[derive(Clone)]
struct TableDocument {
    source: String,
    row_ranges: Vec<(usize, usize)>,
    insertion: usize,
}

impl TableDocument {
    fn parse(source: &str) -> Option<Self> {
        let tags = html_tags(source)?;
        let root = tags.first()?;
        if !source[..root.start].trim().is_empty()
            || root.kind != HtmlTagKind::Open
            || root.name != "table"
            || root.self_closing
        {
            return None;
        }
        let mut stack: Vec<(String, usize)> = Vec::new();
        let mut rows = Vec::new();
        let mut first_tbody = None;
        let mut insertion = None;
        let mut table_close = None;
        let mut table_end = None;
        for tag in tags.iter() {
            match tag.kind {
                HtmlTagKind::Other => {}
                HtmlTagKind::Open => {
                    if tag.name == "table" && !stack.is_empty() {
                        return None;
                    }
                    if !tag.self_closing {
                        if tag.name == "tbody" && first_tbody.is_none() {
                            first_tbody = Some(tag.start);
                        }
                        stack.push((tag.name.clone(), tag.start));
                    }
                }
                HtmlTagKind::Close => {
                    let (name, opened) = stack.pop()?;
                    if name != tag.name {
                        return None;
                    }
                    if name == "tr" {
                        rows.push((opened, tag.end));
                    }
                    if name == "tbody" && Some(opened) == first_tbody {
                        insertion = Some(tag.start);
                    }
                    if name == "table" {
                        if !stack.is_empty() {
                            return None;
                        }
                        table_close = Some(tag.start);
                        table_end = Some(tag.end);
                        break;
                    }
                }
            }
        }
        let table_end = table_end?;
        if !source[table_end..].trim().is_empty() || rows.is_empty() {
            return None;
        }
        rows.sort_unstable_by_key(|(start, _)| *start);
        Some(Self {
            source: source.into(),
            row_ranges: rows,
            insertion: insertion.unwrap_or(table_close?),
        })
    }

    fn serialize(&self, rows: &[TableRow]) -> Option<String> {
        if rows.len() < self.row_ranges.len() {
            return None;
        }
        let mut output = String::with_capacity(self.source.len());
        let mut cursor = 0;
        let mut inserted = false;
        let append = |output: &mut String| {
            for row in &rows[self.row_ranges.len()..] {
                if !row.deleted {
                    output.push_str(&row.raw);
                }
            }
        };
        for (index, (start, end)) in self.row_ranges.iter().copied().enumerate() {
            if !inserted && self.insertion <= start {
                output.push_str(&self.source[cursor..self.insertion]);
                append(&mut output);
                output.push_str(&self.source[self.insertion..start]);
                cursor = start;
                inserted = true;
            }
            output.push_str(&self.source[cursor..start]);
            if !rows[index].deleted {
                output.push_str(&rows[index].raw);
            }
            cursor = end;
        }
        if !inserted {
            output.push_str(&self.source[cursor..self.insertion]);
            append(&mut output);
            cursor = self.insertion;
        }
        output.push_str(&self.source[cursor..]);
        Some(output)
    }
}

#[derive(Clone)]
struct TableRow {
    raw: String,
    deleted: bool,
}

#[derive(Clone)]
struct ParsedCell {
    start: usize,
    open_end: usize,
    content_end: usize,
    end: usize,
    colspan: usize,
    rowspan: usize,
}

struct ParsedRow {
    cells: Vec<ParsedCell>,
    close_start: usize,
}

fn parse_row(row: &str) -> Option<ParsedRow> {
    let tags = html_tags(row)?;
    let root = tags.first()?;
    if !row[..root.start].trim().is_empty()
        || root.kind != HtmlTagKind::Open
        || root.name != "tr"
        || root.self_closing
    {
        return None;
    }
    let mut stack: Vec<(String, usize, usize)> = Vec::new();
    let mut cells = Vec::new();
    let mut active_cell = None;
    let mut close_start = None;
    for tag in tags {
        match tag.kind {
            HtmlTagKind::Other => {}
            HtmlTagKind::Open => {
                if !tag.self_closing {
                    if matches!(tag.name.as_str(), "td" | "th") && stack.len() == 1 {
                        if active_cell.is_some() {
                            return None;
                        }
                        let open = &row[tag.start..tag.end];
                        active_cell = Some((
                            tag.start,
                            tag.end,
                            span_attribute(open, "colspan")?,
                            span_attribute(open, "rowspan")?,
                        ));
                    }
                    stack.push((tag.name, tag.start, tag.end));
                }
            }
            HtmlTagKind::Close => {
                let (name, start, open_end) = stack.pop()?;
                if name != tag.name {
                    return None;
                }
                if matches!(name.as_str(), "td" | "th") {
                    let (cell_start, _, colspan, rowspan) = active_cell.take()?;
                    cells.push(ParsedCell {
                        start: cell_start,
                        open_end,
                        content_end: tag.start,
                        end: tag.end,
                        colspan,
                        rowspan,
                    });
                }
                if name == "tr" {
                    if !stack.is_empty() || !row[tag.end..].trim().is_empty() {
                        return None;
                    }
                    close_start = Some(tag.start);
                    break;
                }
                let _ = start;
            }
        }
    }
    Some(ParsedRow {
        cells,
        close_start: close_start?,
    })
}

fn span_attribute(open: &str, wanted: &str) -> Option<usize> {
    let (range, value) = html_attribute(open, wanted)?;
    match (range.start == open.len(), value) {
        (true, _) => Some(1),
        (false, Some(value)) => value
            .trim()
            .parse::<usize>()
            .ok()
            .filter(|value| *value > 0),
        (false, None) => None,
    }
}

fn html_attribute<'a>(
    open: &'a str,
    wanted: &str,
) -> Option<(std::ops::Range<usize>, Option<&'a str>)> {
    let bytes = open.as_bytes();
    let mut index = 1;
    while bytes
        .get(index)
        .is_some_and(|byte| !byte.is_ascii_whitespace() && *byte != b'>' && *byte != b'/')
    {
        index += 1;
    }
    while index < bytes.len() {
        while bytes.get(index).is_some_and(u8::is_ascii_whitespace) {
            index += 1;
        }
        if matches!(bytes.get(index), None | Some(b'>' | b'/')) {
            break;
        }
        let start = index;
        while bytes
            .get(index)
            .is_some_and(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b':' | b'-' | b'_'))
        {
            index += 1;
        }
        if start == index {
            return None;
        }
        let name = &open[start..index];
        while bytes.get(index).is_some_and(u8::is_ascii_whitespace) {
            index += 1;
        }
        let value = if bytes.get(index) == Some(&b'=') {
            index += 1;
            while bytes.get(index).is_some_and(u8::is_ascii_whitespace) {
                index += 1;
            }
            let value_start = index;
            if matches!(bytes.get(index), Some(b'\'' | b'\"')) {
                let quote = bytes[index];
                index += 1;
                let quoted_start = index;
                while bytes.get(index) != Some(&quote) {
                    index += 1;
                    if index >= bytes.len() {
                        return None;
                    }
                }
                let result = &open[quoted_start..index];
                index += 1;
                Some(result)
            } else {
                while bytes
                    .get(index)
                    .is_some_and(|byte| !byte.is_ascii_whitespace() && *byte != b'>')
                {
                    index += 1;
                }
                Some(&open[value_start..index])
            }
        } else {
            None
        };
        if name.eq_ignore_ascii_case(wanted) {
            return Some((start..index, value));
        }
    }
    Some((open.len()..open.len(), None))
}

fn rewrite_span(cell: &str, name: &str, value: usize) -> Option<String> {
    let tag = html_tags(cell)?.first()?.clone();
    let open = &cell[..tag.end];
    let (range, existing) = html_attribute(open, name)?;
    let mut output = cell.to_owned();
    if existing.is_some() {
        let value_start = open[range.clone()]
            .find('=')
            .map(|offset| range.start + offset + 1)?;
        let mut start = value_start;
        while output
            .as_bytes()
            .get(start)
            .is_some_and(u8::is_ascii_whitespace)
        {
            start += 1;
        }
        let quote = output
            .as_bytes()
            .get(start)
            .copied()
            .filter(|byte| matches!(byte, b'\'' | b'\"'));
        start += usize::from(quote.is_some());
        let mut end = start;
        while output.as_bytes().get(end).is_some_and(|byte| match quote {
            Some(delimiter) => *byte != delimiter,
            None => !byte.is_ascii_whitespace() && *byte != b'>',
        }) {
            end += 1;
        }
        output.replace_range(start..end, &value.to_string());
    } else {
        output.insert_str(tag.end - 1, &format!(" {name}=\"{value}\""));
    }
    Some(output)
}

fn remove_span(cell: &str, name: &str) -> Option<String> {
    let tag = html_tags(cell)?.first()?.clone();
    let (range, existing) = html_attribute(&cell[..tag.end], name)?;
    if existing.is_none() {
        return Some(cell.into());
    }
    let mut output = cell.to_owned();
    let start = if range.start > 0 && output.as_bytes()[range.start - 1].is_ascii_whitespace() {
        range.start - 1
    } else {
        range.start
    };
    output.replace_range(start..range.end, "");
    Some(output)
}

fn row_cell_content(row: &TableRow, cell_index: usize) -> Option<String> {
    let cell = parse_row(&row.raw)?.cells.get(cell_index)?.clone();
    Some(row.raw[cell.open_end..cell.content_end].into())
}

fn replace_cell_content(row: &mut TableRow, cell_index: usize, content: &str) -> Option<()> {
    let cell = parse_row(&row.raw)?.cells.get(cell_index)?.clone();
    row.raw
        .replace_range(cell.open_end..cell.content_end, content);
    Some(())
}

fn set_row_cell_span(
    row: &mut TableRow,
    cell_index: usize,
    name: &str,
    value: usize,
) -> Option<()> {
    let cell = parse_row(&row.raw)?.cells.get(cell_index)?.clone();
    let replacement = if value == 1 {
        remove_span(&row.raw[cell.start..cell.end], name)?
    } else {
        rewrite_span(&row.raw[cell.start..cell.end], name, value)?
    };
    row.raw.replace_range(cell.start..cell.end, &replacement);
    Some(())
}

fn cell_text(content: &str) -> String {
    let Some(tags) = html_tags(content) else {
        return html_unescape(content);
    };
    let mut output = String::new();
    let mut cursor = 0;
    for tag in tags {
        output.push_str(&content[cursor..tag.start]);
        cursor = tag.end;
    }
    output.push_str(&content[cursor..]);
    html_unescape(&output)
}

fn full_to_half(value: &str) -> String {
    value
        .chars()
        .map(|character| match character {
            '！'..='～' => char::from_u32(character as u32 - 0xfee0).expect("full-width ASCII"),
            _ => character,
        })
        .collect()
}

fn cell_has_semantic_content(row: &TableRow, cell_index: usize) -> Option<bool> {
    let content = row_cell_content(row, cell_index)?;
    if !cell_text(&content).trim().is_empty() {
        return Some(true);
    }
    Some(html_tags(&content)?.iter().any(|tag| {
        tag.kind == HtmlTagKind::Open
            && matches!(
                tag.name.as_str(),
                "img" | "svg" | "math" | "eq" | "table" | "figure" | "object" | "embed" | "canvas"
            )
    }))
}

type TailOccupied = BTreeMap<usize, BTreeSet<usize>>;

#[derive(Clone)]
struct RowMetrics {
    row_idx: usize,
    effective_cols: usize,
    actual_cols: usize,
    visual_cols: usize,
}

#[derive(Clone)]
struct RowSignature {
    effective_cols: usize,
    colspans: Vec<usize>,
    rowspans: Vec<usize>,
    normalized_texts: Vec<String>,
    display_texts: Vec<String>,
}

struct RowScan {
    effective: Vec<usize>,
    metrics: Vec<RowMetrics>,
    total_cols: usize,
    last_nonempty: Option<RowMetrics>,
    tail: TailOccupied,
}

struct TableMergeState {
    document: TableDocument,
    rows: Vec<TableRow>,
    total_cols: usize,
    front_headers: Vec<RowSignature>,
    front_metrics: BTreeMap<usize, RowMetrics>,
    last_data: Option<RowMetrics>,
    effective: Vec<usize>,
    tail: TailOccupied,
}

fn scan_rows(rows: &[TableRow], initial: &TailOccupied, start_row_idx: usize) -> Option<RowScan> {
    let mut occupied: BTreeMap<usize, BTreeSet<usize>> = initial.clone();
    let mut total_cols = occupied
        .values()
        .flat_map(|cols| cols.iter())
        .max()
        .map_or(0, |column| column + 1);
    let mut effective = Vec::with_capacity(rows.len());
    let mut metrics = Vec::with_capacity(rows.len());
    let mut last_nonempty = None;
    for (row_idx, row) in rows.iter().enumerate() {
        let parsed = parse_row(&row.raw)?;
        let occupied_row = occupied.entry(row_idx).or_default().clone();
        let mut column = 0;
        let mut actual_cols = 0;
        for cell in &parsed.cells {
            while occupied_row.contains(&column)
                || occupied
                    .get(&row_idx)
                    .is_some_and(|columns| columns.contains(&column))
            {
                column += 1;
            }
            actual_cols += cell.colspan;
            for offset in 0..cell.rowspan {
                occupied
                    .entry(row_idx + offset)
                    .or_default()
                    .extend(column..column + cell.colspan);
            }
            column += cell.colspan;
            total_cols = total_cols.max(column);
        }
        let effective_cols = occupied
            .get(&row_idx)
            .and_then(|columns| columns.last().copied())
            .map_or(0, |column| column + 1);
        total_cols = total_cols.max(effective_cols);
        effective.push(effective_cols);
        let metric = RowMetrics {
            row_idx: start_row_idx + row_idx,
            effective_cols,
            actual_cols,
            visual_cols: parsed.cells.len(),
        };
        if !parsed.cells.is_empty() {
            last_nonempty = Some(metric.clone());
        }
        metrics.push(metric);
    }
    let tail = occupied
        .into_iter()
        .filter_map(|(row_idx, columns)| {
            if row_idx >= rows.len() && !columns.is_empty() {
                Some((row_idx - rows.len(), columns))
            } else {
                None
            }
        })
        .collect();
    Some(RowScan {
        effective,
        metrics,
        total_cols,
        last_nonempty,
        tail,
    })
}

fn row_signature(row: &TableRow, effective_cols: usize) -> Option<RowSignature> {
    let parsed = parse_row(&row.raw)?;
    let mut colspans = Vec::new();
    let mut rowspans = Vec::new();
    let mut normalized_texts = Vec::new();
    let mut display_texts = Vec::new();
    for (index, cell) in parsed.cells.iter().enumerate() {
        let text = cell_text(&row.raw[cell.open_end..cell.content_end]);
        colspans.push(cell.colspan);
        rowspans.push(cell.rowspan);
        normalized_texts.push(full_to_half(&text).split_whitespace().collect::<String>());
        display_texts.push(full_to_half(text.trim()));
        let _ = index;
    }
    Some(RowSignature {
        effective_cols,
        colspans,
        rowspans,
        normalized_texts,
        display_texts,
    })
}

fn build_front_cache(
    rows: &[TableRow],
) -> Option<(Vec<RowSignature>, BTreeMap<usize, RowMetrics>)> {
    let limit = rows.len().min(6);
    let scan = scan_rows(&rows[..limit], &TailOccupied::new(), 0)?;
    let headers = (0..limit.min(5))
        .map(|index| row_signature(&rows[index], scan.effective[index]))
        .collect::<Option<Vec<_>>>()?;
    Some((headers, scan.metrics.into_iter().enumerate().collect()))
}

fn build_table_state(html: &str) -> Option<TableMergeState> {
    let document = TableDocument::parse(html)?;
    let rows = document
        .row_ranges
        .iter()
        .map(|(start, end)| TableRow {
            raw: document.source[*start..*end].into(),
            deleted: false,
        })
        .collect::<Vec<_>>();
    let scan = scan_rows(&rows, &TailOccupied::new(), 0)?;
    let (front_headers, front_metrics) = build_front_cache(&rows)?;
    Some(TableMergeState {
        document,
        rows,
        total_cols: scan.total_cols,
        front_headers,
        front_metrics,
        last_data: scan.last_nonempty,
        effective: scan.effective,
        tail: scan.tail,
    })
}

fn refresh_state(state: &mut TableMergeState) -> Option<()> {
    let scan = scan_rows(&state.rows, &TailOccupied::new(), 0)?;
    let (front_headers, front_metrics) = build_front_cache(&state.rows)?;
    state.total_cols = scan.total_cols;
    state.last_data = scan.last_nonempty;
    state.effective = scan.effective;
    state.tail = scan.tail;
    state.front_headers = front_headers;
    state.front_metrics = front_metrics;
    Some(())
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct CellSource(isize, usize);

fn visual_sources(
    rows: &[TableRow],
    target: usize,
    initial: &TailOccupied,
) -> Option<(BTreeMap<usize, CellSource>, usize)> {
    if target >= rows.len() {
        return None;
    }
    let mut occupied: BTreeMap<usize, BTreeMap<usize, CellSource>> = initial
        .iter()
        .map(|(row, columns)| {
            (
                *row,
                columns
                    .iter()
                    .map(|column| (*column, CellSource(-1, *column)))
                    .collect(),
            )
        })
        .collect();
    let mut total_cols = occupied
        .values()
        .flat_map(|columns| columns.keys())
        .max()
        .map_or(0, |column| column + 1);
    for (row_idx, row) in rows.iter().enumerate().take(target + 1) {
        let mut column = 0;
        let cells = parse_row(&row.raw)?.cells;
        for (cell_idx, cell) in cells.iter().enumerate() {
            while occupied
                .get(&row_idx)
                .is_some_and(|columns| columns.contains_key(&column))
            {
                column += 1;
            }
            let source = CellSource(row_idx as isize, cell_idx);
            for offset in 0..cell.rowspan {
                for target_column in column..column + cell.colspan {
                    occupied
                        .entry(row_idx + offset)
                        .or_default()
                        .insert(target_column, source);
                }
            }
            column += cell.colspan;
            total_cols = total_cols.max(column);
        }
    }
    Some((occupied.remove(&target).unwrap_or_default(), total_cols))
}

fn visual_mapping(rows: &[TableRow], target: usize, initial: &TailOccupied) -> Option<Vec<usize>> {
    let (occupied, _) = visual_sources(rows, target, initial)?;
    let cells = parse_row(&rows[target].raw)?.cells;
    let mut column = 0;
    let mut mapping = Vec::with_capacity(cells.len());
    for cell in cells {
        while occupied
            .get(&column)
            .is_some_and(|source| source.0 < target as isize)
        {
            column += 1;
        }
        mapping.push(column);
        column += cell.colspan;
    }
    Some(mapping)
}

fn rendered_segments(rows: &[TableRow], target: usize, initial: &TailOccupied) -> Option<usize> {
    let (occupied, total_cols) = visual_sources(rows, target, initial)?;
    let mut previous = None;
    let mut count = 0;
    for column in 0..total_cols {
        let source = occupied.get(&column).copied();
        if source != previous {
            if source.is_some() {
                count += 1;
            }
            previous = source;
        }
    }
    Some(count)
}

fn detect_table_headers(
    previous: &TableMergeState,
    current: &TableMergeState,
) -> Option<(usize, Vec<Vec<String>>)> {
    let limit = previous
        .front_headers
        .len()
        .min(current.front_headers.len())
        .min(5);
    let mut count = 0;
    let mut texts = Vec::new();
    for index in 0..limit {
        let left = &previous.front_headers[index];
        let right = &current.front_headers[index];
        if left.colspans.len() == right.colspans.len()
            && left.effective_cols == right.effective_cols
            && left.colspans == right.colspans
            && left.rowspans == right.rowspans
            && left.normalized_texts == right.normalized_texts
        {
            count += 1;
            texts.push(left.display_texts.clone());
        } else {
            break;
        }
    }
    if count > 0 {
        return Some((count, texts));
    }
    for index in 0..limit {
        let left = &previous.front_headers[index];
        let right = &current.front_headers[index];
        if left.normalized_texts == right.normalized_texts
            && rendered_segments(&previous.rows, index, &TailOccupied::new())?
                == rendered_segments(&current.rows, index, &TailOccupied::new())?
        {
            count += 1;
            texts.push(left.display_texts.clone());
        } else {
            break;
        }
    }
    Some((count, texts))
}

fn expand_header_rowspans(rows: &[TableRow], mut count: usize) -> Option<usize> {
    let mut index = 0;
    while index < count.min(rows.len()) {
        for cell in parse_row(&rows[index].raw)?.cells {
            count = count.max(index + cell.rowspan).min(rows.len());
        }
        index += 1;
    }
    Some(count)
}

fn rows_match(previous: &TableMergeState, current: &TableMergeState) -> Option<bool> {
    let last = previous.last_data.as_ref()?;
    let (header_count, _) = detect_table_headers(previous, current)?;
    let first = current
        .front_metrics
        .get(&expand_header_rowspans(&current.rows, header_count)?)?;
    Some(
        last.effective_cols == first.effective_cols
            || last.actual_cols == first.actual_cols
            || rendered_segments(&previous.rows, last.row_idx, &TailOccupied::new())?
                == rendered_segments(&current.rows, first.row_idx, &TailOccupied::new())?,
    )
}

fn compatible_tables(previous: &TableMergeState, current: &TableMergeState) -> Option<bool> {
    if previous.total_cols == current.total_cols {
        Some(true)
    } else {
        rows_match(previous, current)
    }
}

fn adjust_colspans(
    rows: &mut [TableRow],
    start: usize,
    effective: &[usize],
    structure_reference: &TableRow,
    match_reference: &TableRow,
    target_cols: usize,
) -> Option<()> {
    let reference_cells = parse_row(&structure_reference.raw)?.cells;
    let match_cells = parse_row(&match_reference.raw)?.cells;
    let reference_spans = reference_cells
        .iter()
        .map(|cell| cell.colspan)
        .collect::<Vec<_>>();
    for (row_idx, row) in rows.iter_mut().enumerate().skip(start) {
        let parsed = parse_row(&row.raw)?;
        if parsed.cells.is_empty() {
            continue;
        }
        if parsed.cells.len() != reference_cells.len() {
            continue;
        }
        let actual = parsed.cells.iter().map(|cell| cell.colspan).sum::<usize>();
        if effective.get(row_idx).copied().unwrap_or(0) >= target_cols || actual >= target_cols {
            continue;
        }
        let spans_match = parsed.cells.len() == match_cells.len()
            && parsed
                .cells
                .iter()
                .map(|cell| cell.colspan)
                .eq(match_cells.iter().map(|cell| cell.colspan));
        if spans_match {
            for (index, span) in reference_spans.iter().copied().enumerate() {
                if span > 1 {
                    set_row_cell_span(row, index, "colspan", span)?;
                }
            }
        } else {
            let difference = target_cols - effective.get(row_idx).copied().unwrap_or(0);
            let last = parsed.cells.len() - 1;
            set_row_cell_span(
                row,
                last,
                "colspan",
                parsed.cells[last].colspan + difference,
            )?;
        }
    }
    Some(())
}

fn insert_cell_before_visual_column(
    rows: &mut [TableRow],
    target: usize,
    start_column: usize,
    cell: &str,
) -> Option<()> {
    let mapping = visual_mapping(rows, target, &TailOccupied::new())?;
    let parsed = parse_row(&rows[target].raw)?;
    let insert_at = mapping
        .iter()
        .position(|column| *column >= start_column)
        .map(|index| parsed.cells[index].start)
        .unwrap_or(parsed.close_start);
    rows[target].raw.insert_str(insert_at, cell);
    Some(())
}

fn carry_blank_rowspans(rows: &mut [TableRow], row_idx: usize) -> Option<()> {
    if row_idx + 1 >= rows.len() {
        return Some(());
    }
    let parsed = parse_row(&rows[row_idx].raw)?;
    let mapping = visual_mapping(rows, row_idx, &TailOccupied::new())?;
    let mut carried = Vec::new();
    for (index, cell) in parsed.cells.iter().enumerate() {
        if cell.rowspan > 1 && !cell_has_semantic_content(&rows[row_idx], index)? {
            let raw = &rows[row_idx].raw[cell.start..cell.end];
            let copied = if cell.rowspan == 2 {
                remove_span(raw, "rowspan")?
            } else {
                rewrite_span(raw, "rowspan", cell.rowspan - 1)?
            };
            carried.push((mapping[index], copied));
        }
    }
    for (column, cell) in carried.into_iter().rev() {
        insert_cell_before_visual_column(rows, row_idx + 1, column, &cell)?;
    }
    Some(())
}

fn clip_overlapped_blank_rowspans(rows: &mut [TableRow], initial: &TailOccupied) -> Option<bool> {
    let mut moves = Vec::new();
    let mut removals = Vec::new();
    for row_idx in 0..rows.len() {
        let parsed = parse_row(&rows[row_idx].raw)?;
        let mapping = visual_mapping(rows, row_idx, &TailOccupied::new())?;
        for (cell_idx, cell) in parsed.cells.iter().enumerate() {
            if cell.rowspan <= 1 || cell_has_semantic_content(&rows[row_idx], cell_idx)? {
                continue;
            }
            let columns = mapping[cell_idx]..mapping[cell_idx] + cell.colspan;
            let mut overlap = 0;
            while overlap < cell.rowspan
                && initial.get(&(row_idx + overlap)).is_some_and(|occupied| {
                    columns.clone().all(|column| occupied.contains(&column))
                })
            {
                overlap += 1;
            }
            if overlap == 0 || (cell.rowspan > overlap && row_idx + overlap >= rows.len()) {
                continue;
            }
            removals.push((row_idx, cell_idx));
            if cell.rowspan > overlap {
                let raw = &rows[row_idx].raw[cell.start..cell.end];
                let copied = if cell.rowspan == overlap + 1 {
                    remove_span(raw, "rowspan")?
                } else {
                    rewrite_span(raw, "rowspan", cell.rowspan - overlap)?
                };
                moves.push((row_idx + overlap, mapping[cell_idx], copied));
            }
        }
    }
    if removals.is_empty() {
        return Some(false);
    }
    removals.sort_unstable_by(|left, right| right.cmp(left));
    for (row_idx, cell_idx) in removals {
        let cell = parse_row(&rows[row_idx].raw)?.cells.get(cell_idx)?.clone();
        rows[row_idx].raw.replace_range(cell.start..cell.end, "");
    }
    moves.sort_unstable_by(|left, right| right.0.cmp(&left.0).then_with(|| right.1.cmp(&left.1)));
    for (row_idx, column, cell) in moves {
        insert_cell_before_visual_column(rows, row_idx, column, &cell)?;
    }
    Some(true)
}

fn apply_cell_merge(
    previous: &mut TableMergeState,
    current: &mut TableMergeState,
    header_count: usize,
    flags: Option<&Value>,
) -> Option<()> {
    let Some(flags) = flags.and_then(Value::as_array) else {
        return Some(());
    };
    if header_count >= current.rows.len() || previous.rows.is_empty() {
        return Some(());
    }
    let previous_row = previous.rows.len() - 1;
    let previous_cells = parse_row(&previous.rows[previous_row].raw)?.cells;
    let current_cells = parse_row(&current.rows[header_count].raw)?.cells;
    let previous_mapping = visual_mapping(&previous.rows, previous_row, &TailOccupied::new())?;
    let current_mapping = visual_mapping(&current.rows[header_count..], 0, &previous.tail)?;
    let mut previous_by_column = BTreeMap::new();
    let mut current_by_column = BTreeMap::new();
    for (index, cell) in previous_cells.iter().enumerate() {
        for column in previous_mapping[index]..previous_mapping[index] + cell.colspan {
            previous_by_column.insert(column, index);
        }
    }
    for (index, cell) in current_cells.iter().enumerate() {
        for column in current_mapping[index]..current_mapping[index] + cell.colspan {
            current_by_column.insert(column, index);
        }
    }
    let mut pairs = BTreeSet::new();
    for (column, flag) in flags.iter().enumerate() {
        if (flag.as_i64() == Some(1) || flag.as_bool() == Some(true))
            && let (Some(&left), Some(&right)) = (
                previous_by_column.get(&column),
                current_by_column.get(&column),
            )
        {
            pairs.insert((left, right));
        }
    }
    for (left, right) in pairs.iter().copied() {
        let content = row_cell_content(&current.rows[header_count], right)?;
        let mut previous_content = row_cell_content(&previous.rows[previous_row], left)?;
        previous_content.push_str(&content);
        replace_cell_content(&mut previous.rows[previous_row], left, &previous_content)?;
    }
    for right in pairs
        .into_iter()
        .map(|(_, right)| right)
        .collect::<BTreeSet<_>>()
    {
        replace_cell_content(&mut current.rows[header_count], right, "")?;
    }
    let semantic = (0..parse_row(&current.rows[header_count].raw)?.cells.len())
        .any(|index| cell_has_semantic_content(&current.rows[header_count], index).unwrap_or(true));
    if !semantic {
        carry_blank_rowspans(&mut current.rows, header_count)?;
        current.rows[header_count].deleted = true;
    }
    Some(())
}

fn merged_table_html(previous: &Node, current: &Node) -> Option<String> {
    let (previous_body, _, previous_html) = body_data(previous);
    let (current_body, _, current_html) = body_data(current);
    let previous_width = previous_body.bbox[2] - previous_body.bbox[0];
    let current_width = current_body.bbox[2] - current_body.bbox[0];
    if previous_width <= 0
        || current_width <= 0
        || (previous_width - current_width).abs() * 10 >= previous_width.min(current_width)
    {
        return None;
    }
    let mut previous_state = build_table_state(previous_html?)?;
    let mut current_state = build_table_state(current_html?)?;
    if !compatible_tables(&previous_state, &current_state)? {
        return None;
    }
    let (header_count, _) = detect_table_headers(&previous_state, &current_state)?;
    let header_count = expand_header_rowspans(&current_state.rows, header_count)?;
    if header_count < current_state.rows.len()
        && clip_overlapped_blank_rowspans(
            &mut current_state.rows[header_count..],
            &previous_state.tail,
        )?
    {
        refresh_state(&mut current_state)?;
    }
    if header_count < current_state.rows.len() && !previous_state.rows.is_empty() {
        if previous_state.total_cols > current_state.total_cols {
            let reference = previous_state.rows.last()?.clone();
            let match_reference = current_state.rows.get(header_count)?.clone();
            adjust_colspans(
                &mut current_state.rows,
                header_count,
                &current_state.effective,
                &reference,
                &match_reference,
                previous_state.total_cols,
            )?;
        } else if current_state.total_cols > previous_state.total_cols {
            let reference = current_state.rows.get(header_count)?.clone();
            let match_reference = previous_state.rows.last()?.clone();
            adjust_colspans(
                &mut previous_state.rows,
                0,
                &previous_state.effective,
                &reference,
                &match_reference,
                current_state.total_cols,
            )?;
            refresh_state(&mut previous_state)?;
        }
    }
    let flags = match current {
        Node::Visual { cell_merge, .. } => cell_merge.as_ref().filter(|value| json_truthy(value)),
        _ => None,
    };
    apply_cell_merge(&mut previous_state, &mut current_state, header_count, flags)?;
    previous_state.rows.extend(
        current_state
            .rows
            .into_iter()
            .skip(header_count)
            .filter(|row| !row.deleted),
    );
    refresh_state(&mut previous_state)?;
    previous_state.document.serialize(&previous_state.rows)
}

fn tombstone(node: &Node) -> Node {
    match node {
        Node::List { marker, .. } => Node::Tombstone {
            kind: "list".into(),
            bbox: marker.bbox,
            angle: marker.angle,
            index: marker.index,
            sub_type: marker.sub_type.clone(),
        },
        Node::Visual { kind, body, .. } => Node::Tombstone {
            kind: kind.clone(),
            bbox: body.bbox,
            angle: body.angle,
            index: body.index,
            sub_type: None,
        },
        _ => unreachable!("only structural roots become tombstones"),
    }
}

fn merge_document_paras(
    paras: &mut [Vec<Node>],
    table_enable: bool,
    deadline: Option<std::time::Instant>,
) -> VlmResult<()> {
    for nodes in paras.iter_mut() {
        check_builder_deadline(deadline)?;
        merge_para_text(nodes);
    }
    for page in 0..paras.len() {
        check_builder_deadline(deadline)?;
        let mut index = 0;
        while index < paras[page].len() {
            if !is_ref_list(&paras[page][index]) {
                index += 1;
                continue;
            }
            let previous = if index > 0 {
                Some((page, index - 1))
            } else {
                last_surviving_node(paras, page)
            };
            let Some((previous_page, previous_index)) = previous else {
                index += 1;
                continue;
            };
            if !is_ref_list(&paras[previous_page][previous_index]) {
                index += 1;
                continue;
            }
            if previous_page == page {
                let (before, current) = paras[page].split_at_mut(index);
                let moved = match &mut current[0] {
                    Node::List { items, .. } => std::mem::take(items),
                    _ => unreachable!(),
                };
                let deleted = tombstone(&current[0]);
                let Node::List { items, .. } = &mut before[previous_index] else {
                    unreachable!()
                };
                items.extend(moved);
                current[0] = deleted;
            } else {
                let (before, current_pages) = paras.split_at_mut(page);
                let current = &mut current_pages[0][index];
                let mut moved = match current {
                    Node::List { items, .. } => std::mem::take(items),
                    _ => unreachable!(),
                };
                for item in &mut moved {
                    item.cross_page = true;
                }
                let deleted = tombstone(current);
                let Node::List { items, .. } = &mut before[previous_page][previous_index] else {
                    unreachable!()
                };
                items.extend(moved);
                *current = deleted;
            }
            index += 1;
        }
    }
    if !table_enable {
        return Ok(());
    }
    for page in (1..paras.len()).rev() {
        check_builder_deadline(deadline)?;
        if !matches!(paras[page].first(), Some(Node::Visual { kind, .. }) if kind == "table") {
            continue;
        }
        let current_index = 0;
        let Some(previous_index) = paras[page - 1].len().checked_sub(1) else {
            continue;
        };
        if !matches!(&paras[page - 1][previous_index], Node::Visual { kind, .. } if kind == "table")
        {
            continue;
        }
        let (captions, _) = table_children(&paras[page][current_index]).expect("checked table");
        let current_body_bottom = match &paras[page][current_index] {
            Node::Visual { body, .. } => body.bbox[3],
            _ => unreachable!(),
        };
        let merge_captions = captions
            .iter()
            .filter(|caption| {
                is_continuation_text(&block_text(caption)) || caption.bbox[1] < current_body_bottom
            })
            .count();
        let (_, previous_footnotes) =
            table_children(&paras[page - 1][previous_index]).expect("checked table");
        if previous_footnotes.len() > 1
            || (merge_captions == 0 && !previous_footnotes.is_empty())
            || (merge_captions > 0 && !continuation_caption(&paras[page][current_index]))
        {
            continue;
        }
        let Some(html) = merged_table_html(
            &paras[page - 1][previous_index],
            &paras[page][current_index],
        ) else {
            continue;
        };
        let previous_body_index = match &paras[page - 1][previous_index] {
            Node::Visual { body, .. } => body.index,
            _ => unreachable!(),
        };
        *table_html_mut(&mut paras[page - 1][previous_index]).expect("checked table") = html;
        let moved_footnotes = match &paras[page][current_index] {
            Node::Visual { footnotes, .. } => footnotes.clone(),
            _ => unreachable!(),
        };
        if let Node::Visual {
            footnotes: previous_footnotes,
            ..
        } = &mut paras[page - 1][previous_index]
        {
            *previous_footnotes = moved_footnotes
                .into_iter()
                .enumerate()
                .map(|(offset, mut footnote)| {
                    footnote.cross_page = true;
                    footnote.index = previous_body_index + offset + 1;
                    footnote
                })
                .collect();
        }
        let Node::Visual { captions, .. } = &mut paras[page][current_index] else {
            unreachable!()
        };
        let old_captions = std::mem::take(captions);
        let (restored, retained): (Vec<_>, Vec<_>) =
            old_captions.into_iter().partition(|caption| {
                !is_continuation_text(&block_text(caption))
                    && caption.bbox[1] >= current_body_bottom
            });
        *captions = retained;
        for mut caption in restored {
            caption.kind = "text".into();
            caption.merge_prev = None;
            paras[page].push(Node::Leaf(caption));
        }
        let Node::Visual {
            body,
            captions,
            footnotes,
            ..
        } = &mut paras[page][current_index]
        else {
            unreachable!()
        };
        for block in std::iter::once(body)
            .chain(captions.iter_mut())
            .chain(footnotes.iter_mut())
        {
            block.lines.clear();
            block.lines_deleted = true;
        }
        paras[page].sort_by_key(node_index);
    }
    Ok(())
}

fn check_builder_deadline(deadline: Option<std::time::Instant>) -> VlmResult<()> {
    if deadline.is_some_and(|deadline| std::time::Instant::now() >= deadline) {
        Err(VlmError::Timeout {
            operation: "official PDF",
        })
    } else {
        Ok(())
    }
}

fn compose_page(
    page: &OfficialBuildPage,
    assets: &mut BTreeMap<String, Asset>,
    max_asset_bytes: usize,
) -> VlmResult<(Vec<Node>, Vec<Block>)> {
    let page_rgb_md5 = page
        .snapshot
        .iter()
        .any(|raw| {
            matches!(
                raw.block_type.as_str(),
                "table" | "image" | "image_block" | "chart" | "equation"
            )
        })
        .then(|| md5::compute(page.rgb.as_raw()));
    let mut blocks: Vec<Block> = page
        .snapshot
        .iter()
        .enumerate()
        .map(|(index, raw)| {
            raw_block(
                page,
                page_rgb_md5.as_ref(),
                raw,
                index,
                assets,
                max_asset_bytes,
            )
        })
        .collect::<VlmResult<_>>()?;
    caption_fallbacks(&mut blocks);
    let discarded = blocks
        .iter()
        .filter(|block| is_chrome(&block.kind))
        .cloned()
        .collect::<Vec<_>>();

    let mut consumed = HashSet::new();
    let mut nodes = Vec::new();
    let mut list_markers: Vec<_> = blocks
        .iter()
        .filter(|block| block.kind == "list")
        .cloned()
        .collect();
    let candidates: Vec<_> = blocks
        .iter()
        .filter(|block| matches!(block.kind.as_str(), "text" | "ref_text"))
        .cloned()
        .collect();
    for marker in &mut list_markers {
        let mut items = Vec::new();
        for candidate in &candidates {
            if consumed.contains(&candidate.index)
                || overlap_ratio(candidate.bbox, marker.bbox) < 0.8
            {
                continue;
            }
            consumed.insert(candidate.index);
            items.push(candidate.clone());
        }
        if !items.is_empty() {
            consumed.insert(marker.index); // A nonempty marker becomes the list root.
            let mut counts = BTreeMap::new();
            for item in &items {
                *counts.entry(item.kind.clone()).or_insert(0usize) += 1;
            }
            marker.sub_type = counts
                .into_iter()
                .max_by_key(|(_, count)| *count)
                .map(|(kind, _)| kind);
            nodes.push(Node::List {
                marker: marker.clone(),
                items,
            });
        } else {
            // Pinned `fix_list_blocks` drops only the structural null marker.
            consumed.insert(marker.index);
        }
    }

    let mut parents: Vec<_> = blocks
        .iter()
        .filter(|block| is_visual_body(&block.kind))
        .cloned()
        .collect();
    let mut sub_images: BTreeMap<usize, Vec<Value>> = BTreeMap::new();
    let containers: Vec<_> = parents
        .iter()
        .filter(|block| block.kind == "image_block_body")
        .collect();
    let mut absorbed_members = HashSet::new();
    for member in parents
        .iter()
        .filter(|member| matches!(member.kind.as_str(), "image_body" | "chart_body"))
    {
        let Some(container) = containers
            .iter()
            .filter(|container| overlap_ratio(member.bbox, container.bbox) >= 0.9)
            .min_by(|left, right| {
                let left_area = (left.bbox[2] - left.bbox[0]) * (left.bbox[3] - left.bbox[1]);
                let right_area = (right.bbox[2] - right.bbox[0]) * (right.bbox[3] - right.bbox[1]);
                (-overlap_ratio(member.bbox, left.bbox))
                    .total_cmp(&-overlap_ratio(member.bbox, right.bbox))
                    .then_with(|| left_area.cmp(&right_area))
                    .then_with(|| left.index.cmp(&right.index))
            })
        else {
            continue;
        };
        absorbed_members.insert(member.index);
        consumed.insert(member.index);
        let width = (container.bbox[2] - container.bbox[0]) as f64;
        let height = (container.bbox[3] - container.bbox[1]) as f64;
        sub_images.entry(container.index).or_default().push(json!({
            "type":if member.kind == "chart_body" { "chart" } else { "image" },
            "bbox":[
                (((member.bbox[0] - container.bbox[0]) as f64 / width).clamp(0.0, 1.0) * 1000.0).round() as i32 as f64 / 1000.0,
                (((member.bbox[1] - container.bbox[1]) as f64 / height).clamp(0.0, 1.0) * 1000.0).round() as i32 as f64 / 1000.0,
                (((member.bbox[2] - container.bbox[0]) as f64 / width).clamp(0.0, 1.0) * 1000.0).round() as i32 as f64 / 1000.0,
                (((member.bbox[3] - container.bbox[1]) as f64 / height).clamp(0.0, 1.0) * 1000.0).round() as i32 as f64 / 1000.0,
            ],
        }));
    }
    parents.retain(|parent| !consumed.contains(&parent.index));
    let relation_blocks: Vec<_> = blocks
        .iter()
        .filter(|block| {
            !absorbed_members.contains(&block.index) && !consumed.contains(&block.index)
        })
        .cloned()
        .collect();
    let children: Vec<_> = blocks
        .iter()
        .filter(|block| matches!(block.kind.as_str(), "caption" | "footnote"))
        .cloned()
        .collect();
    let mut grouped: BTreeMap<usize, (Vec<Block>, Vec<Block>)> = BTreeMap::new();
    for child in &children {
        let Some(parent) = best_visual_parent(child, &parents, &relation_blocks) else {
            continue;
        };
        let entry = grouped.entry(parent.index).or_default();
        if child.kind == "caption" {
            entry.0.push(child.clone());
        } else {
            entry.1.push(child.clone());
        }
        consumed.insert(child.index);
    }
    for parent in &parents {
        consumed.insert(parent.index);
        let (mut captions, mut footnotes) = grouped.remove(&parent.index).unwrap_or_default();
        captions.sort_by_key(|caption| caption.index);
        footnotes.sort_by_key(|footnote| footnote.index);
        nodes.push(Node::Visual {
            kind: visual_kind(&parent.kind).into(),
            body: parent.clone(),
            captions,
            footnotes,
            sub_type: parent.sub_type.clone(),
            cell_merge: parent.cell_merge.clone(),
            sub_images: sub_images.remove(&parent.index).unwrap_or_default(),
        });
    }

    for block in &blocks {
        if consumed.contains(&block.index) || is_chrome(&block.kind) {
            continue;
        }
        let mut leaf = block.clone();
        if matches!(leaf.kind.as_str(), "caption" | "footnote") {
            leaf.kind = "text".into();
            leaf.merge_prev = None;
        }
        nodes.push(Node::Leaf(leaf));
    }
    nodes.sort_by_key(node_index);
    Ok((nodes, discarded))
}

pub(crate) fn build_official_artifacts(
    pages: Vec<OfficialBuildPage>,
    formula_enable: bool,
    table_enable: bool,
) -> VlmResult<OfficialBuildArtifacts> {
    let mut assets: BTreeMap<String, Asset> = BTreeMap::new();
    let mut prepared = Vec::new();
    for page in pages {
        let built = prepare_official_page(page, usize::MAX, usize::MAX)?;
        for asset in built.assets {
            let path = asset.relative_path.to_string_lossy().into_owned();
            if let Some(existing) = assets.get(&path) {
                if existing.data != asset.data {
                    return err("asset path collision");
                }
            } else {
                assets.insert(path, asset);
            }
        }
        prepared.push(built.page);
    }
    let built = finalize_official_document(prepared, formula_enable, table_enable)?;
    let model_output = built
        .iter()
        .flat_map(|page| page.model_output.clone())
        .collect();
    let infos: Vec<Value> = built
        .iter()
        .flat_map(|page| {
            page.middle_json["pdf_info"]
                .as_array()
                .expect("builder created pdf_info")
                .clone()
        })
        .collect();
    let flat: Vec<Value> = built
        .iter()
        .flat_map(|page| {
            page.content_list
                .as_array()
                .expect("builder created array")
                .clone()
        })
        .collect();
    let v2: Vec<Value> = built
        .iter()
        .flat_map(|page| {
            page.content_list_v2
                .as_array()
                .expect("builder created array")
                .clone()
        })
        .collect();
    let markdown = built
        .iter()
        .map(|page| page.markdown.as_str())
        .filter(|markdown| !markdown.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n");
    Ok(OfficialBuildArtifacts {
        model_output,
        middle_json: json!({"pdf_info":infos,"_backend":"vlm","_version_name":"3.4.4"}),
        content_list: Value::Array(flat),
        content_list_v2: Value::Array(v2),
        markdown,
        assets: assets.into_values().collect(),
    })
}

pub(crate) fn prepare_official_page(
    page: OfficialBuildPage,
    max_asset_bytes: usize,
    max_text_bytes: usize,
) -> VlmResult<OfficialPreparedArtifacts> {
    prepare_official_page_until(page, max_asset_bytes, max_text_bytes, None)
}

pub(crate) fn prepare_official_page_until(
    page: OfficialBuildPage,
    max_asset_bytes: usize,
    max_text_bytes: usize,
    deadline: Option<std::time::Instant>,
) -> VlmResult<OfficialPreparedArtifacts> {
    check_builder_deadline(deadline)?;
    validate_page(&page, None)?;
    let text_bytes = page.snapshot.iter().try_fold(0usize, |total, block| {
        total
            .checked_add(block.block_type.len())
            .and_then(|total| total.checked_add(block.content.as_deref().map_or(0, str::len)))
            .and_then(|total| total.checked_add(block.sub_type.as_deref().map_or(0, str::len)))
            .and_then(|total| total.checked_add(map_text_bytes(&block.extra)?))
            .ok_or_else(|| VlmError::LimitExceeded {
                resource: "staged text/JSON bytes",
                limit: max_text_bytes as u64,
                actual: u64::MAX,
            })
    })?;
    if text_bytes > max_text_bytes {
        return Err(VlmError::LimitExceeded {
            resource: "staged text/JSON bytes",
            limit: max_text_bytes as u64,
            actual: text_bytes as u64,
        });
    }
    let mut assets = BTreeMap::new();
    let (preproc, discarded) = compose_page(&page, &mut assets, max_asset_bytes)?;
    check_builder_deadline(deadline)?;
    Ok(OfficialPreparedArtifacts {
        page: OfficialPreparedPage {
            slice_page_idx: page.slice_page_idx,
            page_size_points: page.page_size_points,
            snapshot: page.snapshot,
            preproc,
            discarded,
        },
        assets: assets.into_values().collect(),
    })
}

fn value_text_bytes(value: &Value) -> Option<usize> {
    match value {
        Value::Null | Value::Bool(_) | Value::Number(_) => Some(0),
        Value::String(value) => Some(value.len()),
        Value::Array(values) => values.iter().try_fold(0usize, |total, value| {
            total.checked_add(value_text_bytes(value)?)
        }),
        Value::Object(values) => map_text_bytes(values),
    }
}

fn map_text_bytes(values: &serde_json::Map<String, Value>) -> Option<usize> {
    values.iter().try_fold(0usize, |total, (key, value)| {
        total
            .checked_add(key.len())?
            .checked_add(value_text_bytes(value)?)
    })
}

pub(crate) fn finalize_official_document(
    pages: Vec<OfficialPreparedPage>,
    formula_enable: bool,
    table_enable: bool,
) -> VlmResult<Vec<OfficialBuildArtifacts>> {
    finalize_official_document_until(pages, formula_enable, table_enable, None)
}

pub(crate) fn finalize_official_document_until(
    mut pages: Vec<OfficialPreparedPage>,
    formula_enable: bool,
    table_enable: bool,
    deadline: Option<std::time::Instant>,
) -> VlmResult<Vec<OfficialBuildArtifacts>> {
    let mut previous = None;
    for page in &pages {
        check_builder_deadline(deadline)?;
        if previous.is_some_and(|index| page.slice_page_idx <= index) {
            return err("official pages must have increasing source indexes");
        }
        previous = Some(page.slice_page_idx);
    }
    let mut paras: Vec<Vec<Node>> = pages.iter().map(|page| page.preproc.clone()).collect();
    merge_document_paras(&mut paras, table_enable, deadline)?;
    pages
        .iter_mut()
        .zip(paras)
        .map(|(page, para)| {
            check_builder_deadline(deadline)?;
            let page_info = OfficialBuildPage {
                slice_page_idx: page.slice_page_idx,
                page_size_points: page.page_size_points,
                // Serialization only needs coordinate scaling; RGB is never revisited here.
                render_scale: 1.0,
                rgb: RgbImage::new(1, 1),
                snapshot: Vec::new(),
            };
            let middle = json!({
                "preproc_blocks": page.preproc.iter().map(node_value).collect::<Vec<_>>(),
                "discarded_blocks": page.discarded.iter().map(|block| block_value(block, None)).collect::<Vec<_>>(),
                "page_size": page.page_size_points,
                "page_idx": page.slice_page_idx,
                "para_blocks": para.iter().map(node_value).collect::<Vec<_>>(),
            });
            let mut page_v2: Vec<_> = para.iter().map(|node| v2_node(node, &page_info)).collect();
            page_v2.extend(
                page.discarded
                    .iter()
                    .map(|block| v2_node(&Node::Leaf(block.clone()), &page_info)),
            );
            let mut flat: Vec<_> = para.iter().map(|node| flat_node(node, &page_info)).collect();
            flat.extend(
                page.discarded
                    .iter()
                    .map(|block| flat_node(&Node::Leaf(block.clone()), &page_info)),
            );
            let markdown = para
                .iter()
                .map(|node| markdown_node(node, formula_enable, table_enable))
                .filter(|content| !content.trim().is_empty())
                .collect::<Vec<_>>()
                .join("\n\n");
            Ok(OfficialBuildArtifacts {
                model_output: vec![page.snapshot.clone()],
                middle_json: json!({"pdf_info":[middle],"_backend":"vlm","_version_name":"3.4.4"}),
                content_list: Value::Array(flat),
                content_list_v2: Value::Array(vec![Value::Array(page_v2)]),
                markdown,
                assets: Vec::new(),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{NormalizedBbox, Rotation};
    use image::{DynamicImage, GenericImageView, ImageFormat};
    use std::io::Cursor;

    fn block(kind: &str, bbox: [f32; 4], content: Option<&str>) -> ModelBlock {
        ModelBlock {
            block_type: kind.into(),
            bbox: Some(NormalizedBbox {
                left: bbox[0],
                top: bbox[1],
                right: bbox[2],
                bottom: bbox[3],
            }),
            angle: Some(Rotation::Deg0),
            content: content.map(str::to_owned),
            ..Default::default()
        }
    }

    fn page(blocks: Vec<ModelBlock>) -> OfficialBuildPage {
        OfficialBuildPage {
            slice_page_idx: 0,
            page_size_points: [100.0, 100.0],
            render_scale: 1.0,
            rgb: RgbImage::from_pixel(100, 100, image::Rgb([3, 4, 5])),
            snapshot: blocks,
        }
    }

    fn page_at(index: usize, blocks: Vec<ModelBlock>) -> OfficialBuildPage {
        OfficialBuildPage {
            slice_page_idx: index,
            ..page(blocks)
        }
    }

    #[test]
    fn accepts_increasing_source_page_indexes_and_rejects_non_increasing_indexes() {
        let source_indexes = vec![
            page_at(3, vec![block("text", [0.1, 0.1, 0.9, 0.9], Some("three"))]),
            page_at(5, vec![block("text", [0.1, 0.1, 0.9, 0.9], Some("five"))]),
        ];
        let built = build_official_artifacts(source_indexes, true, true).unwrap();
        assert_eq!(built.middle_json["pdf_info"][0]["page_idx"], 3);
        assert_eq!(built.middle_json["pdf_info"][1]["page_idx"], 5);

        for indexes in [[3, 3], [5, 3]] {
            let pages = indexes
                .into_iter()
                .map(|index| page_at(index, vec![]))
                .collect();
            assert!(build_official_artifacts(pages, true, true).is_err());
        }
    }

    fn table_node(html: &str, width: i32, flags: Option<Value>) -> Node {
        Node::Visual {
            kind: "table".into(),
            body: Block {
                kind: "table_body".into(),
                bbox: [0, 0, width, 10],
                angle: 0,
                lines: vec![Line {
                    bbox: [0; 4],
                    spans: vec![Span::Table(html.into())],
                }],
                index: 0,
                merge_prev: None,
                sub_type: None,
                guess_lang: None,
                image_path: None,
                cell_merge: None,
                cross_page: false,
                lines_deleted: false,
            },
            captions: vec![],
            footnotes: vec![],
            sub_type: None,
            cell_merge: flags,
            sub_images: vec![],
        }
    }

    #[test]
    fn table_merge_keeps_partial_or_unflagged_incoming_rows() {
        let previous = table_node("<table><tr><td>a</td><td>b</td></tr></table>", 100, None);
        let none = table_node(
            "<table><tr><td>c</td><td>d</td></tr></table>",
            100,
            Some(json!([0, 0])),
        );
        let partial = table_node(
            "<table><tr><td>c</td><td>d</td></tr></table>",
            100,
            Some(json!([1, 0])),
        );
        let no_flags = table_node("<table><tr><td>c</td><td>d</td></tr></table>", 100, None);
        assert!(
            merged_table_html(&previous, &none)
                .unwrap()
                .contains("<td>c</td><td>d</td>")
        );
        let html = merged_table_html(&previous, &partial).unwrap();
        assert!(html.contains("<td>ac</td><td>b</td>") && html.contains("<td></td><td>d</td>"));
        assert!(
            merged_table_html(&previous, &no_flags)
                .unwrap()
                .contains("<td>c</td><td>d</td>")
        );
    }

    #[test]
    fn table_merge_rejects_bad_structure_without_mutating_inputs() {
        let previous = table_node(
            "<table><tr><td colspan=\"2\">a</td></tr></table>",
            100,
            None,
        );
        let current = table_node("<table><tr><td>a</td><td>b</td></tr></table>", 100, None);
        let malformed = table_node("<table><tr><td>broken</tr></table>", 100, None);
        let narrow = table_node("<table><tr><td colspan=\"2\">b</td></tr></table>", 80, None);
        assert!(merged_table_html(&previous, &current).is_some());
        assert!(merged_table_html(&previous, &malformed).is_none());
        assert!(merged_table_html(&previous, &narrow).is_none());
        assert_eq!(
            body_data(&previous).2.unwrap(),
            "<table><tr><td colspan=\"2\">a</td></tr></table>"
        );
        for invalid in [
            "<table><tr><td>a</td></tr></table><table><tr><td>b</td></tr></table>",
            "text<table><tr><td>a</td></tr></table>",
            "<table><tr><td>a</td></tr>",
        ] {
            assert!(TableDocument::parse(invalid).is_none());
        }
    }

    #[test]
    fn stage_zero_fixture_has_closed_reference_schema_without_an_origin() {
        let model: Value = serde_json::from_str(include_str!(
            "../tests/fixtures/official/arxiv_2410.21169v5/vlm/2410.21169v5_model.json"
        ))
        .unwrap();
        let middle: Value = serde_json::from_str(include_str!(
            "../tests/fixtures/official/arxiv_2410.21169v5/vlm/2410.21169v5_middle.json"
        ))
        .unwrap();
        let flat: Value = serde_json::from_str(include_str!(
            "../tests/fixtures/official/arxiv_2410.21169v5/vlm/2410.21169v5_content_list.json"
        ))
        .unwrap();
        let v2: Value = serde_json::from_str(include_str!(
            "../tests/fixtures/official/arxiv_2410.21169v5/vlm/2410.21169v5_content_list_v2.json"
        ))
        .unwrap();
        let markdown =
            include_str!("../tests/fixtures/official/arxiv_2410.21169v5/vlm/2410.21169v5.md");

        assert_eq!(model.as_array().unwrap().len(), 3);
        assert_eq!(middle["_backend"], "vlm");
        assert_eq!(middle["_version_name"], "3.4.4");
        assert_eq!(middle["pdf_info"].as_array().unwrap().len(), 3);
        assert_eq!(middle["pdf_info"][1]["preproc_blocks"][0]["type"], "table");
        assert_eq!(middle["pdf_info"][1]["preproc_blocks"][5]["type"], "list");
        assert_eq!(flat[8]["type"], "table");
        assert_eq!(v2[1][0]["content"]["table_type"], "complex_table");
        assert!(markdown.contains("# 3 Methodology and Taxonomy"));
        assert!(!markdown.contains("Document Parsing Unveiled: Techniques"));

        let image_paths = [
            "cc9d646c918053bb628e661ed5772ce1ec4682952a90dc8e687eff8cb42f5df2.jpg",
            "c87758e60fb7ba943d6d429071e045b3ea6c5305534d4799a5797960ea34699e.jpg",
        ];
        for path in image_paths {
            assert!(
                std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("tests/fixtures/official/arxiv_2410.21169v5/vlm/images")
                    .join(path)
                    .is_file()
            );
            assert!(middle.to_string().contains(path));
        }
        fn image_references(value: &Value, output: &mut Vec<String>) {
            match value {
                Value::Array(values) => {
                    for value in values {
                        image_references(value, output);
                    }
                }
                Value::Object(values) => {
                    for (key, value) in values {
                        if matches!(key.as_str(), "image_path" | "img_path" | "path")
                            && value.as_str().is_some_and(|value| value.ends_with(".jpg"))
                        {
                            output.push(value.as_str().unwrap().into());
                        }
                        image_references(value, output);
                    }
                }
                _ => {}
            }
        }
        let mut references = Vec::new();
        for value in [&middle, &flat, &v2] {
            image_references(value, &mut references);
        }
        assert!(!references.is_empty());
        for reference in references {
            assert!(
                std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("tests/fixtures/official/arxiv_2410.21169v5/vlm/images")
                    .join(reference.rsplit('/').next().unwrap())
                    .is_file()
            );
        }
        assert!(
            std::fs::read_dir(
                std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("tests/fixtures/official/arxiv_2410.21169v5/vlm")
            )
            .unwrap()
            .all(|entry| !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .contains("origin"))
        );
    }

    #[test]
    fn synthetic_crop_uses_point_coordinates_and_is_write_idempotent() {
        let visual = block("image", [0.25, 0.25, 0.75, 0.75], None);
        let duplicate = visual.clone();
        let built = build_official_artifacts(
            vec![OfficialBuildPage {
                slice_page_idx: 0,
                page_size_points: [4.0, 4.0],
                render_scale: 2.0,
                rgb: RgbImage::from_fn(8, 8, |x, y| image::Rgb([x as u8, y as u8, 7])),
                snapshot: vec![visual, duplicate],
            }],
            true,
            true,
        )
        .unwrap();
        assert_eq!(built.assets.len(), 1);
        let asset = &built.assets[0];
        let seed = format!(
            "image/{:X}_0_1_1_3_3",
            md5::compute(
                RgbImage::from_fn(8, 8, |x, y| image::Rgb([x as u8, y as u8, 7])).as_raw()
            )
        );
        assert_eq!(
            asset.relative_path,
            PathBuf::from(format!("images/{}.jpg", sha(seed.as_bytes())))
        );
        assert_eq!(asset.kind, AssetKind::Image);
        assert_eq!(asset.media_type, "image/jpeg");
        assert_eq!(asset.md5, format!("{:x}", md5::compute(&asset.data)));
        assert_eq!(
            image::load_from_memory(&asset.data).unwrap().dimensions(),
            (4, 4)
        );
    }

    #[test]
    fn multiple_visual_crops_keep_deterministic_paths_for_every_kind() {
        let image = RgbImage::from_pixel(100, 100, image::Rgb([3, 4, 5]));
        let built = build_official_artifacts(
            vec![OfficialBuildPage {
                snapshot: vec![
                    block("image", [0.1, 0.1, 0.2, 0.2], None),
                    block(
                        "table",
                        [0.3, 0.1, 0.4, 0.2],
                        Some("<table><tr><td>x</td></tr></table>"),
                    ),
                    block("chart", [0.5, 0.1, 0.6, 0.2], None),
                    block("equation", [0.7, 0.1, 0.8, 0.2], Some("x")),
                ],
                rgb: image.clone(),
                ..page(Vec::new())
            }],
            true,
            true,
        )
        .unwrap();
        assert_eq!(
            built
                .assets
                .iter()
                .map(|asset| asset.relative_path.to_str().unwrap())
                .collect::<Vec<_>>(),
            [
                "images/0ca5e31131400f1e4425edfec914535f4c2d3311a836541ba90edc2eca5dc596.jpg",
                "images/16e03126a13d642b4105251f738c6166f7816ff4ad04b351fadd3db96092ec9e.jpg",
                "images/32490c689287b942bd7b439c0aae5943d94461adc294fdc7f6c8b6616077369c.jpg",
                "images/670f4a507f6a391856b75f7ef99759233776301558d86e089fd8e6617864e3b3.jpg",
            ]
        );
        for (tag, bbox) in [
            ("image", [10, 10, 20, 20]),
            ("table", [30, 10, 40, 20]),
            ("chart", [50, 10, 60, 20]),
            ("interline_equation", [70, 10, 80, 20]),
        ] {
            let seed = format!(
                "{tag}/{:X}_0_{}_{}_{}_{}",
                md5::compute(image.as_raw()),
                bbox[0],
                bbox[1],
                bbox[2],
                bbox[3]
            );
            assert!(built.assets.iter().any(|asset| asset.relative_path
                == PathBuf::from(format!("images/{}.jpg", sha(seed.as_bytes())))));
        }
    }

    #[test]
    fn merge_prev_is_page_local_and_orphan_footnotes_are_text() {
        let mut continuation = block("text", [0.1, 0.1, 0.9, 0.2], Some("continuation"));
        continuation.merge_prev = Some(true);
        let built = build_official_artifacts(
            vec![
                page_at(
                    0,
                    vec![block("text", [0.1, 0.7, 0.9, 0.9], Some("previous"))],
                ),
                page_at(
                    1,
                    vec![
                        continuation,
                        block("footnote", [0.1, 0.3, 0.9, 0.4], Some("orphan footnote")),
                    ],
                ),
            ],
            true,
            true,
        )
        .unwrap();
        assert_eq!(
            built.middle_json["pdf_info"][0]["para_blocks"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            built.middle_json["pdf_info"][1]["para_blocks"]
                .as_array()
                .unwrap()
                .len(),
            2
        );
        assert_eq!(
            built.middle_json["pdf_info"][1]["para_blocks"][1]["type"],
            "text"
        );
    }

    #[test]
    fn inline_table_images_are_strict_and_document_write_once() {
        let mut png = Cursor::new(Vec::new());
        DynamicImage::ImageRgb8(RgbImage::from_pixel(1, 1, image::Rgb([9, 8, 7])))
            .write_to(&mut png, ImageFormat::Png)
            .unwrap();
        let uri = format!("data:image/png;base64,{}", STANDARD.encode(png.get_ref()));
        let html = format!("<table><img src=\"{uri}\"></table>");
        let mut assets = BTreeMap::new();
        let replaced = inline_images(&html, &mut assets, usize::MAX).unwrap();
        let basename = format!("{}.png", sha(uri.as_bytes()));
        assert_eq!(replaced, format!("<table><img src=\"{basename}\"></table>"));
        assert_eq!(assets.len(), 1);
        assert_eq!(assets.values().next().unwrap().kind, AssetKind::Image);
        assert_eq!(assets.values().next().unwrap().media_type, "image/png");
        assert_eq!(
            inline_images(&html, &mut assets, usize::MAX).unwrap(),
            replaced
        );
        let path = format!("images/{basename}");
        let mut collision = BTreeMap::new();
        collision.insert(
            path.clone(),
            Asset {
                kind: AssetKind::Image,
                relative_path: PathBuf::from(path),
                media_type: "image/png".into(),
                md5: "different".into(),
                data: Bytes::from_static(b"different"),
            },
        );
        assert!(inline_images(&html, &mut collision, usize::MAX).is_err());
        let garbage = "data:image/png;base64,%%%";
        let garbage_name = format!("{}.png", sha(garbage.as_bytes()));
        assert_eq!(
            inline_images(
                &format!("{html}<img src=\"{garbage}\">"),
                &mut assets,
                usize::MAX
            )
            .unwrap(),
            format!("{replaced}<img src=\"{garbage_name}\">")
        );
        assert!(assets.values().any(|asset| asset.data.is_empty()));
        let svg = "data:image/svg;base64,AAAA";
        assert_eq!(
            inline_images(&format!("<img src=\"{svg}\">"), &mut assets, usize::MAX).unwrap(),
            format!("<img src=\"{}.svg\">", sha(svg.as_bytes()))
        );
        let mismatched = "<img src=\"data:image/jpeg;base64,cG5nLWJ5dGVz\">";
        assert!(inline_images(mismatched, &mut BTreeMap::new(), usize::MAX).is_ok());
        let ordinary = "<p>Data: 42</p><img data-src=\"data:image/png;base64,%%%\" notsrc=\"data:image/png;base64,%%%\">";
        assert_eq!(
            inline_images(ordinary, &mut BTreeMap::new(), usize::MAX).unwrap(),
            format!("<p>Data: 42</p><img data-src=\"{garbage_name}\" notsrc=\"{garbage_name}\">")
        );
        let mut atomic = BTreeMap::new();
        assert!(
            inline_images(
                &format!("<img src=\"{uri}\"><img src=\"data:image/png;base64,%%%\">"),
                &mut atomic,
                usize::MAX
            )
            .is_ok()
        );
        assert_eq!(atomic.len(), 2);
    }

    #[test]
    fn malformed_single_line_code_fence_has_empty_body() {
        assert_eq!(code_content("```"), "");
    }

    #[test]
    fn table_headers_entities_and_continuation_markers_follow_pinned_rules() {
        let previous = table_node(
            "<table><thead><tr><th colspan=\"2\">A</th></tr></thead><tbody><tr><td>a</td><td>b</td></tr></tbody></table>",
            100,
            None,
        );
        let current = table_node(
            "<table><thead><tr><th colspan=\"2\">A</th></tr></thead><tbody><tr><td>c</td><td>d</td></tr></tbody></table>",
            100,
            None,
        );
        let html = merged_table_html(&previous, &current).unwrap();
        assert_eq!(html.matches("<th colspan=\"2\">A</th>").count(), 1);
        assert!(html.contains("<td>c</td><td>d</td>"));
        assert!(is_continuation_text("Table 1 (cont’d)"));
        for marker in [
            "(续)",
            "(续表)",
            "(续上表)",
            "(continued)",
            "(cont.)",
            "(…continued)",
            "continued",
            "续表",
        ] {
            assert!(
                is_continuation_text(&format!("Table 1 {marker}")),
                "{marker}"
            );
        }
        assert!(!is_continuation_text("continuation"));
        assert!(!is_continuation_text("discontinued"));
        assert_eq!(
            output_html("<eq>&#x3B1; &amp; &nbsp;</eq>"),
            " $α & \u{a0}$ "
        );
        assert_eq!(
            html_unescape("&NotEqualTilde; &#128; &#x1f642; &frac34;"),
            "≂̸ € 🙂 ¾"
        );
    }

    #[test]
    fn html_unescape_uses_python_numeric_recovery() {
        assert_eq!(
            html_unescape("&copy &#x80; &#0; &#xD800; &#x110000; &NotEqualTilde;"),
            "© € � � � ≂\u{338}"
        );
        assert_eq!(
            html_unescape("&notit &notit; &AMPx &ThickSpace; &#13; &#1; &#xFDD0; &acE;"),
            "¬it &notit; &x \u{205f}\u{200a} \r   ∾\u{333}"
        );
        assert_eq!(
            html_unescape("&#129; &#141; &#157;"),
            "\u{81} \u{8d} \u{9d}"
        );
    }

    #[test]
    fn table_document_keeps_source_fragments_and_tbody_append_slot() {
        let previous = table_node(
            "<table data-root=\"x\"><!--lead--><thead data-head=\"y\"><tr data-h=\"1\"><th>H</th></tr></thead><!--between--><tbody id=\"body\"><!--before--><tr data-old=\"1\"><td>old</td></tr><!--tail--></tbody><!--after--></table>",
            100,
            None,
        );
        let current = table_node(
            "<table><thead><tr><th>H</th></tr></thead><tbody><tr data-new=\"1\"><td>new</td></tr></tbody></table>",
            100,
            None,
        );
        let html = merged_table_html(&previous, &current).unwrap();
        assert!(html.starts_with("<table data-root=\"x\"><!--lead--><thead data-head=\"y\">"));
        assert!(html.contains("<!--before--><tr data-old=\"1\"><td>old</td></tr><!--tail--><tr data-new=\"1\"><td>new</td></tr></tbody><!--after-->"));
        assert!(
            merged_table_html(
                &previous,
                &table_node(
                    "<table><tr><td><table><tr><td>nested</td></tr></table></td></tr></table>",
                    100,
                    None
                )
            )
            .is_none()
        );
    }

    #[test]
    fn table_state_uses_strict_visual_and_boundary_metrics() {
        let strict_previous = build_table_state(
            "<table><tr><th>Ａ&nbsp;B</th><th>C</th></tr><tr><td>old</td><td>1</td></tr></table>",
        )
        .unwrap();
        let strict_current = build_table_state(
            "<table><tr><th>A\u{a0}B</th><th>C</th></tr><tr><td>new</td><td>2</td></tr></table>",
        )
        .unwrap();
        assert_eq!(
            detect_table_headers(&strict_previous, &strict_current)
                .unwrap()
                .0,
            1
        );

        let visual_previous = build_table_state(
            "<table><tr><th colspan=\"2\">H</th></tr><tr><td>a</td><td>b</td></tr></table>",
        )
        .unwrap();
        let visual_current =
            build_table_state("<table><tr><th>H</th></tr><tr><td>c</td><td>d</td></tr></table>")
                .unwrap();
        assert_eq!(
            detect_table_headers(&visual_previous, &visual_current)
                .unwrap()
                .0,
            1
        );

        let rowspan = build_table_state("<table><tr><th rowspan=\"2\">A</th><th>B</th></tr><tr><th rowspan=\"2\">C</th></tr><tr><th>D</th></tr><tr><td>x</td><td>y</td></tr></table>").unwrap();
        assert_eq!(expand_header_rowspans(&rowspan.rows, 1), Some(3));
        assert!(
            merged_table_html(
                &table_node("<table><tr><th>H</th></tr></table>", 100, None),
                &table_node("<table><tr><th>H</th></tr></table>", 100, None),
            )
            .is_some()
        );

        let previous = build_table_state(
            "<table><tr><td rowspan=\"2\">A</td><td>B</td></tr><tr><td>C</td></tr></table>",
        )
        .unwrap();
        let actual = build_table_state("<table><tr><td>D</td></tr></table>").unwrap();
        let rendered =
            build_table_state("<table><tr><td colspan=\"2\">D</td><td>E</td></tr></table>")
                .unwrap();
        assert!(rows_match(&previous, &actual).unwrap());
        assert!(rows_match(&previous, &rendered).unwrap());
        assert!(compatible_tables(&strict_previous, &strict_current).unwrap());
    }

    #[test]
    fn table_rowspan_tail_clipping_and_semantic_cell_carry_follow_source() {
        let carried = merged_table_html(
            &table_node("<table><tr><td>A</td><td>B</td></tr></table>", 100, None),
            &table_node(
                "<table><tr><td rowspan=\"2\"></td><td>C</td></tr><tr><td>D</td></tr></table>",
                100,
                Some(json!([0, 1])),
            ),
        )
        .unwrap();
        assert!(carried.contains("<td>A</td><td>BC</td>"));
        assert!(
            carried.contains("<tr><td></td><td>D</td></tr>"),
            "{carried}"
        );

        let clipped = merged_table_html(
            &table_node(
                "<table><tr><td rowspan=\"2\">A</td><td>B</td></tr></table>",
                100,
                None,
            ),
            &table_node(
                "<table><tr><td rowspan=\"2\"></td><td>C</td></tr><tr><td>D</td></tr></table>",
                100,
                None,
            ),
        )
        .unwrap();
        assert!(clipped.contains("<tr><td>C</td></tr>"));
        assert_eq!(clipped.matches("rowspan=\"2\"").count(), 1);
    }

    #[test]
    fn subtype_cell_merge_and_vector_gates_match_source() {
        let mut table = block(
            "table",
            [0.1, 0.1, 0.9, 0.3],
            Some("<table><tr><td>x</td></tr></table>"),
        );
        table.extra.insert("cell_merge".into(), json!([]));
        let mut image = block("image", [0.1, 0.4, 0.3, 0.6], None);
        image.sub_type = Some(String::new());
        let mut chart = block("chart", [0.4, 0.4, 0.6, 0.6], None);
        chart.sub_type = Some("bar".into());
        let mut image_block = block("image_block", [0.7, 0.4, 0.9, 0.6], None);
        image_block.sub_type = Some("ignored".into());
        let built = build_official_artifacts(
            vec![page(vec![table, image, chart, image_block])],
            true,
            true,
        )
        .unwrap();
        let blocks = &built.middle_json["pdf_info"][0]["preproc_blocks"];
        assert!(blocks[0].get("cell_merge").is_none());
        assert!(blocks[1].get("sub_type").is_none());
        assert_eq!(blocks[2]["sub_type"], "bar");
        assert!(blocks[3].get("sub_type").is_none());

        let mut truthy = block(
            "table",
            [0.1, 0.1, 0.9, 0.3],
            Some("<table><tr><td>x</td></tr></table>"),
        );
        truthy.extra.insert("cell_merge".into(), json!([0]));
        let truthy = build_official_artifacts(vec![page(vec![truthy])], true, true).unwrap();
        assert_eq!(
            truthy.middle_json["pdf_info"][0]["preproc_blocks"][0]["cell_merge"],
            json!([0])
        );
        assert_eq!(
            truthy.middle_json["pdf_info"][0]["preproc_blocks"][0]["blocks"][0]["cell_merge"],
            json!([0])
        );

        let mut assets = BTreeMap::new();
        let output = inline_images(
            "src=\"data:image/wmf;base64,AAAA\" src=\"data:image/emf;base64,BBBB\"",
            &mut assets,
            usize::MAX,
        )
        .unwrap();
        assert!(!output.contains("data:image/"));
        assert_eq!(assets.len(), 1);
        let vector = assets.values().next().unwrap();
        assert_eq!(vector.media_type, "image/jpeg");
        assert_eq!(
            image::load_from_memory(&vector.data).unwrap().dimensions(),
            (320, 180)
        );
    }

    #[test]
    fn table_body_headers_and_rowspan_are_skipped_like_source() {
        let previous = table_node(
            "<table><tbody><tr><th>A</th><th>B</th></tr><tr><td>1</td><td>2</td></tr></tbody></table>",
            100,
            None,
        );
        let current = table_node(
            "<table><tbody><tr><th rowspan=\"2\">A</th><th>B</th></tr><tr><th>B2</th></tr><tr><td>3</td><td>4</td></tr></tbody></table>",
            100,
            None,
        );
        // Visual fallback accepts the first repeated row despite its rowspan;
        // expansion drops the covered second header row too.
        let merged = merged_table_html(&previous, &current).unwrap();
        assert!(merged.contains("<td>3</td><td>4</td>"));
        assert!(!merged.contains("B2"));
    }

    #[test]
    fn inline_matcher_keeps_python_substring_carriers() {
        let uri = "data:image/png;base64,YQ==";
        let mut assets = BTreeMap::new();
        let output = inline_images(
            &format!("prose src=\"{uri}\" data-src=\"{uri}\""),
            &mut assets,
            usize::MAX,
        )
        .unwrap();
        assert_eq!(assets.len(), 1);
        assert!(!output.contains(uri));
        let garbage = "data:image/png;base64,%%%";
        assert_eq!(
            inline_images("src=\"data:image/png;base64,%%%\"", &mut assets, usize::MAX).unwrap(),
            format!("src=\"{}.png\"", sha(garbage.as_bytes()))
        );
    }

    #[test]
    fn finalizes_adjacent_pages_as_one_document() {
        let refs = build_official_artifacts(
            vec![
                page_at(
                    0,
                    vec![
                        block("ref_text", [0.1, 0.1, 0.9, 0.2], Some("[1] first")),
                        block("list", [0.1, 0.1, 0.9, 0.2], None),
                    ],
                ),
                page_at(
                    1,
                    vec![
                        block("ref_text", [0.1, 0.1, 0.9, 0.2], Some("[2] second")),
                        block("list", [0.1, 0.1, 0.9, 0.2], None),
                    ],
                ),
            ],
            true,
            true,
        )
        .unwrap();
        assert_eq!(
            refs.middle_json["pdf_info"][0]["para_blocks"][0]["blocks"]
                .as_array()
                .unwrap()
                .len(),
            2
        );
        let list_tombstone = &refs.middle_json["pdf_info"][1]["para_blocks"][0];
        assert_eq!(list_tombstone["type"], "list");
        assert_eq!(list_tombstone["blocks"], json!([]));
        assert_eq!(list_tombstone["lines_deleted"], true);
        assert!(list_tombstone.get("cross_page").is_none());
        assert_eq!(
            refs.middle_json["pdf_info"][0]["para_blocks"][0]["blocks"][1]["lines"][0]["spans"][0]
                ["cross_page"],
            true
        );

        // The route serializes these records after releasing page RGB.  Re-read them
        // before finalization to cover the document-scope staging seam specifically.
        let staged = vec![
            prepare_official_page(
                page_at(
                    2,
                    vec![
                        block("ref_text", [0.1, 0.1, 0.9, 0.2], Some("[3] third")),
                        block("list", [0.1, 0.1, 0.9, 0.2], None),
                    ],
                ),
                usize::MAX,
                usize::MAX,
            )
            .unwrap(),
            prepare_official_page(
                page_at(
                    3,
                    vec![
                        block("ref_text", [0.1, 0.1, 0.9, 0.2], Some("[4] fourth")),
                        block("list", [0.1, 0.1, 0.9, 0.2], None),
                    ],
                ),
                usize::MAX,
                usize::MAX,
            )
            .unwrap(),
        ];
        let pages = staged
            .into_iter()
            .map(|prepared| {
                serde_json::from_slice(&serde_json::to_vec(&prepared.page).unwrap()).unwrap()
            })
            .collect();
        let canonical = finalize_official_document(pages, true, true).unwrap();
        assert_eq!(
            canonical[0].middle_json["pdf_info"][0]["para_blocks"][0]["blocks"]
                .as_array()
                .unwrap()
                .len(),
            2
        );
        assert_eq!(
            canonical[1].middle_json["pdf_info"][0]["para_blocks"][0]["type"],
            "list"
        );
        assert_eq!(
            canonical[1].middle_json["pdf_info"][0]["para_blocks"][0]["lines_deleted"],
            true
        );

        let first = page_at(
            0,
            vec![block(
                "table",
                [0.1, 0.7, 0.9, 0.9],
                Some("<table><tr><td>a</td></tr></table>"),
            )],
        );
        let second = page_at(
            1,
            vec![
                block("table_caption", [0.1, 0.05, 0.9, 0.09], Some("(continued)")),
                block(
                    "table",
                    [0.1, 0.1, 0.9, 0.3],
                    Some("<table><tr><td>b</td></tr></table>"),
                ),
            ],
        );
        let merged = build_official_artifacts(vec![first, second], true, true).unwrap();
        assert_eq!(merged.middle_json["pdf_info"][0]["page_idx"], 0);
        assert_eq!(merged.middle_json["pdf_info"][1]["page_idx"], 1);
        assert!(
            merged.middle_json["pdf_info"][0]["para_blocks"][0]["blocks"][0]["lines"][0]["spans"]
                [0]["html"]
                .as_str()
                .unwrap()
                .contains("<td>b</td>")
        );
        assert_eq!(
            merged.middle_json["pdf_info"][1]["para_blocks"][0]["type"],
            "table"
        );
        assert!(
            !merged.middle_json["pdf_info"][1]["para_blocks"][0]["blocks"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            merged.middle_json["pdf_info"][1]["para_blocks"][0]["blocks"][0]["lines_deleted"],
            true
        );
        assert!(
            merged.middle_json["pdf_info"][1]["para_blocks"][0]
                .get("angle")
                .is_none()
        );

        let no_table_merge = build_official_artifacts(
            vec![
                page_at(
                    0,
                    vec![block(
                        "table",
                        [0.1, 0.7, 0.9, 0.9],
                        Some("<table>a</table>"),
                    )],
                ),
                page_at(
                    1,
                    vec![
                        block("table_caption", [0.1, 0.05, 0.9, 0.09], Some("continued")),
                        block("table", [0.1, 0.1, 0.9, 0.3], Some("<table>b</table>")),
                    ],
                ),
            ],
            true,
            false,
        )
        .unwrap();
        assert_eq!(
            no_table_merge.middle_json["pdf_info"][1]["para_blocks"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn reference_list_continues_past_an_emptied_page() {
        let refs = build_official_artifacts(
            vec![
                page_at(
                    0,
                    vec![
                        block("ref_text", [0.1, 0.1, 0.9, 0.2], Some("[1] first")),
                        block("list", [0.1, 0.1, 0.9, 0.2], None),
                    ],
                ),
                page_at(
                    1,
                    vec![
                        block("ref_text", [0.1, 0.1, 0.9, 0.2], Some("[2] second")),
                        block("list", [0.1, 0.1, 0.9, 0.2], None),
                    ],
                ),
                page_at(
                    2,
                    vec![
                        block("ref_text", [0.1, 0.1, 0.9, 0.2], Some("[3] third")),
                        block("list", [0.1, 0.1, 0.9, 0.2], None),
                    ],
                ),
            ],
            true,
            true,
        )
        .unwrap();
        assert_eq!(
            refs.middle_json["pdf_info"][0]["para_blocks"][0]["blocks"]
                .as_array()
                .unwrap()
                .len(),
            3
        );
        for page in 1..=2 {
            assert_eq!(
                refs.middle_json["pdf_info"][page]["para_blocks"][0]["type"],
                "list"
            );
            assert_eq!(
                refs.middle_json["pdf_info"][page]["para_blocks"][0]["blocks"],
                json!([])
            );
            assert_eq!(
                refs.middle_json["pdf_info"][page]["para_blocks"][0]["lines_deleted"],
                true
            );
        }
    }

    #[test]
    fn table_continues_past_an_emptied_page() {
        let continuation = |content| {
            vec![
                block("table_caption", [0.1, 0.05, 0.9, 0.09], Some("(continued)")),
                block("table", [0.1, 0.1, 0.9, 0.3], Some(content)),
            ]
        };
        let tables = build_official_artifacts(
            vec![
                page_at(
                    0,
                    vec![block(
                        "table",
                        [0.1, 0.7, 0.9, 0.9],
                        Some("<table><tr><td>a</td></tr></table>"),
                    )],
                ),
                page_at(1, continuation("<table><tr><td>b</td></tr></table>")),
                page_at(2, continuation("<table><tr><td>c</td></tr></table>")),
            ],
            true,
            true,
        )
        .unwrap();
        let html =
            tables.middle_json["pdf_info"][0]["para_blocks"][0]["blocks"][0]["lines"][0]["spans"]
                [0]["html"]
                .as_str()
                .unwrap();
        assert!(
            html.contains("<td>a</td>")
                && html.contains("<td>b</td>")
                && html.contains("<td>c</td>")
        );
        for page in 1..=2 {
            assert_eq!(
                tables.middle_json["pdf_info"][page]["para_blocks"][0]["type"],
                "table"
            );
            assert!(
                !tables.middle_json["pdf_info"][page]["para_blocks"][0]["blocks"]
                    .as_array()
                    .unwrap()
                    .is_empty()
            );
            assert_eq!(
                tables.middle_json["pdf_info"][page]["para_blocks"][0]["blocks"][0]["lines_deleted"],
                true
            );
            assert!(
                tables.middle_json["pdf_info"][page]["preproc_blocks"][0]["blocks"][0]["lines"]
                    .as_array()
                    .is_some_and(|lines| !lines.is_empty())
            );
        }
    }

    #[test]
    fn caption_fallback_and_empty_list_marker_follow_magic_model() {
        let built = build_official_artifacts(
            vec![page(vec![
                block("table_caption", [0.1, 0.1, 0.5, 0.15], Some("Table")),
                block("text", [0.5, 0.1, 0.9, 0.15], Some(" one")),
                block("table", [0.1, 0.16, 0.9, 0.4], Some("<table>x</table>")),
                block("list", [0.1, 0.5, 0.9, 0.6], None),
            ])],
            true,
            true,
        )
        .unwrap();
        let blocks = built.middle_json["pdf_info"][0]["preproc_blocks"]
            .as_array()
            .unwrap();
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0]["blocks"].as_array().unwrap().len(), 3);
        assert_eq!(blocks[0]["blocks"][1]["type"], "table_caption");
    }

    #[test]
    fn fixture_shapes_come_from_grouped_table_list_equation_and_chrome() {
        let mut text = block("text", [0.1, 0.5, 0.9, 0.6], Some("one \\( x \\)"));
        text.merge_prev = Some(false);
        let mut reference = block("ref_text", [0.1, 0.6, 0.9, 0.7], Some("two"));
        reference.merge_prev = Some(true);
        let built = build_official_artifacts(
            vec![page(vec![
                block("header", [0.1, 0.01, 0.9, 0.05], Some("chrome")),
                block("table_caption", [0.1, 0.1, 0.9, 0.15], Some("Table 1")),
                block(
                    "table",
                    [0.1, 0.16, 0.9, 0.4],
                    Some("<table><tr><td>x</td></tr></table>"),
                ),
                text,
                reference,
                block("list", [0.1, 0.5, 0.9, 0.7], None),
                block("equation", [0.1, 0.72, 0.9, 0.8], Some("\\[ x = y \\]")),
                block("image", [0.1, 0.81, 0.9, 0.86], Some("diagram")),
                block("image_caption", [0.1, 0.87, 0.9, 0.89], Some("Fig. 1")),
                block("page_footnote", [0.1, 0.9, 0.9, 0.95], Some("foot")),
            ])],
            true,
            true,
        )
        .unwrap();
        let info = &built.middle_json["pdf_info"][0];
        assert_eq!(info["preproc_blocks"][0]["type"], "table");
        assert_eq!(
            info["preproc_blocks"][0]["blocks"][0]["type"],
            "table_caption"
        );
        assert_eq!(info["preproc_blocks"][0]["blocks"][1]["type"], "table_body");
        assert_eq!(info["preproc_blocks"][1]["type"], "list");
        assert_eq!(
            info["preproc_blocks"][1]["blocks"]
                .as_array()
                .unwrap()
                .len(),
            2
        );
        assert_eq!(info["preproc_blocks"][2]["type"], "interline_equation");
        assert_eq!(info["discarded_blocks"].as_array().unwrap().len(), 2);
        assert_eq!(built.content_list[0]["type"], "table");
        assert_eq!(built.content_list[1]["type"], "list");
        assert_eq!(built.content_list[2]["type"], "equation");
        assert_eq!(built.content_list[3]["image_caption"], json!(["Fig. 1"]));
        assert_eq!(
            built.content_list_v2[0][0]["content"]["html"],
            "<table><tr><td>x</td></tr></table>"
        );
        assert_eq!(
            built.content_list_v2[0][1]["content"]["list_type"],
            "text_list"
        );
        assert_eq!(
            built.content_list_v2[0][3]["content"]["image_caption"],
            json!([{"type":"text","content":"Fig. 1"}])
        );
        assert!(built.markdown.contains("Table 1\n\n<table"));
        assert!(built.markdown.contains("one $x$  \ntwo"));
        assert!(built.markdown.contains("$$\n x = y\n$$"));
        assert!(!built.markdown.contains("chrome"));
    }

    #[test]
    fn presentation_flags_only_change_markdown_fallbacks() {
        let blocks = vec![
            block(
                "table",
                [0.1, 0.1, 0.9, 0.4],
                Some("<table><tr><td>x</td></tr></table>"),
            ),
            block("equation", [0.1, 0.5, 0.9, 0.6], Some("\\[ x \\]")),
        ];
        let enabled = build_official_artifacts(vec![page(blocks.clone())], true, true).unwrap();
        let disabled = build_official_artifacts(vec![page(blocks)], false, false).unwrap();
        assert_eq!(enabled.content_list, disabled.content_list);
        assert_eq!(enabled.content_list_v2, disabled.content_list_v2);
        assert!(enabled.markdown.contains("<table"));
        assert!(enabled.markdown.contains("$$\n x\n$$"));
        assert_eq!(disabled.markdown.matches("![](images/").count(), 2);
    }

    #[test]
    fn rejects_every_malformed_carrier_and_orphan_relation() {
        let valid = block("text", [0.1, 0.1, 0.9, 0.9], Some("x"));
        let mut bad_bbox = valid.clone();
        bad_bbox.bbox = Some(NormalizedBbox {
            left: f32::NAN,
            top: 0.0,
            right: 1.0,
            bottom: 1.0,
        });
        let mut missing_angle = valid.clone();
        missing_angle.angle = None;
        let mut missing_bbox = valid.clone();
        missing_bbox.bbox = None;
        let invalid_pages = vec![
            OfficialBuildPage {
                render_scale: 0.0,
                ..page(vec![valid.clone()])
            },
            OfficialBuildPage {
                render_scale: f32::NAN,
                ..page(vec![valid.clone()])
            },
            OfficialBuildPage {
                page_size_points: [0.0, 100.0],
                ..page(vec![valid.clone()])
            },
            OfficialBuildPage {
                rgb: RgbImage::new(99, 100),
                ..page(vec![valid.clone()])
            },
        ];
        for invalid in invalid_pages {
            assert!(build_official_artifacts(vec![invalid], true, true).is_err());
        }
        assert!(build_official_artifacts(vec![page(vec![bad_bbox])], true, true).is_err());
        assert!(build_official_artifacts(vec![page(vec![missing_angle])], true, true).is_err());
        assert!(build_official_artifacts(vec![page(vec![missing_bbox])], true, true).is_err());
        assert!(
            build_official_artifacts(
                vec![page(vec![block("unknown", [0.1, 0.1, 0.9, 0.9], None)])],
                true,
                true
            )
            .is_err()
        );
        let orphan = build_official_artifacts(
            vec![page(vec![block(
                "caption",
                [0.1, 0.1, 0.9, 0.2],
                Some("orphan"),
            )])],
            true,
            true,
        )
        .unwrap();
        assert_eq!(
            orphan.middle_json["pdf_info"][0]["para_blocks"][0]["type"],
            "text"
        );
        assert!(
            build_official_artifacts(
                vec![page(vec![block(
                    "list",
                    [0.1, 0.1, 0.9, 0.2],
                    Some("lost")
                )])],
                true,
                true
            )
            .is_err()
        );
        assert!(
            build_official_artifacts(
                vec![page(vec![block("equation", [0.1, 0.1, 0.9, 0.2], None)])],
                true,
                true
            )
            .is_ok()
        );

        let reversed = block("text", [0.9, 0.8, 0.1, 0.2], Some("x"));
        let built = build_official_artifacts(vec![page(vec![reversed])], true, true).unwrap();
        assert_eq!(
            built.middle_json["pdf_info"][0]["preproc_blocks"][0]["bbox"],
            json!([10, 20, 90, 80])
        );
    }

    #[test]
    fn accepts_every_layout_type_without_inventing_suppressed_content() {
        let blocks = ["formula_number", "index", "list_item", "equation_block"]
            .into_iter()
            .map(|kind| block(kind, [0.1, 0.1, 0.9, 0.2], None))
            .collect();
        let built = build_official_artifacts(vec![page(blocks)], false, false).unwrap();
        assert!(!built.middle_json.to_string().contains("recognized"));
    }

    #[test]
    fn source_markdown_and_colspan_repairs_are_preserved() {
        assert_eq!(markdown_text("Ｈｅｌｌｏ *x*"), "Hello \\*x\\*");
        assert_eq!(markdown_text("  # heading "), "  # heading ");
        let mut rows = vec![TableRow {
            raw: "<tr><td>a</td><td>b</td></tr>".into(),
            deleted: false,
        }];
        let structure = TableRow {
            raw: "<tr><td colspan=\"2\">a</td><td colspan=\"2\">b</td></tr>".into(),
            deleted: false,
        };
        let matching = rows[0].clone();
        adjust_colspans(&mut rows, 0, &[2], &structure, &matching, 4).unwrap();
        assert_eq!(rows[0].raw.matches("colspan=\"2\"").count(), 2);

        let mut rows = vec![TableRow {
            raw: "<tr><td>a</td><td>b</td></tr>".into(),
            deleted: false,
        }];
        let structure = TableRow {
            raw: "<tr><td colspan=\"2\">a</td><td>b</td><td>c</td></tr>".into(),
            deleted: false,
        };
        let matching = rows[0].clone();
        adjust_colspans(&mut rows, 0, &[2], &structure, &matching, 4).unwrap();
        assert_eq!(rows[0].raw, "<tr><td>a</td><td>b</td></tr>");

        let mut code = table_node("<table></table>", 100, None);
        let Node::Visual { body, .. } = &mut code else {
            unreachable!()
        };
        body.kind = "code_body".into();
        body.guess_lang = Some("rust".into());
        body.lines = vec![Line {
            bbox: [0; 4],
            spans: vec![Span::Text("let x = *raw*;".into())],
        }];
        assert_eq!(
            code_markdown(body, Some("code")),
            "```rust\nlet x = *raw*;\n```"
        );
        body.lines[0].spans = vec![Span::Text("<x>".into())];
        assert_eq!(
            code_markdown(body, Some("algorithm")),
            "<div class=\"mineru-algorithm\" style=\"white-space: pre-wrap; font-family:monospace;\">\n&lt;x&gt;\n</div>"
        );
    }

    #[test]
    fn paragraph_and_code_rendering_follow_source_escaping_rules() {
        let text = |content: &str| Block {
            kind: "text".into(),
            bbox: [0; 4],
            angle: 0,
            lines: vec![Line {
                bbox: [0; 4],
                spans: vec![Span::Text(content.into())],
            }],
            index: 0,
            merge_prev: None,
            sub_type: None,
            guess_lang: None,
            image_path: None,
            cell_merge: None,
            cross_page: false,
            lines_deleted: false,
        };
        assert_eq!(render_block(&text("#abc"), true), "#abc");
        assert_eq!(render_block(&text("\t# abc"), true), "\\# abc");
        assert_eq!(
            render_block(&text(r"\* odd \\* even"), true),
            r"\* odd \\\* even"
        );
        let mut caption = text("# caption");
        caption.kind = "caption".into();
        assert_eq!(render_block(&caption, true), "\\# caption");
        assert_eq!(
            markdown_node(
                &Node::List {
                    marker: text(""),
                    items: vec![text("- item")],
                },
                true,
                true,
            ),
            "- item"
        );
        assert_eq!(render_list_item(&text("# item")), "# item");

        let mut code = text("*x*");
        code.kind = "code_body".into();
        assert_eq!(code_markdown(&code, Some("code")), "```txt\n*x*\n```");

        code.lines = vec![
            Line {
                bbox: [0; 4],
                spans: vec![Span::Text("inter-".into())],
            },
            Line {
                bbox: [0; 4],
                spans: vec![Span::Text("national".into())],
            },
        ];
        assert_eq!(
            code_markdown(&code, Some("code")),
            "```txt\ninternational\n```"
        );
        code.lines[1].spans = vec![Span::Text("National".into())];
        assert_eq!(
            code_markdown(&code, Some("code")),
            "```txt\ninter-National\n```"
        );
        code.lines = vec![
            Line {
                bbox: [0; 4],
                spans: vec![Span::Text("中文".into())],
            },
            Line {
                bbox: [0; 4],
                spans: vec![Span::Text("测试".into())],
            },
        ];
        assert_eq!(code_markdown(&code, Some("code")), "```txt\n中文测试\n```");
        code.lines = vec![Line {
            bbox: [0; 4],
            spans: vec![Span::InlineEquation(" x ".into())],
        }];
        assert_eq!(code_markdown(&code, Some("code")), "```txt\n$ x $\n```");

        code.lines[0].spans = vec![
            Span::Text("Ａ < &".into()),
            Span::InlineEquation("x<y&z".into()),
            Span::InlineEquation("q".into()),
        ];
        assert_eq!(
            code_markdown(&code, Some("algorithm")),
            "<div class=\"mineru-algorithm\" style=\"white-space: pre-wrap; font-family:monospace;\">\nA &lt; &amp;$x&lt;y&amp;z$ $q$\n</div>"
        );
        code.lines[0].spans = vec![Span::Text("text".into()), Span::InlineEquation("x".into())];
        assert_eq!(
            code_markdown(&code, Some("algorithm")),
            "<div class=\"mineru-algorithm\" style=\"white-space: pre-wrap; font-family:monospace;\">\ntext$x$\n</div>"
        );
        code.lines = vec![Line {
            bbox: [0; 4],
            spans: vec![Span::Text(" \t".into())],
        }];
        assert_eq!(code_markdown(&code, Some("algorithm")), "");
        code.lines[0].spans = vec![
            Span::InlineEquation("x ".into()),
            Span::InlineEquation("y".into()),
        ];
        assert_eq!(
            code_markdown(&code, Some("algorithm")),
            "<div class=\"mineru-algorithm\" style=\"white-space: pre-wrap; font-family:monospace;\">\n$x $ $y$\n</div>"
        );
    }
}
