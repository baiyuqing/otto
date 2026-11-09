use super::*;

#[test]
fn task_control_errors_distinguish_old_serve_from_missing_session() {
    let unsupported = task_control_error(otto::client::Error::Http {
        status: 404,
        code: String::new(),
        message: String::new(),
    });
    let unsupported = serde_json::to_value(unsupported).unwrap();
    assert_eq!(unsupported["code"], METHOD_NOT_FOUND);
    assert!(
        unsupported["message"]
            .as_str()
            .unwrap()
            .contains("upgrade and restart")
    );

    let missing = task_control_error(otto::client::Error::Http {
        status: 404,
        code: "not_found".into(),
        message: "session not found".into(),
    });
    let missing = serde_json::to_value(missing).unwrap();
    assert_eq!(missing["code"], INTERNAL_ERROR);
    assert_eq!(missing["message"], "HTTP 404 not_found: session not found");
}

fn prompt(meta: Value) -> Value {
    json!({
        "sessionId": "s1",
        "prompt": [{"type":"text", "text":"plain user text"}],
        "_meta": meta,
    })
}

#[test]
fn channel_metadata_is_structured_and_never_changes_prompt_text() {
    let (session, text, channel) = parse_prompt(&prompt(json!({
        "otto": {"channel": "telegram"}
    })))
    .unwrap();
    assert_eq!(session, "s1");
    assert_eq!(text, "plain user text");
    assert_eq!(channel, Some(Channel::Telegram));
}

#[tokio::test]
async fn reloading_a_session_rebinds_notices_to_the_current_controller() {
    let workspace = tempfile::tempdir().unwrap();
    let sessions = tempfile::tempdir().unwrap();
    let first = Arc::new(otto::fixtures::controller(workspace.path(), sessions.path()).await);
    let second = Arc::new(otto::fixtures::controller(workspace.path(), sessions.path()).await);
    let id = first.info().session_id;
    let builder = Arc::clone(first.builder());
    let (out, mut frames) = mpsc::unbounded_channel();
    let connection = Arc::new(Connection {
        workspace: workspace.path().to_path_buf(),
        backend: Backend::Local(Box::new(Local {
            config: Config {
                runtime: otto::fixtures::initial_runtime(&builder),
                builder,
                workspace: workspace.path().to_path_buf(),
                sandbox: None,
            },
            sessions: Mutex::new(HashMap::from([(
                id.clone(),
                Arc::new(Session {
                    controller: Arc::clone(&first),
                    prompt: Arc::new(PromptSlot::default()),
                }),
            )])),
        })),
        out,
        pending: Mutex::new(HashMap::new()),
        next_request_id: AtomicU64::new(1),
        client_capabilities: Mutex::new(Value::Null),
        task_stop: CancellationToken::new(),
        notice_stop: CancellationToken::new(),
        watching_notices: Mutex::new(HashMap::new()),
        notice_ready: Mutex::new(HashMap::new()),
        wake_loops: Mutex::new(HashMap::new()),
    });
    connection.start_notice_watcher(&id).await;
    connection.notices_ready(&Ok(json!({"sessionId": id})));
    first.push_notice("before close");
    let frame = tokio::time::timeout(std::time::Duration::from_secs(2), frames.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        frame
            .pointer("/params/update/content/text")
            .and_then(Value::as_str),
        Some("before close")
    );
    let previous = connection
        .watching_notices
        .lock()
        .unwrap()
        .get(&id)
        .unwrap()
        .stop
        .clone();
    connection.start_notice_watcher(&id).await;
    assert!(
        !previous.is_cancelled(),
        "same controller keeps its subscription"
    );
    let Backend::Local(local) = &connection.backend else {
        unreachable!()
    };
    local.sessions.lock().unwrap().insert(
        id.clone(),
        Arc::new(Session {
            controller: Arc::clone(&second),
            prompt: Arc::new(PromptSlot::default()),
        }),
    );
    connection.start_notice_watcher(&id).await;
    connection.notices_ready(&Ok(json!({"sessionId": id})));
    assert!(
        previous.is_cancelled(),
        "retired controller subscription must stop"
    );
    first.push_notice("old controller");
    second.push_notice("new controller");
    let frame = tokio::time::timeout(std::time::Duration::from_secs(2), frames.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        frame
            .pointer("/params/update/content/text")
            .and_then(Value::as_str),
        Some("new controller")
    );
    connection.notice_stop.cancel();
    first.close().unwrap();
    second.close().unwrap();
}

#[tokio::test]
async fn idle_wake_checks_do_not_signal_slot_release() {
    let workspace = tempfile::tempdir().unwrap();
    let sessions = tempfile::tempdir().unwrap();
    let controller = otto::fixtures::controller(workspace.path(), sessions.path()).await;
    let session = Session {
        controller: Arc::new(controller),
        prompt: Arc::new(PromptSlot::default()),
    };
    let tasks = session.controller.subagent_tasks().expect("task registry");
    assert_eq!(tasks.pending(), 0);
    let mut released = session.prompt.released.subscribe();
    let stop = CancellationToken::new();
    for _ in 0..3 {
        assert!(!wake_turn(&session, &stop).await);
        assert!(
            !released.has_changed().unwrap(),
            "idle checks must stay asleep"
        );
        assert!(session.prompt.begin(&stop).is_some());
        session.prompt.end();
        // A real prompt completion still wakes the loop.
        assert!(released.has_changed().unwrap());
        released.borrow_and_update();
    }
}

#[tokio::test]
async fn a_prompt_is_admitted_when_the_wake_turn_ends_between_begin_and_wait() {
    let slot = PromptSlot::default();
    let parent = CancellationToken::new();
    let wake = slot.begin_wake(&parent).expect("wake turn claims the slot");
    assert!(
        slot.begin(&parent).is_none(),
        "the wake turn holds the slot"
    );
    // The wake turn ends before the prompt registers its wait.
    slot.end();
    drop(wake);
    let token = slot
        .wait_for_wake(&parent)
        .expect("a slot freed after begin failed admits the prompt");
    assert!(slot.claim_after_wake(&token).await);
    assert!(
        slot.begin(&parent).is_none(),
        "the admitted prompt holds the slot"
    );
}

#[tokio::test]
async fn attached_followup_admission_is_fifo_and_cancellation_keeps_its_turn() {
    let slot = Arc::new(PromptSlot::default());
    let parent = CancellationToken::new();
    let (first, first_token) = slot.begin_followup(&parent).unwrap();
    let (second, canceled) = slot.begin_followup(&parent).unwrap();
    let waiting_slot = Arc::clone(&slot);
    let waiting = tokio::spawn(async move {
        waiting_slot.wait_followup_admission(second).await;
        second
    });
    tokio::task::yield_now().await;
    assert!(
        !waiting.is_finished(),
        "second admission must wait for first"
    );

    assert_eq!(slot.cancel(), 0);
    assert!(first_token.is_cancelled());
    assert!(canceled.is_cancelled());
    slot.finish_followup_admission(first);
    assert_eq!(waiting.await.unwrap(), second);
    assert!(canceled.is_cancelled());
    slot.finish_followup_admission(second);
}

#[test]
fn input_id_metadata_is_optional_but_must_be_well_formed() {
    assert_eq!(parse_input_id(&json!({})).unwrap(), None);
    assert_eq!(
        parse_input_id(&json!({"_meta": {"otto": {"inputId": "chat:telegram:c1:7"}}})).unwrap(),
        Some("chat:telegram:c1:7".to_string())
    );
    assert!(parse_input_id(&json!({"_meta": {"otto": {"inputId": "bad id"}}})).is_err());
    assert!(parse_input_id(&json!({"_meta": {"otto": {"inputId": 7}}})).is_err());
}

#[test]
fn unknown_metadata_is_ignored_but_invalid_channel_is_rejected() {
    let (_, text, channel) =
        parse_prompt(&prompt(json!({ "other": {"channel": "slack"} }))).unwrap();
    assert_eq!(text, "plain user text");
    assert_eq!(channel, None);
    let error = parse_prompt(&prompt(json!({
        "otto": {"channel": "slack"}
    })))
    .unwrap_err();
    let code: i32 = error.code.into();
    assert_eq!(code, INVALID_PARAMS);
}

#[test]
fn chat_metadata_becomes_an_origin_only_when_valid_and_consistent() {
    let chat =
        |value: Value, channel| parse_origin(&prompt(json!({"otto": {"chat": value}})), channel);
    let telegram = Some(Channel::Telegram);
    assert_eq!(
        chat(json!("telegram:123"), None).unwrap().as_str(),
        "chat:telegram:123"
    );
    assert_eq!(
        chat(json!("telegram:oc:1"), None).unwrap().as_str(),
        "chat:telegram:oc:1"
    );
    assert!(chat(json!("telegram:123"), telegram).is_some());
    assert_eq!(chat(json!("telegram123"), None), None);
    assert_eq!(chat(json!("slack:1"), None), None);
    assert_eq!(chat(json!("telegram:"), None), None);
    assert_eq!(chat(json!(5), None), None);
    assert_eq!(parse_origin(&prompt(json!({})), None), None);
}

#[test]
fn channel_context_capability_is_advertised() {
    for local in [false, true] {
        let reply = initialize_result(local).unwrap();
        assert_eq!(
            reply["agentCapabilities"]["_meta"]["otto"]["channelContext"],
            true
        );
        assert_eq!(
            reply["agentCapabilities"]["_meta"]["otto"]["scheduleDelivery"],
            true
        );
    }
}

fn local_test_connection(
    workspace: &Path,
    controller: &Arc<Controller>,
) -> (Arc<Connection>, Arc<PromptSlot>, String) {
    let id = controller.info().session_id;
    let builder = Arc::clone(controller.builder());
    let slot = Arc::new(PromptSlot::default());
    let (out, _frames) = mpsc::unbounded_channel();
    let connection = Arc::new(Connection {
        workspace: workspace.to_path_buf(),
        backend: Backend::Local(Box::new(Local {
            config: Config {
                runtime: otto::fixtures::initial_runtime(&builder),
                builder,
                workspace: workspace.to_path_buf(),
                sandbox: None,
            },
            sessions: Mutex::new(HashMap::from([(
                id.clone(),
                Arc::new(Session {
                    controller: Arc::clone(controller),
                    prompt: Arc::clone(&slot),
                }),
            )])),
        })),
        out,
        pending: Mutex::new(HashMap::new()),
        next_request_id: AtomicU64::new(1),
        client_capabilities: Mutex::new(Value::Null),
        task_stop: CancellationToken::new(),
        notice_stop: CancellationToken::new(),
        watching_notices: Mutex::new(HashMap::new()),
        notice_ready: Mutex::new(HashMap::new()),
        wake_loops: Mutex::new(HashMap::new()),
    });
    (connection, slot, id)
}

#[tokio::test(flavor = "multi_thread")]
async fn memory_command_append_failure_is_protocol_error_without_stale_receipt_replay() {
    let workspace = tempfile::tempdir().unwrap();
    let sessions = tempfile::tempdir().unwrap();
    let controller = Arc::new(otto::fixtures::controller(workspace.path(), sessions.path()).await);
    assert!(controller.memory_command("memory", "list").is_err());
    let previous = controller.info().local_commands;
    assert_eq!(previous.len(), 1);
    let before = controller.history();
    controller.current_session().handle().close().unwrap();
    let (mut connection, _, session_id) = local_test_connection(workspace.path(), &controller);
    let (out, mut frames) = mpsc::unbounded_channel();
    Arc::get_mut(&mut connection).unwrap().out = out;
    let error = connection
        .prompt(
            &session_id,
            "/remember must not execute".into(),
            None,
            None,
            (None, None),
            &CancellationToken::new(),
        )
        .await
        .unwrap_err();
    let error = serde_json::to_value(error).unwrap();
    assert_eq!(error["code"], INTERNAL_ERROR);
    assert!(
        error["message"]
            .as_str()
            .unwrap()
            .contains("receipt persistence failed")
    );
    assert!(
        frames.try_recv().is_err(),
        "neither agent message nor old receipt notification"
    );
    assert_eq!(controller.info().local_commands, previous);
    assert_eq!(controller.history(), before);
}

#[test]
fn enqueue_ignores_an_input_id_that_is_already_queued() {
    let slot = PromptSlot::default();
    let queued = |input_id: Option<&str>| {
        slot.enqueue_with_provenance(
            "run".into(),
            (None, None),
            input_id.map(str::to_string),
            None,
        )
    };
    assert_eq!(queued(Some("in_1")), Ok(true));
    assert_eq!(queued(Some("in_1")), Ok(false));
    assert_eq!(queued(Some("in_2")), Ok(false));
    assert_eq!(queued(None), Ok(false));
    assert_eq!(queued(None), Ok(false));
    assert_eq!(
        slot.state.lock().unwrap().queue.len(),
        4,
        "in_1 once, in_2 once, two prompts without an input id"
    );
}

#[tokio::test]
async fn drain_skips_a_queued_prompt_whose_input_id_is_in_the_session() {
    let workspace = tempfile::tempdir().unwrap();
    let sessions = tempfile::tempdir().unwrap();
    let controller = Arc::new(otto::fixtures::controller(workspace.path(), sessions.path()).await);
    let (connection, slot, session_id) = local_test_connection(workspace.path(), &controller);
    otto_core::session::Session::append_input_if_absent(
        &controller.current_session(),
        "in_done",
        otto::fixtures::user("already delivered"),
    )
    .await
    .unwrap();
    assert!(controller.has_input("in_done"));
    let delivered = controller.history().len();
    assert_eq!(
        slot.enqueue_with_provenance("rerun".into(), (None, None), Some("in_done".into()), None),
        Ok(true)
    );
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        Arc::clone(&slot).drain_queue(connection, session_id),
    )
    .await
    .expect("the skipped prompt must not reach the model");
    assert_eq!(controller.history().len(), delivered);
    let state = slot.state.lock().unwrap();
    assert!(state.queue.is_empty() && state.is_idle());
    drop(state);
    controller.close().unwrap();
}

#[tokio::test]
async fn canceled_queue_releases_admission_when_its_worker_finishes() {
    let workspace = tempfile::tempdir().unwrap();
    let sessions = tempfile::tempdir().unwrap();
    let controller = Arc::new(otto::fixtures::controller(workspace.path(), sessions.path()).await);
    let (connection, slot, session_id) = local_test_connection(workspace.path(), &controller);
    let parent = CancellationToken::new();
    assert_eq!(
        slot.enqueue_with_provenance("canceled".into(), (None, None), None, None),
        Ok(true)
    );
    assert_eq!(slot.cancel(), 1);
    assert!(
        slot.begin(&parent).is_none(),
        "the queue worker still owns admission"
    );
    tokio::time::timeout(
        std::time::Duration::from_secs(2),
        Arc::clone(&slot).drain_queue(connection, session_id),
    )
    .await
    .expect("an empty canceled queue must finish without calling a provider");
    assert!(slot.state.lock().unwrap().is_idle());
    assert!(
        slot.begin_wake(&parent).is_some(),
        "the finished worker releases admission"
    );
    slot.end();
    assert!(slot.begin(&parent).is_some());
    controller.close().unwrap();
}

#[tokio::test]
async fn cancel_read_right_after_a_prompt_finds_its_token() {
    let workspace = tempfile::tempdir().unwrap();
    let sessions = tempfile::tempdir().unwrap();
    let controller = Arc::new(otto::fixtures::controller(workspace.path(), sessions.path()).await);
    let (connection, slot, session_id) = local_test_connection(workspace.path(), &controller);
    let mut dispatcher = Dispatcher {
        connection,
        scheduling: false,
        tasks: JoinSet::new(),
        stop: CancellationToken::new(),
    };
    // No await between the two lines: the spawned prompt task has not run.
    dispatcher.handle_line(
        &json!({"jsonrpc":"2.0","id":1,"method":"session/prompt","params":
            {"sessionId": session_id, "prompt":[{"type":"text","text":"work"}]}})
        .to_string(),
    );
    dispatcher.handle_line(
        &json!({"jsonrpc":"2.0","method":"session/cancel","params":{"sessionId": session_id}})
            .to_string(),
    );
    assert!(
        slot.active_cancel().is_cancelled(),
        "the cancel must reach the prompt admitted just before it"
    );
    dispatcher.tasks.abort_all();
    controller.close().unwrap();
}

#[test]
fn application_source_claim_never_replaces_confirmed_acp_ingress() {
    use otto_core::model::{InputSource, InputTransport};
    let facts = acp_input_provenance(
        &json!({"clientCapabilities": {"_meta": {"otto": {"inputSource": "connector"}}}}),
    );
    assert_eq!(facts.source, InputSource::Acp);
    assert_eq!(facts.transport, InputTransport::Acp);
    assert_eq!(facts.client_source, Some(InputSource::Connector));
    assert_eq!(acp_input_provenance(&json!({})).client_source, None);
    assert_eq!(
        acp_input_provenance(
            &json!({"clientCapabilities": {"_meta": {"otto": {"inputSource": "<system>"}}}})
        )
        .client_source,
        None
    );
}
