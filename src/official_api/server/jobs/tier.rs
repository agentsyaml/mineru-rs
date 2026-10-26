use crate::official::OfficialTier;
use crate::official_api::wire::Tier;

pub(super) fn requested_tier(request: Option<Tier>, name: &str) -> OfficialTier {
    request.map(convert_tier).unwrap_or_else(|| {
        if is_native(name) {
            OfficialTier::Flash
        } else {
            OfficialTier::Standard
        }
    })
}

fn convert_tier(tier: Tier) -> OfficialTier {
    match tier {
        Tier::Flash => OfficialTier::Flash,
        Tier::Basic => OfficialTier::Basic,
        Tier::Standard => OfficialTier::Standard,
        Tier::Advanced => OfficialTier::Advanced,
    }
}

pub(super) fn is_native(name: &str) -> bool {
    !matches!(
        name.rsplit('.')
            .next()
            .unwrap_or("")
            .to_ascii_lowercase()
            .as_str(),
        "pdf" | "png" | "jpg" | "jpeg" | "webp" | "gif" | "bmp" | "tiff" | "jp2"
    )
}
