
use super::*;
use crate::cli::testutil::{
    FakeSandbox, builder, controller, initial_runtime, seatbelt_info, user,
};
use crate::runtime::info::SandboxNetwork;
use crate::runtime::sandbox_config::SandboxChange;

#[tokio::test]
async fn idempotent_create_persists_its_receipt_before_acknowledgement() {
    let workspace = tempfile::tempdir().expect("workspace");
    let sessions = tempfile::tempdir().expect("sessions");
    let builder = Arc::new(builder(workspace.path(), sessions.path()));
    let runtime = initial_runtime(&builder);
    let id = "0123456789abcdef0123456789abcdef".to_string();
    let controller =
        Controller::create_with_receipt(Arc::clone(&builder), &runtime, id.clone(), "create-real")
            .await
            .expect("create");
    let path = controller.current_session().path();
    assert!(std::path::Path::new(&path).is_file());
    assert!(
        controller
            .session_custom_data("otto.session_command")
            .unwrap()
            .contains("create-real")
    );
    controller.close().expect("close");

    let (reopened, _) = Controller::open(builder, std::path::Path::new(&path))
        .await
        .expect("open");
    assert_eq!(reopened.info().session_id, id);
    assert!(
        reopened
            .session_custom_data("otto.session_command")
            .unwrap()
            .contains("create-real")
    );
}

#[tokio::test]
async fn permissions_controls_preserve_sandbox_and_report_disabled_reviewer() {
    let workspace = tempfile::tempdir().unwrap();
    let sessions = tempfile::tempdir().unwrap();
    let controller = controller(workspace.path(), sessions.path()).await;
    let sandbox = controller.info().sandbox;
    assert!(
        controller
            .permissions_command("policy never")
            .unwrap()
            .contains("Approval policy: never")
    );
    assert!(
        controller
            .permissions_command("")
            .unwrap()
            .contains("Reviewer: auto_review (disabled)")
    );
    assert_eq!(
        controller.auto_mode_command("").unwrap(),
        "Approval mode: NEVER (reviewer disabled)"
    );
    controller.permissions_command("reviewer user").unwrap();
    let settings = controller.info().approval_settings;
    assert_eq!(settings.policy, otto_core::approval::ApprovalPolicy::Never);
    assert_eq!(
        settings.reviewer,
        otto_core::approval::ApprovalReviewer::User
    );
    assert_eq!(controller.info().sandbox, sandbox);
    assert!(controller.permissions_command("reviewer bypass").is_err());
    controller.auto_mode_command("auto").unwrap();
    let settings = controller.info().approval_settings;
    assert_eq!(
        settings.policy,
        otto_core::approval::ApprovalPolicy::OnRequest
    );
    assert_eq!(
        settings.reviewer,
        otto_core::approval::ApprovalReviewer::AutoReview
    );
    assert_eq!(controller.info().sandbox, sandbox);
}

#[tokio::test]
async fn session_mode_commands_keep_new_sessions_independent_and_resume_preferences() {
    let workspace = tempfile::tempdir().unwrap();
    let sessions = tempfile::tempdir().unwrap();
    let controller = controller(workspace.path(), sessions.path()).await;
    controller
        .current_session()
        .append(user("persist this session"))
        .await
        .unwrap();
    let first = controller.info();
    assert_eq!(first.approval_mode, ApprovalMode::Auto);
    assert_eq!(
        controller.auto_mode_command("off").unwrap(),
        "Approval mode: MANUAL"
    );
    assert_eq!(controller.info().approval_mode, ApprovalMode::Manual);
    // The mode name is the command's own value, and YOLO is one of them.
    assert_eq!(
        controller.auto_mode_command("yolo").unwrap(),
        "Approval mode: YOLO"
    );
    assert_eq!(controller.info().approval_mode, ApprovalMode::Yolo);
    assert_eq!(
        controller.auto_mode_command("").unwrap(),
        "Approval mode: YOLO"
    );
    assert!(controller.auto_mode_command("invalid").is_err());
    controller.auto_mode_command("off").unwrap();
    controller.new_session().await.unwrap();
    assert_eq!(controller.info().approval_mode, ApprovalMode::Auto);
    controller
        .resume_session(&first.session_path)
        .await
        .unwrap();
    assert_eq!(controller.info().approval_mode, ApprovalMode::Manual);
}

#[tokio::test]
async fn forking_switches_to_a_copy_before_the_chosen_prompt() {
    let workspace = tempfile::tempdir().expect("workspace");
    let sessions = tempfile::tempdir().expect("sessions");
    let controller = controller(workspace.path(), sessions.path()).await;
    assert!(controller.fork_points().expect("fork points").is_empty());
    assert_eq!(
        controller.fork_file(None).expect_err("no file yet"),
        ForkFailure::Failed(NOTHING_TO_FORK.to_string())
    );

    let reply = |text: &str| Message {
        role: otto_core::model::Role::Assistant,
        blocks: vec![otto_core::model::Block::text(text)],
        finish_reason: Some(otto_core::model::FinishReason::Stop),
        created_at: chrono::Utc::now(),
        ..Message::default()
    };
    let session = controller.current_session();
    session.append(user("first")).await.expect("append");
    session.append(reply("one")).await.expect("append");
    session.append(user("second")).await.expect("append");
    session.append(reply("two")).await.expect("append");
    let source = controller.info();
    let source_bytes = std::fs::read(&source.session_path).expect("read source");

    let points = controller.fork_points().expect("fork points");
    assert_eq!(
        points
            .iter()
            .map(|point| point.prompt.as_str())
            .collect::<Vec<_>>(),
        ["first", "second"]
    );
    assert!(matches!(
        controller.fork_file(Some("ffffffff")),
        Err(ForkFailure::InvalidPoint(_))
    ));

    let (result, draft) = controller
        .fork_session(Some(&points[1].entry_id))
        .await
        .expect("fork");
    assert_eq!(draft.as_deref(), Some("second"));
    let info = controller.info();
    assert_eq!(info.session_path, result.session_path);
    assert_ne!(info.session_id, source.session_id);
    assert_eq!(
        info.forked_from,
        Some(ForkedFrom {
            session_id: source.session_id.clone(),
            entry_id: Some(points[1].entry_id.clone()),
        })
    );
    assert_eq!(
        controller
            .history()
            .iter()
            .map(Message::text)
            .collect::<Vec<_>>(),
        ["first", "one"]
    );
    assert_eq!(
        std::fs::read(&source.session_path).expect("read source"),
        source_bytes
    );
    assert_eq!(source.forked_from, None);
}

#[tokio::test]
async fn a_saved_prompt_that_already_started_is_not_restored() {
    let workspace = tempfile::tempdir().expect("workspace");
    let sessions = tempfile::tempdir().expect("sessions");
    let controller = controller(workspace.path(), sessions.path()).await;
    // The journal follows the session file, which exists after a write.
    controller
        .current_session()
        .append(user("hello"))
        .await
        .unwrap();
    let queue = controller.turn_queue().expect("journal");
    for (input_id, text) in [("in-1", "started"), ("in-2", "waiting")] {
        queue
            .save(&turn_queue::QueuedPrompt {
                input_id: input_id.into(),
                turn_id: format!("turn-{input_id}"),
                text: text.into(),
                channel: None,
                origin: None,
                image_dropped: false,
            })
            .expect("save");
    }
    let mut started = user("started");
    started.input_id = "in-1".into();
    controller.current_session().append(started).await.unwrap();

    let restorable = controller.restorable_prompts();
    assert_eq!(
        restorable
            .iter()
            .map(|prompt| prompt.text.as_str())
            .collect::<Vec<_>>(),
        ["waiting"]
    );
    assert_eq!(
        queue.pending().len(),
        1,
        "the started prompt's entry is removed"
    );
}

#[tokio::test]
async fn info_reports_the_current_session_and_the_resolved_profile() {
    let workspace = tempfile::tempdir().expect("workspace");
    let sessions = tempfile::tempdir().expect("sessions");
    let controller = controller(workspace.path(), sessions.path()).await;

    let info = controller.info();
    assert_eq!(info.profile, "alpha");
    assert_eq!(info.model, "gpt-alpha");
    assert_eq!(info.provider, "openai-compatible");
    assert_eq!(info.session_id.len(), 32);
    // The file is lazy, so nothing exists until the first message.
    assert_eq!(info.session_path, "");
    assert_eq!(info.sandbox.summary(), controller.sandbox_info().summary());
    assert_eq!(info.workspace, controller.workspace());
}

#[tokio::test]
async fn the_context_report_is_withheld_without_dynamic_content() {
    let workspace = tempfile::tempdir().expect("workspace");
    let sessions = tempfile::tempdir().expect("sessions");
    let open = controller(workspace.path(), sessions.path()).await;
    let report = open.context_report().expect("report");
    assert_eq!(report.model, "gpt-alpha");
    assert!(!report.sections.is_empty());

    let builder = builder(workspace.path(), sessions.path());
    let runtime = initial_runtime(&builder);
    let session = builder.create_session(&runtime).expect("session");
    let runner = builder
        .build_runner(&session, &runtime)
        .await
        .expect("runner");
    let info = builder.runtime_info(&runtime);
    let withheld = Controller::new(builder, false, session, runner, info);
    assert!(withheld.context_report().is_none());
}

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
    use crate::cli::testutil::{FakeSandbox, seatbelt_info};
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

#[tokio::test]
async fn info_carries_the_context_window_and_the_usage_snapshot() {
    let workspace = tempfile::tempdir().expect("workspace");
    let sessions = tempfile::tempdir().expect("sessions");
    let controller = controller(workspace.path(), sessions.path()).await;

    let info = controller.info();
    assert!(!info.usage_present);
    assert_eq!(info.usage, Usage::default());
    assert_eq!(info.context_input_tokens, 0);
    assert!(!info.context_input_tokens_present);
}

#[tokio::test]
async fn new_session_replaces_the_session_and_keeps_the_runtime() {
    let workspace = tempfile::tempdir().expect("workspace");
    let sessions = tempfile::tempdir().expect("sessions");
    let controller = controller(workspace.path(), sessions.path()).await;
    let before = controller.info();

    controller.new_session().await.expect("new session");

    let after = controller.info();
    assert_ne!(after.session_id, before.session_id);
    assert_eq!(after.model, "gpt-alpha");
    assert_eq!(after.profile, "alpha");
}

#[tokio::test]
async fn new_session_keeps_runtime_changed_in_the_current_session() {
    let workspace = tempfile::tempdir().expect("workspace");
    let sessions = tempfile::tempdir().expect("sessions");
    let controller = controller(workspace.path(), sessions.path()).await;

    controller.switch_profile("beta").await.expect("switch");
    controller.set_thinking("high").await.expect("thinking");
    let before = controller.info();

    controller.new_session().await.expect("new session");

    let after = controller.info();
    assert_ne!(after.session_id, before.session_id);
    assert_eq!(after.profile, "beta");
    assert_eq!(after.model, "gpt-beta");
    assert_eq!(after.thinking, "high");
}

#[tokio::test]
async fn resume_keeps_process_selection_and_appends_without_rewriting_history() {
    use crate::runtime::builder::TEST_PROVIDERS;
    use crate::subagent::testsupport::{FakeProvider, assistant_text, match_any};
    struct ProviderGuard(String);
    impl Drop for ProviderGuard {
        fn drop(&mut self) {
            TEST_PROVIDERS
                .lock()
                .unwrap()
                .retain(|(home, _)| home != &self.0);
        }
    }
    let workspace = tempfile::tempdir().expect("workspace");
    let sessions = tempfile::tempdir().expect("sessions");
    let provider = FakeProvider::new();
    provider.add_route(match_any, vec![assistant_text("done", Usage::default())]);
    let home = workspace.path().to_string_lossy().into_owned();
    TEST_PROVIDERS
        .lock()
        .unwrap()
        .push((home.clone(), provider.clone()));
    let _provider_guard = ProviderGuard(home);
    let controller = controller(workspace.path(), sessions.path()).await;
    controller.set_thinking("low").await.unwrap();
    controller
        .prompt("hello", &mut |_| {}, &CancellationToken::new())
        .await
        .unwrap();
    let first = controller.info();
    controller.new_session().await.unwrap();
    controller.switch_profile("beta").await.unwrap();
    controller.set_thinking("high").await.unwrap();
    let selected = controller.info();
    let original = std::fs::read(&first.session_path).unwrap();

    controller
        .resume_session(&first.session_path)
        .await
        .unwrap();

    let resumed = controller.info();
    assert_eq!(resumed.session_id, first.session_id);
    assert_eq!(resumed.profile, selected.profile);
    assert_eq!(resumed.provider, selected.provider);
    assert_eq!(resumed.model, selected.model);
    assert_eq!(resumed.thinking, selected.thinking);
    assert_eq!(controller.history().len(), 2);
    let appended = std::fs::read(&first.session_path).unwrap();
    assert!(
        appended.starts_with(&original),
        "history must remain append-only"
    );
    assert!(
        appended.len() > original.len(),
        "active provenance is appended"
    );
    controller
        .prompt("continue", &mut |_| {}, &CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(provider.requests().last().unwrap().model, selected.model);
    let records = std::fs::read_to_string(&first.session_path).unwrap();
    let records: Vec<serde_json::Value> = records
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let assistant = records
        .iter()
        .rev()
        .find(|record| record["message"]["role"] == "assistant")
        .expect("persisted assistant response");
    assert_eq!(assistant["message"]["model"], selected.model);
    assert_eq!(assistant["message"]["provider"], selected.provider);
    let historical = records
        .iter()
        .find(|record| record["message"]["role"] == "assistant")
        .unwrap();
    assert_eq!(historical["message"]["model"], first.model);
    assert_eq!(historical["message"]["provider"], first.provider);
    let requests = provider.requests();
    assert_eq!(requests[0].model, first.model);
    assert_eq!(requests[0].thinking, "low");
    assert_eq!(requests[1].thinking, "high");
    controller.set_thinking("unset").await.unwrap();
    controller.new_session().await.unwrap();
    assert_eq!(controller.info().model, selected.model);
    assert_eq!(controller.info().thinking, "");
    controller
        .resume_session(&first.session_path)
        .await
        .unwrap();
    assert_eq!(controller.info().model, selected.model);
    assert_eq!(controller.info().thinking, "");
}

#[tokio::test]
async fn opening_a_session_uses_config_instead_of_historical_selection() {
    let workspace = tempfile::tempdir().expect("workspace");
    let sessions = tempfile::tempdir().expect("sessions");
    let controller = controller(workspace.path(), sessions.path()).await;
    controller
        .current_session()
        .append(user("hello"))
        .await
        .expect("append");

    controller.switch_profile("beta").await.expect("switch");
    controller.set_thinking("high").await.expect("thinking");
    let path = controller.info().session_path;
    assert!(!path.is_empty());
    controller.close().expect("close");

    let builder = builder(workspace.path(), sessions.path());
    let (reopened, warnings) = Controller::open(Arc::new(builder), Path::new(&path))
        .await
        .expect("open");

    assert!(warnings.is_empty(), "{warnings:?}");
    assert_eq!(reopened.info().profile, "alpha");
    assert_eq!(reopened.info().model, "gpt-alpha");
    assert_eq!(reopened.info().thinking, "");
}

#[tokio::test]
async fn switching_profile_switches_the_model() {
    let workspace = tempfile::tempdir().expect("workspace");
    let sessions = tempfile::tempdir().expect("sessions");
    let controller = controller(workspace.path(), sessions.path()).await;
    assert_eq!(controller.profiles(), vec!["alpha", "beta"]);

    let before = controller.info();

    controller.switch_profile("beta").await.expect("switch");

    let info = controller.info();
    assert_eq!(info.session_id, before.session_id);
    assert_eq!(info.profile, "beta");
    assert_eq!(info.model, "gpt-beta");
}

#[tokio::test]
async fn an_unknown_profile_leaves_the_session_in_place() {
    let workspace = tempfile::tempdir().expect("workspace");
    let sessions = tempfile::tempdir().expect("sessions");
    let controller = controller(workspace.path(), sessions.path()).await;
    let before = controller.info();

    let error = controller
        .switch_profile("missing")
        .await
        .expect_err("unknown profile");
    assert!(error.contains("not found"), "{error}");
    assert_eq!(controller.info().session_id, before.session_id);
}

#[tokio::test]
async fn renaming_records_the_name_on_the_session() {
    let workspace = tempfile::tempdir().expect("workspace");
    let sessions = tempfile::tempdir().expect("sessions");
    let controller = controller(workspace.path(), sessions.path()).await;

    controller
        .rename_session("  release notes  ")
        .expect("rename");

    assert_eq!(controller.info().session_name, "release notes");
    assert_eq!(
        controller.rename_session("   ").expect_err("blank"),
        INVALID_SESSION_NAME
    );
}

#[tokio::test]
async fn archiving_moves_the_file_and_starts_a_fresh_session() {
    let workspace = tempfile::tempdir().expect("workspace");
    let sessions = tempfile::tempdir().expect("sessions");
    let controller = controller(workspace.path(), sessions.path()).await;

    // The store is lazy: one appended message gives the session a file.
    controller
        .current_session()
        .append(user("hello"))
        .await
        .expect("append");
    let before = controller.info();
    assert!(!before.session_path.is_empty());

    let result = controller.archive_current_session().await.expect("archive");

    assert!(result.path.contains("archive"), "{}", result.path);
    assert!(Path::new(&result.path).exists());
    assert!(!Path::new(&before.session_path).exists());
    assert_ne!(controller.info().session_id, before.session_id);
}

#[tokio::test]
async fn archiving_cancels_the_session_timers() {
    let workspace = tempfile::tempdir().expect("workspace");
    let sessions = tempfile::tempdir().expect("sessions");
    let controller = controller(workspace.path(), sessions.path()).await;
    controller
        .current_session()
        .append(user("hello"))
        .await
        .expect("append");
    let reminders = controller.reminders().expect("timer registry");
    reminders
        .schedule(std::time::Duration::from_secs(60), "outstanding".into())
        .expect("schedule");

    controller.archive_current_session().await.expect("archive");

    assert!(
        reminders.list().is_empty(),
        "archiving must cancel outstanding timers"
    );
    assert!(controller.reminders().expect("registry").list().is_empty());
}

#[tokio::test]
async fn a_session_resumed_from_the_archive_keeps_its_timers_beside_it() {
    let workspace = tempfile::tempdir().expect("workspace");
    let sessions = tempfile::tempdir().expect("sessions");
    let controller = controller(workspace.path(), sessions.path()).await;
    controller
        .current_session()
        .append(user("hello"))
        .await
        .expect("append");
    let archived = controller.archive_current_session().await.expect("archive");
    // The same open sequence as `--resume PATH` in `cli::run`.
    let builder = builder(workspace.path(), sessions.path());
    let runtime = initial_runtime(&builder);
    let (store, _) = crate::session::Prepared::prepare(Path::new(&archived.path), None)
        .expect("prepare archived")
        .activate()
        .expect("activate archived");
    let session = SharedSession::new(Arc::new(store));
    let runner = builder
        .build_runner(&session, &runtime)
        .await
        .expect("runner");
    let info = builder.runtime_info(&runtime);
    let reopened = Controller::new(builder, true, session, runner, info);

    reopened
        .reminders()
        .expect("timer registry")
        .schedule(std::time::Duration::from_secs(60), "outstanding".into())
        .expect("schedule");

    assert!(
        Path::new(&archived.path)
            .with_extension("reminders.json")
            .exists(),
        "the timer sidecar is written beside the archived session file"
    );
}

#[tokio::test]
async fn a_session_with_no_file_cannot_be_archived() {
    let workspace = tempfile::tempdir().expect("workspace");
    let sessions = tempfile::tempdir().expect("sessions");
    let controller = controller(workspace.path(), sessions.path()).await;

    let error = controller
        .archive_current_session()
        .await
        .expect_err("no file");
    assert_eq!(error, PERSISTENCE_DISABLED);
}

#[tokio::test]
async fn the_default_profile_is_written_to_the_configuration_file() {
    let workspace = tempfile::tempdir().expect("workspace");
    let sessions = tempfile::tempdir().expect("sessions");
    let controller = controller(workspace.path(), sessions.path()).await;
    std::fs::write(
        controller.config_path(),
        "default_profile = \"alpha\"\n\n[profiles.alpha]\n\n[profiles.beta]\n",
    )
    .expect("write config");

    controller.set_default_profile("beta").expect("set default");

    let content = std::fs::read_to_string(controller.config_path()).expect("read config");
    assert!(content.contains("default_profile = \"beta\""), "{content}");
}

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

/// The `heartbeat` file layout `failover::lease` writes, reproduced here
/// to simulate a running holder without a live `Lease` of this process's
/// own (see [`crate::session::tests`]'s test-safety note for the same
/// pattern).
fn lease_heartbeat_bytes(epoch: u64, host: &str, pid: u32, released: bool, seq: u64) -> Vec<u8> {
    let json = serde_json::json!({
        "epoch": epoch,
        "host": host,
        "pid": pid,
        "released": released,
        "seq": seq,
    })
    .to_string();
    let mut bytes = vec![b' '; 512];
    bytes[..json.len()].copy_from_slice(json.as_bytes());
    bytes[511] = b'\n';
    bytes
}

/// A child transcript at `<parent without .jsonl>/<task_id>-child.jsonl`,
/// matching the file `subagent::runner::spawn` creates. Duplicated from
/// [`crate::runtime::builder::tests::child_store`]: both are
/// `#[cfg(test)]`-private to their own module, and the fixture is 12
/// lines.
fn child_store(parent: &Path, task_id: &str) -> (crate::subagent::runner::Transcript, String) {
    let name = format!("{task_id}-child");
    let store = crate::session::Store::create_child_lazy(
        parent,
        &name,
        otto_core::session::Header {
            id: "child".into(),
            workspace: parent.parent().expect("dir").to_string_lossy().into_owned(),
            provider: "openai-compatible".into(),
            model: "test-model".into(),
            created_at: chrono::Utc::now(),
            ..otto_core::session::Header::default()
        },
    )
    .expect("child store");
    let path = crate::session::Store::child_path(parent, &name);
    (Arc::new(store), path.to_string_lossy().into_owned())
}

/// Reads a lease's fixed-size heartbeat file and reports whether its
/// `released` field is `true`. Parses only that one field, matching
/// [`crate::failover::lease`]'s own doc comment on why the heartbeat is
/// read back in tests elsewhere in this crate.
fn heartbeat_released(session_path: &str) -> bool {
    let heartbeat = Path::new(session_path)
        .with_extension("lease")
        .join("heartbeat");
    let bytes = std::fs::read(&heartbeat).expect("read heartbeat");
    let text = String::from_utf8_lossy(&bytes);
    text.contains("\"released\":true")
}

/// S7: `Controller::migrate` obtains admission, cancels and records the
/// running and queued sub-agent tasks interrupted, notifies the parent,
/// closes MCP, requests a close, and releases the session's lease when
/// its admission is dropped.
///
/// Drives the sub-agent registry directly with `subagents.start(...).await`
/// rather than a real top-level "agent" tool call from a scripted
/// provider: `Controller::migrate` calls `Runner::migrate`, already
/// covered end to end by
/// `runtime::builder::tests::migrate_cancels_running_and_queued_subagent_tasks_and_notifies_the_parent`,
/// and adds only admission/close/lease-release around it, none of which
/// depends on how the tasks were started.
#[tokio::test]
async fn migrate_releases_the_lease_after_cancelling_subagent_tasks() {
    use crate::subagent::runner::StartRequest;
    use crate::subagent::tasks::{TaskStatus, Tasks};
    use crate::subagent::testsupport::{FakeProvider, assistant_text, match_any, wait_status};
    use otto_core::model::Usage;
    use otto_core::provider::Provider;

    let workspace = tempfile::tempdir().expect("workspace");
    let sessions = tempfile::tempdir().expect("sessions");
    let mut builder = builder(workspace.path(), sessions.path());
    builder.shared_mut().config.failover = otto_core::config::Failover {
        enabled: true,
        lease_seconds: 12,
    };
    let runtime = initial_runtime(&builder);
    let session = builder.create_session(&runtime).expect("create session");
    session.append(user("go")).await.expect("first append");
    let session_path = session.path();

    let tasks = Arc::new(Tasks::new());
    let child_provider = FakeProvider::new();
    child_provider.set_hook(Arc::new(|cancel, _request| {
        Box::pin(async move { cancel.cancelled().await })
    }));
    child_provider.add_route(match_any, vec![assistant_text("done", Usage::default())]);

    let mut child_config =
        crate::subagent::testsupport::test_config(&child_provider, &tasks, Vec::new());
    child_config.max_parallel = 1;
    let parent_path = PathBuf::from(&session_path);
    child_config.child_session = Some(Arc::new(move |task_id: &str| {
        Ok(Some(child_store(&parent_path, task_id)))
    }));
    let (subagents, _) = crate::subagent::runner::Runner::new(child_config).expect("valid config");
    let subagents = Arc::new(subagents);

    subagents
        .start(StartRequest {
            prompt: "first".into(),
            ..StartRequest::default()
        })
        .await
        .expect("start succeeds");
    wait_status(&tasks, "t1", TaskStatus::Running).await;
    subagents
        .start(StartRequest {
            prompt: "second".into(),
            ..StartRequest::default()
        })
        .await
        .expect("start succeeds");
    assert_eq!(
        tasks.get("t2").expect("t2 exists").status,
        TaskStatus::Queued,
        "t2 must stay queued behind t1 with max_parallel 1"
    );

    let mut runner = Runner::scripted(
        session.clone(),
        FakeProvider::new() as Arc<dyn Provider + Send + Sync>,
        Arc::clone(&tasks),
    );
    runner.subagents = Some(subagents);
    let info = builder.runtime_info(&runtime);
    let controller = Controller::new(builder, true, session.clone(), runner, info);

    let warnings = controller.migrate().await;
    assert!(warnings.is_empty(), "{warnings:?}");

    for task_id in ["t1", "t2"] {
        assert_eq!(
            tasks.get(task_id).expect("task exists").status,
            TaskStatus::Canceled
        );
    }
    let notifications = tasks.notifications().snapshot();
    assert_eq!(notifications.len(), 1, "{notifications:?}");
    assert!(notifications[0].text.contains("moved"));

    assert!(
        heartbeat_released(&session_path),
        "migrate must release the lease"
    );
}

/// S7: `Controller::migrate` on an idle lease-managed session with no
/// sub-agent tasks pushes no notification and still releases the lease.
#[tokio::test]
async fn migrate_on_an_idle_lease_managed_session_still_releases_the_lease() {
    use crate::subagent::tasks::Tasks;
    use otto_core::model::{FinishReason, Role};
    use otto_core::provider::Provider;

    let workspace = tempfile::tempdir().expect("workspace");
    let sessions = tempfile::tempdir().expect("sessions");
    let mut builder = builder(workspace.path(), sessions.path());
    builder.shared_mut().config.failover = otto_core::config::Failover {
        enabled: true,
        lease_seconds: 12,
    };
    let runtime = initial_runtime(&builder);
    let session = builder.create_session(&runtime).expect("create session");
    session
        .append(Message {
            role: Role::Assistant,
            blocks: vec![Block::text("done")],
            finish_reason: Some(FinishReason::Stop),
            created_at: chrono::Utc::now(),
            ..Message::default()
        })
        .await
        .expect("append");
    let session_path = session.path();

    let tasks = Arc::new(Tasks::new());
    let runner = Runner::scripted(
        session.clone(),
        crate::subagent::testsupport::FakeProvider::new() as Arc<dyn Provider + Send + Sync>,
        Arc::clone(&tasks),
    );
    let info = builder.runtime_info(&runtime);
    let controller = Controller::new(builder, true, session.clone(), runner, info);

    let warnings = controller.migrate().await;
    assert!(warnings.is_empty(), "{warnings:?}");
    assert!(tasks.notifications().is_empty());
    assert!(
        heartbeat_released(&session_path),
        "migrate must release an idle session's lease too"
    );
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

#[tokio::test]
async fn a_runner_without_a_registry_reports_no_tasks_and_no_wake() {
    let workspace = tempfile::tempdir().expect("workspace");
    let sessions = tempfile::tempdir().expect("sessions");
    let mut builder = builder(workspace.path(), sessions.path());
    builder.shared_mut().config.agents.enabled = Some(false);
    let runtime = initial_runtime(&builder);
    let session = builder.create_session(&runtime).expect("session");
    let runner = builder
        .build_runner(&session, &runtime)
        .await
        .expect("runner");
    let info = builder.runtime_info(&runtime);
    let controller = Controller::new(builder, true, session, runner, info);

    assert!(controller.tasks().is_none());
    assert!(controller.prepare_wake().expect("prepare").is_none());
    controller
        .runner()
        .expect("runner")
        .mailbox()
        .push(otto_core::agent::inbox::Notification {
            kind: Some(NotificationKind::UserMessage),
            text: "change course".to_string(),
            ..Default::default()
        });
    controller
        .prepare_wake()
        .expect("prepare")
        .expect("queued user input claims a wake")
        .cancel();
}

/// The registry is reachable through the view, and an empty inbox still
/// admits no wake turn.
#[tokio::test]
async fn a_runner_with_an_empty_registry_exposes_it_but_admits_no_wake() {
    let workspace = tempfile::tempdir().expect("workspace");
    let sessions = tempfile::tempdir().expect("sessions");
    let controller = controller(workspace.path(), sessions.path()).await;

    let tasks = controller.tasks().expect("registry");
    assert!(tasks.list().is_empty());
    assert_eq!(tasks.pending(), 0);
    assert!(controller.prepare_wake().expect("prepare").is_none());
    controller.begin_operation().expect("admit");
}

/// A pending notification claims a turn, and dropping the claim without
/// running it releases it.
#[tokio::test]
async fn a_pending_notification_claims_a_wake_turn() {
    let workspace = tempfile::tempdir().expect("workspace");
    let sessions = tempfile::tempdir().expect("sessions");
    let controller = controller(workspace.path(), sessions.path()).await;
    controller
        .current_runner()
        .expect("runner")
        .tasks
        .as_ref()
        .expect("registry")
        .notifications()
        .push(otto_core::agent::inbox::Notification {
            text: "[task-notification] task t1 succeeded".to_string(),
            ..otto_core::agent::inbox::Notification::default()
        });

    let wake = controller
        .prepare_wake()
        .expect("prepare")
        .expect("claimed");
    assert_eq!(
        controller.begin_operation().expect_err("busy"),
        PROMPT_ACTIVE
    );
    wake.cancel();
    controller.begin_operation().expect("admit");
}

fn propose_pending(controller: &Controller, key: &str) {
    let memory = controller.memory_wiring();
    memory
        .service
        .propose(&crate::memory::ProposeRequest {
            scope: controller.workspace_memory_scope().clone(),
            kind: "preference".into(),
            key: key.into(),
            text: key.into(),
            reason: "said so".into(),
            confidence: 0.5,
            source: crate::memory::Provenance {
                origin: Some(crate::memory::Origin::Model),
                session_id: controller.info().session_id,
                ..crate::memory::Provenance::default()
            },
            ..crate::memory::ProposeRequest::default()
        })
        .expect("propose");
}

#[tokio::test]
async fn each_session_open_queues_one_pending_memory_notice() {
    let workspace = tempfile::tempdir().expect("workspace");
    let sessions = tempfile::tempdir().expect("sessions");
    let store = tempfile::tempdir().expect("store");
    let controller = crate::cli::testutil::controller_with_memory(
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
    let reopened = crate::cli::testutil::controller_with_memory(
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
    crate::cli::testutil::seed_over_budget_standing(&path);
    let controller =
        crate::cli::testutil::controller_with_memory(workspace.path(), sessions.path(), &path)
            .await;
    assert_eq!(
        controller.take_notices(),
        vec![
            "1 standing preference is not recalled because the standing set is over its 2000-token budget. Forget one with /memory forget <id>; list them with /memory list --scope user."
        ]
    );
}
