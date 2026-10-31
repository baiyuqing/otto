//! Sub-agent registry, inbox and MCP servers survive a runner swap.

use super::*;
use crate::runtime::builder::{TEST_PROVIDERS, TEST_SCRIPTED_SUBAGENT_HOMES};
use crate::subagent::doubles_tests::{FakeProvider, Hook, assistant_text, match_any};
use crate::subagent::runner::StartRequest;
use crate::subagent::tasks::TaskStatus;

/// Removes the scripted provider registered for a builder home on drop.
struct ProviderGuard(String);
impl Drop for ProviderGuard {
    fn drop(&mut self) {
        TEST_PROVIDERS
            .lock()
            .unwrap()
            .retain(|(home, _)| home != &self.0);
        TEST_SCRIPTED_SUBAGENT_HOMES
            .lock()
            .unwrap()
            .retain(|home| home != &self.0);
    }
}

/// A profile switch builds a new runner on the same session. A child blocked
/// inside its provider call stays `Running`, the registry is the same `Arc`,
/// and once the call returns the child's completion notification reaches the
/// new runner's mailbox and the inbox sidecar.
#[tokio::test]
async fn profile_switch_keeps_running_child_and_delivers_its_completion() {
    let workspace = tempfile::tempdir().expect("workspace");
    let sessions = tempfile::tempdir().expect("sessions");
    let provider = FakeProvider::new();
    provider.add_route(
        match_any,
        vec![assistant_text("child done", Usage::default())],
    );
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let hook: Hook = {
        let (entered, release) = (Arc::clone(&entered), Arc::clone(&release));
        Arc::new(move |_cancel, _request| {
            let (entered, release) = (Arc::clone(&entered), Arc::clone(&release));
            Box::pin(async move {
                entered.notify_one();
                release.notified().await;
            })
        })
    };
    provider.set_hook(hook);
    let home = workspace.path().to_string_lossy().into_owned();
    TEST_PROVIDERS
        .lock()
        .unwrap()
        .push((home.clone(), provider.clone()));
    TEST_SCRIPTED_SUBAGENT_HOMES
        .lock()
        .unwrap()
        .push(home.clone());
    let _provider_guard = ProviderGuard(home);
    let controller = controller(workspace.path(), sessions.path()).await;
    // The transcript file, and so the sidecar path, exists after the first append.
    otto_core::session::Session::append_custom(&controller.current_session(), "test", "{}")
        .await
        .expect("append");
    let session_path = std::path::PathBuf::from(controller.info().session_path);
    assert!(session_path.exists());
    let tasks_before = controller.subagent_tasks().expect("registry");
    let subagents = controller
        .runner()
        .expect("runner")
        .subagents()
        .expect("scripted parent has a sub-agent runner");
    let child = subagents
        .start(StartRequest {
            prompt: "work".into(),
            ..StartRequest::default()
        })
        .await
        .expect("start child");
    tokio::time::timeout(Duration::from_secs(5), entered.notified())
        .await
        .expect("child reaches its provider call");

    controller
        .switch_profile_result("beta")
        .await
        .expect("switch");

    let tasks_after = controller.subagent_tasks().expect("registry");
    assert!(Arc::ptr_eq(&tasks_before, &tasks_after));
    let running = tasks_after.get(&child.id).expect("child listed");
    assert_eq!(running.status, TaskStatus::Running);

    release.notify_one();
    let finished = tokio::time::timeout(
        Duration::from_secs(5),
        tasks_after.wait(&child.id, &CancellationToken::new()),
    )
    .await
    .expect("child finishes")
    .expect("child exists");
    assert_eq!(finished.status, TaskStatus::Succeeded);
    let mailbox = controller.runner().expect("runner").mailbox().clone();
    let queued = mailbox.snapshot();
    assert!(
        queued
            .iter()
            .any(|note| note.task_id == child.id
                && note.kind == Some(NotificationKind::TaskFinished)),
        "mailbox: {queued:?}"
    );
    let sidecar = session_path.with_extension("inbox.json");
    let persisted = std::fs::read_to_string(&sidecar).expect("inbox sidecar");
    assert!(persisted.contains(&child.id), "sidecar: {persisted}");
}
