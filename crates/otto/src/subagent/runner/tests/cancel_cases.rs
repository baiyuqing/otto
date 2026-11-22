//! Dropped caller futures and parent messages queued while a child finishes.

use super::*;

#[tokio::test]
async fn dropping_run_with_definition_control_cancels_the_child() {
    let provider = FakeProvider::new();
    provider.set_hook(Arc::new(|cancel, _request| {
        Box::pin(async move { cancel.cancelled().await })
    }));
    let tasks = Arc::new(Tasks::new());
    let (runner, _) = runner(test_config(&provider, &tasks, Vec::new()));

    let control = Control::new(Deadline::after(Duration::from_secs(30)));
    let dropped = tokio::time::timeout(
        Duration::from_millis(100),
        runner.run_with_definition_control(
            StartRequest {
                prompt: "block".into(),
                agent: "worker".into(),
                ..StartRequest::default()
            },
            Arc::new(MemorySession::new()),
            definition("worker"),
            &control,
            RootBudget::new(
                "test-root",
                Default::default(),
                otto_core::agent::budget::untimed(),
            ),
        ),
    )
    .await;
    assert!(dropped.is_err(), "the child must still be running");

    // Without the drop guard the child stays Running and this wait times out.
    assert_eq!(wait_final(&tasks, "t1").await.status, TaskStatus::Canceled);
}

#[tokio::test]
async fn parent_message_queued_during_the_last_provider_call_is_delivered() {
    let provider = FakeProvider::new();
    let tasks = Arc::new(Tasks::new());
    let first_call_pending = Arc::new(AtomicBool::new(true));
    let (hook_tasks, hook_pending) = (Arc::clone(&tasks), Arc::clone(&first_call_pending));
    provider.set_hook(Arc::new(move |_cancel, _request| {
        let (tasks, pending) = (Arc::clone(&hook_tasks), Arc::clone(&hook_pending));
        Box::pin(async move {
            if pending.swap(false, Ordering::SeqCst) {
                tasks
                    .send_message("t1", "late instruction")
                    .expect("the running child accepts the message");
            }
        })
    }));
    provider.add_route(match_any, vec![assistant_text("done", Usage::default())]);
    let (runner, _) = runner(test_config(&provider, &tasks, Vec::new()));
    runner
        .start(StartRequest {
            prompt: "go".into(),
            ..StartRequest::default()
        })
        .await
        .expect("start succeeds");

    assert_eq!(wait_final(&tasks, "t1").await.status, TaskStatus::Succeeded);
    // Fails without the follow-up run: the provider is called only once.
    let requests = provider.requests();
    assert_eq!(requests.len(), 2, "the queued message needs a second call");
    assert!(
        requests[1]
            .messages
            .iter()
            .any(|message| message.text().contains("[parent-message] late instruction"))
    );
}
