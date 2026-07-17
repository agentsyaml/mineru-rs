//! Native Python binding for the MinerU VLM Rust client.
//!
//! Exposes a minimal, deterministic core surface: `canonical_stem` sanitizes an
//! output stem through the same portable-name validation used by the canonical
//! route, and `validate_pdf_options` checks the official PDF option bounds. Both
//! run without network access, so they serve as load/call/error-mapping smokes.

use pyo3::Bound;
use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;

fn map_err(e: impl std::fmt::Display) -> PyErr {
    PyValueError::new_err(e.to_string())
}

/// Sanitize an output stem through the canonical portable-name validator.
#[pyfunction]
fn canonical_stem(s: &str) -> PyResult<String> {
    mineru::canonical_stem(s).map_err(map_err)
}

/// Validate official PDF option bounds (start/end page, workers, limits).
///
/// `start` is zero-based; `end` is inclusive or `None`. Returns `True` on valid
/// bounds and raises `ValueError` with the underlying diagnostic otherwise.
#[pyfunction]
fn validate_pdf_options(
    start_page: usize,
    end_page: Option<usize>,
    formula_enable: bool,
    table_enable: bool,
    image_analysis: bool,
) -> PyResult<bool> {
    let options = mineru::OfficialPdfOptions {
        start_page,
        end_page,
        formula_enable,
        table_enable,
        image_analysis,
        ..Default::default()
    };
    options.validate().map_err(map_err)?;
    Ok(true)
}

#[pymodule]
fn mineru_rs(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(canonical_stem, m)?)?;
    m.add_function(wrap_pyfunction!(validate_pdf_options, m)?)?;
    Ok(())
}
