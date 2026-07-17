use std::path::Path;

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
pub(super) struct Classifier(magika::Session);

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
impl Classifier {
    pub(super) fn new() -> Result<Self, String> {
        magika::Session::new()
            .map(Self)
            .map_err(|error| error.to_string())
    }

    pub(super) fn identify_path(&mut self, path: &Path) -> Result<String, String> {
        self.0
            .identify_file_sync(path)
            .map(|file_type| file_type.info().label.to_owned())
            .map_err(|error| error.to_string())
    }

    pub(super) fn model_name(&self) -> &'static str {
        magika::MODEL_NAME
    }

    pub(super) fn model_major(&self) -> u32 {
        magika::MODEL_MAJOR_VERSION
    }
}

#[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
pub(super) struct Classifier;

#[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
impl Classifier {
    pub(super) fn new() -> Result<Self, String> {
        Err("Magika classification is unsupported on this target".into())
    }

    pub(super) fn identify_path(&mut self, _: &Path) -> Result<String, String> {
        Err("Magika classification is unsupported on this target".into())
    }

    pub(super) fn model_name(&self) -> &'static str {
        "standard_v3_3"
    }

    pub(super) fn model_major(&self) -> u32 {
        3
    }
}

#[cfg(all(test, target_os = "macos", target_arch = "aarch64"))]
mod tests {
    use super::*;

    #[test]
    fn embedded_standard_v3_3_classifies_pdf_and_text() {
        let mut classifier = Classifier::new().unwrap();
        assert_eq!(classifier.model_name(), "standard_v3_3");
        assert_eq!(classifier.model_major(), 3);
        assert_eq!(
            classifier
                .identify_path(Path::new(concat!(
                    env!("CARGO_MANIFEST_DIR"),
                    "/tests/fixtures/pdf/minimal.pdf"
                )))
                .unwrap(),
            "pdf"
        );
        let text = tempfile::Builder::new().suffix(".txt").tempfile().unwrap();
        std::fs::write(
            text.path(),
            "This is ordinary plain text for classification.\n".repeat(64),
        )
        .unwrap();
        assert_eq!(classifier.identify_path(text.path()).unwrap(), "txt");
    }
}
