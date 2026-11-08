//! Delivery manager, receipt and tool-factory installation tests.

use super::*;

#[tokio::test]
async fn delivery_factory_preflight_and_apply_bind_source_before_effect_capability() {
    use otto::runtime::builder::ParentToolFactory;

    struct GitReplies(Mutex<Vec<String>>);
    #[async_trait]
    impl otto::sandbox::CommandExecutor for GitReplies {
        async fn execute(
            &self,
            _request: otto::sandbox::Request,
            streams: otto::sandbox::Streams<'_>,
            _cancel: &CancellationToken,
        ) -> (otto::sandbox::ExitStatus, Result<(), otto::sandbox::Error>) {
            let output = self.0.lock().unwrap().remove(0);
            let _ = streams.stdout.write_all(output.as_bytes());
            (otto::sandbox::ExitStatus::default(), Ok(()))
        }
    }

    let home = tempfile::tempdir().expect("home");
    let target = tempfile::tempdir().expect("target");
    let target_path = canonical_directory(target.path()).expect("canonical target");
    let git_dir = target_path.join(".git");
    std::fs::create_dir(&git_dir).expect("git directory");
    let target_text = format!("{}\n", target_path.display());
    let git_text = format!("{}\n", git_dir.display());
    let head = format!("{}\n", "a".repeat(40));
    let replies = Arc::new(GitReplies(Mutex::new(vec![
        target_text.clone(),
        git_text.clone(),
        git_text.clone(),
        head.clone(),
        target_text,
        git_text.clone(),
        git_text,
        head,
    ])));
    let shared = otto::fixtures::shared_with_offline_sandbox(home.path());
    let mut builder = otto::fixtures::builder(&target_path, home.path());
    builder.command_executor = Some(replies as Arc<dyn otto::sandbox::CommandExecutor>);
    builder.sandbox_environment = Some(Vec::new());
    let host = Arc::new(WorkspaceHost {
        builder: Arc::new(builder),
        workflows: None,
        delivery_setup: None,
        reloader: None,
    });
    let workspaces = Arc::new(Workspaces {
        startup: target_path.to_string_lossy().into_owned(),
        startup_host: Arc::clone(&host),
        roots: Vec::new(),
        config_path: shared.config_path.clone(),
        loaded: tokio::sync::Mutex::new(BTreeMap::from([(
            target_path.to_string_lossy().into_owned(),
            host,
        )])),
    });
    let runtime = otto::fixtures::initial_runtime(&workspaces.startup_host.builder);
    let manager = Arc::new(
        DeliveryManager::open(
            &home.path().to_string_lossy(),
            &shared,
            runtime,
            CancellationToken::new(),
        )
        .expect("manager"),
    );
    manager.attach_workspaces(&workspaces);
    let factory = DeliveryToolFactory::new(Arc::clone(&manager));
    let source = otto::runtime::builder::SharedSession::memory(otto_core::session::Header {
        id: "source-session".into(),
        workspace: "/source".into(),
        ..otto_core::session::Header::default()
    });
    let context = otto::runtime::builder::ParentToolContext {
        workspace: "/source".into(),
        session_id: "source-session".into(),
        source_session: source,
    };
    let tool = factory
        .tools(&context)
        .expect("tool")
        .pop()
        .expect("delivery tool");
    let arguments = otto::tool::testutil::raw(&format!(
        r#"{{"target":"{}","objective":"ship fixture"}}"#,
        target_path.to_string_lossy()
    ));
    let spec = tool
        .approval_spec_async(&arguments, &CancellationToken::new())
        .await
        .expect("preflight")
        .expect("approval spec");
    let approval = approval_for_delivery("approval-1", &spec);
    let error = tool
        .apply_approval(&arguments, &CancellationToken::new(), &approval)
        .await
        .expect_err("no elevated effect capability in fixture");
    assert!(error.contains("constrained worktree setup capability"));
    assert!(
        manager
            .service
            .source_sessions
            .lock()
            .unwrap()
            .contains_key("approval-1")
    );
    assert!(
        manager
            .receipts
            .get("approval-1")
            .expect("receipt read")
            .is_some()
    );
    manager.shutdown().await;
}

#[tokio::test]
async fn delivery_manager_owns_private_receipts_status_and_shutdown() {
    let home = tempfile::tempdir().expect("home");
    let startup = tempfile::tempdir().expect("startup");
    let startup_path = canonical_directory(startup.path()).expect("canonical");
    let sessions = tempfile::tempdir().expect("sessions");
    let startup_host = dummy_host(&startup_path, sessions.path());
    let shared = Arc::clone(&startup_host.builder.shared);
    let workspaces = Arc::new(Workspaces {
        startup: startup_path.to_string_lossy().into_owned(),
        startup_host,
        roots: Vec::new(),
        config_path: PathBuf::new(),
        loaded: tokio::sync::Mutex::new(BTreeMap::new()),
    });
    let runtime = otto::fixtures::initial_runtime(&workspaces.startup_host.builder);
    let manager = DeliveryManager::open(
        &home.path().to_string_lossy(),
        &shared,
        runtime,
        CancellationToken::new(),
    )
    .expect("manager");
    manager.attach_workspaces(&workspaces);
    let source = otto::runtime::builder::SharedSession::memory(otto_core::session::Header {
        id: "source-session".into(),
        workspace: "/source".into(),
        ..otto_core::session::Header::default()
    });
    manager
        .bind_source_session("approval-1", source)
        .expect("bind source");
    assert!(
        manager
            .service
            .source_sessions
            .lock()
            .unwrap()
            .contains_key("approval-1")
    );
    let binding = otto::delivery::ReceiptBinding {
        approval_id: "approval-1".into(),
        source_workspace: "/source".into(),
        target_repository: startup_path.to_string_lossy().into_owned(),
        target_common_dir: startup_path.join(".git").to_string_lossy().into_owned(),
        worktree: startup_path
            .join(".otto/delivery-worktrees/delivery-1")
            .to_string_lossy()
            .into_owned(),
        branch: "agent/test".into(),
        base: "a".repeat(40),
        handoff_digest: "b".repeat(64),
    };
    manager.receipts.prepare(binding).expect("receipt");
    manager
        .receipts
        .begin_task(
            "approval-1",
            "source-session",
            "target-session",
            &startup_path.join(".otto/delivery-worktrees/delivery-1"),
        )
        .expect("task");
    assert_eq!(
        manager
            .status("source-session", "approval-1")
            .expect("status")
            .expect("task")
            .state,
        otto::delivery::TaskState::Running
    );
    assert!(home.path().join(".otto/delivery-receipts.db").is_file());
    // The process that ran the task has exited by the time of the restart.
    let mut exited = std::process::Command::new("true").spawn().expect("spawn");
    let exited_pid = i64::from(exited.id());
    exited.wait().expect("wait");
    manager
        .receipts
        .set_owner_for_test("approval-1", exited_pid, "2020-01-01T00:00:00.000000000Z");
    manager.shutdown().await;
    drop(manager);

    // A restarted server reports the interrupted target run as unknown.
    let restarted = DeliveryManager::open(
        &home.path().to_string_lossy(),
        &shared,
        otto::fixtures::initial_runtime(&workspaces.startup_host.builder),
        CancellationToken::new(),
    )
    .expect("restarted manager");
    let status = restarted
        .status("source-session", "approval-1")
        .expect("status")
        .expect("task");
    assert_eq!(status.state, otto::delivery::TaskState::Unknown);
    assert_eq!(status.error, "delivery runner interrupted");
    restarted.shutdown().await;
}

#[tokio::test]
async fn the_delivery_factory_is_installed_exactly_once_for_later_parent_runners() {
    let startup = tempfile::tempdir().expect("startup");
    let startup_path = canonical_directory(startup.path()).expect("canonical");
    let sessions = tempfile::tempdir().expect("sessions");
    let home = tempfile::tempdir().expect("home");
    let host = dummy_host(&startup_path, sessions.path());
    let shared = Arc::clone(&host.builder.shared);

    assert!(
        !shared.parent_tool_factory.installed(),
        "no frontend tool is present before serve installs one"
    );

    // Serve's order: the manager opens, the slot is installed, and every
    // later parent runner -- the workflow controller included -- composes
    // the same tools.
    let manager = Arc::new(
        DeliveryManager::open(
            &home.path().to_string_lossy(),
            &shared,
            otto::fixtures::initial_runtime(&host.builder),
            CancellationToken::new(),
        )
        .expect("manager"),
    );
    assert!(
        shared
            .parent_tool_factory
            .install(Arc::new(DeliveryToolFactory::new(Arc::clone(&manager))))
    );
    assert!(shared.parent_tool_factory.installed());
    assert!(
        !shared
            .parent_tool_factory
            .install(Arc::new(DeliveryToolFactory::new(Arc::clone(&manager)))),
        "a second installation cannot add a second frontend"
    );
}

#[tokio::test]
async fn delivery_broker_refuses_unadmitted_target_before_loading_a_host() {
    let startup = tempfile::tempdir().expect("startup");
    let startup_path = canonical_directory(startup.path()).expect("canonical");
    let sessions = tempfile::tempdir().expect("sessions");
    let outside = tempfile::tempdir().expect("outside");
    let startup_host = dummy_host(&startup_path, sessions.path());
    let shared = Arc::clone(&startup_host.builder.shared);
    let workspaces = Arc::new(Workspaces {
        startup: startup_path.to_string_lossy().into_owned(),
        startup_host,
        roots: Vec::new(),
        config_path: PathBuf::new(),
        loaded: tokio::sync::Mutex::new(BTreeMap::new()),
    });
    let broker = DeliveryBroker::new(
        &shared,
        otto::fixtures::initial_runtime(&workspaces.startup_host.builder),
        CancellationToken::new(),
    );
    broker.attach_workspaces(&workspaces);

    let error = otto::delivery::Preflight::preview(
        &broker,
        "/source",
        &otto::delivery::Request {
            target: outside.path().to_string_lossy().into_owned(),
            objective: "ship".into(),
            branch: None,
            source_refs: Vec::new(),
        },
        &CancellationToken::new(),
    )
    .await
    .unwrap_err();

    assert_eq!(error, "delivery target is not an admitted workspace");
    assert!(workspaces.loaded.lock().await.is_empty());
}
