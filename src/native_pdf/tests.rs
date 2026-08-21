use super::*;

fn clean_text_pdf(inline_image: bool) -> Vec<u8> {
    use lopdf::{Document, Object, Stream, dictionary};

    let mut document = Document::with_version("1.5");
    let pages = document.new_object_id();
    let font = document.add_object(dictionary! {
        "Type" => "Font",
        "Subtype" => "Type1",
        "BaseFont" => "Helvetica",
    });
    let content = if inline_image {
        b"BT /F1 12 Tf 72 720 Td (Native PDF text contains enough clean words for the conservative native assessment.) Tj 0 -20 Td (second line keeps the document readable and long enough for sparse extraction checks.) Tj 0 -20 Td (third line confirms ordinary text operators and stable extraction.) Tj ET\nBI\n/W 1\n/H 1\n/BPC 8\n/CS /DeviceRGB\nID\n\0\0\0\nEI".to_vec()
    } else {
        b"BT /F1 12 Tf 72 720 Td (Native PDF text contains enough clean words for the conservative native assessment.) Tj 0 -20 Td (second line keeps the document readable and long enough for sparse extraction checks.) Tj 0 -20 Td (third line confirms ordinary text operators and stable extraction.) Tj ET".to_vec()
    };
    let contents = document.add_object(Stream::new(dictionary! {}, content));
    let page = document.add_object(dictionary! {
        "Type" => "Page",
        "Parent" => pages,
        "MediaBox" => vec![0.into(), 0.into(), 612.into(), 792.into()],
        "Resources" => dictionary! { "Font" => dictionary! { "F1" => font } },
        "Contents" => contents,
    });
    document.objects.insert(
        pages,
        Object::Dictionary(dictionary! {
            "Type" => "Pages",
            "Kids" => vec![page.into()],
            "Count" => 1,
        }),
    );
    let catalog = document.add_object(dictionary! { "Type" => "Catalog", "Pages" => pages });
    document.trailer.set("Root", catalog);
    let mut bytes = Vec::new();
    document.save_to(&mut bytes).unwrap();
    bytes
}

fn save_single_page_pdf(
    document: &mut lopdf::Document,
    pages: lopdf::ObjectId,
    contents: lopdf::ObjectId,
    resources: lopdf::Object,
) -> Vec<u8> {
    use lopdf::dictionary;

    let page = document.add_object(lopdf::dictionary! {
        "Type" => "Page",
        "Parent" => pages,
        "MediaBox" => vec![0.into(), 0.into(), 612.into(), 792.into()],
        "Resources" => resources,
        "Contents" => contents,
    });
    document.objects.insert(
        pages,
        lopdf::Object::Dictionary(lopdf::dictionary! {
            "Type" => "Pages",
            "Kids" => vec![page.into()],
            "Count" => 1,
        }),
    );
    let catalog = document.add_object(lopdf::dictionary! { "Type" => "Catalog", "Pages" => pages });
    document.trailer.set("Root", catalog);
    let mut bytes = Vec::new();
    document.save_to(&mut bytes).unwrap();
    bytes
}

fn form_inline_image_pdf() -> Vec<u8> {
    use lopdf::{Document, Stream, dictionary};

    let mut document = Document::with_version("1.5");
    let pages = document.new_object_id();
    let form = document.add_object(Stream::new(
        dictionary! {
            "Type" => "XObject",
            "Subtype" => "Form",
            "BBox" => vec![0.into(), 0.into(), 1.into(), 1.into()],
        },
        b"BI\n/W 1\n/H 1\n/BPC 8\n/CS /DeviceGray\nID\n\0\nEI".to_vec(),
    ));
    let contents = document.add_object(Stream::new(dictionary! {}, b"/Fm1 Do".to_vec()));
    save_single_page_pdf(
        &mut document,
        pages,
        contents,
        dictionary! { "XObject" => dictionary! { "Fm1" => form } }.into(),
    )
}

fn deeply_nested_form_pdf() -> Vec<u8> {
    use lopdf::{Document, Stream, dictionary};

    let mut document = Document::with_version("1.5");
    let pages = document.new_object_id();
    let mut child = None;
    for _ in 0..=MAX_RESOURCE_DEPTH {
        let mut form_dict = dictionary! {
            "Type" => "XObject",
            "Subtype" => "Form",
            "BBox" => vec![0.into(), 0.into(), 1.into(), 1.into()],
        };
        if let Some(child_id) = child {
            form_dict.set(
                "Resources",
                dictionary! { "XObject" => dictionary! { "Next" => child_id } },
            );
        }
        child = Some(document.add_object(Stream::new(form_dict, b"q Q".to_vec())));
    }
    let contents = document.add_object(Stream::new(dictionary! {}, b"/Top Do".to_vec()));
    save_single_page_pdf(
        &mut document,
        pages,
        contents,
        dictionary! { "XObject" => dictionary! { "Top" => child.unwrap() } }.into(),
    )
}

fn cyclic_form_pdf() -> Vec<u8> {
    use lopdf::{Document, Object, Stream, dictionary};

    let mut document = Document::with_version("1.5");
    let pages = document.new_object_id();
    let form_id = document.new_object_id();
    document.objects.insert(
        form_id,
        Object::Stream(Stream::new(
            dictionary! {
                "Type" => "XObject",
                "Subtype" => "Form",
                "BBox" => vec![0.into(), 0.into(), 1.into(), 1.into()],
            },
            b"q Q".to_vec(),
        )),
    );
    document
        .get_object_mut(form_id)
        .unwrap()
        .as_stream_mut()
        .unwrap()
        .dict
        .set(
            "Resources",
            dictionary! { "XObject" => dictionary! { "Self" => form_id } },
        );
    let contents = document.add_object(Stream::new(dictionary! {}, b"/Top Do".to_vec()));
    save_single_page_pdf(
        &mut document,
        pages,
        contents,
        dictionary! { "XObject" => dictionary! { "Top" => form_id } }.into(),
    )
}

#[test]
fn clean_text_pdf_uses_versioned_native_assessment() {
    let bytes = clean_text_pdf(false);
    let assessment = assess(&bytes, 10_000, 1024 * 1024);
    assert!(assessment.metadata.accepted, "{:?}", assessment.metadata);
    assert_eq!(assessment.metadata.schema_version, 1);
    assert_eq!(assessment.metadata.page_count, 1);
    assert!(!assessment.metadata.images_present);
    assert_eq!(assessment.metadata.reason_codes, vec!["accepted"]);
    assert_eq!(assessment.metadata.provenance, NATIVE_PDF_PROVENANCE);
    assert!(
        String::from_utf8(assessment.markdown.unwrap())
            .unwrap()
            .contains("Native PDF text")
    );
}

#[test]
fn inline_image_pdf_is_rejected_as_image_bearing() {
    let bytes = clean_text_pdf(true);
    let assessment = assess(&bytes, 10_000, 1024 * 1024);

    assert!(!assessment.metadata.accepted, "{:?}", assessment.metadata);
    assert!(assessment.metadata.images_present);
    assert!(assessment.metadata.reason_codes.contains(&"images_present"));
}

#[test]
fn form_inline_image_is_detected_without_an_image_xobject() {
    let bytes = form_inline_image_pdf();
    assert_eq!(has_page_images(&bytes), Ok(true));
    let document = lopdf::Document::load_mem(&bytes).unwrap();
    assert_eq!(
        has_page_images_in_document(&document, bytes.len()),
        Ok(true)
    );
}

#[test]
fn resource_depth_limit_is_fail_closed() {
    assert!(has_page_images(&deeply_nested_form_pdf()).is_err());
}

#[test]
fn cyclic_form_resources_are_fail_closed() {
    assert!(has_page_images(&cyclic_form_pdf()).is_err());
}

#[test]
fn quality_check_rejects_empty_short_and_garbled_text() {
    assert_eq!(quality_reason("\n", 100), Some("empty_markdown"));
    assert_eq!(quality_reason("x", 100), Some("low_quality"));
    assert_eq!(
        quality_reason("good\u{fffd}", 100),
        Some("garbled_markdown")
    );
    assert!(quality_reason("clean text", 100).is_none());
}
