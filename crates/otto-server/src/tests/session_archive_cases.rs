//! `POST /v1/sessions/{id}/archive`: closes an idle session and moves its file
//! to the workspace archive through the native API.
use super::*;

async fn archive(harness: &Harness, id: &str) -> Reply {
    harness
        .send("POST", &format!("/v1/sessions/{id}/archive"), None)
        .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn archive_moves_the_file_and_the_session_leaves_get_list_and_resume() {
    let harness = Harness::with(HarnessOptions {
        persistent: true,
        ..HarnessOptions::default()
    });
    let id = harness.create().await;
    let stale = harness.server.lookup(&id).expect("open");
    let path = PathBuf::from(stale.ctrl.info().session_path);
    assert!(path.exists());
    let reply = archive(&harness, &id).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    let archived = PathBuf::from(
        reply.json()["archived_path"]
            .as_str()
            .expect("archived_path"),
    );
    assert!(!path.exists());
    assert!(archived.exists());
    assert_eq!(
        archived.parent().and_then(|dir| dir.file_name()).unwrap(),
        "archive"
    );
    assert_eq!(archived.file_name(), path.file_name());
    let get = harness
        .send("GET", &format!("/v1/sessions/{id}"), None)
        .await;
    assert_eq!(get.status, StatusCode::NOT_FOUND, "{}", get.body);
    let listed = harness.send("GET", "/v1/sessions", None).await;
    assert!(!listed.body.contains(&id), "{}", listed.body);
    let resumed = harness
        .send(
            "POST",
            "/v1/sessions",
            Some(&serde_json::json!({"resume": id}).to_string()),
        )
        .await;
    assert_eq!(resumed.status, StatusCode::NOT_FOUND, "{}", resumed.body);
    assert!(stale.ctrl.begin_operation().is_err());
}

#[tokio::test]
async fn archive_unknown_session_is_404_and_busy_session_is_409_without_closing() {
    let harness = Harness::with(HarnessOptions {
        persistent: true,
        ..HarnessOptions::default()
    });
    assert_eq!(
        archive(&harness, "missing").await.status,
        StatusCode::NOT_FOUND
    );
    let id = harness.create().await;
    let session = harness.server.lookup(&id).expect("session");
    let admission = session.ctrl.begin_operation().expect("native work");
    assert_eq!(archive(&harness, &id).await.status, StatusCode::CONFLICT);
    drop(admission);
    assert!(harness.server.pin_session(&id));
    assert_eq!(archive(&harness, &id).await.status, StatusCode::CONFLICT);
    harness.server.unpin_session(&id);
    assert!(!session.closed.is_cancelled());
    assert!(PathBuf::from(session.ctrl.info().session_path).exists());
    assert_eq!(archive(&harness, &id).await.status, StatusCode::OK);
}

#[tokio::test]
async fn failed_archive_leaves_the_session_on_disk_and_resumable() {
    let harness = Harness::with(HarnessOptions {
        persistent: true,
        ..HarnessOptions::default()
    });
    let id = harness.create().await;
    let path = PathBuf::from(harness.server.lookup(&id).unwrap().ctrl.info().session_path);
    // An existing destination makes the native archive refuse before moving.
    let archive_dir = path.parent().unwrap().join("archive");
    std::fs::create_dir_all(&archive_dir).unwrap();
    std::fs::write(archive_dir.join(path.file_name().unwrap()), "occupied").unwrap();
    let reply = archive(&harness, &id).await;
    assert_eq!(
        reply.status,
        StatusCode::INTERNAL_SERVER_ERROR,
        "{}",
        reply.body
    );
    assert!(path.exists());
    assert!(harness.server.resume_or_create(&id, None).await.is_ok());
}

#[tokio::test]
async fn archive_blocks_idempotent_create_retry() {
    let harness = Harness::with(HarnessOptions {
        persistent: true,
        ..HarnessOptions::default()
    });
    let body = r#"{"request_id":"archive-retry"}"#;
    let created = harness.send("POST", "/v1/sessions", Some(body)).await;
    assert_eq!(created.status, StatusCode::CREATED, "{}", created.body);
    let id = created.json()["id"].as_str().unwrap().to_owned();
    assert_eq!(archive(&harness, &id).await.status, StatusCode::OK);
    let retried = harness.send("POST", "/v1/sessions", Some(body)).await;
    assert_eq!(retried.status, StatusCode::NOT_FOUND, "{}", retried.body);
}
