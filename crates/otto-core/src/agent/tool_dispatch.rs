//! `Agent` tool-call execution, replay of unanswered calls, notification and steering delivery.

use super::*;

impl<P: Provider, T: ToolExecutor, S: Session> Agent<P, T, S> {
    /// Runs one tool call to its durable result: attempt fact, execution,
    /// terminal fact, result message. `prior` is the operation id and the
    /// attempts already recorded when this is a replay of a call a takeover
    /// left without a result; `None` starts a new operation at attempt 1.
    /// Failures are reported through `emit` before they are returned.
    pub(super) async fn run_tool_call(
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
        let mut diagnostic = crate::diagnostics::Observation::start(
            self.options.diagnostics.as_ref(),
            crate::diagnostics::Kind::Tool,
            operation_id.as_str(),
            Some(&block.tool_name),
            &self.options.provider_name,
            &self.options.model,
        );
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
            match self
                .hook(
                    "before_tool",
                    serde_json::json!({
                        "tool": block.tool_name,
                        "tool_call_id": block.tool_call_id,
                        "operation_id": operation_id,
                        "attempt": attempt,
                        "arguments": serde_json::from_str::<serde_json::Value>(arguments.get())
                            .unwrap_or_default()
                    }),
                    control,
                )
                .await
                .inspect(|response| self.hook_notice(emit, response))
            {
                Err(message) => hook_denial = Some(message),
                Ok(response) if response.decision == Some(hooks::Decision::Deny) => {
                    hook_denial = Some(
                        response
                            .reason
                            .unwrap_or_else(|| "Hook denied this tool".into()),
                    )
                }
                Ok(response) => {
                    if let Some(value) = response.arguments {
                        if !value.is_object() {
                            hook_denial = Some("Hook arguments must be a JSON object".into());
                        } else {
                            let replacement =
                                RawValue::from_string(value.to_string()).expect("serialized JSON");
                            if replacement.get() != arguments.get() {
                                let audit = serde_json::json!({
                                    "version": 1,
                                    "tool": block.tool_name,
                                    "tool_call_id": block.tool_call_id,
                                    "operation_id": operation_id,
                                    "arguments": value
                                })
                                .to_string();
                                self.session
                                    .append_custom(
                                        "otto.hook.arguments",
                                        self.redactor.redact_json_strings(&audit).get(),
                                    )
                                    .await
                                    .map_err(|source| {
                                        self.fail(
                                            emit,
                                            AgentError::Persist {
                                                kind: "hook arguments".into(),
                                                source,
                                            },
                                        )
                                    })?;
                                arguments = replacement;
                            }
                        }
                    }
                }
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
            match self
                .hook(
                    "after_tool",
                    serde_json::json!({
                        "tool": block.tool_name,
                        "tool_call_id": block.tool_call_id,
                        "operation_id": operation_id,
                        "attempt": attempt,
                        "arguments": serde_json::from_str::<serde_json::Value>(arguments.get())
                            .unwrap_or_default(),
                        "content": self.redactor.redact_string(&execution.result.content),
                        "is_error": execution.result.is_error,
                        "outcome": execution.outcome
                    }),
                    &token,
                )
                .await
            {
                Err(message) => self.hook_warning(emit, &message),
                Ok(response) => {
                    self.hook_notice(emit, &response);
                    if let Some(content) = response.content {
                        let audit = serde_json::json!({
                            "version": 1,
                            "tool": block.tool_name,
                            "tool_call_id": block.tool_call_id,
                            "operation_id": operation_id,
                            "content": execution.result.persisted_text(),
                            "is_error": execution.result.is_error,
                            "outcome": execution.outcome
                        })
                        .to_string();
                        self.session
                            .append_custom(
                                "otto.hook.result",
                                self.redactor.redact_json_strings(&audit).get(),
                            )
                            .await
                            .map_err(|source| {
                                self.fail(
                                    emit,
                                    AgentError::Persist {
                                        kind: "original tool result".into(),
                                        source,
                                    },
                                )
                            })?;
                        execution.result.content = content;
                        execution.result.persisted_content = None;
                    }
                }
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
        let diagnostic_finish = crate::diagnostics::Finish {
            status: crate::diagnostics::status(&execution.outcome),
            outcome: Some(execution.outcome.clone()),
            attempts: Some(attempt),
            usage: None,
            cached_input_tokens_present: None,
            error_class: execution
                .result
                .is_error
                .then_some(crate::diagnostics::ErrorClass::Tool),
        };
        let is_error = execution.result.is_error;
        // Observed here because this is the only point that knows both the
        // arguments the model sent and whether the result failed.
        // A user- or deadline-requested stop is not evidence of the model
        // repeating itself, so a stopped execution is not observed.
        if execution.outcome.stop_reason.is_none() {
            if self.options.hooks.is_some() {
                // Hook effects are not described by the tool's retry safety.
                state
                    .progress
                    .observe(&block.tool_name, arguments.get(), is_error);
            } else {
                state.progress.observe_execution(
                    &block.tool_name,
                    arguments.get(),
                    &execution,
                    self.tools.retry_safety(&block.tool_name),
                );
            }
        }
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
        if let Err(source) = self.append_message(stored).await {
            return Err(self.fail(
                emit,
                AgentError::Persist {
                    kind: format!("tool result for {}", quote_go(&block.tool_call_id)),
                    source,
                },
            ));
        }
        diagnostic.finish(diagnostic_finish);
        Ok(batch_control)
    }

    /// Runs again the calls a takeover left without a result, when every one
    /// of them names a replayable tool and has at most one recorded attempt
    /// with no terminal fact (the same all-or-nothing rule the store applies
    /// when it leaves them pending). Each runs as the next attempt of its
    /// recorded operation, or as attempt 1 of a new one when none was
    /// recorded. Does nothing for any other pending set, which the store has
    /// already answered.
    pub(super) async fn replay_pending_calls(
        &self,
        emit: EventSink<'_>,
        state: &mut RunDispatchState,
        control: &dyn OperationControl,
    ) -> Result<(), AgentError> {
        let Ok(pending) = pending_tool_calls(&self.session.messages()) else {
            return Ok(());
        };
        if pending.is_empty()
            || !pending.iter().all(|call| {
                self.options.hooks.is_none()
                    && self.tools.replayable(&call.tool_name)
                    && !call.arguments.as_ref().is_some_and(|arguments| {
                        crate::safetext::contains_redaction_marker(arguments.get())
                    })
            })
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
    pub(super) async fn deliver_notifications(
        &self,
        emit: EventSink<'_>,
    ) -> Result<(), AgentError> {
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
                self.append_message(message).await.map(|()| true)
            } else {
                let recorded = self.redactor.redact_record_message(&message);
                let outcome = self
                    .session
                    .append_input_if_absent(&notification.id, recorded.clone())
                    .await
                    .map(|outcome| outcome == crate::session::AppendOutcome::Appended);
                if matches!(outcome, Ok(true)) {
                    self.record_turn_message(recorded);
                }
                outcome
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
                cached_input_tokens_present: notification.cached_input_tokens_present,
            });
        }
        Ok(())
    }

    /// Appends the one-time capability change notice as a context message,
    /// then the `otto.capabilities` record. The message goes first: a failure
    /// between the two repeats the notice, while the reverse order could lose
    /// it. The record is also rewritten, without a notice, when only fields
    /// that produce no notice differ, such as the Otto version.
    pub(super) async fn announce_tool_changes(&self) -> Result<(), AgentError> {
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
            self.append_message(message)
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

    pub(super) async fn deliver_steering(
        &self,
        steering: &steering::Run,
        emit: EventSink<'_>,
    ) -> Result<(), AgentError> {
        self.append_steering(steering.drain(), emit).await
    }

    /// Delivers the queued guidance of a step interrupt. The event carries the
    /// exact number of messages appended, which is zero when the guidance was
    /// withdrawn after the request.
    pub(super) async fn deliver_steering_after_interrupt(
        &self,
        steering: &steering::Run,
        emit: EventSink<'_>,
    ) -> Result<(), AgentError> {
        let messages = steering.drain();
        emit(Event::StepInterrupted {
            guidance: messages.len(),
        });
        self.append_steering(messages, emit).await
    }

    pub(super) async fn append_steering(
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
                self.append_message(message).await.map(|()| true)
            } else {
                let recorded = self.redactor.redact_record_message(&message);
                let outcome = self
                    .session
                    .append_input_if_absent(&steering.input_id, recorded.clone())
                    .await
                    .map(|outcome| outcome == crate::session::AppendOutcome::Appended);
                if matches!(outcome, Ok(true)) {
                    self.record_turn_message(recorded);
                }
                outcome
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
                cached_input_tokens_present: false,
            });
        }
        Ok(())
    }
}
