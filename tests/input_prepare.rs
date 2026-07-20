//! Image fixtures are generated from solid colors. See `fixtures/input/README.md`
//! for exact commands, tool versions and hashes; tests invoke none of those tools.
use bytes::Bytes;
use image::{DynamicImage, ImageBuffer, ImageFormat, Rgba};
use mineru::{
    OfficeWorkers, OfficialPdfOptions,
    input_prepare::{DocumentKind, RasterWorkers, prepare as core_prepare},
};
use std::path::PathBuf;
use std::{io::Cursor, sync::Arc, time::Duration};
#[path = "support/office_fixtures.rs"]
mod office_fixtures;

fn workers() -> OfficeWorkers {
    OfficeWorkers::new().unwrap()
}
fn options() -> OfficialPdfOptions {
    OfficialPdfOptions::default()
}
fn rgba() -> DynamicImage {
    DynamicImage::ImageRgba8(ImageBuffer::from_pixel(2, 3, Rgba([10, 20, 30, 255])))
}
fn encoded(format: ImageFormat) -> Vec<u8> {
    let mut bytes = Vec::new();
    rgba()
        .write_to(&mut Cursor::new(&mut bytes), format)
        .unwrap();
    bytes
}
fn oriented_jpeg() -> Vec<u8> {
    let mut jpeg = encoded(ImageFormat::Jpeg);
    // APP1 Exif, little-endian TIFF IFD with Orientation=6 (rotate 90° CW).
    let exif = b"Exif\0\0II*\0\x08\0\0\0\x01\0\x12\x01\x03\0\x01\0\0\0\x06\0\0\0\0\0\0\0";
    let mut output = vec![0xff, 0xd8, 0xff, 0xe1, 0, exif.len() as u8 + 2];
    output.extend_from_slice(exif);
    output.extend_from_slice(&jpeg.split_off(2));
    output
}
async fn prepared(bytes: Vec<u8>, kind: DocumentKind, options: &OfficialPdfOptions) -> Vec<u8> {
    let value = prepare(bytes, kind, options, &workers(), Duration::from_secs(5))
        .await
        .unwrap();
    assert_eq!(value.kind, kind);
    value.bytes.to_vec()
}
async fn prepare(
    bytes: Vec<u8>,
    kind: DocumentKind,
    options: &OfficialPdfOptions,
    workers: &OfficeWorkers,
    remaining: Duration,
) -> Result<mineru::input_prepare::PreparedPdf, String> {
    let raster_workers = RasterWorkers::default();
    let result = core_prepare(bytes, kind, options, workers, &raster_workers, remaining).await;
    raster_workers.drain().await;
    result
}
fn assert_one_page(pdf: &[u8]) {
    let document = lopdf::Document::load_mem(pdf).unwrap();
    assert!(!document.is_encrypted());
    assert_eq!(document.get_pages().len(), 1);
}

#[tokio::test]
async fn static_rasters_become_bounded_one_page_pdfs() {
    for (kind, format) in [
        (DocumentKind::Png, ImageFormat::Png),
        (DocumentKind::Jpeg, ImageFormat::Jpeg),
        (DocumentKind::Jpg, ImageFormat::Jpeg),
        (DocumentKind::Webp, ImageFormat::WebP),
        (DocumentKind::Gif, ImageFormat::Gif),
        (DocumentKind::Bmp, ImageFormat::Bmp),
        (DocumentKind::Tiff, ImageFormat::Tiff),
    ] {
        let options = options();
        let pdf = prepared(encoded(format), kind, &options).await;
        assert!(pdf.len() <= options.max_pdf_bytes);
        assert_one_page(&pdf);
    }
}

#[tokio::test]
async fn image_content_must_match_declaration_and_be_well_formed() {
    let w = workers();
    let o = options();
    assert!(
        prepare(
            encoded(ImageFormat::Png),
            DocumentKind::Jpeg,
            &o,
            &w,
            Duration::from_secs(1)
        )
        .await
        .is_err()
    );
    assert!(
        prepare(
            b"not an image".to_vec(),
            DocumentKind::Png,
            &o,
            &w,
            Duration::from_secs(1)
        )
        .await
        .is_err()
    );
}

#[tokio::test]
async fn png_alpha_is_flattened_to_white_and_rgb_is_flate_compressed() {
    let image = DynamicImage::ImageRgba8(ImageBuffer::from_pixel(32, 32, Rgba([0, 0, 0, 0])));
    let mut png = Vec::new();
    image
        .write_to(&mut Cursor::new(&mut png), ImageFormat::Png)
        .unwrap();
    let pdf = prepared(png, DocumentKind::Png, &options()).await;
    let document = lopdf::Document::load_mem(&pdf).unwrap();
    let stream = document
        .objects
        .values()
        .find_map(|o| {
            let stream = o.as_stream().ok()?;
            (stream.dict.get(b"Subtype").ok()?.as_name().ok()? == b"Image").then_some(stream)
        })
        .unwrap();
    assert_eq!(
        stream.dict.get(b"Filter").unwrap().as_name().unwrap(),
        b"FlateDecode"
    );
    assert_eq!(
        &stream.decompressed_content().unwrap()[..3],
        &[255, 255, 255]
    );
}

#[tokio::test]
async fn jpeg_exif_orientation_swaps_embedded_image_dimensions() {
    let pdf = prepared(oriented_jpeg(), DocumentKind::Jpeg, &options()).await;
    let document = lopdf::Document::load_mem(&pdf).unwrap();
    let image = document
        .objects
        .values()
        .find_map(|o| {
            let stream = o.as_stream().ok()?;
            (stream.dict.get(b"Subtype").ok()?.as_name().ok()? == b"Image").then_some(stream)
        })
        .unwrap();
    assert_eq!(image.dict.get(b"Width").unwrap().as_i64().unwrap(), 3);
    assert_eq!(image.dict.get(b"Height").unwrap().as_i64().unwrap(), 2);
}

#[tokio::test]
async fn jp2_fixture_is_validated_and_embedded_as_jpx() {
    let jp2 = include_bytes!("fixtures/input/tiny-rgb.jp2").to_vec();
    assert!(hayro_jpeg2000::Image::new(&jp2, &hayro_jpeg2000::DecodeSettings::default()).is_ok());
    let pdf = prepared(jp2.clone(), DocumentKind::Jp2, &options()).await;
    assert_one_page(&pdf);
    let document = lopdf::Document::load_mem(&pdf).unwrap();
    let image = document
        .objects
        .values()
        .find_map(|o| {
            let stream = o.as_stream().ok()?;
            (stream.dict.get(b"Subtype").ok()?.as_name().ok()? == b"Image").then_some(stream)
        })
        .unwrap();
    assert_eq!(
        image.dict.get(b"Filter").unwrap().as_name().unwrap(),
        b"JPXDecode"
    );
    let data: Arc<dyn AsRef<[u8]> + Send + Sync> = Arc::new(pdf);
    let rendered = hayro::render(
        hayro::hayro_syntax::Pdf::new(data)
            .unwrap()
            .pages()
            .get(0)
            .unwrap(),
        &hayro::hayro_interpret::InterpreterSettings::default(),
        &hayro::RenderSettings {
            x_scale: 200.0 / 72.0,
            y_scale: 200.0 / 72.0,
            width: Some(1),
            height: Some(1),
            bg_color: hayro::vello_cpu::color::palette::css::WHITE,
        },
    );
    assert_eq!(rendered.data_as_u8_slice().len(), 4);
    let w = workers();
    assert!(
        prepare(
            jp2[..jp2.len() / 2].to_vec(),
            DocumentKind::Jp2,
            &options(),
            &w,
            Duration::from_secs(1)
        )
        .await
        .is_err()
    );
    for o in [
        OfficialPdfOptions {
            max_page_pixels: 0,
            ..options()
        },
        OfficialPdfOptions {
            max_rendered_image_bytes: 1,
            ..options()
        },
    ] {
        assert!(
            prepare(
                jp2.clone(),
                DocumentKind::Jp2,
                &o,
                &w,
                Duration::from_secs(1)
            )
            .await
            .is_err()
        );
    }
}

#[tokio::test]
async fn limits_and_deadlines_reject_before_unbounded_work() {
    let png = encoded(ImageFormat::Png);
    let w = workers();
    assert!(
        prepare(
            png.clone(),
            DocumentKind::Png,
            &options(),
            &w,
            Duration::ZERO
        )
        .await
        .is_err()
    );
    let too_small = OfficialPdfOptions {
        max_pdf_bytes: png.len() - 1,
        ..options()
    };
    assert!(
        prepare(
            png.clone(),
            DocumentKind::Png,
            &too_small,
            &w,
            Duration::from_secs(1)
        )
        .await
        .is_err()
    );
    let pixels = OfficialPdfOptions {
        max_page_pixels: 5,
        ..options()
    };
    assert!(
        prepare(png, DocumentKind::Png, &pixels, &w, Duration::from_secs(1))
            .await
            .is_err()
    );
}

#[tokio::test]
async fn non_pdf_page_ranges_are_normalized_before_validation() {
    let invalid_range = OfficialPdfOptions {
        start_page: 2,
        end_page: Some(1),
        ..options()
    };
    let image = prepare(
        encoded(ImageFormat::Png),
        DocumentKind::Png,
        &invalid_range,
        &workers(),
        Duration::from_secs(1),
    )
    .await
    .unwrap();
    assert_eq!(image.kind, DocumentKind::Png);
    assert_one_page(&image.bytes);
}

#[tokio::test]
async fn gif_dimensions_are_limited_before_frame_decode() {
    // GIF header declares 3x3, but the missing image data would fail frame decoding.
    let gif = b"GIF89a\x03\0\x03\0\x80\0\0\0\0\0\xff\xff\xff,\0\0\0\0\x03\0\x03\0\0\x02\x01\0\0;"
        .to_vec();
    let options = OfficialPdfOptions {
        max_page_pixels: 4,
        ..options()
    };
    assert_eq!(
        prepare(
            gif,
            DocumentKind::Gif,
            &options,
            &workers(),
            Duration::from_secs(1)
        )
        .await
        .unwrap_err(),
        "image exceeds limits"
    );
}

#[tokio::test]
async fn near_zero_raster_deadline_waits_for_its_tracked_owner() {
    let raster_workers = RasterWorkers::default();
    let result = core_prepare(
        encoded(ImageFormat::Png),
        DocumentKind::Png,
        &options(),
        &workers(),
        &raster_workers,
        Duration::from_nanos(1),
    )
    .await;
    assert_eq!(result.unwrap_err(), "input preparation deadline expired");
    raster_workers.drain().await;
}

#[tokio::test]
async fn prepared_snapshots_keep_pdf_storage_and_exact_inputs() {
    let workers = workers();
    let raster = RasterWorkers::default();
    let pdf = Bytes::from_static(b"%PDF-1.4\n%%EOF");
    let prepared_pdf = core_prepare(
        pdf.clone(),
        DocumentKind::Pdf,
        &options(),
        &workers,
        &raster,
        Duration::from_secs(1),
    )
    .await
    .unwrap();
    assert_eq!(prepared_pdf.bytes.as_ptr(), prepared_pdf.original.as_ptr());
    let image = Bytes::from(encoded(ImageFormat::Png));
    let prepared_image = core_prepare(
        image.clone(),
        DocumentKind::Png,
        &options(),
        &workers,
        &raster,
        Duration::from_secs(1),
    )
    .await
    .unwrap();
    assert_eq!(prepared_image.original, image);
    raster.drain().await;
}

#[tokio::test]
async fn real_animated_and_multipage_images_are_rejected() {
    let mut gif = Vec::new();
    let mut encoder = image::codecs::gif::GifEncoder::new(&mut gif);
    encoder
        .encode_frame(image::Frame::new(rgba().to_rgba8()))
        .unwrap();
    encoder
        .encode_frame(image::Frame::new(rgba().to_rgba8()))
        .unwrap();
    drop(encoder);
    let mut tiff = Cursor::new(Vec::new());
    {
        let mut e = tiff::encoder::TiffEncoder::new(&mut tiff).unwrap();
        e.new_image::<tiff::encoder::colortype::RGB8>(2, 3)
            .unwrap()
            .write_data(&[0; 18])
            .unwrap();
        e.new_image::<tiff::encoder::colortype::RGB8>(2, 3)
            .unwrap()
            .write_data(&[0; 18])
            .unwrap();
    }
    let apng = include_bytes!("fixtures/input/two-frame.apng").to_vec();
    assert!(
        image::codecs::png::PngDecoder::new(Cursor::new(&apng))
            .unwrap()
            .is_apng()
            .unwrap()
    );
    let webp = include_bytes!("fixtures/input/two-frame.webp").to_vec();
    assert!(
        image::codecs::webp::WebPDecoder::new(Cursor::new(&webp))
            .unwrap()
            .has_animation()
    );
    let cases = [
        (gif, DocumentKind::Gif),
        (apng, DocumentKind::Png),
        (webp, DocumentKind::Webp),
        (tiff.into_inner(), DocumentKind::Tiff),
    ];
    for (bytes, kind) in cases {
        let error = prepare(bytes, kind, &options(), &workers(), Duration::from_secs(1))
            .await
            .unwrap_err();
        assert!(
            error.contains("animated or multi-page images are unsupported"),
            "{error}"
        );
    }
}

#[tokio::test]
async fn exact_image_and_pdf_limits_hold_at_the_boundary() {
    let png = encoded(ImageFormat::Png);
    let w = workers();
    for (field, ok) in [(6, true), (5, false)] {
        let o = OfficialPdfOptions {
            max_page_pixels: field,
            ..options()
        };
        assert_eq!(
            prepare(
                png.clone(),
                DocumentKind::Png,
                &o,
                &w,
                Duration::from_secs(1)
            )
            .await
            .is_ok(),
            ok
        );
    }
    for rendered in [true, false] {
        for (cap, ok) in [(18, true), (17, false)] {
            let o = if rendered {
                OfficialPdfOptions {
                    max_rendered_image_bytes: cap,
                    ..options()
                }
            } else {
                OfficialPdfOptions {
                    max_in_flight_image_bytes: cap,
                    ..options()
                }
            };
            assert_eq!(
                prepare(
                    png.clone(),
                    DocumentKind::Png,
                    &o,
                    &w,
                    Duration::from_secs(1)
                )
                .await
                .is_ok(),
                ok
            );
        }
    }
    let pdf = prepared(png.clone(), DocumentKind::Png, &options()).await;
    assert!(png.len() < pdf.len());
    let exact = OfficialPdfOptions {
        max_pdf_bytes: pdf.len(),
        ..options()
    };
    assert!(
        prepare(
            png.clone(),
            DocumentKind::Png,
            &exact,
            &w,
            Duration::from_secs(1)
        )
        .await
        .is_ok()
    );
    assert!(
        prepare(
            png,
            DocumentKind::Png,
            &OfficialPdfOptions {
                max_pdf_bytes: exact.max_pdf_bytes - 1,
                ..options()
            },
            &w,
            Duration::from_secs(1)
        )
        .await
        .is_err()
    );
    let source = b"%PDF-1.4\n%%EOF".to_vec();
    let exact_source = OfficialPdfOptions {
        max_pdf_bytes: source.len(),
        ..options()
    };
    assert!(
        prepare(
            source.clone(),
            DocumentKind::Pdf,
            &exact_source,
            &w,
            Duration::from_secs(1)
        )
        .await
        .is_ok()
    );
    assert!(
        prepare(
            source,
            DocumentKind::Pdf,
            &OfficialPdfOptions {
                max_pdf_bytes: exact_source.max_pdf_bytes - 1,
                ..options()
            },
            &w,
            Duration::from_secs(1)
        )
        .await
        .is_err()
    );
}

#[tokio::test]
async fn office_prepare_uses_real_helper_and_accepts_multiple_pages() {
    let workers =
        OfficeWorkers::with_executable(PathBuf::from(env!("CARGO_BIN_EXE_mineru-office-convert")))
            .unwrap();
    for (bytes, kind) in [
        (office_fixtures::docx(), DocumentKind::Docx),
        (office_fixtures::pptx(), DocumentKind::Pptx),
        (office_fixtures::xlsx(), DocumentKind::Xlsx),
    ] {
        let original = bytes.clone();
        let invalid_range = OfficialPdfOptions {
            start_page: 2,
            end_page: Some(1),
            ..options()
        };
        let value = prepare(
            bytes,
            kind,
            &invalid_range,
            &workers,
            Duration::from_secs(60),
        )
        .await
        .unwrap_or_else(|error| panic!("{kind:?}: {error}"));
        assert_eq!(value.kind, kind);
        assert_eq!(value.original, original);
        assert_one_page(&value.bytes);
    }
    let two = office_fixtures::pptx_two_slides();
    let value = prepare(
        two.clone(),
        DocumentKind::Pptx,
        &OfficialPdfOptions {
            max_pages: 2,
            ..options()
        },
        &workers,
        Duration::from_secs(60),
    )
    .await
    .unwrap();
    assert_eq!(
        lopdf::Document::load_mem(&value.bytes)
            .unwrap()
            .get_pages()
            .len(),
        2
    );
    assert!(
        prepare(
            two,
            DocumentKind::Pptx,
            &OfficialPdfOptions {
                max_pages: 1,
                ..options()
            },
            &workers,
            Duration::from_secs(60)
        )
        .await
        .is_err()
    );
    let bad_workers =
        OfficeWorkers::with_executable(PathBuf::from("definitely-not-a-helper")).unwrap();
    let error = prepare(
        office_fixtures::docx(),
        DocumentKind::Pptx,
        &options(),
        &bad_workers,
        Duration::from_secs(1),
    )
    .await
    .unwrap_err();
    assert_eq!(error, "suffix/content mismatch");
    bad_workers.drain().await;

    let source = office_fixtures::docx();
    let generated = prepare(
        source.clone(),
        DocumentKind::Docx,
        &options(),
        &workers,
        Duration::from_secs(60),
    )
    .await
    .unwrap()
    .bytes;
    assert!(source.len() < generated.len());
    assert!(
        prepare(
            source,
            DocumentKind::Docx,
            &OfficialPdfOptions {
                max_pdf_bytes: generated.len() - 1,
                ..options()
            },
            &workers,
            Duration::from_secs(60),
        )
        .await
        .is_err()
    );
    workers.drain().await;

    let abort_workers =
        OfficeWorkers::with_executable(PathBuf::from(env!("CARGO_BIN_EXE_mineru-office-convert")))
            .unwrap();
    let task_workers = abort_workers.clone();
    let task = tokio::spawn(async move {
        prepare(
            office_fixtures::docx_large(),
            DocumentKind::Docx,
            &options(),
            &task_workers,
            Duration::from_secs(10),
        )
        .await
    });
    tokio::task::yield_now().await;
    tokio::time::sleep(Duration::from_millis(10)).await;
    task.abort();
    let _ = task.await;
    tokio::time::timeout(Duration::from_secs(10), abort_workers.drain())
        .await
        .unwrap();

    let deadline_workers =
        OfficeWorkers::with_executable(PathBuf::from(env!("CARGO_BIN_EXE_mineru-office-convert")))
            .unwrap();
    let _ = prepare(
        office_fixtures::docx(),
        DocumentKind::Docx,
        &options(),
        &deadline_workers,
        Duration::from_nanos(1),
    )
    .await;
    deadline_workers.drain().await;
}
