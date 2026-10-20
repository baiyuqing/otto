
use super::*;

#[test]
fn tcp_startup_prints_connection_details() {
    let mut stdout = Vec::new();
    announce_listen(&mut stdout, "127.0.0.1:8787", "tok");
    assert_eq!(
        String::from_utf8(stdout).expect("utf-8"),
        "otto serve: http://127.0.0.1:8787/?token=tok\n"
    );
}

// ---- admission ----

#[test]
fn the_startup_workspace_is_admitted_with_no_roots() {
    let startup = tempfile::tempdir().expect("startup");
    let startup_path = canonical_directory(startup.path()).expect("canonical");
    let admitted = admit_workspace(&startup_path.to_string_lossy(), &startup_path, &[], &[])
        .expect("admitted");
    assert_eq!(admitted, startup_path);
}

#[test]
fn a_root_itself_is_admitted() {
    let startup = tempfile::tempdir().expect("startup");
    let startup_path = canonical_directory(startup.path()).expect("canonical");
    let root = tempfile::tempdir().expect("root");
    let root_path = canonical_directory(root.path()).expect("canonical");
    let admitted = admit_workspace(
        &root_path.to_string_lossy(),
        &startup_path,
        std::slice::from_ref(&root_path),
        &[],
    )
    .expect("admitted");
    assert_eq!(admitted, root_path);
}

#[test]
fn a_descendant_of_a_root_is_admitted() {
    let startup = tempfile::tempdir().expect("startup");
    let startup_path = canonical_directory(startup.path()).expect("canonical");
    let root = tempfile::tempdir().expect("root");
    let root_path = canonical_directory(root.path()).expect("canonical");
    let child = root_path.join("project");
    std::fs::create_dir(&child).expect("child");
    let admitted =
        admit_workspace(&child.to_string_lossy(), &startup_path, &[root_path], &[]).expect("ok");
    assert_eq!(admitted, child);
}

#[test]
fn a_sibling_of_a_root_is_not_admitted() {
    let startup = tempfile::tempdir().expect("startup");
    let startup_path = canonical_directory(startup.path()).expect("canonical");
    let parent = tempfile::tempdir().expect("parent");
    let root = parent.path().join("root");
    std::fs::create_dir(&root).expect("root");
    let root_path = canonical_directory(&root).expect("canonical");
    let sibling = parent.path().join("sibling");
    std::fs::create_dir(&sibling).expect("sibling");
    assert_eq!(
        admit_workspace(&sibling.to_string_lossy(), &startup_path, &[root_path], &[]),
        Err(Admission::NotAdmitted)
    );
}

#[test]
fn a_prefix_lookalike_sibling_is_not_admitted() {
    let startup = tempfile::tempdir().expect("startup");
    let startup_path = canonical_directory(startup.path()).expect("canonical");
    let parent = tempfile::tempdir().expect("parent");
    let root = parent.path().join("root");
    std::fs::create_dir(&root).expect("root");
    let root_path = canonical_directory(&root).expect("canonical");
    let lookalike = parent.path().join("root-x");
    std::fs::create_dir(&lookalike).expect("lookalike");
    assert_eq!(
        admit_workspace(
            &lookalike.to_string_lossy(),
            &startup_path,
            &[root_path],
            &[]
        ),
        Err(Admission::NotAdmitted)
    );
}

#[test]
fn a_symlink_inside_a_root_pointing_outside_is_not_admitted() {
    let startup = tempfile::tempdir().expect("startup");
    let startup_path = canonical_directory(startup.path()).expect("canonical");
    let root = tempfile::tempdir().expect("root");
    let root_path = canonical_directory(root.path()).expect("canonical");
    let outside = tempfile::tempdir().expect("outside");
    let outside_path = canonical_directory(outside.path()).expect("canonical");
    let link = root_path.join("escape");
    std::os::unix::fs::symlink(&outside_path, &link).expect("symlink");
    assert_eq!(
        admit_workspace(&link.to_string_lossy(), &startup_path, &[root_path], &[]),
        Err(Admission::NotAdmitted)
    );
}

#[test]
fn a_missing_path_is_invalid() {
    let startup = tempfile::tempdir().expect("startup");
    let startup_path = canonical_directory(startup.path()).expect("canonical");
    let missing = startup.path().join("does-not-exist");
    assert_eq!(
        admit_workspace(&missing.to_string_lossy(), &startup_path, &[], &[]),
        Err(Admission::Invalid)
    );
}

#[test]
fn a_file_is_invalid_not_a_directory() {
    let startup = tempfile::tempdir().expect("startup");
    let startup_path = canonical_directory(startup.path()).expect("canonical");
    let file = startup.path().join("file.txt");
    std::fs::write(&file, b"hi").expect("write");
    assert_eq!(
        admit_workspace(&file.to_string_lossy(), &startup_path, &[], &[]),
        Err(Admission::Invalid)
    );
}

#[test]
fn a_relative_path_is_invalid() {
    let startup = tempfile::tempdir().expect("startup");
    let startup_path = canonical_directory(startup.path()).expect("canonical");
    assert_eq!(
        admit_workspace(".", &startup_path, &[], &[]),
        Err(Admission::Invalid)
    );
    assert_eq!(
        admit_workspace("rel/dir", &startup_path, &[], &[]),
        Err(Admission::Invalid)
    );
}

#[test]
fn empty_roots_admits_only_the_startup_path() {
    let startup = tempfile::tempdir().expect("startup");
    let startup_path = canonical_directory(startup.path()).expect("canonical");
    let other = tempfile::tempdir().expect("other");
    let other_path = canonical_directory(other.path()).expect("canonical");
    assert!(admit_workspace(&startup_path.to_string_lossy(), &startup_path, &[], &[]).is_ok());
    assert_eq!(
        admit_workspace(&other_path.to_string_lossy(), &startup_path, &[], &[]),
        Err(Admission::NotAdmitted)
    );
}

#[test]
fn a_trusted_directory_outside_every_root_is_admitted() {
    let startup = tempfile::tempdir().expect("startup");
    let startup_path = canonical_directory(startup.path()).expect("canonical");
    let trusted = tempfile::tempdir().expect("trusted");
    let trusted_path = canonical_directory(trusted.path()).expect("canonical");
    let admitted = admit_workspace(
        &trusted_path.to_string_lossy(),
        &startup_path,
        &[],
        std::slice::from_ref(&trusted_path),
    )
    .expect("admitted");
    assert_eq!(admitted, trusted_path);
}

#[test]
fn a_descendant_of_a_trusted_directory_is_admitted() {
    let startup = tempfile::tempdir().expect("startup");
    let startup_path = canonical_directory(startup.path()).expect("canonical");
    let trusted = tempfile::tempdir().expect("trusted");
    let trusted_path = canonical_directory(trusted.path()).expect("canonical");
    let child = trusted_path.join("project");
    std::fs::create_dir(&child).expect("child");
    let admitted = admit_workspace(
        &child.to_string_lossy(),
        &startup_path,
        &[],
        &[trusted_path],
    )
    .expect("ok");
    assert_eq!(admitted, child);
}

#[test]
fn a_prefix_lookalike_sibling_of_a_trusted_directory_is_not_admitted() {
    let startup = tempfile::tempdir().expect("startup");
    let startup_path = canonical_directory(startup.path()).expect("canonical");
    let parent = tempfile::tempdir().expect("parent");
    let trusted = parent.path().join("app");
    std::fs::create_dir(&trusted).expect("trusted");
    let trusted_path = canonical_directory(&trusted).expect("canonical");
    let lookalike = parent.path().join("app-x");
    std::fs::create_dir(&lookalike).expect("lookalike");
    assert_eq!(
        admit_workspace(
            &lookalike.to_string_lossy(),
            &startup_path,
            &[],
            &[trusted_path]
        ),
        Err(Admission::NotAdmitted)
    );
}

/// A `WorkspaceHost` for tests that need one but never open a real
/// sandbox or workflow controller.
fn dummy_host(workspace_root: &Path, session_root: &Path) -> Arc<WorkspaceHost> {
    Arc::new(WorkspaceHost {
        builder: Arc::new(crate::cli::testutil::builder(workspace_root, session_root)),
        workflows: None,
        delivery_setup: None,
        reloader: None,
    })
}

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
    let runtime = crate::cli::testutil::initial_runtime(&workspaces.startup_host.builder);
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
    let source = crate::runtime::builder::SharedSession::memory(otto_core::session::Header {
        id: "source-session".into(),
        workspace: "/source".into(),
        ..otto_core::session::Header::default()
    });
    manager
        .bind_source_session("approval-1", source.clone())
        .expect("bind source");
    let worktree = startup_path.join(".otto/delivery-worktrees/delivery-1");
    let handoff = crate::delivery::handoff(
        "Ship frozen design",
        [("source-ref-1".into(), "Selected design text".into())],
    )
    .unwrap();
    let preview = crate::delivery::Preview {
        source_workspace: "/source".into(),
        plan: crate::delivery::WorktreePlan {
            target: crate::delivery::Target {
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
    let receipt = crate::delivery::Receipt {
        id: "approval-1".into(),
        base: preview.base.clone(),
        preview,
    };
    manager
        .receipts
        .prepare(crate::delivery::ReceiptBinding {
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
    assert_eq!(status.state, crate::delivery::TaskState::Succeeded);
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
    let runtime = crate::cli::testutil::initial_runtime(&workspaces.startup_host.builder);
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
    let source = crate::runtime::builder::SharedSession::memory(otto_core::session::Header {
        id: "source-session".into(),
        workspace: "/source".into(),
        ..otto_core::session::Header::default()
    });
    manager
        .bind_source_session("approval-1", source.clone())
        .expect("bind source");
    let worktree = startup_path.join(".otto/delivery-worktrees/delivery-1");
    let binding = crate::delivery::ReceiptBinding {
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
            crate::delivery::TaskState::Succeeded,
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
    use crate::runtime::builder::ParentToolFactory;

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
    let runtime = crate::cli::testutil::initial_runtime(&workspaces.startup_host.builder);
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
    let source = crate::runtime::builder::SharedSession::memory(otto_core::session::Header {
        id: "source-session".into(),
        workspace: "/source".into(),
        ..otto_core::session::Header::default()
    });
    let context = crate::runtime::builder::ParentToolContext {
        workspace: "/source".into(),
        session_id: "source-session".into(),
        source_session: source,
    };
    let first = factory.tools(&context).expect("first tools");
    let second = factory.tools(&context).expect("second tools");
    assert_eq!(first.len(), 1);
    assert_eq!(second.len(), 1);
    assert_eq!(first[0].definition().name, crate::delivery::TOOL_NAME);
    assert_eq!(second[0].definition().name, crate::delivery::TOOL_NAME);

    let outside = tempfile::tempdir().expect("outside");
    let arguments = crate::tool::testutil::raw(&format!(
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

fn approval_for_delivery(
    id: &str,
    spec: &crate::tool::ApprovalSpec,
) -> otto_core::approval::Request {
    otto_core::approval::Request {
        options: Vec::new(),
        schema_version: 1,
        id: id.into(),
        revision: 1,
        kind: otto_core::approval::Kind::Authorization,
        authority: otto_core::approval::Authority::Boundary,
        scope: otto_core::approval::Scope {
            workspace: "/source".into(),
            session_id: "source-session".into(),
            task_id: String::new(),
            run_id: String::new(),
        },
        presentation_origin: String::new(),
        subject: otto_core::approval::Subject {
            tool_call_id: "delivery-call".into(),
            source: "delivery".into(),
            id: "op_delivery".into(),
            version: spec.version.clone(),
            details: spec.details.clone(),
        },
        title: spec.title.clone(),
        reason: spec.reason.clone(),
        persistent: false,
        blocking: true,
        created_at: 1,
        expires_at: Some(2),
        state: otto_core::approval::State::Approved,
        decision: None,
        application: otto_core::approval::ApplicationState::Applying,
        application_error: String::new(),
        review: None,
    }
}

#[tokio::test]
async fn delivery_factory_complete_controlled_flow_persists_status_notifies_and_closes() {
    use crate::runtime::builder::ParentToolFactory;

    struct GitReplies(Mutex<Vec<String>>);
    #[async_trait]
    impl crate::sandbox::CommandExecutor for GitReplies {
        async fn execute(
            &self,
            _request: crate::sandbox::Request,
            mut streams: crate::sandbox::Streams<'_>,
            _cancel: &CancellationToken,
        ) -> (
            crate::sandbox::ExitStatus,
            Result<(), crate::sandbox::Error>,
        ) {
            let output = self.0.lock().unwrap().remove(0);
            let _ = std::io::Write::write_all(&mut streams.stdout, output.as_bytes());
            (crate::sandbox::ExitStatus::default(), Ok(()))
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
    let shared = crate::cli::testutil::shared_with_offline_sandbox(home.path());
    let mut builder = crate::cli::testutil::builder(&target_path, home.path());
    builder.command_executor = Some(replies as Arc<dyn crate::sandbox::CommandExecutor>);
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
    let runtime = crate::cli::testutil::initial_runtime(&workspaces.startup_host.builder);
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
    let source = crate::runtime::builder::SharedSession::memory(otto_core::session::Header {
        id: "source-session".into(),
        workspace: "/source".into(),
        ..otto_core::session::Header::default()
    });
    let context = crate::runtime::builder::ParentToolContext {
        workspace: "/source".into(),
        session_id: "source-session".into(),
        source_session: source.clone(),
    };
    let tool = factory
        .tools(&context)
        .expect("tool")
        .pop()
        .expect("delivery tool");
    let arguments = crate::tool::testutil::raw(&format!(
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
                name: crate::delivery::TOOL_NAME,
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
    assert_eq!(status.state, crate::delivery::TaskState::Succeeded);
    assert_eq!(status.result, "completed controlled target work");
    let message = source.messages().pop().expect("source notice").text();
    assert!(message.contains("completed controlled target work"));
    assert!(!message.contains(&target_path.to_string_lossy().to_string()));
    assert!(!message.contains(&worktree.to_string_lossy().to_string()));
    manager.shutdown().await;
}

#[tokio::test]
async fn delivery_factory_preflight_and_apply_bind_source_before_effect_capability() {
    use crate::runtime::builder::ParentToolFactory;

    struct GitReplies(Mutex<Vec<String>>);
    #[async_trait]
    impl crate::sandbox::CommandExecutor for GitReplies {
        async fn execute(
            &self,
            _request: crate::sandbox::Request,
            streams: crate::sandbox::Streams<'_>,
            _cancel: &CancellationToken,
        ) -> (
            crate::sandbox::ExitStatus,
            Result<(), crate::sandbox::Error>,
        ) {
            let output = self.0.lock().unwrap().remove(0);
            let _ = streams.stdout.write_all(output.as_bytes());
            (crate::sandbox::ExitStatus::default(), Ok(()))
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
    let shared = crate::cli::testutil::shared_with_offline_sandbox(home.path());
    let mut builder = crate::cli::testutil::builder(&target_path, home.path());
    builder.command_executor = Some(replies as Arc<dyn crate::sandbox::CommandExecutor>);
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
    let runtime = crate::cli::testutil::initial_runtime(&workspaces.startup_host.builder);
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
    let source = crate::runtime::builder::SharedSession::memory(otto_core::session::Header {
        id: "source-session".into(),
        workspace: "/source".into(),
        ..otto_core::session::Header::default()
    });
    let context = crate::runtime::builder::ParentToolContext {
        workspace: "/source".into(),
        session_id: "source-session".into(),
        source_session: source,
    };
    let tool = factory
        .tools(&context)
        .expect("tool")
        .pop()
        .expect("delivery tool");
    let arguments = crate::tool::testutil::raw(&format!(
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
    let runtime = crate::cli::testutil::initial_runtime(&workspaces.startup_host.builder);
    let manager = DeliveryManager::open(
        &home.path().to_string_lossy(),
        &shared,
        runtime,
        CancellationToken::new(),
    )
    .expect("manager");
    manager.attach_workspaces(&workspaces);
    let source = crate::runtime::builder::SharedSession::memory(otto_core::session::Header {
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
    let binding = crate::delivery::ReceiptBinding {
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
        crate::delivery::TaskState::Running
    );
    assert!(home.path().join(".otto/delivery-receipts.db").is_file());
    manager.shutdown().await;
    drop(manager);

    // A restarted server reports the interrupted target run as unknown.
    let restarted = DeliveryManager::open(
        &home.path().to_string_lossy(),
        &shared,
        crate::cli::testutil::initial_runtime(&workspaces.startup_host.builder),
        CancellationToken::new(),
    )
    .expect("restarted manager");
    let status = restarted
        .status("source-session", "approval-1")
        .expect("status")
        .expect("task");
    assert_eq!(status.state, crate::delivery::TaskState::Unknown);
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
            crate::cli::testutil::initial_runtime(&host.builder),
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
        crate::cli::testutil::initial_runtime(&workspaces.startup_host.builder),
        CancellationToken::new(),
    );
    broker.attach_workspaces(&workspaces);

    let error = crate::delivery::Preflight::preview(
        &broker,
        "/source",
        &crate::delivery::Request {
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

#[test]
fn workspaces_admit_reads_startup_and_roots() {
    let startup = tempfile::tempdir().expect("startup");
    let startup_path = canonical_directory(startup.path()).expect("canonical");
    let sessions = tempfile::tempdir().expect("sessions");
    let root = tempfile::tempdir().expect("root");
    let root_path = canonical_directory(root.path()).expect("canonical");
    let workspaces = Workspaces {
        startup: startup_path.to_string_lossy().into_owned(),
        startup_host: dummy_host(&startup_path, sessions.path()),
        roots: vec![root_path.clone()],
        config_path: PathBuf::new(),
        loaded: tokio::sync::Mutex::new(BTreeMap::new()),
    };
    assert!(workspaces.admit(&startup_path.to_string_lossy()).is_ok());
    assert_eq!(
        workspaces.admit(&root_path.to_string_lossy()),
        Ok(root_path)
    );
}

#[test]
fn a_directory_trusted_after_startup_is_admitted_without_a_restart() {
    let startup = tempfile::tempdir().expect("startup");
    let startup_path = canonical_directory(startup.path()).expect("canonical");
    let sessions = tempfile::tempdir().expect("sessions");
    let config_dir = tempfile::tempdir().expect("config dir");
    let config_path = config_dir.path().join("config.toml");
    std::fs::write(&config_path, b"").expect("write empty config");
    let trusted = tempfile::tempdir().expect("trusted");
    let trusted_path = canonical_directory(trusted.path()).expect("canonical");
    let workspaces = Workspaces {
        startup: startup_path.to_string_lossy().into_owned(),
        startup_host: dummy_host(&startup_path, sessions.path()),
        roots: Vec::new(),
        config_path: config_path.clone(),
        loaded: tokio::sync::Mutex::new(BTreeMap::new()),
    };
    assert_eq!(
        workspaces.admit(&trusted_path.to_string_lossy()),
        Err(Admission::NotAdmitted)
    );

    std::fs::write(
        &config_path,
        format!(
            "[projects.\"{}\"]\ntrust_level = \"trusted\"\n",
            trusted_path.display()
        ),
    )
    .expect("trust it");

    assert_eq!(
        workspaces.admit(&trusted_path.to_string_lossy()),
        Ok(trusted_path)
    );
}

fn factory_with(
    startup: &Path,
    sessions: &Path,
    roots: Vec<PathBuf>,
    config_path: PathBuf,
) -> ServeFactory {
    let startup_host = dummy_host(startup, sessions);
    let runtime = crate::cli::testutil::initial_runtime(&startup_host.builder);
    ServeFactory {
        workspaces: Arc::new(Workspaces {
            startup: startup.to_string_lossy().into_owned(),
            startup_host,
            roots,
            config_path,
            loaded: tokio::sync::Mutex::new(BTreeMap::new()),
        }),
        delivery: None,
        runtime,
        cancel: CancellationToken::new(),
    }
}

#[tokio::test]
async fn trusting_a_workspace_writes_the_config_so_it_is_admitted() {
    let startup = tempfile::tempdir().expect("startup");
    let startup_path = canonical_directory(startup.path()).expect("canonical");
    let sessions = tempfile::tempdir().expect("sessions");
    let config_dir = tempfile::tempdir().expect("config dir");
    let config_path = config_dir.path().join("config.toml");
    let picked = tempfile::tempdir().expect("picked");
    let picked_path = canonical_directory(picked.path()).expect("canonical");
    let factory = factory_with(
        &startup_path,
        sessions.path(),
        Vec::new(),
        config_path.clone(),
    );

    factory
        .trust_workspace(&picked_path.to_string_lossy())
        .await
        .expect("trusted");

    assert_eq!(
        factory.workspaces.admit(&picked_path.to_string_lossy()),
        Ok(picked_path)
    );
    assert!(
        std::fs::read_to_string(&config_path)
            .expect("config")
            .contains("trust_level = \"trusted\"")
    );
}

#[tokio::test]
async fn trusting_a_missing_directory_is_invalid() {
    let startup = tempfile::tempdir().expect("startup");
    let startup_path = canonical_directory(startup.path()).expect("canonical");
    let sessions = tempfile::tempdir().expect("sessions");
    let config_dir = tempfile::tempdir().expect("config dir");
    let factory = factory_with(
        &startup_path,
        sessions.path(),
        Vec::new(),
        config_dir.path().join("config.toml"),
    );

    let got = factory
        .trust_workspace(&startup_path.join("missing").to_string_lossy())
        .await;

    assert!(
        matches!(got, Err(server::WorkspaceLoadError::Invalid(_))),
        "{got:?}"
    );
}

#[test]
fn browse_roots_are_the_home_directory_then_the_configured_roots() {
    let startup = tempfile::tempdir().expect("startup");
    let startup_path = canonical_directory(startup.path()).expect("canonical");
    let sessions = tempfile::tempdir().expect("sessions");
    let root = tempfile::tempdir().expect("root");
    let root_path = canonical_directory(root.path()).expect("canonical");
    // `testutil::builder` uses the workspace directory as home.
    let factory = factory_with(
        &startup_path,
        sessions.path(),
        vec![root_path.clone()],
        PathBuf::new(),
    );

    assert_eq!(factory.browse_roots(), vec![startup_path, root_path]);
}

#[test]
fn a_trusted_path_that_no_longer_exists_is_skipped() {
    let holder = tempfile::tempdir().expect("holder");
    let target = holder.path().join("gone");
    std::fs::create_dir(&target).expect("create");
    let target_path = canonical_directory(&target).expect("canonical");
    let config_dir = tempfile::tempdir().expect("config dir");
    let config_path = config_dir.path().join("config.toml");
    std::fs::write(
        &config_path,
        format!(
            "[projects.\"{}\"]\ntrust_level = \"trusted\"\n",
            target_path.display()
        ),
    )
    .expect("write config");
    std::fs::remove_dir(&target).expect("remove");

    assert_eq!(trusted_projects(&config_path), Vec::<PathBuf>::new());
}

/// Every concurrent caller loading the same path gets the same host, and
/// only one of them built it: the registry mutex is held across the
/// build, so a caller that loses the race sees the path already loaded.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn load_builds_the_workspace_once_under_concurrent_callers() {
    let shared_root = tempfile::tempdir().expect("shared root");
    let workspace = tempfile::tempdir().expect("workspace");
    let sessions = tempfile::tempdir().expect("sessions");
    let canonical = canonical_directory(workspace.path()).expect("canonical");
    let shared = crate::cli::testutil::shared_with_offline_sandbox(shared_root.path());
    let runtime = crate::runtime::builder::resolve_initial_runtime(
        &shared.config,
        &shared.environment,
        None,
        &shared.overrides,
    )
    .expect("runtime");
    let workspaces = Arc::new(Workspaces {
        startup: shared.home.clone(),
        startup_host: dummy_host(Path::new(&shared.home), sessions.path()),
        roots: Vec::new(),
        config_path: PathBuf::new(),
        loaded: tokio::sync::Mutex::new(BTreeMap::new()),
    });
    let cancel = CancellationToken::new();

    let mut handles = Vec::new();
    for _ in 0..4 {
        let workspaces = Arc::clone(&workspaces);
        let shared = Arc::clone(&shared);
        let runtime = runtime.clone();
        let cancel = cancel.clone();
        let canonical = canonical.clone();
        handles.push(tokio::spawn(async move {
            let mut stderr = Vec::new();
            workspaces
                .load(&canonical, &shared, &runtime, &cancel, &mut stderr, false)
                .await
                .expect("load")
        }));
    }
    let mut newly_loaded_count = 0;
    let mut hosts = Vec::new();
    for handle in handles {
        let (host, newly_loaded) = handle.await.expect("join");
        if newly_loaded {
            newly_loaded_count += 1;
        }
        hosts.push(host);
    }
    assert_eq!(newly_loaded_count, 1, "exactly one caller builds the host");
    for host in &hosts[1..] {
        assert!(Arc::ptr_eq(&hosts[0], host));
    }
}

/// A failed load (here: the workspace directory does not exist) leaves
/// the registry untouched, so a later, valid load of the same path is
/// still `newly_loaded`.
#[tokio::test]
async fn a_failed_load_leaves_the_registry_unchanged() {
    let shared_root = tempfile::tempdir().expect("shared root");
    let sessions = tempfile::tempdir().expect("sessions");
    let missing = shared_root.path().join("does-not-exist");
    let shared = crate::cli::testutil::shared_with_offline_sandbox(shared_root.path());
    let runtime = crate::runtime::builder::resolve_initial_runtime(
        &shared.config,
        &shared.environment,
        None,
        &shared.overrides,
    )
    .expect("runtime");
    let workspaces = Workspaces {
        startup: shared.home.clone(),
        startup_host: dummy_host(Path::new(&shared.home), sessions.path()),
        roots: Vec::new(),
        config_path: PathBuf::new(),
        loaded: tokio::sync::Mutex::new(BTreeMap::new()),
    };
    let cancel = CancellationToken::new();
    let mut stderr = Vec::new();

    let error = match workspaces
        .load(&missing, &shared, &runtime, &cancel, &mut stderr, false)
        .await
    {
        Ok(_) => panic!("a missing directory must fail to load"),
        Err(error) => error,
    };
    assert!(!error.is_empty());
    assert!(workspaces.loaded.lock().await.is_empty());

    std::fs::create_dir(&missing).expect("create the workspace directory");
    let (_, newly_loaded) = workspaces
        .load(&missing, &shared, &runtime, &cancel, &mut stderr, false)
        .await
        .expect("load after fixing the path");
    assert!(newly_loaded);
}

/// A session created for a named, already-loaded workspace uses that
/// workspace's own `Builder`: its `Controller::workspace()` and its
/// file-tool `Workspace` root are both that workspace, not the startup
/// one.
#[tokio::test]
async fn create_in_a_named_workspace_uses_that_workspaces_builder() {
    let startup = tempfile::tempdir().expect("startup");
    let startup_path = canonical_directory(startup.path()).expect("canonical");
    let startup_sessions = tempfile::tempdir().expect("startup sessions");
    let startup_host = dummy_host(&startup_path, startup_sessions.path());

    let other = tempfile::tempdir().expect("other workspace");
    let other_path = canonical_directory(other.path()).expect("canonical");
    let other_sessions = tempfile::tempdir().expect("other sessions");
    let shared = crate::cli::testutil::shared_with_offline_sandbox(other_sessions.path());
    std::fs::write(&shared.config_path, "[sandbox]\ndriver='off'\n").expect("config");
    let (mut builder, sandbox) = load_workspace(
        shared,
        &other_path,
        &Disabled::default(),
        &CancellationToken::new(),
        &mut Vec::new(),
    )
    .await
    .expect("load workspace B");
    Arc::make_mut(&mut builder.shared)
        .config
        .profiles
        .get_mut("alpha")
        .unwrap()
        .model = "workspace-b-model".into();
    assert!(
        builder.grant_control.is_some(),
        "loaded workspace wires persistent read grants"
    );
    let other_host = Arc::new(WorkspaceHost {
        builder: Arc::new(builder),
        workflows: None,
        delivery_setup: match (sandbox.approval_executor, sandbox.approval_environment) {
            (Some(executor), Some(environment)) => Some(DeliverySetupCapability {
                executor,
                environment,
            }),
            _ => None,
        },
        reloader: sandbox.reloader,
    });

    let mut loaded = BTreeMap::new();
    loaded.insert(
        other_path.to_string_lossy().into_owned(),
        Arc::clone(&other_host),
    );
    let runtime = crate::cli::testutil::initial_runtime(&startup_host.builder);
    let factory = ServeFactory {
        workspaces: Arc::new(Workspaces {
            startup: startup_path.to_string_lossy().into_owned(),
            startup_host,
            roots: Vec::new(),
            config_path: PathBuf::new(),
            loaded: tokio::sync::Mutex::new(loaded),
        }),
        delivery: None,
        runtime,
        cancel: CancellationToken::new(),
    };

    let controller = factory
        .create(Some(&other_path.to_string_lossy()))
        .await
        .expect("create in workspace B");

    assert_eq!(controller.workspace(), other_path.to_string_lossy());
    assert_eq!(other_host.builder.workspace.root(), other_path);
    assert_eq!(controller.info().model, "workspace-b-model");
    controller
        .reload_sandbox()
        .await
        .expect("reload B when startup has no reloader");
    controller.close_mcp().await;
    controller.close().expect("close");
    other_host
        .reloader
        .as_ref()
        .unwrap()
        .control
        .close()
        .await
        .expect("close sandbox");
}

fn factory_over(startup_path: &Path, sessions: &Path) -> (ServeFactory, Arc<WorkspaceHost>) {
    let startup_host = dummy_host(startup_path, sessions);
    let runtime = crate::cli::testutil::initial_runtime(&startup_host.builder);
    let factory = ServeFactory {
        workspaces: Arc::new(Workspaces {
            startup: startup_path.to_string_lossy().into_owned(),
            startup_host: Arc::clone(&startup_host),
            roots: Vec::new(),
            config_path: PathBuf::new(),
            loaded: tokio::sync::Mutex::new(BTreeMap::from([(
                startup_path.to_string_lossy().into_owned(),
                Arc::clone(&startup_host),
            )])),
        }),
        delivery: None,
        runtime,
        cancel: CancellationToken::new(),
    };
    (factory, startup_host)
}

/// Resume by id reads `<session directory>/<id>.jsonl` directly, so a
/// session older than the newest `MAX_LIST_SESSIONS` still opens.
#[tokio::test]
async fn opening_by_id_finds_a_session_outside_the_newest_listing_window() {
    let workspace = tempfile::tempdir().expect("workspace");
    let workspace_path = canonical_directory(workspace.path()).expect("canonical");
    let sessions = tempfile::tempdir().expect("sessions");
    let (factory, host) = factory_over(&workspace_path, sessions.path());

    use otto_core::session::Session;
    let runtime = crate::cli::testutil::initial_runtime(&host.builder);
    let mut oldest = String::new();
    for index in 0..=MAX_LIST_SESSIONS {
        let session = host.builder.create_session(&runtime).expect("session");
        session
            .append(crate::cli::testutil::user("hello"))
            .await
            .expect("persist the first message");
        let path = PathBuf::from(session.path());
        if index == 0 {
            oldest = path
                .file_stem()
                .expect("file stem")
                .to_string_lossy()
                .into_owned();
            std::fs::OpenOptions::new()
                .write(true)
                .open(&path)
                .expect("session file")
                .set_modified(std::time::SystemTime::now() - std::time::Duration::from_secs(86_400))
                .expect("age the file");
        }
    }
    let listed = listed_for(&host.builder).expect("listing");
    assert_eq!(listed.sessions.len(), MAX_LIST_SESSIONS);
    assert!(listed.sessions.iter().all(|entry| entry.id != oldest));

    let reopened = factory.open(&oldest, None).await.expect("open by id");
    assert_eq!(reopened.info().session_id, oldest);
}

/// An id that is not 32 lowercase hexadecimal characters answers "not
/// found" before it is used as a file name.
#[tokio::test]
async fn opening_a_non_hex_id_is_not_found() {
    let workspace = tempfile::tempdir().expect("workspace");
    let workspace_path = canonical_directory(workspace.path()).expect("canonical");
    let sessions = tempfile::tempdir().expect("sessions");
    let (factory, _host) = factory_over(&workspace_path, sessions.path());
    for id in [
        "../../etc/passwd",
        "some-id",
        &"A".repeat(32),
        &"a".repeat(31),
        &"a".repeat(33),
    ] {
        let error = match factory.open(id, None).await {
            Ok(_) => panic!("{id} opened"),
            Err(error) => error,
        };
        assert_eq!(error, SESSION_NOT_FOUND, "{id}");
    }
    assert!(sessionfs::is_session_id(&"0123456789abcdef".repeat(2)));
}

#[test]
fn a_nonexistent_root_is_a_startup_error_naming_the_key_and_path() {
    let error = canonicalize_workspace_roots(&["/does/not/exist".to_string()]).unwrap_err();
    assert!(error.contains("workspace_roots"), "{error}");
    assert!(error.contains("/does/not/exist"), "{error}");
}

#[test]
fn valid_roots_canonicalize_in_order() {
    let a = tempfile::tempdir().expect("a");
    let b = tempfile::tempdir().expect("b");
    let canonical_a = canonical_directory(a.path()).expect("canonical");
    let canonical_b = canonical_directory(b.path()).expect("canonical");
    let roots = canonicalize_workspace_roots(&[
        a.path().to_string_lossy().into_owned(),
        b.path().to_string_lossy().into_owned(),
    ])
    .expect("canonicalize");
    assert_eq!(roots, vec![canonical_a, canonical_b]);
}

// ---- persisted workspace list ----

fn runtime_for(shared: &Arc<crate::runtime::builder::Shared>) -> Runtime {
    crate::runtime::builder::resolve_initial_runtime(
        &shared.config,
        &shared.environment,
        None,
        &shared.overrides,
    )
    .expect("runtime")
}

/// Adding a workspace persists it, and a fresh registry over the same
/// home (a restart) reloads it, so it shows up in `Factory::workspaces`
/// the same way `GET /v1/workspaces` reports it.
#[tokio::test]
async fn a_persisted_workspace_reloads_at_startup_and_is_listed() {
    let home = tempfile::tempdir().expect("home");
    let startup = tempfile::tempdir().expect("startup");
    let startup_path = canonical_directory(startup.path()).expect("canonical");
    let roots_dir = tempfile::tempdir().expect("roots");
    let roots_path = canonical_directory(roots_dir.path()).expect("canonical");
    let workspace_path = roots_path.join("project");
    std::fs::create_dir(&workspace_path).expect("workspace directory");
    let sessions = tempfile::tempdir().expect("sessions");
    let shared = crate::cli::testutil::shared_with_offline_sandbox(home.path());
    let runtime = runtime_for(&shared);
    let cancel = CancellationToken::new();
    let mut stderr = Vec::new();

    let first_run = Workspaces {
        startup: startup_path.to_string_lossy().into_owned(),
        startup_host: dummy_host(&startup_path, sessions.path()),
        roots: vec![roots_path.clone()],
        config_path: PathBuf::new(),
        loaded: tokio::sync::Mutex::new(BTreeMap::new()),
    };
    first_run
        .load(
            &workspace_path,
            &shared,
            &runtime,
            &cancel,
            &mut stderr,
            true,
        )
        .await
        .expect("load and persist");

    // The restart: a fresh registry seeded with only the startup
    // workspace, as `run` seeds it before reloading the persisted list.
    let restarted = Arc::new(Workspaces {
        startup: startup_path.to_string_lossy().into_owned(),
        startup_host: dummy_host(&startup_path, sessions.path()),
        roots: vec![roots_path],
        config_path: PathBuf::new(),
        loaded: tokio::sync::Mutex::new(BTreeMap::new()),
    });
    load_persisted_workspaces(&restarted, &shared, &runtime, &cancel, &mut stderr).await;
    let factory = ServeFactory {
        workspaces: restarted,
        delivery: None,
        runtime,
        cancel,
    };

    let listed = factory.workspaces().await.loaded;
    assert!(
        listed
            .iter()
            .any(|info| info.path == workspace_path.to_string_lossy()),
        "{listed:?}"
    );
}

/// A path saved while it was inside `workspace_roots` is skipped with a
/// warning once a config change moves it outside; a path still inside
/// loads normally.
#[tokio::test]
async fn a_listed_path_outside_current_roots_is_skipped_others_load() {
    let home = tempfile::tempdir().expect("home");
    let startup = tempfile::tempdir().expect("startup");
    let startup_path = canonical_directory(startup.path()).expect("canonical");
    let sessions = tempfile::tempdir().expect("sessions");
    let roots_dir = tempfile::tempdir().expect("roots");
    let roots_path = canonical_directory(roots_dir.path()).expect("canonical");
    let admitted = roots_path.join("project");
    std::fs::create_dir(&admitted).expect("admitted child");
    let outside = tempfile::tempdir().expect("outside");
    let outside_path = canonical_directory(outside.path()).expect("canonical");
    let shared = crate::cli::testutil::shared_with_offline_sandbox(home.path());
    let runtime = runtime_for(&shared);
    let home_str = home.path().to_str().expect("utf-8 home");
    add_to_workspace_list(home_str, &admitted.to_string_lossy()).expect("add admitted");
    add_to_workspace_list(home_str, &outside_path.to_string_lossy()).expect("add outside");

    let workspaces = Workspaces {
        startup: startup_path.to_string_lossy().into_owned(),
        startup_host: dummy_host(&startup_path, sessions.path()),
        roots: vec![roots_path],
        config_path: PathBuf::new(),
        loaded: tokio::sync::Mutex::new(BTreeMap::new()),
    };
    let cancel = CancellationToken::new();
    let mut stderr = Vec::new();
    load_persisted_workspaces(&workspaces, &shared, &runtime, &cancel, &mut stderr).await;

    let loaded = workspaces.loaded.lock().await;
    assert!(loaded.contains_key(&admitted.to_string_lossy().into_owned()));
    assert!(!loaded.contains_key(&outside_path.to_string_lossy().into_owned()));
    drop(loaded);
    let warning = String::from_utf8(stderr).expect("utf-8");
    assert!(
        warning.contains(outside_path.to_string_lossy().as_ref()),
        "{warning}"
    );
    assert!(
        warning.contains("outside the startup workspace and configured roots"),
        "{warning}"
    );
}

/// A persisted path whose directory was since deleted is skipped with a
/// warning; startup does not fail because of it.
#[tokio::test]
async fn a_deleted_persisted_directory_is_skipped_with_a_warning() {
    let home = tempfile::tempdir().expect("home");
    let startup = tempfile::tempdir().expect("startup");
    let startup_path = canonical_directory(startup.path()).expect("canonical");
    let sessions = tempfile::tempdir().expect("sessions");
    let missing = home.path().join("does-not-exist");
    let shared = crate::cli::testutil::shared_with_offline_sandbox(home.path());
    let runtime = runtime_for(&shared);
    add_to_workspace_list(
        home.path().to_str().expect("utf-8 home"),
        &missing.to_string_lossy(),
    )
    .expect("add missing path");

    let workspaces = Workspaces {
        startup: startup_path.to_string_lossy().into_owned(),
        startup_host: dummy_host(&startup_path, sessions.path()),
        roots: Vec::new(),
        config_path: PathBuf::new(),
        loaded: tokio::sync::Mutex::new(BTreeMap::new()),
    };
    let cancel = CancellationToken::new();
    let mut stderr = Vec::new();
    load_persisted_workspaces(&workspaces, &shared, &runtime, &cancel, &mut stderr).await;

    assert!(workspaces.loaded.lock().await.is_empty());
    let warning = String::from_utf8(stderr).expect("utf-8");
    assert!(
        warning.contains(missing.to_string_lossy().as_ref()),
        "{warning}"
    );
    assert!(warning.contains("not an existing directory"), "{warning}");
}

/// An unparsable persisted file is a startup warning, not a startup
/// failure, and startup never rewrites it: only a later successful add
/// does.
#[tokio::test]
async fn an_unparsable_workspace_list_file_warns_and_is_left_unchanged() {
    let home = tempfile::tempdir().expect("home");
    let startup = tempfile::tempdir().expect("startup");
    let startup_path = canonical_directory(startup.path()).expect("canonical");
    let sessions = tempfile::tempdir().expect("sessions");
    let shared = crate::cli::testutil::shared_with_offline_sandbox(home.path());
    let runtime = runtime_for(&shared);
    let home_str = home.path().to_str().expect("utf-8 home");
    std::fs::create_dir_all(home.path().join(".otto")).expect(".otto dir");
    let garbage = b"not json".to_vec();
    std::fs::write(workspace_list_path(home_str), &garbage).expect("write garbage");

    let workspaces = Workspaces {
        startup: startup_path.to_string_lossy().into_owned(),
        startup_host: dummy_host(&startup_path, sessions.path()),
        roots: Vec::new(),
        config_path: PathBuf::new(),
        loaded: tokio::sync::Mutex::new(BTreeMap::new()),
    };
    let cancel = CancellationToken::new();
    let mut stderr = Vec::new();
    load_persisted_workspaces(&workspaces, &shared, &runtime, &cancel, &mut stderr).await;

    assert!(workspaces.loaded.lock().await.is_empty());
    let warning = String::from_utf8(stderr).expect("utf-8");
    assert!(warning.contains("warning:"), "{warning}");
    let after = std::fs::read(workspace_list_path(home_str)).expect("read back");
    assert_eq!(after, garbage, "an unparsable file must be left untouched");
}

/// The startup workspace is loaded before `Workspaces` exists and is
/// already in `loaded`, so `load` never reaches the persist step for it;
/// adding a different path twice still leaves one entry.
#[tokio::test]
async fn the_startup_workspace_is_never_persisted_and_duplicate_adds_collapse() {
    let home = tempfile::tempdir().expect("home");
    let startup = tempfile::tempdir().expect("startup");
    let startup_path = canonical_directory(startup.path()).expect("canonical");
    let sessions = tempfile::tempdir().expect("sessions");
    let shared = crate::cli::testutil::shared_with_offline_sandbox(home.path());
    let runtime = runtime_for(&shared);
    let cancel = CancellationToken::new();
    let mut stderr = Vec::new();
    let startup_host = dummy_host(&startup_path, sessions.path());
    let workspaces = Workspaces {
        startup: startup_path.to_string_lossy().into_owned(),
        startup_host: Arc::clone(&startup_host),
        roots: Vec::new(),
        config_path: PathBuf::new(),
        loaded: tokio::sync::Mutex::new(BTreeMap::from([(
            startup_path.to_string_lossy().into_owned(),
            startup_host,
        )])),
    };

    let (_, newly_loaded) = workspaces
        .load(&startup_path, &shared, &runtime, &cancel, &mut stderr, true)
        .await
        .expect("load the already-loaded startup workspace");
    assert!(!newly_loaded);
    assert!(
        !workspace_list_path(home.path().to_str().expect("utf-8 home")).exists(),
        "the startup workspace must never be written to the persisted list"
    );

    let other = tempfile::tempdir().expect("other");
    let other_path = canonical_directory(other.path()).expect("canonical");
    let other_string = other_path.to_string_lossy().into_owned();
    let home_str = home.path().to_str().expect("utf-8 home");
    add_to_workspace_list(home_str, &other_string).expect("add once");
    add_to_workspace_list(home_str, &other_string).expect("add twice");
    assert_eq!(
        read_workspace_list(home_str).expect("read"),
        vec![other_string]
    );
}

// ---- removing a workspace ----

/// Removing a loaded, persisted workspace drops its host and its
/// persisted-list entry in one call. The other entries keep their
/// original order: `remove_from_workspace_list`'s `retain` never
/// resorts, unlike `add_to_workspace_list`.
#[tokio::test]
async fn removing_a_loaded_and_persisted_workspace_unloads_it_and_updates_the_list() {
    let home = tempfile::tempdir().expect("home");
    let startup = tempfile::tempdir().expect("startup");
    let startup_path = canonical_directory(startup.path()).expect("canonical");
    let sessions = tempfile::tempdir().expect("sessions");
    let workspace = tempfile::tempdir().expect("workspace");
    let workspace_path = canonical_directory(workspace.path()).expect("canonical");
    let workspace_string = workspace_path.to_string_lossy().into_owned();
    let home_str = home.path().to_str().expect("utf-8 home");
    write_workspace_list(
        home_str,
        vec![
            "z-other".to_string(),
            workspace_string.clone(),
            "a-other".to_string(),
        ],
    )
    .expect("seed the persisted list");

    let mut loaded = BTreeMap::new();
    loaded.insert(
        workspace_string.clone(),
        dummy_host(&workspace_path, sessions.path()),
    );
    let workspaces = Workspaces {
        startup: startup_path.to_string_lossy().into_owned(),
        startup_host: dummy_host(&startup_path, sessions.path()),
        roots: Vec::new(),
        config_path: PathBuf::new(),
        loaded: tokio::sync::Mutex::new(loaded),
    };
    let mut stderr = Vec::new();

    workspaces
        .remove(&workspace_string, home_str, &mut stderr)
        .await
        .expect("remove a loaded workspace");

    assert!(
        !workspaces
            .loaded
            .lock()
            .await
            .contains_key(&workspace_string)
    );
    assert_eq!(
        read_workspace_list(home_str).expect("read"),
        vec!["z-other".to_string(), "a-other".to_string()],
        "the other entries keep their original order"
    );
}

/// A path only in the persisted list, never loaded, is removed from the
/// list; `loaded` is untouched.
#[tokio::test]
async fn removing_a_persisted_only_workspace_removes_it_from_the_list() {
    let home = tempfile::tempdir().expect("home");
    let startup = tempfile::tempdir().expect("startup");
    let startup_path = canonical_directory(startup.path()).expect("canonical");
    let sessions = tempfile::tempdir().expect("sessions");
    let home_str = home.path().to_str().expect("utf-8 home");
    add_to_workspace_list(home_str, "/some/persisted/path").expect("seed");
    add_to_workspace_list(home_str, "/other/path").expect("seed");

    let workspaces = Workspaces {
        startup: startup_path.to_string_lossy().into_owned(),
        startup_host: dummy_host(&startup_path, sessions.path()),
        roots: Vec::new(),
        config_path: PathBuf::new(),
        loaded: tokio::sync::Mutex::new(BTreeMap::new()),
    };
    let mut stderr = Vec::new();

    workspaces
        .remove("/some/persisted/path", home_str, &mut stderr)
        .await
        .expect("remove a persisted-only workspace");

    assert!(workspaces.loaded.lock().await.is_empty());
    assert_eq!(
        read_workspace_list(home_str).expect("read"),
        vec!["/other/path".to_string()]
    );
}

/// A path neither loaded nor in the persisted list is `NotFound`.
#[tokio::test]
async fn removing_an_unknown_path_answers_not_found() {
    let home = tempfile::tempdir().expect("home");
    let startup = tempfile::tempdir().expect("startup");
    let startup_path = canonical_directory(startup.path()).expect("canonical");
    let sessions = tempfile::tempdir().expect("sessions");
    let workspaces = Workspaces {
        startup: startup_path.to_string_lossy().into_owned(),
        startup_host: dummy_host(&startup_path, sessions.path()),
        roots: Vec::new(),
        config_path: PathBuf::new(),
        loaded: tokio::sync::Mutex::new(BTreeMap::new()),
    };
    let mut stderr = Vec::new();
    let home_str = home.path().to_str().expect("utf-8 home");

    let error = workspaces
        .remove("/never/added", home_str, &mut stderr)
        .await
        .expect_err("an unknown path must not be removable");
    assert!(matches!(error, server::WorkspaceRemoveError::NotFound));
}

/// The startup workspace can never be removed, even when it happens to
/// also be in the persisted list.
#[tokio::test]
async fn removing_the_startup_workspace_answers_is_startup() {
    let home = tempfile::tempdir().expect("home");
    let startup = tempfile::tempdir().expect("startup");
    let startup_path = canonical_directory(startup.path()).expect("canonical");
    let sessions = tempfile::tempdir().expect("sessions");
    let workspaces = Workspaces {
        startup: startup_path.to_string_lossy().into_owned(),
        startup_host: dummy_host(&startup_path, sessions.path()),
        roots: Vec::new(),
        config_path: PathBuf::new(),
        loaded: tokio::sync::Mutex::new(BTreeMap::new()),
    };
    let mut stderr = Vec::new();
    let home_str = home.path().to_str().expect("utf-8 home");

    let error = workspaces
        .remove(&startup_path.to_string_lossy(), home_str, &mut stderr)
        .await
        .expect_err("the startup workspace must not be removable");
    assert!(matches!(error, server::WorkspaceRemoveError::IsStartup));
}
