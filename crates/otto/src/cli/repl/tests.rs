use otto_core::approval::ApprovalMode;
use std::path::Path;

use super::*;
use crate::cli::controller::SANDBOX_RELOAD_UNAVAILABLE;
use crate::fixtures::{self, controller, user};
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
mod rendering;
mod sandbox_commands;
mod session_commands;
mod wake_turns;

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
async fn background_notices_print_before_the_prompt() {
    let workspace = tempfile::tempdir().expect("workspace");
    let sessions = tempfile::tempdir().expect("sessions");
    let controller = fixtures::controller(workspace.path(), sessions.path()).await;
    controller.push_notice("memory: 1 candidate(s) queued for review (/memory review), 0 dropped");

    let (stdout, stderr, result) = session("/exit\n", &controller).await;

    assert!(result.is_ok(), "{result:?}");
    assert_eq!(stderr, "");
    let notice = stdout
        .find("memory: 1 candidate(s) queued")
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
    let controller = fixtures::controller(workspace.path(), sessions.path()).await;
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
    let builder = crate::fixtures::builder(workspace_root, session_root);
    let runtime = crate::fixtures::initial_runtime(&builder);
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

#[test]
fn help_text_hides_the_auto_alias() {
    assert!(!super::HELP.contains("/auto"));
}

#[test]
fn help_groups_and_command_order_follow_the_tui_catalog() {
    let headings: Vec<&str> = super::HELP
        .lines()
        .filter(|line| !line.is_empty() && !line.starts_with('/'))
        .collect();
    assert_eq!(
        headings,
        [
            "Help",
            "Setup",
            "During a task",
            "Approvals and sandbox",
            "Memory",
            "Tasks",
            "Sessions",
            "Exit"
        ]
    );
    let mut repl_order: Vec<&str> = Vec::new();
    for line in super::HELP.lines().filter(|line| line.starts_with('/')) {
        let name = line.split_whitespace().next().unwrap();
        if !repl_order.contains(&name) {
            repl_order.push(name);
        }
    }
    let tui_order = crate::tui::catalog_command_names();
    let tui_order_of_repl: Vec<&str> = tui_order
        .iter()
        .copied()
        .filter(|name| repl_order.contains(name))
        .collect();
    assert_eq!(repl_order, tui_order_of_repl);
    assert!(repl_order.len() > 15, "{repl_order:?}");
}
