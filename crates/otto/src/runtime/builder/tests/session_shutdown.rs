use super::*;

/// The awaited process-exit path waits for the real MCP close, and the
/// runner's own `close` afterwards neither panics nor closes anything twice.
#[tokio::test]
async fn close_resources_before_close_actually_waits_for_the_real_close() {
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

    runner.session().close_resources().await;
    assert!(
        closed.load(Ordering::SeqCst),
        "close_resources did not wait for the real close"
    );

    // The runner no longer owns the servers: its `close` stays a harmless
    // no-op for them.
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
    fn memory_references(&self) -> Vec<otto_core::session::TurnMemoryReferences> {
        self.inner.memory_references()
    }

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
/// and [`crate::subagent::session_recovery::notify_moved`]: a running task and a
/// queued task are both cancelled, both transcripts end up recorded
/// interrupted, the parent inbox gets no `task_finished` notification for
/// either, and it gets exactly one "moved" notification naming both.
#[tokio::test]
async fn migrate_cancels_running_and_queued_subagent_tasks_and_notifies_the_parent() {
    use crate::fixtures::subagent_doubles::{FakeProvider, assistant_text, match_any, wait_status};
    use crate::subagent::runner::StartRequest;
    use crate::subagent::tasks::TaskStatus;
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
        crate::fixtures::subagent_doubles::test_config(&child_provider, &tasks, Vec::new());
    child_config.max_parallel = 1;
    let child_parent = parent_path.clone();
    child_config.child_session = Some(Arc::new(move |task_id: &str, _session_id: &str| {
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
    let components = serde_json::json!({"version":1,
        "hooks":{"session_start":[{"command":["/bin/sh",
        "-c",
        "read input; echo start >> lifecycle.log"]}],
        "session_end":[{"command":["/bin/sh",
        "-c",
        "read input; sleep 0.05; echo end >> lifecycle.log"]}]},
        "tools":[{"name":"echo",
        "description":"Echo a value",
        "command":["/bin/sh",
        "-c",
        "read input; printf '{}'"],
        "input_schema":{"type":"object"}}]});
    std::fs::write(package.join("plugin.json"),serde_json::json!({"$schema":crate::plugin::package::PLUGIN_SCHEMA,"name":"demo","extensions":{"dev.otto":components}}).to_string()).unwrap();
    let service = crate::plugin::Service::new(temp.path());
    let installed = service.install(package.to_str().unwrap(), None).unwrap();
    service.change("demo", "trust").unwrap();
    let mut builder = builder(temp.path());
    let root = builder.workspace.root().to_path_buf();
    builder.command_executor = Some(Arc::new(
        Executor::new(
            Arc::new(DirectDriver::new()),
            Policy {
                filesystem: FilesystemMode::Unconfined,
                network: NetworkMode::Allow,
            },
            &root,
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

/// A fresh lazy session with a trusted plugin installed gets no session file
/// from the runner build: the `otto.plugins` record waits for the first
/// durable write.
#[tokio::test]
async fn building_a_runner_with_plugins_does_not_create_the_session_file() {
    let temp = tempfile::tempdir().unwrap();
    let package = temp.path().join("source-demo");
    std::fs::create_dir_all(package.join("skills/example")).unwrap();
    std::fs::write(
        package.join("skills/example/SKILL.md"),
        "---\nname: example\ndescription: Example skill\n---\nDo the example task.\n",
    )
    .unwrap();
    let components = serde_json::json!({"version":1});
    std::fs::write(package.join("plugin.json"),serde_json::json!({"$schema":crate::plugin::package::PLUGIN_SCHEMA,"name":"demo","extensions":{"dev.otto":components}}).to_string()).unwrap();
    let service = crate::plugin::Service::new(temp.path());
    let installed = service.install(package.to_str().unwrap(), None).unwrap();
    service.change("demo", "trust").unwrap();
    let builder = builder_for(shared_with_no_session(temp.path(), false), temp.path());
    let session = builder.create_session(&runtime()).unwrap();
    let runner = builder.build_runner(&session, &runtime()).await.unwrap();
    assert!(runner.system_prompt.contains("Example skill"));
    assert_eq!(
        session.path(),
        "",
        "building the runner created a session file"
    );
    runner.close();
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
    let root = builder.workspace.root().to_path_buf();
    builder.command_executor = Some(Arc::new(
        Executor::new(
            Arc::new(DirectDriver::new()),
            Policy {
                filesystem: FilesystemMode::Unconfined,
                network: NetworkMode::Allow,
            },
            &root,
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
            lua: None,
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
    tokio::time::timeout(
        Duration::from_secs(5),
        session.hooks().start_done.cancelled(),
    )
    .await
    .unwrap();
    assert!(session.hooks().start.get().unwrap().is_err());
    assert!(!root.join("should-not-finish").exists());
}
