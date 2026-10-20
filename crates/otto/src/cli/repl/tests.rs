
use otto_core::approval::ApprovalMode;
use std::path::Path;

use super::*;
use crate::cli::controller::SANDBOX_RELOAD_UNAVAILABLE;
use crate::cli::testutil::{self, controller, user};
use crate::runtime::builder::Runner;
use crate::runtime::info::SandboxNetwork;
use crate::subagent::tasks::Tasks;
use otto_core::agent::inbox::Notification;
use otto_core::agent::{CompactionResult, Event};
use otto_core::model::{
    Block, BlockType, EffectCertainty, FinishReason, Message, OperationDisposition, OperationId,
    OperationOutcome, Role,
};
use otto_core::provider::{
    Provider, ProviderError, Request as ProviderRequest, Response as ProviderResponse, StreamEvent,
    StreamSink,
};
use otto_core::session::Session;
use otto_core::tool::ToolResult;
use std::io::{Cursor, Read};
use std::sync::{Arc, Condvar, Mutex};

fn operation_id() -> OperationId {
    OperationId::new("op_test").expect("operation id")
}

fn completed(disposition: OperationDisposition) -> OperationOutcome {
    OperationOutcome {
        disposition,
        effect_certainty: EffectCertainty::Completed,
        stop_reason: None,
    }
}

#[derive(Clone, Default)]
struct Buffer(Arc<Mutex<Vec<u8>>>);

impl Buffer {
    fn text(&self) -> String {
        String::from_utf8(self.0.lock().expect("buffer").clone()).expect("utf-8")
    }
}

impl Write for Buffer {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        self.0.lock().expect("buffer").extend_from_slice(data);
        Ok(data.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[tokio::test]
async fn notices_from_background_reflection_print_before_the_prompt() {
    let workspace = tempfile::tempdir().expect("workspace");
    let sessions = tempfile::tempdir().expect("sessions");
    let controller = testutil::controller(workspace.path(), sessions.path()).await;
    controller
        .push_notice("reflection: 1 candidate(s) queued for review (/memory review), 0 dropped");

    let (stdout, stderr, result) = session("/exit\n", &controller).await;

    assert!(result.is_ok(), "{result:?}");
    assert_eq!(stderr, "");
    let notice = stdout
        .find("reflection: 1 candidate(s) queued")
        .expect("the notice is printed");
    let prompt = stdout
        .find(crate::tui::gutter::USER_MARK)
        .expect("a prompt is printed");
    assert!(
        notice < prompt,
        "the notice comes before the prompt: {stdout}"
    );
    assert!(controller.take_notices().is_empty(), "it was drained");
}

async fn session(input: &str, controller: &Controller) -> (String, String, Result<(), Error>) {
    let stdout = Buffer::default();
    let stderr = Buffer::default();
    let mut repl = Repl::new(
        controller,
        Box::new(stdout.clone()),
        Box::new(stderr.clone()),
    );
    let result = repl
        .run(
            Cursor::new(input.as_bytes().to_vec()),
            &CancellationToken::new(),
        )
        .await;
    (stdout.text(), stderr.text(), result)
}

#[tokio::test]
async fn idle_repl_decides_a_restored_confirmation_with_feedback_without_a_retry() {
    let workspace = tempfile::tempdir().unwrap();
    let sessions = tempfile::tempdir().unwrap();
    let controller = testutil::controller(workspace.path(), sessions.path()).await;
    let service = controller.approval_service();
    service
        .set_mode(&controller.approval_scope(), ApprovalMode::Manual)
        .unwrap();
    let request = service
        .create(crate::approval::Proposal {
            kind: otto_core::approval::Kind::Confirmation,
            authority: otto_core::approval::Authority::Consent,
            scope: controller.approval_scope(),
            subject: otto_core::approval::Subject {
                tool_call_id: "plan-call".into(),
                source: "conversation".into(),
                id: "restored-operation".into(),
                version: "v1".into(),
                details: "Keep parsing narrow".into(),
            },
            title: "Review scope".into(),
            reason: "Before proceeding".into(),
            persistent: false,
            blocking: true,
            expires_at: None,
        })
        .unwrap();
    let (stdout, stderr, result) = session(
        &format!("/approve {} Keep the old syntax\n/exit\n", request.id),
        &controller,
    )
    .await;
    assert!(result.is_ok(), "{stderr}");
    let decided = service
        .get(
            &request.id,
            &request.scope.workspace,
            Some(&request.scope.session_id),
        )
        .unwrap();
    assert_eq!(decided.state, otto_core::approval::State::Approved);
    assert_eq!(decided.decision.unwrap().feedback, "Keep the old syntax");
    assert_eq!(
        decided.application,
        otto_core::approval::ApplicationState::NotRequired
    );
    assert!(
        stdout.contains("Approved; application: NotRequired"),
        "{stdout}"
    );
    assert!(
        !stdout.contains("[tool]"),
        "a confirmation cannot dispatch an operation"
    );
}

struct ExitThenBlock {
    first: bool,
    release: Arc<(Mutex<bool>, Condvar)>,
}

impl ExitThenBlock {
    fn new(release: Arc<(Mutex<bool>, Condvar)>) -> Self {
        Self {
            first: true,
            release,
        }
    }
}

impl Read for ExitThenBlock {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        if self.first {
            self.first = false;
            let line = b"/exit\n";
            buffer[..line.len()].copy_from_slice(line);
            return Ok(line.len());
        }
        let (lock, signal) = &*self.release;
        let mut released = lock.lock().expect("release lock");
        while !*released {
            released = signal.wait(released).expect("release lock");
        }
        Ok(0)
    }
}

#[test]
fn exit_does_not_wait_for_the_next_blocking_stdin_read_on_runtime_drop() {
    let release = Arc::new((Mutex::new(false), Condvar::new()));
    let thread_release = Arc::clone(&release);
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("runtime");
        runtime.block_on(async move {
            let workspace = tempfile::tempdir().expect("workspace");
            let sessions = tempfile::tempdir().expect("sessions");
            let controller = controller(workspace.path(), sessions.path()).await;
            let stdout = Buffer::default();
            let stderr = Buffer::default();
            let mut repl = Repl::new(&controller, Box::new(stdout), Box::new(stderr));
            let _ = ready_tx.send(());
            repl.run(
                std::io::BufReader::new(ExitThenBlock::new(thread_release)),
                &CancellationToken::new(),
            )
            .await
            .expect("/exit returns");
        });
        drop(runtime);
        let _ = done_tx.send(());
    });

    let release_reader = || {
        let (lock, signal) = &*release;
        *lock.lock().expect("release lock") = true;
        signal.notify_all();
    };
    let ready = ready_rx.recv_timeout(std::time::Duration::from_secs(30));
    if ready.is_err() {
        release_reader();
        match done_rx.recv_timeout(std::time::Duration::from_secs(1)) {
            Ok(()) | Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                worker.join().expect("runtime thread");
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => drop(worker),
        }
        assert!(ready.is_ok(), "REPL setup did not become ready in time");
        return;
    }

    let finished = done_rx.recv_timeout(std::time::Duration::from_millis(500));
    release_reader();
    worker.join().expect("runtime thread");
    assert!(
        finished.is_ok(),
        "runtime shutdown waited for a REPL stdin read that was still blocked"
    );
}

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
    testutil::write_skill(
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
    let (control, calls) = testutil::FakeSandbox::new(
        testutil::seatbelt_info(SandboxNetwork::Allowed),
        testutil::seatbelt_info(SandboxNetwork::Denied),
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
        "/sandbox exclude 'lark-cli *'\n/sandbox exclude lark-cli *\n/exit\n",
        &controller,
    )
    .await;

    assert!(result.is_ok(), "{result:?}");
    assert_eq!(stderr, "");
    assert_eq!(*calls.lock().expect("calls"), 2);
    let written = std::fs::read_to_string(workspace.path().join("config.toml")).expect("config");
    assert!(
        written.contains("excluded_commands = ['lark-cli *']"),
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

#[tokio::test]
async fn end_of_input_ends_the_loop() {
    let workspace = tempfile::tempdir().expect("workspace");
    let sessions = tempfile::tempdir().expect("sessions");
    let controller = controller(workspace.path(), sessions.path()).await;

    let (_, _, result) = session("", &controller).await;

    assert!(result.is_ok(), "{result:?}");
}

#[tokio::test]
async fn a_line_over_one_mebibyte_ends_the_loop_with_an_error() {
    let workspace = tempfile::tempdir().expect("workspace");
    let sessions = tempfile::tempdir().expect("sessions");
    let controller = controller(workspace.path(), sessions.path()).await;
    let line = "x".repeat(MAX_INPUT_BYTES + 1);

    let (_, _, result) = session(&format!("{line}\n"), &controller).await;

    let error = result.expect_err("oversized line");
    assert!(error.to_string().contains("input line too long"), "{error}");
}

#[test]
fn a_bash_approval_request_renders_the_approval_lines() {
    let mut stdout = Buffer::default();
    let mut stderr = Buffer::default();
    render_event(
        &mut stdout,
        &mut stderr,
        &Event::ToolCallFinished {
            operation_id: operation_id(),
            attempt: 1,
            tool_name: "bash".to_string(),
            tool_call_id: "call-1".to_string(),
            result: ToolResult {
                content: "approval required for unsandboxed bash execution.\n\
Approve in Otto: /approve approval-1\n\
Command: \"git push\"\n\
Justification: \"push branch\"\n\
The command did not run."
                    .to_string(),
                is_error: true,
                ..ToolResult::default()
            },
            outcome: OperationOutcome {
                disposition: OperationDisposition::Error,
                effect_certainty: EffectCertainty::NotStarted,
                stop_reason: None,
            },
        },
    );
    assert_eq!(
        stdout.text(),
        "[tool result] Did not run (operation op_test): approval required for unsandboxed bash execution.\n\
Approve in Otto: /approve approval-1\n\
Command: \"git push\"\n\
Justification: \"push branch\"\n"
    );
}

#[test]
fn unknown_tool_outcome_uses_stable_warning_with_operation_id() {
    let mut stdout = Buffer::default();
    let mut stderr = Buffer::default();
    render_event(
        &mut stdout,
        &mut stderr,
        &Event::ToolCallFinished {
            operation_id: operation_id(),
            attempt: 1,
            tool_name: "bash".to_string(),
            tool_call_id: "call-1".to_string(),
            result: ToolResult {
                content: "connection lost".to_string(),
                is_error: true,
                ..ToolResult::default()
            },
            outcome: OperationOutcome {
                disposition: OperationDisposition::Interrupted,
                effect_certainty: EffectCertainty::Unknown,
                stop_reason: None,
            },
        },
    );
    assert_eq!(
        stdout.text(),
        "[tool warning] Outcome unknown — check effects before retrying (operation op_test): connection lost\n"
    );
    assert_eq!(stderr.text(), "");
}

#[test]
fn interrupted_provider_output_is_marked_before_replacement_text() {
    let mut stdout = Buffer::default();
    let mut stderr = Buffer::default();
    for event in [
        Event::TextDelta {
            text: "partial".into(),
        },
        Event::ProviderRetry {
            operation_id: operation_id(),
            attempt: 2,
            max_attempts: 7,
            delay: std::time::Duration::from_secs(1),
            reason: "connection interrupted".into(),
            interrupted_output: true,
        },
        Event::TextDelta {
            text: "replacement".into(),
        },
    ] {
        assert!(!render_event(&mut stdout, &mut stderr, &event));
    }
    assert!(
        stdout
            .text()
            .starts_with("partial\n[Previous response interrupted")
    );
    assert!(stdout.text().ends_with("]\nreplacement"));
    assert!(stderr.text().contains("attempt 2/7"));
}

#[test]
fn provider_retry_renders_progress_without_suppressing_a_terminal_error() {
    let mut stdout = Buffer::default();
    let mut stderr = Buffer::default();

    let rendered_error = render_event(
        &mut stdout,
        &mut stderr,
        &Event::ProviderRetry {
            operation_id: operation_id(),
            attempt: 2,
            max_attempts: 4,
            delay: std::time::Duration::from_millis(1_250),
            reason: "HTTP 503\nresponse body omitted".to_string(),
            interrupted_output: false,
        },
    );

    assert!(!rendered_error);
    assert_eq!(stdout.text(), "");
    assert_eq!(
        stderr.text(),
        "[provider retry] attempt 2/4 in 1250ms: HTTP 503\n"
    );
}

#[test]
fn every_event_renders_to_the_expected_line() {
    let mut stdout = Buffer::default();
    let mut stderr = Buffer::default();
    let events = [
        Event::TextDelta {
            text: "done".to_string(),
        },
        Event::ToolCallStarted {
            operation_id: operation_id(),
            attempt: 1,
            tool_name: "read".to_string(),
            tool_call_id: "call-1".to_string(),
            arguments: String::new(),
        },
        Event::ToolCallFinished {
            operation_id: operation_id(),
            attempt: 1,
            tool_name: "read".to_string(),
            tool_call_id: "call-1".to_string(),
            result: ToolResult {
                content: "read README.md\nfull output must not render".to_string(),
                ..ToolResult::default()
            },
            outcome: completed(OperationDisposition::Succeeded),
        },
        Event::Notification {
            kind: None,
            task_id: "t-1".to_string(),
            text: "[task-notification] done".to_string(),
            usage: Default::default(),
            present: false,
            cached_input_tokens_present: false,
        },
    ];
    let mut rendered_error = false;
    for event in events {
        rendered_error |= render_event(&mut stdout, &mut stderr, &event);
    }
    assert!(!rendered_error);
    assert_eq!(
        stdout.text(),
        "done\n[tool] read (call-1)\n[tool result] Completed (operation op_test): read README.md\n\n[task-notification] done\n"
    );
    assert_eq!(stderr.text(), "");

    let mut stdout = Buffer::default();
    let mut stderr = Buffer::default();
    assert!(render_event(
        &mut stdout,
        &mut stderr,
        &Event::AgentError {
            message: "provider unavailable".to_string(),
        }
    ));
    assert_eq!(stderr.text(), "provider unavailable\n");
    assert_eq!(stdout.text(), "");
}

#[test]
fn compaction_lines_match_the_expected_text() {
    assert_eq!(
        compaction_line(&CompactionResult {
            noop: true,
            ..CompactionResult::default()
        }),
        "\n[context] no-op\n"
    );
    assert_eq!(
        compaction_line(&CompactionResult {
            tokens_before: Some(12_000),
            estimated_tokens_after: Some(800),
            ..CompactionResult::default()
        }),
        "\n[context] compacted 12k → 800 tokens\n"
    );
    assert_eq!(
        compaction_line(&CompactionResult {
            tokens_before: Some(999),
            ..CompactionResult::default()
        }),
        "\n[context] compacted 999 tokens\n"
    );
}

#[test]
fn commands_split_on_the_first_space() {
    assert_eq!(split_command("/model beta"), Some(("model", "beta")));
    assert_eq!(split_command("/model\tbeta  "), Some(("model", "beta")));
    assert_eq!(split_command("/exit"), Some(("exit", "")));
    assert_eq!(split_command("/"), None);
    assert_eq!(split_command("model"), None);
}
// ---- sub-agent wake turns ----

/// One provider call per turn. The `cli` tests have no backend seam, so the
/// script sits one layer down, at the provider, the way `server`'s tests
/// script theirs. `reply` gets the 1-based call index.
struct ScriptedProvider {
    reply: Box<dyn Fn(usize) -> Result<String, String> + Send + Sync>,
    /// The role of each call's last request message: `User` for a prompt
    /// turn, `Context` for a wake turn's delivered notification.
    roles: Mutex<Vec<Role>>,
    calls: tokio::sync::watch::Sender<usize>,
}

impl ScriptedProvider {
    fn new(reply: impl Fn(usize) -> Result<String, String> + Send + Sync + 'static) -> Arc<Self> {
        Arc::new(Self {
            reply: Box::new(reply),
            roles: Mutex::new(Vec::new()),
            calls: tokio::sync::watch::channel(0).0,
        })
    }

    fn roles(&self) -> Vec<Role> {
        self.roles.lock().expect("roles").clone()
    }

    fn calls(&self) -> usize {
        *self.calls.borrow()
    }

    /// Resolves once `count` calls have started.
    async fn wait_calls(&self, count: usize) {
        let mut receiver = self.calls.subscribe();
        receiver
            .wait_for(|seen| *seen >= count)
            .await
            .expect("sender");
    }
}

#[async_trait::async_trait]
impl Provider for ScriptedProvider {
    async fn complete(
        &self,
        request: &ProviderRequest,
        emit: StreamSink<'_>,
        _control: &dyn otto_core::operation::OperationControl,
    ) -> otto_core::provider::ProviderSettlement {
        let call = {
            let mut roles = self.roles.lock().expect("roles");
            roles.push(
                request
                    .messages
                    .last()
                    .map(|message| message.role.clone())
                    .unwrap_or(Role::User),
            );
            roles.len()
        };
        self.calls.send_modify(|seen| *seen = call);
        let text = match (self.reply)(call) {
            Ok(text) => text,
            Err(error) => {
                return otto_core::provider::ProviderSettlement::failed(
                    ProviderError::Other(error),
                    0,
                    otto_core::model::EffectCertainty::NotStarted,
                );
            }
        };
        emit(StreamEvent::TextDelta { text: text.clone() });
        otto_core::provider::ProviderSettlement::succeeded(
            ProviderResponse {
                message: Message {
                    role: Role::Assistant,
                    finish_reason: Some(FinishReason::Stop),
                    blocks: vec![Block {
                        block_type: BlockType::Text,
                        text,
                        ..Block::default()
                    }],
                    ..Message::default()
                },
                cached_input_tokens_present: false,
                remote_compaction: None,
            },
            0,
        )
    }
}

/// Stdin that stays open until the test drops `sender`.
struct Pipe(std::sync::mpsc::Receiver<()>);

impl Read for Pipe {
    fn read(&mut self, _buffer: &mut [u8]) -> std::io::Result<usize> {
        // The sender is never used to send: dropping it reports EOF.
        let _ = self.0.recv();
        Ok(0)
    }
}

fn scripted_controller(
    workspace_root: &Path,
    session_root: &Path,
    provider: Arc<ScriptedProvider>,
    tasks: Arc<Tasks>,
) -> Controller {
    let builder = crate::cli::testutil::builder(workspace_root, session_root);
    let runtime = crate::cli::testutil::initial_runtime(&builder);
    let session = builder.create_session(&runtime).expect("session");
    let info = builder.runtime_info(&runtime);
    let runner = Runner::scripted(
        session.clone(),
        provider as Arc<dyn Provider + Send + Sync>,
        tasks,
    );
    Controller::new(builder, true, session, runner, info)
}

fn repl_buffers<'a>(controller: &'a Controller) -> (Repl<'a>, Buffer, Buffer) {
    let stdout = Buffer::default();
    let stderr = Buffer::default();
    let repl = Repl::new(
        controller,
        Box::new(stdout.clone()),
        Box::new(stderr.clone()),
    );
    (repl, stdout, stderr)
}

#[tokio::test]
async fn a_notification_renders_during_a_one_shot_turn() {
    let workspace = tempfile::tempdir().expect("workspace");
    let sessions = tempfile::tempdir().expect("sessions");
    let tasks = Arc::new(Tasks::new());
    tasks.notifications().push(Notification {
        task_id: "t1".to_string(),
        text: "[task-notification] task t1 (explorer) succeeded · 1s · 1 tool call\nall good"
            .to_string(),
        ..Notification::default()
    });
    let provider = ScriptedProvider::new(|_| Ok("ok".to_string()));
    let controller = scripted_controller(
        workspace.path(),
        sessions.path(),
        Arc::clone(&provider),
        Arc::clone(&tasks),
    );
    let (mut repl, stdout, _stderr) = repl_buffers(&controller);

    repl.run_once("go", &CancellationToken::new())
        .await
        .expect("run once");

    assert!(
        stdout.text().contains(
            "\n[task-notification] task t1 (explorer) succeeded · 1s · 1 tool call\nall good\n"
        ),
        "{}",
        stdout.text()
    );
}

#[tokio::test]
async fn the_loop_wakes_only_when_a_notification_is_pending() {
    let workspace = tempfile::tempdir().expect("workspace");
    let sessions = tempfile::tempdir().expect("sessions");
    let tasks = Arc::new(Tasks::new());
    let provider = ScriptedProvider::new(|_| Ok("woke up".to_string()));
    let controller = scripted_controller(
        workspace.path(),
        sessions.path(),
        Arc::clone(&provider),
        Arc::clone(&tasks),
    );
    let (mut repl, stdout, _stderr) = repl_buffers(&controller);
    let (sender, receiver) = std::sync::mpsc::channel::<()>();
    let cancel = CancellationToken::new();

    let driver = async {
        // A registry signal with nothing pending must not wake a turn.
        tasks
            .add(crate::subagent::tasks::Task::default(), None, None)
            .expect("add");
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(
            provider.calls(),
            0,
            "a wake turn ran before any notification was pending"
        );
        tasks.notifications().push(Notification {
            task_id: "t1".to_string(),
            text: "[task-notification] task t1 succeeded".to_string(),
            ..Notification::default()
        });
        tokio::time::timeout(std::time::Duration::from_secs(2), provider.wait_calls(1))
            .await
            .expect("the pending notification did not trigger a wake turn");
        drop(sender);
    };
    let (result, ()) = tokio::join!(
        repl.run(std::io::BufReader::new(Pipe(receiver)), &cancel),
        driver
    );

    result.expect("run");
    assert_eq!(
        provider.roles(),
        vec![Role::Context],
        "want exactly one wake turn, whose last request message is the notification"
    );
    assert!(stdout.text().contains("\nwoke up"), "{}", stdout.text());
}

#[tokio::test]
async fn a_one_shot_run_waits_for_a_running_task_and_then_wakes() {
    let workspace = tempfile::tempdir().expect("workspace");
    let sessions = tempfile::tempdir().expect("sessions");
    let tasks = Arc::new(Tasks::new());
    let added = tasks
        .add(crate::subagent::tasks::Task::default(), None, None)
        .expect("add");
    tasks.mark_running(&added.id, chrono::Utc::now());
    let child = Arc::clone(&tasks);
    let id = added.id.clone();
    let provider = ScriptedProvider::new(move |call| {
        if call > 1 {
            return Ok("reported".to_string());
        }
        let child = Arc::clone(&child);
        let id = id.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            // Pushed before the final update, as the sub-agent runner
            // does: the wait unblocks on the update and the drain must
            // then find the notification already pending.
            child.notifications().push(Notification {
                task_id: id.clone(),
                text: format!("[task-notification] task {id} succeeded\nchild done"),
                ..Notification::default()
            });
            child.finish(
                &id,
                crate::subagent::tasks::TaskStatus::Succeeded,
                chrono::Utc::now(),
                "child done",
                "",
            );
        });
        Ok("started".to_string())
    });
    let controller = scripted_controller(
        workspace.path(),
        sessions.path(),
        Arc::clone(&provider),
        Arc::clone(&tasks),
    );
    let (mut repl, stdout, _stderr) = repl_buffers(&controller);

    repl.run_once("go", &CancellationToken::new())
        .await
        .expect("run once");

    assert_eq!(
        provider.roles(),
        vec![Role::User, Role::Context],
        "want the initial turn and one wake turn"
    );
    assert!(stdout.text().contains("reported"), "{}", stdout.text());
}

#[tokio::test]
async fn a_one_shot_run_returns_the_wake_turns_error() {
    let workspace = tempfile::tempdir().expect("workspace");
    let sessions = tempfile::tempdir().expect("sessions");
    let tasks = Arc::new(Tasks::new());
    let pushed = Arc::clone(&tasks);
    let provider = ScriptedProvider::new(move |call| {
        if call > 1 {
            return Err("wake provider failed".to_string());
        }
        // Pushed inside the first turn, after the agent drained the
        // inbox, so the notification is still pending when it returns.
        pushed.notifications().push(Notification {
            task_id: "t1".to_string(),
            kind: Some(otto_core::agent::inbox::NotificationKind::TaskReport),
            text: "progress".to_string(),
            ..Notification::default()
        });
        Ok("ok".to_string())
    });
    let controller = scripted_controller(
        workspace.path(),
        sessions.path(),
        Arc::clone(&provider),
        Arc::clone(&tasks),
    );
    let (mut repl, _stdout, _stderr) = repl_buffers(&controller);

    let error = repl
        .run_once("inspect", &CancellationToken::new())
        .await
        .expect_err("the wake turn's error");

    assert!(
        error.to_string().contains("wake provider failed"),
        "{error}"
    );
}
