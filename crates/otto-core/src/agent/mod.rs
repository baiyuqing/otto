//! The provider/tool turn loop.
//!
//! Port of `Agent.Run` in `internal/agent/agent.go`: one user message, then
//! provider calls alternating with tool calls until the model stops asking
//! for tools, wrapped in proactive and overflow-triggered compaction, memory
//! recall, inbox notifications, the secret redactor, and the tool-result
//! overlay.
//!
//! Ownership: the agent owns its provider, tool executor, and session. The
//! caller owns the event sink and the cancellation token.
//!
//! Concurrency and cancellation: `run` takes `&self` but a single agent is
//! meant to serve one turn at a time. The Go implementation serializes `Run`
//! and `Compact` with a mutex; `otto-core` has no async mutex on the wasm
//! target, so the caller must not overlap the two. Cancelling the token stops
//! the provider call, skips the remaining tool calls, and ends the run with an
//! error.
//!
//! Errors: every failure path emits [`Event::AgentError`] and returns the same
//! error, so a frontend that only watches events sees every failure.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use serde_json::value::RawValue;
use tokio_util::sync::CancellationToken;

pub mod compaction;
pub mod compaction_select;
pub mod context_estimate;
pub mod events;
pub mod inbox;
pub mod memory;
pub mod overflow;
pub mod redactor;
pub mod summary;
pub mod summary_details;
pub mod summary_validate;
pub mod tasks;

#[cfg(test)]
mod run_tests;

pub use events::{
    AgentError, ApiStatus, CompactionMode, CompactionPlan, CompactionReason, CompactionResult,
    CompactionSettings, Event, EventSink,
};

use crate::model::{Block, BlockType, ContextMetadata, Message, Role, ToolDefinition, zero_time};
use crate::provider::{Provider, ProviderError, Request, RequestSizer, Response, StreamEvent};
use crate::session::Session;
use crate::tool::{ToolExecutor, ToolResult};

use inbox::Inbox;
use memory::{DEFAULT_RECALL_LIMIT, DEFAULT_RECALL_TOKEN_BUDGET, MemoryRecall};
use overflow::{
    AUTOMATIC_COMPACTION_ATTEMPT_USED_MESSAGE, AUTOMATIC_COMPACTION_HARD_FAILURE_MESSAGE,
    AUTOMATIC_COMPACTION_STILL_TOO_LARGE_MESSAGE, AUTOMATIC_COMPACTION_WARNING_MESSAGE,
    OVERFLOW_COMPACTION_FAILURE_MESSAGE, OVERFLOW_RETRY_FAILURE_MESSAGE, RunDispatchState,
    automatic_cancellation, automatic_compaction_triggers, automatic_dispatch_error,
    is_typed_context_overflow,
};
use redactor::Redactor;
use tasks::TaskRegistry;

/// Turn settings and the two injected capabilities the core cannot provide
/// itself: the clock and the identifier generator. Keeping them here is what
/// lets `otto-core` build for `wasm32-unknown-unknown`.
pub struct Options {
    pub model: String,
    pub provider_name: String,
    pub system_prompt: String,
    pub thinking: String,
    /// Returns the current time. Called once per persisted message and twice
    /// per provider call, to measure its duration.
    pub now: Box<dyn Fn() -> DateTime<Utc> + Send + Sync>,
    /// Returns a fresh message identifier. Must not repeat within a session.
    pub new_id: Box<dyn Fn() -> String + Send + Sync>,
    /// Measures the serialized size of a request. Compaction needs it to
    /// bound the summary request; without it, compaction cannot run.
    pub request_sizer: Option<Arc<dyn RequestSizer + Send + Sync>>,
    /// The context windows and reserves that drive automatic compaction.
    pub compaction: CompactionSettings,
    /// The long-term memory binding consulted once per turn, if any.
    pub memory: Option<Arc<dyn MemoryRecall + Send + Sync>>,
    /// How many records one recall may return.
    pub memory_recall_limit: i64,
    /// How many tokens one recall may spend.
    pub memory_recall_token_budget: i64,
    /// Notifications waiting to be delivered as context messages. Shared with
    /// whoever pushes into it, which is why it is behind an `Arc`.
    pub inbox: Arc<Inbox>,
    /// The subagent task registry, closed by [`Agent::close`].
    pub tasks: Option<Arc<dyn TaskRegistry + Send + Sync>>,
}

/// The default clock returns the zero time and the default identifier
/// generator returns the empty string: `otto-core` has no clock and no
/// randomness of its own, so a real caller must replace both.
impl Default for Options {
    fn default() -> Self {
        Self {
            model: String::new(),
            provider_name: String::new(),
            system_prompt: String::new(),
            thinking: String::new(),
            now: Box::new(zero_time),
            new_id: Box::new(String::new),
            request_sizer: None,
            compaction: CompactionSettings::default(),
            memory: None,
            memory_recall_limit: DEFAULT_RECALL_LIMIT,
            memory_recall_token_budget: DEFAULT_RECALL_TOKEN_BUDGET,
            inbox: Arc::new(Inbox::default()),
            tasks: None,
        }
    }
}

/// Runs provider and tool turns against one session.
///
/// The caller must serialize calls: `run` and `compact` both mutate the
/// session, and neither takes a lock. The Go implementation guards them with
/// one mutex because its agent is shared across goroutines.
pub struct Agent<P, T, S> {
    provider: P,
    tools: T,
    pub(super) session: S,
    pub(super) options: Options,
    pub(super) redactor: Redactor,
}

impl<P: Provider, T: ToolExecutor, S: Session> Agent<P, T, S> {
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

    /// The task registry, if one was configured.
    pub fn tasks(&self) -> Option<&Arc<dyn TaskRegistry + Send + Sync>> {
        self.options.tasks.as_ref()
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
        self.run_with_image(user_text, None, emit, cancel).await
    }

    pub async fn run_with_image(
        &self,
        user_text: &str,
        image: Option<Block>,
        emit: EventSink<'_>,
        cancel: &CancellationToken,
    ) -> Result<(), AgentError> {
        if !self.redactor.allows_dynamic_content() {
            return self.run_with_incomplete_redactions(emit, cancel);
        }
        let text = trim_go_space(user_text);
        if text.is_empty() && image.is_none() && self.options.inbox.is_empty() {
            return Err(self.fail(emit, AgentError::EmptyUserText));
        }
        emit(Event::AgentStarted);

        let mut state = RunDispatchState::default();
        if !text.is_empty() || image.is_some() {
            let redacted = self.redactor.redact_string(user_text);
            let mut blocks = Vec::new();
            if !text.is_empty() {
                blocks.push(Block::text(redacted.clone()));
            }
            blocks.extend(image);
            let user = Message {
                id: (self.options.new_id)(),
                role: Role::User,
                created_at: (self.options.now)(),
                blocks,
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
                match binding.recall(&request, cancel).await {
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
        }
        if let Err(error) = self.deliver_notifications(emit).await {
            return Err(self.fail(emit, error));
        }

        loop {
            let response = match self
                .dispatch_normal_provider_step(emit, &mut state, cancel)
                .await
            {
                Ok(response) => response,
                Err(error) => return Err(self.fail(emit, error)),
            };

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
            });

            let had_tool_call = !tool_calls.is_empty();
            // Tool calls run one at a time, so a frontend can reserve bounded
            // delivery for the terminal event of the single active call.
            for block in tool_calls {
                let arguments = block.arguments.clone().unwrap_or_else(empty_object);
                emit(Event::ToolCallStarted {
                    tool_name: block.tool_name.clone(),
                    tool_call_id: block.tool_call_id.clone(),
                    arguments: arguments.get().to_owned(),
                });
                let mut result = if cancel.is_cancelled() {
                    ToolResult::error(ProviderError::Cancelled.to_string())
                } else {
                    self.tools
                        .execute(&block.tool_name, &arguments, cancel)
                        .await
                };
                result.content = self.redactor.redact_string(&result.content);
                result.persisted_content = result
                    .persisted_content
                    .as_deref()
                    .map(|text| self.redactor.redact_string(text));
                let persisted_text = match &result.persisted_content {
                    Some(persisted) => {
                        // The stored text is a placeholder, so the live text
                        // is kept for this turn's provider requests only.
                        state
                            .tool_result_overlay
                            .insert(block.tool_call_id.clone(), result.content.clone());
                        persisted.clone()
                    }
                    None => result.content.clone(),
                };
                let is_error = result.is_error;
                emit(Event::ToolCallFinished {
                    tool_name: block.tool_name.clone(),
                    tool_call_id: block.tool_call_id.clone(),
                    result,
                });
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
                        ..Block::default()
                    }],
                    ..Message::default()
                };
                // The Go loop persists tool results on a non-cancellable
                // context. `Session::append` is likewise not cancellable.
                if let Err(source) = self.session.append(stored).await {
                    return Err(self.fail(
                        emit,
                        AgentError::Persist {
                            kind: format!("tool result for {}", quote_go(&block.tool_call_id)),
                            source,
                        },
                    ));
                }
            }

            if cancel.is_cancelled() {
                return Err(self.fail(emit, AgentError::Provider(ProviderError::Cancelled)));
            }
            if !had_tool_call {
                emit(Event::AgentFinished);
                return Ok(());
            }
            if let Err(error) = self.deliver_notifications(emit).await {
                return Err(self.fail(emit, error));
            }
        }
    }

    /// Drains the inbox, appending each notification as a display context
    /// message and reporting it. A no-op when the inbox is empty.
    async fn deliver_notifications(&self, emit: EventSink<'_>) -> Result<(), AgentError> {
        for notification in self.options.inbox.drain() {
            let text = self.redactor.redact_string(&notification.text);
            let metadata = ContextMetadata {
                task_id: notification.task_id.clone(),
            };
            let context_metadata = metadata.validate().is_ok().then_some(metadata);
            let message = Message {
                id: (self.options.new_id)(),
                role: Role::Context,
                context_type: notification.context_type().to_owned(),
                display: true,
                created_at: (self.options.now)(),
                usage: notification.usage,
                context_metadata,
                blocks: vec![Block::text(text.clone())],
                ..Message::default()
            };
            self.session
                .append(message)
                .await
                .map_err(|source| AgentError::Persist {
                    kind: "notification".into(),
                    source,
                })?;
            emit(Event::Notification {
                task_id: notification.task_id,
                text,
                usage: notification.usage.unwrap_or_default(),
                present: notification.usage.is_some(),
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
        cancel: &CancellationToken,
    ) -> Result<Response, AgentError> {
        let (mut request, mut estimate) = self.build_normal_provider_request(state);
        let triggers = automatic_compaction_triggers(&self.options.compaction);

        if let Some((soft_trigger, hard_trigger)) = triggers
            && self.options.compaction.auto
            && estimate > soft_trigger
        {
            if !state.proactive_attempted {
                state.proactive_attempted = true;
                match self
                    .compact_locked(CompactionReason::Threshold, "", emit, cancel)
                    .await
                {
                    Err(error) if error.is_nothing_to_compact() => {
                        emit(Event::CompactionCompleted {
                            compaction: CompactionResult {
                                reason: CompactionReason::Threshold,
                                automatic: true,
                                noop: true,
                                ..CompactionResult::default()
                            },
                        });
                        state.proactive_attempted = false;
                    }
                    Err(error) => {
                        if let Some(cancellation) =
                            automatic_cancellation(cancel.is_cancelled(), &error)
                        {
                            return Err(cancellation);
                        }
                        if estimate >= hard_trigger {
                            return Err(automatic_dispatch_error(
                                AUTOMATIC_COMPACTION_HARD_FAILURE_MESSAGE,
                                vec![error.to_string()],
                            ));
                        }
                        emit(Event::CompactionWarning {
                            message: AUTOMATIC_COMPACTION_WARNING_MESSAGE.into(),
                        });
                    }
                    Ok(_) => {
                        if cancel.is_cancelled() {
                            return Err(AgentError::Provider(ProviderError::Cancelled));
                        }
                        (request, estimate) = self.build_normal_provider_request(state);
                        if estimate > hard_trigger {
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

        let (response, visible_text, error) = self
            .complete_normal_provider_attempt(&request, emit, cancel)
            .await;
        let Some(original_overflow) = error else {
            return Ok(response);
        };
        if !self.options.compaction.auto
            || visible_text
            || !is_typed_context_overflow(&original_overflow)
        {
            return Err(original_overflow);
        }

        if let Err(compaction_error) = self
            .compact_locked(CompactionReason::Overflow, "", emit, cancel)
            .await
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
            if let Some(cancellation) =
                automatic_cancellation(cancel.is_cancelled(), &compaction_error)
            {
                return Err(cancellation);
            }
            return Err(automatic_dispatch_error(
                OVERFLOW_COMPACTION_FAILURE_MESSAGE,
                vec![original_overflow.to_string(), compaction_error.to_string()],
            ));
        }
        if cancel.is_cancelled() {
            return Err(AgentError::Provider(ProviderError::Cancelled));
        }

        let (retry_request, retry_estimate) = self.build_normal_provider_request(state);
        if let Some((_, hard_trigger)) = automatic_compaction_triggers(&self.options.compaction)
            && retry_estimate > hard_trigger
        {
            return Err(automatic_dispatch_error(
                AUTOMATIC_COMPACTION_STILL_TOO_LARGE_MESSAGE,
                vec![original_overflow.to_string()],
            ));
        }
        let (response, _, error) = self
            .complete_normal_provider_attempt(&retry_request, emit, cancel)
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

    /// Builds the request for one ordinary provider step and estimates it.
    ///
    /// The messages are clones, so the overlay substitution and the memory
    /// message never reach the session.
    fn build_normal_provider_request(&self, state: &RunDispatchState) -> (Request, i64) {
        let mut messages = self.session.messages();
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
            system_prompt: self.options.system_prompt.clone(),
            thinking: self.options.thinking.clone(),
            messages,
            tools: self.tools.definitions(),
        };
        let latest = self.session.latest_compaction();
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
        cancel: &CancellationToken,
    ) -> (Response, bool, Option<AgentError>) {
        let mut stream = self.redactor.new_stream();
        let visible_text = std::sync::atomic::AtomicBool::new(false);
        let started = (self.options.now)();
        let outcome = {
            let mut on_stream = |event: StreamEvent| {
                let StreamEvent::TextDelta { text: delta } = event else {
                    return;
                };
                let text = stream.write(&delta);
                if !text.is_empty() {
                    visible_text.store(true, std::sync::atomic::Ordering::SeqCst);
                    emit(Event::TextDelta { text });
                }
            };
            self.provider
                .complete(request, &mut on_stream, cancel)
                .await
        };
        let duration = ((self.options.now)() - started)
            .to_std()
            .unwrap_or_default();
        self.emit_provider_api_call(emit, duration, outcome.as_ref().err(), cancel);
        match outcome {
            Err(error) => (
                Response::default(),
                visible_text.load(std::sync::atomic::Ordering::SeqCst),
                Some(AgentError::Provider(error)),
            ),
            Ok(response) => {
                let text = stream.flush();
                if !text.is_empty() {
                    visible_text.store(true, std::sync::atomic::Ordering::SeqCst);
                    emit(Event::TextDelta { text });
                }
                (
                    response,
                    visible_text.load(std::sync::atomic::Ordering::SeqCst),
                    None,
                )
            }
        }
    }

    pub(super) fn emit_provider_api_call(
        &self,
        emit: EventSink<'_>,
        duration: std::time::Duration,
        error: Option<&ProviderError>,
        cancel: &CancellationToken,
    ) {
        let status = match error {
            _ if cancel.is_cancelled() => ApiStatus::Canceled,
            Some(ProviderError::Cancelled) => ApiStatus::Canceled,
            None => ApiStatus::Ok,
            Some(_) => ApiStatus::Error,
        };
        emit(Event::ProviderApiCall {
            provider: self.options.provider_name.clone(),
            model: self.options.model.clone(),
            duration,
            status,
        });
    }

    /// The turn an agent runs when its redactor could not enumerate every
    /// secret: it starts, does nothing, and finishes, so no unredacted text
    /// can reach the provider or the transcript.
    fn run_with_incomplete_redactions(
        &self,
        emit: EventSink<'_>,
        cancel: &CancellationToken,
    ) -> Result<(), AgentError> {
        emit(Event::AgentStarted);
        if cancel.is_cancelled() {
            return Err(self.fail(emit, AgentError::Provider(ProviderError::Cancelled)));
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
    // Go walks the definitions with reflection over every string field. The
    // serialized form contains exactly those strings, so redacting it is the
    // same check.
    let serialized = serde_json::to_string(definitions).unwrap_or_default();
    redactor.redact_string(&serialized) == serialized
}

/// Go's `%q` for the tool-call id in a persist error.
fn quote_go(value: &str) -> String {
    serde_json::to_string(value).expect("a string always encodes")
}

/// Trims exactly the bytes Go's agent trims: space, tab, newline, carriage
/// return.
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
    use crate::provider::{Provider, ProviderError, Request, Response, StreamEvent, StreamSink};
    use crate::session::{MemorySession, Session};
    use crate::tool::{ToolExecutor, ToolResult};

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
            _cancel: &CancellationToken,
        ) -> Result<Response, ProviderError> {
            self.requests
                .lock()
                .expect("requests")
                .push(request.clone());
            let turn = self.turns.lock().expect("turns").pop_front();
            let Some((events, response)) = turn else {
                return Err(ProviderError::Other("no scripted turn left".into()));
            };
            for event in events {
                emit(event);
            }
            Ok(response)
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
            cancel: &CancellationToken,
        ) -> Result<Response, ProviderError> {
            cancel.cancelled().await;
            Err(ProviderError::Cancelled)
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
            name: &str,
            arguments: &RawValue,
            _cancel: &CancellationToken,
        ) -> ToolResult {
            if name != "echo" {
                return ToolResult::unknown_tool(name);
            }
            ToolResult {
                content: arguments.get().to_owned(),
                persisted_content: None,
                is_error: false,
            }
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
        }
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
    async fn run_executes_a_tool_turn_then_a_text_turn() {
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
            provider: "fake".into(),
            model: "test-model".into(),
            duration: std::time::Duration::ZERO,
            status: ApiStatus::Ok,
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
                },
                Event::ToolCallStarted {
                    tool_name: "echo".into(),
                    tool_call_id: "call-1".into(),
                    arguments: r#"{"value":1}"#.into(),
                },
                Event::ToolCallFinished {
                    tool_name: "echo".into(),
                    tool_call_id: "call-1".into(),
                    result: ToolResult {
                        content: r#"{"value":1}"#.into(),
                        persisted_content: None,
                        is_error: false,
                    },
                },
                Event::TextDelta {
                    text: "done".into()
                },
                api_call,
                Event::ProviderUsage {
                    usage: Usage::default(),
                    present: false
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
                _name: &str,
                _arguments: &RawValue,
                _cancel: &CancellationToken,
            ) -> ToolResult {
                ToolResult {
                    content: "live".into(),
                    persisted_content: Some("stored".into()),
                    is_error: false,
                }
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
                    provider: "fake".into(),
                    model: "test-model".into(),
                    duration: std::time::Duration::ZERO,
                    status: ApiStatus::Canceled,
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
}
