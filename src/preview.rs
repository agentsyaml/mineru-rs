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
    fonts.set(
        font.clone(),
        dictionary! { "Type" => "Font", "Subtype" => "Type1", "BaseFont" => "Helvetica" },
    );
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
    use crate::{BlockKind, ContentBlock, NormalizedBbox};
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
