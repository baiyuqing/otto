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
//! meant to serve one turn at a time. `otto-core` has no async mutex on the
//! wasm target, so the caller must not overlap `run` and `compact`. Cancelling
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

mod capabilities;
pub mod compaction;
pub mod compaction_select;
pub mod context_estimate;
pub mod context_report;
pub mod events;
pub mod hooks;
pub mod inbox;
pub mod memory;
pub mod oneshot;
pub mod overflow;
pub mod progress;
pub mod redactor;
pub mod steering;
pub mod summary;
pub mod summary_details;
pub mod summary_validate;
pub mod tasks;

/// The built-in task `/init` submits through the ordinary agent turn.
pub const INIT_PROMPT: &str = "Create an AGENTS.md contributor guide for this repository. First inspect the workspace and check whether AGENTS.md already exists at its root. If it exists, do not overwrite or modify it. Otherwise, write a concise, repository-specific guide covering project structure, build and test commands, coding conventions, and contribution expectations.";

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
use crate::tool::{ToolCall, ToolExecution, ToolExecutor, ToolResult};

use crate::mailbox::{InMemoryMailbox, Mailbox};
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
use tasks::TaskRegistry;

/// Turn settings and the two injected capabilities the core cannot provide
/// itself: the clock and the identifier generator. Keeping them here is what
/// lets `otto-core` build for `wasm32-unknown-unknown`.
pub struct Options {
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
    /// The subagent task registry, closed by [`Agent::close`].
    pub tasks: Option<Arc<dyn TaskRegistry + Send + Sync>>,
    /// The running Otto package version, recorded with the tool set. Empty
    /// when unknown.
    pub version: String,
}

/// The default clock returns the zero time and the default identifier
/// generator returns the empty string: `otto-core` has no clock and no
/// randomness of its own, so a real caller must replace both.
impl Default for Options {
    fn default() -> Self {
        let operation_counter = AtomicU64::new(0);
        Self {
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
            memory: None,
            memory_recall_limit: DEFAULT_RECALL_LIMIT,
            memory_recall_token_budget: DEFAULT_RECALL_TOKEN_BUDGET,
            mailbox: Arc::new(InMemoryMailbox::default()),
            steering: Arc::new(steering::Steering::default()),
            tasks: None,
            version: String::new(),
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
    hook_turn_id: std::sync::Mutex<String>,
    /// The client input id of the next run's user message; taken by every
    /// run, used or not. See [`Agent::set_next_input_id`].
    next_input_id: std::sync::Mutex<Option<String>>,
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
            next_input_id: std::sync::Mutex::new(None),
            last_memory_context: std::sync::Mutex::default(),
        }
    }

    /// What the next ordinary provider request contains, from the same
    /// builder the run loop uses. Per-turn state (a tool-result overlay, the
    /// next recall) does not exist yet, so the request carries neither; the
    /// last turn's recall is listed as its own section.
    pub fn context_report(&self) -> context_report::ContextReport {
        let (request, estimated_total) =
            self.build_normal_provider_request(&RunDispatchState::default(), "");
        let estimate_complete = context_estimate::estimate_complete(&request);
        let memory = self
            .last_memory_context
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
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
        }
    }

    /// The session this agent appends to.
    pub fn session(&self) -> &S {
        &self.session
    }

    /// The provider this agent calls.
    pub fn provider(&self) -> &P {
        &self.provider
    }

    /// The transient mailbox frontends use to steer this agent's active run.
    pub fn steering(&self) -> &Arc<steering::Steering> {
        &self.options.steering
    }

    /// The task registry, if one was configured.
    pub fn tasks(&self) -> Option<&Arc<dyn TaskRegistry + Send + Sync>> {
        self.options.tasks.as_ref()
    }

    /// The mailbox shared with producers that can add context to this agent.
    pub fn mailbox(&self) -> &Arc<dyn Mailbox> {
        &self.options.mailbox
    }

    /// Releases the memory binding and cancels every tracked task.
    ///
    /// Returns the memory binding's error; the tasks are closed either way.
    pub fn close(&self) -> Result<(), memory::MemoryError> {
        if let Some(tasks) = &self.options.tasks {
            tasks.close();
        }
        match &self.options.memory {
            Some(memory) => memory.close(),
            None => Ok(()),
        }
    }

    /// Runs one turn: appends `user_text`, then alternates provider calls and
    /// tool calls until the model returns a message with no tool call.
    ///
    /// Empty `user_text` is allowed only when the inbox has notifications
    /// waiting; that is a wake turn, which appends no user message and skips
    /// memory recall because there is no query text.
    pub async fn run(
        &self,
        user_text: &str,
        emit: EventSink<'_>,
        cancel: &CancellationToken,
    ) -> Result<(), AgentError> {
        self.run_with_system_context(user_text, "", emit, cancel)
            .await
    }

    /// Runs one turn with ephemeral system context appended to the configured
    /// system prompt for this run's ordinary provider requests only.
    pub async fn run_with_system_context(
        &self,
        user_text: &str,
        system_context: &str,
        emit: EventSink<'_>,
        cancel: &CancellationToken,
    ) -> Result<(), AgentError> {
        self.run_with_system_context_control(user_text, system_context, emit, cancel)
            .await
    }

    /// Runs one turn under operation-wide cancellation and deadline control.
    pub async fn run_with_control(
        &self,
        user_text: &str,
        emit: EventSink<'_>,
        control: &dyn OperationControl,
    ) -> Result<(), AgentError> {
        self.run_with_system_context_control(user_text, "", emit, control)
            .await
    }

    /// Runs one turn with ephemeral system context under operation-wide
    /// cancellation and deadline control.
    pub async fn run_with_system_context_control(
        &self,
        user_text: &str,
        system_context: &str,
        emit: EventSink<'_>,
        control: &dyn OperationControl,
    ) -> Result<(), AgentError> {
        self.run_with_image_system_context_control(user_text, None, system_context, emit, control)
            .await
    }

    pub async fn run_with_image(
        &self,
        user_text: &str,
        image: Option<Block>,
        emit: EventSink<'_>,
        cancel: &CancellationToken,
    ) -> Result<(), AgentError> {
        self.run_with_image_system_context_control(user_text, image, "", emit, cancel)
            .await
    }

    pub async fn run_with_image_system_context(
        &self,
        user_text: &str,
        image: Option<Block>,
        system_context: &str,
        emit: EventSink<'_>,
        cancel: &CancellationToken,
    ) -> Result<(), AgentError> {
        self.run_with_image_system_context_control(user_text, image, system_context, emit, cancel)
            .await
    }

    /// Runs one turn with an optional image under operation-wide control.
    pub async fn run_with_image_control(
        &self,
        user_text: &str,
        image: Option<Block>,
        emit: EventSink<'_>,
        control: &dyn OperationControl,
    ) -> Result<(), AgentError> {
        self.run_with_image_system_context_control(user_text, image, "", emit, control)
            .await
    }

    pub async fn run_with_image_system_context_control(
        &self,
        user_text: &str,
        image: Option<Block>,
        system_context: &str,
        emit: EventSink<'_>,
        control: &dyn OperationControl,
    ) -> Result<(), AgentError> {
        self.run_with_image_system_context_control_kind(
            user_text,
            image,
            system_context,
            emit,
            control,
            false,
        )
        .await
    }

    /// Runs one agent-originated follow-up, recording its request as context
    /// rather than as a user message.
    pub async fn run_followup_with_system_context_control(
        &self,
        text: &str,
        system_context: &str,
        emit: EventSink<'_>,
        control: &dyn OperationControl,
    ) -> Result<(), AgentError> {
        self.run_with_image_system_context_control_kind(
            text,
            None,
            system_context,
            emit,
            control,
            true,
        )
        .await
    }

    async fn run_with_image_system_context_control_kind(
        &self,
        user_text: &str,
        image: Option<Block>,
        system_context: &str,
        emit: EventSink<'_>,
        control: &dyn OperationControl,
        followup: bool,
    ) -> Result<(), AgentError> {
        if self.options.hooks.is_none() || !self.redactor.allows_dynamic_content() {
            return self
                .run_inner(user_text, image, system_context, emit, control, followup)
                .await;
        }
        *self
            .hook_turn_id
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = (self.options.new_operation_id)()
            .map_err(|message| self.fail(emit, AgentError::OperationIdentity { message }))?
            .to_string();
        let result = async {
            let mut context = system_context.to_owned();
            let start=self.start_hooks(control).await.map_err(|message|self.fail(emit,self.hook_error(message,control)))?;
            if start.decision==Some(hooks::Decision::Deny) {return Err(self.fail(emit,AgentError::Other(start.reason.unwrap_or_else(||"Hook denied session".into()))));}
            if let Some(text)=start.context {context.push_str(&format!("\n<plugin_context>Untrusted plugin context:\n{text}\n</plugin_context>"));}
            for event in ["before_prompt"] {
                let response = self.hook(event, serde_json::json!({"prompt": self.redactor.redact_string(user_text), "followup":followup,"has_image":image.is_some()}), control).await
                    .map_err(|message| self.fail(emit, self.hook_error(message,control)))?;
                if response.decision == Some(hooks::Decision::Deny) {
                    return Err(self.fail(emit, AgentError::Other(response.reason.unwrap_or_else(|| "Hook denied this turn".into()))));
                }
                if let Some(text) = response.context { context.push_str(&format!("\n<plugin_context>Untrusted plugin context:\n{text}\n</plugin_context>")); }
            }
            self.run_inner(user_text, image, &context, emit, control, followup).await
        }.await;
        let end_control = CancellationToken::new();
        let status = if result.is_ok() {
            "success"
        } else if control.stop_reason().is_some() {
            "cancelled"
        } else {
            "error"
        };
        if let Err(message) = self
            .hook(
                "turn_end",
                serde_json::json!({"status":status,"stop_reason":control.stop_reason(),"error":result.as_ref().err().map(|e|self.redactor.redact_string(&e.to_string()))}),
                &end_control,
            )
            .await
        {
            self.hook_warning(emit, &message);
        }
        self.hook_turn_id
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
        result
    }

    async fn run_inner(
        &self,
        user_text: &str,
        image: Option<Block>,
        system_context: &str,
        emit: EventSink<'_>,
        control: &dyn OperationControl,
        followup: bool,
    ) -> Result<(), AgentError> {
        let input_id = self
            .next_input_id
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if !self.redactor.allows_dynamic_content() {
            return self.run_with_incomplete_redactions(emit, control);
        }
        let text = trim_go_space(user_text);
        if text.is_empty() && image.is_none() && self.options.mailbox.is_empty() {
            return Err(self.fail(emit, AgentError::EmptyUserText));
        }
        let wake = text.is_empty() && image.is_none();
        let (turn_id, resumes) = self
            .start_turn(wake)
            .await
            .map_err(|error| self.fail(emit, error))?;
        let result = self
            .run_turn(
                user_text,
                image,
                system_context,
                emit,
                control,
                followup,
                (&turn_id, resumes.as_deref()),
                input_id,
            )
            .await;
        self.end_turn(turn_id, &result, control).await;
        result
    }

    /// Appends the `otto.turn` start record, the turn's first durable write.
    /// A wake turn that follows a turn process loss interrupted is recorded
    /// as that turn's recovery; every other turn starts a new chain. Returns
    /// the turn id and, when this turn follows an interrupted turn that has
    /// not used its automatic resume, that turn's id: its journaled steering
    /// is delivered by this turn.
    async fn start_turn(&self, wake: bool) -> Result<(String, Option<String>), AgentError> {
        let turn_id = (self.options.new_operation_id)()
            .map_err(|message| AgentError::OperationIdentity { message })?
            .to_string();
        let interrupted = self.session.interrupted_turn();
        let resumes = interrupted
            .as_ref()
            .filter(|prior| !prior.resume_exhausted())
            .map(|prior| prior.turn_id.clone());
        let record = match interrupted {
            Some(prior) if wake => TurnRecord::started(
                turn_id.clone(),
                TurnTrigger::Recovery,
                prior.attempt.saturating_add(1),
                Some(prior.turn_id),
            ),
            _ => TurnRecord::started(
                turn_id.clone(),
                if wake {
                    TurnTrigger::Wake
                } else {
                    TurnTrigger::User
                },
                1,
                None,
            ),
        };
        self.session
            .append_custom(TURN_CUSTOM_TYPE, &encode_turn_record(&record))
            .await
            .map_err(|source| AgentError::Persist {
                kind: "turn start".into(),
                source,
            })?;
        Ok((turn_id, resumes))
    }

    /// Appends the `otto.turn` end record. A failed append is not reported:
    /// the turn keeps its own result, and a store whose write failed rejects
    /// every later write anyway.
    async fn end_turn(
        &self,
        turn_id: String,
        result: &Result<(), AgentError>,
        control: &dyn OperationControl,
    ) {
        let record = match result {
            Ok(()) => TurnRecord::ended(turn_id, TurnOutcome::Completed),
            Err(_) if control.stop_reason().is_some() => {
                TurnRecord::ended(turn_id, TurnOutcome::Cancelled)
            }
            Err(error) => {
                TurnRecord::failed(turn_id, &self.redactor.redact_string(&error.to_string()))
            }
        };
        let _ = self
            .session
            .append_custom(TURN_CUSTOM_TYPE, &encode_turn_record(&record))
            .await;
    }

    /// The body of one admitted turn, between its `otto.turn` records.
    /// `turn` is this turn's id and the interrupted turn it resumes, if any.
    #[allow(clippy::too_many_arguments)]
    async fn run_turn(
        &self,
        user_text: &str,
        image: Option<Block>,
        system_context: &str,
        emit: EventSink<'_>,
        control: &dyn OperationControl,
        followup: bool,
        turn: (&str, Option<&str>),
        input_id: Option<String>,
    ) -> Result<(), AgentError> {
        let text = trim_go_space(user_text);
        let steering = self.options.steering.begin_turn(turn.0, turn.1);
        emit(Event::AgentStarted);
        if let Some(error) = stopped_provider_error(control) {
            let operation_id = match (self.options.new_operation_id)() {
                Ok(id) => id,
                Err(message) => {
                    return Err(self.fail(emit, AgentError::OperationIdentity { message }));
                }
            };
            let reason = control
                .stop_reason()
                .unwrap_or(OperationStopReason::UserCancellation);
            let settlement = ProviderSettlement::stopped(
                if reason == OperationStopReason::Deadline {
                    ProviderError::DeadlineExceeded
                } else {
                    ProviderError::Cancelled
                },
                0,
                EffectCertainty::NotStarted,
                reason,
            );
            self.emit_provider_api_call(
                emit,
                operation_id,
                ProviderOperationKind::Ordinary,
                std::time::Duration::ZERO,
                &settlement,
            );
            return Err(self.fail(emit, AgentError::Provider(error)));
        }

        // Notifications queued before this turn started (a session-lease
        // recovery notice, a task that finished while the user was away)
        // are delivered before the user's own message is appended, so the
        // provider sees them first: the user's prompt reads as a response
        // to what is already in the transcript, not the other way around.
        // Calls a session takeover left without a result come first: nothing
        // may be appended between an assistant message and its tool results.
        let system_context = self.redactor.redact_string(system_context);
        let mut state = RunDispatchState {
            progress: ProgressGuard::new(self.options.progress),
            ..RunDispatchState::default()
        };
        self.replay_pending_calls(emit, &mut state, control).await?;
        if let Some(stall) = state.progress.take_stall() {
            return Err(self.fail(emit, AgentError::NoProgress(stall)));
        }

        if let Err(error) = self.deliver_notifications(emit).await {
            return Err(self.fail(emit, error));
        }

        if let Err(error) = self.announce_tool_changes().await {
            return Err(self.fail(emit, error));
        }

        if !text.is_empty() || image.is_some() {
            let redacted = self.redactor.redact_string(user_text);
            let persisted_text = if followup {
                format!("[Agent follow-up]\n{redacted}")
            } else {
                redacted.clone()
            };
            let mut blocks = Vec::new();
            if !text.is_empty() {
                blocks.push(Block::text(persisted_text));
            }
            blocks.extend(image);
            let user = Message {
                id: (self.options.new_id)(),
                role: if followup { Role::Context } else { Role::User },
                context_type: if followup {
                    "agent_followup".into()
                } else {
                    String::new()
                },
                display: followup,
                created_at: (self.options.now)(),
                blocks,
                input_id: input_id.unwrap_or_default(),
                ..Message::default()
            };
            if let Err(source) = self.session.append(user).await {
                return Err(self.fail(
                    emit,
                    AgentError::Persist {
                        kind: "user message".into(),
                        source,
                    },
                ));
            }
            if !text.is_empty()
                && let Some(binding) = &self.options.memory
            {
                let request = memory::RecallRequest {
                    query: redacted,
                    limit: self.options.memory_recall_limit,
                    token_budget: self.options.memory_recall_token_budget,
                };
                match binding.recall(&request, control.cancellation_token()).await {
                    Ok(result) => {
                        state.memory_context = self
                            .redactor
                            .redact_string(&memory::render_memory_context(&result.records));
                    }
                    Err(error) => emit(Event::MemoryWarning {
                        message: error.to_string(),
                    }),
                }
            }
            *self
                .last_memory_context
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = state.memory_context.clone();
        }
        if let Err(error) = self.deliver_steering(&steering, emit).await {
            return Err(self.fail(emit, error));
        }

        loop {
            state.yield_after_batch = false;
            state.batch_has_errors = false;
            let response = match self
                .dispatch_normal_provider_step(emit, &mut state, &system_context, control)
                .await
            {
                Ok(response) => response,
                Err(error) => return Err(self.fail(emit, error)),
            };

            let cached_input_tokens_present = response.cached_input_tokens_present;
            let mut assistant = self.redact_message(&response.message);
            if assistant.id.is_empty() {
                assistant.id = (self.options.new_id)();
            }
            if assistant.role == Role::default() {
                assistant.role = Role::Assistant;
            }
            if assistant.created_at == zero_time() {
                assistant.created_at = (self.options.now)();
            }
            if let Err(error) = compaction::validate_provider_response_message(&assistant) {
                return Err(self.fail(emit, AgentError::InvalidResponse(error)));
            }

            let usage = assistant.usage;
            let tool_calls: Vec<Block> = assistant
                .blocks
                .iter()
                .filter(|block| block.block_type == BlockType::ToolCall)
                .cloned()
                .collect();
            if let Err(source) = self.session.append(assistant).await {
                return Err(self.fail(
                    emit,
                    AgentError::Persist {
                        kind: "assistant message".into(),
                        source,
                    },
                ));
            }
            emit(Event::ProviderUsage {
                usage: usage.unwrap_or_default(),
                present: usage.is_some(),
                cached_input_tokens_present,
            });

            let had_tool_call = !tool_calls.is_empty();
            // Tool calls run one at a time, so a frontend can reserve bounded
            // delivery for the terminal event of the single active call.
            let mut replan = false;
            for block in tool_calls {
                let batch = self
                    .run_tool_call(emit, &mut state, control, &block, None, replan)
                    .await?;
                replan |= batch == crate::approval::BatchControl::Replan;
            }
            // The whole batch ran before the guard is read, so a stopped turn
            // never leaves an assistant tool call without its result.
            if let Some(stall) = state.progress.take_stall() {
                return Err(self.fail(emit, AgentError::NoProgress(stall)));
            }

            if !had_tool_call || (state.yield_after_batch && !state.batch_has_errors && !replan) {
                if let Some(error) = stopped_provider_error(control) {
                    return Err(self.fail(emit, AgentError::Provider(error)));
                }
                if self.options.mailbox.queued().iter().any(|entry| {
                    state.yield_after_batch
                        || entry.notification.kind == Some(inbox::NotificationKind::UserMessage)
                }) {
                    if let Err(error) = self.deliver_notifications(emit).await {
                        return Err(self.fail(emit, error));
                    }
                    continue;
                }
                if let Some(messages) = steering.take_or_close() {
                    if let Err(error) = self.append_steering(messages, emit).await {
                        return Err(self.fail(emit, error));
                    }
                    continue;
                }
                emit(Event::AgentFinished);
                return Ok(());
            }
            if let Some(error) = stopped_provider_error(control) {
                return Err(self.fail(emit, AgentError::Provider(error)));
            }
            if let Err(error) = self.deliver_steering(&steering, emit).await {
                return Err(self.fail(emit, error));
            }
            if let Err(error) = self.deliver_notifications(emit).await {
                return Err(self.fail(emit, error));
            }
        }
    }

    /// Runs one tool call to its durable result: attempt fact, execution,
    /// terminal fact, result message. `prior` is the operation id and the
    /// attempts already recorded when this is a replay of a call a takeover
    /// left without a result; `None` starts a new operation at attempt 1.
    /// Failures are reported through `emit` before they are returned.
    async fn run_tool_call(
        &self,
        emit: EventSink<'_>,
        state: &mut RunDispatchState,
        control: &dyn OperationControl,
        block: &Block,
        prior: Option<(OperationId, u32)>,
        skipped: bool,
    ) -> Result<crate::approval::BatchControl, AgentError> {
        let mut arguments = block.arguments.clone().unwrap_or_else(empty_object);
        let plan_before = self.session.plan();
        let (operation_id, attempt) = match prior {
            Some((operation_id, attempts)) => (operation_id, attempts + 1),
            None => match (self.options.new_operation_id)() {
                Ok(id) => (id, 1),
                Err(message) => {
                    return Err(self.fail(emit, AgentError::OperationIdentity { message }));
                }
            },
        };
        let attempt_fact = OperationFact::attempt(
            operation_id.clone(),
            attempt,
            block.tool_call_id.clone(),
            block.tool_name.clone(),
        )
        .with_retry_safety(self.tools.retry_safety(&block.tool_name));
        if let Err(source) = self.session.append_operation_fact(attempt_fact).await {
            return Err(self.fail(
                emit,
                AgentError::Persist {
                    kind: format!("operation attempt for {}", quote_go(&block.tool_call_id)),
                    source,
                },
            ));
        }
        emit(Event::ToolCallStarted {
            operation_id: operation_id.clone(),
            attempt,
            tool_name: block.tool_name.clone(),
            tool_call_id: block.tool_call_id.clone(),
            arguments: arguments.get().to_owned(),
        });
        let mut hook_denial = None;
        if !skipped
            && control.admission_stop_reason().is_none()
            && self.options.hooks.is_some()
            && self
                .tools
                .definitions()
                .iter()
                .any(|tool| tool.name == block.tool_name)
        {
            match self.hook("before_tool", serde_json::json!({"tool":block.tool_name, "tool_call_id":block.tool_call_id,"operation_id":operation_id, "attempt":attempt,"arguments":serde_json::from_str::<serde_json::Value>(arguments.get()).unwrap_or_default()}), control).await {
                Err(message) => hook_denial = Some(message),
                Ok(response) if response.decision == Some(hooks::Decision::Deny) => hook_denial = Some(response.reason.unwrap_or_else(|| "Hook denied this tool".into())),
                Ok(response) => if let Some(value) = response.arguments {
                    if !value.is_object() { hook_denial = Some("Hook arguments must be a JSON object".into()); }
                    else {
                        let replacement = RawValue::from_string(value.to_string()).expect("serialized JSON");
                        if replacement.get() != arguments.get() {
                            let audit = serde_json::json!({"version":1,"tool_call_id":block.tool_call_id,"operation_id":operation_id,"arguments":value}).to_string();
                            self.session.append_custom("otto.hook.arguments", self.redactor.redact_json_strings(&audit).get()).await.map_err(|source| self.fail(emit, AgentError::Persist {kind:"hook arguments".into(), source}))?;
                            arguments = replacement;
                        }
                    }
                },
            }
        }
        let hook_blocked = hook_denial.is_some();
        let mut execution = if let Some(reason) = control.admission_stop_reason() {
            stopped_tool_execution(reason)
        } else if let Some(message) = hook_denial {
            ToolExecution {
                result: ToolResult::error(message),
                outcome: OperationOutcome {
                    disposition: OperationDisposition::Error,
                    effect_certainty: EffectCertainty::NotStarted,
                    stop_reason: None,
                },
            }
        } else if skipped {
            ToolExecution {
                result: ToolResult::error("Not started: a preceding approval requires replanning."),
                outcome: OperationOutcome {
                    disposition: OperationDisposition::Error,
                    effect_certainty: EffectCertainty::NotStarted,
                    stop_reason: None,
                },
            }
        } else if let Some(reason) = control.admission_stop_reason() {
            stopped_tool_execution(reason)
        } else {
            self.tools
                .execute(
                    ToolCall {
                        tool_call_id: &block.tool_call_id,
                        operation_id: &operation_id,
                        name: &block.tool_name,
                        arguments: &arguments,
                        attempt,
                    },
                    control,
                )
                .await
        };
        if self.options.hooks.is_some()
            && execution.outcome.effect_certainty != EffectCertainty::NotStarted
        {
            // After hooks have their own bounded cleanup budget, even after cancellation.
            let token = CancellationToken::new();
            match self.hook("after_tool", serde_json::json!({"tool":block.tool_name,"tool_call_id":block.tool_call_id,"operation_id":operation_id,"attempt":attempt,"arguments":serde_json::from_str::<serde_json::Value>(arguments.get()).unwrap_or_default(),"content":self.redactor.redact_string(&execution.result.content),"is_error":execution.result.is_error,"outcome":execution.outcome}), &token).await {
                Err(message) => self.hook_warning(emit, &message),
                Ok(response) => if let Some(content) = response.content {
                    let audit = serde_json::json!({"version":1,"tool_call_id":block.tool_call_id,"operation_id":operation_id,"content":execution.result.persisted_text(),"is_error":execution.result.is_error,"outcome":execution.outcome}).to_string();
                    self.session.append_custom("otto.hook.result", self.redactor.redact_json_strings(&audit).get()).await.map_err(|source| self.fail(emit, AgentError::Persist{kind:"original tool result".into(),source}))?;
                    execution.result.content = content;
                    execution.result.persisted_content = None;
                },
            }
        }
        if let Some(advice) = state.progress.edit_advice(
            &block.tool_name,
            arguments.get(),
            execution.result.is_error
                || execution.outcome.disposition != OperationDisposition::Succeeded,
        ) {
            execution.result.content.push_str(advice);
            if let Some(persisted) = &mut execution.result.persisted_content {
                persisted.push_str(advice);
            }
        }
        state.yield_after_batch |= self.tools.yields_turn(&block.tool_name, &execution);
        state.batch_has_errors |= execution.result.is_error
            || execution.outcome.disposition != OperationDisposition::Succeeded;
        execution.result.content = self.redactor.redact_string(&execution.result.content);
        execution.result.persisted_content = execution
            .result
            .persisted_content
            .as_deref()
            .map(|text| self.redactor.redact_string(text));
        let persisted_text = match &execution.result.persisted_content {
            Some(persisted) => {
                // The stored text is a placeholder, so the live text
                // is kept for this turn's provider requests only.
                state
                    .tool_result_overlay
                    .insert(block.tool_call_id.clone(), execution.result.content.clone());
                persisted.clone()
            }
            None => execution.result.content.clone(),
        };
        let batch_control = if hook_blocked {
            crate::approval::BatchControl::Replan
        } else {
            self.tools.batch_control(&block.tool_name, &execution)
        };
        let is_error = execution.result.is_error;
        // Observed here because this is the only point that knows both the
        // arguments the model sent and whether the result failed.
        state
            .progress
            .observe(&block.tool_name, arguments.get(), is_error);
        let terminal = OperationFact::terminal(
            operation_id.clone(),
            attempt,
            block.tool_call_id.clone(),
            block.tool_name.clone(),
            execution.outcome.clone(),
        );
        if let Err(source) = self.session.append_operation_fact(terminal).await {
            emit(Event::ToolCallFinished {
                operation_id,
                attempt,
                tool_name: block.tool_name.clone(),
                tool_call_id: block.tool_call_id.clone(),
                result: ToolResult::error(
                    "tool outcome unknown: could not persist terminal status",
                ),
                outcome: OperationOutcome {
                    disposition: OperationDisposition::Error,
                    effect_certainty: EffectCertainty::Unknown,
                    stop_reason: None,
                },
            });
            return Err(self.fail(
                emit,
                AgentError::Persist {
                    kind: format!("operation terminal for {}", quote_go(&block.tool_call_id)),
                    source,
                },
            ));
        }
        let operation_metadata = ToolResultMetadata {
            operation_id: Some(operation_id.clone()),
            disposition: execution.outcome.disposition,
            effect_certainty: execution.outcome.effect_certainty,
            stop_reason: execution.outcome.stop_reason,
        };
        emit(Event::ToolCallFinished {
            operation_id: operation_id.clone(),
            attempt,
            tool_name: block.tool_name.clone(),
            tool_call_id: block.tool_call_id.clone(),
            result: execution.result,
            outcome: execution.outcome,
        });
        let plan_after = self.session.plan();
        if plan_after != plan_before {
            emit(Event::PlanUpdated { steps: plan_after });
        }
        let stored = Message {
            id: (self.options.new_id)(),
            role: Role::Tool,
            created_at: (self.options.now)(),
            blocks: vec![Block {
                block_type: BlockType::ToolResult,
                text: persisted_text,
                tool_call_id: block.tool_call_id.clone(),
                tool_name: block.tool_name.clone(),
                is_error,
                operation_metadata: Some(operation_metadata),
                ..Block::default()
            }],
            ..Message::default()
        };
        // `Session::append` is likewise not cancellable.
        if let Err(source) = self.session.append(stored).await {
            return Err(self.fail(
                emit,
                AgentError::Persist {
                    kind: format!("tool result for {}", quote_go(&block.tool_call_id)),
                    source,
                },
            ));
        }
        Ok(batch_control)
    }

    /// Runs again the calls a takeover left without a result, when every one
    /// of them names a replayable tool and has at most one recorded attempt
    /// with no terminal fact (the same all-or-nothing rule the store applies
    /// when it leaves them pending). Each runs as the next attempt of its
    /// recorded operation, or as attempt 1 of a new one when none was
    /// recorded. Does nothing for any other pending set, which the store has
    /// already answered.
    async fn replay_pending_calls(
        &self,
        emit: EventSink<'_>,
        state: &mut RunDispatchState,
        control: &dyn OperationControl,
    ) -> Result<(), AgentError> {
        let Ok(pending) = pending_tool_calls(&self.session.messages()) else {
            return Ok(());
        };
        if pending.is_empty()
            || !pending
                .iter()
                .all(|call| self.options.hooks.is_none() && self.tools.replayable(&call.tool_name))
        {
            return Ok(());
        }
        let ledger = self.session.operation_ledger();
        let mut priors = Vec::with_capacity(pending.len());
        for call in &pending {
            let record = ledger.operation_for_tool_call(&call.tool_call_id);
            if !replay_allowed(record, true) {
                return Ok(());
            }
            priors.push(record.map(|record| (record.operation_id.clone(), record.attempts)));
        }
        for (call, prior) in pending.iter().zip(priors) {
            self.run_tool_call(emit, state, control, call, prior, false)
                .await?;
        }
        Ok(())
    }

    /// Appends each queued notification as a display context message and
    /// reports it, removing it from the inbox only after its append
    /// returned. A no-op when the inbox is empty. A failed append leaves that
    /// item and every later one queued, so a following call (or a later
    /// resume, for a persisted inbox) delivers it again. A notification with
    /// an id is appended through [`Session::append_input_if_absent`], so one
    /// that was appended before an interruption kept it queued is
    /// acknowledged without a second message.
    ///
    /// The queue is read once at the top, not drained: an item pushed by
    /// another task while this loop awaits an append is not in this read and
    /// is picked up by the next call, in order, rather than lost or
    /// reordered.
    async fn deliver_notifications(&self, emit: EventSink<'_>) -> Result<(), AgentError> {
        for entry in self.options.mailbox.queued() {
            let notification = entry.notification;
            let text = self.redactor.redact_string(&notification.text);
            let user_message = notification.kind == Some(inbox::NotificationKind::UserMessage);
            let metadata = ContextMetadata {
                task_id: notification.task_id.clone(),
            };
            let context_metadata =
                (!user_message && metadata.validate().is_ok()).then_some(metadata);
            let message = Message {
                id: (self.options.new_id)(),
                role: if user_message {
                    Role::User
                } else {
                    Role::Context
                },
                context_type: if user_message {
                    String::new()
                } else {
                    notification.context_type().to_owned()
                },
                display: !user_message,
                created_at: (self.options.now)(),
                usage: notification.usage,
                context_metadata,
                blocks: vec![Block::text(text.clone())],
                ..Message::default()
            };
            let appended = if notification.id.is_empty() {
                self.session.append(message).await.map(|()| true)
            } else {
                self.session
                    .append_input_if_absent(&notification.id, message)
                    .await
                    .map(|outcome| outcome == crate::session::AppendOutcome::Appended)
            }
            .map_err(|source| AgentError::Persist {
                kind: "notification".into(),
                source,
            })?;
            self.options.mailbox.ack(entry.seq);
            if !appended {
                // Delivered before an interruption that kept it queued.
                continue;
            }
            emit(Event::Notification {
                kind: notification.kind,
                task_id: notification.task_id,
                text,
                usage: notification.usage.unwrap_or_default(),
                present: notification.usage.is_some(),
            });
        }
        Ok(())
    }

    /// Appends the one-time capability change notice as a context message,
    /// then the `otto.capabilities` record. The message goes first: a failure
    /// between the two repeats the notice, while the reverse order could lose
    /// it. The record is also rewritten, without a notice, when only fields
    /// that produce no notice differ, such as the Otto version.
    async fn announce_tool_changes(&self) -> Result<(), AgentError> {
        let current = self.current_capabilities();
        let observed = self.session.observed_capabilities();
        let has_history = self.session.latest_compaction().is_some()
            || self
                .session
                .model_messages()
                .iter()
                .any(|message| message.role == Role::Assistant);
        let notice = capabilities::notice(observed.as_ref(), &current, has_history);
        if !notice.is_empty() {
            let message = crate::session::context::new_context_message(
                &(self.options.new_id)(),
                crate::session::CAPABILITY_UPDATE_CONTEXT_TYPE,
                true,
                notice,
                (self.options.now)(),
                None,
            );
            self.session
                .append(message)
                .await
                .map_err(|source| AgentError::Persist {
                    kind: "capability update".into(),
                    source,
                })?;
        }
        if observed.as_ref() != Some(&current) {
            self.session
                .append_custom(
                    crate::session::CAPABILITIES_CUSTOM_TYPE,
                    &crate::session::encode_capabilities(&current),
                )
                .await
                .map_err(|source| AgentError::Persist {
                    kind: "capability record".into(),
                    source,
                })?;
        }
        Ok(())
    }

    async fn deliver_steering(
        &self,
        steering: &steering::Run,
        emit: EventSink<'_>,
    ) -> Result<(), AgentError> {
        self.append_steering(steering.drain(), emit).await
    }

    async fn append_steering(
        &self,
        messages: Vec<steering::Message>,
        emit: EventSink<'_>,
    ) -> Result<(), AgentError> {
        for steering in messages {
            let text = self.redactor.redact_string(&steering.text);
            let message = Message {
                id: (self.options.new_id)(),
                role: Role::User,
                created_at: (self.options.now)(),
                blocks: vec![Block::text(text.clone())],
                ..Message::default()
            };
            let appended = if steering.input_id.is_empty() {
                self.session.append(message).await.map(|()| true)
            } else {
                self.session
                    .append_input_if_absent(&steering.input_id, message)
                    .await
                    .map(|outcome| outcome == crate::session::AppendOutcome::Appended)
            }
            .map_err(|source| AgentError::Persist {
                kind: "steering message".into(),
                source,
            })?;
            self.options.steering.delivered(&steering);
            if !appended {
                continue;
            }
            emit(Event::Notification {
                kind: Some(inbox::NotificationKind::UserMessage),
                task_id: String::new(),
                text,
                usage: Usage::default(),
                present: false,
            });
        }
        Ok(())
    }

    /// One provider step, with the two automatic compaction paths around it:
    /// proactive compaction when the estimate crosses the soft trigger, and
    /// one retry after the provider reports a context overflow.
    async fn dispatch_normal_provider_step(
        &self,
        emit: EventSink<'_>,
        state: &mut RunDispatchState,
        system_context: &str,
        control: &dyn OperationControl,
    ) -> Result<Response, AgentError> {
        if let Some(error) = stopped_provider_error(control) {
            let operation_id = (self.options.new_operation_id)()
                .map_err(|message| AgentError::OperationIdentity { message })?;
            let settlement = ProviderSettlement::stopped(
                if matches!(&error, ProviderError::DeadlineExceeded) {
                    ProviderError::DeadlineExceeded
                } else {
                    ProviderError::Cancelled
                },
                0,
                EffectCertainty::NotStarted,
                control
                    .stop_reason()
                    .unwrap_or(OperationStopReason::UserCancellation),
            );
            self.emit_provider_api_call(
                emit,
                operation_id,
                ProviderOperationKind::Ordinary,
                std::time::Duration::ZERO,
                &settlement,
            );
            return Err(AgentError::Provider(error));
        }
        let (mut request, mut estimate) = self.build_normal_provider_request(state, system_context);
        let mut estimate_complete = context_estimate::estimate_complete(&request);
        let triggers = automatic_compaction_triggers(&self.options.compaction);

        let proactive_reason = triggers.and_then(|(soft_trigger, _)| {
            if !self.options.compaction.auto || !estimate_complete {
                None
            } else if estimate > soft_trigger {
                Some(CompactionReason::Threshold)
            } else if estimate > soft_trigger / 2 && self.last_response_idle() {
                Some(CompactionReason::Idle)
            } else {
                None
            }
        });

        if let Some((_, hard_trigger)) = triggers
            && let Some(reason) = proactive_reason
        {
            if !state.proactive_attempted {
                state.proactive_attempted = true;
                // Compaction futures live on the heap so the local and remote
                // compaction state machines do not enlarge every turn future.
                match Box::pin(self.compact_locked(reason, "", emit, control)).await {
                    Err(error) if error.is_nothing_to_compact() => {
                        emit(Event::CompactionCompleted {
                            compaction: CompactionResult {
                                reason,
                                automatic: true,
                                noop: true,
                                ..CompactionResult::default()
                            },
                        });
                        state.proactive_attempted = false;
                    }
                    Err(error) => {
                        if let Some(stop) = stopped_provider_error(control) {
                            return Err(AgentError::Provider(stop));
                        }
                        if let Some(cancellation) = automatic_cancellation(false, &error) {
                            return Err(cancellation);
                        }
                        if estimate >= hard_trigger {
                            return Err(automatic_dispatch_error(
                                AUTOMATIC_COMPACTION_HARD_FAILURE_MESSAGE,
                                vec![error.to_string()],
                            ));
                        }
                        emit(Event::CompactionWarning {
                            message: format!(
                                "{AUTOMATIC_COMPACTION_WARNING_MESSAGE}: {}",
                                self.redactor.redact_error(error)
                            ),
                        });
                    }
                    Ok(_) => {
                        if let Some(error) = stopped_provider_error(control) {
                            return Err(AgentError::Provider(error));
                        }
                        (request, estimate) =
                            self.build_normal_provider_request(state, system_context);
                        estimate_complete = context_estimate::estimate_complete(&request);
                        if estimate_complete && estimate > hard_trigger {
                            return Err(automatic_dispatch_error(
                                AUTOMATIC_COMPACTION_STILL_TOO_LARGE_MESSAGE,
                                Vec::new(),
                            ));
                        }
                    }
                }
            } else if estimate > hard_trigger {
                return Err(automatic_dispatch_error(
                    AUTOMATIC_COMPACTION_ATTEMPT_USED_MESSAGE,
                    Vec::new(),
                ));
            }
        }

        let (response, visible_output, error) = self
            .complete_normal_provider_attempt(&request, emit, control)
            .await;
        let Some(original_overflow) = error else {
            return Ok(response);
        };
        if !self.options.compaction.auto
            || visible_output
            || !is_typed_context_overflow(&original_overflow)
        {
            return Err(original_overflow);
        }

        if let Err(compaction_error) =
            Box::pin(self.compact_locked(CompactionReason::Overflow, "", emit, control)).await
        {
            if compaction_error.is_nothing_to_compact() {
                emit(Event::CompactionCompleted {
                    compaction: CompactionResult {
                        reason: CompactionReason::Overflow,
                        automatic: true,
                        noop: true,
                        ..CompactionResult::default()
                    },
                });
                return Err(original_overflow);
            }
            if let Some(stop) = stopped_provider_error(control) {
                return Err(AgentError::Provider(stop));
            }
            if let Some(cancellation) = automatic_cancellation(false, &compaction_error) {
                return Err(cancellation);
            }
            return Err(automatic_dispatch_error(
                OVERFLOW_COMPACTION_FAILURE_MESSAGE,
                vec![original_overflow.to_string(), compaction_error.to_string()],
            ));
        }
        if let Some(error) = stopped_provider_error(control) {
            return Err(AgentError::Provider(error));
        }

        let (retry_request, retry_estimate) =
            self.build_normal_provider_request(state, system_context);
        if let Some((_, hard_trigger)) = automatic_compaction_triggers(&self.options.compaction)
            && retry_estimate > hard_trigger
        {
            return Err(automatic_dispatch_error(
                AUTOMATIC_COMPACTION_STILL_TOO_LARGE_MESSAGE,
                vec![original_overflow.to_string()],
            ));
        }
        let (response, _, error) = self
            .complete_normal_provider_attempt(&retry_request, emit, control)
            .await;
        match error {
            Some(error) if is_typed_context_overflow(&error) => Err(automatic_dispatch_error(
                OVERFLOW_RETRY_FAILURE_MESSAGE,
                vec![original_overflow.to_string(), error.to_string()],
            )),
            Some(error) => Err(error),
            None => Ok(response),
        }
    }

    /// The capabilities a request sends: sorted, deduplicated tool names,
    /// their definition digests and the version.
    fn current_capabilities(&self) -> crate::session::Capabilities {
        let definitions = self.tools.definitions();
        let mut tools: Vec<String> = definitions.iter().map(|d| d.name.clone()).collect();
        tools.sort();
        tools.dedup();
        crate::session::Capabilities {
            tools,
            definitions: Some(
                definitions
                    .iter()
                    .map(|d| (d.name.clone(), capabilities::definition_digest(d)))
                    .collect(),
            ),
            version: (!self.options.version.is_empty()).then(|| self.options.version.clone()),
        }
    }

    /// Whether the last assistant message is old enough that the provider
    /// prompt cache has likely expired.
    fn last_response_idle(&self) -> bool {
        let last = self
            .session
            .messages()
            .iter()
            .rev()
            .find(|message| message.role == Role::Assistant)
            .map(|message| message.created_at);
        idle_cache_expired(last, (self.options.now)())
    }

    /// Builds the request for one ordinary provider step and estimates it.
    ///
    /// The messages are clones, so the overlay substitution and the memory
    /// message never reach the session.
    fn build_normal_provider_request(
        &self,
        state: &RunDispatchState,
        system_context: &str,
    ) -> (Request, i64) {
        let remote_compaction = matches!(
            self.provider.capabilities().remote_compaction,
            RemoteCompactionSupport::ResponsesV2
        )
        .then(|| self.session.remote_compaction())
        .flatten()
        .filter(|checkpoint| checkpoint.provider == self.options.provider_name);
        let mut messages = if remote_compaction.is_some() {
            self.session.post_remote_compaction_messages()
        } else {
            self.session.model_messages()
        };
        apply_tool_result_overlay(&mut messages, &state.tool_result_overlay);
        if !state.memory_context.is_empty() {
            let current_user = messages
                .iter()
                .rposition(|message| message.role == Role::User)
                .unwrap_or(messages.len());
            messages.insert(
                current_user,
                Message {
                    role: Role::User,
                    blocks: vec![Block::text(state.memory_context.clone())],
                    ..Message::default()
                },
            );
        }
        let request = Request {
            model: self.options.model.clone(),
            system_prompt: append_system_context(&self.options.system_prompt, system_context),
            thinking: self.options.thinking.clone(),
            messages,
            tools: self.tools.definitions(),
            cache_key: self.options.cache_key.clone(),
            remote_compaction,
            request_remote_compaction: false,
        };
        // A remote checkpoint supersedes any older local one, and the request
        // carries only its tail, so the local checkpoint's first message id
        // cannot anchor the estimate there.
        let latest = request
            .remote_compaction
            .is_none()
            .then(|| self.session.latest_compaction())
            .flatten();
        let estimate = context_estimate::estimate_request(&request, latest.as_ref());
        (request, estimate)
    }

    /// Runs one provider call, reporting whether any text reached the user.
    /// That flag decides whether an overflow may be retried: a partly
    /// delivered answer must not be replaced by a second one.
    async fn complete_normal_provider_attempt(
        &self,
        request: &Request,
        emit: EventSink<'_>,
        control: &dyn OperationControl,
    ) -> (Response, bool, Option<AgentError>) {
        let redactor = &self.redactor;
        let mut stream = redactor.new_stream();
        // Reasoning has its own redaction stream; it is flushed before the
        // first text delta so the two keep their provider order.
        let mut reasoning = redactor.new_stream();
        let visible_output = std::sync::atomic::AtomicBool::new(false);
        let operation_id = match (self.options.new_operation_id)() {
            Ok(operation_id) => operation_id,
            Err(message) => {
                return (
                    Response::default(),
                    false,
                    Some(AgentError::OperationIdentity { message }),
                );
            }
        };
        let started = (self.options.now)();
        let mut retries = Vec::new();
        let outcome = {
            let mut on_stream = |event: StreamEvent| match event {
                StreamEvent::ReasoningDelta { text: delta } => {
                    let text = reasoning.write(&delta);
                    if !text.is_empty() {
                        visible_output.store(true, std::sync::atomic::Ordering::SeqCst);
                        emit(Event::ReasoningDelta { text });
                    }
                }
                StreamEvent::TextDelta { text: delta } => {
                    let held = reasoning.flush();
                    if !held.is_empty() {
                        emit(Event::ReasoningDelta { text: held });
                    }
                    let text = stream.write(&delta);
                    if !text.is_empty() {
                        visible_output.store(true, std::sync::atomic::Ordering::SeqCst);
                        emit(Event::TextDelta { text });
                    }
                }
                StreamEvent::Retry {
                    attempt,
                    max_attempts,
                    delay,
                    reason,
                    interrupted_output,
                } => {
                    stream = redactor.new_stream();
                    reasoning = redactor.new_stream();
                    retries.push(serde_json::json!({
                        "attempt": attempt, "maxAttempts": max_attempts,
                        "delayMs": u64::try_from(delay.as_millis()).unwrap_or(u64::MAX),
                        "reason": redactor.redact_string(&reason),
                        "interruptedOutput": interrupted_output,
                    }));
                    emit(Event::ProviderRetry {
                        operation_id: operation_id.clone(),
                        attempt,
                        max_attempts,
                        delay,
                        reason,
                        interrupted_output,
                    });
                }
                StreamEvent::ToolCallDelta { .. } => {
                    visible_output.store(true, std::sync::atomic::Ordering::SeqCst);
                }
            };
            self.provider
                .complete(request, &mut on_stream, control)
                .await
        };
        let duration = ((self.options.now)() - started)
            .to_std()
            .unwrap_or_default();
        self.emit_provider_api_call(
            emit,
            operation_id.clone(),
            ProviderOperationKind::Ordinary,
            duration,
            &outcome,
        );
        // Persist only content-free recovery facts. Failed stream fragments
        // never become model context or executable tool calls.
        if !retries.is_empty() || outcome.result.is_err() {
            let record = serde_json::json!({
                "schemaVersion": 1, "operationId": operation_id,
                "attempts": outcome.attempts, "retries": retries,
                "outcome": outcome.outcome,
                "outputStarted": visible_output.load(std::sync::atomic::Ordering::SeqCst),
            });
            if let Err(source) = self
                .session
                .append_custom("otto.provider_recovery", &record.to_string())
                .await
            {
                return (
                    Response::default(),
                    true,
                    Some(AgentError::Persist {
                        kind: "provider recovery".into(),
                        source,
                    }),
                );
            }
        }
        match outcome.result {
            Err(error) => (
                Response::default(),
                visible_output.load(std::sync::atomic::Ordering::SeqCst),
                Some(AgentError::Provider(error)),
            ),
            Ok(response) => {
                let held = reasoning.flush();
                if !held.is_empty() {
                    emit(Event::ReasoningDelta { text: held });
                }
                let text = stream.flush();
                if !text.is_empty() {
                    visible_output.store(true, std::sync::atomic::Ordering::SeqCst);
                    emit(Event::TextDelta { text });
                }
                (
                    response,
                    visible_output.load(std::sync::atomic::Ordering::SeqCst),
                    None,
                )
            }
        }
    }

    pub(super) fn emit_provider_api_call(
        &self,
        emit: EventSink<'_>,
        operation_id: OperationId,
        kind: ProviderOperationKind,
        duration: std::time::Duration,
        settlement: &ProviderSettlement,
    ) {
        let status = match settlement.outcome.disposition {
            OperationDisposition::Succeeded => ApiStatus::Ok,
            OperationDisposition::Cancelled => ApiStatus::Canceled,
            OperationDisposition::Error
            | OperationDisposition::DeadlineExceeded
            | OperationDisposition::Interrupted => ApiStatus::Error,
        };
        emit(Event::ProviderApiCall {
            operation_id,
            kind,
            provider: self.options.provider_name.clone(),
            model: self.options.model.clone(),
            duration,
            attempts: settlement.attempts,
            status,
            outcome: settlement.outcome.clone(),
        });
    }

    /// The turn an agent runs when its redactor could not enumerate every
    /// secret: it starts, does nothing, and finishes, so no unredacted text
    /// can reach the provider or the transcript.
    fn run_with_incomplete_redactions(
        &self,
        emit: EventSink<'_>,
        control: &dyn OperationControl,
    ) -> Result<(), AgentError> {
        emit(Event::AgentStarted);
        if let Some(error) = stopped_provider_error(control) {
            return Err(self.fail(emit, AgentError::Provider(error)));
        }
        emit(Event::AgentFinished);
        Ok(())
    }

    /// Emits the terminal error event and returns the redacted error, so every
    /// failure path reports exactly once.
    pub(super) fn fail(&self, emit: EventSink<'_>, error: AgentError) -> AgentError {
        let error = self.redactor.redact_error(error);
        emit(Event::AgentError {
            message: error.to_string(),
        });
        error
    }

    /// Returns a copy of `message` with every text-bearing field redacted.
    pub(super) fn redact_message(&self, message: &Message) -> Message {
        let mut redacted = message.clone();
        redacted.id = self.redactor.redact_string(&redacted.id);
        for block in &mut redacted.blocks {
            block.text = self.redactor.redact_string(&block.text);
            block.tool_call_id = self.redactor.redact_string(&block.tool_call_id);
            block.tool_name = self.redactor.redact_string(&block.tool_name);
            if block.block_type == BlockType::ToolCall
                && let Some(arguments) = &block.arguments
            {
                block.arguments = Some(self.redactor.redact_json_strings(arguments.get()));
            }
        }
        redacted
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
mod tests {
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use chrono::{DateTime, Utc};
    use serde_json::value::RawValue;
    use tokio_util::sync::CancellationToken;

    use super::*;
    use crate::model::{Block, BlockType, FinishReason, Message, Role, ToolDefinition, Usage};
    use crate::operation::OperationControl;
    use crate::provider::{
        Provider, ProviderError, ProviderSettlement, Request, Response, StreamEvent, StreamSink,
    };
    use crate::session::{MemorySession, Session};
    use crate::tool::{ToolExecutor, ToolResult};

    struct StoppedControl {
        token: CancellationToken,
        reason: OperationStopReason,
    }

    impl OperationControl for StoppedControl {
        fn cancellation_token(&self) -> &CancellationToken {
            &self.token
        }

        fn remaining(&self) -> Option<std::time::Duration> {
            Some(std::time::Duration::ZERO)
        }

        fn stop_reason(&self) -> Option<OperationStopReason> {
            Some(self.reason)
        }
    }

    fn fixed_clock() -> DateTime<Utc> {
        DateTime::from_timestamp(1_700_000_000, 0).expect("in range")
    }

    fn raw(json: &str) -> Box<RawValue> {
        RawValue::from_string(json.to_owned()).expect("valid JSON")
    }

    fn test_options() -> Options {
        let counter = AtomicUsize::new(0);
        Options {
            model: "test-model".into(),
            provider_name: "fake".into(),
            system_prompt: "be brief".into(),
            thinking: String::new(),
            now: Box::new(fixed_clock),
            new_id: Box::new(move || format!("id-{}", counter.fetch_add(1, Ordering::SeqCst))),
            ..Options::default()
        }
    }

    /// Replays scripted turns and records the requests it was given.
    struct ScriptedProvider {
        turns: Mutex<std::collections::VecDeque<(Vec<StreamEvent>, Response)>>,
        requests: Mutex<Vec<Request>>,
    }

    impl ScriptedProvider {
        fn new(turns: Vec<(Vec<StreamEvent>, Response)>) -> Self {
            Self {
                turns: Mutex::new(turns.into()),
                requests: Mutex::new(Vec::new()),
            }
        }
    }

    #[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
    #[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
    impl Provider for ScriptedProvider {
        async fn complete(
            &self,
            request: &Request,
            emit: StreamSink<'_>,
            _control: &dyn OperationControl,
        ) -> ProviderSettlement {
            self.requests
                .lock()
                .expect("requests")
                .push(request.clone());
            let turn = self.turns.lock().expect("turns").pop_front();
            let Some((events, response)) = turn else {
                return ProviderSettlement::failed(
                    ProviderError::Other("no scripted turn left".into()),
                    0,
                    EffectCertainty::NotStarted,
                );
            };
            for event in events {
                emit(event);
            }
            ProviderSettlement::succeeded(response, 1)
        }
    }

    /// Waits for cancellation and then reports it.
    struct CancellingProvider;

    #[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
    #[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
    impl Provider for CancellingProvider {
        async fn complete(
            &self,
            _request: &Request,
            _emit: StreamSink<'_>,
            control: &dyn OperationControl,
        ) -> ProviderSettlement {
            control.cancellation_token().cancelled().await;
            ProviderSettlement::failed(ProviderError::Cancelled, 1, EffectCertainty::Unknown)
        }
    }

    /// Returns the call arguments as the result text.
    struct EchoExecutor;

    #[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
    #[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
    impl ToolExecutor for EchoExecutor {
        fn definitions(&self) -> Vec<ToolDefinition> {
            vec![ToolDefinition {
                name: "echo".into(),
                description: "returns its arguments".into(),
                parameters: Some(raw(r#"{"type":"object"}"#)),
            }]
        }

        async fn execute(
            &self,
            call: crate::tool::ToolCall<'_>,
            _control: &dyn OperationControl,
        ) -> crate::tool::ToolExecution {
            if call.name != "echo" {
                return crate::tool::ToolExecution {
                    result: ToolResult::unknown_tool(call.name),
                    outcome: crate::model::OperationOutcome {
                        disposition: crate::model::OperationDisposition::Error,
                        effect_certainty: crate::model::EffectCertainty::NotStarted,
                        stop_reason: None,
                    },
                };
            }
            crate::tool::ToolExecution::completed(ToolResult {
                content: call.arguments.get().to_owned(),
                persisted_content: None,
                is_error: false,
                outcome_override: None,
            })
        }
    }

    fn assistant_tool_call() -> Response {
        Response {
            message: Message {
                role: Role::Assistant,
                finish_reason: Some(FinishReason::ToolCalls),
                usage: Some(Usage {
                    input_tokens: 10,
                    output_tokens: 3,
                    cached_input_tokens: 0,
                }),
                blocks: vec![Block {
                    block_type: BlockType::ToolCall,
                    tool_call_id: "call-1".into(),
                    tool_name: "echo".into(),
                    arguments: Some(raw(r#"{"value":1}"#)),
                    ..Block::default()
                }],
                ..Message::default()
            },
            ..Response::default()
        }
    }

    fn assistant_text() -> Response {
        Response {
            message: Message {
                role: Role::Assistant,
                finish_reason: Some(FinishReason::Stop),
                blocks: vec![Block::text("done")],
                ..Message::default()
            },
            ..Response::default()
        }
    }

    /// An executor that serves `echo` and declares the names in `replayable`
    /// replayable, counting how many calls reached it.
    struct ReplayExecutor {
        replayable: &'static [&'static str],
        calls: AtomicUsize,
    }

    #[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
    #[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
    impl ToolExecutor for ReplayExecutor {
        fn definitions(&self) -> Vec<ToolDefinition> {
            EchoExecutor.definitions()
        }

        fn retry_safety(&self, name: &str) -> crate::model::RetrySafety {
            if self.replayable.contains(&name) {
                crate::model::RetrySafety::ReadOnly
            } else {
                crate::model::RetrySafety::NonIdempotent
            }
        }

        async fn execute(
            &self,
            call: crate::tool::ToolCall<'_>,
            control: &dyn OperationControl,
        ) -> crate::tool::ToolExecution {
            self.calls.fetch_add(1, Ordering::SeqCst);
            EchoExecutor.execute(call, control).await
        }
    }

    /// A session that ends on an assistant message with an unanswered `echo`
    /// call, the shape a takeover leaves for a replayable tool, and whose
    /// recorded first attempt (when `attempted`) was never settled.
    async fn session_with_pending_echo(attempted: bool) -> MemorySession {
        let session = MemorySession::new();
        let call = assistant_tool_call().message;
        session.append(call).await.expect("append call");
        if attempted {
            session
                .append_operation_fact(OperationFact::attempt(
                    OperationId::new("op-prior").expect("operation id"),
                    1,
                    "call-1",
                    "echo",
                ))
                .await
                .expect("attempt fact");
        }
        session
    }

    #[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
    async fn run_replays_a_replayable_call_a_takeover_left_unanswered() {
        let provider = ScriptedProvider::new(vec![(Vec::new(), assistant_text())]);
        let executor = ReplayExecutor {
            replayable: &["echo"],
            calls: AtomicUsize::new(0),
        };
        let agent = Agent::new(
            provider,
            executor,
            session_with_pending_echo(true).await,
            test_options(),
        );
        let mut events = Vec::new();
        agent
            .run(
                "continue",
                &mut |event| events.push(event),
                &CancellationToken::new(),
            )
            .await
            .expect("run");

        let started: Vec<_> = events
            .iter()
            .filter_map(|event| match event {
                Event::ToolCallStarted {
                    operation_id,
                    attempt,
                    ..
                } => Some((operation_id.as_str().to_owned(), *attempt)),
                _ => None,
            })
            .collect();
        assert_eq!(
            started,
            vec![("op-prior".to_owned(), 2)],
            "the call reruns as the next attempt of its recorded operation"
        );
        let roles: Vec<Role> = agent
            .session()
            .messages()
            .iter()
            .map(|message| message.role.clone())
            .collect();
        assert_eq!(
            roles,
            vec![
                Role::Assistant,
                Role::Tool,
                Role::Context,
                Role::User,
                Role::Assistant
            ],
            "the result follows its call; the legacy tool-set notice (history, no record) \
             precedes the user message"
        );
    }

    #[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
    async fn run_does_not_replay_a_call_whose_tool_is_not_replayable() {
        let provider = ScriptedProvider::new(vec![(Vec::new(), assistant_text())]);
        let executor = ReplayExecutor {
            replayable: &[],
            calls: AtomicUsize::new(0),
        };
        let agent = Agent::new(
            provider,
            executor,
            session_with_pending_echo(true).await,
            test_options(),
        );
        let result = agent
            .run("continue", &mut |_| {}, &CancellationToken::new())
            .await;
        assert!(
            result.is_err(),
            "a pending call the store should have answered is a loud error, not a silent run"
        );
        assert_eq!(agent.tools.calls.load(Ordering::SeqCst), 0);
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
    async fn run_executes_a_tool_turn_then_a_text_turn() {
        // op_1 is the turn id of the `otto.turn` records.
        let provider = ScriptedProvider::new(vec![
            (Vec::new(), assistant_tool_call()),
            (
                vec![StreamEvent::TextDelta {
                    text: "done".into(),
                }],
                assistant_text(),
            ),
        ]);
        let agent = Agent::new(provider, EchoExecutor, MemorySession::new(), test_options());
        let mut events = Vec::new();
        agent
            .run(
                "hello",
                &mut |event| events.push(event),
                &CancellationToken::new(),
            )
            .await
            .expect("run");

        let api_call = Event::ProviderApiCall {
            operation_id: OperationId::new("op_2").expect("operation id"),
            kind: ProviderOperationKind::Ordinary,
            provider: "fake".into(),
            model: "test-model".into(),
            duration: std::time::Duration::ZERO,
            attempts: 1,
            status: ApiStatus::Ok,
            outcome: OperationOutcome {
                disposition: OperationDisposition::Succeeded,
                effect_certainty: EffectCertainty::Completed,
                stop_reason: None,
            },
        };
        let second_api_call = Event::ProviderApiCall {
            operation_id: OperationId::new("op_4").expect("operation id"),
            kind: ProviderOperationKind::Ordinary,
            provider: "fake".into(),
            model: "test-model".into(),
            duration: std::time::Duration::ZERO,
            attempts: 1,
            status: ApiStatus::Ok,
            outcome: OperationOutcome {
                disposition: OperationDisposition::Succeeded,
                effect_certainty: EffectCertainty::Completed,
                stop_reason: None,
            },
        };
        assert_eq!(
            events,
            vec![
                Event::AgentStarted,
                api_call.clone(),
                Event::ProviderUsage {
                    usage: Usage {
                        input_tokens: 10,
                        output_tokens: 3,
                        cached_input_tokens: 0
                    },
                    present: true,
                    cached_input_tokens_present: false,
                },
                Event::ToolCallStarted {
                    operation_id: OperationId::new("op_3").expect("operation id"),
                    attempt: 1,
                    tool_name: "echo".into(),
                    tool_call_id: "call-1".into(),
                    arguments: r#"{"value":1}"#.into(),
                },
                Event::ToolCallFinished {
                    operation_id: OperationId::new("op_3").expect("operation id"),
                    attempt: 1,
                    tool_name: "echo".into(),
                    tool_call_id: "call-1".into(),
                    result: ToolResult {
                        content: r#"{"value":1}"#.into(),
                        persisted_content: None,
                        is_error: false,
                        outcome_override: None,
                    },
                    outcome: crate::model::OperationOutcome {
                        disposition: crate::model::OperationDisposition::Succeeded,
                        effect_certainty: crate::model::EffectCertainty::Completed,
                        stop_reason: None,
                    },
                },
                Event::TextDelta {
                    text: "done".into()
                },
                second_api_call,
                Event::ProviderUsage {
                    usage: Usage::default(),
                    present: false,
                    cached_input_tokens_present: false
                },
                Event::AgentFinished,
            ]
        );

        let messages = agent.session().messages();
        let roles: Vec<Role> = messages
            .iter()
            .map(|message| message.role.clone())
            .collect();
        assert_eq!(
            roles,
            vec![Role::User, Role::Assistant, Role::Tool, Role::Assistant]
        );
        assert_eq!(messages[0].text(), "hello");
        assert_eq!(messages[2].blocks[0].text, r#"{"value":1}"#);
        assert_eq!(messages[3].text(), "done");
        for message in &messages {
            assert!(!message.id.is_empty(), "message id was not filled in");
            assert_eq!(message.created_at, fixed_clock());
        }
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
    async fn run_persists_the_override_text_and_reports_the_live_text() {
        struct OverridingExecutor;

        #[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
        #[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
        impl ToolExecutor for OverridingExecutor {
            fn definitions(&self) -> Vec<ToolDefinition> {
                Vec::new()
            }
            async fn execute(
                &self,
                _call: crate::tool::ToolCall<'_>,
                _control: &dyn OperationControl,
            ) -> crate::tool::ToolExecution {
                crate::tool::ToolExecution::completed(ToolResult {
                    content: "live".into(),
                    persisted_content: Some("stored".into()),
                    is_error: false,
                    outcome_override: None,
                })
            }
        }

        let provider = ScriptedProvider::new(vec![
            (Vec::new(), assistant_tool_call()),
            (Vec::new(), assistant_text()),
        ]);
        let agent = Agent::new(
            provider,
            OverridingExecutor,
            MemorySession::new(),
            test_options(),
        );
        let mut live = String::new();
        agent
            .run(
                "hello",
                &mut |event| {
                    if let Event::ToolCallFinished { result, .. } = event {
                        live = result.content;
                    }
                },
                &CancellationToken::new(),
            )
            .await
            .expect("run");
        assert_eq!(live, "live");
        assert_eq!(agent.session().messages()[2].blocks[0].text, "stored");
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
    async fn run_rejects_empty_user_text() {
        let agent = Agent::new(
            ScriptedProvider::new(Vec::new()),
            EchoExecutor,
            MemorySession::new(),
            test_options(),
        );
        let mut events = Vec::new();
        let error = agent
            .run(
                " \t\n\r ",
                &mut |event| events.push(event),
                &CancellationToken::new(),
            )
            .await
            .expect_err("empty text accepted");
        assert!(matches!(error, AgentError::EmptyUserText));
        assert_eq!(
            events,
            vec![Event::AgentError {
                message: "user text is required".into()
            }]
        );
        assert!(agent.session().messages().is_empty());
    }

    #[test]
    fn deadline_tool_stop_is_typed_and_not_started() {
        let execution = stopped_tool_execution(OperationStopReason::Deadline);
        assert_eq!(
            execution.outcome,
            OperationOutcome {
                disposition: OperationDisposition::DeadlineExceeded,
                effect_certainty: EffectCertainty::NotStarted,
                stop_reason: Some(OperationStopReason::Deadline),
            }
        );
        assert_eq!(
            execution.result.content,
            ProviderError::DeadlineExceeded.to_string()
        );
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
    async fn deadline_before_provider_dispatch_is_a_typed_error() {
        let provider = ScriptedProvider::new(Vec::new());
        let agent = Agent::new(
            provider,
            EchoExecutor,
            MemorySession::default(),
            test_options(),
        );
        let control = StoppedControl {
            token: CancellationToken::new(),
            reason: OperationStopReason::Deadline,
        };
        let error = agent
            .run_with_control("hello", &mut |_| {}, &control)
            .await
            .expect_err("deadline must stop dispatch");
        assert!(matches!(
            error,
            AgentError::Provider(ProviderError::DeadlineExceeded)
        ));
        assert!(
            agent
                .provider()
                .requests
                .lock()
                .expect("requests")
                .is_empty()
        );
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
    async fn run_reports_cancellation_as_an_agent_error() {
        let agent = Agent::new(
            CancellingProvider,
            EchoExecutor,
            MemorySession::new(),
            test_options(),
        );
        let cancel = CancellationToken::new();
        cancel.cancel();
        let mut events = Vec::new();
        let error = agent
            .run("hello", &mut |event| events.push(event), &cancel)
            .await
            .expect_err("cancellation was not reported");
        assert!(matches!(
            error,
            AgentError::Provider(ProviderError::Cancelled)
        ));
        assert_eq!(
            events,
            vec![
                Event::AgentStarted,
                Event::ProviderApiCall {
                    operation_id: OperationId::new("op_2").expect("operation id"),
                    kind: ProviderOperationKind::Ordinary,
                    provider: "fake".into(),
                    model: "test-model".into(),
                    duration: std::time::Duration::ZERO,
                    attempts: 0,
                    status: ApiStatus::Canceled,
                    outcome: OperationOutcome {
                        disposition: OperationDisposition::Cancelled,
                        effect_certainty: EffectCertainty::NotStarted,
                        stop_reason: Some(OperationStopReason::UserCancellation),
                    },
                },
                Event::AgentError {
                    message: "provider call was cancelled".into()
                },
            ]
        );
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
    async fn run_rejects_a_non_assistant_provider_response() {
        let provider = ScriptedProvider::new(vec![(
            Vec::new(),
            Response {
                message: Message {
                    role: Role::User,
                    blocks: vec![Block::text("wrong")],
                    ..Message::default()
                },
                ..Response::default()
            },
        )]);
        let agent = Agent::new(provider, EchoExecutor, MemorySession::new(), test_options());
        let error = agent
            .run("hello", &mut |_| {}, &CancellationToken::new())
            .await
            .expect_err("non-assistant response accepted");
        assert_eq!(
            error.to_string(),
            "invalid provider response: assistant role is required"
        );
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
    async fn run_sends_the_session_transcript_and_tool_definitions() {
        let provider = ScriptedProvider::new(vec![(Vec::new(), assistant_text())]);
        let agent = Agent::new(provider, EchoExecutor, MemorySession::new(), test_options());
        agent
            .run("hello", &mut |_| {}, &CancellationToken::new())
            .await
            .expect("run");
        let requests = agent.provider().requests.lock().expect("requests").clone();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].model, "test-model");
        assert_eq!(requests[0].system_prompt, "be brief");
        assert_eq!(requests[0].messages.len(), 1);
        assert_eq!(requests[0].messages[0].text(), "hello");
        assert_eq!(requests[0].tools, EchoExecutor.definitions());
    }
    #[derive(Default)]
    struct TestHooks {
        events: Mutex<Vec<(String, serde_json::Value)>>,
        deny: bool,
        after_fail: bool,
    }
    #[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
    #[cfg_attr(target_arch="wasm32",async_trait::async_trait(?Send))]
    impl hooks::Hooks for TestHooks {
        async fn run(
            &self,
            event: &str,
            data: serde_json::Value,
            _: &dyn OperationControl,
        ) -> Result<hooks::Response, String> {
            self.events.lock().unwrap().push((event.into(), data));
            match event {
                "before_prompt" => Ok(hooks::Response {
                    context: Some("plugin context".into()),
                    ..Default::default()
                }),
                "before_tool" if self.deny => Ok(hooks::Response {
                    decision: Some(hooks::Decision::Deny),
                    reason: Some("denied".into()),
                    ..Default::default()
                }),
                "before_tool" => Ok(hooks::Response {
                    arguments: Some(serde_json::json!({"value":2})),
                    ..Default::default()
                }),
                "after_tool" if self.after_fail => Err("failed after hook".into()),
                "after_tool" => Ok(hooks::Response {
                    content: Some("filtered".into()),
                    ..Default::default()
                }),
                _ => Ok(Default::default()),
            }
        }
    }
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
    async fn hooks_rewrite_before_execution_preserve_effects_and_share_session_lifecycle() {
        let hooks = Arc::new(TestHooks::default());
        let mut options = test_options();
        options.hooks = Some(hooks.clone());
        let agent = Agent::new(
            ScriptedProvider::new(vec![
                (Vec::new(), assistant_tool_call()),
                (Vec::new(), assistant_text()),
                (Vec::new(), assistant_text()),
            ]),
            EchoExecutor,
            MemorySession::new(),
            options,
        );
        let mut events = Vec::new();
        agent
            .run("first", &mut |e| events.push(e), &CancellationToken::new())
            .await
            .unwrap();
        agent
            .run("second", &mut |e| events.push(e), &CancellationToken::new())
            .await
            .unwrap();
        agent.end_hooks().await;
        agent.end_hooks().await;
        let recorded = hooks.events.lock().unwrap();
        assert_eq!(
            recorded.iter().map(|(e, _)| e.as_str()).collect::<Vec<_>>(),
            vec![
                "session_start",
                "before_prompt",
                "before_tool",
                "after_tool",
                "turn_end",
                "before_prompt",
                "turn_end",
                "session_end"
            ]
        );
        assert_eq!(
            recorded.iter().find(|(e, _)| e == "after_tool").unwrap().1["content"],
            r#"{"value":2}"#
        );
        assert!(
            !recorded
                .iter()
                .find(|(e, _)| e == "before_prompt")
                .unwrap()
                .1["turn_id"]
                .as_str()
                .unwrap()
                .is_empty()
        );
        assert!(
            agent
                .provider
                .requests
                .lock()
                .unwrap()
                .iter()
                .all(|r| r.system_prompt.contains("Untrusted plugin context"))
        );
        assert!(events.iter().any(|e|matches!(e,Event::ToolCallFinished{result,outcome,..} if result.content=="filtered" && outcome.effect_certainty==EffectCertainty::Completed)));
        assert!(
            agent
                .session
                .messages()
                .iter()
                .any(|m| m.role == Role::Tool && m.blocks.iter().any(|b| b.text == "filtered"))
        );
    }
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
    async fn hook_denial_is_not_started_and_after_failure_keeps_the_tool_result() {
        for deny in [true, false] {
            let hooks = Arc::new(TestHooks {
                deny,
                after_fail: true,
                ..Default::default()
            });
            let mut options = test_options();
            options.hooks = Some(hooks.clone());
            let agent = Agent::new(
                ScriptedProvider::new(vec![
                    (Vec::new(), assistant_tool_call()),
                    (Vec::new(), assistant_text()),
                ]),
                EchoExecutor,
                MemorySession::new(),
                options,
            );
            let mut events = Vec::new();
            agent
                .run("run", &mut |e| events.push(e), &CancellationToken::new())
                .await
                .unwrap();
            let terminal = events
                .iter()
                .find_map(|e| {
                    if let Event::ToolCallFinished {
                        result, outcome, ..
                    } = e
                    {
                        Some((result, outcome))
                    } else {
                        None
                    }
                })
                .unwrap();
            if deny {
                assert_eq!(terminal.1.effect_certainty, EffectCertainty::NotStarted);
                assert_eq!(terminal.0.content, "denied");
                assert!(
                    !hooks
                        .events
                        .lock()
                        .unwrap()
                        .iter()
                        .any(|(e, _)| e == "after_tool")
                );
            } else {
                assert_eq!(terminal.1.effect_certainty, EffectCertainty::Completed);
                assert_eq!(terminal.0.content, r#"{"value":2}"#);
                assert!(!terminal.0.is_error);
                assert!(events.iter().any(|e|matches!(e,Event::Notification{text,..} if text.contains("failed after hook"))));
            }
        }
    }
}
