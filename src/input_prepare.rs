//! Bounded, path-free conversion of declared document bytes into a PDF.
use crate::{OfficeWorkers, OfficialPdfOptions, mineru_api::ooxml};
use bytes::Bytes;
use hayro_jpeg2000::{DecodeSettings, Image as Jp2Image};
use image::{AnimationDecoder, DynamicImage, ImageDecoder, ImageFormat, ImageReader};
use lopdf::{Document, Object, Stream, dictionary};
use std::{
    io::{BufReader, Cursor},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio::{
    sync::{Mutex as AsyncMutex, oneshot},
    task::JoinHandle,
};

#[derive(Clone)]
pub struct RasterWorkers {
    state: Arc<Mutex<RasterWorkerState>>,
    drain_gate: Arc<AsyncMutex<()>>,
}

impl Default for RasterWorkers {
    fn default() -> Self {
        Self {
            state: Arc::default(),
            drain_gate: Arc::default(),
        }
    }
}

#[derive(Default)]
struct RasterWorkerState {
    draining: bool,
    owners: Vec<JoinHandle<()>>,
}

impl RasterWorkers {
    async fn reap(&self) {
        let owners = {
            let mut state = self.state.lock().expect("raster worker registry poisoned");
            let mut owners = Vec::new();
            let mut i = 0;
            while i < state.owners.len() {
                if state.owners[i].is_finished() {
                    owners.push(state.owners.swap_remove(i));
                } else {
                    i += 1;
                }
            }
            owners
        };
        for owner in owners {
            let _ = owner.await;
        }
    }

    async fn submit<T: Send + 'static>(
        &self,
        job: impl FnOnce() -> Result<T, String> + Send + 'static,
    ) -> Result<T, String> {
        self.reap().await;
        let (send, receive) = oneshot::channel();
        {
            let mut state = self.state.lock().expect("raster worker registry poisoned");
            if state.draining {
                return Err("image preparation workers are draining".into());
            }
            let owner = tokio::spawn(async move {
                let result = tokio::task::spawn_blocking(job)
                    .await
                    .map_err(|_| "image preparation worker stopped".to_string())
                    .and_then(|result| result);
                let _ = send.send(result);
            });
            state.owners.push(owner);
        }
        receive
            .await
            .map_err(|_| "image preparation worker stopped".to_string())?
    }

    #[cfg(test)]
    pub(crate) async fn test_admission(&self) -> Result<(), String> {
        self.submit(|| Ok(())).await
    }

    pub async fn drain(&self) {
        let _drain = self.drain_gate.lock().await;
        let owners = {
            let mut state = self.state.lock().expect("raster worker registry poisoned");
            state.draining = true;
            std::mem::take(&mut state.owners)
        };
        for owner in owners {
            let _ = owner.await;
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DocumentKind {
    Pdf,
    Png,
    Jpeg,
    Jpg,
    Jp2,
    Webp,
    Gif,
    Bmp,
    Tiff,
    Docx,
    Pptx,
    Xlsx,
}
impl DocumentKind {
    /// Returns the closed set of input types accepted by the local runners.
    pub fn from_suffix(suffix: &str) -> Option<Self> {
        if suffix.eq_ignore_ascii_case("pdf") {
            Some(Self::Pdf)
        } else if suffix.eq_ignore_ascii_case("png") {
            Some(Self::Png)
        } else if suffix.eq_ignore_ascii_case("jpeg") {
            Some(Self::Jpeg)
        } else if suffix.eq_ignore_ascii_case("jpg") {
            Some(Self::Jpg)
        } else if suffix.eq_ignore_ascii_case("jp2") {
            Some(Self::Jp2)
        } else if suffix.eq_ignore_ascii_case("webp") {
            Some(Self::Webp)
        } else if suffix.eq_ignore_ascii_case("gif") {
            Some(Self::Gif)
        } else if suffix.eq_ignore_ascii_case("bmp") {
            Some(Self::Bmp)
        } else if suffix.eq_ignore_ascii_case("tiff") {
            Some(Self::Tiff)
        } else if suffix.eq_ignore_ascii_case("docx") {
            Some(Self::Docx)
        } else if suffix.eq_ignore_ascii_case("pptx") {
            Some(Self::Pptx)
        } else if suffix.eq_ignore_ascii_case("xlsx") {
            Some(Self::Xlsx)
        } else {
            None
        }
    }
    pub const fn suffix(self) -> &'static str {
        match self {
            Self::Pdf => "pdf",
            Self::Png => "png",
            Self::Jpeg => "jpeg",
            Self::Jpg => "jpg",
            Self::Jp2 => "jp2",
            Self::Webp => "webp",
            Self::Gif => "gif",
            Self::Bmp => "bmp",
            Self::Tiff => "tiff",
            Self::Docx => "docx",
            Self::Pptx => "pptx",
            Self::Xlsx => "xlsx",
        }
    }
    pub const fn is_office(self) -> bool {
        matches!(self, Self::Docx | Self::Pptx | Self::Xlsx)
    }
    pub const fn supports_page_range(self) -> bool {
        matches!(self, Self::Pdf)
    }
}

#[derive(Debug)]
pub struct PreparedPdf {
    pub bytes: Bytes,
    pub kind: DocumentKind,
    pub original: Bytes,
}

pub async fn prepare(
    bytes: impl Into<Bytes>,
    declared: DocumentKind,
    options: &OfficialPdfOptions,
    workers: &OfficeWorkers,
    raster_workers: &RasterWorkers,
    remaining: Duration,
) -> Result<PreparedPdf, String> {
    prepare_with_warning(bytes, declared, options, workers, raster_workers, remaining)
        .await
        .map(|(prepared, _)| prepared)
}

#[doc(hidden)]
pub async fn prepare_with_warning(
    bytes: impl Into<Bytes>,
    declared: DocumentKind,
    options: &OfficialPdfOptions,
    workers: &OfficeWorkers,
    raster_workers: &RasterWorkers,
    remaining: Duration,
) -> Result<(PreparedPdf, Option<String>), String> {
    let bytes = bytes.into();
    let original = bytes.clone();
    let deadline = Instant::now()
        .checked_add(remaining)
        .ok_or("input preparation deadline overflow")?;
    let mut validation_options = options.clone();
    if declared != DocumentKind::Pdf {
        validation_options.start_page = 0;
        validation_options.end_page = None;
    }
    validation_options.validate().map_err(|e| e.to_string())?;
    if bytes.len() > options.max_pdf_bytes {
        return Err("input exceeds PDF byte limit".into());
    }
    if remaining.is_zero() {
        return Err("input preparation deadline expired".into());
    }
    if declared == DocumentKind::Pdf {
        if !bytes.starts_with(b"%PDF-") {
            return Err("suffix/content mismatch".into());
        }
        deadline_expired(deadline)?;
        return Ok((
            PreparedPdf {
                bytes: bytes.clone(),
                kind: declared,
                original,
            },
            None,
        ));
    }
    if declared.is_office() {
        let (bytes, detected) = tokio::task::spawn_blocking(move || {
            let detected = ooxml::detect_bytes(&bytes);
            (bytes, detected)
        })
        .await
        .map_err(|_| "input preparation worker stopped".to_string())?;
        let found = detected
            .map_err(|e| format!("invalid OOXML: {e}"))?
            .ok_or("invalid OOXML")?;
        let kind = match found {
            "docx" => DocumentKind::Docx,
            "pptx" => DocumentKind::Pptx,
            "xlsx" => DocumentKind::Xlsx,
            _ => return Err("invalid OOXML".into()),
        };
        if kind != declared {
            return Err("suffix/content mismatch".into());
        }
        let timeout = deadline
            .checked_duration_since(Instant::now())
            .filter(|timeout| !timeout.is_zero())
            .ok_or("input preparation deadline expired")?;
        let (pdf, warning) = workers
            .convert_with_warning(found, bytes, timeout)
            .await
            .map_err(|e| e.to_string())?;
        validate_prepared_pdf(&pdf, options)?;
        deadline_expired(deadline)?;
        return Ok((
            PreparedPdf {
                bytes: pdf.into(),
                kind,
                original,
            },
            warning,
        ));
    }
    let options = options.clone();
    let pdf = raster_workers
        .submit(move || {
            if declared == DocumentKind::Jp2 {
                jp2_pdf(bytes, &options)
            } else {
                image_pdf(
                    bytes,
                    format_for(declared).ok_or("unsupported document kind")?,
                    &options,
                )
            }
        })
        .await?;
    deadline_expired(deadline)?;
    Ok((
        PreparedPdf {
            bytes: pdf.into(),
            kind: declared,
            original,
        },
        None,
    ))
}

fn deadline_expired(deadline: Instant) -> Result<(), String> {
    if Instant::now() >= deadline {
        Err("input preparation deadline expired".into())
    } else {
        Ok(())
    }
}

fn format_for(kind: DocumentKind) -> Option<ImageFormat> {
    Some(match kind {
        DocumentKind::Png => ImageFormat::Png,
        DocumentKind::Jpeg | DocumentKind::Jpg => ImageFormat::Jpeg,
        DocumentKind::Webp => ImageFormat::WebP,
        DocumentKind::Gif => ImageFormat::Gif,
        DocumentKind::Bmp => ImageFormat::Bmp,
        DocumentKind::Tiff => ImageFormat::Tiff,
        _ => return None,
    })
}
fn image_pdf(
    bytes: Bytes,
    expected: ImageFormat,
    options: &OfficialPdfOptions,
) -> Result<Vec<u8>, String> {
    let reader = ImageReader::with_format(Cursor::new(&bytes), expected);
    let mut decoder = reader.into_decoder().map_err(|_| "invalid image")?;
    let (w, h) = decoder.dimensions();
    limits(w, h, options)?;
    reject_animation(&bytes, expected)?;
    let orientation = decoder.orientation().map_err(|_| "invalid image")?;
    let mut image = DynamicImage::from_decoder(decoder).map_err(|_| "invalid image")?;
    image.apply_orientation(orientation);
    let rgb = white_rgb(image);
    limits(rgb.width(), rgb.height(), options)?;
    pdf_from_rgb(rgb.width(), rgb.height(), rgb.into_raw(), options)
}
fn jp2_pdf(bytes: Bytes, options: &OfficialPdfOptions) -> Result<Vec<u8>, String> {
    let image = Jp2Image::new(&bytes, &DecodeSettings::default()).map_err(|_| "invalid JP2")?;
    let (w, h) = (image.width(), image.height());
    limits(w, h, options)?;
    // Image::new validates the JP2/J2C container and codestream header; JPXDecode
    // retains the validated source bytes without allocating a second raster.
    pdf_from_jpx(w, h, bytes.to_vec(), options)
}
fn limits(w: u32, h: u32, o: &OfficialPdfOptions) -> Result<(), String> {
    let px = (w as u64)
        .checked_mul(h as u64)
        .ok_or("image dimensions overflow")?;
    let rgb = px.checked_mul(3).ok_or("image dimensions overflow")?;
    if w > u16::MAX as u32
        || h > u16::MAX as u32
        || px > o.max_page_pixels
        || rgb > o.max_rendered_image_bytes.min(o.max_in_flight_image_bytes) as u64
    {
        Err("image exceeds limits".into())
    } else {
        Ok(())
    }
}
fn white_rgb(image: DynamicImage) -> image::RgbImage {
    let rgba = image.to_rgba8();
    image::RgbImage::from_fn(rgba.width(), rgba.height(), |x, y| {
        let p = rgba.get_pixel(x, y);
        let a = p[3] as u16;
        image::Rgb([
            ((p[0] as u16 * a + 255 * (255 - a)) / 255) as u8,
            ((p[1] as u16 * a + 255 * (255 - a)) / 255) as u8,
            ((p[2] as u16 * a + 255 * (255 - a)) / 255) as u8,
        ])
    })
}
fn pdf_from_rgb(w: u32, h: u32, data: Vec<u8>, o: &OfficialPdfOptions) -> Result<Vec<u8>, String> {
    let mut doc = Document::with_version("1.5");
    let mut stream = Stream::new(
        dictionary! {"Type"=>"XObject","Subtype"=>"Image","Width"=>w as i64,"Height"=>h as i64,"ColorSpace"=>"DeviceRGB","BitsPerComponent"=>8},
        data,
    );
    stream.compress().map_err(|_| "image compression failed")?;
    let image = doc.add_object(stream);
    finish_image_pdf(&mut doc, image, w, h, o)
}
fn pdf_from_jpx(w: u32, h: u32, data: Vec<u8>, o: &OfficialPdfOptions) -> Result<Vec<u8>, String> {
    let mut doc = Document::with_version("1.5");
    // JPXDecode carries its own colorspace and component metadata.
    let image = doc.add_object(Stream::new(dictionary!{"Type"=>"XObject","Subtype"=>"Image","Width"=>w as i64,"Height"=>h as i64,"Filter"=>"JPXDecode"}, data));
    finish_image_pdf(&mut doc, image, w, h, o)
}
fn finish_image_pdf(
    doc: &mut Document,
    image: lopdf::ObjectId,
    w: u32,
    h: u32,
    o: &OfficialPdfOptions,
) -> Result<Vec<u8>, String> {
    let content = doc.add_object(Stream::new(
        dictionary! {},
        format!(
            "q {} 0 0 {} 0 0 cm /Im0 Do Q",
            w as f64 * 72.0 / 200.0,
            h as f64 * 72.0 / 200.0
        )
        .into_bytes(),
    ));
    let page=doc.add_object(dictionary!{"Type"=>"Page","MediaBox"=>vec![0.into(),0.into(),(w as f64*72./200.).into(),(h as f64*72./200.).into()],"Resources"=>dictionary!{"XObject"=>dictionary!{"Im0"=>image}},"Contents"=>content});
    let pages = doc.new_object_id();
    doc.objects.insert(
        pages,
        Object::Dictionary(dictionary! {"Type"=>"Pages","Kids"=>vec![page.into()],"Count"=>1}),
    );
    doc.get_object_mut(page)
        .map_err(|_| "PDF construction failed")?
        .as_dict_mut()
        .map_err(|_| "PDF construction failed")?
        .set("Parent", pages);
    let catalog = doc.add_object(dictionary! {"Type"=>"Catalog","Pages"=>pages});
    doc.trailer.set("Root", catalog);
    let mut out = Vec::new();
    doc.save_to(&mut out)
        .map_err(|_| "PDF construction failed")?;
    validate_image_pdf(&out, o)?;
    Ok(out)
}
fn validate_prepared_pdf(bytes: &[u8], o: &OfficialPdfOptions) -> Result<(), String> {
    if bytes.len() > o.max_pdf_bytes {
        return Err("PDF exceeds size limit".into());
    }
    let doc = Document::load_mem(bytes).map_err(|_| "invalid generated PDF")?;
    if doc.is_encrypted() || doc.get_pages().is_empty() || doc.get_pages().len() > o.max_pages {
        return Err("PDF page limit exceeded".into());
    }
    Ok(())
}
fn validate_image_pdf(bytes: &[u8], o: &OfficialPdfOptions) -> Result<(), String> {
    validate_prepared_pdf(bytes, o)?;
    (Document::load_mem(bytes)
        .map_err(|_| "invalid generated PDF")?
        .get_pages()
        .len()
        == 1)
        .then_some(())
        .ok_or("generated PDF is not one page".into())
}
fn reject_animation(bytes: &[u8], f: ImageFormat) -> Result<(), String> {
    match f {
        ImageFormat::Png => image::codecs::png::PngDecoder::new(BufReader::new(Cursor::new(bytes)))
            .map_err(|_| "invalid image")?
            .is_apng()
            .map_err(|_| "invalid image")?
            .then_some(()),
        ImageFormat::WebP => image::codecs::webp::WebPDecoder::new(Cursor::new(bytes))
            .map_err(|_| "invalid image")?
            .has_animation()
            .then_some(()),
        ImageFormat::Gif => {
            let mut frames = image::codecs::gif::GifDecoder::new(Cursor::new(bytes))
                .map_err(|_| "invalid image")?
                .into_frames();
            frames.next().transpose().map_err(|_| "invalid image")?;
            frames
                .next()
                .transpose()
                .map_err(|_| "invalid image")?
                .is_some()
                .then_some(())
        }
        ImageFormat::Tiff => tiff::decoder::Decoder::new(Cursor::new(bytes))
            .map_err(|_| "invalid image")?
            .more_images()
            .then_some(()),
        _ => None,
    }
    .map_or(Ok(()), |_| {
        Err("animated or multi-page images are unsupported".into())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    #[test]
    fn document_kind_suffixes_are_closed_and_case_insensitive() {
        let all = [
            DocumentKind::Pdf,
            DocumentKind::Png,
            DocumentKind::Jpeg,
            DocumentKind::Jpg,
            DocumentKind::Jp2,
            DocumentKind::Webp,
            DocumentKind::Gif,
            DocumentKind::Bmp,
            DocumentKind::Tiff,
            DocumentKind::Docx,
            DocumentKind::Pptx,
            DocumentKind::Xlsx,
        ];
        for kind in all {
            assert_eq!(DocumentKind::from_suffix(kind.suffix()), Some(kind));
            assert_eq!(
                DocumentKind::from_suffix(&kind.suffix().to_ascii_uppercase()),
                Some(kind)
            );
        }
        assert_eq!(DocumentKind::from_suffix("txt"), None);
    }

    #[test]
    fn deadline_is_expired_at_the_boundary() {
        assert_eq!(
            deadline_expired(Instant::now()).unwrap_err(),
            "input preparation deadline expired"
        );
    }

    #[tokio::test]
    async fn non_office_preparation_has_no_warning() {
        let workers = OfficeWorkers::with_executable("unused".into());
        let (prepared, warning) = prepare_with_warning(
            Bytes::from_static(b"%PDF-1.4"),
            DocumentKind::Pdf,
            &OfficialPdfOptions::default(),
            &workers,
            &RasterWorkers::default(),
            Duration::from_secs(1),
        )
        .await
        .unwrap();
        assert_eq!(prepared.kind, DocumentKind::Pdf);
        assert_eq!(warning, None);
    }

    #[tokio::test]
    async fn raster_owner_survives_caller_abort_and_drain_closes_admission() {
        let workers = RasterWorkers::default();
        let (started_send, started) = oneshot::channel();
        let (release_send, release) = std::sync::mpsc::channel();
        let done = Arc::new(AtomicBool::new(false));
        let job_workers = workers.clone();
        let job_done = done.clone();
        let caller = tokio::spawn(async move {
            job_workers
                .submit(move || {
                    let _ = started_send.send(());
                    release.recv().unwrap();
                    job_done.store(true, Ordering::SeqCst);
                    Ok(())
                })
                .await
        });
        started.await.unwrap();
        caller.abort();
        let _ = caller.await;
        let mut draining = tokio::spawn({
            let workers = workers.clone();
            async move { workers.drain().await }
        });
        assert!(
            tokio::time::timeout(Duration::from_millis(20), &mut draining)
                .await
                .is_err()
        );
        release_send.send(()).unwrap();
        draining.await.unwrap();
        assert!(done.load(Ordering::SeqCst));
        assert_eq!(
            workers.submit(|| Ok(())).await.unwrap_err(),
            "image preparation workers are draining"
        );
    }

    #[tokio::test]
    async fn concurrent_drains_wait_for_the_same_blocked_owner() {
        let workers = RasterWorkers::default();
        let (started_send, started) = oneshot::channel();
        let (release_send, release) = std::sync::mpsc::channel();
        let job_workers = workers.clone();
        let caller = tokio::spawn(async move {
            job_workers
                .submit(move || {
                    let _ = started_send.send(());
                    release.recv().unwrap();
                    Ok(())
                })
                .await
        });
        started.await.unwrap();
        caller.abort();
        let _ = caller.await;

        let mut first = tokio::spawn({
            let workers = workers.clone();
            async move { workers.drain().await }
        });
        let mut second = tokio::spawn({
            let workers = workers.clone();
            async move { workers.drain().await }
        });
        assert!(
            tokio::time::timeout(Duration::from_millis(20), async {
                tokio::select! {
                    _ = &mut first => (),
                    _ = &mut second => (),
                }
            })
            .await
            .is_err()
        );
        release_send.send(()).unwrap();
        first.await.unwrap();
        second.await.unwrap();
    }
}
