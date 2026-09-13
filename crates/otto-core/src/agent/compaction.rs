//! The compaction pipeline: select, summarize, checkpoint.
//!
//! Port of `internal/agent/compaction.go`. One compaction asks the provider
//! for a summary of the older part of the transcript, then appends a
//! checkpoint that replaces those messages with the summary.
//!
//! Ownership: the pipeline clones out of the session, redacts the clones, and
//! hands the session a checkpoint. Session messages are never mutated.
//!
//! Concurrency and cancellation: neither entry point locks. The caller
//! serializes `run` and `compact`, the way the Go composition root's mutex
//! does. Cancellation is checked before the selection, before the durable append,
//! and again after it. A cancellation seen after the append does not erase
//! the committed checkpoint or its completion event.
//!
//! Errors: [`AgentError::NothingToCompact`] means there was no safe prefix, a
//! successful no-op for a manual compaction.
//! [`AgentError::InvalidCompactionSummary`] covers every rejection of the
//! summary request or its response.
//! [`AgentError::CompactionBoundary`] covers a failure the agent does not own.

use crate::model::{BlockType, Message, Role, ToolDefinition, Usage};
use crate::provider::{Provider, Request};
use crate::session::{CompactionCheckpoint, CompactionDetails, CompactionMetadata, Session};
use crate::tool::ToolExecutor;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use tokio_util::sync::CancellationToken;

use super::compaction_select::{
    COMPACTION_SUMMARY_DISPLAY_PREFIX, CompactionSelection, select_compaction,
};
use super::context_estimate::estimate_request;
use super::summary::{
    SUMMARY_MAXIMUM_BYTES, SummaryRequest, TURN_SUMMARY_MAXIMUM_BYTES, build_summary_request,
};
use super::summary_details::append_compaction_file_blocks;
use super::summary_validate::{
    combine_summary, validate_structured_summary, validate_turn_summary,
};
use super::{
    Agent, AgentError, CompactionMode, CompactionPlan, CompactionReason, CompactionResult,
    CompactionSettings, Event, EventSink, Options,
};

impl<P: Provider, T: ToolExecutor, S: Session> Agent<P, T, S> {
    /// Creates a manual checkpoint.
    ///
    /// A transcript with no safe historic prefix is a successful no-op: the
    /// returned result has `noop` set and no error is reported.
    pub async fn compact(
        &self,
        focus: &str,
        emit: EventSink<'_>,
        cancel: &CancellationToken,
    ) -> Result<CompactionResult, AgentError> {
        match self
            .compact_locked(CompactionReason::Manual, focus, emit, cancel)
            .await
        {
            Ok(result) => Ok(result),
            Err(error) if error.is_nothing_to_compact() => {
                emit(Event::CompactionCompleted {
                    compaction: CompactionResult {
                        reason: CompactionReason::Manual,
                        noop: true,
                        ..CompactionResult::default()
                    },
                });
                Ok(CompactionResult {
                    noop: true,
                    ..CompactionResult::default()
                })
            }
            Err(error) => Err(self.fail(emit, error)),
        }
    }

    /// The pipeline without the manual no-op policy, so the run loop can reuse
    /// it for automatic compaction.
    pub(super) async fn compact_locked(
        &self,
        reason: CompactionReason,
        focus: &str,
        emit: EventSink<'_>,
        cancel: &CancellationToken,
    ) -> Result<CompactionResult, AgentError> {
        let automatic = reason != CompactionReason::Manual;
        emit(Event::CompactionStarted {
            compaction: CompactionResult {
                reason,
                automatic,
                ..CompactionResult::default()
            },
        });
        cancelled(cancel)?;
        if !self.redactor.allows_dynamic_content() {
            return Err(AgentError::NothingToCompact);
        }

        let messages = self.session.messages();
        let latest = self.session.latest_compaction();
        let tools = self.tools.definitions();
        let tokens_before = estimate_request(
            &self.plain_request(messages.clone(), tools.clone()),
            latest.as_ref(),
        );

        let selection = select_compaction(
            &messages,
            latest.as_ref(),
            self.options.compaction.keep_recent_tokens,
            compaction_retained_budget(&self.options.compaction),
        )
        .map_err(|error| {
            if error.is_nothing_to_compact() || matches!(error, AgentError::CurrentTurnTooLarge) {
                return error;
            }
            AgentError::CompactionBoundary {
                message: "select compaction context failed".into(),
                cause: error.to_string(),
            }
        })?;

        let prepared_selection = self.redact_compaction_selection(&selection);
        let previous_details = latest
            .as_ref()
            .map(|latest| self.redact_compaction_details(&latest.details))
            .unwrap_or_default();
        let (prepared, turn_prepared) = self
            .prepare_summary_requests(
                &prepared_selection,
                &self.redactor.redact_string(focus),
                &previous_details,
            )
            .map_err(AgentError::InvalidCompactionSummary)?;

        let structured = !prepared_selection.historical_source.is_empty()
            || (!prepared_selection.turn_prefix_source.is_empty()
                && prepared_selection.previous_summary.is_empty());
        let first_maximum_bytes = if structured {
            SUMMARY_MAXIMUM_BYTES
        } else {
            TURN_SUMMARY_MAXIMUM_BYTES
        };

        // The plan is settled before any provider call, so a frontend can show
        // it while the summary request runs.
        let mode = if turn_prepared.is_some() {
            CompactionMode::SplitTurn
        } else if structured {
            CompactionMode::Structured
        } else {
            CompactionMode::TurnPrefix
        };
        emit(Event::CompactionPlanned {
            plan: CompactionPlan {
                reason,
                automatic,
                tokens_before,
                estimated_tokens_after: estimate_compacted_context(
                    &self.options,
                    &tools,
                    "",
                    &selection.retained,
                ),
                summarized_messages: selection.historical_source.len()
                    + selection.turn_prefix_source.len(),
                retained_messages: selection.retained.len(),
                mode,
            },
        });

        let (generated, mut usage, mut usage_present) = self
            .execute_summary_request(
                &prepared.request,
                first_maximum_bytes,
                structured,
                emit,
                cancel,
            )
            .await?;
        let mut final_summary = generated.clone();
        if prepared_selection.historical_source.is_empty()
            && !prepared_selection.previous_summary.is_empty()
        {
            final_summary = combine_summary(&prepared_selection.previous_summary, &generated)
                .map_err(AgentError::InvalidCompactionSummary)?;
        }
        let mut details = prepared.details;

        if let Some(turn_prepared) = turn_prepared {
            let (turn, turn_usage, turn_usage_present) = self
                .execute_summary_request(
                    &turn_prepared.request,
                    TURN_SUMMARY_MAXIMUM_BYTES,
                    false,
                    emit,
                    cancel,
                )
                .await?;
            final_summary =
                combine_summary(&generated, &turn).map_err(AgentError::InvalidCompactionSummary)?;
            (usage, usage_present) =
                combine_compaction_usage(usage, usage_present, turn_usage, turn_usage_present);
            details = turn_prepared.details;
        }
        final_summary = append_compaction_file_blocks(&final_summary, &details)
            .map_err(AgentError::InvalidCompactionSummary)?;

        let mut result = CompactionResult {
            reason,
            tokens_before,
            estimated_tokens_after: estimate_compacted_context(
                &self.options,
                &tools,
                &final_summary,
                &selection.retained,
            ),
            automatic,
            usage,
            usage_present,
            ..CompactionResult::default()
        };
        let checkpoint = CompactionCheckpoint {
            summary: final_summary,
            first_kept_entry_id: selection.first_kept_id.clone(),
            tokens_before,
            usage: usage_present.then_some(usage),
            details,
            created_at: (self.options.now)(),
        };

        // The last cancellable point before the session's durable append.
        cancelled(cancel)?;
        let metadata = self
            .session
            .append_compaction(checkpoint)
            .await
            .map_err(|error| AgentError::CompactionBoundary {
                message: "persist compaction checkpoint failed".into(),
                cause: error.to_string(),
            })?;
        result.checkpoint_id = metadata.id;
        emit(Event::CompactionCompleted {
            compaction: result.clone(),
        });

        // A cancellation seen after the commit keeps the committed result and
        // its event, but still stops an automatic caller before it acts again.
        cancelled(cancel)?;
        Ok(result)
    }

    fn plain_request(&self, messages: Vec<Message>, tools: Vec<ToolDefinition>) -> Request {
        Request {
            model: self.options.model.clone(),
            system_prompt: self.options.system_prompt.clone(),
            thinking: self.options.thinking.clone(),
            messages,
            tools,
        }
    }

    fn redact_compaction_selection(&self, selection: &CompactionSelection) -> CompactionSelection {
        let redact_all = |messages: &[Message]| -> Vec<Message> {
            messages
                .iter()
                .map(|message| self.redact_message(message))
                .collect()
        };
        CompactionSelection {
            previous_summary: self.redactor.redact_string(&selection.previous_summary),
            historical_source: redact_all(&selection.historical_source),
            turn_prefix_source: redact_all(&selection.turn_prefix_source),
            retained: redact_all(&selection.retained),
            first_kept_id: selection.first_kept_id.clone(),
            split_turn: selection.split_turn,
        }
    }

    fn redact_compaction_details(&self, details: &CompactionDetails) -> CompactionDetails {
        let redact_all = |paths: &[String]| -> Vec<String> {
            paths
                .iter()
                .map(|path| self.redactor.redact_string(path))
                .collect()
        };
        CompactionDetails {
            read_files: redact_all(&details.read_files),
            modified_files: redact_all(&details.modified_files),
            omitted_read_files: details.omitted_read_files,
            omitted_modified_files: details.omitted_modified_files,
        }
    }

    /// Builds the summary request, plus a second one when the current turn was
    /// split and both halves have content.
    fn prepare_summary_requests(
        &self,
        selection: &CompactionSelection,
        focus: &str,
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
        let prepared =
            build_summary_request(&self.options, request_selection, focus, previous_details)?;
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
        let turn_prepared =
            build_summary_request(&self.options, &turn_selection, focus, &prepared.details)?;
        Ok((prepared, Some(turn_prepared)))
    }

    /// Runs one summary request under a child token, so a response that
    /// streams a tool call or runs past `maximum_bytes` is stopped without
    /// cancelling the caller's turn.
    async fn execute_summary_request(
        &self,
        request: &Request,
        maximum_bytes: usize,
        structured: bool,
        emit: EventSink<'_>,
        cancel: &CancellationToken,
    ) -> Result<(String, Usage, bool), AgentError> {
        cancelled(cancel)?;
        let child = cancel.child_token();
        let streamed_bytes = AtomicUsize::new(0);
        let invalid_stream = AtomicBool::new(false);

        let started = (self.options.now)();
        let outcome = {
            let mut on_stream = |event: crate::provider::StreamEvent| {
                if invalid_stream.load(Ordering::SeqCst) {
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
                .complete(request, &mut on_stream, &child)
                .await
        };
        let duration = ((self.options.now)() - started)
            .to_std()
            .unwrap_or_default();
        self.emit_provider_api_call(emit, duration, outcome.as_ref().err(), &child);

        cancelled(cancel)?;
        if invalid_stream.load(Ordering::SeqCst) {
            return Err(AgentError::InvalidCompactionSummary(
                "streamed response exceeded its bound or attempted a tool call".into(),
            ));
        }
        let response = outcome.map_err(|error| AgentError::CompactionBoundary {
            message: "compaction summary provider request failed".into(),
            cause: error.to_string(),
        })?;

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
        Ok((summary, message.usage.unwrap_or_default(), usage_present))
    }
}

/// Rejects a response that carries a non-text block, or whose text blocks add
/// up to more than `maximum_bytes`. It runs before redaction, on the bytes the
/// provider actually sent.
pub fn validate_raw_summary_response_bound(
    message: &Message,
    maximum_bytes: usize,
) -> Result<(), String> {
    let mut total = 0usize;
    for block in &message.blocks {
        if block.block_type != BlockType::Text {
            return Err("response contains a non-text block".into());
        }
        if block.text.len() > maximum_bytes.saturating_sub(total) {
            return Err("response exceeds its byte bound".into());
        }
        total += block.text.len();
    }
    Ok(())
}

/// Go's `validateProviderResponseMessage`, reused by both the run loop and the
/// summary path.
pub(super) fn validate_provider_response_message(message: &Message) -> Result<(), String> {
    if message.role != Role::Assistant {
        return Err("assistant role is required".into());
    }
    message.validate().map_err(|error| error.to_string())
}

/// Adds two usage totals, treating a missing side as absent rather than zero.
pub fn combine_compaction_usage(
    left: Usage,
    left_present: bool,
    right: Usage,
    right_present: bool,
) -> (Usage, bool) {
    if !left_present {
        return (right, right_present);
    }
    if !right_present {
        return (left, true);
    }
    (
        Usage {
            input_tokens: saturating_token_add(left.input_tokens, right.input_tokens),
            output_tokens: saturating_token_add(left.output_tokens, right.output_tokens),
            cached_input_tokens: saturating_token_add(
                left.cached_input_tokens,
                right.cached_input_tokens,
            ),
        },
        true,
    )
}

/// Adds two token counts. A negative operand yields zero and an overflow
/// yields the maximum, matching Go's `saturatingTokenAdd`.
pub fn saturating_token_add(left: i64, right: i64) -> i64 {
    if left < 0 || right < 0 {
        return 0;
    }
    left.saturating_add(right)
}

/// How many tokens the retained part of a compaction may occupy. Zero means
/// there is no ceiling.
pub fn compaction_retained_budget(settings: &CompactionSettings) -> i64 {
    if settings.hard_input_window <= 0 {
        return 0;
    }
    let reserve = settings.reserve_tokens.max(0);
    if reserve >= settings.hard_input_window {
        return 1;
    }
    settings.hard_input_window - reserve
}

/// Estimates what the transcript will cost after the checkpoint: the summary
/// as one synthetic context message, then the retained messages.
///
/// The estimate runs against an empty active checkpoint, which forces the
/// content fallback rather than trusting usage recorded on a retained
/// assistant message from the pre-compaction prompt.
pub fn estimate_compacted_context(
    options: &Options,
    tools: &[ToolDefinition],
    summary: &str,
    retained: &[Message],
) -> i64 {
    let mut messages = Vec::with_capacity(retained.len() + 1);
    messages.push(Message {
        role: Role::Context,
        context_type: "compaction".into(),
        display: true,
        blocks: vec![crate::model::Block::text(format!(
            "{COMPACTION_SUMMARY_DISPLAY_PREFIX}{summary}"
        ))],
        ..Message::default()
    });
    messages.extend_from_slice(retained);
    let request = Request {
        model: options.model.clone(),
        system_prompt: options.system_prompt.clone(),
        thinking: options.thinking.clone(),
        messages,
        tools: tools.to_vec(),
    };
    estimate_request(&request, Some(&CompactionMetadata::default()))
}

fn cancelled(cancel: &CancellationToken) -> Result<(), AgentError> {
    if cancel.is_cancelled() {
        return Err(AgentError::Provider(
            crate::provider::ProviderError::Cancelled,
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Block;

    #[test]
    fn the_retained_budget_leaves_one_token_when_the_reserve_swallows_the_window() {
        let budget = |hard, reserve| {
            compaction_retained_budget(&CompactionSettings {
                hard_input_window: hard,
                reserve_tokens: reserve,
                ..CompactionSettings::default()
            })
        };
        assert_eq!(budget(0, 100), 0);
        assert_eq!(budget(-1, 100), 0);
        assert_eq!(budget(100, 100), 1);
        assert_eq!(budget(100, 200), 1);
        assert_eq!(budget(100, 40), 60);
        assert_eq!(budget(100, -40), 100);
    }

    #[test]
    fn token_addition_saturates_and_floors() {
        assert_eq!(saturating_token_add(2, 3), 5);
        assert_eq!(saturating_token_add(-1, 3), 0);
        assert_eq!(saturating_token_add(2, -3), 0);
        assert_eq!(saturating_token_add(i64::MAX, 1), i64::MAX);
    }

    #[test]
    fn usage_is_combined_only_when_both_sides_reported_it() {
        let left = Usage {
            input_tokens: 1,
            output_tokens: 2,
            cached_input_tokens: 3,
        };
        let right = Usage {
            input_tokens: 10,
            output_tokens: 20,
            cached_input_tokens: 30,
        };
        assert_eq!(
            combine_compaction_usage(left, false, right, true),
            (right, true)
        );
        assert_eq!(
            combine_compaction_usage(left, true, right, false),
            (left, true)
        );
        assert_eq!(
            combine_compaction_usage(left, true, right, true),
            (
                Usage {
                    input_tokens: 11,
                    output_tokens: 22,
                    cached_input_tokens: 33,
                },
                true
            )
        );
        assert_eq!(
            combine_compaction_usage(left, false, right, false),
            (right, false)
        );
    }

    #[test]
    fn the_raw_response_bound_rejects_non_text_and_oversize() {
        let text = |value: &str| Message {
            blocks: vec![Block::text(value)],
            ..Message::default()
        };
        assert!(validate_raw_summary_response_bound(&text("abc"), 3).is_ok());
        assert_eq!(
            validate_raw_summary_response_bound(&text("abcd"), 3).unwrap_err(),
            "response exceeds its byte bound"
        );
        let two = Message {
            blocks: vec![Block::text("ab"), Block::text("cd")],
            ..Message::default()
        };
        assert_eq!(
            validate_raw_summary_response_bound(&two, 3).unwrap_err(),
            "response exceeds its byte bound"
        );
        assert!(validate_raw_summary_response_bound(&two, 4).is_ok());
        let call = Message {
            blocks: vec![Block {
                block_type: BlockType::ToolCall,
                tool_name: "echo".into(),
                tool_call_id: "c1".into(),
                ..Block::default()
            }],
            ..Message::default()
        };
        assert_eq!(
            validate_raw_summary_response_bound(&call, 100).unwrap_err(),
            "response contains a non-text block"
        );
    }

    #[test]
    fn the_compacted_estimate_counts_the_summary_and_the_retained_tail() {
        let options = Options::default();
        let retained = [Message {
            role: Role::User,
            blocks: vec![Block::text("kept")],
            ..Message::default()
        }];
        let empty = estimate_compacted_context(&options, &[], "", &retained);
        let with_summary = estimate_compacted_context(&options, &[], &"s".repeat(300), &retained);
        assert!(
            with_summary > empty + 90,
            "a 300-byte summary must add about 100 tokens: {empty} then {with_summary}"
        );
    }
}
