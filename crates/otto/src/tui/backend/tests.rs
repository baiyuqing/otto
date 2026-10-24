use super::*;
use otto_core::approval::ApprovalMode;
#[tokio::test]
async fn repeated_reads_are_coalesced_and_keep_the_original_ticket() {
    let remote = Arc::new(Remote::offline(Vec::new()));
    let mut driver = Driver::new(Connection::Attach(remote));
    let ticket = Ticket {
        workspace: "/work".into(),
        session: "old".into(),
        surface: 7,
        generation: 4,
    };
    driver.submit(ticket.clone(), Request::Profiles);
    driver.submit(ticket.clone(), Request::Profiles);
    let completion = driver.next().await;
    assert_eq!(completion.ticket, ticket);
    assert!(completion.result.is_err());
    tokio::task::yield_now().await;
    assert!(driver.try_next().is_none());
}

#[tokio::test]
async fn stale_mutation_is_rejected_before_attempting_the_request() {
    let remote = Arc::new(Remote::offline(Vec::new()));
    let mut driver = Driver::new(Connection::Attach(remote));
    driver.submit(
        Ticket {
            workspace: "/work".into(),
            session: "old".into(),
            surface: 0,
            generation: 0,
        },
        Request::Command(Command::Rename("name".into())),
    );
    let completion = driver.next().await;
    assert!(
        matches!(completion.result, Err(message) if message == "session changed; operation was not applied")
    );
}

#[tokio::test]
async fn profile_switch_saves_the_default_and_reports_the_applied_profile() {
    let workspace = tempfile::tempdir().unwrap();
    let sessions = tempfile::tempdir().unwrap();
    let controller =
        Arc::new(crate::cli::fixture_tests::controller(workspace.path(), sessions.path()).await);
    std::fs::write(
        controller.config_path(),
        toml::to_string(&crate::cli::fixture_tests::config()).unwrap(),
    )
    .unwrap();
    let reply = local_command(controller.clone(), Command::SwitchProfile("beta".into()))
        .await
        .unwrap();
    assert!(
        matches!(reply, Reply::Operation { info, history: Some(_), message, .. } if info.profile == "beta" && message.contains("Set as default profile"))
    );
    let saved =
        otto_core::config::parse(&std::fs::read_to_string(controller.config_path()).unwrap())
            .unwrap();
    assert_eq!(saved.default_profile, "beta");
}

#[tokio::test]
async fn profile_switch_receipt_survives_a_later_thinking_failure() {
    let workspace = tempfile::tempdir().unwrap();
    let sessions = tempfile::tempdir().unwrap();
    let controller =
        Arc::new(crate::cli::fixture_tests::controller(workspace.path(), sessions.path()).await);
    let reply = local_command(
        controller,
        Command::SwitchProfileThinking {
            profile: "beta".into(),
            thinking: "invalid".into(),
            save: false,
        },
    )
    .await
    .unwrap();
    assert!(
        matches!(reply, Reply::Operation { info, history: Some(_), message, .. } if info.profile == "beta" && message.contains("/thinking:") && message.contains("switch was applied"))
    );
}

#[tokio::test]
async fn thinking_save_failure_keeps_the_applied_state_receipt() {
    let workspace = tempfile::tempdir().unwrap();
    let sessions = tempfile::tempdir().unwrap();
    let controller =
        Arc::new(crate::cli::fixture_tests::controller(workspace.path(), sessions.path()).await);
    std::fs::create_dir(controller.config_path()).unwrap();
    let reply = local_command(
        controller,
        Command::SetThinking {
            thinking: "high".into(),
            save: true,
        },
    )
    .await
    .unwrap();
    assert!(
        matches!(reply, Reply::Operation { info, message, .. } if info.thinking == "high" && message.contains("profile was not saved"))
    );
}

#[tokio::test]
async fn sandbox_read_grant_uses_canonical_path_and_returns_the_retry_prompt() {
    let workspace = tempfile::tempdir().unwrap();
    let sessions = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let alias = workspace.path().join("alias");
    std::os::unix::fs::symlink(outside.path(), &alias).unwrap();
    let canonical = outside
        .path()
        .canonicalize()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    let (controller, _, _) =
        crate::app::controller_with_approvals(workspace.path(), sessions.path()).await;
    let controller = Arc::new(controller);
    let info = controller.info();
    let ticket = Ticket {
        workspace: info.workspace,
        session: info.session_id,
        surface: 1,
        generation: 1,
    };
    let preview = local(
        controller.clone(),
        &ticket,
        Request::SandboxPath(alias.to_string_lossy().into_owned()),
    )
    .await
    .unwrap();
    assert!(matches!(preview, Reply::SandboxPath(path) if path == canonical));
    let reply = local_command(
        controller.clone(),
        Command::SandboxAllow(alias.to_string_lossy().into_owned()),
    )
    .await
    .unwrap();
    assert!(
        matches!(reply, Reply::Operation { prompt: Some(prompt), .. } if prompt.contains(&canonical) && prompt.contains("retry it now"))
    );
    let config = std::fs::read_to_string(controller.config_path()).unwrap();
    assert!(config.contains(&canonical));
    assert!(!config.contains(&alias.to_string_lossy().into_owned()));
}

#[tokio::test]
async fn sandbox_exclude_saves_reloads_and_returns_feedback_even_when_repeated() {
    let workspace = tempfile::tempdir().unwrap();
    let sessions = tempfile::tempdir().unwrap();
    let (controller, _, _) =
        crate::app::controller_with_approvals(workspace.path(), sessions.path()).await;
    let controller = Arc::new(controller);
    for _ in 0..2 {
        let reply = local_command(controller.clone(), Command::SandboxExclude("gh *".into()))
            .await
            .unwrap();
        let Reply::Operation {
            message, prompt, ..
        } = reply
        else {
            panic!("expected operation receipt");
        };
        assert!(message.contains("excluded command \"gh *\""));
        assert!(message.contains(&controller.config_path().display().to_string()));
        assert!(message.contains("reloaded immediately"));
        assert!(message.contains("existing entries are not duplicated"));
        assert!(message.contains("other approval requirements still apply"));
        assert!(prompt.unwrap().contains("retry it now"));
    }
    let config = crate::config::load_required(controller.config_path()).unwrap();
    assert_eq!(config.sandbox.excluded_commands, ["gh *"]);
}

#[tokio::test]
async fn sandbox_exclude_validation_failure_explains_recovery_without_writing() {
    let workspace = tempfile::tempdir().unwrap();
    let sessions = tempfile::tempdir().unwrap();
    let (controller, _, _) =
        crate::app::controller_with_approvals(workspace.path(), sessions.path()).await;
    let controller = Arc::new(controller);
    let before = std::fs::read(controller.config_path()).ok();
    let error = local_command(
        controller.clone(),
        Command::SandboxExclude("gh pr list; pwd".into()),
    )
    .await
    .unwrap_err();
    assert!(error.contains("invalid excluded command"));
    assert!(error.contains("use one simple command, or a program name followed by ' *'"));
    assert_eq!(std::fs::read(controller.config_path()).ok(), before);
}

#[tokio::test]
async fn sandbox_exclude_config_failure_keeps_the_file_and_has_no_retry_receipt() {
    let workspace = tempfile::tempdir().unwrap();
    let sessions = tempfile::tempdir().unwrap();
    let (controller, _, _) =
        crate::app::controller_with_approvals(workspace.path(), sessions.path()).await;
    let controller = Arc::new(controller);
    let invalid = "[sandbox\n";
    std::fs::write(controller.config_path(), invalid).unwrap();
    let error = local_command(controller.clone(), Command::SandboxExclude("gh *".into()))
        .await
        .unwrap_err();
    assert_eq!(error, "configuration is invalid; no changes made");
    assert_eq!(
        std::fs::read_to_string(controller.config_path()).unwrap(),
        invalid
    );
}

#[tokio::test]
async fn exact_approval_revision_cannot_be_rebound_to_current_state() {
    use crate::approval::Proposal;
    use otto_core::approval::{ApplicationState, Authority, Kind, State, Subject};
    let workspace = tempfile::tempdir().unwrap();
    let sessions = tempfile::tempdir().unwrap();
    let (controller, _, _) =
        crate::app::controller_with_approvals(workspace.path(), sessions.path()).await;
    let scope = controller.approval_scope();
    controller
        .approval_service()
        .set_mode(&scope, ApprovalMode::Manual)
        .unwrap();
    let request = controller
        .approval_service()
        .create(Proposal {
            kind: Kind::Confirmation,
            authority: Authority::Consent,
            scope: scope.clone(),
            subject: Subject {
                source: "backend-test".into(),
                id: "operation".into(),
                version: "1".into(),
                details: "review".into(),
                tool_call_id: String::new(),
            },
            title: "Review".into(),
            reason: String::new(),
            persistent: false,
            blocking: true,
            expires_at: None,
        })
        .unwrap();
    assert!(
        decide(
            &controller,
            &request.id,
            true,
            "",
            Some(request.revision + 1)
        )
        .is_err()
    );
    assert_eq!(
        controller
            .approval_service()
            .get(&request.id, &scope.workspace, Some(&scope.session_id))
            .unwrap()
            .state,
        State::Pending
    );
    decide(
        &controller,
        &request.id,
        true,
        "accepted",
        Some(request.revision),
    )
    .unwrap();
    let decided = controller
        .approval_service()
        .get(&request.id, &scope.workspace, Some(&scope.session_id))
        .unwrap();
    assert_eq!(decided.state, State::Approved);
    assert_eq!(decided.application, ApplicationState::NotRequired);
    assert!(decide(&controller, &request.id, false, "", Some(request.revision)).is_err());
    assert_eq!(
        controller
            .approval_service()
            .get(&request.id, &scope.workspace, Some(&scope.session_id))
            .unwrap()
            .state,
        State::Approved
    );
}

#[tokio::test]
async fn steering_commands_reach_the_controller_for_the_active_run() {
    let workspace = tempfile::tempdir().unwrap();
    let sessions = tempfile::tempdir().unwrap();
    let controller =
        Arc::new(crate::cli::fixture_tests::controller(workspace.path(), sessions.path()).await);

    // The read reaches `Controller::steering_messages`; an idle session has
    // no guidance, which is what the TUI paints as nothing queued.
    assert!(matches!(
        local_command(controller.clone(), Command::SteeringState)
            .await
            .unwrap(),
        Reply::Steering(messages) if messages.is_empty()
    ));
    // The withdrawal reaches its controller call and reports the runtime's own
    // reason.
    assert_eq!(
        local_command(controller.clone(), Command::WithdrawSteer(1))
            .await
            .unwrap_err(),
        "steering message is not queued for the active task"
    );
    // The interrupt reaches `Controller::interrupt_step`; an idle session has
    // no queued guidance, so the reply is that refusal's message.
    assert_eq!(
        local_command(controller, Command::InterruptStep)
            .await
            .unwrap_err(),
        "No queued guidance; Esc again cancels the turn."
    );
}

/// The attach arms route to the client rather than to the
/// `not available with --attach` fallback. `Remote::offline` has no socket,
/// so the steering route in the transport error is what proves the call was
/// attempted rather than rejected locally.
#[tokio::test]
async fn attach_steering_commands_route_to_the_client_transport() {
    let remote = Arc::new(Remote::offline(Vec::new()));
    let info = remote.info();
    let ticket = Ticket {
        workspace: info.workspace,
        session: info.session_id,
        surface: 0,
        generation: 1,
    };
    let read = remote_request(
        remote.clone(),
        &ticket,
        Request::Command(Command::SteeringState),
    )
    .await
    .unwrap_err();
    assert!(read.contains("/tasks/steer"), "{read}");
    let withdraw = remote_request(
        remote.clone(),
        &ticket,
        Request::Command(Command::WithdrawSteer(7)),
    )
    .await
    .unwrap_err();
    assert!(withdraw.contains("/tasks/steer/7"), "{withdraw}");
    assert!(withdraw.contains("outcome unknown"), "{withdraw}");
    let interrupt = remote_request(remote, &ticket, Request::Command(Command::InterruptStep))
        .await
        .unwrap_err();
    assert!(interrupt.contains("/tasks/steer/interrupt"), "{interrupt}");
}

/// The TUI reads the run's guidance right after admitting or withdrawing it,
/// so the read must not overtake the change it is meant to show. Both are
/// commands, which puts them on the driver's single write lane: the read
/// observes the state the admission left behind, and the display cannot flap
/// back to a stale list. The same driver serves the local and attached
/// backends, so this ordering holds for both.
#[tokio::test]
async fn a_guidance_read_follows_the_command_it_observes() {
    let workspace = tempfile::tempdir().unwrap();
    let sessions = tempfile::tempdir().unwrap();
    let controller =
        Arc::new(crate::cli::fixture_tests::controller(workspace.path(), sessions.path()).await);
    let info = controller.info();
    let ticket = Ticket {
        workspace: info.workspace,
        session: info.session_id,
        surface: 0,
        generation: 1,
    };
    let mut driver = Driver::new(Connection::Local(controller));
    // The idle session refuses the withdrawal, and the read that follows still
    // completes second: the failed change cannot be observed out of order.
    driver.submit(ticket.clone(), Request::Command(Command::WithdrawSteer(1)));
    driver.submit(ticket, Request::Command(Command::SteeringState));

    let first = driver.next().await;
    assert!(matches!(first.result, Err(ref error) if error.contains("not queued")));
    assert!(first.mutation);
    let second = driver.next().await;
    assert!(matches!(second.result, Ok(Reply::Steering(_))));
    assert!(
        second.mutation,
        "the read takes the same gate as the command it follows"
    );
}

#[test]
fn session_scope_rejects_a_previous_session_and_workspace() {
    let ticket = Ticket {
        workspace: "/work".into(),
        session: "first".into(),
        surface: 1,
        generation: 1,
    };
    let mut info = Info {
        workspace: "/work".into(),
        session_id: "first".into(),
        ..Info::default()
    };
    assert!(check_scope(&info, &ticket).is_ok());
    info.session_id = "second".into();
    assert!(check_scope(&info, &ticket).is_err());
    info.session_id = "first".into();
    info.workspace = "/other".into();
    assert!(check_scope(&info, &ticket).is_err());
}
