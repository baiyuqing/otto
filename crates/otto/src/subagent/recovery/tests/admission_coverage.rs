//! Incomplete discovery cannot authorize new logical ids.
use super::*;

#[test]
fn incomplete_scan_blocks_new_ids_but_known_interrupted_resume_remains_atomic() {
    let root = tempfile::tempdir().unwrap();
    let parent = parent(root.path());
    let path = std::path::PathBuf::from(parent.path());
    let store = child(&path, "t8");
    store
        .append_custom_entry("otto.task_result", r#"{"status":"interrupted","error":""}"#)
        .unwrap();
    drop(store);
    std::fs::write(path.with_extension("").join("t99-broken.jsonl"), "broken").unwrap();
    let tasks = Tasks::new();
    activate(&path, &tasks, &Inbox::default(), &mut Vec::new());
    assert!(
        tasks
            .add(Task::default(), None, None)
            .unwrap_err()
            .to_string()
            .contains("incomplete")
    );
    assert!(
        tasks
            .recovery_diagnostics()
            .iter()
            .any(|message| message.contains("upper bound unknown"))
    );
    let cancellation = CancellationToken::new();
    assert!(
        tasks
            .begin_resume("t8", "attempt-t8", Task::default(), cancellation, None)
            .is_ok()
    );
    assert_eq!(tasks.get("t8").unwrap().status, TaskStatus::Queued);
}

#[tokio::test]
async fn unavailable_wait_observes_diagnostic_without_success_or_execution() {
    let tasks = Tasks::new();
    tasks
        .import_recovered(Task {
            id: "t1".into(),
            status: TaskStatus::Unavailable,
            error: "no usable child spec".into(),
            ..Task::default()
        })
        .unwrap();
    let cancellation = CancellationToken::new();
    let observed = tasks.wait("t1", &cancellation).await.unwrap();
    assert_eq!(observed.status, TaskStatus::Unavailable);
    assert!(!observed.error.is_empty());
    assert!(
        tasks
            .begin_resume("t1", "", Task::default(), cancellation, None)
            .is_err()
    );
}
