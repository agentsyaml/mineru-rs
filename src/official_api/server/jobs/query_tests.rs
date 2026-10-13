use super::*;
use std::{future::Future, pin::Pin, sync::Arc, task::Poll};
use tokio::sync::Semaphore;

#[tokio::test]
async fn listing_waits_for_locked_jobs_without_holding_registry_lock() {
    let service = service();
    let first = create_job(&service).await;
    let cursor = create_job(&service).await;
    let last = create_job(&service).await;
    for id in [&first, &cursor, &last] {
        set_created_at(&service, id, "2025-01-01T00:00:00Z").await;
    }
    let held = service.inner.jobs.lock().unwrap()[&cursor].clone();
    let held = held.lock().await;
    let (started_tx, started_rx) = tokio::sync::oneshot::channel();
    let state = service.inner.clone();
    let after = cursor.clone();
    let mut listing = tokio::spawn(async move {
        let _ = started_tx.send(());
        list(State(state), list_query(Some(after), ListOrder::Asc)).await
    });
    started_rx.await.unwrap();
    let pending =
        std::future::poll_fn(|cx| Poll::Ready(Pin::new(&mut listing).poll(cx).is_pending())).await;
    assert!(pending, "listing should wait for the held job record");
    assert!(service.inner.jobs.try_lock().is_ok());
    drop(held);
    let page = listing.await.unwrap().unwrap().0;
    assert_eq!(page.data.len(), 1);
    assert_eq!(page.data[0].job_id, last);
    assert!(!page.has_more);

    let all = list(
        State(service.inner.clone()),
        list_query(None, ListOrder::Asc),
    )
    .await
    .unwrap()
    .0;
    assert_eq!(
        all.data
            .iter()
            .map(|entry| entry.job_id.as_str())
            .collect::<Vec<_>>(),
        [first.as_str(), cursor.as_str(), last.as_str()]
    );
    let descending = list(
        State(service.inner.clone()),
        list_query(None, ListOrder::Desc),
    )
    .await
    .unwrap()
    .0;
    assert_eq!(
        descending
            .data
            .iter()
            .map(|entry| entry.job_id.as_str())
            .collect::<Vec<_>>(),
        [last.as_str(), cursor.as_str(), first.as_str()]
    );
}

fn service() -> crate::official_api::server::OfficialService {
    let store = OfficialFileStore::new(
        Arc::new(Semaphore::new(3)),
        Arc::new(Semaphore::new(1)),
        std::time::Duration::from_secs(3600),
    )
    .unwrap();
    crate::official_api::server::OfficialService::new(
        crate::official::OfficialRuntime::new(),
        store,
    )
    .unwrap()
}

fn list_query(after: Option<String>, order: ListOrder) -> Result<Query<JobQuery>, QueryRejection> {
    Ok(Query(JobQuery {
        status: Some("queued".into()),
        limit: Some(10),
        after,
        order: Some(order),
        created_after: None,
    }))
}

async fn create_job(service: &crate::official_api::server::OfficialService) -> String {
    let request = serde_json::from_value(serde_json::json!({"files":[{
        "source":{"type":"inline","name":"doc.pdf","data":"YQ=="}
    }]}))
    .unwrap();
    let (_, response) = super::super::create(State(service.inner.clone()), Ok(Json(request)))
        .await
        .unwrap();
    response.job_id.clone()
}

async fn set_created_at(
    service: &crate::official_api::server::OfficialService,
    id: &str,
    created_at: &str,
) {
    let job = service.inner.jobs.lock().unwrap()[id].clone();
    job.lock().await.response.created_at = created_at.into();
}
