//! A tool-less, single-request text completion.
//!
//! Features that need one model answer outside the turn loop (reflection
//! today) call [`Agent::complete_text`], so they use the agent's own provider,
//! model, thinking setting, and redactor instead of duplicating them.
//!
//! Ownership: the call borrows the agent and returns owned text. It never
//! touches the session, so it appends nothing to history.
//!
//! Concurrency and cancellation: it does not lock. The caller serializes it
//! with turns and compactions. `control` cancels the provider request, and a
//! response that streams a non-text event or runs past `maximum_bytes` is
//! stopped under a child token without cancelling the caller's control.
//!
//! Errors: provider cancellation and deadlines keep their [`AgentError::Provider`]
//! form; every other rejection is [`AgentError::InvalidResponse`] or
//! [`AgentError::Other`] carrying redacted text.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use tokio_util::sync::CancellationToken;

use crate::model::{Block, BlockType, Message, OperationStopReason, Role, Usage};
use crate::operation::OperationControl;
use crate::provider::{Provider, ProviderError, Request, StreamEvent};
use crate::session::Session;
use crate::tool::ToolExecutor;

use super::{Agent, AgentError, EventSink};

/// What to ask. Both texts are sent as given after the user text is redacted;
/// the system prompt is fixed text and is not.
#[derive(Debug, Clone, Copy)]
pub struct TextRequest<'a> {
    pub system_prompt: &'a str,
    pub user_text: &'a str,
    /// The largest accepted response, in bytes of text.
    pub maximum_bytes: usize,
}

/// The accepted response.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TextResponse {
    pub text: String,
    pub usage: Usage,
    /// Whether the provider reported usage at all.
    pub usage_present: bool,
}

struct ChildControl<'a> {
    token: CancellationToken,
    parent: &'a dyn OperationControl,
}

impl OperationControl for ChildControl<'_> {
    fn cancellation_token(&self) -> &CancellationToken {
        &self.token
    }

    fn remaining(&self) -> Option<std::time::Duration> {
        self.parent.remaining()
    }

    fn stop_reason(&self) -> Option<OperationStopReason> {
        self.parent.stop_reason()
    }
}

impl<P: Provider, T: ToolExecutor, S: Session> Agent<P, T, S> {
    /// Whether this agent may send transcript-derived text to a provider at
    /// all. False when its redactor could not enumerate every secret.
    pub fn allows_dynamic_content(&self) -> bool {
        self.redactor.allows_dynamic_content()
    }

    /// Redacts `text` with the agent's redactor, so a caller that builds its
    /// own provider input sees exactly what the provider would.
    pub fn redact_text(&self, text: &str) -> String {
        self.redactor.redact_string(text)
    }

    /// Asks the provider once, with no tools, and returns the response text.
    pub async fn complete_text(
        &self,
        request: &TextRequest<'_>,
        emit: EventSink<'_>,
        control: &dyn OperationControl,
    ) -> Result<TextResponse, AgentError> {
        if !self.redactor.allows_dynamic_content() {
            return Err(AgentError::Other(
                "transcript text cannot be sent: the redaction boundary is closed".into(),
            ));
        }
        stopped(control)?;
        let provider_request = Request {
            model: self.options.model.clone(),
            system_prompt: request.system_prompt.to_owned(),
            thinking: self.options.thinking.clone(),
            messages: vec![Message {
                role: Role::User,
                blocks: vec![Block::text(self.redactor.redact_string(request.user_text))],
                ..Message::default()
            }],
            tools: Vec::new(),
        };

        let child = control.cancellation_token().child_token();
        let child_control = ChildControl {
            token: child.clone(),
            parent: control,
        };
        let streamed_bytes = AtomicUsize::new(0);
        let invalid_stream = AtomicBool::new(false);
        let operation_id = (self.options.new_operation_id)()
            .map_err(|message| AgentError::OperationIdentity { message })?;
        let started = (self.options.now)();
        let outcome = {
            let mut on_stream = |event: StreamEvent| {
                if invalid_stream.load(Ordering::SeqCst) {
                    return;
                }
                if let StreamEvent::Retry {
                    attempt,
                    max_attempts,
                    delay,
                    reason,
                } = event
                {
                    emit(super::Event::ProviderRetry {
                        operation_id: operation_id.clone(),
                        attempt,
                        max_attempts,
                        delay,
                        reason,
                    });
                    return;
                }
                let StreamEvent::TextDelta { text } = event else {
                    // Reasoning deltas are tolerated: they are not part of the
                    // answer and carry no text this call returns.
                    if matches!(event, StreamEvent::ReasoningDelta { .. }) {
                        return;
                    }
                    invalid_stream.store(true, Ordering::SeqCst);
                    child.cancel();
                    return;
                };
                let current = streamed_bytes.fetch_add(text.len(), Ordering::SeqCst) + text.len();
                if current > request.maximum_bytes {
                    invalid_stream.store(true, Ordering::SeqCst);
                    child.cancel();
                }
            };
            self.provider
                .complete(&provider_request, &mut on_stream, &child_control)
                .await
        };
        let duration = ((self.options.now)() - started)
            .to_std()
            .unwrap_or_default();
        self.emit_provider_api_call(emit, operation_id, duration, &outcome);

        stopped(control)?;
        if invalid_stream.load(Ordering::SeqCst) {
            return Err(AgentError::InvalidResponse(
                "streamed response exceeded its bound or attempted a tool call".into(),
            ));
        }
        let response = outcome.result.map_err(|error| match error {
            ProviderError::Cancelled | ProviderError::DeadlineExceeded => {
                AgentError::Provider(error)
            }
            other => AgentError::Other(self.redactor.redact_string(&other.to_string())),
        })?;

        let message = response.message;
        if message.role != Role::Assistant {
            return Err(AgentError::InvalidResponse(
                "response role is not assistant".into(),
            ));
        }
        message
            .validate()
            .map_err(|error| AgentError::InvalidResponse(error.to_string()))?;
        let mut text = String::new();
        for block in &message.blocks {
            match block.block_type {
                BlockType::Text => text.push_str(&block.text),
                BlockType::Reasoning => {}
                _ => {
                    return Err(AgentError::InvalidResponse(
                        "response contains a non-text block".into(),
                    ));
                }
            }
            if text.len() > request.maximum_bytes {
                return Err(AgentError::InvalidResponse(
                    "response exceeds its byte bound".into(),
                ));
            }
        }
        let usage_present = message.usage.is_some();
        Ok(TextResponse {
            text: self.redactor.redact_string(&text),
            usage: message.usage.unwrap_or_default(),
            usage_present,
        })
    }
}

fn stopped(control: &dyn OperationControl) -> Result<(), AgentError> {
    if let Some(error) = super::stopped_provider_error(control) {
        return Err(AgentError::Provider(error));
    }
    Ok(())
}
