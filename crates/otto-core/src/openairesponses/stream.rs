//! Server-sent-event assembler for a Responses API response stream.
//!
//! The assembler is push-based, so it depends on no I/O trait. The caller feeds response bytes to
//! [`StreamAssembler::push`] in whatever sizes the transport delivers and calls
//! [`StreamAssembler::finish`] at end of body. Line assembly spans pushes, so
//! the result does not depend on where the chunk boundaries fall.
//!
//! The Responses stream has no `[DONE]` sentinel: it ends with a
//! `response.completed` or `response.incomplete` event and then the body
//! closes.
//!
//! Ownership: the assembler owns all partial state; one instance serves one
//! response and is not shared. The `emit` callback is borrowed for the duration
//! of the call and is invoked synchronously and in arrival order.
//!
//! Errors: every failure is a [`StreamError`]; the decoder detail appended to
//! [`StreamError::Decode`] is `serde_json`'s message.

use std::collections::HashMap;

use serde_json::value::RawValue;

use crate::model::{Block, BlockType, Message, Role, Usage};
use crate::provider::{Response, StreamEvent};
use crate::session::RemoteCompaction;

use super::protocol::{WireError, WireEvent, finish_reason, valid_arguments};

/// Maximum bytes buffered across the `data:` fields of one SSE event.
const MAX_SSE_EVENT_BYTES: usize = 1024 * 1024;
/// Maximum bytes buffered for one SSE field line, excluding its line ending.
/// The Responses stream sends each event's JSON on one `data:` line, so the
/// line cap must not be lower than the event cap; it only bounds memory for
/// non-data lines.
const MAX_SSE_LINE_BYTES: usize = MAX_SSE_EVENT_BYTES + b"data: ".len();

/// Everything the assembler can reject.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum StreamError {
    /// A `data:` payload was not valid event JSON.
    #[error("decode responses stream: {0}")]
    Decode(String),
    /// One SSE field line exceeded [`MAX_SSE_LINE_BYTES`].
    #[error("responses stream line exceeds {limit} bytes")]
    LineTooLarge { limit: usize },
    /// One SSE event's accumulated `data:` fields exceeded
    /// [`MAX_SSE_EVENT_BYTES`].
    #[error("responses stream event exceeds {limit} bytes")]
    EventTooLarge { limit: usize },
    /// The backend sent a `response.failed` or an `error` event. The text is
    /// the event type followed by `: <code>: <message>` for the parts the
    /// event carried. `code` is the event's error code, empty when absent.
    #[error("responses stream reported {detail}")]
    Reported { detail: String, code: String },
    /// The backend reported a recognized error code without retaining arbitrary
    /// upstream text in the provider-neutral result.
    #[error("responses stream policy capacity unavailable")]
    PolicyCapacityUnavailable,
    /// The body ended before a terminal `response.completed` event.
    #[error("responses stream ended without response.completed")]
    MissingCompletion,
    /// The accumulated arguments of a tool call are not a complete JSON value.
    #[error("tool call {0:?} has malformed arguments")]
    MalformedArguments(String),
    /// The backend violated the remote-compaction response contract. Opaque
    /// encrypted content is deliberately excluded from this diagnostic.
    #[error("responses remote compaction protocol error: {0}")]
    RemoteCompactionProtocol(&'static str),
}

/// A tool call being assembled from the events that share its output item id.
#[derive(Debug, Default)]
struct AssembledToolCall {
    call_id: String,
    name: String,
    arguments: String,
}

/// Assembles one streamed response.
///
/// See the module documentation for the ownership, concurrency, and error
/// rules.
#[derive(Debug, Default)]
pub struct StreamAssembler {
    line: Vec<u8>,
    data_lines: Vec<Vec<u8>>,
    data_bytes: usize,
    event_name: String,
    text: String,
    reasoning: String,
    /// The `(item_id, summary_index)` of the last reasoning delta; a new pair
    /// starts a new summary part, separated from the previous one by a blank
    /// line.
    reasoning_part: Option<(String, i64)>,
    calls: Vec<AssembledToolCall>,
    by_item: HashMap<String, usize>,
    usage: Option<Usage>,
    cached_input_tokens_present: bool,
    status: String,
    incomplete_reason: String,
    request_remote_compaction: bool,
    remote_compaction_provider: String,
    remote_compaction: Option<RemoteCompaction>,
    emitted: bool,
    completed: bool,
}

impl StreamAssembler {
    /// A new assembler with no buffered input.
    pub fn new() -> Self {
        Self::default()
    }

    /// Creates an assembler with the request's remote-compaction expectation.
    /// A requested compaction must be returned exactly once; an unsolicited
    /// compaction is rejected when the response finishes.
    pub fn with_remote_compaction(
        request_remote_compaction: bool,
        remote_compaction_provider: impl Into<String>,
    ) -> Self {
        Self {
            request_remote_compaction,
            remote_compaction_provider: remote_compaction_provider.into(),
            ..Self::default()
        }
    }

    /// Consumes the next slice of response body.
    ///
    /// Complete lines are processed immediately; a trailing partial line is
    /// held until the next push or until [`Self::finish`]. Bytes that arrive
    /// after the terminal event are discarded.
    pub fn push(
        &mut self,
        bytes: &[u8],
        emit: &mut dyn FnMut(StreamEvent),
    ) -> Result<(), StreamError> {
        for &byte in bytes {
            if self.completed {
                return Ok(());
            }
            if byte != b'\n' {
                let terminal_cr = self.line.len() == MAX_SSE_LINE_BYTES && byte == b'\r';
                if self.line.len() >= MAX_SSE_LINE_BYTES && !terminal_cr {
                    return Err(StreamError::LineTooLarge {
                        limit: MAX_SSE_LINE_BYTES,
                    });
                }
                self.line.push(byte);
                continue;
            }
            let line = std::mem::take(&mut self.line);
            self.consume_line(&line, emit)?;
        }
        Ok(())
    }

    /// Ends the body and returns the assembled response.
    ///
    /// A trailing line without a newline and a pending event without a blank
    /// line are both dispatched first, which can emit further events, so the
    /// same callback is required here.
    pub fn finish(mut self, emit: &mut dyn FnMut(StreamEvent)) -> Result<Response, StreamError> {
        if !self.completed {
            if !self.line.is_empty() {
                let line = std::mem::take(&mut self.line);
                self.consume_line(&line, emit)?;
            }
            if !self.completed && !self.data_lines.is_empty() {
                self.dispatch(emit)?;
            }
        }
        if !self.completed {
            return Err(StreamError::MissingCompletion);
        }
        match (
            self.request_remote_compaction,
            self.remote_compaction.is_some(),
        ) {
            (true, false) => {
                return Err(StreamError::RemoteCompactionProtocol(
                    "requested compaction was not returned",
                ));
            }
            (false, true) => {
                return Err(StreamError::RemoteCompactionProtocol(
                    "compaction was returned without being requested",
                ));
            }
            _ => {}
        }

        let has_tool_calls = !self.calls.is_empty();
        let mut blocks = Vec::with_capacity(2 + self.calls.len());
        if !self.reasoning.is_empty() {
            blocks.push(Block::reasoning(self.reasoning));
        }
        if !self.text.is_empty() {
            blocks.push(Block::text(self.text));
        }
        for call in self.calls {
            if !valid_arguments(&call.arguments) {
                return Err(StreamError::MalformedArguments(call.call_id));
            }
            blocks.push(Block {
                block_type: BlockType::ToolCall,
                tool_call_id: call.call_id,
                tool_name: call.name,
                arguments: Some(
                    RawValue::from_string(call.arguments)
                        .expect("arguments were just validated as JSON"),
                ),
                ..Block::default()
            });
        }
        Ok(Response {
            message: Message {
                role: Role::Assistant,
                blocks,
                finish_reason: Some(finish_reason(
                    has_tool_calls,
                    &self.status,
                    &self.incomplete_reason,
                )),
                usage: self.usage,
                ..Message::default()
            },
            cached_input_tokens_present: self.cached_input_tokens_present,
            remote_compaction: self.remote_compaction,
        })
    }

    /// Reports whether any stream event has been emitted. The HTTP layer uses
    /// it to refuse a retry once the frontend has seen part of a response.
    pub fn emitted(&self) -> bool {
        self.emitted
    }

    /// Reports whether a terminal `response.completed` or
    /// `response.incomplete` event has been seen.
    pub fn is_done(&self) -> bool {
        self.completed
    }

    /// Applies one field line of the event stream. An `event:` name supplies
    /// the type only when the JSON payload has no non-empty `type`; a blank
    /// line ends the current event.
    fn consume_line(
        &mut self,
        line: &[u8],
        emit: &mut dyn FnMut(StreamEvent),
    ) -> Result<(), StreamError> {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if line.is_empty() {
            return self.dispatch(emit);
        }
        if line.starts_with(b":") {
            return Ok(());
        }
        if line == b"data" {
            self.push_data_line(&[])?;
            return Ok(());
        }
        if let Some(value) = line.strip_prefix(b"data:") {
            let value = value.strip_prefix(b" ").unwrap_or(value);
            self.push_data_line(value)?;
        } else if let Some(value) = line.strip_prefix(b"event:") {
            let value = value.strip_prefix(b" ").unwrap_or(value);
            self.event_name = String::from_utf8_lossy(value).into_owned();
        }
        Ok(())
    }

    fn push_data_line(&mut self, value: &[u8]) -> Result<(), StreamError> {
        let separator = usize::from(!self.data_lines.is_empty());
        let next = self
            .data_bytes
            .checked_add(separator)
            .and_then(|size| size.checked_add(value.len()))
            .ok_or(StreamError::EventTooLarge {
                limit: MAX_SSE_EVENT_BYTES,
            })?;
        if next > MAX_SSE_EVENT_BYTES {
            return Err(StreamError::EventTooLarge {
                limit: MAX_SSE_EVENT_BYTES,
            });
        }
        self.data_bytes = next;
        self.data_lines.push(value.to_vec());
        Ok(())
    }

    /// Decodes and applies the buffered `data:` payload of one event.
    fn dispatch(&mut self, emit: &mut dyn FnMut(StreamEvent)) -> Result<(), StreamError> {
        if self.data_lines.is_empty() {
            self.event_name.clear();
            return Ok(());
        }
        let event_bytes = std::mem::take(&mut self.data_lines).join(&b'\n');
        self.data_bytes = 0;
        let mut event: WireEvent = serde_json::from_slice(&event_bytes)
            .map_err(|error| StreamError::Decode(error.to_string()))?;
        if event.event_type.is_empty() {
            event.event_type = std::mem::take(&mut self.event_name);
        } else {
            self.event_name.clear();
        }
        match event.event_type.as_str() {
            "response.output_text.delta" => {
                if !event.delta.is_empty() {
                    self.text.push_str(&event.delta);
                    self.emitted = true;
                    emit(StreamEvent::TextDelta { text: event.delta });
                }
            }
            "response.reasoning_summary_text.delta" => {
                if !event.delta.is_empty() {
                    let part = (event.item_id, event.summary_index);
                    if self.reasoning_part.as_ref() != Some(&part) {
                        if !self.reasoning.is_empty() {
                            self.reasoning.push_str("\n\n");
                            emit(StreamEvent::ReasoningDelta {
                                text: "\n\n".into(),
                            });
                        }
                        self.reasoning_part = Some(part);
                    }
                    self.reasoning.push_str(&event.delta);
                    self.emitted = true;
                    emit(StreamEvent::ReasoningDelta { text: event.delta });
                }
            }
            "response.output_item.added" => {
                if let Some(item) = event.item.filter(|item| item.item_type == "function_call") {
                    self.by_item.insert(item.id, self.calls.len());
                    self.calls.push(AssembledToolCall {
                        call_id: item.call_id.clone(),
                        name: item.name.clone(),
                        arguments: String::new(),
                    });
                    self.emitted = true;
                    emit(StreamEvent::ToolCallDelta {
                        tool_call_id: item.call_id,
                        tool_name: item.name,
                        arguments: String::new(),
                    });
                }
            }
            "response.function_call_arguments.delta" => {
                if let Some(&position) = self.by_item.get(&event.item_id)
                    && !event.delta.is_empty()
                {
                    let call = &mut self.calls[position];
                    call.arguments.push_str(&event.delta);
                    self.emitted = true;
                    emit(StreamEvent::ToolCallDelta {
                        tool_call_id: call.call_id.clone(),
                        tool_name: call.name.clone(),
                        arguments: event.delta,
                    });
                }
            }
            "response.output_item.done" => {
                if let Some(item) = event.item {
                    if item.item_type == "function_call" {
                        if let Some(&position) = self.by_item.get(&item.id) {
                            let call = &mut self.calls[position];
                            if call.arguments.is_empty() && !item.arguments.is_empty() {
                                call.arguments = item.arguments;
                            }
                        }
                    } else if matches!(item.item_type.as_str(), "compaction" | "compaction_summary")
                    {
                        if item.encrypted_content.is_empty() {
                            return Err(StreamError::RemoteCompactionProtocol(
                                "compaction encrypted_content is empty",
                            ));
                        }
                        if self.remote_compaction.is_some() {
                            return Err(StreamError::RemoteCompactionProtocol(
                                "multiple compactions were returned",
                            ));
                        }
                        self.remote_compaction = Some(RemoteCompaction {
                            provider: self.remote_compaction_provider.clone(),
                            id: item.id,
                            encrypted_content: item.encrypted_content,
                        });
                    }
                }
            }
            "response.completed" | "response.incomplete" => {
                self.completed = true;
                if let Some(result) = event.response {
                    self.status = result.status;
                    if let Some(usage) = result.usage {
                        self.cached_input_tokens_present = usage.cached_tokens_present();
                        self.usage = Some(Usage {
                            input_tokens: usage.input_tokens,
                            output_tokens: usage.output_tokens,
                            cached_input_tokens: usage.cached_tokens(),
                            ..Default::default()
                        });
                    }
                    if let Some(incomplete) = result.incomplete_details {
                        self.incomplete_reason = incomplete.reason;
                    }
                }
            }
            "response.failed" | "error" => {
                let (code, message) = normalized_error(&event);
                if code == "policy_capacity_unavailable" {
                    return Err(StreamError::PolicyCapacityUnavailable);
                }
                let detail = [code.clone(), message]
                    .into_iter()
                    .filter(|part| !part.is_empty())
                    .collect::<Vec<_>>()
                    .join(": ");
                return Err(StreamError::Reported {
                    detail: if detail.is_empty() {
                        event.event_type
                    } else {
                        format!("{}: {detail}", event.event_type)
                    },
                    code,
                });
            }
            _ => {}
        }
        Ok(())
    }
}

/// Normalizes backend error spellings. Classification takes the first
/// non-empty value in protocol precedence; the message follows the selected
/// object's message first, then falls back through the other error locations.
fn normalized_error(event: &WireEvent) -> (String, String) {
    let response_error = event
        .response
        .as_ref()
        .and_then(|result| result.error.as_ref());
    let nested_error = event.error.as_ref();

    if let Some((code, error)) = classified_error(response_error) {
        return (
            code.to_owned(),
            first_message(error, nested_error, &event.message).to_owned(),
        );
    }
    if let Some((code, error)) = classified_error(nested_error) {
        return (
            code.to_owned(),
            first_message(error, response_error, &event.message).to_owned(),
        );
    }

    (
        event.code.clone(),
        first_nonempty([
            Some(event.message.as_str()),
            response_error.map(|error| error.message.as_str()),
            nested_error.map(|error| error.message.as_str()),
        ])
        .unwrap_or_default()
        .to_owned(),
    )
}

fn classified_error(error: Option<&WireError>) -> Option<(&str, &WireError)> {
    let error = error?;
    first_nonempty([Some(error.code.as_str()), Some(error.error_type.as_str())])
        .map(|code| (code, error))
}

fn first_message<'a>(
    selected: &'a WireError,
    other: Option<&'a WireError>,
    top_level: &'a str,
) -> &'a str {
    first_nonempty([
        Some(selected.message.as_str()),
        other.map(|error| error.message.as_str()),
        Some(top_level),
    ])
    .unwrap_or_default()
}

fn first_nonempty<'a>(values: impl IntoIterator<Item = Option<&'a str>>) -> Option<&'a str> {
    values.into_iter().flatten().find(|value| !value.is_empty())
}

#[cfg(test)]
mod tests;
