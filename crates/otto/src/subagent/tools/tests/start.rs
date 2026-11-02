//! The agent tool starting children: arguments, models, names and skill checks.

use super::*;

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

#[tokio::test]
async fn external_agent_is_routed_and_rejects_unsupported_context() {
    let provider = FakeProvider::new();
    let tasks = Arc::new(Tasks::new());
    let runner = runner(test_config(&provider, &tasks, Vec::new()));
    let result = run(
        tool_named(&runner, "agent").as_ref(),
        r#"{"prompt":"go","agent":"acp:codex","context":"inherit"}"#,
    )
    .await;
    assert!(result.is_error);
    assert!(result.content.contains("fresh context"));
    assert!(tasks.list().is_empty());
}

#[tokio::test]
#[ignore = "requires installed codex-acp and authenticated live Codex"]
async fn live_codex_delegation_returns_task_result() {
    let provider = FakeProvider::new();
    let tasks = Arc::new(Tasks::new());
    let mut config = test_config(&provider, &tasks, Vec::new());
    config.workspace_path = std::env::current_dir().unwrap();
    let runner = runner(config);
    let result = run(
        tool_named(&runner, "agent").as_ref(),
        r#"{"prompt":"Reply exactly ACP connection works. Do not use tools.","agent":"acp:codex"}"#,
    )
    .await;
    assert!(!result.is_error, "{}", result.content);
    let task = tokio::time::timeout(
        std::time::Duration::from_secs(90),
        tasks.wait("t1", &CancellationToken::new()),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(task.status, TaskStatus::Succeeded, "{}", task.error);
    assert!(task.result.contains("ACP connection works"));
}
