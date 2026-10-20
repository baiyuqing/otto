//! Admission, close and closed-boundary tests.

use super::*;

#[tokio::test]
async fn a_closed_boundary_refuses_every_replacement() {
    let workspace = tempfile::tempdir().expect("workspace");
    let sessions = tempfile::tempdir().expect("sessions");
    let builder = builder(workspace.path(), sessions.path());
    let runtime = initial_runtime(&builder);
    let session = builder.create_session(&runtime).expect("session");
    let runner = builder
        .build_runner(&session, &runtime)
        .await
        .expect("runner");
    let info = builder.runtime_info(&runtime);
    let controller = Controller::new(builder, false, session, runner, info);

    assert_eq!(controller.profiles(), Vec::<String>::new());
    assert_eq!(controller.info().model, "");
    assert_eq!(
        controller.new_session().await.expect_err("new"),
        PERSISTENCE_DISABLED
    );
    assert_eq!(
        controller.switch_profile("beta").await.expect_err("switch"),
        PROFILE_SWITCH_UNAVAILABLE
    );
    assert_eq!(
        controller.set_default_profile("beta").expect_err("default"),
        PROFILE_SWITCH_UNAVAILABLE
    );
    assert_eq!(
        controller.rename_session("name").expect_err("rename"),
        PERSISTENCE_DISABLED
    );
    assert_eq!(
        controller.list_sessions(5).expect_err("list"),
        PERSISTENCE_DISABLED
    );
    assert!(controller.history().is_empty());
}

// ---- admission ----

#[tokio::test]
async fn one_operation_at_a_time_is_admitted() {
    let workspace = tempfile::tempdir().expect("workspace");
    let sessions = tempfile::tempdir().expect("sessions");
    let controller = controller(workspace.path(), sessions.path()).await;

    let admission = controller.begin_operation().expect("first");
    assert_eq!(
        controller.begin_operation().expect_err("second"),
        PROMPT_ACTIVE
    );
    assert_eq!(
        controller.rename_session("name").expect_err("rename"),
        PROMPT_ACTIVE
    );
    assert_eq!(
        controller.new_session().await.expect_err("new"),
        PROMPT_ACTIVE
    );
    assert_eq!(
        controller.reload_sandbox().await.expect_err("reload"),
        SANDBOX_RELOAD_UNAVAILABLE
    );
    drop(admission);
    controller.begin_operation().expect("after release");
}

#[tokio::test]
async fn a_closed_controller_refuses_every_operation() {
    let workspace = tempfile::tempdir().expect("workspace");
    let sessions = tempfile::tempdir().expect("sessions");
    let controller = controller(workspace.path(), sessions.path()).await;

    controller.close().expect("close");

    assert_eq!(controller.begin_operation().expect_err("begin"), CLOSED);
    assert_eq!(controller.list_sessions(5).expect_err("list"), CLOSED);
    assert_eq!(
        controller.rename_session("name").expect_err("rename"),
        CLOSED
    );
    assert_eq!(controller.new_session().await.expect_err("new"), CLOSED);
    assert_eq!(
        controller.reload_sandbox().await.expect_err("reload"),
        CLOSED
    );
    assert!(controller.tasks().is_none());
    assert!(!controller.dynamic_content_available());
    assert!(controller.current_session_opt().is_none());
}

#[tokio::test]
async fn close_is_idempotent_and_reports_the_same_result() {
    let workspace = tempfile::tempdir().expect("workspace");
    let sessions = tempfile::tempdir().expect("sessions");
    let controller = controller(workspace.path(), sessions.path()).await;

    assert!(controller.close().is_ok());
    assert!(controller.close().is_ok());
}

#[tokio::test]
async fn a_close_requested_during_an_operation_runs_when_it_ends() {
    let workspace = tempfile::tempdir().expect("workspace");
    let sessions = tempfile::tempdir().expect("sessions");
    let controller = controller(workspace.path(), sessions.path()).await;

    let admission = controller.begin_operation().expect("admit");
    controller.request_close();
    // Still held, so the session is untouched.
    assert!(controller.current_session_opt().is_some());

    drop(admission);

    assert!(controller.current_session_opt().is_none());
    assert!(controller.close().is_ok());
}

#[tokio::test]
async fn request_close_never_closes_an_idle_controller() {
    let workspace = tempfile::tempdir().expect("workspace");
    let sessions = tempfile::tempdir().expect("sessions");
    let controller = controller(workspace.path(), sessions.path()).await;

    controller.request_close();

    assert!(controller.current_session_opt().is_some());
    assert_eq!(controller.begin_operation().expect_err("begin"), CLOSED);
    controller.close().expect("close");
    assert!(controller.current_session_opt().is_none());
}

// ---- browsing ----
