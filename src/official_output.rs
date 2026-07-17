use crate::{
    Asset, AssetKind, OfficialDocument, OfficialOutputManifest, PageResult, VlmError, VlmResult,
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
    staging_parent: Dir,
    stage: Dir,
    parts: Dir,
    pages: Vec<usize>,
    preview_pages: Vec<usize>,
    assets: HashSet<PathBuf>,
    asset_bytes: usize,
    text_bytes: usize,
    max_asset_bytes: usize,
    max_text_bytes: usize,
    preview_written: bool,
    cleanup: Option<CleanupAdmission>,
}

impl OfficialOutputStage {
    pub(crate) fn begin(
        root: &Path,
        stem: &str,
        target_name: OfficialOutputTarget,
        max_asset_bytes: usize,
        max_text_bytes: usize,
        origin: Option<(Bytes, &'static str)>,
    ) -> VlmResult<Self> {
        let stem = canonical_stem(stem)?;
        let tree = OutputTree::open(root, &stem, target_name)?;
        let (staging_parent_name, staging_parent, stage) = create_private_stage(&tree.directory)?;
        if let Err(error) = stage.create_dir("parts").map_err(io) {
            remove_private_stage(&tree.directory, &staging_parent_name, &staging_parent);
            return Err(error);
        }
        let parts = match open_child_nofollow(stage.try_clone().map_err(io)?, "parts") {
            Ok(parts) => parts,
            Err(error) => {
                remove_private_stage(&tree.directory, &staging_parent_name, &staging_parent);
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
                remove_private_stage(&tree.directory, &staging_parent_name, &staging_parent);
                return Err(error);
            }
        };
        let mut output = Self {
            root: root.to_path_buf(),
            stem,
            target_name,
            target: tree.target,
            directory: tree.directory,
            staging_parent,
            stage,
            parts,
            pages: Vec::new(),
            preview_pages: Vec::new(),
            assets: HashSet::new(),
            asset_bytes: 0,
            text_bytes: 0,
            max_asset_bytes,
            max_text_bytes,
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
            &self.parts.try_clone().map_err(io)?,
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
        let prepared_bytes = self.pages.iter().try_fold(0usize, |total, page| {
            let bytes = usize::try_from(
                self.parts
                    .metadata(format!("{page:020}-prepared.json"))
                    .map_err(io)?
                    .len(),
            )
            .unwrap_or(usize::MAX);
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
                    self.parts
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
        let parts = self.parts.try_clone().map_err(io)?;
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
            &self.parts.try_clone().map_err(io)?,
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
                    self.parts
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

    pub(crate) fn remaining_asset_bytes(&self) -> usize {
        self.max_asset_bytes.saturating_sub(self.asset_bytes)
    }

    pub(crate) fn remaining_text_bytes(&self) -> usize {
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
        self.stage.remove_dir_all("parts").map_err(io)?;
        check_stage_deadline(deadline)?;
        Ok(())
    }

    pub(crate) fn prepare_commit(&mut self) -> VlmResult<()> {
        Ok(())
    }

    pub(crate) fn commit(mut self) -> VlmResult<OfficialOutputManifest> {
        let manifest = OfficialOutputManifest {
            root: self.root.clone(),
            stem: self.stem.clone(),
            vlm_dir: self.target.clone(),
        };
        let result = install_stage_in(
            &self.staging_parent,
            std::ffi::OsStr::new("stage"),
            &self.directory,
            std::ffi::OsStr::new(self.target_name.name()),
        );
        match result {
            Ok(backup) => {
                let mut cleanup = self
                    .cleanup
                    .take()
                    .expect("cleanup admitted before stage live");
                cleanup.job.backup = backup;
                cleanup.detach(false);
                Ok(manifest)
            }
            Err(error) => {
                self.cleanup();
                Err(error)
            }
        }
    }

    /// Consumes the stage so cleanup can be scheduled outside an async executor.
    pub(crate) fn cleanup(mut self) {
        self.cleanup_now();
    }

    fn cleanup_now(&mut self) {
        if let Some(cleanup) = self.cleanup.take() {
            cleanup.detach(true);
        }
    }

    fn write_asset(&mut self, asset: &Asset) -> VlmResult<()> {
        let path = if matches!(&asset.kind, AssetKind::Other(kind) if kind == "layout_preview") {
            PathBuf::from(format!("{}_layout.pdf", self.stem))
        } else {
            validate_asset(asset, &self.stem)?
        };
        if !self.assets.insert(path.clone()) {
            let parent =
                open_or_create_relative(&self.stage, path.parent().expect("asset parent"))?;
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
        let next = self
            .asset_bytes
            .checked_add(asset.data.len())
            .unwrap_or(usize::MAX);
        if next > self.max_asset_bytes {
            return Err(VlmError::LimitExceeded {
                resource: "total asset bytes",
                limit: self.max_asset_bytes as u64,
                actual: next as u64,
            });
        }
        let parent = open_or_create_relative(&self.stage, path.parent().expect("asset parent"))?;
        write_bytes_in(&parent, path.file_name().expect("asset name"), &asset.data)?;
        self.asset_bytes = next;
        Ok(())
    }

    fn write_origin(&mut self, origin: Bytes, suffix: &'static str) -> VlmResult<()> {
        if origin.is_empty() {
            return invalid("origin must not be empty");
        }
        let next = self
            .asset_bytes
            .checked_add(origin.len())
            .unwrap_or(usize::MAX);
        if next > self.max_asset_bytes {
            return Err(VlmError::LimitExceeded {
                resource: "total asset bytes",
                limit: self.max_asset_bytes as u64,
                actual: next as u64,
            });
        }
        write_bytes_in(
            &self.stage,
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
            self.remaining_text_bytes(),
        );
        if let Err(error) = serde_json::to_writer(&mut output, value) {
            if output.attempted > output.limit {
                return Err(VlmError::LimitExceeded {
                    resource: "staged text/JSON bytes",
                    limit: self.max_text_bytes as u64,
                    actual: self.text_bytes.saturating_add(output.attempted) as u64,
                });
            }
            return Err(VlmError::Protocol {
                operation: "official output serialization",
                message: error.to_string(),
            });
        }
        self.charge_text(output.written)
    }

    fn write_text_in(
        &mut self,
        directory: &Dir,
        name: impl AsRef<Path>,
        bytes: &[u8],
    ) -> VlmResult<()> {
        self.charge_text(bytes.len())?;
        write_bytes_in(directory, name, bytes)
    }

    fn append_part<W: Write>(&mut self, output: &mut W, name: impl AsRef<Path>) -> VlmResult<()> {
        let bytes = self.parts.metadata(name.as_ref()).map_err(io)?.len();
        let bytes = usize::try_from(bytes).unwrap_or(usize::MAX);
        self.charge_text(bytes)?;
        stdio::copy(&mut self.parts.open(name).map_err(io)?, output).map_err(io)?;
        Ok(())
    }

    fn charge_text(&mut self, bytes: usize) -> VlmResult<()> {
        let next = self.text_bytes.checked_add(bytes).unwrap_or(usize::MAX);
        if next > self.max_text_bytes {
            return Err(VlmError::LimitExceeded {
                resource: "staged text/JSON bytes",
                limit: self.max_text_bytes as u64,
                actual: next as u64,
            });
        }
        self.text_bytes = next;
        Ok(())
    }

    fn write_final_outputs(&mut self, deadline: std::time::Instant) -> VlmResult<()> {
        let mut middle = self
            .stage
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
            .stage
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
            .stage
            .create(format!("{}_content_list.json", self.stem))
            .map_err(io)?;
        self.write_fragment(&mut content, b"[")?;
        let mut content_written = false;
        for page in self.pages.clone() {
            check_stage_deadline(deadline)?;
            let mut source = self
                .parts
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
                self.charge_text(inner)?;
                source.seek(SeekFrom::Start(1)).map_err(io)?;
                stdio::copy(&mut source.take(inner as u64), &mut content).map_err(io)?;
                content_written = true;
            }
        }
        self.write_fragment(&mut content, b"]")?;

        let mut v2 = self
            .stage
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

        let mut markdown = self.stage.create(format!("{}.md", self.stem)).map_err(io)?;
        let mut markdown_written = false;
        for page in self.pages.clone() {
            check_stage_deadline(deadline)?;
            let name = format!("{page:020}-markdown.md");
            let bytes = usize::try_from(self.parts.metadata(&name).map_err(io)?.len())
                .unwrap_or(usize::MAX);
            if bytes == 0 {
                continue;
            }
            if markdown_written {
                self.write_fragment(&mut markdown, b"\n\n")?;
            }
            self.charge_text(bytes)?;
            stdio::copy(&mut self.parts.open(name).map_err(io)?, &mut markdown).map_err(io)?;
            markdown_written = true;
        }
        Ok(())
    }

    fn write_fragment<W: Write>(&mut self, output: &mut W, bytes: &[u8]) -> VlmResult<()> {
        self.charge_text(bytes.len())?;
        output.write_all(bytes).map_err(io)
    }
}

impl Drop for OfficialOutputStage {
    fn drop(&mut self) {
        if let Some(cleanup) = self.cleanup.take() {
            cleanup.detach(false);
        }
    }
}

pub fn write_official_outputs(
    root: &Path,
    stem: &str,
    document: &OfficialDocument,
) -> VlmResult<OfficialOutputManifest> {
    let stem = canonical_stem(stem)?;
    let assets = validate_assets(&document.document.assets, &stem)?;
    let model = crate::vlm_types::model_output_wire(&document.model_output)?;

    let tree = OutputTree::open(root, &stem, OfficialOutputTarget::Vlm)?;
    let (staging_parent_name, staging_parent, stage) = create_private_stage(&tree.directory)?;
    let result = write_stage_in(&stage, &stem, document, &model, &assets);
    if let Err(error) = result {
        remove_private_stage(&tree.directory, &staging_parent_name, &staging_parent);
        return Err(error);
    }

    let backup = match install_stage_in(
        &staging_parent,
        std::ffi::OsStr::new("stage"),
        &tree.directory,
        std::ffi::OsStr::new(OfficialOutputTarget::Vlm.name()),
    ) {
        Ok(backup) => backup,
        Err(error) => {
            remove_private_stage(&tree.directory, &staging_parent_name, &staging_parent);
            return Err(error);
        }
    };
    if let Some(backup) = backup {
        remove_backup(backup);
    }
    remove_private_stage(&tree.directory, &staging_parent_name, &staging_parent);
    Ok(OfficialOutputManifest {
        root: root.to_path_buf(),
        stem,
        vlm_dir: tree.target,
    })
}

fn write_stage_in(
    directory: &Dir,
    stem: &str,
    document: &OfficialDocument,
    model: &serde_json::Value,
    assets: &[(PathBuf, &Asset)],
) -> VlmResult<()> {
    write_json_in(
        directory,
        &format!("{stem}_middle.json"),
        &document.document.middle_json,
    )?;
    write_json_in(directory, &format!("{stem}_model.json"), model)?;
    write_json_in(
        directory,
        &format!("{stem}_content_list.json"),
        &document.document.content_list,
    )?;
    write_json_in(
        directory,
        &format!("{stem}_content_list_v2.json"),
        &document.content_list_v2,
    )?;
    write_bytes_in(
        directory,
        &format!("{stem}.md"),
        document.document.markdown.as_bytes(),
    )?;
    for (path, asset) in assets {
        let parent = open_or_create_relative(
            directory,
            path.parent().expect("validated asset path has parent"),
        )?;
        write_bytes_in(
            &parent,
            path.file_name().expect("validated asset has a file name"),
            &asset.data,
        )?;
    }
    let preview = document
        .document
        .assets
        .iter()
        .find(|asset| matches!(&asset.kind, AssetKind::Other(kind) if kind == "layout_preview"))
        .expect("validated preview");
    write_bytes_in(directory, &format!("{stem}_layout.pdf"), &preview.data)
}

fn write_json_in(directory: &Dir, name: &str, value: &serde_json::Value) -> VlmResult<()> {
    let bytes = serde_json::to_vec(value).map_err(|error| VlmError::Protocol {
        operation: "official output serialization",
        message: error.to_string(),
    })?;
    write_bytes_in(directory, name, &bytes)
}

fn write_bytes_in(directory: &Dir, name: impl AsRef<Path>, bytes: &[u8]) -> VlmResult<()> {
    let mut file = directory.create(name).map_err(io)?;
    file.write_all(bytes).map_err(io)
}

fn validate_assets<'a>(assets: &'a [Asset], stem: &str) -> VlmResult<Vec<(PathBuf, &'a Asset)>> {
    let mut paths = HashSet::new();
    let mut output = Vec::new();
    let mut previews = 0;
    let reserved = [
        format!("{stem}_middle.json"),
        format!("{stem}_model.json"),
        format!("{stem}_content_list.json"),
        format!("{stem}_content_list_v2.json"),
        format!("{stem}.md"),
        format!("{stem}_layout.pdf"),
    ];
    for asset in assets {
        if matches!(&asset.kind, AssetKind::Other(kind) if kind == "layout_preview") {
            previews += 1;
            if asset.media_type != "application/pdf" || asset.data.is_empty() {
                return invalid("layout preview must be a nonempty application/pdf asset");
            }
            continue;
        }
        let path = validate_asset(asset, stem)?;
        let alias = path.to_string_lossy().to_ascii_lowercase();
        if reserved.iter().any(|name| alias.eq_ignore_ascii_case(name))
            || !paths.insert(PathBuf::from(alias))
        {
            return invalid("official assets must have unique normalized images/... paths");
        }
        output.push((path, asset));
    }
    if previews != 1 {
        return invalid("exactly one layout preview asset is required");
    }
    Ok(output)
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
    let stem = if stem.is_empty() {
        "document".to_owned()
    } else {
        stem
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
        && name.is_ascii()
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b' ' | b'.' | b'_' | b'-'))
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
    let start = if path.is_absolute() {
        Path::new("/")
    } else {
        Path::new(".")
    };
    let mut dir = Dir::from_std_file(open_ambient_dir(start, ambient_authority()).map_err(io)?);
    let components: Vec<_> = path.components().collect();
    let macos_private_alias = cfg!(target_os = "macos")
        && matches!(components.as_slice(), [Component::RootDir, Component::Normal(name), ..] if *name == "var" || *name == "tmp");
    if macos_private_alias {
        dir = open_child_nofollow(dir, "private")?;
    }
    for (index, component) in components.into_iter().enumerate() {
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
        remove_private_stage(directory, &parent_name, &parent);
        return Err(error);
    }
    let stage = match parent
        .try_clone()
        .map_err(io)
        .and_then(|parent| open_child_nofollow(parent, "stage"))
    {
        Ok(stage) => stage,
        Err(error) => {
            remove_private_stage(directory, &parent_name, &parent);
            return Err(error);
        }
    };
    Ok((parent_name, parent, stage))
}

fn remove_private_stage(directory: &Dir, parent_name: &std::ffi::OsStr, parent: &Dir) {
    // The parent handle reaches only our private staging tree. The outer removal is
    // descriptor-relative and deliberately non-recursive.
    let _ = parent.remove_dir_all("stage");
    let _ = directory.remove_dir(parent_name);
}

struct CleanupAdmission {
    sender: std::sync::mpsc::SyncSender<CleanupJob>,
    done: std::sync::mpsc::Receiver<()>,
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
                    cleanup.run();
                    let _ = done_sender.send(());
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

    fn detach(self, wait: bool) {
        // The worker was admitted before publication and is blocked in recv, so this cannot
        // silently lose the cleanup capability. If it died, retain no leak by cleaning with the
        // same no-follow capabilities in this caller.
        if let Err(error) = self.sender.send(self.job) {
            error.0.run();
        } else if wait {
            let _ = self.done.recv();
        }
    }
}

impl CleanupJob {
    fn run(self) {
        if let Some(backup) = self.backup {
            remove_backup(backup);
        }
        remove_private_stage(
            &self.directory,
            &self.staging_parent_name,
            &self.staging_parent,
        );
    }
}

struct Backup {
    name: std::ffi::OsString,
    parent: Dir,
    directory: Dir,
}

fn remove_backup(backup: Backup) {
    // The backup was opened no-follow before publication and lives under our private staging
    // parent. Never recurse through its former public pathname.
    if let Ok(entries) = backup.directory.read_dir(".") {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let _ = backup.directory.remove_dir_all(&name);
            let _ = backup.directory.remove_file(name);
        }
    }
    // `name` is private to the staging parent, whose retained capability removes only this
    // original entry after its opened directory has been emptied.
    let _ = backup.parent.remove_dir(&backup.name);
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
        if let Some(backup) = &backup
            && let Err(rollback_error) =
                stage_parent_handle.rename(&backup.name, directory_handle, target_name)
        {
            return Err(VlmError::Io {
                operation: "official output rollback",
                message: format!(
                    "install failed: {install_error}; restoring previous vlm failed: {rollback_error}"
                ),
            });
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
        OfficialOutputStage, OfficialOutputTarget, OutputTree, create_private_stage,
        install_stage_in, open_or_create_root, remove_private_stage,
    };
    use bytes::Bytes;
    use std::path::Path;

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
        remove_private_stage(&tree.directory, &parent_name, &parent);
    }

    #[test]
    fn origin_budget_is_exact_and_rejects_plus_one_without_publication() {
        let temp = tempfile::tempdir().unwrap();
        let stage = OfficialOutputStage::begin(
            temp.path(),
            "document",
            OfficialOutputTarget::Vlm,
            3,
            usize::MAX,
            Some((Bytes::from_static(b"abc"), "png")),
        )
        .unwrap();
        assert_eq!(
            stage
                .stage
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
                usize::MAX,
                Some((Bytes::from_static(b"abc"), "png"))
            )
            .is_err()
        );
        assert!(!temp.path().join("document/vlm").exists());
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
