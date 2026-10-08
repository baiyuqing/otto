//! Content-free execution observations. Native hosts supply storage and clocks;
//! this interface never receives prompts, stream text, tool arguments or errors.
use crate::model::{OperationOutcome, Usage};
use crate::provider::{ProviderError, ProviderSettlement, StreamEvent};
use serde::Serialize;
use std::sync::Arc;

#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Turn,
    Provider,
    Compaction,
    RemoteCompaction,
    OneShot,
    Tool,
}
#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Starting,
    Active,
    ToolOperation,
    Sending,
    Headers,
    Receiving,
    StreamingText,
    StreamingReasoning,
    StreamingTool,
    RetryWait,
}
#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorClass {
    Provider,
    ContextOverflow,
    Deadline,
    Cancelled,
    Tool,
    Agent,
}

#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RetryReason {
    Transport,
    Timeout,
    Http,
    ServerOverloaded,
    ServerError,
    Other,
}

pub fn status(outcome: &OperationOutcome) -> &'static str {
    use crate::model::OperationDisposition::*;
    match outcome.disposition {
        Succeeded => "ok",
        Error => "error",
        Cancelled => "cancelled",
        DeadlineExceeded => "deadline",
        Interrupted => "interrupted",
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct Finish {
    pub status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub outcome: Option<OperationOutcome>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attempts: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cached_input_tokens_present: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_class: Option<ErrorClass>,
}

/// IDs and tool names are metadata; the host still validates/redacts them.
pub trait Diagnostics: Send + Sync {
    fn start(
        &self,
        kind: Kind,
        operation_id: &str,
        tool: Option<&str>,
        provider: &str,
        model: &str,
    ) -> Arc<dyn Observer>;
    fn for_task(&self, _task_id: &str) -> Option<Arc<dyn Diagnostics>> {
        None
    }
}

/// All arguments are typed metadata. HTTP adapters may report headers and byte
/// arrival without changing model-visible stream events.
pub trait Observer: Send + Sync {
    fn activity(&self, _phase: Phase, _bytes: usize, _http_status: Option<u16>) {}
    fn retry(&self, _attempt: u32, _delay_ms: u64, _reason: RetryReason) {}
    fn finish(&self, _finish: Finish) {}
    fn abandoned(&self) {}
}
pub struct Noop;
impl Observer for Noop {}

pub struct Observation {
    observer: Option<Arc<dyn Observer>>,
    finished: bool,
}
impl Observation {
    pub fn start(
        sink: Option<&Arc<dyn Diagnostics>>,
        kind: Kind,
        id: &str,
        tool: Option<&str>,
        provider: &str,
        model: &str,
    ) -> Self {
        Self {
            observer: sink.map(|sink| sink.start(kind, id, tool, provider, model)),
            finished: false,
        }
    }
    pub fn observer(&self) -> &dyn Observer {
        self.observer.as_deref().unwrap_or(&Noop)
    }
    pub fn stream(&self, event: &StreamEvent) {
        match event {
            StreamEvent::TextDelta { text } => {
                self.observer()
                    .activity(Phase::StreamingText, text.len(), None)
            }
            StreamEvent::ReasoningDelta { text } => {
                self.observer()
                    .activity(Phase::StreamingReasoning, text.len(), None)
            }
            StreamEvent::ToolCallDelta { arguments, .. } => {
                self.observer()
                    .activity(Phase::StreamingTool, arguments.len(), None)
            }
            StreamEvent::Retry {
                attempt,
                delay,
                reason,
                ..
            } => self.observer().retry(
                *attempt,
                u64::try_from(delay.as_millis()).unwrap_or(u64::MAX),
                match reason.as_str() {
                    "connection interrupted" | "request could not be sent" => {
                        RetryReason::Transport
                    }
                    "request timeout" => RetryReason::Timeout,
                    "transient HTTP response" => RetryReason::Http,
                    "server_is_overloaded" => RetryReason::ServerOverloaded,
                    "server_error" => RetryReason::ServerError,
                    _ => RetryReason::Other,
                },
            ),
        }
    }
    pub fn finish(&mut self, finish: Finish) {
        self.observer().finish(finish);
        self.finished = true;
    }
    pub fn provider_finished(&mut self, result: &ProviderSettlement) {
        self.finish(Finish {
            status: status(&result.outcome),
            outcome: Some(result.outcome.clone()),
            attempts: Some(result.attempts),
            usage: result
                .result
                .as_ref()
                .ok()
                .and_then(|response| response.message.usage),
            cached_input_tokens_present: result.result.as_ref().ok().and_then(|response| {
                response
                    .message
                    .usage
                    .map(|_| response.cached_input_tokens_present)
            }),
            error_class: result.result.as_ref().err().map(|error| match error {
                ProviderError::Overflow(_) => ErrorClass::ContextOverflow,
                ProviderError::DeadlineExceeded => ErrorClass::Deadline,
                ProviderError::Cancelled => ErrorClass::Cancelled,
                ProviderError::Other(_) => ErrorClass::Provider,
            }),
        });
    }
}
impl Drop for Observation {
    fn drop(&mut self) {
        if !self.finished {
            self.observer().abandoned();
        }
    }
}
