//! Native implementation behind the generated Node.js loader.

use napi::bindgen_prelude::Utf16String;
use napi_derive::napi;
use std::{ffi::OsString, path::PathBuf};

fn napi_error(error: impl std::fmt::Display) -> napi::Error {
    napi::Error::from_reason(error.to_string())
}

#[napi]
pub fn canonical_stem(value: String) -> napi::Result<String> {
    mineru::canonical_stem(&value).map_err(napi_error)
}

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
        end_page: end_page.map(|end| end as usize),
        formula_enable,
        table_enable,
        image_analysis,
        ..Default::default()
    };
    options.validate().map_err(napi_error)?;
    Ok(true)
}

#[napi(object)]
pub struct NativeRunOptions {
    pub path: String,
    pub output: String,
    pub api_url: Option<String>,
    pub method: Option<String>,
    pub backend: Option<String>,
    pub effort: Option<String>,
    pub lang: Option<String>,
    pub url: Option<String>,
    pub start: Option<u32>,
    pub end: Option<u32>,
    pub formula: Option<bool>,
    pub table: Option<bool>,
    pub image_analysis: Option<bool>,
    pub client_side_output_generation: Option<bool>,
}

#[napi(object)]
pub struct NativeRunReport {
    pub warnings: Vec<String>,
}

#[napi(js_name = "_run")]
pub async fn run_native(input: NativeRunOptions, helper: String) -> napi::Result<NativeRunReport> {
    let mut options = mineru::RunOptions::new(input.path, input.output);
    if let Some(value) = input.api_url {
        options.api_url = Some(value);
    }
    if let Some(value) = input.method {
        options.method = value;
    }
    if let Some(value) = input.backend {
        options.backend = value;
    }
    if let Some(value) = input.effort {
        options.effort = value;
    }
    if let Some(value) = input.lang {
        options.lang = value;
    }
    if let Some(value) = input.url {
        options.url = Some(value);
    }
    if let Some(value) = input.start {
        options.start = value as usize;
    }
    if let Some(value) = input.end {
        options.end = Some(value as usize);
    }
    if let Some(value) = input.formula {
        options.formula = value;
    }
    if let Some(value) = input.table {
        options.table = value;
    }
    if let Some(value) = input.image_analysis {
        options.image_analysis = value;
    }
    if let Some(value) = input.client_side_output_generation {
        options.client_side_output_generation = value;
    }
    let context = mineru::command::RunContext::with_office_executable(PathBuf::from(helper))
        .map_err(napi_error)?;
    let task = napi::tokio::spawn(mineru::command::run_with_context(options, context));
    let report = task.await.map_err(napi_error)?.map_err(napi_error)?;
    Ok(NativeRunReport {
        warnings: report.warnings,
    })
}

fn validated_utf16(argv: Vec<Utf16String>) -> Result<Vec<Vec<u16>>, ()> {
    argv.into_iter()
        .map(|value| {
            let mut index = 0;
            while index < value.len() {
                let unit = value[index];
                if unit == 0 || unit == 0xfffd || (0xdc00..=0xdfff).contains(&unit) {
                    return Err(());
                }
                if (0xd800..=0xdbff).contains(&unit) {
                    if value
                        .get(index + 1)
                        .is_none_or(|next| !(0xdc00..=0xdfff).contains(next))
                    {
                        return Err(());
                    }
                    index += 2;
                } else {
                    index += 1;
                }
            }
            Ok(value.to_vec())
        })
        .collect()
}

#[cfg(windows)]
fn os_argv(argv: Vec<Vec<u16>>) -> Vec<OsString> {
    use std::os::windows::ffi::OsStringExt;
    argv.into_iter()
        .map(|value| OsString::from_wide(&value))
        .collect()
}

#[cfg(not(windows))]
fn os_argv(argv: Vec<Vec<u16>>) -> Vec<OsString> {
    argv.into_iter()
        .map(|value| OsString::from(String::from_utf16(&value).expect("validated UTF-16")))
        .collect()
}

#[napi(js_name = "_runCli")]
pub async fn run_cli_native(argv: Vec<Utf16String>, helper: String) -> napi::Result<i32> {
    let argv = match validated_utf16(argv) {
        Ok(argv) => os_argv(argv),
        Err(()) => {
            eprintln!("error: invalid Node CLI argument encoding");
            return Ok(2);
        }
    };
    let context = match mineru::command::RunContext::with_office_executable(PathBuf::from(helper)) {
        Ok(context) => context,
        Err(error) => {
            eprintln!("{error}");
            return Ok(1);
        }
    };
    Ok(mineru::command::run_cli(argv, context).await)
}

#[napi(js_name = "_compileTargetSuffix")]
pub fn compile_target_suffix() -> napi::Result<String> {
    #[cfg(all(target_os = "macos", target_arch = "x86_64"))]
    let suffix = "darwin-x64";
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    let suffix = "darwin-arm64";
    #[cfg(all(target_os = "linux", target_arch = "x86_64", target_env = "gnu"))]
    let suffix = "linux-x64-gnu";
    #[cfg(all(target_os = "linux", target_arch = "aarch64", target_env = "gnu"))]
    let suffix = "linux-arm64-gnu";
    #[cfg(all(target_os = "windows", target_arch = "x86_64", target_env = "msvc"))]
    let suffix = "win32-x64-msvc";
    #[cfg(all(target_os = "windows", target_arch = "aarch64", target_env = "msvc"))]
    let suffix = "win32-arm64-msvc";
    #[cfg(not(any(
        all(target_os = "macos", target_arch = "x86_64"),
        all(target_os = "macos", target_arch = "aarch64"),
        all(target_os = "linux", target_arch = "x86_64", target_env = "gnu"),
        all(target_os = "linux", target_arch = "aarch64", target_env = "gnu"),
        all(target_os = "windows", target_arch = "x86_64", target_env = "msvc"),
        all(target_os = "windows", target_arch = "aarch64", target_env = "msvc")
    )))]
    return Err(napi::Error::from_reason("unsupported MinerU Node target"));
    Ok(suffix.into())
}
