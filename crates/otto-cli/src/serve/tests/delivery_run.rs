//! Controlled delivery run, factory flow and broker tests.

use super::*;

#[tokio::test]
async fn controlled_delivery_run_persists_notifies_and_closes() {
    struct FakeRun {
        prompt: Arc<Mutex<Option<String>>>,
        closed: Arc<std::sync::atomic::AtomicBool>,
    }

    #[async_trait]
    impl DeliveryRun for FakeRun {
        fn target_session(&self) -> String {
            "target-session".into()
        }

        fn worktree(&self) -> String {
            "/target/.otto/delivery-worktrees/delivery-1".into()
        }

        async fn run_frozen(
            &self,
            prompt: &str,
            _cancel: &CancellationToken,
        ) -> Result<String, String> {
            *self.prompt.lock().unwrap() = Some(prompt.into());
            Ok("target completed the frozen handoff".into())
        }

        async fn close(self: Box<Self>) {
            self.closed.store(true, std::sync::atomic::Ordering::SeqCst);
        }
    }

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
    let source = otto::runtime::builder::SharedSession::memory(otto_core::session::Header {
        id: "source-session".into(),
        workspace: "/source".into(),
        ..otto_core::session::Header::default()
    });
    manager
        .bind_source_session("approval-1", source.clone())
        .expect("bind source");
    let worktree = startup_path.join(".otto/delivery-worktrees/delivery-1");
    let handoff = otto::delivery::handoff(
        "Ship frozen design",
        [("source-ref-1".into(), "Selected design text".into())],
    )
    .unwrap();
    let preview = otto::delivery::Preview {
        source_workspace: "/source".into(),
        plan: otto::delivery::WorktreePlan {
            target: otto::delivery::Target {
                repository: startup_path.clone(),
                common_dir: startup_path.join(".git"),
            },
            delivery_id: "delivery-1".into(),
            branch: "agent/test".into(),
            path: worktree.clone(),
        },
        base: "a".repeat(40),
        handoff,
    };
    let receipt = otto::delivery::Receipt {
        id: "approval-1".into(),
        base: preview.base.clone(),
        preview,
    };
    manager
        .receipts
        .prepare(otto::delivery::ReceiptBinding {
            approval_id: receipt.id.clone(),
            source_workspace: receipt.preview.source_workspace.clone(),
            target_repository: receipt
                .preview
                .plan
                .target
                .repository
                .to_string_lossy()
                .into_owned(),
            target_common_dir: receipt
                .preview
                .plan
                .target
                .common_dir
                .to_string_lossy()
                .into_owned(),
            worktree: worktree.to_string_lossy().into_owned(),
            branch: receipt.preview.plan.branch.clone(),
            base: receipt.base.clone(),
            handoff_digest: receipt.preview.handoff.digest.clone(),
        })
        .expect("receipt");
    manager
        .receipts
        .begin_task("approval-1", "source-session", "target-session", &worktree)
        .expect("task");
    let prompt = Arc::new(Mutex::new(None));
    let closed = Arc::new(std::sync::atomic::AtomicBool::new(false));
    manager.service.runtimes.lock().unwrap().insert(
        receipt.id.clone(),
        Box::new(FakeRun {
            prompt: Arc::clone(&prompt),
            closed: Arc::clone(&closed),
        }),
    );

    manager
        .service
        .finish_run(&receipt, &CancellationToken::new())
        .await
        .expect("terminal lifecycle");

    assert_eq!(
        prompt.lock().unwrap().as_deref(),
        Some(
            "Delivery objective:\nShip frozen design\n\nFrozen source handoff:\n\n## source-ref-1\nSelected design text"
        )
    );
    assert!(closed.load(std::sync::atomic::Ordering::SeqCst));
    assert!(manager.service.runtimes.lock().unwrap().is_empty());
    let status = manager
        .status("source-session", "approval-1")
        .unwrap()
        .unwrap();
    assert_eq!(status.state, otto::delivery::TaskState::Succeeded);
    assert_eq!(status.result, "target completed the frozen handoff");
    let messages = source.messages();
    assert_eq!(messages.len(), 1);
    let text = messages[0].text();
    assert!(text.contains("target completed the frozen handoff"));
    assert!(!text.contains(&startup_path.to_string_lossy().to_string()));
    assert!(!text.contains(&worktree.to_string_lossy().to_string()));
    manager.shutdown().await;
}

#[tokio::test]
async fn delivery_source_notification_is_once_only_and_capability_free() {
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
    let source = otto::runtime::builder::SharedSession::memory(otto_core::session::Header {
        id: "source-session".into(),
        workspace: "/source".into(),
        ..otto_core::session::Header::default()
    });
    manager
        .bind_source_session("approval-1", source.clone())
        .expect("bind source");
    let worktree = startup_path.join(".otto/delivery-worktrees/delivery-1");
    let binding = otto::delivery::ReceiptBinding {
        approval_id: "approval-1".into(),
        source_workspace: "/source".into(),
        target_repository: startup_path.to_string_lossy().into_owned(),
        target_common_dir: startup_path.join(".git").to_string_lossy().into_owned(),
        worktree: worktree.to_string_lossy().into_owned(),
        branch: "agent/test".into(),
        base: "a".repeat(40),
        handoff_digest: "b".repeat(64),
    };
    manager.receipts.prepare(binding).expect("receipt");
    manager
        .receipts
        .begin_task("approval-1", "source-session", "target-session", &worktree)
        .expect("task");
    let task = manager
        .receipts
        .finish_task(
            "approval-1",
            otto::delivery::TaskState::Succeeded,
            "target work is complete",
            "",
        )
        .expect("terminal task");
    manager
        .service
        .notify_source(&task)
        .await
        .expect("notify once");
    manager
        .service
        .notify_source(&task)
        .await
        .expect("notify twice");
    let messages = source.messages();
    assert_eq!(messages.len(), 1);
    let text = messages[0].text();
    assert!(text.contains("Delivery task approval-1 completed"));
    assert!(text.contains("target work is complete"));
    assert!(!text.contains(&startup_path.to_string_lossy().to_string()));
    assert!(!text.contains(&worktree.to_string_lossy().to_string()));
    manager.shutdown().await;
}

#[tokio::test]
async fn delivery_tool_factory_is_fresh_and_unadmitted_preflight_has_no_source_binding() {
    use otto::runtime::builder::ParentToolFactory;

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
    let first = factory.tools(&context).expect("first tools");
    let second = factory.tools(&context).expect("second tools");
    assert_eq!(first.len(), 1);
    assert_eq!(second.len(), 1);
    assert_eq!(first[0].definition().name, otto::delivery::TOOL_NAME);
    assert_eq!(second[0].definition().name, otto::delivery::TOOL_NAME);

    let outside = tempfile::tempdir().expect("outside");
    let arguments = otto::tool::testutil::raw(&format!(
        r#"{{"target":"{}","objective":"ship"}}"#,
        outside.path().to_string_lossy()
    ));
    let error = match first[0]
        .approval_spec_async(&arguments, &CancellationToken::new())
        .await
    {
        Err(error) => error,
        Ok(_) => panic!("unadmitted target must be rejected"),
    };
    assert!(error.content.contains("not an admitted workspace"));
    assert!(manager.service.source_sessions.lock().unwrap().is_empty());
    assert!(workspaces.loaded.lock().await.is_empty());
    manager.shutdown().await;
}

#[tokio::test]
async fn delivery_factory_complete_controlled_flow_persists_status_notifies_and_closes() {
    use otto::runtime::builder::ParentToolFactory;

    struct GitReplies(Mutex<Vec<String>>);
    #[async_trait]
    impl otto::sandbox::CommandExecutor for GitReplies {
        async fn execute(
            &self,
            _request: otto::sandbox::Request,
            mut streams: otto::sandbox::Streams<'_>,
            _cancel: &CancellationToken,
        ) -> (otto::sandbox::ExitStatus, Result<(), otto::sandbox::Error>) {
            let output = self.0.lock().unwrap().remove(0);
            let _ = std::io::Write::write_all(&mut streams.stdout, output.as_bytes());
            (otto::sandbox::ExitStatus::default(), Ok(()))
        }
    }

    struct ReadyRun {
        prompt: Arc<Mutex<Option<String>>>,
        closed: Arc<std::sync::atomic::AtomicBool>,
        worktree: String,
    }
    #[async_trait]
    impl DeliveryRun for ReadyRun {
        fn target_session(&self) -> String {
            "target-session".into()
        }
        fn worktree(&self) -> String {
            self.worktree.clone()
        }
        async fn run_frozen(
            &self,
            prompt: &str,
            _cancel: &CancellationToken,
        ) -> Result<String, String> {
            *self.prompt.lock().unwrap() = Some(prompt.into());
            Ok("completed controlled target work".into())
        }
        async fn close(self: Box<Self>) {
            self.closed.store(true, std::sync::atomic::Ordering::SeqCst);
        }
    }

    let home = tempfile::tempdir().expect("home");
    let target = tempfile::tempdir().expect("target");
    let target_path = canonical_directory(target.path()).expect("canonical target");
    let git_dir = target_path.join(".git");
    std::fs::create_dir(&git_dir).expect("git dir");
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
        source_session: source.clone(),
    };
    let tool = factory
        .tools(&context)
        .expect("tool")
        .pop()
        .expect("delivery tool");
    let arguments = otto::tool::testutil::raw(&format!(
        r#"{{"target":"{}","objective":"ship frozen","source_refs":["design"]}}"#,
        target_path.to_string_lossy()
    ));
    let spec = tool
        .approval_spec_async(&arguments, &CancellationToken::new())
        .await
        .expect("preflight")
        .expect("spec");
    let approval = approval_for_delivery("approval-1", &spec);
    let worktree = target_path.join(".otto/delivery-worktrees").join(
        serde_json::from_str::<serde_json::Value>(&spec.details).unwrap()["worktree"]
            .as_str()
            .unwrap()
            .rsplit('/')
            .next()
            .unwrap(),
    );
    let prompt = Arc::new(Mutex::new(None));
    let closed = Arc::new(std::sync::atomic::AtomicBool::new(false));
    manager
        .service
        .inject_ready_run(
            "approval-1",
            Box::new(ReadyRun {
                prompt: Arc::clone(&prompt),
                closed: Arc::clone(&closed),
                worktree: worktree.to_string_lossy().into_owned(),
            }),
        )
        .expect("inject ready run");
    tool.apply_approval(&arguments, &CancellationToken::new(), &approval)
        .await
        .expect("apply approval");
    let operation_id = serde_json::from_str(r#""op_delivery""#).unwrap();
    let result = tool
        .execute_approved(
            otto_core::tool::ToolCall {
                tool_call_id: "call",
                operation_id: &operation_id,
                name: otto::delivery::TOOL_NAME,
                arguments: &arguments,
                attempt: 1,
            },
            &CancellationToken::new(),
            &approval,
        )
        .await;
    assert!(!result.is_error, "{}", result.content);
    assert_eq!(
        prompt.lock().unwrap().as_deref(),
        Some(
            "Delivery objective:\nship frozen\n\nFrozen source handoff:\n\n## source-ref-1\ndesign"
        )
    );
    assert!(closed.load(std::sync::atomic::Ordering::SeqCst));
    let status = manager
        .status("source-session", "approval-1")
        .unwrap()
        .unwrap();
    assert_eq!(status.state, otto::delivery::TaskState::Succeeded);
    assert_eq!(status.result, "completed controlled target work");
    let message = source.messages().pop().expect("source notice").text();
    assert!(message.contains("completed controlled target work"));
    assert!(!message.contains(&target_path.to_string_lossy().to_string()));
    assert!(!message.contains(&worktree.to_string_lossy().to_string()));
    assert!(manager.service.source_sessions.lock().unwrap().is_empty());
    assert!(manager.service.prepared.lock().unwrap().is_empty());
    assert!(manager.service.runtimes.lock().unwrap().is_empty());
    manager.shutdown().await;
}

#[tokio::test]
async fn delivery_start_error_releases_source_session_and_closes_registered_runtime() {
    struct ClosableRun(Arc<std::sync::atomic::AtomicBool>);

    #[async_trait]
    impl DeliveryRun for ClosableRun {
        fn target_session(&self) -> String {
            "target-session".into()
        }

        fn worktree(&self) -> String {
            "/target/.otto/delivery-worktrees/delivery-1".into()
        }

        async fn run_frozen(
            &self,
            _prompt: &str,
            _cancel: &CancellationToken,
        ) -> Result<String, String> {
            Ok(String::new())
        }

        async fn close(self: Box<Self>) {
            self.0.store(true, std::sync::atomic::Ordering::SeqCst);
        }
    }

    let home = tempfile::tempdir().expect("home");
    let startup = tempfile::tempdir().expect("startup");
    let startup_path = canonical_directory(startup.path()).expect("canonical");
    let sessions = tempfile::tempdir().expect("sessions");
    let startup_host = dummy_host(&startup_path, sessions.path());
    let shared = Arc::clone(&startup_host.builder.shared);
    let runtime = otto::fixtures::initial_runtime(&startup_host.builder);
    let manager = DeliveryManager::open(
        &home.path().to_string_lossy(),
        &shared,
        runtime,
        CancellationToken::new(),
    )
    .expect("manager");
    let source = otto::runtime::builder::SharedSession::memory(otto_core::session::Header {
        id: "source-session".into(),
        workspace: "/source".into(),
        ..otto_core::session::Header::default()
    });
    manager
        .bind_source_session("approval-1", source)
        .expect("bind source");
    let closed = Arc::new(std::sync::atomic::AtomicBool::new(false));
    manager.service.runtimes.lock().unwrap().insert(
        "approval-1".into(),
        Box::new(ClosableRun(Arc::clone(&closed))),
    );
    let handoff = otto::delivery::handoff("ship", [("ref".into(), "text".into())]).unwrap();
    let preview = otto::delivery::Preview {
        source_workspace: "/source".into(),
        plan: otto::delivery::WorktreePlan {
            target: otto::delivery::Target {
                repository: startup_path.clone(),
                common_dir: startup_path.join(".git"),
            },
            delivery_id: "delivery-1".into(),
            branch: "agent/test".into(),
            path: startup_path.join(".otto/delivery-worktrees/delivery-1"),
        },
        base: "a".repeat(40),
        handoff,
    };
    // No receipt was prepared in the store, so `receipts.begin` fails first.
    let receipt = otto::delivery::Receipt {
        id: "approval-1".into(),
        base: preview.base.clone(),
        preview,
    };
    use otto::delivery::Service;
    manager
        .service
        .start(&receipt, &CancellationToken::new())
        .await
        .expect_err("unprepared receipt cannot start");
    assert!(manager.service.source_sessions.lock().unwrap().is_empty());
    assert!(manager.service.prepared.lock().unwrap().is_empty());
    assert!(manager.service.runtimes.lock().unwrap().is_empty());
    assert!(closed.load(std::sync::atomic::Ordering::SeqCst));
}
