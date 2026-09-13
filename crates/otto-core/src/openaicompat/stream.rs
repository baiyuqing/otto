//! Server-sent-event assembler for a Chat Completions response stream.
//!
//! Port of `internal/provider/openaicompat/stream.go`. The Go version reads
//! from an `io.Reader`; this one is push-based so it depends on no I/O trait
//! and builds for `wasm32-unknown-unknown`. The caller feeds response bytes to
//! [`StreamAssembler::push`] in whatever sizes the transport delivers and
//! calls [`StreamAssembler::finish`] at end of body. Line assembly spans
//! pushes, so the result does not depend on where the chunk boundaries fall.
//!
//! Ownership: the assembler owns all partial state; one instance serves one
//! response and is not shared. The `emit` callback is borrowed for the
//! duration of the call and is invoked synchronously and in arrival order.
//!
//! Errors: every failure is a [`StreamError`] whose text matches the Go error
//! for the same input, except that the decoder detail appended to
//! [`StreamError::Decode`] is `serde_json`'s message rather than
//! `encoding/json`'s. Transport read failures have no variant here: they
//! belong to the HTTP layer, which is phase 4.

use std::collections::HashMap;

use serde_json::value::RawValue;

use crate::model::{Block, BlockType, FinishReason, Message, Role, Usage};
use crate::provider::{Response, StreamEvent};

use super::protocol::{WireChunk, finish_reason, valid_arguments};

/// Everything the assembler can reject.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum StreamError {
    /// A `data:` payload was not valid chunk JSON.
    #[error("decode chat completion stream: {0}")]
    Decode(String),
    /// The body ended before the `[DONE]` sentinel.
    #[error("chat completion stream ended without [DONE]")]
    MissingDone,
    /// The accumulated arguments of a tool call are not a complete JSON value.
    #[error("tool call {0:?} has malformed arguments")]
    MalformedArguments(String),
}

/// A tool call being assembled from `index`-keyed fragments.
#[derive(Debug, Default)]
struct AssembledToolCall {
    id: String,
    name: String,
    arguments: String,
}

/// Assembles one streamed response.
///
/// See the module documentation for the ownership, concurrency, and error
/// rules.
#[derive(Debug)]
pub struct StreamAssembler {
    line: Vec<u8>,
    data_lines: Vec<Vec<u8>>,
    text: String,
    calls: Vec<AssembledToolCall>,
    by_index: HashMap<i64, usize>,
    usage: Option<Usage>,
    finish: FinishReason,
    emitted: bool,
    done: bool,
}

impl Default for StreamAssembler {
    fn default() -> Self {
        Self {
            line: Vec::new(),
            data_lines: Vec::new(),
            text: String::new(),
            calls: Vec::new(),
            by_index: HashMap::new(),
            usage: None,
            finish: FinishReason::Unknown,
            emitted: false,
            done: false,
        }
    }
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
    /// after the `[DONE]` sentinel are discarded, matching the Go reader,
    /// which stops reading at that point.
    pub fn push(
        &mut self,
        bytes: &[u8],
        emit: &mut dyn FnMut(StreamEvent),
    ) -> Result<(), StreamError> {
        for &byte in bytes {
            if self.done {
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
        if !self.done {
            if !self.line.is_empty() {
                let line = std::mem::take(&mut self.line);
                self.consume_line(&line, emit)?;
            }
            if !self.done && !self.data_lines.is_empty() {
                self.dispatch(emit)?;
            }
        }
        if !self.done {
            return Err(StreamError::MissingDone);
        }

        let mut blocks = Vec::with_capacity(1 + self.calls.len());
        if !self.text.is_empty() {
            blocks.push(Block::text(self.text));
        }
        for call in self.calls {
            if !valid_arguments(&call.arguments) {
                return Err(StreamError::MalformedArguments(call.id));
            }
            blocks.push(Block {
                block_type: BlockType::ToolCall,
                tool_call_id: call.id,
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
                finish_reason: Some(self.finish),
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

    /// Reports whether the `[DONE]` sentinel has been seen.
    pub fn is_done(&self) -> bool {
        self.done
    }

    /// Applies one field line of the event stream. Comments and fields other
    /// than `data` are ignored; a blank line ends the current event.
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
        if data == b"[DONE]" {
            self.done = true;
            return Ok(());
        }
        let chunk: WireChunk = serde_json::from_slice(&data)
            .map_err(|error| StreamError::Decode(error.to_string()))?;
        if let Some(usage) = &chunk.usage {
            self.usage = Some(Usage {
                input_tokens: usage.prompt_tokens,
                output_tokens: usage.completion_tokens,
                cached_input_tokens: usage.cached_tokens(),
            });
        }
        for choice in chunk.choices {
            if !choice.delta.content.is_empty() {
                self.text.push_str(&choice.delta.content);
                self.emitted = true;
                emit(StreamEvent::TextDelta {
                    text: choice.delta.content,
                });
            }
            for delta in choice.delta.tool_calls {
                let position = match self.by_index.get(&delta.index) {
                    Some(position) => *position,
                    None => {
                        self.calls.push(AssembledToolCall::default());
                        let position = self.calls.len() - 1;
                        self.by_index.insert(delta.index, position);
                        position
                    }
                };
                let call = &mut self.calls[position];
                if !delta.id.is_empty() {
                    call.id = delta.id;
                }
                if !delta.function.name.is_empty() {
                    call.name = delta.function.name;
                }
                call.arguments.push_str(&delta.function.arguments);
                self.emitted = true;
                emit(StreamEvent::ToolCallDelta {
                    tool_call_id: call.id.clone(),
                    tool_name: call.name.clone(),
                    arguments: delta.function.arguments,
                });
            }
            if !choice.finish_reason.is_empty() {
                self.finish = finish_reason(&choice.finish_reason);
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{BlockType, FinishReason, Role, Usage};

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
            Ok(()) => {
                let response = assembler.finish(&mut sink);
                (response, events)
            }
        }
    }

    const TEXT_AND_TOOL_CALL: &str = concat!(
        "data: {\"choices\":[{\"delta\":{\"content\":\"I will read. \"}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call-1\",\"type\":\"function\",\"function\":{\"name\":\"read\",\"arguments\":\"{\\\"pa\"}}]}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"th\\\":\\\"README.md\\\"}\"}}]},\"finish_reason\":\"tool_calls\"}]}\n\n",
        "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":11,\"completion_tokens\":7}}\n\n",
        "data: [DONE]\n\n",
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
                cached_input_tokens: 0
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

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn handles_sse_framing_and_multiple_indexed_calls() {
        let large = "x".repeat(70 << 10);
        let body = format!(
            concat!(
                ": keepalive\r\n\r\n",
                "data: {{\"choices\":[{{\"delta\":{{\"content\":\"{large}\"}}}}]}}\r\n\r\n",
                "data: {{\"choices\":[{{\"delta\":{{\"tool_calls\":[{{\"index\":3,\"id\":\"third\",\"function\":{{\"name\":\"write\",\"arguments\":\"{{\\\"pa\"}}}},{{\"index\":1,\"id\":\"first\",\"function\":{{\"name\":\"read\",\"arguments\":\"{{\\\"path\\\":\\\"A\\\"}}\"}}}}]}}}}]}}\r\n\r\n",
                "data: {{\"choices\":[{{\"delta\":{{\"tool_calls\":[{{\"index\":3,\"function\":{{\"arguments\":\"th\\\":\\\"B\\\"}}\"}}}}]}},\"finish_reason\":\"tool_calls\"}}]}}\r\n\r\n",
                "data: {{\"choices\":[],\r\ndata: \"usage\":{{\"prompt_tokens\":5,\"completion_tokens\":9}}}}\r\n\r\n",
                "event: completion\r\ndata: [DONE]\r\n\r\n",
            ),
            large = large
        );
        let (response, _) = assemble(body.as_bytes(), 0);
        let response = response.expect("stream assembles");
        assert_eq!(response.message.blocks.len(), 3);
        assert_eq!(response.message.blocks[0].text, large);
        let first_seen = &response.message.blocks[1];
        assert_eq!(first_seen.tool_call_id, "third");
        assert_eq!(first_seen.tool_name, "write");
        assert_eq!(
            first_seen.arguments.as_ref().map(|raw| raw.get()),
            Some(r#"{"path":"B"}"#)
        );
        let second = &response.message.blocks[2];
        assert_eq!(second.tool_call_id, "first");
        assert_eq!(second.tool_name, "read");
        assert_eq!(
            second.arguments.as_ref().map(|raw| raw.get()),
            Some(r#"{"path":"A"}"#)
        );
        assert_eq!(
            response.message.usage,
            Some(Usage {
                input_tokens: 5,
                output_tokens: 9,
                cached_input_tokens: 0
            })
        );
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn emits_stable_tool_call_identity_for_interleaved_continuations() {
        let body = concat!(
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":1,\"id\":\"call-b\",\"function\":{\"name\":\"write\",\"arguments\":\"{\\\"pa\"}},{\"index\":0,\"id\":\"call-a\",\"function\":{\"name\":\"read\",\"arguments\":\"{\\\"pa\"}}]}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"th\\\":\\\"A\\\"}\"}}]}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":1,\"function\":{\"arguments\":\"th\\\":\\\"B\\\"}\"}}]},\"finish_reason\":\"tool_calls\"}]}\n\n",
            "data: [DONE]\n\n",
        );
        let (response, events) = assemble(body.as_bytes(), 0);
        let response = response.expect("stream assembles");
        let want = |id: &str, name: &str, arguments: &str| StreamEvent::ToolCallDelta {
            tool_call_id: id.into(),
            tool_name: name.into(),
            arguments: arguments.into(),
        };
        assert_eq!(
            events,
            [
                want("call-b", "write", "{\"pa"),
                want("call-a", "read", "{\"pa"),
                want("call-a", "read", "th\":\"A\"}"),
                want("call-b", "write", "th\":\"B\"}"),
            ]
        );
        assert_eq!(response.message.blocks.len(), 2);
        assert_eq!(response.message.blocks[0].tool_call_id, "call-b");
        assert_eq!(
            response.message.blocks[0]
                .arguments
                .as_ref()
                .map(|r| r.get()),
            Some(r#"{"path":"B"}"#)
        );
        assert_eq!(response.message.blocks[1].tool_call_id, "call-a");
        assert_eq!(
            response.message.blocks[1]
                .arguments
                .as_ref()
                .map(|r| r.get()),
            Some(r#"{"path":"A"}"#)
        );
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn rejects_malformed_or_incomplete_streams() {
        let cases: &[(&str, &str, &str)] = &[
            (
                "malformed JSON",
                "data: {not-json}\n\n",
                "decode chat completion stream",
            ),
            (
                "missing done",
                "data: {\"choices\":[]}\n\n",
                "chat completion stream ended without [DONE]",
            ),
            (
                "malformed tool arguments",
                "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"bad\",\"function\":{\"name\":\"read\",\"arguments\":\"{\"}}]},\"finish_reason\":\"tool_calls\"}]}\n\ndata: [DONE]\n\n",
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
    fn maps_unknown_finish_reason() {
        let body = "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"content_filter\"}]}\n\ndata: [DONE]\n\n";
        let (response, _) = assemble(body.as_bytes(), 0);
        assert_eq!(
            response.expect("stream assembles").message.finish_reason,
            Some(FinishReason::Unknown)
        );
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn parses_both_cached_token_spellings() {
        let cases: &[(&str, i64)] = &[
            (
                "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":100,\"completion_tokens\":7,\"prompt_tokens_details\":{\"cached_tokens\":64}}}\n\ndata: [DONE]\n\n",
                64,
            ),
            (
                "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":100,\"completion_tokens\":7,\"prompt_cache_hit_tokens\":80,\"prompt_cache_miss_tokens\":20}}\n\ndata: [DONE]\n\n",
                80,
            ),
            (
                "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":100,\"completion_tokens\":7,\"prompt_tokens_details\":{\"cached_tokens\":0},\"prompt_cache_hit_tokens\":80}}\n\ndata: [DONE]\n\n",
                80,
            ),
        ];
        for (body, cached) in cases {
            let (response, _) = assemble(body.as_bytes(), 0);
            assert_eq!(
                response.expect("stream assembles").message.usage,
                Some(Usage {
                    input_tokens: 100,
                    output_tokens: 7,
                    cached_input_tokens: *cached
                })
            );
        }
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn preserves_usage_presence() {
        let missing =
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";
        let (response, _) = assemble(missing.as_bytes(), 0);
        assert_eq!(response.expect("stream assembles").message.usage, None);

        let zero = "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":0,\"completion_tokens\":0}}\n\ndata: [DONE]\n\n";
        let (response, _) = assemble(zero.as_bytes(), 0);
        assert_eq!(
            response.expect("stream assembles").message.usage,
            Some(Usage::default())
        );
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn accepts_bare_data_lines_and_no_space_after_the_colon() {
        let body = "data\ndata:{\"choices\":[{\"delta\":{\"content\":\"hi\"},\"finish_reason\":\"stop\"}]}\n\ndata:[DONE]\n\n";
        let (response, events) = assemble(body.as_bytes(), 0);
        let response = response.expect("stream assembles");
        assert_eq!(response.message.blocks.len(), 1);
        assert_eq!(response.message.blocks[0].text, "hi");
        assert_eq!(response.message.finish_reason, Some(FinishReason::Stop));
        assert_eq!(events, [StreamEvent::TextDelta { text: "hi".into() }]);
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn dispatches_a_trailing_event_that_has_no_blank_line() {
        let body = "data: {\"choices\":[{\"delta\":{\"content\":\"tail\"}}]}\n\ndata: [DONE]";
        let (response, _) = assemble(body.as_bytes(), 0);
        assert_eq!(
            response.expect("stream assembles").message.blocks[0].text,
            "tail"
        );
    }

    /// Phase 4 hands the provider's own sink straight to the assembler, so
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
                "data: {\"choices\":[{\"delta\":{\"content\":\"x\"}}]}\n\ndata: [DONE]\n\n"
                    .as_bytes(),
                sink,
            )
            .expect("push");
        assert_eq!(count, 1);
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn ignores_bytes_after_done_and_reports_emission() {
        let body = "data: {\"choices\":[{\"delta\":{\"content\":\"a\"}}]}\n\ndata: [DONE]\n\ndata: {not-json}\n\n";
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
}
