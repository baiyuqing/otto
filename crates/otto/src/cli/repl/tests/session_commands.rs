//! Session-level slash command tests.

use super::*;

#[tokio::test]
async fn the_banner_reports_the_session_and_the_sandbox() {
    let workspace = tempfile::tempdir().expect("workspace");
    let sessions = tempfile::tempdir().expect("sessions");
    let controller = controller(workspace.path(), sessions.path()).await;

    let (stdout, stderr, result) = session("/exit\n", &controller).await;

    assert!(result.is_ok(), "{result:?}");
    assert!(stdout.starts_with("     ____  __  __"), "{stdout}");
    assert!(
        stdout.contains(&format!("Session: {}\n", controller.info().session_id)),
        "{stdout}"
    );
    assert!(
        stdout.contains(&format!(
            "Sandbox: {}\n",
            controller.sandbox_info().summary()
        )),
        "{stdout}"
    );
    assert!(stdout.contains(crate::tui::gutter::USER_MARK), "{stdout}");
    assert_eq!(stderr, "");
}

#[tokio::test]
async fn blank_lines_are_skipped_and_unknown_commands_are_reported() {
    let workspace = tempfile::tempdir().expect("workspace");
    let sessions = tempfile::tempdir().expect("sessions");
    let controller = controller(workspace.path(), sessions.path()).await;

    let (stdout, stderr, result) = session("\n   \n/unknown\n/exit\n", &controller).await;

    assert!(result.is_ok(), "{result:?}");
    assert_eq!(stderr, "unknown command: /unknown\n");
    assert_eq!(stdout.matches(crate::tui::gutter::USER_MARK).count(), 4);
}

#[tokio::test]
async fn approve_fails_closed_when_temporary_elevation_is_unavailable() {
    let workspace = tempfile::tempdir().expect("workspace");
    let sessions = tempfile::tempdir().expect("sessions");
    let controller = controller(workspace.path(), sessions.path()).await;

    let (_, _, result) = session("/approve approval-1\n", &controller).await;

    let error = result.expect_err("approval must fail");
    assert!(is_command_error(&error, "/approve"), "{error:?}");
    assert_eq!(error.to_string(), "temporary elevation is unavailable");
}

#[tokio::test]
async fn skill_commands_list_and_show_discovered_skills() {
    let workspace = tempfile::tempdir().expect("workspace");
    let sessions = tempfile::tempdir().expect("sessions");
    fixture_tests::write_skill(
        workspace.path(),
        "rust-helper",
        "Rust guidance",
        "Use small focused Rust changes.",
    );
    let controller = controller(workspace.path(), sessions.path()).await;

    let (stdout, stderr, result) = session(
        "/skill rust-helper\n/skill missing\n/skill\n/skills\n/exit\n",
        &controller,
    )
    .await;

    assert!(result.is_ok(), "{result:?}");
    assert!(
        stdout.contains("Available skills:\n- rust-helper"),
        "{stdout}"
    );
    assert!(!stdout.contains("- rust-helper: Rust guidance"), "{stdout}");
    assert!(stdout.contains("Skill: rust-helper"), "{stdout}");
    assert!(stdout.contains("Description: Rust guidance"), "{stdout}");
    assert!(
        stdout.contains("Use small focused Rust changes."),
        "{stdout}"
    );
    assert!(stderr.contains("unknown skill: missing"), "{stderr}");
    assert!(stderr.contains("unknown command: /skills"), "{stderr}");
}

#[tokio::test]
async fn init_submits_the_builtin_agent_task_and_rejects_arguments() {
    let workspace = tempfile::tempdir().expect("workspace");
    let sessions = tempfile::tempdir().expect("sessions");
    let provider = ScriptedProvider::new(|_| Ok("ok".to_string()));
    let controller = scripted_controller(
        workspace.path(),
        sessions.path(),
        Arc::clone(&provider),
        Arc::new(Tasks::new()),
    );

    let (_stdout, stderr, result) = session("/init\n/init now\n/exit\n", &controller).await;

    assert!(result.is_ok(), "{result:?}");
    assert_eq!(provider.calls(), 1);
    assert_eq!(stderr, "unknown command: /init now\n");
    let history = controller.history();
    assert!(history.iter().any(|message| {
        message.role == Role::User && message.text() == otto_core::agent::INIT_PROMPT
    }));
}

#[tokio::test]
async fn help_and_session_describe_the_ported_commands_and_the_session() {
    let workspace = tempfile::tempdir().expect("workspace");
    let sessions = tempfile::tempdir().expect("sessions");
    let controller = controller(workspace.path(), sessions.path()).await;
    controller
        .current_session()
        .append(user("hello"))
        .await
        .expect("append");
    let info = controller.info();

    let (stdout, stderr, result) = session("/help\n/session\n/exit\n", &controller).await;

    assert!(result.is_ok(), "{result:?}");
    assert_eq!(stderr, "");
    for expected in [
        "/help     show commands",
        "/init     create a repository AGENTS.md guide",
        "/exit     exit Otto",
        "/new      start a new session",
        "/session  show session details",
        "/rename <name> rename current session",
        "/archive  archive current session and start a new one",
        "/model [profile]",
        "/compact [focus] compact context",
        "/sandbox [reload]",
        "/approve <id>",
        "/sandbox exclude <entry> run matching commands outside the sandbox",
        "/approve <id> always",
        "/skill [name] show skills or one skill; /skill set <name> enabled|disabled change a skill state",
    ] {
        assert!(
            stdout.contains(expected),
            "{expected} missing from {stdout}"
        );
    }
    assert!(
            stdout.contains(&format!(
                "ID: {}\nPath: {}\nProvider: openai-compatible\nModel: gpt-alpha\nThinking: default\nSandbox: {}\n",
                info.session_id,
                info.session_path,
                info.sandbox.summary()
            )),
            "{stdout}"
        );
}

#[tokio::test]
async fn renaming_prints_the_new_name_and_an_empty_name_is_unknown() {
    let workspace = tempfile::tempdir().expect("workspace");
    let sessions = tempfile::tempdir().expect("sessions");
    let controller = controller(workspace.path(), sessions.path()).await;

    let (stdout, stderr, result) =
        session("/rename  release notes \n/rename   \n/exit\n", &controller).await;

    assert!(result.is_ok(), "{result:?}");
    assert!(
        stdout.contains("Renamed session: release notes\n"),
        "{stdout}"
    );
    assert_eq!(stderr, "unknown command: /rename\n");
    assert_eq!(controller.info().session_name, "release notes");
}

#[tokio::test]
async fn a_new_session_reports_a_different_id() {
    let workspace = tempfile::tempdir().expect("workspace");
    let sessions = tempfile::tempdir().expect("sessions");
    let controller = controller(workspace.path(), sessions.path()).await;
    let before = controller.info().session_id;

    let (stdout, _, result) = session("/new\n/exit\n", &controller).await;

    assert!(result.is_ok(), "{result:?}");
    let after = controller.info().session_id;
    assert_ne!(after, before);
    assert!(stdout.contains(&format!("Session: {after}\n")), "{stdout}");
}

#[tokio::test]
async fn clear_starts_a_fresh_session_like_new() {
    let workspace = tempfile::tempdir().expect("workspace");
    let sessions = tempfile::tempdir().expect("sessions");
    let controller = controller(workspace.path(), sessions.path()).await;
    let before = controller.info().session_id;

    let (stdout, stderr, result) = session("/clear\n/exit\n", &controller).await;

    assert!(result.is_ok(), "{result:?}");
    assert_eq!(stderr, "");
    let after = controller.info().session_id;
    assert_ne!(after, before);
    assert!(stdout.contains(&format!("Session: {after}\n")), "{stdout}");
}

#[tokio::test]
async fn archiving_prints_the_path_and_the_replacement_session() {
    let workspace = tempfile::tempdir().expect("workspace");
    let sessions = tempfile::tempdir().expect("sessions");
    let controller = controller(workspace.path(), sessions.path()).await;
    controller
        .current_session()
        .append(user("hello"))
        .await
        .expect("append");
    let before = controller.info();

    let (stdout, stderr, result) = session("/archive\n/exit\n", &controller).await;

    assert!(result.is_ok(), "{result:?}");
    assert_eq!(stderr, "");
    assert!(stdout.contains("Archived: "), "{stdout}");
    assert!(!Path::new(&before.session_path).exists());
    assert!(
        stdout.contains(&format!("Session: {}\n", controller.info().session_id)),
        "{stdout}"
    );
}

#[tokio::test]
async fn archiving_a_session_without_a_file_is_a_command_error() {
    let workspace = tempfile::tempdir().expect("workspace");
    let sessions = tempfile::tempdir().expect("sessions");
    let controller = controller(workspace.path(), sessions.path()).await;

    let (_, _, result) = session("/archive\n", &controller).await;

    let error = result.expect_err("archive without a file");
    assert!(is_command_error(&error, "/archive"), "{error:?}");
    assert!(error.to_string().contains("persistence"), "{error}");
}

#[tokio::test]
async fn model_lists_profiles_and_switches_to_one() {
    let workspace = tempfile::tempdir().expect("workspace");
    let sessions = tempfile::tempdir().expect("sessions");
    let controller = controller(workspace.path(), sessions.path()).await;
    std::fs::write(
        controller.config_path(),
        "default_profile = \"alpha\"\n\n[profiles.alpha]\n\n[profiles.beta]\n",
    )
    .expect("write config");

    let (stdout, stderr, result) = session("/model\n/model beta\n/exit\n", &controller).await;

    assert!(result.is_ok(), "{result:?}");
    assert_eq!(stderr, "");
    assert!(
            stdout
                .contains("Current: profile alpha (provider openai-compatible, model gpt-alpha, thinking default)\n"),
            "{stdout}"
        );
    assert!(stdout.contains("Profiles: alpha (openai-compatible/gpt-alpha, thinking default), beta (openai-compatible/gpt-beta, thinking default)\n"), "{stdout}");
    assert!(
            stdout.contains(
                "Switched to profile beta (provider openai-compatible, model gpt-beta, thinking default). Set as default profile.\n"
            ),
            "{stdout}"
        );
    assert_eq!(controller.info().model, "gpt-beta");
}

#[tokio::test]
async fn an_unknown_profile_is_a_command_error() {
    let workspace = tempfile::tempdir().expect("workspace");
    let sessions = tempfile::tempdir().expect("sessions");
    let controller = controller(workspace.path(), sessions.path()).await;

    let (_, _, result) = session("/model missing\n", &controller).await;

    let error = result.expect_err("unknown profile");
    assert!(is_command_error(&error, "/model"), "{error:?}");
}
