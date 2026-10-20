//! The agent tool rejecting invalid resume requests.

use super::*;

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
