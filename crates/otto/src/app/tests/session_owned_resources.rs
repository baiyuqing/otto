//! Sub-agent registry, inbox, admission cap and MCP servers survive a runner swap.

use super::*;
use crate::fixtures::subagent_doubles::{FakeProvider, Hook, assistant_text, match_any};
use crate::runtime::builder::{TEST_PROVIDERS, TEST_SCRIPTED_SUBAGENT_HOMES};
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

/// A controller whose parent and children use one scripted provider. Every
/// provider call sends `()` on `entered` and then waits for one
/// `release.notify_one()`.
struct BlockingChildren {
    controller: Controller,
    entered: tokio::sync::mpsc::UnboundedReceiver<()>,
    release: Arc<tokio::sync::Notify>,
    _provider_guard: ProviderGuard,
}

async fn blocking_children(
    workspace: &std::path::Path,
    sessions: &std::path::Path,
    max_parallel: Option<i64>,
) -> BlockingChildren {
    let provider = FakeProvider::new();
    provider.add_route(
        match_any,
        vec![assistant_text("child done", Usage::default())],
    );
    let (entered_sender, entered) = tokio::sync::mpsc::unbounded_channel();
    let release = Arc::new(tokio::sync::Notify::new());
    let hook: Hook = {
        let release = Arc::clone(&release);
        Arc::new(move |_cancel, _request| {
            let (entered_sender, release) = (entered_sender.clone(), Arc::clone(&release));
            Box::pin(async move {
                let _ = entered_sender.send(());
                release.notified().await;
            })
        })
    };
    provider.set_hook(hook);
    let home = workspace.to_string_lossy().into_owned();
    TEST_PROVIDERS
        .lock()
        .unwrap()
        .push((home.clone(), provider.clone()));
    TEST_SCRIPTED_SUBAGENT_HOMES
        .lock()
        .unwrap()
        .push(home.clone());
    let provider_guard = ProviderGuard(home);
    let mut builder = builder(workspace, sessions);
    builder.shared_mut().config.agents.max_parallel = max_parallel;
    let runtime = initial_runtime(&builder);
    let session = builder.create_session(&runtime).expect("session");
    let runner = builder
        .build_runner(&session, &runtime)
        .await
        .expect("runner");
    let info = builder.runtime_info(&runtime);
    BlockingChildren {
        controller: Controller::new(builder, true, session, runner, info),
        entered,
        release,
        _provider_guard: provider_guard,
    }
}

/// Starts a child through the current runner's sub-agent executor.
async fn start_child(controller: &Controller, prompt: &str) -> String {
    controller
        .runner()
        .expect("runner")
        .subagents()
        .expect("scripted parent has a sub-agent runner")
        .start(StartRequest {
            prompt: prompt.into(),
            ..StartRequest::default()
        })
        .await
        .expect("start child")
        .id
}

/// A model switch builds a new runner on the same session. A child blocked
/// inside its provider call stays `Running`, the registry is the same `Arc`,
/// and once the call returns the child's completion notification reaches the
/// new runner's mailbox and the inbox sidecar.
#[tokio::test]
async fn model_switch_keeps_running_child_and_delivers_its_completion() {
    let workspace = tempfile::tempdir().expect("workspace");
    let sessions = tempfile::tempdir().expect("sessions");
    let mut fixture = blocking_children(workspace.path(), sessions.path(), None).await;
    let controller = &fixture.controller;
    // The transcript file, and so the sidecar path, exists after the first append.
    otto_core::session::Session::append_custom(&controller.current_session(), "test", "{}")
        .await
        .expect("append");
    let session_path = std::path::PathBuf::from(controller.info().session_path);
    assert!(session_path.exists());
    let tasks_before = controller.subagent_tasks().expect("registry");
    let child = start_child(controller, "work").await;
    tokio::time::timeout(Duration::from_secs(5), fixture.entered.recv())
        .await
        .expect("child reaches its provider call");

    controller
        .switch_model_result("beta/gpt-beta")
        .await
        .expect("switch");

    let tasks_after = controller.subagent_tasks().expect("registry");
    assert!(Arc::ptr_eq(&tasks_before, &tasks_after));
    let running = tasks_after.get(&child).expect("child listed");
    assert_eq!(running.status, TaskStatus::Running);

    fixture.release.notify_one();
    let finished = tokio::time::timeout(
        Duration::from_secs(5),
        tasks_after.wait(&child, &CancellationToken::new()),
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
            .any(|note| note.task_id == child && note.kind == Some(NotificationKind::TaskFinished)),
        "mailbox: {queued:?}"
    );
    let sidecar = session_path.with_extension("inbox.json");
    let persisted = std::fs::read_to_string(&sidecar).expect("inbox sidecar");
    assert!(persisted.contains(&child), "sidecar: {persisted}");
}

/// `[agents].max_parallel` caps the session's running children across a
/// runner replacement: with a cap of 1 and a child of the replaced runner
/// still running, a child started by the new runner waits in `Queued` and
/// enters its provider call only after the first child finishes.
#[tokio::test]
async fn children_of_the_replaced_and_the_new_runner_share_the_parallel_cap() {
    let workspace = tempfile::tempdir().expect("workspace");
    let sessions = tempfile::tempdir().expect("sessions");
    let mut fixture = blocking_children(workspace.path(), sessions.path(), Some(1)).await;
    let controller = &fixture.controller;
    let first = start_child(controller, "first").await;
    tokio::time::timeout(Duration::from_secs(5), fixture.entered.recv())
        .await
        .expect("first child reaches its provider call");

    controller
        .switch_model_result("beta/gpt-beta")
        .await
        .expect("switch");
    let second = start_child(controller, "second").await;

    // With one permit per runner the second child would enter its provider
    // call at once; with the session's single permit it cannot.
    let early_entry =
        tokio::time::timeout(Duration::from_millis(300), fixture.entered.recv()).await;
    assert!(early_entry.is_err(), "second child ran past the cap of 1");
    let tasks = controller.subagent_tasks().expect("registry");
    assert_eq!(
        tasks.get(&second).expect("second listed").status,
        TaskStatus::Queued
    );

    fixture.release.notify_one();
    tokio::time::timeout(Duration::from_secs(5), fixture.entered.recv())
        .await
        .expect("second child starts after the first finishes");
    assert!(tasks.get(&first).expect("first listed").is_final());
    fixture.release.notify_one();
    let finished = tokio::time::timeout(
        Duration::from_secs(5),
        tasks.wait(&second, &CancellationToken::new()),
    )
    .await
    .expect("second child finishes")
    .expect("second child exists");
    assert_eq!(finished.status, TaskStatus::Succeeded);
}
