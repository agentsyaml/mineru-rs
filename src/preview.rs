use crate::{Asset, AssetKind, BlockKind, ContentBlock, Error, Limits, PageResult, Result};
use bytes::Bytes;
use lopdf::{Dictionary, Document, Object, Stream, dictionary};
use std::{
    collections::HashSet,
    io::{self, Write},
    path::PathBuf,
};

const PREVIEW_KIND: &str = "layout_preview";

pub(crate) fn generate(
    source: &[u8],
    pages: &[PageResult],
    stem: &str,
    limits: &Limits,
    remaining: usize,
) -> Result<Asset> {
    generate_inner(source, pages, stem, limits, remaining, None)
}

pub(crate) fn generate_until(
    source: &[u8],
    pages: &[PageResult],
    stem: &str,
    limits: &Limits,
    remaining: usize,
    deadline: std::time::Instant,
) -> Result<Asset> {
    generate_inner(source, pages, stem, limits, remaining, Some(deadline))
}

pub(crate) fn generate_selected_until(
    selected: crate::pdf::SelectedPreview,
    pages: &[PageResult],
    stem: &str,
    limits: &Limits,
    remaining: usize,
    deadline: std::time::Instant,
) -> Result<Asset> {
    generate_selected_inner(selected, Some(pages), stem, limits, remaining, deadline)
}

fn generate_selected_inner(
    selected: crate::pdf::SelectedPreview,
    pages: Option<&[PageResult]>,
    stem: &str,
    limits: &Limits,
    remaining: usize,
    deadline: std::time::Instant,
) -> Result<Asset> {
    if !safe_stem(stem) {
        return Err(Error::InvalidInput("unsafe preview file stem".into()));
    }
    check_deadline(Some(deadline))?;
    let cap = remaining.min(limits.max_total_asset_bytes);
    if selected.bytes.len() > cap {
        return Err(overflow(cap, selected.bytes.len()));
    }
    if selected.page_indices.len() != selected.user_units.len()
        || pages.is_some_and(|pages| pages.len() != selected.page_indices.len())
    {
        return Err(Error::InvalidInput(
            "preview page/result count mismatch".into(),
        ));
    }
    if pages.is_some_and(|pages| {
        selected
            .page_indices
            .iter()
            .zip(pages)
            .any(|(index, page)| *index != page.page_index)
    }) {
        return Err(Error::InvalidInput("preview page order mismatch".into()));
    }
    let mut doc = Document::load_mem(&selected.bytes)
        .map_err(|e| Error::Pdf(format!("cannot create layout preview: {e}")))?;
    if doc.is_encrypted() {
        return Err(Error::Pdf("encrypted PDFs are unsupported".into()));
    }
    let selected_pages = doc.get_pages();
    if selected_pages.len() != selected.page_indices.len() {
        return Err(Error::Pdf("selected PDF page count mismatch".into()));
    }
    let mut overlay_remaining = cap;
    for (position, user_unit) in selected.user_units.into_iter().enumerate() {
        check_deadline(Some(deadline))?;
        let id = *selected_pages
            .get(&((position + 1) as u32))
            .ok_or_else(|| Error::Pdf("selected PDF page order mismatch".into()))?;
        if let Some(user_unit) = user_unit {
            doc.get_object_mut(id)
                .map_err(|e| Error::Pdf(e.to_string()))?
                .as_dict_mut()
                .map_err(|e| Error::Pdf(e.to_string()))?
                .set("UserUnit", user_unit);
        }
        if let Some(page) = pages.and_then(|pages| pages.get(position)) {
            append_page(&mut doc, id, page, &mut overlay_remaining, Some(deadline))?;
        }
    }
    check_deadline(Some(deadline))?;
    doc.prune_objects();
    check_deadline(Some(deadline))?;
    let mut out = CappedWriter::new(cap);
    doc.save_to(&mut out)
        .map_err(|e| limit_or_pdf(e, out.limit, out.actual()))?;
    let data = out.into_inner();
    Ok(Asset {
        kind: AssetKind::Other(PREVIEW_KIND.into()),
        relative_path: PathBuf::from(format!("{stem}_layout.pdf")),
        media_type: "application/pdf".into(),
        md5: format!("{:x}", md5::compute(&data)),
        data: Bytes::from(data),
    })
}

fn generate_inner(
    source: &[u8],
    pages: &[PageResult],
    stem: &str,
    limits: &Limits,
    remaining: usize,
    deadline: Option<std::time::Instant>,
) -> Result<Asset> {
    if !safe_stem(stem) {
        return Err(Error::InvalidInput("unsafe preview file stem".into()));
    }
    check_deadline(deadline)?;
    if source.len() > remaining.min(limits.max_total_asset_bytes) {
        return Err(overflow(
            remaining.min(limits.max_total_asset_bytes),
            source.len(),
        ));
    }
    let mut doc = Document::load_mem(source)
        .map_err(|e| Error::Pdf(format!("cannot create layout preview: {e}")))?;
    if doc.is_encrypted() {
        return Err(Error::Pdf("encrypted PDFs are unsupported".into()));
    }
    let page_ids = doc.get_pages();
    let mut overlay_remaining = remaining.min(limits.max_total_asset_bytes);
    for page in pages {
        check_deadline(deadline)?;
        let id = *page_ids
            .get(&((page.page_index + 1) as u32))
            .ok_or_else(|| {
                Error::Pdf(format!(
                    "preview page {} is outside the PDF",
                    page.page_index
                ))
            })?;
        append_page(&mut doc, id, page, &mut overlay_remaining, deadline)?;
    }
    let selected: HashSet<u32> = pages
        .iter()
        .map(|page| page.page_index as u32 + 1)
        .collect();
    let removed: Vec<u32> = page_ids
        .keys()
        .copied()
        .filter(|page| !selected.contains(page))
        .collect();
    doc.delete_pages(&removed);
    // delete_pages updates the page tree but retains unrelated indirect streams.
    // Pruning prevents a selected-page preview from serializing hidden source content.
    doc.prune_objects();
    check_deadline(deadline)?;
    let mut out = CappedWriter::new(remaining.min(limits.max_total_asset_bytes));
    doc.save_to(&mut out)
        .map_err(|e| limit_or_pdf(e, out.limit, out.actual()))?;
    let data = out.into_inner();
    Ok(Asset {
        kind: AssetKind::Other(PREVIEW_KIND.into()),
        relative_path: PathBuf::from(format!("{stem}_layout.pdf")),
        media_type: "application/pdf".into(),
        md5: format!("{:x}", md5::compute(&data)),
        data: Bytes::from(data),
    })
}

fn limit_or_pdf(e: io::Error, limit: usize, actual: usize) -> Error {
    if actual > limit {
        Error::LimitExceeded {
            resource: "total asset bytes",
            limit: limit as u64,
            actual: actual as u64,
        }
    } else {
        Error::Pdf(format!("cannot serialize layout preview: {e}"))
    }
}

fn append_page(
    doc: &mut Document,
    id: lopdf::ObjectId,
    page: &PageResult,
    overlay_remaining: &mut usize,
    deadline: Option<std::time::Instant>,
) -> Result<()> {
    let media = resolve_object(
        doc,
        &inherited(doc, id, b"MediaBox")?
            .ok_or_else(|| Error::Pdf("page has no MediaBox".into()))?,
    )?;
    let crop = match inherited(doc, id, b"CropBox")? {
        Some(crop) => resolve_object(doc, &crop)?,
        None => media.clone(),
    };
    let visible = intersection(rect(doc, &media)?, rect(doc, &crop)?)
        .ok_or_else(|| Error::Pdf("CropBox does not intersect MediaBox".into()))?;
    if let Some(unit) = inherited(doc, id, b"UserUnit")? {
        let unit = number(doc, &unit)?;
        if !unit.is_finite() || unit <= 0.0 {
            return Err(Error::Pdf("invalid UserUnit".into()));
        }
    }
    let rotation = inherited(doc, id, b"Rotate")?
        .and_then(|v| resolve_object(doc, &v).ok())
        .and_then(|v| v.as_i64().ok())
        .unwrap_or(0)
        .rem_euclid(360) as i32;
    if !matches!(rotation, 0 | 90 | 180 | 270) {
        return Err(Error::Pdf("unsupported page rotation".into()));
    }
    // Contents is not inheritable: only the selected page's local stream(s) survive.
    let old_contents = local_contents(doc, id)?;
    validate_contents(doc, old_contents.as_ref())?;
    let mut resources = inherited(doc, id, b"Resources")?
        .map(|v| resource_dict(doc, v))
        .transpose()?
        .unwrap_or_default();
    let mut gs = subresource(doc, &resources, b"ExtGState")?;
    let alpha = unique(&gs, "MinerUPreviewAlpha");
    gs.set(alpha.clone(), dictionary! { "ca" => 0.3 });
    resources.set("ExtGState", gs);
    let mut fonts = subresource(doc, &resources, b"Font")?;
    let font = unique(&fonts, "MinerUPreviewHelvetica");
    let font_id = doc.add_object(
        dictionary! { "Type" => "Font", "Subtype" => "Type1", "BaseFont" => "Helvetica" },
    );
    fonts.set(font.clone(), font_id);
    resources.set("Font", fonts);
    let content = overlay(
        page,
        visible,
        rotation,
        &alpha,
        &font,
        *overlay_remaining,
        deadline,
    )?;
    *overlay_remaining = overlay_remaining.saturating_sub(content.len());
    let stream = doc.add_object(Stream::new(Dictionary::new(), content));
    let contents = match old_contents {
        Some(old) => match resolve_object(doc, &old)? {
            Object::Stream(_) => Object::Array(vec![old, stream.into()]),
            Object::Array(mut values) => {
                values.push(stream.into());
                Object::Array(values)
            }
            _ => {
                return Err(Error::Pdf(
                    "page Contents must be a stream or stream array".into(),
                ));
            }
        },
        None => stream.into(),
    };
    let dict = doc
        .get_object_mut(id)
        .map_err(|e| Error::Pdf(e.to_string()))?
        .as_dict_mut()
        .map_err(|e| Error::Pdf(e.to_string()))?;
    dict.set("Resources", resources); // local copy: inherited resources are never mutated
    dict.set("Contents", contents);
    Ok(())
}

fn validate_contents(doc: &Document, contents: Option<&Object>) -> Result<()> {
    match contents
        .map(|contents| resolve_object(doc, contents))
        .transpose()?
    {
        None | Some(Object::Stream(_)) => Ok(()),
        Some(Object::Array(values)) => {
            for value in &values {
                if !matches!(resolve_object(doc, value)?, Object::Stream(_)) {
                    return Err(Error::Pdf(
                        "page Contents array contains a non-stream".into(),
                    ));
                }
            }
            Ok(())
        }
        Some(_) => Err(Error::Pdf(
            "page Contents must be a stream or stream array".into(),
        )),
    }
}

fn local_contents(doc: &Document, id: lopdf::ObjectId) -> Result<Option<Object>> {
    Ok(doc
        .get_object(id)
        .map_err(|error| Error::Pdf(error.to_string()))?
        .as_dict()
        .map_err(|error| Error::Pdf(error.to_string()))?
        .get(b"Contents")
        .ok()
        .cloned())
}

fn overlay(
    page: &PageResult,
    b: [f32; 4],
    rotation: i32,
    alpha: &str,
    font: &str,
    limit: usize,
    deadline: Option<std::time::Instant>,
) -> Result<Vec<u8>> {
    let mut out = CappedWriter::new(limit);
    write!(
        &mut out,
        "q\n{} {} {} {} re W n\n",
        n(b[0]),
        n(b[1]),
        n(b[2] - b[0]),
        n(b[3] - b[1]),
    )
    .map_err(|_| overflow(limit, out.actual()))?;
    for (i, block) in page.blocks.iter().enumerate() {
        check_deadline(deadline)?;
        writeln!(&mut out, "q /{alpha} gs").map_err(|_| overflow(limit, out.actual()))?;
        let p = [
            point(block.bbox.left, block.bbox.top, b, rotation),
            point(block.bbox.right, block.bbox.top, b, rotation),
            point(block.bbox.right, block.bbox.bottom, b, rotation),
            point(block.bbox.left, block.bbox.bottom, b, rotation),
        ];
        let c = color(block);
        write!(
            &mut out,
            "{} {} {} rg\n{} {} m {} {} l {} {} l {} {} l h f\n",
            n(c.0),
            n(c.1),
            n(c.2),
            n(p[0].0),
            n(p[0].1),
            n(p[1].0),
            n(p[1].1),
            n(p[2].0),
            n(p[2].1),
            n(p[3].0),
            n(p[3].1)
        )
        .map_err(|_| overflow(limit, out.actual()))?;
        out.write_all(b"Q\n")
            .map_err(|_| overflow(limit, out.actual()))?;
        let x = p.iter().map(|p| p.0).fold(f32::INFINITY, f32::min);
        let y = p.iter().map(|p| p.1).fold(f32::INFINITY, f32::min);
        let w = p.iter().map(|p| p.0).fold(f32::NEG_INFINITY, f32::max) - x;
        let h = p.iter().map(|p| p.1).fold(f32::NEG_INFINITY, f32::max) - y;
        let (a, bm, c, d, tx, ty) = match rotation {
            0 => (1, 0, 0, 1, x + w + 2.0, y + h - 10.0),
            90 => (0, 1, -1, 0, x + 10.0, y + h + 2.0),
            180 => (-1, 0, 0, -1, x - 2.0, y + 10.0),
            _ => (0, -1, 1, 0, x + w - 10.0, y - 2.0),
        };
        // The fill's graphics state was restored above; labels are opaque red.
        writeln!(
            &mut out,
            "BT /{} 10 Tf {} {} {} {} {} {} Tm 1 0 0 rg ({}) Tj ET",
            font,
            a,
            bm,
            c,
            d,
            n(tx),
            n(ty),
            i + 1
        )
        .map_err(|_| overflow(limit, out.actual()))?;
    }
    out.write_all(b"Q\n")
        .map_err(|_| overflow(limit, out.actual()))?;
    Ok(out.into_inner())
}

fn inherited(doc: &Document, mut id: lopdf::ObjectId, key: &[u8]) -> Result<Option<Object>> {
    let mut seen = HashSet::new();
    loop {
        if !seen.insert(id) {
            return Err(Error::Pdf("cyclic page parent".into()));
        }
        let d = doc
            .get_object(id)
            .map_err(|error| Error::Pdf(error.to_string()))?
            .as_dict()
            .map_err(|error| Error::Pdf(error.to_string()))?;
        if let Ok(v) = d.get(key) {
            return Ok(Some(v.clone()));
        }
        id = match d.get(b"Parent").ok() {
            Some(Object::Reference(p)) => *p,
            _ => return Ok(None),
        }
    }
}

fn check_deadline(deadline: Option<std::time::Instant>) -> Result<()> {
    if deadline.is_some_and(|deadline| std::time::Instant::now() >= deadline) {
        Err(Error::Timeout {
            operation: "official PDF",
        })
    } else {
        Ok(())
    }
}
fn resource_dict(doc: &Document, o: Object) -> Result<Dictionary> {
    let o = resolve_object(doc, &o)?;
    o.as_dict().cloned().map_err(|e| Error::Pdf(e.to_string()))
}
fn resolve_object(doc: &Document, o: &Object) -> Result<Object> {
    let mut o = o.clone();
    let mut seen = HashSet::new();
    while let Object::Reference(id) = o {
        if !seen.insert(id) {
            return Err(Error::Pdf("cyclic indirect object".into()));
        }
        o = doc
            .get_object(id)
            .map_err(|e| Error::Pdf(e.to_string()))?
            .clone();
    }
    Ok(o)
}
fn subresource(doc: &Document, r: &Dictionary, k: &[u8]) -> Result<Dictionary> {
    r.get(k)
        .ok()
        .cloned()
        .map(|o| resource_dict(doc, o))
        .transpose()
        .map(|v| v.unwrap_or_default())
}
fn unique(d: &Dictionary, base: &str) -> String {
    let mut n = base.to_owned();
    let mut i = 1;
    while d.get(n.as_bytes()).is_ok() {
        n = format!("{base}{i}");
        i += 1
    }
    n
}
fn number(doc: &Document, o: &Object) -> Result<f32> {
    let o = resolve_object(doc, o)?;
    o.as_f32()
        .or_else(|_| o.as_i64().map(|v| v as f32))
        .map_err(|_| Error::Pdf("invalid page number".into()))
}
fn rect(doc: &Document, o: &Object) -> Result<[f32; 4]> {
    let a = o
        .as_array()
        .map_err(|_| Error::Pdf("invalid page box".into()))?;
    if a.len() != 4 {
        return Err(Error::Pdf("invalid page box".into()));
    }
    let r = [
        number(doc, &a[0])?,
        number(doc, &a[1])?,
        number(doc, &a[2])?,
        number(doc, &a[3])?,
    ];
    if !r.iter().all(|x| x.is_finite()) || r[0] >= r[2] || r[1] >= r[3] {
        return Err(Error::Pdf("invalid page box".into()));
    }
    Ok(r)
}
fn intersection(a: [f32; 4], b: [f32; 4]) -> Option<[f32; 4]> {
    let r = [
        a[0].max(b[0]),
        a[1].max(b[1]),
        a[2].min(b[2]),
        a[3].min(b[3]),
    ];
    (r[0] < r[2] && r[1] < r[3]).then_some(r)
}
fn point(x: f32, y: f32, b: [f32; 4], r: i32) -> (f32, f32) {
    let (w, h) = (b[2] - b[0], b[3] - b[1]);
    match r {
        0 => (b[0] + x * w, b[3] - y * h),
        90 => (b[0] + y * w, b[1] + x * h),
        180 => (b[2] - x * w, b[1] + y * h),
        _ => (b[0] + (1.0 - y) * w, b[1] + (1.0 - x) * h),
    }
}
fn color(b: &ContentBlock) -> (f32, f32, f32) {
    let k = b.kind.as_str();
    match k {
        BlockKind::HEADER
        | BlockKind::FOOTER
        | BlockKind::PAGE_NUMBER
        | BlockKind::ASIDE_TEXT
        | BlockKind::PAGE_FOOTNOTE => (158. / 255., 158. / 255., 158. / 255.),
        BlockKind::CODE | BlockKind::ALGORITHM => (102. / 255., 0., 204. / 255.),
        BlockKind::CODE_CAPTION => (204. / 255., 153. / 255., 1.),
        BlockKind::TABLE => (204. / 255., 204. / 255., 0.),
        BlockKind::TABLE_CAPTION => (1., 1., 102. / 255.),
        BlockKind::TABLE_FOOTNOTE => (229. / 255., 1., 204. / 255.),
        BlockKind::IMAGE | BlockKind::IMAGE_BLOCK | BlockKind::CHART => {
            (153. / 255., 1., 51. / 255.)
        }
        BlockKind::IMAGE_CAPTION => (102. / 255., 178. / 255., 1.),
        BlockKind::IMAGE_FOOTNOTE => (1., 178. / 255., 102. / 255.),
        BlockKind::TITLE => (102. / 255., 102. / 255., 1.),
        BlockKind::EQUATION | BlockKind::EQUATION_BLOCK => (0., 1., 0.),
        BlockKind::LIST | BlockKind::LIST_ITEM | BlockKind::INDEX => {
            (40. / 255., 169. / 255., 92. / 255.)
        }
        _ => (153. / 255., 0., 76. / 255.),
    }
}
fn n(v: f32) -> String {
    format!("{v:.3}")
}
fn safe_stem(s: &str) -> bool {
    !s.is_empty() && !s.contains(['/', '\\', '\0']) && s != "." && s != ".."
}
fn overflow(limit: usize, actual: usize) -> Error {
    Error::LimitExceeded {
        resource: "total asset bytes",
        limit: limit as u64,
        actual: actual as u64,
    }
}
struct CappedWriter {
    data: Vec<u8>,
    limit: usize,
    attempted: usize,
}
impl CappedWriter {
    fn new(limit: usize) -> Self {
        Self {
            data: Vec::new(),
            limit,
            attempted: 0,
        }
    }
    fn actual(&self) -> usize {
        self.attempted
    }
    fn into_inner(self) -> Vec<u8> {
        self.data
    }
}
impl Write for CappedWriter {
    fn write(&mut self, b: &[u8]) -> io::Result<usize> {
        self.attempted = self.attempted.saturating_add(b.len());
        let next = self
            .data
            .len()
            .checked_add(b.len())
            .ok_or_else(|| io::Error::other("limit"))?;
        if next > self.limit {
            return Err(io::Error::other("limit"));
        }
        self.data.extend_from_slice(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{BlockKind, ContentBlock, NormalizedBbox, pdf};
    use lopdf::Object;
    use serde_json::Map;

    fn source(rotation: i64, user_unit: Option<f32>) -> Vec<u8> {
        let mut doc = Document::with_version("1.5");
        let pages = doc.new_object_id();
        let page = doc.new_object_id();
        let mut resources = dictionary! {
            "ExtGState" => dictionary! { "MinerUPreviewAlpha" => dictionary! {} },
            "Font" => dictionary! { "MinerUPreviewHelvetica" => dictionary! {} },
        };
        resources.set("ProcSet", vec![Object::Name(b"PDF".to_vec())]);
        let mut page_dict = dictionary! {
            "Type" => "Page", "Parent" => pages, "MediaBox" => vec![0.into(), 0.into(), 200.into(), 300.into()],
            "CropBox" => vec![10.into(), 20.into(), 110.into(), 220.into()], "Rotate" => rotation,
            "Resources" => resources,
        };
        if let Some(unit) = user_unit {
            page_dict.set("UserUnit", unit);
        }
        doc.objects.insert(page, page_dict.into());
        doc.objects.insert(
            pages,
            dictionary! { "Type" => "Pages", "Kids" => vec![page.into()], "Count" => 1 }.into(),
        );
        let catalog = doc.add_object(dictionary! { "Type" => "Catalog", "Pages" => pages });
        doc.trailer.set("Root", catalog);
        let mut bytes = Vec::new();
        doc.save_to(&mut bytes).unwrap();
        bytes
    }

    fn two_page_source() -> Vec<u8> {
        let mut doc = Document::with_version("1.5");
        let pages = doc.new_object_id();
        let first = doc.new_object_id();
        let second = doc.new_object_id();
        for (id, marker) in [
            (first, b"SELECTED".as_slice()),
            (second, b"UNSELECTED".as_slice()),
        ] {
            let contents = doc.add_object(Stream::new(Dictionary::new(), marker.to_vec()));
            doc.objects.insert(
                id,
                dictionary! {
                    "Type" => "Page", "Parent" => pages,
                    "MediaBox" => vec![0.into(), 0.into(), 200.into(), 300.into()],
                    "Contents" => contents,
                }
                .into(),
            );
        }
        doc.objects.insert(
            pages,
            dictionary! { "Type" => "Pages", "Kids" => vec![first.into(), second.into()], "Count" => 2 }.into(),
        );
        let catalog = doc.add_object(dictionary! { "Type" => "Catalog", "Pages" => pages });
        doc.trailer.set("Root", catalog);
        let mut bytes = Vec::new();
        doc.save_to(&mut bytes).unwrap();
        bytes
    }

    #[derive(Clone, Copy)]
    enum ProofGroup {
        None,
        Direct,
        IndirectColorSpace,
    }

    fn hayro_write_source(
        rotations: &[i64],
        group: ProofGroup,
        user_unit: Option<(f32, bool)>,
    ) -> Vec<u8> {
        let mut doc = Document::with_version("1.7");
        let pages = doc.new_object_id();
        let page_ids = rotations
            .iter()
            .map(|_| doc.new_object_id())
            .collect::<Vec<_>>();
        let resources = dictionary! {
            "Font" => dictionary! {
                "F1" => dictionary! { "Type" => "Font", "Subtype" => "Type1", "BaseFont" => "Helvetica" },
            },
            "ExtGState" => dictionary! {
                "SourceVectorGS" => dictionary! { "Type" => "ExtGState", "LW" => 2 },
            },
        };
        let markers = [
            "PAGE_ZERO_TEXT",
            "PAGE_ONE_TEXT",
            "PAGE_OMITTED_TEXT",
            "PAGE_THREE_TEXT",
        ];
        if matches!(group, ProofGroup::IndirectColorSpace) {
            doc.objects
                .insert((90, 0), Object::Name(b"DeviceGray".to_vec()));
        }
        for (index, (&id, &rotation)) in page_ids.iter().zip(rotations).enumerate() {
            let marker = markers[index];
            let content = format!(
                "BT /F1 12 Tf 25 50 Td ({marker}) Tj ET\n/SourceVectorGS gs 30 70 m 120 70 l 120 140 l h S\n"
            );
            let contents = doc.add_object(Stream::new(Dictionary::new(), content.into_bytes()));
            let mut page = dictionary! {
                "Type" => "Page", "Parent" => pages, "Contents" => contents, "Rotate" => rotation,
            };
            match group {
                ProofGroup::None => {}
                ProofGroup::Direct => page.set(
                    "Group",
                    dictionary! {
                        "Type" => "Group", "S" => "Transparency", "CS" => "DeviceCMYK",
                        "I" => false, "K" => true,
                    },
                ),
                ProofGroup::IndirectColorSpace => page.set(
                    "Group",
                    dictionary! {
                        "Type" => "Group", "S" => "Transparency", "CS" => (90, 0),
                        "I" => true, "K" => false,
                    },
                ),
            }
            doc.objects.insert(id, page.into());
        }
        let mut pages_dict = dictionary! {
            "Type" => "Pages", "Kids" => page_ids.iter().copied().map(Object::from).collect::<Vec<_>>(),
            "Count" => page_ids.len() as i64,
            "MediaBox" => vec![5.into(), 10.into(), 205.into(), 310.into()],
            "CropBox" => vec![15.into(), 20.into(), 195.into(), 290.into()],
            "Resources" => resources,
        };
        if let Some((unit, indirect)) = user_unit {
            if indirect {
                let unit = doc.add_object(unit);
                pages_dict.set("UserUnit", unit);
            } else {
                pages_dict.set("UserUnit", unit);
            }
        }
        doc.objects.insert(pages, pages_dict.into());
        let catalog = doc.add_object(dictionary! { "Type" => "Catalog", "Pages" => pages });
        doc.trailer.set("Root", catalog);
        let mut bytes = Vec::new();
        doc.save_to(&mut bytes).unwrap();
        bytes
    }

    struct SelectedPageProof {
        extracted_pdf: Vec<u8>,
        selected_document: Document,
        asset: Asset,
    }

    fn validate_indirect_reference_closure(bytes: &[u8]) -> std::result::Result<(), String> {
        fn walk(
            doc: &Document,
            object: &Object,
            path: &str,
            visited: &mut HashSet<lopdf::ObjectId>,
        ) -> std::result::Result<(), String> {
            match object {
                Object::Reference(id) => {
                    let referenced = doc.objects.get(id).ok_or_else(|| {
                        format!("{path}: dangling indirect reference {} {} R", id.0, id.1)
                    })?;
                    if visited.insert(*id) {
                        walk(doc, referenced, path, visited)?;
                    }
                }
                Object::Array(values) => {
                    for (index, value) in values.iter().enumerate() {
                        walk(doc, value, &format!("{path}[{index}]"), visited)?;
                    }
                }
                Object::Dictionary(dict) => {
                    for (key, value) in dict.iter() {
                        walk(
                            doc,
                            value,
                            &format!("{path}/{}", String::from_utf8_lossy(key)),
                            visited,
                        )?;
                    }
                }
                Object::Stream(stream) => {
                    for (key, value) in stream.dict.iter() {
                        walk(
                            doc,
                            value,
                            &format!("{path}/{}", String::from_utf8_lossy(key)),
                            visited,
                        )?;
                    }
                }
                _ => {}
            }
            Ok(())
        }

        let doc = Document::load_mem(bytes).map_err(|error| error.to_string())?;
        walk(
            &doc,
            &Object::Dictionary(doc.trailer.clone()),
            "",
            &mut HashSet::new(),
        )
    }

    fn selected_page_preview(
        document: &pdf::ParsedPdf,
        page_indices: &[usize],
        overlay_pages: Option<&[PageResult]>,
    ) -> Result<SelectedPageProof> {
        if overlay_pages.is_some_and(|pages| pages.len() != page_indices.len()) {
            return Err(Error::InvalidInput(
                "preview page/result count mismatch".into(),
            ));
        }
        let selected = pdf::extract_selected_pages_for_preview(document, page_indices)?;
        let extracted_pdf = selected.bytes.clone();

        validate_indirect_reference_closure(&extracted_pdf)
            .map_err(|error| Error::Pdf(format!("invalid extracted PDF xref: {error}")))?;
        let asset = generate_selected_inner(
            selected,
            overlay_pages,
            "phase1",
            &Limits::default(),
            usize::MAX,
            std::time::Instant::now() + std::time::Duration::from_secs(30),
        )?;
        validate_indirect_reference_closure(&asset.data)
            .map_err(|error| Error::Pdf(format!("invalid final PDF xref: {error}")))?;
        let selected_document = Document::load_mem(&asset.data)
            .map_err(|error| Error::Pdf(format!("cannot parse selected PDF: {error}")))?;
        Ok(SelectedPageProof {
            extracted_pdf,
            selected_document,
            asset,
        })
    }

    #[test]
    fn phase1_indirect_reference_validator_reports_dangling_path_and_id() {
        let mut doc = Document::with_version("1.7");
        let pages = doc.new_object_id();
        let page = doc.new_object_id();
        doc.objects.insert(
            page,
            dictionary! {
                "Type" => "Page", "Parent" => pages,
                "MediaBox" => vec![0.into(), 0.into(), 100.into(), 100.into()],
                "Resources" => dictionary! {
                    "Font" => dictionary! {
                        "F3" => dictionary! { "DescendantFonts" => vec![Object::Reference((99, 0))] },
                    },
                },
            }
            .into(),
        );
        doc.objects.insert(
            pages,
            dictionary! { "Type" => "Pages", "Kids" => vec![page.into()], "Count" => 1 }.into(),
        );
        let catalog = doc.add_object(dictionary! { "Type" => "Catalog", "Pages" => pages });
        doc.trailer.set("Root", catalog);
        let mut bytes = Vec::new();
        doc.save_to(&mut bytes).unwrap();

        let error = validate_indirect_reference_closure(&bytes).unwrap_err();
        assert!(error.contains("/Root/Pages/Kids[0]/Resources/Font/F3/DescendantFonts[0]"));
        assert!(error.contains("99 0 R"));
    }

    fn inherited_indirect_source() -> Vec<u8> {
        let mut doc = Document::with_version("1.5");
        let pages = doc.new_object_id();
        let page = doc.new_object_id();
        let media = doc.add_object(vec![0.into(), 0.into(), 200.into(), 300.into()]);
        let crop = doc.add_object(vec![10.into(), 20.into(), 110.into(), 220.into()]);
        let rotate = doc.add_object(90i64);
        let unit = doc.add_object(1.0f32);
        let source = doc.add_object(Stream::new(Dictionary::new(), b"q\nQ\n".to_vec()));
        let contents = doc.add_object(vec![source.into()]);
        doc.objects.insert(
            page,
            dictionary! { "Type" => "Page", "Parent" => pages, "Contents" => contents }.into(),
        );
        doc.objects.insert(
            pages,
            dictionary! {
                "Type" => "Pages", "Kids" => vec![page.into()], "Count" => 1,
                "MediaBox" => media, "CropBox" => crop, "Rotate" => rotate, "UserUnit" => unit,
            }
            .into(),
        );
        let catalog = doc.add_object(dictionary! { "Type" => "Catalog", "Pages" => pages });
        doc.trailer.set("Root", catalog);
        let mut bytes = Vec::new();
        doc.save_to(&mut bytes).unwrap();
        bytes
    }

    fn ancestor_contents_source() -> Vec<u8> {
        let mut doc = Document::with_version("1.5");
        let pages = doc.new_object_id();
        let page = doc.new_object_id();
        let contents = doc.add_object(Stream::new(Dictionary::new(), b"ANCESTOR".to_vec()));
        doc.objects.insert(
            page,
            dictionary! { "Type" => "Page", "Parent" => pages }.into(),
        );
        doc.objects.insert(
            pages,
            dictionary! {
                "Type" => "Pages", "Kids" => vec![page.into()], "Count" => 1,
                "MediaBox" => vec![0.into(), 0.into(), 200.into(), 300.into()], "Contents" => contents,
            }
            .into(),
        );
        let catalog = doc.add_object(dictionary! { "Type" => "Catalog", "Pages" => pages });
        doc.trailer.set("Root", catalog);
        let mut bytes = Vec::new();
        doc.save_to(&mut bytes).unwrap();
        bytes
    }

    fn page() -> PageResult {
        PageResult {
            page_index: 0,
            page_size: [100.0, 200.0],
            blocks: vec![ContentBlock {
                kind: BlockKind::new(BlockKind::TEXT),
                bbox: NormalizedBbox::new(0.0, 0.0, 0.5, 0.5).unwrap(),
                angle: None,
                content: None,
                merge_previous: false,
                metadata: Map::new(),
            }],
        }
    }

    #[test]
    fn preview_is_parseable_collision_safe_opaque_and_rotated_in_crop_box() {
        let asset = generate(
            &source(90, None),
            &[page()],
            "preview",
            &Limits::default(),
            1 << 20,
        )
        .unwrap();
        let doc = Document::load_mem(&asset.data).unwrap();
        let page_id = *doc.get_pages().get(&1).unwrap();
        let page = doc.get_object(page_id).unwrap().as_dict().unwrap();
        let resources = page.get(b"Resources").unwrap().as_dict().unwrap();
        let gs = resources.get(b"ExtGState").unwrap().as_dict().unwrap();
        let fonts = resources.get(b"Font").unwrap().as_dict().unwrap();
        assert!(gs.get(b"MinerUPreviewAlpha1").is_ok());
        assert!(fonts.get(b"MinerUPreviewHelvetica1").is_ok());
        let contents = page.get(b"Contents").unwrap();
        let contents = match contents {
            Object::Reference(id) => doc.get_object(*id).unwrap(),
            contents => contents,
        };
        let stream = contents.as_stream().unwrap();
        let content = String::from_utf8(stream.content.clone()).unwrap();
        assert!(content.contains("q /MinerUPreviewAlpha1 gs\n"));
        assert!(content.contains("Q\nBT /MinerUPreviewHelvetica1 10 Tf"));
        assert!(content.contains("1 0 0 rg (1) Tj ET"));
        assert!(content.contains("10.000 20.000 m 10.000 120.000 l 60.000 120.000 l"));
    }

    #[test]
    fn inherited_indirect_attributes_and_contents_array_are_resolved() {
        let asset = generate(
            &inherited_indirect_source(),
            &[page()],
            "preview",
            &Limits::default(),
            1 << 20,
        )
        .unwrap();
        let doc = Document::load_mem(&asset.data).unwrap();
        let page_id = *doc.get_pages().get(&1).unwrap();
        let page = doc.get_object(page_id).unwrap().as_dict().unwrap();
        let contents = page.get(b"Contents").unwrap().as_array().unwrap();
        assert_eq!(contents.len(), 2);
        let source = match &contents[0] {
            Object::Reference(id) => doc.get_object(*id).unwrap(),
            object => object,
        }
        .as_stream()
        .unwrap();
        assert_eq!(source.content, b"q\nQ\n");
        let overlay = match &contents[1] {
            Object::Reference(id) => doc.get_object(*id).unwrap(),
            object => object,
        }
        .as_stream()
        .unwrap();
        let overlay = String::from_utf8(overlay.content.clone()).unwrap();
        assert!(overlay.contains("10.000 20.000 m 10.000 120.000 l 60.000 120.000 l"));
    }

    #[test]
    fn fixture_preview_retains_source_text_and_fonts() {
        let asset = generate(
            include_bytes!("../tests/fixtures/pdf/minimal.pdf"),
            &[page()],
            "minimal",
            &Limits::default(),
            1 << 20,
        )
        .unwrap();
        let preview = Document::load_mem(&asset.data).unwrap();
        let text = preview.extract_text(&[1]).unwrap();
        assert!(text.contains("minimal"));
        assert!(text.contains('1'));
        let page_id = *preview.get_pages().get(&1).unwrap();
        let resources = preview
            .get_object(page_id)
            .unwrap()
            .as_dict()
            .unwrap()
            .get(b"Resources")
            .unwrap()
            .as_dict()
            .unwrap();
        let fonts = resources.get(b"Font").unwrap().as_dict().unwrap();
        assert!(fonts.get(b"F1").is_ok());
        assert!(fonts.get(b"MinerUPreviewHelvetica").is_ok());
    }

    #[test]
    fn malformed_local_contents_is_rejected() {
        let mut doc = Document::load_mem(&source(0, None)).unwrap();
        let page_id = *doc.get_pages().get(&1).unwrap();
        doc.get_object_mut(page_id)
            .unwrap()
            .as_dict_mut()
            .unwrap()
            .set("Contents", 1);
        let mut bytes = Vec::new();
        doc.save_to(&mut bytes).unwrap();
        assert!(matches!(
            generate(&bytes, &[page()], "preview", &Limits::default(), 1 << 20),
            Err(Error::Pdf(_))
        ));
    }

    #[test]
    fn ancestor_contents_is_ignored() {
        let asset = generate(
            &ancestor_contents_source(),
            &[page()],
            "preview",
            &Limits::default(),
            1 << 20,
        )
        .unwrap();
        let preview = Document::load_mem(&asset.data).unwrap();
        assert!(!preview.extract_text(&[1]).unwrap().contains("ANCESTOR"));
        let page_id = *preview.get_pages().get(&1).unwrap();
        let contents = preview
            .get_object(page_id)
            .unwrap()
            .as_dict()
            .unwrap()
            .get(b"Contents")
            .unwrap();
        assert!(matches!(contents, Object::Reference(_)));
    }

    #[test]
    fn rejected_write_reports_limit_exceeded() {
        let mut out = CappedWriter::new(3);
        assert!(out.write_all(b"four").is_err());
        assert!(matches!(
            limit_or_pdf(io::Error::other("limit"), out.limit, out.actual()),
            Error::LimitExceeded { actual: 4, .. }
        ));
    }

    #[test]
    fn rejects_invalid_user_unit() {
        assert!(matches!(
            generate(
                &source(0, Some(0.0)),
                &[page()],
                "preview",
                &Limits::default(),
                1 << 20
            ),
            Err(Error::Pdf(_))
        ));
    }

    #[test]
    fn selected_preview_prunes_unselected_source_objects() {
        let mut selected = page();
        selected.page_index = 0;
        let asset = generate(
            &two_page_source(),
            &[selected],
            "preview",
            &Limits::default(),
            1 << 20,
        )
        .unwrap();
        let preview = Document::load_mem(&asset.data).unwrap();
        assert_eq!(preview.get_pages().len(), 1);
        assert!(
            !asset
                .data
                .windows(b"UNSELECTED".len())
                .any(|x| x == b"UNSELECTED")
        );
    }

    fn parsed_pdf(bytes: Vec<u8>) -> std::sync::Arc<pdf::ParsedPdf> {
        pdf::parse_document(bytes, &Limits::default()).unwrap()
    }

    fn rendered_page(document: &pdf::ParsedPdf, index: usize) -> ([f32; 2], Vec<u8>) {
        let page = pdf::render_page(document, index, &Limits::default()).unwrap();
        (page.size, page.image.as_raw().clone())
    }

    fn first_page_stream(doc: &Document, page_number: u32) -> Vec<u8> {
        let id = *doc.get_pages().get(&page_number).unwrap();
        let page = doc.get_object(id).unwrap().as_dict().unwrap();
        let contents = page.get(b"Contents").unwrap();
        let stream = match contents {
            Object::Array(streams) => resolve_object(doc, &streams[0]).unwrap(),
            stream => resolve_object(doc, stream).unwrap(),
        };
        stream.as_stream().unwrap().decompressed_content().unwrap()
    }

    #[test]
    fn selected_preview_production_preserves_nested_form_transparency_chain() {
        let mut source = Document::with_version("1.7");
        let pages = source.new_object_id();
        let page_id = source.new_object_id();
        let inner = source.add_object(Stream::new(
            dictionary! {
                "Type" => "XObject", "Subtype" => "Form", "FormType" => 1,
                "BBox" => vec![0.into(), 0.into(), 20.into(), 20.into()],
                "Resources" => dictionary! {},
                "Group" => dictionary! {
                    "Type" => "Group", "S" => "Transparency", "CS" => "DeviceRGB",
                },
            },
            b"0 0 1 rg 0 0 20 20 re f\n".to_vec(),
        ));
        let outer = source.add_object(Stream::new(
            dictionary! {
                "Type" => "XObject", "Subtype" => "Form", "FormType" => 1,
                "BBox" => vec![0.into(), 0.into(), 20.into(), 20.into()],
                "Resources" => dictionary! {
                    "XObject" => dictionary! { "InnerPhase2" => inner },
                },
            },
            b"q /InnerPhase2 Do Q\n".to_vec(),
        ));
        let contents = source.add_object(Stream::new(
            Dictionary::new(),
            b"q /OuterPhase2 Do Q\n".to_vec(),
        ));
        source.objects.insert(
            page_id,
            dictionary! {
                "Type" => "Page", "Parent" => pages,
                "MediaBox" => vec![0.into(), 0.into(), 100.into(), 100.into()],
                "Resources" => dictionary! {
                    "XObject" => dictionary! { "OuterPhase2" => outer },
                },
                "Contents" => contents,
            }
            .into(),
        );
        source.objects.insert(
            pages,
            dictionary! { "Type" => "Pages", "Kids" => vec![page_id.into()], "Count" => 1 }.into(),
        );
        let catalog = source.add_object(dictionary! { "Type" => "Catalog", "Pages" => pages });
        source.trailer.set("Root", catalog);
        let mut source_bytes = Vec::new();
        source.save_to(&mut source_bytes).unwrap();

        let parsed = pdf::parse_document(source_bytes, &Limits::default()).unwrap();
        let selected = pdf::extract_selected_pages_for_preview(&parsed, &[0]).unwrap();
        let asset = generate_selected_until(
            selected,
            &[page()],
            "nested",
            &Limits::default(),
            1 << 20,
            std::time::Instant::now() + std::time::Duration::from_secs(30),
        )
        .unwrap();
        let preview = Document::load_mem(&asset.data).unwrap();
        let final_page_id = preview.get_pages()[&1];
        let final_page = preview
            .get_object(final_page_id)
            .unwrap()
            .as_dict()
            .unwrap();
        assert!(
            preview
                .get_page_content(final_page_id)
                .windows(b"/OuterPhase2 Do".len())
                .any(|bytes| bytes == b"/OuterPhase2 Do")
        );
        let page_xobjects = final_page
            .get(b"Resources")
            .unwrap()
            .as_dict()
            .unwrap()
            .get(b"XObject")
            .unwrap()
            .as_dict()
            .unwrap();
        let outer_object =
            resolve_object(&preview, page_xobjects.get(b"OuterPhase2").unwrap()).unwrap();
        let outer = outer_object.as_stream().unwrap();
        assert_eq!(
            outer.dict.get(b"Subtype").unwrap().as_name().unwrap(),
            b"Form"
        );
        assert!(
            outer
                .decompressed_content()
                .unwrap()
                .windows(b"/InnerPhase2 Do".len())
                .any(|bytes| bytes == b"/InnerPhase2 Do")
        );
        let outer_resources_object =
            resolve_object(&preview, outer.dict.get(b"Resources").unwrap()).unwrap();
        let outer_resources = outer_resources_object.as_dict().unwrap();
        let inner_object = resolve_object(
            &preview,
            outer_resources
                .get(b"XObject")
                .unwrap()
                .as_dict()
                .unwrap()
                .get(b"InnerPhase2")
                .unwrap(),
        )
        .unwrap();
        let inner = inner_object.as_stream().unwrap();
        assert_eq!(
            inner.dict.get(b"Subtype").unwrap().as_name().unwrap(),
            b"Form"
        );
        assert!(
            inner
                .decompressed_content()
                .unwrap()
                .windows(b"0 0 20 20 re f".len())
                .any(|bytes| bytes == b"0 0 20 20 re f")
        );
        let group_object = resolve_object(&preview, inner.dict.get(b"Group").unwrap()).unwrap();
        let group = group_object.as_dict().unwrap();
        assert_eq!(group.get(b"S").unwrap().as_name().unwrap(), b"Transparency");
    }

    #[test]
    fn phase1_selected_page_preserves_group_variants() {
        let no_group = parsed_pdf(hayro_write_source(&[0], ProofGroup::None, None));
        let proof = selected_page_preview(&no_group, &[0], None).unwrap();
        let page_id = *proof.selected_document.get_pages().get(&1).unwrap();
        assert!(
            proof
                .selected_document
                .get_object(page_id)
                .unwrap()
                .as_dict()
                .unwrap()
                .get(b"Group")
                .is_err()
        );

        let direct = parsed_pdf(hayro_write_source(&[0], ProofGroup::Direct, None));
        let proof = selected_page_preview(&direct, &[0], None).unwrap();
        let page_id = *proof.selected_document.get_pages().get(&1).unwrap();
        let group = proof
            .selected_document
            .get_object(page_id)
            .unwrap()
            .as_dict()
            .unwrap()
            .get(b"Group")
            .unwrap()
            .as_dict()
            .unwrap();
        assert_eq!(group.get(b"CS").unwrap().as_name().unwrap(), b"DeviceCMYK");
        assert!(!group.get(b"I").unwrap().as_bool().unwrap());
        assert!(group.get(b"K").unwrap().as_bool().unwrap());

        let indirect = parsed_pdf(hayro_write_source(
            &[0],
            ProofGroup::IndirectColorSpace,
            None,
        ));
        let proof = selected_page_preview(&indirect, &[0], None).unwrap();
        let page_id = *proof.selected_document.get_pages().get(&1).unwrap();
        let group = proof
            .selected_document
            .get_object(page_id)
            .unwrap()
            .as_dict()
            .unwrap()
            .get(b"Group")
            .unwrap()
            .as_dict()
            .unwrap();
        let color_space = group.get(b"CS").unwrap().as_reference().unwrap();
        assert_ne!(color_space, (90, 0));
        assert_eq!(
            proof
                .selected_document
                .get_object(color_space)
                .unwrap()
                .as_name()
                .unwrap(),
            b"DeviceGray"
        );
    }

    #[test]
    fn phase1_selected_page_resolves_inherited_indirect_user_unit() {
        let pdf = parsed_pdf(hayro_write_source(
            &[0],
            ProofGroup::None,
            Some((1.25, true)),
        ));
        let proof = selected_page_preview(&pdf, &[0], None).unwrap();
        let page_id = *proof.selected_document.get_pages().get(&1).unwrap();
        let page = proof
            .selected_document
            .get_object(page_id)
            .unwrap()
            .as_dict()
            .unwrap();
        assert_eq!(
            number(&proof.selected_document, page.get(b"UserUnit").unwrap()).unwrap(),
            1.25
        );
    }

    #[test]
    fn phase1_selected_page_rejects_invalid_user_unit() {
        for value in [0.0, -1.0, f32::NAN, f32::INFINITY] {
            let pdf = parsed_pdf(hayro_write_source(
                &[0],
                ProofGroup::None,
                Some((value, true)),
            ));
            assert!(matches!(
                selected_page_preview(&pdf, &[0], None),
                Err(Error::Pdf(message)) if message == "invalid UserUnit"
            ));
        }

        let mut invalid =
            Document::load_mem(&hayro_write_source(&[0], ProofGroup::None, None)).unwrap();
        let page_id = *invalid.get_pages().get(&1).unwrap();
        let parent = invalid
            .get_object(page_id)
            .unwrap()
            .as_dict()
            .unwrap()
            .get(b"Parent")
            .unwrap()
            .as_reference()
            .unwrap();
        invalid
            .get_object_mut(parent)
            .unwrap()
            .as_dict_mut()
            .unwrap()
            .set("UserUnit", Object::Name(b"invalid".to_vec()));
        let mut bytes = Vec::new();
        invalid.save_to(&mut bytes).unwrap();
        let pdf = parsed_pdf(bytes);
        assert!(matches!(
            selected_page_preview(&pdf, &[0], None),
            Err(Error::Pdf(message)) if message == "invalid UserUnit"
        ));
    }

    #[test]
    fn phase1_selected_page_render_matches_all_rotations() {
        let source = parsed_pdf(hayro_write_source(
            &[0, 90, 180, 270],
            ProofGroup::None,
            Some((1.25, true)),
        ));
        let proof = selected_page_preview(&source, &[0, 1, 2, 3], None).unwrap();
        let selected = parsed_pdf(proof.asset.data.to_vec());
        assert_eq!(pdf::page_count(&selected), 4);
        for index in 0..4 {
            assert_eq!(
                rendered_page(&source, index),
                rendered_page(&selected, index)
            );
        }
    }

    #[test]
    fn phase1_selected_page_preserves_text_vectors_and_overlay() {
        let source = parsed_pdf(hayro_write_source(
            &[90, 0, 180],
            ProofGroup::None,
            Some((1.25, true)),
        ));
        let proof = selected_page_preview(&source, &[0], Some(&[page()])).unwrap();
        let preview = Document::load_mem(&proof.asset.data).unwrap();
        assert_eq!(preview.get_pages().len(), 1);
        let text = preview.extract_text(&[1]).unwrap();
        assert!(text.contains("PAGE_ZERO_TEXT"));
        assert!(text.contains('1'));
        assert!(!text.contains("PAGE_ONE_TEXT"));
        assert!(!text.contains("PAGE_OMITTED_TEXT"));
        let page_id = *preview.get_pages().get(&1).unwrap();
        let page = preview.get_object(page_id).unwrap().as_dict().unwrap();
        assert!(page.get(b"Group").is_err());
        assert_eq!(
            number(&preview, page.get(b"UserUnit").unwrap()).unwrap(),
            1.25
        );
        let content = preview.get_page_content(page_id);
        let vector = b"30 70 m 120 70 l 120 140 l h S";
        assert!(content.windows(vector.len()).any(|bytes| bytes == vector));
        let contents = page.get(b"Contents").unwrap().as_array().unwrap();
        let overlay = resolve_object(&preview, &contents[1]).unwrap();
        let overlay = String::from_utf8(overlay.as_stream().unwrap().content.clone()).unwrap();
        assert!(overlay.contains("/MinerUPreviewAlpha gs"));
        assert!(overlay.contains("/MinerUPreviewHelvetica 10 Tf"));
        assert!(overlay.contains("1 0 0 rg (1) Tj ET"));
        assert!(overlay.contains("15.000 20.000 m 15.000 155.000 l 105.000 155.000 l"));
        let resources = page.get(b"Resources").unwrap().as_dict().unwrap();
        assert!(
            resources
                .get(b"Font")
                .unwrap()
                .as_dict()
                .unwrap()
                .get(b"F1")
                .is_ok()
        );
        let overlay_font = resources
            .get(b"Font")
            .unwrap()
            .as_dict()
            .unwrap()
            .get(b"MinerUPreviewHelvetica")
            .unwrap();
        assert!(matches!(overlay_font, Object::Reference(_)));
        assert_eq!(
            resolve_object(&preview, overlay_font)
                .unwrap()
                .as_dict()
                .unwrap()
                .get(b"BaseFont")
                .unwrap()
                .as_name()
                .unwrap(),
            b"Helvetica"
        );
        assert!(
            resources
                .get(b"ExtGState")
                .unwrap()
                .as_dict()
                .unwrap()
                .get(b"SourceVectorGS")
                .is_ok()
        );
        let source_vector_state = resolve_object(
            &preview,
            resources
                .get(b"ExtGState")
                .unwrap()
                .as_dict()
                .unwrap()
                .get(b"SourceVectorGS")
                .unwrap(),
        )
        .unwrap();
        assert_eq!(
            source_vector_state
                .as_dict()
                .unwrap()
                .get(b"LW")
                .unwrap()
                .as_i64()
                .unwrap(),
            2
        );
        let alpha = resolve_object(
            &preview,
            resources
                .get(b"ExtGState")
                .unwrap()
                .as_dict()
                .unwrap()
                .get(b"MinerUPreviewAlpha")
                .unwrap(),
        )
        .unwrap();
        assert_eq!(
            number(&preview, alpha.as_dict().unwrap().get(b"ca").unwrap()).unwrap(),
            0.3
        );
    }

    #[test]
    fn phase1_selected_page_preserves_requested_order() {
        let source = parsed_pdf(hayro_write_source(&[0, 90, 180], ProofGroup::None, None));
        let mut second = page();
        second.page_index = 1;
        let proof = selected_page_preview(&source, &[1, 0], Some(&[second, page()])).unwrap();
        let preview = Document::load_mem(&proof.asset.data).unwrap();
        assert_eq!(preview.get_pages().len(), 2);
        assert!(
            preview
                .extract_text(&[1])
                .unwrap()
                .contains("PAGE_ONE_TEXT")
        );
        assert!(
            preview
                .extract_text(&[2])
                .unwrap()
                .contains("PAGE_ZERO_TEXT")
        );
        assert!(
            !preview
                .objects
                .values()
                .filter_map(|object| object.as_stream().ok())
                .filter_map(|stream| stream.decompressed_content().ok())
                .any(|content| content
                    .windows(17)
                    .any(|bytes| bytes == b"PAGE_OMITTED_TEXT"))
        );
    }

    #[test]
    #[ignore = "Phase 1 benchmark requires MINERU_HAYRO_WRITE_BENCH_PDF"]
    fn phase1_hayro_write_selected_page_benchmark() {
        let Ok(path) = std::env::var("MINERU_HAYRO_WRITE_BENCH_PDF") else {
            return;
        };
        let count = std::env::var("MINERU_HAYRO_WRITE_BENCH_PAGES")
            .map(|value| value.parse::<usize>().unwrap())
            .unwrap_or(200);
        assert!(count > 0);
        let source_pdf =
            pdf::parse_document(std::fs::read(path).unwrap(), &Limits::default()).unwrap();
        assert!(pdf::page_count(&source_pdf) >= count);
        let page_indices = (0..count).collect::<Vec<_>>();
        let overlay_pages = std::env::var_os("MINERU_HAYRO_WRITE_BENCH_NO_OVERLAY")
            .is_none()
            .then(|| {
                page_indices
                    .iter()
                    .map(|index| {
                        let mut result = page();
                        result.page_index = *index;
                        result
                    })
                    .collect::<Vec<_>>()
            });
        let proof =
            selected_page_preview(&source_pdf, &page_indices, overlay_pages.as_deref()).unwrap();
        let reread = Document::load_mem(&proof.asset.data).unwrap();
        assert_eq!(reread.get_pages().len(), count);
        assert_eq!(proof.selected_document.get_pages().len(), count);
        assert!(!proof.extracted_pdf.is_empty());
        for index in 0..count {
            assert_eq!(
                first_page_stream(&reread, index as u32 + 1),
                pdf::page_stream_for_preview_test(&source_pdf, index).unwrap()
            );
        }
        if let Ok(path) = std::env::var("MINERU_HAYRO_WRITE_BENCH_EXTRACTED_OUTPUT") {
            std::fs::write(path, &proof.extracted_pdf).unwrap();
        }
        if let Ok(path) = std::env::var("MINERU_HAYRO_WRITE_BENCH_OUTPUT") {
            std::fs::write(path, &proof.asset.data).unwrap();
        }
    }

    #[test]
    fn page_parent_cycles_are_rejected() {
        let mut doc = Document::with_version("1.5");
        let page = doc.new_object_id();
        doc.objects.insert(
            page,
            dictionary! { "Type" => "Page", "Parent" => page }.into(),
        );
        assert!(matches!(
            inherited(&doc, page, b"MediaBox"),
            Err(Error::Pdf(message)) if message == "cyclic page parent"
        ));
    }
}
