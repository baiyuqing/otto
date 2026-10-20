//! Transcript entries and task records a child writes.

use super::*;

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
