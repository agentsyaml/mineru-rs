//! Bounded byte and metadata reservations shared by the task store.
use super::Inner;
use bytes::Bytes;
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

pub(crate) struct Reservation {
    pub size: AtomicU64,
    inner: std::sync::Weak<Inner>,
    artifact: bool,
}
impl Reservation {
    pub(crate) fn input(bytes: u64, inner: &Arc<Inner>) -> Self {
        Self {
            size: AtomicU64::new(bytes),
            inner: Arc::downgrade(inner),
            artifact: false,
        }
    }
    pub(crate) fn artifact(bytes: u64, inner: &Arc<Inner>) -> Self {
        Self {
            size: AtomicU64::new(bytes),
            inner: Arc::downgrade(inner),
            artifact: true,
        }
    }
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
    pub(crate) fn new(count: usize, inner: &Arc<Inner>) -> Self {
        Self {
            count: std::sync::atomic::AtomicUsize::new(count),
            inner: Arc::downgrade(inner),
        }
    }
    pub(crate) fn split(&self) -> Option<Arc<Self>> {
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
        if count < old
            && let Some(inner) = self.inner.upgrade()
        {
            inner.meta_used.fetch_sub(old - count, Ordering::AcqRel);
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
impl Charged {
    pub(crate) fn new(data: Vec<u8>, reservation: Arc<Reservation>) -> Self {
        Self {
            data,
            _reservation: reservation,
        }
    }
    pub(crate) fn into_bytes(self) -> Bytes {
        Bytes::from_owner(self)
    }
}
