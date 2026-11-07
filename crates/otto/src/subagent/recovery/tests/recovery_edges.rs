//! Coverage errors, durable delivery evidence and unsafe-resume rejection.

use super::*;
use std::io::Write;

#[tokio::test]
async fn delivered_completion_is_not_requeued_on_activation() {
    let root = tempfile::tempdir().unwrap();
    let parent = Arc::new(parent(root.path()));
    let path = std::path::PathBuf::from(parent.path());
    let completed = child(&path, "t1");
    completed
        .append_custom_entry("otto.task_result", r#"{"status":"succeeded","error":""}"#)
        .unwrap();
    drop(completed);
    let id = "child-result:t1:attempt-t1";
    parent
        .append_input_if_absent(
            id,
            &Message {
                role: Role::User,
                blocks: vec![Block::text("already delivered completion")],
                created_at: chrono::Utc::now(),
                ..Message::default()
            },
        )
        .unwrap();
    let tasks = Tasks::new().with_admission_transcript(parent);
    let inbox = Inbox::default();
    activate(&path, &tasks, &inbox, &mut Vec::new());
    assert_eq!(tasks.get("t1").unwrap().status, TaskStatus::Succeeded);
    assert!(
        inbox.is_empty(),
        "durable delivered input evidence must suppress another wake"
    );
}

#[test]
fn stale_attempt_does_not_claim_a_newer_interrupted_attempt() {
    let tasks = Tasks::new();
    tasks
        .import_recovered(Task {
            id: "t1".into(),
            attempt_id: "newer".into(),
            status: TaskStatus::Interrupted,
            session_path: "original".into(),
            ..Task::default()
        })
        .unwrap();
    assert!(
        tasks
            .begin_resume(
                "t1",
                "stale",
                Task::default(),
                CancellationToken::new(),
                None
            )
            .is_err()
    );
    assert_eq!(tasks.get("t1").unwrap().status, TaskStatus::Interrupted);
    assert!(
        tasks
            .begin_resume(
                "t1",
                "newer",
                Task::default(),
                CancellationToken::new(),
                None
            )
            .is_ok()
    );
}

#[test]
fn torn_final_record_recovers_but_interior_corruption_does_not() {
    let root = tempfile::tempdir().unwrap();
    let parent = parent(root.path());
    let path = std::path::PathBuf::from(parent.path());
    let store = child(&path, "t1");
    let child_path = store.path();
    drop(store);
    let original = std::fs::read(&child_path).unwrap();
    std::fs::OpenOptions::new()
        .append(true)
        .open(&child_path)
        .unwrap()
        .write_all(b"{\"type\":")
        .unwrap();
    let inventory = interrupted::inventory(&path);
    assert!(inventory.complete, "{:?}", inventory.diagnostics);
    assert_eq!(inventory.records.len(), 1);
    let tasks = Tasks::new();
    activate(&path, &tasks, &Inbox::default(), &mut Vec::new());
    assert_eq!(tasks.get("t1").unwrap().status, TaskStatus::Interrupted);
    assert_eq!(
        interrupted::inventory(&path).records[0]
            .final_status
            .as_deref(),
        Some("interrupted")
    );
    let mut corrupt = original;
    corrupt.extend_from_slice(b"not JSON\n");
    std::fs::write(&child_path, corrupt).unwrap();
    let inventory = interrupted::inventory(&path);
    assert!(!inventory.complete && inventory.records.is_empty());
}

#[test]
fn malformed_terminal_fact_is_not_reconciled_as_a_fresh_interruption() {
    let root = tempfile::tempdir().unwrap();
    let parent = parent(root.path());
    let path = std::path::PathBuf::from(parent.path());
    let store = child(&path, "t1");
    store
        .append_custom_entry("otto.task_result", r#"{"status":"invented-status"}"#)
        .unwrap();
    let child_path = store.path();
    drop(store);
    let before = std::fs::read(&child_path).unwrap();
    let tasks = Tasks::new();
    activate(&path, &tasks, &Inbox::default(), &mut Vec::new());
    assert!(tasks.get("t1").is_none());
    assert!(!tasks.recovery_diagnostics().is_empty());
    assert_eq!(before, std::fs::read(child_path).unwrap());
}

#[test]
fn owned_child_repair_failure_stays_visible_and_unresumable() {
    let root = tempfile::tempdir().unwrap();
    let parent = parent(root.path());
    let path = std::path::PathBuf::from(parent.path());
    let owned = child(&path, "t1");
    let tasks = Tasks::new();
    let inbox = Inbox::default();
    activate(&path, &tasks, &inbox, &mut Vec::new());
    assert_eq!(tasks.get("t1").unwrap().status, TaskStatus::Unavailable);
    assert!(
        tasks
            .recovery_diagnostics()
            .iter()
            .any(|diagnostic| diagnostic.contains("repair failed"))
    );
    assert!(
        inbox.is_empty(),
        "failed repair must not promise resumability"
    );
    assert!(
        interrupted::inventory(&path).records[0]
            .final_status
            .is_none()
    );
    drop(owned);
}

#[test]
fn oversized_child_and_directory_entry_limit_are_explicit() {
    let root = tempfile::tempdir().unwrap();
    let parent = parent(root.path());
    let path = std::path::PathBuf::from(parent.path());
    let directory = path.with_extension("");
    std::fs::create_dir(&directory).unwrap();
    let oversized = directory.join("huge.jsonl");
    std::fs::File::create(&oversized)
        .unwrap()
        .set_len(16 * 1024 * 1024 + 1)
        .unwrap();
    let inventory = interrupted::inventory(&path);
    assert!(!inventory.complete && inventory.records.is_empty());
    assert!(inventory.diagnostics[0].contains("bounded"));
    for index in 0..4096 {
        std::fs::File::create(directory.join(format!("entry-{index}"))).unwrap();
    }
    let inventory = interrupted::inventory(&path);
    assert!(!inventory.complete && inventory.records.is_empty());
    assert!(inventory.diagnostics[0].contains("entry limit"));
}

#[test]
fn notification_persistence_failure_is_diagnostic_and_regenerated() {
    let root = tempfile::tempdir().unwrap();
    let parent = parent(root.path());
    let path = std::path::PathBuf::from(parent.path());
    drop(child(&path, "t1"));
    let inbox = Inbox::default();
    inbox.set_persist(Box::new(|_| Err("controlled unavailable sidecar".into())));
    let tasks = Tasks::new();
    activate(&path, &tasks, &inbox, &mut Vec::new());
    assert!(
        tasks
            .recovery_diagnostics()
            .iter()
            .any(|diagnostic| diagnostic.contains("notification pending"))
    );
    let regenerated = Inbox::default();
    activate(&path, &Tasks::new(), &regenerated, &mut Vec::new());
    assert_eq!(regenerated.snapshot()[0].id, inbox.snapshot()[0].id);
}
