//! The sub-agent runner: it starts child agent loops, tracks them in the
//! session's task registry, and renders their completion text.
//!
//! Ownership: a [`Runner`] owns the child tool registry it builds from
//! [`Config::tools`]. The task registry, the provider and the clock are shared
//! with the parent through `Arc`. Each child owns its own transcript: a session
//! file beside the parent's when [`Config::child_session`] returns one, and an
//! in-memory session otherwise.
//!
//! Concurrency and cancellation: every child runs in its own Tokio task,
//! admitted by a semaphore that caps [`Config::max_parallel`] concurrent
//! children. The semaphore belongs to the session's [`Tasks`]
//! ([`Tasks::child_slots`]), so runners rebuilt on one session share the cap.
//! Each task's [`CancellationToken`] is held by the registry, so
//! cancelling it either releases a queued child before it starts or stops a
//! running one mid-turn. [`Runner::start`] itself never blocks.
//!
//! Security: a child never receives the agent-control or memory tools, so it
//! can neither start children of its own nor read or write long-term memory. It
//! also never receives `remind`, which wakes the parent session. A definition's
//! `tools` list can only narrow the set the runner already built, never widen
//! it. Definition bodies are untrusted text, appended to the child's system
//! prompt inside a fixed `<subagent_role>` element.
//!
//! Shapes forced by the ownership contracts:
//!
//! - [`Options`] holds boxed closures and is not `Clone`, so
//!   [`OptionsTemplate`] carries the copyable settings plus the shared clock
//!   and id generator, and each child builds its own `Options` from it.
//! - The provider sits behind [`SharedProvider`], because [`Agent`] owns its
//!   provider.
//! - [`Registry`] owns boxed tools and cannot be subset, so a child gets a
//!   [`ChildTools`] view over one shared child registry plus an allowlist.
//! - A persisted child transcript is named after its task id, which exists
//!   only after `Tasks::add`, so the transcript is chosen after the task is
//!   added and the registry's history hook reads it through a `OnceLock`. A
//!   transcript that cannot be created fails only that task.
//! - `Session::append` is async, so an inherited snapshot is replayed at the
//!   top of the child task rather than inside `start`; an invalid snapshot
//!   still fails the task with the same message, just asynchronously.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use chrono::{DateTime, Utc};
use otto_core::agent::inbox::{Inbox, Notification, NotificationKind};
use otto_core::agent::progress::ProgressPolicy;
use otto_core::agent::redactor::Redactor;
use otto_core::agent::{Agent, CompactionSettings, Event, EventSink, Options};
use otto_core::mailbox::Mailbox;
use otto_core::model::{Message, OperationStopReason, Role, ToolDefinition};
use otto_core::operation::OperationControl;
use otto_core::provider::{
    Provider, ProviderError, ProviderSettlement, Request, RequestSizer, StreamSink,
};
use otto_core::session::{MemorySession, Session};
use otto_core::tool::{ToolCall, ToolExecution, ToolExecutor, ToolResult};
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

use super::format::{comma_int, first_runes, one_line, round_to_seconds};
use super::interrupted;
use super::tasks::{REPORT_BUDGET, Task, TaskError, TaskStatus, Tasks};
use super::{Catalog, Definition, WritePolicy, inherit_snapshot};
use crate::deadline::{Control, Deadline};
use crate::failover;
use crate::tool::registry::Registry;
use crate::tool::result::{capped_text_result, decode_strict_json};
use crate::tool::{CONTEXT_CANCELED, Tool, definition, error_result, text_result};

/// Body budget for parent-facing child summaries; full transcripts stay local.
pub(crate) const PARENT_REPORT_BYTES: usize = 4096;

/// Tools a child never receives: the agent-control tools, because delegation
/// depth is fixed at one; the memory tools, because a child gets no memory
/// binding; the timer tools, which read and wake the parent session; the schedule
/// tools, which deliver to the parent conversation's origin; and `update_plan`,
/// because a child's plan would be shown nowhere; and delivery, which only a
/// parent may start and which would otherwise let a child reach another
/// workspace.
pub const EXCLUDED_CHILD_TOOLS: [&str; 22] = [
    // Bound independently by each ChildTools, never inherited from a parent catalog.
    "session_info",
    "zoom",
    "workflow_list",
    "workflow_inspect",
    "workflow_start",
    "workflow_status",
    "agent",
    "agent_status",
    "agent_yield",
    "agent_send",
    "agent_cancel",
    "remember",
    "forget",
    "memory_search",
    "remind",
    "remind_status",
    "remind_cancel",
    "schedule",
    "schedule_list",
    "schedule_cancel",
    "update_plan",
    "start_delivery_task",
];

/// Appended to a child's system prompt inside `<subagent_role>` when it has
/// no definition, or its definition's body is empty.
const GENERIC_SUBAGENT_INSTRUCTION: &str = "You are running as a sub-agent of Otto. Complete the entire delegated \
    change within its assigned file scope. Reuse the agreed shared \
    interfaces; report a needed contract change before expanding scope. \
    Batch known same-file edits and use checked scripts for repetitive \
    call-site changes. Complete only the delegated task below with the \
    available tools, then reply with a self-contained final report. That \
    final message is returned to the caller as your result. Do not send \
    progress updates, plans, or interim findings with agent_report; put \
    them in the final report instead. Call agent_report only to answer a \
    question that arrives in a [parent-message], or to report a blocker \
    that needs a parent decision while you can keep working on other parts \
    of the task. If you cannot continue, end with a final report that \
    states the blocker.";

const DEFAULT_MAX_PARALLEL: usize = 4;
const DEFAULT_MAX_OUTPUT_BYTES: usize = 16384;
const MAX_DESCRIPTION_CHARS: usize = 80;

/// The turn settings every child copies from the parent, and the two injected
/// capabilities [`Options`] needs.
pub struct OptionsTemplate {
    pub diagnostics: Option<Arc<dyn otto_core::diagnostics::Diagnostics>>,
    pub plugins: Option<Arc<crate::plugin::hooks::Runtime>>,
    pub hook_guard: Option<Arc<crate::plugin::hooks::SessionGuard>>,
    pub model: String,
    pub subagent_models: Vec<String>,
    pub provider_name: String,
    pub thinking: String,
    pub compaction: CompactionSettings,
    pub context_strategy: otto_core::config::context_strategy::ContextStrategy,
    /// The parent's progress limits. A child inherits them, so a stuck child
    /// stops on the same rule as the parent's own loop.
    pub progress: ProgressPolicy,
    pub request_sizer: Option<Arc<dyn RequestSizer + Send + Sync>>,
    pub provider_timeout: Option<Duration>,
    pub cancellation_grace: Duration,
    pub now: Arc<dyn Fn() -> DateTime<Utc> + Send + Sync>,
    pub new_id: Arc<dyn Fn() -> String + Send + Sync>,
    pub new_operation_id:
        Arc<dyn Fn() -> Result<otto_core::model::OperationId, String> + Send + Sync>,
    /// Absolute task budget, consumed from task creation while queued.
    pub task_timeout: Option<Duration>,
}

impl Default for OptionsTemplate {
    fn default() -> Self {
        Self {
            diagnostics: None,
            plugins: None,
            hook_guard: None,
            model: String::new(),
            subagent_models: Vec::new(),
            provider_name: String::new(),
            thinking: String::new(),
            compaction: CompactionSettings::default(),
            context_strategy: Default::default(),
            progress: ProgressPolicy::default(),
            request_sizer: None,
            provider_timeout: None,
            cancellation_grace: Duration::from_secs(5),
            now: Arc::new(Utc::now),
            new_id: Arc::new(String::new),
            new_operation_id: Arc::new(|| {
                otto_core::model::OperationId::new("op_test").map_err(|error| error.to_string())
            }),
            task_timeout: None,
        }
    }
}

/// Renders the parent's static system prompt for a tool set.
pub type PromptFor = Arc<dyn Fn(&[ToolDefinition]) -> String + Send + Sync>;

/// Returns the parent's current message list.
pub type ParentSession = Arc<dyn Fn() -> Vec<Message> + Send + Sync>;

/// Returns the parent's messages as they were before the turn that the quoted
/// user prompt opened, read from the parent's session file.
pub type ParentFork = Arc<dyn Fn(&str) -> Result<Vec<Message>, String> + Send + Sync>;

/// Everything a [`Runner`] needs from the parent runtime.
pub struct Config {
    pub approval: Option<(Arc<crate::approval::Service>, otto_core::approval::Scope)>,
    pub provider: Arc<dyn Provider + Send + Sync>,
    /// The parent's tool set; the runner drops [`EXCLUDED_CHILD_TOOLS`] by name
    /// to build the child registry. These are the children's own tool
    /// instances, not the parent registry's.
    pub tools: Vec<Box<dyn Tool + Send + Sync>>,
    /// The secret values the parent's redactor holds, and whether that set was
    /// complete. A child builds an equivalent redactor; none of them mutate
    /// the parent's.
    pub redaction_values: Vec<String>,
    pub redaction_complete: bool,
    pub template: OptionsTemplate,
    /// Renders the parent's static system prompt for a tool set. Its output is
    /// treated as already redacted by the caller.
    pub prompt_for: PromptFor,
    pub tasks: Arc<Tasks>,
    /// The named definitions; the default value means none.
    pub catalog: Catalog,
    /// Returns the parent's current message list, which is the post-compaction
    /// view it would send on its next request. `None` disables
    /// `context: inherit`.
    pub parent_session: Option<ParentSession>,
    /// Returns the parent's messages before the turn the quoted prompt opens.
    /// `None` when the parent has no session file, which makes `fork_before`
    /// fail.
    pub parent_fork: Option<ParentFork>,
    /// Caps concurrent children; zero means four.
    pub max_parallel: usize,
    /// Caps the report text inside notification and wait text; zero means
    /// 16384.
    pub max_output_bytes: usize,
    /// The parent session's usage collector. Each child binds its task id.
    pub usage: Option<crate::usage::Collector>,
    /// Builds a persisted transcript for each [`Runner::start`] child. `None`
    /// keeps every child in memory.
    pub child_session: Option<ChildSession>,
    /// The automatic skill contract check. `None` disables it: `AgentTool`
    /// then triggers no check, regardless of the definition delegated to.
    pub checker: Option<Arc<crate::skill::check::Checker>>,
    /// Locates the current session's children directory for [`Runner::resume`],
    /// read lazily because a lazy parent session has no path until its first
    /// write; returns `None` while that is still the case. `None` (the field
    /// itself) disables `Runner::resume`.
    pub children_dir: Option<Arc<dyn Fn() -> Option<PathBuf> + Send + Sync>>,
    /// Queried once per child tool call by the [`failover::CommitGuard`]
    /// installed on the child registry: the lease backing the parent
    /// session, or `None` when it is not lease-managed. `None` (the field
    /// itself) installs no guard, which is what every test config uses.
    pub lease: Option<failover::LeaseSource>,
    /// The workspace the commit guard syncs to disk after a child tool call
    /// finishes. Ignored when `lease` is `None`.
    pub workspace_path: PathBuf,
    /// Makes a file-backed child's mailbox durable beside its transcript, so
    /// `agent_send` messages it has not read survive a restart and reach it
    /// when it is resumed. `None` keeps child mailboxes in memory.
    pub child_mailbox: Option<ChildMailbox>,
}

/// One delegation request.
#[derive(Clone, Debug, Default)]
pub struct StartRequest {
    pub prompt: String,
    pub description: String,
    /// An optional task name, unique for the session including finished tasks;
    /// see [`Tasks::add`] for the validation rules. Empty means no name.
    pub name: String,
    /// The provider model id the child runs on. Empty, or whitespace only,
    /// falls back to the definition's model, then the template's.
    pub model: String,
    /// A catalog definition name, or empty for the default sub-agent.
    pub agent: String,
    /// `"fresh"` or `"inherit"`. Empty falls back to the definition's setting,
    /// then `"fresh"`.
    pub context: String,
    /// The text of an earlier user prompt of the parent: the child starts with
    /// the parent's messages before the turn that prompt opened, and `context`
    /// is `"inherit"`. Empty means none.
    pub fork_before: String,
    /// The parent session's tool call that starts the task, recorded in the
    /// child's `otto.task_spec`. Empty when no tool call started it.
    pub parent_tool_call_id: String,
}

/// Builds and runs sub-agent tasks for one session.
pub struct Runner {
    reports: super::reports::Reports,
    scratch_root: std::sync::OnceLock<crate::tool::root::Root>,
    /// Owning session of the scratch and report directories.
    scratch_session_id: std::sync::OnceLock<String>,
    output_logs: std::sync::OnceLock<Arc<crate::tool::bash_output::Logs>>,
    config: Config,
    child_registry: Arc<Registry>,
    semaphore: Arc<Semaphore>,
}

struct ChildRun {
    cancel: CancellationToken,
    task_id: String,
    prompt: String,
    child: Agent<SharedProvider, ChildTools, SharedTranscript>,
    transcript: Transcript,
    snapshot: Vec<Message>,
    deadline: Deadline,
}

struct StartGuard {
    runner: Arc<Runner>,
    task_id: String,
    now: DateTime<Utc>,
    cancel: CancellationToken,
    armed: bool,
    interrupted_on_drop: bool,
}

impl StartGuard {
    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for StartGuard {
    fn drop(&mut self) {
        if self.armed {
            self.cancel.cancel();
            self.runner.finish_without_transcript(
                &self.task_id,
                if self.interrupted_on_drop {
                    TaskStatus::Interrupted
                } else {
                    TaskStatus::Canceled
                },
                self.now,
                &[],
                Some("task start was canceled before child execution"),
            );
        }
    }
}

impl Runner {
    /// Validates `config`, builds the child tool registry, and resolves each
    /// definition's allowlist. The returned warnings name every definition
    /// tools entry that does not match a child tool.
    pub fn new(mut config: Config) -> Result<(Self, Vec<String>), String> {
        if config.max_parallel == 0 {
            config.max_parallel = DEFAULT_MAX_PARALLEL;
        }
        if config.max_output_bytes == 0 {
            config.max_output_bytes = DEFAULT_MAX_OUTPUT_BYTES;
        }

        let mut child_tools: Vec<Box<dyn Tool + Send + Sync>> = std::mem::take(&mut config.tools)
            .into_iter()
            .filter(|tool| !EXCLUDED_CHILD_TOOLS.contains(&tool.definition().name.as_str()))
            .collect();
        if let Some((service, scope)) = &config.approval {
            child_tools.push(Box::new(crate::tool::approval::RequestApproval {
                service: service.clone(),
                scope: scope.clone(),
                turn_origin: None,
            }));
        }
        let child_registry = Registry::new(child_tools)
            .map_err(|error| format!("subagent: child registry: {error}"))?;
        let child_registry = match &config.approval {
            Some((service, scope)) => child_registry.with_approval(service.clone(), scope.clone()),
            None => child_registry,
        };
        let child_registry = match &config.lease {
            Some(lease) => child_registry.with_guard(Arc::new(failover::CommitGuard::new(
                Arc::clone(lease),
                config.workspace_path.clone(),
            ))),
            None => child_registry,
        };
        let child_registry = Arc::new(child_registry);
        let child_names: BTreeSet<String> = child_registry
            .definitions()
            .into_iter()
            .map(|definition| definition.name)
            .collect();

        let mut warnings = Vec::new();
        for definition in config.catalog.definitions() {
            let Some(wanted) = &definition.tools else {
                continue;
            };
            for name in wanted {
                if !child_names.contains(name) {
                    warnings.push(format!(
                        "agent {}: unknown tool {name:?} ignored",
                        definition.name
                    ));
                }
            }
        }

        // The session's registry owns the permits, so every runner of the
        // session shares one `max_parallel` cap.
        let semaphore = config.tasks.child_slots(config.max_parallel);
        Ok((
            Self {
                reports: super::reports::Reports::default(),
                output_logs: std::sync::OnceLock::new(),
                scratch_root: std::sync::OnceLock::new(),
                scratch_session_id: std::sync::OnceLock::new(),
                config,
                child_registry,
                semaphore,
            },
            warnings,
        ))
    }

    pub(crate) fn bind_scratch_root(&self, root: crate::tool::root::Root, session_id: &str) {
        assert!(
            self.scratch_root.set(root).is_ok(),
            "scratch root already bound"
        );
        assert!(
            self.scratch_session_id.set(session_id.to_owned()).is_ok(),
            "scratch session already bound"
        );
    }

    /// Bound the parent preview while keeping a durable report pointer visible.
    pub(crate) fn parent_text(&self, task: &Task, text: &str) -> String {
        let cap = self.config.max_output_bytes.min(PARENT_REPORT_BYTES);
        let mut preview = capped_text_result(text, cap).content;
        if task.is_final() {
            preview.push_str(&self.reports.handoff(
                self.scratch_root.get(),
                &task.id,
                &self.redactor().redact_string(&task.result),
                text.len() > cap,
            ));
        }
        preview
    }

    pub(crate) fn bind_output_logs(&self, logs: Arc<crate::tool::bash_output::Logs>) {
        // Bound by native composition before any agent tool can start a task.
        assert!(
            self.output_logs.set(logs).is_ok(),
            "output logs already bound"
        );
    }

    /// The catalog this runner delegates against.
    pub fn catalog(&self) -> &Catalog {
        &self.config.catalog
    }

    /// The session's task registry.
    pub fn tasks(&self) -> &Arc<Tasks> {
        &self.config.tasks
    }

    /// The concurrent-child cap in force.
    pub fn max_parallel(&self) -> usize {
        self.config.max_parallel
    }

    /// The report-text cap in force.
    pub fn max_output_bytes(&self) -> usize {
        self.config.max_output_bytes
    }

    pub(crate) fn activity(&self, task: &Task) -> &'static str {
        if task.status == TaskStatus::Running
            && let Some((service, scope)) = &self.config.approval
        {
            let Ok(activity) = service.activity_snapshot(&scope.workspace, Some(&scope.session_id))
            else {
                return "unknown";
            };
            if activity
                .requests
                .iter()
                .any(|request| request.scope.task_id == task.id)
            {
                return "waiting_for_approval";
            }
            if activity
                .reviewing
                .iter()
                .any(|request| request.scope.task_id == task.id)
            {
                return "approval_review";
            }
        }
        self.config.tasks.activity(&task.id).unwrap_or("unknown")
    }

    /// The runner's clock, which the status tool uses for elapsed columns.
    pub fn now(&self) -> DateTime<Utc> {
        (self.config.template.now)()
    }

    /// The automatic skill contract checker, when enabled.
    pub fn checker(&self) -> Option<&Arc<crate::skill::check::Checker>> {
        self.config.checker.as_ref()
    }

    /// The tool definitions a child with no named definition would see.
    pub fn child_definitions(&self) -> Vec<ToolDefinition> {
        let mut definitions = self.child_registry.definitions();
        // Discovery needs the schema only; no catalog binds a runner identity.
        definitions.push(
            crate::tool::session_info::SessionInfo {
                session_id: String::new(),
            }
            .definition(),
        );
        definitions
    }

    fn allowed_tools(&self, definition: &Definition) -> Option<BTreeSet<String>> {
        let wanted = definition.tools.as_ref()?;
        let available: BTreeSet<String> = self
            .child_definitions()
            .into_iter()
            .map(|tool| tool.name)
            .collect();
        Some(
            wanted
                .iter()
                .filter(|name| available.contains(*name))
                .cloned()
                .collect(),
        )
    }
}

/// Why task admission or setup failed. The start guard finalizes any task
/// already registered when later setup (such as scratch creation) fails.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum StartError {
    #[error("sub-agent temporary research directory unavailable")]
    ScratchUnavailable,
    #[error("unknown agent {0:?}")]
    UnknownAgent(String),
    #[error("external ACP agents support fresh context and their own model configuration only")]
    ExternalUnsupported,
    #[error("external agent input cannot be safely recorded; remove secrets or restore redaction")]
    ExternalSensitiveInput,
    #[error(r#"context must be "fresh" or "inherit""#)]
    InvalidContext,
    #[error("context inherit is not available in this session")]
    InheritUnavailable,
    #[error(r#"fork_before requires context "inherit""#)]
    ForkNeedsInherit,
    #[error("fork_before: {0}")]
    Fork(String),
    #[error("{0}")]
    Registry(#[from] TaskError),
    #[error("resume needs a saved session")]
    ResumeNoSession,
    #[error("create child session identity: {0}")]
    SessionIdentity(String),
    #[error("no sub-agent task {0} in this session")]
    ResumeNotFound(String),
    #[error(
        "task {0} is not interrupted; resume only accepts interrupted tasks. Use agent_status to inspect its status and report path, then read the report for completed tasks"
    )]
    ResumeNotInterrupted(String),
    #[error("resume: {0}")]
    ResumeFailed(String),
}

mod child_run;
mod child_tools;
mod completion;
mod durable_completion;
pub(super) mod external;
mod shared_provider;
mod start;
mod task_spec;

pub use child_tools::*;
pub use completion::*;
pub use shared_provider::*;
pub(crate) use task_spec::*;

#[cfg(test)]
mod tests;
