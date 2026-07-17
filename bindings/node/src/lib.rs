//! Native Node.js binding for the MinerU VLM Rust client.
//!
//! Exposes a minimal, deterministic core surface: `canonicalStem` sanitizes an
//! output stem through the same portable-name validation used by the canonical
//! route, and `validatePdfOptions` checks the official PDF option bounds. Both
//! run without network access, so they serve as load/call/error-mapping smokes.

use napi_derive::napi;

/// Sanitize an output stem through the canonical portable-name validator.
#[napi]
pub fn canonical_stem(s: String) -> napi::Result<String> {
    mineru::canonical_stem(&s).map_err(|e| napi::Error::from_reason(e.to_string()))
}

/// Validate official PDF option bounds (start/end page, workers, limits).
///
/// `start` is zero-based; `end` is inclusive or `null`. Returns `true` on valid
/// bounds and throws with the underlying diagnostic otherwise.
#[napi]
pub fn validate_pdf_options(
    start_page: u32,
    end_page: Option<u32>,
    formula_enable: bool,
    table_enable: bool,
    image_analysis: bool,
) -> napi::Result<bool> {
    let options = mineru::OfficialPdfOptions {
        start_page: start_page as usize,
        end_page: end_page.map(|e| e as usize),
        formula_enable,
        table_enable,
        image_analysis,
        ..Default::default()
    };
    options
        .validate()
        .map_err(|e| napi::Error::from_reason(e.to_string()))?;
    Ok(true)
}
