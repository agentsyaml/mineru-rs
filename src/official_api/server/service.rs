use super::{jobs, store::OfficialFileStore};
use crate::official::{
    OfficialError, OfficialOcrMode, OfficialParser, OfficialRuntime, OfficialTier,
};
use std::{
    collections::{HashMap, VecDeque},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};
use tokio::{
    sync::{Mutex as AsyncMutex, Notify},
    task::JoinHandle,
};

pub(super) struct ServiceInner {
    pub parser: OfficialParser,
    pub store: OfficialFileStore,
    pub jobs: Mutex<HashMap<String, Arc<AsyncMutex<JobRecord>>>>,
    pub queue: Mutex<VecDeque<String>>,
    pub notify: Notify,
    pub started: AtomicBool,
    pub stopping: AtomicBool,
    pub jobs_created: AtomicU64,
    pub pages_processed: AtomicU64,
    pub files_processed: AtomicU64,
    #[cfg(test)]
    pub commit_pause: Mutex<Option<(std::sync::mpsc::Sender<()>, std::sync::mpsc::Receiver<()>)>>,
    #[cfg(test)]
    pub runner_empty_pause: Mutex<
        Option<(
            tokio::sync::oneshot::Sender<()>,
            tokio::sync::oneshot::Receiver<()>,
        )>,
    >,
    #[cfg(test)]
    pub publication_pause: Mutex<
        Option<(
            tokio::sync::oneshot::Sender<()>,
            tokio::sync::oneshot::Receiver<()>,
        )>,
    >,
    #[cfg(test)]
    pub stop_waiting: Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
}
pub(super) struct JobRecord {
    pub response: crate::official_api::wire::JobResponse,
    pub files: Vec<PreparedFile>,
    pub cancel: Arc<AtomicBool>,
    pub _slot: tokio::sync::OwnedSemaphorePermit,
    pub _metadata: Arc<super::store::MetadataReservation>,
    pub _artifact_budget: Arc<super::store::Reservation>,
    pub expiry: Option<i64>,
    pub ocr_mode: OfficialOcrMode,
}
#[derive(Clone)]
pub(super) struct PreparedFile {
    pub file_id: Option<String>,
    pub name: String,
    pub data: bytes::Bytes,
    pub page_range: Option<String>,
    pub tier: OfficialTier,
}
struct Lifecycle {
    runner: Option<JoinHandle<()>>,
}
#[derive(Clone)]
pub struct OfficialService {
    pub(super) inner: Arc<ServiceInner>,
    lifecycle: Arc<Mutex<Lifecycle>>,
}
impl OfficialService {
    pub fn new(runtime: OfficialRuntime, store: OfficialFileStore) -> Result<Self, OfficialError> {
        let parser = OfficialParser::new(runtime.clone())?;
        Ok(Self {
            inner: Arc::new(ServiceInner {
                parser,
                store,
                jobs: Mutex::new(HashMap::new()),
                queue: Mutex::new(VecDeque::new()),
                notify: Notify::new(),
                started: AtomicBool::new(false),
                stopping: AtomicBool::new(false),
                jobs_created: AtomicU64::new(0),
                pages_processed: AtomicU64::new(0),
                files_processed: AtomicU64::new(0),
                #[cfg(test)]
                commit_pause: Mutex::new(None),
                #[cfg(test)]
                runner_empty_pause: Mutex::new(None),
                #[cfg(test)]
                publication_pause: Mutex::new(None),
                #[cfg(test)]
                stop_waiting: Mutex::new(None),
            }),
            lifecycle: Arc::new(Mutex::new(Lifecycle { runner: None })),
        })
    }
    pub fn router(&self) -> axum::Router {
        let body_cap = self.inner.store.body_cap();
        let job_routes = axum::Router::new()
            .route(
                "/v1/parse/jobs",
                axum::routing::post(jobs::create).get(jobs::list),
            )
            .route(
                "/v1/parse/jobs/{id}",
                axum::routing::get(jobs::get).delete(jobs::cancel),
            )
            .with_state(self.inner.clone());
        axum::Router::new()
            .merge(super::super::official_files_router(
                self.inner.store.clone(),
            ))
            .merge(job_routes)
            .merge(super::tasks::router(self.inner.clone()))
            .merge(super::discovery::router(self.inner.clone()))
            .layer(axum::extract::DefaultBodyLimit::max(body_cap))
    }
    pub fn start(&self) -> Result<(), OfficialError> {
        if self.inner.started.swap(true, Ordering::AcqRel) {
            return Ok(());
        }
        let inner = self.inner.clone();
        let task = tokio::spawn(async move {
            runner(inner).await;
        });
        self.lifecycle.lock().map_err(|_| internal())?.runner = Some(task);
        Ok(())
    }
    pub fn cleanup_expired(&self) {
        self.inner.store.cleanup_expired();
        if let Ok(mut jobs) = self.inner.jobs.lock() {
            let now = super::store::OfficialFileStore::now();
            jobs.retain(|_, job| {
                Arc::strong_count(job) > 1
                    || job
                        .try_lock()
                        .map(|j| {
                            matches!(j.response.status.as_str(), "queued" | "running")
                                || j.expiry.is_none_or(|expiry| expiry > now)
                        })
                        .unwrap_or(true)
            });
        }
    }
    pub fn stop_admissions(&self) {
        #[cfg(test)]
        if let Some(waiting) = self
            .inner
            .stop_waiting
            .lock()
            .ok()
            .and_then(|mut v| v.take())
        {
            let _ = waiting.send(());
        }
        {
            let _admission = self
                .inner
                .store
                .inner
                .admission
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            self.inner.stopping.store(true, Ordering::Release);
            self.inner
                .store
                .inner
                .accepting
                .store(false, Ordering::Release);
        }
        if let Ok(jobs) = self.inner.jobs.lock() {
            for job in jobs.values() {
                if let Ok(record) = job.try_lock()
                    && matches!(record.response.status.as_str(), "queued" | "running")
                {
                    record.cancel.store(true, Ordering::Release);
                }
            }
        }
        self.inner.notify.notify_waiters();
    }
    pub async fn shutdown(&self) -> Result<(), OfficialError> {
        self.stop_admissions();
        let runner = self.lifecycle.lock().map_err(|_| internal())?.runner.take();
        if let Some(runner) = runner {
            runner.await.map_err(|_| internal())?;
        }
        self.inner.parser.shutdown().await?;
        if let Ok(mut jobs) = self.inner.jobs.lock() {
            jobs.clear();
        }
        self.inner.store.clear_retained();
        Ok(())
    }
}
fn internal() -> OfficialError {
    OfficialError {
        kind: crate::official::OfficialErrorKind::RuntimeUnavailable,
        message: "official service state unavailable".into(),
    }
}

async fn runner(inner: Arc<ServiceInner>) {
    loop {
        let notified = inner.notify.notified();
        let id = inner.queue.lock().ok().and_then(|mut q| q.pop_front());
        if let Some(id) = id {
            jobs::run_job(inner.clone(), id).await;
            continue;
        }
        #[cfg(test)]
        {
            let pause = inner
                .runner_empty_pause
                .lock()
                .ok()
                .and_then(|mut p| p.take());
            if let Some((ready, resume)) = pause {
                let _ = ready.send(());
                let _ = resume.await;
            }
        }
        let _admission = inner
            .store
            .inner
            .admission
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let stopping = inner.stopping.load(Ordering::Acquire);
        let has_queued = inner.queue.lock().is_ok_and(|q| !q.is_empty());
        drop(_admission);
        if stopping && !has_queued {
            break;
        }
        notified.await;
    }
}
