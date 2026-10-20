//! Parent handoffs and persisted child reports.

use super::*;

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

    let workspace = std::sync::Arc::new(Workspace::new(dir.path()).unwrap());
    let read = ReadTool::new(std::sync::Arc::clone(&workspace), 16384);
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
