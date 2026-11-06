//! `Agent` local compaction: summary request construction, redaction and execution.

use super::*;

impl<P: Provider, T: ToolExecutor, S: Session> Agent<P, T, S> {
    pub(super) fn plain_request(
        &self,
        messages: Vec<Message>,
        tools: Vec<ToolDefinition>,
    ) -> Request {
        Request {
            model: self.options.model.clone(),
            system_prompt: self.options.system_prompt.clone(),
            thinking: self.options.thinking.clone(),
            messages,
            tools,
            cache_key: self.options.cache_key.clone(),
            ..Request::default()
        }
    }

    pub(super) fn redact_compaction_selection(
        &self,
        selection: &CompactionSelection,
    ) -> Result<CompactionSelection, AgentError> {
        let redact_all = |messages: &[Message]| -> Result<Vec<Message>, AgentError> {
            messages
                .iter()
                .map(|message| {
                    let request = Request {
                        messages: vec![message.clone()],
                        ..Request::default()
                    };
                    self.redactor
                        .guard_provider_request_secrets_only(&request)?
                        .messages
                        .into_iter()
                        .next()
                        .ok_or(AgentError::ProviderEgressSecret)
                })
                .collect()
        };
        let redact_texts = |messages: &[String]| -> Result<Vec<String>, AgentError> {
            messages
                .iter()
                .map(|message| self.redactor.redact_provider_text(message))
                .collect()
        };
        Ok(CompactionSelection {
            previous_summary: self
                .redactor
                .redact_provider_text(&selection.previous_summary)?,
            previous_user_messages: redact_texts(&selection.previous_user_messages)?,
            previous_capability_updates: redact_texts(&selection.previous_capability_updates)?,
            historical_source: redact_all(&selection.historical_source)?,
            turn_prefix_source: redact_all(&selection.turn_prefix_source)?,
            retained: redact_all(&selection.retained)?,
            first_kept_id: selection.first_kept_id.clone(),
            split_turn: selection.split_turn,
        })
    }

    pub(super) fn redact_compaction_details(
        &self,
        details: &CompactionDetails,
    ) -> CompactionDetails {
        let redact_all = |paths: &[String]| -> Vec<String> {
            paths
                .iter()
                .map(|path| self.redactor.redact_string(path))
                .collect()
        };
        // Record ids and revisions are opaque tokens, not text; they pass
        // through unchanged.
        CompactionDetails {
            read_files: redact_all(&details.read_files),
            modified_files: redact_all(&details.modified_files),
            omitted_read_files: details.omitted_read_files,
            omitted_modified_files: details.omitted_modified_files,
            memory_records: details.memory_records.clone(),
            memory_coverage: details.memory_coverage,
            memory_retracted: details.memory_retracted.clone(),
        }
    }

    /// Builds the summary request, plus a second one when the current turn was
    /// split and both halves have content. `retraction_instructions` is the
    /// rendered block of retracted memory records the summarizer must drop,
    /// or the empty string.
    pub(super) fn prepare_summary_requests(
        &self,
        selection: &CompactionSelection,
        focus: &str,
        retraction_instructions: &str,
        previous_details: &CompactionDetails,
    ) -> Result<(SummaryRequest, Option<SummaryRequest>), String> {
        let turn_prefix_only = CompactionSelection {
            historical_source: selection.turn_prefix_source.clone(),
            ..CompactionSelection::default()
        };
        let request_selection = if selection.historical_source.is_empty()
            && !selection.turn_prefix_source.is_empty()
            && selection.previous_summary.is_empty()
        {
            &turn_prefix_only
        } else {
            selection
        };
        let prepared = build_summary_request(
            &self.options,
            request_selection,
            focus,
            retraction_instructions,
            previous_details,
        )?;
        if !selection.split_turn
            || selection.historical_source.is_empty()
            || selection.turn_prefix_source.is_empty()
        {
            return Ok((prepared, None));
        }
        let turn_selection = CompactionSelection {
            turn_prefix_source: selection.turn_prefix_source.clone(),
            ..CompactionSelection::default()
        };
        let turn_prepared = build_summary_request(
            &self.options,
            &turn_selection,
            focus,
            retraction_instructions,
            &prepared.details,
        )?;
        Ok((prepared, Some(turn_prepared)))
    }

    /// Runs one summary request under a child token, so a response that
    /// streams a tool call or runs past `maximum_bytes` is stopped without
    /// cancelling the caller's turn.
    pub(super) async fn execute_summary_request(
        &self,
        request: &Request,
        maximum_bytes: usize,
        structured: bool,
        emit: EventSink<'_>,
        control: &dyn OperationControl,
    ) -> Result<(String, Usage, bool, bool), AgentError> {
        stopped(control)?;
        let child = control.cancellation_token().child_token();
        let summary_control = SummaryControl {
            token: child.clone(),
            parent: control,
        };
        let streamed_bytes = AtomicUsize::new(0);
        let invalid_stream = AtomicBool::new(false);

        let guarded_request = self.redactor.guard_provider_request(request)?;
        let operation_id = (self.options.new_operation_id)()
            .map_err(|message| AgentError::OperationIdentity { message })?;
        let mut diagnostic = crate::diagnostics::Observation::start(
            self.options.diagnostics.as_ref(),
            crate::diagnostics::Kind::Compaction,
            operation_id.as_str(),
            None,
            &self.options.provider_name,
            &self.options.model,
        );
        let started = (self.options.now)();
        let mut tracker = super::super::provider_call::CallTracker::new(started);
        let now = &self.options.now;
        let redactor = &self.redactor;
        let outcome = {
            let mut on_stream = |event: crate::provider::StreamEvent| {
                diagnostic.stream(&event);
                tracker.observe(&event, now(), redactor);
                if invalid_stream.load(Ordering::SeqCst) {
                    return;
                }
                if let crate::provider::StreamEvent::Retry {
                    attempt,
                    max_attempts,
                    delay,
                    reason,
                    interrupted_output,
                } = event
                {
                    streamed_bytes.store(0, Ordering::SeqCst);
                    emit(super::Event::ProviderRetry {
                        operation_id: operation_id.clone(),
                        attempt,
                        max_attempts,
                        delay,
                        reason,
                        interrupted_output,
                    });
                    return;
                }
                // Reasoning is not part of the summary; the response check drops it.
                if matches!(event, crate::provider::StreamEvent::ReasoningDelta { .. }) {
                    return;
                }
                let crate::provider::StreamEvent::TextDelta { text } = event else {
                    invalid_stream.store(true, Ordering::SeqCst);
                    child.cancel();
                    return;
                };
                let current = streamed_bytes.fetch_add(text.len(), Ordering::SeqCst) + text.len();
                if current > maximum_bytes {
                    invalid_stream.store(true, Ordering::SeqCst);
                    child.cancel();
                }
            };
            self.provider
                .complete_observed(
                    &guarded_request,
                    &mut on_stream,
                    &summary_control,
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
            super::super::ProviderOperationKind::LocalCompaction,
            duration,
            &outcome,
        );
        self.append_provider_call(super::super::provider_call::SettledCall {
            operation_id: &operation_id,
            kind: super::super::ProviderOperationKind::LocalCompaction,
            tracker,
            duration,
            settlement: &outcome,
            request,
            memory: super::super::provider_call::RequestMemory::default(),
        })
        .await?;

        stopped(control)?;
        if invalid_stream.load(Ordering::SeqCst) {
            return Err(AgentError::InvalidCompactionSummary(
                "streamed response exceeded its bound or attempted a tool call".into(),
            ));
        }
        let response = outcome.result.map_err(|error| match error {
            crate::provider::ProviderError::Cancelled
            | crate::provider::ProviderError::DeadlineExceeded => AgentError::Provider(error),
            _ => AgentError::CompactionBoundary {
                message: "compaction summary provider request failed".into(),
                cause: error.to_string(),
            },
        })?;

        let cached_input_tokens_present = response.cached_input_tokens_present;
        let message = response.message;
        validate_provider_response_message(&message)
            .map_err(AgentError::InvalidCompactionSummary)?;
        validate_raw_summary_response_bound(&message, maximum_bytes)
            .map_err(AgentError::InvalidCompactionSummary)?;
        let message = self.redact_message(&message);
        if message.role != Role::Assistant {
            return Err(AgentError::InvalidCompactionSummary(
                "response role is not assistant".into(),
            ));
        }
        let summary = if structured {
            validate_structured_summary(&message)
        } else {
            validate_turn_summary(&message)
        }
        .map_err(AgentError::InvalidCompactionSummary)?;

        let usage_present = message.usage.is_some();
        Ok((
            summary,
            message.usage.unwrap_or_default(),
            usage_present,
            usage_present && cached_input_tokens_present,
        ))
    }
}
