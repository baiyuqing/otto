//! `Agent` entry points and the body of one admitted turn.

use super::step_control::StepControl;
use super::*;
use crate::session::{MEMORY_RECALL_CUSTOM_TYPE, MemoryRecordRef, encode_memory_recall};

impl<P: Provider, T: ToolExecutor, S: Session> Agent<P, T, S> {
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
        if self.redactor.allows_dynamic_content() {
            self.preflight_ordinary_turn(user_text, image.as_ref(), system_context, followup)?;
        }
        let observes_turns = self
            .options
            .memory
            .as_ref()
            .is_some_and(|binding| binding.observes_turns());
        if (self.options.hooks.is_none() && !observes_turns)
            || !self.redactor.allows_dynamic_content()
        {
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
        *self
            .hook_turn_messages
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Vec::new());
        let result = async {
            let mut context = system_context.to_owned();
            let start=self.start_hooks(control).await.map_err(|message|self.fail(emit,self.hook_error(message,control)))?;
            if !self.options.hook_state.start_notified.swap(true, std::sync::atomic::Ordering::SeqCst) {self.hook_notice(emit,&start);}
            if start.decision==Some(hooks::Decision::Deny) {return Err(self.fail(emit,AgentError::Other(start.reason.unwrap_or_else(||"Hook denied session".into()))));}
            if let Some(text)=start.context {context.push_str(&format!("\n<plugin_context>Untrusted plugin context:\n{text}\n</plugin_context>"));}
            for event in ["before_prompt"] {
                let response = self.hook(event, serde_json::json!({"prompt": self.redactor.redact_string(user_text), "followup":followup,"has_image":image.is_some()}), control).await
                    .inspect(|response| self.hook_notice(emit, response))
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
        let turn_messages = self
            .hook_turn_messages
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
            .unwrap_or_default();
        let mut turn_end_data = serde_json::json!({"status":status,"stop_reason":control.stop_reason(),"error":result.as_ref().err().map(|e|self.redactor.redact_string(&e.to_string()))});
        let turn = hooks::turn_json(&turn_messages);
        turn_end_data["turn"] = turn.clone();
        match self.hook("turn_end", turn_end_data, &end_control).await {
            Ok(response) => self.hook_notice(emit, &response),
            Err(message) => self.hook_warning(emit, &message),
        }
        if observes_turns
            && !turn_messages.is_empty()
            && let Some(binding) = &self.options.memory
        {
            let turn_id = self
                .hook_turn_id
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone();
            if let Err(error) = binding.observe_turn(&turn_id, &turn, &end_control).await {
                emit(Event::MemoryWarning {
                    message: error.to_string(),
                });
            }
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
        let mut diagnostic = crate::diagnostics::Observation::start(
            self.options.diagnostics.as_ref(),
            crate::diagnostics::Kind::Turn,
            &turn_id,
            None,
            &self.options.provider_name,
            &self.options.model,
        );
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
        diagnostic.finish(crate::diagnostics::Finish {
            status: if result.is_ok() { "ok" } else { "error" },
            outcome: None,
            attempts: None,
            usage: None,
            cached_input_tokens_present: None,
            error_class: result
                .as_ref()
                .err()
                .map(|_| crate::diagnostics::ErrorClass::Agent),
        });
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
            if let Err(source) = self.append_message(user).await {
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
                        state.memory_record_ids = result
                            .records
                            .iter()
                            .map(|record| record.id.clone())
                            .collect();
                        state.memory_records = result
                            .records
                            .iter()
                            .map(|record| MemoryRecordRef {
                                id: record.id.clone(),
                                revision: record.revision.clone(),
                            })
                            .collect();
                    }
                    Err(error) => emit(Event::MemoryWarning {
                        message: error.to_string(),
                    }),
                }
            }
            if !followup
                && let Err(source) = self
                    .session
                    .append_custom(
                        MEMORY_RECALL_CUSTOM_TYPE,
                        &encode_memory_recall(&state.memory_records),
                    )
                    .await
            {
                return Err(self.fail(
                    emit,
                    AgentError::Persist {
                        kind: "memory recall record".into(),
                        source,
                    },
                ));
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
            let step = StepControl::new(control);
            steering.arm(step.cancellation_token());
            let provider_step = self
                .dispatch_normal_provider_step(emit, &mut state, &system_context, &step)
                .await;
            steering.disarm();
            let response = match provider_step {
                Ok(response) => response,
                Err(AgentError::Provider(ProviderError::Cancelled)) if step.interrupted() => {
                    // Only the step token is cancelled: a step interrupt stopped
                    // the provider call or an automatic compaction around it,
                    // and the turn continues with the guidance appended.
                    if let Err(error) = self.deliver_steering_after_interrupt(&steering, emit).await
                    {
                        return Err(self.fail(emit, error));
                    }
                    continue;
                }
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
            if let Err(source) = self.append_message(assistant).await {
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
                steering.arm(step.cancellation_token());
                let batch = self
                    .run_tool_call(emit, &mut state, &step, &block, None, replan)
                    .await;
                steering.disarm();
                let batch = batch?;
                replan |= batch == crate::approval::BatchControl::Replan;
            }
            // The whole batch ran before the guard is read, so a stopped turn
            // never leaves an assistant tool call without its result.
            if let Some(stall) = state.progress.take_stall() {
                return Err(self.fail(emit, AgentError::NoProgress(stall)));
            }

            // An interrupted batch always delivers its guidance with the event
            // below instead of ending or yielding the turn.
            let batch_interrupted = had_tool_call && step.interrupted();
            if !had_tool_call
                || (state.yield_after_batch
                    && !state.batch_has_errors
                    && !replan
                    && !batch_interrupted)
            {
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
            let delivered = if batch_interrupted {
                self.deliver_steering_after_interrupt(&steering, emit).await
            } else {
                self.deliver_steering(&steering, emit).await
            };
            if let Err(error) = delivered {
                return Err(self.fail(emit, error));
            }
            if let Err(error) = self.deliver_notifications(emit).await {
                return Err(self.fail(emit, error));
            }
        }
    }
}
