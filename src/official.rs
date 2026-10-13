//! Additive SDK for the pinned official Python MinerU parser.

mod batch;
mod error;
mod input;
mod options;
mod parser;
mod render;
mod result;
mod result_assets;
mod runtime;
#[cfg(test)]
mod tests;
mod writer;

pub use crate::command::OfficialWorkerMode;
pub use batch::{OfficialBatchItem, OfficialBatchResult};
pub use error::{OfficialError, OfficialErrorKind};
pub use input::OfficialInput;
pub use options::{
    HtmlSourceContext, OfficialOcrMode, OfficialParseOptions, OfficialTier, OfficialVlmConfig,
};
pub use parser::OfficialParser;
pub use render::{
    OfficialLatexDelimiter, OfficialLatexDelimiters, OfficialPdfLayout, OfficialRenderFormat,
    OfficialRenderMode, OfficialRenderOptions, OfficialRendered,
};
pub use result::{OfficialResult, OfficialVersionMetadata};
pub use runtime::OfficialRuntime;
pub use writer::OfficialDataWriter;
