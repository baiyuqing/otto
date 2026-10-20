
use super::*;
use crate::sandbox::direct::DirectDriver;
use crate::sandbox::{Executor, FilesystemMode, NetworkMode, Policy};
use otto_core::config::Profile;
use otto_core::model::{EffectCertainty, OperationDisposition};
use otto_core::operation::OperationControl;
use otto_core::provider::ProviderSettlement;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

#[tokio::test]
async fn recording_captures_appended_messages_until_finished() {
    let session = SharedSession::memory(Header::default());
    let message = |text: &str| Message {
        id: text.into(),
        role: otto_core::model::Role::User,
        blocks: vec![Block::text(text)],
        ..Message::default()
    };
    session.append(message("before")).await.unwrap();
    assert!(session.finish_recording().is_empty());

    session.start_recording();
    session.clone().append(message("during")).await.unwrap();
    let recorded = session.finish_recording();
    assert_eq!(recorded.len(), 1);
    assert_eq!(recorded[0].text(), "during");

    session.append(message("after")).await.unwrap();
    assert!(session.finish_recording().is_empty());
}

struct PendingProvider {
    polls: Arc<AtomicUsize>,
    dropped: Arc<AtomicBool>,
}

struct LogSink(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for LogSink {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        self.0.lock().expect("log bytes").extend_from_slice(buffer);
        Ok(buffer.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

struct DropFlag(Arc<AtomicBool>);

impl Drop for DropFlag {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

#[async_trait::async_trait]
impl Provider for PendingProvider {
    async fn complete(
        &self,
        _request: &Request,
        _emit: StreamSink<'_>,
        control: &dyn OperationControl,
    ) -> ProviderSettlement {
        self.polls.fetch_add(1, Ordering::SeqCst);
        let _drop = DropFlag(Arc::clone(&self.dropped));
        control.cancellation_token().cancelled().await;
        std::future::pending().await
    }
}

#[tokio::test]
async fn remote_compaction_logs_safe_diagnostic_metadata_without_opaque_content() {
    let bytes = Arc::new(Mutex::new(Vec::new()));
    let logger = crate::log::Logger::new(Box::new(LogSink(Arc::clone(&bytes))));
    let session = SharedSession::memory(Header::default());
    session
        .append_remote_compaction(otto_core::session::RemoteCompaction {
            provider: "chatgpt".into(),
            id: "compact-1".into(),
            encrypted_content: "opaque-secret-must-not-appear".into(),
        })
        .await
        .expect("append checkpoint");

    log_runtime_event(
        &logger,
        &session,
        &Event::ProviderApiCall {
            operation_id: otto_core::model::OperationId::new("operation-1").unwrap(),
            kind: otto_core::agent::ProviderOperationKind::RemoteCompaction,
            provider: "chatgpt".into(),
            model: "test-model".into(),
            duration: Duration::from_millis(42),
            attempts: 2,
            status: otto_core::agent::ApiStatus::Ok,
            outcome: otto_core::model::OperationOutcome {
                disposition: OperationDisposition::Succeeded,
                effect_certainty: EffectCertainty::Completed,
                stop_reason: None,
            },
        },
    );
    log_runtime_event(
        &logger,
        &session,
        &Event::CompactionCompleted {
            compaction: CompactionResult {
                checkpoint_id: "compact-1".into(),
                tokens_before: Some(120),
                usage_present: true,
                cached_input_tokens_present: true,
                ..CompactionResult::default()
            },
        },
    );

    let text = String::from_utf8(bytes.lock().expect("log bytes").clone()).unwrap();
    for expected in [
        "msg=provider_operation_finished",
        "operation_id=operation-1",
        "kind=remote_compaction",
        "attempts=2",
        "status=ok",
        "effect_certainty=completed",
        "duration_ms=42",
        "msg=remote_compaction_finished",
        "checkpoint_id=compact-1",
        "provider=chatgpt",
        "tail_messages=0",
        "tokens_before=120",
        "usage_present=true",
        "cached_input_tokens_present=true",
    ] {
        assert!(text.contains(expected), "missing {expected:?} in {text:?}");
    }
    assert!(!text.contains("opaque-secret-must-not-appear"), "{text}");
}

#[tokio::test]
async fn a_noop_compaction_does_not_log_an_unnamed_remote_checkpoint() {
    let bytes = Arc::new(Mutex::new(Vec::new()));
    let logger = crate::log::Logger::new(Box::new(LogSink(Arc::clone(&bytes))));
    let session = SharedSession::memory(Header::default());
    session
        .append_remote_compaction(otto_core::session::RemoteCompaction {
            provider: "chatgpt".into(),
            id: String::new(),
            encrypted_content: "opaque".into(),
        })
        .await
        .expect("append checkpoint");

    log_runtime_event(
        &logger,
        &session,
        &Event::CompactionCompleted {
            compaction: CompactionResult {
                noop: true,
                ..CompactionResult::default()
            },
        },
    );

    let text = String::from_utf8(bytes.lock().expect("log bytes").clone()).unwrap();
    assert!(!text.contains("remote_compaction_finished"), "{text}");
}

#[tokio::test(start_paused = true)]
async fn provider_deadline_drops_an_uncooperative_future_after_grace() {
    let polls = Arc::new(AtomicUsize::new(0));
    let dropped = Arc::new(AtomicBool::new(false));
    let provider = PendingProvider {
        polls: Arc::clone(&polls),
        dropped: Arc::clone(&dropped),
    };
    let parent = Control::new(Deadline::unlimited());
    let mut emit = |_| {};
    let request = Request::default();
    let complete = complete_with_timeout(
        &provider,
        &request,
        &mut emit,
        &parent,
        Some(Duration::from_secs(2)),
        Duration::from_secs(3),
    );
    tokio::pin!(complete);
    assert!(
        tokio::time::timeout(Duration::ZERO, &mut complete)
            .await
            .is_err()
    );
    tokio::time::advance(Duration::from_secs(2)).await;
    assert!(
        tokio::time::timeout(Duration::ZERO, &mut complete)
            .await
            .is_err()
    );
    tokio::time::advance(Duration::from_secs(3)).await;
    let settlement = complete.await;

    assert_eq!(polls.load(Ordering::SeqCst), 1);
    assert!(dropped.load(Ordering::SeqCst));
    assert_eq!(settlement.attempts, 1);
    assert_eq!(
        settlement.outcome.disposition,
        OperationDisposition::DeadlineExceeded
    );
    assert_eq!(
        settlement.outcome.effect_certainty,
        EffectCertainty::Unknown
    );
}

#[tokio::test(start_paused = true)]
async fn expired_parent_refuses_provider_before_polling_it() {
    let polls = Arc::new(AtomicUsize::new(0));
    let provider = PendingProvider {
        polls: Arc::clone(&polls),
        dropped: Arc::new(AtomicBool::new(false)),
    };
    let parent = Control::new(Deadline::after(Duration::ZERO));
    let mut emit = |_| {};
    let request = Request::default();
    let settlement = complete_with_timeout(
        &provider,
        &request,
        &mut emit,
        &parent,
        None,
        Duration::from_secs(3),
    )
    .await;

    assert_eq!(polls.load(Ordering::SeqCst), 0);
    assert_eq!(settlement.attempts, 0);
    assert_eq!(
        settlement.outcome.effect_certainty,
        EffectCertainty::NotStarted
    );
    assert_eq!(
        settlement.outcome.disposition,
        OperationDisposition::DeadlineExceeded
    );
}

#[tokio::test(start_paused = true)]
async fn drive_prefers_an_already_ready_completion() {
    let parent = CancellationToken::new();
    parent.cancel();
    let control = Control::new(Deadline::after(Duration::ZERO));

    let result = drive_with_control(async { 42 }, &parent, &control).await;

    assert_eq!(result, 42);
    assert_eq!(control.stop_reason(), None);
}

#[tokio::test(start_paused = true)]
async fn deadline_waits_for_the_owned_future_to_clean_up() {
    let parent = CancellationToken::new();
    let control = Control::new(Deadline::after(Duration::from_secs(5)));
    let cleaned = AtomicBool::new(false);
    let run = async {
        control.cancellation_token().cancelled().await;
        cleaned.store(true, Ordering::SeqCst);
    };

    let drive = drive_with_control(run, &parent, &control);
    tokio::pin!(drive);
    tokio::time::advance(Duration::from_secs(5)).await;
    drive.await;

    assert!(cleaned.load(Ordering::SeqCst));
    assert_eq!(control.stop_reason(), Some(OperationStopReason::Deadline));
}

#[tokio::test]
async fn parent_cancellation_keeps_its_typed_reason_and_waits_for_cleanup() {
    let parent = CancellationToken::new();
    let control = Control::new(Deadline::unlimited());
    let cleaned = AtomicBool::new(false);
    let run = async {
        control.cancellation_token().cancelled().await;
        cleaned.store(true, Ordering::SeqCst);
    };
    parent.cancel();

    drive_with_control(run, &parent, &control).await;

    assert!(cleaned.load(Ordering::SeqCst));
    assert_eq!(
        control.stop_reason(),
        Some(OperationStopReason::UserCancellation)
    );
}

fn shared(root: &Path) -> Arc<Shared> {
    Arc::new(Shared {
        storage: Arc::new(Default::default()),
        approvals: Arc::new(crate::approval::Service::in_memory().expect("approval store")),
        config_path: root.join("config.toml"),
        config: File::default(),
        environment: HashMap::new(),
        home: root.to_string_lossy().into_owned(),
        session_root: root.join("sessions"),
        shell: "/bin/sh".to_string(),
        no_session: true,
        headless: false,
        host_entries: Vec::new(),
        overrides: Overrides::default(),
        sandbox_secrets_baseline: Vec::new(),
        sandbox_secrets_baseline_complete: true,
        sandbox_driver_override: None,
        explicit_config: false,
        auth_path: String::new(),
        auth_credentials: crate::auth::Credentials::default(),
        auth_credentials_loaded: false,
        usage: None,
        task_recorder: None,
        schedule: None,
        skill_checker: None,
        memory: Default::default(),
        reflector: Arc::new(crate::reflection::Reflector::new(Default::default(), None)),
        logger: Arc::new(crate::log::Logger::discard()),
        parent_tool_factory: ParentToolSlot::default(),
    })
}

fn builder_for(shared: Arc<Shared>, root: &Path) -> Builder {
    let workspace = leaked_workspace(root).expect("workspace");
    let workspace_path = workspace.root().to_string_lossy().into_owned();
    let mut builder = Builder::for_workspace(
        shared,
        workspace,
        workspace_path,
        McpRuntime {
            enabled: false,
            call_timeout_secs: 60,
            connect_timeout_secs: 20,
            servers: Vec::new(),
        },
    );
    builder.sandbox_info = SandboxInfo::unavailable(SandboxReason::SeatbeltMissing);
    builder
}

fn builder(root: &Path) -> Builder {
    builder_for(shared(root), root)
}

#[tokio::test]
async fn authorization_admission_preserves_each_original_and_legacy_new_task_can_review() {
    let directory = tempfile::tempdir().unwrap();
    let builder =
        crate::cli::testutil::builder(directory.path(), &directory.path().join("sessions"));
    let runtime = crate::cli::testutil::initial_runtime(&builder);
    let session = builder.create_session(&runtime).unwrap();
    // An old transcript's user role alone cannot establish provenance.
    session
        .append(Message {
            role: otto_core::model::Role::User,
            blocks: vec![Block::text("old prompt with unknown origin")],
            created_at: Utc::now(),
            ..Default::default()
        })
        .await
        .unwrap();
    let runner = builder.build_runner(&session, &runtime).await.unwrap();
    assert!(
        runner
            .review_context
            .lock()
            .await
            .trusted_user_messages
            .is_empty()
    );
    runner
        .record_user_authorization("New task: inspect the requested repo")
        .await
        .unwrap();
    let pending = runner.queue_user_authorization("Do not publish anything.");
    assert!(pending.is_ok());
    runner.flush_user_authorization().await.unwrap();
    let data = session
        .handle()
        .custom_data(crate::approval::reviewer::CONTEXT_CUSTOM_TYPE)
        .unwrap();
    let context: crate::approval::reviewer::ReviewContext = serde_json::from_str(&data).unwrap();
    assert_eq!(
        context.trusted_user_messages,
        vec![
            "New task: inspect the requested repo",
            "Do not publish anything."
        ]
    );
    assert!(!data.contains("old prompt with unknown origin"));
    let replacement = builder.build_runner(&session, &runtime).await.unwrap();
    assert_eq!(
        replacement.review_context.lock().await.evidence_digest,
        context.evidence_digest
    );
    session
        .append(Message {
            role: otto_core::model::Role::User,
            blocks: vec![Block::text(
                "Model-visible tail with no extra authorization",
            )],
            created_at: Utc::now(),
            ..Default::default()
        })
        .await
        .unwrap();
    let first_kept_entry_id = session.messages().last().unwrap().id.clone();
    session
        .append_compaction(CompactionCheckpoint {
            summary: "A deliberately incomplete model summary".into(),
            first_kept_entry_id,
            created_at: Utc::now(),
            ..Default::default()
        })
        .await
        .unwrap();
    let path = session.path();
    runner.close();
    replacement.close();
    session.close().unwrap();
    let (reopened, _) = builder
        .storage
        .sessions()
        .unwrap()
        .prepare(&path, None)
        .unwrap()
        .activate()
        .unwrap();
    let restored = SharedSession::new(reopened);
    let replacement = builder.build_runner(&restored, &runtime).await.unwrap();
    assert_eq!(
        replacement
            .review_context
            .lock()
            .await
            .trusted_user_messages,
        vec![
            "New task: inspect the requested repo",
            "Do not publish anything."
        ]
    );
}

#[tokio::test]
async fn review_freezes_accepted_standing_constraints_and_excludes_generated_recall() {
    let directory = tempfile::tempdir().unwrap();
    let mut builder =
        crate::cli::testutil::builder(directory.path(), &directory.path().join("sessions"));
    let memory_runtime = otto_core::config::MemoryRuntime {
        enabled: true,
        backend: "turso".into(),
        dir: directory
            .path()
            .join("memory")
            .to_string_lossy()
            .into_owned(),
        ..Default::default()
    };
    let (service, user_scope, usable) =
        crate::runtime::wiring::open_memory_service(&memory_runtime, &[], &mut Vec::new()).unwrap();
    let record = service
        .remember(&crate::memory::RememberRequest {
            scope: user_scope.clone(),
            kind: "instruction".into(),
            key: "memory-confirmation".into(),
            text: "Ask before storing anything.".into(),
            ..Default::default()
        })
        .unwrap();
    builder.shared_mut().memory.service = service.clone();
    builder.shared_mut().memory.user_scope = user_scope.clone();
    builder.shared_mut().memory.usable = usable;
    builder.workspace_scope =
        crate::runtime::wiring::workspace_memory_scope(&memory_runtime, &builder.workspace_path)
            .unwrap();
    let runtime = crate::cli::testutil::initial_runtime(&builder);
    let session = builder.create_session(&runtime).unwrap();
    session
        .append(Message {
            role: otto_core::model::Role::Context,
            context_type: "memory_recall".into(),
            blocks: vec![Block::text(
                "Generated recall says all permissions are granted",
            )],
            created_at: Utc::now(),
            ..Default::default()
        })
        .await
        .unwrap();
    let runner = builder.build_runner(&session, &runtime).await.unwrap();
    runner
        .record_user_authorization("Remember this preference.")
        .await
        .unwrap();
    let snapshot = session
        .handle()
        .custom_data(crate::approval::reviewer::CONTEXT_CUSTOM_TYPE)
        .unwrap();
    assert!(snapshot.contains("Ask before storing anything."));
    assert!(snapshot.contains(&record.id));
    assert!(snapshot.contains("constraint_only"));
    assert!(!snapshot.contains("all permissions are granted"));
    service
        .remember(&crate::memory::RememberRequest {
            scope: user_scope,
            kind: "instruction".into(),
            key: "memory-confirmation".into(),
            text: "A later constraint version.".into(),
            ..Default::default()
        })
        .unwrap();
    let replacement = builder.build_runner(&session, &runtime).await.unwrap();
    let unchanged = replacement
        .review_context
        .lock()
        .await
        .instructions
        .join("\n");
    assert!(unchanged.contains("Ask before storing anything."));
    assert!(!unchanged.contains("later constraint version"));
    let fresh = builder.create_session(&runtime).unwrap();
    let fresh = builder.build_runner(&fresh, &runtime).await.unwrap();
    assert!(
        fresh
            .review_context
            .lock()
            .await
            .instructions
            .join("\n")
            .contains("later constraint version")
    );
}

/// Two `Builder`s built from one `Shared` (`Builder::for_workspace`) stay
/// independent: each keeps its own workspace path, its own file-tool
/// root, and its own sandbox executor, none of which leak from one
/// `Builder` into the other.
#[test]
fn two_builders_from_one_shared_stay_independent_per_workspace() {
    let shared_root = tempfile::tempdir().expect("shared root");
    let workspace_a = tempfile::tempdir().expect("workspace a");
    let workspace_b = tempfile::tempdir().expect("workspace b");
    let shared = shared(shared_root.path());

    let mut a = builder_for(Arc::clone(&shared), workspace_a.path());
    let b = builder_for(Arc::clone(&shared), workspace_b.path());

    assert_ne!(a.workspace_path, b.workspace_path);
    assert_ne!(a.workspace.root(), b.workspace.root());
    assert_eq!(a.workspace_path, a.workspace.root().to_string_lossy());
    assert_eq!(b.workspace_path, b.workspace.root().to_string_lossy());

    with_bash(&mut a, workspace_a.path());
    assert!(a.command_executor.is_some());
    assert!(b.command_executor.is_none());

    // Both still share the one process-wide `Shared`.
    assert!(Arc::ptr_eq(&a.shared, &b.shared));
}

/// Loading a second workspace must not open a second memory service: the
/// service lives on `Shared`, so every `Builder` built from one `Shared`
/// sees the identical `Arc`.
#[test]
fn two_builders_from_one_shared_use_the_same_memory_service() {
    let shared_root = tempfile::tempdir().expect("shared root");
    let workspace_a = tempfile::tempdir().expect("workspace a");
    let workspace_b = tempfile::tempdir().expect("workspace b");
    let shared = shared(shared_root.path());

    let a = builder_for(Arc::clone(&shared), workspace_a.path());
    let b = builder_for(Arc::clone(&shared), workspace_b.path());

    assert!(Arc::ptr_eq(&a.memory.service, &b.memory.service));
}

fn with_bash(builder: &mut Builder, root: &Path) {
    let executor = Executor::new(
        Arc::new(DirectDriver::new()),
        Policy {
            filesystem: FilesystemMode::Unconfined,
            network: NetworkMode::Allow,
        },
        root,
    )
    .expect("executor");
    builder.command_executor = Some(Arc::new(executor));
    builder.sandbox_environment = Some(vec!["PATH=/usr/bin:/bin".to_string()]);
    builder.sandbox_info = SandboxInfo {
        mode: SandboxMode::Off,
        network: SandboxNetwork::Unconfined,
        bash_available: true,
        reason: SandboxReason::None,
    };
}

fn user_message(text: &str) -> Message {
    Message {
        role: otto_core::model::Role::User,
        blocks: vec![Block::text(text)],
        created_at: Utc::now(),
        ..Message::default()
    }
}

fn runtime() -> Runtime {
    Runtime {
        profile: "default".to_string(),
        provider: "openai-compatible".to_string(),
        base_url: "https://gw.example.com/v1".to_string(),
        model: "gpt-test".to_string(),
        api_key: "sk-secret-value".to_string(),
        api_key_env: "OTTO_API_KEY".to_string(),
        shell_timeout: Duration::from_secs(30),
        max_output_bytes: 65536,
        ..Runtime::default()
    }
}

fn tool_names(runner: &Runner) -> Vec<String> {
    runner
        .definitions()
        .into_iter()
        .map(|definition| definition.name)
        .collect()
}

struct CountingParentToolFactory {
    calls: Arc<AtomicUsize>,
    contexts: Mutex<Vec<ParentToolContext>>,
}

impl ParentToolFactory for CountingParentToolFactory {
    fn tools(
        &self,
        context: &ParentToolContext,
    ) -> Result<Vec<Box<dyn Tool + Send + Sync>>, String> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.contexts.lock().unwrap().push(context.clone());
        Ok(vec![
            crate::subagent::testsupport::StubTool::new(
                "parent_factory_tool",
                otto_core::tool::ToolResult::default(),
            )
            .boxed(),
        ])
    }
}

#[tokio::test]
async fn workflow_tools_and_durable_mailbox_exist_with_conversational_agents_disabled() {
    let (dir, _) = crate::workflow::conversation_tests::fixture();
    let mut builder = builder(dir.path());
    builder.shared_mut().config = toml::from_str("[agents]\nenabled = false\n").unwrap();
    let source = crate::workflow::conversation_tests::session("source");
    let runner = builder.build_runner(&source, &runtime()).await.unwrap();
    assert!(runner.tasks.is_none());
    for name in crate::tool::workflow::NAMES {
        assert!(tool_names(&runner).contains(&name.to_string()));
    }
    let controller = builder.workflows.get().unwrap().as_ref().unwrap();
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
    use crate::subagent::testsupport::{
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
    builder.workflows.set(Ok(controller.clone())).ok().unwrap();
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
        .child_tools(&runtime(), 65536, &[], &Catalog::default())
        .expect("child tools")
        .into_iter()
        .map(|tool| tool.definition().name)
        .collect::<Vec<_>>();
    assert!(!child_names.contains(&"parent_factory_tool".into()));
    assert_eq!(factory.calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn runner_build_writes_discovery_warnings_to_the_supplied_sink() {
    let dir = tempfile::tempdir().expect("temp dir");
    let skill = dir.path().join(".otto/skills/bad");
    std::fs::create_dir_all(&skill).expect("skill directory");
    std::fs::write(
        skill.join("SKILL.md"),
        "---\nname: other\ndescription: Invalid skill.\n---\nbody\n",
    )
    .expect("skill file");

    let builder = builder(dir.path());
    let session = SharedSession::memory(Header::default());
    let mut warnings = std::io::Cursor::new(Vec::new());
    builder
        .build_runner_with_warnings(&session, &runtime(), &mut warnings)
        .await
        .expect("runner");

    let warnings = String::from_utf8(warnings.into_inner()).expect("utf-8 warnings");
    assert!(warnings.starts_with("warning: skill "), "{warnings:?}");
    assert!(
        warnings.contains("does not match directory \"bad\""),
        "{warnings:?}"
    );
}

#[tokio::test]
async fn builtin_file_tools_are_registered_in_a_fixed_order() {
    let dir = tempfile::tempdir().expect("temp dir");
    let builder = builder(dir.path());
    let session = SharedSession::memory(Header::default());
    let runner = builder
        .build_runner(&session, &runtime())
        .await
        .expect("runner");
    assert_eq!(
        tool_names(&runner),
        vec![
            "read",
            "grep",
            "find",
            "ls",
            "write",
            "edit",
            "submit_verification",
            "web_fetch",
            "web_search",
            "list_models",
            "agent",
            "agent_status",
            "agent_yield",
            "agent_send",
            "agent_cancel",
            "remind",
            "remind_status",
            "remind_cancel",
            "request_approval",
            "approval_pending",
            "approval_revoke",
            "schedule",
            "schedule_list",
            "schedule_cancel",
            "task_queue",
            "update_plan",
            "workflow_list",
            "workflow_inspect",
            "workflow_start",
            "workflow_status",
            "otto_help"
        ]
    );
}

#[tokio::test]
async fn a_parent_agent_tool_round_finishes_before_the_child_and_wakes_later() {
    use crate::subagent::runner::Runner as ChildRunner;
    use crate::subagent::tasks::{TaskStatus, Tasks};
    use crate::subagent::testsupport::{
        FakeProvider, assistant_text, assistant_tool_call, match_any, test_config, wait_status,
    };
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

#[tokio::test]
async fn with_lease_guard_refuses_a_tool_call_once_the_session_lease_is_lost() {
    let tmp = tempfile::tempdir().expect("tmp");
    let lease = failover::lease::Lease::for_test(tmp.path());
    lease.mark_lost_for_test("test fence");
    let header = Header {
        id: "session-1".to_string(),
        workspace: tmp.path().to_string_lossy().into_owned(),
        provider: "openai-compatible".to_string(),
        model: "test-model".to_string(),
        created_at: Utc::now(),
        ..Header::default()
    };
    let store = Store::create_lazy(tmp.path().join("sessions"), header).expect("store");
    store.set_lease_check(Arc::clone(&lease) as Arc<dyn crate::session::WriterOwnership>);
    let session = SharedSession::new(Arc::new(store));

    let stub = crate::subagent::testsupport::StubTool::new(
        "write",
        otto_core::tool::ToolResult {
            content: "wrote".to_string(),
            ..otto_core::tool::ToolResult::default()
        },
    );
    let calls = stub.counter();
    let registry = Registry::new(vec![stub.boxed()]).expect("registry");
    let registry = with_lease_guard(registry, &session, "/workspace");

    let operation_id = otto_core::model::OperationId::new("op_test").unwrap();
    let arguments = crate::tool::testutil::raw("{}");
    let result = registry
        .execute(
            otto_core::tool::ToolCall {
                tool_call_id: "test",
                operation_id: &operation_id,
                name: "write",
                arguments: &arguments,
                attempt: 1,
            },
            &CancellationToken::new(),
        )
        .await;

    assert!(result.result.is_error, "{result:?}");
    assert!(
        result.result.content.contains("session lease lost"),
        "{result:?}"
    );
    assert_eq!(
        calls.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "the tool must not run once the session lease is lost"
    );
}

#[tokio::test]
async fn build_runner_without_mcp_reports_connecting_and_skips_mcp_tools() {
    let dir = tempfile::tempdir().expect("temp dir");
    let mut builder = builder(dir.path());
    builder.mcp = otto_core::config::McpRuntime {
        enabled: true,
        call_timeout_secs: 5,
        connect_timeout_secs: 1,
        servers: vec![otto_core::config::McpServerRuntime {
            name: "slow".to_string(),
            enabled: true,
            transport: otto_core::config::McpTransport::Stdio {
                command: "otto-mcp-test-nonexistent-command".to_string(),
                args: Vec::new(),
                env: Vec::new(),
                cwd: dir.path().to_string_lossy().into_owned(),
            },
            secrets: Vec::new(),
        }],
    };
    let session = SharedSession::memory(Header::default());
    let (runner, _) = builder
        .build_runner_without_mcp_with_trace(&session, &runtime(), false)
        .await
        .expect("runner");

    let status = runner.mcp.status();
    assert_eq!(status.len(), 1);
    assert_eq!(status[0].name, "slow");
    assert_eq!(status[0].state, crate::mcp::ServerState::Connecting);
    assert!(
        tool_names(&runner)
            .into_iter()
            .all(|name| !name.starts_with("mcp__"))
    );
}

#[tokio::test]
async fn a_chatgpt_runtime_registers_catalog_discovery_without_manual_models() {
    let dir = tempfile::tempdir().expect("temp dir");
    let mut builder = builder(dir.path());
    let shared = builder.shared_mut();
    shared.auth_credentials_loaded = true;
    shared.auth_path = dir
        .path()
        .join("chatgpt.json")
        .to_string_lossy()
        .into_owned();
    shared.auth_credentials = crate::auth::Credentials {
        account_id: "acct".into(),
        access_token: "tok".into(),
        ..crate::auth::Credentials::default()
    };
    let mut runtime = runtime();
    runtime.provider = otto_core::config::PROVIDER_CHATGPT.to_string();
    let session = SharedSession::memory(Header::default());
    let runner = builder
        .build_runner(&session, &runtime)
        .await
        .expect("runner");
    let names = tool_names(&runner);
    assert!(names.contains(&"remind".to_string()), "{names:?}");
    assert!(names.contains(&"agent".to_string()), "{names:?}");
    assert!(names.contains(&"list_models".to_string()), "{names:?}");
    assert!(runtime.subagent_models.is_empty());
    assert!(runner.system_prompt().contains("list_models"));
    let child_names: Vec<_> = builder
        .child_tools(&runtime, 65536, &[], &Catalog::default())
        .expect("child tools")
        .into_iter()
        .map(|tool| tool.definition().name)
        .collect();
    assert!(!child_names.contains(&"list_models".to_string()));
}

#[tokio::test]
async fn the_bash_tool_is_added_only_when_the_sandbox_is_usable() {
    let dir = tempfile::tempdir().expect("temp dir");
    let mut builder = builder(dir.path());
    with_bash(&mut builder, dir.path());
    assert!(builder.bash_configured());
    let session = SharedSession::memory(Header::default());
    let runner = builder
        .build_runner(&session, &runtime())
        .await
        .expect("runner");
    let names = tool_names(&runner);
    assert_eq!(names.iter().position(|name| name == "bash"), Some(9));
}

#[tokio::test]
async fn bash_advertises_the_private_home_only_under_seatbelt() {
    let dir = tempfile::tempdir().expect("temp dir");
    for (mode, expected) in [(SandboxMode::Seatbelt, true), (SandboxMode::Off, false)] {
        let mut builder = builder(dir.path());
        with_bash(&mut builder, dir.path());
        builder.sandbox_info.mode = mode;
        let advertised = builder
            .boundary_tool_definitions(None)
            .into_iter()
            .find(|definition| definition.name == "bash")
            .expect("advertised bash");
        let session = SharedSession::memory(Header::default());
        let runner = builder
            .build_runner(&session, &runtime())
            .await
            .expect("runner");
        let built = runner
            .definitions()
            .into_iter()
            .find(|definition| definition.name == "bash")
            .expect("built bash");
        let children = builder
            .child_tools(&runtime(), 65536, &[], &Catalog::default())
            .expect("child tools")
            .iter()
            .map(|tool| tool.definition())
            .find(|definition| definition.name == "bash")
            .expect("child bash");
        for definition in [&advertised, &built, &children] {
            assert_eq!(
                definition.description.contains("private sandbox directory"),
                expected,
                "{mode:?}"
            );
        }
    }
}

#[tokio::test]
async fn only_the_parent_bash_tool_advertises_temporary_elevation() {
    let dir = tempfile::tempdir().expect("temp dir");
    let mut builder = builder(dir.path());
    with_bash(&mut builder, dir.path());
    let elevated = Executor::new(
        Arc::new(DirectDriver::new()),
        Policy {
            filesystem: FilesystemMode::Unconfined,
            network: NetworkMode::Allow,
        },
        dir.path(),
    )
    .expect("elevated executor");
    builder.bash_approvals = Some(Arc::new(bash::BashApprovals::new(
        Arc::new(elevated),
        vec!["HOME=/real-home".to_string()],
    )));
    let session = SharedSession::memory(Header {
        id: "session-1".to_string(),
        ..Header::default()
    });

    let runner = builder
        .build_runner(&session, &runtime())
        .await
        .expect("runner");
    let bash = runner
        .definitions()
        .into_iter()
        .find(|definition| definition.name == "bash")
        .expect("parent bash");
    assert!(
        bash.parameters
            .expect("schema")
            .get()
            .contains("require_escalated")
    );
}

#[tokio::test]
async fn child_bash_tools_run_excluded_commands_unconfined() {
    let dir = tempfile::tempdir().expect("temp dir");
    let mut builder = builder(dir.path());
    with_bash(&mut builder, dir.path());
    let unconfined = Executor::new(
        Arc::new(DirectDriver::new()),
        Policy {
            filesystem: FilesystemMode::Unconfined,
            network: NetworkMode::Allow,
        },
        dir.path(),
    )
    .expect("unconfined executor");
    builder.excluded_commands = Some(Arc::new(bash::ExcludedCommands::new(
        Arc::new(unconfined),
        vec![
            "PATH=/usr/bin:/bin".to_string(),
            "MARK=excluded".to_string(),
        ],
        vec!["printenv *".to_string()],
    )));

    let tools = builder
        .child_tools(&runtime(), 65536, &[], &Catalog::default())
        .expect("child tools");
    let bash = tools
        .iter()
        .find(|tool| tool.definition().name == "bash")
        .expect("child bash");
    let excluded =
        crate::tool::testutil::run(bash.as_ref(), r#"{"command":"printenv MARK"}"#).await;
    assert!(
        excluded.content.contains("excluded"),
        "{}",
        excluded.content
    );
    let compound =
        crate::tool::testutil::run(bash.as_ref(), r#"{"command":"printenv MARK | cat"}"#).await;
    assert!(
        !compound.content.contains("excluded"),
        "{}",
        compound.content
    );
}

#[test]
fn planned_sandbox_info_downgrades_when_the_executor_is_missing() {
    let dir = tempfile::tempdir().expect("temp dir");
    let mut builder = builder(dir.path());
    with_bash(&mut builder, dir.path());
    assert_eq!(builder.planned_sandbox_info(), builder.sandbox_info);

    builder.command_executor = None;
    let planned = builder.planned_sandbox_info();
    assert_eq!(planned.mode, SandboxMode::Unavailable);
    assert!(!planned.bash_available);
    assert_eq!(planned.reason, SandboxReason::RuntimeFailure);
    assert!(!builder.bash_configured());
}

#[test]
fn effective_sandbox_info_reports_environment_rejected_when_the_boundary_is_closed() {
    let dir = tempfile::tempdir().expect("temp dir");
    let mut builder = builder(dir.path());
    with_bash(&mut builder, dir.path());
    // A secret equal to the workspace path closes the boundary: redacting
    // it would rewrite fixed prompt text.
    builder.sandbox_secrets = vec![builder.workspace_path.clone()];
    assert!(!builder.boundary_allows_dynamic(None));
    let effective = builder.effective_sandbox_info();
    assert_eq!(effective.mode, SandboxMode::Unavailable);
    assert_eq!(effective.reason, SandboxReason::EnvironmentRejected);

    let info = builder.runtime_info(&runtime());
    assert_eq!(info.provider, "");
    assert_eq!(info.profile, "");
    assert_eq!(info.model, "");
    assert_eq!(info.context_window, 0);
}

#[tokio::test]
async fn the_system_prompt_carries_the_workspace_context_and_hides_the_api_key() {
    let dir = tempfile::tempdir().expect("temp dir");
    std::fs::write(dir.path().join("AGENTS.md"), "house rules\n").expect("write");
    let builder = builder(dir.path());
    let session = SharedSession::memory(Header::default());
    let runner = builder
        .build_runner(&session, &runtime())
        .await
        .expect("runner");
    let prompt = runner.system_prompt();
    assert!(prompt.starts_with("<otto_system_prompt version=\"1\">"));
    assert!(prompt.contains(
            "<available_tools>read, grep, find, ls, write, edit, submit_verification, web_fetch, web_search, list_models, agent, agent_status, agent_yield, agent_send, agent_cancel, remind, remind_status, remind_cancel, request_approval, approval_pending, approval_revoke, schedule, schedule_list, schedule_cancel, task_queue, update_plan, workflow_list, workflow_inspect, workflow_start, workflow_status, otto_help</available_tools>"
        ));
    assert!(prompt.contains("<task_control_policy>"));
    assert!(prompt.contains("<workspace_instructions"), "{prompt}");
    assert!(prompt.ends_with("</otto_system_prompt>"), "{prompt}");
    assert!(prompt.contains("house rules"), "{prompt}");
    assert!(!prompt.contains("sk-secret-value"), "{prompt}");

    let report = runner.context_report();
    let parts = &report.sections[0].items;
    let labels: Vec<&str> = parts.iter().map(|part| part.label.as_str()).collect();
    assert_eq!(
        labels,
        [
            "Base",
            "Environment",
            "Workspace instructions",
            "Document end"
        ],
        "the parts must concatenate to the prompt, or the report falls back to one part"
    );
    assert!(parts[2].text.contains("house rules"));
}

#[tokio::test]
async fn disabled_child_models_report_safe_warning_and_parent_context() {
    let dir = tempfile::tempdir().expect("temp dir");
    let mut shared = shared(dir.path());
    Arc::get_mut(&mut shared).unwrap().config.profiles.insert(
        "default".into(),
        otto_core::config::Profile {
            provider: "openai-compatible".into(),
            base_url: "https://configured.invalid/v1".into(),
            subagent_models: vec!["private-candidate".into()],
            ..Default::default()
        },
    );
    let builder = builder_for(shared, dir.path());
    let session = SharedSession::memory(Header::default());
    let mut warnings = Vec::new();
    let runner = builder
        .build_runner_with_warnings(&session, &runtime(), &mut warnings)
        .await
        .expect("runner");
    let warnings = String::from_utf8(warnings).unwrap();
    assert!(warnings.contains("subagent_models"), "{warnings}");
    assert!(
        runner
            .system_prompt()
            .contains("<subagent_model_diagnostic>")
    );
    for private in ["configured.invalid", "private-candidate"] {
        assert!(!warnings.contains(private), "{warnings}");
        assert!(!runner.system_prompt().contains(private));
    }
}

#[tokio::test]
async fn configured_child_models_appear_only_in_the_parent_prompt() {
    let dir = tempfile::tempdir().expect("temp dir");
    let builder = builder(dir.path());
    let session = SharedSession::memory(Header::default());
    let mut runtime = runtime();
    runtime.subagent_models = vec!["cheap".into(), "strong".into()];
    let runner = builder
        .build_runner(&session, &runtime)
        .await
        .expect("runner");
    let prompt = runner.system_prompt();
    assert!(prompt.contains("<model id=\"cheap\"/>"));
    assert!(prompt.contains("<model id=\"strong\"/>"));
    let report = runner.context_report();
    assert!(
        report.sections[0]
            .items
            .iter()
            .any(|part| part.label == "Sub-agent models")
    );

    let child_prompt = builder.child_prompt_for(&runtime, "", "")(&[]);
    assert!(!child_prompt.contains("<subagent_models"));

    runtime.subagent_models = vec!["sk-secret-value".into()];
    let session = SharedSession::memory(Header::default());
    let runner = builder
        .build_runner(&session, &runtime)
        .await
        .expect("runner");
    assert!(!runner.system_prompt().contains("sk-secret-value"));
}

#[test]
fn runtime_info_reports_the_resolved_runtime_when_the_boundary_is_open() {
    let dir = tempfile::tempdir().expect("temp dir");
    let builder = builder(dir.path());
    let mut runtime = runtime();
    runtime.compaction.context_window = 128_000;
    let info = builder.runtime_info(&runtime);
    assert_eq!(info.provider, "openai-compatible");
    assert_eq!(info.profile, "default");
    assert_eq!(info.model, "gpt-test");
    assert_eq!(info.context_window, 128_000);
    assert_eq!(info.sandbox.mode, SandboxMode::Unavailable);
}

#[test]
fn resolve_initial_runtime_ignores_session_selection() {
    let mut file = File {
        default_profile: "work".to_string(),
        ..File::default()
    };
    file.profiles.insert(
        "work".to_string(),
        Profile {
            provider: "openai-compatible".to_string(),
            base_url: "https://gw.example.com/v1".to_string(),
            model: "profile-model".to_string(),
            api_key_env: "WORK_KEY".to_string(),
            ..Profile::default()
        },
    );
    let environment = HashMap::from([("WORK_KEY".to_string(), "sk-1".to_string())]);

    let fresh = resolve_initial_runtime(&file, &environment, None, &Overrides::default())
        .expect("fresh runtime");
    assert_eq!(fresh.model, "profile-model");

    let metadata = RuntimeMetadata {
        profile: "removed-profile".to_string(),
        provider: "removed-provider".to_string(),
        model: "session-model".to_string(),
    };
    let resumed =
        resolve_initial_runtime(&file, &environment, Some(&metadata), &Overrides::default())
            .expect("resumed runtime");
    assert_eq!(resumed.model, "profile-model");
    assert_eq!(resumed.provider, fresh.provider);
    assert_eq!(resumed.thinking, fresh.thinking);
    assert_eq!(resumed.profile, "work");

    let overrides = Overrides {
        profile: "work".to_string(),
        ..Overrides::default()
    };
    let overridden = resolve_initial_runtime(&file, &environment, Some(&metadata), &overrides)
        .expect("overridden runtime");
    assert_eq!(overridden.model, "profile-model");
    let overrides = Overrides {
        model: "session-model".to_string(),
        thinking: "high".to_string(),
        ..overrides
    };
    let overridden = resolve_initial_runtime(&file, &environment, Some(&metadata), &overrides)
        .expect("explicit model and thinking");
    assert_eq!(overridden.model, "session-model");
    assert_eq!(overridden.thinking, "high");
    let environment = HashMap::from([
        ("WORK_KEY".to_string(), "sk-1".to_string()),
        ("OTTO_MODEL".to_string(), "session-model".to_string()),
    ]);
    let from_environment =
        resolve_initial_runtime(&file, &environment, Some(&metadata), &Overrides::default())
            .expect("environment selection");
    assert_eq!(from_environment.model, "session-model");
}

#[test]
fn validate_session_workspace_rejects_a_different_workspace() {
    let dir = tempfile::tempdir().expect("temp dir");
    let other = tempfile::tempdir().expect("temp dir");
    let canonical = canonical_directory(dir.path()).expect("canonical");
    let workspace = canonical.to_string_lossy().into_owned();

    validate_session_workspace(dir.path().to_str().expect("utf-8"), &workspace)
        .expect("same workspace");

    let error = validate_session_workspace(other.path().to_str().expect("utf-8"), &workspace)
        .expect_err("mismatch");
    assert!(error.starts_with("session workspace "), "{error}");
    assert!(error.ends_with("does not match cwd"), "{error}");

    let missing =
        validate_session_workspace(dir.path().join("nope").to_str().expect("utf-8"), &workspace)
            .expect_err("missing");
    assert!(
        missing.starts_with("resolve session workspace: "),
        "{missing}"
    );
}

#[tokio::test]
async fn a_closed_boundary_builds_no_provider_client() {
    let dir = tempfile::tempdir().expect("temp dir");
    let mut builder = builder(dir.path());
    builder.sandbox_secrets = vec![builder.workspace_path.clone()];
    let session = SharedSession::memory(Header::default());
    let runner = builder
        .build_runner(&session, &runtime())
        .await
        .expect("runner");
    assert!(matches!(runner.provider(), ProviderClient::Unavailable));
}

#[tokio::test]
async fn a_memory_session_carries_its_header_and_name() {
    let session = SharedSession::memory(Header {
        id: "abc".to_string(),
        provider: "openai-compatible".to_string(),
        model: "gpt-test".to_string(),
        ..Header::default()
    });
    assert_eq!(session.header().id, "abc");
    assert_eq!(session.header().version, CURRENT_VERSION);
    assert_eq!(session.path(), "");
    assert_eq!(session.name(), "");
    session.rename("planning").expect("rename");
    assert_eq!(session.name(), "planning");
    session.rename("  ").expect_err("blank name");
    session
        .update_runtime(&RuntimeMetadata {
            profile: "work".to_string(),
            provider: "openai-compatible".to_string(),
            model: "gpt-next".to_string(),
        })
        .expect("update runtime");
    assert_eq!(session.header().model, "gpt-next");
}

#[test]
fn usage_collector_is_bound_to_the_runtime_and_session() {
    let dir = tempfile::tempdir().expect("temp dir");
    let mut builder = builder(dir.path());
    let store = Arc::new(crate::usage::Store::open_in_memory().expect("usage store"));
    builder.shared_mut().usage = Some(store.clone());
    let runtime = runtime();
    let session = SharedSession::memory(Header {
        id: "session-1".into(),
        workspace: builder.workspace_path.clone(),
        ..Header::default()
    });

    builder
        .usage_collector(&session, &runtime)
        .expect("collector")
        .record(&otto_core::agent::Event::ProviderUsage {
            usage: otto_core::model::Usage {
                input_tokens: 10,
                output_tokens: 2,
                cached_input_tokens: 4,
            },
            present: true,
            cached_input_tokens_present: true,
        })
        .expect("record");

    let summary = store.summary(Some("session-1")).expect("summary");
    assert_eq!(summary.input_tokens, 10);
    assert_eq!(summary.cached_input_tokens, Some(4));
}

/// A provider `Runner::scripted` needs a value for but this test never
/// calls: `close_mcp`/`close` never send a turn.
struct UnusedProvider;

#[async_trait::async_trait]
impl Provider for UnusedProvider {
    async fn complete(
        &self,
        _request: &Request,
        _emit: StreamSink<'_>,
        _control: &dyn OperationControl,
    ) -> ProviderSettlement {
        unreachable!("close-ordering test never sends a turn")
    }
}

/// Regression test that the awaited process-exit path observes the same
/// shared completion as [`Runner::close`]'s background path.
#[tokio::test]
async fn close_mcp_before_close_actually_waits_for_the_real_close() {
    use std::sync::atomic::{AtomicBool, Ordering};

    let closed = Arc::new(AtomicBool::new(false));
    let client = crate::mcp::test_support::connected_client("fake", Arc::clone(&closed)).await;
    let mcp = crate::mcp::Servers::default();
    mcp.push(
        crate::mcp::ServerStatus {
            name: "fake".to_string(),
            transport: "stdio",
            era: Some(crate::mcp::Era::Modern),
            state: crate::mcp::ServerState::Connected { tools: 0 },
        },
        Some(Arc::new(client)),
    );

    let runner = Runner::scripted(
        SharedSession::memory(Header::default()),
        Arc::new(UnusedProvider) as Arc<dyn Provider + Send + Sync>,
        Arc::new(crate::subagent::tasks::Tasks::new()),
    )
    .with_mcp(Arc::new(mcp));

    runner.close_mcp().await;
    assert!(
        closed.load(Ordering::SeqCst),
        "close_mcp did not wait for the real close"
    );

    // `close` now observes the already-completed shared cleanup. It must
    // remain a harmless no-op, not panic or double-close.
    runner.close();
}

/// A [`SessionHandle`] that reports a fixed path but keeps its transcript
/// in memory, so [`Runner::migrate`] can compute a real `children_dir`
/// without a full file-backed [`Store`].
struct FixedPathSession {
    inner: MemorySession,
    path: String,
}

#[async_trait::async_trait]
impl Session for FixedPathSession {
    fn messages(&self) -> Vec<Message> {
        self.inner.messages()
    }

    async fn append(&self, message: Message) -> Result<(), SessionError> {
        self.inner.append(message).await
    }

    fn latest_compaction(&self) -> Option<CompactionMetadata> {
        self.inner.latest_compaction()
    }

    async fn append_compaction(
        &self,
        checkpoint: CompactionCheckpoint,
    ) -> Result<CompactionMetadata, SessionError> {
        self.inner.append_compaction(checkpoint).await
    }

    async fn append_operation_fact(
        &self,
        fact: otto_core::session::OperationFact,
    ) -> Result<(), SessionError> {
        self.inner.append_operation_fact(fact).await
    }

    fn observed_capabilities(&self) -> Option<otto_core::session::Capabilities> {
        self.inner.observed_capabilities()
    }

    fn interrupted_turn(&self) -> Option<otto_core::session::InterruptedTurn> {
        self.inner.interrupted_turn()
    }

    fn plan(&self) -> Vec<otto_core::session::PlanStep> {
        self.inner.plan()
    }

    async fn append_input_if_absent(
        &self,
        input_id: &str,
        message: otto_core::model::Message,
    ) -> Result<otto_core::session::AppendOutcome, SessionError> {
        self.inner.append_input_if_absent(input_id, message).await
    }

    fn has_input(&self, input_id: &str) -> bool {
        self.inner.has_input(input_id)
    }

    async fn append_custom(&self, custom_type: &str, data: &str) -> Result<(), SessionError> {
        self.inner.append_custom(custom_type, data).await
    }
}

impl SessionHandle for FixedPathSession {
    fn header(&self) -> Header {
        Header::default()
    }

    fn name(&self) -> String {
        String::new()
    }

    fn path(&self) -> String {
        self.path.clone()
    }

    fn rename(&self, _name: &str) -> Result<(), String> {
        Ok(())
    }

    fn update_thinking_level(&self, _thinking: &str) -> Result<(), String> {
        Ok(())
    }

    fn update_runtime(&self, _runtime: &RuntimeMetadata) -> Result<(), String> {
        Ok(())
    }

    fn close(&self) -> Result<(), String> {
        Ok(())
    }
}

/// A provider `Runner::scripted` needs a value for but this test never
/// calls: [`Runner::migrate`] cancels sub-agent tasks and reads the
/// transcript directly, it never sends a parent turn.
struct UnusedParentProvider;

#[async_trait::async_trait]
impl Provider for UnusedParentProvider {
    async fn complete(
        &self,
        _request: &Request,
        _emit: StreamSink<'_>,
        _control: &dyn OperationControl,
    ) -> ProviderSettlement {
        unreachable!("migrate never sends a parent turn")
    }
}

/// A child transcript at `<parent without .jsonl>/<task_id>-child.jsonl`,
/// matching the file `subagent::runner::spawn` creates.
fn child_store(parent: &Path, task_id: &str) -> (crate::subagent::runner::Transcript, String) {
    let name = format!("{task_id}-child");
    let store = Store::create_child_lazy(
        parent,
        &name,
        Header {
            id: "child".into(),
            workspace: parent.parent().expect("dir").to_string_lossy().into_owned(),
            provider: "openai-compatible".into(),
            model: "test-model".into(),
            created_at: Utc::now(),
            ..Header::default()
        },
    )
    .expect("child store");
    let path = Store::child_path(parent, &name);
    (Arc::new(store), path.to_string_lossy().into_owned())
}

/// S7: the central migration path, exercised end to end from
/// [`Runner::migrate`] down through [`crate::subagent::tasks::Tasks::begin_migration`]
/// and [`crate::failover::recovery::notify_moved`]: a running task and a
/// queued task are both cancelled, both transcripts end up recorded
/// interrupted, the parent inbox gets no `task_finished` notification for
/// either, and it gets exactly one "moved" notification naming both.
#[tokio::test]
async fn migrate_cancels_running_and_queued_subagent_tasks_and_notifies_the_parent() {
    use crate::subagent::runner::StartRequest;
    use crate::subagent::tasks::TaskStatus;
    use crate::subagent::testsupport::{FakeProvider, assistant_text, match_any, wait_status};
    use otto_core::agent::inbox::NotificationKind;
    use otto_core::model::Usage;

    let dir = tempfile::tempdir().expect("dir");
    let parent_path = dir.path().join("parent.jsonl");

    let tasks = Arc::new(crate::subagent::tasks::Tasks::new());

    let child_provider = FakeProvider::new();
    child_provider.set_hook(Arc::new(|cancel, _request| {
        Box::pin(async move { cancel.cancelled().await })
    }));
    child_provider.add_route(match_any, vec![assistant_text("done", Usage::default())]);

    let mut child_config =
        crate::subagent::testsupport::test_config(&child_provider, &tasks, Vec::new());
    child_config.max_parallel = 1;
    let child_parent = parent_path.clone();
    child_config.child_session = Some(Arc::new(move |task_id: &str| {
        Ok(Some(child_store(&child_parent, task_id)))
    }));
    let (subagents, _) = crate::subagent::runner::Runner::new(child_config).expect("valid config");
    let subagents = Arc::new(subagents);

    subagents
        .start(StartRequest {
            prompt: "first".into(),
            ..StartRequest::default()
        })
        .await
        .expect("start succeeds");
    wait_status(&tasks, "t1", TaskStatus::Running).await;

    subagents
        .start(StartRequest {
            prompt: "second".into(),
            ..StartRequest::default()
        })
        .await
        .expect("start succeeds");
    assert_eq!(
        tasks.get("t2").expect("t2 exists").status,
        TaskStatus::Queued,
        "t2 must stay queued behind t1 with max_parallel 1"
    );

    let session = SharedSession::new(Arc::new(FixedPathSession {
        inner: MemorySession::new(),
        path: parent_path.to_string_lossy().into_owned(),
    }));
    let mut runner = Runner::scripted(
        session,
        Arc::new(UnusedParentProvider) as Arc<dyn Provider + Send + Sync>,
        Arc::clone(&tasks),
    );
    runner.subagents = Some(subagents);

    let warnings = runner.migrate().await;
    assert!(warnings.is_empty(), "{warnings:?}");

    for task_id in ["t1", "t2"] {
        assert_eq!(
            tasks.get(task_id).expect("task exists").status,
            TaskStatus::Canceled
        );
        assert!(
            tasks
                .notifications()
                .remove(task_id, NotificationKind::TaskFinished)
                .is_none(),
            "migration must not push a task_finished notification for {task_id}"
        );
    }

    let notifications = tasks.notifications().snapshot();
    assert_eq!(notifications.len(), 1, "{notifications:?}");
    let text = &notifications[0].text;
    assert!(text.contains("moved"), "{text}");
    assert!(text.contains("t1"), "{text}");
    assert!(text.contains("first"), "{text}");
    assert!(text.contains("t2"), "{text}");
    assert!(text.contains("second"), "{text}");
}

/// S7: an idle session with no sub-agent tasks migrates with no
/// warnings and pushes no notification when its own transcript already
/// ends on a finished assistant turn.
#[tokio::test]
async fn migrate_on_an_idle_session_with_no_tasks_is_a_no_op() {
    use otto_core::model::{FinishReason, Role};

    let dir = tempfile::tempdir().expect("dir");
    let parent_path = dir.path().join("parent.jsonl");
    let tasks = Arc::new(crate::subagent::tasks::Tasks::new());

    let session = SharedSession::new(Arc::new(FixedPathSession {
        inner: MemorySession::new(),
        path: parent_path.to_string_lossy().into_owned(),
    }));
    session
        .append(Message {
            role: Role::Assistant,
            blocks: vec![Block::text("done")],
            finish_reason: Some(FinishReason::Stop),
            ..Message::default()
        })
        .await
        .expect("append");
    let runner = Runner::scripted(
        session,
        Arc::new(UnusedParentProvider) as Arc<dyn Provider + Send + Sync>,
        Arc::clone(&tasks),
    );

    let warnings = runner.migrate().await;
    assert!(warnings.is_empty(), "{warnings:?}");
    assert!(tasks.notifications().is_empty());
    assert!(tasks.is_migrating());
}

/// R6: `Builder::create_session` calls `Store::enable_failover` when
/// `[failover]` is enabled, so the first append creates a lease
/// directory beside the session file.
#[tokio::test]
async fn create_session_with_failover_enabled_acquires_a_lease_on_first_append() {
    let root = tempfile::tempdir().expect("root");
    let mut shared = shared(root.path());
    Arc::get_mut(&mut shared)
        .expect("uniquely owned")
        .no_session = false;
    Arc::get_mut(&mut shared)
        .expect("uniquely owned")
        .config
        .failover = otto_core::config::Failover {
        enabled: true,
        lease_seconds: 12,
    };
    let builder = builder_for(shared, root.path());

    let session = builder.create_session(&runtime()).expect("create session");
    assert!(
        session.lease().is_none(),
        "a lazy store has no lease until the first append"
    );

    session
        .append(user_message("hello"))
        .await
        .expect("first append");

    let path = session.path();
    let dir = Path::new(&path).with_extension("lease");
    assert!(dir.join("lease.json").is_file());
    assert!(dir.join("epoch-1").is_file());
    assert!(session.lease().is_some());

    session.close().expect("close");
}

/// R6: `[failover]` disabled (the default) creates no lease directory.
#[tokio::test]
async fn create_session_with_failover_disabled_creates_no_lease_directory() {
    let root = tempfile::tempdir().expect("root");
    let mut shared = shared(root.path());
    Arc::get_mut(&mut shared)
        .expect("uniquely owned")
        .no_session = false;
    let builder = builder_for(shared, root.path());

    let session = builder.create_session(&runtime()).expect("create session");
    session
        .append(user_message("hello"))
        .await
        .expect("first append");

    let path = session.path();
    let dir = Path::new(&path).with_extension("lease");
    assert!(!dir.exists());
    assert!(session.lease().is_none());

    session.close().expect("close");
}

#[test]
fn the_chatgpt_client_needs_loaded_credentials_and_a_path() {
    let stored = crate::auth::Credentials {
        account_id: "acct-1".to_owned(),
        ..crate::auth::Credentials::default()
    };
    assert_eq!(
        chatgpt_client("/tmp/x.json", &stored, false).err(),
        Some(crate::auth::AuthError::NoCredentials.to_string())
    );
    assert_eq!(
        chatgpt_client("   ", &stored, true).err(),
        Some(crate::auth::AuthError::CredentialsUnavailable.to_string())
    );
    assert!(chatgpt_client("/tmp/x.json", &stored, true).is_ok());
}
#[tokio::test]
async fn plugin_packages_register_skills_tools_and_lifecycle_once_across_runner_rebuilds() {
    let temp = tempfile::tempdir().unwrap();
    let package = temp.path().join("source-demo");
    std::fs::create_dir_all(package.join("skills/example")).unwrap();
    std::fs::write(
        package.join("skills/example/SKILL.md"),
        "---\nname: example\ndescription: Example skill\n---\nDo the example task.\n",
    )
    .unwrap();
    let components = serde_json::json!({"version":1,"hooks":{"session_start":[{"command":["/bin/sh","-c","read input; echo start >> lifecycle.log"]}],"session_end":[{"command":["/bin/sh","-c","read input; sleep 0.05; echo end >> lifecycle.log"]}]},"tools":[{"name":"echo","description":"Echo a value","command":["/bin/sh","-c","read input; printf '{}'"],"input_schema":{"type":"object"}}]});
    std::fs::write(package.join("plugin.json"),serde_json::json!({"$schema":crate::plugin::package::PLUGIN_SCHEMA,"name":"demo","extensions":{"dev.otto":components}}).to_string()).unwrap();
    let service = crate::plugin::Service::new(temp.path());
    let installed = service.install(package.to_str().unwrap(), None).unwrap();
    service.change("demo", "trust").unwrap();
    let mut builder = builder(temp.path());
    let root = builder.workspace.root();
    builder.command_executor = Some(Arc::new(
        Executor::new(
            Arc::new(DirectDriver::new()),
            Policy {
                filesystem: FilesystemMode::Unconfined,
                network: NetworkMode::Allow,
            },
            root,
        )
        .unwrap(),
    ));
    builder.sandbox_environment = Some(vec!["PATH=/usr/bin:/bin".into()]);
    let session = SharedSession::memory(Header {
        id: "session-plugins".into(),
        ..Default::default()
    });
    let first = Arc::new(builder.build_runner(&session, &runtime()).await.unwrap());
    assert!(tool_names(&first).contains(&"plugin__demo__echo".into()));
    assert!(first.system_prompt.contains("Example skill"));
    let second = Arc::new(builder.build_runner(&session, &runtime()).await.unwrap());
    first.close();
    let ((), ()) = tokio::join!(second.end_session_hooks(), second.end_session_hooks());
    assert_eq!(
        std::fs::read_to_string(root.join("lifecycle.log")).unwrap(),
        "start\nend\n"
    );
    second.close();
    service.change("demo", "disable").unwrap();
    let disabled = builder
        .build_runner(&SharedSession::memory(Header::default()), &runtime())
        .await
        .unwrap();
    assert!(!tool_names(&disabled).contains(&"plugin__demo__echo".into()));
    assert!(!disabled.system_prompt.contains("Example skill"));
    disabled.close();
    use std::os::unix::fs::PermissionsExt;
    let snapshot = service.package_path(&installed.digest);
    for path in [
        snapshot.clone(),
        snapshot.join("skills"),
        snapshot.join("skills/example"),
    ] {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
}
#[tokio::test]
async fn closing_a_session_cancels_the_owned_startup_hook_after_build_waiter_drop() {
    let temp = tempfile::tempdir().unwrap();
    let mut builder = builder(temp.path());
    let root = builder.workspace.root();
    builder.command_executor = Some(Arc::new(
        Executor::new(
            Arc::new(DirectDriver::new()),
            Policy {
                filesystem: FilesystemMode::Unconfined,
                network: NetworkMode::Allow,
            },
            root,
        )
        .unwrap(),
    ));
    builder.sandbox_environment = Some(vec!["PATH=/usr/bin:/bin".into()]);
    builder
        .shared_mut()
        .config
        .hooks
        .session_start
        .push(otto_core::config::plugins::Hook {
            command: vec![
                "/bin/sh".into(),
                "-c".into(),
                "read input; touch started; sleep 20; touch should-not-finish".into(),
            ],
            timeout: "10s".into(),
            matcher: Vec::new(),
        });
    let session = SharedSession::memory(Header::default());
    let policy = runtime();
    {
        let build = builder.build_runner(&session, &policy);
        tokio::pin!(build);
        let started = async {
            while !root.join("started").exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        };
        tokio::select! {result=&mut build=>panic!("startup returned unexpectedly: {}",result.is_ok()),result=tokio::time::timeout(Duration::from_secs(5),started)=>result.unwrap()}
    }
    session.close().unwrap();
    tokio::time::timeout(Duration::from_secs(5), session.1.start_done.cancelled())
        .await
        .unwrap();
    assert!(session.1.start.get().unwrap().is_err());
    assert!(!root.join("should-not-finish").exists());
}

/// Answers only once the turn is cancelled, so the test decides exactly
/// when guidance is still waiting for a safe boundary.
struct CancelledProvider;

#[async_trait::async_trait]
impl Provider for CancelledProvider {
    async fn complete(
        &self,
        _request: &Request,
        _emit: StreamSink<'_>,
        control: &dyn OperationControl,
    ) -> ProviderSettlement {
        control.cancellation_token().cancelled().await;
        ProviderSettlement::failed(
            otto_core::provider::ProviderError::Cancelled,
            1,
            EffectCertainty::Unknown,
        )
    }
}

/// Guidance a cancelled turn accepted and never delivered must come back
/// with its own words: the user typed it, the turn that would have carried
/// it is gone, and before this the guidance simply vanished.
#[tokio::test]
async fn a_cancelled_turn_reports_the_guidance_it_never_delivered() {
    use crate::subagent::tasks::Tasks;

    let runner = Runner::scripted(
        SharedSession::memory(Header::default()),
        Arc::new(CancelledProvider),
        Arc::new(Tasks::new()),
    );
    let steering = runner.steering();
    let cancel = CancellationToken::new();
    let mut events = Vec::new();
    let run = async {
        // Esc only means something once the run accepts guidance.
        while steering.ticket().is_none() {
            tokio::task::yield_now().await;
        }
        steering.submit("keep my exact words").expect("active run");
        cancel.cancel();
    };
    let mut sink = |event| events.push(event);
    let (result, ()) = tokio::join!(runner.run("hello", &mut sink, &cancel), run);

    assert!(result.is_err(), "a cancelled turn reports its error");
    let notice = runner
        .session()
        .messages()
        .into_iter()
        .find(|message| message.context_type == "agent_steering")
        .expect("persisted steering-loss notice");
    assert_eq!(notice.role, otto_core::model::Role::Context);
    assert!(notice.display, "the loss must be visible, not hidden");
    assert!(
        notice.text().contains("keep my exact words"),
        "the notice must quote the guidance: {}",
        notice.text()
    );
    assert!(
        matches!(events.as_slice(), [.., Event::Notification { task_id, text, .. }] if task_id == "agent_steering" && text == &notice.text()),
        "the live event must say what the session recorded: {events:?}"
    );
}

/// A turn that delivers its guidance loses nothing, so it adds no notice.
#[tokio::test]
async fn a_turn_that_delivers_its_guidance_reports_no_loss() {
    use crate::subagent::tasks::Tasks;
    use crate::subagent::testsupport::{FakeProvider, assistant_text, match_any};
    use otto_core::model::Usage;

    let provider = FakeProvider::new();
    provider.add_route(match_any, vec![assistant_text("done", Usage::default())]);
    let runner = Runner::scripted(
        SharedSession::memory(Header::default()),
        provider,
        Arc::new(Tasks::new()),
    );
    let cancel = CancellationToken::new();
    let mut events = Vec::new();
    let mut sink = |event| events.push(event);
    runner
        .run("hello", &mut sink, &cancel)
        .await
        .expect("an answered turn succeeds");

    assert!(
        runner
            .session()
            .messages()
            .iter()
            .all(|message| message.context_type != "agent_steering"),
        "an ordinary turn must not claim it lost guidance"
    );
    assert!(
        events.iter().all(|event| !matches!(
            event,
            Event::Notification { task_id, .. } if task_id == "agent_steering"
        )),
        "{events:?}"
    );
}

#[test]
fn turn_limit_takes_the_earlier_limit_only_when_unattended() {
    let m = |n: u64| Some(Duration::from_secs(n * 60));
    for (turn, unattended_limit) in [(m(10), m(30)), (None, m(30)), (None, None)] {
        assert_eq!(turn_limit(turn, unattended_limit, false), turn);
    }
    assert_eq!(turn_limit(None, m(30), true), m(30));
    assert_eq!(turn_limit(m(10), m(30), true), m(10));
    assert_eq!(turn_limit(m(60), m(30), true), m(30));
    assert_eq!(turn_limit(m(60), None, true), m(60));
    assert_eq!(turn_limit(None, None, true), None);
}

/// Answers only when the turn's control stops the operation; the scripted
/// client bypasses `complete_with_timeout`, so it must observe the stop.
struct UntilStopped;

#[async_trait::async_trait]
impl Provider for UntilStopped {
    async fn complete(
        &self,
        _request: &Request,
        _emit: StreamSink<'_>,
        control: &dyn OperationControl,
    ) -> ProviderSettlement {
        control.cancellation_token().cancelled().await;
        provider_stop_settlement(
            control
                .stop_reason()
                .unwrap_or(OperationStopReason::Deadline),
        )
    }
}

/// Runs one turn against a provider that never answers, advancing paused
/// time past the unattended deadline plus the cancellation grace.
async fn unattended_turn_ends(origin: crate::schedule::Origin, headless: bool) -> bool {
    let mut runner = Runner::scripted(
        SharedSession::memory(Header::default()),
        Arc::new(UntilStopped),
        Arc::new(crate::subagent::tasks::Tasks::new()),
    );
    runner.unattended_turn_timeout = Some(Duration::from_secs(30 * 60));
    runner.headless = headless;
    runner.schedule_origin.set(Some(origin));
    let cancel = CancellationToken::new();
    let mut emit = |_| {};
    let run = runner.run("hello", &mut emit, &cancel);
    tokio::pin!(run);
    assert!(
        tokio::time::timeout(Duration::ZERO, &mut run)
            .await
            .is_err()
    );
    tokio::time::advance(Duration::from_secs(30 * 60)).await;
    match tokio::time::timeout(Duration::from_secs(1), &mut run).await {
        Ok(result) => {
            assert!(result.is_err(), "{result:?}");
            true
        }
        Err(_) => false,
    }
}

#[tokio::test(start_paused = true)]
async fn chat_turn_ends_at_the_unattended_deadline() {
    let origin = crate::schedule::Origin::chat("telegram", "1").unwrap();
    assert!(unattended_turn_ends(origin, false).await);
}

#[tokio::test(start_paused = true)]
async fn terminal_turn_ignores_the_unattended_deadline() {
    assert!(!unattended_turn_ends(crate::schedule::Origin::terminal(), false).await);
}

#[tokio::test(start_paused = true)]
async fn headless_terminal_turn_ends_at_the_unattended_deadline() {
    assert!(unattended_turn_ends(crate::schedule::Origin::terminal(), true).await);
}
