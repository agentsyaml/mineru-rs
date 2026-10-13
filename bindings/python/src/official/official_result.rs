use crate::runtime_error;
use mineru::official::{OfficialRenderFormat, OfficialRendered, OfficialResult, OfficialRuntime};
use pyo3::{Bound, PyAny, PyResult, Python, prelude::*, types::PyDict};
use std::{collections::BTreeMap, path::PathBuf};

#[pyclass(name = "_OfficialResult")]
pub(super) struct NativeResult {
    pub(super) result: OfficialResult,
    pub(super) runtime: OfficialRuntime,
}

#[pymethods]
impl NativeResult {
    #[getter]
    fn middle_json(&self) -> String {
        self.result.middle_json().to_string()
    }
    #[getter]
    fn model_json(&self) -> Option<String> {
        self.result.model_json().map(ToString::to_string)
    }
    #[getter]
    fn markdown(&self) -> String {
        self.result.markdown().to_owned()
    }
    #[getter]
    fn structured_content(&self) -> String {
        self.result.structured_content().to_string()
    }
    #[getter]
    fn pages(&self) -> String {
        serde_value_array(self.result.pages())
    }
    #[getter]
    fn assets(&self) -> Vec<(String, Vec<u8>)> {
        self.result
            .assets()
            .iter()
            .map(|(name, data)| (name.clone(), data.to_vec()))
            .collect()
    }
    #[getter]
    fn warnings(&self) -> Vec<String> {
        self.result.warnings().to_vec()
    }
    #[getter]
    fn version_metadata(&self) -> (String, String, String) {
        let m = self.result.version_metadata();
        (
            m.mineru_package.clone(),
            m.schema_version.clone(),
            m.bundle_name.clone(),
        )
    }
    fn export_pages(&self) -> String {
        serde_value_array(&self.result.export_pages())
    }
    #[pyo3(signature=(skip_defaults=false))]
    fn to_dict<'py>(&self, py: Python<'py>, skip_defaults: bool) -> PyResult<Bound<'py, PyAny>> {
        let result = self.result.clone();
        let runtime = self.runtime.clone();
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            let value = result
                .to_dict(&runtime, skip_defaults)
                .await
                .map_err(runtime_error)?;
            Ok(value.to_string())
        })
    }
    #[pyo3(signature=(skip_defaults=false))]
    fn to_json<'py>(&self, py: Python<'py>, skip_defaults: bool) -> PyResult<Bound<'py, PyAny>> {
        let result = self.result.clone();
        let runtime = self.runtime.clone();
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            result
                .to_json(&runtime, skip_defaults)
                .await
                .map_err(runtime_error)
        })
    }
    fn save(&self, root: PathBuf, stem: &str) -> PyResult<()> {
        self.result.save(root, stem).map_err(runtime_error)
    }
    fn save_with_writer(&self, py: Python<'_>, writer: &Bound<'_, PyAny>) -> PyResult<()> {
        let mut buffer = BufferedWriter::default();
        self.result
            .save_with_writer(&mut buffer)
            .map_err(runtime_error)?;
        for (path, data) in buffer.0 {
            match data {
                BufferedData::Text(text) => {
                    writer.call_method1("write_string", (path, text))?;
                }
                BufferedData::Bytes(bytes) => {
                    writer.call_method1("write", (path, bytes))?;
                }
            }
        }
        let _ = py;
        Ok(())
    }
    #[pyo3(signature=(format, options=None))]
    fn render<'py>(
        &self,
        py: Python<'py>,
        format: &str,
        options: Option<&Bound<'py, PyDict>>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let format = render_format(format)?;
        validate_render_options(format, options)?;
        let options = crate::official::render_options(options)?;
        let result = self.result.clone();
        let runtime = self.runtime.clone();
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            let rendered = result
                .render(&runtime, format, options)
                .await
                .map_err(runtime_error)?;
            rendered_parts(rendered)
        })
    }
}

#[derive(Default)]
struct BufferedWriter(Vec<(String, BufferedData)>);
enum BufferedData {
    Text(String),
    Bytes(Vec<u8>),
}
impl mineru::official::OfficialDataWriter for BufferedWriter {
    fn write(&mut self, path: &str, bytes: &[u8]) -> Result<(), mineru::official::OfficialError> {
        self.0
            .push((path.into(), BufferedData::Bytes(bytes.to_vec())));
        Ok(())
    }
    fn write_string(
        &mut self,
        path: &str,
        text: &str,
    ) -> Result<(), mineru::official::OfficialError> {
        self.0.push((path.into(), BufferedData::Text(text.into())));
        Ok(())
    }
}

fn render_format(value: &str) -> PyResult<OfficialRenderFormat> {
    match value {
        "markdown" => Ok(OfficialRenderFormat::Markdown),
        "html" => Ok(OfficialRenderFormat::Html),
        "latex" => Ok(OfficialRenderFormat::Latex),
        "docx" => Ok(OfficialRenderFormat::Docx),
        "epub" => Ok(OfficialRenderFormat::Epub),
        "structured_content" => Ok(OfficialRenderFormat::StructuredContent),
        "content_list" => Ok(OfficialRenderFormat::ContentList),
        "content_list_v2" => Ok(OfficialRenderFormat::ContentListV2),
        "pdf" => Ok(OfficialRenderFormat::Pdf),
        _ => Err(crate::value_error("unsupported render format")),
    }
}

fn validate_render_options(
    format: OfficialRenderFormat,
    dict: Option<&Bound<'_, PyDict>>,
) -> PyResult<()> {
    let allowed: &[&str] = match format {
        OfficialRenderFormat::Markdown => &["mode", "asset_base_url", "latex_delimiters"],
        OfficialRenderFormat::Html => &["mode", "asset_base_url", "standalone", "document_title"],
        OfficialRenderFormat::Latex => &["asset_base_path", "document_title"],
        OfficialRenderFormat::Docx => &[],
        OfficialRenderFormat::Epub => {
            &["title", "authors", "language", "identifier", "modified_at"]
        }
        OfficialRenderFormat::StructuredContent
        | OfficialRenderFormat::ContentList
        | OfficialRenderFormat::ContentListV2 => &["asset_base_url", "latex_delimiters"],
        OfficialRenderFormat::Pdf => &["document_title", "layout"],
        _ => return Err(crate::value_error("unsupported render format")),
    };
    if let Some(dict) = dict {
        for (key, value) in dict.iter() {
            let key: String = key.extract()?;
            if !value.is_none() && !allowed.contains(&key.as_str()) {
                return Err(crate::value_error(
                    "render option is not valid for this format",
                ));
            }
        }
    }
    Ok(())
}

fn rendered_parts(
    value: OfficialRendered,
) -> PyResult<(
    String,
    Option<String>,
    Option<Vec<u8>>,
    Vec<(String, Vec<u8>)>,
)> {
    match value {
        OfficialRendered::Text { content, sidecars } => {
            Ok(("text".into(), Some(content), None, asset_pairs(sidecars)))
        }
        OfficialRendered::Json { value, sidecars } => Ok((
            "json".into(),
            Some(value.to_string()),
            None,
            asset_pairs(sidecars),
        )),
        OfficialRendered::Binary { bytes, sidecars } => Ok((
            "binary".into(),
            None,
            Some(bytes.to_vec()),
            asset_pairs(sidecars),
        )),
        _ => Err(runtime_error("unsupported rendered output")),
    }
}
fn asset_pairs<T: AsRef<[u8]>>(map: BTreeMap<String, T>) -> Vec<(String, Vec<u8>)> {
    map.into_iter()
        .map(|(name, bytes)| (name, bytes.as_ref().to_vec()))
        .collect()
}
fn serde_value_array<T: ToString>(values: &[T]) -> String {
    format!(
        "[{}]",
        values
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(",")
    )
}
