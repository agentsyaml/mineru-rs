//! Shared AnyDoc conversion for the bundled Rust helper's `backend=local` lane and legacy PDF
//! fallback.

use crate::{command::service::OfficeLimits, input_prepare::DocumentKind};
use lopdf::{Document, Object, Stream, dictionary};

/// The explicit warning attached to the non-local legacy-to-PDF fallback.
pub const LEGACY_PDF_WARNING: &str = "legacy format uses a text-only best-effort PDF fallback; original layout, images, tables, formulas, and macros may be lost, and non-ASCII characters may be replaced with '?'";

/// The user-visible recommendation shared by successful warnings and conversion failures.
pub const LEGACY_PDF_RECOMMENDATION: &str = "for better results, first convert the file with Microsoft Office or LibreOffice to DOCX, XLSX, or PPTX";

/// Returns the AnyDoc format selected by a legacy document kind.
pub fn format_for_kind(kind: DocumentKind) -> Option<anydoc::Format> {
    Some(match kind {
        DocumentKind::Doc => anydoc::Format::Doc,
        DocumentKind::Ppt => anydoc::Format::Ppt,
        DocumentKind::Xls => anydoc::Format::Excel,
        DocumentKind::Odt => anydoc::Format::Odt,
        DocumentKind::Rtf => anydoc::Format::Rtf,
        DocumentKind::Epub => anydoc::Format::Epub,
        DocumentKind::Ods => anydoc::Format::Ods,
        DocumentKind::Odp => anydoc::Format::Odp,
        DocumentKind::Csv => anydoc::Format::Csv,
        _ => return None,
    })
}

/// Resolves the closed legacy suffix set used by the helper and direct CLI.
pub fn kind_from_name(name: &str) -> Option<DocumentKind> {
    DocumentKind::from_suffix(name).filter(|kind| kind.is_legacy_office() && kind.suffix() == name)
}

/// Cross-validates a declared legacy kind against the input signature.
pub fn format_matches(kind: DocumentKind, input: &[u8]) -> bool {
    let Some(format) = format_for_kind(kind) else {
        return false;
    };
    let input = input.strip_prefix(b"\xef\xbb\xbf").unwrap_or(input);
    kind == DocumentKind::Csv
        || anydoc::Format::from_bytes(input).is_some_and(|found| found == format)
}

/// Converts one legacy document to Markdown while retaining the helper's resource and validation
/// contract. This function is synchronous by design; callers that already run asynchronously
/// should invoke it from a blocking worker.
pub fn to_markdown_bytes(
    kind: DocumentKind,
    input: &[u8],
    limits: OfficeLimits,
) -> Result<Vec<u8>, String> {
    if input.len() > limits.input_bytes {
        return Err(format!(
            "input too large: office input exceeds limit of {} bytes; limit {} bytes; raise with --office-input-bytes or MINERU_OFFICE_INPUT_BYTES",
            input.len(),
            limits.input_bytes
        ));
    }
    let format = format_for_kind(kind).ok_or("legacy office kind required")?;
    let input = input.strip_prefix(b"\xef\xbb\xbf").unwrap_or(input);
    if !format_matches(kind, input) {
        return Err("input format does not match requested format".into());
    }
    let markdown = anydoc::to_markdown_bytes(input, Some(format))
        .map_err(|error| format!("conversion failed: {error}"))?
        .into_bytes();
    if markdown.len() > limits.output_bytes {
        return Err("conversion produced oversized output".into());
    }
    Ok(markdown)
}

/// Runs the conservative native PDF assessment and returns Markdown only when it is accepted.
/// Rejections retain the stable local-lane message so the parent does not need to inspect the PDF.
pub fn native_pdf_to_markdown(
    input: &[u8],
    max_pages: usize,
    max_output_bytes: usize,
) -> Result<Vec<u8>, String> {
    let assessment = crate::native_pdf::assess(input, max_pages, max_output_bytes);
    if !assessment.metadata.accepted {
        return Err(assessment.rejection_message());
    }
    assessment
        .markdown
        .ok_or_else(|| "native PDF assessment omitted Markdown".into())
}

/// Converts a legacy document through AnyDoc Markdown into a bounded, valid text PDF.
///
/// This is deliberately a fallback, not a claim that Office layout is preserved. The PDF
/// uses the built-in Helvetica Type1 font and ASCII-safe text; characters without a portable
/// built-in glyph are represented as `?` so the generated file remains valid and renderable.
pub fn to_pdf_bytes(
    kind: DocumentKind,
    input: &[u8],
    limits: OfficeLimits,
) -> Result<Vec<u8>, String> {
    let markdown_cap = limits
        .output_bytes
        .checked_sub(PDF_FIXED_RESERVE)
        .ok_or_else(|| "conversion produced oversized output".to_owned())?;
    let mut markdown_limits = limits;
    // Leave room for PDF objects/xref before asking AnyDoc to materialize Markdown. The parser
    // remains bounded by the same input cap, and the PDF builder below applies the tighter
    // incremental budget while it consumes this already-capped text.
    markdown_limits.output_bytes = markdown_cap;
    let markdown = to_markdown_bytes(kind, input, markdown_limits)?;
    markdown_to_pdf(&markdown, limits.output_bytes)
}

const PDF_FIXED_RESERVE: usize = 4096;
const PDF_PAGE_RESERVE: usize = 1024;
const PDF_OBJECT_RESERVE: usize = 32;
const PDF_LINE_RESERVE: usize = 32;
const PDF_MAX_CONTENT_STREAM_BYTES: usize = 16 * 1024;
const PDF_MAX_LINE_BYTES: usize = 96;
const PDF_LINES_PER_PAGE: usize = 50;

struct PdfBudget {
    output_cap: usize,
    estimated_bytes: usize,
    content_bytes: usize,
    page_count: usize,
    line_count: usize,
    object_count: usize,
    max_pages: usize,
    max_lines: usize,
    max_objects: usize,
}

impl PdfBudget {
    fn new(output_cap: usize) -> Result<Self, String> {
        let max_pages = output_cap
            .checked_sub(PDF_FIXED_RESERVE)
            .map(|remaining| remaining / PDF_PAGE_RESERVE)
            .filter(|pages| *pages > 0)
            .ok_or_else(|| "conversion produced oversized output".to_owned())?;
        Ok(Self {
            output_cap,
            estimated_bytes: PDF_FIXED_RESERVE,
            content_bytes: 0,
            page_count: 0,
            line_count: 0,
            object_count: 0,
            max_pages,
            max_lines: output_cap / PDF_LINE_RESERVE,
            max_objects: output_cap / PDF_OBJECT_RESERVE,
        })
    }

    fn reserve(&mut self, bytes: usize) -> Result<(), String> {
        let next = self
            .estimated_bytes
            .checked_add(bytes)
            .ok_or_else(|| "conversion produced oversized output".to_owned())?;
        if next > self.output_cap {
            return Err("conversion produced oversized output".into());
        }
        self.estimated_bytes = next;
        Ok(())
    }

    fn reserve_objects(&mut self, count: usize) -> Result<(), String> {
        let next = self
            .object_count
            .checked_add(count)
            .ok_or_else(|| "conversion produced oversized output".to_owned())?;
        if next > self.max_objects {
            return Err("conversion produced oversized output".into());
        }
        self.object_count = next;
        Ok(())
    }

    fn reserve_line(&mut self, bytes: usize) -> Result<(), String> {
        let next = self
            .line_count
            .checked_add(1)
            .ok_or_else(|| "conversion produced oversized output".to_owned())?;
        if next > self.max_lines {
            return Err("conversion produced oversized output".into());
        }
        let content = self
            .content_bytes
            .checked_add(bytes)
            .ok_or_else(|| "conversion produced oversized output".to_owned())?;
        if content > self.output_cap.saturating_sub(PDF_FIXED_RESERVE) {
            return Err("conversion produced oversized output".into());
        }
        self.reserve(bytes)?;
        self.content_bytes = content;
        self.line_count = next;
        Ok(())
    }

    fn reserve_page(&mut self) -> Result<(), String> {
        if self.page_count >= self.max_pages {
            return Err("conversion produced oversized output".into());
        }
        self.reserve(PDF_PAGE_RESERVE)?;
        self.reserve_objects(2)?;
        self.page_count += 1;
        Ok(())
    }
}

fn markdown_to_pdf(markdown: &[u8], output_cap: usize) -> Result<Vec<u8>, String> {
    let markdown = std::str::from_utf8(markdown).map_err(|_| "invalid text output")?;
    let mut budget = PdfBudget::new(output_cap)?;
    let mut document = Document::with_version("1.4");
    let pages = document.new_object_id();
    budget.reserve_objects(1)?;
    let font = document.add_object(dictionary! {
        "Type" => "Font",
        "Subtype" => "Type1",
        "BaseFont" => "Helvetica",
    });
    budget.reserve_objects(1)?;
    let mut kids = Vec::new();
    let mut content = Vec::with_capacity(PDF_MAX_CONTENT_STREAM_BYTES.min(output_cap));
    let mut lines_on_page = 0;
    let mut wrote_line = false;
    for raw in markdown.lines() {
        let mut line = Vec::with_capacity(PDF_MAX_LINE_BYTES);
        let mut wrote_for_raw = false;
        for character in raw.chars() {
            if line.len() == PDF_MAX_LINE_BYTES {
                append_pdf_line(&mut content, &line, lines_on_page, &mut budget)?;
                lines_on_page += 1;
                wrote_line = true;
                wrote_for_raw = true;
                line.clear();
                if lines_on_page == PDF_LINES_PER_PAGE {
                    add_pdf_page(
                        &mut document,
                        pages,
                        font,
                        &mut kids,
                        &mut content,
                        &mut budget,
                    )?;
                    content = Vec::with_capacity(PDF_MAX_CONTENT_STREAM_BYTES.min(output_cap));
                    lines_on_page = 0;
                }
            }
            line.push(safe_pdf_byte(character));
        }
        if !line.is_empty() || !wrote_for_raw {
            append_pdf_line(&mut content, &line, lines_on_page, &mut budget)?;
            lines_on_page += 1;
            wrote_line = true;
        }
        if lines_on_page == PDF_LINES_PER_PAGE {
            add_pdf_page(
                &mut document,
                pages,
                font,
                &mut kids,
                &mut content,
                &mut budget,
            )?;
            content = Vec::with_capacity(PDF_MAX_CONTENT_STREAM_BYTES.min(output_cap));
            lines_on_page = 0;
        }
    }
    if lines_on_page > 0 {
        add_pdf_page(
            &mut document,
            pages,
            font,
            &mut kids,
            &mut content,
            &mut budget,
        )?;
    }
    if !wrote_line {
        return Err("conversion produced empty output".into());
    }
    let page_count = budget.page_count;
    document.objects.insert(
        pages,
        Object::Dictionary(dictionary! {
            "Type" => "Pages",
            "Kids" => kids,
            "Count" => page_count as i64,
        }),
    );
    budget.reserve_objects(1)?;
    let catalog = document.add_object(dictionary! { "Type" => "Catalog", "Pages" => pages });
    document.trailer.set("Root", catalog);
    let mut output = Vec::new();
    document
        .save_to(&mut output)
        .map_err(|_| "PDF construction failed")?;
    if output.len() > output_cap {
        return Err("conversion produced oversized output".into());
    }
    let parsed = Document::load_mem(&output).map_err(|_| "invalid generated PDF")?;
    if parsed.is_encrypted() || parsed.get_pages().is_empty() {
        return Err("invalid generated PDF".into());
    }
    Ok(output)
}

fn safe_pdf_byte(character: char) -> u8 {
    match character {
        '\t' => b' ',
        character if character.is_ascii() && !character.is_ascii_control() => character as u8,
        _ => b'?',
    }
}

fn append_pdf_line(
    content: &mut Vec<u8>,
    line: &[u8],
    line_index: usize,
    budget: &mut PdfBudget,
) -> Result<(), String> {
    let escaped = escape_pdf_text(line);
    let escaped = String::from_utf8(escaped).map_err(|_| "PDF construction failed")?;
    let y = 756 - line_index as i64 * 14;
    let rendered = format!("BT /F1 10 Tf 36 {y} Td ({escaped}) Tj ET\n");
    if content.len().saturating_add(rendered.len()) > PDF_MAX_CONTENT_STREAM_BYTES {
        return Err("conversion produced oversized output".into());
    }
    budget.reserve_line(rendered.len())?;
    content.extend_from_slice(rendered.as_bytes());
    Ok(())
}

fn add_pdf_page(
    document: &mut Document,
    pages: lopdf::ObjectId,
    font: lopdf::ObjectId,
    kids: &mut Vec<Object>,
    content: &mut Vec<u8>,
    budget: &mut PdfBudget,
) -> Result<(), String> {
    budget.reserve_page()?;
    let contents = document.add_object(Stream::new(dictionary! {}, std::mem::take(content)));
    let page = document.add_object(dictionary! {
        "Type" => "Page",
        "Parent" => pages,
        "MediaBox" => vec![0.into(), 0.into(), 612.into(), 792.into()],
        "Resources" => dictionary! { "Font" => dictionary! { "F1" => font } },
        "Contents" => contents,
    });
    kids.push(page.into());
    Ok(())
}

fn escape_pdf_text(text: &[u8]) -> Vec<u8> {
    let mut escaped = Vec::with_capacity(text.len());
    for &byte in text {
        match byte {
            b'(' | b')' | b'\\' => {
                escaped.push(b'\\');
                escaped.push(byte);
            }
            _ => escaped.push(byte),
        }
    }
    escaped
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mapping_and_signature_validation_are_closed() {
        assert_eq!(kind_from_name("doc"), Some(DocumentKind::Doc));
        assert_eq!(kind_from_name("DOC"), None);
        assert_eq!(
            format_for_kind(DocumentKind::Xls),
            Some(anydoc::Format::Excel)
        );
        assert!(format_matches(DocumentKind::Rtf, b"{\\rtf1\\ansi text}"));
        assert!(format_matches(
            DocumentKind::Rtf,
            b"\xef\xbb\xbf{\\rtf1\\ansi text}"
        ));
        assert!(format_matches(DocumentKind::Csv, b"not signature based"));
        assert!(!format_matches(DocumentKind::Doc, b"{\\rtf1\\ansi text}"));
        assert!(format_for_kind(DocumentKind::Docx).is_none());
    }

    #[test]
    fn legacy_pdf_fallback_is_valid_and_bounded() {
        let pdf = to_pdf_bytes(
            DocumentKind::Rtf,
            b"{\\rtf1\\ansi Legacy PDF fallback \xe4\xb8\xad\xe6\x96\x87}",
            OfficeLimits::default(),
        )
        .unwrap();
        assert!(pdf.starts_with(b"%PDF-"));
        let document = Document::load_mem(&pdf).unwrap();
        assert_eq!(document.get_pages().len(), 1);
        let text = document.extract_text(&[1]).unwrap();
        assert!(text.contains("Legacy PDF fallback"));
        assert!(
            text.contains('?'),
            "non-ASCII fallback marker missing: {text:?}"
        );
        assert!(pdf.len() <= OfficeLimits::default().output_bytes);
    }

    #[test]
    fn legacy_pdf_fallback_reports_output_cap() {
        let limits = OfficeLimits {
            output_bytes: 32,
            ..OfficeLimits::default()
        };
        let error = to_pdf_bytes(DocumentKind::Csv, b"text", limits).unwrap_err();
        assert_eq!(error, "conversion produced oversized output");
    }

    #[test]
    fn large_markdown_is_rejected_by_incremental_pdf_budget() {
        let markdown = "line\n".repeat(100_000);
        let error = markdown_to_pdf(markdown.as_bytes(), 16 * 1024).unwrap_err();
        assert_eq!(error, "conversion produced oversized output");
    }
}
