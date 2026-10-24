//! Session creation, fork, restore and info tests.

use super::*;

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
    assert!(
        controller
            .permissions_command("")
            .unwrap()
            .starts_with("Approval mode: NEVER (reviewer disabled)\n")
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
    controller.permissions_command("auto").unwrap();
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
    assert!(
        controller
            .permissions_command("off")
            .unwrap()
            .starts_with("Approval mode: MANUAL\n")
    );
    assert_eq!(controller.info().approval_mode, ApprovalMode::Manual);
    // The mode name is the command's own value, and YOLO is one of them.
    let yolo_report = controller.permissions_command("yolo").unwrap();
    assert!(
        yolo_report.starts_with("Approval mode: YOLO\nApproval policy: "),
        "{yolo_report}"
    );
    assert_eq!(controller.info().approval_mode, ApprovalMode::Yolo);
    assert!(
        controller
            .permissions_command("")
            .unwrap()
            .starts_with("Approval mode: YOLO\n")
    );
    assert!(controller.permissions_command("invalid").is_err());
    controller.permissions_command("off").unwrap();
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
