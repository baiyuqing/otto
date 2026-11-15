use super::*;

#[tokio::test]
async fn permanent_delete_current_replaces_and_cleans_all_sidecars() {
    let workspace = tempfile::tempdir().unwrap();
    let sessions = tempfile::tempdir().unwrap();
    let controller = controller(workspace.path(), sessions.path()).await;
    controller
        .current_session()
        .append(user("hello"))
        .await
        .unwrap();
    let before = controller.info();
    let path = Path::new(&before.session_path);
    for suffix in [
        "reminders.json",
        "inbox.json",
        "inputs.json",
        "followups.json",
        "turns.json",
    ] {
        std::fs::write(path.with_extension(suffix), "[]").unwrap();
    }
    std::fs::create_dir_all(path.with_extension("").join("nested")).unwrap();
    std::fs::write(path.with_extension("").join("nested/child.jsonl"), "child").unwrap();
    controller.delete_current_session().await.unwrap();
    assert_ne!(controller.info().session_id, before.session_id);
    assert!(!path.exists());
    assert!(!path.with_extension("").exists());
    for suffix in [
        "reminders.json",
        "inbox.json",
        "inputs.json",
        "followups.json",
        "turns.json",
        "lease",
    ] {
        assert!(!path.with_extension(suffix).exists(), "{suffix}");
    }
}

#[tokio::test]
async fn permanent_delete_busy_refuses_without_cancelling() {
    let workspace = tempfile::tempdir().unwrap();
    let sessions = tempfile::tempdir().unwrap();
    let controller = controller(workspace.path(), sessions.path()).await;
    controller
        .current_session()
        .append(user("hello"))
        .await
        .unwrap();
    let before = controller.info();
    let admission = controller.begin_replacement().unwrap();
    assert_eq!(
        controller.delete_current_session().await.unwrap_err(),
        PROMPT_ACTIVE
    );
    assert!(Path::new(&before.session_path).exists());
    assert_eq!(controller.info().session_id, before.session_id);
    drop(admission);
}

#[tokio::test]
async fn permanent_delete_other_session_and_path_safety() {
    let workspace = tempfile::tempdir().unwrap();
    let sessions = tempfile::tempdir().unwrap();
    let controller = controller(workspace.path(), sessions.path()).await;
    controller
        .current_session()
        .append(user("hello"))
        .await
        .unwrap();
    let before = controller.info();
    controller.new_session().await.unwrap();
    let current = controller.info();
    let outside = workspace.path().join("outside.jsonl");
    std::fs::copy(&before.session_path, &outside).unwrap();
    assert!(
        controller
            .delete_session(outside.to_str().unwrap())
            .await
            .is_err()
    );
    assert!(outside.exists());
    controller
        .delete_session(&before.session_path)
        .await
        .unwrap();
    assert!(!Path::new(&before.session_path).exists());
    assert_eq!(controller.info().session_id, current.session_id);
    assert!(
        controller
            .delete_session(&before.session_path)
            .await
            .is_err(),
        "missing session is not successful deletion"
    );
}

#[tokio::test]
async fn permanent_delete_current_failure_keeps_current_session() {
    use std::os::unix::fs::PermissionsExt;
    let workspace = tempfile::tempdir().unwrap();
    let sessions = tempfile::tempdir().unwrap();
    let controller = controller(workspace.path(), sessions.path()).await;
    controller
        .current_session()
        .append(user("hello"))
        .await
        .unwrap();
    let before = controller.info();
    let children = Path::new(&before.session_path).with_extension("");
    std::fs::create_dir(&children).unwrap();
    std::fs::set_permissions(&children, std::fs::Permissions::from_mode(0o000)).unwrap();
    let result = controller.delete_current_session().await;
    std::fs::set_permissions(&children, std::fs::Permissions::from_mode(0o700)).unwrap();
    assert!(result.is_err());
    assert!(Path::new(&before.session_path).exists());
    assert_eq!(controller.info().session_id, before.session_id);
    controller
        .current_session()
        .append(user("still writable"))
        .await
        .unwrap();
}

#[tokio::test]
async fn permanent_delete_refuses_running_children_without_cancelling() {
    let workspace = tempfile::tempdir().unwrap();
    let sessions = tempfile::tempdir().unwrap();
    let controller = controller(workspace.path(), sessions.path()).await;
    controller
        .current_session()
        .append(user("hello"))
        .await
        .unwrap();
    let before = controller.info();
    let tasks = controller.subagent_tasks().unwrap();
    let cancel = CancellationToken::new();
    let task = tasks
        .add(
            crate::subagent::tasks::Task {
                agent: "explorer".into(),
                prompt: "working".into(),
                ..Default::default()
            },
            Some(cancel.clone()),
            None,
        )
        .unwrap();
    tasks.mark_running(&task.id, chrono::Utc::now());
    assert_eq!(
        controller.delete_current_session().await.unwrap_err(),
        PROMPT_ACTIVE
    );
    assert!(!cancel.is_cancelled());
    assert!(Path::new(&before.session_path).exists());
}

#[tokio::test]
async fn permanent_delete_current_lease_leaves_no_heartbeat() {
    let workspace = tempfile::tempdir().unwrap();
    let sessions = tempfile::tempdir().unwrap();
    let controller = controller(workspace.path(), sessions.path()).await;
    controller
        .current_session()
        .append(user("hello"))
        .await
        .unwrap();
    let before = controller.info();
    controller.new_session().await.unwrap();
    otto_session::lease::create_lease_dir(Path::new(&before.session_path), 30).unwrap();
    controller
        .resume_session(&before.session_path)
        .await
        .unwrap();
    controller.delete_current_session().await.unwrap();
    assert!(!Path::new(&before.session_path).exists());
    assert!(
        !Path::new(&before.session_path)
            .with_extension("lease")
            .exists()
    );
}
