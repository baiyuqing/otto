//! SIGTERM migration, compaction, sandbox reload and status tests.

use super::*;

// ---- SIGTERM migration ----

/// A child transcript at `<parent without .jsonl>/<task_id>-child.jsonl`,
/// matching the file `subagent::runner::spawn` creates. Duplicated from
/// [`otto::app::tests::child_store`] and
/// [`otto::runtime::builder::tests::child_store`]: each is
/// `#[cfg(test)]`-private to its own module, and the fixture is 12
/// lines.
pub(super) fn child_store(
    parent: &std::path::Path,
    task_id: &str,
) -> (otto::subagent::runner::Transcript, String) {
    let name = format!("{task_id}-child");
    let store = otto_session::Store::create_child_lazy(
        parent,
        &name,
        otto_core::session::Header {
            id: "child".into(),
            workspace: parent.parent().expect("dir").to_string_lossy().into_owned(),
            provider: "openai-compatible".into(),
            model: "test-model".into(),
            created_at: chrono::Utc::now(),
            ..otto_core::session::Header::default()
        },
    )
    .expect("child store");
    let path = otto_session::Store::child_path(parent, &name);
    (Arc::new(store), path.to_string_lossy().into_owned())
}

/// Reads a lease's fixed-size heartbeat file and reports whether its
/// `released` field is `true`. Duplicated from
/// [`otto::app::tests::heartbeat_released`] for the same reason as
/// `child_store` above.
pub(super) fn heartbeat_released(session_path: &str) -> bool {
    let heartbeat = std::path::Path::new(session_path)
        .with_extension("lease")
        .join("heartbeat");
    let bytes = std::fs::read(&heartbeat).expect("read heartbeat");
    String::from_utf8_lossy(&bytes).contains("\"released\":true")
}

/// S9(b): `Server::migrate`, which `otto serve` runs for a `SIGTERM` decided
/// as a migration, cancels every open session's work and
/// runs `Controller::migrate` on it. Asserts the running child task's
/// transcript ends `interrupted` and the session's lease heartbeat is
/// released, the same outcome `otto serve` produces by calling
/// `Server::migrate` after cancelling its token.
#[tokio::test]
async fn migrate_cancels_a_running_child_task_and_releases_the_lease() {
    use otto::fixtures::subagent_doubles::{FakeProvider, assistant_text, match_any, wait_status};
    use otto::subagent::runner::StartRequest;
    use otto::subagent::tasks::{TaskStatus, Tasks};

    let harness = Harness::new();

    let workspace = tempfile::tempdir().expect("workspace");
    let sessions = tempfile::tempdir().expect("sessions");
    let mut builder = fixtures::builder(workspace.path(), sessions.path());
    builder.shared_mut().config.failover = otto_core::config::Failover {
        enabled: true,
        lease_seconds: 12,
    };
    let runtime = fixtures::initial_runtime(&builder);
    let session = builder.create_session(&runtime).expect("create session");
    session
        .append(fixtures::user("go"))
        .await
        .expect("first append");
    let session_path = session.path();

    let tasks = Arc::new(Tasks::new());
    let child_provider = FakeProvider::new();
    child_provider.set_hook(Arc::new(|cancel, _request| {
        Box::pin(async move { cancel.cancelled().await })
    }));
    child_provider.add_route(match_any, vec![assistant_text("done", Usage::default())]);

    let mut child_config =
        otto::fixtures::subagent_doubles::test_config(&child_provider, &tasks, Vec::new());
    child_config.max_parallel = 1;
    let parent_path = PathBuf::from(&session_path);
    child_config.child_session = Some(Arc::new(move |task_id: &str, _session_id: &str| {
        Ok(Some(child_store(&parent_path, task_id)))
    }));
    let (subagents, _) = otto::subagent::runner::Runner::new(child_config).expect("valid config");
    let subagents = Arc::new(subagents);

    subagents
        .start(StartRequest {
            prompt: "first".into(),
            ..StartRequest::default()
        })
        .await
        .expect("start succeeds");
    wait_status(&tasks, "t1", TaskStatus::Running).await;

    let mut runner = Runner::scripted(
        session.clone(),
        FakeProvider::new() as Arc<dyn Provider + Send + Sync>,
        Arc::clone(&tasks),
    );
    runner.install_subagents(subagents);
    let info = builder.runtime_info(&runtime);
    let ctrl = Controller::new(builder, true, session.clone(), runner, info);
    harness.server.register(ctrl);

    let warnings = harness.server.migrate().await;
    assert!(warnings.is_empty(), "{warnings:?}");

    assert_eq!(
        tasks.get("t1").expect("t1 exists").status,
        TaskStatus::Canceled
    );
    let children_dir = std::path::Path::new(&session_path).with_extension("");
    let records = otto::subagent::interrupted::scan(&children_dir);
    assert_eq!(records.len(), 1, "{records:?}");
    assert_eq!(
        records[0].final_status.as_deref(),
        Some(otto::subagent::interrupted::INTERRUPTED_STATUS)
    );
    assert!(
        heartbeat_released(&session_path),
        "migrate must release the lease"
    );
}

// ---- compaction, sandbox reload, tasks ----

#[tokio::test]
async fn compacting_an_unknown_session_is_404() {
    let harness = Harness::new();
    let reply = harness
        .send("POST", "/v1/sessions/missing/compact", None)
        .await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn compacting_while_a_turn_runs_is_409_turn_active() {
    let (gate, options) = gated();
    let harness = Harness::with(options);
    let id = harness.create().await;
    let (_stream, _turn_id) = start_stream(&harness, &id).await;
    harness.provider.wait_started(1).await;

    let reply = harness
        .send("POST", &format!("/v1/sessions/{id}/compact"), None)
        .await;
    assert_eq!(reply.status, StatusCode::CONFLICT);
    assert_eq!(reply.json()["error"]["code"], "turn_active");
    gate.cancel();
}

/// Dropping the request future (the client closed the connection) must not
/// leave the session marked as compacting.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_dropped_compact_request_still_releases_the_session() {
    const TURNS: usize = 3;
    let gate = CancellationToken::new();
    let harness = Harness::with(HarnessOptions {
        script: Script {
            deltas: vec!["ok".to_string()],
            gate: Some(gate.clone()),
            // The turns finish; the provider call after them (the
            // compaction summary) waits on the gate.
            gate_after_calls: TURNS,
            ..Script::default()
        },
        ..HarnessOptions::default()
    });
    let id = harness.create().await;
    // Large enough that the retained tail cannot hold the earlier turns, so
    // the compaction has something to summarize through the provider.
    let text = "x".repeat(80_000);
    for _ in 0..TURNS {
        super::sessions_cases::complete_turn(&harness, &id, &text).await;
    }

    let path = format!("/v1/sessions/{id}/compact");
    let mut request = Box::pin(harness.send("POST", &path, None));
    tokio::select! {
        reply = &mut request => panic!("compaction finished before the gate: {}", reply.body),
        () = harness.provider.wait_started(TURNS + 1) => {}
    }
    drop(request);
    gate.cancel();

    let session = harness.server.lookup(&id).expect("session");
    tokio::time::timeout(Duration::from_secs(5), async {
        while session.lock().is_compacting() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("compacting stayed set after the request was dropped");
    let reply = harness
        .send(
            "POST",
            &format!("/v1/sessions/{id}/turns"),
            Some(r#"{"text":"hi","stream":false}"#),
        )
        .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
}

// ---- status ----

/// Reads the next frame off a `/v1/status` reader and decodes its data.
pub(super) async fn status_snapshot(reader: &mut SseReader) -> Value {
    let frame = reader.next().await.expect("status frame");
    assert_eq!(frame.event, "status");
    assert_eq!(frame.id, None, "the status stream sends no id");
    serde_json::from_str(&frame.data).expect("status json")
}

#[tokio::test]
async fn status_stream_without_a_token_is_401() {
    let harness = Harness::with(HarnessOptions {
        token: "secret".to_string(),
        ..HarnessOptions::default()
    });
    assert_eq!(
        harness.send("GET", "/v1/status", None).await.status,
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn status_stream_with_no_open_sessions_is_an_empty_snapshot() {
    let harness = Harness::new();
    let response = harness.raw("GET", "/v1/status", None, &[]).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok()),
        Some("text/event-stream")
    );
    let mut reader = SseReader::new(response);
    let snapshot = status_snapshot(&mut reader).await;
    assert_eq!(snapshot, serde_json::json!({"sessions": []}));
}

#[tokio::test]
async fn status_stream_ends_when_the_server_is_cancelled() {
    let harness = Harness::new();
    let response = harness.raw("GET", "/v1/status", None, &[]).await;
    let mut reader = SseReader::new(response);
    status_snapshot(&mut reader).await;

    harness.server.cancel_token().cancel();

    let frame = tokio::time::timeout(Duration::from_secs(2), reader.next())
        .await
        .expect("the status stream should end once the server is cancelled");
    assert!(frame.is_none(), "expected end of stream, got {frame:?}");
}

#[tokio::test]
async fn creating_a_session_appears_in_the_status_stream() {
    let harness = Harness::new();
    let response = harness.raw("GET", "/v1/status", None, &[]).await;
    let mut reader = SseReader::new(response);
    let empty = status_snapshot(&mut reader).await;
    assert_eq!(empty["sessions"].as_array().expect("rows").len(), 0);

    let id = harness.create().await;
    let snapshot = status_snapshot(&mut reader).await;
    let rows = snapshot["sessions"].as_array().expect("rows");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["id"], id.as_str());
    assert!(rows[0]["turn"].is_null());
    assert_eq!(rows[0]["approvals"], 0);
    assert_eq!(rows[0]["tasks"], 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn starting_a_turn_shows_running_then_finishing_shows_ok() {
    let (gate, options) = gated();
    let harness = Harness::with(options);
    let id = harness.create().await;

    let response = harness.raw("GET", "/v1/status", None, &[]).await;
    let mut reader = SseReader::new(response);
    let before = status_snapshot(&mut reader).await;
    assert!(before["sessions"][0]["turn"].is_null());

    let (_stream, turn_id) = start_stream(&harness, &id).await;
    harness.provider.wait_started(1).await;

    let running = status_snapshot(&mut reader).await;
    assert_eq!(running["sessions"][0]["turn"], turn::TURN_RUNNING);

    gate.cancel();
    harness.wait_turn_done(&id, &turn_id).await;

    let done = status_snapshot(&mut reader).await;
    assert_eq!(done["sessions"][0]["turn"], turn::TURN_OK);
}

#[tokio::test]
async fn a_failing_turn_shows_error_in_the_status_stream() {
    let harness = Harness::with(HarnessOptions {
        script: Script {
            error: Some("boom".to_string()),
            ..Script::default()
        },
        ..HarnessOptions::default()
    });
    let id = harness.create().await;
    let response = harness.raw("GET", "/v1/status", None, &[]).await;
    let mut reader = SseReader::new(response);
    status_snapshot(&mut reader).await; // the session with no turn yet

    harness
        .send(
            "POST",
            &format!("/v1/sessions/{id}/turns"),
            Some(r#"{"text":"hi","stream":false}"#),
        )
        .await;

    let after = status_snapshot(&mut reader).await;
    assert_eq!(after["sessions"][0]["turn"], turn::TURN_ERROR);
}

#[tokio::test]
async fn a_session_in_a_second_workspace_appears_with_its_workspace() {
    let harness = Harness::with(HarnessOptions {
        workspaces: FakeWorkspaces {
            admitted: vec!["/other".to_string()],
            ..FakeWorkspaces::default()
        },
        ..HarnessOptions::default()
    });
    let response = harness.raw("GET", "/v1/status", None, &[]).await;
    let mut reader = SseReader::new(response);
    status_snapshot(&mut reader).await;

    let reply = harness
        .send("POST", "/v1/sessions", Some(r#"{"workspace":"/other"}"#))
        .await;
    assert_eq!(reply.status, StatusCode::CREATED, "{}", reply.body);

    let snapshot = status_snapshot(&mut reader).await;
    let rows = snapshot["sessions"].as_array().expect("rows");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["workspace"], "/other");
}

#[tokio::test]
async fn closing_a_session_removes_it_from_the_status_stream() {
    let harness = Harness::new();
    harness.create().await;
    let response = harness.raw("GET", "/v1/status", None, &[]).await;
    let mut reader = SseReader::new(response);
    let first = status_snapshot(&mut reader).await;
    assert_eq!(first["sessions"].as_array().expect("rows").len(), 1);

    let id = first["sessions"][0]["id"].as_str().expect("id").to_string();
    harness
        .send("DELETE", &format!("/v1/sessions/{id}"), None)
        .await;

    let after = status_snapshot(&mut reader).await;
    assert_eq!(after["sessions"].as_array().expect("rows").len(), 0);
}

#[tokio::test]
async fn identical_snapshots_in_a_row_are_sent_once() {
    let harness = Harness::new();
    let response = harness.raw("GET", "/v1/status", None, &[]).await;
    let mut reader = SseReader::new(response);
    status_snapshot(&mut reader).await;

    // Two bumps that leave the snapshot unchanged must coalesce into no
    // further frame; the reader only sees a change.
    harness
        .server
        .status_changed
        .send_modify(|version| *version += 1);
    harness
        .server
        .status_changed
        .send_modify(|version| *version += 1);

    assert!(
        tokio::time::timeout(Duration::from_millis(100), reader.next())
            .await
            .is_err(),
        "an unchanged snapshot must not be resent"
    );
}

#[tokio::test]
async fn api_server_does_not_serve_frontend_resources() {
    let harness = Harness::with(HarnessOptions {
        token: "secret".to_string(),
        ..HarnessOptions::default()
    });
    for path in ["/", "/assets/app.js", "/no-such-path"] {
        assert_eq!(
            harness.send("GET", path, None).await.status,
            StatusCode::NOT_FOUND,
            "{path} must not serve a frontend"
        );
    }
    assert_eq!(
        harness.send("GET", "/healthz", None).await.status,
        StatusCode::OK
    );
    assert_eq!(
        harness.send("GET", "/v1/sessions", None).await.status,
        StatusCode::UNAUTHORIZED
    );
}
