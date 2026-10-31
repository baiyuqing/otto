//! Sub-agent registry, inbox and MCP servers survive a runner swap.

use super::*;
use crate::subagent::tasks::{Task, TaskStatus};

/// A profile switch builds a new runner on the same session. The running
/// child, its registry and the session inbox stay, and the child's
/// completion notification reaches the new runner and the inbox sidecar.
#[tokio::test]
async fn profile_switch_keeps_running_child_and_delivers_its_completion() {
    let workspace = tempfile::tempdir().expect("workspace");
    let sessions = tempfile::tempdir().expect("sessions");
    let controller = controller(workspace.path(), sessions.path()).await;
    // The transcript file, and so the sidecar path, exists after the first append.
    otto_core::session::Session::append_custom(&controller.current_session(), "test", "{}")
        .await
        .expect("append");
    let session_path = std::path::PathBuf::from(controller.info().session_path);
    assert!(session_path.exists());
    let tasks_before = controller.subagent_tasks().expect("registry");
    let cancel = CancellationToken::new();
    let child = tasks_before
        .add(
            Task {
                name: "blocking".to_string(),
                status: TaskStatus::Running,
                ..Task::default()
            },
            Some(cancel.clone()),
            None,
        )
        .expect("add child");

    controller
        .switch_profile_result("beta")
        .await
        .expect("switch");

    let tasks_after = controller.subagent_tasks().expect("registry");
    assert!(Arc::ptr_eq(&tasks_before, &tasks_after));
    assert!(!cancel.is_cancelled());
    let listed = tasks_after.get(&child.id).expect("child listed");
    assert!(!listed.is_final(), "child status {:?}", listed.status);

    tasks_after
        .notifications()
        .push(otto_core::agent::inbox::Notification {
            task_id: child.id.clone(),
            kind: Some(NotificationKind::TaskFinished),
            text: "child finished".to_string(),
            ..Default::default()
        });
    let mailbox = controller.runner().expect("runner").mailbox().clone();
    let queued = mailbox.snapshot();
    assert_eq!(queued.len(), 1);
    assert_eq!(queued[0].task_id, child.id);
    let sidecar = session_path.with_extension("inbox.json");
    let persisted = std::fs::read_to_string(&sidecar).expect("inbox sidecar");
    assert!(persisted.contains(&child.id), "sidecar: {persisted}");
}
