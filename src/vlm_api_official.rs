use crate::{
    official::OfficialRuntime,
    official_api::{OfficialFileStore, OfficialService},
};
use std::{
    io,
    sync::{Arc, atomic::AtomicU64},
    time::Duration,
};
use tokio::sync::Semaphore;

pub(crate) fn start(
    runtime: OfficialRuntime,
    slots: Arc<Semaphore>,
    gate: Arc<Semaphore>,
    ids: Arc<AtomicU64>,
    retention: Duration,
    file_cap: u64,
    body_cap: usize,
    zip_cap: u64,
) -> io::Result<OfficialService> {
    let store = OfficialFileStore::new(slots, gate, retention)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?
        .with_shared_ids(ids)
        .with_transport_limits(file_cap, body_cap, zip_cap);
    let service = OfficialService::new(runtime, store)
        .map_err(|error| io::Error::other(error.to_string()))?;
    service
        .start()
        .map_err(|error| io::Error::other(error.to_string()))?;
    Ok(service)
}
