//! Resuming interrupted children.

use super::*;

#[tokio::test]
async fn restart_preserves_recorded_model_source_without_rewriting_history() {
    use crate::subagent::tasks::ModelSource;
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
        model_source: crate::subagent::tasks::ModelSource::Unknown,
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
