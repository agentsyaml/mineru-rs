use crate::official::{OfficialError, OfficialInput, OfficialParseOptions, OfficialResult};

#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct OfficialBatchItem {
    pub input: OfficialInput,
    pub options: OfficialParseOptions,
}

impl OfficialBatchItem {
    pub fn new(input: OfficialInput) -> Self {
        Self {
            input,
            options: OfficialParseOptions::default(),
        }
    }
    pub fn with_options(mut self, options: OfficialParseOptions) -> Self {
        self.options = options;
        self
    }
}

#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct OfficialBatchResult {
    pub index: usize,
    pub filename: String,
    pub result: Result<OfficialResult, OfficialError>,
}
