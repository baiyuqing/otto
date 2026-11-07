//! Migration and lease release tests.

use super::*;

/// S7: `Controller::migrate` obtains admission, cancels and records the
/// running and queued sub-agent tasks interrupted, notifies the parent,
/// closes MCP, requests a close, and releases the session's lease when
/// its admission is dropped.
///
/// Drives the sub-agent registry directly with `subagents.start(...).await`
/// rather than a real top-level "agent" tool call from a scripted
/// provider: `Controller::migrate` calls `Runner::migrate`, already
/// covered end to end by
/// `runtime::builder::tests::migrate_cancels_running_and_queued_subagent_tasks_and_notifies_the_parent`,
/// and adds only admission/close/lease-release around it, none of which
/// depends on how the tasks were started.
#[tokio::test]
async fn migrate_releases_the_lease_after_cancelling_subagent_tasks() {
    use crate::fixtures::subagent_doubles::{FakeProvider, assistant_text, match_any, wait_status};
    use crate::subagent::runner::StartRequest;
    use crate::subagent::tasks::{TaskStatus, Tasks};
    use otto_core::model::Usage;
    use otto_core::provider::Provider;

    let workspace = tempfile::tempdir().expect("workspace");
    let sessions = tempfile::tempdir().expect("sessions");
    let mut builder = builder(workspace.path(), sessions.path());
    builder.shared_mut().config.failover = otto_core::config::Failover {
        enabled: true,
        lease_seconds: 12,
    };
    let runtime = initial_runtime(&builder);
    let session = builder.create_session(&runtime).expect("create session");
    session.append(user("go")).await.expect("first append");
    let session_path = session.path();

    let tasks = Arc::new(Tasks::new());
    let child_provider = FakeProvider::new();
    child_provider.set_hook(Arc::new(|cancel, _request| {
        Box::pin(async move { cancel.cancelled().await })
    }));
    child_provider.add_route(match_any, vec![assistant_text("done", Usage::default())]);

    let mut child_config =
        crate::fixtures::subagent_doubles::test_config(&child_provider, &tasks, Vec::new());
    child_config.max_parallel = 1;
    let parent_path = PathBuf::from(&session_path);
    child_config.child_session = Some(Arc::new(move |task_id: &str, _session_id: &str| {
        Ok(Some(child_store(&parent_path, task_id)))
    }));
    let (subagents, _) = crate::subagent::runner::Runner::new(child_config).expect("valid config");
    let subagents = Arc::new(subagents);

    subagents
        .start(StartRequest {
            prompt: "first".into(),
            ..StartRequest::default()
        })
        .await
        .expect("start succeeds");
    wait_status(&tasks, "t1", TaskStatus::Running).await;
    subagents
        .start(StartRequest {
            prompt: "second".into(),
            ..StartRequest::default()
        })
        .await
        .expect("start succeeds");
    assert_eq!(
        tasks.get("t2").expect("t2 exists").status,
        TaskStatus::Queued,
        "t2 must stay queued behind t1 with max_parallel 1"
    );

    let mut runner = Runner::scripted(
        session.clone(),
        FakeProvider::new() as Arc<dyn Provider + Send + Sync>,
        Arc::clone(&tasks),
    );
    runner.subagents = Some(subagents);
    let info = builder.runtime_info(&runtime);
    let controller = Controller::new(builder, true, session.clone(), runner, info);

    let warnings = controller.migrate().await;
    assert!(warnings.is_empty(), "{warnings:?}");

    for task_id in ["t1", "t2"] {
        assert_eq!(
            tasks.get(task_id).expect("task exists").status,
            TaskStatus::Canceled
        );
    }
    let notifications = tasks.notifications().snapshot();
    assert_eq!(notifications.len(), 1, "{notifications:?}");
    assert!(notifications[0].text.contains("moved"));

    assert!(
        heartbeat_released(&session_path),
        "migrate must release the lease"
    );
}

/// S7: `Controller::migrate` on an idle lease-managed session with no
/// sub-agent tasks pushes no notification and still releases the lease.
#[tokio::test]
async fn migrate_on_an_idle_lease_managed_session_still_releases_the_lease() {
    use crate::subagent::tasks::Tasks;
    use otto_core::model::{FinishReason, Role};
    use otto_core::provider::Provider;

    let workspace = tempfile::tempdir().expect("workspace");
    let sessions = tempfile::tempdir().expect("sessions");
    let mut builder = builder(workspace.path(), sessions.path());
    builder.shared_mut().config.failover = otto_core::config::Failover {
        enabled: true,
        lease_seconds: 12,
    };
    let runtime = initial_runtime(&builder);
    let session = builder.create_session(&runtime).expect("create session");
    session
        .append(Message {
            role: Role::Assistant,
            blocks: vec![Block::text("done")],
            finish_reason: Some(FinishReason::Stop),
            created_at: chrono::Utc::now(),
            ..Message::default()
        })
        .await
        .expect("append");
    let session_path = session.path();

    let tasks = Arc::new(Tasks::new());
    let runner = Runner::scripted(
        session.clone(),
        crate::fixtures::subagent_doubles::FakeProvider::new() as Arc<dyn Provider + Send + Sync>,
        Arc::clone(&tasks),
    );
    let info = builder.runtime_info(&runtime);
    let controller = Controller::new(builder, true, session.clone(), runner, info);

    let warnings = controller.migrate().await;
    assert!(warnings.is_empty(), "{warnings:?}");
    assert!(tasks.notifications().is_empty());
    assert!(
        heartbeat_released(&session_path),
        "migrate must release an idle session's lease too"
    );
}
