//! Conservative native Markdown assessment for text PDFs.

use pdf_inspector::{DetectionConfig, PdfOptions, PdfType, ProcessMode, ScanStrategy};
use std::collections::{HashMap, HashSet};

pub(crate) const NATIVE_PDF_METADATA_SCHEMA_VERSION: u16 = 1;
pub(crate) const NATIVE_PDF_PROVENANCE: &str = "anydoc::Format::Pdf via pdf-inspector";
const MIN_NATIVE_CONFIDENCE: f32 = 0.9;
const MIN_ALPHANUMERIC_CHARS: usize = 4;
const MAX_RESOURCE_DEPTH: usize = 32;

/// Stable internal assessment metadata. The schema is intentionally not exposed as an output
/// file: native Markdown has no official JSON/profile to impersonate.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct NativePdfMetadata {
    pub(crate) schema_version: u16,
    pub(crate) page_count: usize,
    pub(crate) accepted: bool,
    pub(crate) images_present: bool,
    pub(crate) reason_codes: Vec<&'static str>,
    pub(crate) provenance: &'static str,
}

#[derive(Debug)]
pub(crate) struct NativePdfAssessment {
    pub(crate) metadata: NativePdfMetadata,
    pub(crate) markdown: Option<Vec<u8>>,
}

impl NativePdfAssessment {
    pub(crate) fn rejection_message(&self) -> String {
        let reasons = if self.metadata.reason_codes.is_empty() {
            "unknown".to_owned()
        } else {
            self.metadata.reason_codes.join(", ")
        };
        format!(
            "native PDF Markdown unavailable: assessment rejected ({reasons}); backend=local has no VLM fallback"
        )
    }
}

pub(crate) fn assess(
    bytes: &[u8],
    max_pages: usize,
    max_output_bytes: usize,
) -> NativePdfAssessment {
    let mut metadata = NativePdfMetadata {
        schema_version: NATIVE_PDF_METADATA_SCHEMA_VERSION,
        page_count: 0,
        accepted: false,
        images_present: false,
        reason_codes: Vec::new(),
        provenance: NATIVE_PDF_PROVENANCE,
    };

    let source_pages = match lopdf::Document::load_metadata_mem(bytes) {
        Ok(metadata) => metadata.page_count as usize,
        Err(_) => {
            push_reason(&mut metadata, "invalid_or_unsupported_pdf");
            return NativePdfAssessment {
                metadata,
                markdown: None,
            };
        }
    };
    metadata.page_count = source_pages;
    if source_pages == 0 {
        push_reason(&mut metadata, "empty_pdf");
    }
    if source_pages > max_pages {
        push_reason(&mut metadata, "page_limit");
    }
    if !metadata.reason_codes.is_empty() {
        return NativePdfAssessment {
            metadata,
            markdown: None,
        };
    }

    // Share one parsed document across page-content and resource/image inspection.
    let image_result = lopdf::Document::load_mem(bytes)
        .map_err(|_| ())
        .and_then(|document| has_page_images_in_document(&document, bytes.len()));
    match image_result {
        Ok(true) => {
            metadata.images_present = true;
            push_reason(&mut metadata, "images_present");
            push_reason(&mut metadata, "mixed_pdf");
        }
        Ok(false) => {}
        Err(()) => push_reason(&mut metadata, "image_detection_uncertain"),
    }

    let options = PdfOptions::new()
        .mode(ProcessMode::Full)
        .detection(DetectionConfig {
            // A fast sample can miss a scanned page in a large mixed document. Native output is
            // a quality claim, so the assessment deliberately scans every page.
            strategy: ScanStrategy::Full,
            ..DetectionConfig::default()
        });
    let result = match pdf_inspector::process_pdf_mem_with_options(bytes, options) {
        Ok(result) => result,
        Err(_) => {
            push_reason(&mut metadata, "invalid_or_unsupported_pdf");
            return NativePdfAssessment {
                metadata,
                markdown: None,
            };
        }
    };
    metadata.page_count = result.page_count as usize;
    if metadata.page_count != source_pages {
        push_reason(&mut metadata, "page_count_mismatch");
    }
    match result.pdf_type {
        PdfType::TextBased => {}
        PdfType::Scanned => push_reason(&mut metadata, "scanned"),
        PdfType::ImageBased => push_reason(&mut metadata, "image_based"),
        PdfType::Mixed => push_reason(&mut metadata, "mixed_pdf"),
    }
    if !result.pages_needing_ocr.is_empty() {
        push_reason(&mut metadata, "ocr_required");
    }
    if result.has_encoding_issues {
        push_reason(&mut metadata, "encoding_issues");
    }
    if result.layout.is_complex {
        push_reason(&mut metadata, "complex_layout");
    }
    if result.confidence < MIN_NATIVE_CONFIDENCE {
        push_reason(&mut metadata, "low_confidence");
    }
    if let Some(markdown) = result.markdown.as_deref() {
        if let Some(reason) = quality_reason(markdown, max_output_bytes) {
            push_reason(&mut metadata, reason);
        }
    } else {
        push_reason(&mut metadata, "empty_markdown");
    }

    let pages = match pdf_inspector::extract_pages_markdown_mem(bytes, None) {
        Ok(pages) => pages,
        Err(_) => {
            push_reason(&mut metadata, "page_assessment_unavailable");
            return NativePdfAssessment {
                metadata,
                markdown: None,
            };
        }
    };
    if pages.pages.len() != source_pages {
        push_reason(&mut metadata, "page_coverage_uncertain");
    }
    let mut reliable_pages = 0;
    let mut unreliable_pages = 0;
    for page in &pages.pages {
        if page.needs_ocr || quality_reason(&page.markdown, max_output_bytes).is_some() {
            unreliable_pages += 1;
        } else {
            reliable_pages += 1;
        }
    }
    if unreliable_pages > 0 {
        push_reason(&mut metadata, "page_needs_ocr");
        if reliable_pages > 0 {
            push_reason(&mut metadata, "mixed_pdf");
        }
    }
    if pages.is_complex {
        push_reason(&mut metadata, "complex_layout");
    }
    if !metadata.reason_codes.is_empty() {
        return NativePdfAssessment {
            metadata,
            markdown: None,
        };
    }

    // AnyDoc's public PDF route is the contract used by the bundled helper's local mode. Keep its
    // output rather than copying pdf-inspector's internal Markdown, while using the public
    // inspector result only for the conservative assessment above. Logs are intentionally not
    // consulted.
    let markdown = match anydoc::to_markdown_bytes(bytes, Some(anydoc::Format::Pdf)) {
        Ok(markdown) => markdown,
        Err(_) => {
            push_reason(&mut metadata, "native_extraction_failed");
            return NativePdfAssessment {
                metadata,
                markdown: None,
            };
        }
    };
    if let Some(reason) = quality_reason(&markdown, max_output_bytes) {
        push_reason(&mut metadata, reason);
        return NativePdfAssessment {
            metadata,
            markdown: None,
        };
    }
    metadata.accepted = true;
    push_reason(&mut metadata, "accepted");
    NativePdfAssessment {
        metadata,
        markdown: Some(markdown.into_bytes()),
    }
}

fn push_reason(metadata: &mut NativePdfMetadata, reason: &'static str) {
    if !metadata.reason_codes.contains(&reason) {
        metadata.reason_codes.push(reason);
    }
}

#[derive(Default)]
struct ImageScanCache {
    form_inline_images: HashMap<lopdf::ObjectId, Result<bool, ()>>,
}

#[cfg(test)]
fn has_page_images(bytes: &[u8]) -> Result<bool, ()> {
    let document = lopdf::Document::load_mem(bytes).map_err(|_| ())?;
    has_page_images_in_document(&document, bytes.len())
}

/// Detects image XObjects reachable from page resources and inline images in page content without
/// relying on pdf-inspector's internal logging or optional image-emission settings. An undecidable
/// resource graph or content stream rejects the native lane rather than silently dropping a visible
/// image.
fn has_page_images_in_document(
    document: &lopdf::Document,
    content_limit: usize,
) -> Result<bool, ()> {
    // Cache only immutable form-content decoding; resource recursion stays path-local so cycles
    // and depth failures remain fail-closed.
    let mut cache = ImageScanCache::default();
    for page_id in document.get_pages().values().copied() {
        let content = document
            .get_page_content_with_limit(page_id, content_limit)
            .map_err(|_| ())?;
        let content = lopdf::content::Content::decode(&content).map_err(|_| ())?;
        if content
            .operations
            .iter()
            .any(|operation| operation.operator == "BI")
        {
            return Ok(true);
        }
        if page_resources_have_images(
            document,
            page_id,
            &mut HashSet::new(),
            0,
            content_limit,
            &mut cache,
        )? {
            return Ok(true);
        }
    }
    Ok(false)
}

fn page_resources_have_images(
    document: &lopdf::Document,
    mut page_id: lopdf::ObjectId,
    seen_pages: &mut HashSet<lopdf::ObjectId>,
    mut depth: usize,
    content_limit: usize,
    cache: &mut ImageScanCache,
) -> Result<bool, ()> {
    loop {
        check_resource_depth(depth)?;
        if !seen_pages.insert(page_id) {
            return Err(());
        }
        let page = document
            .get_object(page_id)
            .map_err(|_| ())?
            .as_dict()
            .map_err(|_| ())?;
        if let Ok(resources) = page.get(b"Resources") {
            return resource_object_has_images(
                document,
                resources,
                &mut HashSet::new(),
                depth,
                content_limit,
                cache,
            );
        }
        page_id = match page.get(b"Parent") {
            Ok(lopdf::Object::Reference(parent)) => *parent,
            _ => return Ok(false),
        };
        depth = next_resource_depth(depth)?;
    }
}

fn resource_object_has_images(
    document: &lopdf::Document,
    object: &lopdf::Object,
    seen_objects: &mut HashSet<lopdf::ObjectId>,
    depth: usize,
    content_limit: usize,
    cache: &mut ImageScanCache,
) -> Result<bool, ()> {
    check_resource_depth(depth)?;
    let resources = resolve_dictionary(document, object, seen_objects, depth)?;
    let Some(xobjects) = resources.get(b"XObject").ok() else {
        return Ok(false);
    };
    let child_depth = next_resource_depth(depth)?;
    let xobjects = resolve_dictionary(document, xobjects, seen_objects, child_depth)?;
    for (_, xobject) in xobjects.iter() {
        if object_has_image(
            document,
            xobject,
            seen_objects,
            child_depth,
            content_limit,
            cache,
            None,
        )? {
            return Ok(true);
        }
    }
    Ok(false)
}

fn object_has_image(
    document: &lopdf::Document,
    object: &lopdf::Object,
    seen_objects: &mut HashSet<lopdf::ObjectId>,
    depth: usize,
    content_limit: usize,
    cache: &mut ImageScanCache,
    object_id: Option<lopdf::ObjectId>,
) -> Result<bool, ()> {
    check_resource_depth(depth)?;
    match object {
        lopdf::Object::Reference(id) => {
            if !seen_objects.insert(*id) {
                return Err(());
            }
            let result = object_has_image(
                document,
                document.get_object(*id).map_err(|_| ())?,
                seen_objects,
                next_resource_depth(depth)?,
                content_limit,
                cache,
                Some(*id),
            );
            seen_objects.remove(id);
            result
        }
        lopdf::Object::Stream(stream) => {
            let subtype = stream
                .dict
                .get(b"Subtype")
                .ok()
                .and_then(|value| value.as_name().ok());
            if subtype == Some(b"Image".as_slice()) {
                return Ok(true);
            }
            if subtype == Some(b"Form".as_slice()) {
                if form_stream_has_inline_image(stream, content_limit, cache, object_id)? {
                    return Ok(true);
                }
                if let Ok(resources) = stream.dict.get(b"Resources") {
                    return resource_object_has_images(
                        document,
                        resources,
                        seen_objects,
                        next_resource_depth(depth)?,
                        content_limit,
                        cache,
                    );
                }
            }
            Ok(false)
        }
        // Every page XObject must be an indirect/direct stream. Treat malformed or opaque values
        // as undecidable so native Markdown never claims to preserve a resource it cannot inspect.
        _ => Err(()),
    }
}

fn form_stream_has_inline_image(
    stream: &lopdf::Stream,
    content_limit: usize,
    cache: &mut ImageScanCache,
    object_id: Option<lopdf::ObjectId>,
) -> Result<bool, ()> {
    if let Some(id) = object_id
        && let Some(result) = cache.form_inline_images.get(&id)
    {
        return *result;
    }
    let result = (|| {
        if stream.dict.has(b"Filter") && stream.filters().is_err() {
            return Err(());
        }
        let content = stream
            .get_plain_content_with_limit(content_limit)
            .map_err(|_| ())?;
        let content = lopdf::content::Content::decode(&content).map_err(|_| ())?;
        Ok(content
            .operations
            .iter()
            .any(|operation| operation.operator == "BI"))
    })();
    if let Some(id) = object_id {
        cache.form_inline_images.insert(id, result);
    }
    result
}

fn resolve_dictionary<'a>(
    document: &'a lopdf::Document,
    object: &'a lopdf::Object,
    seen_objects: &mut HashSet<lopdf::ObjectId>,
    depth: usize,
) -> Result<lopdf::Dictionary, ()> {
    check_resource_depth(depth)?;
    match object {
        lopdf::Object::Reference(id) => {
            if !seen_objects.insert(*id) {
                return Err(());
            }
            let result = resolve_dictionary(
                document,
                document.get_object(*id).map_err(|_| ())?,
                seen_objects,
                next_resource_depth(depth)?,
            );
            seen_objects.remove(id);
            result
        }
        lopdf::Object::Dictionary(dictionary) => Ok(dictionary.clone()),
        _ => Err(()),
    }
}

fn check_resource_depth(depth: usize) -> Result<(), ()> {
    if depth > MAX_RESOURCE_DEPTH {
        Err(())
    } else {
        Ok(())
    }
}

fn next_resource_depth(depth: usize) -> Result<usize, ()> {
    let next = depth.checked_add(1).ok_or(())?;
    check_resource_depth(next)?;
    Ok(next)
}

fn quality_reason(markdown: &str, max_output_bytes: usize) -> Option<&'static str> {
    let trimmed = markdown.trim();
    if trimmed.is_empty() {
        return Some("empty_markdown");
    }
    if markdown.len() > max_output_bytes {
        return Some("output_limit");
    }
    if markdown.contains('\u{fffd}')
        || markdown
            .chars()
            .any(|c| c == '\0' || c.is_control() && !matches!(c, '\n' | '\r' | '\t'))
    {
        return Some("garbled_markdown");
    }
    let alphanumeric = trimmed.chars().filter(|c| c.is_alphanumeric()).count();
    if alphanumeric < MIN_ALPHANUMERIC_CHARS {
        return Some("low_quality");
    }
    None
}

#[cfg(test)]
mod tests;
