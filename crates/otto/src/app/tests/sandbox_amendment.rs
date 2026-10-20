//! Runner replacement and sandbox amendment tests.

use super::*;

#[tokio::test]
async fn replacing_runner_only_happens_between_turns() {
    let workspace = tempfile::tempdir().expect("workspace");
    let sessions = tempfile::tempdir().expect("sessions");
    let controller = controller(workspace.path(), sessions.path()).await;
    let session = controller.current_session();
    let session_id = session.header().id;
    let runtime = initial_runtime(controller.builder());
    let info = controller.builder().runtime_info(&runtime);

    let candidate = controller
        .builder()
        .build_runner(&session, &runtime)
        .await
        .expect("runner");
    let admission = controller.begin_operation().expect("admit turn");
    assert_eq!(
        controller.replace_runner_if_current(&session_id, candidate, info.clone()),
        Err(PROMPT_ACTIVE.to_string())
    );
    drop(admission);

    let candidate = controller
        .builder()
        .build_runner(&session, &runtime)
        .await
        .expect("runner");
    assert_eq!(
        controller.replace_runner_if_current(&session_id, candidate, info),
        Ok(true)
    );
}

#[tokio::test]
async fn amending_the_sandbox_writes_the_configuration_and_reloads_it() {
    let workspace = tempfile::tempdir().expect("workspace");
    let sessions = tempfile::tempdir().expect("sessions");
    let (control, calls) = FakeSandbox::new(
        seatbelt_info(SandboxNetwork::Allowed),
        seatbelt_info(SandboxNetwork::Denied),
        None,
    );
    let controller = controller(workspace.path(), sessions.path())
        .await
        .with_sandbox_control(control);
    std::fs::create_dir(workspace.path().join("cache")).expect("cache");
    let resolved = controller
        .resolve_sandbox_read_path("~/cache")
        .expect("resolve");

    let info = controller
        .amend_sandbox(SandboxChange::AllowReadPath(resolved.clone()))
        .await
        .expect("amend");

    assert_eq!(info.network, SandboxNetwork::Denied);
    assert_eq!(*calls.lock().expect("calls"), 1);
    let written = std::fs::read_to_string(controller.config_path()).expect("read config");
    assert!(written.contains(&resolved), "{written}");
}

#[tokio::test]
async fn a_failed_reload_rolls_the_configuration_back() {
    let workspace = tempfile::tempdir().expect("workspace");
    let sessions = tempfile::tempdir().expect("sessions");
    let (control, calls) = FakeSandbox::new(
        seatbelt_info(SandboxNetwork::Allowed),
        seatbelt_info(SandboxNetwork::Allowed),
        Some("sandbox reload failed: self-test-failed"),
    );
    let controller = controller(workspace.path(), sessions.path())
        .await
        .with_sandbox_control(control);
    std::fs::write(controller.config_path(), "# preserved\n").expect("write config");

    let error = controller
        .amend_sandbox(SandboxChange::Network("deny".to_string()))
        .await
        .expect_err("reload fails");

    assert!(error.contains("self-test-failed"), "{error}");
    assert!(error.contains("rolled back"), "{error}");
    assert_eq!(*calls.lock().expect("calls"), 1);
    assert_eq!(
        std::fs::read_to_string(controller.config_path()).expect("read config"),
        "# preserved\n"
    );
}

#[tokio::test]
async fn amending_without_a_sandbox_control_or_while_busy_writes_nothing() {
    let workspace = tempfile::tempdir().expect("workspace");
    let sessions = tempfile::tempdir().expect("sessions");
    let controller = controller(workspace.path(), sessions.path()).await;

    assert_eq!(
        controller
            .amend_sandbox(SandboxChange::Network("deny".to_string()))
            .await,
        Err(SANDBOX_RELOAD_UNAVAILABLE.to_string())
    );

    let (control, _) = FakeSandbox::new(
        seatbelt_info(SandboxNetwork::Allowed),
        seatbelt_info(SandboxNetwork::Denied),
        None,
    );
    let controller = controller.with_sandbox_control(control);
    let _admission = controller.begin_operation().expect("admit turn");
    assert_eq!(
        controller
            .amend_sandbox(SandboxChange::Network("deny".to_string()))
            .await,
        Err(PROMPT_ACTIVE.to_string())
    );
    assert!(!controller.config_path().exists());
}
