//! Child prompts, concurrency, deadlines, cancellation and provider failures.

use super::*;

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
    let origin = url::Url::parse(&server.url)
        .unwrap()
        .origin()
        .ascii_serialization();
    let endpoint = otto_core::config::endpoint::TrustedEndpoint::authorize(
        &server.url,
        "chatgpt",
        std::slice::from_ref(&origin),
        std::slice::from_ref(&origin),
    )
    .expect("test-owned HTTP endpoint");
    config.provider = Arc::new(crate::provider::chatgpt::Client::new_trusted(
        &endpoint,
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
    config.child_session = Some(Arc::new(move |task_id: &str, _session_id: &str| {
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
