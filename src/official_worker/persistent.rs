mod child;
mod protocol;
#[cfg(test)]
mod tests;

use std::{
    io::Write,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex as StdMutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Instant,
};

use tempfile::TempDir;
use tokio::{
    sync::{OwnedSemaphorePermit, Semaphore, mpsc, oneshot, watch},
    task::JoinHandle,
};

use super::{
    OfficialBundle, OfficialRequest, PACKAGE_VERSION, PERSISTENT_BACKEND, PERSISTENT_MODEL_STACKS,
    SCHEMA_VERSION,
};
use child::{PersistentChild, persistent_error_text, persistent_stderr_text};
use protocol::validate_persistent_request;

#[allow(dead_code)]
#[derive(Clone, Debug)]
pub(crate) struct OfficialSessionConfig {
    backend: &'static str,
    package_version: &'static str,
    schema_version: &'static str,
    model_stack: String,
    model_base_dir: Option<PathBuf>,
    config: Option<PathBuf>,
    vl_api_key: Option<String>,
    vl_model_name: Option<String>,
}

#[allow(dead_code)]
impl OfficialSessionConfig {
    pub(crate) fn new(
        model_stack: String,
        model_base_dir: Option<PathBuf>,
        config: Option<PathBuf>,
        vl_api_key: Option<String>,
        vl_model_name: Option<String>,
    ) -> Result<Self, String> {
        if !PERSISTENT_MODEL_STACKS.contains(&model_stack.as_str()) {
            return Err("official persistent model stack is unsupported".into());
        }
        Ok(Self {
            backend: PERSISTENT_BACKEND,
            package_version: PACKAGE_VERSION,
            schema_version: SCHEMA_VERSION,
            model_stack,
            model_base_dir,
            config,
            vl_api_key,
            vl_model_name,
        })
    }

    fn matches_request(&self, request: &OfficialRequest) -> Result<(), String> {
        if request.backend != self.backend
            || request.model_stack != self.model_stack
            || request.model_base_dir != self.model_base_dir
            || request.config != self.config
            || request.vl_api_key != self.vl_api_key
            || request.vl_model_name != self.vl_model_name
        {
            return Err("official persistent request does not match its session config".into());
        }
        Ok(())
    }
}

#[allow(dead_code)]
pub(crate) struct OfficialPersistentWorker {
    executable: PathBuf,
    config: OfficialSessionConfig,
    admission: Arc<Semaphore>,
    commands: mpsc::Sender<PersistentCommand>,
    shutdown: watch::Sender<bool>,
    closed: AtomicBool,
    receiver: Arc<StdMutex<Option<mpsc::Receiver<PersistentCommand>>>>,
    owner: Arc<StdMutex<Option<JoinHandle<()>>>>,
}

#[allow(dead_code)]
struct PersistentCommand {
    request: OfficialRequest,
    temporary: TempDir,
    permit: OwnedSemaphorePermit,
    deadline: Instant,
    cancel: oneshot::Receiver<()>,
    response: oneshot::Sender<Result<OfficialBundle, String>>,
}

#[allow(dead_code)]
impl OfficialPersistentWorker {
    pub(crate) fn new(
        executable: Option<PathBuf>,
        config: OfficialSessionConfig,
    ) -> Result<Self, String> {
        let executable = super::process::official_executable(executable)?;
        let (commands, receiver) = mpsc::channel(1);
        let (shutdown, _) = watch::channel(false);
        let owner = Arc::new(StdMutex::new(None));
        let worker = Self {
            executable,
            config,
            admission: Arc::new(Semaphore::new(1)),
            commands,
            shutdown,
            closed: AtomicBool::new(false),
            receiver: Arc::new(StdMutex::new(Some(receiver))),
            owner,
        };
        Ok(worker)
    }

    fn spawn_owner(&self, shutdown: watch::Receiver<bool>) -> Result<(), String> {
        let mut owner = self
            .owner
            .lock()
            .map_err(|_| "official persistent owner lock poisoned".to_owned())?;
        if owner.is_some() {
            return Ok(());
        }
        if self.closed.load(Ordering::Acquire) {
            return Err("official persistent worker is shut down".into());
        }
        let receiver = self
            .receiver
            .lock()
            .map_err(|_| "official persistent receiver lock poisoned".to_owned())?
            .take()
            .ok_or_else(|| "official persistent owner receiver is unavailable".to_owned())?;
        let executable = self.executable.clone();
        let config = self.config.clone();
        *owner = Some(tokio::spawn(async move {
            persistent_owner_loop(receiver, executable, config, shutdown).await;
        }));
        Ok(())
    }

    pub(crate) async fn run(
        &self,
        input: &[u8],
        suffix: &str,
        mut request: OfficialRequest,
        deadline: Instant,
    ) -> Result<OfficialBundle, String> {
        if self.closed.load(Ordering::Acquire) {
            return Err("official persistent worker is shut down".into());
        }
        if input.is_empty() {
            return Err("official Hybrid input is empty".into());
        }
        self.config.matches_request(&request)?;
        validate_persistent_request(&request)?;

        let permit = acquire_persistent_permit(&self.admission, &self.shutdown, deadline).await?;
        if self.closed.load(Ordering::Acquire) {
            drop(permit);
            return Err("official persistent worker is shut down".into());
        }

        let temporary = tempfile::tempdir().map_err(|e| format!("official worker tempdir: {e}"))?;
        let input_path = temporary.path().join(format!("input.{suffix}"));
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&input_path)
            .map_err(|e| format!("official worker input snapshot: {e}"))?;
        file.write_all(input)
            .and_then(|_| file.flush())
            .map_err(|e| format!("official worker input snapshot: {e}"))?;
        let bundle = temporary.path().join(crate::hybrid_v4_output::BUNDLE_NAME);
        request.input_path = input_path;
        request.bundle_path = bundle;
        if Instant::now() >= deadline {
            drop(permit);
            return Err("official persistent worker deadline expired".into());
        }

        self.spawn_owner(self.shutdown.subscribe())?;

        let (response, response_receiver) = oneshot::channel();
        let (cancel, cancel_receiver) = oneshot::channel();
        let command = PersistentCommand {
            request,
            temporary,
            permit,
            deadline,
            cancel: cancel_receiver,
            response,
        };
        let send = self.commands.send(command);
        tokio::pin!(send);
        let sleep = tokio::time::sleep_until(tokio::time::Instant::from_std(deadline));
        tokio::pin!(sleep);
        tokio::select! {
            result = &mut send => {
                result.map_err(|_| "official persistent owner task is unavailable".to_owned())?;
            }
            _ = &mut sleep => {
                return Err("official persistent worker deadline expired".into());
            }
        }

        match tokio::time::timeout_at(tokio::time::Instant::from_std(deadline), response_receiver)
            .await
        {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err("official persistent owner task stopped".into()),
            Err(_) => {
                let _ = cancel.send(());
                Err("official persistent worker deadline expired".into())
            }
        }
    }

    pub(crate) async fn drain(&self) -> Result<(), String> {
        self.shutdown().await
    }

    pub(crate) async fn shutdown(&self) -> Result<(), String> {
        self.closed.store(true, Ordering::Release);
        let _ = self.shutdown.send(true);
        let handle = self
            .owner
            .lock()
            .map_err(|_| "official persistent owner lock poisoned".to_owned())?
            .take();
        if let Some(handle) = handle {
            handle
                .await
                .map_err(|error| format!("official persistent owner join failed: {error}"))?;
        }
        Ok(())
    }
}

impl Drop for OfficialPersistentWorker {
    fn drop(&mut self) {
        self.closed.store(true, Ordering::Release);
        let _ = self.shutdown.send(true);
        // Dropping the join handle detaches the owner; its shutdown watch/channel closure
        // still lets it kill and reap the child asynchronously when a runtime is alive.
        let _ = self.owner.lock().ok().and_then(|mut owner| owner.take());
    }
}

#[allow(dead_code)]
enum PersistentFailure {
    Dead(String),
    Document(String),
}

#[allow(dead_code)]
async fn ensure_persistent_session(
    session: &mut Option<PersistentChild>,
    executable: &Path,
    config: &OfficialSessionConfig,
    deadline: Instant,
    cancel: &mut oneshot::Receiver<()>,
    shutdown: &mut watch::Receiver<bool>,
) -> Result<(), PersistentFailure> {
    if session.is_some() {
        return Ok(());
    }
    let mut child = PersistentChild::spawn(executable).map_err(PersistentFailure::Dead)?;
    if let Err(error) = child.handshake(config, deadline, cancel, shutdown).await {
        let error = persistent_stderr_text(error, child.take_diagnostic());
        child.shutdown().await;
        return Err(PersistentFailure::Dead(error));
    }
    *session = Some(child);
    Ok(())
}

#[allow(dead_code)]
async fn execute_persistent_request(
    session: &mut Option<PersistentChild>,
    executable: &Path,
    config: &OfficialSessionConfig,
    request: &OfficialRequest,
    deadline: Instant,
    cancel: &mut oneshot::Receiver<()>,
    shutdown: &mut watch::Receiver<bool>,
) -> Result<(), PersistentFailure> {
    ensure_persistent_session(session, executable, config, deadline, cancel, shutdown).await?;
    let child = session
        .as_mut()
        .expect("persistent session was just initialized");
    let frame = match child
        .request(request, config, deadline, cancel, shutdown)
        .await
    {
        Ok(frame) => frame,
        Err(error) => {
            return Err(PersistentFailure::Dead(persistent_stderr_text(
                error,
                child.take_diagnostic(),
            )));
        }
    };
    if frame.status == "error" {
        return Err(PersistentFailure::Document(persistent_error_text(
            "official persistent document failed".into(),
            frame
                .error
                .as_deref()
                .unwrap_or("official persistent document failed"),
            frame.diagnostic.as_deref(),
            child.take_diagnostic(),
        )));
    }
    let _ = child.take_diagnostic();
    Ok(())
}

#[allow(dead_code)]
async fn persistent_owner_loop(
    mut receiver: mpsc::Receiver<PersistentCommand>,
    executable: PathBuf,
    config: OfficialSessionConfig,
    mut shutdown: watch::Receiver<bool>,
) {
    let mut session = None;
    loop {
        if *shutdown.borrow() {
            break;
        }
        let command = tokio::select! {
            changed = shutdown.changed() => {
                let _ = changed;
                break;
            }
            command = receiver.recv() => command,
        };
        let Some(command) = command else {
            break;
        };
        let PersistentCommand {
            request,
            temporary,
            permit,
            deadline,
            mut cancel,
            response,
        } = command;
        let result = execute_persistent_request(
            &mut session,
            &executable,
            &config,
            &request,
            deadline,
            &mut cancel,
            &mut shutdown,
        )
        .await;
        let result = match result {
            Ok(()) => Ok(OfficialBundle {
                bundle: request.bundle_path.clone(),
                _temporary: temporary,
                _permit: Some(permit),
            }),
            Err(PersistentFailure::Document(error)) => Err(error),
            Err(PersistentFailure::Dead(error)) => {
                if let Some(child) = session.take() {
                    child.shutdown().await;
                }
                Err(error)
            }
        };
        let _ = response.send(result);
    }
    if let Some(child) = session.take() {
        child.shutdown().await;
    }
}

#[allow(dead_code)]
async fn acquire_persistent_permit(
    admission: &Arc<Semaphore>,
    shutdown: &watch::Sender<bool>,
    deadline: Instant,
) -> Result<OwnedSemaphorePermit, String> {
    let mut shutdown = shutdown.subscribe();
    if *shutdown.borrow() {
        return Err("official persistent worker is shut down".into());
    }
    let acquire = admission.clone().acquire_owned();
    tokio::pin!(acquire);
    let sleep = tokio::time::sleep_until(tokio::time::Instant::from_std(deadline));
    tokio::pin!(sleep);
    tokio::select! {
        permit = &mut acquire => permit.map_err(|_| "official persistent admission is closed".to_owned()),
        _ = shutdown.changed() => Err("official persistent worker is shut down".into()),
        _ = &mut sleep => Err("official persistent worker deadline expired".into()),
    }
}
