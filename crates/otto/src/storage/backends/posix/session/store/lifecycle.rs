//! Compaction, close and interrupted-turn repair for [`Store`].

use super::*;

impl Store {
    /// Records a compaction checkpoint and returns the metadata now in force.
    ///
    /// Every check runs before the write: the checkpoint is validated, its
    /// anchor is resolved against the active path, and the candidate entry is
    /// replayed. Nothing is persisted and no state changes when any of them
    /// fails.
    pub fn append_compaction(
        &self,
        checkpoint: &CompactionCheckpoint,
    ) -> Result<CompactionMetadata, PiError> {
        let mut state = self.lock()?;
        state.writable()?;
        validate_compaction_checkpoint(checkpoint)?;

        let first_kept_entry_id = state.compaction_anchor(&checkpoint.first_kept_entry_id)?;
        let timestamp = format_persisted_timestamp(checkpoint.created_at, "compaction")?;
        let entry_id = state.new_entry_id("compaction")?;
        let usage = compaction_usage_to_pi(checkpoint.usage.as_ref())?;

        let mut entry = PiEntry::new("compaction", &entry_id, state.leaf_id.clone(), &timestamp);
        entry.compaction = Some(Box::new(PiCompaction {
            summary: recording::RECORD_REDACTOR.redact_record_text(&checkpoint.summary),
            first_kept_entry_id: Some(first_kept_entry_id),
            tokens_before: checkpoint.tokens_before,
            usage,
            ..PiCompaction::default()
        }));

        if compaction_details_present(&checkpoint.details)
            || checkpoint
                .usage
                .is_some_and(|usage| usage.cache_write_input_tokens.is_some())
        {
            let mut details = serde_json::to_value(&checkpoint.details)
                .map_err(|error| PiError::other(format!("encode compaction details: {error}")))?;
            if let Some(usage) = checkpoint
                .usage
                .filter(|usage| usage.cache_write_input_tokens.is_some())
            {
                details["otto"] = serde_json::json!({ "usage": usage });
            }
            if let Some(compaction) = entry.compaction.as_mut() {
                compaction.details = Some(raw_value(details.to_string())?);
            }
        }

        let mut candidate = state.entries.clone();
        candidate.push(entry.clone());
        let (resolved, _) = build_context(&candidate, &entry_id)?;

        let last = candidate.len() - 1;
        candidate[last] = entry.clone();

        let mut metadata = latest_compaction_metadata(&candidate, &entry_id)?
            .filter(|metadata| metadata.id == entry_id)
            .ok_or_else(|| PiError::invalid("candidate compaction did not become active"))?;
        project_compaction_metadata(
            &mut metadata,
            &candidate,
            &entry_id,
            &resolved.model_messages,
        )?;

        let encoded = encode_pi_record(PiRecord::Entry(&entry))?;
        let record_bytes = state.reserve(&encoded)?;
        state.ensure_file_fatal()?;
        state.write_record(&encoded)?;

        state.entries = candidate;
        state.entry_ids.insert(entry_id.clone());
        state.leaf_id = Some(entry_id);
        state.messages = resolved.messages;
        state.input_ids = resolved.input_ids;
        state.operation_ledger = resolved.operation_ledger;
        state.aggregate_usage = resolved.usage;
        state.usage_present = resolved.usage_present;
        state.file_bytes += record_bytes;
        state.latest_compaction = Some(metadata.clone());
        Ok(metadata)
    }

    /// Closes the session file. Idempotent. Releases the lease this store
    /// acquired (see [`Store::set_lease_check`] for a store that only
    /// checks a lease it does not own). The lease is released on every path,
    /// including when the file is missing or its sync fails, because
    /// `closed` stops `Drop` from releasing it; after a failed sync the
    /// sync error is returned and the file is not written again. A release
    /// failure is returned only when the sync succeeded.
    pub fn close(&self) -> Result<(), PiError> {
        let mut state = self.lock()?;
        if state.closed {
            return Ok(());
        }
        state.closed = true;
        let sync_result = match state.file.take() {
            Some(file) => file
                .sync_all()
                .map_err(|error| PiError::other(format!("close session file: {error}"))),
            None => Ok(()),
        };
        let release_result = if !state.owns_lease {
            Ok(())
        } else if let Some(ownership) = state.writer_ownership.as_ref() {
            ownership.release().map_err(|error| error.to_string())
        } else if let Some(lease) = state.lease.as_ref() {
            lease.release().map_err(|error| error.to_string())
        } else {
            Ok(())
        };
        sync_result?;
        release_result.map_err(|error| PiError::other(format!("release session lease: {error}")))
    }

    /// Repairs trailing tool calls according to their durable operation facts.
    /// A terminal fact is always persisted before its synthetic result.
    ///
    /// When every pending call is replayable (see
    /// [`Self::all_replayable`]), nothing is settled or answered and the calls
    /// are returned as the third element instead of the second.
    pub(super) fn repair_dangling_tool_calls(
        &self,
        replayable: fn(&str) -> bool,
    ) -> Result<RepairOutcome, PiError> {
        let pending = pending_tool_calls(&self.messages())?;
        let repaired = unanswered_calls_from(&pending);
        if self.all_replayable(&pending, replayable)? {
            return Ok((Vec::new(), Vec::new(), repaired));
        }
        let legacy_stand_ins = missing_tool_results(&pending);
        let mut warnings = Vec::new();
        for (index, call) in pending.into_iter().enumerate() {
            let record = self
                .lock()?
                .operation_ledger
                .operation_for_tool_call(&call.tool_call_id)
                .cloned();
            let block = match record {
                Some(record) => {
                    if record.corrupt {
                        warnings.push(Warning::new(format!(
                            "operation history for tool call {} is corrupt; effects are unknown",
                            call.tool_call_id
                        )));
                    }
                    let operation_id = record.operation_id.clone();
                    let outcome = if record.corrupt {
                        OperationOutcome {
                            disposition: OperationDisposition::Interrupted,
                            effect_certainty: EffectCertainty::Unknown,
                            stop_reason: Some(OperationStopReason::ProcessLost),
                        }
                    } else {
                        record.terminal.clone().unwrap_or(OperationOutcome {
                            disposition: OperationDisposition::Interrupted,
                            effect_certainty: EffectCertainty::Unknown,
                            stop_reason: Some(OperationStopReason::ProcessLost),
                        })
                    };
                    if record.terminal.is_none() {
                        let terminal = OperationFact::terminal(
                            operation_id.clone(),
                            record.attempts,
                            record.tool_call_id,
                            record.tool_name,
                            outcome.clone(),
                        );
                        if let Err(error) = self.append_operation_fact(terminal) {
                            if !record.corrupt {
                                return Err(error.context(format!(
                                    "repair operation for dangling tool call {:?}",
                                    call.tool_call_id
                                )));
                            }
                            warnings.push(Warning::new(format!(
                                "could not settle corrupt operation for tool call {}; effects are unknown",
                                call.tool_call_id
                            )));
                        }
                    }
                    Block {
                        block_type: otto_core::model::BlockType::ToolResult,
                        text: OPERATION_RESULT_UNAVAILABLE_TEXT.into(),
                        tool_call_id: call.tool_call_id.clone(),
                        tool_name: call.tool_name.clone(),
                        is_error: true,
                        operation_metadata: Some(ToolResultMetadata {
                            operation_id: Some(operation_id),
                            disposition: outcome.disposition,
                            effect_certainty: outcome.effect_certainty,
                            stop_reason: outcome.stop_reason,
                        }),
                        ..Block::default()
                    }
                }
                None => legacy_stand_ins[index].clone(),
            };
            let message = Message {
                role: Role::Tool,
                created_at: Utc::now(),
                blocks: vec![block],
                ..Message::default()
            };
            self.append_message(&message).map_err(|error| {
                error.context(format!("repair dangling tool call {:?}", call.tool_call_id))
            })?;
            warnings.push(Warning::new(format!(
                "repaired dangling tool call {}",
                call.tool_call_id
            )));
        }
        Ok((warnings, repaired, Vec::new()))
    }

    /// Whether the agent may run every call in `pending` again: there is at
    /// least one, each names a replayable tool, and each has at most one
    /// recorded attempt with no terminal fact and no corrupt history. All or
    /// nothing, so tool results keep call order and the calls with results
    /// stay a prefix of the message's calls. The attempt bound makes a call
    /// that killed its process once fall back to the interrupted repair the
    /// next time instead of looping.
    pub(super) fn all_replayable(
        &self,
        pending: &[Block],
        replayable: fn(&str) -> bool,
    ) -> Result<bool, PiError> {
        if pending.is_empty() {
            return Ok(false);
        }
        let state = self.lock()?;
        Ok(pending.iter().all(|call| {
            !call.arguments.as_ref().is_some_and(|arguments| {
                otto_core::safetext::contains_redaction_marker(arguments.get())
            }) && replay_allowed(
                state
                    .operation_ledger
                    .operation_for_tool_call(&call.tool_call_id),
                replayable(&call.tool_name),
            )
        }))
    }
}
