//! Run-loop tests: the turn loop, overflow, compaction, memory context, and
//! tasks.

use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::Poll;

use serde_json::value::RawValue;
use tokio_util::sync::CancellationToken;

use crate::model::{Block, BlockType, FinishReason, Message, Role, ToolDefinition, Usage};
use crate::operation::OperationControl;
use crate::provider::{
    ContextOverflowError, Provider, ProviderCapabilities, ProviderError, ProviderSettlement,
    RemoteCompactionSupport, Request, RequestSizer, Response, StreamEvent, StreamSink,
};
use crate::session::{MemorySession, RemoteCompaction, Session};
use crate::tool::{ToolExecutor, ToolResult};

use super::context_report::SectionKind;
use super::inbox::{Inbox, Notification, NotificationKind};
use super::memory::{
    MemoryError, MemoryRecall, RecallRequest, RecallResult, Record, RecordLifecycle, RecordStatus,
    Scope,
};
use super::progress::{ProgressPolicy, StallKind};
use super::redactor::Redactor;
use super::summary::SUMMARIZATION_SYSTEM_PROMPT;
use super::summary_validate::REQUIRED_SUMMARY_HEADINGS;
use super::{Agent, AgentError, CompactionSettings, Event, Options};

#[derive(Default)]
struct CapturingDiagnostics {
    models: Arc<Mutex<Vec<String>>>,
}

impl crate::diagnostics::Diagnostics for CapturingDiagnostics {
    fn start(
        &self,
        _kind: crate::diagnostics::Kind,
        _operation_id: &str,
        _tool: Option<&str>,
        _provider: &str,
        model: &str,
    ) -> Arc<dyn crate::diagnostics::Observer> {
        self.models.lock().expect("diagnostics").push(model.into());
        Arc::new(crate::diagnostics::Noop)
    }
}

mod auto_compaction;
mod capability_plan;
mod claude_cache_prefix;
mod context_oneshot;
mod context_view;
mod context_view_images;
mod durable_boundaries;
mod input_delivery;
mod input_provenance;
mod manual_compaction;
mod memory_compaction;
mod memory_context;
mod memory_observation;
mod memory_references;
mod memory_retraction;
mod memory_summary;
mod progress_recovery;
mod provider_tools;
mod recording;
mod redaction;
mod redaction_feedback;
mod step_interrupt;
mod turn_end_turn;
mod turn_records;
mod yield_provider_calls;

use auto_compaction::{seeded_session, seeded_session_with_echo_capabilities, tight_compaction};
use context_oneshot::{CompactionHooks, text_request};
use memory_context::one_record;
use provider_tools::{CancellingExecutor, two_tool_calls};
use turn_records::TurnRecordingSession;

// -- harness ---------------------------------------------------------------

fn raw(json: &str) -> Box<RawValue> {
    RawValue::from_string(json.to_owned()).expect("valid JSON")
}

fn clock() -> chrono::DateTime<chrono::Utc> {
    chrono::DateTime::from_timestamp(1_700_000_000, 0).expect("in range")
}

fn options() -> Options {
    let counter = AtomicUsize::new(0);
    Options {
        model: "test-model".into(),
        provider_name: "fake".into(),
        system_prompt: "be brief".into(),
        now: Box::new(clock),
        new_id: Box::new(move || format!("id-{}", counter.fetch_add(1, Ordering::SeqCst))),
        request_sizer: Some(Arc::new(JsonSizer)),
        ..Options::default()
    }
}

/// One scripted provider turn.
struct Turn {
    events: Vec<StreamEvent>,
    outcome: Result<Response, ProviderError>,
}

impl Turn {
    fn text(text: &str) -> Self {
        Self {
            events: vec![StreamEvent::TextDelta { text: text.into() }],
            outcome: Ok(Response {
                message: Message {
                    role: Role::Assistant,
                    finish_reason: Some(FinishReason::Stop),
                    blocks: vec![Block::text(text)],
                    ..Message::default()
                },
                ..Response::default()
            }),
        }
    }

    fn tool_call(call_id: &str, arguments: &str) -> Self {
        Self::named_tool_call("echo", call_id, arguments)
    }

    /// A tool call to `name`, so a test can script a call the executor
    /// reports as an unknown tool.
    fn named_tool_call(name: &str, call_id: &str, arguments: &str) -> Self {
        Self {
            events: Vec::new(),
            outcome: Ok(Response {
                message: Message {
                    role: Role::Assistant,
                    finish_reason: Some(FinishReason::ToolCalls),
                    blocks: vec![Block {
                        block_type: BlockType::ToolCall,
                        tool_call_id: call_id.into(),
                        tool_name: name.into(),
                        arguments: Some(raw(arguments)),
                        ..Block::default()
                    }],
                    ..Message::default()
                },
                ..Response::default()
            }),
        }
    }

    fn summary(text: &str) -> Self {
        Self {
            events: Vec::new(),
            outcome: Ok(Response {
                message: Message {
                    role: Role::Assistant,
                    finish_reason: Some(FinishReason::Stop),
                    blocks: vec![Block::text(text)],
                    usage: Some(Usage {
                        input_tokens: 5,
                        output_tokens: 7,
                        cached_input_tokens: 0,
                        ..Default::default()
                    }),
                    ..Message::default()
                },
                ..Response::default()
            }),
        }
    }

    fn remote_compaction(id: &str, encrypted_content: &str) -> Self {
        Self {
            events: Vec::new(),
            outcome: Ok(Response {
                message: Message {
                    role: Role::Assistant,
                    usage: Some(Usage {
                        input_tokens: 50,
                        output_tokens: 2,
                        cached_input_tokens: 20,
                        ..Default::default()
                    }),
                    ..Message::default()
                },
                cached_input_tokens_present: true,
                remote_compaction: Some(RemoteCompaction {
                    provider: "fake".into(),
                    id: id.into(),
                    encrypted_content: encrypted_content.into(),
                }),
            }),
        }
    }

    fn overflow() -> Self {
        Self {
            events: Vec::new(),
            outcome: Err(ProviderError::Overflow(ContextOverflowError::default())),
        }
    }

    fn failure(message: &str) -> Self {
        Self {
            events: Vec::new(),
            outcome: Err(ProviderError::Other(message.into())),
        }
    }

    fn with_events(mut self, events: Vec<StreamEvent>) -> Self {
        self.events = events;
        self
    }
}

/// Replays scripted turns and records every request it was given.
struct FakeProvider {
    turns: Mutex<VecDeque<Turn>>,
    requests: Mutex<Vec<Request>>,
    remote_compaction: bool,
}

impl FakeProvider {
    fn new(turns: Vec<Turn>) -> Self {
        Self {
            turns: Mutex::new(turns.into()),
            requests: Mutex::new(Vec::new()),
            remote_compaction: false,
        }
    }

    fn with_remote_compaction(mut self) -> Self {
        self.remote_compaction = true;
        self
    }

    fn requests(&self) -> Vec<Request> {
        self.requests.lock().expect("requests").clone()
    }

    /// The requests that are ordinary turns, not summary calls. A summary
    /// request carries no tool definitions and one user message.
    fn normal_requests(&self) -> Vec<Request> {
        self.requests()
            .into_iter()
            .filter(|request| {
                !request
                    .system_prompt
                    .starts_with(SUMMARIZATION_SYSTEM_PROMPT)
            })
            .collect()
    }

    fn summary_requests(&self) -> Vec<Request> {
        self.requests()
            .into_iter()
            .filter(|request| {
                request
                    .system_prompt
                    .starts_with(SUMMARIZATION_SYSTEM_PROMPT)
            })
            .collect()
    }
}

#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
impl Provider for FakeProvider {
    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            remote_compaction: if self.remote_compaction {
                RemoteCompactionSupport::ResponsesV2
            } else {
                RemoteCompactionSupport::Unsupported
            },
        }
    }

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
        let Some(turn) = self.turns.lock().expect("turns").pop_front() else {
            return ProviderSettlement::failed(
                ProviderError::Other("no scripted turn left".into()),
                0,
                crate::model::EffectCertainty::NotStarted,
            );
        };
        // Every scripted retry event starts one more attempt.
        let attempts = 1 + u32::try_from(
            turn.events
                .iter()
                .filter(|event| matches!(event, StreamEvent::Retry { .. }))
                .count(),
        )
        .expect("small count");
        for event in turn.events {
            emit(event);
        }
        match turn.outcome {
            Ok(response) => ProviderSettlement::succeeded(response, attempts),
            Err(error) => ProviderSettlement::failed(
                error,
                attempts,
                crate::model::EffectCertainty::Completed,
            ),
        }
    }
}

/// The size a request serializes to, used by the summary-request bound.
struct JsonSizer;

impl RequestSizer for JsonSizer {
    fn serialized_request_size(&self, request: &Request) -> Result<usize, ProviderError> {
        let mut total = request.system_prompt.len() + request.model.len();
        for message in &request.messages {
            total += message.text().len();
        }
        Ok(total)
    }
}

/// Returns its arguments, or the configured override.
#[derive(Default)]
struct EchoExecutor {
    persisted: Option<String>,
    content: Option<String>,
}

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
            content: self
                .content
                .clone()
                .unwrap_or_else(|| call.arguments.get().to_owned()),
            persisted_content: self.persisted.clone(),
            is_error: false,
            outcome_override: None,
        })
    }
}

/// A memory binding that returns a configurable result, counts its calls and
/// answers lifecycle queries from a table a test can change between calls.
struct FakeMemory {
    records: Mutex<Vec<Record>>,
    error: Option<String>,
    calls: AtomicUsize,
    close_error: Option<String>,
    /// Lifecycle answers that replace the default for an id.
    lifecycle_overrides: Mutex<BTreeMap<String, RecordLifecycle>>,
    lifecycle_error: Option<String>,
    lifecycle_calls: AtomicUsize,
}

impl FakeMemory {
    fn with_records(records: Vec<Record>) -> Self {
        Self {
            records: Mutex::new(records),
            error: None,
            calls: AtomicUsize::new(0),
            close_error: None,
            lifecycle_overrides: Mutex::new(BTreeMap::new()),
            lifecycle_error: None,
            lifecycle_calls: AtomicUsize::new(0),
        }
    }

    fn failing(message: &str) -> Self {
        Self {
            error: Some(message.into()),
            ..Self::with_records(Vec::new())
        }
    }

    fn failing_lifecycle(records: Vec<Record>, message: &str) -> Self {
        Self {
            lifecycle_error: Some(message.into()),
            ..Self::with_records(records)
        }
    }

    /// Replaces what the next recall returns.
    fn set_records(&self, records: Vec<Record>) {
        *self.records.lock().expect("records") = records;
    }

    /// Reports `id` with `status` and no revision from now on.
    fn set_status(&self, id: &str, status: RecordStatus) {
        self.set_status_at(id, status, None);
    }

    /// Reports `id` with `status` at `revision` from now on.
    fn set_status_at(&self, id: &str, status: RecordStatus, revision: Option<&str>) {
        self.lifecycle_overrides.lock().expect("lifecycle").insert(
            id.into(),
            RecordLifecycle {
                id: id.into(),
                revision: revision.map(str::to_owned),
                status,
            },
        );
    }

    fn lifecycle_call_count(&self) -> usize {
        self.lifecycle_calls.load(Ordering::SeqCst)
    }
}

#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
impl MemoryRecall for FakeMemory {
    async fn recall(
        &self,
        _request: &RecallRequest,
        _cancel: &CancellationToken,
    ) -> Result<RecallResult, MemoryError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        match &self.error {
            Some(message) => Err(MemoryError(message.clone())),
            None => Ok(RecallResult {
                records: self.records.lock().expect("records").clone(),
                used_tokens: 1,
            }),
        }
    }

    async fn lifecycle(
        &self,
        ids: &[String],
        _cancel: &CancellationToken,
    ) -> Result<Vec<RecordLifecycle>, MemoryError> {
        self.lifecycle_calls.fetch_add(1, Ordering::SeqCst);
        if let Some(message) = &self.lifecycle_error {
            return Err(MemoryError(message.clone()));
        }
        let overrides = self.lifecycle_overrides.lock().expect("lifecycle");
        let records = self.records.lock().expect("records");
        Ok(ids
            .iter()
            .map(|id| {
                if let Some(answer) = overrides.get(id) {
                    return answer.clone();
                }
                match records.iter().find(|record| &record.id == id) {
                    Some(record) => RecordLifecycle {
                        id: id.clone(),
                        revision: record.revision.clone(),
                        status: RecordStatus::Active,
                    },
                    None => RecordLifecycle {
                        id: id.clone(),
                        revision: None,
                        status: RecordStatus::Unknown,
                    },
                }
            })
            .collect())
    }

    fn close(&self) -> Result<(), MemoryError> {
        match &self.close_error {
            Some(message) => Err(MemoryError(message.clone())),
            None => Ok(()),
        }
    }
}

/// A structured summary that passes validation.
fn structured_summary() -> String {
    REQUIRED_SUMMARY_HEADINGS
        .iter()
        .map(|heading| format!("{heading}\nnote\n"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn collect(events: &Arc<Mutex<Vec<Event>>>) -> Vec<Event> {
    events.lock().expect("events").clone()
}

fn names(events: &[Event]) -> Vec<&'static str> {
    events.iter().map(Event::name).collect()
}
