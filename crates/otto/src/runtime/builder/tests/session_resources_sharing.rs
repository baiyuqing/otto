use super::*;

/// Two runners built on one session share its resources, registry, inbox
/// and MCP servers; the second build connects nothing new.
#[tokio::test]
async fn two_runner_builds_on_one_session_share_its_resources() {
    let dir = tempfile::tempdir().expect("temp dir");
    let builder = builder(dir.path());
    let session = SharedSession::memory(Header::default());

    let first = builder
        .build_runner_quiet(&session, &runtime())
        .await
        .expect("first runner");
    let resources_first = session.resources().expect("resources after first build");
    let second = builder
        .build_runner_quiet(&session, &runtime())
        .await
        .expect("second runner");
    let resources_second = session.resources().expect("resources after second build");

    assert!(Arc::ptr_eq(&resources_first, &resources_second));
    assert!(Arc::ptr_eq(&first.mcp(), &second.mcp()));
    assert!(Arc::ptr_eq(
        &first.tasks().expect("first tasks"),
        &second.tasks().expect("second tasks")
    ));
    assert!(Arc::ptr_eq(first.mailbox(), second.mailbox()));
    // Closing a superseded runner leaves the session's resources open.
    first.close();
    assert!(!resources_second.is_closed());
}

/// Two sessions of one builder own separate resources, registries and
/// inboxes.
#[tokio::test]
async fn two_sessions_of_one_builder_own_separate_resources() {
    let dir = tempfile::tempdir().expect("temp dir");
    let builder = builder(dir.path());
    let session_a = SharedSession::memory(Header::default());
    let session_b = SharedSession::memory(Header::default());

    let runner_a = builder
        .build_runner_quiet(&session_a, &runtime())
        .await
        .expect("runner a");
    let runner_b = builder
        .build_runner_quiet(&session_b, &runtime())
        .await
        .expect("runner b");

    let resources_a = session_a.resources().expect("resources a");
    let resources_b = session_b.resources().expect("resources b");
    assert!(!Arc::ptr_eq(&resources_a, &resources_b));
    assert!(!Arc::ptr_eq(
        &runner_a.tasks().expect("tasks a"),
        &runner_b.tasks().expect("tasks b")
    ));
    assert!(!Arc::ptr_eq(runner_a.mailbox(), runner_b.mailbox()));
    assert!(!Arc::ptr_eq(&runner_a.mcp(), &runner_b.mcp()));
    session_a.close_resources().await;
    assert!(resources_a.is_closed());
    assert!(!resources_b.is_closed());
}

/// The session owns its resources, so they hold it weakly: dropping the last
/// session handle and the runner releases the transcript.
#[tokio::test]
async fn dropping_the_session_and_runner_releases_the_transcript() {
    let dir = tempfile::tempdir().expect("temp dir");
    let builder = builder(dir.path());
    let session = SharedSession::memory(Header::default());
    let transcript = Arc::downgrade(session.handle());

    let runner = builder
        .build_runner_quiet(&session, &runtime())
        .await
        .expect("runner");
    runner.close();
    drop(runner);
    drop(session);

    assert!(transcript.upgrade().is_none());
}
