//! Shared session lifecycle and the capability contracts every frontend uses.
//!
//! The REPL, `otto serve`, and a later TUI all drive one [`Controller`]: it
//! owns the current session, the runner built for it, and the admission rule
//! that lets exactly one operation touch them at a time.
//!
//! Concurrency: one `Mutex<State>` guards everything. Admission is an RAII
//! guard ([`Admission`]), so a dropped future releases the claim. No lock is
//! held across an `await`.
//!
//! Close: [`Controller::request_close`] never blocks and never closes, which is
//! what a callback running inside an operation needs. [`Controller::close`]
//! completes the close, waiting on a condition variable when an operation is
//! still in flight. It blocks a thread, so an async caller cancels the
//! in-flight work and awaits it before calling `close`.
//!
//! Errors: every failure is already-redacted text, matching
//! [`Builder::redact_error`]. Callers that must distinguish a cause compare
//! against the constants below, the way `cli::run` already compares
//! [`SESSION_OPERATION_UNAVAILABLE`].

pub mod approval;
pub mod approval_control;
pub mod auto_reflection;
pub mod channel;
pub(crate) mod management;
mod requested_reflection;
pub(crate) mod task_control;
pub(crate) mod turn_queue;

/// Fixed, content-free frontend notice for a successful memory proposal.
pub const MEMORY_REVIEW_AVAILABLE: &str = "Memory review available. Run /memory review.";
pub mod sandbox;
pub(crate) mod scheduled;
pub mod tasks;
pub mod wake;

use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::Duration;

use otto_core::agent::inbox::{Notification, NotificationKind};
use otto_core::agent::{AgentError, CompactionResult, EventSink};
use otto_core::config::resolve::Runtime;
use otto_core::model::{Block, Message, Usage};
use otto_core::session::{ListResult, RuntimeMetadata, Session};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use crate::runtime::builder::{Builder, Runner, RuntimeInfo, SharedSession};
use crate::runtime::info::SandboxInfo;
use crate::runtime::sandbox_config::{self, SandboxChange};
use crate::session::{self as sessionfs, ArchiveResult, MAX_LIST_SESSIONS};
use crate::tool::remind::Reminders;
pub use channel::Channel;

pub use approval::{ApprovalDecision, ApprovalRequest, Step, Stop};
use otto_core::approval::ApprovalMode;
pub use sandbox::SandboxControl;
pub use tasks::{Task, TaskStatus, TaskView};
pub use wake::WakeOperation;

pub const PERSISTENCE_DISABLED: &str = "session persistence is disabled";
pub const PROFILE_SWITCH_UNAVAILABLE: &str = "profile switching is not available";
pub const SANDBOX_RELOAD_UNAVAILABLE: &str = "sandbox reload is not available";
pub const SESSION_OPERATION_UNAVAILABLE: &str = "session operation is unavailable";
/// The server answers it with 409 `turn_active`.
pub const PROMPT_ACTIVE: &str = "a prompt is already active";
pub const NOTHING_TO_FORK: &str = "the session has no history to fork yet";
pub const CLOSED: &str = "controller is closed";
pub const SESSION_RENAME_UNAVAILABLE: &str = "session rename is unavailable";
pub const INVALID_SESSION_NAME: &str = "session is invalid: session name is required";

/// The deterministic result of submitting ordinary human text while a session
/// may have an active turn. It never silently turns steering into a queued
/// future turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ActiveInputAdmission {
    /// The text is bound to the current run and will arrive at its next safe
    /// provider boundary.
    Steered,
    /// There is no active run. The caller may explicitly start a new turn.
    Idle,
    /// A pending approval owns natural-language input until it is resolved.
    ApprovalPending,
}

/// One configured profile row a frontend can display.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProfileSummary {
    pub name: String,
    pub provider: String,
    pub model: String,
    pub thinking: String,
}

/// What a frontend may display.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Info {
    pub session_id: String,
    pub session_name: String,
    pub session_path: String,
    pub workspace: String,
    pub provider: String,
    pub profile: String,
    pub model: String,
    pub thinking: String,
    pub usage: Usage,
    pub usage_present: bool,
    pub context_window: i64,
    pub context_input_tokens: i64,
    pub context_input_tokens_present: bool,
    pub context_input_tokens_pending: bool,
    pub sandbox: SandboxInfo,
    /// The session's approval policy: MANUAL, AUTO or YOLO.
    pub approval_mode: ApprovalMode,
    pub approval_settings: crate::approval::ApprovalSettings,
    /// Where this session was forked from, when it is a fork.
    pub forked_from: Option<ForkedFrom>,
    /// The session plan in force; empty when there is none.
    pub plan: Vec<otto_core::session::PlanStep>,
}

/// The session and prompt a fork was taken from.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ForkedFrom {
    pub session_id: String,
    /// The user message the fork stops before; absent for a whole fork.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entry_id: Option<String>,
}

impl From<otto_core::session::ForkRecord> for ForkedFrom {
    fn from(record: otto_core::session::ForkRecord) -> Self {
        Self {
            session_id: record.source_session_id,
            entry_id: record.source_entry_id,
        }
    }
}

/// Why a fork was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ForkFailure {
    /// The id names no user prompt that starts a turn on the history.
    InvalidPoint(String),
    Failed(String),
}

impl std::fmt::Display for ForkFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidPoint(message) | Self::Failed(message) => f.write_str(message),
        }
    }
}

/// What a completed session replacement reports.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ResumeResult {
    pub session_path: String,
    pub warnings: Vec<String>,
}

pub fn permissions_update(
    args: &str,
    current: crate::approval::ApprovalSettings,
) -> Result<
    Option<(
        crate::approval::ApprovalPolicy,
        crate::approval::ApprovalReviewer,
    )>,
    String,
> {
    use crate::approval::{ApprovalPolicy as Policy, ApprovalReviewer as Reviewer};
    match args.split_whitespace().collect::<Vec<_>>().as_slice() {
        [] => Ok(None),
        ["policy", "on_request"] => Ok(Some((Policy::OnRequest, current.reviewer))),
        ["policy", "never"] => Ok(Some((Policy::Never, current.reviewer))),
        ["reviewer", "user"] => Ok(Some((current.policy, Reviewer::User))),
        ["reviewer", "auto_review"] => Ok(Some((current.policy, Reviewer::AutoReview))),
        _ => {
            Err("usage: /permissions [policy on_request|never | reviewer user|auto_review]".into())
        }
    }
}

pub fn format_permissions(settings: crate::approval::ApprovalSettings) -> String {
    use crate::approval::{ApprovalPolicy, ApprovalReviewer};
    let policy = match settings.policy {
        ApprovalPolicy::OnRequest => "on_request",
        ApprovalPolicy::Never => "never",
    };
    let reviewer = match settings.reviewer {
        ApprovalReviewer::User => "user",
        ApprovalReviewer::AutoReview => "auto_review",
        ApprovalReviewer::Bypass => "bypass",
    };
    format!(
        "Approval policy: {policy}\nReviewer: {reviewer}{}\nRevision: {}",
        if settings.policy == ApprovalPolicy::Never {
            " (disabled)"
        } else {
            ""
        },
        settings.revision
    )
}

pub fn parse_auto_mode(args: &str) -> Result<Option<ApprovalMode>, String> {
    match args.trim() {
        "" => Ok(None),
        "on" => Ok(Some(ApprovalMode::Auto)),
        "off" => Ok(Some(ApprovalMode::Manual)),
        "manual" => Ok(Some(ApprovalMode::Manual)),
        "auto" => Ok(Some(ApprovalMode::Auto)),
        "yolo" => Ok(Some(ApprovalMode::Yolo)),
        _ => Err("usage: /auto [manual|auto|yolo]".into()),
    }
}

fn validate_thinking_arg(thinking: &str) -> Result<(), String> {
    if matches!(
        thinking,
        "" | "unset" | "default" | "low" | "medium" | "high" | "xhigh" | "max"
    ) {
        return Ok(());
    }
    Err("thinking must be one of unset, low, medium, high, xhigh, max".to_string())
}

fn normalize_thinking_arg(thinking: &str) -> String {
    match thinking {
        "unset" | "default" => String::new(),
        other => other.to_string(),
    }
}

/// The session, runner and resolved runtime currently in force.
struct Current {
    session: SharedSession,
    runner: Arc<Runner>,
    info: RuntimeInfo,
    /// The canonical workspace recorded in the session header.
    workspace: String,
}

impl Current {
    fn build(session: SharedSession, runner: Arc<Runner>, info: RuntimeInfo) -> Self {
        let workspace = canonical_session_path(&session.header().workspace);
        Self {
            session,
            runner,
            info,
            workspace,
        }
    }

    /// The session file path as the store reports it. A lazily created store
    /// has none until its first append, so this is read live rather than
    /// captured; `Info.session_path` and every path comparison use it.
    fn path(&self) -> String {
        self.session.path()
    }

    /// The same path, symlink-resolved, for comparing two references to one
    /// file.
    fn canonical_path(&self) -> String {
        canonical_session_path(&self.session.path())
    }

    fn discard(self) -> Result<(), String> {
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            let runner = self.runner.clone();
            handle.spawn(async move {
                runner.end_session_hooks().await;
            });
        }
        self.runner.close();
        self.session.close()
    }
}

#[derive(Default)]
struct State {
    current: Option<Current>,
    /// An operation holds admission. Covers both the active and the replacing
    /// state, because every caller treats them identically.
    busy: bool,
    /// Bumped on every admission so a stale [`Admission`] guard is a no-op
    /// after [`Controller::request_close`] releases an unstarted wake.
    generation: u64,
    /// A wake claim that has not run yet. `request_close` releases it instead
    /// of waiting.
    wake_unstarted: bool,
    closed: bool,
    close_pending: bool,
    close_in_progress: bool,
    close_done: bool,
    close_err: Option<String>,
}

pub struct Controller {
    builder: Arc<Builder>,
    /// False when the redaction boundary is closed. Every operation that would
    /// persist or display provider identity is then refused.
    dynamic_content: bool,
    sandbox: Option<Arc<dyn SandboxControl>>,
    state: Mutex<State>,
    close_signal: Condvar,
    /// Lines from background reflection for a frontend to show.
    notices: Arc<auto_reflection::Notices>,
    /// Model-written texts of finished wake turns, for ACP clients.
    wake_replies: auto_reflection::Notices,
    /// Whether a background reflection run is in flight; one at a time.
    auto_running: Arc<std::sync::atomic::AtomicBool>,
    /// Cancelled by `request_close`, which stops a background run.
    auto_cancel: CancellationToken,
    approval_control: tokio::sync::Mutex<()>,
    /// Set by [`Controller::push_scheduled`]; the TUI rings the terminal bell
    /// once per [`Controller::take_bell`].
    bell: std::sync::atomic::AtomicBool,
    /// Reports this session's lifecycle to the Herdr pane that started the
    /// process. `None` outside Herdr and for every frontend that is not the
    /// pane's agent.
    herdr: std::sync::RwLock<Option<Arc<crate::herdr::Reporter>>>,
}

/// Clears passive presentation routing when one logical user turn ends.
pub(crate) struct PresentationOriginGuard(Arc<crate::schedule::tools::TurnOrigin>);

impl Drop for PresentationOriginGuard {
    fn drop(&mut self) {
        self.0.set(None);
    }
}

/// One admitted operation. Dropping it releases admission and performs a
/// close that arrived while the operation was running.
pub struct Admission<'a> {
    controller: &'a Controller,
    generation: u64,
}

impl std::fmt::Debug for Admission<'_> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Admission")
            .field("generation", &self.generation)
            .finish()
    }
}

impl Drop for Admission<'_> {
    fn drop(&mut self) {
        self.controller.release(self.generation);
    }
}

impl Controller {
    /// The composition-root constructor `cli::run` uses.
    pub fn new(
        builder: Builder,
        dynamic_content: bool,
        session: SharedSession,
        runner: Runner,
        info: RuntimeInfo,
    ) -> Self {
        Self::with_builder(Arc::new(builder), dynamic_content, session, runner, info)
    }

    /// The per-session constructor `server` uses: one [`Builder`] backs every
    /// open session, so it is shared rather than moved.
    pub fn with_builder(
        builder: Arc<Builder>,
        dynamic_content: bool,
        session: SharedSession,
        runner: Runner,
        info: RuntimeInfo,
    ) -> Self {
        let controller = Self {
            builder,
            dynamic_content,
            sandbox: None,
            state: Mutex::new(State {
                current: Some(Current::build(session, Arc::new(runner), info)),
                ..State::default()
            }),
            close_signal: Condvar::new(),
            notices: Arc::new(auto_reflection::Notices::new()),
            wake_replies: auto_reflection::Notices::new(),
            auto_running: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            auto_cancel: CancellationToken::new(),
            approval_control: tokio::sync::Mutex::new(()),
            bell: std::sync::atomic::AtomicBool::new(false),
            herdr: std::sync::RwLock::new(None),
        };
        controller.notify_memory_state();
        controller.bind_reflection();
        controller
    }

    /// Starts reporting this session's lifecycle to the Herdr pane that
    /// started the process. `cli::run` calls this for the frontends that are
    /// the pane's agent; without it nothing is reported.
    ///
    /// The pane is announced here, not at the first turn: a Herdr pane is
    /// nobody until the agent inside it reports, so Otto would be missing from
    /// the agent list — and unreachable through `herdr agent ...` — until the
    /// user sent a prompt.
    pub fn install_herdr(&self, reporter: Arc<crate::herdr::Reporter>) {
        reporter.ready(&self.herdr_session());
        *self
            .herdr
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(reporter);
    }

    fn herdr_reporter(&self) -> Option<Arc<crate::herdr::Reporter>> {
        self.herdr
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// What Herdr shows and restores for this session. Read without the
    /// [`Controller::info`] snapshot, which clones the transcript.
    fn herdr_session(&self) -> crate::herdr::SessionRef {
        let state = self.lock();
        match state.current.as_ref() {
            Some(current) => crate::herdr::SessionRef {
                id: current.session.header().id,
                path: current.path(),
            },
            None => crate::herdr::SessionRef {
                id: String::new(),
                path: String::new(),
            },
        }
    }

    /// Queues the notices about memory that needs the user's attention:
    /// pending candidates in the current user and workspace scopes, and
    /// standing preferences that recall does not inject because the standing
    /// set is over its budget. Called once per session open: on construction
    /// and when `commit` installs a replacement session.
    fn notify_memory_state(&self) {
        let memory = &self.builder.memory;
        if !memory.usable {
            return;
        }
        let scopes = [
            memory.user_scope.clone(),
            self.builder.workspace_scope.clone(),
        ];
        if let Ok(summary) = memory.service.pending_summary(&scopes)
            && let Some(oldest) = summary.oldest
        {
            let line = match summary.count {
                1 => format!(
                    "1 memory proposal is pending review (since {}). Run /memory review to accept or reject it.",
                    oldest.format("%Y-%m-%d")
                ),
                count => format!(
                    "{count} memory proposals are pending review (oldest since {}). Run /memory review to accept or reject them.",
                    oldest.format("%Y-%m-%d")
                ),
            };
            self.notices.push(line);
        }
        if let Ok(usage) = memory.service.standing_usage(&memory.user_scope)
            && usage.omitted > 0
        {
            let budget = usage.budget;
            self.notices.push(match usage.omitted {
                1 => format!(
                    "1 standing preference is not recalled because the standing set is over its {budget}-token budget. Forget one with /memory forget <id>; list them with /memory list --scope user."
                ),
                omitted => format!(
                    "{omitted} standing preferences are not recalled because the standing set is over its {budget}-token budget. Forget some with /memory forget <id>; list them with /memory list --scope user."
                ),
            });
        }
    }

    /// Wires the process sandbox. The control reports the state now in effect,
    /// so every controller built from one composition root agrees after a
    /// reload.
    pub fn with_sandbox_control(mut self, control: Arc<dyn SandboxControl>) -> Self {
        self.sandbox = Some(control);
        self
    }

    pub fn builder(&self) -> &Arc<Builder> {
        &self.builder
    }

    /// A content-free generation signal for successfully queued memory review
    /// candidates. Frontends still use their normal scoped commands to inspect
    /// or decide a candidate.
    pub fn memory_review_available_changed(&self) -> watch::Receiver<u64> {
        self.builder.memory.service.review_available_changed()
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    }

    // ---- admission ----

    pub fn begin_operation(&self) -> Result<Admission<'_>, String> {
        let mut state = self.lock();
        if state.closed {
            return Err(CLOSED.to_string());
        }
        if state.busy {
            return Err(PROMPT_ACTIVE.to_string());
        }
        state.busy = true;
        state.generation += 1;
        Ok(Admission {
            controller: self,
            generation: state.generation,
        })
    }

    pub(crate) fn begin_runner_operation(&self, runner: &Runner) -> Result<Admission<'_>, String> {
        let admission = self.begin_operation()?;
        let current = self.lock();
        let is_current = current
            .current
            .as_ref()
            .is_some_and(|current| std::ptr::eq(current.runner.as_ref(), runner));
        drop(current);
        if !is_current {
            return Err("the active session changed before the task resumed".into());
        }
        Ok(admission)
    }

    fn release(&self, generation: u64) {
        let victim = {
            let mut state = self.lock();
            if !state.busy || state.generation != generation {
                return;
            }
            state.busy = false;
            if !state.close_pending {
                return;
            }
            state.close_pending = false;
            state.close_in_progress = true;
            state.current.take()
        };
        self.finish_close(victim);
    }

    fn finish_close(&self, victim: Option<Current>) {
        let error = victim.and_then(|current| current.discard().err());
        let mut state = self.lock();
        if state.close_err.is_none() {
            state.close_err = error;
        }
        state.close_in_progress = false;
        state.close_done = true;
        drop(state);
        self.close_signal.notify_all();
    }

    /// Rejects new work and hands the close to whichever operation is in
    /// flight. Never waits and never closes, so a callback running inside an
    /// operation may call it.
    pub fn request_close(&self) {
        self.auto_cancel.cancel();
        let mut state = self.lock();
        state.closed = true;
        if state.busy {
            if state.wake_claim_is_unstarted() {
                // Release the claim, so the close need not wait for a wake turn
                // that will now never run.
                state.busy = false;
                state.generation += 1;
                state.wake_unstarted = false;
            } else {
                state.close_pending = true;
            }
        }
    }

    /// Completes the close, waiting for an in-flight operation to finish.
    /// Blocking: an async caller must cancel and await its own work first.
    pub fn close(&self) -> Result<(), String> {
        self.request_close();
        let victim = {
            let mut state = self.lock();
            if state.close_done {
                return state.close_err.clone().map_or(Ok(()), Err);
            }
            if state.busy || state.close_in_progress {
                let state = self
                    .close_signal
                    .wait_while(state, |state| !state.close_done)
                    .unwrap_or_else(|poison| poison.into_inner());
                return state.close_err.clone().map_or(Ok(()), Err);
            }
            state.close_in_progress = true;
            state.current.take()
        };
        self.finish_close(victim);
        self.lock().close_err.clone().map_or(Ok(()), Err)
    }

    // ---- reads ----

    /// The captured ChatGPT credential path, empty when none was captured.
    pub fn auth_path(&self) -> &str {
        &self.builder.auth_path
    }

    /// The process-wide memory service, its user scope, and the recall
    /// limits. `/memory` and `/remember` read them through
    /// `Controller::memory_manager`.
    pub(crate) fn memory_wiring(&self) -> &crate::runtime::wiring::MemoryWiring {
        &self.builder.memory
    }

    pub fn memory_service(&self) -> Option<Arc<crate::memory::client::Client>> {
        self.dynamic_content_available()
            .then(|| self.builder.memory.client.clone())
            .flatten()
            .filter(|client| !client.is_closed())
    }

    pub fn memory_context(
        &self,
        origin: otto_core::memory::Origin,
    ) -> otto_core::memory::MemoryContext {
        self.builder
            .memory_context(origin, Some(self.info().session_id))
    }

    /// This session's memory workspace scope, unlike `memory_wiring`'s
    /// process-wide service and user scope.
    pub(crate) fn workspace_memory_scope(&self) -> &crate::memory::Scope {
        &self.builder.workspace_scope
    }

    /// The runner currently in force. `/tasks` and `/task` read its sub-agent
    /// task registry.
    pub(crate) fn current_runner(&self) -> Option<Arc<Runner>> {
        self.runner().ok()
    }

    pub fn config_path(&self) -> &PathBuf {
        &self.builder.config_path
    }

    /// The sandbox state now in effect: the live process sandbox when one is
    /// wired, otherwise what the builder resolved at startup.
    pub fn sandbox_info(&self) -> SandboxInfo {
        match &self.sandbox {
            Some(control) => control.info(),
            None => self.builder.effective_sandbox_info(),
        }
    }

    pub fn dynamic_content(&self) -> bool {
        self.dynamic_content
    }

    pub fn dynamic_content_available(&self) -> bool {
        !self.lock().closed && self.dynamic_content
    }

    pub fn workspace(&self) -> &str {
        &self.builder.workspace_path
    }

    /// A closed boundary reports the sandbox only.
    pub fn info(&self) -> Info {
        let sandbox = self.sandbox_info();
        if !self.dynamic_content {
            return Info {
                sandbox,
                ..Info::default()
            };
        }
        let state = self.lock();
        let Some(current) = state.current.as_ref() else {
            return Info {
                sandbox,
                ..Info::default()
            };
        };
        let header = current.session.header();
        let snapshot = current.session.snapshot();
        let approval_settings = self
            .builder
            .approvals
            .settings(&crate::approval::Scope {
                workspace: header.workspace.clone(),
                session_id: header.id.clone(),
                task_id: String::new(),
                run_id: String::new(),
            })
            .unwrap_or_default();
        Info {
            session_id: header.id,
            session_name: current.session.name(),
            session_path: current.path(),
            workspace: header.workspace,
            provider: current.info.provider.clone(),
            profile: current.info.profile.clone(),
            model: current.info.model.clone(),
            thinking: current.info.thinking.clone(),
            usage: snapshot.aggregate_usage,
            usage_present: snapshot.aggregate_usage_present,
            context_window: current.info.context_window,
            context_input_tokens: snapshot.context_input_tokens,
            context_input_tokens_present: snapshot.context_input_tokens_present,
            context_input_tokens_pending: snapshot.context_input_tokens_pending,
            sandbox,
            approval_mode: approval_settings.mode(),
            approval_settings,
            forked_from: current.session.forked_from().map(ForkedFrom::from),
            plan: otto_core::session::Session::plan(&current.session),
        }
    }

    /// Whether the current session's transcript holds a message delivering
    /// input `input_id` (a client retry of an input already delivered).
    pub fn has_input(&self, input_id: &str) -> bool {
        let state = self.lock();
        state.current.as_ref().is_some_and(|current| {
            otto_core::session::Session::has_input(&current.session, input_id)
        })
    }

    /// The session's durable turn queue; `None` under `--no-session`.
    pub(crate) fn turn_queue(&self) -> Option<Arc<turn_queue::TurnQueue>> {
        self.current_runner()?.task_control().turn_queue()
    }

    /// The prompts saved in the turn queue that have not started, oldest
    /// first. A saved prompt whose user message is already in the transcript
    /// started before Otto stopped: its entry is removed, and the
    /// interrupted turn's recovery covers it.
    pub(crate) fn restorable_prompts(&self) -> Vec<turn_queue::QueuedPrompt> {
        let Some(queue) = self.turn_queue() else {
            return Vec::new();
        };
        queue
            .pending()
            .into_iter()
            .filter(|prompt| {
                let started = self.has_input(&prompt.input_id);
                if started {
                    queue.forget(&prompt.input_id);
                }
                !started
            })
            .collect()
    }

    /// Queues the context notice that `prompts` were restored, for the
    /// first of them to deliver before its user message. Its id is stable,
    /// so a second restore of the same prompts does not repeat it.
    pub(crate) fn announce_restored_prompts(&self, prompts: &[turn_queue::QueuedPrompt]) {
        let (Some(first), Some(tasks)) = (prompts.first(), self.subagent_tasks()) else {
            return;
        };
        let _ = tasks
            .notifications()
            .enqueue(otto_core::agent::inbox::Notification {
                id: format!("restored:{}", first.input_id),
                text: turn_queue::restored_notice(prompts),
                ..otto_core::agent::inbox::Notification::default()
            });
    }

    /// Records the client's input id for the next turn's user message; see
    /// `Agent::set_next_input_id`.
    pub fn set_next_input_id(&self, input_id: Option<String>) {
        if let Some(runner) = self.current_runner() {
            runner.set_next_input_id(input_id);
        }
    }

    /// Whether the session's last turn was interrupted by process loss, so
    /// the next wake turn resumes it (see `otto_core::session::turn`).
    pub fn resumes_interrupted_turn(&self) -> bool {
        let state = self.lock();
        state.current.as_ref().is_some_and(|current| {
            otto_core::session::Session::interrupted_turn(&current.session).is_some()
        })
    }

    pub fn history(&self) -> Vec<Message> {
        if !self.dynamic_content {
            return Vec::new();
        }
        let state = self.lock();
        match state.current.as_ref() {
            Some(current) => current.session.messages(),
            None => Vec::new(),
        }
    }

    /// What the next provider request contains, or `None` after a close or
    /// when the redaction boundary withholds dynamic content.
    pub fn context_report(&self) -> Option<otto_core::agent::context_report::ContextReport> {
        if !self.dynamic_content {
            return None;
        }
        let runner = self.current_runner()?;
        Some(runner.context_report())
    }

    /// The session currently in force, or `None` after a close.
    pub fn current_session_opt(&self) -> Option<SharedSession> {
        self.lock()
            .current
            .as_ref()
            .map(|current| current.session.clone())
    }

    /// The session currently in force. Panics only if called after a close,
    /// which no frontend does; `current_session_opt` is the checked form.
    pub fn current_session(&self) -> SharedSession {
        self.current_session_opt()
            .expect("controller has no current session")
    }

    /// The system prompt the current runner was built with.
    pub fn system_prompt(&self) -> String {
        self.lock()
            .current
            .as_ref()
            .map(|current| current.runner.system_prompt().to_string())
            .unwrap_or_default()
    }

    /// The skill catalog fixed for the current runner.
    pub fn skills(&self) -> crate::skill::Catalog {
        self.lock()
            .current
            .as_ref()
            .map(|current| current.runner.skills().clone())
            .unwrap_or_default()
    }

    /// The automatic skill contract checker, when the feature is enabled and
    /// the current runner has sub-agents built.
    pub fn skill_checker(&self) -> Option<Arc<crate::skill::check::Checker>> {
        self.lock()
            .current
            .as_ref()
            .and_then(|current| current.runner.subagents())
            .and_then(|subagents| subagents.checker().cloned())
    }

    /// The MCP server status rows for `/mcp`, in configuration order. Empty
    /// for a closed controller.
    pub fn mcp(&self) -> Vec<crate::mcp::ServerStatus> {
        self.lock()
            .current
            .as_ref()
            .map(|current| current.runner.mcp.status())
            .unwrap_or_default()
    }

    /// The names of the tools the current runner offers the model.
    #[cfg(test)]
    pub(crate) fn tool_names(&self) -> Vec<String> {
        self.runner()
            .map(|runner| {
                runner
                    .definitions()
                    .iter()
                    .map(|d| d.name.clone())
                    .collect()
            })
            .unwrap_or_default()
    }

    fn runner(&self) -> Result<Arc<Runner>, String> {
        self.lock()
            .current
            .as_ref()
            .map(|current| Arc::clone(&current.runner))
            .ok_or_else(|| CLOSED.to_string())
    }

    /// Installs a fully rebuilt runner only when no operation is active and
    /// the session is still the one the caller built against. This is the
    /// background MCP hot-swap path: the slow runner build happens outside
    /// the controller lock, and this small commit step refuses to run mid-turn.
    /// A rejected candidate is closed before returning.
    pub fn replace_runner_if_current(
        &self,
        session_id: &str,
        runner: Runner,
        info: RuntimeInfo,
    ) -> Result<bool, String> {
        enum Decision {
            Installed(Arc<Runner>),
            Busy,
            Closed,
            StaleSession,
        }

        let mut runner = Some(runner);
        let decision = {
            let mut state = self.lock();
            if state.closed {
                Decision::Closed
            } else if state.busy {
                Decision::Busy
            } else {
                let current = state.current.as_mut().ok_or_else(|| CLOSED.to_string())?;
                if current.session.header().id != session_id {
                    Decision::StaleSession
                } else {
                    current.info = info;
                    Decision::Installed(std::mem::replace(
                        &mut current.runner,
                        Arc::new(runner.take().expect("runner is still available")),
                    ))
                }
            }
        };
        match decision {
            Decision::Installed(old) => {
                old.close();
                self.bind_reflection();
                Ok(true)
            }
            Decision::Busy => {
                if let Some(runner) = runner.take() {
                    runner.close();
                }
                Err(PROMPT_ACTIVE.to_string())
            }
            Decision::Closed => {
                if let Some(runner) = runner.take() {
                    runner.close();
                }
                Err(CLOSED.to_string())
            }
            Decision::StaleSession => {
                if let Some(runner) = runner.take() {
                    runner.close();
                }
                Ok(false)
            }
        }
    }

    /// Shuts every connected MCP server down, waiting up to 5 seconds. A
    /// no-op on a controller with no current runner (already closed, or
    /// never opened). [`Self::close`] also starts an MCP close, but only in
    /// the background (`Runner::close`, via `Current::discard`): a process
    /// exiting right after `close` returns would drop the runtime before
    /// that spawned close ran. At a process-exit site, call this *before*
    /// `close`, not after: [`crate::mcp::Servers::close`] uses `mem::take`
    /// internally, so whichever of this awaited call or `close`'s
    /// background one runs first is the one that actually closes anything.
    pub async fn close_mcp(&self) {
        let runner = self
            .lock()
            .current
            .as_ref()
            .map(|current| Arc::clone(&current.runner));
        if let Some(runner) = runner {
            runner.end_session_hooks().await;
            runner.close_mcp().await;
        }
    }

    /// Runs a SIGTERM migration's session-side work, in the spec's order:
    /// cancel (already done by the caller before this is called), wait for
    /// the cancelled sub-agent tasks and record them interrupted, notify the
    /// parent session (all three inside [`Runner::migrate`]), close MCP,
    /// release the lease. Called from a frontend's exit tail after its own
    /// turn was already cancelled, so it takes admission the same way a turn
    /// does rather than assuming the controller is idle: retries on
    /// [`PROMPT_ACTIVE`] every 20ms (the in-flight operation is the
    /// just-cancelled turn winding down) and gives up with no warnings on
    /// [`CLOSED`] (nothing left to migrate). Requests a close while the
    /// admission is still held, so dropping it runs [`Self::finish_close`]
    /// (closes the runner and the session, which writes the lease's
    /// `released: true`); the caller's own `close_mcp`/`close` pair still
    /// runs afterward and finds the controller already closed.
    pub async fn migrate(&self) -> Vec<String> {
        let admission = loop {
            match self.begin_operation() {
                Ok(admission) => break admission,
                Err(ref message) if message == PROMPT_ACTIVE => {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
                Err(_) => return Vec::new(),
            }
        };
        let Ok(runner) = self.runner() else {
            return Vec::new();
        };
        let warnings = runner.migrate().await;
        runner.end_session_hooks().await;
        runner.close_mcp().await;
        self.request_close();
        drop(admission);
        warnings
    }

    /// Reads the session's approval mode, or sets it when `args` selects one:
    /// `manual`, `auto`, `yolo`, and the older `on` (AUTO) and `off` (MANUAL).
    pub fn auto_mode_command(&self, args: &str) -> Result<String, String> {
        let selected = parse_auto_mode(args)?;
        if let Some(mode) = selected {
            self.approval_service()
                .set_mode(&self.approval_scope(), mode)
                .map_err(|e| e.to_string())?;
        }
        let settings = self
            .approval_service()
            .settings(&self.approval_scope())
            .map_err(|e| e.to_string())?;
        Ok(format!(
            "Approval mode: {}",
            if settings.policy == crate::approval::ApprovalPolicy::Never {
                "NEVER (reviewer disabled)"
            } else {
                settings.mode().label()
            }
        ))
    }

    pub fn permissions_command(&self, args: &str) -> Result<String, String> {
        let scope = self.approval_scope();
        let current = self
            .approval_service()
            .settings(&scope)
            .map_err(|e| e.to_string())?;
        let settings = match permissions_update(args, current)? {
            Some((policy, reviewer)) => self
                .approval_service()
                .set_settings(&scope, current.revision, policy, reviewer)
                .map_err(|e| e.to_string())?,
            None => current,
        };
        self.approval_snapshot().map_err(|e| e.to_string())?;
        let activity = self
            .approval_service()
            .activity_snapshot(&scope.workspace, Some(&scope.session_id))
            .map_err(|e| e.to_string())?;
        Ok(format!(
            "{}\n{} human pending · {} reviewing · {} application failed",
            format_permissions(settings),
            activity.counts.human_pending,
            activity.counts.reviewing,
            activity.counts.application_failed
        ))
    }

    pub fn approval_service(&self) -> &Arc<crate::approval::Service> {
        &self.builder.approvals
    }

    pub fn approval_scope(&self) -> crate::approval::Scope {
        let info = self.info();
        crate::approval::Scope {
            workspace: info.workspace,
            session_id: info.session_id,
            task_id: String::new(),
            run_id: String::new(),
        }
    }

    pub fn review_memory(
        &self,
        request: &crate::memory::ReviewRequest,
        transport: &str,
    ) -> crate::memory::Result<crate::memory::ReviewResult> {
        use crate::memory::{CandidateState, Error, ErrorKind, ReviewDecision};
        let (memory, user, workspace) = self
            .memory_manager()
            .ok_or_else(|| Error::new(ErrorKind::Unavailable))?;
        if request.reference.scope != user && request.reference.scope != workspace {
            return Err(Error::new(ErrorKind::NotFound));
        }
        let candidate = memory.get_candidate(&request.reference)?;
        if candidate.state != CandidateState::Pending {
            return Err(Error::new(ErrorKind::Conflict));
        }
        let snapshot = self
            .approval_snapshot()
            .map_err(|_| Error::new(ErrorKind::Unavailable))?;
        let id = format!(
            "{}/{}/{}/{}",
            user.id,
            request.reference.scope.namespace,
            request.reference.scope.id,
            request.reference.id
        );
        let approval = snapshot
            .requests
            .into_iter()
            .find(|r| r.subject.source == "memory" && r.subject.id == id)
            .ok_or_else(|| Error::new(ErrorKind::Unavailable))?;
        self.approval_service()
            .decide_and_claim(
                &approval.id,
                &approval.scope,
                approval.revision,
                request.decision == ReviewDecision::Accept,
                &crate::approval::Actor::local(transport),
            )
            .map_err(|_| Error::new(ErrorKind::Conflict))?;
        let result = memory.review(request);
        let state = if result.is_ok() {
            crate::approval::ApplicationState::Applied
        } else {
            crate::approval::ApplicationState::Failed
        };
        self.approval_service()
            .application(
                &approval.id,
                &approval.scope,
                crate::approval::ApplicationState::Applying,
                state,
                if result.is_err() {
                    "Memory review could not be applied."
                } else {
                    ""
                },
            )
            .map_err(|_| Error::new(ErrorKind::Unavailable))?;
        result
    }

    pub fn approval_snapshot(&self) -> Result<crate::approval::Snapshot, crate::approval::Error> {
        let scope = self.approval_scope();
        if let Some((memory, user, workspace)) = self.memory_manager() {
            crate::approval::memory::attach(
                self.approval_service(),
                &memory,
                vec![user.clone(), workspace.clone()],
                &scope.workspace,
            )?;
            crate::approval::memory::reconcile(
                self.approval_service(),
                &memory,
                &[user, workspace],
                &scope.workspace,
            )?;
        }
        self.approval_service().apply_available(&scope.workspace)?;
        self.approval_service()
            .snapshot(&scope.workspace, Some(&scope.session_id))
    }

    pub fn decide_shared_approval(
        &self,
        id: &str,
        allow: bool,
        feedback: &str,
        transport: &str,
    ) -> Result<crate::approval::Request, crate::approval::Error> {
        let scope = self.approval_scope();
        let request = self
            .approval_service()
            .get(id, &scope.workspace, Some(&scope.session_id))?;
        let decided = self.approval_service().decide(
            id,
            &request.scope,
            request.revision,
            allow,
            feedback,
            &crate::approval::Actor::local(transport),
        )?;
        self.approval_service().apply_available(&scope.workspace)?;
        Ok(decided)
    }

    // ---- turns ----

    pub async fn prompt(
        &self,
        text: &str,
        emit: EventSink<'_>,
        cancel: &CancellationToken,
    ) -> Result<(), AgentError> {
        self.prompt_with_system_context(
            text,
            "",
            Some(crate::schedule::Origin::terminal()),
            emit,
            cancel,
        )
        .await
    }

    pub(crate) async fn prompt_with_system_context(
        &self,
        text: &str,
        system_context: &str,
        origin: Option<crate::schedule::Origin>,
        emit: EventSink<'_>,
        cancel: &CancellationToken,
    ) -> Result<(), AgentError> {
        let _admission = self.begin_operation().map_err(AgentError::Other)?;
        let runner = self.runner().map_err(AgentError::Other)?;
        let _presentation = self.begin_user_turn(&runner, origin).await;
        runner.record_user_authorization(text).await?;
        let task_run = runner
            .task_control()
            .begin(&runner.session().header().id, &runner.redact_text(text))
            .map_err(AgentError::Other)?;
        self.record_unrun_followups(&runner, task_run.discarded(), &mut *emit)
            .await?;
        self.run_prompt_step(&runner, text, system_context, &mut *emit, cancel)
            .await?;
        self.run_followups_held(&runner, &task_run, system_context, &mut *emit, cancel)
            .await
    }

    /// One prompt step inside an already-admitted logical turn. Approval
    /// retries call this without opening a second follow-up scope.
    pub(crate) async fn run_prompt_step(
        &self,
        runner: &Runner,
        text: &str,
        system_context: &str,
        emit: EventSink<'_>,
        cancel: &CancellationToken,
    ) -> Result<(), AgentError> {
        let observation = auto_reflection::RunObservation::default();
        let reporter = self.herdr_reporter();
        let session = match &reporter {
            Some(_) => self.herdr_session(),
            None => crate::herdr::SessionRef {
                id: String::new(),
                path: String::new(),
            },
        };
        let mut observed = auto_reflection::observing_prompt(&mut *emit, &observation);
        let result = runner
            .run_with_system_context(
                text,
                system_context,
                &mut crate::herdr::observing(&mut observed, reporter.as_deref(), &session),
                cancel,
            )
            .await;
        self.after_prompt(&observation, result.is_ok() && !cancel.is_cancelled());
        result
    }

    async fn run_followups_held(
        &self,
        runner: &Runner,
        task_run: &task_control::Run,
        system_context: &str,
        emit: EventSink<'_>,
        cancel: &CancellationToken,
    ) -> Result<(), AgentError> {
        while let Some(followup) = task_run.next_followup() {
            if self.pending_approval().is_some() {
                task_run.clear_followups();
                self.record_skipped_followup(runner, emit).await?;
                return Ok(());
            }
            if otto_core::session::Session::has_input(runner.session(), &followup.input_id) {
                // Its step ran before an interruption kept it journaled.
                task_run.followup_done(&followup);
                continue;
            }
            task_run.set_current_task(&followup.text);
            runner.set_next_input_id(Some(followup.input_id.clone()));
            self.run_followup_step(runner, &followup.text, system_context, &mut *emit, cancel)
                .await?;
            task_run.followup_done(&followup);
        }
        Ok(())
    }

    /// Records, as a persisted and visible `agent_followup` notice, that
    /// `count` follow-ups of an earlier task were discarded because Otto
    /// stopped before they started and that task is not being resumed.
    pub(crate) async fn record_unrun_followups(
        &self,
        runner: &Runner,
        count: usize,
        emit: EventSink<'_>,
    ) -> Result<(), AgentError> {
        if count == 0 {
            return Ok(());
        }
        let text = format!(
            "{count} agent-scheduled follow-up(s) from an earlier task were not run because Otto stopped before they started. Schedule them again if they are still needed."
        );
        runner
            .session()
            .append(Message {
                role: otto_core::model::Role::Context,
                blocks: vec![Block::text(&text)],
                created_at: chrono::Utc::now(),
                context_type: "agent_followup".into(),
                display: true,
                ..Message::default()
            })
            .await
            .map_err(|error| AgentError::Other(error.to_string()))?;
        emit(otto_core::agent::Event::Notification {
            kind: Some(NotificationKind::TaskReport),
            task_id: "agent_followup".into(),
            text,
            usage: Usage::default(),
            present: false,
        });
        Ok(())
    }

    /// Runs the journaled follow-ups of the task a wake turn just resumed,
    /// in order, under the same rules as any follow-up.
    pub(crate) async fn run_resumed_followups(
        &self,
        runner: &Runner,
        emit: EventSink<'_>,
        cancel: &CancellationToken,
    ) -> Result<(), AgentError> {
        let task_run = runner
            .task_control()
            .begin_resuming(&runner.session().header().id, "", true)
            .map_err(AgentError::Other)?;
        self.run_followups_held(runner, &task_run, "", emit, cancel)
            .await
    }

    /// Whether the session's last turn was interrupted by process loss and
    /// still has its automatic resume: the next wake turn resumes it and
    /// adopts its journaled follow-ups.
    pub(crate) fn resumable_interrupted_turn(&self) -> bool {
        let state = self.lock();
        state.current.as_ref().is_some_and(|current| {
            otto_core::session::Session::interrupted_turn(&current.session)
                .is_some_and(|turn| !turn.resume_exhausted())
        })
    }

    /// Sets the turn origin the schedule tools read and appends the scheduled
    /// deliveries for that origin that no session has recorded yet, before the
    /// user message. Called once per logical user turn, not for follow-ups or
    /// wake turns.
    pub(crate) async fn begin_user_turn(
        &self,
        runner: &Runner,
        origin: Option<crate::schedule::Origin>,
    ) -> PresentationOriginGuard {
        runner.schedule_origin.set(origin.clone());
        runner.presentation_origin.set(origin.clone());
        let presentation = PresentationOriginGuard(Arc::clone(&runner.presentation_origin));

        let (Some(origin), Some(store)) = (origin, self.builder.schedule.as_ref()) else {
            return presentation;
        };
        let session_id = runner.session().header().id.clone();
        // A store or append error ends the write-back; the next user turn
        // retries, because the row stays unmarked.
        let Ok(deliveries) = store.pending_writebacks(&origin, &session_id, 10) else {
            return presentation;
        };
        for delivery in deliveries {
            let appended = runner
                .session()
                .append(Message {
                    role: otto_core::model::Role::Context,
                    blocks: vec![Block::text(runner.redact_text(&delivery.context_text()))],
                    created_at: chrono::Utc::now(),
                    context_type: "scheduled_delivery".into(),
                    display: true,
                    ..Message::default()
                })
                .await;
            if appended.is_err()
                || store
                    .mark_appended(delivery.item_id, delivery.due_at)
                    .is_err()
            {
                return presentation;
            }
        }
        presentation
    }

    async fn record_skipped_followup(
        &self,
        runner: &Runner,
        emit: EventSink<'_>,
    ) -> Result<(), AgentError> {
        let text = "An agent-scheduled follow-up was not run because this terminal turn ended while approval is pending. Retry it after resolving the approval.";
        runner
            .session()
            .append(Message {
                role: otto_core::model::Role::Context,
                blocks: vec![Block::text(text)],
                created_at: chrono::Utc::now(),
                context_type: "agent_followup".into(),
                display: true,
                ..Message::default()
            })
            .await
            .map_err(|error| AgentError::Other(error.to_string()))?;
        emit(otto_core::agent::Event::Notification {
            kind: Some(NotificationKind::TaskReport),
            task_id: "agent_followup".into(),
            text: text.into(),
            usage: Usage::default(),
            present: false,
        });
        Ok(())
    }

    async fn run_followup_step(
        &self,
        runner: &Runner,
        text: &str,
        system_context: &str,
        emit: EventSink<'_>,
        cancel: &CancellationToken,
    ) -> Result<(), AgentError> {
        if cancel.is_cancelled() {
            return Ok(());
        }
        emit(otto_core::agent::Event::TextDelta {
            text: "\n\n".into(),
        });
        let observation = auto_reflection::RunObservation::default();
        let result = runner
            .run_followup_with_system_context(
                text,
                system_context,
                &mut auto_reflection::observing_prompt(&mut *emit, &observation),
                cancel,
            )
            .await;
        self.after_prompt(&observation, result.is_ok() && !cancel.is_cancelled());
        result
    }

    pub async fn prompt_with_image(
        &self,
        text: &str,
        image: Block,
        emit: EventSink<'_>,
        cancel: &CancellationToken,
    ) -> Result<(), AgentError> {
        self.prompt_with_image_and_system_context(
            text,
            image,
            "",
            Some(crate::schedule::Origin::terminal()),
            emit,
            cancel,
        )
        .await
    }

    pub(crate) async fn prompt_with_image_and_system_context(
        &self,
        text: &str,
        image: Block,
        system_context: &str,
        origin: Option<crate::schedule::Origin>,
        emit: EventSink<'_>,
        cancel: &CancellationToken,
    ) -> Result<(), AgentError> {
        let _admission = self.begin_operation().map_err(AgentError::Other)?;
        let runner = self.runner().map_err(AgentError::Other)?;
        let _presentation = self.begin_user_turn(&runner, origin).await;
        runner.record_user_authorization(text).await?;
        let task_run = runner
            .task_control()
            .begin(&runner.session().header().id, &runner.redact_text(text))
            .map_err(AgentError::Other)?;
        self.record_unrun_followups(&runner, task_run.discarded(), &mut *emit)
            .await?;
        let observation = auto_reflection::RunObservation::default();
        let result = runner
            .run_with_image_system_context(
                text,
                image,
                system_context,
                &mut auto_reflection::observing_prompt(&mut *emit, &observation),
                cancel,
            )
            .await;
        self.after_prompt(&observation, result.is_ok() && !cancel.is_cancelled());
        result?;
        self.run_followups_held(&runner, &task_run, system_context, &mut *emit, cancel)
            .await
    }

    pub async fn compact(
        &self,
        focus: &str,
        emit: EventSink<'_>,
        cancel: &CancellationToken,
    ) -> Result<CompactionResult, AgentError> {
        let _admission = self.begin_operation().map_err(AgentError::Other)?;
        let runner = self.runner().map_err(AgentError::Other)?;
        let compacted = std::sync::atomic::AtomicBool::new(false);
        let result = runner
            .compact(
                focus,
                &mut auto_reflection::observing(emit, &compacted),
                cancel,
            )
            .await;
        self.after_operation(&compacted);
        result
    }

    /// Starts background reflection when the operation just ended compacted
    /// the transcript.
    pub(crate) fn after_operation(&self, compacted: &std::sync::atomic::AtomicBool) {
        if compacted.load(std::sync::atomic::Ordering::SeqCst) {
            self.start_auto_reflection(crate::reflection::Trigger::OnCompaction);
        }
    }

    /// Runs compaction reflection as before, then considers a task-end run
    /// only for a successful completed prompt with enough successful tools.
    fn after_prompt(&self, observation: &auto_reflection::RunObservation, completed: bool) {
        if observation.compacted() {
            self.start_auto_reflection(crate::reflection::Trigger::OnCompaction);
        }
        if observation.eligible_work(completed) {
            self.start_auto_reflection(crate::reflection::Trigger::OnWork);
        }
    }

    /// Reflects on the part of the current session no earlier run covered and
    /// queues what it finds as memory candidates for human review.
    ///
    /// Takes the same admission as a turn or a compaction, so it never runs
    /// beside either. Appends nothing to the session.
    pub async fn reflect(
        &self,
        focus: &str,
        cancel: &CancellationToken,
    ) -> Result<crate::reflection::Report, crate::reflection::Error> {
        self.reflect_as(crate::reflection::Trigger::Manual, focus, cancel)
            .await
    }

    /// The newest reflection run logs, newest first. Reads files only, so it
    /// needs no admission and may run beside a turn.
    pub fn reflection_runs(
        &self,
    ) -> Result<Vec<crate::reflection::runlog::Summary>, crate::reflection::Error> {
        self.builder
            .reflector
            .runs(crate::reflection::runlog::MAXIMUM_LISTED)
    }

    /// One reflection run log by the id [`Self::reflection_runs`] shows.
    pub fn reflection_run(
        &self,
        id: &str,
    ) -> Result<Option<(PathBuf, String)>, crate::reflection::Error> {
        self.builder.reflector.run_log(id)
    }

    /// Where reflection writes and looks up skills, or `None` when skills are
    /// disabled in `[skills]` or there is no home directory.
    pub fn reflection_skill_roots(&self) -> Option<crate::reflection::skillwrite::Roots> {
        let skills = otto_core::config::resolve_skills(
            &self.builder.config,
            &self.builder.environment,
            &self.builder.workspace_path,
        );
        skills
            .enabled
            .then(|| crate::reflection::skill_roots(&self.builder.home, &skills.roots))
            .flatten()
    }

    // ---- profiles ----

    /// The configured profile names, sorted.
    pub fn profiles(&self) -> Vec<String> {
        if !self.dynamic_content_available() {
            return Vec::new();
        }
        let mut names: Vec<String> = self.builder.config.profiles.keys().cloned().collect();
        names.sort();
        names
    }

    pub fn profile_summaries(&self) -> Vec<ProfileSummary> {
        if !self.dynamic_content_available() {
            return Vec::new();
        }
        let current = self
            .lock()
            .current
            .as_ref()
            .map(|current| (current.info.profile.clone(), current.info.thinking.clone()));
        let mut rows: Vec<ProfileSummary> = self
            .builder
            .config
            .profiles
            .iter()
            .map(|(name, profile)| ProfileSummary {
                name: name.clone(),
                provider: profile.provider.clone(),
                model: profile.model.clone(),
                thinking: current
                    .as_ref()
                    .filter(|(current_name, _)| current_name == name)
                    .map(|(_, thinking)| thinking.clone())
                    .unwrap_or_else(|| profile.thinking.clone()),
            })
            .collect();
        rows.sort_by(|a, b| a.name.cmp(&b.name));
        rows
    }

    pub fn set_default_profile(&self, profile: &str) -> Result<(), String> {
        if self.lock().closed {
            return Err(CLOSED.to_string());
        }
        if !self.dynamic_content {
            return Err(PROFILE_SWITCH_UNAVAILABLE.to_string());
        }
        crate::config::set_default_profile_file(&self.builder.config_path, profile)
            .map_err(|error| self.builder.redact_error(&error.to_string(), None))
    }

    pub fn profile_effective_thinking(&self, profile: &str) -> String {
        if !self.dynamic_content_available() {
            return String::new();
        }
        self.builder
            .resolve_profile(profile)
            .map(|runtime| runtime.thinking)
            .unwrap_or_default()
    }

    pub async fn set_thinking(&self, thinking: &str) -> Result<(), String> {
        validate_thinking_arg(thinking)?;
        let admission = self.begin_replacement()?;
        if !self.dynamic_content {
            return Err(PROFILE_SWITCH_UNAVAILABLE.to_string());
        }
        let (session, runtime) = {
            let state = self.lock();
            let current = state.current.as_ref().ok_or_else(|| CLOSED.to_string())?;
            let metadata = RuntimeMetadata {
                profile: current.info.profile.clone(),
                provider: current.info.provider.clone(),
                model: current.info.model.clone(),
            };
            let mut runtime = self.replacement_runtime(&metadata)?;
            runtime.profile = current.info.profile.clone();
            runtime.provider = current.info.provider.clone();
            runtime.model = current.info.model.clone();
            runtime.thinking = normalize_thinking_arg(thinking);
            (current.session.clone(), runtime)
        };
        let runner = Arc::new(self.builder.build_runner_quiet(&session, &runtime).await?);
        session
            .update_thinking_level(&runtime.thinking)
            .map_err(|error| self.builder.redact_error(&error, Some(&runtime)))?;
        {
            let mut state = self.lock();
            let current = state.current.as_mut().ok_or_else(|| CLOSED.to_string())?;
            current.info = self.builder.runtime_info(&runtime);
            let old = std::mem::replace(&mut current.runner, runner);
            old.close();
        }
        self.bind_reflection();
        drop(admission);
        Ok(())
    }

    pub fn save_profile_thinking(&self, thinking: &str) -> Result<(), String> {
        validate_thinking_arg(thinking)?;
        if self.lock().closed {
            return Err(CLOSED.to_string());
        }
        if !self.dynamic_content {
            return Err(PROFILE_SWITCH_UNAVAILABLE.to_string());
        }
        let profile = self.info().profile;
        if profile.is_empty() {
            return Err("current profile is empty".to_string());
        }
        crate::config::set_profile_thinking_file(
            &self.builder.config_path,
            &profile,
            &normalize_thinking_arg(thinking),
        )
        .map_err(|error| self.builder.redact_error(&error.to_string(), None))
    }

    // ---- session replacement ----

    pub fn rename_session(&self, name: &str) -> Result<(), String> {
        let name = name.trim();
        if name.is_empty() {
            return Err(INVALID_SESSION_NAME.to_string());
        }
        let _admission = self.begin_replacement()?;
        if !self.dynamic_content {
            return Err(PERSISTENCE_DISABLED.to_string());
        }
        let session = self
            .lock()
            .current
            .as_ref()
            .map(|current| current.session.clone())
            .ok_or_else(|| CLOSED.to_string())?;
        session
            .rename(name)
            .map_err(|error| self.builder.redact_error(&error, None))
    }

    /// Admission for an operation that replaces the session. Ordering: closed
    /// first, then the capability gate, then the active check.
    fn begin_replacement(&self) -> Result<Admission<'_>, String> {
        let mut state = self.lock();
        if state.closed {
            return Err(CLOSED.to_string());
        }
        if state.busy {
            return Err(PROMPT_ACTIVE.to_string());
        }
        state.busy = true;
        state.generation += 1;
        Ok(Admission {
            controller: self,
            generation: state.generation,
        })
    }

    pub async fn new_session(&self) -> Result<(), String> {
        let admission = self.begin_replacement()?;
        let runtime = self.current_runtime()?;
        let replacement = self.fresh_replacement(&runtime).await?;
        self.commit(replacement, admission)?;
        Ok(())
    }

    /// Discards the resume result the line frontend does not use.
    pub async fn switch_profile(&self, profile: &str) -> Result<(), String> {
        self.switch_profile_result(profile).await.map(|_| ())
    }

    pub async fn switch_profile_result(&self, profile: &str) -> Result<ResumeResult, String> {
        let admission = self.begin_replacement()?;
        if !self.dynamic_content {
            return Err(PROFILE_SWITCH_UNAVAILABLE.to_string());
        }
        let runtime = self.builder.resolve_profile(profile)?;
        if !self.builder.boundary_allows_dynamic(Some(&runtime)) {
            return Err(SESSION_OPERATION_UNAVAILABLE.to_string());
        }
        let session = self
            .lock()
            .current
            .as_ref()
            .map(|current| current.session.clone())
            .ok_or_else(|| CLOSED.to_string())?;
        let runner = Arc::new(self.builder.build_runner_quiet(&session, &runtime).await?);
        if let Err(error) = self.builder.update_session_runtime(&session, &runtime) {
            runner.close();
            return Err(error);
        }
        let path = session.path();
        {
            let mut state = self.lock();
            let current = state.current.as_mut().ok_or_else(|| CLOSED.to_string())?;
            current.info = self.builder.runtime_info(&runtime);
            let old = std::mem::replace(&mut current.runner, runner);
            old.close();
        }
        self.bind_reflection();
        drop(admission);
        Ok(ResumeResult {
            session_path: path,
            warnings: Vec::new(),
        })
    }

    /// Rows carry `current` for the file already open.
    pub fn list_sessions(&self, limit: usize) -> Result<ListResult, String> {
        if self.lock().closed {
            return Err(CLOSED.to_string());
        }
        if !self.dynamic_content {
            return Err(PERSISTENCE_DISABLED.to_string());
        }
        let limit = limit.clamp(1, MAX_LIST_SESSIONS);
        let current_path = self
            .lock()
            .current
            .as_ref()
            .map(|current| current.session.path())
            .unwrap_or_default();
        let session_root = self
            .builder
            .storage
            .session_root(&self.builder.session_root)
            .map_err(|error| self.builder.redact_error(&error, None))?;
        self.builder
            .storage
            .sessions()
            .and_then(|repository| {
                repository.list(
                    &session_root,
                    &self.builder.workspace_path,
                    &current_path,
                    limit,
                )
            })
            .map_err(|error| self.builder.redact_error(&error.to_string(), None))
    }

    /// Resuming the session already in force is a no-op that reports its path.
    pub async fn resume_session(&self, path: &str) -> Result<ResumeResult, String> {
        let requested = canonical_session_path(path);
        let admission = self.begin_replacement()?;
        if !self.dynamic_content {
            return Err(PERSISTENCE_DISABLED.to_string());
        }
        {
            let state = self.lock();
            if let Some(current) = state.current.as_ref()
                && !requested.is_empty()
                && requested == current.canonical_path()
            {
                return Ok(ResumeResult {
                    session_path: current.path(),
                    warnings: Vec::new(),
                });
            }
        }
        let (replacement, warnings) = self.resumed_replacement(Path::new(path)).await?;
        let workspace_before = self
            .lock()
            .current
            .as_ref()
            .map(|current| current.workspace.clone())
            .unwrap_or_default();
        if replacement.path().is_empty() {
            let _ = replacement.discard();
            return Err("replacement session path is required".to_string());
        }
        if replacement.workspace != workspace_before {
            let _ = replacement.discard();
            return Err(
                "replacement session workspace does not match current workspace".to_string(),
            );
        }
        let path = self.commit(replacement, admission)?;
        Ok(ResumeResult {
            session_path: path,
            warnings,
        })
    }

    /// The places the current session can be forked, oldest first. A session
    /// with no file yet has none.
    pub fn fork_points(&self) -> Result<Vec<otto_core::session::ForkPoint>, String> {
        let path = {
            let state = self.lock();
            if state.closed {
                return Err(CLOSED.to_string());
            }
            if !self.dynamic_content {
                return Err(PERSISTENCE_DISABLED.to_string());
            }
            state
                .current
                .as_ref()
                .map(|current| current.session.path())
                .unwrap_or_default()
        };
        if path.is_empty() {
            return Ok(Vec::new());
        }
        self.builder
            .session_fork_points(&path)
            .map_err(|error| self.builder.redact_error(&error, None))
    }

    /// Writes a new session forked from the current one, stopping before the
    /// turn the user message `before` opens, or at the last turn boundary
    /// without `before`. The current session is unchanged and stays open.
    pub fn fork_file(&self, before: Option<&str>) -> Result<crate::session::Forked, ForkFailure> {
        let path = {
            let state = self.lock();
            if state.closed {
                return Err(ForkFailure::Failed(CLOSED.to_string()));
            }
            if !self.dynamic_content {
                return Err(ForkFailure::Failed(PERSISTENCE_DISABLED.to_string()));
            }
            state
                .current
                .as_ref()
                .map(|current| current.session.path())
                .unwrap_or_default()
        };
        if path.is_empty() {
            return Err(ForkFailure::Failed(NOTHING_TO_FORK.to_string()));
        }
        self.builder
            .fork_session_file(&path, before)
            .map_err(|error| match error {
                otto_core::session::ForkError::InvalidPoint(_) => {
                    ForkFailure::InvalidPoint(error.to_string())
                }
                otto_core::session::ForkError::Session(error) => {
                    ForkFailure::Failed(self.builder.redact_error(&error.to_string(), None))
                }
            })
    }

    /// Forks the current session (see [`Controller::fork_file`]) and switches
    /// to the fork. Returns the switch result and the prompt the fork stops
    /// before, for the editor.
    pub async fn fork_session(
        &self,
        before: Option<&str>,
    ) -> Result<(ResumeResult, Option<String>), String> {
        if self.lock().busy {
            return Err(PROMPT_ACTIVE.to_string());
        }
        let forked = self.fork_file(before).map_err(|error| error.to_string())?;
        let result = self.resume_session(&forked.path.to_string_lossy()).await?;
        Ok((result, forked.point.map(|point| point.prompt)))
    }

    /// Archiving the current session delegates so a picker row behaves like
    /// `/archive`.
    pub async fn archive_session(&self, path: &str) -> Result<ArchiveResult, String> {
        let requested = canonical_session_path(path);
        let is_current = {
            let state = self.lock();
            if state.closed {
                return Err(CLOSED.to_string());
            }
            if !self.dynamic_content {
                return Err(PERSISTENCE_DISABLED.to_string());
            }
            if state.busy {
                return Err(PROMPT_ACTIVE.to_string());
            }
            state.current.as_ref().is_some_and(|current| {
                !requested.is_empty() && requested == current.canonical_path()
            })
        };
        if is_current {
            return self.archive_current_session().await;
        }
        let session_root = self
            .builder
            .storage
            .session_root(&self.builder.session_root)
            .map_err(|error| self.builder.redact_error(&error, None))?;
        self.builder
            .storage
            .sessions()
            .and_then(|repository| {
                repository.archive(&session_root, &self.builder.workspace_path, path)
            })
            .map_err(|error| self.builder.redact_error(&error.to_string(), None))
    }

    /// The replacement is built before the archive move, so every failure path
    /// leaves the current session intact. The move is the last and only
    /// committed state change.
    ///
    /// Archiving ends the session, so its outstanding timers end with it:
    /// `sessionfs::archive` removes the sidecar and the registry is cleared
    /// here. The clear runs only after the move succeeds, because a refused
    /// archive leaves the session running and its timers must survive. It also
    /// runs before the swap, so a timer that fires between the two cannot write
    /// the sidecar back.
    pub async fn archive_current_session(&self) -> Result<ArchiveResult, String> {
        let admission = self.begin_replacement()?;
        let (session, path, info) = {
            let state = self.lock();
            let current = state.current.as_ref().ok_or_else(|| CLOSED.to_string())?;
            (
                current.session.clone(),
                current.session.path(),
                current.info.clone(),
            )
        };
        if path.is_empty() {
            return Err(PERSISTENCE_DISABLED.to_string());
        }
        let runtime = self.current_runtime_from_info(info)?;
        let replacement = self.fresh_replacement(&runtime).await?;
        match session.archive(&self.builder.session_root, &self.builder.workspace_path) {
            Ok(result) => {
                if let Some(reminders) = self.reminders() {
                    reminders.clear();
                }
                self.commit(replacement, admission)?;
                Ok(result)
            }
            Err(error) => {
                let _ = replacement.discard();
                Err(self
                    .builder
                    .redact_error(&error.to_string(), Some(&runtime)))
            }
        }
    }

    // ---- tasks and wake ----

    /// The current runner's task registry, or `None` when it tracks none or the
    /// controller is closed.
    pub fn tasks(&self) -> Option<Arc<dyn TaskView>> {
        let state = self.lock();
        if state.closed {
            return None;
        }
        tasks::task_view(state.current.as_ref()?.runner.as_ref())
    }

    /// Accepts guidance for the exact active run, at its next safe provider
    /// boundary. The acceptance is transient and never wakes a later run.
    pub fn steer(&self, text: &str) -> Result<otto_core::agent::steering::Message, String> {
        self.steer_input(text, None)
    }

    /// [`Self::steer`] with the client's `input_id`: a retry while the
    /// guidance is queued returns the same message, and one after it was
    /// delivered fails with [`task_control::DUPLICATE_INPUT`].
    pub fn steer_input(
        &self,
        text: &str,
        input_id: Option<&str>,
    ) -> Result<otto_core::agent::steering::Message, String> {
        let text = task_control::checked_text(text)?;
        if input_id.is_some_and(|id| self.has_input(id)) {
            return Err(task_control::DUPLICATE_INPUT.into());
        }
        if self.pending_approval().is_some() {
            return Err("the task is waiting for approval; guidance is not accepted yet".into());
        }
        let runner = self.runner()?;
        let Some(ticket) = runner.steering().ticket() else {
            return Err("there is no active task accepting guidance".to_string());
        };
        let session_id = runner.session().header().id.clone();
        runner
            .task_control()
            .steer(&session_id, ticket, text, input_id)
    }

    /// Lists the transient steering lifecycle for the current active run.
    pub fn steering_messages(&self) -> Result<Vec<otto_core::agent::steering::Message>, String> {
        Ok(self.runner()?.steering().messages())
    }

    /// Withdraws one queued steering message before its safe-point delivery.
    pub fn withdraw_steering(
        &self,
        id: u64,
    ) -> Result<otto_core::agent::steering::Message, String> {
        self.runner()?
            .steering()
            .withdraw(id)
            .ok_or_else(|| "steering message is not queued for the active task".to_string())
    }

    /// Deterministically admits ordinary human text while this session is
    /// active. It never queues a later turn: callers explicitly choose that
    /// separate operation after receiving [`ActiveInputAdmission::Idle`].
    pub fn admit_active_input(&self, text: &str) -> Result<ActiveInputAdmission, String> {
        self.admit_active_input_with(text, None)
    }

    /// [`Self::admit_active_input`] with the client's `input_id`; see
    /// [`Self::steer_input`].
    pub fn admit_active_input_with(
        &self,
        text: &str,
        input_id: Option<&str>,
    ) -> Result<ActiveInputAdmission, String> {
        let text = task_control::checked_text(text)?;
        if input_id.is_some_and(|id| self.has_input(id)) {
            return Err(task_control::DUPLICATE_INPUT.into());
        }
        if self.pending_approval().is_some() {
            return Ok(ActiveInputAdmission::ApprovalPending);
        }
        let runner = self.runner()?;
        let Some(ticket) = runner.steering().ticket() else {
            return Ok(ActiveInputAdmission::Idle);
        };
        let session_id = runner.session().header().id.clone();
        match runner
            .task_control()
            .steer(&session_id, ticket, text, input_id)
        {
            Ok(_) => {
                runner.queue_user_authorization(text)?;
                Ok(ActiveInputAdmission::Steered)
            }
            // The active run can close after observing its ticket. This text
            // was not accepted and must not be silently queued elsewhere.
            Err(error) if error.contains("active task") => Ok(ActiveInputAdmission::Idle),
            Err(error) => Err(error),
        }
    }

    /// Routes natural input during an active turn through a restricted control
    /// dialogue. `None` means the caller should handle it as a normal prompt.
    pub async fn control_message_with_channel(
        &self,
        text: &str,
        channel: Option<Channel>,
        cancel: &CancellationToken,
    ) -> Result<Option<approval_control::ControlReply>, String> {
        if let Some(reply) = self
            .approval_message_with_channel(text, channel, cancel)
            .await?
        {
            return Ok(Some(reply));
        }
        let runner = self.runner()?;
        let Some(ticket) = runner.steering().ticket() else {
            return Ok(None);
        };
        let _serial = tokio::select! {
            guard = self.approval_control.lock() => guard,
            () = cancel.cancelled() => return Err("context canceled".into()),
            () = self.auto_cancel.cancelled() => return Err(CLOSED.into()),
        };
        if self.pending_approval().is_some() {
            drop(_serial);
            return self
                .approval_message_with_channel(text, channel, cancel)
                .await;
        }
        let is_current = self
            .lock()
            .current
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(&current.runner, &runner));
        if !is_current || runner.steering().ticket() != Some(ticket) {
            return Ok(None);
        }
        runner
            .record_user_authorization(text)
            .await
            .map_err(|error| error.to_string())?;
        task_control::control_message(self, &runner, text, channel, ticket, cancel).await
    }

    /// The current runner's timer registry, or `None` when the timer tools
    /// are not registered or the controller is closed. `/timers` lists it and
    /// [`Self::archive_current_session`] clears it.
    pub fn reminders(&self) -> Option<Arc<Reminders>> {
        let state = self.lock();
        if state.closed {
            return None;
        }
        state.current.as_ref()?.runner.reminders.clone()
    }

    /// Claims a turn only when the runner has pending inbox messages.
    pub fn prepare_wake(&self) -> Result<Option<WakeOperation<'_>>, String> {
        let pending = {
            let state = self.lock();
            if state.closed {
                return Err(CLOSED.to_string());
            }
            if state.busy {
                return Err(PROMPT_ACTIVE.to_string());
            }
            let Some(current) = state.current.as_ref() else {
                return Err(CLOSED.to_string());
            };
            current.runner.mailbox().len()
        };
        if pending == 0 {
            return Ok(None);
        }
        let admission = self.begin_operation()?;
        self.lock().wake_unstarted = true;
        Ok(Some(WakeOperation::new(self, admission)))
    }

    /// Queues user input for the active turn's next safe checkpoint.
    pub fn queue_user_message(&self, text: &str) -> bool {
        let text = text.trim();
        if text.is_empty() {
            return false;
        }
        let runner = {
            let state = self.lock();
            if state.closed || !state.busy {
                return false;
            }
            let Some(current) = state.current.as_ref() else {
                return false;
            };
            Arc::clone(&current.runner)
        };
        if runner.queue_user_authorization(text).is_err() {
            return false;
        }
        runner.mailbox().push(Notification {
            kind: Some(NotificationKind::UserMessage),
            text: text.to_string(),
            ..Notification::default()
        });
        true
    }

    /// Withdraws the TUI's one pending in-turn user message, if still queued.
    pub fn withdraw_user_message(&self) -> bool {
        let runner = {
            let state = self.lock();
            let Some(current) = state.current.as_ref() else {
                return false;
            };
            Arc::clone(&current.runner)
        };
        runner
            .mailbox()
            .remove("", NotificationKind::UserMessage)
            .is_some()
    }

    pub async fn reload_sandbox(&self) -> Result<SandboxInfo, String> {
        let control = {
            let state = self.lock();
            if state.closed {
                return Err(CLOSED.to_string());
            }
            let Some(control) = self.sandbox.clone() else {
                return Err(SANDBOX_RELOAD_UNAVAILABLE.to_string());
            };
            if state.busy {
                return Err(PROMPT_ACTIVE.to_string());
            }
            control
        };
        control.reload().await
    }

    /// Resolves a user-supplied `/sandbox allow` path against this process's
    /// home directory and the filesystem, without changing anything.
    pub fn resolve_sandbox_read_path(&self, input: &str) -> Result<String, String> {
        sandbox_config::resolve_read_path(input, &self.builder.home)
    }

    /// Persists one `[sandbox]` change to the configuration file and applies
    /// it to the live sandbox.
    ///
    /// Nothing is written unless a sandbox control exists and no turn is
    /// running, and a reload that fails puts the previous configuration back,
    /// so the file on disk always describes a sandbox this process accepted.
    /// Only `read_paths` and `network` are offered: an `allow_env` change
    /// cannot be reloaded into a running process at all.
    pub async fn amend_sandbox(&self, change: SandboxChange) -> Result<SandboxInfo, String> {
        let _admission = self.begin_operation()?;
        self.amend_sandbox_with_admission(change).await
    }

    async fn amend_sandbox_with_admission(
        &self,
        change: SandboxChange,
    ) -> Result<SandboxInfo, String> {
        let control = {
            let state = self.lock();
            if state.closed {
                return Err(CLOSED.to_string());
            }
            let Some(control) = self.sandbox.clone() else {
                return Err(SANDBOX_RELOAD_UNAVAILABLE.to_string());
            };
            control
        };
        let path = self.builder.config_path.clone();
        let amendment = sandbox_config::amend_sandbox_config(&path, &change)?;
        match control.reload().await {
            Ok(info) => Ok(info),
            Err(message) => Err(
                match sandbox_config::revert_sandbox_config(&path, &amendment) {
                    Ok(()) => format!("{message}; the configuration change was rolled back"),
                    Err(failure) => format!(
                        "{message}; the configuration change could not be rolled back: {failure}"
                    ),
                },
            ),
        }
    }

    fn admitted_session_id(&self) -> Result<String, String> {
        let state = self.lock();
        if state.closed {
            return Err(CLOSED.to_string());
        }
        Ok(state
            .current
            .as_ref()
            .ok_or_else(|| CLOSED.to_string())?
            .session
            .header()
            .id)
    }

    fn bash_approvals(&self) -> Result<&Arc<crate::tool::bash::BashApprovals>, String> {
        self.builder
            .bash_approvals
            .as_ref()
            .ok_or_else(|| "temporary elevation is unavailable".to_string())
    }

    /// Called only by human/client decision handlers, never by model tools.
    pub fn reserve_approval(&self, id: &str) -> Result<(), String> {
        self.bash_approvals()?
            .reserve(&self.info().session_id, id)
            .map_err(str::to_string)
    }

    /// Applies an approved sandbox read grant or approves one elevated command.
    pub async fn approve_tool(&self, id: &str) -> Result<String, String> {
        let _admission = self.begin_operation()?;
        let session_id = self.admitted_session_id()?;
        self.approve_tool_for_session(&session_id, id).await
    }

    pub(crate) async fn approve_tool_for_session(
        &self,
        session_id: &str,
        id: &str,
    ) -> Result<String, String> {
        let approvals = self.bash_approvals()?;
        let _decision = approvals.decision.lock().await;
        let path = approvals
            .pending_read_path(session_id, id)
            .ok_or_else(|| "approval request not found".to_string())?;
        if path.is_empty() {
            return self.approve_bash_for_session(session_id, id);
        }
        // A symlink must still resolve to the path the user reviewed.
        if self.resolve_sandbox_read_path(&path)? != path {
            return Err("sandbox read path changed; request approval again".to_string());
        }
        self.amend_sandbox_with_admission(SandboxChange::AllowReadPath(path))
            .await?;
        approvals.approve(session_id, id).map_err(str::to_string)?;
        Ok(format!(
            "The user approved {id} for persistent sandbox read access. Retry the same Bash call inside the sandbox now."
        ))
    }

    pub fn deny_tool(&self, id: &str) -> Result<(), String> {
        let _admission = self.begin_operation()?;
        let session_id = self.admitted_session_id()?;
        self.deny_tool_for_session(&session_id, id)
    }

    pub(crate) fn deny_tool_for_session(&self, session_id: &str, id: &str) -> Result<(), String> {
        self.bash_approvals()?.discard(session_id, id);
        Ok(())
    }

    /// Grants one pending elevated Bash command for the current session.
    pub fn approve_bash(&self, id: &str) -> Result<String, String> {
        let _admission = self.begin_operation()?;
        let session_id = self.admitted_session_id()?;
        self.approve_bash_for_session(&session_id, id)
    }

    fn approve_bash_for_session(&self, session_id: &str, id: &str) -> Result<String, String> {
        let approvals = self.bash_approvals()?;
        if approvals
            .pending_read_path(session_id, id)
            .is_some_and(|path| !path.is_empty())
        {
            return Err("this request requires sandbox read approval".to_string());
        }
        approvals.approve(session_id, id).map_err(str::to_string)?;
        Ok(format!(
            "The user approved {id} for one exact command. Retry the same elevated Bash command now."
        ))
    }

    /// Excludes the exact pending command from the sandbox, then grants the
    /// pending command once. Nothing is written when
    /// `excluded_command_entry` refuses the command.
    pub async fn approve_bash_always(&self, id: &str) -> Result<String, String> {
        let _admission = self.begin_operation()?;
        let session_id = self.admitted_session_id()?;
        self.approve_bash_always_for_session(&session_id, id).await
    }

    async fn approve_bash_always_for_session(
        &self,
        session_id: &str,
        id: &str,
    ) -> Result<String, String> {
        let approvals = self.bash_approvals()?;
        if approvals
            .pending_read_path(session_id, id)
            .is_some_and(|path| !path.is_empty())
        {
            return self.approve_tool_for_session(session_id, id).await;
        }
        let _decision = approvals.decision.lock().await;
        let command = approvals
            .pending_command(session_id, id)
            .ok_or_else(|| "approval request not found".to_string())?;
        let entry = otto_core::config::sandbox::excluded_command_entry(&command)
            .map_err(|reason| {
                format!(
                    "cannot exclude this command from the sandbox: {reason}; use /approve <id> instead for one run, or /sandbox exclude '<program> *' to exclude a program deliberately"
                )
            })?;
        self.amend_sandbox_with_admission(SandboxChange::ExcludeCommand(entry.clone()))
            .await?;
        approvals.approve(session_id, id).map_err(str::to_string)?;
        Ok(format!(
            "The user approved {id} and excluded '{entry}' from the sandbox. Retry the same Bash command now."
        ))
    }

    // ---- replacement helpers ----

    pub(crate) fn current_runtime(&self) -> Result<Runtime, String> {
        let info = self
            .lock()
            .current
            .as_ref()
            .map(|current| current.info.clone())
            .ok_or_else(|| CLOSED.to_string())?;
        self.current_runtime_from_info(info)
    }

    fn current_runtime_from_info(&self, info: RuntimeInfo) -> Result<Runtime, String> {
        let mut runtime = self.replacement_runtime(&metadata_of(&info))?;
        runtime.thinking = info.thinking;
        Ok(runtime)
    }

    /// The two gates before every replacement: persistence must be enabled, and
    /// the boundary must be open both before and after resolving.
    fn replacement_runtime(&self, metadata: &RuntimeMetadata) -> Result<Runtime, String> {
        if !self.dynamic_content {
            return Err(PERSISTENCE_DISABLED.to_string());
        }
        resolve_replacement(&self.builder, metadata)
    }

    /// A new session and runner for an already-resolved runtime. Every failure
    /// closes what it built.
    async fn fresh_replacement(&self, runtime: &Runtime) -> Result<Current, String> {
        let session = self.builder.create_session(runtime)?;
        attach_runner(&self.builder, session, runtime).await
    }

    /// The resume path: pin the named file, activate it, and build a runner
    /// for the runtime its header records.
    async fn resumed_replacement(&self, path: &Path) -> Result<(Current, Vec<String>), String> {
        if !self.dynamic_content {
            return Err(PERSISTENCE_DISABLED.to_string());
        }
        open_current(&self.builder, path).await
    }

    /// Installs the replacement and closes what it displaced. A close that
    /// arrived while the replacement was building closes the replacement too.
    fn commit(&self, replacement: Current, admission: Admission<'_>) -> Result<String, String> {
        let path = replacement.path();
        let (displaced, deferred) = {
            let mut state = self.lock();
            if state.closed {
                (Some(replacement), true)
            } else {
                (state.current.replace(replacement), false)
            }
        };
        let close_error = displaced.and_then(|current| current.discard().err());
        drop(admission);
        if deferred {
            return Err(CLOSED.to_string());
        }
        self.bind_reflection();
        self.notify_memory_state();
        match close_error {
            Some(error) => Err(self.builder.redact_error(&error, None)),
            None => Ok(path),
        }
    }

    // ---- the `otto serve` factories ----

    /// A controller over a brand new session. There is nothing to replace, so
    /// the built session and runner become the controller's first [`Current`]
    /// directly.
    ///
    /// The caller has already confirmed the redaction boundary is open, so
    /// `dynamic_content` is true for every controller built this way.
    pub async fn create(builder: Arc<Builder>, runtime: &Runtime) -> Result<Self, String> {
        let session = builder.create_session(runtime)?;
        let current = attach_runner(&builder, session, runtime).await?;
        Ok(Self::from_current(builder, current))
    }

    /// A controller over the session file at `path`, with the repair warnings
    /// activating it produced.
    pub async fn open(builder: Arc<Builder>, path: &Path) -> Result<(Self, Vec<String>), String> {
        let (current, warnings) = open_current(&builder, path).await?;
        Ok((Self::from_current(builder, current), warnings))
    }

    fn from_current(builder: Arc<Builder>, current: Current) -> Self {
        let controller = Self {
            builder,
            dynamic_content: true,
            sandbox: None,
            state: Mutex::new(State {
                current: Some(current),
                ..State::default()
            }),
            close_signal: Condvar::new(),
            notices: Arc::new(auto_reflection::Notices::new()),
            wake_replies: auto_reflection::Notices::new(),
            auto_running: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            auto_cancel: CancellationToken::new(),
            approval_control: tokio::sync::Mutex::new(()),
            bell: std::sync::atomic::AtomicBool::new(false),
            herdr: std::sync::RwLock::new(None),
        };
        controller.notify_memory_state();
        controller.bind_reflection();
        controller
    }
}

/// The boundary half of the replacement gates, without a controller to read
/// `dynamic_content` from: the persistence gate stays with the caller.
fn resolve_replacement(builder: &Builder, metadata: &RuntimeMetadata) -> Result<Runtime, String> {
    if !builder.boundary_allows_dynamic(None) {
        return Err(SESSION_OPERATION_UNAVAILABLE.to_string());
    }
    let runtime = builder.resolve_session(metadata)?;
    if !builder.boundary_allows_dynamic(Some(&runtime)) {
        return Err(SESSION_OPERATION_UNAVAILABLE.to_string());
    }
    Ok(runtime)
}

/// Builds the runner for `session` and pairs them into a [`Current`]. Every
/// failure closes what it built.
async fn attach_runner(
    builder: &Arc<Builder>,
    session: SharedSession,
    runtime: &Runtime,
) -> Result<Current, String> {
    let runner = match builder.build_runner_quiet(&session, runtime).await {
        Ok(runner) => Arc::new(runner),
        Err(error) => {
            let _ = session.close();
            return Err(error);
        }
    };
    if let Err(error) = builder.update_session_runtime(&session, runtime) {
        runner.close_mcp().await;
        runner.close();
        let _ = session.close();
        return Err(error);
    }
    Ok(Current::build(
        session,
        runner,
        builder.runtime_info(runtime),
    ))
}

/// Pins the session file at `path`, activates it, and builds a runner for the
/// runtime its header records.
async fn open_current(
    builder: &Arc<Builder>,
    path: &Path,
) -> Result<(Current, Vec<String>), String> {
    let create_lease = builder
        .create_lease()
        .map_err(|error| builder.redact_error(&error, None))?;
    // A lease-managed open can block up to 7*lease_seconds/6 (35s at the
    // default 30s lease) waiting out another host's holder; run it on a
    // blocking thread so it does not stall this async task's runtime worker.
    let session_root = builder
        .storage
        .session_root(&builder.session_root)
        .map_err(|error| builder.redact_error(&error, None))?;
    let storage = Arc::clone(&builder.storage);
    let workspace = builder.workspace_path.clone();
    let blocking_path = path.to_path_buf();
    let prepared = tokio::task::spawn_blocking(move || {
        storage.sessions()?.prepare_listed(
            &session_root,
            &workspace,
            &blocking_path.to_string_lossy(),
            create_lease,
        )
    })
    .await
    .map_err(|error| format!("session open task panicked: {error}"))?
    .map_err(|error| builder.redact_error(&error.to_string(), None))?;
    let info = prepared.info();
    let metadata = RuntimeMetadata {
        profile: info.profile.clone(),
        provider: info.provider.clone(),
        model: info.model.clone(),
    };
    let session_thinking = prepared.info().thinking.clone();
    let mut runtime = match resolve_replacement(builder, &metadata) {
        Ok(runtime) => runtime,
        Err(message) => {
            let _ = prepared.close();
            return Err(message);
        }
    };
    if !session_thinking.is_empty() {
        runtime.thinking = session_thinking;
    }
    let prepared = prepared.with_replayable(crate::tool::safety::is_replayable);
    let (session, warnings) = tokio::task::spawn_blocking(move || prepared.activate())
        .await
        .map_err(|error| format!("session activation task panicked: {error}"))?
        .map_err(|error| builder.redact_error(&error, Some(&runtime)))?;
    let warnings = warnings
        .into_iter()
        .map(|warning| builder.redact_error(&warning.message, Some(&runtime)))
        .collect();
    let session = SharedSession::new(session);
    let current = attach_runner(builder, session, &runtime).await?;
    Ok((current, warnings))
}

impl State {
    fn wake_claim_is_unstarted(&self) -> bool {
        self.wake_unstarted
    }
}

fn metadata_of(info: &RuntimeInfo) -> RuntimeMetadata {
    RuntimeMetadata {
        profile: info.profile.clone(),
        provider: info.provider.clone(),
        model: info.model.clone(),
    }
}

/// Absolute, then symlink-resolved, then lexically cleaned, falling back a step
/// at a time.
pub fn canonical_session_path(path: &str) -> String {
    if path.is_empty() {
        return String::new();
    }
    let path = Path::new(path);
    let absolute = match std::path::absolute(path) {
        Ok(absolute) => absolute,
        Err(_) => return clean(path),
    };
    match std::fs::canonicalize(&absolute) {
        Ok(canonical) => clean(&canonical),
        Err(_) => clean(&absolute),
    }
}

fn clean(path: &Path) -> String {
    sessionfs::clean_go_path(&path.to_string_lossy())
}

/// A controller whose builder carries bash approvals and a sandbox control,
/// for the frontends' `/approve <id> always` tests. Returns the approvals so a
/// test can create a pending request, the reload counter, and the approvals'
/// session id.
#[cfg(test)]
pub(crate) async fn controller_with_approvals(
    workspace: &std::path::Path,
    sessions: &std::path::Path,
) -> (
    Controller,
    Arc<crate::tool::bash::BashApprovals>,
    Arc<Mutex<usize>>,
) {
    controller_with_approvals_and_schedule(workspace, sessions, None).await
}

/// [`controller_with_approvals`] with a scheduled-delivery store.
#[cfg(test)]
pub(crate) async fn controller_with_approvals_and_schedule(
    workspace: &std::path::Path,
    sessions: &std::path::Path,
    schedule: Option<Arc<dyn crate::schedule::ScheduleStore>>,
) -> (
    Controller,
    Arc<crate::tool::bash::BashApprovals>,
    Arc<Mutex<usize>>,
) {
    use crate::cli::testutil::{FakeSandbox, builder, initial_runtime, seatbelt_info};
    use crate::runtime::info::SandboxNetwork;
    use crate::sandbox::direct::DirectDriver;
    use crate::sandbox::{Executor, FilesystemMode, NetworkMode, Policy};

    let executor = Executor::new(
        Arc::new(DirectDriver::new()),
        Policy {
            filesystem: FilesystemMode::Unconfined,
            network: NetworkMode::Allow,
        },
        workspace,
    )
    .expect("elevated executor");
    let approvals = Arc::new(crate::tool::bash::BashApprovals::new(
        Arc::new(executor),
        Vec::new(),
    ));
    let mut builder = builder(workspace, sessions);
    builder.bash_approvals = Some(Arc::clone(&approvals));
    builder.shared_mut().schedule = schedule;
    let runtime = initial_runtime(&builder);
    let session = builder.create_session(&runtime).expect("session");
    let runner = builder
        .build_runner(&session, &runtime)
        .await
        .expect("runner");
    let info = builder.runtime_info(&runtime);
    let (control, calls) = FakeSandbox::new(
        seatbelt_info(SandboxNetwork::Allowed),
        seatbelt_info(SandboxNetwork::Denied),
        None,
    );
    let controller =
        Controller::new(builder, true, session, runner, info).with_sandbox_control(control);
    (controller, approvals, calls)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::testutil::{
        FakeSandbox, builder, controller, initial_runtime, seatbelt_info, user,
    };
    use crate::runtime::info::SandboxNetwork;
    use crate::runtime::sandbox_config::SandboxChange;

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
        assert_eq!(settings.policy, crate::approval::ApprovalPolicy::Never);
        assert_eq!(settings.reviewer, crate::approval::ApprovalReviewer::User);
        assert_eq!(controller.info().sandbox, sandbox);
        assert!(controller.permissions_command("reviewer bypass").is_err());
        controller.auto_mode_command("auto").unwrap();
        let settings = controller.info().approval_settings;
        assert_eq!(settings.policy, crate::approval::ApprovalPolicy::OnRequest);
        assert_eq!(
            settings.reviewer,
            crate::approval::ApprovalReviewer::AutoReview
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
    async fn bash_approval_fails_closed_when_unavailable_or_busy() {
        let workspace = tempfile::tempdir().expect("workspace");
        let sessions = tempfile::tempdir().expect("sessions");
        let controller = controller(workspace.path(), sessions.path()).await;
        assert_eq!(
            controller.approve_bash("approval-1"),
            Err("temporary elevation is unavailable".to_string())
        );
        let _admission = controller.begin_operation().expect("admit turn");
        assert_eq!(
            controller.approve_bash("approval-1"),
            Err(PROMPT_ACTIVE.to_string())
        );
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
        assert!(controller.approve_bash(&id).is_err());
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
    async fn resuming_a_session_keeps_the_session_thinking_level() {
        let workspace = tempfile::tempdir().expect("workspace");
        let sessions = tempfile::tempdir().expect("sessions");
        let controller = controller(workspace.path(), sessions.path()).await;
        controller
            .current_session()
            .append(user("hello"))
            .await
            .expect("append");

        controller.set_thinking("high").await.expect("thinking");
        let path = controller.info().session_path;
        assert!(!path.is_empty());
        controller.close().expect("close");

        let builder = builder(workspace.path(), sessions.path());
        let (reopened, warnings) = Controller::open(Arc::new(builder), Path::new(&path))
            .await
            .expect("open");

        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(reopened.info().thinking, "high");
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
    fn lease_heartbeat_bytes(
        epoch: u64,
        host: &str,
        pid: u32,
        released: bool,
        seq: u64,
    ) -> Vec<u8> {
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
        let (subagents, _) =
            crate::subagent::runner::Runner::new(child_config).expect("valid config");
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
    async fn resuming_a_session_held_by_another_renewing_holder_fails_and_keeps_the_current_session()
     {
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
        let operation = controller.begin_operation().expect("admit");
        assert!(controller.queue_user_message("change course"));
        drop(operation);
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
            &store.path().join("m.db"),
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

        controller.close().expect("close");
        let reopened = crate::cli::testutil::controller_with_memory(
            workspace.path(),
            sessions.path(),
            &store.path().join("m.db"),
        )
        .await;
        assert_eq!(reopened.take_notices().len(), 1);
    }

    #[tokio::test]
    async fn session_open_queues_the_over_budget_standing_notice() {
        let workspace = tempfile::tempdir().expect("workspace");
        let sessions = tempfile::tempdir().expect("sessions");
        let store = tempfile::tempdir().expect("store");
        let path = store.path().join("m.db");
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
}
