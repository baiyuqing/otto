//! Session rename, queued-turn restore, idle eviction, fork and create-retry tests.

use super::*;

// ---- sessions ----

#[tokio::test]
async fn renaming_a_session_updates_the_session_and_the_list() {
    let harness = Harness::with(HarnessOptions {
        list: Some(ListResult::default()),
        ..HarnessOptions::default()
    });
    let id = harness.create().await;

    let reply = harness
        .send(
            "PATCH",
            &format!("/v1/sessions/{id}"),
            Some(r#"{"name":"dev"}"#),
        )
        .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    assert_eq!(reply.json()["name"], "dev");

    let list = harness.send("GET", "/v1/sessions", None).await.json();
    assert_eq!(list["sessions"].as_array().expect("rows").len(), 1);
    assert_eq!(list["sessions"][0]["name"], "dev");
}

#[tokio::test]
async fn renaming_rejects_an_unknown_session_and_a_blank_name() {
    let harness = Harness::new();
    let reply = harness
        .send("PATCH", "/v1/sessions/missing", Some(r#"{"name":"dev"}"#))
        .await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND);

    let id = harness.create().await;
    let reply = harness
        .send(
            "PATCH",
            &format!("/v1/sessions/{id}"),
            Some(r#"{"name":" \t"}"#),
        )
        .await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    assert_eq!(reply.json()["error"]["code"], "bad_request");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn queued_turns_survive_a_shutdown_and_run_once_in_order_after_reopen() {
    let (gate, options) = gated();
    let harness = Harness::with(HarnessOptions {
        persistent: true,
        ..options
    });
    let id = harness.create().await;
    let path = format!("/v1/sessions/{id}/turns");
    // The first turn holds the session at the gate; two more queue.
    let first = post_turn(&harness, &id, "running", false).await;
    assert_eq!(first.status(), StatusCode::OK);
    tokio::time::timeout(Duration::from_secs(2), harness.provider.wait_started(1))
        .await
        .expect("first turn started");
    let mut queued = Vec::new();
    for body in [
        r#"{"text":"second","queue":true,"input_id":"req-2"}"#,
        r#"{"text":"third","queue":true}"#,
    ] {
        let reply = harness.raw("POST", &path, Some(body), &[]).await;
        assert_eq!(reply.status(), StatusCode::OK);
        let turn_id = reply
            .headers()
            .get("otto-turn-id")
            .and_then(|value| value.to_str().ok())
            .expect("turn id")
            .to_string();
        let summary = harness
            .send("GET", &format!("{path}/{turn_id}"), None)
            .await
            .json();
        assert_eq!(summary["status"], turn::TURN_QUEUED, "{summary}");
        queued.push(turn_id);
    }

    // A shutdown cancels the running turn and ends the queued ones
    // `canceled` in memory, but keeps their saved copies.
    let session = harness.server.lookup(&id).expect("session");
    session.cancel_work();
    let removed = harness.server.remove(&id).expect("registered");
    let ctrl = Arc::clone(&removed.ctrl);
    tokio::task::spawn_blocking(move || ctrl.close())
        .await
        .unwrap()
        .expect("close");
    gate.cancel();
    let calls_before = harness.provider.texts().len();

    // Reopening the session restores and runs them, keeping their ids.
    let reopened = harness
        .send(
            "POST",
            "/v1/sessions",
            Some(&serde_json::json!({ "resume": id }).to_string()),
        )
        .await;
    assert_eq!(reopened.status, StatusCode::OK, "{}", reopened.body);
    // Only the newest turn is retained, so wait on the last one; the
    // provider calls show both ran, in order.
    let done = harness.wait_turn_done(&id, &queued[1]).await;
    assert_eq!(done["status"], turn::TURN_OK, "{done}");
    let texts = harness.provider.texts();
    assert_eq!(texts[calls_before..], ["second", "third"], "{texts:?}");

    // The first restored turn delivered the restore notice first.
    let history = harness
        .send("GET", &format!("/v1/sessions/{id}/history"), None)
        .await
        .json();
    let history: Vec<String> = history
        .as_array()
        .expect("history")
        .iter()
        .map(|message| {
            message["blocks"][0]["text"]
                .as_str()
                .unwrap_or_default()
                .to_string()
        })
        .collect();
    let notice = history
        .iter()
        .position(|text| text.contains("Otto stopped before 2 queued prompts"))
        .expect("restore notice");
    let second = history
        .iter()
        .position(|text| text == "second")
        .expect("second");
    assert!(notice < second, "{history:?}");

    // A retry returns the restored turn, and nothing waits any more.
    let retry = harness
        .raw(
            "POST",
            &path,
            Some(r#"{"text":"second","queue":true,"input_id":"req-2"}"#),
            &[],
        )
        .await;
    assert_eq!(retry.status(), StatusCode::OK);
    assert_eq!(
        retry
            .headers()
            .get("otto-turn-id")
            .and_then(|value| value.to_str().ok()),
        Some(queued[0].as_str()),
        "the retry returns the restored turn"
    );
    assert_eq!(
        harness.provider.texts().len(),
        calls_before + 2,
        "and runs nothing"
    );
    let session = harness.server.lookup(&id).expect("session");
    assert!(
        session
            .ctrl
            .turn_queue()
            .expect("journal")
            .pending()
            .is_empty()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_cancelled_or_deleted_queued_turn_is_not_restored() {
    let (gate, options) = gated();
    let harness = Harness::with(HarnessOptions {
        persistent: true,
        ..options
    });
    let id = harness.create().await;
    let path = format!("/v1/sessions/{id}/turns");
    let _running = post_turn(&harness, &id, "running", false).await;
    tokio::time::timeout(Duration::from_secs(2), harness.provider.wait_started(1))
        .await
        .expect("first turn started");
    let queued = post_turn(&harness, &id, "cancel me", true).await;
    let queued_id = queued
        .headers()
        .get("otto-turn-id")
        .and_then(|value| value.to_str().ok())
        .expect("turn id")
        .to_string();
    let kept = post_turn(&harness, &id, "delete me", true).await;
    assert_eq!(kept.status(), StatusCode::OK);
    let session = harness.server.lookup(&id).expect("session");
    let journal = session.ctrl.turn_queue().expect("journal");
    assert_eq!(journal.pending().len(), 2);

    let cancel = harness
        .send("POST", &format!("{path}/{queued_id}/cancel"), None)
        .await;
    assert_eq!(cancel.status, StatusCode::ACCEPTED);
    assert_eq!(
        journal
            .pending()
            .iter()
            .map(|prompt| prompt.text.as_str())
            .collect::<Vec<_>>(),
        ["delete me"]
    );

    let deleted = harness
        .send("DELETE", &format!("/v1/sessions/{id}"), None)
        .await;
    assert_eq!(deleted.status, StatusCode::NO_CONTENT);
    gate.cancel();
    assert!(journal.pending().is_empty());
}

/// Runs one turn on `session` to completion.
pub(super) async fn complete_turn(harness: &Harness, session: &str, text: &str) {
    let response = post_turn(harness, session, text, false).await;
    assert_eq!(response.status(), StatusCode::OK);
    let turn_id = response
        .headers()
        .get("otto-turn-id")
        .and_then(|value| value.to_str().ok())
        .expect("Otto-Turn-Id header")
        .to_string();
    SseReader::new(response).all().await;
    let summary = harness.wait_turn_done(session, &turn_id).await;
    assert_eq!(summary["status"], turn::TURN_OK, "{summary}");
}

// ---- idle eviction ----

#[test]
fn a_session_route_names_a_session_id_and_nothing_else() {
    let id = "0123456789abcdef0123456789abcdef";
    assert_eq!(session_id_in_path(&format!("/v1/sessions/{id}")), Some(id));
    assert_eq!(
        session_id_in_path(&format!("/v1/sessions/{id}/turns/{id}/events")),
        Some(id)
    );
    for path in [
        "/v1/sessions",
        "/v1/sessions/",
        "/v1/sessions/missing",
        "/v1/sessions/missing/history",
        // Uppercase and non-hex ids are not session ids.
        "/v1/sessions/0123456789ABCDEF0123456789ABCDEF/history",
        "/v1/tasks/s1/with-transcript",
        "/v1/approvals/changes?session_id=1",
        "/healthz",
    ] {
        assert_eq!(session_id_in_path(path), None, "{path}");
    }
}

#[tokio::test]
async fn an_idle_session_is_evicted_and_its_next_request_opens_it_again() {
    let harness = Harness::with(HarnessOptions {
        script: Script {
            deltas: vec!["answer".to_string()],
            ..Script::default()
        },
        persistent: true,
        session_idle_timeout: Some(Duration::ZERO),
        ..HarnessOptions::default()
    });
    let id = harness.create().await;
    complete_turn(&harness, &id, "first").await;
    let history = harness
        .send("GET", &format!("/v1/sessions/{id}/history"), None)
        .await
        .json();

    // Idle for any length of time: the session closes and leaves the
    // registry, which is what keeps a long-lived server bounded.
    let evicted = harness.server.sweep_idle_sessions().await;
    assert_eq!(evicted, vec![id.clone()]);
    assert!(harness.server.lookup(&id).is_none());
    let metrics = harness.metrics_body().await;
    assert!(metrics.contains("otto_sessions_open 0"), "{metrics}");
    assert!(
        metrics.contains("otto_sessions_evicted_total 1"),
        "{metrics}"
    );

    // The client still holds the id: the next request re-opens the session
    // from disk and answers exactly as it did before the eviction.
    let reply = harness
        .send("GET", &format!("/v1/sessions/{id}/history"), None)
        .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    assert_eq!(reply.json(), history);
    assert!(harness.server.lookup(&id).is_some());
    let metrics = harness.metrics_body().await;
    assert!(metrics.contains("otto_sessions_open 1"), "{metrics}");
}

#[tokio::test]
async fn an_evicted_session_keeps_accepting_turns_over_its_own_routes() {
    let harness = Harness::with(HarnessOptions {
        script: Script {
            deltas: vec!["answer".to_string()],
            ..Script::default()
        },
        persistent: true,
        session_idle_timeout: Some(Duration::ZERO),
        ..HarnessOptions::default()
    });
    let id = harness.create().await;
    complete_turn(&harness, &id, "first").await;
    assert_eq!(harness.server.sweep_idle_sessions().await, vec![id.clone()]);

    complete_turn(&harness, &id, "second").await;
    let history = harness
        .send("GET", &format!("/v1/sessions/{id}/history"), None)
        .await
        .json();
    let texts: Vec<&str> = history
        .as_array()
        .expect("history")
        .iter()
        .filter_map(|message| message["blocks"][0]["text"].as_str())
        .collect();
    assert_eq!(texts, ["first", "answer", "second", "answer"], "{history}");
}

#[tokio::test]
async fn a_closed_session_is_not_re_opened_by_a_later_request() {
    let harness = Harness::new();
    let id = harness.create().await;
    let deleted = harness
        .send("DELETE", &format!("/v1/sessions/{id}"), None)
        .await;
    assert_eq!(deleted.status, StatusCode::NO_CONTENT);

    // The client closed this one itself: closing it must keep winning, so
    // eviction stays the only state a request re-opens.
    for path in [
        format!("/v1/sessions/{id}"),
        format!("/v1/sessions/{id}/history"),
    ] {
        let reply = harness.send("GET", &path, None).await;
        assert_eq!(reply.status, StatusCode::NOT_FOUND, "{path}");
    }
    assert!(harness.server.lookup(&id).is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_session_with_a_running_turn_is_never_evicted() {
    let (gate, mut options) = gated();
    options.session_idle_timeout = Some(Duration::ZERO);
    let harness = Harness::with(options);
    let id = harness.create().await;
    let (_stream, _turn_id) = start_stream(&harness, &id).await;
    harness.provider.wait_started(1).await;

    assert!(harness.server.sweep_idle_sessions().await.is_empty());
    assert!(harness.server.lookup(&id).is_some());
    gate.cancel();
}

#[tokio::test]
async fn a_pinned_session_is_never_evicted_until_the_holder_releases_it() {
    let harness = Harness::with(HarnessOptions {
        session_idle_timeout: Some(Duration::ZERO),
        ..HarnessOptions::default()
    });
    let id = harness.create().await;
    assert!(harness.server.pin_session(&id));
    assert!(harness.server.sweep_idle_sessions().await.is_empty());

    harness.server.unpin_session(&id);
    assert_eq!(harness.server.sweep_idle_sessions().await, vec![id.clone()]);
}

/// The sweep selects victims first and removes them later; work that starts in
/// between must keep the session open.
#[tokio::test]
async fn a_session_that_became_busy_after_selection_is_not_removed() {
    let harness = Harness::with(HarnessOptions {
        session_idle_timeout: Some(Duration::ZERO),
        ..HarnessOptions::default()
    });
    let id = harness.create().await;
    let session = harness.server.lookup(&id).expect("session");

    session.lock().compacting = Some(CancellationToken::new());
    assert!(!harness.server.remove_if_idle(&id, &session));
    assert!(harness.server.lookup(&id).is_some(), "busy session removed");
    session.lock().compacting = None;

    assert!(harness.server.pin_session(&id));
    assert!(!harness.server.remove_if_idle(&id, &session));
    assert!(
        harness.server.lookup(&id).is_some(),
        "pinned session removed"
    );
    harness.server.unpin_session(&id);

    assert!(harness.server.remove_if_idle(&id, &session));
    assert!(harness.server.lookup(&id).is_none());
}

#[tokio::test]
async fn the_sweep_closes_the_least_recently_active_sessions_over_the_cap() {
    const CAP: usize = 3;
    let harness = Harness::with(HarnessOptions {
        // Nothing is old enough to be idle, so only the cap applies.
        session_idle_timeout: Some(Duration::from_secs(3600)),
        max_open_sessions: Some(CAP),
        ..HarnessOptions::default()
    });
    let mut ids = Vec::new();
    for _ in 0..(CAP + 2) {
        ids.push(harness.create().await);
    }
    // A request on the first session makes it the most recently active one,
    // which the cap must leave open. The sleep separates its stamp from the
    // creation stamps.
    tokio::time::sleep(Duration::from_millis(5)).await;
    let touched = harness
        .send("GET", &format!("/v1/sessions/{}", ids[0]), None)
        .await;
    assert_eq!(touched.status, StatusCode::OK);

    let evicted = harness.server.sweep_idle_sessions().await;
    assert_eq!(evicted.len(), 2, "{evicted:?}");
    assert!(!evicted.contains(&ids[0]), "{evicted:?}");
    assert_eq!(harness.server.all_sessions().len(), CAP);
    assert_eq!(
        harness.server.sweep_idle_sessions().await,
        Vec::<String>::new()
    );
}

#[tokio::test]
async fn a_sweep_with_no_limits_closes_nothing() {
    let harness = Harness::new();
    let id = harness.create().await;
    assert!(harness.server.sweep_idle_sessions().await.is_empty());
    assert!(harness.server.lookup(&id).is_some());

    // An idle window with no cap still closes an aged session.
    let timed = Harness::with(HarnessOptions {
        session_idle_timeout: Some(Duration::ZERO),
        ..HarnessOptions::default()
    });
    let id = timed.create().await;
    assert_eq!(timed.server.sweep_idle_sessions().await, vec![id]);
}

#[tokio::test]
async fn forking_a_session_opens_a_copy_before_the_chosen_prompt() {
    let harness = Harness::with(HarnessOptions {
        script: Script {
            deltas: vec!["answer".to_string()],
            ..Script::default()
        },
        persistent: true,
        ..HarnessOptions::default()
    });
    let id = harness.create().await;
    complete_turn(&harness, &id, "first").await;
    complete_turn(&harness, &id, "second").await;

    let points = harness
        .send("GET", &format!("/v1/sessions/{id}/fork-points"), None)
        .await;
    assert_eq!(points.status, StatusCode::OK, "{}", points.body);
    let points = points.json()["fork_points"].clone();
    assert_eq!(points[0]["prompt"], "first");
    assert_eq!(points[1]["prompt"], "second");
    let second = points[1]["entry_id"]
        .as_str()
        .expect("entry id")
        .to_string();

    let reply = harness
        .send(
            "POST",
            &format!("/v1/sessions/{id}/fork"),
            Some(&serde_json::json!({ "before": second }).to_string()),
        )
        .await;
    assert_eq!(reply.status, StatusCode::CREATED, "{}", reply.body);
    let fork = reply.json();
    let fork_id = fork["id"].as_str().expect("fork id").to_string();
    assert_ne!(fork_id, id);
    assert_eq!(fork["prompt"], "second");
    assert_eq!(fork["forked_from"]["session_id"], id.as_str());
    assert_eq!(fork["forked_from"]["entry_id"], second.as_str());

    let history = harness
        .send("GET", &format!("/v1/sessions/{fork_id}/history"), None)
        .await
        .json();
    let texts: Vec<&str> = history
        .as_array()
        .expect("history")
        .iter()
        .filter_map(|message| message["blocks"][0]["text"].as_str())
        .collect();
    assert_eq!(texts, ["first", "answer"]);
    let source = harness
        .send("GET", &format!("/v1/sessions/{id}"), None)
        .await
        .json();
    assert!(source.get("forked_from").is_none(), "{source}");

    let reply = harness
        .send(
            "POST",
            &format!("/v1/sessions/{id}/fork"),
            Some(r#"{"before":"ffffffff"}"#),
        )
        .await;
    assert_eq!(reply.status, StatusCode::CONFLICT, "{}", reply.body);
    assert_eq!(reply.json()["error"]["code"], "invalid_fork_point");
}

#[tokio::test]
async fn session_create_retry_returns_the_original_after_restart() {
    let mut harness = Harness::with(HarnessOptions {
        persistent: true,
        ..HarnessOptions::default()
    });
    let body = r#"{"request_id":"create-1"}"#;
    let first = harness.send("POST", "/v1/sessions", Some(body)).await;
    assert_eq!(first.status, StatusCode::CREATED, "{}", first.body);
    let id = first.json()["id"].as_str().unwrap().to_string();
    let file = otto::session::session_directory(
        std::path::Path::new(&harness.factory.builder.session_root),
        &harness.factory.builder.workspace_path,
    )
    .unwrap()
    .join(format!("{id}.jsonl"));
    assert!(
        file.is_file(),
        "the acknowledgement requires a durable receipt"
    );
    let retry = harness.send("POST", "/v1/sessions", Some(body)).await;
    assert_eq!(retry.status, StatusCode::OK);
    assert_eq!(retry.json()["id"], id);
    assert_eq!(harness.factory.create_calls.load(Ordering::SeqCst), 1);

    harness.restart().await;
    let retry = harness.send("POST", "/v1/sessions", Some(body)).await;
    assert_eq!(retry.status, StatusCode::OK, "{}", retry.body);
    assert_eq!(retry.json()["id"], id);
    assert_eq!(harness.factory.create_calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn concurrent_session_create_retries_create_once() {
    let harness = Harness::with(HarnessOptions {
        persistent: true,
        ..HarnessOptions::default()
    });
    let body = r#"{"request_id":"create-concurrent"}"#;
    let (a, b) = tokio::join!(
        harness.send("POST", "/v1/sessions", Some(body)),
        harness.send("POST", "/v1/sessions", Some(body)),
    );
    assert_eq!(
        [a.status, b.status]
            .into_iter()
            .filter(|status| *status == StatusCode::CREATED)
            .count(),
        1
    );
    assert_eq!(a.json()["id"], b.json()["id"]);
    assert_eq!(harness.factory.create_calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn create_retry_repairs_a_header_written_before_its_receipt() {
    let harness = Harness::with(HarnessOptions {
        persistent: true,
        ..HarnessOptions::default()
    });
    let id = command_session_id(
        "create",
        &harness.factory.builder.workspace_path,
        "partial-1",
    );
    let header = Header {
        version: CURRENT_VERSION,
        id: id.clone(),
        workspace: harness.factory.builder.workspace_path.clone(),
        provider: "openai-compatible".into(),
        profile: "alpha".into(),
        model: "test-model".into(),
        created_at: chrono::Utc::now(),
    };
    otto::session::Store::create(&harness.factory.builder.session_root, header)
        .expect("header written")
        .close()
        .expect("close partial file");
    let response = harness
        .send(
            "POST",
            "/v1/sessions",
            Some(r#"{"request_id":"partial-1"}"#),
        )
        .await;
    assert_eq!(response.status, StatusCode::OK, "{}", response.body);
    assert_eq!(response.json()["id"], id);
    assert!(
        harness
            .server
            .lookup(&id)
            .unwrap()
            .ctrl
            .session_custom_data("otto.session_command")
            .is_some()
    );
}

#[tokio::test]
async fn concurrent_fork_retries_publish_one_copy() {
    let harness = Harness::with(HarnessOptions {
        script: Script {
            deltas: vec!["answer".into()],
            ..Script::default()
        },
        persistent: true,
        ..HarnessOptions::default()
    });
    let source = harness.create().await;
    complete_turn(&harness, &source, "first").await;
    let path = format!("/v1/sessions/{source}/fork");
    let body = r#"{"request_id":"fork-concurrent"}"#;
    let (a, b) = tokio::join!(
        harness.send("POST", &path, Some(body)),
        harness.send("POST", &path, Some(body)),
    );
    assert_eq!(
        [a.status, b.status]
            .into_iter()
            .filter(|status| *status == StatusCode::CREATED)
            .count(),
        1
    );
    assert_eq!(a.json()["id"], b.json()["id"]);
}

#[tokio::test]
async fn fork_retry_returns_one_copy_and_rejects_changed_arguments() {
    let mut harness = Harness::with(HarnessOptions {
        script: Script {
            deltas: vec!["answer".into()],
            ..Script::default()
        },
        persistent: true,
        ..HarnessOptions::default()
    });
    let source = harness.create().await;
    complete_turn(&harness, &source, "first").await;
    complete_turn(&harness, &source, "second").await;
    let points = harness
        .send("GET", &format!("/v1/sessions/{source}/fork-points"), None)
        .await
        .json();
    let before = points["fork_points"][1]["entry_id"].as_str().unwrap();
    let body = serde_json::json!({"before":before,"request_id":"fork-1"}).to_string();
    let path = format!("/v1/sessions/{source}/fork");
    let first = harness.send("POST", &path, Some(&body)).await;
    assert_eq!(first.status, StatusCode::CREATED, "{}", first.body);
    let fork_id = first.json()["id"].as_str().unwrap().to_string();
    let retry = harness.send("POST", &path, Some(&body)).await;
    assert_eq!(retry.status, StatusCode::OK, "{}", retry.body);
    assert_eq!(retry.json()["id"], fork_id);
    assert_eq!(retry.json()["prompt"], "second");
    complete_turn(&harness, &source, "third").await;
    let retry_after_source_changed = harness.send("POST", &path, Some(&body)).await;
    assert_eq!(retry_after_source_changed.status, StatusCode::OK);
    assert_eq!(retry_after_source_changed.json()["id"], fork_id);
    let changed = serde_json::json!({"request_id":"fork-1"}).to_string();
    let conflict = harness.send("POST", &path, Some(&changed)).await;
    assert_eq!(conflict.status, StatusCode::CONFLICT);
    assert_eq!(conflict.json()["error"]["code"], "request_id_conflict");
    let other_source = harness.create().await;
    complete_turn(&harness, &other_source, "other").await;
    let wrong_source = harness
        .send(
            "POST",
            &format!("/v1/sessions/{other_source}/fork"),
            Some(&changed),
        )
        .await;
    assert_eq!(wrong_source.status, StatusCode::CONFLICT);
    assert_eq!(wrong_source.json()["error"]["code"], "request_id_conflict");

    harness.restart().await;
    let retry = harness.send("POST", &path, Some(&body)).await;
    assert_eq!(retry.status, StatusCode::OK, "{}", retry.body);
    assert_eq!(retry.json()["id"], fork_id);
}

#[tokio::test]
async fn forking_an_unknown_or_unwritten_session_is_refused() {
    let harness = Harness::new();
    let reply = harness
        .send("POST", "/v1/sessions/missing/fork", None)
        .await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND);

    // A session with no file has nothing to fork.
    let id = harness.create().await;
    let reply = harness
        .send("POST", &format!("/v1/sessions/{id}/fork"), None)
        .await;
    assert_eq!(reply.status, StatusCode::CONFLICT, "{}", reply.body);
    assert_eq!(reply.json()["error"]["code"], "invalid_fork_point");
    let points = harness
        .send("GET", &format!("/v1/sessions/{id}/fork-points"), None)
        .await;
    assert_eq!(points.status, StatusCode::OK);
    assert_eq!(points.json()["fork_points"], serde_json::json!([]));
}
