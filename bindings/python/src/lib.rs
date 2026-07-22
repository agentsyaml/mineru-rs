//! Private native implementation for the mixed-layout Python package.

use pyo3::{
    Bound,
    exceptions::{PyRuntimeError, PyValueError},
    prelude::*,
};
use std::{ffi::OsString, path::PathBuf};

fn value_error(error: impl std::fmt::Display) -> PyErr {
    PyValueError::new_err(error.to_string())
}

fn runtime_error(error: impl std::fmt::Display) -> PyErr {
    PyRuntimeError::new_err(error.to_string())
}

#[pyfunction]
fn canonical_stem(value: &str) -> PyResult<String> {
    mineru::canonical_stem(value).map_err(value_error)
}

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
    options.validate().map_err(value_error)?;
    Ok(true)
}

#[allow(clippy::too_many_arguments)]
#[pyfunction(name = "_run")]
fn run_native<'py>(
    py: Python<'py>,
    path: PathBuf,
    output: PathBuf,
    api_url: Option<String>,
    method: String,
    backend: String,
    effort: String,
    lang: String,
    url: Option<String>,
    start: usize,
    end: Option<usize>,
    formula: bool,
    table: bool,
    image_analysis: bool,
    client_side_output_generation: bool,
    helper: PathBuf,
) -> PyResult<Bound<'py, PyAny>> {
    let mut options = mineru::RunOptions::new(path, output);
    options.api_url = api_url;
    options.method = method;
    options.backend = backend;
    options.effort = effort;
    options.lang = lang;
    options.url = url;
    options.start = start;
    options.end = end;
    options.formula = formula;
    options.table = table;
    options.image_analysis = image_analysis;
    options.client_side_output_generation = client_side_output_generation;
    let context =
        mineru::command::RunContext::with_office_executable(helper).map_err(runtime_error)?;
    let task = pyo3_async_runtimes::tokio::get_runtime()
        .spawn(mineru::command::run_with_context(options, context));

    pyo3_async_runtimes::tokio::future_into_py(py, async move {
        let report = task.await.map_err(runtime_error)?.map_err(runtime_error)?;
        Ok(report.warnings)
    })
}

#[cfg(unix)]
fn decode_argv(argv: Vec<Vec<u8>>) -> Result<Vec<OsString>, ()> {
    use std::os::unix::ffi::OsStringExt;
    argv.into_iter()
        .map(|bytes| {
            if bytes.contains(&0) {
                Err(())
            } else {
                Ok(OsString::from_vec(bytes))
            }
        })
        .collect()
}

#[cfg(windows)]
fn decode_argv(argv: Vec<Vec<u8>>) -> Result<Vec<OsString>, ()> {
    use std::os::windows::ffi::OsStringExt;
    argv.into_iter()
        .map(|bytes| {
            if bytes.len() % 2 != 0 {
                return Err(());
            }
            let wide = bytes
                .chunks_exact(2)
                .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
                .collect::<Vec<_>>();
            if wide.contains(&0) {
                Err(())
            } else {
                Ok(OsString::from_wide(&wide))
            }
        })
        .collect()
}

#[pyfunction(name = "_run_cli")]
fn run_cli_native<'py>(
    py: Python<'py>,
    argv: Vec<Vec<u8>>,
    helper: PathBuf,
) -> PyResult<Bound<'py, PyAny>> {
    let argv = match decode_argv(argv) {
        Ok(argv) => argv,
        Err(()) => {
            eprintln!("error: invalid Python CLI argument encoding");
            return pyo3_async_runtimes::tokio::future_into_py(py, async { Ok(2) });
        }
    };
    let context = match mineru::command::RunContext::with_office_executable(helper) {
        Ok(context) => context,
        Err(error) => {
            eprintln!("{error}");
            return pyo3_async_runtimes::tokio::future_into_py(py, async { Ok(1) });
        }
    };
    pyo3_async_runtimes::tokio::future_into_py(py, async move {
        Ok(mineru::command::run_cli(argv, context).await)
    })
}

#[pymodule]
fn _native(module: &Bound<'_, PyModule>) -> PyResult<()> {
    module.add_function(wrap_pyfunction!(canonical_stem, module)?)?;
    module.add_function(wrap_pyfunction!(validate_pdf_options, module)?)?;
    module.add_function(wrap_pyfunction!(run_native, module)?)?;
    module.add_function(wrap_pyfunction!(run_cli_native, module)?)?;
    Ok(())
}
