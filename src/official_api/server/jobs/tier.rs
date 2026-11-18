use crate::official::OfficialTier;
use crate::official_api::wire::{TaskFileInput, Tier};

/// `resolve_task_tier`: native-only batches always use flash; otherwise the
/// explicit tier wins and the default is standard.
pub(crate) fn resolve(request: Option<Tier>, files: &[TaskFileInput]) -> Tier {
    if files.iter().all(|file| is_native(&file.file_name)) {
        return Tier::Flash;
    }
    request.unwrap_or(Tier::Standard)
}

/// Per-file tier recorded in task detail: native documents are flash.
pub(crate) fn file_tier(task: Tier, name: &str) -> Tier {
    if is_native(name) { Tier::Flash } else { task }
}

/// Per-file tier: native documents are always parsed with the flash engine.
pub(crate) fn requested_tier(task: Tier, name: &str) -> OfficialTier {
    if is_native(name) {
        OfficialTier::Flash
    } else {
        convert(task)
    }
}

fn convert(tier: Tier) -> OfficialTier {
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
