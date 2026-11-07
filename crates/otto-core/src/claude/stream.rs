//! Incremental byte-oriented SSE assembler for Claude Messages. Unknown event,
//! block and delta types are ignored; known malformed events fail closed.
//! Message deltas update metadata independently of block lifetimes. Only a
//! message_stop completes the response, finalizing any blocks without an explicit
//! stop using the same strict tool-input validation. Errors are content-free.
//! After any error, discard this assembler.
use super::{json, protocol::finish_reason};
use crate::model::{Block, BlockType, FinishReason, Message, Role, Usage};
use crate::provider::{Response, StreamEvent};
use serde::Deserialize;
use serde_json::{Value, value::RawValue};
use std::collections::{BTreeMap, HashSet};

/// Limits bound framing, total stream work, block count and retained content.
pub const MAX_EVENT_BYTES: usize = 1 << 20;
pub const MAX_STREAM_BYTES: usize = 16 << 20;
pub const MAX_BLOCKS: usize = 1024;
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum StreamError {
    #[error("malformed Claude stream event")]
    Decode,
    #[error("invalid Claude stream sequence: {0}")]
    Sequence(&'static str),
    #[error("Claude stream exceeded safety limit")]
    Limit,
    #[error("Claude stream ended before message_stop")]
    MissingStop,
    #[error("malformed Claude tool input")]
    MalformedArguments,
    #[error("Claude stream reported an API error")]
    Api,
}
#[derive(Debug)]
struct PendingBlock {
    block: Option<Block>,
    input: String,
    stopped: bool,
}
impl PendingBlock {
    /// Explicit block stops and the final message stop share the same checks.
    fn finish(&mut self) -> Result<(), StreamError> {
        if self.stopped {
            return Ok(());
        }
        if !self.input.is_empty() {
            let input =
                json::parse(self.input.as_bytes()).map_err(|_| StreamError::MalformedArguments)?;
            if !input.is_object() {
                return Err(StreamError::MalformedArguments);
            }
            if let Some(block) = self.block.as_mut() {
                block.arguments = Some(
                    RawValue::from_string(std::mem::take(&mut self.input))
                        .map_err(|_| StreamError::MalformedArguments)?,
                );
            }
        }
        self.stopped = true;
        Ok(())
    }
}

// Decode initial arguments from the already validated wire event, preserving
// decimal and large integer literals rather than serializing a Value again.
#[derive(Deserialize)]
struct ToolInputEvent {
    content_block: ToolInputBlock,
}
#[derive(Deserialize)]
struct ToolInputBlock {
    input: Box<RawValue>,
}

#[derive(Debug, Default)]
pub struct StreamAssembler {
    line: Vec<u8>,
    data: Vec<u8>,
    event: String,
    bytes: usize,
    started: bool,
    done: bool,
    id: String,
    blocks: BTreeMap<u64, PendingBlock>,
    call_ids: HashSet<String>,
    usage: Option<Usage>,
    finish: Option<FinishReason>,
}
fn string<'a>(value: &'a Value, key: &str) -> Result<&'a str, StreamError> {
    value
        .get(key)
        .and_then(Value::as_str)
        .ok_or(StreamError::Decode)
}
impl StreamAssembler {
    pub fn new() -> Self {
        Self::default()
    }
    /// True only after successfully handling message_stop. The transport may
    /// stop reading immediately rather than waiting for socket EOF.
    pub fn is_done(&self) -> bool {
        self.done
    }
    /// Feeds arbitrary byte chunks, including split UTF-8 code points. Bytes
    /// after message_stop are ignored. Discard the instance after any error.
    pub fn push(
        &mut self,
        bytes: &[u8],
        emit: &mut dyn FnMut(StreamEvent),
    ) -> Result<(), StreamError> {
        for &byte in bytes {
            if self.done {
                break;
            }
            self.bytes = self.bytes.checked_add(1).ok_or(StreamError::Limit)?;
            if self.bytes > MAX_STREAM_BYTES {
                return Err(StreamError::Limit);
            }
            if byte == b'\n' {
                let line = std::mem::take(&mut self.line);
                self.consume_line(&line, emit)?;
            } else {
                if self.line.len() >= MAX_EVENT_BYTES {
                    return Err(StreamError::Limit);
                }
                self.line.push(byte);
            }
        }
        Ok(())
    }
    fn consume_line(
        &mut self,
        line: &[u8],
        emit: &mut dyn FnMut(StreamEvent),
    ) -> Result<(), StreamError> {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if line.is_empty() {
            if !self.data.is_empty() {
                let data = std::mem::take(&mut self.data);
                let value = json::parse(&data).map_err(|_| StreamError::Decode)?;
                let kind = string(&value, "type")?;
                if !self.event.is_empty() && self.event != kind {
                    return Err(StreamError::Decode);
                }
                self.consume_event(kind, &value, &data, emit)?;
            }
            self.event.clear();
        } else if let Some(data) = line.strip_prefix(b"data:") {
            let data = data.strip_prefix(b" ").unwrap_or(data);
            if self.data.len() + data.len() + 1 > MAX_EVENT_BYTES {
                return Err(StreamError::Limit);
            }
            if !self.data.is_empty() {
                self.data.push(b'\n');
            }
            self.data.extend_from_slice(data);
        } else if let Some(event) = line.strip_prefix(b"event:") {
            if !self.event.is_empty() {
                return Err(StreamError::Decode);
            }
            self.event = std::str::from_utf8(event.strip_prefix(b" ").unwrap_or(event))
                .map_err(|_| StreamError::Decode)?
                .to_owned();
        }
        Ok(())
    }
    fn usage(&mut self, value: &Value) -> Result<(), StreamError> {
        let object = value.as_object().ok_or(StreamError::Decode)?;
        let usage = self.usage.get_or_insert_with(Usage::default);
        for (key, target) in [
            ("input_tokens", &mut usage.input_tokens),
            ("output_tokens", &mut usage.output_tokens),
        ] {
            if let Some(count) = object.get(key).filter(|count| !count.is_null()) {
                let count = count
                    .as_i64()
                    .filter(|count| *count >= 0)
                    .ok_or(StreamError::Decode)?;
                *target = count;
            }
        }
        Ok(())
    }
    fn consume_event(
        &mut self,
        kind: &str,
        value: &Value,
        raw_event: &[u8],
        emit: &mut dyn FnMut(StreamEvent),
    ) -> Result<(), StreamError> {
        match kind {
            "ping" => {}
            "error" => return Err(StreamError::Api),
            "message_start" => {
                if self.started {
                    return Err(StreamError::Sequence("message_start: duplicate start"));
                }
                let message = value.get("message").ok_or(StreamError::Decode)?;
                if string(message, "role")? != "assistant" {
                    return Err(StreamError::Decode);
                }
                if message
                    .get("content")
                    .and_then(Value::as_array)
                    .is_none_or(|blocks| !blocks.is_empty())
                {
                    return Err(StreamError::Decode);
                }
                self.id = string(message, "id")?.to_owned();
                self.usage(message.get("usage").ok_or(StreamError::Decode)?)?;
                self.started = true;
            }
            "content_block_start" => {
                if !self.started {
                    return Err(StreamError::Sequence(
                        "content_block_start: before message_start",
                    ));
                }
                let index = value
                    .get("index")
                    .and_then(Value::as_u64)
                    .ok_or(StreamError::Decode)?;
                if self.blocks.contains_key(&index) {
                    return Err(StreamError::Sequence(
                        "content_block_start: duplicate block index",
                    ));
                }
                if self.blocks.len() >= MAX_BLOCKS || index >= MAX_BLOCKS as u64 {
                    return Err(StreamError::Limit);
                }
                let wire = value.get("content_block").ok_or(StreamError::Decode)?;
                let block = match string(wire, "type")? {
                    "text" => {
                        let text = string(wire, "text")?.to_owned();
                        if !text.is_empty() {
                            emit(StreamEvent::TextDelta { text: text.clone() });
                        }
                        Some(Block {
                            block_type: BlockType::Text,
                            text,
                            ..Block::default()
                        })
                    }
                    "tool_use" => {
                        let id = string(wire, "id")?.to_owned();
                        let name = string(wire, "name")?.to_owned();
                        if id.is_empty() || name.is_empty() || !self.call_ids.insert(id.clone()) {
                            return Err(StreamError::Sequence(
                                "content_block_start: empty tool id/name or duplicate tool id",
                            ));
                        }
                        wire.get("input")
                            .filter(|input| input.is_object())
                            .ok_or(StreamError::Decode)?;
                        Some(Block {
                            block_type: BlockType::ToolCall,
                            tool_call_id: id,
                            tool_name: name,
                            arguments: Some(
                                serde_json::from_slice::<ToolInputEvent>(raw_event)
                                    .map_err(|_| StreamError::Decode)?
                                    .content_block
                                    .input,
                            ),
                            ..Block::default()
                        })
                    }
                    _ => None,
                };
                self.blocks.insert(
                    index,
                    PendingBlock {
                        block,
                        input: String::new(),
                        stopped: false,
                    },
                );
            }
            "content_block_delta" => {
                if !self.started {
                    return Err(StreamError::Sequence(
                        "content_block_delta: before message_start",
                    ));
                }
                let index = value
                    .get("index")
                    .and_then(Value::as_u64)
                    .ok_or(StreamError::Decode)?;
                let pending = self
                    .blocks
                    .get_mut(&index)
                    .filter(|block| !block.stopped)
                    .ok_or(StreamError::Sequence(
                        "content_block_delta: block missing or already stopped",
                    ))?;
                let delta = value.get("delta").ok_or(StreamError::Decode)?;
                match string(delta, "type")? {
                    "text_delta" => {
                        let text = string(delta, "text")?;
                        if let Some(block) = pending.block.as_mut() {
                            if block.block_type != BlockType::Text {
                                return Err(StreamError::Sequence(
                                    "content_block_delta: text_delta on non-text block",
                                ));
                            }
                            block.text.push_str(text);
                            emit(StreamEvent::TextDelta {
                                text: text.to_owned(),
                            });
                        }
                    }
                    "input_json_delta" => {
                        let fragment = string(delta, "partial_json")?;
                        if let Some(block) = pending.block.as_ref() {
                            if block.block_type != BlockType::ToolCall {
                                return Err(StreamError::Sequence(
                                    "content_block_delta: input_json_delta on non-tool block",
                                ));
                            }
                            pending.input.push_str(fragment);
                            emit(StreamEvent::ToolCallDelta {
                                tool_call_id: block.tool_call_id.clone(),
                                tool_name: block.tool_name.clone(),
                                arguments: fragment.to_owned(),
                            });
                        }
                    }
                    _ => {}
                }
            }
            "content_block_stop" => {
                if !self.started {
                    return Err(StreamError::Sequence(
                        "content_block_stop: before message_start",
                    ));
                }
                let index = value
                    .get("index")
                    .and_then(Value::as_u64)
                    .ok_or(StreamError::Decode)?;
                let pending = self
                    .blocks
                    .get_mut(&index)
                    .filter(|block| !block.stopped)
                    .ok_or(StreamError::Sequence(
                        "content_block_stop: block missing or already stopped",
                    ))?;
                pending.finish()?;
            }
            "message_delta" => {
                if !self.started {
                    return Err(StreamError::Sequence("message_delta: before message_start"));
                }
                let delta = value
                    .get("delta")
                    .and_then(Value::as_object)
                    .ok_or(StreamError::Decode)?;
                if let Some(reason) = delta.get("stop_reason").filter(|reason| !reason.is_null()) {
                    let reason = reason
                        .as_str()
                        .filter(|reason| !reason.is_empty())
                        .ok_or(StreamError::Decode)?;
                    self.finish = Some(finish_reason(reason));
                }
                if let Some(usage) = value.get("usage").filter(|usage| !usage.is_null()) {
                    self.usage(usage)?;
                }
            }
            "message_stop" => {
                if !self.started {
                    return Err(StreamError::Sequence("message_stop: before message_start"));
                }
                if self.finish.is_none() {
                    return Err(StreamError::Sequence("message_stop: missing stop_reason"));
                }
                for pending in self.blocks.values_mut() {
                    pending.finish()?;
                }
                self.done = true;
            }
            _ => {}
        }
        Ok(())
    }
    /// Flushes a final unterminated SSE event and requires message_stop.
    pub fn finish(mut self, emit: &mut dyn FnMut(StreamEvent)) -> Result<Response, StreamError> {
        if !self.done {
            let line = std::mem::take(&mut self.line);
            self.consume_line(&line, emit)?;
            self.consume_line(b"", emit)?;
        }
        if !self.done {
            return Err(StreamError::MissingStop);
        }
        Ok(Response {
            message: Message {
                id: self.id,
                role: Role::Assistant,
                blocks: self
                    .blocks
                    .into_values()
                    .filter_map(|pending| pending.block)
                    .collect(),
                usage: self.usage,
                finish_reason: self.finish,
                ..Message::default()
            },
            ..Response::default()
        })
    }
}
