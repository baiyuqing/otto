//! Offline recovery acceptance: durable state, never process replay.

mod crash_process;
mod recovery_edges;

use super::*;
use crate::session::Store;
use otto_core::agent::inbox::Inbox;
use otto_core::model::{Block, Message, Role};
use otto_core::session::Header;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

fn parent(root: &Path) -> Store {
    Store::create(
        root,
        Header {
            id: "parent".into(),
            workspace: root.to_string_lossy().into(),
            created_at: chrono::Utc::now(),
            provider: "openai-compatible".into(),
            model: "test".into(),
            ..Header::default()
        },
    )
    .unwrap()
}

fn child(parent: &Path, id: &str) -> Store {
    let child_id = format!("child{id}");
    let store = Store::create_child_lazy(
        parent,
        &format!("{id}-{child_id}"),
        Header {
            id: child_id,
            workspace: parent.parent().unwrap().to_string_lossy().into(),
            created_at: chrono::Utc::now(),
            provider: "openai-compatible".into(),
            model: "test".into(),
            ..Header::default()
        },
    )
    .unwrap();
    store
        .append_custom_entry(
            "otto.task_spec",
            &serde_json::json!({"id": id,
        "name": "", "agent": "", "description": "recover fixture", "model": "test",
        "context": "fresh", "prompt": "work", "definition": null})
            .to_string(),
        )
        .unwrap();
    store
        .append_custom_entry(
            "otto.task_attempt",
            &serde_json::json!({"version": 1,
        "taskId": id, "attemptId": format!("attempt-{id}"), "lineage": "admitted"})
            .to_string(),
        )
        .unwrap();
    store
}

#[test]
fn recovery_hydrates_three_unfinished_children_and_converges_without_execution() {
    let root = tempfile::tempdir().unwrap();
    let parent = parent(root.path());
    let parent_path = std::path::PathBuf::from(parent.path());
    for id in ["t1", "t2", "t3"] {
        drop(child(&parent_path, id));
    }
    let inbox = Inbox::default();
    let tasks = Tasks::new();
    activate(&parent_path, &tasks, &inbox, &mut Vec::new());
    assert_eq!(
        tasks.list().len(),
        3,
        "activation must hydrate, not just scan"
    );
    assert!(
        tasks
            .list()
            .iter()
            .all(|task| task.status == TaskStatus::Interrupted)
    );
    assert_eq!(inbox.len(), 3);
    let before: Vec<_> = interrupted::inventory(&parent_path)
        .records
        .iter()
        .map(|record| std::fs::read(&record.path).unwrap())
        .collect();
    let reopened = Tasks::new();
    activate(&parent_path, &reopened, &inbox, &mut Vec::new());
    assert_eq!(reopened.list(), tasks.list());
    assert_eq!(inbox.len(), 3, "stable ids deduplicate repeated recovery");
    let after: Vec<_> = interrupted::inventory(&parent_path)
        .records
        .iter()
        .map(|record| std::fs::read(&record.path).unwrap())
        .collect();
    assert_eq!(
        before, after,
        "repeat recovery must not append another interruption"
    );
    assert_eq!(tasks.add(Task::default(), None, None).unwrap().id, "t4");
    assert!(!root.path().join("external-marker").exists());
}

#[test]
fn completion_survives_memory_receipt_but_new_attempt_invalidates_it() {
    let root = tempfile::tempdir().unwrap();
    let parent = parent(root.path());
    let parent_path = std::path::PathBuf::from(parent.path());
    let child = child(&parent_path, "t1");
    child
        .append_custom_entry("otto.task_result", r#"{"status":"succeeded","error":""}"#)
        .unwrap();
    child
        .append_custom_entry("otto.local_command", r#"{"command":"memory"}"#)
        .unwrap();
    let child_path = child.path();
    drop(child);
    let tasks = Tasks::new();
    let inbox = Inbox::default();
    activate(&parent_path, &tasks, &inbox, &mut Vec::new());
    assert_eq!(tasks.get("t1").unwrap().status, TaskStatus::Succeeded);
    assert!(inbox.snapshot()[0].id.ends_with("attempt-t1"));
    let (store, _) = Store::open(child_path).unwrap();
    store
        .append_custom_entry(
            "otto.task_attempt",
            r#"{"version":1,"taskId":"t1","attemptId":"second","lineage":"resumed"}"#,
        )
        .unwrap();
    drop(store);
    let tasks = Tasks::new();
    activate(&parent_path, &tasks, &inbox, &mut Vec::new());
    assert_eq!(tasks.get("t1").unwrap().status, TaskStatus::Interrupted);
    assert_eq!(tasks.get("t1").unwrap().attempt_id, "second");
    assert_eq!(inbox.len(), 2);
}

#[test]
fn scan_errors_are_not_reported_as_empty_and_never_follow_symlinks() {
    let root = tempfile::tempdir().unwrap();
    let parent = parent(root.path());
    let path = std::path::PathBuf::from(parent.path());
    let missing = interrupted::inventory(&path);
    assert!(missing.complete && missing.directory_missing && missing.records.is_empty());
    std::fs::create_dir(path.with_extension("")).unwrap();
    assert!(interrupted::inventory(&path).complete);
    std::fs::write(path.with_extension("").join("bad.jsonl"), "not JSON").unwrap();
    std::os::unix::fs::symlink(&path, path.with_extension("").join("link.jsonl")).unwrap();
    let inventory = interrupted::inventory(&path);
    assert!(!inventory.complete);
    assert_eq!(inventory.diagnostics.len(), 2);
    let tasks = Tasks::new();
    let mut warnings = Vec::new();
    activate(&path, &tasks, &Inbox::default(), &mut warnings);
    assert!(tasks.list().is_empty());
    assert!(!tasks.recovery_diagnostics().is_empty());
    assert!(!warnings.is_empty());
}

#[test]
fn wrong_parent_and_duplicate_child_identity_are_unavailable() {
    let root = tempfile::tempdir().unwrap();
    let parent = parent(root.path());
    let path = std::path::PathBuf::from(parent.path());
    let child = child(&path, "t1");
    let source = std::path::PathBuf::from(child.path());
    drop(child);
    let text = std::fs::read_to_string(&source).unwrap();
    let wrong = text.replace(
        &path.to_string_lossy().to_string(),
        "/not/the/selected/parent.jsonl",
    );
    std::fs::write(&source, wrong).unwrap();
    let inventory = interrupted::inventory(&path);
    assert!(!inventory.complete && inventory.records.is_empty());
    std::fs::write(&source, text).unwrap();
    // Same child id under another logical task is a conflict, not two children.
    let mut duplicate = std::fs::read_to_string(&source).unwrap();
    duplicate = duplicate.replace("\"id\":\"t1\"", "\"id\":\"t2\"");
    std::fs::write(path.with_extension("").join("t2-childt1.jsonl"), duplicate).unwrap();
    let inventory = interrupted::inventory(&path);
    assert!(!inventory.complete && inventory.records.is_empty());
}

#[tokio::test]
async fn admission_before_spec_remains_visible_but_not_executable() {
    let root = tempfile::tempdir().unwrap();
    let parent = Arc::new(parent(root.path()));
    let path = std::path::PathBuf::from(parent.path());
    let tasks = Tasks::new().with_admission_transcript(parent);
    let task = tasks.add(Task::default(), None, None).unwrap();
    tasks.persist_admission(&task).await.unwrap();
    let recovered = Tasks::new();
    activate(&path, &recovered, &Inbox::default(), &mut Vec::new());
    let task = recovered.get("t1").expect("admission must not disappear");
    assert_eq!(task.status, TaskStatus::Interrupted);
    assert!(task.session_path.is_empty());
    assert!(task.error.contains("no usable child spec"));
    assert_eq!(recovered.add(Task::default(), None, None).unwrap().id, "t2");
}

#[test]
fn imported_and_live_interruption_resume_admission_is_atomic() {
    for imported in [true, false] {
        let tasks = Arc::new(Tasks::new());
        if imported {
            tasks
                .import_recovered(Task {
                    id: "t1".into(),
                    status: TaskStatus::Interrupted,
                    session_path: "original.jsonl".into(),
                    ..Task::default()
                })
                .unwrap();
        } else {
            tasks
                .add(Task::default(), Some(CancellationToken::new()), None)
                .unwrap();
            tasks.set_session_path("t1", "original.jsonl");
            tasks.finish(
                "t1",
                TaskStatus::Interrupted,
                chrono::Utc::now(),
                "",
                "lost owner",
            );
        }
        let barrier = Arc::new(std::sync::Barrier::new(3));
        let workers: Vec<_> = (0..2)
            .map(|_| {
                let tasks = tasks.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    tasks.begin_resume("t1", "", Task::default(), CancellationToken::new(), None)
                })
            })
            .collect();
        barrier.wait();
        let successes = workers
            .into_iter()
            .map(|worker| worker.join().unwrap().is_ok())
            .filter(|ok| *ok)
            .count();
        assert_eq!(successes, 1);
        let task = tasks.get("t1").unwrap();
        assert_eq!(task.status, TaskStatus::Queued);
        assert_eq!(task.session_path, "original.jsonl");
        assert!(
            tasks
                .add_with_id("t1".into(), Task::default(), None, None)
                .is_err()
        );
    }
}

#[tokio::test]
async fn tool_effect_is_not_replayed_during_reconciliation() {
    let root = tempfile::tempdir().unwrap();
    let parent = parent(root.path());
    let path = std::path::PathBuf::from(parent.path());
    let store = child(&path, "t1");
    let call = Block {
        block_type: otto_core::model::BlockType::ToolCall,
        tool_call_id: "effect".into(),
        tool_name: "bash".into(),
        arguments: Some(
            serde_json::value::RawValue::from_string(r#"{"command":"external-effect"}"#.into())
                .unwrap(),
        ),
        ..Block::default()
    };
    store
        .append_message(&Message {
            role: Role::Assistant,
            created_at: chrono::Utc::now(),
            finish_reason: Some(otto_core::model::FinishReason::ToolCalls),
            blocks: vec![call],
            ..Message::default()
        })
        .unwrap();
    let marker = root.path().join("external-marker");
    std::fs::write(&marker, "once\n").unwrap();
    drop(store);
    for _ in 0..2 {
        activate(&path, &Tasks::new(), &Inbox::default(), &mut Vec::new());
    }
    assert_eq!(std::fs::read_to_string(marker).unwrap(), "once\n");
    let records = interrupted::inventory(&path).records;
    assert_eq!(records[0].final_status.as_deref(), Some("interrupted"));
}
