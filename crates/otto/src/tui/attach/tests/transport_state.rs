use super::*;
use axum::response::IntoResponse;
use std::sync::Mutex;

#[derive(Clone, Default)]
struct RequestGate {
    started: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
}

impl RequestGate {
    async fn hold(&self) {
        self.started.notify_one();
        self.release.notified().await;
    }

    async fn reached(&self, ui: &mut Ui<'_>) {
        tokio::time::timeout(Duration::from_secs(1), async {
            tokio::select! {
                _ = self.started.notified() => {}
                result = ui.transport.pending_mut().expect("request owned").call.as_mut() => {
                    panic!("request completed before gate release: {}", result.is_ok());
                }
            }
        })
        .await
        .expect("request must reach router");
    }
}

#[derive(Clone, Default)]
struct AttachedServer {
    metadata: RequestGate,
    send: RequestGate,
    cancel: RequestGate,
    requests: Arc<Mutex<Vec<String>>>,
    resumed_session: Arc<Mutex<String>>,
    unknown_send: bool,
    wrong_metadata_session: bool,
    fail_history: bool,
}

impl AttachedServer {
    fn remote(&self) -> Remote {
        let server = self.clone();
        let router = axum::Router::new().fallback(move |uri: axum::http::Uri| {
            let server = server.clone();
            async move {
                let path = uri.path();
                server.requests.lock().unwrap().push(path.to_string());
                match path {
                    "/v1/status" => axum::response::Response::builder()
                        .header("content-type", "text/event-stream")
                        .body(axum::body::Body::from("data: []\n\n")).unwrap(),
                    "/v1/sessions" => {
                        let session = server.resumed_session.lock().unwrap().clone();
                        axum::Json(serde_json::json!({"id": if session.is_empty() { "s1" } else { &session }})).into_response()
                    }
                    "/v1/sessions/s1" => {
                        server.metadata.hold().await;
                        axum::Json(serde_json::json!({
                            "id": if server.wrong_metadata_session { "s2" } else { "s1" },
                            "workspace":"/authoritative"
                        })).into_response()
                    }
                    "/v1/sessions/s1/history" if server.fail_history => axum::http::StatusCode::SERVICE_UNAVAILABLE.into_response(),
                    "/v1/sessions/s1/history" => axum::Json(serde_json::json!([])).into_response(),
                    "/v1/sessions/s1/tasks" => axum::Json(serde_json::json!({"tasks":[]})).into_response(),
                    "/v1/sessions/s1/approvals/message" => axum::Json(serde_json::Value::Null).into_response(),
                    "/v1/sessions/s1/turns" => {
                        server.send.hold().await;
                        let mut response = axum::response::Response::builder()
                            .header("content-type", "text/event-stream");
                        if !server.unknown_send {
                            response = response.header("Otto-Turn-Id", "accepted-turn");
                        }
                        response.body(axum::body::Body::empty()).unwrap()
                    }
                    "/v1/sessions/s1/turns/followed-turn/events" => axum::response::Response::builder()
                        .header("content-type", "text/event-stream")
                        .body(axum::body::Body::empty()).unwrap(),
                    "/v1/sessions/s1/turns/accepted-turn/cancel"
                    | "/v1/sessions/s1/turns/followed-turn/cancel" => {
                        server.cancel.hold().await;
                        axum::http::StatusCode::OK.into_response()
                    }
                    _ => axum::http::StatusCode::NOT_FOUND.into_response(),
                }
            }
        });
        let remote = Remote::new(Client::in_process(router), "/work");
        {
            let mut state = remote.state();
            state.session = "s1".into();
            state.info = Info {
                session_id: "s1".into(),
                workspace: "/work".into(),
                ..Info::default()
            };
        }
        remote
    }

    fn count(&self, suffix: &str) -> usize {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .filter(|path| path.ends_with(suffix))
            .count()
    }
}

async fn settle(ui: &mut Ui<'_>) {
    let result = tokio::time::timeout(
        Duration::from_secs(1),
        ui.transport
            .pending_mut()
            .expect("request owned")
            .call
            .as_mut(),
    )
    .await
    .expect("released request settles");
    ui.on_transport(result);
}

fn prompt(ui: &mut Ui<'_>, text: &str) {
    ui.actions.push_back(Action::Prompt(text.into()));
    assert!(!ui.run_actions());
}

fn cancel(ui: &mut Ui<'_>) {
    let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(80, 24)).unwrap();
    ui.on_input(
        &mut terminal,
        TuiEvent::Key(crossterm::event::KeyCode::Esc.into()),
    )
    .unwrap();
}

#[tokio::test]
async fn disconnect_discards_read_but_retains_send_and_cancel_until_settlement() {
    let server = AttachedServer::default();
    let remote = server.remote();
    let cancellation = CancellationToken::new();
    let mut ui = attached_ui(&remote, &cancellation).await;
    ui.needs_sync = true;
    ui.sync();
    server.metadata.reached(&mut ui).await;
    ui.disconnect();
    assert!(ui.transport.is_idle(), "replaceable read must be dropped");

    ui.connection = ConnectionState::Connected(test_status().await);
    prompt(&mut ui, "send once");
    server.send.reached(&mut ui).await;
    ui.disconnect();
    assert!(ui.transport.is_owned(), "disconnect retains admitted send");
    let requests = server.requests.lock().unwrap().len();
    ui.reconnect();
    assert_eq!(server.requests.lock().unwrap().len(), requests);
    assert!(ui.transport.is_owned(), "reconnect cannot replace send");
    server.send.release.notify_one();
    settle(&mut ui).await;
    assert_eq!(remote.own(), vec!["accepted-turn"]);
    assert!(!ui.connection.is_connected());
    assert!(
        ui.followed.is_none(),
        "send completion does not restore disconnected stream"
    );
    assert_eq!(server.count("/turns"), 1);

    ui.cancel_turn("accepted-turn".into());
    server.cancel.reached(&mut ui).await;
    ui.disconnect();
    assert!(
        ui.transport.is_owned(),
        "already disconnected still retains cancel"
    );
    server.cancel.release.notify_one();
    settle(&mut ui).await;
    assert!(ui.transport.is_idle());
    assert_eq!(server.count("/cancel"), 1);
}

#[tokio::test]
async fn reconnect_requires_complete_same_session_snapshot_and_preserves_draft() {
    let server = AttachedServer::default();
    let remote = server.remote();
    let cancellation = CancellationToken::new();
    let mut ui = attached_ui(&remote, &cancellation).await;
    ui.app.composer.input = "draft".chars().collect();
    ui.disconnect();
    ui.reconnect();
    server.metadata.reached(&mut ui).await;
    assert!(
        !ui.connection.is_connected(),
        "status headers alone cannot reconnect"
    );
    assert_eq!(remote.state().info.workspace, "/work");
    server.metadata.release.notify_one();
    settle(&mut ui).await;
    assert!(ui.connection.is_connected());
    assert!(ui.app.connected);
    assert_eq!(remote.state().info.workspace, "/authoritative");
    assert_eq!(ui.app.composer.input.iter().collect::<String>(), "draft");

    ui.disconnect();
    *server.resumed_session.lock().unwrap() = "other-session".into();
    ui.reconnect();
    settle(&mut ui).await;
    assert!(
        !ui.connection.is_connected(),
        "wrong resumed session cannot reconnect"
    );
    assert_eq!(remote.session(), "s1");
}

#[tokio::test]
async fn stale_reconnect_session_epoch_or_transcript_never_commits_snapshot() {
    for stale_scope in ["session", "epoch", "transcript"] {
        let server = AttachedServer::default();
        let remote = server.remote();
        let cancellation = CancellationToken::new();
        let mut ui = attached_ui(&remote, &cancellation).await;
        ui.disconnect();
        ui.reconnect();
        server.metadata.reached(&mut ui).await;
        match stale_scope {
            "session" => remote.state().session = "s2".into(),
            "epoch" => ui.epoch += 1,
            _ => ui.transcript += 1,
        }
        server.metadata.release.notify_one();
        settle(&mut ui).await;
        assert!(
            !ui.connection.is_connected(),
            "stale {stale_scope} cannot reconnect"
        );
        assert_eq!(
            remote.state().info.workspace,
            "/work",
            "stale {stale_scope} cannot commit metadata"
        );
        assert!(ui.followed.is_none());
    }
}

#[tokio::test]
async fn deferred_cancel_targets_accepted_turn_once_and_does_not_leak_to_next_send() {
    let server = AttachedServer::default();
    let remote = server.remote();
    let cancellation = CancellationToken::new();
    let mut ui = attached_ui(&remote, &cancellation).await;
    prompt(&mut ui, "first");
    server.send.reached(&mut ui).await;
    cancel(&mut ui);
    cancel(&mut ui);
    server.send.release.notify_one();
    settle(&mut ui).await;
    assert!(
        ui.transport.is_owned(),
        "send acceptance must produce owned cancel"
    );
    server.cancel.reached(&mut ui).await;
    cancel(&mut ui);
    server.cancel.release.notify_one();
    settle(&mut ui).await;
    assert_eq!(server.count("/cancel"), 1);
    ui.followed = None;
    prompt(&mut ui, "second");
    server.send.reached(&mut ui).await;
    server.send.release.notify_one();
    settle(&mut ui).await;
    assert!(
        ui.transport.is_idle(),
        "old cancellation cannot attach to a later send"
    );
    assert_eq!(server.count("/turns"), 2);
    assert_eq!(server.count("/cancel"), 1);
}

#[tokio::test]
async fn unknown_send_outcome_is_visible_without_automatic_replay() {
    let server = AttachedServer {
        unknown_send: true,
        ..AttachedServer::default()
    };
    let remote = server.remote();
    let cancellation = CancellationToken::new();
    let mut ui = attached_ui(&remote, &cancellation).await;
    prompt(&mut ui, "possibly accepted");
    server.send.reached(&mut ui).await;
    ui.disconnect();
    server.send.release.notify_one();
    settle(&mut ui).await;
    assert!(
        ui.app
            .entries
            .iter()
            .any(|entry| entry.raw.contains("outcome unknown"))
    );
    assert!(ui.transport.is_idle());
    ui.run_actions();
    ui.sync();
    assert!(
        ui.transport.is_idle(),
        "unknown mutation cannot schedule replay"
    );
    assert_eq!(server.count("/turns"), 1);
    assert!(ui.actions.is_empty());
}

#[tokio::test]
async fn exit_drain_settles_send_and_generated_cancel_before_returning() {
    let server = AttachedServer::default();
    let remote = server.remote();
    let cancellation = CancellationToken::new();
    let mut ui = attached_ui(&remote, &cancellation).await;
    prompt(&mut ui, "send before exit");
    server.send.reached(&mut ui).await;
    cancel(&mut ui);
    ui.actions.push_back(Action::Exit);
    assert!(ui.run_actions());
    {
        let drain = ui.drain_transport();
        tokio::pin!(drain);
        server.send.release.notify_one();
        tokio::time::timeout(Duration::from_secs(1), async {
            tokio::select! {
                _ = server.cancel.started.notified() => {}
                _ = &mut drain => panic!("drain returned before generated cancellation reached server"),
            }
        })
        .await
        .expect("drain must poll generated cancel");
        server.cancel.release.notify_one();
        tokio::time::timeout(Duration::from_secs(1), &mut drain)
            .await
            .expect("drain settles cancel");
    }
    assert_eq!(server.count("/turns"), 1);
    assert_eq!(server.count("/cancel"), 1);
}

#[tokio::test]
async fn reconnect_rejects_wrong_metadata_session_and_partial_snapshot() {
    for wrong_metadata_session in [false, true] {
        let server = AttachedServer {
            wrong_metadata_session,
            fail_history: !wrong_metadata_session,
            ..AttachedServer::default()
        };
        let remote = server.remote();
        let cancellation = CancellationToken::new();
        let mut ui = attached_ui(&remote, &cancellation).await;
        ui.disconnect();
        ui.reconnect();
        // A failed history can finish try_join before metadata is polled.
        server.metadata.release.notify_one();
        settle(&mut ui).await;
        assert!(
            !ui.connection.is_connected(),
            "incomplete or foreign snapshot cannot reconnect"
        );
        assert_eq!(remote.state().info.workspace, "/work");
        assert_eq!(remote.session(), "s1");
    }
}

#[tokio::test]
async fn accepted_send_from_old_session_or_epoch_cannot_adopt_stream() {
    for change_session in [false, true] {
        let server = AttachedServer::default();
        let remote = server.remote();
        let cancellation = CancellationToken::new();
        let mut ui = attached_ui(&remote, &cancellation).await;
        prompt(&mut ui, "old scope");
        server.send.reached(&mut ui).await;
        if change_session {
            remote.state().session = "s2".into();
        } else {
            ui.epoch += 1;
        }
        server.send.release.notify_one();
        settle(&mut ui).await;
        assert!(
            remote.own().is_empty(),
            "old acceptance cannot become current session ownership"
        );
        assert!(
            ui.followed.is_none(),
            "old acceptance cannot attach its stream"
        );
        assert!(ui.transport.is_idle());
    }
}

#[tokio::test]
async fn disconnected_exit_drain_cancels_accepted_send_once_without_reconnecting() {
    let server = AttachedServer::default();
    let remote = server.remote();
    let cancellation = CancellationToken::new();
    let mut ui = attached_ui(&remote, &cancellation).await;
    prompt(&mut ui, "send before connection loss");
    server.send.reached(&mut ui).await;
    cancel(&mut ui);
    cancel(&mut ui);
    ui.disconnect();
    ui.actions.push_back(Action::Exit);
    assert!(ui.run_actions());
    {
        let drain = ui.drain_transport();
        tokio::pin!(drain);
        server.send.release.notify_one();
        tokio::time::timeout(Duration::from_secs(1), async {
            tokio::select! {
                _ = server.cancel.started.notified() => {}
                _ = &mut drain => panic!("disconnected drain dropped deferred cancellation"),
            }
        })
        .await
        .expect("disconnected drain must poll generated cancel");
        assert_eq!(server.count("/turns/accepted-turn/cancel"), 1);
        server.cancel.release.notify_one();
        tokio::time::timeout(Duration::from_secs(1), &mut drain)
            .await
            .expect("disconnected drain must settle generated cancel");
    }
    assert!(
        ui.transport.is_idle(),
        "drain must settle send and cancel ownership"
    );
    assert!(!ui.connection.is_connected());
    assert!(!ui.app.connected);
    assert!(
        ui.followed.is_none(),
        "disconnected acceptance cannot adopt its stream"
    );
    assert_eq!(server.count("/turns"), 1);
    assert_eq!(server.count("/cancel"), 1);
    assert_eq!(server.count("/v1/status"), 0, "exit drain cannot reconnect");
}

#[tokio::test]
async fn deferred_cancel_preserves_followed_turn_priority_over_newly_accepted_turn() {
    let server = AttachedServer::default();
    let remote = server.remote();
    let cancellation = CancellationToken::new();
    let mut ui = attached_ui(&remote, &cancellation).await;
    let stream = remote
        .client
        .turn_events("s1", "followed-turn", None)
        .await
        .unwrap();
    ui.adopt_stream(stream);
    prompt(&mut ui, "queued behind followed turn");
    server.send.reached(&mut ui).await;
    cancel(&mut ui);
    server.send.release.notify_one();
    settle(&mut ui).await;
    server.cancel.reached(&mut ui).await;
    assert_eq!(server.count("/turns/followed-turn/cancel"), 1);
    assert_eq!(server.count("/turns/accepted-turn/cancel"), 0);
    assert_eq!(
        ui.followed.as_ref().unwrap().stream.turn_id,
        "followed-turn"
    );
    server.cancel.release.notify_one();
    settle(&mut ui).await;
    assert!(ui.transport.is_idle());
    assert_eq!(
        server.count("/cancel"),
        1,
        "deferred Esc cancels exactly the followed turn"
    );
    assert_eq!(
        remote.own(),
        vec!["accepted-turn"],
        "new send is accepted without stealing cancellation priority"
    );
}
