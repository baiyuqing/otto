use super::*;

#[tokio::test]
async fn workflow_tools_and_durable_mailbox_exist_with_conversational_agents_disabled() {
    let (dir, _) = crate::workflow::conversation_tests::fixture();
    let mut builder = builder(dir.path());
    builder.shared_mut().config = toml::from_str("[agents]\nenabled = false\n").unwrap();
    let source = crate::workflow::conversation_tests::session("source");
    let runner = builder.build_runner(&source, &runtime()).await.unwrap();
    assert!(runner.tasks().is_none());
    for name in crate::tool::workflow::NAMES {
        assert!(tool_names(&runner).contains(&name.to_string()));
    }
    let controller = builder.workflows.get().unwrap();
    assert!(
        controller.inspect("documentation-review").is_ok(),
        "internal workflow agents must still discover named definitions"
    );
    let mut updates = crate::runtime::workflow_notifications::updates(runner.mailbox().as_ref());
    runner
        .mailbox()
        .enqueue(otto_core::mailbox::Notification {
            id: "workflow:test:1".into(),
            text: "controlled lifecycle notice".into(),
            ..Default::default()
        })
        .unwrap();
    assert!(updates.has_changed().unwrap());
    updates.borrow_and_update();
    let app = crate::app::Controller::new(builder, true, source, runner, Default::default());
    assert!(
        app.prepare_wake().unwrap().is_some(),
        "mailbox wake admission must not require Tasks"
    );
    app.close_workflows().await;
    app.close().unwrap();
}

#[tokio::test]
async fn live_saved_workflow_human_gate_terminal_wake_reopen_and_new_isolation() {
    use crate::fixtures::subagent_doubles::{
        FakeProvider, assistant_text, assistant_tool_call, match_any,
    };
    use crate::workflow::{RunStatus, WorkflowStore};
    use otto_core::mailbox::Mailbox;
    use otto_core::model::Usage;
    let (dir, catalog) = crate::workflow::conversation_tests::fixture();
    let store = Arc::new(crate::workflow::Store::open(&dir.path().join("flow.db")).unwrap());
    let owner =
        crate::workflow::conversation_tests::controller(store.clone(), catalog, "/w", dir.path());
    let approvals = Arc::new(crate::approval::Service::in_memory().unwrap());
    owner.attach_approvals(&approvals).unwrap();
    let source = SharedSession::new(Arc::new(
        crate::session::Store::create(
            dir.path().join("sessions"),
            Header {
                id: "0123456789abcdef0123456789abcdef".into(),
                workspace: "/w".into(),
                provider: "openai-compatible".into(),
                model: "test-model".into(),
                created_at: Utc::now(),
                ..Default::default()
            },
        )
        .unwrap(),
    ));
    let source_id = source.header().id;
    let path = source.path();
    let weak = Arc::downgrade(source.handle());
    let provider = FakeProvider::new();
    provider.add_route(
        match_any,
        vec![
            assistant_tool_call(
                "start",
                "workflow_start",
                r#"{"name":"documentation-review","input":"scope"}"#,
                Usage::default(),
            ),
            assistant_text("Waiting for human acceptance.", Usage::default()),
            assistant_text("Lifecycle received.", Usage::default()),
            assistant_text("Completed.", Usage::default()),
            assistant_text("Reopened.", Usage::default()),
        ],
    );
    let tasks = Arc::new(crate::subagent::tasks::Tasks::new());
    // A binding is held by the session's resources, so they exist before it.
    source.install_test_resources(
        crate::runtime::session_resources::SessionResources::for_tests(
            &source_id,
            Some(Arc::clone(&tasks)),
        ),
    );
    let inbox = tasks.notifications_inbox().clone();
    let sidecar = Path::new(&path).with_extension("inbox.json");
    let sidecar_path = sidecar.clone();
    crate::runtime::sidecar_mailbox::install(&inbox, ".inbox.json", move || {
        Some(sidecar_path.clone())
    });
    let mailbox: Arc<dyn Mailbox> = inbox.clone();
    let mut changes = crate::runtime::workflow_notifications::updates(mailbox.as_ref());
    let old_mailbox: Arc<dyn Mailbox> = Arc::new(otto_core::mailbox::Inbox::default());
    let old_binding =
        crate::runtime::workflow_notifications::bind(&owner, source.clone(), &old_mailbox);
    let binding = crate::runtime::workflow_notifications::bind(&owner, source.clone(), &mailbox);
    tokio::time::timeout(Duration::from_secs(2), old_binding.join())
        .await
        .unwrap();
    let parent = crate::tool::workflow::Parent::new(Ok(owner.clone()), source_id.clone(), true);
    let runner = Runner::scripted_with_tools(source.clone(), provider, tasks, parent.tools());
    runner
        .run("Review scope", &mut |_| {}, &CancellationToken::new())
        .await
        .unwrap();
    let run = store.origin_runs("/w", &source_id).unwrap().pop().unwrap();
    assert_eq!(owner.wait(&run).await.unwrap().status, RunStatus::Waiting);
    assert!(
        old_mailbox.is_empty(),
        "replaced adapter must not write to retained mailbox"
    );
    let domain = owner.requests(&run).unwrap().pop().unwrap();
    let request = crate::approval::workflow::publish(&approvals, "/w", &domain).unwrap();
    assert_eq!(request.state, otto_core::approval::State::Pending);
    assert!(request.decision.is_none(), "no human decision exists yet");
    assert!(
        !owner
            .events(&run, 0)
            .unwrap()
            .iter()
            .any(|e| e.kind == "run_succeeded")
    );
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if inbox
                .snapshot()
                .iter()
                .any(|n| n.text.contains("run_waiting"))
            {
                break;
            }
            changes.changed().await.unwrap();
        }
    })
    .await
    .unwrap();
    let app = crate::app::Controller::new(
        builder(dir.path()),
        true,
        source.clone(),
        runner,
        Default::default(),
    );
    app.prepare_wake()
        .unwrap()
        .unwrap()
        .run(&mut |_| {}, &CancellationToken::new())
        .await
        .unwrap();
    // Real test-human action through the existing shared authority, not store.respond.
    approvals
        .decide(
            &request.id,
            &request.scope,
            request.revision,
            true,
            "accepted by test human",
            &crate::approval::Actor::local("test-human"),
        )
        .unwrap();
    approvals.apply_available("/w").unwrap();
    assert_eq!(owner.wait(&run).await.unwrap().status, RunStatus::Succeeded);
    let terminal = owner
        .events(&run, 0)
        .unwrap()
        .into_iter()
        .find(|e| e.kind == "run_succeeded")
        .unwrap();
    let terminal_id = format!("workflow:{run}:{}", terminal.seq);
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if inbox.snapshot().iter().any(|n| n.id == terminal_id) {
                break;
            }
            changes.changed().await.unwrap();
        }
    })
    .await
    .unwrap();
    // Crash boundary: terminal is durable in sidecar but not yet in transcript.
    assert!(!source.has_input(&terminal_id));
    binding.join().await;
    let restored = otto_core::mailbox::Inbox::default();
    assert!(crate::runtime::sidecar_mailbox::load(&restored, &sidecar).is_none());
    assert!(restored.snapshot().iter().any(|n| n.id == terminal_id));
    app.prepare_wake()
        .unwrap()
        .unwrap()
        .run(&mut |_| {}, &CancellationToken::new())
        .await
        .unwrap();
    assert!(source.has_input(&terminal_id));
    let count = source
        .messages()
        .iter()
        .filter(|m| m.text().contains(&terminal_id))
        .count();
    assert_eq!(count, 1);
    // Append-before-ack: restore the prior persisted notification into the live inbox.
    inbox.load(restored.queued());
    app.prepare_wake()
        .unwrap()
        .unwrap()
        .run(&mut |_| {}, &CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(
        source
            .messages()
            .iter()
            .filter(|m| m.text().contains(&terminal_id))
            .count(),
        1
    );
    // /new closes exact source while old mailbox and controller Arcs remain retained.
    source.close().unwrap();
    let old_len = inbox.len();
    let outstanding = owner
        .start_parent("documentation-review", "later", &source_id, "later")
        .await
        .unwrap();
    owner.wait(&outstanding.id).await.unwrap();
    let replacement = crate::workflow::conversation_tests::session("replacement");
    install_resources(&replacement);
    let replacement_inbox: Arc<dyn Mailbox> = Arc::new(otto_core::mailbox::Inbox::default());
    let replacement_binding = crate::runtime::workflow_notifications::bind(
        &owner,
        replacement.clone(),
        &replacement_inbox,
    );
    tokio::time::sleep(Duration::from_millis(450)).await;
    assert_eq!(
        inbox.len(),
        old_len,
        "closed source must not write despite retained old Arcs"
    );
    assert!(
        replacement_inbox.is_empty(),
        "/new must never redirect old-source events"
    );
    let (reopened, warnings) = crate::session::Store::open(&path).unwrap();
    assert!(warnings.is_empty());
    let reopened = SharedSession::new(Arc::new(reopened));
    install_resources(&reopened);
    assert!(reopened.has_input(&terminal_id));
    let catchup = Arc::new(otto_core::mailbox::Inbox::default());
    let catchup_mailbox: Arc<dyn Mailbox> = catchup.clone();
    let catchup_binding =
        crate::runtime::workflow_notifications::bind(&owner, reopened.clone(), &catchup_mailbox);
    tokio::time::timeout(Duration::from_secs(5), async {
        while catchup.is_empty() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    assert!(
        catchup
            .snapshot()
            .iter()
            .all(|n| n.task_id == outstanding.id)
    );
    tokio::time::timeout(Duration::from_secs(2), owner.close())
        .await
        .unwrap();
    catchup_binding.join().await;
    replacement_binding.join().await;
    app.close().unwrap();
    drop(app);
    drop(source);
    tokio::time::timeout(Duration::from_secs(2), async {
        while weak.upgrade().is_some() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("stopped adapter must release saved session even with old mailbox/controller retained");
    reopened.close().unwrap();
}

/// Gives `session` resources, which hold its workflow delivery worker.
fn install_resources(session: &SharedSession) {
    session.install_test_resources(
        crate::runtime::session_resources::SessionResources::for_tests(
            &session.header().id,
            Some(Arc::new(crate::subagent::tasks::Tasks::new())),
        ),
    );
}

#[tokio::test]
async fn workflow_controller_slot_is_shared_across_parents_and_retains_lock() {
    let (dir, catalog) = crate::workflow::conversation_tests::fixture();
    let builder = builder(dir.path());
    let store = Arc::new(crate::workflow::Store::open_in_memory());
    let controller = crate::workflow::conversation_tests::controller(
        store,
        catalog,
        &builder.workspace_path,
        dir.path(),
    );
    let lock =
        crate::runtime::workflow::lock_workspace(&builder.home, &builder.workspace_path).unwrap();
    controller.set_guard(Box::new(lock));
    builder.workflows.set(controller.clone()).ok().unwrap();
    let first = crate::runtime::workflow::shared_controller(&builder, &runtime(), &mut Vec::new())
        .await
        .unwrap();
    let second = crate::runtime::workflow::shared_controller(&builder, &runtime(), &mut Vec::new())
        .await
        .unwrap();
    assert!(Arc::ptr_eq(&first, &second));
    assert!(Arc::ptr_eq(&first, &controller));
    assert!(
        crate::runtime::workflow::lock_workspace(&builder.home, &builder.workspace_path).is_err()
    );
    crate::runtime::workflow::close(&builder).await;
    // Closing joins work, but clones retain the one ownership guard.
    drop(builder);
    drop(controller);
    drop(first);
    assert!(
        crate::runtime::workflow::lock_workspace(&dir.path().to_string_lossy(), &second.workspace)
            .is_err()
    );
    drop(second);
}

#[tokio::test]
async fn workflow_controller_error_is_not_cached_after_the_lock_is_released() {
    let dir = tempfile::tempdir().expect("temp dir");
    let builder = builder(dir.path());
    let held =
        crate::runtime::workflow::lock_workspace(&builder.home, &builder.workspace_path).unwrap();
    let blocked =
        crate::runtime::workflow::shared_controller(&builder, &runtime(), &mut Vec::new()).await;
    assert!(blocked.is_err());
    drop(held);
    let retried =
        crate::runtime::workflow::shared_controller(&builder, &runtime(), &mut Vec::new()).await;
    assert!(retried.is_ok(), "{:?}", retried.err());
}

#[tokio::test]
async fn parent_tool_factory_is_fresh_per_parent_runner_and_absent_from_children() {
    let dir = tempfile::tempdir().expect("temp dir");
    let builder = builder(dir.path());
    let factory = Arc::new(CountingParentToolFactory {
        calls: Arc::new(AtomicUsize::new(0)),
        contexts: Mutex::new(Vec::new()),
    });
    builder.shared.parent_tool_factory.install(factory.clone());
    let first = SharedSession::memory(Header {
        id: "session-one".into(),
        ..Header::default()
    });
    let second = SharedSession::memory(Header {
        id: "session-two".into(),
        ..Header::default()
    });

    let first_runner = builder
        .build_runner(&first, &runtime())
        .await
        .expect("first runner");
    let second_runner = builder
        .build_runner(&second, &runtime())
        .await
        .expect("second runner");
    assert!(tool_names(&first_runner).contains(&"parent_factory_tool".into()));
    assert!(tool_names(&second_runner).contains(&"parent_factory_tool".into()));
    assert_eq!(factory.calls.load(Ordering::SeqCst), 2);
    assert_eq!(
        factory
            .contexts
            .lock()
            .unwrap()
            .iter()
            .map(|context| context.session_id.as_str())
            .collect::<Vec<_>>(),
        ["session-one", "session-two"]
    );

    let child_names = builder
        .child_tools(&runtime(), 65536, &[], &Catalog::default(), "session-x")
        .expect("child tools")
        .into_iter()
        .map(|tool| tool.definition().name)
        .collect::<Vec<_>>();
    assert!(!child_names.contains(&"parent_factory_tool".into()));
    assert_eq!(factory.calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn a_parent_agent_tool_round_finishes_before_the_child_and_wakes_later() {
    use crate::fixtures::subagent_doubles::{
        FakeProvider, assistant_text, assistant_tool_call, match_any, test_config, wait_status,
    };
    use crate::subagent::runner::Runner as ChildRunner;
    use crate::subagent::tasks::{TaskStatus, Tasks};
    use otto_core::agent::inbox::NotificationKind;
    use otto_core::model::{BlockType, Usage};

    let tasks = Arc::new(Tasks::new());
    let release = CancellationToken::new();
    let child_provider = FakeProvider::new();
    let child_release = release.clone();
    child_provider.set_hook(Arc::new(move |cancel, _request| {
        let release = child_release.clone();
        Box::pin(async move {
            tokio::select! {
                () = release.cancelled() => {}
                () = cancel.cancelled() => {}
            }
        })
    }));
    child_provider.add_route(
        match_any,
        vec![assistant_text("child done", Usage::default())],
    );
    let (children, _) =
        ChildRunner::new(test_config(&child_provider, &tasks, Vec::new())).expect("child runner");
    let children = Arc::new(children);

    let parent_provider = FakeProvider::new();
    parent_provider.add_route(
        match_any,
        vec![
            assistant_tool_call(
                "call-agent",
                "agent",
                r#"{"prompt":"inspect it"}"#,
                Usage::default(),
            ),
            assistant_text("parent released", Usage::default()),
        ],
    );
    let session = SharedSession::memory(Header::default());
    let parent = Runner::scripted_with_tools(
        session.clone(),
        parent_provider.clone() as Arc<dyn Provider + Send + Sync>,
        Arc::clone(&tasks),
        crate::subagent::tools::tools(&children),
    );

    tokio::time::timeout(
        Duration::from_secs(1),
        parent.run("delegate", &mut |_| {}, &CancellationToken::new()),
    )
    .await
    .expect("parent turn blocked on the child")
    .expect("parent turn");
    assert_eq!(parent_provider.requests().len(), 2);
    wait_status(&tasks, "t1", TaskStatus::Running).await;
    assert!(session.messages().iter().any(|message| {
        message.blocks.iter().any(|block| {
            block.block_type == BlockType::ToolResult
                && block.tool_call_id == "call-agent"
                && block.text.contains("task t1")
        })
    }));

    release.cancel();
    wait_status(&tasks, "t1", TaskStatus::Succeeded).await;
    let notifications = tasks.notifications().snapshot();
    assert_eq!(notifications.len(), 1, "{notifications:?}");
    assert_eq!(notifications[0].kind, Some(NotificationKind::TaskFinished));
}
