//! Persisted parent activation must rebuild tasks independently of turn recovery.

use super::*;

#[test]
fn saved_finished_parent_hydrates_children_without_executor_or_parent_recovery() {
    let root = tempfile::tempdir().unwrap();
    let builder = builder_for(shared_with_no_session(root.path(), false), root.path());
    let parent = crate::session::Store::create(
        root.path().join("sessions"),
        Header {
            id: "parent".into(),
            workspace: root.path().to_string_lossy().into(),
            created_at: chrono::Utc::now(),
            provider: "openai-compatible".into(),
            model: "test".into(),
            ..Header::default()
        },
    )
    .unwrap();
    let path = std::path::PathBuf::from(parent.path());
    parent
        .append_message(&Message {
            role: otto_core::model::Role::Assistant,
            blocks: vec![otto_core::model::Block::text("parent finished normally")],
            created_at: chrono::Utc::now(),
            finish_reason: Some(otto_core::model::FinishReason::Stop),
            ..Message::default()
        })
        .unwrap();
    let child = crate::session::Store::create_child_lazy(
        &path,
        "t1-child",
        Header {
            id: "child".into(),
            workspace: root.path().to_string_lossy().into(),
            created_at: chrono::Utc::now(),
            provider: "openai-compatible".into(),
            model: "test".into(),
            ..Header::default()
        },
    )
    .unwrap();
    child.append_custom_entry("otto.task_spec", r#"{"id":"t1","name":"","description":"orphaned child","model":"test","context":"fresh","prompt":"work","definition":null}"#).unwrap();
    drop(child);
    drop(parent);
    let (parent, _) = crate::session::Store::open(&path).unwrap();
    assert!(
        parent.take_recovery().is_none(),
        "parent must not itself look interrupted"
    );
    let session = SharedSession::new(Arc::new(parent));
    let mut warnings = Vec::new();
    let resources = builder.session_resources(&session, false, 4096, &mut warnings);
    let tasks = resources
        .tasks()
        .expect("diagnostic registry exists without executor");
    let task = tasks
        .get("t1")
        .expect("activation must import original child");
    assert_eq!(task.status, crate::subagent::TaskStatus::Interrupted);
    assert_eq!(
        crate::subagent::interrupted::scan(&path.with_extension(""))[0]
            .final_status
            .as_deref(),
        Some("interrupted")
    );
    let second = builder.session_resources(&session, false, 4096, &mut warnings);
    assert!(Arc::ptr_eq(&resources, &second));
    assert_eq!(tasks.list().len(), 1);
}
