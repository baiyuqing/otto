//! `/sandbox` and `/approve` command tests.

use super::*;

#[tokio::test]
async fn sandbox_prints_the_state_and_rejects_an_unknown_subcommand() {
    let workspace = tempfile::tempdir().expect("workspace");
    let sessions = tempfile::tempdir().expect("sessions");
    let controller = controller(workspace.path(), sessions.path()).await;

    let (stdout, stderr, result) = session("/sandbox\n/sandbox bogus\n/exit\n", &controller).await;

    assert!(result.is_ok(), "{result:?}");
    assert!(
        stdout.contains(&format!(
            "Sandbox: {}\n",
            controller.sandbox_info().summary()
        )),
        "{stdout}"
    );
    assert_eq!(stderr, "unknown command: /sandbox bogus\n");
}

async fn reloading_controller(
    workspace: &Path,
    sessions: &Path,
    failure: Option<&str>,
) -> (Controller, Arc<Mutex<usize>>) {
    let (control, calls) = fixtures::FakeSandbox::new(
        fixtures::seatbelt_info(SandboxNetwork::Allowed),
        fixtures::seatbelt_info(SandboxNetwork::Denied),
        failure,
    );
    let controller = controller(workspace, sessions)
        .await
        .with_sandbox_control(control);
    (controller, calls)
}

#[tokio::test]
async fn sandbox_shows_the_current_state_without_reloading() {
    let workspace = tempfile::tempdir().expect("workspace");
    let sessions = tempfile::tempdir().expect("sessions");
    let (controller, calls) = reloading_controller(workspace.path(), sessions.path(), None).await;

    let (stdout, stderr, result) = session("/sandbox\n/exit\n", &controller).await;

    assert!(result.is_ok(), "{result:?}");
    assert!(
        stdout.contains("Sandbox: seatbelt · workspace-write · network allowed\n"),
        "{stdout}"
    );
    assert_eq!(stderr, "");
    assert_eq!(*calls.lock().expect("calls"), 0);
}

#[tokio::test]
async fn sandbox_lists_inert_read_paths_and_skipped_deny_paths() {
    let workspace = tempfile::tempdir().expect("workspace");
    let sessions = tempfile::tempdir().expect("sessions");
    let mut info = fixtures::seatbelt_info(SandboxNetwork::Allowed);
    info.inert_read_paths = vec!["~/.otto".to_string()];
    info.skipped_deny_paths = vec!["~/.config/gh".to_string()];
    let (control, _) = fixtures::FakeSandbox::new(info.clone(), info, None);
    let controller = controller(workspace.path(), sessions.path())
        .await
        .with_sandbox_control(control);

    let (stdout, _, result) = session("/sandbox\n/exit\n", &controller).await;

    assert!(result.is_ok(), "{result:?}");
    assert!(
        stdout.contains(
            "Sandbox: seatbelt · workspace-write · network allowed\n\
Sandbox read_paths ~/.otto: not below any denied path; remove it from read_paths\n\
Sandbox deny_read_paths ~/.config/gh: contains a writable root; not applied\n"
        ),
        "{stdout}"
    );
}

#[tokio::test]
async fn sandbox_prints_no_path_lines_when_none_apply() {
    let workspace = tempfile::tempdir().expect("workspace");
    let sessions = tempfile::tempdir().expect("sessions");
    let (controller, _) = reloading_controller(workspace.path(), sessions.path(), None).await;

    let (stdout, _, _) = session("/sandbox\n/exit\n", &controller).await;

    assert!(!stdout.contains("Sandbox read_paths"), "{stdout}");
    assert!(!stdout.contains("Sandbox deny_read_paths"), "{stdout}");
}

#[tokio::test]
async fn sandbox_allow_refuses_a_path_the_denylist_does_not_cover() {
    let workspace = tempfile::tempdir().expect("workspace");
    let sessions = tempfile::tempdir().expect("sessions");
    let home = tempfile::tempdir().expect("home");
    let denied = home.path().join("secrets");
    let readable = workspace.path().join("cache");
    std::fs::create_dir(&denied).expect("denied");
    std::fs::create_dir(&readable).expect("readable");
    let denylist = crate::sandbox::read_denylist::ReadDenylist::new(
        home.path(),
        vec![denied.to_string_lossy().into_owned()],
    );
    let (control, calls) = fixtures::FakeSandbox::with_read_denylist(
        fixtures::seatbelt_info(SandboxNetwork::Allowed),
        denylist,
    );
    let controller = controller(workspace.path(), sessions.path())
        .await
        .with_sandbox_control(control);

    let (_, _, result) = session(
        &format!("/sandbox allow {}\n", readable.display()),
        &controller,
    )
    .await;

    let error = result.expect_err("already readable");
    assert_eq!(
        error.to_string(),
        "path is already readable; no read_paths entry is needed"
    );
    assert_eq!(*calls.lock().expect("calls"), 0);
    assert!(!workspace.path().join("config.toml").exists());

    let (_, stderr, result) = session(
        &format!("/sandbox allow {}\n/exit\n", denied.display()),
        &controller,
    )
    .await;
    assert!(result.is_ok(), "{result:?}");
    assert_eq!(stderr, "");
    assert_eq!(*calls.lock().expect("calls"), 1);
    let written = std::fs::read_to_string(workspace.path().join("config.toml")).expect("config");
    assert!(written.contains("secrets"), "{written}");
}

#[tokio::test]
async fn sandbox_reload_reports_the_new_state() {
    let workspace = tempfile::tempdir().expect("workspace");
    let sessions = tempfile::tempdir().expect("sessions");
    let (controller, calls) = reloading_controller(workspace.path(), sessions.path(), None).await;

    let (stdout, _, result) = session("/sandbox reload\n/exit\n", &controller).await;

    assert!(result.is_ok(), "{result:?}");
    assert_eq!(*calls.lock().expect("calls"), 1);
    assert!(stdout.contains("network denied"), "{stdout}");
}

#[tokio::test]
async fn sandbox_reload_reports_a_failure() {
    let workspace = tempfile::tempdir().expect("workspace");
    let sessions = tempfile::tempdir().expect("sessions");
    let (controller, _) = reloading_controller(
        workspace.path(),
        sessions.path(),
        Some("sandbox reload failed: self-test-failed"),
    )
    .await;

    let (_, _, result) = session("/sandbox reload\n", &controller).await;

    let error = result.expect_err("reload");
    assert!(is_command_error(&error, "/sandbox"), "{error:?}");
    assert_eq!(error.to_string(), "sandbox reload failed: self-test-failed");
}

/// The CLI builds no control when bash never came up.
#[tokio::test]
async fn sandbox_reload_without_a_control_is_reported() {
    let workspace = tempfile::tempdir().expect("workspace");
    let sessions = tempfile::tempdir().expect("sessions");
    let controller = controller(workspace.path(), sessions.path()).await;

    let (_, _, result) = session("/sandbox reload\n", &controller).await;

    let error = result.expect_err("reload");
    assert!(is_command_error(&error, "/sandbox"), "{error:?}");
    assert_eq!(error.to_string(), SANDBOX_RELOAD_UNAVAILABLE);
}

#[tokio::test]
async fn sandbox_allow_writes_the_read_path_and_reloads() {
    let workspace = tempfile::tempdir().expect("workspace");
    let sessions = tempfile::tempdir().expect("sessions");
    let granted = workspace.path().join("cache");
    std::fs::create_dir(&granted).expect("cache");
    let (controller, calls) = reloading_controller(workspace.path(), sessions.path(), None).await;

    let (stdout, stderr, result) = session(
        &format!("/sandbox allow {}\n/exit\n", granted.display()),
        &controller,
    )
    .await;

    assert!(result.is_ok(), "{result:?}");
    assert_eq!(stderr, "");
    assert_eq!(*calls.lock().expect("calls"), 1);
    assert!(stdout.contains("network denied"), "{stdout}");
    let written = std::fs::read_to_string(workspace.path().join("config.toml")).expect("config");
    assert!(written.contains("read_paths"), "{written}");
    assert!(written.contains("cache"), "{written}");
}

#[tokio::test]
async fn sandbox_exclude_writes_the_entry_and_reloads() {
    let workspace = tempfile::tempdir().expect("workspace");
    let sessions = tempfile::tempdir().expect("sessions");
    let (controller, calls) = reloading_controller(workspace.path(), sessions.path(), None).await;

    let (_, stderr, result) = session(
        "/sandbox exclude 'acme-cli *'\n/sandbox exclude acme-cli *\n/exit\n",
        &controller,
    )
    .await;

    assert!(result.is_ok(), "{result:?}");
    assert_eq!(stderr, "");
    assert_eq!(*calls.lock().expect("calls"), 2);
    let written = std::fs::read_to_string(workspace.path().join("config.toml")).expect("config");
    assert!(
        written.contains("excluded_commands = ['acme-cli *']"),
        "{written}"
    );
}

#[tokio::test]
async fn sandbox_exclude_without_an_entry_or_with_an_invalid_one_writes_nothing() {
    let workspace = tempfile::tempdir().expect("workspace");
    let sessions = tempfile::tempdir().expect("sessions");
    let (controller, calls) = reloading_controller(workspace.path(), sessions.path(), None).await;

    let (_, _, result) = session("/sandbox exclude\n", &controller).await;
    let error = result.expect_err("no entry");
    assert!(is_command_error(&error, "/sandbox"), "{error:?}");
    assert!(
        error.to_string().starts_with("usage: /sandbox exclude"),
        "{error}"
    );

    let (_, _, result) = session("/sandbox exclude a; b\n", &controller).await;
    let error = result.expect_err("invalid entry");
    assert!(
        error.to_string().contains("invalid excluded command"),
        "{error}"
    );

    assert_eq!(*calls.lock().expect("calls"), 0);
    assert!(!workspace.path().join("config.toml").exists());
}

/// The success path starts a provider turn, so it is covered at the
/// controller (`approve_always_excludes_the_exact_command_reloads_and_grants_the_request`);
/// here only the dispatch and the refusals, which end before any turn.
#[tokio::test]
async fn approve_always_dispatches_to_the_controller_and_refuses_extra_words() {
    let workspace = tempfile::tempdir().expect("workspace");
    let sessions = tempfile::tempdir().expect("sessions");
    let (controller, approvals, calls) =
        crate::app::controller_with_approvals(workspace.path(), sessions.path()).await;
    let session_id = controller.info().session_id;
    let id = approvals.request(&session_id, "git push; rm -rf x");

    let (_, stderr, result) =
        session(&format!("/approve {id} always now\n/exit\n"), &controller).await;
    assert!(result.is_ok(), "{result:?}");
    assert_eq!(
        stderr,
        format!("unknown command: /approve {id} always now\n")
    );

    let (_, _, result) = session("/approve approval-99 always\n", &controller).await;
    let error = result.expect_err("unknown id");
    assert!(is_command_error(&error, "/approve"), "{error:?}");
    assert_eq!(error.to_string(), "approval request not found");

    let (_, _, result) = session(&format!("/approve {id} always\n"), &controller).await;
    let error = result.expect_err("compound command");
    assert!(
        error.to_string().contains("use /approve <id> instead"),
        "{error}"
    );

    assert_eq!(*calls.lock().expect("calls"), 0);
    assert!(!workspace.path().join("config.toml").exists());
}

#[tokio::test]
async fn sandbox_allow_rejects_a_path_that_does_not_exist() {
    let workspace = tempfile::tempdir().expect("workspace");
    let sessions = tempfile::tempdir().expect("sessions");
    let (controller, calls) = reloading_controller(workspace.path(), sessions.path(), None).await;

    let (_, _, result) = session("/sandbox allow ~/missing\n", &controller).await;

    let error = result.expect_err("allow");
    assert!(is_command_error(&error, "/sandbox"), "{error:?}");
    assert_eq!(error.to_string(), "no such path: ~/missing");
    assert_eq!(*calls.lock().expect("calls"), 0);
    assert!(!workspace.path().join("config.toml").exists());
}

#[tokio::test]
async fn sandbox_network_sets_the_mode_and_reports_an_unknown_one() {
    let workspace = tempfile::tempdir().expect("workspace");
    let sessions = tempfile::tempdir().expect("sessions");
    let (controller, calls) = reloading_controller(workspace.path(), sessions.path(), None).await;

    let (stdout, stderr, result) = session(
        "/sandbox network deny\n/sandbox network sometimes\n/exit\n",
        &controller,
    )
    .await;

    assert!(result.is_ok(), "{result:?}");
    assert_eq!(*calls.lock().expect("calls"), 1);
    assert!(stdout.contains("network denied"), "{stdout}");
    assert_eq!(stderr, "unknown command: /sandbox network sometimes\n");
    let written = std::fs::read_to_string(workspace.path().join("config.toml")).expect("config");
    assert!(written.contains("network = 'deny'"), "{written}");
}
