use super::*;
use crate::official_api::wire::{ListOrder, UploadCreate};
use axum::{extract::Query, http::HeaderMap, response::IntoResponse};
use std::{sync::Arc, time::Duration};
use tokio::sync::Semaphore;

fn store(ids: &[&str]) -> OfficialFileStore {
    let store = OfficialFileStore::new(
        Arc::new(Semaphore::new(2)),
        Arc::new(Semaphore::new(1)),
        Duration::from_secs(3600),
    )
    .unwrap();
    for id in ids {
        insert_file(&store, id);
    }
    store
}

fn insert_file(store: &OfficialFileStore, id: &str) {
    store.inner.entries.lock().unwrap().insert(
        id.into(),
        Entry::File(super::super::store::File {
            object: FileObject {
                id: id.into(),
                object: "file",
                bytes: 0,
                created_at: 7,
                expires_at: None,
                filename: format!("{id}.csv"),
                purpose: "parse".into(),
                sha256sum: None,
            },
            bytes: bytes::Bytes::new(),
            source: false,
            _metadata: None,
        }),
    );
}

async fn list_ids(
    store: &OfficialFileStore,
    after: Option<&str>,
    order: ListOrder,
    limit: u16,
) -> (Vec<String>, bool) {
    let response = super::list(
        State(store.clone()),
        Ok(Query(FileQuery {
            after: after.map(str::to_owned),
            limit: Some(limit),
            order: Some(order),
            purpose: None,
        })),
    )
    .await
    .unwrap()
    .0;
    (
        response.data.into_iter().map(|file| file.id).collect(),
        response.has_more,
    )
}

#[tokio::test]
async fn tied_file_timestamps_have_insertion_independent_total_order() {
    let forward = store(&["file_c", "file_a", "file_b"]);
    let reverse = store(&["file_b", "file_a", "file_c"]);
    for (order, expected) in [
        (ListOrder::Asc, vec!["file_a", "file_b", "file_c"]),
        (ListOrder::Desc, vec!["file_c", "file_b", "file_a"]),
    ] {
        let a = list_ids(&forward, None, order, 10).await.0;
        let b = list_ids(&reverse, None, order, 10).await.0;
        assert_eq!(a.iter().map(String::as_str).collect::<Vec<_>>(), expected);
        assert_eq!(b.iter().map(String::as_str).collect::<Vec<_>>(), expected);
    }
}

#[tokio::test]
async fn tied_timestamp_cursor_survives_insert_between_pages_in_both_orders() {
    for (order, first, after, second, originals) in [
        (
            ListOrder::Asc,
            vec!["file_a", "file_b"],
            "file_b",
            vec!["file_bb", "file_c"],
            vec!["file_a", "file_b", "file_c"],
        ),
        (
            ListOrder::Desc,
            vec!["file_c", "file_b"],
            "file_b",
            vec!["file_a"],
            vec!["file_a", "file_b", "file_c"],
        ),
    ] {
        let store = store(&["file_c", "file_a", "file_b"]);
        let (page, has_more) = list_ids(&store, None, order, 2).await;
        assert_eq!(page.iter().map(String::as_str).collect::<Vec<_>>(), first);
        assert!(has_more);
        insert_file(&store, "file_bb");
        let (next, has_more) = list_ids(&store, Some(after), order, 2).await;
        assert_eq!(next.iter().map(String::as_str).collect::<Vec<_>>(), second);
        assert!(!has_more);
        let original_count = page
            .iter()
            .chain(&next)
            .filter(|id| originals.contains(&id.as_str()))
            .count();
        assert_eq!(original_count, originals.len());
    }
}

#[tokio::test]
async fn deleting_upload_id_does_not_remove_upload() {
    let store = store(&[]);
    let req = UploadCreate {
        filename: "input.pdf".into(),
        bytes: 3,
        mime_type: "application/pdf".into(),
        purpose: "parse".into(),
        sha256sum: None,
        expires_after: None,
    };
    let _ = super::super::uploads::commit_upload(
        &store,
        "upload_test".into(),
        req,
        OfficialFileStore::now(),
        3600,
        store.reserve_metadata(2).unwrap(),
        store.input_reservation(3).unwrap(),
    )
    .unwrap();

    let error = delete(State(store.clone()), Path("upload_test".into()))
        .await
        .unwrap_err();
    assert_eq!(error.status, StatusCode::NOT_FOUND);
    assert!(matches!(
        store.inner.entries.lock().unwrap().get("upload_test"),
        Some(Entry::Upload(_))
    ));
    let put = super::super::uploads::put(
        State(store.clone()),
        Path("upload_test".into()),
        HeaderMap::new(),
        Body::from("pdf"),
    )
    .await
    .unwrap();
    assert_eq!(put.into_response().status(), StatusCode::OK);
    assert_eq!(
        super::super::uploads::complete(State(store), Path("upload_test".into()), Ok(None),)
            .await
            .unwrap()
            .0
            .status,
        "completed"
    );
}
