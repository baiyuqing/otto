//! Server-sent-event assembler for a Responses API response stream.
//!
//! Port of `internal/provider/openairesponses/stream.go`. The Go version reads
//! from an `io.Reader`; this one is push-based so it depends on no I/O trait
//! and builds for `wasm32-unknown-unknown`. The caller feeds response bytes to
//! [`StreamAssembler::push`] in whatever sizes the transport delivers and
//! calls [`StreamAssembler::finish`] at end of body. Line assembly spans
//! pushes, so the result does not depend on where the chunk boundaries fall.
//!
//! The Responses stream has no `[DONE]` sentinel: it ends with a
//! `response.completed` or `response.incomplete` event and then the body
//! closes.
//!
//! Ownership: the assembler owns all partial state; one instance serves one
//! response and is not shared. The `emit` callback is borrowed for the
//! duration of the call and is invoked synchronously and in arrival order.
//!
//! Errors: every failure is a [`StreamError`] whose text matches the Go error
//! for the same input, except that the decoder detail appended to
//! [`StreamError::Decode`] is `serde_json`'s message rather than
//! `encoding/json`'s. Transport read failures have no variant here: they
//! belong to the HTTP layer in `otto::provider::chatgpt`.

use std::collections::HashMap;

use serde_json::value::RawValue;

use crate::model::{Block, BlockType, Message, Role, Usage};
use crate::provider::{Response, StreamEvent};

use super::protocol::{WireEvent, finish_reason, valid_arguments};

/// Everything the assembler can reject.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum StreamError {
    /// A `data:` payload was not valid event JSON.
    #[error("decode responses stream: {0}")]
    Decode(String),
    /// The backend sent a `response.failed` or an `error` event.
    #[error("responses stream reported {0}")]
    Reported(String),
    /// The body ended before a terminal `response.completed` event.
    #[error("responses stream ended without response.completed")]
    MissingCompletion,
    /// The accumulated arguments of a tool call are not a complete JSON value.
    #[error("tool call {0:?} has malformed arguments")]
    MalformedArguments(String),
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
    text: String,
    calls: Vec<AssembledToolCall>,
    by_item: HashMap<String, usize>,
    usage: Option<Usage>,
    status: String,
    incomplete_reason: String,
    emitted: bool,
    completed: bool,
}

impl StreamAssembler {
    /// A new assembler with no buffered input.
    pub fn new() -> Self {
        Self::default()
    }

    /// Consumes the next slice of response body.
    ///
    /// Complete lines are processed immediately; a trailing partial line is
    /// held until the next push or until [`Self::finish`]. Bytes that arrive
    /// after the terminal event are discarded, matching the Go reader, which
    /// stops reading at that point.
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

        let has_tool_calls = !self.calls.is_empty();
        let mut blocks = Vec::with_capacity(1 + self.calls.len());
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

    /// Applies one field line of the event stream. Comments, `event:` names,
    /// and every other field are ignored because the `data:` payload repeats
    /// the event name in its `type` field; a blank line ends the current
    /// event.
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
            self.data_lines.push(Vec::new());
            return Ok(());
        }
        if let Some(value) = line.strip_prefix(b"data:") {
            let value = value.strip_prefix(b" ").unwrap_or(value);
            self.data_lines.push(value.to_vec());
        }
        Ok(())
    }

    /// Decodes and applies the buffered `data:` payload of one event.
    fn dispatch(&mut self, emit: &mut dyn FnMut(StreamEvent)) -> Result<(), StreamError> {
        if self.data_lines.is_empty() {
            return Ok(());
        }
        let data = std::mem::take(&mut self.data_lines).join(&b'\n');
        let event: WireEvent = serde_json::from_slice(&data)
            .map_err(|error| StreamError::Decode(error.to_string()))?;
        match event.event_type.as_str() {
            "response.output_text.delta" => {
                if !event.delta.is_empty() {
                    self.text.push_str(&event.delta);
                    self.emitted = true;
                    emit(StreamEvent::TextDelta { text: event.delta });
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
                if let Some(item) = event.item.filter(|item| item.item_type == "function_call")
                    && let Some(&position) = self.by_item.get(&item.id)
                {
                    let call = &mut self.calls[position];
                    if call.arguments.is_empty() && !item.arguments.is_empty() {
                        call.arguments = item.arguments;
                    }
                }
            }
            "response.completed" | "response.incomplete" => {
                self.completed = true;
                if let Some(result) = event.response {
                    self.status = result.status;
                    if let Some(usage) = result.usage {
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
                return Err(StreamError::Reported(event.event_type));
            }
            _ => {}
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::FinishReason;

    /// Feeds `body` in slices of `chunk` bytes (`0` meaning one push) and
    /// returns the assembled result together with every emitted event.
    fn assemble(body: &[u8], chunk: usize) -> (Result<Response, StreamError>, Vec<StreamEvent>) {
        let mut events = Vec::new();
        let mut sink = |event: StreamEvent| events.push(event);
        let mut assembler = StreamAssembler::new();
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
