use super::*;
use otto_core::model::{Block, FinishReason, Message, Role};
use otto_core::session::{Header, Session};

fn read_history(root: &std::path::Path, row: &TaskRow) -> Option<Vec<ObserverEntry>> {
    history::read(root, row, None).map(|history| history.entries)
}

#[tokio::test]
async fn observer_restart_history_is_read_only_bounded_and_rejects_symlink_escape() {
    let directory = tempfile::tempdir().expect("sessions");
    let parent = directory.path().join("parent.jsonl");
    let store = otto_session::Store::create_child_lazy(
        &parent,
        "t1-child",
        Header {
            id: "child".into(),
            workspace: directory.path().to_string_lossy().into_owned(),
            provider: "openai-compatible".into(),
            model: "fixture".into(),
            created_at: chrono::Utc::now(),
            ..Default::default()
        },
    )
    .expect("child store");
    let session = crate::runtime::builder::SharedSession::new(std::sync::Arc::new(store));
    session
        .append(Message {
            role: Role::User,
            created_at: chrono::DateTime::from_timestamp(1_793_750_400, 0).unwrap(),
            blocks: vec![Block::text("private prompt")],
            ..Default::default()
        })
        .await
        .expect("prompt");
    session
        .append(Message {
            role: Role::Assistant,
            created_at: chrono::DateTime::from_timestamp(1_793_750_401, 0).unwrap(),
            finish_reason: Some(FinishReason::Stop),
            blocks: vec![Block::text("retained answer")],
            ..Default::default()
        })
        .await
        .expect("answer");
    let path = session.path();
    session.close().expect("close");
    let mut row = TaskRow {
        task_id: "t1".into(),
        parent_session_path: parent.to_string_lossy().into_owned(),
        session_path: path.clone(),
        ..Default::default()
    };
    let before = std::fs::read(&path).expect("before");
    let entries = read_history(directory.path(), &row).expect("history");
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].text, "retained answer");
    assert_eq!(std::fs::read(&path).expect("after"), before);
    let mut cached = history::read(directory.path(), &row, None).expect("cache");
    cached.entries[0].text = "cache hit sentinel".into();
    assert_eq!(
        history::read(directory.path(), &row, Some(&cached))
            .unwrap()
            .entries[0]
            .text,
        "cache hit sentinel",
        "unchanged terminal transcript must not be decoded repeatedly"
    );
    for status in ["queued", "running"] {
        row.status = status.into();
        assert_eq!(
            history::read(directory.path(), &row, Some(&cached))
                .unwrap()
                .entries[0]
                .text,
            "retained answer",
            "active rows must bypass a former terminal cache entry"
        );
    }
    row.status = "succeeded".into();
    let updated = String::from_utf8(before.clone())
        .unwrap()
        .replace("retained answer", "replaced answer");
    std::fs::write(&path, updated).expect("external transcript update");
    assert_eq!(
        history::read(directory.path(), &row, Some(&cached))
            .unwrap()
            .entries[0]
            .text,
        "replaced answer",
        "same-length external changes must invalidate cached terminal entries"
    );
    let replacement = parent.with_extension("").join("t1-replacement.jsonl");
    std::fs::hard_link(&path, &replacement).expect("same inode under a new path");
    row.session_path = replacement.to_string_lossy().into_owned();
    let mut cached = history::read(
        directory.path(),
        &TaskRow {
            session_path: path.clone(),
            ..row.clone()
        },
        None,
    )
    .unwrap();
    cached.entries[0].text = "cache hit sentinel".into();
    assert_ne!(
        history::read(directory.path(), &row, Some(&cached))
            .unwrap()
            .entries[0]
            .text,
        "cache hit sentinel",
        "path must also participate in cache validation"
    );
    let replacement_cache = history::read(directory.path(), &row, None).unwrap();
    std::fs::remove_file(&replacement).expect("external removal");
    assert!(
        history::read(directory.path(), &row, Some(&replacement_cache)).is_none(),
        "cached terminal output must not hide an unavailable transcript"
    );
    row.session_path = path.clone();
    row.task_id = "../escape".into();
    assert!(read_history(directory.path(), &row).is_none());
    row.task_id = "t1".into();
    let link = parent.with_extension("").join("t1-link.jsonl");
    std::os::unix::fs::symlink(&path, &link).expect("symlink");
    row.session_path = link.to_string_lossy().into_owned();
    assert!(read_history(directory.path(), &row).is_none());

    // A valid-looking path whose ancestor is a link must not be opened.
    let alias = directory.path().join("ancestor-link");
    std::os::unix::fs::symlink(directory.path(), &alias).expect("ancestor symlink");
    row.parent_session_path = alias.join("parent.jsonl").to_string_lossy().into_owned();
    row.session_path = alias
        .join("parent")
        .join(std::path::Path::new(&path).file_name().unwrap())
        .to_string_lossy()
        .into_owned();
    assert!(read_history(directory.path(), &row).is_none());
    assert!(
        read_history(
            &alias,
            &TaskRow {
                parent_session_path: parent.to_string_lossy().into_owned(),
                session_path: path,
                ..row
            }
        )
        .is_none(),
        "recorded absolute paths cannot escape the configured root"
    );
}

#[tokio::test]
async fn observer_live_projection_takes_precedence_over_child_history_and_survives_finish() {
    let workspace = tempfile::tempdir().expect("workspace");
    let sessions = tempfile::tempdir().expect("sessions");
    let controller = crate::fixtures::controller(workspace.path(), sessions.path()).await;
    let registry = controller.subagent_tasks().expect("registry");
    let task = registry
        .add(
            crate::subagent::tasks::Task::default(),
            None,
            Some(std::sync::Arc::new(|| {
                vec![Message {
                    role: Role::Assistant,
                    created_at: chrono::DateTime::from_timestamp(1_793_750_402, 0).unwrap(),
                    blocks: vec![Block::text("history must not duplicate live")],
                    ..Default::default()
                }]
            })),
        )
        .expect("task");
    registry.mark_running(&task.id, chrono::Utc::now());
    registry.observe(
        &task.id,
        &otto_core::agent::Event::TextDelta {
            text: "live partial".into(),
        },
    );
    let snapshot = controller.observer_snapshot().expect("snapshot");
    assert_eq!(snapshot.tasks[0].entries.len(), 1);
    assert_eq!(snapshot.tasks[0].entries[0].text, "live partial");
    assert_eq!(snapshot.tasks[0].row.status, "running");
    registry.finish(
        &task.id,
        crate::subagent::tasks::TaskStatus::Succeeded,
        chrono::Utc::now(),
        "done",
        "",
    );
    let snapshot = controller.observer_snapshot().expect("finished snapshot");
    assert_eq!(snapshot.tasks[0].entries[0].text, "live partial");
    assert_eq!(snapshot.tasks[0].row.status, "succeeded");
}

#[tokio::test]
async fn observer_missing_history_is_unavailable_not_truncated() {
    use crate::subagent::record::{Recorder, Store, TaskContext};
    let workspace = tempfile::tempdir().unwrap();
    let sessions = tempfile::tempdir().unwrap();
    let store = std::sync::Arc::new(Store::open_in_memory().unwrap());
    let mut builder = crate::fixtures::builder(workspace.path(), sessions.path());
    builder.shared_mut().task_recorder = Some(store.clone());
    let runtime = crate::fixtures::initial_runtime(&builder);
    let session = builder.create_session(&runtime).unwrap();
    let runner = builder.build_runner(&session, &runtime).await.unwrap();
    let info = builder.runtime_info(&runtime);
    let controller = Controller::new(builder, true, session, runner, info);
    let (parent, parent_path, workspace) = {
        let state = controller.lock();
        let current = state.current.as_ref().unwrap();
        (
            current.session.header().id,
            current.path(),
            current.workspace.clone(),
        )
    };
    store.upsert(
        &TaskContext {
            parent_session: parent,
            parent_session_path: parent_path,
            workspace,
            ..Default::default()
        },
        &crate::subagent::tasks::Task {
            id: "unavailable".into(),
            status: crate::subagent::tasks::TaskStatus::Succeeded,
            ..Default::default()
        },
    );
    let snapshot = controller.observer_snapshot().unwrap();
    assert_eq!(snapshot.tasks.len(), 1);
    assert!(snapshot.tasks[0].missing);
    assert!(!snapshot.truncated);
    assert_eq!(
        snapshot.tasks[0].entries[0].text,
        "Child transcript unavailable."
    );
}

#[tokio::test]
async fn observer_recorder_resolves_parent_session_path_after_first_write() {
    use crate::subagent::record::Store;
    let workspace = tempfile::tempdir().unwrap();
    let sessions = tempfile::tempdir().unwrap();
    let store = std::sync::Arc::new(Store::open_in_memory().unwrap());
    let mut builder = crate::fixtures::builder(workspace.path(), sessions.path());
    builder.shared_mut().task_recorder = Some(store.clone());
    let runtime = crate::fixtures::initial_runtime(&builder);
    let session = builder.create_session(&runtime).unwrap();
    let runner = builder.build_runner(&session, &runtime).await.unwrap();
    assert!(
        session.path().is_empty(),
        "the fixture session must be lazy: no path before the first write"
    );
    session
        .append(Message {
            role: Role::User,
            created_at: chrono::Utc::now(),
            blocks: vec![Block::text("first prompt")],
            ..Default::default()
        })
        .await
        .unwrap();
    assert!(!session.path().is_empty());
    let registry = runner.tasks().expect("registry");
    let task = registry
        .add(crate::subagent::tasks::Task::default(), None, None)
        .unwrap();
    let row = store
        .get(&session.header().id, &task.id)
        .unwrap()
        .expect("recorded row");
    assert_eq!(row.parent_session_path, session.path());
}

#[tokio::test]
async fn observer_includes_stored_rows_recorded_without_parent_session_path() {
    use crate::subagent::record::{Recorder, Store, TaskContext};
    let workspace = tempfile::tempdir().unwrap();
    let sessions = tempfile::tempdir().unwrap();
    let store = std::sync::Arc::new(Store::open_in_memory().unwrap());
    let mut builder = crate::fixtures::builder(workspace.path(), sessions.path());
    builder.shared_mut().task_recorder = Some(store.clone());
    let runtime = crate::fixtures::initial_runtime(&builder);
    let session = builder.create_session(&runtime).unwrap();
    let runner = builder.build_runner(&session, &runtime).await.unwrap();
    session
        .append(Message {
            role: Role::User,
            created_at: chrono::Utc::now(),
            blocks: vec![Block::text("parent prompt")],
            ..Default::default()
        })
        .await
        .unwrap();
    let info = builder.runtime_info(&runtime);
    let controller = Controller::new(builder, true, session, runner, info);
    let (parent, parent_path, workspace) = {
        let state = controller.lock();
        let current = state.current.as_ref().unwrap();
        (
            current.session.header().id,
            current.path(),
            current.workspace.clone(),
        )
    };
    assert!(!parent_path.is_empty());
    let child_store = otto_session::Store::create_child_lazy(
        std::path::Path::new(&parent_path),
        "t1-child",
        Header {
            id: "child".into(),
            workspace: workspace.clone(),
            provider: "openai-compatible".into(),
            model: "fixture".into(),
            created_at: chrono::Utc::now(),
            ..Default::default()
        },
    )
    .expect("child store");
    let child_session =
        crate::runtime::builder::SharedSession::new(std::sync::Arc::new(child_store));
    child_session
        .append(Message {
            role: Role::User,
            created_at: chrono::DateTime::from_timestamp(1_793_750_400, 0).unwrap(),
            blocks: vec![Block::text("private prompt")],
            ..Default::default()
        })
        .await
        .unwrap();
    child_session
        .append(Message {
            role: Role::Assistant,
            created_at: chrono::DateTime::from_timestamp(1_793_750_401, 0).unwrap(),
            finish_reason: Some(FinishReason::Stop),
            blocks: vec![Block::text("retained answer")],
            ..Default::default()
        })
        .await
        .unwrap();
    let child_path = child_session.path();
    child_session.close().unwrap();
    store.upsert(
        &TaskContext {
            parent_session: parent,
            parent_session_path: String::new(),
            workspace,
            ..Default::default()
        },
        &crate::subagent::tasks::Task {
            id: "t1".into(),
            status: crate::subagent::tasks::TaskStatus::Succeeded,
            session_path: child_path,
            ..Default::default()
        },
    );
    let snapshot = controller.observer_snapshot().unwrap();
    assert_eq!(snapshot.tasks.len(), 1);
    assert!(!snapshot.tasks[0].missing);
    assert_eq!(snapshot.tasks[0].entries[0].text, "retained answer");
}

#[test]
fn observer_metadata_excludes_prompts_paths_and_full_result() {
    let mut row = TaskRow {
        prompt: "private".into(),
        context: "private".into(),
        result: "large output".into(),
        session_path: "/private/path".into(),
        parent_session_path: "/private/parent".into(),
        description: "x".repeat(50000),
        ..Default::default()
    };
    sanitize_row(&mut row);
    assert!(row.prompt.is_empty() && row.context.is_empty() && row.result.is_empty());
    assert!(row.session_path.is_empty() && row.parent_session_path.is_empty());
    assert!(row.description.len() <= 4096);
}
