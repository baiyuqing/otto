
use super::*;
use crate::faults::files::{FaultyFileDriver, FileOp};
use crate::faults::{Fault, FaultError, FaultPlan, Filter, Rule, Trigger};
use crate::storage::drivers::posix::PosixDriver;
use crate::storage::files::FileDriver;
use crate::subagent::testsupport::{
    FakeProvider, RouteStep, StubTool, assistant_text, assistant_tool_call, last_user_text,
    match_any, match_prompt, raw, redaction, redactor, stub, test_config, test_prompt_for,
    tool_names, wait_status,
};
use otto_core::agent::inbox::NotificationKind;
use otto_core::model::{Block, BlockType, FinishReason, Message, Role, Usage};
use otto_core::provider::Response;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

/// There is no publish window to race: `MemorySession` carries no header,
/// so the transcript is created before `Tasks::add` publishes the history
/// closure.
const _: () = ();

#[tokio::test]
async fn a_shared_transcript_keeps_the_session_contract() {
    let fresh = || SharedTranscript(Arc::new(otto_core::session::MemorySession::new()));
    otto_core::conformance::session(&fresh, None)
        .await
        .unwrap_or_else(|failure| panic!("{failure}"));
}

fn runner(config: Config) -> (Arc<Runner>, Vec<String>) {
    let (runner, warnings) = Runner::new(config).expect("the test config is valid");
    (Arc::new(runner), warnings)
}

/// Waits for `id` to finish, failing the test after five seconds.
async fn wait_final(tasks: &Tasks, id: &str) -> Task {
    let cancel = CancellationToken::new();
    tokio::select! {
        result = tasks.wait(id, &cancel) => result.expect("the task exists"),
        () = tokio::time::sleep(std::time::Duration::from_secs(5)) => {
            panic!("task {id} did not finish in time")
        }
    }
}

fn definition(name: &str) -> Definition {
    Definition {
        name: name.to_string(),
        ..Definition::default()
    }
}

fn catalog(definitions: Vec<Definition>) -> Catalog {
    Catalog::from_definitions(definitions)
}

#[tokio::test]
async fn agent_starts_task_and_pushes_matching_notification() {
    let provider = FakeProvider::new();
    provider.add_route(
        match_prompt("do the thing"),
        vec![assistant_text(
            "all done",
            Usage {
                input_tokens: 100,
                output_tokens: 20,
                ..Usage::default()
            },
        )],
    );

    let tasks = Arc::new(Tasks::new());
    let (runner, _) = runner(test_config(&provider, &tasks, Vec::new()));

    let tools = crate::subagent::tools::tools(&runner);
    let agent_tool = tools
        .iter()
        .find(|tool| tool.definition().name == "agent")
        .expect("the agent tool is registered");
    let result = agent_tool
        .execute(
            &raw(r#"{"prompt":"do the thing"}"#),
            &CancellationToken::new(),
        )
        .await;
    assert!(
        !result.is_error,
        "agent tool returned error: {}",
        result.content
    );
    assert_eq!(result.content, "task t1 (default) started");

    let final_task = wait_final(&tasks, "t1").await;
    assert_eq!(final_task.status, TaskStatus::Succeeded);
    assert_eq!(final_task.result, "all done");

    let notification = tasks
        .notifications()
        .remove("t1", NotificationKind::TaskFinished)
        .expect("a task_finished notification");
    assert_eq!(
        notification.text,
        completion_text(&final_task, runner.max_output_bytes())
    );
}

#[tokio::test]
async fn child_provider_usage_reaches_the_shared_collector() {
    let provider = FakeProvider::new();
    provider.add_route(
        match_any,
        vec![assistant_text(
            "done",
            Usage {
                input_tokens: 20,
                output_tokens: 4,
                cached_input_tokens: 10,
            },
        )],
    );
    let store = Arc::new(crate::usage::Store::open_in_memory().expect("usage store"));
    let tasks = Arc::new(Tasks::new());
    let mut config = test_config(&provider, &tasks, Vec::new());
    config.usage = Some(crate::usage::Collector::new(
        Arc::clone(&store),
        crate::usage::Context {
            session_id: "parent".into(),
            ..crate::usage::Context::default()
        },
    ));
    let (runner, _) = runner(config);

    runner
        .start(StartRequest {
            prompt: "go".into(),
            ..StartRequest::default()
        })
        .await
        .expect("start");
    wait_final(&tasks, "t1").await;

    let summary = store.summary(Some("parent")).expect("summary");
    assert_eq!(summary.usage_events, 1);
    assert_eq!(summary.input_tokens, 20);
    assert_eq!(summary.cached_input_tokens, Some(10));
}

#[tokio::test]
async fn task_finished_notification_carries_usage_and_its_cache_breakdown_presence() {
    let provider = FakeProvider::new();
    provider.add_route(
        match_any,
        vec![assistant_text(
            "done",
            Usage {
                input_tokens: 20,
                output_tokens: 4,
                cached_input_tokens: 10,
            },
        )],
    );

    let tasks = Arc::new(Tasks::new());
    let (runner, _) = runner(test_config(&provider, &tasks, Vec::new()));
    runner
        .start(StartRequest {
            prompt: "go".into(),
            ..StartRequest::default()
        })
        .await
        .expect("start succeeds");

    wait_final(&tasks, "t1").await;
    let notification = tasks
        .notifications()
        .remove("t1", NotificationKind::TaskFinished)
        .expect("a task_finished notification");
    assert_eq!(
        notification.usage,
        Some(Usage {
            input_tokens: 20,
            output_tokens: 4,
            cached_input_tokens: 10,
        }),
        "the parent's session totals include the task's tokens"
    );
    assert!(
        notification.cached_input_tokens_present,
        "the parent must know the cached share is measured, not assumed"
    );
}

#[tokio::test]
async fn task_finished_notification_omits_absent_usage() {
    let provider = FakeProvider::new();
    provider.add_route(
        match_any,
        vec![RouteStep::Reply(Box::new(Response {
            message: Message {
                role: Role::Assistant,
                finish_reason: Some(FinishReason::Stop),
                blocks: vec![Block::text("done")],
                ..Message::default()
            },
            cached_input_tokens_present: false,
            remote_compaction: None,
        }))],
    );

    let tasks = Arc::new(Tasks::new());
    let (runner, _) = runner(test_config(&provider, &tasks, Vec::new()));
    runner
        .start(StartRequest {
            prompt: "go".into(),
            ..StartRequest::default()
        })
        .await
        .expect("start succeeds");

    wait_final(&tasks, "t1").await;
    let notification = tasks
        .notifications()
        .remove("t1", NotificationKind::TaskFinished)
        .expect("a task_finished notification");
    assert_eq!(notification.usage, None);
}

fn child_tools(
    registry: Arc<Registry>,
    write_policy: WritePolicy,
    write_paths: Vec<String>,
    parent_inbox: Arc<dyn Mailbox>,
) -> ChildTools {
    child_tools_with_budget(
        registry,
        write_policy,
        write_paths,
        parent_inbox,
        REPORT_BUDGET,
    )
}

fn child_tools_with_budget(
    registry: Arc<Registry>,
    write_policy: WritePolicy,
    write_paths: Vec<String>,
    parent_inbox: Arc<dyn Mailbox>,
    budget: usize,
) -> ChildTools {
    ChildTools {
        registry,
        allowed: None,
        write_policy,
        write_paths,
        parent_inbox,
        task_id: "t1".to_string(),
        max_output_bytes: 16384,
        reports_left: Arc::new(AtomicUsize::new(budget)),
    }
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

async fn execute_child(tools: &ChildTools, name: &str, arguments: &str) -> ToolExecution {
    let operation_id = otto_core::model::OperationId::new("op_test").expect("operation id");
    let arguments = raw(arguments);
    tools
        .execute(
            ToolCall {
                tool_call_id: "",
                operation_id: &operation_id,
                name,
                arguments: &arguments,
                attempt: 1,
            },
            &CancellationToken::new(),
        )
        .await
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
async fn child_system_prompt_shape() {
    let provider = FakeProvider::new();
    provider.add_route(match_any, vec![assistant_text("done", Usage::default())]);
    let tasks = Arc::new(Tasks::new());
    let (runner, _) = runner(test_config(&provider, &tasks, Vec::new()));
    runner
        .start(StartRequest {
            prompt: "go".into(),
            ..StartRequest::default()
        })
        .await
        .expect("start succeeds");
    wait_final(&tasks, "t1").await;

    let requests = provider.requests();
    let prompt = &requests
        .first()
        .expect("one provider request")
        .system_prompt;
    assert!(
        prompt.starts_with(&test_prompt_for(&runner.child_definitions())),
        "system prompt does not start with the parent prompt:\n{prompt}"
    );
    assert!(prompt.contains("<subagent_role>"), "{prompt}");
    assert!(
        !prompt.contains("<research_report>"),
        "no artifact path without storage"
    );
    assert!(prompt.ends_with("</otto_system_prompt>"), "{prompt}");
    assert!(prompt.contains(GENERIC_SUBAGENT_INSTRUCTION), "{prompt}");
}

/// [`Redactor`] is immutable and each child builds its own from the
/// configured values, so what is checked here is the observable half: a
/// secret reaching the runner as the parent model or as a per-call model is
/// redacted before it leaves in a provider request.
#[tokio::test]
async fn subagent_run_redacts_the_model() {
    let secret = "sk-supersecret123";
    for per_call in [false, true] {
        let provider = FakeProvider::new();
        provider.add_route(match_any, vec![assistant_text("done", Usage::default())]);
        let tasks = Arc::new(Tasks::new());
        let mut config = test_config(&provider, &tasks, Vec::new());
        config.redaction_values = redaction(&[secret]);
        config.template.model = if per_call {
            "gpt-parent".into()
        } else {
            secret.into()
        };
        let (runner, _) = runner(config);

        runner
            .start(StartRequest {
                prompt: "go".into(),
                model: if per_call {
                    secret.into()
                } else {
                    String::new()
                },
                ..StartRequest::default()
            })
            .await
            .expect("start succeeds");

        let final_task = wait_final(&tasks, "t1").await;
        assert_eq!(final_task.status, TaskStatus::Succeeded);

        let want = redactor(&[secret]).redact_string(secret);
        assert_ne!(
            want, secret,
            "the redactor must rewrite its configured secret"
        );
        let requests = provider.requests();
        assert_eq!(requests.first().expect("one request").model, want);
    }
}

#[tokio::test]
async fn max_parallel_limits_concurrency() {
    let provider = FakeProvider::new();
    let release = CancellationToken::new();
    let counts = Arc::new(Mutex::new((0usize, 0usize)));

    let hook_release = release.clone();
    let hook_counts = Arc::clone(&counts);
    provider.set_hook(Arc::new(move |cancel, _request| {
        let release = hook_release.clone();
        let counts = Arc::clone(&hook_counts);
        Box::pin(async move {
            {
                let mut guard = counts.lock().expect("the counter lock is intact");
                guard.0 += 1;
                guard.1 = guard.1.max(guard.0);
            }
            tokio::select! {
                () = release.cancelled() => {}
                () = cancel.cancelled() => {}
            }
            counts.lock().expect("the counter lock is intact").0 -= 1;
        })
    }));
    provider.add_route(match_any, vec![assistant_text("done", Usage::default())]);

    let tasks = Arc::new(Tasks::new());
    let mut config = test_config(&provider, &tasks, Vec::new());
    config.max_parallel = 2;
    let (runner, _) = runner(config);

    // t1 and t2 must hold both slots before t3 starts: starting all three
    // at once races three tasks against a two-slot semaphore with no
    // ordering tied to start() call order.
    for index in 0..2 {
        runner
            .start(StartRequest {
                prompt: format!("task {index}"),
                ..StartRequest::default()
            })
            .await
            .expect("start succeeds");
    }
    wait_status(&tasks, "t1", TaskStatus::Running).await;
    wait_status(&tasks, "t2", TaskStatus::Running).await;

    runner
        .start(StartRequest {
            prompt: "task 2".into(),
            ..StartRequest::default()
        })
        .await
        .expect("start succeeds");
    assert_eq!(
        tasks.get("t3").expect("t3 exists").status,
        TaskStatus::Queued,
        "t3 must stay queued while t1 and t2 hold the only two slots"
    );

    release.cancel();
    for id in ["t1", "t2", "t3"] {
        assert_eq!(wait_final(&tasks, id).await.status, TaskStatus::Succeeded);
    }
    assert!(
        counts.lock().expect("the counter lock is intact").1 <= 2,
        "observed more than two concurrent provider calls"
    );
}

#[tokio::test(start_paused = true)]
async fn queued_task_deadline_is_consumed_before_a_permit_is_acquired() {
    let provider = FakeProvider::new();
    let release = CancellationToken::new();
    let hook_release = release.clone();
    provider.set_hook(Arc::new(move |cancel, _request| {
        let release = hook_release.clone();
        Box::pin(async move {
            tokio::select! {
                () = release.cancelled() => {}
                () = cancel.cancelled() => {}
            }
        })
    }));
    provider.add_route(match_any, vec![assistant_text("done", Usage::default())]);

    let tasks = Arc::new(Tasks::new());
    let mut config = test_config(&provider, &tasks, Vec::new());
    config.max_parallel = 1;
    let (runner, _) = runner(config);
    runner
        .start(StartRequest {
            prompt: "occupy the slot".into(),
            ..StartRequest::default()
        })
        .await
        .expect("start blocker");
    wait_status(&tasks, "t1", TaskStatus::Running).await;

    let control = Control::new(Deadline::after(Duration::from_secs(2)));
    let queued = runner.run_with_definition_control(
        StartRequest {
            prompt: "must not start".into(),
            agent: "worker".into(),
            ..StartRequest::default()
        },
        Arc::new(MemorySession::new()),
        definition("worker"),
        &control,
    );
    tokio::pin!(queued);
    tokio::time::advance(Duration::from_secs(2)).await;
    let task = queued.await.expect("deadline returns a durable task");

    assert_eq!(task.status, TaskStatus::Interrupted);
    assert_eq!(task.started_at, None);
    assert_eq!(task.error, "task deadline exceeded");

    release.cancel();
    assert_eq!(wait_final(&tasks, "t1").await.status, TaskStatus::Succeeded);
}

#[tokio::test(start_paused = true)]
async fn deadline_does_not_detach_effectful_child() {
    let provider = FakeProvider::new();
    let release = CancellationToken::new();
    let hook_release = release.clone();
    provider.set_hook(Arc::new(move |_cancel, _request| {
        let release = hook_release.clone();
        Box::pin(async move { release.cancelled().await })
    }));
    provider.add_route(match_any, vec![assistant_text("done", Usage::default())]);
    let tasks = Arc::new(Tasks::new());
    let mut config = test_config(&provider, &tasks, Vec::new());
    config.template.task_timeout = Some(Duration::from_secs(1));
    let (runner, _) = runner(config);
    runner
        .start(StartRequest {
            prompt: "go".into(),
            ..StartRequest::default()
        })
        .await
        .expect("start");
    wait_status(&tasks, "t1", TaskStatus::Running).await;
    tokio::time::advance(Duration::from_secs(1)).await;
    tokio::task::yield_now().await;
    assert_eq!(tasks.get("t1").expect("task").status, TaskStatus::Running);
    release.cancel();
    assert_eq!(
        wait_final(&tasks, "t1").await.status,
        TaskStatus::Interrupted
    );
}

#[tokio::test]
async fn cancel_running_task() {
    let provider = FakeProvider::new();
    provider.set_hook(Arc::new(|cancel, _request| {
        Box::pin(async move { cancel.cancelled().await })
    }));

    let tasks = Arc::new(Tasks::new());
    let (runner, _) = runner(test_config(&provider, &tasks, Vec::new()));
    runner
        .start(StartRequest {
            prompt: "go".into(),
            ..StartRequest::default()
        })
        .await
        .expect("start succeeds");

    wait_status(&tasks, "t1", TaskStatus::Running).await;
    tasks.cancel("t1").expect("cancel succeeds");
    assert_eq!(wait_final(&tasks, "t1").await.status, TaskStatus::Canceled);
}

#[tokio::test]
async fn cancel_queued_task() {
    let provider = FakeProvider::new();
    let block = CancellationToken::new();
    let hook_block = block.clone();
    provider.set_hook(Arc::new(move |cancel, _request| {
        let block = hook_block.clone();
        Box::pin(async move {
            tokio::select! {
                () = block.cancelled() => {}
                () = cancel.cancelled() => {}
            }
        })
    }));
    provider.add_route(match_any, vec![assistant_text("done", Usage::default())]);

    let tasks = Arc::new(Tasks::new());
    let mut config = test_config(&provider, &tasks, Vec::new());
    config.max_parallel = 1;
    let (runner, _) = runner(config);

    runner
        .start(StartRequest {
            prompt: "first".into(),
            ..StartRequest::default()
        })
        .await
        .expect("start succeeds");
    wait_status(&tasks, "t1", TaskStatus::Running).await;

    runner
        .start(StartRequest {
            prompt: "second".into(),
            ..StartRequest::default()
        })
        .await
        .expect("start succeeds");
    assert_eq!(
        tasks.get("t2").expect("t2 exists").status,
        TaskStatus::Queued
    );

    tasks.cancel("t2").expect("cancel succeeds");
    let final_task = wait_final(&tasks, "t2").await;
    assert_eq!(final_task.status, TaskStatus::Canceled);
    assert_eq!(
        final_task.started_at, None,
        "a task canceled while queued never started"
    );

    block.cancel();
    wait_final(&tasks, "t1").await;
}

#[tokio::test]
async fn chatgpt_stream_recovery_completes_child_and_persists_safe_attempt_facts() {
    use crate::faults::http::{self, Script};
    const PARTIAL: &str = r#"{"type":"response.output_text.delta","delta":"abandoned answer long enough to be visible"}"#;
    const FINAL: &str = r#"{"type":"response.output_text.delta","delta":"recovered answer"}"#;
    const DONE: &str = r#"{"type":"response.completed","response":{"status":"completed","usage":{"input_tokens":1,"output_tokens":1}}}"#;
    let server = http::spawn_scripted(vec![
        Script::sse_cut_after(&[PARTIAL, DONE], 1),
        Script::sse_without_done(&[FINAL, DONE]),
    ])
    .await;
    let dir = tempfile::tempdir().unwrap();
    let tasks = Arc::new(Tasks::new());
    let mut config = test_config(&FakeProvider::new(), &tasks, Vec::new());
    config.provider = Arc::new(crate::provider::chatgpt::Client::with_base_url(
        &server.url,
        crate::auth::token::TokenSource::new(
            crate::auth::Credentials {
                access_token: "test-token".into(),
                ..Default::default()
            },
            dir.path().join("absent-credentials.json"),
            CancellationToken::new(),
        ),
        "test-account",
    ));
    let parent = dir.path().join("parent.jsonl");
    config.child_session = Some(Arc::new(move |task_id: &str| {
        Ok(Some(child_store(&parent, task_id, None)))
    }));
    let (runner, _) = runner(config);
    runner
        .start(StartRequest {
            prompt: "go".into(),
            ..Default::default()
        })
        .await
        .unwrap();
    let task = wait_final(&tasks, "t1").await;
    assert_eq!(task.status, TaskStatus::Succeeded, "{}", task.error);
    assert_eq!(task.last_text, "recovered answer");
    assert_eq!(server.count(), 2);
    let requests = server.requests();
    assert_eq!(requests[0].body, requests[1].body);
    let path = std::path::Path::new(&task.session_path);
    let entries = read_custom_entries(path);
    let (_, audit) = entries
        .iter()
        .find(|(kind, _)| kind == "otto.provider_recovery")
        .unwrap();
    assert_eq!(audit["attempts"], 2);
    assert_eq!(audit["retries"][0]["interruptedOutput"], true);
    assert_eq!(audit["retries"][0]["reason"], "connection interrupted");
    let persisted = std::fs::read_to_string(path).unwrap();
    assert!(!persisted.contains("abandoned answer"));
    assert!(!persisted.contains("test-token"));
    assert!(!persisted.contains("test-account"));
}

#[tokio::test]
async fn provider_error_fails_task() {
    let provider = FakeProvider::new();
    provider.add_route(match_any, vec![RouteStep::Fail("provider exploded".into())]);

    let tasks = Arc::new(Tasks::new());
    let mut config = test_config(&provider, &tasks, Vec::new());
    config.template.model = "parent".into();
    config.template.subagent_models = vec!["cheap".into()];
    let (runner, _) = runner(config);
    runner
        .start(StartRequest {
            prompt: "go".into(),
            ..StartRequest::default()
        })
        .await
        .expect("start succeeds");

    let final_task = wait_final(&tasks, "t1").await;
    assert_eq!(final_task.status, TaskStatus::Failed);
    assert!(
        final_task.error.contains("provider exploded"),
        "error = {:?}",
        final_task.error
    );
    assert_eq!(final_task.model, "cheap");
    assert_eq!(provider.requests().len(), 1);
    assert_eq!(provider.requests()[0].model, "cheap");
}

#[tokio::test]
async fn otto_task_spec_is_written_before_the_child_runs_and_covers_a_queued_task() {
    let dir = tempfile::tempdir().expect("dir");
    let parent_path = dir.path().join("parent.jsonl");
    let provider = FakeProvider::new();
    let block = CancellationToken::new();
    let hook_block = block.clone();
    provider.set_hook(Arc::new(move |cancel, _request| {
        let block = hook_block.clone();
        Box::pin(async move {
            tokio::select! {
                () = block.cancelled() => {}
                () = cancel.cancelled() => {}
            }
        })
    }));
    provider.add_route(match_any, vec![assistant_text("done", Usage::default())]);

    let tasks = Arc::new(Tasks::new());
    let mut config = test_config(&provider, &tasks, Vec::new());
    config.max_parallel = 1;
    config.catalog = catalog(vec![Definition {
        body: "Review the change.".into(),
        tools: Some(vec!["read".into()]),
        write_policy: WritePolicy::ReadOnly,
        write_paths: vec!["docs/**".into()],
        ..definition("reviewer")
    }]);
    let parent = parent_path.clone();
    config.child_session = Some(Arc::new(move |task_id: &str| {
        Ok(Some(child_store(&parent, task_id, None)))
    }));
    let (runner, _) = runner(config);

    runner
        .start(StartRequest {
            prompt: "go".into(),
            ..StartRequest::default()
        })
        .await
        .expect("start succeeds");
    wait_status(&tasks, "t1", TaskStatus::Running).await;

    runner
        .start(StartRequest {
            agent: "reviewer".into(),
            description: "look this over".into(),
            prompt: "look at this".into(),
            ..StartRequest::default()
        })
        .await
        .expect("start succeeds");
    assert_eq!(
        tasks.get("t2").expect("t2 exists").status,
        TaskStatus::Queued,
        "the second task must stay queued behind the first"
    );

    // Read both transcripts while t2 is still queued: a queued task that
    // never started must already have otto.task_spec on file.
    for (task_id, want_definition) in [("t1", false), ("t2", true)] {
        let task = tasks.get(task_id).expect("task exists");
        let path = dir
            .path()
            .join("parent")
            .join(format!("{task_id}-child.jsonl"));
        let entries = read_custom_entries(&path);
        assert_eq!(entries[0].0, "otto.runtime", "{entries:?}");
        assert_eq!(entries[1].0, "otto.task_spec", "{entries:?}");
        let data = &entries[1].1;
        assert_eq!(data["id"], task_id);
        assert_eq!(data["name"], task.name);
        assert_eq!(data["description"], task.description);
        assert_eq!(data["model"], task.model);
        assert_eq!(data["model_source"], task.model_source.as_str());
        assert_eq!(data["context"], task.context);
        assert_eq!(data["prompt"], task.prompt);
        if want_definition {
            let definition = &data["definition"];
            assert_eq!(definition["name"], "reviewer");
            assert_eq!(definition["body"], "Review the change.");
            assert_eq!(definition["tools"], serde_json::json!(["read"]));
            assert_eq!(definition["writePolicy"], "read_only");
            assert_eq!(definition["writePaths"], serde_json::json!(["docs/**"]));
        } else {
            assert!(data["definition"].is_null(), "{data:?}");
        }
    }

    block.cancel();
    wait_final(&tasks, "t1").await;
    wait_final(&tasks, "t2").await;
}

/// A transcript wrapper that asserts, at the moment `otto.task_result` is
/// appended, that the task's completion notification already sits in the
/// parent inbox. This checks the order `finish`
/// (crates/otto/src/subagent/runner.rs) is required to follow: the
/// notification is pushed before the transcript entry is appended.
struct OrderCheckingTranscript {
    inner: Transcript,
    tasks: Arc<Tasks>,
    task_id: String,
}

#[async_trait::async_trait]
impl Session for OrderCheckingTranscript {
    fn messages(&self) -> Vec<Message> {
        self.inner.messages()
    }

    async fn append(&self, message: Message) -> Result<(), otto_core::session::SessionError> {
        self.inner.append(message).await
    }

    fn latest_compaction(&self) -> Option<otto_core::session::CompactionMetadata> {
        self.inner.latest_compaction()
    }

    async fn append_compaction(
        &self,
        checkpoint: otto_core::session::CompactionCheckpoint,
    ) -> Result<otto_core::session::CompactionMetadata, otto_core::session::SessionError> {
        self.inner.append_compaction(checkpoint).await
    }

    async fn append_operation_fact(
        &self,
        fact: otto_core::session::OperationFact,
    ) -> Result<(), otto_core::session::SessionError> {
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
    ) -> Result<otto_core::session::AppendOutcome, otto_core::session::SessionError> {
        self.inner.append_input_if_absent(input_id, message).await
    }

    fn has_input(&self, input_id: &str) -> bool {
        self.inner.has_input(input_id)
    }

    async fn append_custom(
        &self,
        custom_type: &str,
        data: &str,
    ) -> Result<(), otto_core::session::SessionError> {
        if custom_type == TASK_RESULT_CUSTOM_TYPE {
            let pushed = self
                .tasks
                .notifications()
                .snapshot()
                .iter()
                .any(|notification| {
                    notification.task_id == self.task_id
                        && notification.kind == Some(NotificationKind::TaskFinished)
                });
            assert!(
                pushed,
                "otto.task_result for {} was appended before its completion notification was pushed",
                self.task_id
            );
        }
        self.inner.append_custom(custom_type, data).await
    }
}

#[tokio::test]
async fn otto_task_result_is_appended_after_the_notification_when_succeeded() {
    let dir = tempfile::tempdir().expect("dir");
    let parent_path = dir.path().join("parent.jsonl");
    let provider = FakeProvider::new();
    provider.add_route(match_any, vec![assistant_text("done", Usage::default())]);

    let tasks = Arc::new(Tasks::new());
    let mut config = test_config(&provider, &tasks, Vec::new());
    let parent = parent_path.clone();
    let checking_tasks = Arc::clone(&tasks);
    config.child_session = Some(Arc::new(move |task_id: &str| {
        let (inner, path) = child_store(&parent, task_id, None);
        let wrapped: Transcript = Arc::new(OrderCheckingTranscript {
            inner,
            tasks: Arc::clone(&checking_tasks),
            task_id: task_id.to_string(),
        });
        Ok(Some((wrapped, path)))
    }));
    let (runner, _) = runner(config);

    runner
        .start(StartRequest {
            prompt: "go".into(),
            ..StartRequest::default()
        })
        .await
        .expect("start succeeds");
    let done = wait_final(&tasks, "t1").await;
    assert_eq!(done.status, TaskStatus::Succeeded, "{}", done.error);

    let path = dir.path().join("parent").join("t1-child.jsonl");
    let (custom_type, data) = read_custom_entries(&path)
        .into_iter()
        .last()
        .expect("otto.runtime is at least present");
    assert_eq!(custom_type, "otto.task_result");
    assert_eq!(data["status"], "succeeded");
    assert_eq!(data["error"], "");
}

#[tokio::test]
async fn otto_task_result_is_appended_after_the_notification_when_failed() {
    let dir = tempfile::tempdir().expect("dir");
    let parent_path = dir.path().join("parent.jsonl");
    let provider = FakeProvider::new();
    provider.add_route(match_any, vec![RouteStep::Fail("provider exploded".into())]);

    let tasks = Arc::new(Tasks::new());
    let mut config = test_config(&provider, &tasks, Vec::new());
    let parent = parent_path.clone();
    let checking_tasks = Arc::clone(&tasks);
    config.child_session = Some(Arc::new(move |task_id: &str| {
        let (inner, path) = child_store(&parent, task_id, None);
        let wrapped: Transcript = Arc::new(OrderCheckingTranscript {
            inner,
            tasks: Arc::clone(&checking_tasks),
            task_id: task_id.to_string(),
        });
        Ok(Some((wrapped, path)))
    }));
    let (runner, _) = runner(config);

    runner
        .start(StartRequest {
            prompt: "go".into(),
            ..StartRequest::default()
        })
        .await
        .expect("start succeeds");
    let done = wait_final(&tasks, "t1").await;
    assert_eq!(done.status, TaskStatus::Failed);

    let path = dir.path().join("parent").join("t1-child.jsonl");
    let (custom_type, data) = read_custom_entries(&path)
        .into_iter()
        .last()
        .expect("otto.runtime is at least present");
    assert_eq!(custom_type, "otto.task_result");
    assert_eq!(data["status"], "failed");
    assert!(
        data["error"]
            .as_str()
            .unwrap_or_default()
            .contains("provider exploded"),
        "{data:?}"
    );
}

#[tokio::test]
async fn otto_task_result_is_appended_after_the_notification_when_canceled() {
    let dir = tempfile::tempdir().expect("dir");
    let parent_path = dir.path().join("parent.jsonl");
    let provider = FakeProvider::new();
    provider.set_hook(Arc::new(|cancel, _request| {
        Box::pin(async move { cancel.cancelled().await })
    }));

    let tasks = Arc::new(Tasks::new());
    let mut config = test_config(&provider, &tasks, Vec::new());
    let parent = parent_path.clone();
    let checking_tasks = Arc::clone(&tasks);
    config.child_session = Some(Arc::new(move |task_id: &str| {
        let (inner, path) = child_store(&parent, task_id, None);
        let wrapped: Transcript = Arc::new(OrderCheckingTranscript {
            inner,
            tasks: Arc::clone(&checking_tasks),
            task_id: task_id.to_string(),
        });
        Ok(Some((wrapped, path)))
    }));
    let (runner, _) = runner(config);

    runner
        .start(StartRequest {
            prompt: "go".into(),
            ..StartRequest::default()
        })
        .await
        .expect("start succeeds");
    wait_status(&tasks, "t1", TaskStatus::Running).await;
    tasks.cancel("t1").expect("cancel succeeds");
    let done = wait_final(&tasks, "t1").await;
    assert_eq!(done.status, TaskStatus::Canceled);

    let path = dir.path().join("parent").join("t1-child.jsonl");
    let (custom_type, data) = read_custom_entries(&path)
        .into_iter()
        .last()
        .expect("otto.runtime is at least present");
    assert_eq!(custom_type, "otto.task_result");
    assert_eq!(data["status"], "canceled");
    assert_eq!(data["error"], "");
}

#[tokio::test]
async fn migration_marks_running_and_queued_tasks_interrupted_and_pushes_no_finished_notification()
{
    let dir = tempfile::tempdir().expect("dir");
    let parent_path = dir.path().join("parent.jsonl");
    let provider = FakeProvider::new();
    provider.set_hook(Arc::new(|cancel, _request| {
        Box::pin(async move { cancel.cancelled().await })
    }));

    let tasks = Arc::new(Tasks::new());
    let mut config = test_config(&provider, &tasks, Vec::new());
    config.max_parallel = 1;
    let parent = parent_path.clone();
    config.child_session = Some(Arc::new(move |task_id: &str| {
        Ok(Some(child_store(&parent, task_id, None)))
    }));
    let (runner, _) = runner(config);

    runner
        .start(StartRequest {
            prompt: "first".into(),
            ..StartRequest::default()
        })
        .await
        .expect("start succeeds");
    wait_status(&tasks, "t1", TaskStatus::Running).await;

    runner
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

    let ids = tasks.begin_migration();
    assert_eq!(ids, vec!["t1".to_string(), "t2".to_string()]);
    tasks.wait_final(&ids).await;

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

        let path = dir
            .path()
            .join("parent")
            .join(format!("{task_id}-child.jsonl"));
        let (custom_type, data) = read_custom_entries(&path)
            .into_iter()
            .last()
            .expect("otto.runtime is at least present");
        assert_eq!(custom_type, "otto.task_result");
        assert_eq!(data["status"], "interrupted", "{task_id}: {data:?}");
        assert_eq!(data["error"], "", "{task_id}: {data:?}");
    }
}

#[tokio::test]
async fn task_record_fields_update_across_provider_steps() {
    let provider = FakeProvider::new();
    provider.add_route(
        match_any,
        vec![
            assistant_tool_call(
                "call-1",
                "echo",
                r#"{"msg":"hi"}"#,
                Usage {
                    input_tokens: 10,
                    output_tokens: 5,
                    ..Usage::default()
                },
            ),
            assistant_text(
                "final report",
                Usage {
                    input_tokens: 20,
                    output_tokens: 8,
                    ..Usage::default()
                },
            ),
        ],
    );

    let echo = StubTool::new(
        "echo",
        ToolResult {
            content: "echoed".into(),
            ..ToolResult::default()
        },
    );
    let calls = echo.counter();

    let tasks = Arc::new(Tasks::new());
    let (runner, _) = runner(test_config(&provider, &tasks, vec![echo.boxed()]));
    runner
        .start(StartRequest {
            prompt: "go".into(),
            ..StartRequest::default()
        })
        .await
        .expect("start succeeds");

    let final_task = wait_final(&tasks, "t1").await;
    assert_eq!(final_task.status, TaskStatus::Succeeded);
    assert_eq!(final_task.steps, 2);
    assert_eq!(final_task.tool_calls, 1);
    assert!(
        final_task.last_tool.contains("echo"),
        "{:?}",
        final_task.last_tool
    );
    assert_eq!(final_task.last_text, "final report");
    assert_eq!(final_task.usage.input_tokens, 30);
    assert_eq!(final_task.usage.output_tokens, 13);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
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
async fn start_unknown_agent_rejected_before_task_created() {
    let provider = FakeProvider::new();
    let tasks = Arc::new(Tasks::new());
    let (runner, _) = runner(test_config(&provider, &tasks, Vec::new()));

    let error = runner
        .start(StartRequest {
            prompt: "go".into(),
            agent: "nope".into(),
            ..StartRequest::default()
        })
        .await
        .expect_err("an unknown agent is rejected");
    assert_eq!(error.to_string(), r#"unknown agent "nope""#);
    assert!(tasks.list().is_empty());
}

#[tokio::test]
async fn start_named_agent_sets_task_agent() {
    let provider = FakeProvider::new();
    provider.add_route(match_any, vec![assistant_text("done", Usage::default())]);
    let tasks = Arc::new(Tasks::new());
    let mut config = test_config(&provider, &tasks, Vec::new());
    config.catalog = catalog(vec![definition("reviewer")]);
    let (runner, _) = runner(config);

    let task = runner
        .start(StartRequest {
            prompt: "go".into(),
            agent: "reviewer".into(),
            ..StartRequest::default()
        })
        .await
        .expect("start succeeds");
    assert_eq!(task.agent, "reviewer");
    assert_eq!(wait_final(&tasks, &task.id).await.agent, "reviewer");
}

#[tokio::test]
async fn start_model_precedence() {
    for (call_model, definition_model, configured, want, source) in [
        ("call-model", "def-model", true, "call-model", "explicit"),
        ("", "def-model", true, "def-model", "definition"),
        ("", "", true, "cheap-model", "configured_first"),
        ("", "", false, "sess-model", "parent"),
    ] {
        let provider = FakeProvider::new();
        provider.add_route(match_any, vec![assistant_text("done", Usage::default())]);
        let tasks = Arc::new(Tasks::new());
        let mut config = test_config(&provider, &tasks, Vec::new());
        config.template.model = "sess-model".into();
        if configured {
            config.template.subagent_models = vec!["cheap-model".into(), "strong-model".into()];
        }
        config.catalog = catalog(vec![Definition {
            name: "reviewer".into(),
            model: definition_model.into(),
            ..Definition::default()
        }]);
        let (runner, _) = runner(config);

        runner
            .start(StartRequest {
                prompt: "go".into(),
                agent: "reviewer".into(),
                model: call_model.into(),
                ..StartRequest::default()
            })
            .await
            .expect("start succeeds");
        wait_final(&tasks, "t1").await;

        assert_eq!(
            provider.requests().first().expect("one request").model,
            want
        );
        let task = tasks.get("t1").expect("recorded task");
        assert_eq!(task.model, want);
        assert_eq!(task.model_source.as_str(), source);
    }
}

/// A definition's tool allowlist narrows the child registry to exactly
/// those tools, in the parent's order. The assertion reads the child's
/// system prompt, because the test prompt names the child's tools.
#[tokio::test]
async fn new_runner_tools_allowlist_parent_order() {
    let provider = FakeProvider::new();
    provider.add_route(match_any, vec![assistant_text("done", Usage::default())]);
    let tasks = Arc::new(Tasks::new());
    let mut config = test_config(
        &provider,
        &tasks,
        vec![stub("grep"), stub("read"), stub("write")],
    );
    config.catalog = catalog(vec![Definition {
        name: "reviewer".into(),
        tools: Some(vec!["read".into(), "grep".into()]),
        ..Definition::default()
    }]);
    let (runner, warnings) = runner(config);
    assert!(warnings.is_empty(), "{warnings:?}");

    runner
        .start(StartRequest {
            prompt: "go".into(),
            agent: "reviewer".into(),
            ..StartRequest::default()
        })
        .await
        .expect("start succeeds");
    wait_final(&tasks, "t1").await;

    let requests = provider.requests();
    assert!(
        requests
            .first()
            .expect("one request")
            .system_prompt
            .starts_with("PARENT PROMPT tools=grep,read,agent_report\n"),
        "{}",
        requests[0].system_prompt
    );
}

#[tokio::test]
async fn new_runner_unknown_tool_warning() {
    for bad in ["bogus", "agent", "remember"] {
        let provider = FakeProvider::new();
        provider.add_route(match_any, vec![assistant_text("done", Usage::default())]);
        let tasks = Arc::new(Tasks::new());
        let mut config = test_config(&provider, &tasks, vec![stub("read")]);
        config.catalog = catalog(vec![Definition {
            name: "reviewer".into(),
            tools: Some(vec!["read".into(), bad.into()]),
            ..Definition::default()
        }]);
        let (runner, warnings) = runner(config);
        assert!(
            warnings.contains(&format!("agent reviewer: unknown tool {bad:?} ignored")),
            "{warnings:?}"
        );

        runner
            .start(StartRequest {
                prompt: "go".into(),
                agent: "reviewer".into(),
                ..StartRequest::default()
            })
            .await
            .expect("start succeeds");
        wait_final(&tasks, "t1").await;
        assert!(
            provider.requests()[0]
                .system_prompt
                .starts_with("PARENT PROMPT tools=read,agent_report\n"),
            "{}",
            provider.requests()[0].system_prompt
        );
    }
}

#[tokio::test]
async fn start_system_prompt_uses_definition_body() {
    let provider = FakeProvider::new();
    provider.add_route(match_any, vec![assistant_text("done", Usage::default())]);
    let tasks = Arc::new(Tasks::new());
    let mut config = test_config(&provider, &tasks, Vec::new());
    config.catalog = catalog(vec![Definition {
        name: "reviewer".into(),
        body: "You are a reviewer.".into(),
        ..Definition::default()
    }]);
    let (runner, _) = runner(config);
    runner
        .start(StartRequest {
            prompt: "go".into(),
            agent: "reviewer".into(),
            ..StartRequest::default()
        })
        .await
        .expect("start succeeds");
    wait_final(&tasks, "t1").await;

    let prompt = provider.requests()[0].system_prompt.clone();
    assert!(
        prompt.contains("<subagent_role>\nYou are a reviewer.\n</subagent_role>"),
        "{prompt}"
    );
    assert!(!prompt.contains(GENERIC_SUBAGENT_INSTRUCTION), "{prompt}");
}

#[tokio::test]
async fn start_system_prompt_empty_body_uses_generic_instruction() {
    let provider = FakeProvider::new();
    provider.add_route(match_any, vec![assistant_text("done", Usage::default())]);
    let tasks = Arc::new(Tasks::new());
    let mut config = test_config(&provider, &tasks, Vec::new());
    config.catalog = catalog(vec![definition("reviewer")]);
    let (runner, _) = runner(config);
    runner
        .start(StartRequest {
            prompt: "go".into(),
            agent: "reviewer".into(),
            ..StartRequest::default()
        })
        .await
        .expect("start succeeds");
    wait_final(&tasks, "t1").await;

    assert!(
        provider.requests()[0]
            .system_prompt
            .contains(GENERIC_SUBAGENT_INSTRUCTION)
    );
}

fn user_message(id: &str, text: &str) -> Message {
    Message {
        id: id.into(),
        role: Role::User,
        blocks: vec![Block::text(text)],
        ..Message::default()
    }
}

fn assistant_message(id: &str, text: &str) -> Message {
    Message {
        id: id.into(),
        role: Role::Assistant,
        blocks: vec![Block::text(text)],
        ..Message::default()
    }
}

fn tool_call_message(id: &str, tool_name: &str) -> Message {
    Message {
        id: id.into(),
        role: Role::Assistant,
        blocks: vec![Block {
            block_type: BlockType::ToolCall,
            tool_name: tool_name.into(),
            tool_call_id: "call-1".into(),
            ..Block::default()
        }],
        ..Message::default()
    }
}

#[tokio::test]
async fn start_context_inherit_snapshot_in_first_request() {
    let parent = vec![
        user_message("u1", "first"),
        assistant_message("a1", "reply one"),
        user_message("u2", "second"),
        tool_call_message("a2", "agent"),
        // A sibling call's result, appended after the pending agent call.
        Message {
            id: "tr1".into(),
            role: Role::Tool,
            blocks: vec![Block {
                block_type: BlockType::ToolResult,
                tool_call_id: "call-0".into(),
                ..Block::default()
            }],
            ..Message::default()
        },
    ];
    let want_snapshot = parent[..3].to_vec();

    let provider = FakeProvider::new();
    provider.add_route(match_any, vec![assistant_text("done", Usage::default())]);
    let tasks = Arc::new(Tasks::new());
    let mut config = test_config(&provider, &tasks, vec![stub("write"), stub("grep")]);
    let source = parent.clone();
    config.parent_session = Some(Arc::new(move || source.clone()));
    let (runner, _) = runner(config);

    let task = runner
        .start(StartRequest {
            prompt: "delegated task".into(),
            context: "inherit".into(),
            ..StartRequest::default()
        })
        .await
        .expect("start succeeds");
    assert_eq!(task.context, "inherit");
    wait_final(&tasks, &task.id).await;

    let messages = provider.requests()[0].messages.clone();
    // The child has history and no tool-set record, so its first run adds
    // the legacy tool-set notice between the snapshot and the prompt.
    assert_eq!(messages.len(), want_snapshot.len() + 2, "{messages:#?}");
    assert_eq!(&messages[..want_snapshot.len()], want_snapshot.as_slice());
    let notice = &messages[want_snapshot.len()];
    assert_eq!(notice.role, Role::Context);
    assert_eq!(notice.context_type, "capability_update");
    let mut names: Vec<String> = provider.requests()[0]
        .tools
        .iter()
        .map(|definition| definition.name.clone())
        .collect();
    assert!(!names.is_empty());
    names.sort();
    let text = notice.text();
    assert!(text.contains("was not recorded"), "{text}");
    assert!(
        text.lines()
            .any(|line| line == format!("Current tools: {}", names.join(", "))),
        "{text}"
    );
    assert!(!text.contains("Added:"), "{text}");
    assert!(!text.contains("Removed:"), "{text}");
    let last = &messages[want_snapshot.len() + 1];
    assert_eq!(last.role, Role::User);
    assert_eq!(last.text(), "delegated task");
}

/// A child store for task `task_id` beside `parent`. With a `plan`, the
/// store's journal runs over a `FaultyFileDriver` for it.
fn child_store(
    parent: &std::path::Path,
    task_id: &str,
    plan: Option<Arc<FaultPlan<FileOp>>>,
) -> (Transcript, String) {
    let name = format!("{task_id}-child");
    let driver: Arc<dyn FileDriver> = match plan {
        Some(plan) => Arc::new(FaultyFileDriver::new(Arc::new(PosixDriver), plan)),
        None => Arc::new(PosixDriver),
    };
    let store = crate::session::Store::create_child_lazy_with_driver(
        parent,
        &name,
        otto_core::session::Header {
            id: "child".into(),
            workspace: parent.parent().expect("dir").to_string_lossy().into_owned(),
            provider: "openai-compatible".into(),
            model: "test-model".into(),
            created_at: Utc::now(),
            ..otto_core::session::Header::default()
        },
        driver,
    )
    .expect("child store");
    let path = crate::session::Store::child_path(parent, &name);
    (Arc::new(store), path.to_string_lossy().into_owned())
}

/// The `custom` entries in a child transcript file, as `(customType,
/// data)` pairs, in file order.
fn read_custom_entries(path: &std::path::Path) -> Vec<(String, serde_json::Value)> {
    std::fs::read_to_string(path)
        .expect("read child transcript")
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter(|entry| entry.get("type").and_then(|value| value.as_str()) == Some("custom"))
        .map(|entry| {
            let custom_type = entry
                .get("customType")
                .and_then(|value| value.as_str())
                .unwrap_or_default()
                .to_string();
            let data = entry
                .get("data")
                .cloned()
                .unwrap_or(serde_json::Value::Null);
            (custom_type, data)
        })
        .collect()
}

#[tokio::test]
async fn a_child_transcript_with_its_inherited_context_is_written_to_its_own_file() {
    let dir = tempfile::tempdir().expect("dir");
    let parent_path = dir.path().join("parent.jsonl");
    let provider = FakeProvider::new();
    provider.add_route(match_any, vec![assistant_text("done", Usage::default())]);
    let tasks = Arc::new(Tasks::new());
    let mut config = test_config(&provider, &tasks, Vec::new());
    // A stored parent message always carries a timestamp.
    let inherited: Vec<Message> = [
        user_message("u1", "first"),
        tool_call_message("a1", "agent"),
    ]
    .into_iter()
    .map(|message| Message {
        created_at: Utc::now(),
        ..message
    })
    .collect();
    config.parent_session = Some(Arc::new(move || inherited.clone()));
    let parent = parent_path.clone();
    config.child_session = Some(Arc::new(move |task_id: &str| {
        Ok(Some(child_store(&parent, task_id, None)))
    }));
    let (runner, _) = runner(config);

    let task = runner
        .start(StartRequest {
            prompt: "delegated task".into(),
            context: "inherit".into(),
            ..StartRequest::default()
        })
        .await
        .expect("start succeeds");
    let done = wait_final(&tasks, &task.id).await;

    let want = dir
        .path()
        .join("parent")
        .join(format!("{}-child.jsonl", task.id));
    assert_eq!(done.status, TaskStatus::Succeeded, "{}", done.error);
    assert_eq!(done.session_path, want.to_string_lossy());
    let texts: Vec<String> = crate::session::Store::read_transcript(&want)
        .expect("read child")
        .iter()
        .map(Message::text)
        .collect();
    assert_eq!(texts, ["first", "delegated task", "done"]);
}

#[tokio::test]
async fn a_child_transcript_write_failure_fails_only_that_task() {
    let dir = tempfile::tempdir().expect("dir");
    let parent_path = dir.path().join("parent.jsonl");
    let provider = FakeProvider::new();
    provider.add_route(match_any, vec![assistant_text("done", Usage::default())]);
    let tasks = Arc::new(Tasks::new());
    let mut config = test_config(&provider, &tasks, Vec::new());
    // The child's header and runtime record are journal writes 1 and 2;
    // every later write fails before it reaches the file.
    let plan = FaultPlan::new(vec![
        Rule::new(
            FileOp::Write,
            Trigger::FromNth(3),
            Fault::Before(FaultError::Os(libc::EIO)),
        )
        .filter(Filter::NameContains(".jsonl")),
    ]);
    let child_plan = Arc::clone(&plan);
    config.child_session = Some(Arc::new(move |task_id: &str| {
        Ok(Some(child_store(
            &parent_path,
            task_id,
            Some(Arc::clone(&child_plan)),
        )))
    }));
    let (runner, _) = runner(config);

    let task = runner
        .start(StartRequest {
            prompt: "delegated task".into(),
            ..StartRequest::default()
        })
        .await
        .expect("start succeeds");
    let done = wait_final(&tasks, &task.id).await;

    assert_eq!(done.status, TaskStatus::Failed);
    assert!(!done.error.is_empty());
    plan.assert_all_fired();
}

#[tokio::test]
async fn start_context_fresh_only_prompt() {
    let provider = FakeProvider::new();
    provider.add_route(match_any, vec![assistant_text("done", Usage::default())]);
    let tasks = Arc::new(Tasks::new());
    let mut config = test_config(&provider, &tasks, Vec::new());
    let consulted = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&consulted);
    config.parent_session = Some(Arc::new(move || {
        flag.store(true, Ordering::SeqCst);
        Vec::new()
    }));
    let (runner, _) = runner(config);

    let task = runner
        .start(StartRequest {
            prompt: "go".into(),
            context: "fresh".into(),
            ..StartRequest::default()
        })
        .await
        .expect("start succeeds");
    assert_eq!(task.context, "fresh");
    wait_final(&tasks, &task.id).await;

    assert!(
        !consulted.load(Ordering::SeqCst),
        "the parent session must not be consulted for context fresh"
    );
    let messages = provider.requests()[0].messages.clone();
    assert_eq!(messages.len(), 1, "{messages:#?}");
    assert_eq!(messages[0].text(), "go");
}

#[tokio::test]
async fn start_context_definition_inherit_honoured_and_call_override() {
    let parent = vec![
        user_message("u1", "hello"),
        tool_call_message("a1", "agent"),
    ];

    for (request_context, want_context, want_messages) in
        [("", "inherit", 2usize), ("fresh", "fresh", 1)]
    {
        let provider = FakeProvider::new();
        provider.add_route(match_any, vec![assistant_text("done", Usage::default())]);
        let tasks = Arc::new(Tasks::new());
        let mut config = test_config(&provider, &tasks, Vec::new());
        let source = parent.clone();
        config.parent_session = Some(Arc::new(move || source.clone()));
        config.catalog = catalog(vec![Definition {
            name: "reviewer".into(),
            context: "inherit".into(),
            ..Definition::default()
        }]);
        let (runner, _) = runner(config);

        let task = runner
            .start(StartRequest {
                prompt: "go".into(),
                agent: "reviewer".into(),
                context: request_context.into(),
                ..StartRequest::default()
            })
            .await
            .expect("start succeeds");
        assert_eq!(task.context, want_context);
        wait_final(&tasks, &task.id).await;
        assert_eq!(provider.requests()[0].messages.len(), want_messages);
    }
}

#[tokio::test]
async fn start_context_inherit_without_parent_session_errors() {
    let provider = FakeProvider::new();
    let tasks = Arc::new(Tasks::new());
    let (runner, _) = runner(test_config(&provider, &tasks, Vec::new()));

    let error = runner
        .start(StartRequest {
            prompt: "go".into(),
            context: "inherit".into(),
            ..StartRequest::default()
        })
        .await
        .expect_err("inherit without a parent session is rejected");
    assert_eq!(
        error.to_string(),
        "context inherit is not available in this session"
    );
    assert!(tasks.list().is_empty());
}

#[tokio::test]
async fn start_fork_before_gives_the_child_the_forked_messages_as_inherit() {
    let forked = vec![user_message("u1", "first"), assistant_message("a1", "one")];
    let provider = FakeProvider::new();
    provider.add_route(match_any, vec![assistant_text("done", Usage::default())]);
    let tasks = Arc::new(Tasks::new());
    let mut config = test_config(&provider, &tasks, vec![stub("write")]);
    let quoted = Arc::new(std::sync::Mutex::new(String::new()));
    let (source, seen) = (forked.clone(), Arc::clone(&quoted));
    config.parent_fork = Some(Arc::new(move |prompt| {
        *seen.lock().unwrap() = prompt.to_owned();
        Ok(source.clone())
    }));
    let (runner, _) = runner(config);

    let task = runner
        .start(StartRequest {
            prompt: "delegated task".into(),
            fork_before: "  second\n".into(),
            ..StartRequest::default()
        })
        .await
        .expect("start succeeds");
    assert_eq!(task.context, "inherit");
    wait_final(&tasks, &task.id).await;

    assert_eq!(*quoted.lock().unwrap(), "second");
    let messages = provider.requests()[0].messages.clone();
    assert_eq!(&messages[..forked.len()], forked.as_slice());
}

#[tokio::test]
async fn start_fork_before_errors_create_no_task() {
    let provider = FakeProvider::new();
    let tasks = Arc::new(Tasks::new());
    let mut config = test_config(&provider, &tasks, Vec::new());
    config.parent_fork = Some(Arc::new(|_| Err("2 user prompts match".into())));
    let (forking, _) = runner(config);
    for (context, want) in [
        ("fresh", r#"fork_before requires context "inherit""#),
        ("", "fork_before: 2 user prompts match"),
    ] {
        let error = forking
            .start(StartRequest {
                prompt: "go".into(),
                context: context.into(),
                fork_before: "x".into(),
                ..StartRequest::default()
            })
            .await
            .expect_err("fork_before fails");
        assert_eq!(error.to_string(), want);
    }

    let (runner, _) = runner(test_config(&provider, &tasks, Vec::new()));
    let error = runner
        .start(StartRequest {
            prompt: "go".into(),
            fork_before: "x".into(),
            ..StartRequest::default()
        })
        .await
        .expect_err("no saved session");
    assert_eq!(error.to_string(), "fork_before: needs a saved session");
    assert!(tasks.list().is_empty());
}

#[tokio::test]
async fn start_invalid_context_errors() {
    let provider = FakeProvider::new();
    let tasks = Arc::new(Tasks::new());
    let (runner, _) = runner(test_config(&provider, &tasks, Vec::new()));

    let error = runner
        .start(StartRequest {
            prompt: "go".into(),
            context: "bogus".into(),
            ..StartRequest::default()
        })
        .await
        .expect_err("an invalid context is rejected");
    assert_eq!(error.to_string(), r#"context must be "fresh" or "inherit""#);
    assert!(tasks.list().is_empty());
}

fn base() -> DateTime<Utc> {
    "2026-01-01T00:00:00Z"
        .parse::<DateTime<Utc>>()
        .expect("the base timestamp parses")
}

fn finished(seconds: i64) -> Option<DateTime<Utc>> {
    Some(base() + chrono::Duration::seconds(seconds))
}

#[test]
fn completion_text_renders_every_status() {
    let cases: Vec<(&str, Task, usize, &str)> = vec![
        (
            "succeeded",
            Task {
                id: "t1".into(),
                status: TaskStatus::Succeeded,
                created_at: Some(base()),
                started_at: Some(base()),
                finished_at: finished(42),
                tool_calls: 7,
                usage: Usage {
                    input_tokens: 10000,
                    output_tokens: 2310,
                    ..Usage::default()
                },
                result: "the full report".into(),
                ..Task::default()
            },
            16384,
            "[task-notification] task t1 (default) succeeded · 42s · 7 tool calls · 12,310 tokens\nthe full report",
        ),
        (
            "failed",
            Task {
                id: "t1".into(),
                status: TaskStatus::Failed,
                started_at: Some(base()),
                finished_at: finished(12),
                tool_calls: 3,
                error: "provider exploded".into(),
                ..Task::default()
            },
            16384,
            "[task-notification] task t1 (default) failed · 12s · 3 tool calls\nprovider exploded",
        ),
        (
            "canceled",
            Task {
                id: "t1".into(),
                status: TaskStatus::Canceled,
                started_at: Some(base()),
                finished_at: finished(12),
                tool_calls: 3,
                ..Task::default()
            },
            16384,
            "[task-notification] task t1 (default) canceled · 12s · 3 tool calls",
        ),
        (
            "canceled while queued",
            Task {
                id: "t3".into(),
                status: TaskStatus::Canceled,
                started_at: None,
                finished_at: Some(base()),
                ..Task::default()
            },
            16384,
            "[task-notification] task t3 (default) canceled · 0s · 0 tool calls",
        ),
        (
            "singular tool call",
            Task {
                id: "t4".into(),
                status: TaskStatus::Succeeded,
                started_at: Some(base()),
                finished_at: finished(1),
                tool_calls: 1,
                usage: Usage {
                    input_tokens: 1,
                    ..Usage::default()
                },
                result: "ok".into(),
                ..Task::default()
            },
            16384,
            "[task-notification] task t4 (default) succeeded · 1s · 1 tool call · 1 tokens\nok",
        ),
        (
            "duration over a minute",
            Task {
                id: "t5".into(),
                status: TaskStatus::Succeeded,
                started_at: Some(base()),
                finished_at: finished(65),
                tool_calls: 2,
                result: "done".into(),
                ..Task::default()
            },
            16384,
            "[task-notification] task t5 (default) succeeded · 1m5s · 2 tool calls · 0 tokens\ndone",
        ),
        (
            "succeeded with model",
            Task {
                id: "t1".into(),
                status: TaskStatus::Succeeded,
                started_at: Some(base()),
                finished_at: finished(42),
                tool_calls: 7,
                model: "gpt-4o-mini".into(),
                usage: Usage {
                    input_tokens: 10000,
                    output_tokens: 2310,
                    ..Usage::default()
                },
                result: "the full report".into(),
                ..Task::default()
            },
            16384,
            "[task-notification] task t1 (default) succeeded · gpt-4o-mini · 42s · 7 tool calls · 12,310 tokens\nthe full report",
        ),
        (
            "failed with model",
            Task {
                id: "t1".into(),
                status: TaskStatus::Failed,
                started_at: Some(base()),
                finished_at: finished(12),
                tool_calls: 3,
                model: "gpt-4o-mini".into(),
                error: "provider exploded".into(),
                ..Task::default()
            },
            16384,
            "[task-notification] task t1 (default) failed · gpt-4o-mini · 12s · 3 tool calls\nprovider exploded",
        ),
        (
            "canceled with model",
            Task {
                id: "t1".into(),
                status: TaskStatus::Canceled,
                started_at: Some(base()),
                finished_at: finished(12),
                tool_calls: 3,
                model: "gpt-4o-mini".into(),
                ..Task::default()
            },
            16384,
            "[task-notification] task t1 (default) canceled · gpt-4o-mini · 12s · 3 tool calls",
        ),
        (
            "succeeded with a name",
            Task {
                id: "t1".into(),
                name: "lint-check".into(),
                status: TaskStatus::Succeeded,
                started_at: Some(base()),
                finished_at: finished(42),
                tool_calls: 7,
                usage: Usage {
                    input_tokens: 10000,
                    output_tokens: 2310,
                    ..Usage::default()
                },
                result: "the full report".into(),
                ..Task::default()
            },
            16384,
            "[task-notification] task t1 lint-check (default) succeeded · 42s · 7 tool calls · 12,310 tokens\nthe full report",
        ),
    ];

    for (name, task, max_output_bytes, want) in cases {
        assert_eq!(
            completion_text(&task, max_output_bytes),
            want,
            "case {name}"
        );
    }
}

#[test]
fn completion_text_caps_the_result() {
    let long = "a".repeat(5000);
    let task = Task {
        id: "t6".into(),
        status: TaskStatus::Succeeded,
        started_at: Some(base()),
        finished_at: finished(3),
        result: long.clone(),
        ..Task::default()
    };
    let got = completion_text(&task, 100);
    let want = format!(
        "[task-notification] task t6 (default) succeeded · 3s · 0 tool calls · 0 tokens\n{}",
        capped_text_result(&long, 100).content
    );
    assert_eq!(got, want);
    assert!(got.contains("truncated"), "{got}");
}

// -----------------------------------------------------------------------
// resume
// -----------------------------------------------------------------------

/// An assistant message with one dangling tool call, for building fixture
/// transcripts directly.
fn dangling_call() -> Message {
    Message {
        role: Role::Assistant,
        blocks: vec![Block {
            block_type: BlockType::ToolCall,
            tool_call_id: "call-1".into(),
            tool_name: "read".into(),
            arguments: Some(raw("{}")),
            ..Block::default()
        }],
        finish_reason: Some(FinishReason::ToolCalls),
        created_at: Utc::now(),
        ..Message::default()
    }
}

/// Writes a child transcript directly, bypassing the runner: an
/// `otto.task_spec` entry, the prompt as a user message when `started`,
/// a dangling tool call when `dangling`, and an `otto.task_result` entry
/// with `status`. Returns the file path. Sets up the fixtures the resume
/// tests below exercise through [`Runner::resume`] and the `agent` tool.
#[allow(clippy::too_many_arguments)]
fn interrupted_fixture(
    parent: &std::path::Path,
    task_id: &str,
    prompt: &str,
    definition: Option<TaskSpecDefinition>,
    started: bool,
    dangling: bool,
    status: &str,
) -> PathBuf {
    interrupted_fixture_with_capabilities(
        parent, task_id, prompt, definition, started, dangling, status, None,
    )
}

/// [`interrupted_fixture`] that also records `capabilities` as the child's
/// `otto.capabilities` entry when set.
#[allow(clippy::too_many_arguments)]
fn interrupted_fixture_with_capabilities(
    parent: &std::path::Path,
    task_id: &str,
    prompt: &str,
    definition: Option<TaskSpecDefinition>,
    started: bool,
    dangling: bool,
    status: &str,
    capabilities: Option<&[String]>,
) -> PathBuf {
    let name = format!("{task_id}-child");
    let store = crate::session::Store::create_child_lazy(
        parent,
        &name,
        otto_core::session::Header {
            id: "child".into(),
            workspace: parent.parent().expect("dir").to_string_lossy().into_owned(),
            provider: "openai-compatible".into(),
            model: "test-model".into(),
            created_at: Utc::now(),
            ..otto_core::session::Header::default()
        },
    )
    .expect("child store");

    let spec = serde_json::to_string(&TaskSpecData {
        id: task_id.to_string(),
        name: String::new(),
        description: String::new(),
        model: "test-model".to_string(),
        model_source: super::super::tasks::ModelSource::Unknown,
        context: "fresh".to_string(),
        prompt: prompt.to_string(),
        definition,
        parent_tool_call_id: String::new(),
    })
    .expect("serialize task_spec");
    store
        .append_custom_entry(TASK_SPEC_CUSTOM_TYPE, &spec)
        .expect("append task_spec");

    if started {
        let message = Message {
            role: Role::User,
            blocks: vec![Block::text(prompt)],
            created_at: Utc::now(),
            ..Message::default()
        };
        store.append_message(&message).expect("append prompt");
    }
    if dangling {
        store
            .append_message(&dangling_call())
            .expect("append dangling call");
    }
    if let Some(names) = capabilities {
        store
            .append_custom_entry(
                otto_core::session::CAPABILITIES_CUSTOM_TYPE,
                &otto_core::session::encode_capabilities(&otto_core::session::Capabilities {
                    tools: names.to_vec(),
                    ..Default::default()
                }),
            )
            .expect("append capabilities");
    }

    let result = serde_json::to_string(&TaskResultData { status, error: "" })
        .expect("serialize task_result");
    store
        .append_custom_entry(TASK_RESULT_CUSTOM_TYPE, &result)
        .expect("append task_result");

    let path = crate::session::Store::child_path(parent, &name);
    store.close().expect("close");
    path
}

/// A [`Config`] whose `children_dir` resolves to `dir`, for resume tests.
fn config_with_children_dir(
    provider: &Arc<FakeProvider>,
    tasks: &Arc<Tasks>,
    dir: PathBuf,
) -> Config {
    let mut config = test_config(provider, tasks, Vec::new());
    config.children_dir = Some(Arc::new(move || Some(dir.clone())));
    config
}

#[tokio::test]
async fn restart_preserves_recorded_model_source_without_rewriting_history() {
    use super::super::tasks::ModelSource;
    for source in [
        None,
        Some(ModelSource::Explicit),
        Some(ModelSource::Definition),
        Some(ModelSource::ConfiguredFirst),
        Some(ModelSource::Parent),
    ] {
        let dir = tempfile::tempdir().expect("dir");
        let parent = dir.path().join("parent.jsonl");
        let path = interrupted_fixture(
            &parent,
            "t1",
            "original",
            None,
            true,
            true,
            interrupted::INTERRUPTED_STATUS,
        );
        // Simulate both actual legacy JSON (field absent) and a current spec.
        // This fixture preparation precedes the production read/append path.
        let text = std::fs::read_to_string(&path).expect("transcript");
        let text = match source {
            None => text.replace(",\"model_source\":\"unknown\"", ""),
            Some(source) => text.replace(
                "\"model_source\":\"unknown\"",
                &format!("\"model_source\":\"{}\"", source.as_str()),
            ),
        };
        std::fs::write(&path, &text).expect("fixture");
        let expected = source.unwrap_or_default();
        let records = interrupted::scan(&parent.with_extension(""));
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].model_source, expected);
        let provider = FakeProvider::new();
        provider.add_route(match_any, vec![assistant_text("done", Usage::default())]);
        let tasks = Arc::new(Tasks::new());
        let mut config = config_with_children_dir(&provider, &tasks, parent.with_extension(""));
        config.template.model = "different-parent".into();
        config.template.subagent_models = vec!["different-candidate".into()];
        let (runner, _) = runner(config);
        runner
            .resume("t1", "continue".into())
            .await
            .expect("resume");
        let task = wait_final(&tasks, "t1").await;
        assert_eq!(task.model_source, expected);
        assert_eq!(task.model, "test-model");
        let after = std::fs::read_to_string(&path).expect("resumed transcript");
        assert!(after.starts_with(&text), "resume must only append history");
        assert_eq!(after.matches("otto.task_spec").count(), 1);
    }
}

#[tokio::test]
async fn resume_delivers_parent_messages_the_child_had_not_read() {
    let dir = tempfile::tempdir().expect("dir");
    let parent_path = dir.path().join("parent.jsonl");
    let children_dir = parent_path.with_extension("");
    let path = interrupted_fixture(
        &parent_path,
        "t1",
        "look at this",
        None,
        true,
        true,
        interrupted::INTERRUPTED_STATUS,
    );
    let sidecar = path.with_extension("inbox.json");
    std::fs::write(
            &sidecar,
            r#"[{"seq":0,"task_id":"t1","kind":"message","text":"[parent-message] use the v2 API","usage":null,"id":"in_1"}]"#,
        )
        .expect("pending parent message");

    let provider = FakeProvider::new();
    provider.add_route(
        match_any,
        vec![assistant_text("all done", Usage::default())],
    );
    let tasks = Arc::new(Tasks::new());
    let mut config = config_with_children_dir(&provider, &tasks, children_dir);
    config.child_mailbox = Some(crate::runtime::wiring::child_mailbox());
    let (runner, _) = runner(config);
    runner
        .resume("t1", "continue please".to_string())
        .await
        .expect("resume");
    let final_task = wait_final(&tasks, "t1").await;
    assert_eq!(final_task.status, TaskStatus::Succeeded);

    let requests = provider.requests();
    let request = &requests[0];
    assert!(
        request
            .messages
            .iter()
            .any(|message| message.text().contains("use the v2 API")),
        "the unread parent message reaches the resumed child: {:?}",
        request.messages
    );
    assert!(!sidecar.exists(), "delivered messages leave the sidecar");
}

#[tokio::test]
async fn resume_continues_an_interrupted_task_through_the_agent_tool() {
    let dir = tempfile::tempdir().expect("dir");
    let parent_path = dir.path().join("parent.jsonl");
    let children_dir = parent_path.with_extension("");
    let path = interrupted_fixture(
        &parent_path,
        "t1",
        "look at this",
        Some(TaskSpecDefinition {
            name: "reviewer".to_string(),
            body: "Review the change.".to_string(),
            tools: None,
            write_policy: "read_only".to_string(),
            write_paths: Vec::new(),
        }),
        true,
        true,
        interrupted::INTERRUPTED_STATUS,
    );

    let provider = FakeProvider::new();
    provider.add_route(
        match_any,
        vec![assistant_text("all done", Usage::default())],
    );
    let tasks = Arc::new(Tasks::new());
    let (runner, _) = runner(config_with_children_dir(&provider, &tasks, children_dir));

    let tools = crate::subagent::tools::tools(&runner);
    let agent_tool = tools
        .iter()
        .find(|tool| tool.definition().name == "agent")
        .expect("the agent tool is registered");
    let result = agent_tool
        .execute(
            &raw(r#"{"resume":"t1","prompt":"continue please"}"#),
            &CancellationToken::new(),
        )
        .await;
    assert!(!result.is_error, "{}", result.content);

    let final_task = wait_final(&tasks, "t1").await;
    assert_eq!(final_task.id, "t1");
    assert_eq!(final_task.status, TaskStatus::Succeeded);

    let notification = tasks
        .notifications()
        .remove("t1", NotificationKind::TaskFinished)
        .expect("a task_finished notification reaches the parent inbox");
    assert_eq!(
        notification.text,
        completion_text(&final_task, runner.max_output_bytes())
    );

    let requests = provider.requests();
    assert_eq!(requests.len(), 1);
    let request = &requests[0];
    assert!(
        request.system_prompt.contains("Review the change."),
        "system prompt must use the recorded definition body: {}",
        request.system_prompt
    );
    assert_eq!(last_user_text(request), "continue please");
    assert!(
        request
            .messages
            .iter()
            .any(|message| message.role == Role::Tool
                && message
                    .blocks
                    .iter()
                    .any(|block| block.tool_call_id == "call-1")),
        "the request must carry the synthetic result for the dangling call: {:?}",
        request.messages
    );

    let entries = read_custom_entries(&path);
    let last = entries.last().expect("at least one custom entry");
    assert_eq!(last.0, "otto.task_result");
    assert_eq!(last.1["status"], "succeeded");
}

#[tokio::test]
async fn resume_of_a_never_started_task_sends_the_new_prompt_as_the_first_message() {
    let dir = tempfile::tempdir().expect("dir");
    let parent_path = dir.path().join("parent.jsonl");
    let children_dir = parent_path.with_extension("");
    interrupted_fixture(
        &parent_path,
        "t1",
        "look at this",
        None,
        false,
        false,
        interrupted::INTERRUPTED_STATUS,
    );

    let provider = FakeProvider::new();
    provider.add_route(match_any, vec![assistant_text("done", Usage::default())]);
    let tasks = Arc::new(Tasks::new());
    let (runner, _) = runner(config_with_children_dir(&provider, &tasks, children_dir));

    runner
        .resume("t1", "start now".to_string())
        .await
        .expect("resume succeeds");
    wait_final(&tasks, "t1").await;

    let requests = provider.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].messages.len(), 1, "{:?}", requests[0].messages);
    assert_eq!(requests[0].messages[0].role, Role::User);
    assert_eq!(requests[0].messages[0].text(), "start now");
}

/// Resumes a started, dangling child whose recorded tool set is
/// `recorded`, with a runner that offers `stub("read")`, `stub("grep")` and
/// the child-only `agent_report`. Returns the first provider request.
async fn resume_with_recorded_tools(recorded: &[&str]) -> otto_core::provider::Request {
    let dir = tempfile::tempdir().expect("dir");
    let parent_path = dir.path().join("parent.jsonl");
    let children_dir = parent_path.with_extension("");
    let recorded: Vec<String> = recorded.iter().map(|name| (*name).to_string()).collect();
    interrupted_fixture_with_capabilities(
        &parent_path,
        "t1",
        "look at this",
        None,
        true,
        true,
        interrupted::INTERRUPTED_STATUS,
        Some(&recorded),
    );

    let provider = FakeProvider::new();
    provider.add_route(match_any, vec![assistant_text("done", Usage::default())]);
    let tasks = Arc::new(Tasks::new());
    let mut config = config_with_children_dir(&provider, &tasks, children_dir);
    config.tools = vec![stub("read"), stub("grep")];
    let (runner, _) = runner(config);

    runner
        .resume("t1", "continue please".to_string())
        .await
        .expect("resume succeeds");
    wait_final(&tasks, "t1").await;
    let requests = provider.requests();
    assert_eq!(requests.len(), 1);
    requests[0].clone()
}

#[tokio::test]
async fn resume_announces_a_changed_tool_set_once_before_the_prompt() {
    let request = resume_with_recorded_tools(&["agent_report", "grep", "retired_tool"]).await;

    let notices: Vec<usize> = request
        .messages
        .iter()
        .enumerate()
        .filter(|(_, message)| message.context_type == "capability_update")
        .map(|(index, _)| index)
        .collect();
    assert_eq!(notices.len(), 1, "{:#?}", request.messages);
    let index = notices[0];
    let notice = &request.messages[index];
    assert_eq!(notice.role, Role::Context);
    let text = notice.text();
    assert!(text.lines().any(|line| line == "Added: read"), "{text}");
    assert!(
        text.lines().any(|line| line == "Removed: retired_tool"),
        "{text}"
    );
    assert!(!text.contains("was not recorded"), "{text}");
    // After the stored history (prompt, dangling call and its result),
    // immediately before the resume prompt.
    assert!(index >= 3, "{:#?}", request.messages);
    assert_eq!(index, request.messages.len() - 2, "{:#?}", request.messages);
    let last = request.messages.last().expect("messages");
    assert_eq!(last.role, Role::User);
    assert_eq!(last.text(), "continue please");
}

#[tokio::test]
async fn resume_with_an_unchanged_tool_set_adds_no_notice() {
    let request = resume_with_recorded_tools(&["agent_report", "grep", "read"]).await;

    assert!(
        request
            .messages
            .iter()
            .all(|message| message.context_type != "capability_update"),
        "{:#?}",
        request.messages
    );
    assert_eq!(last_user_text(&request), "continue please");
}

#[tokio::test]
async fn agent_tool_rejects_resume_of_a_task_that_is_not_interrupted() {
    let dir = tempfile::tempdir().expect("dir");
    let parent_path = dir.path().join("parent.jsonl");
    let children_dir = parent_path.with_extension("");
    interrupted_fixture(&parent_path, "t1", "go", None, true, false, "succeeded");

    let provider = FakeProvider::new();
    let tasks = Arc::new(Tasks::new());
    let (runner, _) = runner(config_with_children_dir(&provider, &tasks, children_dir));
    let tools = crate::subagent::tools::tools(&runner);
    let agent_tool = tools
        .iter()
        .find(|tool| tool.definition().name == "agent")
        .expect("the agent tool is registered");

    let result = agent_tool
        .execute(
            &raw(r#"{"resume":"t1","prompt":"continue"}"#),
            &CancellationToken::new(),
        )
        .await;
    assert!(result.is_error);
    assert!(
        result.content.contains("not interrupted"),
        "{}",
        result.content
    );
    assert!(result.content.contains("Use agent_status"));
    assert!(result.content.contains("read the report"));
}

#[tokio::test]
async fn agent_tool_rejects_resume_of_an_unknown_task() {
    let dir = tempfile::tempdir().expect("dir");
    let parent_path = dir.path().join("parent.jsonl");
    let children_dir = parent_path.with_extension("");
    std::fs::create_dir_all(&children_dir).expect("create children dir");

    let provider = FakeProvider::new();
    let tasks = Arc::new(Tasks::new());
    let (runner, _) = runner(config_with_children_dir(&provider, &tasks, children_dir));
    let tools = crate::subagent::tools::tools(&runner);
    let agent_tool = tools
        .iter()
        .find(|tool| tool.definition().name == "agent")
        .expect("the agent tool is registered");

    let result = agent_tool
        .execute(
            &raw(r#"{"resume":"unknown","prompt":"continue"}"#),
            &CancellationToken::new(),
        )
        .await;
    assert!(result.is_error);
    assert!(result.content.contains("unknown"), "{}", result.content);
}

#[tokio::test]
async fn agent_tool_rejects_resume_with_model_set() {
    let dir = tempfile::tempdir().expect("dir");
    let parent_path = dir.path().join("parent.jsonl");
    let children_dir = parent_path.with_extension("");
    interrupted_fixture(
        &parent_path,
        "t1",
        "go",
        None,
        true,
        false,
        interrupted::INTERRUPTED_STATUS,
    );

    let provider = FakeProvider::new();
    let tasks = Arc::new(Tasks::new());
    let (runner, _) = runner(config_with_children_dir(&provider, &tasks, children_dir));
    let tools = crate::subagent::tools::tools(&runner);
    let agent_tool = tools
        .iter()
        .find(|tool| tool.definition().name == "agent")
        .expect("the agent tool is registered");

    let result = agent_tool
        .execute(
            &raw(r#"{"resume":"t1","prompt":"continue","model":"gpt-4o"}"#),
            &CancellationToken::new(),
        )
        .await;
    assert!(result.is_error);
    assert!(result.content.contains("model"), "{}", result.content);
}

#[tokio::test]
async fn agent_tool_resume_accepts_fresh_context_and_empty_fields() {
    let dir = tempfile::tempdir().expect("dir");
    let parent_path = dir.path().join("parent.jsonl");
    let children_dir = parent_path.with_extension("");
    interrupted_fixture(
        &parent_path,
        "t1",
        "go",
        None,
        true,
        false,
        interrupted::INTERRUPTED_STATUS,
    );

    let provider = FakeProvider::new();
    provider.add_route(
        match_any,
        vec![assistant_text("all done", Usage::default())],
    );
    let tasks = Arc::new(Tasks::new());
    let (runner, _) = runner(config_with_children_dir(&provider, &tasks, children_dir));
    let tools = crate::subagent::tools::tools(&runner);
    let agent_tool = tools
        .iter()
        .find(|tool| tool.definition().name == "agent")
        .expect("the agent tool is registered");

    let result = agent_tool
            .execute(
                &raw(
                    r#"{"agent":"","context":"fresh","description":"","model":"","name":"","prompt":"Retry the audit.","resume":"t1"}"#,
                ),
                &CancellationToken::new(),
            )
            .await;
    assert!(!result.is_error, "{}", result.content);
    wait_final(&tasks, "t1").await;
}

#[tokio::test]
async fn agent_tool_rejects_resume_with_inherit_context() {
    let dir = tempfile::tempdir().expect("dir");
    let parent_path = dir.path().join("parent.jsonl");
    let children_dir = parent_path.with_extension("");
    interrupted_fixture(
        &parent_path,
        "t1",
        "go",
        None,
        true,
        false,
        interrupted::INTERRUPTED_STATUS,
    );

    let provider = FakeProvider::new();
    let tasks = Arc::new(Tasks::new());
    let (runner, _) = runner(config_with_children_dir(&provider, &tasks, children_dir));
    let tools = crate::subagent::tools::tools(&runner);
    let agent_tool = tools
        .iter()
        .find(|tool| tool.definition().name == "agent")
        .expect("the agent tool is registered");

    let result = agent_tool
        .execute(
            &raw(r#"{"resume":"t1","prompt":"continue","context":"inherit"}"#),
            &CancellationToken::new(),
        )
        .await;
    assert!(result.is_error);
    assert!(result.content.contains("context"), "{}", result.content);
}

#[tokio::test]
async fn agent_tool_rejects_resume_with_fork_before() {
    let dir = tempfile::tempdir().expect("dir");
    let parent_path = dir.path().join("parent.jsonl");
    let children_dir = parent_path.with_extension("");
    interrupted_fixture(
        &parent_path,
        "t1",
        "go",
        None,
        true,
        false,
        interrupted::INTERRUPTED_STATUS,
    );

    let provider = FakeProvider::new();
    let tasks = Arc::new(Tasks::new());
    let (runner, _) = runner(config_with_children_dir(&provider, &tasks, children_dir));
    let tools = crate::subagent::tools::tools(&runner);
    let agent_tool = tools
        .iter()
        .find(|tool| tool.definition().name == "agent")
        .expect("the agent tool is registered");

    let result = agent_tool
        .execute(
            &raw(r#"{"resume":"t1","prompt":"continue","fork_before":"go"}"#),
            &CancellationToken::new(),
        )
        .await;
    assert!(result.is_error);
    assert_eq!(result.content, "resume does not accept fork_before");
}

#[tokio::test]
async fn agent_tool_rejects_resume_without_a_prompt() {
    let dir = tempfile::tempdir().expect("dir");
    let parent_path = dir.path().join("parent.jsonl");
    let children_dir = parent_path.with_extension("");
    interrupted_fixture(
        &parent_path,
        "t1",
        "go",
        None,
        true,
        false,
        interrupted::INTERRUPTED_STATUS,
    );

    let provider = FakeProvider::new();
    let tasks = Arc::new(Tasks::new());
    let (runner, _) = runner(config_with_children_dir(&provider, &tasks, children_dir));
    let tools = crate::subagent::tools::tools(&runner);
    let agent_tool = tools
        .iter()
        .find(|tool| tool.definition().name == "agent")
        .expect("the agent tool is registered");

    let result = agent_tool
        .execute(
            &raw(r#"{"resume":"t1","prompt":"   "}"#),
            &CancellationToken::new(),
        )
        .await;
    assert!(result.is_error);
    assert!(
        result.content.contains("prompt is required"),
        "{}",
        result.content
    );
}

#[tokio::test]
async fn resume_of_a_task_already_resumed_and_running_again_is_rejected() {
    let dir = tempfile::tempdir().expect("dir");
    let parent_path = dir.path().join("parent.jsonl");
    let children_dir = parent_path.with_extension("");

    let store = crate::session::Store::create_child_lazy(
        &parent_path,
        "t1-child",
        otto_core::session::Header {
            id: "child".into(),
            workspace: parent_path
                .parent()
                .expect("dir")
                .to_string_lossy()
                .into_owned(),
            provider: "openai-compatible".into(),
            model: "test-model".into(),
            created_at: Utc::now(),
            ..otto_core::session::Header::default()
        },
    )
    .expect("child store");

    let spec = serde_json::to_string(&TaskSpecData {
        id: "t1".to_string(),
        name: String::new(),
        description: String::new(),
        model: "test-model".to_string(),
        model_source: super::super::tasks::ModelSource::Unknown,
        context: "fresh".to_string(),
        prompt: "look at this".to_string(),
        definition: None,
        parent_tool_call_id: String::new(),
    })
    .expect("serialize task_spec");
    store
        .append_custom_entry(TASK_SPEC_CUSTOM_TYPE, &spec)
        .expect("append task_spec");

    let result = serde_json::to_string(&TaskResultData {
        status: interrupted::INTERRUPTED_STATUS,
        error: "",
    })
    .expect("serialize task_result");
    store
        .append_custom_entry(TASK_RESULT_CUSTOM_TYPE, &result)
        .expect("append task_result");

    // The task was resumed and ran again: a user message and an assistant
    // reply were appended after the otto.task_result entry.
    store
        .append_message(&Message {
            role: Role::User,
            blocks: vec![Block::text("continue please")],
            created_at: Utc::now(),
            ..Message::default()
        })
        .expect("append resumed prompt");
    store
        .append_message(&Message {
            role: Role::Assistant,
            blocks: vec![Block::text("still working")],
            finish_reason: Some(FinishReason::Stop),
            created_at: Utc::now(),
            ..Message::default()
        })
        .expect("append resumed reply");
    store.close().expect("close");

    let records = interrupted::scan(&children_dir);
    assert_eq!(records.len(), 1);
    assert_eq!(
        records[0].final_status, None,
        "an entry appended after otto.task_result must clear final_status"
    );

    let provider = FakeProvider::new();
    let tasks = Arc::new(Tasks::new());
    let (runner, _) = runner(config_with_children_dir(&provider, &tasks, children_dir));
    let error = runner
        .resume("t1", "again".to_string())
        .await
        .expect_err("resume must reject a task that is running again");
    assert_eq!(error, StartError::ResumeNotInterrupted("t1".to_string()));
}

#[tokio::test]
async fn resume_fails_when_the_recorded_write_policy_is_unrecognized() {
    let dir = tempfile::tempdir().expect("dir");
    let parent_path = dir.path().join("parent.jsonl");
    let children_dir = parent_path.with_extension("");
    interrupted_fixture(
        &parent_path,
        "t1",
        "go",
        Some(TaskSpecDefinition {
            name: "reviewer".to_string(),
            body: "Review the change.".to_string(),
            tools: None,
            write_policy: "bogus".to_string(),
            write_paths: Vec::new(),
        }),
        true,
        false,
        interrupted::INTERRUPTED_STATUS,
    );

    let provider = FakeProvider::new();
    let tasks = Arc::new(Tasks::new());
    let (runner, _) = runner(config_with_children_dir(&provider, &tasks, children_dir));

    let error = runner
        .resume("t1", "continue".to_string())
        .await
        .expect_err("resume must reject an unrecognized write_policy");
    let StartError::ResumeFailed(message) = error else {
        panic!("expected ResumeFailed, got {error:?}");
    };
    assert!(message.contains("bogus"), "{message}");
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
