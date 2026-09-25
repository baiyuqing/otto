//! The neutral provider contract.
//!
//! A [`Provider`] implementation may be shared by a parent agent and its
//! sub-agents, so `complete` takes `&self` and must be safe to call
//! concurrently.
//!
//! Ownership: the request is borrowed read-only for the duration of the call
//! and must not be retained. The returned [`Response`] belongs to the caller.
//!
//! Concurrency and cancellation: the `emit` callback is called synchronously
//! and in order from inside `complete`, and must not be called after `complete`
//! returns. When the token is cancelled, an implementation stops the call and
//! returns [`ProviderError::Cancelled`].
//!
//! Errors: every failure is a [`ProviderError`]. A context-window rejection is
//! [`ProviderError::Overflow`] so the agent can distinguish it from a transport
//! failure.

use tokio_util::sync::CancellationToken;

use crate::model::{Message, ToolDefinition};

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
        cancel: &CancellationToken,
    ) -> Result<Response, ProviderError>;
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn provider_error_cancelled_text() {
        assert_eq!(
            ProviderError::Cancelled.to_string(),
            "provider call was cancelled"
        );
    }
}
