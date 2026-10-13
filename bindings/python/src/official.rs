use crate::{runtime_error, value_error};
use mineru::official::{
    HtmlSourceContext, OfficialBatchItem, OfficialInput, OfficialLatexDelimiter,
    OfficialLatexDelimiters, OfficialOcrMode, OfficialParseOptions, OfficialParser,
    OfficialPdfLayout, OfficialRenderOptions, OfficialResult, OfficialRuntime, OfficialTier,
    OfficialVlmConfig, OfficialWorkerMode,
};
use pyo3::{
    Bound, Py, PyAny, PyResult, Python,
    prelude::*,
    types::{PyAnyMethods, PyDict, PyDictMethods, PyModule},
};
use std::{collections::BTreeMap, path::PathBuf, sync::Arc, time::Duration};

mod official_result;
use official_result::NativeResult;

#[pyclass(name = "_OfficialRuntime", frozen)]
struct NativeRuntime {
    inner: OfficialRuntime,
}

#[pymethods]
impl NativeRuntime {
    #[new]
    #[pyo3(signature=(python=None, config=None, model_dir=None, mode="per-document", timeout=86400))]
    fn new(
        python: Option<PathBuf>,
        config: Option<PathBuf>,
        model_dir: Option<PathBuf>,
        mode: &str,
        timeout: u64,
    ) -> PyResult<Self> {
        let worker_mode = match mode {
            "per-document" => OfficialWorkerMode::PerDocument,
            "persistent" => OfficialWorkerMode::Persistent,
            _ => return Err(value_error("mode must be per-document or persistent")),
        };
        let mut runtime = OfficialRuntime::new()
            .with_worker_mode(worker_mode)
            .with_timeout(Duration::from_secs(timeout));
        if let Some(path) = python {
            runtime = runtime.with_python(path);
        }
        if let Some(path) = config {
            runtime = runtime.with_config(path);
        }
        if let Some(path) = model_dir {
            runtime = runtime.with_model_dir(path);
        }
        Ok(Self { inner: runtime })
    }
}

#[pyclass(name = "_OfficialParser")]
struct NativeParser {
    parser: Arc<OfficialParser>,
    runtime: OfficialRuntime,
}

fn parse_input(
    path: Option<PathBuf>,
    data: Option<Vec<u8>>,
    filename: Option<String>,
) -> PyResult<OfficialInput> {
    match (path, data, filename) {
        (Some(path), None, None) => Ok(OfficialInput::path(path)),
        (None, Some(data), Some(filename)) => Ok(OfficialInput::bytes(filename, data)),
        _ => Err(value_error("provide either path or data with filename")),
    }
}

fn opt_string(dict: Option<&Bound<'_, PyDict>>, name: &str) -> PyResult<Option<String>> {
    let Some(dict) = dict else { return Ok(None) };
    match dict.get_item(name)? {
        None => Ok(None),
        Some(value) if value.is_none() => Ok(None),
        Some(value) => value.extract().map(Some),
    }
}

fn parse_options(dict: Option<&Bound<'_, PyDict>>) -> PyResult<OfficialParseOptions> {
    let mut options = OfficialParseOptions::new();
    if let Some(value) = opt_string(dict, "tier")? {
        options = options.with_tier(match value.as_str() {
            "flash" => OfficialTier::Flash,
            "basic" => OfficialTier::Basic,
            "standard" => OfficialTier::Standard,
            "advanced" => OfficialTier::Advanced,
            _ => return Err(value_error("invalid tier")),
        });
    }
    if let Some(value) = opt_string(dict, "ocr_mode")? {
        options = options.with_ocr_mode(match value.as_str() {
            "auto" => OfficialOcrMode::Auto,
            "txt" => OfficialOcrMode::Txt,
            "ocr" => OfficialOcrMode::Ocr,
            _ => return Err(value_error("invalid OCR mode")),
        });
    }
    if let Some(dict) = dict {
        option_dict(
            Some(dict),
            &[
                "tier",
                "ocr_mode",
                "image_analysis",
                "page_range",
                "source_context",
                "vlm_config",
                "vlm_server_url",
                "vlm_api_key",
                "vlm_model",
            ],
        )?;
        if let Some(value) = dict.get_item("image_analysis")? {
            if !value.is_none() {
                options = options.with_image_analysis(value.extract()?);
            }
        }
    }
    if let Some(value) = opt_string(dict, "page_range")? {
        options = options.with_page_range(value);
    }
    if let Some(dict) = dict {
        if let Some(ctx) = dict.get_item("source_context")? {
            if !ctx.is_none() {
                options = options.with_source_context(source_context(&ctx)?);
            }
        }
        if let Some(config) = dict.get_item("vlm_config")? {
            if !config.is_none() {
                options = options.with_vlm_config(vlm_config(&config)?);
            }
        }
    }
    if let Some(value) = opt_string(dict, "vlm_server_url")? {
        options = options.with_vlm_server_url(value);
    }
    if let Some(value) = opt_string(dict, "vlm_api_key")? {
        options = options.with_vlm_api_key(value);
    }
    if let Some(value) = opt_string(dict, "vlm_model")? {
        options = options.with_vlm_model(value);
    }
    Ok(options)
}

fn source_context(value: &Bound<'_, PyAny>) -> PyResult<HtmlSourceContext> {
    let dict = value.cast::<PyDict>()?;
    option_dict(
        Some(dict),
        &["source_uri", "local_resource_root", "transport_encoding"],
    )?;
    let mut context = HtmlSourceContext::new();
    if let Some(s) = opt_string(Some(dict), "source_uri")? {
        context = context.with_source_uri(s);
    }
    if let Some(s) = opt_string(Some(dict), "local_resource_root")? {
        context = context.with_local_resource_root(PathBuf::from(s));
    }
    if let Some(s) = opt_string(Some(dict), "transport_encoding")? {
        context = context.with_transport_encoding(s);
    }
    Ok(context)
}
fn vlm_config(value: &Bound<'_, PyAny>) -> PyResult<OfficialVlmConfig> {
    let dict = value.cast::<PyDict>()?;
    option_dict(
        Some(dict),
        &[
            "engine",
            "server_url",
            "api_key",
            "model",
            "http_timeout",
            "max_concurrency",
        ],
    )?;
    let mut config = OfficialVlmConfig::new();
    if let Some(v) = opt_string(Some(dict), "engine")? {
        config = config.with_engine(v);
    }
    if let Some(v) = opt_string(Some(dict), "server_url")? {
        config = config.with_server_url(v);
    }
    if let Some(v) = opt_string(Some(dict), "api_key")? {
        config = config.with_api_key(v);
    }
    if let Some(v) = opt_string(Some(dict), "model")? {
        config = config.with_model(v);
    }
    if let Some(v) = dict.get_item("http_timeout")? {
        config = config.with_http_timeout(v.extract()?);
    }
    if let Some(v) = dict.get_item("max_concurrency")? {
        config = config.with_max_concurrency(v.extract()?);
    }
    Ok(config)
}

fn option_dict(value: Option<&Bound<'_, PyDict>>, allowed: &[&str]) -> PyResult<()> {
    if let Some(dict) = value {
        for (key, _) in dict.iter() {
            let key: String = key.extract()?;
            if !allowed.contains(&key.as_str()) {
                return Err(value_error(format!("unknown option: {key}")));
            }
        }
    }
    Ok(())
}

fn render_options(dict: Option<&Bound<'_, PyDict>>) -> PyResult<OfficialRenderOptions> {
    option_dict(
        dict,
        &[
            "mode",
            "asset_base_url",
            "standalone",
            "document_title",
            "asset_base_path",
            "title",
            "authors",
            "language",
            "identifier",
            "modified_at",
            "layout",
            "latex_delimiters",
        ],
    )?;
    let mut out = OfficialRenderOptions::new();
    if let Some(mode) = opt_string(dict, "mode")? {
        out = out.with_mode(match mode.as_str() {
            "default" => mineru::official::OfficialRenderMode::Default,
            "full" => mineru::official::OfficialRenderMode::Full,
            _ => return Err(value_error("invalid render mode")),
        });
    }
    if let Some(v) = opt_string(dict, "asset_base_url")? {
        out = out.with_asset_base_url(v);
    }
    if let Some(v) = opt_string(dict, "document_title")? {
        out = out.with_document_title(v);
    }
    if let Some(v) = opt_string(dict, "asset_base_path")? {
        out = out.with_asset_base_path(v);
    }
    if let Some(v) = opt_string(dict, "title")? {
        out = out.with_title(v);
    }
    if let Some(v) = opt_string(dict, "language")? {
        out = out.with_language(v);
    }
    if let Some(v) = opt_string(dict, "identifier")? {
        out = out.with_identifier(v);
    }
    if let Some(v) = opt_string(dict, "modified_at")? {
        out = out.with_modified_at(v);
    }
    if let Some(dict) = dict {
        if let Some(v) = dict.get_item("standalone")? {
            out = out.with_standalone(v.extract()?);
        }
        if let Some(v) = dict.get_item("authors")? {
            out = out.with_authors(v.extract()?);
        }
        if let Some(v) = dict.get_item("layout")? {
            let value: String = v.extract()?;
            out = out.with_pdf_layout(match value.as_str() {
                "auto" => OfficialPdfLayout::Auto,
                "original" => OfficialPdfLayout::Original,
                "reflow" => OfficialPdfLayout::Reflow,
                _ => return Err(value_error("invalid PDF layout")),
            });
        }
        if let Some(v) = dict.get_item("latex_delimiters")? {
            let delimiters = v.cast::<PyDict>()?;
            option_dict(Some(delimiters), &["display", "inline"])?;
            let defaults = OfficialLatexDelimiters::default();
            let display = delimiter(delimiters.get_item("display")?.as_ref(), defaults.display)?;
            let inline = delimiter(delimiters.get_item("inline")?.as_ref(), defaults.inline)?;
            out = out.with_latex_delimiters(OfficialLatexDelimiters::new(display, inline));
        }
    }
    Ok(out)
}

fn delimiter(
    value: Option<&Bound<'_, PyAny>>,
    default: OfficialLatexDelimiter,
) -> PyResult<OfficialLatexDelimiter> {
    let Some(value) = value else {
        return Ok(default);
    };
    let dict = value.cast::<PyDict>()?;
    option_dict(Some(dict), &["left", "right"])?;
    let left = opt_string(Some(dict), "left")?.unwrap_or(default.left);
    let right = opt_string(Some(dict), "right")?.unwrap_or(default.right);
    Ok(OfficialLatexDelimiter::new(left, right))
}

#[pymethods]
impl NativeParser {
    #[new]
    fn new(runtime: PyRef<'_, NativeRuntime>) -> PyResult<Self> {
        let core = OfficialParser::new(runtime.inner.clone()).map_err(value_error)?;
        Ok(Self {
            parser: Arc::new(core),
            runtime: runtime.inner.clone(),
        })
    }

    #[pyo3(signature=(path=None, data=None, filename=None, options=None))]
    fn parse_rich<'py>(
        &self,
        py: Python<'py>,
        path: Option<PathBuf>,
        data: Option<Vec<u8>>,
        filename: Option<String>,
        options: Option<&Bound<'py, PyDict>>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let input = parse_input(path, data, filename)?;
        let options = parse_options(options)?;
        let parser = Arc::clone(&self.parser);
        let runtime = self.runtime.clone();
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            let result = parser
                .parse_rich(input, options)
                .await
                .map_err(runtime_error)?;
            Python::attach(|py| Py::new(py, NativeResult { result, runtime }))
        })
    }

    #[pyo3(signature=(items))]
    fn parse_batch_rich<'py>(
        &self,
        py: Python<'py>,
        items: Vec<(Option<PathBuf>, Option<Vec<u8>>, Option<String>, Py<PyDict>)>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let batch = Python::attach(|py| {
            items
                .into_iter()
                .map(|(path, data, filename, opts)| {
                    let input = parse_input(path, data, filename)?;
                    let options = parse_options(Some(opts.bind(py)))?;
                    Ok(OfficialBatchItem::new(input).with_options(options))
                })
                .collect::<PyResult<Vec<_>>>()
        })?;
        let parser = Arc::clone(&self.parser);
        let runtime = self.runtime.clone();
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            let results = parser
                .parse_batch_rich(batch)
                .await
                .map_err(runtime_error)?;
            Python::attach(|py| {
                let rows = pyo3::types::PyList::empty(py);
                for item in results {
                    let (result, error) = match item.result {
                        Ok(result) => (
                            Some(Py::new(
                                py,
                                NativeResult {
                                    result,
                                    runtime: runtime.clone(),
                                },
                            )?),
                            None,
                        ),
                        Err(error) => (None, Some(error.to_string())),
                    };
                    rows.append((item.index, item.filename, result, error))?;
                }
                Ok(rows.into_any().unbind())
            })
        })
    }

    fn shutdown<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let parser = Arc::clone(&self.parser);
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            parser.shutdown().await.map_err(runtime_error)?;
            Ok(())
        })
    }
}

#[pyfunction]
#[pyo3(signature=(runtime, middle_json, assets=None))]
fn _restore<'py>(
    py: Python<'py>,
    runtime: PyRef<'_, NativeRuntime>,
    middle_json: String,
    assets: Option<Vec<(String, Vec<u8>)>>,
) -> PyResult<Bound<'py, PyAny>> {
    let runtime = runtime.inner.clone();
    let assets: BTreeMap<String, _> = assets
        .unwrap_or_default()
        .into_iter()
        .map(|(path, data)| (path, data.into()))
        .collect();
    pyo3_async_runtimes::tokio::future_into_py(py, async move {
        let result = OfficialResult::from_json(&runtime, &middle_json, assets)
            .await
            .map_err(runtime_error)?;
        Python::attach(|py| Py::new(py, NativeResult { result, runtime }))
    })
}

pub(super) fn register(module: &Bound<'_, PyModule>) -> PyResult<()> {
    module.add_class::<NativeRuntime>()?;
    module.add_class::<NativeParser>()?;
    module.add_class::<NativeResult>()?;
    module.add_function(wrap_pyfunction!(_restore, module)?)?;
    Ok(())
}
