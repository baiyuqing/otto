//! The neutral provider contract.
//!
//! A [`Provider`] implementation may be shared by a parent agent and its
//! sub-agents, so `complete` takes `&self` and must be safe to call
//! concurrently.
//!
//! Ownership: the request is borrowed read-only for the duration of the call
//! and must not be retained. The returned [`Response`] belongs to the caller.
//!
//! Concurrency and operation control: the `emit` callback is called
//! synchronously and in order from inside `complete`, and must not be called
//! after `complete` returns. Implementations observe the shared operation
//! control without resetting its budget. User cancellation returns
//! [`ProviderError::Cancelled`], while deadline exhaustion returns
//! [`ProviderError::DeadlineExceeded`].
//!
//! Errors: every failure is a [`ProviderError`]. A context-window rejection is
//! [`ProviderError::Overflow`] so the agent can distinguish it from a transport
//! failure.

use crate::model::{
    EffectCertainty, Message, OperationDisposition, OperationOutcome, OperationStopReason,
    ToolDefinition,
};
use crate::operation::OperationControl;

/// One completion request. Built fresh from session state for each provider
/// call; implementations translate it to their wire format.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Request {
    pub model: String,
    pub system_prompt: String,
    pub thinking: String,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolDefinition>,
}

/// One completion response.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Response {
    /// The single source of response finish and usage metadata.
    pub message: Message,
}

/// The result and content-free metadata for one logical provider operation.
#[derive(Debug)]
pub struct ProviderSettlement {
    pub result: Result<Response, ProviderError>,
    /// Requests whose send future began polling.
    pub attempts: u32,
    pub outcome: OperationOutcome,
}

impl ProviderSettlement {
    pub fn succeeded(response: Response, attempts: u32) -> Self {
        Self {
            result: Ok(response),
            attempts,
            outcome: OperationOutcome {
                disposition: OperationDisposition::Succeeded,
                effect_certainty: EffectCertainty::Completed,
                stop_reason: None,
            },
        }
    }

    pub fn failed(error: ProviderError, attempts: u32, effect_certainty: EffectCertainty) -> Self {
        let (disposition, stop_reason) = match error {
            ProviderError::DeadlineExceeded => (
                OperationDisposition::DeadlineExceeded,
                Some(OperationStopReason::Deadline),
            ),
            ProviderError::Cancelled => (
                OperationDisposition::Cancelled,
                Some(OperationStopReason::UserCancellation),
            ),
            _ => (OperationDisposition::Error, None),
        };
        Self {
            result: Err(error),
            attempts,
            outcome: OperationOutcome {
                disposition,
                effect_certainty,
                stop_reason,
            },
        }
    }

    pub fn transport_lost(error: ProviderError, attempts: u32) -> Self {
        Self::stopped(
            error,
            attempts,
            EffectCertainty::Unknown,
            OperationStopReason::TransportLost,
        )
    }

    pub fn stopped(
        error: ProviderError,
        attempts: u32,
        effect_certainty: EffectCertainty,
        reason: OperationStopReason,
    ) -> Self {
        Self {
            result: Err(error),
            attempts,
            outcome: OperationOutcome {
                disposition: match reason {
                    OperationStopReason::Deadline => OperationDisposition::DeadlineExceeded,
                    OperationStopReason::UserCancellation => OperationDisposition::Cancelled,
                    OperationStopReason::Shutdown
                    | OperationStopReason::Migration
                    | OperationStopReason::TransportLost
                    | OperationStopReason::ProcessLost => OperationDisposition::Interrupted,
                },
                effect_certainty,
                stop_reason: Some(reason),
            },
        }
    }
}

/// An incremental update observed while a response streams.
///
/// Text and reasoning deltas reach the frontend as they arrive. Tool-call
/// deltas are reported for progress display only: the authoritative tool
/// calls are the blocks of [`Response::message`]. The concatenated reasoning
/// deltas equal the text of the response's [`BlockType::Reasoning`] block.
///
/// [`BlockType::Reasoning`]: crate::model::BlockType::Reasoning
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreamEvent {
    TextDelta {
        text: String,
    },
    ReasoningDelta {
        text: String,
    },
    ToolCallDelta {
        tool_call_id: String,
        tool_name: String,
        arguments: String,
    },
    /// A failed attempt will be retried after `delay`. `attempt` is the
    /// 1-based number of the attempt about to start; `reason` is an HTTP
    /// status or a transport error class, never response body text.
    Retry {
        attempt: u32,
        max_attempts: u32,
        delay: std::time::Duration,
        reason: String,
    },
}

/// The provider rejected the request because it exceeds the context window.
///
/// Every field is optional detail: a provider that reports none still produces
/// the bare message.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ContextOverflowError {
    pub status: u16,
    pub code: String,
    pub current_tokens: i64,
    pub maximum_tokens: i64,
}

impl ContextOverflowError {
    /// The text shared by every context-overflow error, with no detail.
    pub const MESSAGE: &'static str = "context window exceeded";
}

impl std::fmt::Display for ContextOverflowError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut details: Vec<String> = Vec::with_capacity(3);
        if self.status > 0 {
            details.push(format!("HTTP {}", self.status));
        }
        if !self.code.is_empty() {
            details.push(format!("code {}", self.code));
        }
        if self.current_tokens > 0 && self.maximum_tokens > 0 {
            details.push(format!(
                "requested {} tokens, maximum {}",
                self.current_tokens, self.maximum_tokens
            ));
        }
        formatter.write_str(Self::MESSAGE)?;
        if details.is_empty() {
            return Ok(());
        }
        write!(formatter, " ({})", details.join(", "))
    }
}

impl std::error::Error for ContextOverflowError {}

/// Everything a provider call can fail with.
#[derive(Debug, thiserror::Error)]
pub enum ProviderError {
    /// The request does not fit in the model's context window.
    #[error(transparent)]
    Overflow(#[from] ContextOverflowError),
    /// The operation's deadline was exhausted.
    #[error("provider call deadline exceeded")]
    DeadlineExceeded,
    /// The call stopped because its cancellation token was cancelled.
    #[error("provider call was cancelled")]
    Cancelled,
    /// Any other failure: transport, decoding, or an unclassified API error.
    /// Implementations must redact credentials before constructing it.
    #[error("{0}")]
    Other(String),
}

/// Estimates the serialized size of a request, used by compaction to decide
/// when the transcript must shrink. Implemented by providers that know their
/// own wire format.
pub trait RequestSizer {
    /// Returns the byte size the request would occupy on the wire.
    fn serialized_request_size(&self, request: &Request) -> Result<usize, ProviderError>;
}

/// The stream-event callback a provider writes into.
///
/// Native futures must be `Send` so the agent loop can run on a
/// multi-threaded tokio runtime, which requires the callback to be `Send`
/// too. wasm futures are never `Send`, so the bound is dropped there.
#[cfg(not(target_arch = "wasm32"))]
pub type StreamSink<'a> = &'a mut (dyn FnMut(StreamEvent) + Send);
/// See the native definition above.
#[cfg(target_arch = "wasm32")]
pub type StreamSink<'a> = &'a mut dyn FnMut(StreamEvent);

/// A model backend.
///
/// See the module documentation for the ownership, concurrency, cancellation,
/// and error rules every implementation must follow.
#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
pub trait Provider {
    async fn complete(
        &self,
        request: &Request,
        emit: StreamSink<'_>,
        control: &dyn OperationControl,
    ) -> ProviderSettlement;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_settlements_are_valid_and_content_free() {
        let not_started = ProviderSettlement::failed(
            ProviderError::Other("local preflight".into()),
            0,
            EffectCertainty::NotStarted,
        );
        assert_eq!(not_started.attempts, 0);
        not_started
            .outcome
            .validate()
            .expect("valid preflight outcome");

        let completed = ProviderSettlement::succeeded(Response::default(), 1);
        assert_eq!(completed.attempts, 1);
        assert_eq!(
            completed.outcome.effect_certainty,
            EffectCertainty::Completed
        );
        completed.outcome.validate().expect("valid success outcome");

        let lost = ProviderSettlement::transport_lost(ProviderError::Other("transport".into()), 1);
        assert_eq!(lost.attempts, 1);
        assert_eq!(lost.outcome.disposition, OperationDisposition::Interrupted);
        assert_eq!(lost.outcome.effect_certainty, EffectCertainty::Unknown);
        assert_eq!(
            lost.outcome.stop_reason,
            Some(OperationStopReason::TransportLost)
        );
        lost.outcome.validate().expect("valid transport outcome");
    }

    #[test]
    fn context_overflow_error_display() {
        let cases: &[(ContextOverflowError, &str)] = &[
            (ContextOverflowError::default(), "context window exceeded"),
            (
                ContextOverflowError {
                    status: 400,
                    code: "context_length_exceeded".into(),
                    current_tokens: 100,
                    maximum_tokens: 50,
                },
                "context window exceeded (HTTP 400, code context_length_exceeded, requested 100 tokens, maximum 50)",
            ),
            (
                ContextOverflowError {
                    status: 429,
                    ..ContextOverflowError::default()
                },
                "context window exceeded (HTTP 429)",
            ),
            (
                ContextOverflowError {
                    code: "too_long".into(),
                    ..ContextOverflowError::default()
                },
                "context window exceeded (code too_long)",
            ),
            (
                ContextOverflowError {
                    current_tokens: 10,
                    maximum_tokens: 0,
                    ..ContextOverflowError::default()
                },
                "context window exceeded",
            ),
        ];
        for (error, want) in cases {
            assert_eq!(error.to_string(), *want);
        }
    }

    #[test]
    fn provider_error_overflow_carries_the_overflow_text() {
        let error = ProviderError::Overflow(ContextOverflowError {
            status: 400,
            ..ContextOverflowError::default()
        });
        assert_eq!(error.to_string(), "context window exceeded (HTTP 400)");
    }

    #[test]
    fn provider_error_deadline_text() {
        assert_eq!(
            ProviderError::DeadlineExceeded.to_string(),
            "provider call deadline exceeded"
        );
    }

    #[test]
    fn provider_error_cancelled_text() {
        assert_eq!(
            ProviderError::Cancelled.to_string(),
            "provider call was cancelled"
        );
    }
}
