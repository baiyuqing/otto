//! Permanent deletion contract and stale-handle admission regression tests.
use super::*;

#[tokio::test]
async fn permanent_delete_close_keeps_history_then_permanent_removes_closed_session() {
    let harness = Harness::with(HarnessOptions {
        persistent: true,
        ..HarnessOptions::default()
    });
    let id = harness.create().await;
    let path = PathBuf::from(harness.server.lookup(&id).unwrap().ctrl.info().session_path);
    assert_eq!(
        harness
            .send("DELETE", &format!("/v1/sessions/{id}"), None)
            .await
            .status,
        StatusCode::NO_CONTENT
    );
    assert!(path.exists(), "legacy DELETE only closes");
    assert_eq!(erase(&harness, &id).await, StatusCode::NO_CONTENT);
    assert!(!path.exists());
}

#[tokio::test]
async fn permanent_delete_maps_native_busy_and_io_errors() {
    for (error, status) in [
        (
            "session is held by host fixture pid 123 (lease epoch 1)",
            StatusCode::CONFLICT,
        ),
        (
            "file is already locked by another process",
            StatusCode::CONFLICT,
        ),
        (
            "permanent delete journal: input/output error",
            StatusCode::INTERNAL_SERVER_ERROR,
        ),
    ] {
        let harness = Harness::with(HarnessOptions {
            open_error: Some(error.into()),
            ..HarnessOptions::default()
        });
        assert_eq!(erase(&harness, "missing").await, status, "{error}");
    }
}

#[tokio::test]
async fn permanent_delete_cleanup_failure_is_500_and_resume_stays_blocked() {
    use std::os::unix::fs::PermissionsExt;
    let harness = Harness::with(HarnessOptions {
        persistent: true,
        ..HarnessOptions::default()
    });
    let id = harness.create().await;
    let path = PathBuf::from(harness.server.lookup(&id).unwrap().ctrl.info().session_path);
    let descendants = path.with_extension("");
    std::fs::create_dir_all(&descendants).unwrap();
    std::fs::write(descendants.join("child"), "protected").unwrap();
    std::fs::set_permissions(&descendants, std::fs::Permissions::from_mode(0o000)).unwrap();
    let result = erase(&harness, &id).await;
    std::fs::set_permissions(&descendants, std::fs::Permissions::from_mode(0o700)).unwrap();
    assert_eq!(result, StatusCode::INTERNAL_SERVER_ERROR);
    assert!(path.exists());
    assert!(harness.server.resume_or_create(&id, None).await.is_err());
}

#[tokio::test]
async fn permanent_delete_blocks_idempotent_create_retry() {
    let harness = Harness::with(HarnessOptions {
        persistent: true,
        ..HarnessOptions::default()
    });
    let body = r#"{"request_id":"delete-retry"}"#;
    let created = harness.send("POST", "/v1/sessions", Some(body)).await;
    assert_eq!(created.status, StatusCode::CREATED, "{}", created.body);
    let id = created.json()["id"].as_str().unwrap().to_owned();
    assert_eq!(erase(&harness, &id).await, StatusCode::NO_CONTENT);
    assert_eq!(
        harness
            .send("POST", "/v1/sessions", Some(body))
            .await
            .status,
        StatusCode::NOT_FOUND
    );
}

async fn erase(harness: &Harness, id: &str) -> StatusCode {
    harness
        .send(
            "DELETE",
            &format!("/v1/sessions/{id}/permanent"),
            Some(&serde_json::json!({"confirm": id}).to_string()),
        )
        .await
        .status
}

#[tokio::test]
async fn permanent_delete_validates_confirmation_and_unknown_id() {
    let harness = Harness::with(HarnessOptions {
        persistent: true,
        ..HarnessOptions::default()
    });
    for body in [
        "",
        "{}",
        "null",
        "{\"confirm\":1}",
        "{\"confirm\":\"missing \"}",
        "{\"confirm\":\"missing\",\"extra\":true}",
    ] {
        let reply = harness
            .send("DELETE", "/v1/sessions/missing/permanent", Some(body))
            .await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{body}");
    }
    assert_eq!(erase(&harness, "missing").await, StatusCode::NOT_FOUND);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn permanent_delete_removes_file_and_rejects_resume_and_stale_turn() {
    let harness = Harness::with(HarnessOptions {
        persistent: true,
        ..HarnessOptions::default()
    });
    let id = harness.create().await;
    let stale = harness.server.lookup(&id).expect("open");
    let path = PathBuf::from(stale.ctrl.info().session_path);
    assert!(path.exists());
    let descendants = path.with_extension("");
    std::fs::create_dir_all(&descendants).unwrap();
    std::fs::write(descendants.join("child.jsonl"), "child").unwrap();
    for suffix in [
        "reminders.json",
        "inbox.json",
        "inputs.json",
        "followups.json",
        "turns.json",
    ] {
        std::fs::write(path.with_extension(suffix), "[]").unwrap();
    }
    assert_eq!(erase(&harness, &id).await, StatusCode::NO_CONTENT);
    assert!(!descendants.exists());
    for suffix in [
        "reminders.json",
        "inbox.json",
        "inputs.json",
        "followups.json",
        "turns.json",
    ] {
        assert!(!path.with_extension(suffix).exists());
    }
    assert!(!path.exists());
    assert!(!path.with_extension("lease").exists());
    assert!(harness.server.resume_or_create(&id, None).await.is_err());
    assert!(
        harness
            .server
            .start_turn(&stale, "stale".into(), None, (None, None), false, "", None)
            .is_err()
    );
    assert!(stale.ctrl.begin_operation().is_err());
    let response = harness
        .send(
            "POST",
            "/v1/sessions",
            Some(&serde_json::json!({"resume": id}).to_string()),
        )
        .await;
    assert_eq!(response.status, StatusCode::NOT_FOUND, "{}", response.body);
}

#[tokio::test]
async fn permanent_delete_rejects_native_admission_pin_and_compaction_without_closing() {
    let harness = Harness::with(HarnessOptions {
        persistent: true,
        ..HarnessOptions::default()
    });
    let id = harness.create().await;
    let session = harness.server.lookup(&id).expect("session");
    let admission = session.ctrl.begin_operation().expect("native work");
    assert_eq!(erase(&harness, &id).await, StatusCode::CONFLICT);
    drop(admission);
    assert!(harness.server.pin_session(&id));
    assert_eq!(erase(&harness, &id).await, StatusCode::CONFLICT);
    harness.server.unpin_session(&id);
    let compaction = session
        .lock()
        .begin_compaction(CancellationToken::new())
        .expect("idle session admits compaction");
    assert_eq!(erase(&harness, &id).await, StatusCode::CONFLICT);
    assert!(session.lock().finish_compaction(compaction));
    assert!(session.ctrl.begin_operation().is_ok());
    assert!(PathBuf::from(session.ctrl.info().session_path).exists());
    assert_eq!(erase(&harness, &id).await, StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn permanent_delete_refuses_waiting_approval_and_saved_queue() {
    let harness = Harness::with(HarnessOptions {
        persistent: true,
        ..HarnessOptions::default()
    });
    let id = harness.create().await;
    let session = harness.server.lookup(&id).unwrap();
    let (decide, _receiver) = tokio::sync::oneshot::channel();
    session.lock().waiting = Some(WaitingApproval {
        id: "waiting".into(),
        shared_revision: None,
        decide,
    });
    assert_eq!(erase(&harness, &id).await, StatusCode::CONFLICT);
    session.lock().waiting = None;
    let journal = session.ctrl.turn_queue().unwrap();
    journal
        .save(&app::turn_queue::QueuedPrompt {
            input_provenance: None,
            input_id: "saved".into(),
            turn_id: "saved-turn".into(),
            text: "not delivered".into(),
            channel: None,
            origin: None,
            image_dropped: false,
        })
        .unwrap();
    assert_eq!(erase(&harness, &id).await, StatusCode::CONFLICT);
    assert!(!session.closed.is_cancelled());
    assert_eq!(journal.pending().len(), 1);
    journal.clear();
    assert_eq!(erase(&harness, &id).await, StatusCode::NO_CONTENT);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn permanent_delete_refuses_running_and_queued_turn_without_cancelling() {
    let (gate, options) = gated();
    let harness = Harness::with(HarnessOptions {
        persistent: true,
        ..options
    });
    let id = harness.create().await;
    assert_eq!(
        post_turn(&harness, &id, "running", false).await.status(),
        StatusCode::OK
    );
    harness.provider.wait_started(1).await;
    assert_eq!(
        post_turn(&harness, &id, "queued", true).await.status(),
        StatusCode::OK
    );
    assert_eq!(erase(&harness, &id).await, StatusCode::CONFLICT);
    let session = harness.server.lookup(&id).expect("still open");
    assert!(!session.closed.is_cancelled());
    assert!(session.lock().turn_running());
    gate.cancel();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn permanent_delete_races_new_turn_and_resume_fail_closed() {
    let (gate, options) = gated();
    let harness = Harness::with(HarnessOptions {
        persistent: true,
        ..options
    });
    let id = harness.create().await;
    let stale = harness.server.lookup(&id).expect("session");
    let (deleted, started, reopened) = tokio::join!(
        erase(&harness, &id),
        async {
            harness
                .server
                .start_turn(&stale, "racing".into(), None, (None, None), false, "", None)
        },
        harness.server.resume_or_create(&id, None),
    );
    if deleted == StatusCode::NO_CONTENT {
        assert!(started.is_err());
        assert!(harness.server.resume_or_create(&id, None).await.is_err());
        if let Ok((old, _)) = reopened {
            assert!(old.ctrl.begin_operation().is_err());
        }
    } else {
        assert_eq!(deleted, StatusCode::CONFLICT);
        assert!(started.is_ok());
        assert!(PathBuf::from(stale.ctrl.info().session_path).exists());
        assert!(!stale.closed.is_cancelled());
    }
    gate.cancel();
}
