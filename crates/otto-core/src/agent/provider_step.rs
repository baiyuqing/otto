//! `Agent` provider step: proactive and overflow compaction, request building, attempt settlement.

use super::*;

impl<P: Provider, T: ToolExecutor, S: Session> Agent<P, T, S> {
    /// One provider step, with the two automatic compaction paths around it:
    /// proactive compaction when the estimate crosses the soft trigger, and
    /// one retry after the provider reports a context overflow.
    pub(super) async fn dispatch_normal_provider_step(
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
        // The recalled set is re-filtered and the notices re-selected for
        // every step, so a forget or retirement inside the tool loop reaches
        // the next request (#591 §5).
        self.refresh_memory_context(emit, state, control).await;
        state
            .sent_record_ids
            .extend(state.memory_record_ids.iter().cloned());
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
                if self
                    .attempt_proactive_compaction(
                        reason,
                        estimate,
                        hard_trigger,
                        state,
                        emit,
                        control,
                    )
                    .await?
                {
                    (request, estimate) = self.build_normal_provider_request(state, system_context);
                    estimate_complete = context_estimate::estimate_complete(&request);
                    if estimate_complete && estimate > hard_trigger {
                        return Err(automatic_dispatch_error(
                            AUTOMATIC_COMPACTION_STILL_TOO_LARGE_MESSAGE,
                            Vec::new(),
                        ));
                    }
                }
            } else if estimate > hard_trigger {
                return Err(automatic_dispatch_error(
                    AUTOMATIC_COMPACTION_ATTEMPT_USED_MESSAGE,
                    Vec::new(),
                ));
            }
        }

        self.admit_shared()?;
        let (response, visible_output, error) = self
            .complete_normal_provider_attempt(request, state, emit, control)
            .await;
        let Some(original_overflow) = error else {
            state.next_step_tool_results.clear();
            return Ok(response);
        };
        if !self.options.compaction.auto
            || visible_output
            || !is_typed_context_overflow(&original_overflow)
        {
            return Err(original_overflow);
        }

        if !self
            .compact_after_overflow(&original_overflow, emit, control)
            .await?
        {
            return Err(original_overflow);
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
        self.admit_shared()?;
        let (response, _, error) = self
            .complete_normal_provider_attempt(retry_request, state, emit, control)
            .await;
        state.next_step_tool_results.clear();
        match error {
            Some(error) if is_typed_context_overflow(&error) => Err(automatic_dispatch_error(
                OVERFLOW_RETRY_FAILURE_MESSAGE,
                vec![original_overflow.to_string(), error.to_string()],
            )),
            Some(error) => Err(error),
            None => Ok(response),
        }
    }

    /// One automatic proactive compaction, at most once per turn (the caller
    /// checks `state.proactive_attempted`). Returns `Ok(true)` when a
    /// checkpoint was written, `Ok(false)` when there was nothing to compact
    /// or compaction failed with `estimate` below `hard_trigger` (a warning
    /// event is emitted), and an error when the stop or hard-limit rules apply.
    pub(super) async fn attempt_proactive_compaction(
        &self,
        reason: CompactionReason,
        estimate: i64,
        hard_trigger: i64,
        state: &mut RunDispatchState,
        emit: EventSink<'_>,
        control: &dyn OperationControl,
    ) -> Result<bool, AgentError> {
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
                Ok(false)
            }
            Err(error) => {
                if let Some(stop) = stopped_provider_error(control) {
                    return Err(AgentError::Provider(stop));
                }
                if let Some(cancellation) = automatic_cancellation(false, &error) {
                    return Err(cancellation);
                }
                // A budget stop is final; it is not a compaction failure that
                // the turn can continue past.
                if matches!(error, AgentError::BudgetExceeded(_)) {
                    return Err(error);
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
                Ok(false)
            }
            Ok(_) => {
                if let Some(error) = stopped_provider_error(control) {
                    return Err(AgentError::Provider(error));
                }
                Ok(true)
            }
        }
    }

    /// Compaction after a typed context overflow. `Ok(true)` means a
    /// checkpoint was written and the caller may retry; `Ok(false)` means
    /// there was nothing to compact and the caller reports the original
    /// overflow; an error is final.
    pub(super) async fn compact_after_overflow(
        &self,
        original_overflow: &AgentError,
        emit: EventSink<'_>,
        control: &dyn OperationControl,
    ) -> Result<bool, AgentError> {
        let Err(compaction_error) =
            Box::pin(self.compact_locked(CompactionReason::Overflow, "", emit, control)).await
        else {
            return Ok(true);
        };
        if compaction_error.is_nothing_to_compact() {
            emit(Event::CompactionCompleted {
                compaction: CompactionResult {
                    reason: CompactionReason::Overflow,
                    automatic: true,
                    noop: true,
                    ..CompactionResult::default()
                },
            });
            return Ok(false);
        }
        if let Some(stop) = stopped_provider_error(control) {
            return Err(AgentError::Provider(stop));
        }
        if let Some(cancellation) = automatic_cancellation(false, &compaction_error) {
            return Err(cancellation);
        }
        if matches!(compaction_error, AgentError::BudgetExceeded(_)) {
            return Err(compaction_error);
        }
        Err(automatic_dispatch_error(
            OVERFLOW_COMPACTION_FAILURE_MESSAGE,
            vec![original_overflow.to_string(), compaction_error.to_string()],
        ))
    }

    /// The capabilities a request sends: sorted, deduplicated tool names,
    /// their definition digests and the version.
    pub(super) fn current_capabilities(&self) -> crate::session::Capabilities {
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

    /// Validates everything already known when an ordinary turn is admitted,
    /// before hooks, operation IDs, diagnostics, events, or durable writes.
    /// The prospective input exists only in this cloned provider request.
    pub(super) fn preflight_ordinary_turn(
        &self,
        user_text: &str,
        images: &[Block],
        system_context: &str,
        followup: bool,
    ) -> Result<(), AgentError> {
        let (mut request, _) =
            self.build_normal_provider_request(&RunDispatchState::default(), system_context);
        for entry in self.options.mailbox.queued() {
            let notification = entry.notification;
            let user_message = notification.kind == Some(inbox::NotificationKind::UserMessage);
            let context_type = if user_message {
                String::new()
            } else {
                notification.context_type().to_owned()
            };
            let metadata = ContextMetadata {
                task_id: notification.task_id,
            };
            request.messages.push(Message {
                role: if user_message {
                    Role::User
                } else {
                    Role::Context
                },
                context_type,
                context_metadata: (!user_message && metadata.validate().is_ok())
                    .then_some(metadata),
                blocks: vec![Block::text(notification.text)],
                ..Message::default()
            });
        }
        if !trim_go_space(user_text).is_empty() || !images.is_empty() {
            let mut blocks = Vec::new();
            if !trim_go_space(user_text).is_empty() {
                blocks.push(Block::text(if followup {
                    format!("[Agent follow-up]\n{user_text}")
                } else {
                    user_text.to_owned()
                }));
            }
            blocks.extend(images.iter().cloned());
            request.messages.push(Message {
                role: if followup { Role::Context } else { Role::User },
                context_type: if followup {
                    "agent_followup".into()
                } else {
                    String::new()
                },
                display: followup,
                input_id: self
                    .next_input_id
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .clone()
                    .unwrap_or_default(),
                blocks,
                ..Message::default()
            });
        }
        self.redactor.guard_provider_request(&request).map(|_| ())
    }

    /// Builds the request for one ordinary provider step and estimates it.
    ///
    /// The messages are clones, so the overlay substitution and the memory
    /// message never reach the session.
    pub(super) fn build_normal_provider_request(
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
        let projected_history = bash_history::summarize(&mut messages);
        apply_tool_result_overlay(&mut messages, &state.tool_result_overlay);
        state.apply_next_step_results(&mut messages);
        messages = crate::model::project_input_provenance(messages);
        // Message counts before the request-local memory message is inserted:
        // the prefixes ending before the last two user messages recur in the
        // next requests, which lack that message and any overlay text.
        let mut cache_boundaries: Vec<usize> = messages
            .iter()
            .enumerate()
            .rev()
            .filter(|(_, message)| message.role == Role::User)
            .map(|(index, _)| {
                index
                    .checked_sub(1)
                    .filter(|previous| messages[*previous].context_type == "input_origin")
                    .unwrap_or(index)
            })
            .filter(|index| *index > 0)
            .take(2)
            .collect();
        cache_boundaries.reverse();
        let memory_message = super::memory_retraction::memory_request_text(
            &state.memory_context,
            &state.retraction_context,
        );
        if !memory_message.is_empty() {
            let current_user = messages
                .iter()
                .rposition(|message| message.role == Role::User)
                .unwrap_or(messages.len());
            // Keep the provider-only label immediately adjacent to its input.
            // The request-local runtime memory must not inherit human attribution.
            let insertion = current_user
                .checked_sub(1)
                .filter(|index| messages[*index].context_type == "input_origin")
                .unwrap_or(current_user);
            messages.insert(
                insertion,
                Message {
                    role: Role::User,
                    blocks: vec![Block::text(memory_message)],
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
            cache_boundaries,
        };
        // A remote checkpoint supersedes any older local one, and the request
        // carries only its tail, so the local checkpoint's first message id
        // cannot anchor the estimate there.
        let latest = request
            .remote_compaction
            .is_none()
            .then(|| self.session.latest_compaction())
            .flatten();
        let estimate = if projected_history && request.remote_compaction.is_none() {
            context_estimate::estimate_unanchored_request(&request)
        } else {
            context_estimate::estimate_request(&request, latest.as_ref())
        };
        (request, estimate)
    }

    /// Runs one provider call, reporting whether any text reached the user.
    /// `before_provider_call` and `after_provider_call` run around it.
    /// That flag decides whether an overflow may be retried: a partly
    /// delivered answer must not be replaced by a second one.
    pub(super) async fn complete_normal_provider_attempt(
        &self,
        request: Request,
        state: &RunDispatchState,
        emit: EventSink<'_>,
        control: &dyn OperationControl,
    ) -> (Response, bool, Option<AgentError>) {
        let redactor = &self.redactor;
        let mut stream = redactor.new_stream();
        // Reasoning has its own redaction stream; it is flushed before the
        // first text delta so the two keep their provider order.
        let mut reasoning = redactor.new_stream();
        let visible_output = std::sync::atomic::AtomicBool::new(false);
        // The hook payload and its audit record share this call's operation ID.
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
        // Hook changes are applied before the guard, so hook text is guarded too.
        let hooked = match self
            .before_provider_call(request, &operation_id, emit, control)
            .await
        {
            Ok(provider_hooks::BeforeProviderCall::Proceed(hooked)) => hooked,
            Ok(provider_hooks::BeforeProviderCall::Denied { reason }) => {
                return (Response::default(), false, Some(AgentError::Other(reason)));
            }
            Err(error) => return (Response::default(), false, Some(error)),
        };
        let request = hooked.request();
        let guarded_request = match self.redactor.guard_provider_request(request) {
            Ok(request) => request,
            Err(error) => return (Response::default(), false, Some(error)),
        };
        let mut diagnostic = crate::diagnostics::Observation::start(
            self.options.diagnostics.as_ref(),
            crate::diagnostics::Kind::Provider,
            operation_id.as_str(),
            None,
            &self.options.provider_name,
            &self.options.model,
        );
        let started = (self.options.now)();
        let mut tracker = provider_call::CallTracker::new(started);
        let now = &self.options.now;
        let outcome = {
            let mut on_stream = |event: StreamEvent| {
                diagnostic.stream(&event);
                tracker.observe(&event, now(), redactor);
                match event {
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
                }
            };
            self.provider
                .complete_observed(
                    &guarded_request,
                    &mut on_stream,
                    control,
                    diagnostic.observer(),
                )
                .await
        };
        diagnostic.provider_finished(&outcome);
        if let Ok(response) = &outcome.result {
            self.charge_budget(response.message.usage);
        }
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
        let retries = tracker.retries().to_vec();
        if let Err(error) = self
            .append_provider_call(provider_call::SettledCall {
                operation_id: &operation_id,
                kind: ProviderOperationKind::Ordinary,
                tracker,
                duration,
                settlement: &outcome,
                request,
                memory: provider_call::RequestMemory {
                    text: &state.memory_context,
                    record_ids: &state.memory_record_ids,
                },
            })
            .await
        {
            return (Response::default(), true, Some(error));
        }
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
        if outcome.result.is_ok() {
            let held = reasoning.flush();
            if !held.is_empty() {
                emit(Event::ReasoningDelta { text: held });
            }
            let text = stream.flush();
            if !text.is_empty() {
                visible_output.store(true, std::sync::atomic::Ordering::SeqCst);
                emit(Event::TextDelta { text });
            }
        }
        self.after_provider_call(emit, &operation_id, &outcome, duration)
            .await;
        let visible_output = visible_output.load(std::sync::atomic::Ordering::SeqCst);
        match outcome.result {
            Err(error) => (
                Response::default(),
                visible_output,
                Some(AgentError::Provider(error)),
            ),
            Ok(response) => (response, visible_output, None),
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
    pub(super) fn run_with_incomplete_redactions(
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

    /// Ends a turn the progress guard stopped. The diagnostics hook is called
    /// here, the only place a stall becomes an error, so each stopped turn is
    /// reported once and before redaction can replace the error.
    pub(super) fn stop_stalled(
        &self,
        emit: EventSink<'_>,
        stall: super::progress::Stall,
    ) -> AgentError {
        if let Some(diagnostics) = self.options.diagnostics.as_ref() {
            diagnostics.turn_stalled(&stall);
        }
        self.fail(emit, AgentError::NoProgress(stall))
    }

    /// Reports a reached budget limit to the diagnostics hook and returns the
    /// error. This is the only place a budget stop becomes an error, so each
    /// stopped holder is reported once and before redaction can replace the
    /// error. The caller ends the turn with [`Self::fail`], directly or by
    /// returning the error to the turn loop.
    pub(super) fn report_budget(&self, exceeded: super::budget::Exceeded) -> AgentError {
        if let Some(diagnostics) = self.options.diagnostics.as_ref() {
            let budget_id = self.with_budget(|budget| budget.budget_id().to_owned());
            diagnostics.turn_budget_exceeded(&exceeded, &budget_id);
        }
        AgentError::BudgetExceeded(exceeded)
    }

    /// Ends a turn at a budget limit; the counterpart of `stop_stalled`.
    pub(super) fn stop_budget(
        &self,
        emit: EventSink<'_>,
        exceeded: super::budget::Exceeded,
    ) -> AgentError {
        let error = self.report_budget(exceeded);
        self.fail(emit, error)
    }

    fn with_budget<R>(&self, f: impl FnOnce(&super::budget::TurnBudget) -> R) -> R {
        f(&self
            .budget
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner))
    }

    /// Starts the budget of a run or a manual compaction from its scope.
    pub(super) fn begin_budget(&self, scope: super::budget::BudgetScope) {
        *self
            .budget
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = scope.into_turn();
    }

    /// Starts the holder that counts the run in progress on the root's
    /// active-time clock; see [`super::budget::Holder`].
    pub(super) fn enter_holder(&self) -> super::budget::Holder {
        self.with_budget(|budget| budget.root().enter())
    }

    /// Charges the usage of one provider response, as soon as it is known.
    pub(super) fn charge_budget(&self, usage: Option<Usage>) {
        self.with_budget(|budget| budget.charge(usage));
    }

    /// Admits the next ordinary provider call and counts it as a step.
    pub(super) fn admit_step(&self) -> Result<(), super::budget::Exceeded> {
        self.with_budget(|budget| budget.admit_step())
    }

    /// Refuses a request once a limit shared by the root (tokens or active
    /// time) is reached. Used before a retry after overflow and before each
    /// compaction request.
    pub(super) fn admit_shared(&self) -> Result<(), AgentError> {
        self.with_budget(|budget| budget.admit_shared())
            .map_err(|exceeded| self.report_budget(exceeded))
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
