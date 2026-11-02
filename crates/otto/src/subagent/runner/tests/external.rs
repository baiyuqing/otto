use super::*;
use std::os::unix::fs::PermissionsExt;

#[tokio::test]
async fn external_turn_finishes_task_and_records_result() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("fake-acp");
    std::fs::write(&path, r##"#!/bin/sh
read -r line
printf '%s\n' '{"id":1,"result":{"protocolVersion":1}}'
read -r line
printf '%s\n' '{"id":2,"result":{"sessionId":"s1"}}'
read -r line
printf '%s\n' '{"method":"session/update","params":{"sessionId":"s1","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"child result"}}}}'
printf '%s\n' '{"id":3,"result":{"stopReason":"end_turn"}}'
while read -r line; do :; done
"##).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
    let provider = FakeProvider::new();
    let tasks = Arc::new(Tasks::new());
    let mut config = test_config(&provider, &tasks, Vec::new());
    config.workspace_path = dir.path().to_path_buf();
    let (runner, _) = runner(config);
    let cancel = CancellationToken::new();
    let task = tasks
        .add(
            Task {
                agent: "acp:codex".into(),
                ..Task::default()
            },
            Some(cancel.clone()),
            None,
        )
        .unwrap();
    let transcript: Transcript = Arc::new(MemorySession::new());
    let executable: &'static str = Box::leak(path.to_string_lossy().into_owned().into_boxed_str());
    runner
        .run_external(
            task.id.clone(),
            "hello".into(),
            executable,
            cancel,
            transcript,
            Deadline::unlimited(),
        )
        .await;
    let finished = tasks.get(&task.id).unwrap();
    assert_eq!(finished.status, TaskStatus::Succeeded);
    assert_eq!(finished.result, "child result");
}

#[tokio::test]
async fn external_agent_rejects_inherited_context_before_creating_task() {
    let provider = FakeProvider::new();
    let tasks = Arc::new(Tasks::new());
    let (runner, _) = runner(test_config(&provider, &tasks, Vec::new()));
    let error = runner
        .start(StartRequest {
            agent: "acp:codex".into(),
            prompt: "go".into(),
            context: "inherit".into(),
            ..StartRequest::default()
        })
        .await
        .unwrap_err();
    assert!(matches!(error, StartError::ExternalUnsupported));
    assert!(tasks.list().is_empty());
}

#[test]
fn messages_to_external_tasks_are_rejected_before_enqueue() {
    let tasks = Tasks::new();
    let task = tasks
        .add(
            Task {
                agent: "acp:codex".into(),
                ..Task::default()
            },
            None,
            None,
        )
        .unwrap();
    assert!(matches!(
        tasks.send_message(&task.id, "change direction"),
        Err(TaskError::Invalid(_))
    ));
    assert!(tasks.child_inbox(&task.id).unwrap().is_empty());
}

#[tokio::test]
async fn external_agent_rejects_secret_in_task_before_recording() {
    let provider = FakeProvider::new();
    let tasks = Arc::new(Tasks::new());
    let mut config = test_config(&provider, &tasks, Vec::new());
    config.redaction_values = vec!["fixture-secret-value".into()];
    let (runner, _) = runner(config);
    let error = runner
        .start(StartRequest {
            agent: "acp:codex".into(),
            prompt: "fixture-secret-value".into(),
            ..StartRequest::default()
        })
        .await
        .unwrap_err();
    assert!(matches!(error, StartError::ExternalSensitiveInput));
    assert!(tasks.list().is_empty());
}
