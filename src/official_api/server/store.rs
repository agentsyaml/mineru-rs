use crate::official_api::wire::{
    ErrorDetail, FileObjectModel, FileParseInfo, TaskFileState, TaskStatus, Tier, UploadTaskOptions,
};
use bytes::Bytes;
use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::sync::{Mutex as AsyncMutex, OwnedSemaphorePermit, Semaphore};

mod accounting;
pub(crate) use accounting::{Charged, MetadataReservation, Reservation};

const INPUT_CAP: u64 = 256 * 1024 * 1024;
const META_CAP: usize = 1024;
pub(crate) struct Inner {
    pub admission: Mutex<()>,
    /// Task records keyed by task id; each record owns its per-file state.
    pub tasks: Mutex<HashMap<String, Arc<AsyncMutex<TaskRecord>>>>,
    /// Published parse results keyed by the task file id they belong to.
    pub files: Mutex<HashMap<String, StoredFile>>,
    pub used: AtomicU64,
    pub artifact_used: AtomicU64,
    pub meta_used: std::sync::atomic::AtomicUsize,
    pub ids: Mutex<Arc<AtomicU64>>,
    pub accepting: std::sync::atomic::AtomicBool,
}
pub(crate) struct StoredFile {
    pub object: FileObjectModel,
    pub bytes: Bytes,
    _metadata: Arc<MetadataReservation>,
}
/// One input file of a task, owning its upload reservation and result state.
pub(crate) struct TaskFile {
    pub file_id: String,
    pub file_name: String,
    /// Declared size from the request; kept for upload consistency checks.
    pub declared: Option<u64>,
    /// Actual size once bytes or a URL download were accepted.
    pub actual: Option<u64>,
    pub url: Option<String>,
    pub sha256sum: Option<String>,
    pub page_range: String,
    pub tier: Tier,
    pub status: TaskFileState,
    pub expired: bool,
    /// Whether an upload or a URL download already delivered this input.
    pub uploaded: bool,
    pub upload_active: bool,
    pub submitted: bool,
    pub started_at: Option<String>,
    pub finished_at: Option<String>,
    pub parse: Option<FileParseInfo>,
    pub error: Option<ErrorDetail>,
    pub data: Option<Bytes>,
    /// Serializes uploads of this file; a repeat after completion is a no-op.
    pub upload_lock: Arc<AsyncMutex<()>>,
}
pub(crate) struct TaskRecord {
    pub id: String,
    pub created_at: String,
    pub finished_at: Option<String>,
    pub status: TaskStatus,
    pub options: UploadTaskOptions,
    pub files: Vec<TaskFile>,
    pub expires_at: i64,
    pub retention_at: Option<i64>,
    pub error: Option<ErrorDetail>,
    pub running: bool,
    pub _slot: OwnedSemaphorePermit,
    pub _metadata: Arc<MetadataReservation>,
    pub _artifact_budget: Arc<Reservation>,
}
impl TaskFile {
    pub(crate) fn bytes(&self) -> Option<u64> {
        self.actual.or(self.declared)
    }
    /// Mirrors upstream `_upload_descriptor`: only an actionable upload shows.
    pub(crate) fn upload_available(&self, task: &TaskRecord, now: i64) -> bool {
        self.url.is_none()
            && !self.uploaded
            && !self.upload_active
            && self.status == TaskFileState::WaitingFile
            && task.status != TaskStatus::Canceled
            && task.expires_at > now
    }
}
pub(crate) fn task_terminal(status: TaskStatus) -> bool {
    matches!(
        status,
        TaskStatus::Completed | TaskStatus::Partial | TaskStatus::Failed | TaskStatus::Canceled
    )
}
pub(crate) fn timestamp() -> String {
    time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default()
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
                tasks: Mutex::new(HashMap::new()),
                files: Mutex::new(HashMap::new()),
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
    pub(crate) fn ingest_body_cap(&self) -> usize {
        self.body_cap().min(INPUT_CAP as usize)
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
        self.reserve(bytes)
            .then(|| Arc::new(Reservation::input(bytes, &self.inner)))
    }
    pub(crate) fn artifact_budget(&self, bytes: u64) -> Option<Arc<Reservation>> {
        self.inner
            .artifact_used
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                n.checked_add(bytes).filter(|v| *v <= 256 * 1024 * 1024)
            })
            .ok()?;
        Some(Arc::new(Reservation::artifact(bytes, &self.inner)))
    }
    pub(crate) fn reserve_metadata(&self, count: usize) -> Option<Arc<MetadataReservation>> {
        self.inner
            .meta_used
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                n.checked_add(count).filter(|v| *v <= META_CAP)
            })
            .ok()?;
        Some(Arc::new(MetadataReservation::new(count, &self.inner)))
    }
    pub(crate) fn charged(&self, data: Vec<u8>, reservation: Arc<Reservation>) -> Bytes {
        Charged::new(data, reservation).into_bytes()
    }
    pub(crate) fn retention_secs(&self) -> u64 {
        self.retention.as_secs()
    }
    pub(crate) fn max_concurrent(&self) -> usize {
        self.max_concurrent
    }
    pub(crate) fn ingest_concurrency(&self) -> usize {
        self.max_concurrent
            .min((INPUT_CAP / self.ingest_body_cap().max(1) as u64) as usize)
            .max(1)
    }
    pub(crate) fn clear_retained(&self) {
        let removed = self
            .inner
            .tasks
            .lock()
            .map(|mut m| std::mem::take(&mut *m))
            .unwrap_or_default();
        let files = self
            .inner
            .files
            .lock()
            .map(|mut m| std::mem::take(&mut *m))
            .unwrap_or_default();
        drop(removed);
        drop(files);
    }
    pub(crate) fn lookup(&self, id: &str) -> Option<Arc<AsyncMutex<TaskRecord>>> {
        self.inner.tasks.lock().ok()?.get(id).cloned()
    }
    pub(crate) fn insert_task(&self, id: String, record: Arc<AsyncMutex<TaskRecord>>) {
        if let Ok(mut tasks) = self.inner.tasks.lock() {
            tasks.insert(id, record);
        }
    }
    /// Recompute the aggregate status, finishing the task once every file ends.
    pub(crate) fn aggregate(&self, task: &mut TaskRecord) {
        if task.status == TaskStatus::Canceled {
            return;
        }
        let (mut running, mut pending, mut waiting, mut completed) = (false, false, false, 0u32);
        for file in &task.files {
            match file.status {
                TaskFileState::Running => running = true,
                TaskFileState::Pending => pending = true,
                TaskFileState::WaitingFile => waiting = true,
                TaskFileState::Completed => completed += 1,
                TaskFileState::Failed | TaskFileState::Canceled => {}
            }
        }
        task.status = if running {
            TaskStatus::Running
        } else if pending {
            TaskStatus::Pending
        } else if waiting {
            TaskStatus::WaitingFile
        } else if completed as usize == task.files.len() {
            TaskStatus::Completed
        } else if completed > 0 {
            TaskStatus::Partial
        } else {
            TaskStatus::Failed
        };
        if task_terminal(task.status) && task.finished_at.is_none() {
            task.finished_at = Some(timestamp());
            task.retention_at = Some(Self::now() + self.retention_secs() as i64);
        }
    }
    /// Mirrors upstream `update_lease`: a caller-supplied lease extends how long
    /// the task's uploads and finished results stay retrievable.
    pub(crate) fn update_lease(&self, task: &mut TaskRecord, lease: Option<u64>) {
        let Some(seconds) = lease else {
            return;
        };
        let deadline = Self::now() + seconds as i64;
        task.expires_at = task.expires_at.max(deadline);
        task.retention_at = task.retention_at.map(|at| at.max(deadline));
    }
    /// Mirrors upstream `_expire_uploads`: end files that never uploaded.
    pub(crate) fn expire_uploads(&self, task: &mut TaskRecord) {
        if task.expires_at > Self::now() || task.status == TaskStatus::Canceled {
            return;
        }
        for file in &mut task.files {
            if file.url.is_none()
                && file.data.is_none()
                && file.status == TaskFileState::WaitingFile
                && !file.upload_active
            {
                file.expired = true;
                file.status = TaskFileState::Failed;
                file.error = Some(ErrorDetail {
                    code: "upload_expired".into(),
                    message: "Upload expired".into(),
                    param: None,
                });
            }
        }
        self.aggregate(task);
    }
    pub(crate) fn file(&self, id: &str) -> Option<FileObjectModel> {
        self.content(id).map(|(object, _)| object)
    }
    pub(crate) fn content(&self, id: &str) -> Option<(FileObjectModel, Bytes)> {
        let files = self.inner.files.lock().ok()?;
        let file = files.get(id)?;
        if file
            .object
            .expires_at
            .is_some_and(|expiry| expiry <= Self::now())
        {
            return None;
        }
        Some((file.object.clone(), file.bytes.clone()))
    }
    pub(crate) fn publish_artifact_batch(
        &self,
        files: Vec<(FileObjectModel, Vec<u8>)>,
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
        let mut entries = match self.inner.files.lock() {
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
                StoredFile {
                    object,
                    bytes,
                    _metadata: file_metadata,
                },
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
        let now = Self::now();
        if let Ok(mut files) = self.inner.files.lock() {
            files.retain(|_, file| file.object.expires_at.is_none_or(|expiry| expiry > now));
        }
        if let Ok(mut tasks) = self.inner.tasks.lock() {
            tasks.retain(|_, task| {
                if Arc::strong_count(task) > 1 {
                    return true;
                }
                match task.try_lock() {
                    Ok(state) => state.retention_at.is_none_or(|at| at > now),
                    Err(_) => true,
                }
            });
        }
    }
}
