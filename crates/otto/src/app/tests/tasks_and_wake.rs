//! Task view and wake claim tests.

use super::*;

#[tokio::test]
async fn a_runner_without_a_registry_reports_no_tasks_and_no_wake() {
    let workspace = tempfile::tempdir().expect("workspace");
    let sessions = tempfile::tempdir().expect("sessions");
    let mut builder = builder(workspace.path(), sessions.path());
    builder.shared_mut().config.agents.enabled = Some(false);
    let runtime = initial_runtime(&builder);
    let session = builder.create_session(&runtime).expect("session");
    let runner = builder
        .build_runner(&session, &runtime)
        .await
        .expect("runner");
    let info = builder.runtime_info(&runtime);
    let controller = Controller::new(builder, true, session, runner, info);

    assert!(controller.tasks().is_none());
    assert!(controller.prepare_wake().expect("prepare").is_none());
    controller
        .runner()
        .expect("runner")
        .mailbox()
        .push(otto_core::agent::inbox::Notification {
            kind: Some(NotificationKind::UserMessage),
            text: "change course".to_string(),
            ..Default::default()
        });
    controller
        .prepare_wake()
        .expect("prepare")
        .expect("queued user input claims a wake")
        .cancel();
}

/// The registry is reachable through the view, and an empty inbox still
/// admits no wake turn.
#[tokio::test]
async fn a_runner_with_an_empty_registry_exposes_it_but_admits_no_wake() {
    let workspace = tempfile::tempdir().expect("workspace");
    let sessions = tempfile::tempdir().expect("sessions");
    let controller = controller(workspace.path(), sessions.path()).await;

    let tasks = controller.tasks().expect("registry");
    assert!(tasks.list().is_empty());
    assert_eq!(tasks.pending(), 0);
    assert!(controller.prepare_wake().expect("prepare").is_none());
    controller.begin_operation().expect("admit");
}

/// A pending notification claims a turn, and dropping the claim without
/// running it releases it.
#[tokio::test]
async fn a_pending_notification_claims_a_wake_turn() {
    let workspace = tempfile::tempdir().expect("workspace");
    let sessions = tempfile::tempdir().expect("sessions");
    let controller = controller(workspace.path(), sessions.path()).await;
    controller
        .current_runner()
        .expect("runner")
        .tasks
        .as_ref()
        .expect("registry")
        .notifications()
        .push(otto_core::agent::inbox::Notification {
            text: "[task-notification] task t1 succeeded".to_string(),
            ..otto_core::agent::inbox::Notification::default()
        });

    let wake = controller
        .prepare_wake()
        .expect("prepare")
        .expect("claimed");
    assert_eq!(
        controller.begin_operation().expect_err("busy"),
        PROMPT_ACTIVE
    );
    wake.cancel();
    controller.begin_operation().expect("admit");
}
