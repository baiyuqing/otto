//! Server-sent-event assembler for a Responses API response stream.
//!
//! The assembler is push-based, so it depends on no I/O trait and builds for
//! `wasm32-unknown-unknown`. The caller feeds response bytes to
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
        let data = std::mem::take(&mut self.data_lines).join(&b'\n');
        self.data_bytes = 0;
        let mut event: WireEvent = serde_json::from_slice(&data)
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
                        });
                    }
                    if let Some(incomplete) = result.incomplete_details {
                        self.incomplete_reason = incomplete.reason;
                    }
                }
            }
            "response.failed" | "error" => {
                let (code, message) = normalized_error(&event);
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
mod tests {
    use super::*;
    use crate::model::FinishReason;

    /// Feeds `body` in slices of `chunk` bytes (`0` meaning one push) and
    /// returns the assembled result together with every emitted event.
    fn assemble(body: &[u8], chunk: usize) -> (Result<Response, StreamError>, Vec<StreamEvent>) {
        assemble_with_compaction(body, chunk, false)
    }

    fn assemble_with_compaction(
        body: &[u8],
        chunk: usize,
        request_remote_compaction: bool,
    ) -> (Result<Response, StreamError>, Vec<StreamEvent>) {
        let mut events = Vec::new();
        let mut sink = |event: StreamEvent| events.push(event);
        let mut assembler =
            StreamAssembler::with_remote_compaction(request_remote_compaction, "test-provider");
        let step = if chunk == 0 { body.len().max(1) } else { chunk };
        let mut result = Ok(());
        for slice in body.chunks(step) {
            result = assembler.push(slice, &mut sink);
            if result.is_err() {
                break;
            }
        }
        match result {
            Err(error) => (Err(error), events),
            Ok(()) => (assembler.finish(&mut sink), events),
        }
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn captures_requested_compaction_without_exposing_opaque_content() {
        for item_type in ["compaction", "compaction_summary"] {
            let body = format!(
                concat!(
                    "data: {{\"type\":\"response.output_text.delta\",\"delta\":\"hi\"}}\n\n",
                    "data: {{\"type\":\"response.output_item.done\",\"item\":{{\"type\":\"{}\",\"id\":\"cmp_1\",\"encrypted_content\":\"opaque-secret\"}}}}\n\n",
                    "data: {{\"type\":\"response.completed\",\"response\":{{\"status\":\"completed\"}}}}\n\n",
                ),
                item_type
            );
            let (response, events) = assemble_with_compaction(body.as_bytes(), 0, true);
            let response = response.expect("requested compaction assembles");
            assert_eq!(response.message.blocks, [Block::text("hi")]);
            assert_eq!(events, [StreamEvent::TextDelta { text: "hi".into() }]);
            assert_eq!(
                response.remote_compaction,
                Some(RemoteCompaction {
                    provider: "test-provider".into(),
                    id: "cmp_1".into(),
                    encrypted_content: "opaque-secret".into(),
                })
            );
        }
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn validates_compaction_cardinality_and_content() {
        let cases = [
            (
                "empty encrypted content",
                true,
                concat!(
                    "data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"compaction\",\"id\":\"c\",\"encrypted_content\":\"\"}}\n\n",
                    "data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\"}}\n\n",
                ),
                "responses remote compaction protocol error: compaction encrypted_content is empty",
            ),
            (
                "duplicate aliases",
                true,
                concat!(
                    "data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"compaction\",\"id\":\"c1\",\"encrypted_content\":\"one\"}}\n\n",
                    "data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"compaction_summary\",\"id\":\"c2\",\"encrypted_content\":\"two\"}}\n\n",
                    "data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\"}}\n\n",
                ),
                "responses remote compaction protocol error: multiple compactions were returned",
            ),
            (
                "requested but missing",
                true,
                "data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\"}}\n\n",
                "responses remote compaction protocol error: requested compaction was not returned",
            ),
            (
                "unsolicited",
                false,
                concat!(
                    "data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"compaction\",\"id\":\"c\",\"encrypted_content\":\"opaque-secret\"}}\n\n",
                    "data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\"}}\n\n",
                ),
                "responses remote compaction protocol error: compaction was returned without being requested",
            ),
        ];
        for (name, requested, body, expected) in cases {
            let (result, events) = assemble_with_compaction(body.as_bytes(), 0, requested);
            let error = result.expect_err(name);
            assert_eq!(error.to_string(), expected, "{name}");
            assert!(!error.to_string().contains("opaque-secret"), "{name}");
            assert!(events.is_empty(), "{name}");
        }
    }

    const TEXT_AND_TOOL_CALL: &str = concat!(
        "event: response.output_text.delta\n",
        "data: {\"type\":\"response.output_text.delta\",\"delta\":\"I will read. \"}\n\n",
        "data: {\"type\":\"response.output_item.added\",\"item\":{\"type\":\"function_call\",\"id\":\"item-1\",\"call_id\":\"call-1\",\"name\":\"read\"}}\n\n",
        "data: {\"type\":\"response.function_call_arguments.delta\",\"item_id\":\"item-1\",\"delta\":\"{\\\"pa\"}\n\n",
        "data: {\"type\":\"response.function_call_arguments.delta\",\"item_id\":\"item-1\",\"delta\":\"th\\\":\\\"README.md\\\"}\"}\n\n",
        "data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"function_call\",\"id\":\"item-1\",\"call_id\":\"call-1\",\"name\":\"read\",\"arguments\":\"{}\"}}\n\n",
        "data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\",\"usage\":{\"input_tokens\":11,\"output_tokens\":7,\"input_tokens_details\":{\"cached_tokens\":4}}}}\n\n",
    );

    fn check_text_and_tool_call(response: &Response, events: &[StreamEvent]) {
        assert_eq!(response.message.role, Role::Assistant);
        assert_eq!(
            response.message.finish_reason,
            Some(FinishReason::ToolCalls)
        );
        assert_eq!(
            response.message.usage,
            Some(Usage {
                input_tokens: 11,
                output_tokens: 7,
                cached_input_tokens: 4
            })
        );
        assert_eq!(response.message.blocks.len(), 2);
        assert_eq!(response.message.blocks[0].block_type, BlockType::Text);
        assert_eq!(response.message.blocks[0].text, "I will read. ");
        let call = &response.message.blocks[1];
        assert_eq!(call.block_type, BlockType::ToolCall);
        assert_eq!(call.tool_call_id, "call-1");
        assert_eq!(call.tool_name, "read");
        assert_eq!(
            call.arguments.as_ref().map(|raw| raw.get()),
            Some(r#"{"path":"README.md"}"#)
        );
        assert_eq!(
            events,
            [
                StreamEvent::TextDelta {
                    text: "I will read. ".into()
                },
                StreamEvent::ToolCallDelta {
                    tool_call_id: "call-1".into(),
                    tool_name: "read".into(),
                    arguments: String::new()
                },
                StreamEvent::ToolCallDelta {
                    tool_call_id: "call-1".into(),
                    tool_name: "read".into(),
                    arguments: "{\"pa".into()
                },
                StreamEvent::ToolCallDelta {
                    tool_call_id: "call-1".into(),
                    tool_name: "read".into(),
                    arguments: "th\":\"README.md\"}".into()
                },
            ]
        );
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn streams_text_and_assembles_tool_call() {
        let (response, events) = assemble(TEXT_AND_TOOL_CALL.as_bytes(), 0);
        check_text_and_tool_call(&response.expect("stream assembles"), &events);
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn assembly_is_independent_of_chunk_boundaries() {
        for chunk in 1..=3 {
            let (response, events) = assemble(TEXT_AND_TOOL_CALL.as_bytes(), chunk);
            check_text_and_tool_call(&response.expect("stream assembles"), &events);
        }
    }

    /// `response.output_item.done` fills in arguments only when the delta
    /// events produced none, so a backend that sends both spellings does not
    /// double the text.
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn done_item_supplies_arguments_only_when_no_delta_arrived() {
        let body = concat!(
            "data: {\"type\":\"response.output_item.added\",\"item\":{\"type\":\"function_call\",\"id\":\"i1\",\"call_id\":\"c1\",\"name\":\"read\"}}\n\n",
            "data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"function_call\",\"id\":\"i1\",\"call_id\":\"c1\",\"name\":\"read\",\"arguments\":\"{\\\"path\\\":\\\"A\\\"}\"}}\n\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\"}}\n\n",
        );
        let (response, _) = assemble(body.as_bytes(), 0);
        let response = response.expect("stream assembles");
        assert_eq!(
            response.message.blocks[0]
                .arguments
                .as_ref()
                .map(|r| r.get()),
            Some(r#"{"path":"A"}"#)
        );
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn keeps_tool_call_identity_across_interleaved_items() {
        let body = concat!(
            "data: {\"type\":\"response.output_item.added\",\"item\":{\"type\":\"function_call\",\"id\":\"i1\",\"call_id\":\"c1\",\"name\":\"read\"}}\n\n",
            "data: {\"type\":\"response.output_item.added\",\"item\":{\"type\":\"function_call\",\"id\":\"i2\",\"call_id\":\"c2\",\"name\":\"write\"}}\n\n",
            "data: {\"type\":\"response.function_call_arguments.delta\",\"item_id\":\"i2\",\"delta\":\"{\\\"path\\\":\\\"B\\\"}\"}\n\n",
            "data: {\"type\":\"response.function_call_arguments.delta\",\"item_id\":\"i1\",\"delta\":\"{\\\"path\\\":\\\"A\\\"}\"}\n\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\"}}\n\n",
        );
        let (response, events) = assemble(body.as_bytes(), 0);
        let response = response.expect("stream assembles");
        assert_eq!(response.message.blocks.len(), 2);
        assert_eq!(response.message.blocks[0].tool_call_id, "c1");
        assert_eq!(
            response.message.blocks[0]
                .arguments
                .as_ref()
                .map(|r| r.get()),
            Some(r#"{"path":"A"}"#)
        );
        assert_eq!(response.message.blocks[1].tool_call_id, "c2");
        assert_eq!(
            response.message.blocks[1]
                .arguments
                .as_ref()
                .map(|r| r.get()),
            Some(r#"{"path":"B"}"#)
        );
        assert_eq!(events.len(), 4);
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn reasoning_summary_deltas_stream_and_become_one_reasoning_block() {
        let body = concat!(
            "data: {\"type\":\"response.reasoning_summary_text.delta\",\"item_id\":\"r1\",\"summary_index\":0,\"delta\":\"Read \"}\n\n",
            "data: {\"type\":\"response.reasoning_summary_text.delta\",\"item_id\":\"r1\",\"summary_index\":0,\"delta\":\"the file\"}\n\n",
            "data: {\"type\":\"response.reasoning_summary_text.delta\",\"item_id\":\"r1\",\"summary_index\":1,\"delta\":\"Then answer\"}\n\n",
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"hi\"}\n\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\"}}\n\n",
        );
        for chunk in [0, 1, 7] {
            let (result, events) = assemble(body.as_bytes(), chunk);
            let response = result.expect("assembled");
            let reasoning = |text: &str| StreamEvent::ReasoningDelta { text: text.into() };
            assert_eq!(
                events,
                vec![
                    reasoning("Read "),
                    reasoning("the file"),
                    reasoning("\n\n"),
                    reasoning("Then answer"),
                    StreamEvent::TextDelta { text: "hi".into() },
                ]
            );
            assert_eq!(
                response.message.blocks,
                vec![
                    Block::reasoning("Read the file\n\nThen answer"),
                    Block::text("hi")
                ]
            );
        }
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn a_reasoning_delta_marks_the_stream_emitted() {
        let mut assembler = StreamAssembler::new();
        let mut sink = |_: StreamEvent| {};
        assembler
            .push(
                b"data: {\"type\":\"response.reasoning_summary_text.delta\",\"item_id\":\"r1\",\"summary_index\":0,\"delta\":\"x\"}\n\n",
                &mut sink,
            )
            .expect("push");
        assert!(assembler.emitted());
    }

    /// A non-function output item, a reasoning summary for example, is ignored
    /// and produces no tool call.
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn ignores_output_items_that_are_not_function_calls() {
        let body = concat!(
            "data: {\"type\":\"response.output_item.added\",\"item\":{\"type\":\"message\",\"id\":\"i1\"}}\n\n",
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"hi\"}\n\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\"}}\n\n",
        );
        let (response, events) = assemble(body.as_bytes(), 0);
        let response = response.expect("stream assembles");
        assert_eq!(response.message.blocks.len(), 1);
        assert_eq!(response.message.blocks[0].text, "hi");
        assert_eq!(response.message.finish_reason, Some(FinishReason::Stop));
        assert_eq!(events, [StreamEvent::TextDelta { text: "hi".into() }]);
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn incomplete_response_with_max_output_tokens_finishes_as_length() {
        let body = concat!(
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"partial\"}\n\n",
            "data: {\"type\":\"response.incomplete\",\"response\":{\"status\":\"incomplete\",\"incomplete_details\":{\"reason\":\"max_output_tokens\"}}}\n\n",
        );
        let (response, _) = assemble(body.as_bytes(), 0);
        assert_eq!(
            response.expect("stream assembles").message.finish_reason,
            Some(FinishReason::Length)
        );
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn reported_errors_carry_the_server_detail() {
        let cases: &[(&str, &str, &str)] = &[
            (
                "error code and message",
                "data: {\"type\":\"error\",\"code\":\"server_error\",\"message\":\"try later\",\"param\":null}\n\n",
                "responses stream reported error: server_error: try later",
            ),
            (
                "error with a nested error object",
                "data: {\"type\":\"error\",\"status\":429,\"error\":{\"type\":\"usage_limit_reached\",\"code\":\"rate_limit_exceeded\",\"message\":\"The usage limit has been reached\"}}\n\n",
                "responses stream reported error: rate_limit_exceeded: The usage limit has been reached",
            ),
            (
                "nested type is retained when code is empty",
                "data: {\"type\":\"error\",\"error\":{\"type\":\"server_error\",\"code\":\"\",\"message\":\"try later\"}}\n\n",
                "responses stream reported error: server_error: try later",
            ),
            (
                "response error type precedes nested error code",
                "data: {\"type\":\"response.failed\",\"response\":{\"error\":{\"code\":\"\",\"type\":\"server_error\",\"message\":\"response message\"}},\"error\":{\"code\":\"nested_code\",\"message\":\"nested message\"},\"code\":\"top_code\",\"message\":\"top message\"}\n\n",
                "responses stream reported response.failed: server_error: response message",
            ),
            (
                "empty nested classification falls through to top level",
                "data: {\"type\":\"error\",\"error\":{\"code\":\"\",\"type\":\"\",\"message\":\"nested message\"},\"code\":\"top_code\",\"message\":\"top message\"}\n\n",
                "responses stream reported error: top_code: top message",
            ),
            (
                "selected classification uses a fallback message",
                "data: {\"type\":\"response.failed\",\"response\":{\"error\":{\"code\":\"response_code\",\"message\":\"\"}},\"error\":{\"message\":\"nested message\"},\"message\":\"top message\"}\n\n",
                "responses stream reported response.failed: response_code: nested message",
            ),
            (
                "error message only",
                "data: {\"type\":\"error\",\"code\":null,\"message\":\"boom\"}\n\n",
                "responses stream reported error: boom",
            ),
            (
                "failed response error",
                "data: {\"type\":\"response.failed\",\"response\":{\"status\":\"failed\",\"error\":{\"code\":\"rate_limit\",\"message\":\"slow down\"}}}\n\n",
                "responses stream reported response.failed: rate_limit: slow down",
            ),
            (
                "failed response with null error",
                "data: {\"type\":\"response.failed\",\"response\":{\"status\":\"failed\",\"error\":null}}\n\n",
                "responses stream reported response.failed",
            ),
        ];
        for (name, body, want) in cases {
            let (result, _) = assemble(body.as_bytes(), 0);
            let error = result.expect_err(name).to_string();
            assert_eq!(error, *want, "{name}");
        }
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn rejects_malformed_failed_and_truncated_streams() {
        let cases: &[(&str, &str, &str)] = &[
            (
                "malformed JSON",
                "data: {not-json}\n\n",
                "decode responses stream",
            ),
            (
                "missing completion",
                "data: {\"type\":\"response.output_text.delta\",\"delta\":\"x\"}\n\n",
                "responses stream ended without response.completed",
            ),
            (
                "reported failure",
                "data: {\"type\":\"response.failed\",\"response\":{\"status\":\"failed\"}}\n\n",
                "responses stream reported response.failed",
            ),
            (
                "reported error",
                "data: {\"type\":\"error\",\"message\":\"boom\"}\n\n",
                "responses stream reported error",
            ),
            (
                "malformed tool arguments",
                concat!(
                    "data: {\"type\":\"response.output_item.added\",\"item\":{\"type\":\"function_call\",\"id\":\"i1\",\"call_id\":\"bad\",\"name\":\"read\"}}\n\n",
                    "data: {\"type\":\"response.function_call_arguments.delta\",\"item_id\":\"i1\",\"delta\":\"{\"}\n\n",
                    "data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\"}}\n\n",
                ),
                "tool call \"bad\" has malformed arguments",
            ),
        ];
        for (name, body, want) in cases {
            let (result, _) = assemble(body.as_bytes(), 0);
            let error = result.expect_err(name).to_string();
            assert!(error.contains(want), "{name}: error = {error:?}");
        }
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn accepts_sse_framing_variants() {
        let body = concat!(
            ": keepalive\r\n\r\n",
            "data\r\n",
            "data:{\"type\":\"response.output_text.delta\",\r\n",
            "data:\"delta\":\"hi\"}\r\n\r\n",
            "event: response.completed\r\n",
            "data:{\"type\":\"response.completed\",\"response\":{\"status\":\"completed\"}}",
        );
        let (response, events) = assemble(body.as_bytes(), 0);
        let response = response.expect("stream assembles");
        assert_eq!(response.message.blocks[0].text, "hi");
        assert_eq!(events, [StreamEvent::TextDelta { text: "hi".into() }]);
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn event_field_supplies_only_an_empty_json_type() {
        let cases: &[(&str, &str, &str)] = &[
            (
                "missing JSON type",
                "event: error\ndata: {\"code\":\"server_error\",\"message\":\"boom\"}\n\n",
                "responses stream reported error: server_error: boom",
            ),
            (
                "empty JSON type",
                "event: error\ndata: {\"type\":\"\",\"message\":\"boom\"}\n\n",
                "responses stream reported error: boom",
            ),
            (
                "JSON type wins conflicts",
                "event: response.completed\ndata: {\"type\":\"error\",\"message\":\"boom\"}\n\n",
                "responses stream reported error: boom",
            ),
        ];
        for (name, body, want) in cases {
            let (result, _) = assemble(body.as_bytes(), 0);
            assert_eq!(result.expect_err(name).to_string(), *want, "{name}");
        }
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn rejects_oversized_lines_before_accumulating_them() {
        let mut sink = |_: StreamEvent| {};
        for ending in [b"\n".as_slice(), b"\r\n".as_slice()] {
            let mut assembler = StreamAssembler::new();
            let mut bytes = vec![b'x'; MAX_SSE_LINE_BYTES];
            bytes.extend_from_slice(ending);
            assert_eq!(
                assembler.push(&bytes, &mut sink),
                Ok(()),
                "the line ending is excluded from the limit"
            );
            assert!(assembler.line.is_empty());
        }

        for suffix in [b"x".as_slice(), b"\rx".as_slice()] {
            let mut assembler = StreamAssembler::new();
            let mut bytes = vec![b'x'; MAX_SSE_LINE_BYTES];
            bytes.extend_from_slice(suffix);
            assert_eq!(
                assembler.push(&bytes, &mut sink),
                Err(StreamError::LineTooLarge {
                    limit: MAX_SSE_LINE_BYTES
                })
            );
            assert!(assembler.line.len() <= MAX_SSE_LINE_BYTES + 1);
        }
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn rejects_oversized_events_before_accumulating_the_last_data_line() {
        let mut assembler = StreamAssembler::new();
        let mut sink = |_: StreamEvent| {};
        let line = vec![b'x'; 64 * 1024];
        let mut accepted = 0usize;
        loop {
            let mut field = b"data:".to_vec();
            field.extend_from_slice(&line);
            field.push(b'\n');
            match assembler.push(&field, &mut sink) {
                Ok(()) => accepted += 1,
                Err(error) => {
                    assert_eq!(
                        error,
                        StreamError::EventTooLarge {
                            limit: MAX_SSE_EVENT_BYTES
                        }
                    );
                    break;
                }
            }
        }
        assert_eq!(assembler.data_lines.len(), accepted);
        assert!(assembler.data_bytes <= MAX_SSE_EVENT_BYTES);
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn accepts_completed_event_larger_than_64_kib_on_one_line() {
        let instructions = "x".repeat(200 * 1024);
        let body = format!(
            "data: {{\"type\":\"response.completed\",\"response\":{{\"status\":\"completed\",\"instructions\":\"{instructions}\"}}}}\n\n"
        );
        assert!(body.len() > 64 * 1024);
        for chunk in [0, 4096] {
            let (result, _) = assemble(body.as_bytes(), chunk);
            assert!(result.is_ok(), "{result:?}");
        }
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn data_line_limit_matches_event_limit() {
        let mut sink = |_: StreamEvent| {};
        let mut assembler = StreamAssembler::new();
        let mut at_limit = b"data: ".to_vec();
        at_limit.extend(std::iter::repeat_n(b'x', MAX_SSE_EVENT_BYTES));
        assert_eq!(assembler.push(&at_limit, &mut sink), Ok(()));
        assert_eq!(assembler.push(b"\n", &mut sink), Ok(()));
        assert_eq!(assembler.data_bytes, MAX_SSE_EVENT_BYTES);

        let mut assembler = StreamAssembler::new();
        // Without the optional space, the line stays within the line cap.
        let mut over = b"data:".to_vec();
        over.extend(std::iter::repeat_n(b'x', MAX_SSE_EVENT_BYTES + 1));
        assert_eq!(assembler.push(&over, &mut sink), Ok(()));
        assert_eq!(
            assembler.push(b"\n", &mut sink),
            Err(StreamError::EventTooLarge {
                limit: MAX_SSE_EVENT_BYTES
            })
        );
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn usage_absence_is_preserved() {
        let body =
            "data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\"}}\n\n";
        let (response, _) = assemble(body.as_bytes(), 0);
        assert_eq!(response.expect("stream assembles").message.usage, None);
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn ignores_bytes_after_the_terminal_event_and_reports_emission() {
        let body = concat!(
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"a\"}\n\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\"}}\n\n",
            "data: {not-json}\n\n",
        );
        let mut events = Vec::new();
        let mut sink = |event: StreamEvent| events.push(event);
        let mut assembler = StreamAssembler::new();
        assembler.push(body.as_bytes(), &mut sink).expect("push");
        assert!(assembler.emitted());
        assert!(assembler.is_done());
        let response = assembler.finish(&mut sink).expect("stream assembles");
        assert_eq!(response.message.blocks.len(), 1);
        assert_eq!(events.len(), 1);
    }

    /// The provider hands its own sink straight to the assembler, so
    /// [`crate::provider::StreamSink`] has to coerce to the callback type.
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn accepts_the_provider_stream_sink() {
        let mut count = 0usize;
        let mut sink = |_event: StreamEvent| count += 1;
        let sink: crate::provider::StreamSink<'_> = &mut sink;
        let mut assembler = StreamAssembler::new();
        assembler
            .push(
                "data: {\"type\":\"response.output_text.delta\",\"delta\":\"x\"}\n\n".as_bytes(),
                sink,
            )
            .expect("push");
        assert_eq!(count, 1);
    }
}
