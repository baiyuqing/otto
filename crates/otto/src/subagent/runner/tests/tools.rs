//! The child tool view: write policy, report budget and registry limits.

use super::*;

#[tokio::test]
async fn a_shared_transcript_keeps_the_session_contract() {
    let fresh = || SharedTranscript(Arc::new(otto_core::session::MemorySession::new()));
    otto_core::conformance::session(&fresh, None)
        .await
        .unwrap_or_else(|failure| panic!("{failure}"));
}

#[test]
fn child_tools_preserve_retry_safety_only_for_allowed_registry_tools() {
    use otto_core::model::RetrySafety;

    let registry = Arc::new(
        Registry::new(vec![
            Box::new(crate::tool::otto::Help {
                definitions: Vec::new(),
            }),
            stub("read"),
            stub("edit"),
        ])
        .unwrap(),
    );
    let mut tools = child_tools(
        registry,
        WritePolicy::SingleWriter,
        Vec::new(),
        Arc::new(Inbox::new(None)),
    );
    assert_eq!(tools.retry_safety("otto_help"), RetrySafety::ReadOnly);
    for name in ["read", "edit", "agent_report", "missing"] {
        assert_eq!(tools.retry_safety(name), RetrySafety::NonIdempotent);
    }
    tools.allowed = Some(["edit".into()].into());
    assert_eq!(tools.retry_safety("otto_help"), RetrySafety::NonIdempotent);
}

#[tokio::test]
async fn write_policy_denies_mutation_tools_before_execution() {
    let registry = Arc::new(Registry::new(vec![stub("read"), stub("write")]).unwrap());
    let tools = child_tools(
        registry,
        WritePolicy::ProposeOnly,
        Vec::new(),
        Arc::new(Inbox::new(None)),
    );

    let result = execute_child(&tools, "write", r#"{"path":"src/lib.rs","content":"x"}"#).await;

    assert!(result.result.is_error, "{result:?}");
    assert!(result.result.content.contains("propose_only"), "{result:?}");
}

#[tokio::test]
async fn owned_paths_allows_only_matching_mutations() {
    let registry = Arc::new(Registry::new(vec![stub("write")]).unwrap());
    let tools = child_tools(
        registry,
        WritePolicy::OwnedPaths,
        vec!["crates/otto/**".to_string(), "docs/*.md".to_string()],
        Arc::new(Inbox::new(None)),
    );

    let allowed = execute_child(
        &tools,
        "write",
        r#"{"path":"crates/otto/src/lib.rs","content":"x"}"#,
    )
    .await;
    assert!(!allowed.result.is_error, "{allowed:?}");

    let denied = execute_child(
        &tools,
        "write",
        r#"{"path":"crates/otto-core/src/lib.rs","content":"x"}"#,
    )
    .await;
    assert!(denied.result.is_error, "{denied:?}");
    assert!(denied.result.content.contains("owned_paths"), "{denied:?}");
}

#[tokio::test]
async fn agent_report_is_child_only_and_pushes_parent_notification() {
    let parent_inbox = Arc::new(Inbox::new(None));
    let registry = Arc::new(Registry::new(vec![stub("read")]).unwrap());
    let tools = child_tools(
        registry,
        WritePolicy::SingleWriter,
        Vec::new(),
        parent_inbox.clone(),
    );
    assert!(
        tool_names(&tools.definitions()).contains(&"agent_report".to_string()),
        "child definitions should include agent_report"
    );

    let result = execute_child(
        &tools,
        "agent_report",
        r#"{"message":"found the failing test"}"#,
    )
    .await;

    assert!(!result.result.is_error, "{result:?}");
    assert_eq!(result.result.content, "report sent to parent");
    let notification = parent_inbox
        .remove("t1", NotificationKind::TaskReport)
        .expect("a task report was queued");
    assert_eq!(notification.task_id, "t1");
    assert_eq!(notification.kind, Some(NotificationKind::TaskReport));
    assert!(notification.text.contains("[task-report] task t1"));
    assert!(notification.text.contains("found the failing test"));
}

#[tokio::test]
async fn agent_report_errors_once_budget_is_used_up() {
    let parent_inbox = Arc::new(Inbox::new(None));
    let registry = Arc::new(Registry::new(vec![stub("read")]).unwrap());
    let tools = child_tools_with_budget(
        registry,
        WritePolicy::SingleWriter,
        Vec::new(),
        parent_inbox.clone(),
        REPORT_BUDGET,
    );

    for i in 0..REPORT_BUDGET {
        let result = execute_child(
            &tools,
            "agent_report",
            &format!(r#"{{"message":"report {i}"}}"#),
        )
        .await;
        assert!(!result.result.is_error, "report {i}: {result:?}");
    }
    assert_eq!(
        parent_inbox.len(),
        REPORT_BUDGET,
        "every on-budget report should reach the parent"
    );

    let over_budget = execute_child(&tools, "agent_report", r#"{"message":"one too many"}"#).await;
    assert!(over_budget.result.is_error, "{over_budget:?}");
    assert_eq!(
        over_budget.outcome.effect_certainty,
        otto_core::model::EffectCertainty::NotStarted
    );
    assert_eq!(
        parent_inbox.len(),
        REPORT_BUDGET,
        "the over-budget report must not reach the parent"
    );
}

#[tokio::test]
async fn agent_report_invalid_call_does_not_consume_budget() {
    let parent_inbox = Arc::new(Inbox::new(None));
    let registry = Arc::new(Registry::new(vec![stub("read")]).unwrap());
    let tools = child_tools_with_budget(
        registry,
        WritePolicy::SingleWriter,
        Vec::new(),
        parent_inbox.clone(),
        1,
    );

    let empty = execute_child(&tools, "agent_report", r#"{"message":""}"#).await;
    assert!(empty.result.is_error, "{empty:?}");

    let valid = execute_child(&tools, "agent_report", r#"{"message":"still have budget"}"#).await;
    assert!(
        !valid.result.is_error,
        "the empty-message call must not have consumed the budget: {valid:?}"
    );
}

#[tokio::test]
async fn parent_message_grants_one_more_report() {
    let parent_inbox = Arc::new(Inbox::new(None));
    let tasks = Tasks::new();
    let task = tasks
        .add(Task::default(), None, None)
        .expect("task registers");
    let reports_left = tasks
        .report_allowance(&task.id)
        .expect("the new task has a report allowance");

    let registry = Arc::new(Registry::new(vec![stub("read")]).unwrap());
    let tools = ChildTools {
        registry,
        allowed: None,
        write_policy: WritePolicy::SingleWriter,
        write_paths: Vec::new(),
        parent_inbox: parent_inbox.clone(),
        task_id: task.id.clone(),
        max_output_bytes: 16384,
        reports_left,
    };

    for i in 0..REPORT_BUDGET {
        let result = execute_child(
            &tools,
            "agent_report",
            &format!(r#"{{"message":"report {i}"}}"#),
        )
        .await;
        assert!(!result.result.is_error, "report {i}: {result:?}");
    }

    let over_budget = execute_child(&tools, "agent_report", r#"{"message":"one too many"}"#).await;
    assert!(over_budget.result.is_error, "{over_budget:?}");
    assert_eq!(
        parent_inbox.len(),
        REPORT_BUDGET,
        "only the on-budget reports should have reached the parent"
    );

    tasks
        .send_message(&task.id, "a question for you")
        .expect("parent message delivers");

    let granted = execute_child(
        &tools,
        "agent_report",
        r#"{"message":"answer to the question"}"#,
    )
    .await;
    assert!(
        !granted.result.is_error,
        "the parent message should grant one more report: {granted:?}"
    );

    let exhausted_again = execute_child(
        &tools,
        "agent_report",
        r#"{"message":"one too many again"}"#,
    )
    .await;
    assert!(exhausted_again.result.is_error, "{exhausted_again:?}");
}

#[tokio::test]
async fn child_registry_excludes_control_and_memory_tools() {
    let provider = FakeProvider::new();
    let tasks = Arc::new(Tasks::new());

    let mut parent: Vec<Box<dyn Tool + Send + Sync>> = vec![stub("read"), stub("write")];
    for name in EXCLUDED_CHILD_TOOLS {
        parent.push(stub(name));
    }
    parent.push(stub("grep"));

    let (runner, _) = runner(test_config(&provider, &tasks, parent));
    assert_eq!(
        tool_names(&runner.child_definitions()).join(","),
        "read,write,grep"
    );
}

#[tokio::test]
async fn child_registry_refuses_a_tool_call_once_the_parent_lease_is_lost() {
    let provider = FakeProvider::new();
    provider.add_route(
        match_any,
        vec![
            assistant_tool_call("call-1", "write", "{}", Usage::default()),
            assistant_text("final report", Usage::default()),
        ],
    );

    let write = StubTool::new(
        "write",
        ToolResult {
            content: "wrote".into(),
            ..ToolResult::default()
        },
    );
    let calls = write.counter();

    let tmp = tempfile::tempdir().expect("tmp");
    let lease = failover::lease::Lease::for_test(tmp.path());
    lease.mark_lost_for_test("test fence");

    let tasks = Arc::new(Tasks::new());
    let mut config = test_config(&provider, &tasks, vec![write.boxed()]);
    config.lease = Some(Arc::new(move || {
        Some(Arc::clone(&lease) as Arc<dyn crate::session::WriterOwnership>)
    }));
    let (runner, _) = runner(config);
    runner
        .start(StartRequest {
            prompt: "go".into(),
            ..StartRequest::default()
        })
        .await
        .expect("start succeeds");

    wait_final(&tasks, "t1").await;

    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "the tool must not run once the session lease is lost"
    );
    let requests = provider.requests();
    let result_block = requests
        .get(1)
        .expect("a follow-up request carrying the tool result")
        .messages
        .iter()
        .flat_map(|message| message.blocks.iter())
        .find(|block| block.block_type == BlockType::ToolResult && block.tool_call_id == "call-1")
        .expect("the tool result reached the follow-up request");
    assert!(result_block.is_error, "{result_block:?}");
    assert!(
        result_block.text.contains("session lease lost"),
        "{result_block:?}"
    );
}

#[tokio::test]
async fn plugin_argument_rewrites_obey_child_owned_path_policy() {
    use crate::sandbox::{Executor, FilesystemMode, NetworkMode, Policy, direct::DirectDriver};
    let temp = tempfile::tempdir().unwrap();
    let workspace = std::fs::canonicalize(temp.path()).unwrap();
    let executor = Executor::new(
        Arc::new(DirectDriver::new()),
        Policy {
            filesystem: FilesystemMode::Unconfined,
            network: NetworkMode::Allow,
        },
        &workspace,
    )
    .unwrap();
    let hooks=otto_core::config::plugins::Hooks{before_tool:vec![otto_core::config::plugins::Hook{command:vec!["/bin/sh".into(),"-c".into(),r#"read input; case "$input" in *'"task_id":"t1"'*) printf '%s' '{"arguments":{"path":"outside.txt","content":"wrong"}}';; *) exit 1;; esac"#.into()],timeout:"1s".into(),matcher:vec!["write".into()]}],..Default::default()};
    let runtime = crate::plugin::hooks::Runtime::new(
        &crate::plugin::Snapshot::default(),
        temp.path(),
        &hooks,
        crate::plugin::hooks::Execution {
            executor: Some(Arc::new(executor)),
            workspace,
            environment: vec!["PATH=/usr/bin:/bin".into()],
            guard: None,
            secrets: Vec::new(),
            logger: Arc::new(crate::log::Logger::discard()),
        },
    )
    .unwrap();
    let provider = FakeProvider::new();
    provider.add_route(
        match_any,
        vec![
            assistant_tool_call(
                "call-write",
                "write",
                r#"{"path":"owned/file.txt","content":"ok"}"#,
                Usage::default(),
            ),
            assistant_text("done", Usage::default()),
        ],
    );
    let tool = StubTool::new("write", text_result("wrote"));
    let count = tool.counter();
    let tasks = Arc::new(Tasks::new());
    let mut config = test_config(&provider, &tasks, vec![tool.boxed()]);
    config.template.plugins = Some(runtime);
    config.catalog = catalog(vec![Definition {
        name: "writer".into(),
        write_policy: WritePolicy::OwnedPaths,
        write_paths: vec!["owned/**".into()],
        ..Definition::default()
    }]);
    let (runner, _) = runner(config);
    runner
        .start(StartRequest {
            agent: "writer".into(),
            prompt: "write".into(),
            ..Default::default()
        })
        .await
        .unwrap();
    wait_final(&tasks, "t1").await;
    assert_eq!(count.load(Ordering::SeqCst), 0);
}
