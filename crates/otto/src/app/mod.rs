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
//! against the constants below, the way `otto_cli::run` already compares
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
                    if let Some(calls) = pending.get_mut(&call_id)
                        && !calls.is_empty()
                    {
                        calls.remove(0);
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
pub mod channel;
mod input_submissions;
mod local_commands;
pub mod management;
pub mod task_control;
pub mod turn_queue;

/// Fixed, content-free frontend notice for a successful memory proposal.
pub const MEMORY_REVIEW_AVAILABLE: &str = "Memory review available. Run /memory review.";
mod inspection;
mod lifecycle;
use lifecycle::State;
mod model_selection;
pub mod notices;
mod prompt;
mod replacement;
mod reporting;
mod runtime_control;
mod sandbox_approval;
pub mod scheduled;
mod session_delete;
mod session_ops;
mod steering;
mod task_observer;
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
use crate::sandbox::config_edit::{self as sandbox_config, SandboxChange};
use crate::sandbox::info::SandboxInfo;
use crate::schedule::remind::Reminders;
use crate::session::{self as sessionfs, ArchiveResult, MAX_LIST_SESSIONS};
pub use channel::Channel;

use crate::sandbox::control::SandboxControl;
pub use approval::{ApprovalDecision, ApprovalRequest, Step, Stop};
use otto_core::approval::ApprovalMode;
pub use tasks::{Task, TaskStatus, TaskView};
pub use wake::WakeOperation;

pub const PERSISTENCE_DISABLED: &str = "session persistence is disabled";
pub const MODEL_SWITCH_UNAVAILABLE: &str = "model switching is not available";
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

/// One configured model row a frontend can display.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ModelSummary {
    /// `<provider>/<model_id>`.
    pub selector: String,
    pub provider: String,
    pub provider_type: String,
    pub model_id: String,
    pub thinking: String,
}

/// What a frontend may display.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Info {
    pub local_commands: Vec<otto_core::session::local_command::LocalCommandReceipt>,
    pub session_id: String,
    pub session_name: String,
    pub session_path: String,
    pub workspace: String,
    /// The provider name from the selector.
    pub provider: String,
    pub provider_type: String,
    /// `<provider>/<model_id>`.
    pub model_selector: String,
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

/// What `/permissions` (and its hidden alias `/auto`) asks to change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionsChange {
    /// `manual`, `auto` or `yolo`: replaces policy and reviewer together.
    Preset(ApprovalMode),
    /// `policy ...` or `reviewer ...`: the new policy and reviewer pair.
    Settings(
        otto_core::approval::ApprovalPolicy,
        otto_core::approval::ApprovalReviewer,
    ),
}

pub const PERMISSIONS_USAGE: &str =
    "usage: /permissions [manual|auto|yolo | policy on_request|never | reviewer user|auto_review]";

/// Parses `/permissions` arguments; `Ok(None)` means show the report only.
/// `on` and `off` are undocumented synonyms of `auto` and `manual`.
pub fn parse_permissions_command(
    args: &str,
    current: otto_core::approval::ApprovalSettings,
) -> Result<Option<PermissionsChange>, String> {
    use PermissionsChange::{Preset, Settings};
    use otto_core::approval::{ApprovalPolicy as Policy, ApprovalReviewer as Reviewer};
    match args.split_whitespace().collect::<Vec<_>>().as_slice() {
        [] => Ok(None),
        ["manual" | "off"] => Ok(Some(Preset(ApprovalMode::Manual))),
        ["auto" | "on"] => Ok(Some(Preset(ApprovalMode::Auto))),
        ["yolo"] => Ok(Some(Preset(ApprovalMode::Yolo))),
        ["policy", "on_request"] => Ok(Some(Settings(Policy::OnRequest, current.reviewer))),
        ["policy", "never"] => Ok(Some(Settings(Policy::Never, current.reviewer))),
        ["reviewer", "user"] => Ok(Some(Settings(current.policy, Reviewer::User))),
        ["reviewer", "auto_review"] => Ok(Some(Settings(current.policy, Reviewer::AutoReview))),
        _ => Err(PERMISSIONS_USAGE.into()),
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
    let mode = if settings.policy == ApprovalPolicy::Never {
        "NEVER (reviewer disabled)"
    } else {
        settings.mode().label()
    };
    format!(
        "Approval mode: {mode}\nApproval policy: {policy}\nReviewer: {reviewer}{}\nRevision: {}",
        if settings.policy == ApprovalPolicy::Never {
            " (disabled)"
        } else {
            ""
        },
        settings.revision
    )
}

/// Accepts the thinking levels `/thinking` and the session thinking route take.
pub fn validate_thinking_arg(thinking: &str) -> Result<(), String> {
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

pub struct Controller {
    builder: Arc<Builder>,
    /// False when the redaction boundary is closed. Every operation that would
    /// persist or display provider identity is then refused.
    dynamic_content: bool,
    sandbox: Option<Arc<dyn SandboxControl>>,
    state: Mutex<State>,
    close_signal: Condvar,
    /// Cancels pending controller operations during close.
    close_cancel: CancellationToken,
    /// Lines from background operations for a frontend to show.
    notices: Arc<notices::Notices>,
    /// Model-written texts of finished wake turns, for ACP clients.
    wake_replies: notices::Notices,
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
    observer_cache: task_observer::ObserverCache,
}

/// Clears passive presentation routing when one logical user turn ends.
pub(crate) struct PresentationOriginGuard(Arc<crate::session::origin::TurnOrigin>);

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
    /// The composition-root constructor `otto_cli::run` uses.
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
            state: Mutex::new(State::new(Current::build(session, Arc::new(runner), info))),
            close_signal: Condvar::new(),
            close_cancel: CancellationToken::new(),
            notices: Arc::new(notices::Notices::new()),
            wake_replies: notices::Notices::new(),
            approval_control: tokio::sync::Mutex::new(()),
            bell: std::sync::atomic::AtomicBool::new(false),
            herdr: std::sync::RwLock::new(None),
            trace_cache: trace::TraceCache::default(),
            observer_cache: task_observer::ObserverCache::default(),
        };
        controller.notify_memory_state();
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
}

fn metadata_of(info: &RuntimeInfo) -> RuntimeMetadata {
    RuntimeMetadata {
        profile: info.model_selector.clone(),
        provider: info.provider_type.clone(),
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
    use crate::fixtures::{FakeSandbox, builder, initial_runtime, seatbelt_info};
    use crate::sandbox::direct::DirectDriver;
    use crate::sandbox::info::SandboxNetwork;
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
