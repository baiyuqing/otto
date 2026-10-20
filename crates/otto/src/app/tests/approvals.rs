//! Read, bash and followup approval tests.

use super::*;

#[tokio::test]
async fn read_approval_persists_reloads_and_denial_invalidates_the_request() {
    let workspace = tempfile::tempdir().unwrap();
    let sessions = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let path = outside
        .path()
        .canonicalize()
        .unwrap()
        .to_str()
        .unwrap()
        .to_owned();
    let (controller, approvals, calls) =
        controller_with_approvals(workspace.path(), sessions.path()).await;
    let session = controller.info().session_id;
    let id = approvals.request_read_path(&session, "cat fixture", &path);
    assert!(!controller.config_path().exists());
    controller.deny_tool(&id).unwrap();
    assert!(controller.approve_tool(&id).await.is_err());
    assert_eq!(*calls.lock().unwrap(), 0);
    let id = approvals.request_read_path(&session, "cat fixture", &path);
    let prompt = controller.approve_tool(&id).await.unwrap();
    assert!(prompt.contains("inside the sandbox"));
    assert_eq!(*calls.lock().unwrap(), 1);
    let config = std::fs::read_to_string(controller.config_path()).unwrap();
    assert!(config.contains(&path), "{config}");
    assert!(config.contains("read_paths"));
    assert!(!config.contains("excluded_commands"));
    assert!(!approvals.take(&session, "cat fixture"));
    assert!(approvals.take_read_path(&session, "cat fixture", &path));
}

#[tokio::test]
async fn direct_terminal_followup_is_discarded_with_a_persisted_notice_when_approval_waits() {
    use otto_core::session::Session;

    let workspace = tempfile::tempdir().unwrap();
    let sessions = tempfile::tempdir().unwrap();
    let (controller, approvals, _) =
        controller_with_approvals(workspace.path(), sessions.path()).await;
    let runner = controller.runner().expect("runner");
    let session_id = runner.session().header().id.clone();
    let _approval = approvals.request(&session_id, "git push");
    let task_run = runner
        .task_control()
        .begin(&session_id, "initial task")
        .expect("logical task");
    runner
        .task_control()
        .queue_followup("inspect the pushed branch")
        .expect("followup");
    let mut events = Vec::new();
    controller
        .run_followups_held(
            &runner,
            &task_run,
            "",
            &mut |event| events.push(event),
            &CancellationToken::new(),
        )
        .await
        .expect("record skip");

    let notice = runner
        .session()
        .messages()
        .into_iter()
        .find(|message| message.context_type == "agent_followup")
        .expect("persisted source-labelled notice");
    assert_eq!(notice.role, otto_core::model::Role::Context);
    assert!(notice.display);
    assert!(notice.text().contains("approval is pending"));
    assert!(
        matches!(events.as_slice(), [otto_core::agent::Event::Notification { text, .. }] if text == &notice.text())
    );
    assert!(task_run.next_followup().is_none());
}

#[tokio::test]
async fn a_followup_whose_step_already_ran_is_not_run_again() {
    use otto_core::session::Session;

    let workspace = tempfile::tempdir().unwrap();
    let sessions = tempfile::tempdir().unwrap();
    let (controller, _approvals, _) =
        controller_with_approvals(workspace.path(), sessions.path()).await;
    let runner = controller.runner().expect("runner");
    let session_id = runner.session().header().id.clone();
    let task_run = runner
        .task_control()
        .begin(&session_id, "initial task")
        .expect("logical task");
    runner
        .task_control()
        .queue_followup("inspect the pushed branch")
        .expect("followup");
    // The step's agent_followup message was appended before Otto stopped.
    let queued = task_run.next_followup().expect("queued");
    runner
        .session()
        .append_input_if_absent(
            &queued.input_id,
            otto_core::model::Message {
                role: otto_core::model::Role::Context,
                context_type: "agent_followup".into(),
                display: true,
                blocks: vec![Block::text("[Agent follow-up]\ninspect the pushed branch")],
                created_at: chrono::Utc::now(),
                ..otto_core::model::Message::default()
            },
        )
        .await
        .expect("earlier step");
    task_run.requeue(queued);
    let mut events = Vec::new();
    // No provider is scripted: running the step would fail.
    controller
        .run_followups_held(
            &runner,
            &task_run,
            "",
            &mut |event| events.push(event),
            &CancellationToken::new(),
        )
        .await
        .expect("the step is skipped");
    assert!(task_run.next_followup().is_none());
}

#[tokio::test]
async fn read_approval_reload_failure_rolls_back_and_does_not_grant() {
    use crate::cli::fixture_tests::{FakeSandbox, seatbelt_info};
    use crate::runtime::info::SandboxNetwork;
    let workspace = tempfile::tempdir().unwrap();
    let sessions = tempfile::tempdir().unwrap();
    let path = workspace.path().canonicalize().unwrap();
    let (mut controller, approvals, _) =
        controller_with_approvals(workspace.path(), sessions.path()).await;
    let info = seatbelt_info(SandboxNetwork::Allowed);
    controller.sandbox = Some(FakeSandbox::new(info, info, Some("reload failed")).0);
    let original = "# preserve this config\n";
    std::fs::write(controller.config_path(), original).unwrap();
    let session = controller.info().session_id;
    let id = approvals.request_read_path(&session, "cat fixture", path.to_str().unwrap());
    assert!(
        controller
            .approve_tool(&id)
            .await
            .unwrap_err()
            .contains("rolled back")
    );
    assert_eq!(
        std::fs::read_to_string(controller.config_path()).unwrap(),
        original
    );
    assert!(!approvals.take_read_path(&session, "cat fixture", path.to_str().unwrap()));
}

#[tokio::test]
async fn read_approval_refuses_changed_paths_without_persisting() {
    let workspace = tempfile::tempdir().unwrap();
    let sessions = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let path = outside.path().join("fixture");
    std::fs::write(&path, "fixture").unwrap();
    let path = path.canonicalize().unwrap();
    let (controller, approvals, calls) =
        controller_with_approvals(workspace.path(), sessions.path()).await;
    let session = controller.info().session_id;
    let id = approvals.request_read_path(&session, "cat fixture", path.to_str().unwrap());
    std::fs::remove_file(&path).unwrap();
    std::os::unix::fs::symlink(workspace.path(), &path).unwrap();
    assert!(
        controller
            .approve_tool(&id)
            .await
            .unwrap_err()
            .contains("path changed")
    );
    assert!(!controller.config_path().exists());
    assert_eq!(*calls.lock().unwrap(), 0);
}

#[tokio::test]
async fn approve_always_excludes_the_exact_command_reloads_and_grants_the_request() {
    let workspace = tempfile::tempdir().expect("workspace");
    let sessions = tempfile::tempdir().expect("sessions");
    let (controller, approvals, calls) =
        controller_with_approvals(workspace.path(), sessions.path()).await;
    let session_id = controller.info().session_id;
    let id = approvals.request(&session_id, "lark-cli auth status");

    let message = controller.approve_bash_always(&id).await.expect("approve");

    assert_eq!(
        message,
        format!(
            "The user approved {id} and excluded 'lark-cli auth status' from the sandbox. Retry the same Bash command now."
        )
    );
    assert_eq!(*calls.lock().expect("calls"), 1);
    let written = std::fs::read_to_string(controller.config_path()).expect("read config");
    assert!(
        written.contains("excluded_commands = ['lark-cli auth status']"),
        "{written}"
    );
}

#[tokio::test]
async fn approve_always_refuses_commands_the_model_could_steer() {
    let workspace = tempfile::tempdir().expect("workspace");
    let sessions = tempfile::tempdir().expect("sessions");
    let (controller, approvals, calls) =
        controller_with_approvals(workspace.path(), sessions.path()).await;
    let session_id = controller.info().session_id;

    for command in [
        "git push; rm -rf x",
        "bash -c 'lark-cli x'",
        "sudo ls",
        "make test",
        "./build.sh",
    ] {
        let id = approvals.request(&session_id, command);
        let error = controller
            .approve_bash_always(&id)
            .await
            .expect_err(command);
        assert!(
            error.contains("cannot exclude") && error.contains("use /approve <id> instead"),
            "{error}"
        );
    }

    assert_eq!(*calls.lock().expect("calls"), 0);
    assert!(!controller.config_path().exists());
}

#[tokio::test]
async fn approve_always_refuses_an_unknown_id_and_a_busy_controller() {
    let workspace = tempfile::tempdir().expect("workspace");
    let sessions = tempfile::tempdir().expect("sessions");
    let (controller, approvals, _) =
        controller_with_approvals(workspace.path(), sessions.path()).await;
    let session_id = controller.info().session_id;
    let id = approvals.request(&session_id, "lark-cli auth status");

    assert_eq!(
        controller.approve_bash_always("approval-99").await,
        Err("approval request not found".to_string())
    );
    let _admission = controller.begin_operation().expect("admit turn");
    assert_eq!(
        controller.approve_bash_always(&id).await,
        Err(PROMPT_ACTIVE.to_string())
    );
    assert!(!controller.config_path().exists());
}
