
use super::*;
use crate::subagent::Definition;
use crate::subagent::runner::{Config, StartRequest};
use crate::subagent::testsupport::{
    FakeProvider, RouteStep, StubTool, assistant_text, assistant_tool_call, match_any, raw, stub,
    test_config, wait_status,
};
use otto_core::agent::inbox::NotificationKind;
use otto_core::model::Usage;
use otto_core::tool::ToolResult;
use std::sync::Arc;

fn runner(config: Config) -> Arc<Runner> {
    let (runner, _) = Runner::new(config).expect("the test config is valid");
    Arc::new(runner)
}

fn tool_named(runner: &Arc<Runner>, name: &str) -> Box<dyn Tool + Send + Sync> {
    tools(runner)
        .into_iter()
        .find(|tool| tool.definition().name == name)
        .unwrap_or_else(|| panic!("tool {name:?} is registered"))
}

async fn run(tool: &dyn Tool, arguments: &str) -> ToolResult {
    tool.execute(&raw(arguments), &CancellationToken::new())
        .await
}

async fn wait_final(tasks: &Tasks, id: &str) -> Task {
    let cancel = CancellationToken::new();
    tokio::select! {
        result = tasks.wait(id, &cancel) => result.expect("the task exists"),
        () = tokio::time::sleep(std::time::Duration::from_secs(5)) => {
            panic!("task {id} did not finish in time")
        }
    }
}

/// A hook that blocks every provider call until `release` fires, or the
/// child is cancelled.
fn block_until(release: CancellationToken) -> crate::subagent::testsupport::Hook {
    Arc::new(move |cancel, _request| {
        let release = release.clone();
        Box::pin(async move {
            tokio::select! {
                () = release.cancelled() => {}
                () = cancel.cancelled() => {}
            }
        })
    })
}

#[tokio::test]
async fn agent_records_the_spawning_tool_call_in_the_child_task_spec() {
    let dir = tempfile::tempdir().expect("dir");
    let parent = dir.path().join("parent.jsonl");
    let provider = FakeProvider::new();
    provider.add_route(match_any, vec![assistant_text("done", Usage::default())]);
    let tasks = Arc::new(Tasks::new());
    let mut config = test_config(&provider, &tasks, Vec::new());
    config.child_session = Some(Arc::new(move |task_id: &str| {
        let name = format!("{task_id}-child");
        let store = crate::session::Store::create_child_lazy(
            &parent,
            &name,
            otto_core::session::Header {
                id: "child".into(),
                workspace: parent.parent().expect("dir").to_string_lossy().into_owned(),
                provider: "openai-compatible".into(),
                model: "test-model".into(),
                created_at: chrono::Utc::now(),
                ..otto_core::session::Header::default()
            },
        )
        .expect("child store");
        let path = crate::session::Store::child_path(&parent, &name);
        Ok(Some((Arc::new(store), path.to_string_lossy().into_owned())))
    }));
    let runner = runner(config);
    let tool = tool_named(&runner, "agent");
    let operation_id = otto_core::model::OperationId::new("op_test").expect("operation id");
    let arguments = raw(r#"{"prompt":"look"}"#);
    tool.execute_call(
        otto_core::tool::ToolCall {
            tool_call_id: "call_7",
            operation_id: &operation_id,
            name: "agent",
            arguments: &arguments,
            attempt: 1,
        },
        &CancellationToken::new(),
    )
    .await;
    let task = wait_final(&tasks, "t1").await;

    let text = std::fs::read_to_string(&task.session_path).expect("child transcript");
    let spec: serde_json::Value = text
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .find(|entry| entry["customType"] == "otto.task_spec")
        .expect("task spec entry");
    assert_eq!(spec["data"]["parentToolCallId"], "call_7");
}

#[tokio::test(start_paused = true)]
async fn agent_yield_is_nonblocking_and_requires_an_outstanding_child() {
    let provider = FakeProvider::new();
    let tasks = Arc::new(Tasks::new());
    let runner = runner(test_config(&provider, &tasks, Vec::new()));
    let tool = tool_named(&runner, "agent_yield");
    assert!(run(tool.as_ref(), "{}").await.is_error);
    let task = tasks.add(Task::default(), None, None).unwrap();
    let result = tokio::time::timeout(std::time::Duration::from_secs(1), run(tool.as_ref(), "{}"))
        .await
        .expect("yield must not wait inside the tool");
    assert!(!result.is_error);
    assert!(tool.yields_turn());
    assert!(provider.requests().is_empty());
    assert!(!tasks.get(&task.id).unwrap().is_final());
    assert!(run(tool.as_ref(), r#"{"unexpected":true}"#).await.is_error);
}

#[tokio::test]
async fn live_activity_clears_the_tool_phase_without_losing_last_tool() {
    let tasks = Tasks::new();
    let task = tasks.add(Task::default(), None, None).unwrap();
    assert_eq!(tasks.activity(&task.id), Some("queued"));
    tasks.mark_running(&task.id, chrono::Utc::now());
    assert_eq!(tasks.activity(&task.id), Some("provider_or_runtime"));
    tasks.record_tool_call(&task.id, "read file.rs");
    assert_eq!(tasks.activity(&task.id), Some("tool"));
    tasks.record_tool_finished(&task.id);
    assert_eq!(tasks.activity(&task.id), Some("provider_or_runtime"));
    assert_eq!(tasks.get(&task.id).unwrap().last_tool, "read file.rs");
}

#[tokio::test]
async fn agent_returns_while_a_child_is_still_running() {
    let provider = FakeProvider::new();
    provider.set_hook(block_until(CancellationToken::new()));
    let tasks = Arc::new(Tasks::new());
    let runner = runner(test_config(&provider, &tasks, Vec::new()));

    let result = tokio::time::timeout(
        std::time::Duration::from_secs(1),
        run(
            tool_named(&runner, "agent").as_ref(),
            r#"{"prompt":"do it"}"#,
        ),
    )
    .await
    .expect("agent blocked on its child");
    assert!(!result.is_error, "{}", result.content);
    assert!(result.content.contains("task t1"), "{}", result.content);
    wait_status(&tasks, "t1", TaskStatus::Running).await;

    tasks.cancel("t1").expect("cancel succeeds");
    wait_final(&tasks, "t1").await;
    assert_eq!(
        tasks.notifications().snapshot()[0].kind,
        Some(NotificationKind::TaskFinished),
        "completion stays queued for the runtime wake turn"
    );
}

#[tokio::test]
async fn agent_rejects_the_removed_wait_argument() {
    let provider = FakeProvider::new();
    let tasks = Arc::new(Tasks::new());
    let runner = runner(test_config(&provider, &tasks, Vec::new()));

    let result = run(
        tool_named(&runner, "agent").as_ref(),
        r#"{"prompt":"do it","wait":true}"#,
    )
    .await;
    assert!(result.is_error, "{}", result.content);
    assert!(tasks.list().is_empty());
}

#[tokio::test]
async fn agent_tool_rejects_blank_prompt() {
    let provider = FakeProvider::new();
    let tasks = Arc::new(Tasks::new());
    let runner = runner(test_config(&provider, &tasks, Vec::new()));
    let agent_tool = tool_named(&runner, "agent");

    for body in [r#"{"prompt":"   "}"#, "{}"] {
        assert!(
            run(agent_tool.as_ref(), body).await.is_error,
            "body {body} must be rejected"
        );
    }
    assert!(tasks.list().is_empty());
}

#[tokio::test]
async fn agent_tool_model_selection() {
    for (body, want) in [
        (r#"{"prompt":"go","model":"cheap-model"}"#, "cheap-model"),
        (r#"{"prompt":"go"}"#, "gpt-parent"),
        (r#"{"prompt":"go","model":"   "}"#, "gpt-parent"),
    ] {
        let provider = FakeProvider::new();
        provider.add_route(match_any, vec![assistant_text("done", Usage::default())]);
        let tasks = Arc::new(Tasks::new());
        let mut config = test_config(&provider, &tasks, Vec::new());
        config.template.model = "gpt-parent".into();
        let runner = runner(config);

        let result = run(tool_named(&runner, "agent").as_ref(), body).await;
        assert!(!result.is_error, "{}", result.content);

        assert_eq!(wait_final(&tasks, "t1").await.model, want, "body {body}");
        assert_eq!(
            provider.requests().first().expect("one request").model,
            want
        );
    }
}

#[tokio::test]
async fn agent_tool_uses_configured_model_when_omitted() {
    let provider = FakeProvider::new();
    provider.add_route(match_any, vec![assistant_text("done", Usage::default())]);
    let tasks = Arc::new(Tasks::new());
    let mut config = test_config(&provider, &tasks, Vec::new());
    config.template.model = "parent".into();
    config.template.subagent_models = vec!["cheap".into(), "strong".into()];
    let runner = runner(config);
    let result = run(tool_named(&runner, "agent").as_ref(), r#"{"prompt":"go"}"#).await;
    assert!(!result.is_error, "{}", result.content);
    assert_eq!(wait_final(&tasks, "t1").await.model, "cheap");
    assert_eq!(provider.requests()[0].model, "cheap");
}

#[tokio::test]
async fn agent_status_empty_registry() {
    let provider = FakeProvider::new();
    let tasks = Arc::new(Tasks::new());
    let runner = runner(test_config(&provider, &tasks, Vec::new()));
    let result = run(tool_named(&runner, "agent_status").as_ref(), "{}").await;
    assert!(!result.is_error, "{}", result.content);
    assert_eq!(result.content, "no tasks in this session");
}

#[tokio::test]
async fn agent_status_unknown_task_id() {
    let provider = FakeProvider::new();
    provider.add_route(match_any, vec![assistant_text("done", Usage::default())]);
    let tasks = Arc::new(Tasks::new());
    let runner = runner(test_config(&provider, &tasks, Vec::new()));
    runner
        .start(StartRequest {
            prompt: "go".into(),
            ..StartRequest::default()
        })
        .await
        .expect("start succeeds");

    let result = run(
        tool_named(&runner, "agent_status").as_ref(),
        r#"{"task_id":"t9"}"#,
    )
    .await;
    assert!(result.is_error);
    assert_eq!(result.content, "unknown task: t9");
}

#[tokio::test]
async fn agent_status_listing_shows_running_and_queued_rows() {
    let provider = FakeProvider::new();
    provider.set_hook(block_until(CancellationToken::new()));
    let tasks = Arc::new(Tasks::new());
    let mut config = test_config(&provider, &tasks, Vec::new());
    config.max_parallel = 1;
    let runner = runner(config);

    runner
        .start(StartRequest {
            prompt: "explore the repo".into(),
            description: "explore".into(),
            ..StartRequest::default()
        })
        .await
        .expect("start succeeds");
    wait_status(&tasks, "t1", TaskStatus::Running).await;
    runner
        .start(StartRequest {
            prompt: "review the diff".into(),
            ..StartRequest::default()
        })
        .await
        .expect("start succeeds");

    let result = run(tool_named(&runner, "agent_status").as_ref(), "{}").await;
    assert!(!result.is_error, "{}", result.content);
    let lines: Vec<&str> = result.content.split('\n').collect();
    assert_eq!(lines.len(), 2, "{}", result.content);
    for needle in ["t1", "(default)", "running", "explore"] {
        assert!(lines[0].contains(needle), "t1 line = {:?}", lines[0]);
    }
    for needle in ["t2", "(default)", "queued", "review the diff"] {
        assert!(lines[1].contains(needle), "t2 line = {:?}", lines[1]);
    }

    for id in ["t1", "t2"] {
        tasks.cancel(id).expect("cancel succeeds");
        wait_final(&tasks, id).await;
    }
}

#[tokio::test]
async fn agent_status_detail_shows_history_and_result() {
    let provider = FakeProvider::new();
    let usage = Usage {
        input_tokens: 1,
        output_tokens: 1,
        ..Usage::default()
    };
    provider.add_route(
        match_any,
        vec![
            assistant_tool_call("call-1", "echo", r#"{"msg":"hi"}"#, usage),
            assistant_text("wrap-up", usage),
        ],
    );
    let echo = StubTool::new(
        "echo",
        ToolResult {
            content: "echoed".into(),
            ..ToolResult::default()
        },
    );
    let tasks = Arc::new(Tasks::new());
    let runner = runner(test_config(&provider, &tasks, vec![echo.boxed()]));
    runner
        .start(StartRequest {
            prompt: "go".into(),
            ..StartRequest::default()
        })
        .await
        .expect("start succeeds");
    wait_final(&tasks, "t1").await;

    let summary = run(
        tool_named(&runner, "agent_status").as_ref(),
        r#"{"task_id":"t1"}"#,
    )
    .await;
    assert!(summary.content.contains("result:\nwrap-up"));
    assert!(!summary.content.contains("assistant:"));
    assert!(!summary.content.contains("→ echo"));

    let result = run(
        tool_named(&runner, "agent_status").as_ref(),
        r#"{"task_id":"t1","include_history":true}"#,
    )
    .await;
    assert!(!result.is_error, "{}", result.content);
    for needle in ["succeeded", "echo", "assistant: wrap-up"] {
        assert!(result.content.contains(needle), "{}", result.content);
    }
    assert!(
        result.content.ends_with("result:\nwrap-up"),
        "{}",
        result.content
    );
}

#[tokio::test]
async fn parent_handoffs_are_bounded_but_the_full_child_result_is_retained() {
    let provider = FakeProvider::new();
    // The runner trims the final assistant text; end the fixture without
    // whitespace so the retained result equals it byte for byte.
    let report = "conclusion and evidence "
        .repeat(1000)
        .trim_end()
        .to_owned();
    provider.add_route(match_any, vec![assistant_text(&report, Usage::default())]);
    let tasks = Arc::new(Tasks::new());
    let runner = runner(test_config(&provider, &tasks, Vec::new()));
    runner
        .start(StartRequest {
            prompt: "research".into(),
            ..StartRequest::default()
        })
        .await
        .unwrap();
    wait_final(&tasks, "t1").await;
    assert_eq!(tasks.get("t1").unwrap().result, report);
    let notices = tasks.notifications().snapshot();
    assert_eq!(notices.len(), 1);
    assert!(notices[0].text.len() < 4300);
    assert!(notices[0].text.contains("truncated"));
    let result = run(
        tool_named(&runner, "agent_status").as_ref(),
        r#"{"task_id":"t1"}"#,
    )
    .await;
    assert!(result.content.len() < 4300);
    assert!(result.content.contains("truncated"));
}

#[tokio::test]
async fn long_final_report_is_readable_after_child_exit_without_another_model_call() {
    use crate::tool::{read::ReadTool, root::Root, workspace::Workspace};
    let dir = tempfile::tempdir().unwrap();
    let provider = FakeProvider::new();
    let report = format!("# 摘要\n{}\n## 证据\n末尾证据", "中文研究内容".repeat(5000));
    provider.add_route(match_any, vec![assistant_text(&report, Usage::default())]);
    let tasks = Arc::new(Tasks::new());
    let runner = runner(test_config(&provider, &tasks, Vec::new()));
    runner.bind_scratch_root(Root::open(dir.path()).unwrap());
    runner
        .start(StartRequest {
            prompt: "research".into(),
            name: "research".into(),
            ..StartRequest::default()
        })
        .await
        .unwrap();
    let task = wait_final(&tasks, "t1").await;
    assert_eq!(task.status, TaskStatus::Succeeded);
    assert_eq!(task.result, report);
    let notices = tasks.notifications().snapshot();
    let notice = &notices[0].text;
    let report_path = notice
        .split("[Full report: ")
        .nth(1)
        .expect("read pointer survives truncation")
        .split(" (")
        .next()
        .unwrap()
        .to_owned();
    assert!(notice.len() < 4700);
    assert!(notice.contains("truncated"));
    assert!(notice.contains("UTF-8 bytes"));
    assert!(notice.contains("/index.md"));
    let status = run(
        tool_named(&runner, "agent_status").as_ref(),
        r#"{"task_id":"research","include_history":true}"#,
    )
    .await;
    assert!(status.content.contains(&report_path));
    assert!(status.content.len() < 4700);
    let prompt = &provider.requests()[0].system_prompt;
    assert!(prompt.contains("<research_report>"));
    assert!(prompt.contains("/report.md"));
    assert!(prompt.contains("Existing write permissions"));
    drop(runner);

    let workspace = Workspace::new(dir.path()).unwrap();
    let read = ReadTool::new(&workspace, 16384);
    let index_path = report_path.replace("/final.md", "/index.md");
    let index = run(&read, &serde_json::json!({"path": index_path}).to_string()).await;
    assert!(!index.is_error, "{}", index.content);
    assert!(index.content.contains("line 3: 证据"));
    let tail = run(
        &read,
        &serde_json::json!({"path": report_path, "offset": 3, "limit": 2}).to_string(),
    )
    .await;
    assert!(!tail.is_error, "{}", tail.content);
    assert!(tail.content.contains("末尾证据"));
    assert_eq!(
        std::fs::read_to_string(dir.path().join(&report_path)).unwrap(),
        report
    );
    assert_eq!(
        provider.requests().len(),
        1,
        "reading must not restart the child"
    );
}

#[tokio::test]
async fn tiny_preview_keeps_the_read_pointer_and_persisted_report_is_redacted() {
    use crate::tool::root::Root;
    let dir = tempfile::tempdir().unwrap();
    let provider = FakeProvider::new();
    let private = "private-report-marker";
    provider.add_route(
        match_any,
        vec![assistant_text(
            &format!("{private}\nfindings"),
            Usage::default(),
        )],
    );
    let tasks = Arc::new(Tasks::new());
    let mut config = test_config(&provider, &tasks, Vec::new());
    config.max_output_bytes = 1;
    config.redaction_values = vec![private.to_owned()];
    let runner = runner(config);
    runner.bind_scratch_root(Root::open(dir.path()).unwrap());
    runner
        .start(StartRequest {
            prompt: "research".into(),
            ..StartRequest::default()
        })
        .await
        .unwrap();
    let task = wait_final(&tasks, "t1").await;
    let notices = tasks.notifications().snapshot();
    let path = notices[0]
        .text
        .split("[Full report: ")
        .nth(1)
        .unwrap()
        .split(" (")
        .next()
        .unwrap();
    let saved = std::fs::read_to_string(dir.path().join(path)).unwrap();
    assert!(!saved.contains(private));
    assert!(saved.contains("findings"));
    assert_eq!(saved, task.result);
    let status = run(
        tool_named(&runner, "agent_status").as_ref(),
        r#"{"task_id":"t1"}"#,
    )
    .await;
    assert!(status.content.contains(path));
}

#[tokio::test]
async fn report_storage_failure_preserves_success_and_reports_the_missing_artifact() {
    use crate::tool::root::Root;
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join(".otto"), "blocks directory creation").unwrap();
    let provider = FakeProvider::new();
    let report = "findings ".repeat(1000).trim_end().to_owned();
    provider.add_route(match_any, vec![assistant_text(&report, Usage::default())]);
    let tasks = Arc::new(Tasks::new());
    let runner = runner(test_config(&provider, &tasks, Vec::new()));
    runner.bind_scratch_root(Root::open(dir.path()).unwrap());
    runner
        .start(StartRequest {
            prompt: "research".into(),
            ..StartRequest::default()
        })
        .await
        .unwrap();
    let task = wait_final(&tasks, "t1").await;
    assert_eq!(task.status, TaskStatus::Succeeded);
    assert_eq!(task.result, report);
    let notices = tasks.notifications().snapshot();
    assert!(notices[0].text.contains("report file unavailable"));
    assert!(!notices[0].text.contains("[Full report:"));
}

#[tokio::test]
async fn agent_status_detail_shows_model_when_set() {
    let provider = FakeProvider::new();
    provider.add_route(match_any, vec![assistant_text("done", Usage::default())]);
    let tasks = Arc::new(Tasks::new());
    let runner = runner(test_config(&provider, &tasks, Vec::new()));
    runner
        .start(StartRequest {
            prompt: "go".into(),
            model: "gpt-test-model".into(),
            ..StartRequest::default()
        })
        .await
        .expect("start succeeds");
    wait_final(&tasks, "t1").await;

    let result = run(
        tool_named(&runner, "agent_status").as_ref(),
        r#"{"task_id":"t1"}"#,
    )
    .await;
    let lines: Vec<&str> = result.content.split('\n').collect();
    assert!(lines.len() >= 2, "{}", result.content);
    assert_eq!(lines[1], "model: gpt-test-model", "{}", result.content);
    assert_eq!(lines[2], "model_source: explicit", "{}", result.content);
}

#[tokio::test]
async fn agent_status_detail_shows_unknown_for_legacy_task() {
    let provider = FakeProvider::new();
    let tasks = Arc::new(Tasks::new());
    let runner = runner(test_config(&provider, &tasks, Vec::new()));
    tasks
        .add(
            super::super::tasks::Task {
                model: "legacy-model".into(),
                ..Default::default()
            },
            None,
            None,
        )
        .expect("task");
    let result = run(
        tool_named(&runner, "agent_status").as_ref(),
        r#"{"task_id":"t1"}"#,
    )
    .await;
    assert!(!result.is_error, "{}", result.content);
    assert!(
        result.content.contains("model_source: unknown"),
        "{}",
        result.content
    );
}

#[tokio::test]
async fn agent_status_detail_shows_error_on_failure() {
    let provider = FakeProvider::new();
    provider.add_route(match_any, vec![RouteStep::Fail("boom".into())]);
    let tasks = Arc::new(Tasks::new());
    let runner = runner(test_config(&provider, &tasks, Vec::new()));
    runner
        .start(StartRequest {
            prompt: "go".into(),
            ..StartRequest::default()
        })
        .await
        .expect("start succeeds");
    wait_final(&tasks, "t1").await;

    let result = run(
        tool_named(&runner, "agent_status").as_ref(),
        r#"{"task_id":"t1"}"#,
    )
    .await;
    assert!(!result.is_error, "{}", result.content);
    assert!(result.content.contains("error: "), "{}", result.content);
    assert!(result.content.contains("boom"), "{}", result.content);
}

#[tokio::test]
async fn agent_tool_unknown_agent_error() {
    let provider = FakeProvider::new();
    let tasks = Arc::new(Tasks::new());
    let mut config = test_config(&provider, &tasks, Vec::new());
    config.catalog = Catalog::from_definitions(vec![Definition {
        name: "reviewer".into(),
        ..Definition::default()
    }]);
    let runner = runner(config);

    let result = run(
        tool_named(&runner, "agent").as_ref(),
        r#"{"prompt":"go","agent":"nope"}"#,
    )
    .await;
    assert!(result.is_error);
    assert!(
        result.content.contains("unknown agent: nope")
            && result.content.contains("available: reviewer"),
        "{}",
        result.content
    );
    assert!(tasks.list().is_empty());
}

#[tokio::test]
async fn agent_tool_named_agent_started_message() {
    let provider = FakeProvider::new();
    provider.add_route(match_any, vec![assistant_text("done", Usage::default())]);
    let tasks = Arc::new(Tasks::new());
    let mut config = test_config(&provider, &tasks, Vec::new());
    config.catalog = Catalog::from_definitions(vec![Definition {
        name: "reviewer".into(),
        ..Definition::default()
    }]);
    let runner = runner(config);

    let result = run(
        tool_named(&runner, "agent").as_ref(),
        r#"{"prompt":"go","agent":"reviewer"}"#,
    )
    .await;
    assert!(!result.is_error, "{}", result.content);
    assert_eq!(result.content, "task t1 (reviewer) started");
}

fn write_skill_with_vague_output(directory: &std::path::Path) {
    std::fs::create_dir_all(directory).expect("mkdir");
    std::fs::write(
            directory.join("SKILL.md"),
            "---\nname: reviewer\ndescription: reviews things\ninput: a path\noutput: the result\n---\nBody.\n",
        )
        .expect("write SKILL.md");
}

fn checked_skill(directory: &std::path::Path) -> crate::skill::Skill {
    crate::skill::Skill {
        name: "reviewer".to_string(),
        description: String::new(),
        contract: None,
        directory: directory.to_path_buf(),
        path: directory.join("SKILL.md"),
    }
}

#[tokio::test]
async fn agent_tool_triggers_one_check_for_a_skill_derived_definition() {
    let provider = FakeProvider::new();
    provider.add_route(match_any, vec![assistant_text("done", Usage::default())]);
    let tasks = Arc::new(Tasks::new());
    let dir = tempfile::tempdir().expect("tempdir");
    write_skill_with_vague_output(dir.path());
    // The base URL is never contacted: the vague `output` field fires a
    // rule, so this checks the trigger wiring without a network stub.
    let checker = Arc::new(crate::skill::check::Checker::open_in_memory(
        "http://127.0.0.1:1",
        "key",
    ));
    let mut config = test_config(&provider, &tasks, Vec::new());
    config.catalog = Catalog::from_definitions(vec![Definition {
        name: "reviewer".into(),
        directory: dir.path().to_path_buf(),
        path: dir.path().join("SKILL.md"),
        is_skill_derived: true,
        ..Definition::default()
    }]);
    config.checker = Some(Arc::clone(&checker));
    let runner = runner(config);

    let result = run(
        tool_named(&runner, "agent").as_ref(),
        r#"{"prompt":"go","agent":"reviewer"}"#,
    )
    .await;
    assert!(!result.is_error, "{}", result.content);

    let skill = checked_skill(dir.path());
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
    loop {
        if checker.display(&skill) != "not checked yet" {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "background check did not complete in time"
        );
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    let text = checker.display(&skill);
    assert!(text.starts_with("rules"), "{text}");
}

#[tokio::test]
async fn agent_tool_triggers_no_check_for_a_real_agent_definition() {
    let provider = FakeProvider::new();
    provider.add_route(match_any, vec![assistant_text("done", Usage::default())]);
    let tasks = Arc::new(Tasks::new());
    let dir = tempfile::tempdir().expect("tempdir");
    write_skill_with_vague_output(dir.path());
    let checker = Arc::new(crate::skill::check::Checker::open_in_memory(
        "http://127.0.0.1:1",
        "key",
    ));
    let mut config = test_config(&provider, &tasks, Vec::new());
    config.catalog = Catalog::from_definitions(vec![Definition {
        name: "reviewer".into(),
        directory: dir.path().to_path_buf(),
        path: dir.path().join("SKILL.md"),
        is_skill_derived: false,
        ..Definition::default()
    }]);
    config.checker = Some(Arc::clone(&checker));
    let runner = runner(config);

    let result = run(
        tool_named(&runner, "agent").as_ref(),
        r#"{"prompt":"go","agent":"reviewer"}"#,
    )
    .await;
    assert!(!result.is_error, "{}", result.content);

    let skill = checked_skill(dir.path());
    assert_eq!(checker.display(&skill), "not checked yet");
}

#[tokio::test]
async fn agent_tool_name_in_started_message() {
    let provider = FakeProvider::new();
    provider.add_route(match_any, vec![assistant_text("done", Usage::default())]);
    let tasks = Arc::new(Tasks::new());
    let runner = runner(test_config(&provider, &tasks, Vec::new()));

    let result = run(
        tool_named(&runner, "agent").as_ref(),
        r#"{"prompt":"go","name":"lint-check"}"#,
    )
    .await;
    assert!(!result.is_error, "{}", result.content);
    assert_eq!(result.content, "task t1 lint-check (default) started");
}

#[tokio::test]
async fn agent_tool_duplicate_name_is_error_result() {
    let provider = FakeProvider::new();
    provider.add_route(match_any, vec![assistant_text("done", Usage::default())]);
    let tasks = Arc::new(Tasks::new());
    let runner = runner(test_config(&provider, &tasks, Vec::new()));
    let agent_tool = tool_named(&runner, "agent");

    let first = run(
        agent_tool.as_ref(),
        r#"{"prompt":"go","name":"lint-check"}"#,
    )
    .await;
    assert!(!first.is_error, "{}", first.content);
    let second = run(
        agent_tool.as_ref(),
        r#"{"prompt":"go again","name":"lint-check"}"#,
    )
    .await;
    assert!(second.is_error);
    assert!(
        second.content.contains("already used by t1"),
        "{}",
        second.content
    );
    assert_eq!(tasks.list().len(), 1);
}

#[tokio::test]
async fn agent_send_queues_message_for_running_task() {
    let provider = FakeProvider::new();
    let release = CancellationToken::new();
    provider.set_hook(block_until(release.clone()));
    provider.add_route(
        match_any,
        vec![
            assistant_tool_call("call-1", "noop", "{}", Usage::default()),
            assistant_text("saw update", Usage::default()),
        ],
    );
    let tasks = Arc::new(Tasks::new());
    let runner = runner(test_config(&provider, &tasks, vec![stub("noop")]));
    runner
        .start(StartRequest {
            prompt: "start".into(),
            ..StartRequest::default()
        })
        .await
        .expect("start succeeds");
    wait_status(&tasks, "t1", TaskStatus::Running).await;

    let result = run(
        tool_named(&runner, "agent_send").as_ref(),
        r#"{"task_id":"t1","message":"use the new constraint"}"#,
    )
    .await;
    assert!(!result.is_error, "{}", result.content);
    assert_eq!(
        result.content,
        "message queued for task t1; it will be read at the next checkpoint"
    );
    release.cancel();
    wait_final(&tasks, "t1").await;

    let requests = provider.requests();
    let second = requests.get(1).expect("a second provider request was made");
    assert!(
        second.messages.iter().any(|message| {
            message.role == otto_core::model::Role::Context
                && message.context_type == "parent_message"
                && message.text().contains("use the new constraint")
        }),
        "second request should include the parent message: {second:#?}"
    );
}

#[tokio::test]
async fn agent_send_rejects_unknown_blank_and_finished_tasks() {
    let provider = FakeProvider::new();
    provider.add_route(match_any, vec![assistant_text("done", Usage::default())]);
    let tasks = Arc::new(Tasks::new());
    let runner = runner(test_config(&provider, &tasks, Vec::new()));
    let send = tool_named(&runner, "agent_send");

    let unknown = run(send.as_ref(), r#"{"task_id":"missing","message":"hello"}"#).await;
    assert!(unknown.is_error);
    assert_eq!(unknown.content, "unknown task: missing");

    let blank = run(send.as_ref(), r#"{"task_id":"t1","message":"   "}"#).await;
    assert!(blank.is_error);
    assert_eq!(blank.content, "message is required");

    runner
        .start(StartRequest {
            prompt: "finish".into(),
            ..StartRequest::default()
        })
        .await
        .expect("start succeeds");
    wait_final(&tasks, "t1").await;
    let finished = run(send.as_ref(), r#"{"task_id":"t1","message":"too late"}"#).await;
    assert!(finished.is_error);
    assert_eq!(
        finished.content,
        "task t1 is already completed; cannot send message"
    );
}

#[tokio::test]
async fn agent_send_can_address_named_queued_task() {
    let provider = FakeProvider::new();
    let release = CancellationToken::new();
    provider.set_hook(block_until(release.clone()));
    provider.add_route(match_any, vec![assistant_text("done", Usage::default())]);
    let tasks = Arc::new(Tasks::new());
    let mut config = test_config(&provider, &tasks, Vec::new());
    config.max_parallel = 1;
    let runner = runner(config);
    for (name, prompt) in [("first", "a"), ("second", "b")] {
        runner
            .start(StartRequest {
                prompt: prompt.into(),
                name: name.into(),
                ..StartRequest::default()
            })
            .await
            .expect("start succeeds");
    }
    wait_status(&tasks, "t1", TaskStatus::Running).await;
    assert_eq!(
        tasks.get("second").expect("named task").status,
        TaskStatus::Queued
    );

    let sent = run(
        tool_named(&runner, "agent_send").as_ref(),
        r#"{"task_id":"second","message":"queued context"}"#,
    )
    .await;
    assert!(!sent.is_error, "{}", sent.content);
    assert!(sent.content.starts_with("message queued for task t2"));
    release.cancel();
    wait_final(&tasks, "t1").await;
    wait_final(&tasks, "t2").await;

    let queued_request = provider
        .requests()
        .into_iter()
        .find(|request| crate::subagent::testsupport::last_user_text(request) == "b")
        .expect("queued task eventually ran");
    assert!(queued_request.messages.iter().any(|message| {
        message.role == otto_core::model::Role::Context
            && message.context_type == "parent_message"
            && message.text().contains("queued context")
    }));
}

#[tokio::test]
async fn tool_definitions_include_agent_send_and_agent_cancel() {
    let names: Vec<String> = tool_definitions()
        .into_iter()
        .map(|definition| definition.name)
        .collect();
    assert_eq!(
        names,
        [
            "agent",
            "agent_status",
            "agent_yield",
            "agent_send",
            "agent_cancel"
        ]
    );
}

#[tokio::test]
async fn agent_cancel_stops_a_running_task_and_reports_it() {
    let provider = FakeProvider::new();
    provider.set_hook(block_until(CancellationToken::new()));
    let tasks = Arc::new(Tasks::new());
    let runner = runner(test_config(&provider, &tasks, Vec::new()));
    runner
        .start(StartRequest {
            prompt: "go".into(),
            name: "stuck".into(),
            ..StartRequest::default()
        })
        .await
        .expect("start succeeds");
    wait_status(&tasks, "t1", TaskStatus::Running).await;

    let result = run(
        tool_named(&runner, "agent_cancel").as_ref(),
        r#"{"task_id":"t1"}"#,
    )
    .await;
    assert!(!result.is_error, "{}", result.content);
    assert!(result.content.contains("cancel requested for task t1"));

    // A name resolves to the same task.
    let final_task = wait_final(&tasks, "t1").await;
    assert_eq!(final_task.status, TaskStatus::Canceled);
    let named = run(
        tool_named(&runner, "agent_cancel").as_ref(),
        r#"{"task_id":"stuck"}"#,
    )
    .await;
    assert!(named.is_error, "{}", named.content);
    assert_eq!(
        named.content,
        "task stuck is already completed; nothing to cancel"
    );
}

#[tokio::test]
async fn agent_cancel_stops_a_queued_task_before_it_starts() {
    let provider = FakeProvider::new();
    provider.set_hook(block_until(CancellationToken::new()));
    let tasks = Arc::new(Tasks::new());
    let mut config = test_config(&provider, &tasks, Vec::new());
    config.max_parallel = 1;
    let runner = runner(config);
    for prompt in ["first", "second"] {
        runner
            .start(StartRequest {
                prompt: prompt.into(),
                ..StartRequest::default()
            })
            .await
            .expect("start succeeds");
    }
    wait_status(&tasks, "t1", TaskStatus::Running).await;
    assert_eq!(
        tasks.get("t2").expect("t2 exists").status,
        TaskStatus::Queued
    );

    let result = run(
        tool_named(&runner, "agent_cancel").as_ref(),
        r#"{"task_id":"t2"}"#,
    )
    .await;
    assert!(!result.is_error, "{}", result.content);
    assert_eq!(wait_final(&tasks, "t2").await.status, TaskStatus::Canceled);
    // The running task is untouched by cancelling its neighbour.
    assert_eq!(
        tasks.get("t1").expect("t1 exists").status,
        TaskStatus::Running
    );
    tasks.cancel("t1").expect("cancel succeeds");
    wait_final(&tasks, "t1").await;
}

#[tokio::test]
async fn agent_cancel_rejects_an_unknown_task_and_a_missing_id() {
    let provider = FakeProvider::new();
    let tasks = Arc::new(Tasks::new());
    let runner = runner(test_config(&provider, &tasks, Vec::new()));
    let tool = tool_named(&runner, "agent_cancel");

    let unknown = run(tool.as_ref(), r#"{"task_id":"t9"}"#).await;
    assert!(unknown.is_error);
    assert_eq!(unknown.content, "unknown task: t9");

    for body in [
        r#"{}"#,
        r#"{"task_id":"   "}"#,
        r#"{"task_id":"t1","extra":1}"#,
    ] {
        assert!(
            run(tool.as_ref(), body).await.is_error,
            "body {body} must be rejected"
        );
    }
}

#[tokio::test]
async fn agent_cancel_errors_for_a_finished_task() {
    let provider = FakeProvider::new();
    provider.add_route(match_any, vec![assistant_text("done", Usage::default())]);
    let tasks = Arc::new(Tasks::new());
    let runner = runner(test_config(&provider, &tasks, Vec::new()));
    runner
        .start(StartRequest {
            prompt: "go".into(),
            ..StartRequest::default()
        })
        .await
        .expect("start succeeds");
    wait_final(&tasks, "t1").await;

    let result = run(
        tool_named(&runner, "agent_cancel").as_ref(),
        r#"{"task_id":"t1"}"#,
    )
    .await;
    assert!(result.is_error, "{}", result.content);
    assert_eq!(
        result.content,
        "task t1 is already completed; nothing to cancel"
    );
}

#[tokio::test]
async fn agent_status_by_name() {
    let provider = FakeProvider::new();
    provider.add_route(match_any, vec![assistant_text("done", Usage::default())]);
    let tasks = Arc::new(Tasks::new());
    let runner = runner(test_config(&provider, &tasks, Vec::new()));
    runner
        .start(StartRequest {
            prompt: "go".into(),
            name: "lint-check".into(),
            ..StartRequest::default()
        })
        .await
        .expect("start succeeds");
    wait_final(&tasks, "t1").await;

    let result = run(
        tool_named(&runner, "agent_status").as_ref(),
        r#"{"task_id":"lint-check"}"#,
    )
    .await;
    assert!(!result.is_error, "{}", result.content);
    assert!(result.content.contains("t1"), "{}", result.content);
    assert!(result.content.contains("succeeded"), "{}", result.content);
}

#[tokio::test]
async fn agent_tool_model_selection_with_definition() {
    for (body, want) in [
        (
            r#"{"prompt":"go","agent":"reviewer","model":"call-model"}"#,
            "call-model",
        ),
        (r#"{"prompt":"go","agent":"reviewer"}"#, "def-model"),
    ] {
        let provider = FakeProvider::new();
        provider.add_route(match_any, vec![assistant_text("done", Usage::default())]);
        let tasks = Arc::new(Tasks::new());
        let mut config = test_config(&provider, &tasks, Vec::new());
        config.template.model = "gpt-parent".into();
        config.catalog = Catalog::from_definitions(vec![Definition {
            name: "reviewer".into(),
            model: "def-model".into(),
            ..Definition::default()
        }]);
        let runner = runner(config);

        let result = run(tool_named(&runner, "agent").as_ref(), body).await;
        assert!(!result.is_error, "{}", result.content);
        assert_eq!(wait_final(&tasks, "t1").await.model, want, "body {body}");
    }
}

/// The `agent` tool reports a queued task when the concurrency limit is
/// already reached, rather than the started text.
#[tokio::test]
async fn agent_tool_reports_queued_when_at_the_limit() {
    let provider = FakeProvider::new();
    let release = CancellationToken::new();
    provider.set_hook(block_until(release.clone()));
    provider.add_route(match_any, vec![assistant_text("done", Usage::default())]);
    let tasks = Arc::new(Tasks::new());
    let mut config = test_config(&provider, &tasks, vec![stub("read")]);
    config.max_parallel = 1;
    let runner = runner(config);
    let agent_tool = tool_named(&runner, "agent");

    run(agent_tool.as_ref(), r#"{"prompt":"first"}"#).await;
    wait_status(&tasks, "t1", TaskStatus::Running).await;
    let second = run(agent_tool.as_ref(), r#"{"prompt":"second"}"#).await;
    assert_eq!(
        second.content,
        "task t2 (default) queued (1 running, limit 1)"
    );

    release.cancel();
    for id in ["t1", "t2"] {
        wait_final(&tasks, id).await;
    }
}
