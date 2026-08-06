use crate::{
    Asset, AssetKind, OfficialOutputManifest, PageResult, VlmError, VlmResult,
    official_builders::{
        OfficialBuildArtifacts, OfficialPreparedPage, finalize_official_document_until,
    },
};
use bytes::Bytes;
use cap_primitives::fs::{open_ambient_dir, open_dir_nofollow};
use cap_std::{ambient_authority, fs::Dir};
use std::{
    collections::HashSet,
    io::{self as stdio, Read, Seek, SeekFrom, Write},
    path::{Component, Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// Closed publication targets for official output.
#[derive(Clone, Copy)]
pub(crate) enum OfficialOutputTarget {
    Vlm,
    Office,
}

impl OfficialOutputTarget {
    fn name(self) -> &'static str {
        match self {
            Self::Vlm => "vlm",
            Self::Office => "office",
        }
    }
}

/// Route-owned staging keeps generated assets and serialized page output off the heap.
pub(crate) struct OfficialOutputStage {
    root: PathBuf,
    stem: String,
    target_name: OfficialOutputTarget,
    target: PathBuf,
    directory: Dir,
    staging_parent: Option<Dir>,
    stage: Option<Dir>,
    parts: Option<Dir>,
    pages: Vec<usize>,
    preview_pages: Vec<usize>,
    assets: HashSet<PathBuf>,
    asset_bytes: u64,
    text_bytes: u64,
    max_asset_bytes: u64,
    max_text_bytes: u64,
    resident_asset_bytes: usize,
    resident_text_bytes: usize,
    preview_written: bool,
    cleanup: Option<CleanupAdmission>,
}

impl OfficialOutputStage {
    #[cfg(test)]
    pub(crate) fn begin(
        root: &Path,
        stem: &str,
        target_name: OfficialOutputTarget,
        max_asset_bytes: u64,
        max_text_bytes: u64,
        origin: Option<(Bytes, &'static str)>,
    ) -> VlmResult<Self> {
        Self::begin_with_resident(
            root,
            stem,
            target_name,
            max_asset_bytes,
            max_text_bytes,
            usize::try_from(max_asset_bytes).unwrap_or(usize::MAX),
            usize::try_from(max_text_bytes).unwrap_or(usize::MAX),
            origin,
        )
    }
    pub(crate) fn begin_with_resident(
        root: &Path,
        stem: &str,
        target_name: OfficialOutputTarget,
        max_asset_bytes: u64,
        max_text_bytes: u64,
        resident_asset_bytes: usize,
        resident_text_bytes: usize,
        origin: Option<(Bytes, &'static str)>,
    ) -> VlmResult<Self> {
        let stem = canonical_stem(stem)?;
        let tree = OutputTree::open(root, &stem, target_name)?;
        let (staging_parent_name, staging_parent, stage) = create_private_stage(&tree.directory)?;
        if let Err(error) = stage.create_dir("parts").map_err(io) {
            drop(stage);
            remove_private_stage(&tree.directory, &staging_parent_name, staging_parent);
            return Err(error);
        }
        let parts = match open_child_nofollow(stage.try_clone().map_err(io)?, "parts") {
            Ok(parts) => parts,
            Err(error) => {
                drop(stage);
                remove_private_stage(&tree.directory, &staging_parent_name, staging_parent);
                return Err(error);
            }
        };
        // Admit cleanup while begin is already running on a blocking worker. Drop can then
        // transfer retained descriptor capabilities without cloning handles or spawning.
        let cleanup = match CleanupAdmission::new(
            &tree.directory,
            staging_parent_name.clone(),
            &staging_parent,
        ) {
            Ok(cleanup) => cleanup,
            Err(error) => {
                drop(parts);
                drop(stage);
                remove_private_stage(&tree.directory, &staging_parent_name, staging_parent);
                return Err(error);
            }
        };
        let mut output = Self {
            root: root.to_path_buf(),
            stem,
            target_name,
            target: tree.target,
            directory: tree.directory,
            staging_parent: Some(staging_parent),
            stage: Some(stage),
            parts: Some(parts),
            pages: Vec::new(),
            preview_pages: Vec::new(),
            assets: HashSet::new(),
            asset_bytes: 0,
            text_bytes: 0,
            max_asset_bytes,
            max_text_bytes,
            resident_asset_bytes,
            resident_text_bytes,
            preview_written: false,
            cleanup: Some(cleanup),
        };
        if let Some((origin, suffix)) = origin {
            output.write_origin(origin, suffix)?;
        }
        Ok(output)
    }

    pub(crate) fn write_prepared_page(
        &mut self,
        source_page_idx: usize,
        prepared: OfficialPreparedPage,
        assets: &[Asset],
    ) -> VlmResult<()> {
        if self
            .pages
            .last()
            .is_some_and(|previous| *previous >= source_page_idx)
        {
            return invalid("official pages must have increasing source indexes");
        }
        self.write_serialized_in(
            &self.parts()?.try_clone().map_err(io)?,
            format!("{source_page_idx:020}-prepared.json"),
            &prepared,
        )?;
        for asset in assets {
            self.write_asset(asset)?;
        }
        self.pages.push(source_page_idx);
        Ok(())
    }

    pub(crate) fn finalize_document(
        &mut self,
        formula_enable: bool,
        table_enable: bool,
        deadline: std::time::Instant,
    ) -> VlmResult<()> {
        let prepared_bytes = self.pages.iter().try_fold(0u64, |total, page| {
            let bytes = self
                .parts()?
                .metadata(format!("{page:020}-prepared.json"))
                .map_err(io)?
                .len();
            Ok::<_, VlmError>(total.saturating_add(bytes))
        })?;
        if prepared_bytes > self.max_text_bytes {
            return Err(VlmError::LimitExceeded {
                resource: "staged prepared-page bytes",
                limit: self.max_text_bytes as u64,
                actual: prepared_bytes as u64,
            });
        }
        let prepared = self
            .pages
            .iter()
            .map(|page| {
                check_stage_deadline(deadline)?;
                serde_json::from_reader(
                    self.parts()?
                        .open(format!("{page:020}-prepared.json"))
                        .map_err(io)?,
                )
                .map_err(|error| VlmError::Protocol {
                    operation: "official output staging",
                    message: error.to_string(),
                })
            })
            .collect::<VlmResult<Vec<OfficialPreparedPage>>>()?;
        let built = finalize_official_document_until(
            prepared,
            formula_enable,
            table_enable,
            Some(deadline),
        )?;
        if built.len() != self.pages.len() {
            return invalid("official document page count changed during canonicalization");
        }
        for (source_page_idx, built) in self.pages.clone().into_iter().zip(built) {
            check_stage_deadline(deadline)?;
            self.write_final_page(source_page_idx, built)?;
        }
        Ok(())
    }

    fn write_final_page(
        &mut self,
        source_page_idx: usize,
        built: OfficialBuildArtifacts,
    ) -> VlmResult<()> {
        let model = crate::vlm_types::model_output_wire(&built.model_output)?;
        let model = model
            .as_array()
            .and_then(|pages| pages.first())
            .ok_or_else(|| VlmError::Protocol {
                operation: "official output staging",
                message: "page model output is missing".into(),
            })?;
        let middle = built
            .middle_json
            .get("pdf_info")
            .and_then(serde_json::Value::as_array)
            .and_then(|pages| pages.first())
            .ok_or_else(|| VlmError::Protocol {
                operation: "official output staging",
                message: "page middle output is missing".into(),
            })?;
        let v2 = built
            .content_list_v2
            .as_array()
            .and_then(|pages| pages.first())
            .ok_or_else(|| VlmError::Protocol {
                operation: "official output staging",
                message: "page content-list-v2 output is missing".into(),
            })?;
        let parts = self.parts()?.try_clone().map_err(io)?;
        self.write_json_in(&parts, format!("{source_page_idx:020}-model.json"), model)?;
        self.write_json_in(&parts, format!("{source_page_idx:020}-middle.json"), middle)?;
        self.write_json_in(
            &parts,
            format!("{source_page_idx:020}-content.json"),
            &built.content_list,
        )?;
        self.write_json_in(&parts, format!("{source_page_idx:020}-v2.json"), v2)?;
        self.write_text_in(
            &parts,
            format!("{source_page_idx:020}-markdown.md"),
            built.markdown.as_bytes(),
        )?;
        Ok(())
    }

    pub(crate) fn write_preview_page(&mut self, page: &PageResult) -> VlmResult<()> {
        if self
            .preview_pages
            .last()
            .is_some_and(|previous| *previous >= page.page_index)
        {
            return invalid("official pages must have increasing source indexes");
        }
        self.write_serialized_in(
            &self.parts()?.try_clone().map_err(io)?,
            format!("{:020}-preview.json", page.page_index),
            page,
        )?;
        self.preview_pages.push(page.page_index);
        Ok(())
    }

    pub(crate) fn preview_pages(&self) -> VlmResult<Vec<PageResult>> {
        self.preview_pages
            .iter()
            .map(|page| {
                serde_json::from_reader(
                    self.parts()?
                        .open(format!("{page:020}-preview.json"))
                        .map_err(io)?,
                )
                .map_err(|error| VlmError::Protocol {
                    operation: "official output staging",
                    message: error.to_string(),
                })
            })
            .collect()
    }

    pub(crate) fn remaining_asset_bytes(&self) -> u64 {
        self.max_asset_bytes.saturating_sub(self.asset_bytes)
    }

    pub(crate) fn remaining_asset_buffer_bytes(&self) -> usize {
        usize::try_from(
            self.remaining_asset_bytes()
                .min(self.resident_asset_bytes as u64),
        )
        .expect("resident asset cap fits usize")
    }

    pub(crate) fn remaining_text_bytes(&self) -> u64 {
        self.max_text_bytes.saturating_sub(self.text_bytes)
    }

    pub(crate) fn write_preview(&mut self, asset: &Asset) -> VlmResult<()> {
        if self.preview_written
            || !matches!(&asset.kind, AssetKind::Other(kind) if kind == "layout_preview")
            || asset.media_type != "application/pdf"
            || asset.data.is_empty()
        {
            return invalid("exactly one layout preview asset is required");
        }
        self.write_asset(asset)?;
        self.preview_written = true;
        Ok(())
    }

    pub(crate) fn assemble(&mut self, deadline: std::time::Instant) -> VlmResult<()> {
        if self.pages.is_empty() || !self.preview_written || self.pages != self.preview_pages {
            return invalid("official stage is incomplete");
        }
        self.write_final_outputs(deadline)?;
        drop(self.parts.take());
        self.stage()?.remove_dir_all("parts").map_err(io)?;
        check_stage_deadline(deadline)?;
        Ok(())
    }

    pub(crate) fn commit(mut self) -> VlmResult<OfficialCommit> {
        let manifest = OfficialOutputManifest {
            root: self.root.clone(),
            stem: self.stem.clone(),
            vlm_dir: self.target.clone(),
        };
        self.close_stage_handles();
        let result = install_stage_in(
            self.staging_parent()?,
            std::ffi::OsStr::new("stage"),
            &self.directory,
            std::ffi::OsStr::new(self.target_name.name()),
        );
        match result {
            Ok(backup) => {
                drop(self.staging_parent.take());
                let mut cleanup = self
                    .cleanup
                    .take()
                    .expect("cleanup admitted before stage live");
                cleanup.job.backup = backup;
                // Publication is complete, but successful callers must not report completion
                // while this admitted, capability-safe cleanup still owns private artifacts.
                let cleanup = cleanup.detach(true);
                Ok(OfficialCommit { manifest, cleanup })
            }
            Err(error) => {
                self.cleanup();
                Err(error)
            }
        }
    }

    /// Consumes the stage so cleanup can be scheduled outside an async executor.
    pub(crate) fn cleanup(mut self) -> CleanupOutcome {
        self.cleanup_now()
    }

    fn cleanup_now(&mut self) -> CleanupOutcome {
        self.close_stage_handles();
        drop(self.staging_parent.take());
        if let Some(cleanup) = self.cleanup.take() {
            cleanup.detach(true)
        } else {
            CleanupOutcome::default()
        }
    }

    fn stage(&self) -> VlmResult<&Dir> {
        self.stage.as_ref().ok_or_else(|| VlmError::Protocol {
            operation: "official output staging",
            message: "stage directory is closed".into(),
        })
    }

    fn parts(&self) -> VlmResult<&Dir> {
        self.parts.as_ref().ok_or_else(|| VlmError::Protocol {
            operation: "official output staging",
            message: "parts directory is closed".into(),
        })
    }

    fn staging_parent(&self) -> VlmResult<&Dir> {
        self.staging_parent
            .as_ref()
            .ok_or_else(|| VlmError::Protocol {
                operation: "official output staging",
                message: "staging parent directory is closed".into(),
            })
    }

    fn close_stage_handles(&mut self) {
        drop(self.parts.take());
        drop(self.stage.take());
    }

    fn write_asset(&mut self, asset: &Asset) -> VlmResult<()> {
        let path = if matches!(&asset.kind, AssetKind::Other(kind) if kind == "layout_preview") {
            PathBuf::from(format!("{}_layout.pdf", self.stem))
        } else {
            validate_asset(asset, &self.stem)?
        };
        if !self.assets.insert(path.clone()) {
            let parent =
                open_or_create_relative(self.stage()?, path.parent().expect("asset parent"))?;
            let mut existing = Vec::new();
            parent
                .open(path.file_name().expect("asset name"))
                .map_err(io)?
                .read_to_end(&mut existing)
                .map_err(io)?;
            if existing != asset.data {
                return invalid("asset path collision");
            }
            return Ok(());
        }
        let next = self.asset_bytes.saturating_add(asset.data.len() as u64);
        if next > self.max_asset_bytes {
            return Err(VlmError::LimitExceeded {
                resource: "total asset bytes",
                limit: self.max_asset_bytes,
                actual: next,
            });
        }
        let parent = open_or_create_relative(self.stage()?, path.parent().expect("asset parent"))?;
        write_bytes_in(&parent, path.file_name().expect("asset name"), &asset.data)?;
        self.asset_bytes = next;
        Ok(())
    }

    fn write_origin(&mut self, origin: Bytes, suffix: &'static str) -> VlmResult<()> {
        if origin.is_empty() {
            return invalid("origin must not be empty");
        }
        let next = self.asset_bytes.saturating_add(origin.len() as u64);
        if next > self.max_asset_bytes {
            return Err(VlmError::LimitExceeded {
                resource: "total asset bytes",
                limit: self.max_asset_bytes,
                actual: next,
            });
        }
        write_bytes_in(
            self.stage()?,
            format!("{}_origin.{suffix}", self.stem),
            &origin,
        )?;
        self.asset_bytes = next;
        Ok(())
    }

    fn write_json_in(
        &mut self,
        directory: &Dir,
        name: impl AsRef<Path>,
        value: &serde_json::Value,
    ) -> VlmResult<()> {
        self.write_serialized_in(directory, name, value)
    }

    fn write_serialized_in<T: serde::Serialize>(
        &mut self,
        directory: &Dir,
        name: impl AsRef<Path>,
        value: &T,
    ) -> VlmResult<()> {
        let mut output = CappedFile::new(
            directory.create(name).map_err(io)?,
            self.remaining_text_buffer_bytes(),
        );
        if let Err(error) = serde_json::to_writer(&mut output, value) {
            if output.attempted > output.limit {
                return Err(VlmError::LimitExceeded {
                    resource: "staged text/JSON bytes",
                    limit: self.max_text_bytes,
                    actual: self.text_bytes.saturating_add(output.attempted as u64),
                });
            }
            return Err(VlmError::Protocol {
                operation: "official output serialization",
                message: error.to_string(),
            });
        }
        self.charge_text(output.written as u64)
    }

    fn write_text_in(
        &mut self,
        directory: &Dir,
        name: impl AsRef<Path>,
        bytes: &[u8],
    ) -> VlmResult<()> {
        self.charge_text(bytes.len() as u64)?;
        write_bytes_in(directory, name, bytes)
    }

    fn append_part<W: Write>(&mut self, output: &mut W, name: impl AsRef<Path>) -> VlmResult<()> {
        let bytes = self.parts()?.metadata(name.as_ref()).map_err(io)?.len();
        self.charge_text(bytes)?;
        stdio::copy(&mut self.parts()?.open(name).map_err(io)?, output).map_err(io)?;
        Ok(())
    }

    pub(crate) fn remaining_text_buffer_bytes(&self) -> usize {
        usize::try_from(
            self.remaining_text_bytes()
                .min(self.resident_text_bytes as u64),
        )
        .expect("resident text cap fits usize")
    }

    fn charge_text(&mut self, bytes: u64) -> VlmResult<()> {
        let next = self.text_bytes.saturating_add(bytes);
        if next > self.max_text_bytes {
            return Err(VlmError::LimitExceeded {
                resource: "staged text/JSON bytes",
                limit: self.max_text_bytes,
                actual: next,
            });
        }
        self.text_bytes = next;
        Ok(())
    }

    fn write_final_outputs(&mut self, deadline: std::time::Instant) -> VlmResult<()> {
        let mut middle = self
            .stage()?
            .create(format!("{}_middle.json", self.stem))
            .map_err(io)?;
        self.write_fragment(&mut middle, b"{\"pdf_info\":[")?;
        for (position, page) in self.pages.clone().into_iter().enumerate() {
            check_stage_deadline(deadline)?;
            if position != 0 {
                self.write_fragment(&mut middle, b",")?;
            }
            self.append_part(&mut middle, format!("{page:020}-middle.json"))?;
        }
        self.write_fragment(
            &mut middle,
            b"],\"_backend\":\"vlm\",\"_version_name\":\"3.4.4\"}",
        )?;

        let mut model = self
            .stage()?
            .create(format!("{}_model.json", self.stem))
            .map_err(io)?;
        self.write_fragment(&mut model, b"[")?;
        for (position, page) in self.pages.clone().into_iter().enumerate() {
            check_stage_deadline(deadline)?;
            if position != 0 {
                self.write_fragment(&mut model, b",")?;
            }
            self.append_part(&mut model, format!("{page:020}-model.json"))?;
        }
        self.write_fragment(&mut model, b"]")?;

        let mut content = self
            .stage()?
            .create(format!("{}_content_list.json", self.stem))
            .map_err(io)?;
        self.write_fragment(&mut content, b"[")?;
        let mut content_written = false;
        for page in self.pages.clone() {
            check_stage_deadline(deadline)?;
            let mut source = self
                .parts()?
                .open(format!("{page:020}-content.json"))
                .map_err(io)?;
            let length =
                usize::try_from(source.metadata().map_err(io)?.len()).unwrap_or(usize::MAX);
            if length < 2 {
                return Err(VlmError::Protocol {
                    operation: "official output staging",
                    message: "page content output is not an array".into(),
                });
            }
            let mut first = [0; 1];
            let mut last = [0; 1];
            source.read_exact(&mut first).map_err(io)?;
            source.seek(SeekFrom::End(-1)).map_err(io)?;
            source.read_exact(&mut last).map_err(io)?;
            let inner = length - 2;
            if first != *b"[" || last != *b"]" {
                return Err(VlmError::Protocol {
                    operation: "official output staging",
                    message: "page content output is not an array".into(),
                });
            }
            if inner != 0 {
                if content_written {
                    self.write_fragment(&mut content, b",")?;
                }
                self.charge_text(inner as u64)?;
                source.seek(SeekFrom::Start(1)).map_err(io)?;
                stdio::copy(&mut source.take(inner as u64), &mut content).map_err(io)?;
                content_written = true;
            }
        }
        self.write_fragment(&mut content, b"]")?;

        let mut v2 = self
            .stage()?
            .create(format!("{}_content_list_v2.json", self.stem))
            .map_err(io)?;
        self.write_fragment(&mut v2, b"[")?;
        for (position, page) in self.pages.clone().into_iter().enumerate() {
            check_stage_deadline(deadline)?;
            if position != 0 {
                self.write_fragment(&mut v2, b",")?;
            }
            self.append_part(&mut v2, format!("{page:020}-v2.json"))?;
        }
        self.write_fragment(&mut v2, b"]")?;

        let mut markdown = self
            .stage()?
            .create(format!("{}.md", self.stem))
            .map_err(io)?;
        let mut markdown_written = false;
        for page in self.pages.clone() {
            check_stage_deadline(deadline)?;
            let name = format!("{page:020}-markdown.md");
            let bytes = usize::try_from(self.parts()?.metadata(&name).map_err(io)?.len())
                .unwrap_or(usize::MAX);
            if bytes == 0 {
                continue;
            }
            if markdown_written {
                self.write_fragment(&mut markdown, b"\n\n")?;
            }
            self.charge_text(bytes as u64)?;
            stdio::copy(&mut self.parts()?.open(name).map_err(io)?, &mut markdown).map_err(io)?;
            markdown_written = true;
        }
        Ok(())
    }

    fn write_fragment<W: Write>(&mut self, output: &mut W, bytes: &[u8]) -> VlmResult<()> {
        self.charge_text(bytes.len() as u64)?;
        output.write_all(bytes).map_err(io)
    }
}

impl Drop for OfficialOutputStage {
    fn drop(&mut self) {
        self.close_stage_handles();
        drop(self.staging_parent.take());
        if let Some(cleanup) = self.cleanup.take() {
            cleanup.detach(false);
        }
    }
}

fn write_bytes_in(directory: &Dir, name: impl AsRef<Path>, bytes: &[u8]) -> VlmResult<()> {
    let mut file = directory.create(name).map_err(io)?;
    file.write_all(bytes).map_err(io)
}

fn validate_asset(asset: &Asset, stem: &str) -> VlmResult<PathBuf> {
    let path = &asset.relative_path;
    let Some(text) = path.to_str() else {
        return invalid("official assets must have unique normalized images/... paths");
    };
    let reserved = [
        format!("{stem}_middle.json"),
        format!("{stem}_model.json"),
        format!("{stem}_content_list.json"),
        format!("{stem}_content_list_v2.json"),
        format!("{stem}.md"),
        format!("{stem}_layout.pdf"),
    ];
    if !portable_asset_path(text) || reserved.iter().any(|name| text.eq_ignore_ascii_case(name)) {
        return invalid("official assets must have unique normalized images/... paths");
    }
    Ok(path.clone())
}

/// Canonical portable output directory name shared by route preflight and publication.
pub fn canonical_stem(value: &str) -> VlmResult<String> {
    let stem = crate::vlm_types::sanitize_stem(value);
    // Windows silently strips trailing dots/spaces from names, which would let a
    // published `foo.` directory alias `foo`; trim them so the stem is portable.
    // Empty and dot-only stems collapse to the generic document name.
    let stem = stem.trim_end_matches(['.', ' ']);
    let stem = if stem.is_empty() {
        "document".to_owned()
    } else {
        stem.to_owned()
    };
    if portable_name(&stem) && !windows_device_name(&stem) {
        Ok(stem)
    } else {
        invalid("official output stem is not portable")
    }
}

fn portable_asset_path(path: &str) -> bool {
    !path.is_empty()
        && !Path::new(path).is_absolute()
        && !path.contains('\\')
        && path.split('/').count() > 1
        && path.split('/').next() == Some("images")
        && path.split('/').all(portable_name)
}

fn portable_name(name: &str) -> bool {
    !name.is_empty()
        && !name.ends_with(['.', ' '])
        && name.chars().all(crate::vlm_types::is_safe_stem_char)
        && !windows_device_name(name)
}

fn windows_device_name(name: &str) -> bool {
    let base = name.split('.').next().unwrap_or_default();
    base.eq_ignore_ascii_case("con")
        || base.eq_ignore_ascii_case("prn")
        || base.eq_ignore_ascii_case("aux")
        || base.eq_ignore_ascii_case("nul")
        || (base.len() == 4
            && (base[..3].eq_ignore_ascii_case("com") || base[..3].eq_ignore_ascii_case("lpt"))
            && matches!(base.as_bytes()[3], b'1'..=b'9'))
}

fn sibling(parent: &Path, kind: &str) -> PathBuf {
    parent.join(format!(
        ".vlm-{kind}-{}-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ))
}

fn sibling_name(kind: &str) -> std::ffi::OsString {
    sibling(Path::new(""), kind)
        .file_name()
        .expect("sibling name")
        .to_os_string()
}

struct OutputTree {
    directory: Dir,
    target: PathBuf,
}

impl OutputTree {
    fn open(root: &Path, stem: &str, target_name: OfficialOutputTarget) -> VlmResult<Self> {
        let root_dir = open_or_create_root(root)?;
        if root_dir.symlink_metadata(stem).is_err() {
            root_dir.create_dir(stem).map_err(io)?;
        }
        let directory = open_child_nofollow(root_dir, std::ffi::OsStr::new(stem))?;
        Ok(Self {
            directory,
            target: root.join(stem).join(target_name.name()),
        })
    }
}

fn open_or_create_root(path: &Path) -> VlmResult<Dir> {
    #[cfg(windows)]
    let (start, skip) = windows_root_anchor(path)?;
    #[cfg(not(windows))]
    let start = if path.is_absolute() {
        PathBuf::from("/")
    } else {
        PathBuf::from(".")
    };
    let components: Vec<_> = path.components().collect();
    #[cfg(not(windows))]
    let skip = 0;
    let mut dir = Dir::from_std_file(open_ambient_dir(&start, ambient_authority()).map_err(io)?);
    let macos_private_alias = cfg!(target_os = "macos")
        && matches!(components.as_slice(), [Component::RootDir, Component::Normal(name), ..] if *name == "var" || *name == "tmp");
    if macos_private_alias {
        dir = open_child_nofollow(dir, "private")?;
    }
    for (index, component) in components.into_iter().enumerate().skip(skip) {
        match component {
            Component::RootDir | Component::CurDir => continue,
            Component::ParentDir => {
                return Err(VlmError::InvalidInput(
                    "output root must not contain parent traversal".into(),
                ));
            }
            Component::Prefix(_) => {
                return Err(VlmError::InvalidInput("unsupported output root".into()));
            }
            Component::Normal(name) => {
                // macOS's /var and /tmp are fixed /private system aliases, not
                // caller-controlled output component.
                if macos_private_alias && index == 1 {
                    dir = open_child_nofollow(dir, name)?;
                    continue;
                }
                if dir.symlink_metadata(name).is_err() {
                    dir.create_dir(name).map_err(io)?;
                }
                dir = open_child_nofollow(dir, name)?;
            }
        }
    }
    Ok(dir)
}

#[cfg(windows)]
fn windows_root_anchor(path: &Path) -> VlmResult<(PathBuf, usize)> {
    use std::path::Prefix;

    let components: Vec<_> = path.components().collect();
    let Some(Component::Prefix(prefix)) = components.first() else {
        if matches!(components.first(), Some(Component::RootDir)) {
            return Err(VlmError::InvalidInput(
                "Windows output root must include a drive or UNC share".into(),
            ));
        }
        return Ok((PathBuf::from("."), 0));
    };
    match prefix.kind() {
        Prefix::Disk(_) | Prefix::UNC(_, _) if path.is_absolute() => {
            let mut anchor = PathBuf::from(prefix.as_os_str());
            anchor.push(std::path::MAIN_SEPARATOR_STR);
            let skip = 1 + usize::from(matches!(components.get(1), Some(Component::RootDir)));
            Ok((anchor, skip))
        }
        Prefix::Disk(_) | Prefix::UNC(_, _) => Err(VlmError::InvalidInput(
            "drive-relative output roots are unsupported".into(),
        )),
        Prefix::DeviceNS(_) => Err(VlmError::InvalidInput(
            "Windows device namespace output roots are unsupported".into(),
        )),
        Prefix::Verbatim(_) | Prefix::VerbatimDisk(_) | Prefix::VerbatimUNC(_, _) => Err(
            VlmError::InvalidInput("Windows verbatim output roots are unsupported".into()),
        ),
    }
}

fn open_or_create_relative(parent: &Dir, path: &Path) -> VlmResult<Dir> {
    let mut dir = parent.try_clone().map_err(io)?;
    for component in path.components() {
        let Component::Normal(name) = component else {
            return Err(VlmError::InvalidInput(
                "output path must be relative without traversal".into(),
            ));
        };
        if dir.symlink_metadata(name).is_err() {
            dir.create_dir(name).map_err(io)?;
        }
        dir = open_child_nofollow(dir, name)?;
    }
    Ok(dir)
}

fn open_child_nofollow(parent: Dir, name: impl AsRef<Path>) -> VlmResult<Dir> {
    Ok(Dir::from_std_file(
        open_dir_nofollow(&parent.into_std_file(), name.as_ref()).map_err(io)?,
    ))
}

fn create_private_stage(directory: &Dir) -> VlmResult<(std::ffi::OsString, Dir, Dir)> {
    let parent_name = sibling_name("staging-parent");
    directory.create_dir(&parent_name).map_err(io)?;
    let parent = match directory
        .try_clone()
        .map_err(io)
        .and_then(|directory| open_child_nofollow(directory, &parent_name))
    {
        Ok(parent) => parent,
        Err(error) => {
            let _ = directory.remove_dir(&parent_name);
            return Err(error);
        }
    };
    if let Err(error) = parent.create_dir("stage").map_err(io) {
        remove_private_stage(directory, &parent_name, parent);
        return Err(error);
    }
    let stage = match parent
        .try_clone()
        .map_err(io)
        .and_then(|parent| open_child_nofollow(parent, "stage"))
    {
        Ok(stage) => stage,
        Err(error) => {
            remove_private_stage(directory, &parent_name, parent);
            return Err(error);
        }
    };
    Ok((parent_name, parent, stage))
}

fn remove_private_stage(directory: &Dir, parent_name: &std::ffi::OsStr, parent: Dir) {
    let _ = remove_private_stage_outcome(directory, parent_name, parent);
}

fn remove_private_stage_outcome(
    directory: &Dir,
    parent_name: &std::ffi::OsStr,
    parent: Dir,
) -> CleanupOutcome {
    // The parent handle reaches only our private staging tree. The outer removal is
    // descriptor-relative and deliberately non-recursive.
    let mut outcome = CleanupOutcome::default();
    if let Err(error) = parent.remove_dir_all("stage")
        && error.kind() != stdio::ErrorKind::NotFound
    {
        outcome.failed = true;
    }
    drop(parent);
    if directory.remove_dir(parent_name).is_err() {
        outcome.failed = true;
    }
    outcome
}

pub(crate) struct OfficialCommit {
    pub(crate) manifest: OfficialOutputManifest,
    pub(crate) cleanup: CleanupOutcome,
}

#[derive(Clone, Copy, Default)]
pub(crate) struct CleanupOutcome {
    failed: bool,
}

impl CleanupOutcome {
    pub(crate) fn failed(self) -> bool {
        self.failed
    }

    fn merge(&mut self, other: Self) {
        self.failed |= other.failed;
    }
}

struct CleanupAdmission {
    sender: std::sync::mpsc::SyncSender<CleanupJob>,
    done: std::sync::mpsc::Receiver<CleanupOutcome>,
    job: CleanupJob,
}

struct CleanupJob {
    directory: Dir,
    backup: Option<Backup>,
    staging_parent_name: std::ffi::OsString,
    staging_parent: Dir,
}

impl CleanupAdmission {
    fn new(
        directory: &Dir,
        staging_parent_name: std::ffi::OsString,
        staging_parent: &Dir,
    ) -> VlmResult<Self> {
        // Retain every capability before admitting the worker. If admission fails, begin owns
        // the original capabilities and removes the staging tree synchronously.
        let directory = directory.try_clone().map_err(io)?;
        let staging_parent = staging_parent.try_clone().map_err(io)?;
        let (sender, receiver) = std::sync::mpsc::sync_channel::<CleanupJob>(1);
        let (done_sender, done) = std::sync::mpsc::sync_channel(1);
        std::thread::Builder::new()
            .name("official-output-cleanup".into())
            .spawn(move || {
                if let Ok(cleanup) = receiver.recv() {
                    let _ = done_sender.send(cleanup.run());
                }
            })
            .map_err(io)?;
        Ok(Self {
            sender,
            done,
            job: CleanupJob {
                directory,
                backup: None,
                staging_parent_name,
                staging_parent,
            },
        })
    }

    fn detach(self, wait: bool) -> CleanupOutcome {
        // The worker was admitted before publication and is blocked in recv, so this cannot
        // silently lose the cleanup capability. If it died, retain no leak by cleaning with the
        // same no-follow capabilities in this caller.
        if let Err(error) = self.sender.send(self.job) {
            error.0.run()
        } else if wait {
            self.done.recv().unwrap_or(CleanupOutcome { failed: true })
        } else {
            CleanupOutcome::default()
        }
    }
}

impl CleanupJob {
    fn run(self) -> CleanupOutcome {
        let Self {
            directory,
            backup,
            staging_parent_name,
            staging_parent,
        } = self;
        let mut outcome = CleanupOutcome::default();
        if let Some(backup) = backup {
            outcome.merge(remove_backup(backup));
        }
        outcome.merge(remove_private_stage_outcome(
            &directory,
            &staging_parent_name,
            staging_parent,
        ));
        outcome
    }
}

struct Backup {
    name: std::ffi::OsString,
    parent: Dir,
    directory: Dir,
}

fn remove_backup(backup: Backup) -> CleanupOutcome {
    // The backup was opened no-follow before publication and lives under our private staging
    // parent. Never recurse through its former public pathname.
    let Backup {
        name,
        parent,
        directory,
    } = backup;
    let mut outcome = CleanupOutcome::default();
    match directory.read_dir(".") {
        Ok(entries) => {
            for entry in entries {
                match entry {
                    Ok(entry) => {
                        let name = entry.file_name();
                        if directory.remove_dir_all(&name).is_err()
                            && directory.remove_file(&name).is_err()
                        {
                            outcome.failed = true;
                        }
                    }
                    Err(_) => {
                        outcome.failed = true;
                    }
                }
            }
        }
        Err(_) => outcome.failed = true,
    }
    // `name` is private to the staging parent, whose retained capability removes only this
    // original entry after its opened directory has been emptied.
    drop(directory);
    if parent.remove_dir(&name).is_err() {
        outcome.failed = true;
    }
    outcome
}

fn install_stage_in(
    stage_parent_handle: &Dir,
    stage_name: &std::ffi::OsStr,
    directory_handle: &Dir,
    target_name: &std::ffi::OsStr,
) -> VlmResult<Option<Backup>> {
    let target_exists = match directory_handle.symlink_metadata(target_name) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            return Err(VlmError::InvalidInput(
                "official output target became a symlink during installation".into(),
            ));
        }
        Ok(_) => true,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => return Err(io(error)),
    };
    // Keep the parent capabilities before moving the target. Every fallible operation after the
    // rename rolls the original target back without ever following or deleting a public path.
    let backup_parent = stage_parent_handle.try_clone().map_err(io)?;
    let backup_directory_parent = stage_parent_handle.try_clone().map_err(io)?;
    let backup = if target_exists {
        let backup = format!(
            ".vlm-backup-{}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos(),
            COUNTER.fetch_add(1, Ordering::Relaxed),
        );
        let backup_name = std::ffi::OsString::from(backup);
        if let Err(error) = directory_handle.rename(target_name, stage_parent_handle, &backup_name)
        {
            return Err(io(error));
        }
        let directory = match open_child_nofollow(backup_directory_parent, &backup_name) {
            Ok(directory) => directory,
            Err(error) => {
                return rollback_target(
                    stage_parent_handle,
                    &backup_name,
                    directory_handle,
                    target_name,
                    error,
                );
            }
        };
        Some(Backup {
            directory,
            parent: backup_parent,
            name: backup_name,
        })
    } else {
        None
    };
    if let Err(error) = stage_parent_handle.rename(stage_name, directory_handle, target_name) {
        let install_error = error.to_string();
        if let Some(backup) = backup {
            let Backup {
                name, directory, ..
            } = backup;
            drop(directory);
            if let Err(rollback_error) =
                stage_parent_handle.rename(&name, directory_handle, target_name)
            {
                return Err(VlmError::Io {
                    operation: "official output rollback",
                    message: format!(
                        "install failed: {install_error}; restoring previous vlm failed: {rollback_error}"
                    ),
                });
            }
        }
        return Err(VlmError::Io {
            operation: "official output install",
            message: install_error,
        });
    }
    // Publication is committed once stage has been installed. The caller retains descriptor
    // capabilities and detaches any potentially large backup cleanup.
    Ok(backup)
}

fn rollback_target<T>(
    stage_parent_handle: &Dir,
    backup_name: &std::ffi::OsStr,
    directory_handle: &Dir,
    target_name: &std::ffi::OsStr,
    error: VlmError,
) -> VlmResult<T> {
    match stage_parent_handle.rename(backup_name, directory_handle, target_name) {
        Ok(()) => Err(error),
        Err(rollback_error) => Err(VlmError::Io {
            operation: "official output rollback",
            message: format!(
                "install failed: {error}; restoring previous vlm failed: {rollback_error}"
            ),
        }),
    }
}
fn invalid<T>(message: &str) -> VlmResult<T> {
    Err(VlmError::Protocol {
        operation: "official output validation",
        message: message.into(),
    })
}
fn io(error: std::io::Error) -> VlmError {
    VlmError::Io {
        operation: "official output",
        message: error.to_string(),
    }
}

fn check_stage_deadline(deadline: std::time::Instant) -> VlmResult<()> {
    if std::time::Instant::now() >= deadline {
        Err(VlmError::Timeout {
            operation: "official PDF",
        })
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{
        OfficialOutputStage, OfficialOutputTarget, OutputTree, canonical_stem,
        create_private_stage, install_stage_in, open_or_create_root, remove_private_stage,
    };
    use crate::VlmError;
    use bytes::Bytes;
    use std::path::Path;

    #[test]
    fn canonical_stem_preserves_unicode_and_replaces_only_unsafe_chars() {
        // Chinese plus CJK punctuation survives unchanged.
        assert_eq!(
            canonical_stem("文档《报告》·2026").unwrap(),
            "文档《报告》·2026"
        );
        assert_eq!(
            canonical_stem("日本語-フォルダ_名").unwrap(),
            "日本語-フォルダ_名"
        );
        assert_eq!(canonical_stem("foo bar.pdf").unwrap(), "foo bar.pdf");
        // Separators and Windows-reserved characters are replaced.
        assert_eq!(canonical_stem("../office").unwrap(), ".._office");
        assert_eq!(canonical_stem("a\\b/c?d").unwrap(), "a_b_c_d");
        assert_eq!(canonical_stem("a\u{0}b\u{7f}c").unwrap(), "a_b_c");
        // Trailing dots/spaces are trimmed; dot-only and empty stems fall back.
        assert_eq!(canonical_stem("报告. ").unwrap(), "报告");
        assert_eq!(canonical_stem("...").unwrap(), "document");
        assert_eq!(canonical_stem("..").unwrap(), "document");
        assert_eq!(canonical_stem(".").unwrap(), "document");
        assert_eq!(canonical_stem("").unwrap(), "document");
        // Windows device names stay rejected.
        for device in ["CON", "com1", "nul.txt", "PRN", "lpt9"] {
            assert!(canonical_stem(device).is_err(), "{device}");
        }
        // canonical_stem is idempotent for accepted stems (round-trip gate).
        for stem in ["文档《报告》", "报告", ".._office", "a_b_c", "日本語"] {
            let once = canonical_stem(stem).unwrap();
            assert_eq!(canonical_stem(&once).unwrap(), once);
        }
    }

    #[cfg(windows)]
    #[test]
    fn windows_root_anchors_are_separated_from_relative_traversal() {
        use super::windows_root_anchor;
        use std::path::PathBuf;

        assert_eq!(
            windows_root_anchor(Path::new(r"C:\output\nested")).unwrap(),
            (PathBuf::from(r"C:\"), 2)
        );
        assert_eq!(
            windows_root_anchor(Path::new(r"\\server\share\output")).unwrap(),
            (PathBuf::from(r"\\server\share\"), 2)
        );
        assert!(windows_root_anchor(Path::new(r"C:output")).is_err());
        assert!(windows_root_anchor(Path::new(r"\\.\C:\output")).is_err());
        assert!(windows_root_anchor(Path::new(r"\\?\C:\output")).is_err());
    }

    #[cfg(windows)]
    #[test]
    fn stage_begin_rejects_junction_in_root_ancestor() {
        let temp = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let junction = temp.path().join("junction");
        let status = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(&junction)
            .arg(outside.path())
            .status()
            .expect("run mklink");
        assert!(status.success(), "mklink /J failed: {status}");

        let rejected = match OfficialOutputStage::begin(
            &junction.join("nested/root"),
            "document",
            OfficialOutputTarget::Vlm,
            u64::MAX,
            u64::MAX,
            None,
        ) {
            Err(_) => true,
            Ok(stage) => {
                stage.cleanup();
                false
            }
        };
        assert!(
            !outside.path().join("nested").exists(),
            "output traversal mutated the junction target"
        );
        std::fs::remove_dir(&junction).expect("remove junction");
        assert!(rejected, "output root traversed a directory junction");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_tmp_alias_is_opened_through_private_tmp() {
        assert!(open_or_create_root(Path::new("/tmp")).is_ok());
    }

    #[test]
    fn install_restores_non_directory_target_when_backup_open_fails() {
        let temp = tempfile::tempdir().expect("temporary root");
        let tree = OutputTree::open(temp.path(), "document", OfficialOutputTarget::Vlm)
            .expect("output tree");
        let (parent_name, parent, stage) = create_private_stage(&tree.directory).expect("stage");
        stage.create("new").expect("stage file");
        tree.directory.create("vlm").expect("old target file");

        assert!(
            install_stage_in(&parent, "stage".as_ref(), &tree.directory, "vlm".as_ref()).is_err()
        );
        assert!(tree.directory.open("vlm").is_ok(), "old file was restored");
        drop(stage);
        remove_private_stage(&tree.directory, &parent_name, parent);
    }

    #[test]
    fn origin_budget_is_exact_and_rejects_plus_one_without_publication() {
        let temp = tempfile::tempdir().unwrap();
        let stage = OfficialOutputStage::begin(
            temp.path(),
            "document",
            OfficialOutputTarget::Vlm,
            3,
            u64::MAX,
            Some((Bytes::from_static(b"abc"), "png")),
        )
        .unwrap();
        assert_eq!(
            stage
                .stage
                .as_ref()
                .unwrap()
                .open("document_origin.png")
                .unwrap()
                .metadata()
                .unwrap()
                .len(),
            3
        );
        stage.cleanup();
        assert!(
            OfficialOutputStage::begin(
                temp.path(),
                "document",
                OfficialOutputTarget::Vlm,
                2,
                u64::MAX,
                Some((Bytes::from_static(b"abc"), "png"))
            )
            .is_err()
        );
        assert!(!temp.path().join("document/vlm").exists());
    }

    #[test]
    fn output_totals_remain_u64_across_files() {
        let temp = tempfile::tempdir().unwrap();
        let mut stage = OfficialOutputStage::begin(
            temp.path(),
            "document",
            OfficialOutputTarget::Vlm,
            u64::from(u32::MAX) + 2,
            u64::from(u32::MAX) + 2,
            None,
        )
        .unwrap();
        stage.asset_bytes = u64::from(u32::MAX);
        stage.write_origin(Bytes::from_static(b"a"), "one").unwrap();
        assert_eq!(stage.asset_bytes, u64::from(u32::MAX) + 1);
        assert!(matches!(
            stage.write_origin(Bytes::from_static(b"bc"), "two"),
            Err(VlmError::LimitExceeded {
                limit,
                actual,
                ..
            }) if limit == u64::from(u32::MAX) + 2 && actual == u64::from(u32::MAX) + 3
        ));
        stage.cleanup();
    }

    #[test]
    fn commit_replaces_same_target_without_private_artifacts() {
        let temp = tempfile::tempdir().unwrap();
        for contents in [b"first".as_slice(), b"second".as_slice()] {
            let mut stage = OfficialOutputStage::begin(
                temp.path(),
                "document",
                OfficialOutputTarget::Vlm,
                u64::MAX,
                u64::MAX,
                None,
            )
            .unwrap();
            stage.pages.push(0);
            stage.preview_pages.push(0);
            stage.preview_written = true;
            let parts = stage.parts().unwrap();
            for (suffix, bytes) in [
                ("middle.json", b"{}".as_slice()),
                ("model.json", b"{}".as_slice()),
                ("content.json", b"[]".as_slice()),
                ("v2.json", b"{}".as_slice()),
                ("markdown.md", contents),
            ] {
                super::write_bytes_in(parts, format!("{:020}-{suffix}", 0), bytes).unwrap();
            }
            super::write_bytes_in(stage.stage().unwrap(), "document_layout.pdf", b"pdf").unwrap();
            stage
                .assemble(std::time::Instant::now() + std::time::Duration::from_secs(5))
                .unwrap();
            assert!(stage.stage().unwrap().symlink_metadata("parts").is_err());
            let committed = stage.commit().unwrap();
            assert!(!committed.cleanup.failed());
            let manifest = committed.manifest;
            assert_eq!(
                std::fs::read(manifest.vlm_dir.join("document.md")).unwrap(),
                contents
            );
            assert!(!manifest.vlm_dir.join("parts").exists());
        }
        let document = temp.path().join("document");
        let private = std::fs::read_dir(&document)
            .unwrap()
            .filter_map(Result::ok)
            .map(|entry| entry.file_name())
            .filter(|name| name.to_string_lossy().starts_with(".vlm-"))
            .collect::<Vec<_>>();
        assert!(
            private.is_empty(),
            "private official-output artifacts remain: {private:?}"
        );
    }

    #[test]
    fn commit_keeps_publication_when_private_cleanup_fails() {
        let temp = tempfile::tempdir().unwrap();
        let stage = OfficialOutputStage::begin(
            temp.path(),
            "document",
            OfficialOutputTarget::Vlm,
            u64::MAX,
            u64::MAX,
            None,
        )
        .unwrap();
        stage.staging_parent().unwrap().create("marker").unwrap();
        stage.stage().unwrap().create("published").unwrap();

        let committed = stage.commit().unwrap();
        assert!(committed.cleanup.failed());
        assert!(committed.manifest.vlm_dir.join("published").is_file());
    }

    #[test]
    fn stage_begin_creates_a_missing_nested_root() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("missing/nested/root");
        let stage = OfficialOutputStage::begin(
            &root,
            "document",
            OfficialOutputTarget::Vlm,
            u64::MAX,
            u64::MAX,
            None,
        )
        .unwrap();

        assert!(root.join("document").is_dir());
        stage.cleanup();
    }

    #[cfg(unix)]
    #[test]
    fn stage_begin_rejects_symlinked_roots_and_stems() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let linked_root = temp.path().join("linked-root");
        symlink(outside.path(), &linked_root).unwrap();
        assert!(
            OfficialOutputStage::begin(
                &linked_root,
                "document",
                OfficialOutputTarget::Vlm,
                u64::MAX,
                u64::MAX,
                None,
            )
            .is_err()
        );

        let root = temp.path().join("root");
        std::fs::create_dir(&root).unwrap();
        symlink(outside.path(), root.join("document")).unwrap();
        assert!(
            OfficialOutputStage::begin(
                &root,
                "document",
                OfficialOutputTarget::Vlm,
                u64::MAX,
                u64::MAX,
                None,
            )
            .is_err()
        );
    }
}

struct CappedFile<F> {
    file: F,
    limit: usize,
    written: usize,
    attempted: usize,
}

impl<F> CappedFile<F> {
    fn new(file: F, limit: usize) -> Self {
        Self {
            file,
            limit,
            written: 0,
            attempted: 0,
        }
    }
}

impl<F: Write> Write for CappedFile<F> {
    fn write(&mut self, bytes: &[u8]) -> stdio::Result<usize> {
        self.attempted = self.attempted.saturating_add(bytes.len());
        if self.written.saturating_add(bytes.len()) > self.limit {
            return Err(stdio::Error::other("output limit"));
        }
        self.file.write_all(bytes)?;
        self.written = self.written.saturating_add(bytes.len());
        Ok(bytes.len())
    }

    fn flush(&mut self) -> stdio::Result<()> {
        self.file.flush()
    }
}
