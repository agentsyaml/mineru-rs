use crate::vlm_types::OfficialPdfOptions;
use crate::{Error, Result};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Limits {
    pub max_pdf_bytes: usize,
    pub max_total_asset_bytes: usize,
    pub max_pages: usize,
    pub max_page_pixels: u64,
    pub max_response_bytes: usize,
    pub max_rendered_image_bytes: usize,
    pub max_in_flight_image_bytes: usize,
    pub max_blocks_per_page: usize,
    pub page_window_size: usize,
}
impl Default for Limits {
    fn default() -> Self {
        // Derive the fields that semantically overlap `OfficialPdfOptions` from its defaults so
        // the two parallel defaults cannot drift apart. `max_response_bytes` has no
        // `OfficialPdfOptions` counterpart (it is the HTTP client response cap).
        let defaults = OfficialPdfOptions::default();
        Self {
            max_pdf_bytes: defaults.max_pdf_bytes,
            max_total_asset_bytes: defaults.max_total_asset_bytes,
            max_pages: defaults.max_pages,
            max_page_pixels: defaults.max_page_pixels,
            max_response_bytes: 10 * 1024 * 1024,
            max_rendered_image_bytes: defaults.max_rendered_image_bytes,
            max_in_flight_image_bytes: defaults.max_in_flight_image_bytes,
            max_blocks_per_page: defaults.max_layout_blocks_per_page,
            page_window_size: defaults.processing_window_size,
        }
    }
}
impl Limits {
    pub fn validate(&self) -> Result<()> {
        if self.max_pdf_bytes == 0
            || self.max_total_asset_bytes == 0
            || self.max_pages == 0
            || self.max_page_pixels == 0
            || self.max_response_bytes == 0
            || self.max_rendered_image_bytes == 0
            || self.max_in_flight_image_bytes == 0
            || self.max_blocks_per_page == 0
            || self.page_window_size == 0
        {
            return Err(Error::InvalidConfig(
                "all limits must be greater than zero".into(),
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::Limits;

    #[test]
    fn rejects_zero_total_asset_bytes() {
        let limits = Limits {
            max_total_asset_bytes: 0,
            ..Limits::default()
        };
        assert!(limits.validate().is_err());
    }

    #[test]
    fn rejects_zero_blocks_per_page() {
        assert!(
            Limits {
                max_blocks_per_page: 0,
                ..Limits::default()
            }
            .validate()
            .is_err()
        );
    }
}
