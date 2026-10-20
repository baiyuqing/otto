//! The agent_send and agent_cancel tools.

use super::*;

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
