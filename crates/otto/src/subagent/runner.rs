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
//! children. Each task's [`CancellationToken`] is held by the registry, so
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
pub const EXCLUDED_CHILD_TOOLS: [&str; 20] = [
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
const GENERIC_SUBAGENT_INSTRUCTION: &str = "You are running as a sub-agent of Otto. Complete the entire delegated change within its assigned file scope. Reuse the agreed shared interfaces; report a needed contract change before expanding scope. Batch known same-file edits and use checked scripts for repetitive call-site changes. Complete only the delegated task below with the available tools, then reply with a self-contained final report. That final message is returned to the caller as your result. Do not send progress updates, plans, or interim findings with agent_report; put them in the final report instead. Call agent_report only to answer a question that arrives in a [parent-message], or to report a blocker that needs a parent decision while you can keep working on other parts of the task. If you cannot continue, end with a final report that states the blocker.";

const DEFAULT_MAX_PARALLEL: usize = 4;
const DEFAULT_MAX_OUTPUT_BYTES: usize = 16384;
const MAX_DESCRIPTION_CHARS: usize = 80;

/// The `customType` of the entry a child transcript gets as soon as its task
/// is created, before the child agent runs. See [`spawn`](Runner::spawn).
///
/// `pub(crate)` so [`crate::subagent::interrupted::scan`] can find this entry
/// type without duplicating the literal.
pub(crate) const TASK_SPEC_CUSTOM_TYPE: &str = "otto.task_spec";

/// The `customType` of the entry a child transcript gets when its task
/// reaches a terminal status. See [`finish`](Runner::finish).
///
/// `pub(crate)` so [`crate::subagent::interrupted`] can find and write this
/// entry type without duplicating the literal.
pub(crate) const TASK_RESULT_CUSTOM_TYPE: &str = "otto.task_result";

/// The JSON `data` of an `otto.task_spec` custom entry: the task as created,
/// and the agent definition as resolved at start (the same snapshot
/// [`Runner::run_with_definition_control`] uses), so a queued task's transcript
/// already records what it was asked to do before it starts.
///
/// `pub(crate)` and `Deserialize` so [`crate::subagent::interrupted::scan`]
/// reads back the exact type [`Runner::spawn`] writes, instead of a
/// hand-copied twin. `prompt` defaults on deserialize so a transcript written
/// before this field existed still parses.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct TaskSpecData {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) description: String,
    pub(crate) model: String,
    #[serde(default)]
    pub(crate) model_source: super::tasks::ModelSource,
    pub(crate) context: String,
    #[serde(default)]
    pub(crate) prompt: String,
    pub(crate) definition: Option<TaskSpecDefinition>,
    /// The tool call of the parent session that started this task; empty for
    /// tasks started outside a tool call and for transcripts written before
    /// this field existed.
    #[serde(
        rename = "parentToolCallId",
        default,
        skip_serializing_if = "String::is_empty"
    )]
    pub(crate) parent_tool_call_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct TaskSpecDefinition {
    pub(crate) name: String,
    pub(crate) body: String,
    /// The definition's own tool allowlist, `None` for every child tool.
    /// Unresolved against the live registry, matching what
    /// [`Runner::run_with_definition_control`] snapshots.
    #[serde(default)]
    pub(crate) tools: Option<Vec<String>>,
    #[serde(rename = "writePolicy")]
    pub(crate) write_policy: String,
    #[serde(rename = "writePaths", default)]
    pub(crate) write_paths: Vec<String>,
}

impl TaskSpecDefinition {
    /// Rebuilds a [`Definition`] from this snapshot, for a resumed task: the
    /// fields a spawn actually reads (`name`, `body`, `tools`, `write_policy`,
    /// `write_paths`) come from the recording; `description`, `model`,
    /// `context`, `directory`, `path` and `is_skill_derived` are not part of
    /// the `otto.task_spec` snapshot and are left at their default.
    ///
    /// Fails when `write_policy` is not one of the recognized values, naming
    /// it in the error, rather than silently falling back to a default
    /// policy for a task whose recorded write coordination cannot be
    /// honored.
    pub(crate) fn to_definition(&self) -> Result<Definition, String> {
        let write_policy = self.write_policy.parse().map_err(|error: String| {
            format!(
                "recorded task_spec has write_policy {:?}: {error}",
                self.write_policy
            )
        })?;
        Ok(Definition {
            name: self.name.clone(),
            tools: self.tools.clone(),
            write_policy,
            write_paths: self.write_paths.clone(),
            body: self.body.clone(),
            ..Definition::default()
        })
    }
}

/// The JSON `data` of an `otto.task_result` custom entry.
///
/// `pub(crate)` so [`crate::subagent::interrupted::mark_interrupted`] writes
/// the same shape [`Runner::finish`] does.
#[derive(Debug, Serialize)]
pub(crate) struct TaskResultData<'a> {
    pub(crate) status: &'a str,
    pub(crate) error: &'a str,
}

/// Shares one provider between the parent and every child, because [`Agent`]
/// owns the provider it calls.
pub struct SharedProvider {
    provider: Arc<dyn Provider + Send + Sync>,
    timeout: Option<Duration>,
    cancellation_grace: Duration,
}

#[async_trait::async_trait]
impl Provider for SharedProvider {
    async fn complete(
        &self,
        request: &Request,
        emit: StreamSink<'_>,
        control: &dyn OperationControl,
    ) -> ProviderSettlement {
        self.complete_observed(request, emit, control, &otto_core::diagnostics::Noop)
            .await
    }

    fn complete_observed<'a>(
        &'a self,
        request: &'a Request,
        emit: StreamSink<'a>,
        control: &'a dyn OperationControl,
        observer: &'a dyn otto_core::diagnostics::Observer,
    ) -> otto_core::provider::DiagnosticFuture<'a> {
        Box::pin(async move {
            if let Some(reason) = control.admission_stop_reason() {
                return ProviderSettlement::stopped(
                    if reason == OperationStopReason::Deadline {
                        ProviderError::DeadlineExceeded
                    } else {
                        ProviderError::Cancelled
                    },
                    0,
                    otto_core::model::EffectCertainty::NotStarted,
                    reason,
                );
            }
            let child = Control::new(Deadline::child(control.remaining(), self.timeout));
            let mut complete = std::pin::pin!(
                self.provider
                    .complete_observed(request, emit, &child, observer)
            );
            let deadline = child.deadline();
            let reason = tokio::select! {
                biased;
                result = &mut complete => return result,
                () = control.cancellation_token().cancelled() => {
                    let reason = control
                        .stop_reason()
                        .unwrap_or(OperationStopReason::UserCancellation);
                    child.stop(reason);
                    reason
                }
                () = deadline.expired() => {
                    child.stop(OperationStopReason::Deadline);
                    OperationStopReason::Deadline
                }
            };
            match tokio::time::timeout(self.cancellation_grace, &mut complete).await {
                Ok(settlement) => settlement,
                Err(_) => ProviderSettlement::stopped(
                    if reason == OperationStopReason::Deadline {
                        ProviderError::DeadlineExceeded
                    } else {
                        ProviderError::Cancelled
                    },
                    1,
                    otto_core::model::EffectCertainty::Unknown,
                    reason,
                ),
            }
        })
    }
}

/// Shares one child transcript between the child's agent and the runner, which
/// reads it back to build the final report.
pub type Transcript = Arc<dyn Session + Send + Sync>;

/// Builds the transcript for the task with the given id and returns it with
/// its file path. `Ok(None)` keeps the child in memory; an error fails the
/// task before it starts.
pub type ChildSession =
    Arc<dyn Fn(&str) -> Result<Option<(Transcript, String)>, String> + Send + Sync>;

/// Builds the durable mailbox for the child whose transcript is at the given
/// path, restoring the parent messages it had not read when it stopped.
pub type ChildMailbox = Arc<dyn Fn(&str) -> Arc<dyn Mailbox> + Send + Sync>;

struct SharedTranscript(Transcript);

#[async_trait::async_trait]
impl Session for SharedTranscript {
    fn messages(&self) -> Vec<Message> {
        self.0.messages()
    }

    fn model_messages(&self) -> Vec<Message> {
        self.0.model_messages()
    }

    async fn append(&self, message: Message) -> Result<(), otto_core::session::SessionError> {
        self.0.append(message).await
    }

    fn latest_compaction(&self) -> Option<otto_core::session::CompactionMetadata> {
        self.0.latest_compaction()
    }

    fn remote_compaction(&self) -> Option<otto_core::session::RemoteCompaction> {
        self.0.remote_compaction()
    }

    fn post_remote_compaction_messages(&self) -> Vec<Message> {
        self.0.post_remote_compaction_messages()
    }

    async fn append_remote_compaction(
        &self,
        checkpoint: otto_core::session::RemoteCompaction,
    ) -> Result<(), otto_core::session::SessionError> {
        self.0.append_remote_compaction(checkpoint).await
    }

    async fn append_compaction(
        &self,
        checkpoint: otto_core::session::CompactionCheckpoint,
    ) -> Result<otto_core::session::CompactionMetadata, otto_core::session::SessionError> {
        self.0.append_compaction(checkpoint).await
    }

    async fn append_operation_fact(
        &self,
        fact: otto_core::session::OperationFact,
    ) -> Result<(), otto_core::session::SessionError> {
        self.0.append_operation_fact(fact).await
    }

    fn operation_history(&self) -> Vec<otto_core::session::OperationFact> {
        self.0.operation_history()
    }

    fn operation_ledger(&self) -> otto_core::session::OperationLedger {
        self.0.operation_ledger()
    }

    fn observed_capabilities(&self) -> Option<otto_core::session::Capabilities> {
        self.0.observed_capabilities()
    }

    fn interrupted_turn(&self) -> Option<otto_core::session::InterruptedTurn> {
        self.0.interrupted_turn()
    }

    fn plan(&self) -> Vec<otto_core::session::PlanStep> {
        self.0.plan()
    }

    async fn append_input_if_absent(
        &self,
        input_id: &str,
        message: otto_core::model::Message,
    ) -> Result<otto_core::session::AppendOutcome, otto_core::session::SessionError> {
        self.0.append_input_if_absent(input_id, message).await
    }

    fn has_input(&self, input_id: &str) -> bool {
        self.0.has_input(input_id)
    }

    async fn append_custom(
        &self,
        custom_type: &str,
        data: &str,
    ) -> Result<(), otto_core::session::SessionError> {
        self.0.append_custom(custom_type, data).await
    }
}

/// One child's view of the shared child registry: the tools whose names are in
/// `allowed`, in the registry's own order. `None` allows every child tool.
pub struct ChildTools {
    registry: Arc<Registry>,
    allowed: Option<BTreeSet<String>>,
    write_policy: WritePolicy,
    write_paths: Vec<String>,
    parent_inbox: Arc<dyn Mailbox>,
    task_id: String,
    max_output_bytes: usize,
    /// Remaining `agent_report` calls; see [`REPORT_BUDGET`] and
    /// [`ChildTools::report`].
    reports_left: Arc<AtomicUsize>,
}

impl ChildTools {
    fn permits(&self, name: &str) -> bool {
        self.allowed
            .as_ref()
            .is_none_or(|allowed| allowed.contains(name))
    }

    fn permits_write_tool(&self, name: &str, arguments: &RawValue) -> Result<(), String> {
        if !matches!(name, "write" | "edit") {
            return Ok(());
        }
        match self.write_policy {
            WritePolicy::ReadOnly | WritePolicy::ProposeOnly => Err(format!(
                "write_policy {} denies workspace mutation tool {name}",
                self.write_policy.as_str()
            )),
            WritePolicy::SingleWriter => Ok(()),
            WritePolicy::OwnedPaths => {
                let path = mutation_path(arguments)?;
                if self
                    .write_paths
                    .iter()
                    .any(|pattern| path_matches(pattern, &path))
                {
                    Ok(())
                } else {
                    Err(format!(
                        "write_policy owned_paths denies {name} for path {path:?}"
                    ))
                }
            }
        }
    }

    /// Child-only tool that sends one message to the parent without ending
    /// this sub-agent task. Use only to answer a `[parent-message]` question
    /// or to report a blocker needing a parent decision, never for progress
    /// updates, plans, or interim findings. Budgeted by [`REPORT_BUDGET`]; see
    /// [`ChildTools::report`].
    fn agent_report_definition() -> ToolDefinition {
        let description = format!(
            "Send a message to the parent agent without ending this sub-agent task. Use it only to answer a question the parent sent as a [parent-message], or to report a blocker that needs a parent decision while you keep working on other parts of the task. Do not use it for progress updates, plans, raw logs, source dumps or interim findings. Keep the answer or blocker within 4 KiB; the parent transport truncates longer reports. For longer answers, write a persistent report when permitted and send its summary, path and section index; this call does not automatically save overflow. Limited to {REPORT_BUDGET} reports per task on your own initiative, plus one more for each [parent-message] received; a call past the limit returns an error instead of reaching the parent. A report that cannot be saved to the parent's pending-notification file also returns an error and does not reach the parent."
        );
        definition(
            "agent_report",
            &description,
            serde_json::json!({
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "message": {
                        "type": "string",
                        "description": "The answer to the parent's question, or the blocker and the decision it needs."
                    }
                },
                "required": ["message"]
            }),
        )
    }

    /// Pushes `message` to the parent's inbox, consuming one report from
    /// [`REPORT_BUDGET`]. The caller must check and decrement `reports_left`
    /// before calling this; see [`ChildTools::execute`].
    fn report(&self, message: &str) -> ToolResult {
        let text = capped_text_result(
            &format!("[task-report] task {}\n{}", self.task_id, message),
            self.max_output_bytes.min(PARENT_REPORT_BYTES),
        )
        .content;
        if let Err(otto_core::agent::inbox::MailboxError::NotPersisted { seq, reason }) =
            self.parent_inbox.enqueue(Notification {
                task_id: self.task_id.clone(),
                kind: Some(NotificationKind::TaskReport),
                text,
                usage: None,
                ..Default::default()
            })
        {
            self.parent_inbox.ack(seq);
            return error_result(format!("report not sent: {reason}"));
        }
        text_result("report sent to parent")
    }
}

fn mutation_path(arguments: &RawValue) -> Result<String, String> {
    let value: serde_json::Value = serde_json::from_str(arguments.get())
        .map_err(|_| "invalid tool arguments for write policy".to_string())?;
    let path = value
        .get("path")
        .and_then(|value| value.as_str())
        .ok_or_else(|| "missing required argument: path".to_string())?;
    Ok(path.to_string())
}

fn path_matches(pattern: &str, path: &str) -> bool {
    if let Some(prefix) = pattern.strip_suffix("/**") {
        return path == prefix || path.starts_with(&format!("{prefix}/"));
    }
    if let Some(prefix) = pattern.strip_suffix("/*") {
        return path
            .strip_prefix(&format!("{prefix}/"))
            .is_some_and(|rest| !rest.contains('/'));
    }
    if let Some((prefix, suffix)) = pattern.split_once('*') {
        return path.starts_with(prefix) && path.ends_with(suffix);
    }
    pattern == path
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct AgentReportArgs {
    #[serde(default)]
    message: String,
}

pub(crate) const AGENT_REPORT_RETRY: (&str, otto_core::model::RetrySafety) =
    ("agent_report", otto_core::model::RetrySafety::NonIdempotent);

#[async_trait::async_trait]
impl ToolExecutor for ChildTools {
    fn definitions(&self) -> Vec<ToolDefinition> {
        let mut definitions: Vec<ToolDefinition> = self
            .registry
            .definitions()
            .into_iter()
            .filter(|definition| self.permits(&definition.name))
            .collect();
        definitions.push(Self::agent_report_definition());
        definitions
    }

    fn retry_safety(&self, name: &str) -> otto_core::model::RetrySafety {
        if name == AGENT_REPORT_RETRY.0 {
            return AGENT_REPORT_RETRY.1;
        }
        if self.permits(name) {
            self.registry.retry_safety(name)
        } else {
            otto_core::model::RetrySafety::NonIdempotent
        }
    }

    fn batch_control(
        &self,
        name: &str,
        execution: &ToolExecution,
    ) -> otto_core::approval::BatchControl {
        self.registry.batch_control(name, execution)
    }

    async fn execute(&self, call: ToolCall<'_>, control: &dyn OperationControl) -> ToolExecution {
        let local = |mut result: ToolResult| {
            let outcome =
                result
                    .outcome_override
                    .take()
                    .unwrap_or(otto_core::model::OperationOutcome {
                        disposition: if result.is_error {
                            otto_core::model::OperationDisposition::Error
                        } else {
                            otto_core::model::OperationDisposition::Succeeded
                        },
                        effect_certainty: if result.is_error {
                            otto_core::model::EffectCertainty::NotStarted
                        } else {
                            otto_core::model::EffectCertainty::Completed
                        },
                        stop_reason: None,
                    });
            ToolExecution { result, outcome }
        };
        if call.name == "agent_report" {
            match control.admission_stop_reason() {
                Some(OperationStopReason::Deadline) => {
                    return local(
                        error_result("operation deadline exceeded").deadline_not_started(),
                    );
                }
                Some(_) => {
                    return local(error_result(CONTEXT_CANCELED).cancelled_not_started());
                }
                None => {}
            }
            let args: AgentReportArgs = match decode_strict_json(call.arguments.get(), &["message"])
            {
                Ok(args) => args,
                Err(message) => return local(error_result(message).not_started()),
            };
            let message = args.message.trim();
            if message.is_empty() {
                return local(error_result("message is required").not_started());
            }
            if self
                .reports_left
                .try_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
                .is_err()
            {
                return local(
                    error_result(
                        "agent_report limit used up for this task; put this in your final report instead. Each [parent-message] from the parent allows one more report.",
                    )
                    .not_started(),
                );
            }
            return local(self.report(message));
        }
        if !self.permits(call.name) {
            return local(ToolResult::unknown_tool(call.name));
        }
        if let Err(message) = self.permits_write_tool(call.name, call.arguments) {
            return local(crate::tool::error_result(message).not_started());
        }
        if let Some((service, mut scope, _)) = self.registry.approval_binding() {
            scope.task_id = self.task_id.clone();
            if call.name == "request_approval" {
                return local(
                    crate::tool::approval::RequestApproval {
                        service,
                        scope,
                        turn_origin: None,
                    }
                    .execute_call(call, control.cancellation_token())
                    .await,
                );
            }
            if matches!(call.name, "approval_pending" | "approval_revoke") {
                return local(
                    crate::tool::approval::Control {
                        service,
                        scope,
                        revoke: call.name == "approval_revoke",
                        expected_id: None,
                        legacy: None,
                    }
                    .execute(call.arguments, control.cancellation_token())
                    .await,
                );
            }
            crate::workflow::provenance::non_parent(self.registry.execute_with_scope(
                call,
                control,
                Some(&scope),
            ))
            .await
        } else {
            crate::workflow::provenance::non_parent(self.registry.execute(call, control)).await
        }
    }
}

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
                TaskStatus::Canceled,
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

        let semaphore = Arc::new(Semaphore::new(config.max_parallel));
        Ok((
            Self {
                reports: super::reports::Reports::default(),
                output_logs: std::sync::OnceLock::new(),
                scratch_root: std::sync::OnceLock::new(),
                config,
                child_registry,
                semaphore,
            },
            warnings,
        ))
    }

    pub(crate) fn bind_scratch_root(&self, root: crate::tool::root::Root) {
        assert!(
            self.scratch_root.set(root).is_ok(),
            "scratch root already bound"
        );
    }

    /// Bound the parent preview while keeping a durable report pointer visible.
    pub(crate) fn parent_text(&self, task: &Task, text: &str) -> String {
        let cap = self.config.max_output_bytes.min(PARENT_REPORT_BYTES);
        let mut preview = capped_text_result(text, cap).content;
        if task.is_final() {
            preview.push_str(&self.reports.handoff(
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
        self.child_registry.definitions()
    }

    fn allowed_tools(&self, definition: &Definition) -> Option<BTreeSet<String>> {
        let wanted = definition.tools.as_ref()?;
        let available: BTreeSet<String> = self
            .child_registry
            .definitions()
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

    /// Registers a task and spawns its child, or leaves it queued when
    /// [`Config::max_parallel`] children are already running. An unknown agent
    /// name, an invalid context, or `inherit` with no parent session is
    /// rejected before any task is created.
    pub async fn start(self: &Arc<Self>, request: StartRequest) -> Result<Task, StartError> {
        self.start_resolved_with_deadline(request, None, None, None)
            .await
    }

    async fn start_resolved_with_deadline(
        self: &Arc<Self>,
        request: StartRequest,
        transcript: Option<Transcript>,
        inline_definition: Option<Definition>,
        parent_deadline: Option<Deadline>,
    ) -> Result<Task, StartError> {
        let now = self.now();
        let description = truncate_with_ellipsis(request.description.trim(), MAX_DESCRIPTION_CHARS);

        let agent_name = request.agent.trim();
        let definition: Option<Definition> = if let Some(definition) = inline_definition {
            if agent_name != definition.name {
                return Err(StartError::UnknownAgent(agent_name.to_string()));
            }
            Some(definition)
        } else if agent_name.is_empty() {
            None
        } else {
            Some(
                self.config
                    .catalog
                    .lookup(agent_name)
                    .ok_or_else(|| StartError::UnknownAgent(agent_name.to_string()))?
                    .clone(),
            )
        };

        use super::tasks::ModelSource;
        let (model, model_source) = [
            (request.model.trim(), ModelSource::Explicit),
            (
                definition.as_ref().map_or("", |d| d.model.trim()),
                ModelSource::Definition,
            ),
            (
                self.config
                    .template
                    .subagent_models
                    .first()
                    .map_or("", |s| s.trim()),
                ModelSource::ConfiguredFirst,
            ),
            (self.config.template.model.trim(), ModelSource::Parent),
        ]
        .into_iter()
        .find(|(model, _)| !model.is_empty())
        .map(|(model, source)| (model.to_owned(), source))
        .unwrap_or_else(|| (String::new(), ModelSource::Parent));
        let mut context = first_non_empty(&[
            request.context.trim(),
            definition.as_ref().map_or("", |d| d.context.trim()),
            "fresh",
        ]);
        if context != "fresh" && context != "inherit" {
            return Err(StartError::InvalidContext);
        }
        let fork_before = request.fork_before.trim();
        let snapshot = if !fork_before.is_empty() {
            if request.context.trim() == "fresh" {
                return Err(StartError::ForkNeedsInherit);
            }
            let parent_fork = self
                .config
                .parent_fork
                .as_ref()
                .ok_or_else(|| StartError::Fork("needs a saved session".into()))?;
            context = "inherit".to_owned();
            parent_fork(fork_before).map_err(StartError::Fork)?
        } else if context == "inherit" {
            let parent_session = self
                .config
                .parent_session
                .as_ref()
                .ok_or(StartError::InheritUnavailable)?;
            inherit_snapshot(&parent_session()).unwrap_or_default()
        } else {
            Vec::new()
        };
        self.spawn(
            request,
            definition,
            description,
            model,
            model_source,
            context,
            now,
            snapshot,
            transcript.map(|transcript| (transcript, String::new())),
            None,
            self.task_deadline(parent_deadline),
        )
        .await
    }

    fn task_deadline(&self, parent: Option<Deadline>) -> Deadline {
        let parent = parent.unwrap_or_else(Deadline::unlimited);
        self.config
            .template
            .task_timeout
            .map_or(parent, |timeout| Deadline::earlier(parent, timeout))
    }

    /// Resumes an interrupted sub-agent task under its original id. `target`
    /// matches a child transcript's task id first, then its name, among the
    /// current session's children. `prompt` becomes the task's next user
    /// message; the run replays no inherited snapshot, since the existing
    /// transcript already holds the task's history.
    pub async fn resume(
        self: &Arc<Self>,
        target: &str,
        prompt: String,
    ) -> Result<Task, StartError> {
        let locate = self
            .config
            .children_dir
            .as_ref()
            .ok_or(StartError::ResumeNoSession)?;
        let children_dir = locate().ok_or(StartError::ResumeNoSession)?;

        let records = interrupted::scan(&children_dir);
        let record = records
            .iter()
            .find(|record| record.task_id == target)
            .or_else(|| records.iter().find(|record| record.name == target))
            .ok_or_else(|| StartError::ResumeNotFound(target.to_string()))?;
        if record.final_status.as_deref() != Some(interrupted::INTERRUPTED_STATUS) {
            return Err(StartError::ResumeNotInterrupted(record.task_id.clone()));
        }

        let (store, _warnings) = crate::session::Store::open(&record.path)
            .map_err(|error| StartError::ResumeFailed(error.to_string()))?;
        if let Some(lease) = self.config.lease.as_ref().and_then(|source| source()) {
            store.set_lease_check(lease);
        }
        let transcript: Transcript = Arc::new(store);
        let definition = record
            .definition
            .as_ref()
            .map(TaskSpecDefinition::to_definition)
            .transpose()
            .map_err(StartError::ResumeFailed)?;
        let now = self.now();
        let description = truncate_with_ellipsis(record.description.trim(), MAX_DESCRIPTION_CHARS);
        let request = StartRequest {
            prompt,
            description: record.description.clone(),
            name: record.name.clone(),
            model: record.model.clone(),
            agent: record.agent.clone(),
            context: record.context.clone(),
            fork_before: String::new(),
            parent_tool_call_id: String::new(),
        };
        self.spawn(
            request,
            definition,
            description,
            record.model.clone(),
            record.model_source,
            record.context.clone(),
            now,
            Vec::new(),
            Some((transcript, record.path.to_string_lossy().into_owned())),
            Some(record.task_id.clone()),
            self.task_deadline(None),
        )
        .await
    }

    /// Runs using a definition snapshot captured when a durable workflow was
    /// created, so editing `AGENT.md` cannot change a resumed run, while
    /// inheriting the caller's absolute deadline. The child's local task
    /// timeout may only shorten that budget.
    pub async fn run_with_definition_control(
        self: &Arc<Self>,
        request: StartRequest,
        transcript: Transcript,
        definition: Definition,
        control: &Control,
    ) -> Result<Task, String> {
        self.run_resolved(
            request,
            transcript,
            Some(definition),
            control.cancellation_token(),
            Some(control.deadline()),
        )
        .await
    }

    async fn run_resolved(
        self: &Arc<Self>,
        request: StartRequest,
        transcript: Transcript,
        definition: Option<Definition>,
        cancel: &CancellationToken,
        parent_deadline: Option<Deadline>,
    ) -> Result<Task, String> {
        let task = self
            .start_resolved_with_deadline(request, Some(transcript), definition, parent_deadline)
            .await
            .map_err(|error| error.to_string())?;
        let id = task.id.clone();
        let result = match self.config.tasks.wait(&id, cancel).await {
            Ok(task) => Ok(task),
            Err(TaskError::Canceled) => {
                self.config
                    .tasks
                    .cancel(&id)
                    .map_err(|error| error.to_string())?;
                self.config
                    .tasks
                    .wait(&id, &CancellationToken::new())
                    .await
                    .map_err(|error| error.to_string())
            }
            Err(error) => Err(error.to_string()),
        };
        if result.is_ok() {
            self.config.tasks.remove_final(&id);
        }
        result
    }

    /// Creates the task record, builds the child agent, and spawns it.
    /// `resume_id` is `Some` only from [`Runner::resume`]: the task is
    /// registered under that id instead of a freshly counted one, and no
    /// `otto.task_spec` entry is written, since the transcript already has
    /// one from the task's original run.
    #[allow(clippy::too_many_arguments)]
    async fn spawn(
        self: &Arc<Self>,
        request: StartRequest,
        definition: Option<Definition>,
        description: String,
        model: String,
        model_source: super::tasks::ModelSource,
        context: String,
        now: DateTime<Utc>,
        snapshot: Vec<Message>,
        transcript: Option<(Transcript, String)>,
        resume_id: Option<String>,
        deadline: Deadline,
    ) -> Result<Task, StartError> {
        let cancel = CancellationToken::new();
        // The task id names the child's file, so the transcript is built
        // after the task is registered; history reads it once it is set.
        let slot: Arc<std::sync::OnceLock<Transcript>> = Arc::default();
        let history_source = Arc::clone(&slot);
        let task_template = Task {
            name: request.name.trim().to_string(),
            agent: definition
                .as_ref()
                .map_or(String::new(), |d| d.name.clone()),
            description,
            prompt: request.prompt.clone(),
            context,
            model: model.clone(),
            model_source,
            created_at: Some(now),
            ..Task::default()
        };
        let history = Some(Arc::new(move || {
            history_source
                .get()
                .map(|transcript| transcript.messages())
                .unwrap_or_default()
        }) as Arc<dyn Fn() -> Vec<Message> + Send + Sync>);
        let task = match resume_id.clone() {
            Some(id) => {
                self.config
                    .tasks
                    .add_with_id(id, task_template, Some(cancel.clone()), history)?
            }
            None => self
                .config
                .tasks
                .add(task_template, Some(cancel.clone()), history)?,
        };
        let mut start_guard = StartGuard {
            runner: Arc::clone(self),
            task_id: task.id.clone(),
            now,
            cancel: cancel.clone(),
            armed: true,
        };
        let transcript = match (transcript, &self.config.child_session) {
            (Some((transcript, path)), _) => {
                if !path.is_empty() {
                    self.config.tasks.set_session_path(&task.id, &path);
                }
                transcript
            }
            (None, None) => Arc::new(MemorySession::new()),
            (None, Some(build)) => match build(&task.id) {
                Ok(Some((transcript, path))) => {
                    self.config.tasks.set_session_path(&task.id, &path);
                    transcript
                }
                Ok(None) => Arc::new(MemorySession::new()),
                Err(error) => {
                    // No transcript was built, so there is nothing to append
                    // otto.task_result to.
                    self.finish_without_transcript(
                        &task.id,
                        TaskStatus::Failed,
                        now,
                        &[],
                        Some(&error),
                    );
                    start_guard.disarm();
                    return Ok(self.config.tasks.get(&task.id).unwrap_or(task));
                }
            },
        };
        let _ = slot.set(Arc::clone(&transcript));
        if let Some(build) = &self.config.child_mailbox
            && let Some(path) = self
                .config
                .tasks
                .get(&task.id)
                .map(|task| task.session_path)
                .filter(|path| !path.is_empty())
        {
            self.config
                .tasks
                .replace_child_inbox(&task.id, build(&path));
        }

        // Durable custom entries are awaited before `start` returns, so even
        // a task that remains queued has its complete specification on disk.
        if resume_id.is_none() {
            let task_spec = TaskSpecData {
                id: task.id.clone(),
                name: task.name.clone(),
                description: task.description.clone(),
                model: task.model.clone(),
                model_source: task.model_source,
                context: task.context.clone(),
                prompt: task.prompt.clone(),
                definition: definition.as_ref().map(|definition| TaskSpecDefinition {
                    name: definition.name.clone(),
                    body: definition.body.clone(),
                    tools: definition.tools.clone(),
                    write_policy: definition.write_policy.as_str().to_string(),
                    write_paths: definition.write_paths.clone(),
                }),
                parent_tool_call_id: request.parent_tool_call_id.clone(),
            };
            if let Err(error) =
                append_task_custom(&transcript, TASK_SPEC_CUSTOM_TYPE, &task_spec).await
            {
                cancel.cancel();
                let messages = transcript.messages();
                self.finish(
                    &task.id,
                    TaskStatus::Failed,
                    now,
                    &messages,
                    Some(&error.to_string()),
                    Some(&transcript),
                )
                .await;
                start_guard.disarm();
                return Ok(self.config.tasks.get(&task.id).unwrap_or(task));
            }
        }

        let tools = ChildTools {
            registry: Arc::clone(&self.child_registry),
            allowed: definition
                .as_ref()
                .and_then(|definition| self.allowed_tools(definition)),
            write_policy: definition
                .as_ref()
                .map_or(WritePolicy::SingleWriter, |definition| {
                    definition.write_policy
                }),
            write_paths: definition
                .as_ref()
                .map_or_else(Vec::new, |definition| definition.write_paths.clone()),
            parent_inbox: Arc::clone(self.config.tasks.notifications()),
            task_id: task.id.clone(),
            max_output_bytes: self.config.max_output_bytes,
            reports_left: self
                .config
                .tasks
                .report_allowance(&task.id)
                .unwrap_or_else(|| Arc::new(AtomicUsize::new(REPORT_BUDGET))),
        };

        let scratch = self
            .scratch_root
            .get()
            .filter(|_| tools.permits("bash") && self.child_registry.lookup("bash").is_some())
            .map(super::scratch::Scratch::new)
            .transpose()
            .map_err(|_| StartError::ScratchUnavailable)?;
        let report_path = self.reports.prepare(self.scratch_root.get(), &task.id);
        let report_policy = report_path.map_or_else(String::new, |path| format!(
            "\n<research_report>For research too detailed for the handoff, use write, when available and permitted, to save the complete report in {path}/report.md with Markdown section headings. Return a concise summary, uncertainties, the report path and a section index with line numbers within 4 KiB. This directory persists after task exit; final.md and index.md are reserved for runtime overflow capture. Do not place reports in the temporary research directory. Existing write permissions and assigned file scope still apply. If you cannot write a report, return your findings as final text; the runtime saves an oversized final answer automatically.</research_report>"));
        let scratch_policy = scratch.as_ref().map_or_else(String::new, |scratch| format!(
            "\n<research_workspace>Use {} for temporary source downloads and analysis scripts. Download each pinned source once, then search/analyze locally and return only conclusions with original URL or file/line evidence. Keep durable deliverables outside this directory. Otto deletes this directory when your task exits, including failure or cancellation. Do not return its temporary paths as evidence.</research_workspace>", scratch.path()));

        let role_body = definition
            .as_ref()
            .map(|d| d.body.as_str())
            .filter(|body| !body.is_empty())
            .unwrap_or(GENERIC_SUBAGENT_INSTRUCTION);
        let redactor = self.redactor();
        let system_prompt = redactor.redact_string(&format!(
            "{}\n<subagent_role>\n{}\n</subagent_role>{scratch_policy}{report_policy}\n<child_report_policy>Keep research process, source text and logs in your own context. Return only the verified conclusion, essential evidence references, uncertainty and decisions needed from the parent. Keep handoff summaries and agent_report messages within 4 KiB; put detailed findings in a report file when permitted. Do not include raw logs or code dumps in handoffs. The parent-facing transport caps previews; oversized final answers are saved to a persistent report when workspace storage is available. The full transcript remains local.</child_report_policy>\n</otto_system_prompt>",
            (self.config.prompt_for)(&tools.definitions()),
            crate::skill::prompt::escape_html(role_body),
        ));

        let template = &self.config.template;
        let now_clock = Arc::clone(&template.now);
        let new_id = Arc::clone(&template.new_id);
        let new_operation_id = Arc::clone(&template.new_operation_id);
        let child_inbox = self
            .config
            .tasks
            .child_inbox(&task.id)
            .unwrap_or_else(|| Arc::new(Inbox::new(None)) as Arc<dyn Mailbox>);
        // The transcript file stem (`<task-id>-<child-id>`), never the full
        // path, which contains the home directory. Empty for memory sessions.
        let cache_key = self
            .config
            .tasks
            .get(&task.id)
            .and_then(|t| {
                std::path::Path::new(&t.session_path)
                    .file_stem()
                    .map(|stem| stem.to_string_lossy().into_owned())
            })
            .unwrap_or_default();
        let options = Options {
            diagnostics: template
                .diagnostics
                .as_ref()
                .and_then(|sink| sink.for_task(&task.id)),
            hooks: template
                .plugins
                .as_ref()
                .filter(|p| p.has_hooks())
                .map(|p| {
                    p.bind(
                        format!(
                            "{}:{}",
                            self.config
                                .approval
                                .as_ref()
                                .map_or("child", |(_, s)| s.session_id.as_str()),
                            task.id
                        ),
                        self.config
                            .approval
                            .as_ref()
                            .map(|(_, s)| s.session_id.clone()),
                        task.id.clone(),
                        if transcript.messages().is_empty() {
                            "new"
                        } else {
                            "resume"
                        },
                        template.hook_guard.clone(),
                    )
                }),
            model: redactor.redact_string(&model),
            provider_name: template.provider_name.clone(),
            system_prompt,
            thinking: redactor.redact_string(&template.thinking),
            cache_key,
            now: Box::new(move || now_clock()),
            new_id: Box::new(move || new_id()),
            new_operation_id: Box::new(move || new_operation_id()),
            request_sizer: template.request_sizer.clone(),
            compaction: template.compaction,
            progress: template.progress,
            // A child gets no memory binding, no registry of its own, and a
            // private inbox: it can neither recall, delegate, nor observe the
            // parent's notifications, but the parent can send messages into
            // this private queue with agent_send.
            memory: None,
            tasks: None,
            mailbox: child_inbox,
            version: env!("CARGO_PKG_VERSION").into(),
            ..Options::default()
        };

        let child = Agent::with_redactor(
            SharedProvider {
                provider: Arc::clone(&self.config.provider),
                timeout: template.provider_timeout,
                cancellation_grace: template.cancellation_grace,
            },
            tools,
            SharedTranscript(Arc::clone(&transcript)),
            options,
            self.redactor(),
        );

        let runner = Arc::clone(self);
        let task_id = task.id.clone();
        let prompt = request.prompt;
        // Acquire before spawning: even a queued child must outlive the parent's
        // turn lease. The closure releases this on success, error or cancellation.
        let output_lease = self.output_logs.get().map(|logs| logs.lease());
        tokio::spawn(async move {
            let _output_lease = output_lease;
            let _scratch = scratch;
            runner
                .run_child(ChildRun {
                    cancel,
                    task_id,
                    prompt,
                    child,
                    transcript,
                    snapshot,
                    deadline,
                })
                .await;
        });
        start_guard.disarm();

        Ok(task)
    }

    fn redactor(&self) -> Redactor {
        Redactor::with_completeness(
            &self.config.redaction_values,
            self.config.redaction_complete,
        )
    }

    /// Replays the inherited snapshot, waits for a semaphore slot or for
    /// cancellation while queued, runs the child to completion, and always
    /// finishes the task record.
    async fn run_child(&self, run: ChildRun) {
        let ChildRun {
            cancel,
            task_id,
            prompt,
            child,
            transcript,
            snapshot,
            deadline,
        } = run;
        for message in snapshot {
            if let Err(error) = transcript.append(message).await {
                cancel.cancel();
                let messages = transcript.messages();
                self.finish(
                    &task_id,
                    TaskStatus::Failed,
                    self.now(),
                    &messages,
                    Some(&error.to_string()),
                    Some(&transcript),
                )
                .await;
                return;
            }
        }

        let permit = tokio::select! {
            biased;
            () = deadline.expired() => {
                let messages = transcript.messages();
                self.finish(&task_id, TaskStatus::Interrupted, self.now(), &messages, Some("task deadline exceeded"), Some(&transcript)).await;
                return;
            }
            permit = Arc::clone(&self.semaphore).acquire_owned() => permit,
            () = cancel.cancelled() => {
                let messages = transcript.messages();
                self.finish(&task_id, TaskStatus::Canceled, self.now(), &messages, None, Some(&transcript)).await;
                return;
            }
        };

        self.config.tasks.mark_running(&task_id, self.now());

        let mut progress = ChildProgress::new(Arc::clone(&self.config.tasks), task_id.clone());
        let usage = self
            .config
            .usage
            .as_ref()
            .map(|collector| collector.for_task(&task_id));
        let control = Control::new(deadline);
        if cancel.is_cancelled() {
            control.stop(OperationStopReason::UserCancellation);
        }
        let outcome = {
            let mut handle = |event: Event| {
                if let Some(usage) = &usage {
                    let _ = usage.record(&event);
                }
                progress.handle(event);
            };
            let sink: EventSink<'_> = &mut handle;
            let run = child.run_with_control(&prompt, sink, &control);
            tokio::pin!(run);
            tokio::select! {
                biased;
                outcome = &mut run => outcome,
                () = cancel.cancelled() => {
                    control.stop(OperationStopReason::UserCancellation);
                    run.await
                }
                () = deadline.expired() => {
                    control.stop(OperationStopReason::Deadline);
                    // Deadline requests cooperative stop; the effectful child
                    // remains owned here until it safely settles.
                    run.await
                }
            }
        };
        drop(permit);
        child.end_hooks().await;
        let _ = child.close();

        let status = match &outcome {
            _ if control.stop_reason() == Some(OperationStopReason::Deadline) => {
                TaskStatus::Interrupted
            }
            Ok(()) => TaskStatus::Succeeded,
            Err(_) if cancel.is_cancelled() => TaskStatus::Canceled,
            Err(_) => TaskStatus::Failed,
        };
        let error = if status == TaskStatus::Interrupted {
            Some("task deadline exceeded".to_string())
        } else {
            outcome.err().map(|error| error.to_string())
        };
        let messages = transcript.messages();
        self.finish(
            &task_id,
            status,
            self.now(),
            &messages,
            error.as_deref(),
            Some(&transcript),
        )
        .await;
    }

    /// Marks a task final and pushes its completion notification. The
    /// notification is pushed before `otto.task_result` is appended to
    /// `transcript` (`None` when the task never got one, such as when the
    /// child-session builder itself failed), and before the registry update
    /// that releases [`Tasks::wait`], so a caller unblocked by a wait always
    /// finds the notification already in the inbox.
    async fn finish(
        &self,
        task_id: &str,
        status: TaskStatus,
        finished_at: DateTime<Utc>,
        messages: &[Message],
        run_error: Option<&str>,
        transcript: Option<&Transcript>,
    ) {
        let (result, error_text, migrating) =
            self.prepare_finish(task_id, status, finished_at, messages, run_error);

        if let Some(transcript) = transcript {
            let result_status = if migrating {
                crate::subagent::interrupted::INTERRUPTED_STATUS
            } else {
                status.as_str()
            };
            let _ = append_task_custom(
                transcript,
                TASK_RESULT_CUSTOM_TYPE,
                &TaskResultData {
                    status: result_status,
                    error: &error_text,
                },
            )
            .await;
        }
        self.config
            .tasks
            .finish(task_id, status, finished_at, &result, &error_text);
    }

    fn finish_without_transcript(
        &self,
        task_id: &str,
        status: TaskStatus,
        finished_at: DateTime<Utc>,
        messages: &[Message],
        run_error: Option<&str>,
    ) {
        let (result, error_text, _) =
            self.prepare_finish(task_id, status, finished_at, messages, run_error);
        self.config
            .tasks
            .finish(task_id, status, finished_at, &result, &error_text);
    }

    fn prepare_finish(
        &self,
        task_id: &str,
        status: TaskStatus,
        finished_at: DateTime<Utc>,
        messages: &[Message],
        run_error: Option<&str>,
    ) -> (String, String, bool) {
        let mut result = last_assistant_text(messages);
        if result.is_empty() {
            result = "(sub-agent returned no final text)".to_string();
        }
        let error_text = match (status, run_error) {
            (TaskStatus::Failed | TaskStatus::Interrupted, Some(error)) => error.to_string(),
            _ => String::new(),
        };

        let mut final_task = self.config.tasks.get(task_id).unwrap_or_default();
        final_task.status = status;
        final_task.finished_at = Some(finished_at);
        final_task.result = result.clone();
        final_task.error = error_text.clone();

        // A task cancelled by `Tasks::begin_migration` moved because its
        // process is leaving, not because it failed or was asked to stop:
        // skip the ordinary completion notification (`notify_moved` sends
        // one notification for the whole migration instead) and record the
        // transcript's own result as "interrupted" rather than "canceled",
        // matching how a takeover-side scan would classify it.
        let migrating = status == TaskStatus::Canceled && self.config.tasks.is_migrating();
        if !migrating {
            self.config.tasks.notifications().push(Notification {
                task_id: task_id.to_string(),
                kind: Some(NotificationKind::TaskFinished),
                text: self.parent_text(&final_task, &completion_text(&final_task, usize::MAX)),
                usage: final_task.usage_present.then_some(final_task.usage),
                // The parent's footer adds this task's tokens to the session
                // totals; it needs to know whether the cached share behind
                // them is measured or merely unreported.
                cached_input_tokens_present: final_task.cached_input_tokens_present,
                ..Default::default()
            });
        }
        (result, error_text, migrating)
    }
}

/// Encodes `data` and appends it to `transcript` as a `custom` entry typed
/// `custom_type`. A failure to encode or append is dropped, the same way
/// [`Recorder::upsert`](crate::subagent::record::Recorder::upsert) drops a
/// task record write failure: in TUI mode a stderr write from a background
/// task is drawn over the screen. The task's completion notification to the
/// parent does not depend on these entries.
async fn append_task_custom(
    transcript: &Transcript,
    custom_type: &str,
    data: &impl Serialize,
) -> Result<(), otto_core::session::SessionError> {
    let json = serde_json::to_string(data)
        .map_err(|error| otto_core::session::SessionError::Persist(error.to_string()))?;
    transcript.append_custom(custom_type, &json).await
}

/// Why task admission or setup failed. The start guard finalizes any task
/// already registered when later setup (such as scratch creation) fails.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum StartError {
    #[error("sub-agent temporary research directory unavailable")]
    ScratchUnavailable,
    #[error("unknown agent {0:?}")]
    UnknownAgent(String),
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
    #[error("no sub-agent task {0} in this session")]
    ResumeNotFound(String),
    #[error(
        "task {0} is not interrupted; resume only accepts interrupted tasks. Use agent_status to inspect its status and report path, then read the report for completed tasks"
    )]
    ResumeNotInterrupted(String),
    #[error("resume: {0}")]
    ResumeFailed(String),
}

/// Turns a child's events into task record updates. It never forwards an event
/// to the parent's frontend.
struct ChildProgress {
    tasks: Arc<Tasks>,
    task_id: String,
    text: String,
}

impl ChildProgress {
    fn new(tasks: Arc<Tasks>, task_id: String) -> Self {
        Self {
            tasks,
            task_id,
            text: String::new(),
        }
    }

    fn handle(&mut self, event: Event) {
        match event {
            Event::TextDelta { text } => self.text.push_str(&text),
            Event::ProviderRetry {
                interrupted_output: true,
                ..
            } => self.text.clear(),
            Event::ProviderUsage {
                usage,
                present,
                cached_input_tokens_present,
            } => {
                let text = cap_last_bytes(&self.text, 500).to_string();
                self.text.clear();
                self.tasks.record_provider_step(
                    &self.task_id,
                    usage,
                    &text,
                    present,
                    cached_input_tokens_present,
                );
            }
            Event::ToolCallFinished { .. } => self.tasks.record_tool_finished(&self.task_id),
            Event::ToolCallStarted {
                tool_name,
                arguments,
                ..
            } => {
                let preview = compact_args_preview(&arguments, 60);
                let last_tool = if preview.is_empty() {
                    tool_name
                } else {
                    format!("{tool_name} {preview}")
                };
                self.tasks.record_tool_call(&self.task_id, &last_tool);
            }
            _ => {}
        }
    }
}

/// The notification and wait text for a final task.
pub fn completion_text(task: &Task, max_output_bytes: usize) -> String {
    let name = agent_label(task);
    let duration = completion_duration(task);
    let calls = pluralize_tool_calls(task.tool_calls);
    let model_segment = if task.model.is_empty() {
        String::new()
    } else {
        format!(" · {}", task.model)
    };

    match task.status {
        TaskStatus::Failed => format!(
            "[task-notification] task {} {name} failed{model_segment} · {duration} · {calls}\n{}",
            task.id, task.error
        ),
        TaskStatus::Interrupted => format!(
            "[task-notification] task {} {name} interrupted{model_segment} · {duration} · {calls}\n{}",
            task.id, task.error
        ),
        // Every non-final status plus canceled renders this way; only a final
        // task ever reaches this function.
        TaskStatus::Canceled | TaskStatus::Queued | TaskStatus::Running => format!(
            "[task-notification] task {} {name} canceled{model_segment} · {duration} · {calls}",
            task.id
        ),
        TaskStatus::Succeeded => {
            let tokens = comma_int(task.usage.input_tokens + task.usage.output_tokens);
            format!(
                "[task-notification] task {} {name} succeeded{model_segment} · {duration} · {calls} · {tokens} tokens\n{}",
                task.id,
                capped_text_result(&task.result, max_output_bytes).content
            )
        }
    }
}

/// The label shown after a task id: the parenthesized definition name, such as
/// `(default)` or `(explorer)`, prefixed by the task's name and a space when
/// it has one, such as `lint-check (explorer)`.
pub fn agent_label(task: &Task) -> String {
    let label = definition_label(task);
    if task.name.is_empty() {
        label
    } else {
        format!("{} {label}", task.name)
    }
}

/// The parenthesized definition name alone. The status table uses it for its
/// definition column, so a task name appears only once per line.
pub fn definition_label(task: &Task) -> String {
    if task.agent.is_empty() {
        "(default)".to_string()
    } else {
        format!("({})", task.agent)
    }
}

/// The finished duration rounded to the second, or `"0s"` for a task canceled
/// while still queued, which never got a start time.
fn completion_duration(task: &Task) -> String {
    let Some(started) = task.started_at else {
        return "0s".to_string();
    };
    let finished = task.finished_at.unwrap_or(started);
    round_to_seconds(finished.signed_duration_since(started))
}

/// The trimmed text of the last assistant message, or `""` when there is none.
fn last_assistant_text(messages: &[Message]) -> String {
    messages
        .iter()
        .rev()
        .find(|message| message.role == Role::Assistant)
        .map_or(String::new(), |message| message.text().trim().to_string())
}

pub(crate) fn pluralize_tool_calls(count: i64) -> String {
    if count == 1 {
        "1 tool call".to_string()
    } else {
        format!("{count} tool calls")
    }
}

pub(crate) fn pluralize_tools(count: i64) -> String {
    if count == 1 {
        "1 tool".to_string()
    } else {
        format!("{count} tools")
    }
}

/// `value` unchanged when it has at most `max_chars` characters, otherwise its
/// first `max_chars - 1` characters plus `…`, so the result has exactly
/// `max_chars` characters.
fn truncate_with_ellipsis(value: &str, max_chars: usize) -> String {
    if value.chars().count() <= max_chars {
        return value.to_string();
    }
    if max_chars == 0 {
        return String::new();
    }
    format!("{}…", first_runes(value, max_chars - 1))
}

/// The last `max_bytes` bytes of `value`, advanced to the next character
/// boundary so the result never starts mid-character.
pub(crate) fn cap_last_bytes(value: &str, max_bytes: usize) -> &str {
    if value.len() <= max_bytes {
        return value;
    }
    let mut start = value.len() - max_bytes;
    while start < value.len() && !value.is_char_boundary(start) {
        start += 1;
    }
    &value[start..]
}

/// Raw JSON with its insignificant whitespace removed. Invalid JSON falls back
/// to collapsing the raw text onto one line.
pub(crate) fn compact_json(raw: &str) -> String {
    match serde_json::from_str::<serde_json::Value>(raw)
        .ok()
        .as_ref()
        .map(serde_json::to_string)
    {
        Some(Ok(text)) => text,
        _ => one_line(raw),
    }
}

/// Tool call arguments as compact JSON capped at `max_chars` characters.
fn compact_args_preview(raw: &str, max_chars: usize) -> String {
    truncate_with_ellipsis(&compact_json(raw), max_chars)
}

fn first_non_empty(candidates: &[&str]) -> String {
    candidates
        .iter()
        .find(|candidate| !candidate.is_empty())
        .unwrap_or(&"")
        .to_string()
}

#[cfg(test)]
mod tests;
