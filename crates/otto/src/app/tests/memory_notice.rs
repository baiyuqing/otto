//! Pending memory notice tests.

use super::*;

#[tokio::test]
async fn each_session_open_queues_one_pending_memory_notice() {
    let workspace = tempfile::tempdir().expect("workspace");
    let sessions = tempfile::tempdir().expect("sessions");
    let store = tempfile::tempdir().expect("store");
    let controller = crate::cli::fixture_tests::controller_with_memory(
        workspace.path(),
        sessions.path(),
        &store.path().join("memory"),
    )
    .await;
    assert!(controller.take_notices().is_empty(), "nothing is pending");
    controller.new_session().await.expect("new session");
    assert!(controller.take_notices().is_empty(), "nothing is pending");

    propose_pending(&controller, "a");
    assert!(
        controller.take_notices().is_empty(),
        "a proposal is not a session open"
    );
    propose_pending(&controller, "b");
    controller.new_session().await.expect("new session");
    let today = chrono::Utc::now().format("%Y-%m-%d");
    assert_eq!(
        controller.take_notices(),
        vec![format!(
            "2 memory proposals are pending review (oldest since {today}). Run /memory review to accept or reject them."
        )]
    );
    assert!(controller.take_notices().is_empty(), "queued once");
    let review = controller
        .management(crate::app::management::Request::MemoryReviewList)
        .expect("review list");
    let crate::app::management::Reply::MemoryCandidates(candidates) = review else {
        panic!("unexpected management reply");
    };
    assert_eq!(
        candidates.len(),
        2,
        "proposals from the previous session remain actionable"
    );

    controller.close().expect("close");
    let reopened = crate::cli::fixture_tests::controller_with_memory(
        workspace.path(),
        sessions.path(),
        &store.path().join("memory"),
    )
    .await;
    assert_eq!(reopened.take_notices().len(), 1);
}

#[tokio::test]
async fn session_open_queues_the_over_budget_standing_notice() {
    let workspace = tempfile::tempdir().expect("workspace");
    let sessions = tempfile::tempdir().expect("sessions");
    let store = tempfile::tempdir().expect("store");
    let path = store.path().join("memory");
    crate::cli::fixture_tests::seed_over_budget_standing(&path);
    let controller =
        crate::cli::fixture_tests::controller_with_memory(workspace.path(), sessions.path(), &path)
            .await;
    assert_eq!(
        controller.take_notices(),
        vec![
            "1 standing preference is not recalled because the standing set is over its 2000-token budget. Forget one with /memory forget <id>; list them with /memory list --scope user."
        ]
    );
}
