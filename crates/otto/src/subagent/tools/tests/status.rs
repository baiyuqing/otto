//! The agent_status tool: listings and per-task detail.

use super::*;

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
            crate::subagent::tasks::Task {
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
