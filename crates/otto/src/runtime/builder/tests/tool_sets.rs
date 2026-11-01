use super::*;

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

    let stub = crate::fixtures::subagent_doubles::StubTool::new(
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

    let status = runner.mcp().status();
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
    runtime.provider_type = otto_core::config::PROVIDER_CHATGPT.to_string();
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
        .child_tools(&runtime, 65536, &[], &Catalog::default(), "session-x")
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
            .child_tools(&runtime(), 65536, &[], &Catalog::default(), "session-x")
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
        .child_tools(&runtime(), 65536, &[], &Catalog::default(), "session-x")
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

/// A dropped `Builder` and its file tools release the `Workspace` (open
/// directory fd and lock map); nothing keeps it for the process lifetime.
#[test]
fn dropping_builder_and_file_tools_frees_the_workspace() {
    let dir = tempfile::tempdir().expect("temp dir");
    let builder = builder(dir.path());
    let workspace = Arc::downgrade(&builder.workspace);
    let tools = builder.builtin_file_tools(1024);
    assert!(!tools.is_empty());

    drop(builder);
    assert!(
        workspace.upgrade().is_some(),
        "the file tools own the workspace"
    );
    drop(tools);
    assert!(workspace.upgrade().is_none(), "the workspace is freed");
}

#[tokio::test]
async fn child_bash_approvals_are_keyed_by_the_owning_session() {
    let dir = tempfile::tempdir().expect("temp dir");
    let mut builder = builder(dir.path());
    with_bash(&mut builder, dir.path());
    let approvals = Arc::new(crate::tool::bash::BashApprovals::new(
        builder.command_executor.clone().expect("executor"),
        Vec::new(),
    ));
    builder.bash_approvals = Some(Arc::clone(&approvals));
    // A redaction value turns on the approval gates of the bash tool.
    let redaction_values = ["redaction-secret-value".to_string()];
    let tools = builder
        .child_tools(
            &runtime(),
            65536,
            &redaction_values,
            &Catalog::default(),
            "session-x",
        )
        .expect("child tools");
    let bash = tools
        .iter()
        .find(|tool| tool.definition().name == "bash")
        .expect("child bash");
    let arguments = serde_json::json!({
        "command": "git push",
        "sandbox_permissions": "require_escalated",
        "justification": "push the reviewed branch",
    })
    .to_string();
    let arguments = serde_json::value::RawValue::from_string(arguments).expect("raw arguments");
    let result = bash
        .execute(&arguments, &tokio_util::sync::CancellationToken::new())
        .await;
    assert!(result.content.contains("/approve"), "{}", result.content);
    assert_eq!(approvals.pending_count("session-x"), 1);
    assert_eq!(approvals.pending_count(""), 0);
}
