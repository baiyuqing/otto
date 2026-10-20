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

/// Add read-only identity to pending transcript calls from durable facts.
fn project_invocation_identity(
    messages: &mut [Message],
    ledger: &otto_core::session::OperationLedger,
) {
    // Only unmatched calls need the ledger: completed calls carry
    // their precise identity in result metadata, even when provider
    // call IDs were reused. This is a read-only projection.
    let mut pending: std::collections::HashMap<String, Vec<(usize, usize)>> =
        std::collections::HashMap::new();
    for message in messages.iter_mut() {
        for block in &mut message.blocks {
            if let Some(record) = block
                .operation_metadata
                .as_ref()
                .and_then(|metadata| metadata.operation_id.as_ref())
                .and_then(|id| ledger.operation(id))
            {
                block.invocation_id = record.operation_id.to_string();
                block.invocation_attempt = Some(record.attempts);
            }
        }
    }
    for (mi, message) in messages.iter().enumerate() {
        for (bi, block) in message.blocks.iter().enumerate() {
            let call_id = block.tool_call_id.clone();
            match block.block_type {
                otto_core::model::BlockType::ToolCall => {
                    pending.entry(call_id).or_default().push((mi, bi));
                }
                otto_core::model::BlockType::ToolResult => {
                    if let Some(calls) = pending.get_mut(&call_id) {
                        if !calls.is_empty() {
                            calls.remove(0);
                        }
                    }
                }
                _ => {}
            }
        }
    }
    for (call_id, calls) in pending {
        if let Some(record) = ledger.operation_for_tool_call(&call_id) {
            // The ledger owns the latest provider ID binding only.
            if let Some((mi, bi)) = calls.last() {
                let block = &mut messages[*mi].blocks[*bi];
                block.invocation_id = record.operation_id.to_string();
                block.invocation_attempt = Some(record.attempts);
            }
        }
    }
}

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
mod inspection;
mod profile;
mod prompt;
mod replacement;
mod reporting;
mod runtime_control;
pub mod sandbox;
mod sandbox_approval;
pub(crate) mod scheduled;
mod session_ops;
mod steering;
pub mod tasks;
mod trace;
pub mod wake;

use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::Duration;

use otto_core::agent::inbox::NotificationKind;
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
    pub approval_settings: otto_core::approval::ApprovalSettings,
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
    current: otto_core::approval::ApprovalSettings,
) -> Result<
    Option<(
        otto_core::approval::ApprovalPolicy,
        otto_core::approval::ApprovalReviewer,
    )>,
    String,
> {
    use otto_core::approval::{ApprovalPolicy as Policy, ApprovalReviewer as Reviewer};
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

pub fn format_permissions(settings: otto_core::approval::ApprovalSettings) -> String {
    use otto_core::approval::{ApprovalPolicy, ApprovalReviewer};
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
    /// Parsed session files read for the trace.
    trace_cache: trace::TraceCache,
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
            trace_cache: trace::TraceCache::default(),
        };
        controller.notify_memory_state();
        controller.bind_reflection();
        controller
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
mod tests;

#[cfg(test)]
mod invocation_projection_tests;
