//! The provider/tool turn loop.
//!
//! One user message, then provider calls alternating with tool calls until the
//! model stops asking for tools, wrapped in proactive and overflow-triggered
//! compaction, memory recall, inbox notifications, the secret redactor, and the
//! tool-result overlay.
//!
//! Ownership: the agent owns its provider, tool executor, and session. The
//! caller owns the event sink and the cancellation token.
//!
//! Concurrency and cancellation: `run` takes `&self` but a single agent is
//! meant to serve one turn at a time. `otto-core` holds no async mutex, so the
//! caller must not overlap `run` and `compact`. Cancelling
//! the token stops the provider call, skips the remaining tool calls, and ends
//! the run with an error.
//!
//! Errors: every failure path emits [`Event::AgentError`] and returns the same
//! error, so a frontend that only watches events sees every failure.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use chrono::{DateTime, Utc};
use serde_json::value::RawValue;
use tokio_util::sync::CancellationToken;

mod bash_history;
pub mod budget;
mod capabilities;
pub mod compaction;
pub mod compaction_select;
pub mod context_estimate;
pub mod context_report;
pub mod events;
mod goals;
pub mod hooks;
mod image_input;
pub mod inbox;
pub mod memory;
pub mod memory_retraction;
mod memory_step;
pub mod oneshot;
pub mod overflow;
pub mod progress;
mod provider_call;
mod provider_hooks;
mod provider_step;
pub mod redactor;
mod sent_request;
pub mod steering;
mod step_control;
pub mod summary;
pub mod summary_details;
pub mod summary_validate;
mod tool_dispatch;
mod turn_run;

/// The built-in task `/init` submits through the ordinary agent turn.
pub const INIT_PROMPT: &str = "Create an AGENTS.md contributor guide for this repository. First \
    inspect the workspace and check whether AGENTS.md already exists at its \
    root. If it exists, do not overwrite or modify it. Otherwise, write a \
    concise, repository-specific guide covering project structure, build \
    and test commands, coding conventions, and contribution expectations.";

#[cfg(test)]
mod run_tests;

pub use events::{
    AgentError, ApiStatus, CompactionMode, CompactionPlan, CompactionReason, CompactionResult,
    CompactionSettings, Event, EventSink, ProviderOperationKind,
};

use crate::model::{
    Block, BlockType, ContextMetadata, EffectCertainty, Message, OperationDisposition, OperationId,
    OperationOutcome, OperationStopReason, Role, ToolDefinition, ToolResultMetadata, Usage,
    zero_time,
};
use crate::operation::OperationControl;
use crate::provider::{
    Provider, ProviderError, ProviderSettlement, RemoteCompactionSupport, Request, RequestSizer,
    Response, StreamEvent,
};
use crate::session::Session;
use crate::session::context::pending_tool_calls;
use crate::session::operation::{OperationFact, replay_allowed};
use crate::session::turn::{
    TURN_CUSTOM_TYPE, TurnOutcome, TurnRecord, TurnTrigger, encode_turn_record,
};
use crate::tool::{ToolCall, ToolContentScope, ToolExecution, ToolExecutor, ToolResult};

use crate::mailbox::{InMemoryMailbox, Mailbox};
use budget::{BudgetPolicy, BudgetScope, TurnBudget};
use memory::{DEFAULT_RECALL_LIMIT, DEFAULT_RECALL_TOKEN_BUDGET, MemoryRecall};
use overflow::{
    AUTOMATIC_COMPACTION_ATTEMPT_USED_MESSAGE, AUTOMATIC_COMPACTION_HARD_FAILURE_MESSAGE,
    AUTOMATIC_COMPACTION_STILL_TOO_LARGE_MESSAGE, AUTOMATIC_COMPACTION_WARNING_MESSAGE,
    OVERFLOW_COMPACTION_FAILURE_MESSAGE, OVERFLOW_RETRY_FAILURE_MESSAGE, RunDispatchState,
    automatic_cancellation, automatic_compaction_triggers, automatic_dispatch_error,
    idle_cache_expired, is_typed_context_overflow,
};
use progress::{ProgressGuard, ProgressPolicy};
use redactor::Redactor;

/// Snapshot of the child registry as `(task ID, is final)` pairs.
pub type GoalChildRegistry = Arc<dyn Fn() -> Vec<(String, bool)> + Send + Sync>;

/// Turn settings and the two injected capabilities the core cannot provide
/// itself: the clock and the identifier generator. Injecting them keeps
/// `otto-core` free of clock and randomness access.
pub struct Options {
    /// Out-of-band content-free observations; never become model context.
    pub diagnostics: Option<Arc<dyn crate::diagnostics::Diagnostics>>,
    pub hooks: Option<Arc<dyn hooks::Hooks>>,
    pub hook_state: Arc<hooks::State>,
    pub model: String,
    pub provider_name: String,
    pub system_prompt: String,
    /// Labeled pieces of `system_prompt`, for the context report only. They
    /// are ignored unless they concatenate to `system_prompt` exactly.
    pub system_prompt_parts: Vec<(String, String)>,
    pub thinking: String,
    /// Conversation identifier sent as `Request::cache_key`; empty means none.
    pub cache_key: String,
    /// Returns the current time. Called once per persisted message and twice
    /// per provider call, to measure its duration.
    pub now: Box<dyn Fn() -> DateTime<Utc> + Send + Sync>,
    /// Returns a fresh message identifier. Must not repeat within a session.
    pub new_id: Box<dyn Fn() -> String + Send + Sync>,
    /// Returns a fresh logical-operation identifier. Unlike `new_id`, this is
    /// persisted before a tool dispatch and must be valid and unique within
    /// the session.
    pub new_operation_id: Box<dyn Fn() -> Result<OperationId, String> + Send + Sync>,
    /// Measures the serialized size of a request. Compaction needs it to
    /// bound the summary request; without it, compaction cannot run.
    pub request_sizer: Option<Arc<dyn RequestSizer + Send + Sync>>,
    /// The context windows and reserves that drive automatic compaction.
    pub compaction: CompactionSettings,
    /// The limits that stop a turn making no progress. Every turn uses one,
    /// including each sub-agent's.
    pub progress: ProgressPolicy,
    /// The limits of a run that asks for [`BudgetScope::Fresh`] through the
    /// `run*` methods without a scope and of a manual compaction. A run with
    /// [`BudgetScope::Shared`] uses the policy of its root.
    pub budget: BudgetPolicy,
    /// The long-term memory binding consulted once per turn, if any.
    pub memory: Option<Arc<dyn MemoryRecall + Send + Sync>>,
    /// How many records one recall may return.
    pub memory_recall_limit: i64,
    /// How many tokens one recall may spend.
    pub memory_recall_token_budget: i64,
    /// Notifications waiting to be delivered as context messages. Shared with
    /// producers through the public mailbox port.
    pub mailbox: Arc<dyn Mailbox>,
    /// Turn-bound guidance accepted by a frontend while a run is active.
    pub steering: Arc<steering::Steering>,
    /// The running Otto package version, recorded with the tool set. Empty
    /// when unknown.
    pub version: String,
    /// Authoritative child registry snapshot: (task ID, is final).
    pub goal_children: Option<GoalChildRegistry>,
}

/// The default clock returns the zero time and the default identifier
/// generator returns the empty string: `otto-core` has no clock and no
/// randomness of its own, so a real caller must replace both.
impl Default for Options {
    fn default() -> Self {
        let operation_counter = AtomicU64::new(0);
        Self {
            diagnostics: None,
            hooks: None,
            hook_state: Arc::default(),
            model: String::new(),
            provider_name: String::new(),
            system_prompt: String::new(),
            system_prompt_parts: Vec::new(),
            thinking: String::new(),
            cache_key: String::new(),
            now: Box::new(zero_time),
            new_id: Box::new(String::new),
            new_operation_id: Box::new(move || {
                let next = operation_counter.fetch_add(1, Ordering::Relaxed) + 1;
                OperationId::new(format!("op_{next}")).map_err(|error| error.to_string())
            }),
            request_sizer: None,
            compaction: CompactionSettings::default(),
            progress: ProgressPolicy::default(),
            budget: BudgetPolicy::default(),
            memory: None,
            memory_recall_limit: DEFAULT_RECALL_LIMIT,
            memory_recall_token_budget: DEFAULT_RECALL_TOKEN_BUDGET,
            mailbox: Arc::new(InMemoryMailbox::default()),
            steering: Arc::new(steering::Steering::default()),
            version: String::new(),
            goal_children: None,
        }
    }
}

/// Runs provider and tool turns against one session.
///
/// The caller must serialize calls: `run` and `compact` both mutate the
/// session, and neither takes a lock.
pub struct Agent<P, T, S> {
    provider: P,
    tools: T,
    pub(super) session: S,
    pub(super) options: Options,
    pub(super) redactor: Redactor,
    /// The memory block the last user turn recalled, for the context report.
    last_memory_context: std::sync::Mutex<String>,
    /// The last ordinary request handed to the provider, for the context
    /// report. Set from the guarded request only; see `sent_request`.
    last_sent_request: std::sync::Mutex<Option<Arc<context_report::SentRequest>>>,
    hook_turn_id: std::sync::Mutex<String>,
    /// Messages appended during the current hooked run; `None` when no hooks are active.
    hook_turn_messages: std::sync::Mutex<Option<Vec<Message>>>,
    /// The client input id of the next run's user message; taken by every
    /// run, used or not. See [`Agent::set_next_input_id`].
    next_input_id: std::sync::Mutex<Option<String>>,
    next_input_provenance: std::sync::Mutex<Option<crate::model::InputProvenance>>,
    /// The budget of the run in progress; see [`budget`]. Replaced from the
    /// run's [`BudgetScope`] when a run or a manual compaction starts.
    budget: std::sync::Mutex<TurnBudget>,
}

impl<P: Provider, T: ToolExecutor, S: Session> Agent<P, T, S> {
    /// Records the client's input id for the user message of the next run,
    /// so a client retry can be recognised in the transcript
    /// ([`Session::has_input`]). Every run takes it, whether or not it
    /// appends a user message.
    pub fn set_next_input_id(&self, input_id: Option<String>) {
        *self
            .next_input_id
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = input_id;
    }

    /// Sets origin facts for only the next input; never grants authority.
    pub fn set_next_input_provenance(&self, provenance: Option<crate::model::InputProvenance>) {
        *self
            .next_input_provenance
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = provenance;
    }

    /// An agent that redacts nothing.
    pub fn new(provider: P, tools: T, session: S, options: Options) -> Self {
        Self::with_redactor(provider, tools, session, options, Redactor::new(&[]))
    }

    /// An agent that redacts `redactor`'s values out of everything it stores,
    /// emits, or returns.
    ///
    /// A redactor whose value list was incomplete cannot promise that dynamic
    /// content is safe, so the agent blanks the model, provider name, and
    /// thinking setting and refuses to run a turn. A complete redactor that
    /// would rewrite one of those settings, or any tool definition, is
    /// downgraded to incomplete for the same reason: the secret is already in
    /// the configuration, so no output can be trusted.
    pub fn with_redactor(
        provider: P,
        tools: T,
        session: S,
        mut options: Options,
        mut redactor: Redactor,
    ) -> Self {
        if redactor.allows_dynamic_content()
            && !boundary_unchanged(&redactor, &options, &tools.definitions())
        {
            redactor = Redactor::with_completeness(&[], false);
        }
        let initial_budget = BudgetScope::Fresh(options.budget).into_turn();
        if !redactor.allows_dynamic_content() {
            options.provider_name = String::new();
            options.model = String::new();
            options.thinking = String::new();
        }
        Self {
            provider,
            tools,
            session,
            options,
            redactor,
            hook_turn_id: std::sync::Mutex::new(String::new()),
            hook_turn_messages: std::sync::Mutex::new(None),
            next_input_id: std::sync::Mutex::new(None),
            next_input_provenance: std::sync::Mutex::new(None),
            budget: std::sync::Mutex::new(initial_budget),
            last_memory_context: std::sync::Mutex::default(),
            last_sent_request: std::sync::Mutex::default(),
        }
    }

    /// What the next ordinary provider request contains, from the same
    /// builder the run loop uses. Per-turn state (a tool-result overlay, the
    /// next recall) does not exist yet, so the request carries neither; the
    /// last turn's recall is listed as its own section. `last_sent` is the last
    /// request that was handed to the provider, if any.
    pub fn context_report(&self) -> context_report::ContextReport {
        let (request, estimated_total) =
            self.build_normal_provider_request(&RunDispatchState::default(), "");
        let estimate_complete = context_estimate::estimate_complete(&request);
        let memory = self
            .last_memory_context
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let last_sent = self
            .last_sent_request
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_deref()
            .cloned();
        let settings = &self.options.compaction;
        let compaction_threshold = match automatic_compaction_triggers(settings) {
            Some((working, _)) if settings.auto => working,
            _ => 0,
        };
        let snapshot = crate::session::context::snapshot_from_state(
            crate::model::Usage::default(),
            false,
            &request.messages,
            self.session.latest_compaction().as_ref(),
        );
        context_report::ContextReport {
            model: request.model.clone(),
            context_window: settings.hard_input_window.max(0),
            compaction_threshold,
            estimated_total,
            estimate_complete,
            reported_input_tokens: snapshot
                .context_input_tokens_present
                .then_some(snapshot.context_input_tokens),
            sections: context_report::sections(
                &request,
                &self.options.system_prompt_parts,
                &memory,
            ),
            last_sent,
        }
    }

    /// The session this agent appends to.
    pub fn session(&self) -> &S {
        &self.session
    }

    /// The provider this agent calls.
    pub fn diagnostics(&self) -> Option<&Arc<dyn crate::diagnostics::Diagnostics>> {
        self.options.diagnostics.as_ref()
    }

    pub fn provider(&self) -> &P {
        &self.provider
    }

    /// The transient mailbox frontends use to steer this agent's active run.
    pub fn steering(&self) -> &Arc<steering::Steering> {
        &self.options.steering
    }

    /// The mailbox shared with producers that can add context to this agent.
    pub fn mailbox(&self) -> &Arc<dyn Mailbox> {
        &self.options.mailbox
    }

    /// Releases the memory binding. Returns the memory binding's error.
    pub fn close(&self) -> Result<(), memory::MemoryError> {
        match &self.options.memory {
            Some(memory) => memory.close(),
            None => Ok(()),
        }
    }
}

fn stopped_provider_error(control: &dyn OperationControl) -> Option<ProviderError> {
    match control.admission_stop_reason()? {
        OperationStopReason::Deadline => Some(ProviderError::DeadlineExceeded),
        OperationStopReason::UserCancellation
        | OperationStopReason::Shutdown
        | OperationStopReason::Migration
        | OperationStopReason::TransportLost
        | OperationStopReason::ProcessLost => Some(ProviderError::Cancelled),
    }
}

fn stopped_tool_execution(reason: OperationStopReason) -> ToolExecution {
    let (error, disposition) = match reason {
        OperationStopReason::UserCancellation => {
            (ProviderError::Cancelled, OperationDisposition::Cancelled)
        }
        OperationStopReason::Deadline => (
            ProviderError::DeadlineExceeded,
            OperationDisposition::DeadlineExceeded,
        ),
        OperationStopReason::Shutdown
        | OperationStopReason::Migration
        | OperationStopReason::TransportLost
        | OperationStopReason::ProcessLost => {
            (ProviderError::Cancelled, OperationDisposition::Interrupted)
        }
    };
    ToolExecution {
        result: ToolResult::error(error.to_string()),
        outcome: OperationOutcome {
            disposition,
            effect_certainty: EffectCertainty::NotStarted,
            stop_reason: Some(reason),
        },
    }
}

/// Substitutes the full tool-result text back into cloned tool-result blocks,
/// in place. `messages` must already be a clone; the session's stored blocks
/// are never mutated.
fn apply_tool_result_overlay(
    messages: &mut [Message],
    overlay: &std::collections::HashMap<String, String>,
) {
    if overlay.is_empty() {
        return;
    }
    for message in messages.iter_mut().filter(|m| m.role == Role::Tool) {
        for block in &mut message.blocks {
            if block.block_type != BlockType::ToolResult {
                continue;
            }
            if let Some(full) = overlay.get(&block.tool_call_id) {
                block.text = full.clone();
            }
        }
    }
}

/// Whether the configuration a complete redactor was built against still
/// survives redaction unchanged. If any of it would be rewritten, one of the
/// redacted values is in the configuration itself and nothing downstream can
/// be trusted.
fn boundary_unchanged(
    redactor: &Redactor,
    options: &Options,
    definitions: &[ToolDefinition],
) -> bool {
    for value in [
        &options.provider_name,
        &options.model,
        &options.system_prompt,
        &options.thinking,
    ] {
        if &redactor.redact_string(value) != value {
            return false;
        }
    }
    // The serialized form contains every string field, so redacting it is the
    // same check.
    let serialized = serde_json::to_string(definitions).unwrap_or_default();
    redactor.redact_string(&serialized) == serialized
}

fn append_system_context(system_prompt: &str, system_context: &str) -> String {
    if system_context.is_empty() {
        system_prompt.to_owned()
    } else if system_prompt.is_empty() {
        system_context.to_owned()
    } else {
        format!("{system_prompt}\n\n{system_context}")
    }
}

fn quote_go(value: &str) -> String {
    serde_json::to_string(value).expect("a string always encodes")
}

/// Trims exactly these bytes: space, tab, newline, carriage return.
fn trim_go_space(text: &str) -> &str {
    text.trim_matches(|character| matches!(character, ' ' | '\t' | '\n' | '\r'))
}

fn empty_object() -> Box<RawValue> {
    RawValue::from_string("{}".to_owned()).expect("valid JSON")
}

#[cfg(test)]
mod tests;
