//! Wake turn tests.

use super::*;

// ---- wake turns ----

/// A harness whose every controller shares `tasks`, plus the registry.
pub(super) fn wake_harness(script: Script) -> (Harness, Arc<crate::subagent::tasks::Tasks>) {
    let tasks = Arc::new(crate::subagent::tasks::Tasks::new());
    let harness = Harness::with(HarnessOptions {
        script,
        tasks: Some(Arc::clone(&tasks)),
        ..HarnessOptions::default()
    });
    (harness, tasks)
}

pub(super) fn notification() -> otto_core::agent::inbox::Notification {
    otto_core::agent::inbox::Notification {
        task_id: "t1".to_string(),
        kind: Some(otto_core::agent::inbox::NotificationKind::TaskFinished),
        text: "done".to_string(),
        usage: None,
        ..Default::default()
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_pending_notification_starts_a_wake_turn_while_idle() {
    let (harness, tasks) = wake_harness(Script {
        deltas: vec!["ok".to_string()],
        ..Script::default()
    });
    let id = harness.create().await;

    tasks.notifications().push(notification());

    tokio::time::timeout(Duration::from_secs(2), harness.provider.wait_started(1))
        .await
        .expect("the pending notification did not start a wake turn");
    assert_eq!(harness.provider.roles(), vec![Role::Context]);

    let session = harness
        .send("GET", &format!("/v1/sessions/{id}"), None)
        .await;
    let turn = session.json()["turn"].clone();
    assert_eq!(turn["trigger"], turn::TRIGGER_TASK, "{}", session.body);
    let turn_id = turn["id"].as_str().expect("turn id").to_string();

    let done = harness.wait_turn_done(&id, &turn_id).await;
    assert_eq!(done["trigger"], turn::TRIGGER_TASK);
}

/// A client retry of `POST .../turns` with the same `input_id` returns
/// the turn it already started; after a restart, when only the
/// transcript remembers the id, it answers `409 duplicate_input`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_repeated_input_id_starts_one_turn() {
    let harness = Harness::with(HarnessOptions {
        script: Script {
            deltas: vec!["ok".to_string()],
            ..Script::default()
        },
        ..HarnessOptions::default()
    });
    let id = harness.create().await;
    let path = format!("/v1/sessions/{id}/turns");
    let body = serde_json::json!({"text": "hi", "stream": false, "input_id": "req-1"}).to_string();
    let first = harness.send("POST", &path, Some(&body)).await;
    assert_eq!(first.status, StatusCode::OK, "{}", first.body);
    let again = harness.send("POST", &path, Some(&body)).await;
    assert_eq!(again.status, StatusCode::OK, "{}", again.body);
    assert_eq!(first.json()["id"], again.json()["id"]);
    assert_eq!(harness.provider.texts().len(), 1, "the retry ran no turn");

    let session = harness.server.lookup(&id).expect("session");
    assert!(
        session.ctrl.has_input("req-1"),
        "the user message carries it"
    );
    // A restart forgets the turns the server started.
    session.lock().inputs.clear();
    let after_restart = harness.send("POST", &path, Some(&body)).await;
    assert_eq!(after_restart.status, StatusCode::CONFLICT);
    assert_eq!(after_restart.json()["error"]["code"], "duplicate_input");

    let bad = serde_json::json!({"text": "hi", "input_id": "no spaces"}).to_string();
    let rejected = harness.send("POST", &path, Some(&bad)).await;
    assert_eq!(rejected.status, StatusCode::BAD_REQUEST);
}

/// A wake turn on a session whose last turn never ended resumes it and
/// is reported with trigger `recovery`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_wake_turn_resuming_an_interrupted_turn_reports_trigger_recovery() {
    let (harness, tasks) = wake_harness(Script {
        deltas: vec!["ok".to_string()],
        ..Script::default()
    });
    let id = harness.create().await;
    let session = harness.server.lookup(&id).expect("session");
    let runner = session.ctrl.current_runner().expect("runner");
    otto_core::session::Session::append_custom(
        runner.session(),
        otto_core::session::TURN_CUSTOM_TYPE,
        &otto_core::session::encode_turn_record(&otto_core::session::TurnRecord::started(
            "op_lost",
            otto_core::session::TurnTrigger::User,
            1,
            None,
        )),
    )
    .await
    .expect("seed an interrupted turn");

    tasks.notifications().push(notification());

    tokio::time::timeout(Duration::from_secs(2), harness.provider.wait_started(1))
        .await
        .expect("the notification did not start a wake turn");
    let reply = harness
        .send("GET", &format!("/v1/sessions/{id}"), None)
        .await;
    let turn = reply.json()["turn"].clone();
    assert_eq!(turn["trigger"], turn::TRIGGER_RECOVERY, "{}", reply.body);
    let turn_id = turn["id"].as_str().expect("turn id").to_string();
    let done = harness.wait_turn_done(&id, &turn_id).await;
    assert_eq!(done["trigger"], turn::TRIGGER_RECOVERY);
    assert!(!session.ctrl.resumes_interrupted_turn());
}

/// R13: `failover::recovery::notify` pushes through the same
/// `tasks.notifications()` inbox `Builder::session_resources` wires the recovery
/// push into, so it starts a wake turn the same way any other pending
/// notification does; no separate signaling code exists for it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_recovery_notification_starts_a_wake_turn_while_idle() {
    let (harness, tasks) = wake_harness(Script {
        deltas: vec!["ok".to_string()],
        ..Script::default()
    });
    harness.create().await;

    let takeover = crate::session::Recovery {
        cause: crate::session::RecoveryCause::TakenOver(crate::failover::lease::Holder {
            epoch: 1,
            host: "stopped-host".to_string(),
            pid: 4242,
        }),
        repaired: Vec::new(),
        replayable: Vec::new(),
        interrupted_turn: None,
    };
    crate::failover::recovery::notify(
        tasks.notifications().as_ref(),
        Some(takeover),
        &tempfile::tempdir().expect("dir").path().join("children"),
        1000,
        &[],
    );

    tokio::time::timeout(Duration::from_secs(2), harness.provider.wait_started(1))
        .await
        .expect("the recovery notification did not start a wake turn");
    assert_eq!(harness.provider.roles(), vec![Role::Context]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_message_wake_logs_inbox_kind() {
    let (harness, tasks) = wake_harness(Script {
        deltas: vec!["ok".to_string()],
        ..Script::default()
    });
    harness.create().await;
    tasks
        .notifications()
        .push(otto_core::agent::inbox::Notification {
            kind: Some(otto_core::agent::inbox::NotificationKind::Message),
            text: "[feishu] hi".to_string(),
            ..otto_core::agent::inbox::Notification::default()
        });
    tokio::time::timeout(Duration::from_secs(2), harness.provider.wait_started(1))
        .await
        .expect("the message did not start a wake turn");
    let log = harness.logged();
    assert!(log.contains("msg=turn_started"), "{log}");
    assert!(log.contains("inbox_kind=message"), "{log}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_notification_during_a_user_turn_waits_for_it_to_finish() {
    let gate = CancellationToken::new();
    let (harness, tasks) = wake_harness(Script {
        deltas: vec!["ok".to_string()],
        gate: Some(gate.clone()),
        ..Script::default()
    });
    let id = harness.create().await;
    let (_stream, _turn_id) = start_stream(&harness, &id).await;
    harness.provider.wait_started(1).await;

    // A notification while the user turn is active must not start a
    // second turn: the active turn's own inbox drain handles it.
    tasks.notifications().push(notification());
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(harness.provider.roles(), vec![Role::User]);

    gate.cancel();

    // The user turn's end-of-turn check finds the notification still
    // pending and starts exactly one wake turn.
    tokio::time::timeout(Duration::from_secs(2), harness.provider.wait_started(2))
        .await
        .expect("the finished user turn did not start a wake turn");
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(harness.provider.roles(), vec![Role::User, Role::Context]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_notification_landing_as_a_user_turn_ends_starts_one_wake_turn() {
    let tasks = Arc::new(crate::subagent::tasks::Tasks::new());
    let pusher = Arc::clone(&tasks);
    let harness = Harness::with(HarnessOptions {
        script: Script {
            deltas: vec!["ok".to_string()],
            // The notification lands during the parent's last provider
            // call, so the turn's own drain never sees it.
            on_call: Some(Box::new(move |call| {
                if call == 1 {
                    pusher
                        .notifications()
                        .push(otto_core::agent::inbox::Notification {
                            task_id: "t1".to_string(),
                            kind: Some(otto_core::agent::inbox::NotificationKind::TaskFinished),
                            text: "done".to_string(),
                            usage: None,
                            ..Default::default()
                        });
                }
            })),
            ..Script::default()
        },
        tasks: Some(Arc::clone(&tasks)),
        ..HarnessOptions::default()
    });
    let id = harness.create().await;

    let reply = harness
        .send(
            "POST",
            &format!("/v1/sessions/{id}/turns"),
            Some(r#"{"text":"hi","stream":false}"#),
        )
        .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);

    tokio::time::timeout(Duration::from_secs(2), harness.provider.wait_started(2))
        .await
        .expect("the late notification did not start a wake turn");
    // Give an incorrect extra wake turn a chance to start.
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(harness.provider.roles(), vec![Role::User, Role::Context]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn closing_the_server_ends_the_wake_loop() {
    let (harness, _tasks) = wake_harness(Script::default());
    harness.create().await;

    tokio::time::timeout(Duration::from_secs(2), harness.server.close())
        .await
        .expect("close did not return; the wake loop leaked")
        .expect("close");
}

/// The wake loop opened with the session keeps waking after a profile switch
/// builds a new runner, because the inbox and registry belong to the session.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_pending_notification_starts_a_wake_turn_after_a_profile_switch() {
    let (harness, tasks) = wake_harness(Script {
        deltas: vec!["ok".to_string()],
        ..Script::default()
    });
    let id = harness.create().await;
    let reply = harness
        .send(
            "POST",
            &format!("/v1/sessions/{id}/profile"),
            Some(r#"{"profile":"beta"}"#),
        )
        .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);

    tasks.notifications().push(notification());

    // The rebuilt runner has the real provider client, which the offline
    // test cannot reach, so the wake turn is observed through the session's
    // turn record instead of the scripted provider.
    let started = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let reply = harness
                .send("GET", &format!("/v1/sessions/{id}"), None)
                .await;
            if reply.json()["turn"]["trigger"] == turn::TRIGGER_TASK {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await;
    assert!(
        started.is_ok(),
        "the notification did not start a wake turn after the profile switch"
    );
}
