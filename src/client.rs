use crate::{
    ClientConfig, Document, ModelInfo, ParseOptions, PdfInput, Result, extractor::PageExtractor,
    openai::OpenAi, pipeline,
};

pub struct MinerUClient {
    config: ClientConfig,
    extractor: PageExtractor,
}

impl MinerUClient {
    pub fn new(config: ClientConfig) -> Result<Self> {
        config.validate()?;
        let extractor = PageExtractor::new(&config)?;
        Ok(Self { config, extractor })
    }
    pub async fn check_model(&self) -> Result<ModelInfo> {
        OpenAi::new(&self.config)?.models().await
    }
    pub async fn parse_pdf(&self, input: PdfInput, options: ParseOptions) -> Result<Document> {
        tokio::time::timeout(
            self.config.timeouts.total,
            pipeline::parse(&self.config, &self.extractor, input, options),
        )
        .await
        .map_err(|_| crate::Error::Timeout {
            operation: "parse_pdf",
        })?
    }
}
