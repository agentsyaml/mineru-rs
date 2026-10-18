use crate::official::{OfficialDataWriter, OfficialError, OfficialErrorKind};
use bytes::Bytes;
use regex::Regex;
use serde_json::Value;
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    sync::OnceLock,
};

use super::result::OfficialResult;

pub(super) fn validate_owned_assets(result: &OfficialResult) -> Result<(), OfficialError> {
    if result.retained_bytes() > crate::official::runtime::RETAINED_RESULT_CAP {
        return Err(OfficialError::new(
            OfficialErrorKind::LimitExceeded,
            "result exceeds retained byte limit",
        ));
    }
    if !result.middle.is_object() {
        return Err(OfficialError::new(
            OfficialErrorKind::InvalidInput,
            "middle JSON must be an object",
        ));
    }
    for path in result.assets.keys() {
        validate_asset_name(path)?;
    }
    validate_image_references(&result.middle, &result.assets)?;
    validate_markup_assets(&result.markdown, &result.assets)
}

fn validate_asset_name(value: &str) -> Result<(), OfficialError> {
    let components = value.split('/').collect::<Vec<_>>();
    let invalid = components.len() < 2
        || components[0] != "images"
        || components.iter().any(|part| {
            let base = part.split('.').next().unwrap_or("").to_ascii_lowercase();
            let device = matches!(base.as_str(), "con" | "prn" | "aux" | "nul")
                || ["com", "lpt"].iter().any(|prefix| {
                    base.strip_prefix(*prefix).is_some_and(|suffix| {
                        suffix.len() == 1
                            && matches!(suffix, "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9")
                    })
                });
            part.is_empty()
                || *part == "."
                || *part == ".."
                || matches!(part.chars().last(), Some('.') | Some(' '))
                || part.len() > 255
                || device
                || part
                    .chars()
                    .any(|ch| ch.is_control() || "\\<>:\"|?*".contains(ch))
        });
    if invalid || value.len() > 4096 || Path::new(value).is_absolute() {
        return Err(OfficialError::new(
            OfficialErrorKind::InvalidInput,
            "unsafe result asset path",
        ));
    }
    Ok(())
}

fn validate_image_references(
    middle: &Value,
    assets: &BTreeMap<String, Bytes>,
) -> Result<(), OfficialError> {
    let Some(pages) = middle.get("pages").and_then(Value::as_array) else {
        return Ok(());
    };
    for page in pages {
        if let Some(blocks) = page.get("blocks").and_then(Value::as_array) {
            for block in blocks {
                validate_block_references(block, assets)?;
            }
        }
    }
    Ok(())
}

fn is_http_url(value: &str) -> bool {
    value
        .get(..7)
        .is_some_and(|scheme| scheme.eq_ignore_ascii_case("http://"))
        || value
            .get(..8)
            .is_some_and(|scheme| scheme.eq_ignore_ascii_case("https://"))
}

fn validate_block_references(
    block: &Value,
    assets: &BTreeMap<String, Bytes>,
) -> Result<(), OfficialError> {
    let Some(object) = block.as_object() else {
        return Ok(());
    };
    let kind = object.get("type").and_then(Value::as_str).unwrap_or("");
    if matches!(
        kind,
        "equation" | "image_body" | "table_body" | "chart_body"
    ) && let Some(path) = object.get("image_path").and_then(Value::as_str)
        && !is_http_url(path)
        && !assets.contains_key(path)
    {
        return Err(OfficialError::new(
            OfficialErrorKind::InvalidInput,
            "result references a missing image asset",
        ));
    }
    if matches!(kind, "image_body" | "table_body" | "chart_body")
        && let Some(text) = object.get("content").and_then(Value::as_str)
    {
        validate_markup_assets(text, assets)?;
    }
    if let Some(children) = object.get("content").and_then(Value::as_array) {
        for child in children {
            validate_block_references(child, assets)?;
        }
    }
    Ok(())
}

fn validate_markup_assets(
    markdown: &str,
    assets: &BTreeMap<String, Bytes>,
) -> Result<(), OfficialError> {
    static MARKDOWN: OnceLock<Regex> = OnceLock::new();
    static HTML: OnceLock<Regex> = OnceLock::new();
    let markdown_images =
        MARKDOWN.get_or_init(|| Regex::new(r"!\[[^\]]*\]\(([^)\s]+)").expect("constant regex"));
    let html_images = HTML.get_or_init(|| {
        Regex::new(r#"(?is)<img\b[^>]*?\bsrc\s*=\s*["']?([^"'>\s]+)"#).expect("constant regex")
    });
    let searchable = mask_markdown_code(markdown);
    if markdown_images
        .captures_iter(&searchable)
        .chain(html_images.captures_iter(&searchable))
        .filter_map(|capture| capture.get(1))
        .any(|path| {
            let reference = path.as_str();
            if is_http_url(reference) || reference.starts_with("data:") {
                return false;
            }
            !assets.contains_key(reference) && !assets.contains_key(&format!("images/{reference}"))
        })
    {
        return Err(OfficialError::new(
            OfficialErrorKind::InvalidInput,
            "result references a missing image asset",
        ));
    }
    Ok(())
}

fn mask_markdown_code(markdown: &str) -> String {
    static PRE: OnceLock<Regex> = OnceLock::new();
    static CODE: OnceLock<Regex> = OnceLock::new();
    let text = PRE
        .get_or_init(|| Regex::new(r"(?is)<pre\b[^>]*>.*?</pre\s*>").expect("constant regex"))
        .replace_all(markdown, |captures: &regex::Captures<'_>| {
            " ".repeat(captures[0].len())
        });
    let text = CODE
        .get_or_init(|| Regex::new(r"(?is)<code\b[^>]*>.*?</code\s*>").expect("constant regex"))
        .replace_all(&text, |captures: &regex::Captures<'_>| {
            " ".repeat(captures[0].len())
        });
    let mut masked = String::with_capacity(text.len());
    let mut fence: Option<(char, usize)> = None;
    for line in text.split_inclusive('\n') {
        let body = line.trim_end_matches('\n');
        let indent = body.len() - body.trim_start_matches(' ').len();
        let trimmed = body.trim_start_matches(' ');
        let run = trimmed
            .chars()
            .take_while(|ch| *ch == '`' || *ch == '~')
            .collect::<Vec<_>>();
        if let Some((marker, width)) = fence {
            let closes = indent <= 3
                && run.len() >= width
                && run.first() == Some(&marker)
                && trimmed[run.len()..].trim().is_empty();
            masked.push_str(&" ".repeat(body.len()));
            if line.ends_with('\n') {
                masked.push('\n');
            }
            if closes {
                fence = None;
            }
        } else if indent <= 3 && run.len() >= 3 && run.iter().all(|ch| *ch == run[0]) {
            fence = Some((run[0], run.len()));
            masked.push_str(&" ".repeat(body.len()));
            if line.ends_with('\n') {
                masked.push('\n');
            }
        } else if indent >= 4 {
            masked.push_str(&" ".repeat(body.len()));
            if line.ends_with('\n') {
                masked.push('\n');
            }
        } else {
            masked.push_str(&mask_inline_code(body));
            if line.ends_with('\n') {
                masked.push('\n');
            }
        }
    }
    masked
}

fn mask_inline_code(line: &str) -> String {
    let mut chars = line.chars().collect::<Vec<_>>();
    let mut index = 0;
    while index < chars.len() {
        if chars[index] != '`' {
            index += 1;
            continue;
        }
        let start = index;
        while index < chars.len() && chars[index] == '`' {
            index += 1;
        }
        let width = index - start;
        let mut close = index;
        let mut end = None;
        while close < chars.len() {
            if chars[close] == '`' {
                let run = close;
                while close < chars.len() && chars[close] == '`' {
                    close += 1;
                }
                if close - run == width {
                    end = Some(close);
                    break;
                }
            } else {
                close += 1;
            }
        }
        if let Some(end) = end {
            for ch in &mut chars[start..end] {
                *ch = ' ';
            }
            index = end;
        }
    }
    chars.into_iter().collect()
}

pub(super) struct LocalBundleWriter {
    root: PathBuf,
}
impl LocalBundleWriter {
    pub(super) fn new(root: PathBuf) -> Self {
        Self { root }
    }
}
impl OfficialDataWriter for LocalBundleWriter {
    fn write(&mut self, path: &str, bytes: &[u8]) -> Result<(), OfficialError> {
        self.put(path, bytes)
    }
    fn write_string(&mut self, path: &str, text: &str) -> Result<(), OfficialError> {
        self.put(path, text.as_bytes())
    }
}
impl LocalBundleWriter {
    fn put(&mut self, path: &str, bytes: &[u8]) -> Result<(), OfficialError> {
        let relative = Path::new(path);
        if relative.is_absolute()
            || relative
                .components()
                .any(|part| !matches!(part, std::path::Component::Normal(_)))
        {
            return Err(OfficialError::new(
                OfficialErrorKind::InvalidInput,
                "unsafe result path",
            ));
        }
        let target = self.root.join(relative);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent).map_err(|error| {
                OfficialError::new(OfficialErrorKind::Transport, error.to_string())
            })?;
        }
        fs::write(target, bytes)
            .map_err(|error| OfficialError::new(OfficialErrorKind::Transport, error.to_string()))
    }
}

pub(super) fn collect_assets(
    root: &Path,
    current: &Path,
    total: &mut usize,
    cap: usize,
    assets: &mut BTreeMap<String, Bytes>,
) -> Result<(), OfficialError> {
    let at_root = current == root;
    for entry in fs::read_dir(current)
        .map_err(|e| OfficialError::new(OfficialErrorKind::Transport, e.to_string()))?
    {
        let entry =
            entry.map_err(|e| OfficialError::new(OfficialErrorKind::Transport, e.to_string()))?;
        let kind = entry
            .file_type()
            .map_err(|e| OfficialError::new(OfficialErrorKind::Transport, e.to_string()))?;
        if kind.is_symlink() {
            return Err(OfficialError::new(
                OfficialErrorKind::InvalidInput,
                "worker returned a symlink",
            ));
        }
        if kind.is_dir() {
            collect_assets(root, &entry.path(), total, cap, assets)?;
            continue;
        }
        if !kind.is_file()
            || (at_root
                && matches!(
                    entry.file_name().to_str(),
                    Some(
                        "markdown.md"
                            | "middle_json.json"
                            | "structured_content.json"
                            | "model_output.json"
                            | "operation.json"
                            | "render.json"
                            | "render.bin"
                    )
                ))
        {
            continue;
        }
        let relative = entry
            .path()
            .strip_prefix(root)
            .map_err(|e| OfficialError::new(OfficialErrorKind::InvalidInput, e.to_string()))?
            .to_owned();
        let remaining = cap.saturating_sub(*total);
        let bytes =
            crate::hybrid_v4_output::read_bundle_file_nofollow(root, &relative, remaining as u64)
                .map_err(crate::official::error::classify)?;
        *total = total.checked_add(bytes.len()).ok_or_else(|| {
            OfficialError::new(
                OfficialErrorKind::LimitExceeded,
                "result exceeds retained byte limit",
            )
        })?;
        assets.insert(
            relative.to_string_lossy().replace('\\', "/"),
            Bytes::from(bytes),
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validation_ignores_extensions_titles_and_code_bodies() {
        let middle = serde_json::json!({
            "metadata": {"title": "sample.png"},
            "extensions": {"image_path": "ordinary-value"},
            "pages": [{"blocks": [
                {"type": "code_body", "content": "![literal](missing.png)"},
                {"type": "image_body", "content": "```html\n<img src='missing.png'>\n```"}
            ]}]
        });
        assert!(validate_image_references(&middle, &BTreeMap::new()).is_ok());
    }

    #[test]
    fn actual_missing_image_reference_is_rejected() {
        let middle = serde_json::json!({"pages": [{"blocks": [
            {"type": "image_body", "image_path": "images/missing.png", "content": "image"}
        ]}]});
        assert!(validate_image_references(&middle, &BTreeMap::new()).is_err());
    }

    #[test]
    fn missing_typed_image_fails_before_writer_callback() {
        struct Writer(usize);
        impl OfficialDataWriter for Writer {
            fn write(&mut self, _: &str, _: &[u8]) -> Result<(), OfficialError> {
                self.0 += 1;
                Ok(())
            }
            fn write_string(&mut self, _: &str, _: &str) -> Result<(), OfficialError> {
                self.0 += 1;
                Ok(())
            }
        }
        let result = OfficialResult {
            middle: serde_json::json!({"pages": [{"blocks": [{"type":"image_body","image_path":"images/missing.png","content":"x"}]}]}),
            model: None,
            markdown: String::new(),
            structured: Value::Null,
            assets: BTreeMap::new(),
            warnings: Vec::new(),
            metadata: crate::official::OfficialVersionMetadata {
                mineru_package: "4.0.12".into(),
                schema_version: "2.0".into(),
                bundle_name: "hybrid-v4".into(),
            },
        };
        let mut writer = Writer(0);
        assert!(result.save_with_writer(&mut writer).is_err());
        assert_eq!(writer.0, 0);
    }

    #[test]
    fn asset_validation_ignores_prose_code_and_remote_image_urls() {
        let assets = BTreeMap::new();
        let text = "images/missing.png in prose; `![x](images/code.png)`; ![remote](https://host/images/remote.png)\n```html\n<img src='images/code.png'>\n```\n    ![indented](images/code.png)\n<pre><code><img src='images/code.png'></code></pre>";
        assert!(validate_markup_assets(text, &assets).is_ok());
        assert!(validate_markup_assets("![local](images/missing.png)", &assets).is_err());
        assert!(validate_markup_assets("<img src='images/missing.png'>", &assets).is_err());
    }

    #[test]
    fn remote_image_urls_with_mixed_case_schemes_save_unchanged() {
        struct Writer(Vec<String>);
        impl OfficialDataWriter for Writer {
            fn write(&mut self, _: &str, _: &[u8]) -> Result<(), OfficialError> {
                Ok(())
            }
            fn write_string(&mut self, _: &str, text: &str) -> Result<(), OfficialError> {
                self.0.push(text.to_owned());
                Ok(())
            }
        }
        let typed_url = "HTTPS://example.test/Case/Image.png";
        let markup_url = "HtTpS://example.test/Case/Markup.png";
        let html_url = "HtTpS://example.test/Case/Html.png";
        let markdown =
            "![remote](hTtP://example.test/Image.png) <img src='HtTpS://example.test/Other.png'>";
        let result = OfficialResult {
            middle: serde_json::json!({
                "schema": "docvortex.middle",
                "schema_version": "2.0",
                "pages": [{"blocks": [{"type":"image_body","image_path":typed_url,"content":format!("![remote]({markup_url}) <img src='{html_url}'>")}]}]
            }),
            model: None,
            markdown: markdown.into(),
            structured: serde_json::json!({}),
            assets: BTreeMap::new(),
            warnings: Vec::new(),
            metadata: crate::official::OfficialVersionMetadata {
                mineru_package: "4.0.12".into(),
                schema_version: "2.0".into(),
                bundle_name: "hybrid-v4".into(),
            },
        };
        let mut writer = Writer(Vec::new());
        result.save_with_writer(&mut writer).unwrap();
        assert_eq!(writer.0[0], markdown);
        assert!(writer.0[1].contains(typed_url));
        assert!(writer.0[1].contains(markup_url));
        assert!(writer.0[1].contains(html_url));

        let output = tempfile::tempdir().unwrap();
        result.save(output.path(), "remote").unwrap();
        let saved = output.path().join("remote/hybrid-v4");
        assert_eq!(
            fs::read_to_string(saved.join("markdown.md")).unwrap(),
            markdown
        );
        let middle = fs::read_to_string(saved.join("middle_json.json")).unwrap();
        assert!(middle.contains(typed_url));
        assert!(middle.contains(markup_url));
        assert!(middle.contains(html_url));

        let unsafe_typed = serde_json::json!({"pages": [{"blocks": [
            {"type": "image_body", "image_path": "file:///private/image.png"}
        ]}]});
        assert!(validate_image_references(&unsafe_typed, &BTreeMap::new()).is_err());
        assert!(
            validate_markup_assets("![local](file:///private/image.png)", &BTreeMap::new())
                .is_err()
        );
    }

    #[test]
    fn aggregate_asset_cap_is_checked_before_file_read() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("images")).unwrap();
        fs::write(dir.path().join("images/large.png"), [0u8; 8]).unwrap();
        let mut total = 0;
        let mut assets = BTreeMap::new();
        let error = collect_assets(dir.path(), dir.path(), &mut total, 4, &mut assets).unwrap_err();
        assert_eq!(error.kind, OfficialErrorKind::LimitExceeded);
        assert!(assets.is_empty());
    }
}
