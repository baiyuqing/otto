//! Task completion notifications and completion text.

use super::*;

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
                ..Default::default()
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
                ..Default::default()
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
            ..Default::default()
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
