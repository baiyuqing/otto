use super::*;

mod program_status_tests;
mod structured_approvals;
mod transport_state;

#[tokio::test]
async fn delayed_session_snapshot_keeps_input_and_stream_live_and_cannot_erase_deltas() {
    use axum::response::IntoResponse;
    let started = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let router = axum::Router::new().fallback({
        let started = started.clone();
        let release = release.clone();
        move |uri: axum::http::Uri| {
            let started = started.clone();
            let release = release.clone();
            async move {
                match uri.path() {
                    "/v1/sessions/s1" => {
                        started.notify_one();
                        release.notified().await;
                        axum::Json(serde_json::json!({"id":"s1", "workspace":"/work"}))
                            .into_response()
                    }
                    "/v1/sessions/s1/tasks" => {
                        axum::Json(serde_json::json!({"tasks":[]})).into_response()
                    }
                    "/v1/sessions/s1/history" => axum::Json(serde_json::json!([])).into_response(),
                    "/v1/sessions/s1/turns/t1/events" => {
                        let event = WireEvent {
                            event_type: "text_delta".into(),
                            text: "live ".into(),
                            ..WireEvent::default()
                        };
                        let data = serde_json::to_string(&event).unwrap();
                        axum::response::Response::builder()
                            .header("content-type", "text/event-stream")
                            .body(axum::body::Body::from(format!(
                                "data: {data}\n\ndata: {data}\n\n"
                            )))
                            .unwrap()
                    }
                    _ => axum::http::StatusCode::NOT_FOUND.into_response(),
                }
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
    let cancel = CancellationToken::new();
    let mut ui = Ui {
        remote: &remote,
        app: App::new(&Backend::Attach(&remote)),
        cancel: &cancel,
        followed: None,
        connection: ConnectionState::Connected(test_status().await),
        rows: vec![row(Some("ok"), Some("old"))],
        last_row: None,
        last_done: None,
        fresh: false,
        compacting: None,
        transport: TransportState::Idle,
        epoch: 0,
        transcript: 0,
        needs_sync: false,
        actions: VecDeque::new(),
    };
    ui.sync();
    tokio::select! {
        result = ui.transport.pending_mut().unwrap().call.as_mut() => panic!("snapshot completed before release: {}", result.is_ok()),
        _ = started.notified() => {}
    }
    let stream = remote.client.turn_events("s1", "t1", None).await.unwrap();
    ui.adopt_stream(stream);
    let (keys, mut received) = mpsc::channel(1);
    keys.send(TuiEvent::Paste("draft".into())).await.unwrap();
    let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(80, 24)).unwrap();
    let mut input = false;
    let mut frames = 0;
    while !input || frames < 2 {
        let event = tokio::time::timeout(Duration::from_secs(1), async {
                tokio::select! {
                    result = ui.transport.pending_mut().unwrap().call.as_mut() => Ev::Transport(result),
                    event = received.recv(), if !input => Ev::Input(event),
                    frame = ui.followed.as_mut().unwrap().stream.next(), if frames < 2 => Ev::Frame(frame),
                }
            }).await.expect("input and frames must not await the session query");
        match event {
            Ev::Input(Some(event)) => {
                ui.on_input(&mut terminal, event).await.unwrap();
                input = true;
            }
            Ev::Frame(frame) => {
                let entries = ui.app.entries.len();
                ui.on_frame(frame);
                frames += 1;
                if frames == 2 {
                    assert_eq!(
                        ui.app.entries.len(),
                        entries,
                        "a delta updates an existing entry"
                    );
                }
            }
            _ => panic!("blocked query completed unexpectedly"),
        }
    }
    assert_eq!(ui.app.composer.input.iter().collect::<String>(), "draft");
    assert!(ui.app.entries.iter().any(|entry| entry.raw == "live live "));
    release.notify_one();
    let result = ui.transport.pending_mut().unwrap().call.as_mut().await;
    ui.on_transport(result);
    assert!(
        ui.app.entries.iter().any(|entry| entry.raw == "live live "),
        "stale history must not overwrite received deltas"
    );
    assert!(ui.needs_sync);
}

#[tokio::test]
async fn the_channel_sink_forwards_text_and_fails_when_the_ui_is_gone() {
    let (tx, mut rx) = mpsc::channel(1);
    let sink = ChannelSink(tx);
    sink.deliver(&Origin::terminal(), "⏰ stretch")
        .await
        .unwrap();
    assert_eq!(rx.recv().await.as_deref(), Some("⏰ stretch"));
    drop(rx);
    assert!(sink.deliver(&Origin::terminal(), "x").await.is_err());
}

#[tokio::test]
async fn reconnect_restores_pending_input_in_visible_main_conversation() {
    use axum::response::IntoResponse;
    let router = axum::Router::new().fallback(|uri: axum::http::Uri| async move {
        match uri.path() {
            "/v1/status" => axum::response::Response::builder()
                .header("content-type", "text/event-stream")
                .body(axum::body::Body::from("data: []\n\n"))
                .unwrap(),
            "/v1/sessions" | "/v1/sessions/s1" =>
                axum::Json(serde_json::json!({"id":"s1", "workspace":"/work"})).into_response(),
            "/v1/sessions/s1/history" => axum::Json(serde_json::json!([])).into_response(),
            "/v1/sessions/s1/tasks" => axum::Json(serde_json::json!({"tasks":[]})).into_response(),
            "/v1/sessions/s1/tasks/steer" => axum::Json(serde_json::json!([{
                "id": 1, "text": "reconnected pending guidance", "status": "queued", "input_id": "input-1"
            }])).into_response(),
            _ => axum::http::StatusCode::NOT_FOUND.into_response(),
        }
    });
    let remote = Arc::new(Remote::new(Client::in_process(router), "/work"));
    {
        let mut state = remote.state();
        state.session = "s1".into();
        state.info = Info {
            session_id: "s1".into(),
            workspace: "/work".into(),
            ..Default::default()
        };
    }
    let cancel = CancellationToken::new();
    let mut app = App::new(&Backend::Attach(&remote));
    app.driver = Some(super::super::backend::Driver::new(
        super::super::backend::Connection::Attach(Arc::clone(&remote)),
    ));
    let mut ui = Ui {
        remote: &remote,
        app,
        cancel: &cancel,
        followed: None,
        connection: ConnectionState::Disconnected,
        rows: Vec::new(),
        last_row: None,
        last_done: None,
        fresh: false,
        compacting: None,
        transport: TransportState::Idle,
        epoch: 0,
        transcript: 0,
        needs_sync: false,
        actions: VecDeque::new(),
    };
    ui.reconnect();
    let result = ui
        .transport
        .pending_mut()
        .expect("reconnect call")
        .call
        .as_mut()
        .await;
    ui.on_transport(result);
    assert!(
        ui.connection.is_connected(),
        "authoritative reconnect succeeds"
    );
    let completion = tokio::time::timeout(Duration::from_secs(1), ui.app.next_completion())
        .await
        .expect("reconnect must schedule a queue read");
    ui.app.apply_completion(completion);
    let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(100, 32)).unwrap();
    terminal.draw(|frame| render::draw(frame, &ui.app)).unwrap();
    let buffer = terminal.backend().buffer();
    let rows: Vec<String> = (0..32)
        .map(|y| {
            (0..100)
                .filter_map(|x| buffer.cell((x, y)))
                .map(|cell| cell.symbol())
                .collect()
        })
        .collect();
    let main = rows
        .into_iter()
        .take_while(|row| !row.contains("◆ QUEUED") && !row.contains('╭'))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        main.contains("[Queued for current turn] reconnected pending guidance"),
        "reconnect must restore pending text in main conversation, not only a banner: {main}"
    );
    assert!(
        !ui.app
            .entries
            .iter()
            .any(|entry| entry.raw.contains("pending guidance")),
        "reconnect cannot persist pending input as delivered history"
    );
}

fn row(turn: Option<&str>, turn_id: Option<&str>) -> StatusRow {
    StatusRow {
        id: "s1".into(),
        turn: turn.map(str::to_string),
        turn_id: turn_id.map(str::to_string),
        ..StatusRow::default()
    }
}

#[test]
fn follow_decision_follows_a_running_turn_once() {
    let own = vec!["t-own".to_string()];
    let cases = [
        // Another client's running turn is followed with its history.
        (
            row(Some("running"), Some("t-x")),
            None,
            None,
            Some(Follow::Foreign("t-x".into())),
        ),
        // This process's queued turn that has started running.
        (
            row(Some("running"), Some("t-own")),
            None,
            None,
            Some(Follow::Own("t-own".into())),
        ),
        // Already following a turn.
        (row(Some("running"), Some("t-x")), Some("t-y"), None, None),
        // The turn that just ended can still be named by the row.
        (row(Some("running"), Some("t-x")), None, Some("t-x"), None),
        // Not running.
        (row(Some("queued"), Some("t-own")), None, None, None),
        (row(None, None), None, None, None),
        // A turn that started and ended between two snapshots.
        (
            row(Some("error"), Some("t-new")),
            None,
            Some("t-old"),
            Some(Follow::Reload("t-new".into())),
        ),
        (row(Some("ok"), Some("t-x")), None, Some("t-x"), None),
        (row(Some("ok"), Some("t-x")), Some("t-y"), None, None),
    ];
    for (row, followed, last_done, want) in cases {
        assert_eq!(
            follow_decision(&row, followed, &own, last_done),
            want,
            "{row:?}"
        );
    }
}

/// A real stream keeps connected fixtures faithful to the transport contract.
async fn test_status() -> StatusStream {
    let router = axum::Router::new().fallback(|| async {
        axum::response::Response::builder()
            .header("content-type", "text/event-stream")
            .body(axum::body::Body::from("data: []\n\n"))
            .unwrap()
    });
    Client::in_process(router).status().await.unwrap()
}

async fn attached_ui<'a>(remote: &'a Remote, cancel: &'a CancellationToken) -> Ui<'a> {
    Ui {
        remote,
        app: App::new(&Backend::Attach(remote)),
        cancel,
        followed: None,
        connection: ConnectionState::Connected(test_status().await),
        rows: Vec::new(),
        last_row: None,
        last_done: None,
        fresh: false,
        compacting: None,
        transport: TransportState::Idle,
        epoch: 0,
        transcript: 0,
        needs_sync: false,
        actions: VecDeque::new(),
    }
}
