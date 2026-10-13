mod converters;
use converters::{
    asset_files, assets, error_kind, input, official_error, parse_options, render_format,
    render_options, runtime, safe_error_message,
};
use mineru::official::{
    OfficialBatchItem, OfficialDataWriter, OfficialError, OfficialParser, OfficialRendered,
    OfficialResult, OfficialRuntime,
};
use napi::{Error, Result, bindgen_prelude::Buffer};
use napi_derive::napi;
use serde_json::Value;

#[napi(object)]
pub struct NativeRuntimeOptions {
    pub python: Option<String>,
    pub config: Option<String>,
    pub model_dir: Option<String>,
    pub worker_mode: Option<String>,
    pub timeout_ms: Option<u32>,
}

#[napi(object)]
pub struct NativeInput {
    pub path: Option<String>,
    pub filename: Option<String>,
    pub data: Option<Buffer>,
}

#[napi(object)]
pub struct NativeBatchInput {
    pub input: NativeInput,
    pub options: Option<String>,
}

#[napi(object)]
pub struct NativeAsset {
    pub path: String,
    pub data: Buffer,
}

#[napi(object)]
pub struct NativeWriterFile {
    pub path: String,
    pub text: Option<String>,
    pub data: Option<Buffer>,
}

#[napi(object)]
pub struct NativeVersionMetadata {
    pub mineru_package: String,
    pub schema_version: String,
    pub bundle_name: String,
}

#[napi(object)]
pub struct NativeRendered {
    pub kind: String,
    pub text: Option<String>,
    pub json: Option<String>,
    pub data: Option<Buffer>,
    pub sidecars: Vec<NativeAsset>,
}

#[napi]
pub struct NativeOfficialResult {
    result: OfficialResult,
    runtime: OfficialRuntime,
}

#[napi]
impl NativeOfficialResult {
    #[napi(getter)]
    pub fn middle_json(&self) -> Result<String> {
        serde_json::to_string(self.result.middle_json())
            .map_err(|_| Error::from_reason("upstream: could not serialize middle JSON"))
    }

    #[napi(getter)]
    pub fn model_json(&self) -> Result<Option<String>> {
        self.result
            .model_json()
            .map(serde_json::to_string)
            .transpose()
            .map_err(|_| Error::from_reason("upstream: could not serialize model JSON"))
    }

    #[napi(getter)]
    pub fn markdown(&self) -> String {
        self.result.markdown().to_owned()
    }

    #[napi(getter)]
    pub fn structured_content(&self) -> Result<String> {
        serde_json::to_string(self.result.structured_content())
            .map_err(|_| Error::from_reason("upstream: could not serialize structured content"))
    }

    #[napi(getter)]
    pub fn pages(&self) -> Result<String> {
        serde_json::to_string(self.result.pages())
            .map_err(|_| Error::from_reason("upstream: could not serialize pages"))
    }

    #[napi]
    pub fn export_pages(&self) -> Result<String> {
        serde_json::to_string(&self.result.export_pages())
            .map_err(|_| Error::from_reason("upstream: could not serialize exported pages"))
    }

    #[napi(getter)]
    pub fn assets(&self) -> Vec<NativeAsset> {
        asset_files(self.result.assets().clone())
    }

    #[napi(getter)]
    pub fn warnings(&self) -> Vec<String> {
        self.result.warnings().to_vec()
    }

    #[napi(getter)]
    pub fn version_metadata(&self) -> NativeVersionMetadata {
        let metadata = self.result.version_metadata();
        NativeVersionMetadata {
            mineru_package: metadata.mineru_package.clone(),
            schema_version: metadata.schema_version.clone(),
            bundle_name: metadata.bundle_name.clone(),
        }
    }

    #[napi(js_name = "toDict")]
    pub async fn to_dict(&self, skip_defaults: bool) -> Result<String> {
        let value = self
            .result
            .to_dict(&self.runtime, skip_defaults)
            .await
            .map_err(official_error)?;
        serde_json::to_string(&value)
            .map_err(|_| Error::from_reason("upstream: could not serialize dictionary"))
    }

    #[napi(js_name = "toJson")]
    pub async fn to_json(&self, skip_defaults: bool) -> Result<String> {
        self.result
            .to_json(&self.runtime, skip_defaults)
            .await
            .map_err(official_error)
    }

    #[napi(js_name = "render")]
    pub async fn render_native(
        &self,
        format: String,
        options: Option<String>,
    ) -> Result<NativeRendered> {
        let format = render_format(&format)?;
        let options = render_options(options)?;
        let rendered = self
            .result
            .render(&self.runtime, format, options)
            .await
            .map_err(official_error)?;
        Ok(match rendered {
            OfficialRendered::Text { content, sidecars } => NativeRendered {
                kind: "text".into(),
                text: Some(content),
                json: None,
                data: None,
                sidecars: asset_files(sidecars),
            },
            OfficialRendered::Json { value, sidecars } => NativeRendered {
                kind: "json".into(),
                text: None,
                json: Some(serde_json::to_string(&value).map_err(|_| {
                    Error::from_reason("upstream: could not serialize rendered JSON")
                })?),
                data: None,
                sidecars: asset_files(sidecars),
            },
            OfficialRendered::Binary { bytes, sidecars } => NativeRendered {
                kind: "binary".into(),
                text: None,
                json: None,
                data: Some(Buffer::from(bytes.to_vec())),
                sidecars: asset_files(sidecars),
            },
            _ => return Err(Error::from_reason("upstream: unsupported rendered result")),
        })
    }

    #[napi(js_name = "save")]
    pub fn save_native(&self, root: String, stem: String) -> Result<()> {
        self.result.save(root, &stem).map_err(official_error)
    }

    #[napi(js_name = "writerFiles")]
    pub fn writer_files(&self) -> Result<Vec<NativeWriterFile>> {
        let mut writer = NodeMemoryWriter(Vec::new());
        self.result
            .save_with_writer(&mut writer)
            .map_err(official_error)?;
        Ok(writer.0)
    }
}

struct NodeMemoryWriter(Vec<NativeWriterFile>);
impl OfficialDataWriter for NodeMemoryWriter {
    fn write(&mut self, path: &str, data: &[u8]) -> std::result::Result<(), OfficialError> {
        self.0.push(NativeWriterFile {
            path: path.into(),
            text: None,
            data: Some(Buffer::from(data.to_vec())),
        });
        Ok(())
    }
    fn write_string(&mut self, path: &str, text: &str) -> std::result::Result<(), OfficialError> {
        self.0.push(NativeWriterFile {
            path: path.into(),
            text: Some(text.into()),
            data: None,
        });
        Ok(())
    }
}

#[napi]
pub struct NativeBatchResult {
    index: u32,
    filename: String,
    result: Option<OfficialResult>,
    runtime: OfficialRuntime,
    error_kind: Option<String>,
    error_message: Option<String>,
}

#[napi]
impl NativeBatchResult {
    #[napi(getter)]
    pub fn index(&self) -> u32 {
        self.index
    }
    #[napi(getter)]
    pub fn filename(&self) -> String {
        self.filename.clone()
    }
    #[napi(js_name = "takeResult")]
    pub fn take_result(&mut self) -> Option<NativeOfficialResult> {
        self.result.take().map(|result| NativeOfficialResult {
            result,
            runtime: self.runtime.clone(),
        })
    }
    #[napi(getter)]
    pub fn error_kind(&self) -> Option<String> {
        self.error_kind.clone()
    }
    #[napi(getter)]
    pub fn error_message(&self) -> Option<String> {
        self.error_message.clone()
    }
}

#[napi]
pub struct NativeOfficialParser {
    parser: OfficialParser,
    runtime: OfficialRuntime,
}

#[napi]
impl NativeOfficialParser {
    #[napi(constructor)]
    pub fn new(options: Option<NativeRuntimeOptions>) -> Result<Self> {
        let runtime = runtime(options)?;
        let parser = OfficialParser::new(runtime.clone()).map_err(official_error)?;
        Ok(Self { parser, runtime })
    }

    #[napi(js_name = "parseRich")]
    pub async fn parse_rich(
        &self,
        source: NativeInput,
        options: Option<String>,
    ) -> Result<NativeOfficialResult> {
        let result = self
            .parser
            .parse_rich(input(source)?, parse_options(options)?)
            .await
            .map_err(official_error)?;
        Ok(NativeOfficialResult {
            result,
            runtime: self.runtime.clone(),
        })
    }

    #[napi(js_name = "parseBatchRich")]
    pub async fn parse_batch_rich(
        &self,
        items: Vec<NativeBatchInput>,
    ) -> Result<Vec<NativeBatchResult>> {
        let items = items
            .into_iter()
            .map(|item| {
                Ok(OfficialBatchItem::new(input(item.input)?)
                    .with_options(parse_options(item.options)?))
            })
            .collect::<Result<Vec<_>>>()?;
        let results = self
            .parser
            .parse_batch_rich(items)
            .await
            .map_err(official_error)?;
        results
            .into_iter()
            .map(|item| {
                let index = u32::try_from(item.index).map_err(|_| {
                    Error::from_reason("limit_exceeded: batch index exceeds Node limits")
                })?;
                let (result, error_kind, error_message) = match item.result {
                    Ok(result) => (Some(result), None, None),
                    Err(error) => (
                        None,
                        Some(error_kind(error.kind).into()),
                        Some(safe_error_message(error.kind).into()),
                    ),
                };
                Ok(NativeBatchResult {
                    index,
                    filename: item.filename,
                    result,
                    runtime: self.runtime.clone(),
                    error_kind,
                    error_message,
                })
            })
            .collect()
    }

    #[napi(js_name = "fromDict")]
    pub async fn from_dict(
        &self,
        middle_json: String,
        owned_assets: Vec<NativeAsset>,
    ) -> Result<NativeOfficialResult> {
        let middle: Value = serde_json::from_str(&middle_json)
            .map_err(|_| Error::from_reason("invalid_input: middle JSON must be valid JSON"))?;
        let result = OfficialResult::from_dict(&self.runtime, middle, assets(owned_assets)?)
            .await
            .map_err(official_error)?;
        Ok(NativeOfficialResult {
            result,
            runtime: self.runtime.clone(),
        })
    }

    #[napi(js_name = "fromJson")]
    pub async fn from_json(
        &self,
        middle_json: String,
        owned_assets: Vec<NativeAsset>,
    ) -> Result<NativeOfficialResult> {
        let result = OfficialResult::from_json(&self.runtime, &middle_json, assets(owned_assets)?)
            .await
            .map_err(official_error)?;
        Ok(NativeOfficialResult {
            result,
            runtime: self.runtime.clone(),
        })
    }

    #[napi]
    pub async fn shutdown(&self) -> Result<()> {
        self.parser.shutdown().await.map_err(official_error)
    }
}
