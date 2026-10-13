use crate::official_api::wire::{FileObject, UploadCreate};
use bytes::Bytes;
use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::sync::Semaphore;

const INPUT_CAP: u64 = 256 * 1024 * 1024;
const META_CAP: usize = 1024;
pub(crate) struct Inner {
    pub admission: Mutex<()>,
    pub entries: Mutex<HashMap<String, Entry>>,
    pub used: AtomicU64,
    pub artifact_used: AtomicU64,
    pub meta_used: std::sync::atomic::AtomicUsize,
    pub ids: Mutex<Arc<AtomicU64>>,
    pub accepting: std::sync::atomic::AtomicBool,
}
pub(crate) enum Entry {
    Upload(Upload),
    File(File),
}
pub(crate) struct Upload {
    pub request: UploadCreate,
    pub created: i64,
    pub expires: i64,
    pub bytes: Option<Bytes>,
    pub reservation: Option<Arc<Reservation>>,
    pub metadata: Arc<MetadataReservation>,
    pub file_id: Option<String>,
    pub uploading: bool,
}
pub(crate) struct File {
    pub object: FileObject,
    pub bytes: Bytes,
    pub source: bool,
    pub _metadata: Option<Arc<MetadataReservation>>,
}
pub(crate) struct Reservation {
    pub size: std::sync::atomic::AtomicU64,
    inner: std::sync::Weak<Inner>,
    artifact: bool,
}
impl Reservation {
    pub(crate) fn split(&self, bytes: u64) -> Option<Arc<Self>> {
        self.size
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |size| {
                size.checked_sub(bytes)
            })
            .ok()?;
        Some(Arc::new(Self {
            size: AtomicU64::new(bytes),
            inner: self.inner.clone(),
            artifact: self.artifact,
        }))
    }
    pub(crate) fn shrink_to(&self, size: u64) {
        let old = self.size.swap(size, Ordering::AcqRel);
        if let Some(inner) = self.inner.upgrade() {
            let pool = if self.artifact {
                &inner.artifact_used
            } else {
                &inner.used
            };
            if size < old {
                pool.fetch_sub(old - size, Ordering::AcqRel);
            }
        }
    }
}
pub(crate) struct MetadataReservation {
    count: std::sync::atomic::AtomicUsize,
    inner: std::sync::Weak<Inner>,
}
impl MetadataReservation {
    fn split(&self) -> Option<Arc<Self>> {
        self.count
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |count| {
                count.checked_sub(1)
            })
            .ok()?;
        Some(Arc::new(Self {
            count: std::sync::atomic::AtomicUsize::new(1),
            inner: self.inner.clone(),
        }))
    }
    pub(crate) fn shrink_to(&self, count: usize) {
        let old = self.count.swap(count, Ordering::AcqRel);
        if count < old {
            if let Some(inner) = self.inner.upgrade() {
                inner.meta_used.fetch_sub(old - count, Ordering::AcqRel);
            }
        }
    }
}
impl Drop for MetadataReservation {
    fn drop(&mut self) {
        if let Some(inner) = self.inner.upgrade() {
            inner
                .meta_used
                .fetch_sub(self.count.load(Ordering::Acquire), Ordering::AcqRel);
        }
    }
}
impl Drop for Reservation {
    fn drop(&mut self) {
        if let Some(inner) = self.inner.upgrade() {
            let pool = if self.artifact {
                &inner.artifact_used
            } else {
                &inner.used
            };
            pool.fetch_sub(self.size.load(Ordering::Acquire), Ordering::AcqRel);
        }
    }
}
pub(crate) struct Charged {
    pub data: Vec<u8>,
    _reservation: Arc<Reservation>,
}
impl AsRef<[u8]> for Charged {
    fn as_ref(&self) -> &[u8] {
        &self.data
    }
}
#[derive(Clone)]
pub struct OfficialFileStore {
    pub(crate) inner: Arc<Inner>,
    _slots: Arc<Semaphore>,
    _gate: Arc<Semaphore>,
    retention: Duration,
    limits: Arc<Mutex<TransportLimits>>,
    max_concurrent: usize,
}
#[derive(Clone, Copy)]
struct TransportLimits {
    file: u64,
    body: usize,
    zip: u64,
}
impl OfficialFileStore {
    pub fn new(
        slots: Arc<Semaphore>,
        gate: Arc<Semaphore>,
        retention: Duration,
    ) -> Result<Self, &'static str> {
        if retention < Duration::from_secs(3600) {
            return Err("official file retention must be at least one hour");
        }
        let max_concurrent = slots.available_permits();
        Ok(Self {
            inner: Arc::new(Inner {
                admission: Mutex::new(()),
                entries: Mutex::new(HashMap::new()),
                used: AtomicU64::new(0),
                artifact_used: AtomicU64::new(0),
                meta_used: std::sync::atomic::AtomicUsize::new(0),
                ids: Mutex::new(Arc::new(AtomicU64::new(1))),
                accepting: std::sync::atomic::AtomicBool::new(true),
            }),
            _slots: slots,
            _gate: gate,
            retention,
            limits: Arc::new(Mutex::new(TransportLimits {
                file: 16_777_216,
                body: 16_777_216,
                zip: 64 * 1024 * 1024,
            })),
            max_concurrent,
        })
    }
    pub fn with_shared_ids(self, ids: Arc<AtomicU64>) -> Self {
        if let Ok(mut source) = self.inner.ids.lock() {
            *source = ids;
        }
        self
    }
    pub fn with_transport_limits(self, file_cap: u64, body_cap: usize, zip_cap: u64) -> Self {
        if let Ok(mut limits) = self.limits.lock() {
            limits.file = limits.file.min(file_cap);
            limits.body = limits.body.min(body_cap);
            limits.zip = limits.zip.min(zip_cap);
        }
        self
    }
    pub(crate) fn file_cap(&self) -> u64 {
        self.limits.lock().map(|v| v.file).unwrap_or(0)
    }
    pub(crate) fn body_cap(&self) -> usize {
        self.limits.lock().map(|v| v.body).unwrap_or(0)
    }
    pub(crate) fn zip_cap(&self) -> u64 {
        self.limits.lock().map(|v| v.zip).unwrap_or(0)
    }
    pub(crate) fn now() -> i64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64
    }
    pub(crate) fn id(&self) -> String {
        self.next_id("upload")
    }
    pub(crate) fn next_id(&self, prefix: &str) -> String {
        let ids = self
            .inner
            .ids
            .lock()
            .map(|v| v.clone())
            .unwrap_or_else(|_| Arc::new(AtomicU64::new(1)));
        format!("{prefix}_{:016x}", ids.fetch_add(1, Ordering::Relaxed))
    }
    pub(crate) fn shared_slots(&self) -> &Arc<Semaphore> {
        &self._slots
    }
    pub(crate) fn shared_gate(&self) -> &Arc<Semaphore> {
        &self._gate
    }
    pub(crate) fn reserve(&self, bytes: u64) -> bool {
        self.inner
            .used
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                n.checked_add(bytes).filter(|v| *v <= INPUT_CAP)
            })
            .is_ok()
    }
    pub(crate) fn input_reservation(&self, bytes: u64) -> Option<Arc<Reservation>> {
        self.reserve(bytes).then(|| {
            Arc::new(Reservation {
                size: AtomicU64::new(bytes),
                inner: Arc::downgrade(&self.inner),
                artifact: false,
            })
        })
    }
    pub(crate) fn artifact_budget(&self, bytes: u64) -> Option<Arc<Reservation>> {
        self.inner
            .artifact_used
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                n.checked_add(bytes).filter(|v| *v <= 256 * 1024 * 1024)
            })
            .ok()?;
        Some(Arc::new(Reservation {
            size: AtomicU64::new(bytes),
            inner: Arc::downgrade(&self.inner),
            artifact: true,
        }))
    }
    pub(crate) fn reserve_metadata(&self, count: usize) -> Option<Arc<MetadataReservation>> {
        self.inner
            .meta_used
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                n.checked_add(count).filter(|v| *v <= META_CAP)
            })
            .ok()?;
        Some(Arc::new(MetadataReservation {
            count: std::sync::atomic::AtomicUsize::new(count),
            inner: Arc::downgrade(&self.inner),
        }))
    }
    pub(crate) fn charged(&self, data: Vec<u8>, reservation: Arc<Reservation>) -> Bytes {
        Bytes::from_owner(Charged {
            data,
            _reservation: reservation,
        })
    }
    pub(crate) fn retain_for(&self, seconds: u64) -> bool {
        seconds <= self.retention.as_secs()
    }
    pub(crate) fn retention_secs(&self) -> u64 {
        self.retention.as_secs()
    }
    pub(crate) fn max_concurrent(&self) -> usize {
        self.max_concurrent
    }
    pub(crate) fn clear_retained(&self) {
        let removed = self
            .inner
            .entries
            .lock()
            .map(|mut m| std::mem::take(&mut *m))
            .unwrap_or_default();
        drop(removed);
    }
    pub(crate) fn file(&self, id: &str) -> Option<FileObject> {
        match self.inner.entries.lock().ok()?.get(id)? {
            Entry::File(f)
                if f.object
                    .expires_at
                    .is_none_or(|expiry| expiry > Self::now()) =>
            {
                Some(f.object.clone())
            }
            _ => None,
        }
    }
    pub(crate) fn input_lease(&self, id: &str) -> Option<(FileObject, Bytes)> {
        match self.inner.entries.lock().ok()?.get(id)? {
            Entry::File(f)
                if f.source
                    && f.object
                        .expires_at
                        .is_none_or(|expiry| expiry > Self::now()) =>
            {
                Some((f.object.clone(), f.bytes.clone()))
            }
            _ => None,
        }
    }
    pub(crate) fn publish_artifact_batch(
        &self,
        files: Vec<(FileObject, Vec<u8>)>,
        metadata: Arc<MetadataReservation>,
        reservation: Arc<Reservation>,
    ) -> Result<(), &'static str> {
        let total = files
            .iter()
            .try_fold(0u64, |n, (_, v)| {
                n.checked_add(v.len() as u64)
                    .filter(|_| v.len() <= 64 * 1024 * 1024)
            })
            .ok_or("artifact exceeds per-file/job limit")?;
        if total > reservation.size.load(Ordering::Acquire) {
            return Err("artifact reservation exhausted");
        }
        let mut entries = match self.inner.entries.lock() {
            Ok(v) => v,
            Err(_) => return Err("store unavailable"),
        };
        if entries.len().saturating_add(files.len()) > META_CAP {
            return Err("metadata capacity exhausted");
        }
        if files.iter().any(|(f, _)| entries.contains_key(&f.id)) {
            return Err("artifact identifier conflict");
        }
        let mut charged_bytes = Vec::with_capacity(files.len());
        for (object, data) in files {
            let reservation = reservation
                .split(data.len() as u64)
                .ok_or("artifact reservation exhausted")?;
            let file_metadata = metadata.split().ok_or("metadata capacity exhausted")?;
            charged_bytes.push((object, self.charged(data, reservation), file_metadata));
        }
        for (object, bytes, file_metadata) in charged_bytes {
            let id = object.id.clone();
            entries.insert(
                id,
                Entry::File(File {
                    object,
                    bytes,
                    source: false,
                    _metadata: Some(file_metadata),
                }),
            );
        }
        Ok(())
    }
    pub fn stop_admissions(&self) {
        let _admission = self
            .inner
            .admission
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.inner.accepting.store(false, Ordering::Release);
    }
    pub fn cleanup_expired(&self) {
        let mut removed = Vec::new();
        if let Ok(mut entries) = self.inner.entries.lock() {
            let now = Self::now();
            let ids: Vec<_> = entries
                .iter()
                .filter_map(|(id, entry)| match entry {
                    Entry::Upload(u) if u.expires <= now => Some(id.clone()),
                    Entry::File(f) if f.object.expires_at.is_some_and(|t| t <= now) => {
                        Some(id.clone())
                    }
                    _ => None,
                })
                .collect();
            for id in ids {
                if let Some(entry) = entries.remove(&id) {
                    removed.push(entry);
                }
            }
        }
        drop(removed);
    }
}
