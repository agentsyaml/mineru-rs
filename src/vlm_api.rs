//! Deliberately small protocol-2 task service.
use crate::{
    MinerUVlmClient, MinerUVlmConfig, OfficeWorkers, OfficialOutputManifest, OfficialPdfOptions,
    ProgressCallback, ProgressEvent, VlmHttpConfig,
    input_prepare::{DocumentKind, RasterWorkers, prepare_with_warning},
};
use axum::{
    Json, Router,
    body::Body,
    extract::{
        DefaultBodyLimit, FromRequestParts, Multipart, Path, State,
        multipart::{MultipartError, MultipartRejection},
    },
    http::{StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use bytes::Bytes;
use cap_fs_ext::{DirExt, FollowSymlinks, OpenOptionsFollowExt};
use cap_std::{
    ambient_authority,
    fs::{Dir, OpenOptions},
};
use futures_util::{FutureExt, stream};
use serde_json::json;
use std::{
    collections::{HashMap, HashSet},
    future::Future,
    io::{Read, Seek, SeekFrom, Write},
    path::{Path as FsPath, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};
use tempfile::TempDir;
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use tokio::{
    sync::{OwnedSemaphorePermit, Semaphore, oneshot},
    task::JoinSet,
};

const FILE_CAP: u64 = 512 * 1024 * 1024;
const BODY_CAP: usize = 512 * 1024 * 1024 + 1024 * 1024;
const TEXT_CAP: usize = 64 * 1024;
const TEXT_TOTAL_CAP: usize = 256 * 1024;
const RECORD_CAP: usize = 32;
const RETENTION: Duration = Duration::from_secs(24 * 60 * 60);
const CLEANUP_INTERVAL: Duration = Duration::from_secs(300);
const REQUEST_DEADLINE_EXPIRED: &str = "request deadline expired";

#[derive(Clone)]
struct App {
    public_listener: bool,
    allow_public_http_client: bool,
    records: Arc<Mutex<HashMap<String, Arc<Record>>>>,
    slots: Arc<Semaphore>,
    gate: Arc<Semaphore>,
    ids: Arc<AtomicU64>,
    output_root: PathBuf,
    route: OfficialPdfOptions,
    env_formula: Option<bool>,
    env_table: Option<bool>,
    concurrency: usize,
    official_page_concurrency: usize,
    retention: Duration,
    cleanup_interval: Duration,
    workers: Arc<Mutex<Option<WorkerRegistry>>>,
    office_workers: OfficeWorkers,
    raster_workers: RasterWorkers,
    limits: RequestLimits,
    server_zip_cap: u64,
    totals: crate::document_limits::OfficialDocumentTotals,
    events: Option<ProgressCallback>,
    #[cfg(test)]
    test_http: Option<VlmHttpConfig>,
}
struct WorkerRegistry {
    tasks: JoinSet<()>,
    associations: HashMap<tokio::task::Id, WorkerAssociation>,
}
impl WorkerRegistry {
    fn new() -> Self {
        Self {
            tasks: JoinSet::new(),
            associations: HashMap::new(),
        }
    }
}
type SyncOutcome = Result<ResultFile, (StatusCode, String)>;
#[derive(Clone)]
struct SyncCompletion {
    sender: Arc<Mutex<Option<oneshot::Sender<SyncOutcome>>>>,
    events: Option<ProgressCallback>,
    label: String,
    document: String,
}
impl SyncCompletion {
    #[cfg(test)]
    fn new(sender: oneshot::Sender<SyncOutcome>) -> Self {
        Self::with_events(sender, None, String::new())
    }
    fn with_events(
        sender: oneshot::Sender<SyncOutcome>,
        events: Option<ProgressCallback>,
        label: String,
    ) -> Self {
        Self {
            sender: Arc::new(Mutex::new(Some(sender))),
            events,
            document: label.clone(),
            label,
        }
    }
    fn complete(&self, outcome: SyncOutcome) {
        if let Some(sender) = self.take_sender() {
            let failure = match &outcome {
                Ok(_) => None,
                Err((_, message)) => Some(message.clone()),
            };
            Self::deliver(Some(sender), outcome);
            if let Some(message) = failure {
                crate::progress_events::emit(
                    &self.events,
                    ProgressEvent::DocumentFailed {
                        document: self.document.clone(),
                        message: message.clone(),
                    },
                );
                crate::progress_events::emit(
                    &self.events,
                    ProgressEvent::RequestFailed {
                        label: self.label.clone(),
                        message,
                    },
                );
            } else {
                crate::progress_events::emit(
                    &self.events,
                    ProgressEvent::RequestCompleted {
                        label: self.label.clone(),
                    },
                );
            }
        }
    }
    fn take_sender(&self) -> Option<oneshot::Sender<SyncOutcome>> {
        self.sender
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
    }
    fn deliver(sender: Option<oneshot::Sender<SyncOutcome>>, outcome: SyncOutcome) {
        if let Some(sender) = sender {
            drop(sender.send(outcome));
        } else {
            drop(outcome);
        }
    }
}
enum WorkerAssociation {
    Async(String),
    Sync(SyncCompletion),
}
struct SyncWorkerGuard {
    input: Option<Arc<JobInput>>,
    completion: SyncCompletion,
    armed: bool,
}
impl SyncWorkerGuard {
    fn new(input: Arc<JobInput>, completion: SyncCompletion) -> Self {
        Self {
            input: Some(input),
            completion,
            armed: true,
        }
    }
    fn success(mut self, mut result: ResultFile) {
        self.armed = false;
        result.keepalive = self.input.take();
        self.completion.complete(Ok(result));
    }
    fn failure(mut self, error: (StatusCode, String)) {
        self.armed = false;
        drop(self.input.take());
        self.completion.complete(Err(error));
    }
    fn discard(mut self) {
        drop(self.completion.take_sender());
        self.armed = false;
        drop(self.input.take());
    }
}
impl Drop for SyncWorkerGuard {
    fn drop(&mut self) {
        if self.armed {
            self.armed = false;
            // Approved service flow never aborts this task; arbitrary cancellation while pathful work is awaited is unsafe/out of scope.
            if let Some(input) = self.input.take() {
                cleanup_failure(input.root.path(), &input.stem);
                drop(input);
            }
            self.completion.complete(Err((
                StatusCode::CONFLICT,
                "task worker terminated unexpectedly".into(),
            )));
        }
    }
}
#[derive(Clone, Copy)]
struct RequestLimits {
    body: usize,
    file: u64,
    text: usize,
    text_total: usize,
    fields: usize,
}
#[derive(Clone)]
struct WorkerContext {
    gate: Arc<Semaphore>,
    route: OfficialPdfOptions,
    env_formula: Option<bool>,
    env_table: Option<bool>,
    official_page_concurrency: usize,
    office_workers: OfficeWorkers,
    raster_workers: RasterWorkers,
    events: Option<ProgressCallback>,
    server_zip_cap: u64,
    totals: crate::document_limits::OfficialDocumentTotals,
    #[cfg(test)]
    test_http: Option<VlmHttpConfig>,
}
#[doc(hidden)]
#[derive(Clone)]
pub struct ServiceConfig {
    pub concurrency: usize,
    pub output_root: PathBuf,
    pub route: OfficialPdfOptions,
    pub formula: Option<bool>,
    pub table: Option<bool>,
    official_page_concurrency: usize,
    public_bind_exposed: bool,
    allow_public_http_client: bool,
    retention: Duration,
    cleanup_interval: Duration,
    record_cap: usize,
    limits: RequestLimits,
    document_limits: Option<crate::DocumentLimitPolicy>,
    #[doc(hidden)]
    progress_callback: Option<ProgressCallback>,
    #[cfg(test)]
    test_http: Option<VlmHttpConfig>,
    #[cfg(test)]
    test_office: Option<OfficeWorkers>,
    #[cfg(test)]
    shutdown_probe: Option<tokio::sync::mpsc::UnboundedSender<&'static str>>,
}
impl ServiceConfig {
    pub fn new(
        concurrency: usize,
        output_root: PathBuf,
        route: OfficialPdfOptions,
        formula: Option<bool>,
        table: Option<bool>,
    ) -> Result<Self, String> {
        if concurrency == 0 || concurrency > Semaphore::MAX_PERMITS {
            Err("MINERU_API_MAX_CONCURRENT_REQUESTS must be positive".into())
        } else {
            Ok(Self {
                concurrency,
                output_root,
                route,
                formula,
                table,
                official_page_concurrency: 4,
                public_bind_exposed: false,
                allow_public_http_client: false,
                retention: RETENTION,
                cleanup_interval: CLEANUP_INTERVAL,
                record_cap: RECORD_CAP,
                limits: RequestLimits {
                    body: BODY_CAP,
                    file: FILE_CAP,
                    text: TEXT_CAP,
                    text_total: TEXT_TOTAL_CAP,
                    fields: 32,
                },
                document_limits: None,
                progress_callback: None,
                #[cfg(test)]
                test_http: None,
                #[cfg(test)]
                test_office: None,
                #[cfg(test)]
                shutdown_probe: None,
            })
        }
    }
    #[doc(hidden)]
    pub fn official_page_concurrency(mut self, concurrency: usize) -> Result<Self, String> {
        if !(1..=8).contains(&concurrency) {
            return Err("MINERU_OFFICIAL_PAGE_CONCURRENCY must be an integer from 1 to 8".into());
        }
        self.official_page_concurrency = concurrency;
        Ok(self)
    }
    #[doc(hidden)]
    pub fn public_policy(mut self, exposed: bool, allow_http_client: bool) -> Self {
        self.public_bind_exposed = exposed;
        self.allow_public_http_client = allow_http_client;
        self
    }
    #[doc(hidden)]
    pub fn document_limits(mut self, limits: crate::DocumentLimitPolicy) -> Self {
        self.document_limits = Some(limits);
        self
    }
    #[doc(hidden)]
    pub fn progress_callback(mut self, callback: ProgressCallback) -> Self {
        self.progress_callback = Some(callback);
        self
    }
    #[doc(hidden)]
    pub fn task_lifecycle(
        mut self,
        retention: Duration,
        cleanup_interval: Duration,
    ) -> Result<Self, String> {
        if cleanup_interval.is_zero() {
            return Err("MINERU_API_TASK_CLEANUP_INTERVAL_SECONDS must be positive".into());
        }
        self.retention = retention;
        self.cleanup_interval = cleanup_interval;
        Ok(self)
    }
    #[cfg(test)]
    fn test_limits(
        mut self,
        limits: RequestLimits,
        record_cap: usize,
        retention: Duration,
        cleanup_interval: Duration,
    ) -> Self {
        self.limits = limits;
        self.record_cap = record_cap;
        self.retention = retention;
        self.cleanup_interval = cleanup_interval;
        self
    }
    #[cfg(test)]
    pub(crate) fn test_http(mut self, config: VlmHttpConfig) -> Self {
        self.test_http = Some(config);
        self
    }
    #[cfg(test)]
    pub(crate) fn test_office(mut self, workers: OfficeWorkers) -> Self {
        self.test_office = Some(workers);
        self
    }
    #[cfg(test)]
    fn test_shutdown_probe(
        mut self,
        probe: tokio::sync::mpsc::UnboundedSender<&'static str>,
    ) -> Self {
        self.shutdown_probe = Some(probe);
        self
    }
}
struct Record {
    sequence: u64,
    base_url: String,
    input: JobInput,
    state: Mutex<TaskState>,
    created_at: String,
}
struct JobInput {
    root: TempDir,
    _slot: OwnedSemaphorePermit,
    deadline: Instant,
    stem: String,
    canonical_filename: String,
    kind: DocumentKind,
    upload: PathBuf,
    options: Submit,
}
#[derive(Clone)]
enum TaskState {
    Pending,
    Processing {
        started_at: String,
    },
    Completed {
        result: ResultFile,
        started_at: String,
        completed_at: String,
        terminal_at: Instant,
    },
    Failed {
        error: String,
        started_at: Option<String>,
        completed_at: String,
        terminal_at: Instant,
    },
}
struct Submit {
    server_url: Option<String>,
    backend: Option<String>,
    language: Option<String>,
    effort: Option<String>,
    parse_method: Option<String>,
    start: u64,
    end: u64,
    formula: bool,
    table: bool,
    image: bool,
    kind: Option<DocumentKind>,
    original_filename: Option<String>,
    md: bool,
    middle: bool,
    model: bool,
    content: bool,
    images: bool,
    zip: bool,
    origin: bool,
    client_side: bool,
}
impl Default for Submit {
    fn default() -> Self {
        Self {
            server_url: None,
            backend: Some("hybrid-engine".into()),
            language: Some("ch".into()),
            effort: Some("medium".into()),
            parse_method: Some("auto".into()),
            start: 0,
            end: 99_999,
            formula: true,
            table: true,
            image: true,
            kind: None,
            original_filename: None,
            md: true,
            middle: false,
            model: false,
            content: false,
            images: false,
            zip: false,
            origin: false,
            client_side: false,
        }
    }
}
#[derive(Clone)]
struct ResultFile {
    path: PathBuf,
    content_type: &'static str,
    keepalive: Option<Arc<JobInput>>,
}

/// Serve protocol 2 on an already-bound listener.
pub async fn serve(
    listener: tokio::net::TcpListener,
    config: ServiceConfig,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> Result<(), std::io::Error> {
    let transport_policy = config
        .document_limits
        .unwrap_or_else(crate::DocumentLimitPolicy::defaults);
    let totals = config
        .document_limits
        .map(crate::document_limits::OfficialDocumentTotals::from_policy)
        .unwrap_or_else(|| {
            crate::document_limits::OfficialDocumentTotals::from_options(&config.route)
        });
    let body = crate::document_limits::usize_with_max(
        transport_policy.multipart_body_bytes,
        usize::MAX as u64,
    )
    .map_err(std::io::Error::other)?;
    let policy_limits = RequestLimits {
        body,
        file: transport_policy.max_input_bytes,
        ..config.limits
    };
    #[cfg(test)]
    let limits = if config.limits.body != BODY_CAP || config.limits.file != FILE_CAP {
        config.limits
    } else {
        policy_limits
    };
    #[cfg(not(test))]
    let limits = policy_limits;
    let addr = listener.local_addr()?;
    let public_listener = !addr.ip().is_loopback();
    if public_listener && !config.public_bind_exposed {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "vlm API requires a loopback listener",
        ));
    }
    std::fs::create_dir_all(&config.output_root)?;
    #[cfg(test)]
    let shutdown_probe = config.shutdown_probe.clone();
    #[cfg(test)]
    let office_workers = match config.test_office.clone() {
        Some(workers) => workers,
        None => OfficeWorkers::new().map_err(|error| std::io::Error::other(error.to_string()))?,
    };
    #[cfg(not(test))]
    let office_workers =
        OfficeWorkers::new().map_err(|error| std::io::Error::other(error.to_string()))?;
    let app = App {
        public_listener,
        allow_public_http_client: config.allow_public_http_client,
        records: Arc::new(Mutex::new(HashMap::new())),
        slots: Arc::new(Semaphore::new(config.record_cap)),
        gate: Arc::new(Semaphore::new(config.concurrency)),
        ids: Arc::new(AtomicU64::new(1)),
        output_root: config.output_root,
        route: config.route,
        env_formula: config.formula,
        env_table: config.table,
        official_page_concurrency: config.official_page_concurrency,
        concurrency: config.concurrency,
        retention: config.retention,
        cleanup_interval: config.cleanup_interval,
        workers: Arc::new(Mutex::new(Some(WorkerRegistry::new()))),
        office_workers,
        raster_workers: RasterWorkers::default(),
        limits,
        server_zip_cap: transport_policy.server_zip_bytes,
        totals,
        events: config.progress_callback,
        #[cfg(test)]
        test_http: config.test_http,
    };
    let (cleanup_stop, mut cleanup_stopped) = oneshot::channel();
    let cleanup_app = app.clone();
    let cleanup = tokio::spawn(async move {
        let mut tick = tokio::time::interval(cleanup_app.cleanup_interval);
        tick.tick().await;
        loop {
            tokio::select! {
                _ = tick.tick() => { cleanup_records(&cleanup_app.records, cleanup_app.retention); reap_workers(&cleanup_app.workers, &cleanup_app.records, &cleanup_app.events).await; }
                _ = &mut cleanup_stopped => break,
            }
        }
    });
    let router = Router::new()
        .route("/health", get(health))
        .route("/tasks", post(submit))
        .route("/file_parse", post(file_parse))
        .route("/tasks/{id}", get(status))
        .route("/tasks/{id}/result", get(result))
        .layer(DefaultBodyLimit::max(app.limits.body))
        .with_state(app.clone());
    crate::progress_events::emit(
        &app.events,
        ProgressEvent::ServerStarted {
            address: addr.to_string(),
        },
    );
    let result = axum::serve(listener, router)
        .with_graceful_shutdown(shutdown)
        .await;
    let _ = cleanup_stop.send(());
    let _ = cleanup.await;
    let workers = { app.workers.lock().unwrap().take() };
    if let Some(mut workers) = workers {
        while let Some(exit) = workers.tasks.join_next_with_id().await {
            let association = match exit {
                Ok((id, ())) => workers.associations.remove(&id),
                Err(error) => workers.associations.remove(&error.id()),
            };
            if let Some(association) = association {
                inspect_worker_exit(&app.records, association, &app.events).await;
            }
        }
    }
    #[cfg(test)]
    if let Some(probe) = &shutdown_probe {
        let _ = probe.send("service_workers");
    }
    app.office_workers.drain().await;
    #[cfg(test)]
    if let Some(probe) = &shutdown_probe {
        let _ = probe.send("office");
    }
    app.raster_workers.drain().await;
    #[cfg(test)]
    if let Some(probe) = &shutdown_probe {
        let _ = probe.send("raster");
    }
    app.records.lock().unwrap().clear();
    #[cfg(test)]
    if let Some(probe) = &shutdown_probe {
        let _ = probe.send("records");
    }
    crate::progress_events::emit(&app.events, ProgressEvent::ServerStopped);
    result
}
fn error(code: StatusCode, message: &'static str) -> Response {
    (code, Json(json!({"detail":message}))).into_response()
}
struct PublicPostPolicy;
impl FromRequestParts<App> for PublicPostPolicy {
    type Rejection = Response;

    async fn from_request_parts(
        _: &mut axum::http::request::Parts,
        app: &App,
    ) -> Result<Self, Self::Rejection> {
        if app.public_listener && !app.allow_public_http_client {
            rejected(&app.events, "public HTTP-client requests are disabled");
            return Err(error(
                StatusCode::BAD_REQUEST,
                "public HTTP-client requests are disabled",
            ));
        }
        Ok(Self)
    }
}
struct RequestAuthority(String);
fn parse_request_authority(raw: &str) -> Option<(String, Option<u16>)> {
    raw.parse::<axum::http::uri::Authority>().ok()?;
    if raw.is_empty() || raw.contains(['@', '/', '?', '#', ',']) {
        return None;
    }
    let (host, port) = if let Some(rest) = raw.strip_prefix('[') {
        let (host, rest) = rest.split_once(']')?;
        host.parse::<std::net::Ipv6Addr>().ok()?;
        if rest.is_empty() {
            (host, None)
        } else {
            (host, Some(rest.strip_prefix(':')?))
        }
    } else {
        let mut parts = raw.split(':');
        let host = parts.next()?;
        if host.is_empty() || host.contains(['[', ']']) {
            return None;
        }
        (host, parts.next())
    };
    if raw.starts_with('[') && raw.matches(']').count() != 1
        || (!raw.starts_with('[') && raw.matches(':').count() > 1)
    {
        return None;
    }
    let port = match port {
        Some(port) if !port.is_empty() && port.bytes().all(|byte| byte.is_ascii_digit()) => {
            Some(port.parse().ok()?)
        }
        Some(_) => return None,
        None => None,
    };
    Some((host.into(), port))
}
fn request_authority(parts: &axum::http::request::Parts) -> Option<String> {
    fn parse(value: &axum::http::HeaderValue) -> Option<(String, Option<u16>)> {
        parse_request_authority(value.to_str().ok()?)
    }
    let hosts: Vec<_> = parts.headers.get_all(header::HOST).iter().collect();
    match (parts.uri.scheme(), parts.uri.authority()) {
        (None, None) if hosts.len() == 1 => hosts[0]
            .to_str()
            .ok()
            .and_then(|value| parse_request_authority(value).map(|_| value.to_owned())),
        (Some(scheme), Some(uri_authority)) if scheme == "http" && hosts.len() <= 1 => {
            let uri = parse_request_authority(uri_authority.as_str())?;
            if let Some(host) = hosts.first() {
                let host = parse(host)?;
                if !host.0.eq_ignore_ascii_case(&uri.0) || host.1 != uri.1 {
                    return None;
                }
            }
            Some(uri_authority.to_string())
        }
        _ => None,
    }
}
impl FromRequestParts<App> for RequestAuthority {
    type Rejection = Response;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        app: &App,
    ) -> Result<Self, Self::Rejection> {
        fn invalid() -> Response {
            error(StatusCode::BAD_REQUEST, "invalid request authority")
        }
        Ok(Self(request_authority(parts).ok_or_else(|| {
            rejected(&app.events, "invalid request authority");
            invalid()
        })?))
    }
}
fn rejected(events: &Option<ProgressCallback>, message: &'static str) {
    crate::progress_events::emit(
        events,
        ProgressEvent::RequestRejected {
            message: message.into(),
        },
    );
}
fn timestamp() -> String {
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .expect("system time must format as RFC3339")
}
fn record_state(record: &Record) -> std::sync::MutexGuard<'_, TaskState> {
    record
        .state
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}
fn state_snapshot(record: &Record) -> TaskState {
    record_state(record).clone()
}
async fn health(State(app): State<App>) -> Json<serde_json::Value> {
    Json(
        json!({"status":"healthy","protocol_version":2,"max_concurrent_requests":app.concurrency,"processing_window_size":app.route.processing_window_size,"task_count":app.records.lock().unwrap().len()}),
    )
}

fn reserve(app: &App) -> Option<OwnedSemaphorePermit> {
    let mut records = app.records.lock().unwrap();
    remove_expired(&mut records, app.retention);
    let mut permit = app.slots.clone().try_acquire_owned().ok();
    while permit.is_none() {
        let oldest = records
            .iter()
            .filter_map(|(id, r)| match state_snapshot(r) {
                TaskState::Completed { terminal_at, .. }
                | TaskState::Failed { terminal_at, .. } => Some((id.clone(), terminal_at)),
                _ => None,
            })
            .min_by_key(|(_, at)| *at)
            .map(|(id, _)| id);
        if let Some(id) = oldest {
            records.remove(&id);
            permit = app.slots.clone().try_acquire_owned().ok();
        } else {
            return None;
        }
    }
    permit
}
fn remove_expired(records: &mut HashMap<String, Arc<Record>>, retention: Duration) {
    let now = Instant::now();
    records.retain(|_, r| !matches!(state_snapshot(r), TaskState::Completed { terminal_at, .. } | TaskState::Failed { terminal_at, .. } if now.duration_since(terminal_at) >= retention));
}
fn cleanup_records(records: &Arc<Mutex<HashMap<String, Arc<Record>>>>, retention: Duration) {
    remove_expired(&mut records.lock().unwrap(), retention);
}
async fn reap_workers(
    workers: &Arc<Mutex<Option<WorkerRegistry>>>,
    records: &Arc<Mutex<HashMap<String, Arc<Record>>>>,
    events: &Option<ProgressCallback>,
) {
    let mut ids = Vec::new();
    {
        let mut workers = workers.lock().unwrap();
        let Some(workers) = workers.as_mut() else {
            return;
        };
        while let Some(exit) = workers.tasks.try_join_next_with_id() {
            let association = match exit {
                Ok((id, ())) => workers.associations.remove(&id),
                Err(error) => workers.associations.remove(&error.id()),
            };
            if let Some(association) = association {
                ids.push(association);
            }
        }
    }
    for association in ids {
        inspect_worker_exit(records, association, events).await;
    }
}
async fn inspect_worker_exit(
    records: &Arc<Mutex<HashMap<String, Arc<Record>>>>,
    association: WorkerAssociation,
    events: &Option<ProgressCallback>,
) {
    match association {
        WorkerAssociation::Async(id) => {
            let record = { records.lock().unwrap().get(&id).cloned() };
            if let Some(record) = record {
                if matches!(
                    state_snapshot(&record),
                    TaskState::Pending | TaskState::Processing { .. }
                ) {
                    if let Some(message) =
                        fail_record(&record, "task worker terminated unexpectedly").await
                    {
                        crate::progress_events::emit(
                            events,
                            ProgressEvent::DocumentFailed {
                                document: record.input.stem.clone(),
                                message: message.clone(),
                            },
                        );
                        crate::progress_events::emit(
                            events,
                            ProgressEvent::RequestFailed { label: id, message },
                        );
                    }
                }
            }
        }
        WorkerAssociation::Sync(completion) => completion.complete(Err((
            StatusCode::CONFLICT,
            "task worker terminated unexpectedly".into(),
        ))),
    }
}
async fn submit(
    State(app): State<App>,
    _: PublicPostPolicy,
    RequestAuthority(authority): RequestAuthority,
    form: Result<Multipart, MultipartRejection>,
) -> Response {
    let Some(deadline) = Instant::now().checked_add(app.route.total_deadline) else {
        rejected(&app.events, REQUEST_DEADLINE_EXPIRED);
        return error(StatusCode::REQUEST_TIMEOUT, REQUEST_DEADLINE_EXPIRED);
    };
    let mut form = match form {
        Ok(form) => form,
        Err(rejection) => {
            rejected(&app.events, "invalid multipart form");
            return rejection.into_response();
        }
    };
    reap_workers(&app.workers, &app.records, &app.events).await;
    let Some(slot) = reserve(&app) else {
        rejected(&app.events, "task capacity is full");
        return error(StatusCode::SERVICE_UNAVAILABLE, "task capacity is full");
    };
    let parsed = parse_form_until(&mut form, &app.output_root, app.limits, slot, deadline).await;
    match parsed {
        Ok(input) => {
            let sequence = app.ids.fetch_add(1, Ordering::Relaxed);
            let id = format!("local-{sequence}");
            let record = Arc::new(Record {
                sequence,
                base_url: format!("http://{authority}"),
                input,
                state: Mutex::new(TaskState::Pending),
                created_at: timestamp(),
            });
            app.records
                .lock()
                .unwrap()
                .insert(id.clone(), record.clone());
            let context = worker_context(&app);
            let value = status_snapshot(&app, &id, &record);
            let spawned = {
                let mut workers = app.workers.lock().unwrap();
                if let Some(workers) = workers.as_mut() {
                    crate::progress_events::emit(
                        &app.events,
                        ProgressEvent::RequestAccepted { label: id.clone() },
                    );
                    let task_id = workers
                        .tasks
                        .spawn(worker(
                            context,
                            record.clone(),
                            id.clone(),
                            app.events.clone(),
                        ))
                        .id();
                    workers
                        .associations
                        .insert(task_id, WorkerAssociation::Async(id.clone()));
                    true
                } else {
                    false
                }
            };
            if !spawned {
                fail_record(&record, "task service is shutting down").await;
                rejected(&app.events, "task service is shutting down");
                return error(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "task service is shutting down",
                );
            }
            let mut value = value;
            value["message"] = json!("Task submitted successfully");
            (StatusCode::ACCEPTED, Json(value)).into_response()
        }
        Err((code, msg)) => {
            rejected(&app.events, msg);
            error(code, msg)
        }
    }
}
async fn file_parse(
    State(app): State<App>,
    _: PublicPostPolicy,
    form: Result<Multipart, MultipartRejection>,
) -> Response {
    let Some(deadline) = Instant::now().checked_add(app.route.total_deadline) else {
        rejected(&app.events, REQUEST_DEADLINE_EXPIRED);
        return error(StatusCode::REQUEST_TIMEOUT, REQUEST_DEADLINE_EXPIRED);
    };
    let mut form = match form {
        Ok(form) => form,
        Err(rejection) => {
            rejected(&app.events, "invalid multipart form");
            return rejection.into_response();
        }
    };
    reap_workers(&app.workers, &app.records, &app.events).await;
    let Some(slot) = reserve(&app) else {
        rejected(&app.events, "task capacity is full");
        return error(StatusCode::SERVICE_UNAVAILABLE, "task capacity is full");
    };
    let input =
        match parse_form_until(&mut form, &app.output_root, app.limits, slot, deadline).await {
            Ok(input) => Arc::new(input),
            Err((code, message)) => {
                rejected(&app.events, message);
                return error(code, message);
            }
        };
    let (sender, receiver) = oneshot::channel();
    let label = input.stem.clone();
    let guard = SyncWorkerGuard::new(
        input,
        SyncCompletion::with_events(sender, app.events.clone(), label.clone()),
    );
    let receiver = match spawn_sync_worker(
        &app.workers,
        worker_context(&app),
        guard,
        &app.events,
        label,
    ) {
        Ok(()) => receiver,
        Err(guard) => {
            guard.discard();
            drop(receiver);
            rejected(&app.events, "task service is shutting down");
            return error(
                StatusCode::SERVICE_UNAVAILABLE,
                "task service is shutting down",
            );
        }
    };
    match receiver.await {
        Ok(Ok(result)) => stream_result(result, None).await,
        Ok(Err((status, message))) => (status, Json(json!({"detail":message}))).into_response(),
        Err(_) => error(StatusCode::CONFLICT, "task worker terminated unexpectedly"),
    }
}
async fn parse_form_until(
    form: &mut Multipart,
    output_root: &FsPath,
    limits: RequestLimits,
    slot: OwnedSemaphorePermit,
    deadline: Instant,
) -> Result<JobInput, (StatusCode, &'static str)> {
    tokio::time::timeout_at(
        tokio::time::Instant::from_std(deadline),
        parse_form(form, output_root, limits, slot, deadline),
    )
    .await
    .map_err(|_| (StatusCode::REQUEST_TIMEOUT, REQUEST_DEADLINE_EXPIRED))?
}
async fn parse_form(
    form: &mut Multipart,
    output_root: &FsPath,
    limits: RequestLimits,
    slot: OwnedSemaphorePermit,
    deadline: Instant,
) -> Result<JobInput, (StatusCode, &'static str)> {
    let root = TempDir::new_in(output_root).map_err(|_| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "temporary storage failed",
        )
    })?;
    let mut out = Submit::default();
    let mut known = HashSet::new();
    let mut fields = 0;
    let mut text_total = 0usize;
    let mut file = false;
    let mut stem = None;
    while let Some(field) = form.next_field().await.map_err(multipart_error)? {
        fields += 1;
        if fields > limits.fields {
            return Err((StatusCode::BAD_REQUEST, "too many fields"));
        };
        let name = field.name().unwrap_or("").to_owned();
        if name == "files" {
            if file {
                return Err((
                    StatusCode::UNPROCESSABLE_ENTITY,
                    "exactly one document is required",
                ));
            };
            let filename = field.file_name().unwrap_or("").to_owned();
            let kind = FsPath::new(&filename)
                .extension()
                .and_then(|s| s.to_str())
                .and_then(DocumentKind::from_suffix)
                .ok_or((StatusCode::UNPROCESSABLE_ENTITY, "unsupported file type"))?;
            let upload = root.path().join(format!("upload.{}", kind.suffix()));
            let mut f = tokio::fs::File::create(&upload).await.map_err(|_| {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "temporary storage failed",
                )
            })?;
            let mut n = 0u64;
            let mut field = field;
            while let Some(chunk) = field.chunk().await.map_err(multipart_error)? {
                n = n
                    .checked_add(
                        u64::try_from(chunk.len())
                            .map_err(|_| (StatusCode::PAYLOAD_TOO_LARGE, "input too large"))?,
                    )
                    .ok_or((StatusCode::PAYLOAD_TOO_LARGE, "input too large"))?;
                if n > limits.file {
                    return Err((StatusCode::PAYLOAD_TOO_LARGE, "input too large"));
                };
                tokio::io::AsyncWriteExt::write_all(&mut f, &chunk)
                    .await
                    .map_err(|_| {
                        (
                            StatusCode::INTERNAL_SERVER_ERROR,
                            "temporary storage failed",
                        )
                    })?;
            }
            tokio::io::AsyncWriteExt::flush(&mut f).await.map_err(|_| {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "temporary storage failed",
                )
            })?;
            stem = Some(
                crate::canonical_stem(
                    FsPath::new(&filename)
                        .file_stem()
                        .and_then(|x| x.to_str())
                        .ok_or((StatusCode::UNPROCESSABLE_ENTITY, "invalid filename"))?,
                )
                .map_err(|_| (StatusCode::UNPROCESSABLE_ENTITY, "invalid filename"))?,
            );
            file = true;
            out.kind = Some(kind);
            out.original_filename = Some(filename);
            continue;
        }
        let known_name = matches!(
            name.as_str(),
            "lang_list"
                | "backend"
                | "effort"
                | "parse_method"
                | "formula_enable"
                | "table_enable"
                | "image_analysis"
                | "return_md"
                | "return_middle_json"
                | "return_model_output"
                | "return_content_list"
                | "return_images"
                | "response_format_zip"
                | "return_original_file"
                | "client_side_output_generation"
                | "start_page_id"
                | "end_page_id"
                | "server_url"
        );
        let mut data = Vec::new();
        let mut field = field;
        while let Some(chunk) = field.chunk().await.map_err(multipart_error)? {
            let field_len = data
                .len()
                .checked_add(chunk.len())
                .ok_or((StatusCode::PAYLOAD_TOO_LARGE, "text field too large"))?;
            if field_len > limits.text {
                return Err((StatusCode::PAYLOAD_TOO_LARGE, "text field too large"));
            }
            text_total = text_total
                .checked_add(chunk.len())
                .ok_or((StatusCode::PAYLOAD_TOO_LARGE, "text too large"))?;
            if text_total > limits.text_total {
                return Err((StatusCode::PAYLOAD_TOO_LARGE, "text too large"));
            }
            data.extend_from_slice(&chunk);
        }
        if !known_name {
            continue;
        };
        if !known.insert(name.clone()) {
            return Err((StatusCode::BAD_REQUEST, "duplicate form field"));
        };
        let value =
            std::str::from_utf8(&data).map_err(|_| (StatusCode::BAD_REQUEST, "invalid text"))?;
        match name.as_str() {
            "backend" => out.backend = Some(value.into()),
            "effort" if !matches!(value, "medium" | "high") => {
                return Err((StatusCode::BAD_REQUEST, "unsupported effort"));
            }
            "parse_method" if !matches!(value, "auto" | "txt" | "ocr") => {
                return Err((StatusCode::BAD_REQUEST, "unsupported parse method"));
            }
            "lang_list"
                if !matches!(
                    value,
                    "ch" | "ch_server"
                        | "korean"
                        | "ta"
                        | "te"
                        | "ka"
                        | "th"
                        | "el"
                        | "arabic"
                        | "east_slavic"
                        | "cyrillic"
                        | "devanagari"
                        | "en"
                        | "japan"
                        | "chinese_cht"
                        | "latin"
                ) =>
            {
                return Err((StatusCode::BAD_REQUEST, "invalid language"));
            }
            "lang_list" => out.language = Some(value.into()),
            "effort" => out.effort = Some(value.into()),
            "parse_method" => out.parse_method = Some(value.into()),
            "formula_enable" => out.formula = boolean(value)?,
            "table_enable" => out.table = boolean(value)?,
            "image_analysis" => out.image = boolean(value)?,
            "start_page_id" => out.start = number(value)?,
            "end_page_id" => out.end = number(value)?,
            "server_url" if !value.is_empty() => out.server_url = Some(value.into()),
            "return_md" => out.md = boolean(value)?,
            "return_middle_json" => out.middle = boolean(value)?,
            "return_model_output" => out.model = boolean(value)?,
            "return_content_list" => out.content = boolean(value)?,
            "return_images" => out.images = boolean(value)?,
            "response_format_zip" => out.zip = boolean(value)?,
            "return_original_file" => out.origin = boolean(value)?,
            "client_side_output_generation" => out.client_side = boolean(value)?,
            _ => {}
        }
    }
    if out.backend.as_deref() != Some("vlm-http-client") {
        return Err((StatusCode::BAD_REQUEST, "unsupported backend"));
    }
    apply_client_side(&mut out);
    if !file {
        return Err((
            StatusCode::UNPROCESSABLE_ENTITY,
            "exactly one document is required",
        ));
    };
    let kind = out.kind.expect("validated form has a document kind");
    let upload = root.path().join(format!("upload.{}", kind.suffix()));
    let stem = stem.expect("validated form has a filename stem");
    Ok(JobInput {
        root,
        _slot: slot,
        deadline,
        canonical_filename: format!("{}.{}", stem, kind.suffix()),
        stem,
        kind,
        upload,
        options: out,
    })
}
fn worker_context(app: &App) -> WorkerContext {
    WorkerContext {
        gate: app.gate.clone(),
        route: app.route.clone(),
        env_formula: app.env_formula,
        env_table: app.env_table,
        official_page_concurrency: app.official_page_concurrency,
        office_workers: app.office_workers.clone(),
        raster_workers: app.raster_workers.clone(),
        events: app.events.clone(),
        server_zip_cap: app.server_zip_cap,
        totals: app.totals,
        #[cfg(test)]
        test_http: app.test_http.clone(),
    }
}
fn apply_client_side(out: &mut Submit) {
    if out.client_side {
        out.md = false;
        out.middle = true;
        out.model = true;
        out.content = false;
        out.images = true;
    }
}
fn multipart_error(error: MultipartError) -> (StatusCode, &'static str) {
    if error.status() == StatusCode::PAYLOAD_TOO_LARGE {
        (StatusCode::PAYLOAD_TOO_LARGE, "request payload too large")
    } else {
        (StatusCode::BAD_REQUEST, "invalid multipart")
    }
}
fn boolean(s: &str) -> Result<bool, (StatusCode, &'static str)> {
    match s {
        "true" => Ok(true),
        "false" => Ok(false),
        _ => Err((
            StatusCode::BAD_REQUEST,
            "boolean fields must be true or false",
        )),
    }
}
fn number(s: &str) -> Result<u64, (StatusCode, &'static str)> {
    if s.is_empty()
        || (s.len() > 1 && s.starts_with('0'))
        || !s.bytes().all(|byte| byte.is_ascii_digit())
    {
        return Err((StatusCode::BAD_REQUEST, "invalid page number"));
    }
    s.parse()
        .map_err(|_| (StatusCode::BAD_REQUEST, "invalid page number"))
}

async fn worker(
    app: WorkerContext,
    record: Arc<Record>,
    label: String,
    events: Option<ProgressCallback>,
) {
    let permit = match tokio::time::timeout_at(
        tokio::time::Instant::from_std(record.input.deadline),
        app.gate.clone().acquire_owned(),
    )
    .await
    {
        Ok(Ok(permit)) => permit,
        Ok(Err(_)) => {
            finish_worker(record, label, events, async {
                Err("task gate closed".to_owned())
            })
            .await;
            return;
        }
        Err(_) => {
            finish_worker(record, label, events, async {
                Err(REQUEST_DEADLINE_EXPIRED.to_owned())
            })
            .await;
            return;
        }
    };
    let root_lease = crate::TaskWorkLease::from_permit(permit);
    let task_work_lease = root_lease.clone();
    finish_worker(record.clone(), label, events, async {
        let started_at = timestamp();
        *record_state(&record) = TaskState::Processing {
            started_at: started_at.clone(),
        };
        let deadline = record.input.deadline;
        Ok::<_, String>((
            run_task(&app, &record.input, deadline, task_work_lease).await?,
            started_at,
        ))
    })
    .await;
    drop(root_lease);
}
async fn sync_worker(app: WorkerContext, guard: SyncWorkerGuard) {
    let input = guard
        .input
        .as_ref()
        .expect("armed guard owns input")
        .clone();
    let permit = match tokio::time::timeout_at(
        tokio::time::Instant::from_std(input.deadline),
        app.gate.clone().acquire_owned(),
    )
    .await
    {
        Ok(Ok(permit)) => permit,
        Ok(Err(_)) => {
            finish_sync_worker(guard, async { Err("task gate closed".to_owned()) }).await;
            return;
        }
        Err(_) => {
            finish_sync_worker(guard, async { Err(REQUEST_DEADLINE_EXPIRED.to_owned()) }).await;
            return;
        }
    };
    let root_lease = crate::TaskWorkLease::from_permit(permit);
    let task_work_lease = root_lease.clone();
    finish_sync_worker(guard, async move {
        let deadline = input.deadline;
        match run_task(&app, &input, deadline, task_work_lease).await {
            Ok(result) => Ok(result),
            Err(error) => {
                cleanup_input(&input).await;
                if Instant::now() >= deadline {
                    Err(REQUEST_DEADLINE_EXPIRED.to_owned())
                } else {
                    Err(error)
                }
            }
        }
    })
    .await;
    drop(root_lease);
}
async fn finish_sync_worker<F>(guard: SyncWorkerGuard, future: F)
where
    F: Future<Output = Result<ResultFile, String>>,
{
    match std::panic::AssertUnwindSafe(future).catch_unwind().await {
        Ok(Ok(result)) => guard.success(result),
        Ok(Err(error)) => guard.failure((
            if error == REQUEST_DEADLINE_EXPIRED {
                StatusCode::REQUEST_TIMEOUT
            } else {
                StatusCode::CONFLICT
            },
            crate::error::sanitize_vlm_error_bytes(error.as_bytes(), 4096),
        )),
        Err(_) => {
            if let Some(input) = guard.input.as_ref() {
                cleanup_input(input).await;
            }
            guard.failure((StatusCode::CONFLICT, "task worker panicked".into()));
        }
    }
}
fn spawn_sync_worker(
    workers: &Arc<Mutex<Option<WorkerRegistry>>>,
    context: WorkerContext,
    guard: SyncWorkerGuard,
    events: &Option<ProgressCallback>,
    label: String,
) -> Result<(), SyncWorkerGuard> {
    let mut workers = workers.lock().unwrap();
    let Some(workers) = workers.as_mut() else {
        return Err(guard);
    };
    let completion = guard.completion.clone();
    crate::progress_events::emit(events, ProgressEvent::RequestAccepted { label });
    let id = workers.tasks.spawn(sync_worker(context, guard)).id();
    workers
        .associations
        .insert(id, WorkerAssociation::Sync(completion));
    Ok(())
}
async fn finish_worker<F>(
    record: Arc<Record>,
    label: String,
    events: Option<ProgressCallback>,
    future: F,
) where
    F: Future<Output = Result<(ResultFile, String), String>>,
{
    let outcome = std::panic::AssertUnwindSafe(future).catch_unwind().await;
    match outcome {
        Ok(Ok((result, started_at))) => {
            *record_state(&record) = TaskState::Completed {
                result,
                started_at,
                completed_at: timestamp(),
                terminal_at: Instant::now(),
            };
            crate::progress_events::emit(&events, ProgressEvent::RequestCompleted { label });
        }
        Ok(Err(error)) => emit_async_failure(&record, &label, &events, &error).await,
        Err(_) => emit_async_failure(&record, &label, &events, "task worker panicked").await,
    }
}
async fn emit_async_failure(
    record: &Record,
    label: &str,
    events: &Option<ProgressCallback>,
    error: &str,
) {
    if let Some(message) = fail_record(record, error).await {
        crate::progress_events::emit(
            events,
            ProgressEvent::DocumentFailed {
                document: record.input.stem.clone(),
                message: message.clone(),
            },
        );
        crate::progress_events::emit(
            events,
            ProgressEvent::RequestFailed {
                label: label.into(),
                message,
            },
        );
    }
}
async fn fail_record(record: &Record, error: &str) -> Option<String> {
    let started_at = match state_snapshot(record) {
        TaskState::Pending => None,
        TaskState::Processing { started_at } => Some(started_at),
        TaskState::Completed { .. } | TaskState::Failed { .. } => return None,
    };
    cleanup_input(&record.input).await;
    let mut state = record_state(record);
    if matches!(*state, TaskState::Pending | TaskState::Processing { .. }) {
        let error = crate::error::sanitize_vlm_error_bytes(error.as_bytes(), 4096);
        *state = TaskState::Failed {
            error: error.clone(),
            started_at,
            completed_at: timestamp(),
            terminal_at: Instant::now(),
        };
        Some(error)
    } else {
        None
    }
}
async fn cleanup_input(input: &JobInput) {
    let root = input.root.path().to_owned();
    let stem = input.stem.clone();
    let _ = tokio::task::spawn_blocking(move || cleanup_failure(&root, &stem)).await;
}
fn cleanup_failure(root: &FsPath, stem: &str) {
    let _ = std::fs::remove_file(root.join("result.zip"));
    let _ = std::fs::remove_file(root.join("result.zip.partial"));
    let _ = std::fs::remove_file(root.join("result.json"));
    let _ = std::fs::remove_file(root.join("result.json.partial"));
    let _ = std::fs::remove_dir_all(root.join(stem));
}
async fn run_task(
    app: &WorkerContext,
    input: &JobInput,
    deadline: Instant,
    task_work_lease: crate::TaskWorkLease,
) -> Result<ResultFile, String> {
    crate::progress_events::emit(
        &app.events,
        ProgressEvent::DocumentStarted {
            document: input.stem.clone(),
        },
    );
    let kind = input.kind;
    let compact = input.root.path().join("selected.pdf");
    let source = if kind == DocumentKind::Pdf {
        let source_input = input.upload.clone();
        let compact_job = compact.clone();
        let start = input.options.start;
        let end = input.options.end;
        let max_pdf_bytes = app.route.max_pdf_bytes;
        remaining(deadline)?;
        tokio::task::spawn_blocking(move || {
            compact_pdf(
                &source_input,
                &compact_job,
                start,
                end,
                max_pdf_bytes,
                deadline,
            )
            .map_err(|error| error.to_string())
        })
        .await
        .map_err(|error| format!("compact join: {error}"))?
        .map_err(|error| format!("compact PDF: {error}"))?;
        remaining(deadline)?;
        compact.clone()
    } else {
        input.upload.clone()
    };
    remaining(deadline)?;
    let source_bytes = tokio::task::spawn_blocking({
        let source = source.clone();
        let cap = app.route.max_pdf_bytes;
        move || read_capped(&source, cap, deadline)
    })
    .await
    .map_err(|error| format!("source read join: {error}"))?
    .map_err(|error| format!("source read: {error}"))?;
    remaining(deadline)?;
    let mut options = app.route.clone();
    options.start_page = 0;
    options.end_page = None;
    options.formula_enable = input.options.formula;
    options.table_enable = input.options.table;
    options.image_analysis = input.options.image;
    if let Some(value) = app.env_formula {
        options.formula_enable = value;
    }
    if let Some(value) = app.env_table {
        options.table_enable = value;
    }
    let (prepared, warning) = prepare_with_warning(
        source_bytes,
        kind,
        &options,
        &app.office_workers,
        &app.raster_workers,
        remaining(deadline)?,
    )
    .await?;
    if let Some(message) = warning {
        crate::progress_events::emit(
            &app.events,
            ProgressEvent::OfficeWarning {
                document: input.stem.clone(),
                message,
            },
        );
    }
    crate::progress_events::emit(
        &app.events,
        ProgressEvent::DocumentPrepared {
            document: input.stem.clone(),
        },
    );
    remaining(deadline)?;
    let config = task_vlm_config(
        {
            let config = VlmHttpConfig::default();
            #[cfg(test)]
            let config = app.test_http.clone().unwrap_or(config);
            config
        },
        input.options.server_url.as_deref(),
    )
    .map_err(|_| "task URL config: invalid server URL".to_owned())?;
    let page_concurrency = crate::official_route::OfficialPageConcurrency::new(
        app.official_page_concurrency,
        app.route.processing_window_size,
        config.max_concurrency,
    );
    let client = tokio::time::timeout_at(
        tokio::time::Instant::from_std(deadline),
        MinerUVlmClient::connect_for_task(
            config,
            MinerUVlmConfig::default(),
            task_work_lease.clone(),
        ),
    )
    .await
    .map_err(|_| "model connect timed out".to_owned())?
    .map_err(|error| format!("model connect: {error}"))?;
    remaining(deadline)?;
    options.total_deadline = remaining(deadline)?;
    let manifest = client
        .parse_and_write_prepared_pdf_with_totals_and_page_concurrency(
            prepared,
            options,
            input.root.path(),
            &input.stem,
            app.events.clone(),
            None,
            app.totals,
            page_concurrency,
        )
        .await
        .map_err(|error| format!("official route: {error}"))?;
    remaining(deadline)?;
    let result = input.root.path().join(if input.options.zip {
        "result.zip"
    } else {
        "result.json"
    });
    let root = input.root.path().to_owned();
    let stem = input.stem.clone();
    let origin = committed_origin(&manifest, kind);
    let result_job = result.clone();
    let selectors = input.options.clone_selectors();
    let route = app.route.clone();
    let server_zip_cap = app.server_zip_cap;
    remaining(deadline)?;
    tokio::task::spawn_blocking(move || {
        if selectors.zip {
            zip_result_capped(
                &root,
                &stem,
                kind,
                &origin,
                &result_job,
                &selectors,
                &route,
                server_zip_cap,
                deadline,
            )
        } else {
            json_result(
                &root,
                &stem,
                kind,
                &result_job,
                &selectors,
                &route,
                deadline,
            )
        }
        .map_err(|error| error.to_string())
    })
    .await
    .map_err(|error| format!("result worker join: {error}"))?
    .map_err(|error| format!("result create: {error}"))?;
    remaining(deadline)?;
    crate::progress_events::emit(
        &app.events,
        ProgressEvent::DocumentCompleted {
            document: input.stem.clone(),
        },
    );
    Ok(ResultFile {
        path: result,
        content_type: if selectors.zip {
            "application/zip"
        } else {
            "application/json"
        },
        keepalive: None,
    })
}
fn remaining(deadline: Instant) -> Result<Duration, String> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|duration| !duration.is_zero())
        .ok_or_else(|| "task deadline expired".to_owned())
}
fn check_deadline(deadline: Instant) -> std::io::Result<()> {
    if Instant::now() >= deadline {
        Err(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "task deadline expired",
        ))
    } else {
        Ok(())
    }
}
fn read_capped(path: &FsPath, cap: usize, deadline: Instant) -> std::io::Result<Bytes> {
    check_deadline(deadline)?;
    let mut file = std::fs::File::open(path)?;
    let mut bytes = Vec::new();
    let mut buffer = [0; 65536];
    loop {
        check_deadline(deadline)?;
        let allowed = cap
            .saturating_add(1)
            .saturating_sub(bytes.len())
            .min(buffer.len());
        let n = file.read(&mut buffer[..allowed])?;
        if n == 0 {
            break;
        }
        if bytes.len().checked_add(n).is_none_or(|size| size > cap) {
            return Err(std::io::Error::other("input exceeds PDF byte limit"));
        }
        bytes.extend_from_slice(&buffer[..n]);
        check_deadline(deadline)?;
    }
    check_deadline(deadline)?;
    Ok(Bytes::from(bytes))
}
fn committed_origin(manifest: &OfficialOutputManifest, kind: DocumentKind) -> PathBuf {
    manifest
        .vlm_dir
        .join(format!("{}_origin.{}", manifest.stem, kind.suffix()))
}
#[derive(Clone, Copy)]
struct Selectors {
    md: bool,
    middle: bool,
    model: bool,
    content: bool,
    images: bool,
    origin: bool,
    zip: bool,
}
impl Submit {
    fn clone_selectors(&self) -> Selectors {
        Selectors {
            md: self.md,
            middle: self.middle,
            model: self.model,
            content: self.content,
            images: self.images,
            origin: self.origin,
            zip: self.zip,
        }
    }
}
fn task_vlm_config(
    mut config: VlmHttpConfig,
    server_url: Option<&str>,
) -> Result<VlmHttpConfig, ()> {
    if let Some(url) = server_url.filter(|url| !url.is_empty()) {
        config.server_url = Some(url.parse().map_err(|_| ())?);
        config.invalid_server_url = false;
        config.api_key = None;
        config
            .headers
            .retain(|header| !header.name().eq_ignore_ascii_case("authorization"));
    }
    Ok(config)
}
fn compact_pdf(
    input: &FsPath,
    output: &FsPath,
    start: u64,
    end: u64,
    cap: usize,
    deadline: Instant,
) -> Result<(), Box<dyn std::error::Error>> {
    struct Capped(std::fs::File, usize, usize, Instant);
    impl Write for Capped {
        fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
            check_deadline(self.3)?;
            let next = self
                .1
                .checked_add(data.len())
                .ok_or_else(|| std::io::Error::other("PDF exceeds size limit"))?;
            if next > self.2 {
                return Err(std::io::Error::other("PDF exceeds size limit"));
            }
            let n = self.0.write(data)?;
            self.1 += n;
            check_deadline(self.3)?;
            Ok(n)
        }
        fn flush(&mut self) -> std::io::Result<()> {
            check_deadline(self.3)?;
            self.0.flush()?;
            check_deadline(self.3)
        }
    }
    let partial = output.with_extension("pdf.partial");
    let mut renamed = false;
    let mut work = || -> Result<(), Box<dyn std::error::Error>> {
        check_deadline(deadline)?;
        let input = std::fs::File::open(input)?;
        let metadata = input.metadata()?;
        if !metadata.is_file() {
            return Err("input is not a regular file".into());
        }
        if metadata.len() > u64::try_from(cap)? {
            return Err("PDF exceeds size limit".into());
        }
        let mut doc = lopdf::Document::load_from(input)?;
        check_deadline(deadline)?;
        if doc.is_encrypted() {
            return Err("encrypted PDF".into());
        };
        let pages = doc.get_pages();
        if pages.is_empty() {
            return Err("empty PDF".into());
        };
        let start = usize::try_from(start)?;
        if start >= pages.len() {
            return Err("page range".into());
        };
        let end = selected_end(end, pages.len() - 1);
        if end < start {
            return Err("page range".into());
        };
        let remove: Vec<u32> = pages
            .keys()
            .copied()
            .enumerate()
            .filter_map(|(i, p)| (i < start || i > end).then_some(p))
            .collect();
        // lopdf's page deletion rebuilds the tree; preserve inherited rendering inputs on leaves.
        for page in pages.values().copied() {
            check_deadline(deadline)?;
            for key in [b"Resources".as_slice(), b"MediaBox", b"CropBox", b"Rotate"] {
                if let Some(value) = inherited_page_value(&doc, page, key)? {
                    doc.get_object_mut(page)?.as_dict_mut()?.set(key, value);
                }
                check_deadline(deadline)?;
            }
        }
        check_deadline(deadline)?;
        doc.delete_pages(&remove);
        check_deadline(deadline)?;
        check_deadline(deadline)?;
        doc.prune_objects();
        check_deadline(deadline)?;
        check_deadline(deadline)?;
        doc.renumber_objects();
        check_deadline(deadline)?;
        let mut writer = Capped(std::fs::File::create(&partial)?, 0, cap, deadline);
        check_deadline(deadline)?;
        doc.save_to(&mut writer)?;
        check_deadline(deadline)?;
        check_deadline(deadline)?;
        writer.flush()?;
        check_deadline(deadline)?;
        check_deadline(deadline)?;
        writer.0.sync_all()?;
        check_deadline(deadline)?;
        check_deadline(deadline)?;
        let check = lopdf::Document::load(&partial)?;
        check_deadline(deadline)?;
        if check.get_pages().len() != end - start + 1 {
            return Err("PDF selection validation".into());
        };
        check_deadline(deadline)?;
        std::fs::rename(&partial, output)?;
        renamed = true;
        check_deadline(deadline)?;
        Ok(())
    };
    let result = work();
    if result.is_err() {
        let _ = std::fs::remove_file(&partial);
        if renamed {
            let _ = std::fs::remove_file(output);
        }
    }
    result
}
fn selected_end(end: u64, last: usize) -> usize {
    if end == 99_999 {
        last
    } else {
        usize::try_from(end).unwrap_or(usize::MAX).min(last)
    }
}
fn inherited_page_value(
    doc: &lopdf::Document,
    page: lopdf::ObjectId,
    key: &[u8],
) -> Result<Option<lopdf::Object>, Box<dyn std::error::Error>> {
    let mut current = page;
    let mut visited = HashSet::new();
    loop {
        if !visited.insert(current) {
            return Err("cyclic PDF parent reference".into());
        }
        let dict = match doc.get_object(current).and_then(lopdf::Object::as_dict) {
            Ok(dict) => dict,
            Err(_) => return Ok(None),
        };
        if let Ok(value) = dict.get(key) {
            return Ok(Some(value.clone()));
        }
        current = match dict.get(b"Parent").and_then(lopdf::Object::as_reference) {
            Ok(parent) => parent,
            Err(_) => return Ok(None),
        };
    }
}
fn zip_result_capped(
    task_root: &FsPath,
    stem: &str,
    kind: DocumentKind,
    origin: &FsPath,
    destination: &FsPath,
    selectors: &Selectors,
    _route: &OfficialPdfOptions,
    cap: u64,
    deadline: Instant,
) -> Result<(), Box<dyn std::error::Error>> {
    let temporary = destination.with_extension("zip.partial");
    let mut renamed = false;
    let result = (|| -> Result<(), Box<dyn std::error::Error>> {
        check_deadline(deadline)?;
        let mut zip = zip::ZipWriter::new(CappedFile::new(&temporary, cap, deadline)?);
        for Artifact {
            name,
            mut file,
            len,
        } in package_files(task_root, stem, kind, selectors, deadline)?
        {
            check_deadline(deadline)?;
            zip.start_file(
                format!("{stem}/{}/{name}", artifact_target(kind)),
                zip::write::SimpleFileOptions::default()
                    .compression_method(zip::CompressionMethod::Deflated),
            )?;
            check_deadline(deadline)?;
            copy_snapshot(&mut file, len, &mut zip, deadline)?;
            check_deadline(deadline)?;
        }
        if selectors.origin {
            check_deadline(deadline)?;
            let mut source = open_origin(task_root, origin)?;
            let len = source.metadata()?.len();
            zip.start_file(
                format!(
                    "{stem}/{}/{stem}_origin.{}",
                    artifact_target(kind),
                    kind.suffix()
                ),
                zip::write::SimpleFileOptions::default()
                    .compression_method(zip::CompressionMethod::Deflated),
            )?;
            copy_snapshot(&mut source, len, &mut zip, deadline)?;
            check_deadline(deadline)?;
        }
        check_deadline(deadline)?;
        zip.finish()?.finish()?;
        check_deadline(deadline)?;
        check_deadline(deadline)?;
        std::fs::rename(&temporary, destination)?;
        renamed = true;
        check_deadline(deadline)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
        if renamed {
            let _ = std::fs::remove_file(destination);
        }
    }
    result
}
#[cfg(test)]
fn zip_result(
    task_root: &FsPath,
    stem: &str,
    kind: DocumentKind,
    origin: &FsPath,
    destination: &FsPath,
    selectors: &Selectors,
    route: &OfficialPdfOptions,
    deadline: Instant,
) -> Result<(), Box<dyn std::error::Error>> {
    let cap = u64::try_from(route.max_pdf_bytes)
        .map_err(|_| "ZIP size limit overflow")?
        .checked_add(route.max_total_asset_bytes as u64)
        .and_then(|n| n.checked_add(route.max_staged_text_bytes as u64))
        .and_then(|n| n.checked_add(1024 * 1024))
        .ok_or("ZIP size limit overflow")?;
    zip_result_capped(
        task_root,
        stem,
        kind,
        origin,
        destination,
        selectors,
        route,
        cap,
        deadline,
    )
}
fn json_result(
    task_root: &FsPath,
    stem: &str,
    kind: DocumentKind,
    destination: &FsPath,
    selectors: &Selectors,
    route: &OfficialPdfOptions,
    deadline: Instant,
) -> Result<(), Box<dyn std::error::Error>> {
    let partial = destination.with_extension("json.partial");
    let mut renamed = false;
    let result = (|| -> Result<(), Box<dyn std::error::Error>> {
        check_deadline(deadline)?;
        let mut files = package_files(task_root, stem, kind, selectors, deadline)?;
        // JSON has one official content_list key; the v2 companion is ZIP-only.
        files.retain(|artifact| !artifact.name.ends_with("_content_list_v2.json"));
        let resident_cap = route.max_staged_text_bytes as u64;
        if files.iter().any(|artifact| artifact.len > resident_cap) {
            return Err("JSON artifact exceeds resident staged-text limit".into());
        }
        let mut remaining = route.max_staged_text_bytes as u64;
        for Artifact { name, len, .. } in &files {
            check_deadline(deadline)?;
            if !name.starts_with("images/") {
                let n = *len;
                remaining = remaining
                    .checked_sub(n)
                    .ok_or("JSON source exceeds size limit")?;
            } else {
                let n = *len;
                let encoded = n
                    .checked_add(2)
                    .and_then(|n| n.checked_div(3))
                    .and_then(|n| n.checked_mul(4))
                    .ok_or("JSON source size overflow")?;
                remaining = remaining
                    .checked_sub(
                        encoded
                            .checked_add(u64::try_from(name.len() + 64)?)
                            .ok_or("JSON source size overflow")?,
                    )
                    .ok_or("JSON source exceeds size limit")?;
            }
        }
        let mut out = CappedFile::new(&partial, route.max_staged_text_bytes as u64, deadline)?;
        write!(
            out,
            "{{\"backend\":\"vlm-http-client\",\"version\":{},\"results\":{{{}:{{",
            serde_json::to_string(env!("CARGO_PKG_VERSION"))?,
            serde_json::to_string(stem)?
        )?;
        let mut first = true;
        let mut images = Vec::new();
        for Artifact {
            name,
            mut file,
            len,
        } in files
        {
            check_deadline(deadline)?;
            if !name.starts_with("images/") {
                if !first {
                    write!(out, ",")?;
                }
                first = false;
                let key = match name.as_str() {
                    x if x == format!("{stem}.md") => "md_content",
                    x if x.ends_with("_middle.json") => "middle_json",
                    x if x.ends_with("_model.json") => "model_output",
                    _ => "content_list",
                };
                let text =
                    String::from_utf8(read_snapshot(&mut file, usize::try_from(len)?, deadline)?)?;
                serde_json::to_writer(&mut out, key)?;
                write!(out, ":")?;
                serde_json::to_writer(&mut out, &text)?;
            } else {
                images.push((name, file, len));
            }
        }
        if selectors.images {
            if !first {
                write!(out, ",")?;
            }
            serde_json::to_writer(&mut out, "images")?;
            write!(out, ":{{")?;
            for (i, (image, mut input, len)) in images.into_iter().enumerate() {
                check_deadline(deadline)?;
                if i != 0 {
                    write!(out, ",")?;
                }
                let mime = image_mime(&image).ok_or("unsupported image MIME")?;
                serde_json::to_writer(&mut out, image.trim_start_matches("images/"))?;
                write!(out, ":\"data:{mime};base64,")?;
                {
                    let mut encoder = base64::write::EncoderWriter::new(
                        &mut out,
                        &base64::engine::general_purpose::STANDARD,
                    );
                    copy_snapshot(&mut input, len, &mut encoder, deadline)?;
                    encoder.finish()?;
                }
                write!(out, "\"")?;
                check_deadline(deadline)?;
            }
            write!(out, "}}")?;
        }
        write!(out, "}}}}}}")?;
        check_deadline(deadline)?;
        out.finish()?;
        check_deadline(deadline)?;
        std::fs::rename(&partial, destination)?;
        renamed = true;
        check_deadline(deadline)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&partial);
        if renamed {
            let _ = std::fs::remove_file(destination);
        }
    }
    result
}
fn task_dir(root: &FsPath) -> Result<Dir, Box<dyn std::error::Error>> {
    Ok(Dir::open_ambient_dir(root, ambient_authority())?)
}
fn open_origin(
    task_root: &FsPath,
    origin: &FsPath,
) -> Result<cap_std::fs::File, Box<dyn std::error::Error>> {
    let relative = origin
        .strip_prefix(task_root)
        .map_err(|_| "origin is outside task root")?;
    let components: Vec<_> = relative
        .components()
        .map(|component| match component {
            std::path::Component::Normal(name) => Ok(name),
            _ => Err("unsafe origin path"),
        })
        .collect::<Result<_, _>>()?;
    let (name, parents) = components.split_last().ok_or("empty origin path")?;
    let mut dir = task_dir(task_root)?;
    for parent in parents {
        dir = dir.open_dir_nofollow(parent)?;
    }
    open_regular(&dir, name)
}
fn open_regular(
    dir: &Dir,
    name: &std::ffi::OsStr,
) -> Result<cap_std::fs::File, Box<dyn std::error::Error>> {
    let mut options = OpenOptions::new();
    options.read(true).follow(FollowSymlinks::No);
    let file = dir.open_with(name, &options)?;
    if !file.metadata()?.is_file() {
        return Err("artifact is not a regular file".into());
    }
    Ok(file)
}
struct Artifact {
    name: String,
    file: cap_std::fs::File,
    len: u64,
}
fn package_files(
    root: &FsPath,
    stem: &str,
    kind: DocumentKind,
    selectors: &Selectors,
    deadline: Instant,
) -> Result<Vec<Artifact>, Box<dyn std::error::Error>> {
    check_deadline(deadline)?;
    let root = task_dir(root)?;
    check_deadline(deadline)?;
    let stem_dir = root.open_dir_nofollow(stem)?;
    check_deadline(deadline)?;
    let vlm = stem_dir.open_dir_nofollow(artifact_target(kind))?;
    check_deadline(deadline)?;
    let mut out = Vec::new();
    for (enabled, name) in [
        (selectors.md, format!("{stem}.md")),
        (selectors.middle, format!("{stem}_middle.json")),
        (selectors.model, format!("{stem}_model.json")),
        (selectors.content, format!("{stem}_content_list.json")),
        (selectors.content, format!("{stem}_content_list_v2.json")),
    ] {
        if enabled {
            check_deadline(deadline)?;
            let file = open_regular(&vlm, name.as_ref())?;
            let len = file.metadata()?.len();
            out.push(Artifact { name, file, len });
            check_deadline(deadline)?;
        }
    }
    if !selectors.images {
        return Ok(out);
    }
    match vlm.open_dir_nofollow("images") {
        Ok(images) => {
            check_deadline(deadline)?;
            let mut names = Vec::new();
            for entry in images.entries()? {
                check_deadline(deadline)?;
                let entry = entry?;
                let name = entry.file_name();
                let meta = images.symlink_metadata(&name)?;
                if !meta.is_file() || meta.file_type().is_symlink() {
                    return Err("images contains a symlink, special file, or directory".into());
                }
                if name.to_str().is_none() {
                    return Err("image name is not UTF-8".into());
                }
                names.push(name);
            }
            names.sort();
            check_deadline(deadline)?;
            for name in names {
                check_deadline(deadline)?;
                let shown = format!("images/{}", name.to_str().ok_or("image name is not UTF-8")?);
                let file = open_regular(&images, &name)?;
                let len = file.metadata()?.len();
                out.push(Artifact {
                    name: shown,
                    file,
                    len,
                });
                check_deadline(deadline)?;
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    check_deadline(deadline)?;
    Ok(out)
}
fn artifact_target(kind: DocumentKind) -> &'static str {
    if kind.is_office() { "office" } else { "vlm" }
}
fn read_snapshot(
    file: &mut cap_std::fs::File,
    expected: usize,
    deadline: Instant,
) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    check_deadline(deadline)?;
    let mut bytes = vec![0; expected];
    for chunk in bytes.chunks_mut(65536) {
        check_deadline(deadline)?;
        file.read_exact(chunk)?;
        check_deadline(deadline)?;
    }
    let mut overflow = [0];
    check_deadline(deadline)?;
    let overflowed = file.read(&mut overflow)? != 0;
    check_deadline(deadline)?;
    let actual = file.metadata()?.len();
    check_deadline(deadline)?;
    if overflowed || actual != u64::try_from(expected)? {
        return Err("artifact changed while packaging".into());
    }
    Ok(bytes)
}
fn copy_snapshot(
    file: &mut cap_std::fs::File,
    expected: u64,
    output: &mut impl Write,
    deadline: Instant,
) -> Result<(), Box<dyn std::error::Error>> {
    check_deadline(deadline)?;
    let mut copied = 0u64;
    let mut buffer = [0; 65536];
    while copied < expected {
        check_deadline(deadline)?;
        let wanted = usize::try_from((expected - copied).min(buffer.len() as u64))?;
        let n = file.read(&mut buffer[..wanted])?;
        if n == 0 {
            break;
        }
        output.write_all(&buffer[..n])?;
        copied += u64::try_from(n)?;
        check_deadline(deadline)?;
    }
    let mut overflow = [0];
    check_deadline(deadline)?;
    let overflowed = file.read(&mut overflow)? != 0;
    check_deadline(deadline)?;
    let actual = file.metadata()?.len();
    check_deadline(deadline)?;
    if copied != expected || overflowed || actual != expected {
        return Err("artifact changed while packaging".into());
    }
    Ok(())
}
fn image_mime(name: &str) -> Option<&'static str> {
    match name.rsplit('.').next()?.to_ascii_lowercase().as_str() {
        "png" => Some("image/png"),
        "jpg" | "jpeg" => Some("image/jpeg"),
        "webp" => Some("image/webp"),
        _ => None,
    }
}
struct CappedFile {
    file: std::fs::File,
    used: u64,
    cap: u64,
    deadline: Instant,
}
impl CappedFile {
    fn new(path: &FsPath, cap: u64, deadline: Instant) -> std::io::Result<Self> {
        check_deadline(deadline)?;
        let file = std::fs::File::create(path)?;
        check_deadline(deadline)?;
        Ok(Self {
            file,
            used: 0,
            cap,
            deadline,
        })
    }
    fn finish(mut self) -> std::io::Result<()> {
        check_deadline(self.deadline)?;
        self.flush()?;
        self.file.sync_all()?;
        check_deadline(self.deadline)
    }
}
impl Write for CappedFile {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        check_deadline(self.deadline)?;
        let position = self.file.stream_position()?;
        let end = position
            .checked_add(
                u64::try_from(b.len())
                    .map_err(|_| std::io::Error::other("result exceeds size limit"))?,
            )
            .ok_or_else(|| std::io::Error::other("result exceeds size limit"))?;
        if end > self.cap {
            return Err(std::io::Error::other("result exceeds size limit"));
        }
        let n = self.file.write(b)?;
        self.used = self.used.max(
            position
                .checked_add(
                    u64::try_from(n)
                        .map_err(|_| std::io::Error::other("result exceeds size limit"))?,
                )
                .ok_or_else(|| std::io::Error::other("result exceeds size limit"))?,
        );
        check_deadline(self.deadline)?;
        Ok(n)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        check_deadline(self.deadline)?;
        self.file.flush()?;
        check_deadline(self.deadline)
    }
}
impl Seek for CappedFile {
    fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
        check_deadline(self.deadline)?;
        let position = self.file.seek(pos)?;
        check_deadline(self.deadline)?;
        Ok(position)
    }
}
fn status_snapshot(app: &App, id: &str, record: &Record) -> serde_json::Value {
    let state = state_snapshot(record);
    let queued_ahead = if matches!(state, TaskState::Pending) {
        app.records
            .lock()
            .unwrap()
            .values()
            .take(RECORD_CAP)
            .filter(|other| {
                other.sequence < record.sequence
                    && matches!(state_snapshot(other), TaskState::Pending)
            })
            .count()
    } else {
        0
    };
    let (status, started_at, completed_at, error) = match state {
        TaskState::Pending => ("pending", None, None, None),
        TaskState::Processing { started_at } => ("processing", Some(started_at), None, None),
        TaskState::Completed {
            started_at,
            completed_at,
            ..
        } => ("completed", Some(started_at), Some(completed_at), None),
        TaskState::Failed {
            error,
            started_at,
            completed_at,
            ..
        } => ("failed", started_at, Some(completed_at), Some(error)),
    };
    let base = &record.base_url;
    json!({"task_id":id,"status":status,"backend":"vlm-http-client","file_names":[record.input.canonical_filename],"created_at":record.created_at,"started_at":started_at,"completed_at":completed_at,"error":error,"queued_ahead":queued_ahead,"status_url":format!("{base}/tasks/{id}"),"result_url":format!("{base}/tasks/{id}/result")})
}
async fn status(State(app): State<App>, Path(id): Path<String>) -> Response {
    let Some(record) = app.records.lock().unwrap().get(&id).cloned() else {
        return error(StatusCode::NOT_FOUND, "unknown task");
    };
    Json(status_snapshot(&app, &id, &record)).into_response()
}
async fn result(State(app): State<App>, Path(id): Path<String>) -> Response {
    let Some(record) = app.records.lock().unwrap().get(&id).cloned() else {
        return error(StatusCode::NOT_FOUND, "unknown task");
    };
    let result = {
        let state = record_state(&record);
        match &*state {
            TaskState::Completed { result, .. } => result.clone(),
            TaskState::Pending | TaskState::Processing { .. } => {
                return (
                    StatusCode::ACCEPTED,
                    Json(json!({"status":"processing","message":"task is not complete"})),
                )
                    .into_response();
            }
            TaskState::Failed { error: message, .. } => {
                return (
                    StatusCode::CONFLICT,
                    Json(json!({"status":"failed","message":message})),
                )
                    .into_response();
            }
        }
    };
    stream_result(result, Some(record)).await
}
async fn stream_result(result: ResultFile, record: Option<Arc<Record>>) -> Response {
    let content_type = result.content_type;
    let file = match tokio::fs::File::open(&result.path).await {
        Ok(file) => file,
        Err(_) => return error(StatusCode::INTERNAL_SERVER_ERROR, "result is unavailable"),
    };
    let stream = stream::try_unfold(
        (Some(file), result, record),
        |(file, result, record)| async move {
            let Some(mut file) = file else {
                return Ok::<
                    Option<(
                        bytes::Bytes,
                        (Option<tokio::fs::File>, ResultFile, Option<Arc<Record>>),
                    )>,
                    std::io::Error,
                >(None);
            };
            let mut buf = vec![0; 65536];
            let n = tokio::io::AsyncReadExt::read(&mut file, &mut buf).await?;
            Ok(if n == 0 {
                None
            } else {
                Some((
                    bytes::Bytes::copy_from_slice(&buf[..n]),
                    (Some(file), result, record),
                ))
            })
        },
    );
    (
        [(header::CONTENT_TYPE, content_type)],
        Body::from_stream(stream),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::StreamExt;
    use lopdf::{Dictionary, Object, Stream, dictionary};
    use reqwest::multipart::{Form, Part};
    use std::io::Read;
    use std::sync::atomic::{AtomicBool, AtomicUsize};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn nested_fixture() -> Vec<u8> {
        let mut doc = lopdf::Document::with_version("1.5");
        let root = doc.new_object_id();
        let nested = doc.new_object_id();
        let pages: Vec<_> = (0..3).map(|_| doc.new_object_id()).collect();
        let font = doc.add_object(
            dictionary! { "Type" => "Font", "Subtype" => "Type1", "BaseFont" => "Helvetica" },
        );
        let resources = doc.add_object(dictionary! { "Font" => dictionary! { "F" => font } });
        for (i, page) in pages.iter().copied().enumerate() {
            let marker = ["UNIQUE_DELETED", "SELECTED_GREEN", "SELECTED_BLUE"][i];
            let color = [("1 0 0", "deleted"), ("0 1 0", "green"), ("0 0 1", "blue")][i].0;
            let content =
                format!("% {marker}\n{color} rg\nBT /F 18 Tf 20 40 Td ({marker}) Tj ET\n");
            let stream = doc.add_object(Stream::new(Dictionary::new(), content.into_bytes()));
            doc.objects.insert(
                page,
                dictionary! { "Type" => "Page", "Parent" => nested, "Contents" => stream }.into(),
            );
        }
        doc.objects.insert(nested, dictionary! { "Type" => "Pages", "Parent" => root, "Kids" => pages.iter().copied().map(Object::Reference).collect::<Vec<_>>(), "Count" => 3, "CropBox" => vec![10.into(), 20.into(), 110.into(), 220.into()], "Rotate" => 90 }.into());
        doc.objects.insert(root, dictionary! { "Type" => "Pages", "Kids" => vec![nested.into()], "Count" => 3, "Resources" => resources, "MediaBox" => vec![0.into(), 0.into(), 200.into(), 300.into()] }.into());
        let catalog = doc.add_object(dictionary! { "Type" => "Catalog", "Pages" => root });
        doc.trailer.set("Root", catalog);
        let mut bytes = Vec::new();
        doc.save_to(&mut bytes).unwrap();
        bytes
    }
    fn far_deadline() -> Instant {
        Instant::now() + Duration::from_secs(30)
    }
    fn effective(doc: &lopdf::Document, page: lopdf::ObjectId, key: &[u8]) -> Object {
        inherited_page_value(doc, page, key).unwrap().unwrap()
    }

    struct TestService {
        base: String,
        output: PathBuf,
        root: TempDir,
        stop: oneshot::Sender<()>,
        task: tokio::task::JoinHandle<Result<(), std::io::Error>>,
    }
    impl TestService {
        async fn stop(self) {
            let _ = self.stop.send(());
            assert!(
                tokio::time::timeout(Duration::from_secs(2), self.task)
                    .await
                    .unwrap()
                    .unwrap()
                    .is_ok()
            );
            drop(self.root);
        }
    }
    async fn test_service_with(
        limits: RequestLimits,
        record_cap: usize,
        concurrency: usize,
        http: VlmHttpConfig,
        retention: Duration,
        cleanup_interval: Duration,
    ) -> TestService {
        let root = tempfile::tempdir().unwrap();
        let output = root.path().join("tasks");
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let (stop, stopped) = oneshot::channel();
        let config = ServiceConfig::new(
            concurrency,
            output.clone(),
            OfficialPdfOptions::default(),
            None,
            None,
        )
        .unwrap()
        .test_limits(limits, record_cap, retention, cleanup_interval)
        .test_http(http);
        let task = tokio::spawn(serve(listener, config, async move {
            let _ = stopped.await;
        }));
        TestService {
            base,
            output,
            root,
            stop,
            task,
        }
    }
    async fn test_service(limits: RequestLimits, record_cap: usize) -> TestService {
        test_service_with(
            limits,
            record_cap,
            3,
            VlmHttpConfig {
                model_name: Some("mock".into()),
                skip_model_name_checking: true,
                max_retries: 0,
                http_timeout: Duration::from_secs(10),
                connect_timeout: Duration::from_secs(1),
                ..Default::default()
            },
            RETENTION,
            CLEANUP_INTERVAL,
        )
        .await
    }
    async fn test_service_events(events: Option<ProgressCallback>) -> TestService {
        let root = tempfile::tempdir().unwrap();
        let output = root.path().join("tasks");
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let (stop, stopped) = oneshot::channel();
        let mut config =
            ServiceConfig::new(1, output.clone(), OfficialPdfOptions::default(), None, None)
                .unwrap()
                .test_limits(pdf_limits(), 3, RETENTION, CLEANUP_INTERVAL)
                .test_http(VlmHttpConfig {
                    model_name: Some("mock".into()),
                    skip_model_name_checking: true,
                    max_retries: 0,
                    http_timeout: Duration::from_secs(10),
                    connect_timeout: Duration::from_secs(1),
                    ..Default::default()
                });
        if let Some(events) = events {
            config = config.progress_callback(events);
        }
        let task = tokio::spawn(serve(listener, config, async move {
            let _ = stopped.await;
        }));
        TestService {
            base,
            output,
            root,
            stop,
            task,
        }
    }
    async fn test_service_one(limits: RequestLimits, record_cap: usize) -> TestService {
        test_service_with(
            limits,
            record_cap,
            1,
            VlmHttpConfig {
                model_name: Some("mock".into()),
                skip_model_name_checking: true,
                max_retries: 0,
                http_timeout: Duration::from_secs(10),
                connect_timeout: Duration::from_secs(1),
                ..Default::default()
            },
            RETENTION,
            CLEANUP_INTERVAL,
        )
        .await
    }
    async fn test_service_office(limits: RequestLimits, workers: OfficeWorkers) -> TestService {
        let root = tempfile::tempdir().unwrap();
        let output = root.path().join("tasks");
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let (stop, stopped) = oneshot::channel();
        let config =
            ServiceConfig::new(3, output.clone(), OfficialPdfOptions::default(), None, None)
                .unwrap()
                .test_limits(limits, 3, RETENTION, CLEANUP_INTERVAL)
                .test_http(VlmHttpConfig {
                    model_name: Some("mock".into()),
                    skip_model_name_checking: true,
                    max_retries: 0,
                    http_timeout: Duration::from_secs(10),
                    connect_timeout: Duration::from_secs(1),
                    ..Default::default()
                })
                .test_office(workers);
        let task = tokio::spawn(serve(listener, config, async move {
            let _ = stopped.await;
        }));
        TestService {
            base,
            output,
            root,
            stop,
            task,
        }
    }
    async fn test_service_office_events(events: ProgressCallback) -> TestService {
        let root = tempfile::tempdir().unwrap();
        let output = root.path().join("tasks");
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let (stop, stopped) = oneshot::channel();
        let config =
            ServiceConfig::new(1, output.clone(), OfficialPdfOptions::default(), None, None)
                .unwrap()
                .test_limits(pdf_limits(), 3, RETENTION, CLEANUP_INTERVAL)
                .test_http(VlmHttpConfig {
                    model_name: Some("mock".into()),
                    skip_model_name_checking: true,
                    max_retries: 0,
                    http_timeout: Duration::from_secs(10),
                    connect_timeout: Duration::from_secs(1),
                    ..Default::default()
                })
                .test_office(OfficeWorkers::with_test_executable(
                    std::env::current_exe().unwrap(),
                ))
                .progress_callback(events);
        let task = tokio::spawn(serve(listener, config, async move {
            let _ = stopped.await;
        }));
        TestService {
            base,
            output,
            root,
            stop,
            task,
        }
    }
    async fn test_service_route(
        limits: RequestLimits,
        record_cap: usize,
        concurrency: usize,
        route: OfficialPdfOptions,
        http: VlmHttpConfig,
    ) -> TestService {
        let root = tempfile::tempdir().unwrap();
        let output = root.path().join("tasks");
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let (stop, stopped) = oneshot::channel();
        let config = ServiceConfig::new(concurrency, output.clone(), route, None, None)
            .unwrap()
            .test_limits(limits, record_cap, RETENTION, CLEANUP_INTERVAL)
            .test_http(http);
        let task = tokio::spawn(serve(listener, config, async move {
            let _ = stopped.await;
        }));
        TestService {
            base,
            output,
            root,
            stop,
            task,
        }
    }
    fn limits() -> RequestLimits {
        RequestLimits {
            body: 16 * 1024,
            file: 1024,
            text: 512,
            text_total: 2048,
            fields: 32,
        }
    }
    fn pdf_limits() -> RequestLimits {
        RequestLimits {
            body: 128 * 1024,
            file: 64 * 1024,
            ..limits()
        }
    }
    fn canonical_fields() -> Vec<(String, String)> {
        [
            ("lang_list", "en"),
            ("backend", "vlm-http-client"),
            ("effort", "medium"),
            ("parse_method", "auto"),
            ("formula_enable", "true"),
            ("table_enable", "true"),
            ("image_analysis", "true"),
            ("return_md", "true"),
            ("return_middle_json", "true"),
            ("return_model_output", "true"),
            ("return_content_list", "true"),
            ("return_images", "true"),
            ("response_format_zip", "true"),
            ("return_original_file", "true"),
            ("client_side_output_generation", "false"),
            ("start_page_id", "0"),
            ("end_page_id", "0"),
            ("server_url", "http://127.0.0.1:9"),
        ]
        .into_iter()
        .map(|(k, v)| (k.into(), v.into()))
        .collect()
    }
    fn canonical_form() -> Form {
        form(
            canonical_fields(),
            vec![("files", "input.pdf", b"%PDF-x".to_vec())],
        )
    }
    fn pdf_form(server_url: &str, start: &str, end: &str) -> Form {
        pdf_form_named(server_url, start, end, "input.pdf")
    }
    fn pdf_form_named(server_url: &str, start: &str, end: &str, filename: &str) -> Form {
        let mut fields = canonical_fields();
        for (key, value) in &mut fields {
            if key == "server_url" {
                *value = server_url.into();
            }
            if key == "start_page_id" {
                *value = start.into();
            }
            if key == "end_page_id" {
                *value = end.into();
            }
        }
        form(fields, vec![("files", filename, nested_fixture())])
    }
    fn tiny_png() -> Vec<u8> {
        base64::Engine::decode(
            &base64::engine::general_purpose::STANDARD,
            "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=",
        )
        .unwrap()
    }
    fn tiny_docx() -> Vec<u8> {
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let options = zip::write::SimpleFileOptions::default();
        for (name, value) in [
            (
                "[Content_Types].xml",
                r#"<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/></Types>"#,
            ),
            (
                "_rels/.rels",
                r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="word/document.xml"/></Relationships>"#,
            ),
            (
                "word/document.xml",
                r#"<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body/></w:document>"#,
            ),
        ] {
            zip.start_file(name, options).unwrap();
            zip.write_all(value.as_bytes()).unwrap();
        }
        zip.finish().unwrap().into_inner()
    }
    fn mixed_form(server_url: &str, filename: &str, bytes: Vec<u8>) -> Form {
        let mut fields = canonical_fields();
        fields
            .iter_mut()
            .find(|(name, _)| name == "server_url")
            .unwrap()
            .1 = server_url.into();
        form(fields, vec![("files", filename, bytes)])
    }
    #[derive(Clone)]
    struct MockState {
        block: Arc<AtomicBool>,
        entered: Arc<AtomicUsize>,
        release: Arc<tokio::sync::Notify>,
        fail: bool,
    }
    struct Mock {
        url: String,
        state: MockState,
        stop: oneshot::Sender<()>,
        task: tokio::task::JoinHandle<()>,
    }
    async fn mock(fail: bool, block: bool) -> Mock {
        async fn chat(
            axum::extract::State(state): axum::extract::State<MockState>,
            Json(request): Json<serde_json::Value>,
        ) -> Response {
            state.entered.fetch_add(1, Ordering::SeqCst);
            if state.block.load(Ordering::SeqCst) {
                state.release.notified().await;
            }
            if state.fail {
                return StatusCode::INTERNAL_SERVER_ERROR.into_response();
            }
            let content = if request.to_string().contains("Layout Detection") {
                "<|box_start|>0 0 1000 1000<|box_end|><|ref_start|>text<|ref_end|><|rotate_up|>"
            } else {
                "recognized"
            };
            Json(json!({"choices":[{"finish_reason":"stop","message":{"content":content}}]}))
                .into_response()
        }
        let state = MockState {
            block: Arc::new(AtomicBool::new(block)),
            entered: Arc::new(AtomicUsize::new(0)),
            release: Arc::new(tokio::sync::Notify::new()),
            fail,
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let (stop, stopped) = oneshot::channel();
        let server_state = state.clone();
        let task = tokio::spawn(async move {
            axum::serve(
                listener,
                Router::new()
                    .route("/v1/chat/completions", axum::routing::post(chat))
                    .with_state(server_state),
            )
            .with_graceful_shutdown(async move {
                let _ = stopped.await;
            })
            .await
            .unwrap();
        });
        Mock {
            url,
            state,
            stop,
            task,
        }
    }
    async fn wait_status(url: &str, wanted: &str) -> serde_json::Value {
        tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                let value: serde_json::Value =
                    reqwest::get(url).await.unwrap().json().await.unwrap();
                if value["status"] == wanted {
                    return value;
                }
                if matches!(value["status"].as_str(), Some("completed" | "failed")) {
                    panic!("unexpected terminal task state while waiting for {wanted}: {value}");
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap()
    }
    fn form(fields: Vec<(String, String)>, files: Vec<(&str, &str, Vec<u8>)>) -> Form {
        let form = fields
            .into_iter()
            .fold(Form::new(), |form, (name, value)| form.text(name, value));
        files.into_iter().fold(form, |form, (name, file, bytes)| {
            form.part(
                name.to_owned(),
                Part::bytes(bytes).file_name(file.to_owned()),
            )
        })
    }
    async fn post(service: &TestService, form: Form) -> reqwest::Response {
        reqwest::Client::new()
            .post(format!("{}/tasks", service.base))
            .multipart(form)
            .send()
            .await
            .unwrap()
    }
    async fn file_post(service: &TestService, form: Form) -> reqwest::Response {
        reqwest::Client::new()
            .post(format!("{}/file_parse", service.base))
            .multipart(form)
            .send()
            .await
            .unwrap()
    }
    fn mixed_sync_form(server_url: &str, filename: &str, bytes: Vec<u8>, zip: bool) -> Form {
        let mut fields = canonical_fields();
        fields
            .iter_mut()
            .find(|(name, _)| name == "server_url")
            .unwrap()
            .1 = server_url.into();
        fields
            .iter_mut()
            .find(|(name, _)| name == "response_format_zip")
            .unwrap()
            .1 = zip.to_string();
        form(fields, vec![("files", filename, bytes)])
    }
    async fn assert_invalid(service: &TestService, form: Form, expected: StatusCode) {
        assert_eq!(post(service, form).await.status(), expected);
        let health: serde_json::Value = reqwest::get(format!("{}/health", service.base))
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(health["task_count"], 0);
        assert_eq!(std::fs::read_dir(&service.output).unwrap().count(), 0);
    }

    #[tokio::test]
    async fn protocol2_loopback_health_submission_and_async_failure() {
        let service = test_service(limits(), 1).await;
        let health: serde_json::Value = reqwest::get(format!("{}/health", service.base))
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(
            health,
            json!({"status":"healthy","protocol_version":2,"max_concurrent_requests":3,"processing_window_size":64,"task_count":0})
        );
        assert_eq!(
            reqwest::get(format!("{}/tasks/nope", service.base))
                .await
                .unwrap()
                .status(),
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            reqwest::get(format!("{}/tasks/nope/result", service.base))
                .await
                .unwrap()
                .status(),
            StatusCode::NOT_FOUND
        );
        let accepted: serde_json::Value =
            post(&service, canonical_form()).await.json().await.unwrap();
        assert!(
            accepted["status_url"]
                .as_str()
                .unwrap()
                .starts_with(&service.base)
        );
        assert!(
            accepted["result_url"]
                .as_str()
                .unwrap()
                .starts_with(&service.base)
        );
        let status_url = accepted["status_url"].as_str().unwrap().to_owned();
        let result_url = accepted["result_url"].as_str().unwrap().to_owned();
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let value: serde_json::Value = reqwest::get(&status_url)
                    .await
                    .unwrap()
                    .json()
                    .await
                    .unwrap();
                if value["status"] == "failed" {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(
            reqwest::get(result_url).await.unwrap().status(),
            StatusCode::CONFLICT
        );
        service.stop().await;
    }

    #[tokio::test]
    async fn protocol2_invalid_forms_are_bounded_and_release_admission() {
        let service = test_service(limits(), 1).await;
        let mut duplicate = canonical_fields();
        duplicate.push(("backend".into(), "vlm-http-client".into()));
        assert_invalid(
            &service,
            form(duplicate, vec![("files", "input.pdf", b"%PDF-x".to_vec())]),
            StatusCode::BAD_REQUEST,
        )
        .await;
        assert_invalid(
            &service,
            form(canonical_fields(), vec![]),
            StatusCode::UNPROCESSABLE_ENTITY,
        )
        .await;
        assert_invalid(
            &service,
            form(
                canonical_fields(),
                vec![
                    ("files", "a.pdf", b"%PDF-x".to_vec()),
                    ("files", "b.pdf", b"%PDF-x".to_vec()),
                ],
            ),
            StatusCode::UNPROCESSABLE_ENTITY,
        )
        .await;
        assert_invalid(
            &service,
            form(
                canonical_fields(),
                vec![("files", "a.txt", b"%PDF-x".to_vec())],
            ),
            StatusCode::UNPROCESSABLE_ENTITY,
        )
        .await;
        for (field, value) in [
            ("backend", "other"),
            ("formula_enable", "TRUE"),
            ("table_enable", "yes"),
            ("start_page_id", "01"),
            ("start_page_id", "01x"),
            ("end_page_id", "18446744073709551616"),
            ("lang_list", "not-a-language"),
        ] {
            let mut fields = canonical_fields();
            fields.iter_mut().find(|(key, _)| key == field).unwrap().1 = value.into();
            assert_invalid(
                &service,
                form(fields, vec![("files", "input.pdf", b"%PDF-x".to_vec())]),
                StatusCode::BAD_REQUEST,
            )
            .await;
        }
        let mut alias = canonical_fields();
        alias
            .iter_mut()
            .find(|(key, _)| key == "lang_list")
            .unwrap()
            .1 = "japan".into();
        assert_eq!(
            post(
                &service,
                form(alias, vec![("files", "input.pdf", b"%PDF-x".to_vec())])
            )
            .await
            .status(),
            StatusCode::ACCEPTED
        );
        service.stop().await;
    }

    #[tokio::test]
    async fn multipart_deadline_returns_408_and_releases_tempdir_and_slot() {
        let mut route = OfficialPdfOptions::default();
        route.total_deadline = Duration::from_millis(50);
        let service = test_service_route(
            limits(),
            1,
            1,
            route,
            VlmHttpConfig {
                model_name: Some("mock".into()),
                skip_model_name_checking: true,
                ..Default::default()
            },
        )
        .await;
        let body = stream::once(async {
            Ok::<_, std::io::Error>(Bytes::from_static(
                b"--stall\r\nContent-Disposition: form-data; name=\"files\"; filename=\"input.pdf\"\r\n\r\npartial",
            ))
        })
        .chain(stream::pending());
        let response = tokio::time::timeout(
            Duration::from_secs(2),
            reqwest::Client::new()
                .post(format!("{}/tasks", service.base))
                .header("content-type", "multipart/form-data; boundary=stall")
                .body(reqwest::Body::wrap_stream(body))
                .send(),
        )
        .await
        .expect("stalled multipart exceeded route deadline")
        .unwrap();
        assert_eq!(response.status(), StatusCode::REQUEST_TIMEOUT);
        assert_eq!(
            response.text().await.unwrap(),
            r#"{"detail":"request deadline expired"}"#
        );
        assert_eq!(std::fs::read_dir(&service.output).unwrap().count(), 0);

        let response = post(&service, Form::new().text("backend", "vlm-http-client")).await;
        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(std::fs::read_dir(&service.output).unwrap().count(), 0);
        service.stop().await;
    }

    #[tokio::test]
    async fn sync_queue_uses_job_deadline_and_releases_input() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().to_owned();
        let slots = Arc::new(Semaphore::new(1));
        let input = Arc::new(JobInput {
            root,
            _slot: slots.clone().acquire_owned().await.unwrap(),
            deadline: Instant::now() + Duration::from_millis(20),
            stem: "input".into(),
            canonical_filename: "input.pdf".into(),
            kind: DocumentKind::Pdf,
            upload: path.join("upload.pdf"),
            options: Submit::default(),
        });
        let context = WorkerContext {
            gate: Arc::new(Semaphore::new(0)),
            route: OfficialPdfOptions::default(),
            env_formula: None,
            env_table: None,
            official_page_concurrency: 4,
            office_workers: OfficeWorkers::new().unwrap(),
            raster_workers: RasterWorkers::default(),
            events: None,
            server_zip_cap: crate::DocumentLimitPolicy::defaults().server_zip_bytes,
            totals: crate::document_limits::OfficialDocumentTotals::from_options(
                &OfficialPdfOptions::default(),
            ),
            test_http: None,
        };
        let (sender, receiver) = oneshot::channel();
        sync_worker(
            context,
            SyncWorkerGuard::new(input, SyncCompletion::new(sender)),
        )
        .await;
        assert!(matches!(
            receiver.await.unwrap(),
            Err((StatusCode::REQUEST_TIMEOUT, message)) if message == REQUEST_DEADLINE_EXPIRED
        ));
        assert!(!path.exists());
        assert!(slots.try_acquire().is_ok());
    }

    #[tokio::test]
    async fn malformed_pdf_fails_asynchronously_before_model_or_publication() {
        let model = mock(false, false).await;
        let service = test_service(pdf_limits(), 1).await;
        let mut fields = canonical_fields();
        fields
            .iter_mut()
            .find(|(name, _)| name == "server_url")
            .unwrap()
            .1 = model.url.clone();
        let response = post(
            &service,
            form(fields, vec![("files", "input.pdf", b"not-pdf".to_vec())]),
        )
        .await;
        assert_eq!(response.status(), StatusCode::ACCEPTED);
        let task: serde_json::Value = response.json().await.unwrap();
        wait_status(task["status_url"].as_str().unwrap(), "failed").await;
        assert_eq!(
            reqwest::get(task["result_url"].as_str().unwrap())
                .await
                .unwrap()
                .status(),
            StatusCode::CONFLICT
        );
        assert_eq!(model.state.entered.load(Ordering::SeqCst), 0);
        let task_root = std::fs::read_dir(&service.output)
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        assert!(task_root.join("upload.pdf").exists());
        assert!(!task_root.join("result.json").exists());
        assert!(!task_root.join("result.json.partial").exists());
        assert!(!task_root.join("input").exists());
        assert!(!task_root.join("selected.pdf.partial").exists());
        service.stop().await;
        let _ = model.stop.send(());
        model.task.await.unwrap();
    }

    #[tokio::test]
    async fn protocol2_unknown_and_size_limits_have_expected_statuses() {
        let service = test_service(limits(), 2).await;
        let mut unknown = canonical_fields();
        unknown.push(("ignored".into(), "x".repeat(400)));
        assert_eq!(
            post(
                &service,
                form(unknown, vec![("files", "input.pdf", b"%PDF-x".to_vec())])
            )
            .await
            .status(),
            StatusCode::ACCEPTED
        );
        service.stop().await;
        let service = test_service(limits(), 1).await;
        let mut many = canonical_fields();
        many.extend((0..15).map(|n| (format!("unknown{n}"), "x".into())));
        assert_invalid(
            &service,
            form(many, vec![("files", "input.pdf", b"%PDF-x".to_vec())]),
            StatusCode::BAD_REQUEST,
        )
        .await;
        service.stop().await;

        let mut file_limits = limits();
        file_limits.file = 5;
        let service = test_service(file_limits, 1).await;
        assert_invalid(&service, canonical_form(), StatusCode::PAYLOAD_TOO_LARGE).await;
        service.stop().await;
        let mut text_limits = limits();
        text_limits.text = 1;
        let service = test_service(text_limits, 1).await;
        assert_invalid(&service, canonical_form(), StatusCode::PAYLOAD_TOO_LARGE).await;
        service.stop().await;
        let mut total_limits = limits();
        total_limits.text_total = 1;
        let service = test_service(total_limits, 1).await;
        assert_invalid(&service, canonical_form(), StatusCode::PAYLOAD_TOO_LARGE).await;
        service.stop().await;
        let mut body_limits = limits();
        body_limits.body = 100;
        let service = test_service(body_limits, 1).await;
        assert_eq!(
            post(&service, canonical_form()).await.status(),
            StatusCode::PAYLOAD_TOO_LARGE
        );
        service.stop().await;
    }

    #[tokio::test]
    #[ignore = "full VLM API lifecycle integration e2e"]
    async fn protocol2_successful_range_download_is_repeatable_and_complete() {
        let model = mock(false, false).await;
        let service = test_service(pdf_limits(), 3).await;
        let accepted: serde_json::Value = post(&service, pdf_form(&model.url, "1", "2"))
            .await
            .json()
            .await
            .unwrap();
        let status = accepted["status_url"].as_str().unwrap();
        let result = accepted["result_url"].as_str().unwrap();
        let initial: serde_json::Value = reqwest::get(status).await.unwrap().json().await.unwrap();
        assert!(matches!(
            initial["status"].as_str(),
            Some("pending" | "processing")
        ));
        let processing = reqwest::get(result).await.unwrap();
        assert_eq!(processing.status(), StatusCode::ACCEPTED);
        wait_status(status, "completed").await;
        let first = reqwest::get(result).await.unwrap();
        assert_eq!(first.headers()[header::CONTENT_TYPE], "application/zip");
        let first = first.bytes().await.unwrap();
        let second = reqwest::get(result).await.unwrap().bytes().await.unwrap();
        assert_eq!(first, second);
        let mut zip = zip::ZipArchive::new(std::io::Cursor::new(first)).unwrap();
        let mut names = (0..zip.len())
            .map(|i| zip.by_index(i).unwrap().name().to_owned())
            .collect::<Vec<_>>();
        names.sort();
        assert_eq!(
            names,
            [
                "input/vlm/input.md",
                "input/vlm/input_content_list.json",
                "input/vlm/input_content_list_v2.json",
                "input/vlm/input_middle.json",
                "input/vlm/input_model.json",
                "input/vlm/input_origin.pdf"
            ]
        );
        let mut origin = Vec::new();
        zip.by_name("input/vlm/input_origin.pdf")
            .unwrap()
            .read_to_end(&mut origin)
            .unwrap();
        assert_eq!(
            lopdf::Document::load_mem(&origin)
                .unwrap()
                .get_pages()
                .len(),
            2
        );
        let mut middle = String::new();
        zip.by_name("input/vlm/input_middle.json")
            .unwrap()
            .read_to_string(&mut middle)
            .unwrap();
        let middle: serde_json::Value = serde_json::from_str(&middle).unwrap();
        assert_eq!(
            middle["pdf_info"]
                .as_array()
                .unwrap()
                .iter()
                .map(|page| page["page_idx"].as_u64().unwrap())
                .collect::<Vec<_>>(),
            [0, 1]
        );
        service.stop().await;
        let _ = model.stop.send(());
        model.task.await.unwrap();
    }

    #[tokio::test]
    #[ignore = "full VLM API lifecycle integration e2e"]
    async fn status_snapshots_preserve_times_filename_and_queue() {
        let model = mock(false, true).await;
        let service = test_service_one(pdf_limits(), 3).await;
        let tasks = futures_util::future::join_all((0..3).map(|index| {
            post(
                &service,
                pdf_form_named(
                    &model.url,
                    "0",
                    "0",
                    if index == 0 {
                        "Report.PDF"
                    } else {
                        "input.pdf"
                    },
                ),
            )
        }))
        .await;
        let mut tasks: Vec<serde_json::Value> =
            futures_util::future::join_all(tasks.into_iter().map(|r| async move {
                assert_eq!(r.status(), StatusCode::ACCEPTED);
                r.json().await.unwrap()
            }))
            .await;
        tasks.sort_by_key(|task| {
            task["task_id"]
                .as_str()
                .unwrap()
                .strip_prefix("local-")
                .unwrap()
                .parse::<u64>()
                .unwrap()
        });
        assert!(
            tasks
                .iter()
                .any(|task| task["file_names"] == json!(["Report.pdf"]))
        );
        assert!(tasks[2]["queued_ahead"].as_u64().unwrap() > 0);
        for task in &tasks {
            assert!(OffsetDateTime::parse(task["created_at"].as_str().unwrap(), &Rfc3339).is_ok());
        }
        tokio::time::timeout(Duration::from_secs(10), async {
            while model.state.entered.load(Ordering::SeqCst) == 0 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(
            wait_status(tasks[0]["status_url"].as_str().unwrap(), "processing").await["status"],
            "processing"
        );
        assert_eq!(
            reqwest::get(tasks[1]["status_url"].as_str().unwrap())
                .await
                .unwrap()
                .json::<serde_json::Value>()
                .await
                .unwrap()["queued_ahead"],
            0
        );
        assert_eq!(
            reqwest::get(tasks[2]["status_url"].as_str().unwrap())
                .await
                .unwrap()
                .json::<serde_json::Value>()
                .await
                .unwrap()["queued_ahead"],
            1
        );
        assert_eq!(
            post(&service, pdf_form(&model.url, "0", "0"))
                .await
                .status(),
            StatusCode::SERVICE_UNAVAILABLE
        );
        model.state.block.store(false, Ordering::SeqCst);
        model.state.release.notify_waiters();
        for task in &tasks {
            let terminal = wait_status(task["status_url"].as_str().unwrap(), "completed").await;
            assert_eq!(terminal["queued_ahead"], 0);
            for field in ["created_at", "started_at", "completed_at"] {
                assert!(OffsetDateTime::parse(terminal[field].as_str().unwrap(), &Rfc3339).is_ok());
            }
        }
        service.stop().await;
        let _ = model.stop.send(());
        model.task.await.unwrap();

        let failed = mock(true, false).await;
        let service = test_service(pdf_limits(), 1).await;
        let task: serde_json::Value = post(&service, pdf_form(&failed.url, "0", "0"))
            .await
            .json()
            .await
            .unwrap();
        wait_status(task["status_url"].as_str().unwrap(), "failed").await;
        assert_eq!(
            reqwest::get(task["result_url"].as_str().unwrap())
                .await
                .unwrap()
                .status(),
            StatusCode::CONFLICT
        );
        for entry in std::fs::read_dir(&service.output).unwrap() {
            let path = entry.unwrap().path();
            assert!(
                !path.join("result.zip").exists()
                    && !path.join("result.zip.partial").exists()
                    && !path.join("input").exists()
            );
        }
        service.stop().await;
        let _ = failed.stop.send(());
        failed.task.await.unwrap();
    }

    #[tokio::test]
    #[ignore = "full VLM API lifecycle integration e2e"]
    async fn protocol2_terminal_pressure_evicts_completed_record_over_http() {
        let model = mock(false, false).await;
        let service = test_service(pdf_limits(), 1).await;
        let first: serde_json::Value = post(&service, pdf_form(&model.url, "0", "0"))
            .await
            .json()
            .await
            .unwrap();
        wait_status(first["status_url"].as_str().unwrap(), "completed").await;
        let second: serde_json::Value = post(&service, pdf_form(&model.url, "0", "0"))
            .await
            .json()
            .await
            .unwrap();
        assert_eq!(
            reqwest::get(first["status_url"].as_str().unwrap())
                .await
                .unwrap()
                .status(),
            StatusCode::NOT_FOUND
        );
        wait_status(second["status_url"].as_str().unwrap(), "completed").await;
        service.stop().await;
        let _ = model.stop.send(());
        model.task.await.unwrap();
    }

    #[tokio::test]
    #[ignore = "full VLM API lifecycle integration e2e"]
    async fn protocol2_retention_cleanup_removes_record_and_tempdir() {
        let model = mock(false, false).await;
        let service = test_service_with(
            pdf_limits(),
            1,
            1,
            VlmHttpConfig {
                model_name: Some("mock".into()),
                skip_model_name_checking: true,
                max_retries: 0,
                http_timeout: Duration::from_secs(10),
                connect_timeout: Duration::from_secs(1),
                ..Default::default()
            },
            Duration::from_millis(30),
            Duration::from_millis(10),
        )
        .await;
        let task: serde_json::Value = post(&service, pdf_form(&model.url, "0", "0"))
            .await
            .json()
            .await
            .unwrap();
        wait_status(task["status_url"].as_str().unwrap(), "completed").await;
        assert_eq!(std::fs::read_dir(&service.output).unwrap().count(), 1);
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if reqwest::get(task["status_url"].as_str().unwrap())
                    .await
                    .unwrap()
                    .status()
                    == StatusCode::NOT_FOUND
                    && std::fs::read_dir(&service.output).unwrap().count() == 0
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        service.stop().await;
        let _ = model.stop.send(());
        model.task.await.unwrap();
    }

    #[tokio::test]
    #[ignore = "full VLM API lifecycle integration e2e"]
    async fn protocol2_shutdown_waits_for_active_service_worker() {
        let model = mock(false, true).await;
        let service = test_service_one(pdf_limits(), 1).await;
        let submitted: serde_json::Value = post(&service, pdf_form(&model.url, "0", "0"))
            .await
            .json()
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(10), async {
            while model.state.entered.load(Ordering::SeqCst) == 0 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        wait_status(submitted["status_url"].as_str().unwrap(), "processing").await;
        let TestService {
            output,
            root,
            stop,
            mut task,
            ..
        } = service;
        assert_eq!(std::fs::read_dir(&output).unwrap().count(), 1);
        stop.send(()).unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(50), &mut task)
                .await
                .is_err()
        );
        assert_eq!(std::fs::read_dir(&output).unwrap().count(), 1);
        model.state.block.store(false, Ordering::SeqCst);
        model.state.release.notify_waiters();
        assert!(
            tokio::time::timeout(Duration::from_secs(10), task)
                .await
                .unwrap()
                .unwrap()
                .is_ok()
        );
        assert_eq!(std::fs::read_dir(&output).unwrap().count(), 0);
        drop(root);
        let _ = model.stop.send(());
        model.task.await.unwrap();
    }

    #[tokio::test]
    #[ignore = "full VLM API lifecycle integration e2e"]
    async fn shutdown_orders_worker_registries_before_records() {
        let model = mock(false, true).await;
        let root = tempfile::tempdir().unwrap();
        let output = root.path().join("tasks");
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let (stop, stopped) = oneshot::channel();
        let (probe, mut phases) = tokio::sync::mpsc::unbounded_channel();
        let events = Arc::new(Mutex::new(Vec::new()));
        let config = ServiceConfig::new(1, output, OfficialPdfOptions::default(), None, None)
            .unwrap()
            .test_limits(pdf_limits(), 1, RETENTION, CLEANUP_INTERVAL)
            .test_http(VlmHttpConfig {
                model_name: Some("mock".into()),
                skip_model_name_checking: true,
                max_retries: 0,
                http_timeout: Duration::from_secs(10),
                connect_timeout: Duration::from_secs(1),
                ..Default::default()
            })
            .test_shutdown_probe(probe)
            .progress_callback({
                let events = events.clone();
                Arc::new(move |event| events.lock().unwrap().push(event))
            });
        let server = tokio::spawn(serve(listener, config, async move {
            let _ = stopped.await;
        }));
        let service = TestService {
            base,
            output: root.path().join("tasks"),
            root,
            stop,
            task: server,
        };
        tokio::time::timeout(Duration::from_secs(1), async {
            while events.lock().unwrap().is_empty() {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .unwrap();
        assert!(matches!(
            events.lock().unwrap().first(),
            Some(ProgressEvent::ServerStarted { .. })
        ));
        let task: serde_json::Value = post(&service, pdf_form(&model.url, "0", "0"))
            .await
            .json()
            .await
            .unwrap();
        wait_status(task["status_url"].as_str().unwrap(), "processing").await;
        tokio::time::timeout(Duration::from_secs(5), async {
            while model.state.entered.load(Ordering::SeqCst) == 0 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        let TestService {
            root,
            stop,
            task: server,
            ..
        } = service;
        stop.send(()).unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(100), phases.recv())
                .await
                .is_err()
        );
        model.state.block.store(false, Ordering::SeqCst);
        model.state.release.notify_waiters();
        for expected in ["service_workers", "office", "raster", "records"] {
            assert_eq!(
                tokio::time::timeout(Duration::from_secs(10), phases.recv())
                    .await
                    .unwrap(),
                Some(expected)
            );
        }
        assert!(server.await.unwrap().is_ok());
        assert!(matches!(
            events.lock().unwrap().first(),
            Some(ProgressEvent::ServerStarted { .. })
        ));
        assert!(matches!(
            events.lock().unwrap().last(),
            Some(ProgressEvent::ServerStopped)
        ));
        drop(root);
        let _ = model.stop.send(());
        model.task.await.unwrap();
    }

    #[tokio::test]
    #[ignore = "full VLM API lifecycle integration e2e"]
    async fn file_parse_shares_slots_gate_survives_disconnect_and_shutdown() {
        let model = mock(false, true).await;
        let root = tempfile::tempdir().unwrap();
        let output = root.path().join("tasks");
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let (stop, stopped) = oneshot::channel();
        let (probe, mut phases) = tokio::sync::mpsc::unbounded_channel();
        let config =
            ServiceConfig::new(1, output.clone(), OfficialPdfOptions::default(), None, None)
                .unwrap()
                .test_limits(pdf_limits(), 2, RETENTION, CLEANUP_INTERVAL)
                .test_http(VlmHttpConfig {
                    model_name: Some("mock".into()),
                    skip_model_name_checking: true,
                    max_retries: 0,
                    http_timeout: Duration::from_secs(10),
                    connect_timeout: Duration::from_secs(1),
                    ..Default::default()
                })
                .test_shutdown_probe(probe);
        let server = tokio::spawn(serve(listener, config, async move {
            let _ = stopped.await;
        }));
        let service = TestService {
            base: base.clone(),
            output: output.clone(),
            root,
            stop,
            task: server,
        };
        let model_url = model.url.clone();
        let sync = tokio::spawn(async move {
            reqwest::Client::new()
                .post(format!("{base}/file_parse"))
                .multipart(mixed_sync_form(
                    &model_url,
                    "input.pdf",
                    nested_fixture(),
                    true,
                ))
                .send()
                .await
        });
        tokio::time::timeout(Duration::from_secs(10), async {
            while model.state.entered.load(Ordering::SeqCst) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();

        let task: serde_json::Value = post(&service, pdf_form(&model.url, "0", "0"))
            .await
            .json()
            .await
            .unwrap();
        assert_eq!(task["status"], "pending");
        assert!(task["started_at"].is_null());
        let response = post(&service, pdf_form(&model.url, "0", "0")).await;
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(
            response.json::<serde_json::Value>().await.unwrap()["detail"],
            "task capacity is full"
        );

        sync.abort();
        assert!(sync.await.unwrap_err().is_cancelled());
        assert!(output.exists());
        assert!(std::fs::read_dir(&output).unwrap().count() > 0);
        assert!(
            tokio::time::timeout(Duration::from_millis(100), phases.recv())
                .await
                .is_err()
        );

        let TestService {
            root,
            stop,
            task: server,
            ..
        } = service;
        stop.send(()).unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(100), phases.recv())
                .await
                .is_err()
        );
        model.state.block.store(false, Ordering::SeqCst);
        model.state.release.notify_waiters();
        for expected in ["service_workers", "office", "raster", "records"] {
            assert_eq!(
                tokio::time::timeout(Duration::from_secs(10), phases.recv())
                    .await
                    .unwrap(),
                Some(expected)
            );
        }
        assert!(server.await.unwrap().is_ok());
        assert_eq!(std::fs::read_dir(&output).unwrap().count(), 0);
        drop(root);
        let _ = model.stop.send(());
        model.task.await.unwrap();
    }

    #[test]
    fn compact_pdf_preserves_nested_inheritance_and_renderer_compatibility() {
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("in.pdf");
        let output = dir.path().join("out.pdf");
        std::fs::write(&input, nested_fixture()).unwrap();
        compact_pdf(&input, &output, 1, 99_999, 64 * 1024, far_deadline()).unwrap();
        let doc = lopdf::Document::load(&output).unwrap();
        assert_eq!(doc.get_pages().keys().copied().collect::<Vec<_>>(), [1, 2]);
        let pages: Vec<_> = doc.get_pages().into_values().collect();
        assert_eq!(pages.len(), 2);
        for (index, page) in pages.into_iter().enumerate() {
            assert_eq!(
                effective(&doc, page, b"MediaBox").as_array().unwrap(),
                &vec![0.into(), 0.into(), 200.into(), 300.into()]
            );
            assert_eq!(
                effective(&doc, page, b"CropBox").as_array().unwrap(),
                &vec![10.into(), 20.into(), 110.into(), 220.into()]
            );
            assert_eq!(effective(&doc, page, b"Rotate").as_i64().unwrap(), 90);
            assert!(matches!(
                effective(&doc, page, b"Resources"),
                Object::Reference(_) | Object::Dictionary(_)
            ));
            let contents = doc
                .get_object(
                    doc.get_object(page)
                        .unwrap()
                        .as_dict()
                        .unwrap()
                        .get(b"Contents")
                        .unwrap()
                        .as_reference()
                        .unwrap(),
                )
                .unwrap()
                .as_stream()
                .unwrap()
                .content
                .clone();
            let marker: &[u8] = [b"SELECTED_GREEN".as_slice(), b"SELECTED_BLUE".as_slice()][index];
            assert!(contents.windows(marker.len()).any(|x| x == marker));
        }
        let bytes = std::fs::read(&output).unwrap();
        assert!(
            !bytes
                .windows(b"UNIQUE_DELETED".len())
                .any(|x| x == b"UNIQUE_DELETED")
        );
        assert!(
            bytes
                .windows(b"SELECTED_GREEN".len())
                .any(|x| x == b"SELECTED_GREEN")
        );
        assert!(
            bytes
                .windows(b"SELECTED_BLUE".len())
                .any(|x| x == b"SELECTED_BLUE")
        );
        let parsed = crate::pdf::parse_document(bytes, &crate::Limits::default()).unwrap();
        for index in 0..2 {
            let rendered =
                crate::pdf::render_page_safe(&parsed, index, &crate::Limits::default()).unwrap();
            assert_eq!(rendered.index, index);
            assert_eq!(rendered.size, [200.0, 100.0]);
        }
        assert!(compact_pdf(&input, &output, 0, 99_999, 64 * 1024, far_deadline()).is_ok());
        assert!(compact_pdf(&input, &output, 0, u64::MAX, 64 * 1024, far_deadline()).is_ok());
        assert!(compact_pdf(&input, &output, 3, 3, 64 * 1024, far_deadline()).is_err());
        assert!(compact_pdf(&input, &output, 2, 1, 64 * 1024, far_deadline()).is_err());
    }

    #[test]
    fn compact_pdf_cap_is_exact_and_failure_is_atomic() {
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("in.pdf");
        let probe = dir.path().join("probe.pdf");
        std::fs::write(&input, nested_fixture()).unwrap();
        compact_pdf(&input, &probe, 1, 99_999, 64 * 1024, far_deadline()).unwrap();
        let cap = usize::try_from(std::fs::metadata(&input).unwrap().len()).unwrap();

        let exact = dir.path().join("exact.pdf");
        compact_pdf(&input, &exact, 1, 99_999, cap, far_deadline()).unwrap();
        assert!(usize::try_from(std::fs::metadata(&exact).unwrap().len()).unwrap() <= cap);

        let failure = dir.path().join("failure.pdf");
        let partial = failure.with_extension("pdf.partial");
        std::fs::write(&failure, b"sentinel").unwrap();
        std::fs::write(&partial, b"stale partial").unwrap();
        assert!(
            compact_pdf(
                &input,
                &failure,
                1,
                99_999,
                cap.checked_sub(1).unwrap(),
                far_deadline()
            )
            .is_err()
        );
        assert_eq!(std::fs::read(&failure).unwrap(), b"sentinel");
        assert!(!partial.exists());
    }

    #[test]
    fn compact_pdf_rejects_sparse_source_above_cap_before_lopdf() {
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("oversize.pdf");
        let output = dir.path().join("selected.pdf");
        std::fs::File::create(&input).unwrap().set_len(9).unwrap();
        let error = compact_pdf(&input, &output, 0, 0, 8, far_deadline()).unwrap_err();
        assert!(error.to_string().contains("PDF exceeds size limit"));
        assert!(!output.exists() && !output.with_extension("pdf.partial").exists());
    }

    #[test]
    fn compact_deadline_failure_is_atomic() {
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("in.pdf");
        let output = dir.path().join("out.pdf");
        let partial = output.with_extension("pdf.partial");
        std::fs::write(&input, nested_fixture()).unwrap();
        std::fs::write(&output, b"sentinel").unwrap();
        assert!(compact_pdf(&input, &output, 0, 99_999, 64 * 1024, Instant::now()).is_err());
        assert_eq!(std::fs::read(&output).unwrap(), b"sentinel");
        assert!(!partial.exists());
        compact_pdf(&input, &output, 0, 99_999, 64 * 1024, far_deadline()).unwrap();
        assert_eq!(lopdf::Document::load(&output).unwrap().get_pages().len(), 3);
    }

    async fn terminal(slot: Arc<Semaphore>, age: Duration) -> Arc<Record> {
        let root = tempfile::tempdir().unwrap();
        let record = Arc::new(Record {
            sequence: 0,
            base_url: "http://127.0.0.1:1".into(),
            input: JobInput {
                upload: root.path().join("upload.pdf"),
                canonical_filename: "out.pdf".into(),
                stem: "out".into(),
                kind: DocumentKind::Pdf,
                options: Submit::default(),
                _slot: slot.acquire_owned().await.unwrap(),
                deadline: far_deadline(),
                root,
            },
            state: Mutex::new(TaskState::Pending),
            created_at: timestamp(),
        });
        *record_state(&record) = TaskState::Completed {
            result: ResultFile {
                path: record.input.root.path().join("result.zip"),
                content_type: "application/zip",
                keepalive: None,
            },
            started_at: timestamp(),
            completed_at: timestamp(),
            terminal_at: Instant::now() - age,
        };
        record
    }

    #[tokio::test]
    async fn cleanup_drops_expired_map_owner_but_not_stream_owner() {
        let slots = Arc::new(Semaphore::new(1));
        let record = terminal(slots.clone(), Duration::from_secs(2)).await;
        let root = record.input.root.path().to_owned();
        let records = Arc::new(Mutex::new(HashMap::from([("old".into(), record.clone())])));
        cleanup_records(&records, Duration::from_secs(1));
        assert!(records.lock().unwrap().is_empty());
        assert!(root.exists());
        assert!(slots.try_acquire().is_err());
        drop(record);
        assert!(slots.try_acquire().is_ok());
    }

    #[tokio::test]
    async fn pressure_eviction_continues_past_held_terminal_records() {
        let slots = Arc::new(Semaphore::new(2));
        let held = terminal(slots.clone(), Duration::from_secs(2)).await;
        let free = terminal(slots.clone(), Duration::from_secs(1)).await;
        let app = App {
            public_listener: false,
            allow_public_http_client: false,
            records: Arc::new(Mutex::new(HashMap::from([
                ("held".into(), held.clone()),
                ("free".into(), free),
            ]))),
            slots: slots.clone(),
            gate: Arc::new(Semaphore::new(1)),
            ids: Arc::new(AtomicU64::new(1)),
            output_root: tempfile::tempdir().unwrap().keep(),
            route: OfficialPdfOptions::default(),
            env_formula: None,
            env_table: None,
            official_page_concurrency: 4,
            concurrency: 1,
            retention: RETENTION,
            cleanup_interval: CLEANUP_INTERVAL,
            workers: Arc::new(Mutex::new(Some(WorkerRegistry::new()))),
            office_workers: OfficeWorkers::new().unwrap(),
            raster_workers: RasterWorkers::default(),
            events: None,
            server_zip_cap: crate::DocumentLimitPolicy::defaults().server_zip_bytes,
            totals: crate::document_limits::OfficialDocumentTotals::from_options(
                &OfficialPdfOptions::default(),
            ),
            limits: RequestLimits {
                body: BODY_CAP,
                file: FILE_CAP,
                text: TEXT_CAP,
                text_total: TEXT_TOTAL_CAP,
                fields: 32,
            },
            test_http: None,
        };
        let permit = reserve(&app);
        assert!(permit.is_some());
        drop(permit);
        assert!(app.records.lock().unwrap().is_empty());
        assert!(slots.try_acquire().is_ok());
        drop(held);
    }

    #[tokio::test]
    async fn worker_registry_drain_waits_without_aborting() {
        let (started, release) = (
            Arc::new(tokio::sync::Notify::new()),
            Arc::new(tokio::sync::Notify::new()),
        );
        let mut workers = WorkerRegistry::new();
        let wait = release.clone();
        let signal = started.clone();
        workers.tasks.spawn(async move {
            signal.notify_one();
            wait.notified().await;
        });
        started.notified().await;
        assert!(
            tokio::time::timeout(Duration::from_millis(10), workers.tasks.join_next_with_id())
                .await
                .is_err()
        );
        release.notify_one();
        assert!(workers.tasks.join_next_with_id().await.unwrap().is_ok());
    }

    #[tokio::test]
    async fn worker_exit_paths_terminalize_records() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let callback: ProgressCallback = {
            let events = events.clone();
            Arc::new(move |event| events.lock().unwrap().push(event))
        };
        let slots = Arc::new(Semaphore::new(3));
        let panic_record = terminal(slots.clone(), Duration::ZERO).await;
        *record_state(&panic_record) = TaskState::Processing {
            started_at: timestamp(),
        };
        std::fs::write(
            panic_record.input.root.path().join("result.zip.partial"),
            b"x",
        )
        .unwrap();
        std::fs::create_dir(panic_record.input.root.path().join("out")).unwrap();
        finish_worker(
            panic_record.clone(),
            "panic".into(),
            Some(callback.clone()),
            async { panic!("test worker panic") },
        )
        .await;
        match state_snapshot(&panic_record) {
            TaskState::Failed {
                error,
                started_at: Some(_),
                ..
            } => {
                assert_eq!(error, "task worker panicked")
            }
            _ => panic!("caught panic did not terminalize"),
        }
        assert!(
            !panic_record
                .input
                .root
                .path()
                .join("result.zip.partial")
                .exists()
        );
        assert!(!panic_record.input.root.path().join("out").exists());
        assert_eq!(
            *events.lock().unwrap(),
            vec![
                ProgressEvent::DocumentFailed {
                    document: panic_record.input.stem.clone(),
                    message: "task worker panicked".into()
                },
                ProgressEvent::RequestFailed {
                    label: "panic".into(),
                    message: "task worker panicked".into()
                },
            ]
        );
        emit_async_failure(&panic_record, "panic", &Some(callback.clone()), "again").await;
        assert_eq!(events.lock().unwrap().len(), 2);
        events.lock().unwrap().clear();

        let raw_record = terminal(slots.clone(), Duration::ZERO).await;
        *record_state(&raw_record) = TaskState::Pending;
        std::fs::write(
            raw_record.input.root.path().join("result.zip.partial"),
            b"x",
        )
        .unwrap();
        let records = Arc::new(Mutex::new(HashMap::from([(
            "raw".into(),
            raw_record.clone(),
        )])));
        let workers = Arc::new(Mutex::new(Some(WorkerRegistry::new())));
        {
            let mut registry = workers.lock().unwrap();
            let registry = registry.as_mut().unwrap();
            let task_id = registry.tasks.spawn(async { panic!("raw panic") }).id();
            registry
                .associations
                .insert(task_id, WorkerAssociation::Async("raw".into()));
        }
        tokio::task::yield_now().await;
        reap_workers(&workers, &records, &Some(callback.clone())).await;
        match state_snapshot(&raw_record) {
            TaskState::Failed {
                started_at: None, ..
            } => {}
            _ => panic!("raw JoinError did not terminalize"),
        }
        assert!(
            !raw_record
                .input
                .root
                .path()
                .join("result.zip.partial")
                .exists()
        );
        assert_eq!(
            *events.lock().unwrap(),
            vec![
                ProgressEvent::DocumentFailed {
                    document: raw_record.input.stem.clone(),
                    message: "task worker terminated unexpectedly".into()
                },
                ProgressEvent::RequestFailed {
                    label: "raw".into(),
                    message: "task worker terminated unexpectedly".into()
                },
            ]
        );
        inspect_worker_exit(
            &records,
            WorkerAssociation::Async("raw".into()),
            &Some(callback.clone()),
        )
        .await;
        assert_eq!(events.lock().unwrap().len(), 2);
        events.lock().unwrap().clear();
        assert!(
            workers
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .associations
                .is_empty()
        );

        let gate_record = terminal(slots, Duration::ZERO).await;
        *record_state(&gate_record) = TaskState::Pending;
        std::fs::write(
            gate_record.input.root.path().join("result.zip.partial"),
            b"x",
        )
        .unwrap();
        let gate = Arc::new(Semaphore::new(1));
        gate.close();
        worker(
            WorkerContext {
                gate,
                route: OfficialPdfOptions::default(),
                env_formula: None,
                env_table: None,
                official_page_concurrency: 4,
                office_workers: OfficeWorkers::new().unwrap(),
                raster_workers: RasterWorkers::default(),
                events: Some(callback.clone()),
                server_zip_cap: crate::DocumentLimitPolicy::defaults().server_zip_bytes,
                totals: crate::document_limits::OfficialDocumentTotals::from_options(
                    &OfficialPdfOptions::default(),
                ),
                test_http: None,
            },
            gate_record.clone(),
            "gate".into(),
            Some(callback.clone()),
        )
        .await;
        match state_snapshot(&gate_record) {
            TaskState::Failed {
                started_at: None, ..
            } => {}
            _ => panic!("closed gate did not terminalize"),
        }
        assert!(
            !gate_record
                .input
                .root
                .path()
                .join("result.zip.partial")
                .exists()
        );
        assert_eq!(events.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn sync_completion_and_registry_association_are_exactly_once() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let callback: ProgressCallback = {
            let events = events.clone();
            Arc::new(move |event| events.lock().unwrap().push(event))
        };
        let (sender, receiver) = oneshot::channel();
        let completion = SyncCompletion::with_events(sender, Some(callback.clone()), "sync".into());
        completion.complete(Err((StatusCode::CONFLICT, "first".into())));
        let root = tempfile::tempdir().unwrap();
        let root_path = root.path().to_owned();
        let slots = Arc::new(Semaphore::new(1));
        completion.complete(Ok(ResultFile {
            path: root_path.join("result.json"),
            content_type: "application/json",
            keepalive: Some(Arc::new(JobInput {
                root,
                _slot: slots.clone().acquire_owned().await.unwrap(),
                deadline: far_deadline(),
                stem: "input".into(),
                canonical_filename: "input.pdf".into(),
                kind: DocumentKind::Pdf,
                upload: root_path.join("upload.pdf"),
                options: Submit::default(),
            })),
        }));
        assert!(!root_path.exists());
        assert!(slots.try_acquire().is_ok());
        assert!(matches!(receiver.await.unwrap(), Err((_, message)) if message == "first"));
        assert_eq!(
            *events.lock().unwrap(),
            vec![
                ProgressEvent::DocumentFailed {
                    document: "sync".into(),
                    message: "first".into()
                },
                ProgressEvent::RequestFailed {
                    label: "sync".into(),
                    message: "first".into()
                },
            ]
        );

        let workers = Arc::new(Mutex::new(Some(WorkerRegistry::new())));
        let records = Arc::new(Mutex::new(HashMap::new()));
        let (sender, receiver) = oneshot::channel();
        let completion = SyncCompletion::new(sender);
        {
            let mut registry = workers.lock().unwrap();
            let registry = registry.as_mut().unwrap();
            let task_id = registry
                .tasks
                .spawn(async { panic!("raw sync panic") })
                .id();
            registry
                .associations
                .insert(task_id, WorkerAssociation::Sync(completion.clone()));
        }
        tokio::task::yield_now().await;
        reap_workers(&workers, &records, &None).await;
        assert!(matches!(
            receiver.await.unwrap(),
            Err((StatusCode::CONFLICT, _))
        ));
        assert!(
            workers
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .associations
                .is_empty()
        );
        completion.complete(Err((StatusCode::CONFLICT, "ignored".into())));

        let (sender, receiver) = oneshot::channel();
        let completion = SyncCompletion::new(sender);
        {
            let mut registry = workers.lock().unwrap();
            let registry = registry.as_mut().unwrap();
            let task_id = registry.tasks.spawn(async {}).id();
            registry
                .associations
                .insert(task_id, WorkerAssociation::Sync(completion.clone()));
        }
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                reap_workers(&workers, &records, &None).await;
                if workers
                    .lock()
                    .unwrap()
                    .as_ref()
                    .unwrap()
                    .associations
                    .is_empty()
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(matches!(
            receiver.await.unwrap(),
            Err((StatusCode::CONFLICT, message)) if message == "task worker terminated unexpectedly"
        ));
        completion.complete(Err((StatusCode::CONFLICT, "ignored".into())));
    }

    #[tokio::test]
    async fn sync_completion_handles_caught_panic_raw_join_and_closed_registry_once() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().to_owned();
        let slots = Arc::new(Semaphore::new(1));
        std::fs::create_dir(path.join("input")).unwrap();
        std::fs::write(path.join("result.zip.partial"), b"x").unwrap();
        let input = Arc::new(JobInput {
            root,
            _slot: slots.clone().acquire_owned().await.unwrap(),
            deadline: far_deadline(),
            stem: "input".into(),
            canonical_filename: "input.pdf".into(),
            kind: DocumentKind::Pdf,
            upload: path.join("upload.pdf"),
            options: Submit::default(),
        });
        let (sender, receiver) = oneshot::channel();
        finish_sync_worker(
            SyncWorkerGuard::new(input, SyncCompletion::new(sender)),
            async { panic!("caught") },
        )
        .await;
        assert!(
            matches!(receiver.await.unwrap(), Err((StatusCode::CONFLICT, message)) if message == "task worker panicked")
        );
        assert!(!path.exists());
        assert!(slots.try_acquire().is_ok());

        let root = tempfile::tempdir().unwrap();
        let path = root.path().to_owned();
        let slots = Arc::new(Semaphore::new(1));
        let input = Arc::new(JobInput {
            root,
            _slot: slots.clone().acquire_owned().await.unwrap(),
            deadline: far_deadline(),
            stem: "input".into(),
            canonical_filename: "input.pdf".into(),
            kind: DocumentKind::Pdf,
            upload: path.join("upload.pdf"),
            options: Submit::default(),
        });
        let (sender, receiver) = oneshot::channel();
        let guard = SyncWorkerGuard::new(input, SyncCompletion::new(sender));
        let workers = Arc::new(Mutex::new(Some(WorkerRegistry::new())));
        let records = Arc::new(Mutex::new(HashMap::new()));
        {
            let mut registry = workers.lock().unwrap();
            let registry = registry.as_mut().unwrap();
            let completion = guard.completion.clone();
            let id = registry
                .tasks
                .spawn(async move {
                    let _guard = guard;
                    panic!("outer")
                })
                .id();
            registry
                .associations
                .insert(id, WorkerAssociation::Sync(completion));
        }
        assert!(matches!(
            tokio::time::timeout(Duration::from_secs(1), receiver)
                .await
                .unwrap()
                .unwrap(),
            Err((StatusCode::CONFLICT, _))
        ));
        reap_workers(&workers, &records, &None).await;
        assert!(
            workers
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .associations
                .is_empty()
        );
        assert!(!path.exists() && slots.try_acquire().is_ok());

        let root = tempfile::tempdir().unwrap();
        let path = root.path().to_owned();
        let slots = Arc::new(Semaphore::new(1));
        let input = Arc::new(JobInput {
            root,
            _slot: slots.clone().acquire_owned().await.unwrap(),
            deadline: far_deadline(),
            stem: "input".into(),
            canonical_filename: "input.pdf".into(),
            kind: DocumentKind::Pdf,
            upload: path.join("upload.pdf"),
            options: Submit::default(),
        });
        let (sender, receiver) = oneshot::channel();
        let guard = SyncWorkerGuard::new(input, SyncCompletion::new(sender));
        let none = Arc::new(Mutex::new(None));
        let context = WorkerContext {
            gate: Arc::new(Semaphore::new(1)),
            route: OfficialPdfOptions::default(),
            env_formula: None,
            env_table: None,
            official_page_concurrency: 4,
            office_workers: OfficeWorkers::new().unwrap(),
            raster_workers: RasterWorkers::default(),
            events: None,
            server_zip_cap: crate::DocumentLimitPolicy::defaults().server_zip_bytes,
            totals: crate::document_limits::OfficialDocumentTotals::from_options(
                &OfficialPdfOptions::default(),
            ),
            test_http: None,
        };
        let guard = spawn_sync_worker(&none, context, guard, &None, "input".into()).unwrap_err();
        guard.discard();
        assert!(receiver.await.is_err());
        assert!(!path.exists() && slots.try_acquire().is_ok());

        let root = tempfile::tempdir().unwrap();
        let path = root.path().to_owned();
        let slots = Arc::new(Semaphore::new(1));
        let input = Arc::new(JobInput {
            root,
            _slot: slots.clone().acquire_owned().await.unwrap(),
            deadline: far_deadline(),
            stem: "input".into(),
            canonical_filename: "input.pdf".into(),
            kind: DocumentKind::Pdf,
            upload: path.join("upload.pdf"),
            options: Submit::default(),
        });
        let (sender, receiver) = oneshot::channel::<SyncOutcome>();
        drop(receiver);
        finish_sync_worker(
            SyncWorkerGuard::new(input, SyncCompletion::new(sender)),
            async {
                Ok(ResultFile {
                    path: path.join("result.json"),
                    content_type: "application/json",
                    keepalive: None,
                })
            },
        )
        .await;
        assert!(!path.exists() && slots.try_acquire().is_ok());
    }

    #[tokio::test]
    async fn sync_result_stream_owns_input_until_eof_or_drop() {
        async fn fixture(bytes: Vec<u8>) -> (Response, PathBuf, Arc<Semaphore>) {
            let root = tempfile::tempdir().unwrap();
            let path = root.path().to_owned();
            let result = path.join("result.json");
            tokio::fs::write(&result, bytes).await.unwrap();
            let slots = Arc::new(Semaphore::new(1));
            let input = Arc::new(JobInput {
                root,
                _slot: slots.clone().acquire_owned().await.unwrap(),
                deadline: far_deadline(),
                stem: "input".into(),
                canonical_filename: "input.pdf".into(),
                kind: DocumentKind::Pdf,
                upload: path.join("upload.pdf"),
                options: Submit::default(),
            });
            (
                stream_result(
                    ResultFile {
                        path: result,
                        content_type: "application/json",
                        keepalive: Some(input),
                    },
                    None,
                )
                .await,
                path,
                slots,
            )
        }
        let (response, path, slots) = fixture(b"one".to_vec()).await;
        assert!(path.exists() && slots.try_acquire().is_err());
        drop(response);
        assert!(!path.exists() && slots.try_acquire().is_ok());

        let (response, path, slots) = fixture(b"full".to_vec()).await;
        assert!(path.exists() && slots.try_acquire().is_err());
        let bytes = axum::body::to_bytes(response.into_body(), 1024)
            .await
            .unwrap();
        assert_eq!(&bytes[..], b"full");
        assert!(!path.exists() && slots.try_acquire().is_ok());

        let (response, path, slots) = fixture(vec![b'x'; 200_000]).await;
        let mut stream = response.into_body().into_data_stream();
        assert_eq!(stream.next().await.unwrap().unwrap().len(), 65536);
        assert!(path.exists() && slots.try_acquire().is_err());
        drop(stream);
        assert!(!path.exists() && slots.try_acquire().is_ok());
    }

    #[tokio::test]
    #[ignore = "full VLM API lifecycle integration e2e"]
    async fn file_parse_pdf_image_office_zip_json_is_recordless() {
        let model = mock(false, false).await;
        let service = test_service_office(
            pdf_limits(),
            OfficeWorkers::with_test_executable(std::env::current_exe().unwrap()),
        )
        .await;
        for (name, bytes) in [
            ("picked.pdf", nested_fixture()),
            ("photo.png", tiny_png()),
            ("letter.docx", tiny_docx()),
        ] {
            for zip in [true, false] {
                let response = file_post(
                    &service,
                    mixed_sync_form(&model.url, name, bytes.clone(), zip),
                )
                .await;
                assert_eq!(response.status(), StatusCode::OK);
                assert_eq!(
                    response.headers()[header::CONTENT_TYPE],
                    if zip {
                        "application/zip"
                    } else {
                        "application/json"
                    }
                );
                assert!(response.headers().get("location").is_none());
                assert!(
                    response
                        .headers()
                        .keys()
                        .all(|name| !name.as_str().starts_with("x-mineru-task"))
                );
                let body = response.bytes().await.unwrap();
                let stem = name.rsplit_once('.').unwrap().0;
                let target = if name.ends_with(".docx") {
                    "office"
                } else {
                    "vlm"
                };
                if zip {
                    let mut archive = zip::ZipArchive::new(std::io::Cursor::new(body)).unwrap();
                    assert!((0..archive.len()).all(|index| {
                        archive
                            .by_index(index)
                            .unwrap()
                            .name()
                            .starts_with(&format!("{stem}/{target}/"))
                    }));
                    assert!(
                        archive
                            .by_name(&format!("{stem}/{target}/{stem}.md"))
                            .is_ok()
                    );
                    let mut origin = Vec::new();
                    archive
                        .by_name(&format!(
                            "{stem}/{target}/{stem}_origin.{}",
                            name.rsplit('.').next().unwrap()
                        ))
                        .unwrap()
                        .read_to_end(&mut origin)
                        .unwrap();
                    if name.ends_with(".pdf") {
                        assert_eq!(
                            lopdf::Document::load_mem(&origin)
                                .unwrap()
                                .get_pages()
                                .len(),
                            1
                        );
                    } else {
                        assert_eq!(origin, bytes);
                    }
                } else {
                    let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
                    assert_eq!(value["backend"], "vlm-http-client");
                    assert!(value["version"].is_string());
                    assert_eq!(
                        value
                            .as_object()
                            .unwrap()
                            .keys()
                            .map(String::as_str)
                            .collect::<std::collections::BTreeSet<_>>(),
                        ["backend", "results", "version"].into_iter().collect()
                    );
                    let result = value["results"].as_object().unwrap();
                    assert_eq!(result.len(), 1);
                    assert!(result.contains_key(stem));
                    let result = result[stem].as_object().unwrap();
                    assert_eq!(
                        result
                            .keys()
                            .map(String::as_str)
                            .collect::<std::collections::BTreeSet<_>>(),
                        [
                            "content_list",
                            "images",
                            "md_content",
                            "middle_json",
                            "model_output"
                        ]
                        .into_iter()
                        .collect()
                    );
                    for key in [
                        "origin",
                        "layout",
                        "task",
                        "status",
                        "message",
                        "status_url",
                        "result_url",
                    ] {
                        assert!(!result.contains_key(key));
                    }
                }
                assert_eq!(std::fs::read_dir(&service.output).unwrap().count(), 0);
                let health: serde_json::Value = reqwest::get(format!("{}/health", service.base))
                    .await
                    .unwrap()
                    .json()
                    .await
                    .unwrap();
                assert_eq!(health["task_count"], 0);
            }
        }
        let health: serde_json::Value = reqwest::get(format!("{}/health", service.base))
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(health["task_count"], 0);
        let task: serde_json::Value = post(&service, pdf_form(&model.url, "0", "0"))
            .await
            .json()
            .await
            .unwrap();
        assert_eq!(task["task_id"], "local-1");
        wait_status(task["status_url"].as_str().unwrap(), "completed").await;
        service.stop().await;
        let _ = model.stop.send(());
        model.task.await.unwrap();
    }

    #[tokio::test]
    #[ignore = "full VLM API lifecycle integration e2e"]
    async fn sync_failure_is_sanitized_and_atomic() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().to_owned();
        for name in [
            "result.zip",
            "result.zip.partial",
            "result.json",
            "result.json.partial",
        ] {
            std::fs::write(path.join(name), b"partial").unwrap();
        }
        std::fs::create_dir(path.join("input")).unwrap();
        let slots = Arc::new(Semaphore::new(1));
        let input = Arc::new(JobInput {
            root,
            _slot: slots.clone().acquire_owned().await.unwrap(),
            deadline: far_deadline(),
            stem: "input".into(),
            canonical_filename: "input.pdf".into(),
            kind: DocumentKind::Pdf,
            upload: path.join("upload.pdf"),
            options: Submit::default(),
        });
        let future_input = input.clone();
        let (sender, receiver) = oneshot::channel();
        finish_sync_worker(
            SyncWorkerGuard::new(input, SyncCompletion::new(sender)),
            async move {
                cleanup_input(&future_input).await;
                Err::<ResultFile, String>(
                    "Authorization: Bearer sync-auth-secret https://host/path?key=sync-query-secret"
                        .into(),
                )
            },
        )
        .await;
        let Err((status, detail)) = receiver.await.unwrap() else {
            panic!("sync worker unexpectedly succeeded");
        };
        assert_eq!(status, StatusCode::CONFLICT);
        for secret in [
            "sync-auth-secret",
            "sync-query-secret",
            "Bearer sync-auth-secret",
            "https://host/path?key=sync-query-secret",
        ] {
            assert!(!detail.contains(secret));
        }
        assert!(!path.exists());
        assert!(slots.try_acquire().is_ok());

        let model = mock(false, false).await;
        let service = test_service(pdf_limits(), 1).await;
        let response = file_post(
            &service,
            mixed_sync_form(&model.url, "bad.png", b"not png".to_vec(), true),
        )
        .await;
        assert_eq!(response.status(), StatusCode::CONFLICT);
        assert!(response.headers().get(header::LOCATION).is_none());
        assert!(
            response
                .headers()
                .keys()
                .all(|name| !name.as_str().starts_with("x-mineru-task"))
        );
        let value: serde_json::Value = response.json().await.unwrap();
        assert!(!value["detail"].as_str().unwrap().is_empty());
        assert!(!value["detail"].as_str().unwrap().contains("Bearer"));
        assert_eq!(model.state.entered.load(Ordering::SeqCst), 0);
        assert_eq!(std::fs::read_dir(&service.output).unwrap().count(), 0);
        let response = file_post(&service, form(canonical_fields(), vec![])).await;
        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
        response.bytes().await.unwrap();
        assert_eq!(std::fs::read_dir(&service.output).unwrap().count(), 0);
        let response = file_post(
            &service,
            mixed_sync_form(&model.url, "picked.pdf", nested_fixture(), true),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        response.bytes().await.unwrap();
        assert_eq!(std::fs::read_dir(&service.output).unwrap().count(), 0);
        service.stop().await;
        let _ = model.stop.send(());
        model.task.await.unwrap();
    }

    #[tokio::test]
    #[ignore = "full VLM API lifecycle integration e2e"]
    async fn async_pdf_image_office_use_shared_prepared_route() {
        let model = mock(false, false).await;
        let service = test_service_office(
            pdf_limits(),
            OfficeWorkers::with_test_executable(std::env::current_exe().unwrap()),
        )
        .await;
        let inputs = [
            ("picked.pdf", nested_fixture(), "vlm", "pdf"),
            ("photo.png", tiny_png(), "vlm", "png"),
            ("letter.docx", tiny_docx(), "office", "docx"),
        ];
        for (filename, bytes, target, suffix) in inputs {
            let task: serde_json::Value =
                post(&service, mixed_form(&model.url, filename, bytes.clone()))
                    .await
                    .json()
                    .await
                    .unwrap();
            wait_status(task["status_url"].as_str().unwrap(), "completed").await;
            let archive = reqwest::get(task["result_url"].as_str().unwrap())
                .await
                .unwrap()
                .bytes()
                .await
                .unwrap();
            let mut zip = zip::ZipArchive::new(std::io::Cursor::new(archive)).unwrap();
            let stem = filename.rsplit_once('.').unwrap().0;
            let origin_name = format!("{stem}/{target}/{stem}_origin.{suffix}");
            let mut origin = Vec::new();
            zip.by_name(&origin_name)
                .unwrap()
                .read_to_end(&mut origin)
                .unwrap();
            if suffix == "pdf" {
                assert_eq!(
                    lopdf::Document::load_mem(&origin)
                        .unwrap()
                        .get_pages()
                        .len(),
                    1
                );
            } else {
                assert_eq!(origin, bytes);
            }
            assert!(
                zip.file_names()
                    .any(|name| name.starts_with(&format!("{stem}/{target}/")))
            );
        }
        service.stop().await;
        let _ = model.stop.send(());
        model.task.await.unwrap();
    }

    #[tokio::test]
    async fn mixed_content_mismatch_fails_and_cleans() {
        let model = mock(false, false).await;
        let service = test_service(pdf_limits(), 1).await;
        let task: serde_json::Value = post(
            &service,
            mixed_form(&model.url, "bad.png", b"not png".to_vec()),
        )
        .await
        .json()
        .await
        .unwrap();
        wait_status(task["status_url"].as_str().unwrap(), "failed").await;
        assert_eq!(model.state.entered.load(Ordering::SeqCst), 0);
        let root = std::fs::read_dir(&service.output)
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        assert!(!root.join("result.zip").exists() && !root.join("result.zip.partial").exists());
        assert!(!root.join("bad").exists());
        service.stop().await;
        let _ = model.stop.send(());
        model.task.await.unwrap();
    }

    #[tokio::test]
    #[ignore = "full VLM API lifecycle integration e2e"]
    async fn deadline_includes_queue_wait_and_bounds_connect() {
        let deadline = Duration::from_secs(4);
        let mut route = OfficialPdfOptions::default();
        route.total_deadline = deadline;
        let http = VlmHttpConfig {
            model_name: Some("mock".into()),
            skip_model_name_checking: true,
            max_retries: 0,
            http_timeout: Duration::from_secs(5),
            connect_timeout: Duration::from_secs(5),
            ..Default::default()
        };
        let model = mock(false, true).await;
        let service = test_service_route(pdf_limits(), 2, 1, route.clone(), http).await;
        let first: serde_json::Value = post(&service, pdf_form(&model.url, "0", "0"))
            .await
            .json()
            .await
            .unwrap();
        wait_status(first["status_url"].as_str().unwrap(), "processing").await;
        let second_at = Instant::now();
        let second: serde_json::Value = post(&service, pdf_form(&model.url, "0", "0"))
            .await
            .json()
            .await
            .unwrap();
        let queued: serde_json::Value = reqwest::get(second["status_url"].as_str().unwrap())
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(queued["status"], "pending");
        assert!(queued["started_at"].is_null());
        tokio::time::timeout(Duration::from_secs(5), async {
            while model.state.entered.load(Ordering::SeqCst) == 0 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        wait_status(first["status_url"].as_str().unwrap(), "failed").await;
        assert!(second_at.elapsed() >= deadline.saturating_sub(Duration::from_millis(400)));
        wait_status(second["status_url"].as_str().unwrap(), "failed").await;
        assert!(second_at.elapsed() < deadline + Duration::from_secs(2));
        model.state.block.store(false, Ordering::SeqCst);
        model.state.release.notify_waiters();
        assert!(std::fs::read_dir(&service.output).unwrap().any(|entry| {
            let path = entry.unwrap().path();
            !path.join("result.zip").exists()
                && !path.join("result.json").exists()
                && !path.join("input").exists()
        }));
        service.stop().await;
        let _ = model.stop.send(());
        model.task.await.unwrap();

        #[derive(Clone)]
        struct ModelsState {
            entered: Arc<tokio::sync::Notify>,
            release: Arc<tokio::sync::Notify>,
        }
        async fn models(State(state): State<ModelsState>) -> Response {
            state.entered.notify_one();
            state.release.notified().await;
            Json(json!({"data":[]})).into_response()
        }
        let state = ModelsState {
            entered: Arc::new(tokio::sync::Notify::new()),
            release: Arc::new(tokio::sync::Notify::new()),
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let (stop, stopped) = oneshot::channel();
        let server_state = state.clone();
        let server = tokio::spawn(async move {
            axum::serve(
                listener,
                Router::new()
                    .route("/v1/models", get(models))
                    .with_state(server_state),
            )
            .with_graceful_shutdown(async move {
                let _ = stopped.await;
            })
            .await
            .unwrap();
        });
        let api = test_service_route(
            pdf_limits(),
            1,
            1,
            route,
            VlmHttpConfig {
                model_name: None,
                skip_model_name_checking: false,
                max_retries: 0,
                http_timeout: Duration::from_secs(15),
                connect_timeout: Duration::from_secs(15),
                ..Default::default()
            },
        )
        .await;
        let started = Instant::now();
        let task: serde_json::Value = post(&api, pdf_form(&url, "0", "0"))
            .await
            .json()
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(5), state.entered.notified())
            .await
            .unwrap();
        let failed = wait_status(task["status_url"].as_str().unwrap(), "failed").await;
        assert!(started.elapsed() < deadline + Duration::from_secs(2));
        assert!(
            failed["error"]
                .as_str()
                .is_some_and(|error| !error.is_empty())
        );
        let root = std::fs::read_dir(&api.output)
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        assert!(
            !root.join("result.zip").exists()
                && !root.join("result.json").exists()
                && !root.join("input").exists()
        );
        api.stop().await;
        state.release.notify_waiters();
        let _ = stop.send(());
        server.await.unwrap();
    }

    #[test]
    fn packaging_origin_path_is_committed_manifest_origin() {
        let root = tempfile::tempdir().unwrap();
        let manifest = OfficialOutputManifest {
            root: root.path().to_owned(),
            stem: "report".into(),
            vlm_dir: root.path().join("nested/office"),
        };
        std::fs::create_dir_all(&manifest.vlm_dir).unwrap();
        std::fs::write(root.path().join("upload.docx"), b"wrong").unwrap();
        let origin = committed_origin(&manifest, DocumentKind::Docx);
        assert_eq!(origin, manifest.vlm_dir.join("report_origin.docx"));
        assert!(origin.starts_with(&manifest.vlm_dir));
    }

    #[test]
    fn failed_cleanup_removes_published_artifacts() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("result.zip"), b"x").unwrap();
        std::fs::write(root.path().join("result.zip.partial"), b"x").unwrap();
        std::fs::create_dir(root.path().join("out")).unwrap();
        cleanup_failure(root.path(), "out");
        assert!(!root.path().join("result.zip").exists());
        assert!(!root.path().join("result.zip.partial").exists());
        assert!(!root.path().join("out").exists());
    }

    #[test]
    fn page_numbers_are_ascii_decimal_only() {
        for value in ["", "+1", " 1", "1 ", "1_0", "١", "-1"] {
            assert!(number(value).is_err(), "{value:?}");
        }
        assert_eq!(number("12"), Ok(12));
        assert!(number("0012").is_err());
    }

    #[test]
    fn selected_end_only_treats_exact_sentinel_as_last_page() {
        assert_eq!(selected_end(99_999, 4), 4);
        assert_eq!(selected_end(100_000, 100_001), 100_000);
        assert_eq!(selected_end(u64::MAX, 4), 4);
        assert_eq!(selected_end(2, 4), 2);
    }

    #[test]
    fn service_config_rejects_invalid_semaphore_capacity() {
        let config = |concurrency| {
            ServiceConfig::new(
                concurrency,
                PathBuf::new(),
                OfficialPdfOptions::default(),
                None,
                None,
            )
        };
        assert!(config(0).is_err());
        assert!(config(Semaphore::MAX_PERMITS).is_ok());
        if let Some(too_many) = Semaphore::MAX_PERMITS.checked_add(1) {
            assert!(config(too_many).is_err());
        }
        let config = config(1).unwrap();
        assert!(!config.public_bind_exposed && !config.allow_public_http_client);
        let config = config.public_policy(true, true);
        assert!(config.public_bind_exposed && config.allow_public_http_client);
        let lifecycle = config
            .clone()
            .task_lifecycle(Duration::ZERO, Duration::from_secs(1))
            .unwrap();
        assert_eq!(lifecycle.retention, Duration::ZERO);
        assert_eq!(lifecycle.cleanup_interval, Duration::from_secs(1));
        assert!(matches!(
            config.task_lifecycle(Duration::ZERO, Duration::ZERO),
            Err(message) if message == "MINERU_API_TASK_CLEANUP_INTERVAL_SECONDS must be positive"
        ));
    }

    #[test]
    fn service_config_page_concurrency_reaches_task_effective_permits() {
        let mut route = OfficialPdfOptions::default();
        route.processing_window_size = 8;
        let configured = ServiceConfig::new(1, PathBuf::new(), route, None, None)
            .unwrap()
            .official_page_concurrency(7)
            .unwrap();
        assert_eq!(configured.official_page_concurrency, 7);
        assert_eq!(
            crate::official_route::effective_page_concurrency(
                configured.official_page_concurrency,
                configured.route.processing_window_size,
                8,
            ),
            7
        );
        assert_eq!(
            ServiceConfig::new(1, PathBuf::new(), OfficialPdfOptions::default(), None, None)
                .unwrap()
                .official_page_concurrency,
            4
        );
        for value in [0, 9] {
            assert!(configured.clone().official_page_concurrency(value).is_err());
        }
    }

    #[test]
    fn request_authority_policy_table() {
        for value in ["127.0.0.1", "ExAmPlE.test:080", "[::1]:80"] {
            assert!(parse_request_authority(value).is_some(), "{value}");
        }
        for value in [
            "",
            "bad host",
            "host,other",
            "@host",
            "host/path",
            "host?x",
            "host#x",
            "[::1",
            "::1",
            "host:",
            "host:no",
            "host:65536",
            ":80",
            "[nope]",
        ] {
            assert!(parse_request_authority(value).is_none(), "{value}");
        }
        assert_eq!(parse_request_authority("host:080").unwrap().1, Some(80));

        let authority = |uri: &str, hosts: &[&[u8]]| {
            let mut request = axum::http::Request::builder().uri(uri).body(()).unwrap();
            for host in hosts {
                request.headers_mut().append(
                    header::HOST,
                    axum::http::HeaderValue::from_bytes(host).unwrap(),
                );
            }
            request_authority(&request.into_parts().0)
        };
        for (uri, host, expected) in [
            ("/tasks", b"127.0.0.1".as_slice(), "127.0.0.1"),
            ("/tasks", b"ExAmPlE.test:080".as_slice(), "ExAmPlE.test:080"),
            ("/tasks", b"[::1]:80".as_slice(), "[::1]:80"),
            (
                "http://Original.Example:080/tasks",
                b"".as_slice(),
                "Original.Example:080",
            ),
            (
                "http://Original.Example:080/tasks",
                b"original.example:80".as_slice(),
                "Original.Example:080",
            ),
        ] {
            let hosts = (!host.is_empty())
                .then_some(host)
                .into_iter()
                .collect::<Vec<_>>();
            assert_eq!(authority(uri, &hosts).as_deref(), Some(expected));
        }
        for (uri, hosts) in [
            ("/tasks", vec![]),
            ("/tasks", vec![b"one".as_slice(), b"two".as_slice()]),
            ("/tasks", vec![b"\xff".as_slice()]),
            ("/tasks", vec![b"bad/path".as_slice()]),
            ("http://example.test/tasks", vec![b"other.test".as_slice()]),
            (
                "http://example.test:80/tasks",
                vec![b"example.test".as_slice()],
            ),
            (
                "http://example.test/tasks",
                vec![b"example.test:80".as_slice()],
            ),
            ("https://example.test/tasks", vec![]),
            ("ftp://example.test/tasks", vec![]),
            (
                "http://example.test/tasks",
                vec![b"example.test".as_slice(), b"example.test".as_slice()],
            ),
        ] {
            assert!(authority(uri, &hosts).is_none(), "{uri:?} {hosts:?}");
        }
        let mut request = axum::http::Request::builder()
            .uri("/tasks")
            .body(())
            .unwrap();
        request
            .headers_mut()
            .insert("forwarded", "host=other.test".parse().unwrap());
        request
            .headers_mut()
            .insert("x-forwarded-host", "other.test".parse().unwrap());
        request
            .headers_mut()
            .insert(header::HOST, "example.test".parse().unwrap());
        assert_eq!(
            request_authority(&request.into_parts().0).as_deref(),
            Some("example.test")
        );
    }

    #[test]
    fn compact_pdf_rejects_cyclic_parent_references() {
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("cycle.pdf");
        let output = dir.path().join("out.pdf");
        let mut doc = lopdf::Document::with_version("1.5");
        let root = doc.new_object_id();
        let page = doc.new_object_id();
        doc.objects.insert(
            page,
            dictionary! { "Type" => "Page", "Parent" => page }.into(),
        );
        doc.objects.insert(
            root,
            dictionary! { "Type" => "Pages", "Kids" => vec![page.into()], "Count" => 1 }.into(),
        );
        let catalog = doc.add_object(dictionary! { "Type" => "Catalog", "Pages" => root });
        doc.trailer.set("Root", catalog);
        doc.save(&input).unwrap();
        assert!(compact_pdf(&input, &output, 0, 0, 64 * 1024, far_deadline()).is_err());
    }

    #[test]
    fn task_server_url_overrides_invalid_default_without_inherited_credentials() {
        let config = VlmHttpConfig {
            invalid_server_url: true,
            api_key: Some("api-key-secret".into()),
            headers: vec![
                crate::VlmHeader::new("Authorization", "Bearer header-secret").unwrap(),
                crate::VlmHeader::new("X-Trace", "kept").unwrap(),
                crate::VlmHeader::new("AUTHORIZATION", "Basic second-secret").unwrap(),
            ],
            ..Default::default()
        };
        let config = task_vlm_config(config, Some("http://127.0.0.1:8000")).unwrap();
        assert!(!config.invalid_server_url);
        assert_eq!(config.authorization(), None);
        assert_eq!(config.api_key, None);
        assert_eq!(config.headers.len(), 1);
        assert_eq!(config.headers[0].name(), "X-Trace");
        assert_eq!(config.headers[0].value(), "kept");
        assert_eq!(
            config.server_url.unwrap().as_str(),
            "http://127.0.0.1:8000/"
        );
    }

    #[tokio::test]
    async fn task_server_override_sends_non_auth_headers_but_no_credentials() {
        let seen = Arc::new(Mutex::new(None));
        let captured = seen.clone();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(
            axum::serve(
                listener,
                Router::new().route(
                    "/v1/models",
                    get(move |headers: axum::http::HeaderMap| {
                        *captured.lock().unwrap() = Some(headers);
                        async { Json(json!({"data":[{"id":"mock"}]})) }
                    }),
                ),
            )
            .into_future(),
        );
        let config = task_vlm_config(
            VlmHttpConfig {
                api_key: Some("api-key-secret".into()),
                headers: vec![
                    crate::VlmHeader::new("Authorization", "Bearer header-secret").unwrap(),
                    crate::VlmHeader::new("X-Trace", "kept").unwrap(),
                ],
                ..Default::default()
            },
            Some(&url),
        )
        .unwrap();
        MinerUVlmClient::connect_for_task(
            config,
            MinerUVlmConfig::default(),
            crate::TaskWorkLease::default(),
        )
        .await
        .unwrap();
        let headers = seen.lock().unwrap().take().unwrap();
        assert!(headers.get(header::AUTHORIZATION).is_none());
        assert_eq!(headers["x-trace"], "kept");
        server.abort();
    }

    #[tokio::test]
    async fn serve_is_spawnable_and_drains_shutdown() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let (stop, stopped) = tokio::sync::oneshot::channel();
        let events = Arc::new(Mutex::new(Vec::new()));
        let callback: ProgressCallback = {
            let events = events.clone();
            Arc::new(move |event| events.lock().unwrap().push(event))
        };
        let config = ServiceConfig::new(
            1,
            tempfile::tempdir().unwrap().keep(),
            OfficialPdfOptions::default(),
            None,
            None,
        )
        .unwrap()
        .progress_callback(callback);
        let task = tokio::spawn(serve(listener, config, async move {
            let _ = stopped.await;
        }));
        stop.send(()).unwrap();
        assert!(
            tokio::time::timeout(Duration::from_secs(1), task)
                .await
                .unwrap()
                .unwrap()
                .is_ok()
        );
        assert!(matches!(
            events.lock().unwrap().as_slice(),
            [
                ProgressEvent::ServerStarted { .. },
                ProgressEvent::ServerStopped
            ]
        ));
    }

    #[tokio::test]
    async fn serve_rejects_non_loopback_listener() {
        let listener = tokio::net::TcpListener::bind("0.0.0.0:0").await.unwrap();
        let events = Arc::new(Mutex::new(Vec::new()));
        assert_eq!(
            serve(
                listener,
                ServiceConfig::new(
                    1,
                    tempfile::tempdir().unwrap().keep(),
                    OfficialPdfOptions::default(),
                    None,
                    None
                )
                .unwrap()
                .progress_callback({
                    let events = events.clone();
                    Arc::new(move |event| events.lock().unwrap().push(event))
                }),
                std::future::pending()
            )
            .await
            .unwrap_err()
            .kind(),
            std::io::ErrorKind::PermissionDenied
        );
        assert!(events.lock().unwrap().is_empty());
    }

    #[tokio::test]
    #[ignore = "full VLM API lifecycle integration e2e"]
    async fn progress_events_cover_router_rejections_and_acceptance_labels() {
        let model = mock(false, false).await;
        let events = Arc::new(Mutex::new(Vec::new()));
        let service = test_service_events(Some({
            let events = events.clone();
            Arc::new(move |event| events.lock().unwrap().push(event))
        }))
        .await;

        let response = reqwest::Client::new()
            .post(format!("{}/tasks", service.base))
            .body("not multipart")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let mut fields = canonical_fields();
        fields
            .iter_mut()
            .find(|(name, _)| name == "backend")
            .unwrap()
            .1 = "wrong".into();
        let response = post(&service, form(fields, vec![])).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            response.text().await.unwrap(),
            r#"{"detail":"unsupported backend"}"#
        );

        let async_task: serde_json::Value = post(&service, pdf_form(&model.url, "0", "0"))
            .await
            .json()
            .await
            .unwrap();
        wait_status(async_task["status_url"].as_str().unwrap(), "completed").await;
        assert_eq!(
            file_post(
                &service,
                pdf_form_named(&model.url, "0", "0", "sync-name.pdf")
            )
            .await
            .status(),
            StatusCode::OK
        );
        let request_events: Vec<_> = events
            .lock()
            .unwrap()
            .iter()
            .filter_map(|event| match event {
                ProgressEvent::RequestAccepted { label } => Some((true, label.clone())),
                ProgressEvent::RequestRejected { message } => Some((false, message.clone())),
                _ => None,
            })
            .collect();
        assert_eq!(
            request_events,
            vec![
                (false, "invalid multipart form".into()),
                (false, "unsupported backend".into()),
                (true, "local-1".into()),
                (true, "sync-name".into()),
            ]
        );
        let document_events: Vec<_> = events
            .lock()
            .unwrap()
            .iter()
            .filter_map(|event| match event {
                ProgressEvent::RequestAccepted { label }
                | ProgressEvent::DocumentStarted { document: label }
                | ProgressEvent::DocumentPrepared { document: label }
                | ProgressEvent::DocumentCompleted { document: label } => Some(label.clone()),
                ProgressEvent::DocumentPageCompleted { document, .. } => Some(document.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(
            document_events,
            vec![
                "local-1",
                "input",
                "input",
                "input",
                "input",
                "sync-name",
                "sync-name",
                "sync-name",
                "sync-name",
                "sync-name"
            ]
        );
        let completed: Vec<_> = events
            .lock()
            .unwrap()
            .iter()
            .filter_map(|event| match event {
                ProgressEvent::RequestCompleted { label } => Some(label.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(completed, ["local-1", "sync-name"]);
        service.stop().await;
        let _ = model.stop.send(());
        model.task.await.unwrap();
    }

    #[tokio::test]
    #[ignore = "full VLM API lifecycle integration e2e"]
    async fn progress_callback_panics_do_not_change_rejection_or_shutdown() {
        let model = mock(false, false).await;
        let service = test_service_events(Some(Arc::new(|_| panic!("callback")))).await;
        let response = file_post(&service, pdf_form(&model.url, "0", "0")).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert!(!response.bytes().await.unwrap().is_empty());
        service.stop().await;
        let _ = model.stop.send(());
        model.task.await.unwrap();
    }

    #[tokio::test]
    async fn office_success_warns_before_document_prepared() {
        let model = mock(false, false).await;
        let events = Arc::new(Mutex::new(Vec::new()));
        let service = test_service_office_events({
            let events = events.clone();
            Arc::new(move |event| events.lock().unwrap().push(event))
        })
        .await;
        assert_eq!(
            file_post(
                &service,
                mixed_sync_form(&model.url, "letter.docx", tiny_docx(), false)
            )
            .await
            .status(),
            StatusCode::OK
        );
        let events = events.lock().unwrap();
        let start = events.iter().position(|event| matches!(event, ProgressEvent::DocumentStarted { document } if document == "letter")).unwrap();
        let warning = events.iter().position(|event| matches!(event, ProgressEvent::OfficeWarning { document, message } if document == "letter" && message == "simultaneous stderr\\n")).unwrap();
        let prepared = events.iter().position(|event| matches!(event, ProgressEvent::DocumentPrepared { document } if document == "letter")).unwrap();
        let completed = events.iter().position(|event| matches!(event, ProgressEvent::DocumentCompleted { document } if document == "letter")).unwrap();
        assert!(start < warning && warning < prepared && prepared < completed);
        drop(events);
        service.stop().await;
        let _ = model.stop.send(());
        model.task.await.unwrap();
    }

    #[tokio::test]
    async fn preparation_error_does_not_complete_document_event() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let service = test_service_events(Some({
            let events = events.clone();
            Arc::new(move |event| events.lock().unwrap().push(event))
        }))
        .await;
        let task: serde_json::Value = post(&service, canonical_form()).await.json().await.unwrap();
        wait_status(task["status_url"].as_str().unwrap(), "failed").await;
        let terminal: Vec<_> = events
            .lock()
            .unwrap()
            .iter()
            .filter(|event| match event {
                ProgressEvent::RequestAccepted { label }
                | ProgressEvent::RequestFailed { label, .. } => label == "local-1",
                ProgressEvent::DocumentStarted { document }
                | ProgressEvent::DocumentFailed { document, .. } => document == "input",
                _ => false,
            })
            .cloned()
            .collect();
        assert!(matches!(terminal.as_slice(), [
            ProgressEvent::RequestAccepted { .. },
            ProgressEvent::DocumentStarted { .. },
            ProgressEvent::DocumentFailed { message: first, .. },
            ProgressEvent::RequestFailed { message: second, .. },
        ] if first == second));
        assert!(!events.lock().unwrap().iter().any(|event| matches!(event, ProgressEvent::DocumentCompleted { document } if document == "input") || matches!(event, ProgressEvent::RequestCompleted { label } if label == "local-1")));
        service.stop().await;
    }

    #[tokio::test]
    async fn default_progress_callback_keeps_router_response() {
        let service = test_service_events(None).await;
        assert_eq!(
            reqwest::get(format!("{}/health", service.base))
                .await
                .unwrap()
                .status(),
            StatusCode::OK
        );
        service.stop().await;
    }

    #[tokio::test]
    async fn public_policy_exposure_and_pre_body_posts() {
        async fn raw(addr: std::net::SocketAddr, path: &str, host: &str) -> String {
            let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
            stream
                .write_all(format!("POST {path} HTTP/1.1\r\nHost: {host}\r\nContent-Type: multipart/form-data; boundary=x\r\nContent-Length: 1\r\n\r\n").as_bytes())
                .await
                .unwrap();
            let mut response = vec![0; 1024];
            let size = tokio::time::timeout(Duration::from_secs(1), stream.read(&mut response))
                .await
                .unwrap()
                .unwrap();
            String::from_utf8_lossy(&response[..size]).into_owned()
        }
        let root = tempfile::tempdir().unwrap();
        let output = root.path().join("tasks");
        let listener = tokio::net::TcpListener::bind("0.0.0.0:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let client_addr = std::net::SocketAddr::from(([127, 0, 0, 1], addr.port()));
        let (stop, stopped) = oneshot::channel();
        let config =
            ServiceConfig::new(1, output.clone(), OfficialPdfOptions::default(), None, None)
                .unwrap()
                .public_policy(true, false)
                .test_limits(pdf_limits(), 0, RETENTION, CLEANUP_INTERVAL);
        let server = tokio::spawn(serve(listener, config, async move {
            let _ = stopped.await;
        }));
        let base = format!("http://127.0.0.1:{}", addr.port());
        assert_eq!(
            reqwest::get(format!("{base}/health"))
                .await
                .unwrap()
                .status(),
            StatusCode::OK
        );
        assert_eq!(
            reqwest::get(format!("{base}/tasks/nope"))
                .await
                .unwrap()
                .status(),
            StatusCode::NOT_FOUND
        );
        for path in ["/tasks", "/file_parse"] {
            let response = raw(client_addr, path, "localhost").await;
            assert!(response.starts_with("HTTP/1.1 400"));
            assert!(response.contains("public HTTP-client requests are disabled"));
        }
        let response = raw(client_addr, "/tasks", "bad/path").await;
        assert!(response.starts_with("HTTP/1.1 400"));
        assert!(response.contains("public HTTP-client requests are disabled"));
        assert_eq!(std::fs::read_dir(&output).unwrap().count(), 0);
        stop.send(()).unwrap();
        assert!(server.await.unwrap().is_ok());

        let root = tempfile::tempdir().unwrap();
        let output = root.path().join("tasks");
        let listener = tokio::net::TcpListener::bind("0.0.0.0:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let client_addr = std::net::SocketAddr::from(([127, 0, 0, 1], addr.port()));
        let (stop, stopped) = oneshot::channel();
        let config =
            ServiceConfig::new(1, output.clone(), OfficialPdfOptions::default(), None, None)
                .unwrap()
                .public_policy(true, true)
                .test_limits(pdf_limits(), 0, RETENTION, CLEANUP_INTERVAL);
        let server = tokio::spawn(serve(listener, config, async move {
            let _ = stopped.await;
        }));
        let response = raw(client_addr, "/tasks", "bad/path").await;
        assert!(response.starts_with("HTTP/1.1 400"));
        assert!(response.contains("invalid request authority"));
        assert_eq!(std::fs::read_dir(&output).unwrap().count(), 0);
        stop.send(()).unwrap();
        assert!(server.await.unwrap().is_ok());

        let root = tempfile::tempdir().unwrap();
        let output = root.path().join("tasks");
        let listener = tokio::net::TcpListener::bind("0.0.0.0:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (stop, stopped) = oneshot::channel();
        let config =
            ServiceConfig::new(1, output.clone(), OfficialPdfOptions::default(), None, None)
                .unwrap()
                .public_policy(true, true)
                .test_limits(pdf_limits(), 1, RETENTION, CLEANUP_INTERVAL);
        let server = tokio::spawn(serve(listener, config, async move {
            let _ = stopped.await;
        }));
        let service = TestService {
            base: format!("http://127.0.0.1:{}", addr.port()),
            output,
            root,
            stop,
            task: server,
        };
        assert_eq!(
            post(&service, form(canonical_fields(), vec![]))
                .await
                .status(),
            StatusCode::UNPROCESSABLE_ENTITY
        );
        assert_eq!(
            file_post(&service, form(canonical_fields(), vec![]))
                .await
                .status(),
            StatusCode::UNPROCESSABLE_ENTITY
        );
        let no_proxy = reqwest::Client::builder().no_proxy().build().unwrap();
        let invalid_file = no_proxy
            .post(format!("{}/file_parse", service.base))
            .header(header::HOST, "bad/path")
            .multipart(form(canonical_fields(), vec![]))
            .send()
            .await
            .unwrap();
        assert_eq!(invalid_file.status(), StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(
            invalid_file.json::<serde_json::Value>().await.unwrap()["detail"],
            "exactly one document is required"
        );
        let health: serde_json::Value = reqwest::get(format!("{}/health", service.base))
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(health["task_count"], 0);

        let accepted: serde_json::Value = no_proxy
            .post(format!("{}/tasks", service.base))
            .header(header::HOST, "Original.Example:8123")
            .header("forwarded", "host=other.test")
            .header("x-forwarded-host", "other.test")
            .multipart(canonical_form())
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let id = accepted["task_id"].as_str().unwrap();
        let status_url = format!("http://Original.Example:8123/tasks/{id}");
        let result_url = format!("{status_url}/result");
        assert_eq!(accepted["status_url"], status_url);
        assert_eq!(accepted["result_url"], result_url);
        let fetched: serde_json::Value = no_proxy
            .get(format!("{}/tasks/{id}", service.base))
            .header(header::HOST, "Different.Example:9000")
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(fetched["status_url"], status_url);
        assert_eq!(fetched["result_url"], result_url);
        service.stop().await;
    }

    fn package_fixture() -> (TempDir, String, PathBuf) {
        let root = tempfile::tempdir().unwrap();
        let stem = "Report".to_owned();
        let vlm = root.path().join(&stem).join("vlm");
        std::fs::create_dir_all(vlm.join("images")).unwrap();
        for (name, bytes) in [
            ("Report.md", b"<md>&\n".as_slice()),
            ("Report_middle.json", b"{\"middle\":1}".as_slice()),
            ("Report_model.json", b"{\"model\":1}".as_slice()),
            ("Report_content_list.json", b"[1]".as_slice()),
            ("Report_content_list_v2.json", b"[2]".as_slice()),
            ("layout.pdf", b"no".as_slice()),
        ] {
            std::fs::write(vlm.join(name), bytes).unwrap();
        }
        std::fs::write(vlm.join("images/z.png"), b"z").unwrap();
        std::fs::write(vlm.join("images/a.jpg"), b"a").unwrap();
        let compact = root.path().join("selected.pdf");
        std::fs::write(&compact, b"pdf").unwrap();
        (root, stem, compact)
    }
    fn selected(zip: bool) -> Selectors {
        Selectors {
            md: true,
            middle: true,
            model: true,
            content: true,
            images: true,
            origin: false,
            zip,
        }
    }

    #[test]
    fn packaging_zip_is_exact_sorted_and_origin_is_opt_in() {
        let (root, stem, compact) = package_fixture();
        let output = root.path().join("result.zip");
        zip_result(
            root.path(),
            &stem,
            DocumentKind::Pdf,
            &compact,
            &output,
            &selected(true),
            &OfficialPdfOptions::default(),
            far_deadline(),
        )
        .unwrap();
        let mut archive = zip::ZipArchive::new(std::fs::File::open(&output).unwrap()).unwrap();
        let names: Vec<_> = (0..archive.len())
            .map(|i| archive.by_index(i).unwrap().name().to_owned())
            .collect();
        assert_eq!(
            names,
            vec![
                "Report/vlm/Report.md",
                "Report/vlm/Report_middle.json",
                "Report/vlm/Report_model.json",
                "Report/vlm/Report_content_list.json",
                "Report/vlm/Report_content_list_v2.json",
                "Report/vlm/images/a.jpg",
                "Report/vlm/images/z.png"
            ]
        );
        let mut with_origin = selected(true);
        with_origin.origin = true;
        zip_result(
            root.path(),
            &stem,
            DocumentKind::Pdf,
            &compact,
            &output,
            &with_origin,
            &OfficialPdfOptions::default(),
            far_deadline(),
        )
        .unwrap();
        assert!(
            zip::ZipArchive::new(std::fs::File::open(output).unwrap())
                .unwrap()
                .by_name("Report/vlm/Report_origin.pdf")
                .is_ok()
        );
    }

    #[test]
    fn packaging_targets_and_origins_follow_document_kind() {
        for (kind, origin_name, target) in [
            (DocumentKind::Pdf, "selected.pdf", "vlm"),
            (DocumentKind::Jpeg, "upload.jpeg", "vlm"),
            (DocumentKind::Docx, "upload.docx", "office"),
        ] {
            let (root, stem, compact) = package_fixture();
            if target == "office" {
                std::fs::rename(
                    root.path().join(&stem).join("vlm"),
                    root.path().join(&stem).join(target),
                )
                .unwrap();
            }
            let origin = if kind == DocumentKind::Pdf {
                compact
            } else {
                root.path().join(origin_name)
            };
            std::fs::write(&origin, b"origin").unwrap();
            let output = root.path().join("result.zip");
            let selectors = Selectors {
                origin: true,
                ..selected(true)
            };
            zip_result(
                root.path(),
                &stem,
                kind,
                &origin,
                &output,
                &selectors,
                &OfficialPdfOptions::default(),
                far_deadline(),
            )
            .unwrap();
            let mut zip = zip::ZipArchive::new(std::fs::File::open(output).unwrap()).unwrap();
            assert!(
                zip.by_name(&format!("{stem}/{target}/{stem}_origin.{}", kind.suffix()))
                    .is_ok()
            );
            if target == "office" {
                assert!((0..zip.len()).all(|i| !zip.by_index(i).unwrap().name().contains("/vlm/")));
            }
        }
    }

    #[test]
    fn packaging_reads_the_exact_nested_origin_path() {
        let (root, stem, _) = package_fixture();
        let nested = root.path().join("transaction/origin.pdf");
        std::fs::create_dir_all(nested.parent().unwrap()).unwrap();
        std::fs::write(&nested, b"nested").unwrap();
        std::fs::write(root.path().join("origin.pdf"), b"root collision").unwrap();
        let output = root.path().join("result.zip");
        let selectors = Selectors {
            origin: true,
            ..selected(true)
        };
        zip_result(
            root.path(),
            &stem,
            DocumentKind::Pdf,
            &nested,
            &output,
            &selectors,
            &OfficialPdfOptions::default(),
            far_deadline(),
        )
        .unwrap();
        let mut zip = zip::ZipArchive::new(std::fs::File::open(output).unwrap()).unwrap();
        let mut origin = Vec::new();
        zip.by_name("Report/vlm/Report_origin.pdf")
            .unwrap()
            .read_to_end(&mut origin)
            .unwrap();
        assert_eq!(origin, b"nested");
    }

    #[cfg(unix)]
    #[test]
    fn packaging_rejects_origin_with_symlinked_parent() {
        use std::os::unix::fs::symlink;
        let (root, stem, _) = package_fixture();
        let actual = root.path().join("actual");
        std::fs::create_dir(&actual).unwrap();
        std::fs::write(actual.join("origin.pdf"), b"origin").unwrap();
        symlink("actual", root.path().join("linked")).unwrap();
        let output = root.path().join("result.zip");
        let selectors = Selectors {
            origin: true,
            ..selected(true)
        };
        assert!(
            zip_result(
                root.path(),
                &stem,
                DocumentKind::Pdf,
                &root.path().join("linked/origin.pdf"),
                &output,
                &selectors,
                &OfficialPdfOptions::default(),
                far_deadline()
            )
            .is_err()
        );
        assert!(!output.exists() && !root.path().join("result.zip.partial").exists());
    }

    #[test]
    fn packaging_json_shape_escaping_and_exact_cap() {
        let (root, stem, _) = package_fixture();
        let output = root.path().join("result.json");
        let mut selectors = selected(false);
        selectors.origin = true;
        json_result(
            root.path(),
            &stem,
            DocumentKind::Pdf,
            &output,
            &selectors,
            &OfficialPdfOptions::default(),
            far_deadline(),
        )
        .unwrap();
        let bytes = std::fs::read(&output).unwrap();
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(value["backend"], "vlm-http-client");
        assert_eq!(value["version"], env!("CARGO_PKG_VERSION"));
        let result = &value["results"][&stem];
        assert_eq!(result["md_content"], "<md>&\n");
        assert_eq!(result["content_list"], "[1]");
        assert!(result.get("Report_content_list_v2.json").is_none());
        assert_eq!(result["images"]["a.jpg"], "data:image/jpeg;base64,YQ==");
        let raw = String::from_utf8(bytes.clone()).unwrap();
        assert!(raw.find("\"a.jpg\"").unwrap() < raw.find("\"z.png\"").unwrap());
        assert!(
            !raw.contains("origin.pdf")
                && !raw.contains("layout.pdf")
                && !raw.contains("content_list_v2")
        );
        let mut route = OfficialPdfOptions::default();
        route.max_staged_text_bytes = bytes.len();
        json_result(
            root.path(),
            &stem,
            DocumentKind::Pdf,
            &output,
            &selected(false),
            &route,
            far_deadline(),
        )
        .unwrap();
        route.max_staged_text_bytes -= 1;
        std::fs::remove_file(&output).unwrap();
        assert!(
            json_result(
                root.path(),
                &stem,
                DocumentKind::Pdf,
                &output,
                &selected(false),
                &route,
                far_deadline()
            )
            .is_err()
        );
        assert!(!output.exists() && !root.path().join("result.json.partial").exists());
    }

    #[test]
    fn packaging_all_false_json_is_empty_and_images_fail_closed() {
        let (root, stem, _) = package_fixture();
        let output = root.path().join("result.json");
        let none = Selectors {
            md: false,
            middle: false,
            model: false,
            content: false,
            images: false,
            origin: false,
            zip: false,
        };
        json_result(
            root.path(),
            &stem,
            DocumentKind::Pdf,
            &output,
            &none,
            &OfficialPdfOptions::default(),
            far_deadline(),
        )
        .unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&std::fs::read(output).unwrap()).unwrap()["results"]
                [&stem],
            json!({})
        );
        let (root, stem, _) = package_fixture();
        std::fs::remove_dir_all(root.path().join(&stem).join("vlm/images")).unwrap();
        let output = root.path().join("selected-empty.json");
        let mut selected_empty = none;
        selected_empty.images = true;
        json_result(
            root.path(),
            &stem,
            DocumentKind::Pdf,
            &output,
            &selected_empty,
            &OfficialPdfOptions::default(),
            far_deadline(),
        )
        .unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&std::fs::read(output).unwrap()).unwrap()["results"]
                [&stem],
            json!({"images":{}})
        );
        let (root, stem, _) = package_fixture();
        std::fs::create_dir(root.path().join(&stem).join("vlm/images/nested")).unwrap();
        assert!(
            package_files(root.path(), &stem, DocumentKind::Pdf, &none, far_deadline()).is_ok()
        );
        let mut images = none;
        images.images = true;
        assert!(
            package_files(
                root.path(),
                &stem,
                DocumentKind::Pdf,
                &images,
                far_deadline()
            )
            .is_err()
        );
    }

    #[cfg(unix)]
    #[test]
    fn packaging_rejects_image_symlinks_and_non_utf8_names() {
        use std::os::unix::{ffi::OsStringExt, fs::symlink, net::UnixListener};
        let (root, stem, _) = package_fixture();
        let images = root.path().join(&stem).join("vlm/images");
        symlink("a.jpg", images.join("link.png")).unwrap();
        assert!(
            package_files(
                root.path(),
                &stem,
                DocumentKind::Pdf,
                &selected(false),
                far_deadline()
            )
            .is_err()
        );

        let (root, stem, _) = package_fixture();
        let vlm = root.path().join(&stem).join("vlm");
        std::fs::rename(vlm.join("images"), vlm.join("real-images")).unwrap();
        symlink("real-images", vlm.join("images")).unwrap();
        assert!(
            package_files(
                root.path(),
                &stem,
                DocumentKind::Pdf,
                &selected(false),
                far_deadline()
            )
            .is_err()
        );

        let (root, stem, _) = package_fixture();
        let images = root.path().join(&stem).join("vlm/images");
        if std::fs::write(
            images.join(std::ffi::OsString::from_vec(b"bad-\xff.png".to_vec())),
            b"x",
        )
        .is_ok()
        {
            assert!(
                package_files(
                    root.path(),
                    &stem,
                    DocumentKind::Pdf,
                    &selected(false),
                    far_deadline()
                )
                .is_err()
            );
        } // macOS's UTF-8-normalizing volume rejects this fixture.

        let (root, stem, _) = package_fixture();
        let socket = root.path().join(&stem).join("vlm/images/socket");
        let _listener = UnixListener::bind(socket).unwrap();
        assert!(
            package_files(
                root.path(),
                &stem,
                DocumentKind::Pdf,
                &selected(false),
                far_deadline()
            )
            .is_err()
        );
    }

    #[tokio::test]
    #[ignore = "full VLM API lifecycle integration e2e"]
    async fn official_defaults_and_client_selector_mutation_are_order_independent() {
        let default = Submit::default();
        assert_eq!(default.language.as_deref(), Some("ch"));
        assert_eq!(default.backend.as_deref(), Some("hybrid-engine"));
        assert_eq!(default.effort.as_deref(), Some("medium"));
        assert_eq!(default.parse_method.as_deref(), Some("auto"));
        assert!(default.formula && default.table && default.image && default.md);
        let mut before = Submit {
            backend: Some("vlm-http-client".into()),
            client_side: true,
            md: true,
            middle: false,
            model: false,
            content: true,
            images: false,
            ..Default::default()
        };
        let mut after = Submit {
            backend: Some("vlm-http-client".into()),
            client_side: true,
            md: false,
            middle: true,
            model: true,
            content: false,
            images: true,
            ..Default::default()
        };
        apply_client_side(&mut before);
        apply_client_side(&mut after);
        let a = before.clone_selectors();
        let b = after.clone_selectors();
        assert_eq!(
            (a.md, a.middle, a.model, a.content, a.images),
            (false, true, true, false, true)
        );
        assert_eq!(
            (a.md, a.middle, a.model, a.content, a.images),
            (b.md, b.middle, b.model, b.content, b.images)
        );

        let model = mock(false, false).await;
        let service = test_service(pdf_limits(), 4).await;
        assert_eq!(
            post(
                &service,
                form(vec![], vec![("files", "input.pdf", nested_fixture())]),
            )
            .await
            .status(),
            StatusCode::BAD_REQUEST
        );

        let default_response = post(
            &service,
            form(
                vec![
                    ("backend".into(), "vlm-http-client".into()),
                    ("server_url".into(), model.url.clone()),
                ],
                vec![("files", "input.pdf", nested_fixture())],
            ),
        )
        .await;
        assert_eq!(default_response.status(), StatusCode::ACCEPTED);
        let default_task: serde_json::Value = default_response.json().await.unwrap();
        wait_status(default_task["status_url"].as_str().unwrap(), "completed").await;
        let default_result: serde_json::Value =
            reqwest::get(default_task["result_url"].as_str().unwrap())
                .await
                .unwrap()
                .json()
                .await
                .unwrap();
        let default_result = &default_result["results"]["input"];
        assert!(default_result.get("md_content").is_some());
        assert!(default_result.get("middle_json").is_none());
        assert!(default_result.get("model_output").is_none());
        assert!(default_result.get("content_list").is_none());
        assert!(default_result.get("images").is_none());

        let client_fields = |client_first| {
            let mut fields = vec![
                ("backend".into(), "vlm-http-client".into()),
                ("server_url".into(), model.url.clone()),
                ("start_page_id".into(), "0".into()),
                ("end_page_id".into(), "0".into()),
            ];
            let client = ("client_side_output_generation".into(), "true".into());
            let selectors = vec![
                ("return_md".into(), "true".into()),
                ("return_middle_json".into(), "false".into()),
                ("return_model_output".into(), "false".into()),
                ("return_content_list".into(), "true".into()),
                ("return_images".into(), "false".into()),
            ];
            if client_first {
                fields.push(client);
                fields.extend(selectors);
            } else {
                fields.extend(selectors);
                fields.push(client);
            }
            form(fields, vec![("files", "input.pdf", nested_fixture())])
        };
        let before_response = post(&service, client_fields(true)).await;
        let after_response = post(&service, client_fields(false)).await;
        assert_eq!(before_response.status(), StatusCode::ACCEPTED);
        assert_eq!(after_response.status(), StatusCode::ACCEPTED);
        let before: serde_json::Value = before_response.json().await.unwrap();
        let after: serde_json::Value = after_response.json().await.unwrap();
        let before_status = wait_status(before["status_url"].as_str().unwrap(), "completed").await;
        let after_status = wait_status(after["status_url"].as_str().unwrap(), "completed").await;
        let before: serde_json::Value = reqwest::get(before["result_url"].as_str().unwrap())
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let after: serde_json::Value = reqwest::get(after["result_url"].as_str().unwrap())
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(before_status["status"], after_status["status"]);
        let selector_effects = |result: &serde_json::Value| {
            (
                result.get("md_content").is_some(),
                result.get("content_list").is_some(),
                result.get("middle_json").is_some(),
                result.get("model_output").is_some(),
            )
        };
        assert_eq!(
            selector_effects(&before["results"]["input"]),
            selector_effects(&after["results"]["input"])
        );
        for result in [&before["results"]["input"], &after["results"]["input"]] {
            assert!(result.get("md_content").is_none());
            assert!(result.get("content_list").is_none());
            assert!(result.get("middle_json").is_some());
            assert!(result.get("model_output").is_some());
        }
        service.stop().await;
        let _ = model.stop.send(());
        model.task.await.unwrap();
    }

    #[test]
    fn zip_empty_failure_cleanup_and_capped_boundary() {
        let (root, stem, compact) = package_fixture();
        let output = root.path().join("result.zip");
        let none = Selectors {
            md: false,
            middle: false,
            model: false,
            content: false,
            images: false,
            origin: false,
            zip: true,
        };
        zip_result(
            root.path(),
            &stem,
            DocumentKind::Pdf,
            &compact,
            &output,
            &none,
            &OfficialPdfOptions::default(),
            far_deadline(),
        )
        .unwrap();
        assert_eq!(
            zip::ZipArchive::new(std::fs::File::open(&output).unwrap())
                .unwrap()
                .len(),
            0
        );
        std::fs::remove_file(&output).unwrap();
        std::fs::remove_file(root.path().join(&stem).join("vlm/Report_model.json")).unwrap();
        assert!(
            zip_result(
                root.path(),
                &stem,
                DocumentKind::Pdf,
                &compact,
                &output,
                &selected(true),
                &OfficialPdfOptions::default(),
                far_deadline()
            )
            .is_err()
        );
        assert!(!output.exists() && !root.path().join("result.zip.partial").exists());
        let path = root.path().join("cap");
        let mut exact = CappedFile::new(&path, 2, far_deadline()).unwrap();
        assert_eq!(exact.write(b"ok").unwrap(), 2);
        exact.finish().unwrap();
        let mut short = CappedFile::new(&path, 1, far_deadline()).unwrap();
        assert!(short.write(b"ok").is_err());
    }

    #[test]
    fn snapshots_are_exact_and_reject_growth_or_shrink() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("file");
        std::fs::write(&path, b"abc").unwrap();
        let dir = task_dir(root.path()).unwrap();
        let mut exact = open_regular(&dir, std::ffi::OsStr::new("file")).unwrap();
        assert_eq!(
            read_snapshot(&mut exact, 3, far_deadline()).unwrap(),
            b"abc"
        );
        let mut exact = open_regular(&dir, std::ffi::OsStr::new("file")).unwrap();
        let mut copied = Vec::new();
        copy_snapshot(&mut exact, 3, &mut copied, far_deadline()).unwrap();
        assert_eq!(copied, b"abc");
        let mut file = open_regular(&dir, std::ffi::OsStr::new("file")).unwrap();
        std::fs::write(&path, b"abcd").unwrap();
        assert!(read_snapshot(&mut file, 3, far_deadline()).is_err());
        let mut file = open_regular(&dir, std::ffi::OsStr::new("file")).unwrap();
        let mut copied = Vec::new();
        assert!(copy_snapshot(&mut file, 3, &mut copied, far_deadline()).is_err());
        std::fs::write(&path, b"abc").unwrap();
        let mut file = open_regular(&dir, std::ffi::OsStr::new("file")).unwrap();
        std::fs::write(&path, b"ab").unwrap();
        assert!(read_snapshot(&mut file, 3, far_deadline()).is_err());
        let mut file = open_regular(&dir, std::ffi::OsStr::new("file")).unwrap();
        let mut copied = Vec::new();
        assert!(copy_snapshot(&mut file, 3, &mut copied, far_deadline()).is_err());
    }

    #[tokio::test]
    async fn expired_packaging_is_atomic_and_cleans_partials() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("cap");
        let deadline = Instant::now() + Duration::from_millis(25);
        let mut capped = CappedFile::new(&path, 8, deadline).unwrap();
        capped.write_all(b"ok").unwrap();
        std::thread::sleep(Duration::from_millis(60));
        assert_eq!(
            capped.write(b"x").unwrap_err().kind(),
            std::io::ErrorKind::TimedOut
        );

        let (root, stem, origin) = package_fixture();
        let zip = root.path().join("result.zip");
        let json = root.path().join("result.json");
        std::fs::write(&zip, b"sentinel").unwrap();
        let expired = Instant::now();
        assert!(
            zip_result(
                root.path(),
                &stem,
                DocumentKind::Pdf,
                &origin,
                &zip,
                &selected(true),
                &OfficialPdfOptions::default(),
                expired
            )
            .is_err()
        );
        assert_eq!(std::fs::read(&zip).unwrap(), b"sentinel");
        assert!(!root.path().join("result.zip.partial").exists());
        assert!(
            json_result(
                root.path(),
                &stem,
                DocumentKind::Pdf,
                &json,
                &selected(false),
                &OfficialPdfOptions::default(),
                expired
            )
            .is_err()
        );
        assert!(!json.exists() && !root.path().join("result.json.partial").exists());

        let slots = Arc::new(Semaphore::new(1));
        let record = terminal(slots, Duration::ZERO).await;
        *record_state(&record) = TaskState::Processing {
            started_at: timestamp(),
        };
        std::fs::write(record.input.root.path().join("result.zip.partial"), b"x").unwrap();
        std::fs::create_dir(record.input.root.path().join("out")).unwrap();
        fail_record(&record, "expired packaging").await;
        assert!(matches!(state_snapshot(&record), TaskState::Failed { .. }));
        assert!(!record.input.root.path().join("result.zip").exists());
        assert!(!record.input.root.path().join("result.zip.partial").exists());
        assert!(!record.input.root.path().join("out").exists());
    }
}
