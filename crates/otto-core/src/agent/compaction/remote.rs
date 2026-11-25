//! `Agent` remote compaction: the provider-held checkpoint path.

use super::*;

impl<P: Provider, T: ToolExecutor, S: Session> Agent<P, T, S> {
    pub(super) async fn compact_remote(
        &self,
        reason: CompactionReason,
        automatic: bool,
        emit: EventSink<'_>,
        control: &dyn OperationControl,
    ) -> Result<CompactionResult, AgentError> {
        stopped(control)?;
        let remote_compaction = self
            .session
            .remote_compaction()
            .filter(|checkpoint| checkpoint.provider == self.options.provider_name);
        let mut messages = if remote_compaction.is_some() {
            self.session.post_remote_compaction_messages()
        } else {
            self.session.model_messages()
        };
        bash_history::summarize(&mut messages);
        if messages.is_empty() && remote_compaction.is_none() {
            return Err(AgentError::NothingToCompact);
        }
        let tools = self.tools.definitions();
        let tokens_before = remote_compaction.is_none().then(|| {
            estimate_request(
                &self.plain_request(messages.clone(), tools.clone()),
                self.session.latest_compaction().as_ref(),
            )
        });
        let summarized_messages = messages.len();
        let request = Request {
            model: self.options.model.clone(),
            system_prompt: self.options.system_prompt.clone(),
            thinking: self.options.thinking.clone(),
            messages,
            tools,
            cache_key: self.options.cache_key.clone(),
            remote_compaction,
            request_remote_compaction: true,
            cache_boundaries: Vec::new(),
        };
        let guarded_request = self.redactor.guard_provider_request(&request)?;
        emit(Event::CompactionPlanned {
            plan: CompactionPlan {
                reason,
                automatic,
                tokens_before,
                estimated_tokens_after: None,
                summarized_messages,
                retained_messages: 0,
                mode: CompactionMode::Remote,
            },
        });
        let operation_id = (self.options.new_operation_id)()
            .map_err(|message| AgentError::OperationIdentity { message })?;
        let mut diagnostic = crate::diagnostics::Observation::start(
            self.options.diagnostics.as_ref(),
            crate::diagnostics::Kind::RemoteCompaction,
            operation_id.as_str(),
            None,
            &self.options.provider_name,
            &self.options.model,
        );
        let started = (self.options.now)();
        let mut tracker = super::super::provider_call::CallTracker::new(started);
        let now = &self.options.now;
        let redactor = &self.redactor;
        let invalid_stream = AtomicBool::new(false);
        let child = control.cancellation_token().child_token();
        let remote_control = SummaryControl {
            token: child.clone(),
            parent: control,
        };
        let outcome = {
            let mut on_stream = |event: crate::provider::StreamEvent| {
                diagnostic.stream(&event);
                tracker.observe(&event, now(), redactor);
                match event {
                    crate::provider::StreamEvent::Retry {
                        attempt,
                        max_attempts,
                        delay,
                        reason,
                        interrupted_output,
                    } => emit(Event::ProviderRetry {
                        operation_id: operation_id.clone(),
                        attempt,
                        max_attempts,
                        delay,
                        reason,
                        interrupted_output,
                    }),
                    // The request carries the thinking setting, so a reasoning
                    // summary can stream; it is not part of the checkpoint.
                    crate::provider::StreamEvent::ReasoningDelta { .. } => {}
                    _ => {
                        invalid_stream.store(true, Ordering::SeqCst);
                        child.cancel();
                    }
                }
            };
            self.provider
                .complete_observed(
                    &guarded_request,
                    &mut on_stream,
                    &remote_control,
                    diagnostic.observer(),
                )
                .await
        };
        diagnostic.provider_finished(&outcome);
        let duration = ((self.options.now)() - started)
            .to_std()
            .unwrap_or_default();
        self.emit_provider_api_call(
            emit,
            operation_id.clone(),
            super::super::ProviderOperationKind::RemoteCompaction,
            duration,
            &outcome,
        );
        self.append_provider_call(super::super::provider_call::SettledCall {
            operation_id: &operation_id,
            kind: super::super::ProviderOperationKind::RemoteCompaction,
            tracker,
            duration,
            settlement: &outcome,
            request: &request,
            memory: super::super::provider_call::RequestMemory::default(),
        })
        .await?;
        stopped(control)?;
        if invalid_stream.load(Ordering::SeqCst) {
            return Err(AgentError::CompactionBoundary {
                message: "remote compaction returned visible output".into(),
                cause: "responses remote compaction protocol error".into(),
            });
        }
        let response = outcome.result.map_err(AgentError::Provider)?;
        let usage = response.usage();
        let cached_input_tokens_present = usage.is_some() && response.cached_input_tokens_present;
        let checkpoint =
            response
                .remote_compaction
                .ok_or_else(|| AgentError::CompactionBoundary {
                    message: "remote compaction response was missing its checkpoint".into(),
                    cause: "responses remote compaction protocol error".into(),
                })?;
        stopped(control)?;
        self.session
            .append_remote_compaction(checkpoint.clone())
            .await
            .map_err(|error| AgentError::CompactionBoundary {
                message: "persist remote compaction checkpoint failed".into(),
                cause: error.to_string(),
            })?;
        let result = CompactionResult {
            checkpoint_id: checkpoint.id,
            reason,
            tokens_before: usage.map(|usage| usage.input_tokens).or(tokens_before),
            estimated_tokens_after: None,
            automatic,
            usage: usage.unwrap_or_default(),
            usage_present: usage.is_some(),
            cached_input_tokens_present,
            ..CompactionResult::default()
        };
        let token = CancellationToken::new();
        match self
            .hook(
                "after_compaction",
                serde_json::json!({"checkpoint_id":result.checkpoint_id,"reason":format!("{reason:?}")}),
                &token,
            )
            .await
        {
            Ok(response) => self.hook_notice(emit, &response),
            Err(message) => self.hook_warning(emit, &message),
        }
        emit(Event::CompactionCompleted {
            compaction: result.clone(),
        });
        stopped(control)?;
        Ok(result)
    }
}
