//! Builds the `otto.provider_call` record for one settled provider operation.

use std::time::Duration;

use chrono::{DateTime, Utc};

use crate::model::{OperationId, Role};
use crate::provider::{ProviderSettlement, Request, StreamEvent};
use crate::session::{
    PROVIDER_CALL_CUSTOM_TYPE, PROVIDER_CALL_SCHEMA_VERSION, ProviderCallKind, ProviderCallRecord,
    ProviderRetryRecord, RequestEstimate, Session,
};

use super::context_estimate::estimate_message;
use super::context_report::{SectionKind, sections};
use super::redactor::Redactor;
use super::{Agent, AgentError, ProviderOperationKind};
use crate::provider::Provider;
use crate::tool::ToolExecutor;

/// Timing facts one operation collects from its stream events.
pub(super) struct CallTracker {
    started: DateTime<Utc>,
    first_output_ms: Option<u64>,
    retries: Vec<ProviderRetryRecord>,
}

impl CallTracker {
    pub(super) fn new(started: DateTime<Utc>) -> Self {
        Self {
            started,
            first_output_ms: None,
            retries: Vec::new(),
        }
    }

    /// Records the first output time, and a retry with its redacted reason.
    /// A retry clears the first output time so that it describes the attempt
    /// that settled.
    pub(super) fn observe(&mut self, event: &StreamEvent, now: DateTime<Utc>, redactor: &Redactor) {
        let elapsed = millis((now - self.started).to_std().unwrap_or_default());
        match event {
            StreamEvent::ReasoningDelta { .. }
            | StreamEvent::TextDelta { .. }
            | StreamEvent::ToolCallDelta { .. } => {
                self.first_output_ms.get_or_insert(elapsed);
            }
            StreamEvent::Retry {
                attempt,
                max_attempts,
                delay,
                reason,
                interrupted_output,
            } => {
                self.first_output_ms = None;
                self.retries.push(ProviderRetryRecord {
                    attempt: *attempt,
                    max_attempts: *max_attempts,
                    delay_ms: millis(*delay),
                    reason: redactor.redact_string(reason),
                    interrupted_output: *interrupted_output,
                    at_ms: elapsed,
                });
            }
        }
    }

    pub(super) fn retries(&self) -> &[ProviderRetryRecord] {
        &self.retries
    }
}

fn millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

/// The memory a request carries: the rendered text and the recalled record ids.
#[derive(Clone, Copy, Default)]
pub(super) struct RequestMemory<'a> {
    pub text: &'a str,
    pub record_ids: &'a [String],
}

/// Estimates the parts of `request`. The memory message is already one of the
/// request's messages, so it is moved from the message estimate to the memory
/// estimate.
fn request_estimate(request: &Request, memory: RequestMemory<'_>) -> RequestEstimate {
    let mut estimate = RequestEstimate {
        memory_record_ids: memory.record_ids.to_vec(),
        ..RequestEstimate::default()
    };
    for section in sections(request, &[], "") {
        let total = match section.kind {
            SectionKind::SystemPrompt => &mut estimate.system_prompt_tokens,
            SectionKind::Tools | SectionKind::McpTools => &mut estimate.tool_tokens,
            SectionKind::Messages | SectionKind::CompactionSummary => &mut estimate.message_tokens,
            SectionKind::Memory => &mut estimate.memory_tokens,
        };
        *total = total.saturating_add(section.tokens);
    }
    if !memory.text.is_empty()
        && let Some(message) = request
            .messages
            .iter()
            .find(|message| message.role == Role::User && message.text() == memory.text)
    {
        let tokens = estimate_message(message);
        estimate.message_tokens = estimate.message_tokens.saturating_sub(tokens);
        estimate.memory_tokens = estimate.memory_tokens.saturating_add(tokens);
    }
    estimate
}

/// What one settled provider operation tells the record.
pub(super) struct SettledCall<'a> {
    pub operation_id: &'a OperationId,
    pub kind: ProviderOperationKind,
    pub tracker: CallTracker,
    pub duration: Duration,
    pub settlement: &'a ProviderSettlement,
    pub request: &'a Request,
    pub memory: RequestMemory<'a>,
}

impl<P: Provider, T: ToolExecutor, S: Session> Agent<P, T, S> {
    /// Appends the `otto.provider_call` entry for a settled operation.
    pub(super) async fn append_provider_call(
        &self,
        call: SettledCall<'_>,
    ) -> Result<(), AgentError> {
        let message = call.settlement.result.as_ref().ok().map(|r| &r.message);
        let record = ProviderCallRecord {
            schema_version: PROVIDER_CALL_SCHEMA_VERSION,
            operation_id: call.operation_id.as_str().to_owned(),
            kind: match call.kind {
                ProviderOperationKind::Ordinary => ProviderCallKind::Ordinary,
                ProviderOperationKind::LocalCompaction => ProviderCallKind::LocalCompaction,
                ProviderOperationKind::RemoteCompaction => ProviderCallKind::RemoteCompaction,
            },
            started_at: call.tracker.started.timestamp_millis(),
            duration_ms: millis(call.duration),
            first_output_ms: call.tracker.first_output_ms,
            attempts: call.settlement.attempts,
            retries: call.tracker.retries,
            outcome: call.settlement.outcome.clone(),
            finish_reason: message.and_then(|message| message.finish_reason.clone()),
            usage: message.and_then(|message| message.usage),
            request: Some(request_estimate(call.request, call.memory)),
        };
        let data = serde_json::to_string(&record).expect("provider call record serializes");
        self.session
            .append_custom(PROVIDER_CALL_CUSTOM_TYPE, &data)
            .await
            .map_err(|source| AgentError::Persist {
                kind: "provider call".into(),
                source,
            })
    }
}
