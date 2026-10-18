use super::*;
use std::{sync::Arc, time::Duration};
use tokio::sync::Semaphore;

fn store() -> OfficialFileStore {
    OfficialFileStore::new(
        Arc::new(Semaphore::new(2)),
        Arc::new(Semaphore::new(1)),
        Duration::from_secs(3600),
    )
    .unwrap()
}

#[tokio::test]
async fn get_returns_not_found_if_completed_file_was_deleted() {
    let store = store();
    let req = UploadCreate {
        filename: "input.pdf".into(),
        bytes: 3,
        mime_type: "application/pdf".into(),
        purpose: "parse".into(),
        sha256sum: None,
        expires_after: None,
    };
    let _ = commit_upload(
        &store,
        "upload_test".into(),
        req,
        OfficialFileStore::now(),
        3600,
        store.reserve_metadata(2).unwrap(),
        store.input_reservation(3).unwrap(),
    )
    .unwrap();
    put(
        State(store.clone()),
        Path("upload_test".into()),
        HeaderMap::new(),
        Body::from("pdf"),
    )
    .await
    .unwrap();

    let completed = complete(State(store.clone()), Path("upload_test".into()), Ok(None))
        .await
        .unwrap()
        .0;
    assert_eq!(completed.status, "completed");
    assert_eq!(
        complete(State(store.clone()), Path("upload_test".into()), Ok(None))
            .await
            .unwrap()
            .0
            .file
            .unwrap()
            .id,
        completed.file.as_ref().unwrap().id
    );
    let _ = super::super::files::delete(State(store.clone()), Path(completed.file.unwrap().id))
        .await
        .unwrap();

    let error = get(State(store), Path("upload_test".into()))
        .await
        .unwrap_err();
    assert_eq!(error.status, StatusCode::NOT_FOUND);
}
