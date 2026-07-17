pub(crate) const LAYOUT_PROMPT: &str = "\nLayout Detection:";
pub(crate) const TEXT_PROMPT: &str = "\nText Recognition:";
pub(crate) const TABLE_PROMPT: &str = "\nTable Recognition:";
pub(crate) const EQUATION_PROMPT: &str = "\nFormula Recognition:";
pub(crate) const IMAGE_PROMPT: &str = "\nImage Analysis:";

#[derive(Clone, Copy)]
pub(crate) struct Sampling {
    pub temperature: f32,
    pub top_p: f32,
    pub top_k: u32,
    pub presence_penalty: f32,
    pub frequency_penalty: f32,
    pub repetition_penalty: f32,
    pub no_repeat_ngram_size: u32,
}

pub(crate) const COMMON: Sampling = Sampling {
    temperature: 0.0,
    top_p: 0.01,
    top_k: 1,
    presence_penalty: 1.0,
    frequency_penalty: 0.05,
    repetition_penalty: 1.0,
    no_repeat_ngram_size: 100,
};
pub(crate) const LAYOUT_SAMPLING: Sampling = Sampling {
    presence_penalty: 0.0,
    frequency_penalty: 0.0,
    ..COMMON
};
pub(crate) const TABLE_SAMPLING: Sampling = Sampling {
    presence_penalty: 1.0,
    frequency_penalty: 0.005,
    ..COMMON
};
pub(crate) const RECOGNITION_SAMPLING: Sampling = Sampling { ..COMMON };
