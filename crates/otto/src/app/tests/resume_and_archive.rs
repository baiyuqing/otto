//! Listing, resume and archive tests.

use super::*;

#[tokio::test]
async fn listing_marks_the_open_session_and_resuming_it_is_a_no_op() {
    let workspace = tempfile::tempdir().expect("workspace");
    let sessions = tempfile::tempdir().expect("sessions");
    let controller = controller(workspace.path(), sessions.path()).await;
    controller
        .current_session()
        .append(user("hello"))
        .await
        .expect("append");
    let open = controller.info();

    let listed = controller.list_sessions(5).expect("list");
    let row = listed
        .sessions
        .iter()
        .find(|row| row.id == open.session_id)
        .expect("row for the open session");
    assert!(row.current, "the open session must be marked current");

    let resumed = controller
        .resume_session(&open.session_path)
        .await
        .expect("resume");
    assert_eq!(
        canonical_session_path(&resumed.session_path),
        canonical_session_path(&open.session_path)
    );
    assert_eq!(controller.info().session_id, open.session_id);
}

#[tokio::test]
async fn resuming_another_session_replaces_the_current_one() {
    let workspace = tempfile::tempdir().expect("workspace");
    let sessions = tempfile::tempdir().expect("sessions");
    let controller = controller(workspace.path(), sessions.path()).await;
    controller
        .current_session()
        .append(user("first"))
        .await
        .expect("append");
    let first = controller.info();

    controller.new_session().await.expect("new session");
    controller
        .current_session()
        .append(user("second"))
        .await
        .expect("append");
    let second = controller.info();
    assert_ne!(first.session_id, second.session_id);

    let resumed = controller
        .resume_session(&first.session_path)
        .await
        .expect("resume");
    assert!(resumed.warnings.is_empty(), "{:?}", resumed.warnings);
    assert_eq!(controller.info().session_id, first.session_id);
    assert_eq!(controller.history().len(), 1);
}

#[tokio::test]
async fn resuming_a_session_held_by_another_renewing_holder_fails_and_keeps_the_current_session() {
    let workspace = tempfile::tempdir().expect("workspace");
    let sessions = tempfile::tempdir().expect("sessions");
    let controller = controller(workspace.path(), sessions.path()).await;
    controller
        .current_session()
        .append(user("first"))
        .await
        .expect("append");
    let first = controller.info();

    controller.new_session().await.expect("new session");
    controller
        .current_session()
        .append(user("second"))
        .await
        .expect("append");
    let second = controller.info();

    let dir = Path::new(&first.session_path).with_extension("lease");
    std::fs::create_dir_all(&dir).expect("create lease dir");
    std::fs::write(
        dir.join("lease.json"),
        serde_json::json!({ "lease_seconds": 2 }).to_string(),
    )
    .expect("write lease.json");
    std::fs::write(dir.join("epoch-1"), b"{}").expect("write epoch marker");
    std::fs::write(
        dir.join("heartbeat"),
        lease_heartbeat_bytes(1, "other-host", 4242, false, 1),
    )
    .expect("write heartbeat");

    let heartbeat_path = dir.join("heartbeat");
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let writer_stop = Arc::clone(&stop);
    let writer = std::thread::spawn(move || {
        let mut seq = 2u64;
        while !writer_stop.load(std::sync::atomic::Ordering::Relaxed) {
            std::fs::write(
                &heartbeat_path,
                lease_heartbeat_bytes(1, "other-host", 4242, false, seq),
            )
            .expect("rewrite heartbeat");
            seq += 1;
            std::thread::sleep(std::time::Duration::from_millis(200));
        }
    });

    let error = controller
        .resume_session(&first.session_path)
        .await
        .expect_err("holder is still live");

    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    writer.join().expect("join heartbeat writer");

    assert!(error.contains("other-host"), "{error}");
    assert!(error.contains("4242"), "{error}");
    assert_eq!(
        controller.info().session_id,
        second.session_id,
        "the current session must stay in place after a failed resume"
    );
}

#[tokio::test]
async fn archiving_a_listed_session_leaves_the_current_one_open() {
    let workspace = tempfile::tempdir().expect("workspace");
    let sessions = tempfile::tempdir().expect("sessions");
    let controller = controller(workspace.path(), sessions.path()).await;
    controller
        .current_session()
        .append(user("first"))
        .await
        .expect("append");
    let first = controller.info();
    controller.new_session().await.expect("new session");
    controller
        .current_session()
        .append(user("second"))
        .await
        .expect("append");
    let second = controller.info();

    let result = controller
        .archive_session(&first.session_path)
        .await
        .expect("archive");

    assert_eq!(result.id, first.session_id);
    assert!(!Path::new(&first.session_path).exists());
    assert_eq!(controller.info().session_id, second.session_id);
}

#[tokio::test]
async fn archiving_the_current_session_by_path_starts_a_fresh_one() {
    let workspace = tempfile::tempdir().expect("workspace");
    let sessions = tempfile::tempdir().expect("sessions");
    let controller = controller(workspace.path(), sessions.path()).await;
    controller
        .current_session()
        .append(user("hello"))
        .await
        .expect("append");
    let before = controller.info();

    controller
        .archive_session(&before.session_path)
        .await
        .expect("archive");

    assert_ne!(controller.info().session_id, before.session_id);
}

// ---- tasks and wake ----
