//! Responses API wire types and request translation.
//!
//! Port of `internal/provider/openairesponses/protocol.go`. Field names, field
//! order, and the omit-empty rules reproduce Go's `encoding/json` output for
//! the same structs, so a request serialized here is byte-identical to the Go
//! request for the same [`Request`].
//!
//! One difference is deliberate: Go holds a tool's `parameters` in a
//! `map[string]any` and therefore sorts its keys, while here the schema is raw
//! JSON passed through verbatim and keeps the caller's key order. This matches
//! the same decision in [`crate::openaicompat::protocol`].
//!
//! Ownership: [`build_request`] borrows the request and returns an owned wire
//! value. Decoding types own their data.
//!
//! Errors: only [`serialized_request_size`] can fail, and only if the request
//! does not serialize.

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::model::{BlockType, FinishReason, Message, Role};
use crate::provider::Request;

/// One `POST /responses` body. Otto always streams and never asks the backend
/// to store the response.
#[derive(Debug, Clone, Serialize)]
pub struct WireRequest {
    pub model: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub instructions: String,
    #[serde(serialize_with = "serialize_input")]
    pub input: Vec<WireItem>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<WireTool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<WireReasoning>,
    pub stream: bool,
    pub store: bool,
}

/// The reasoning-effort request field, present only when the model asks for
/// one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WireReasoning {
    pub effort: String,
}

/// One advertised tool. The Responses API flattens the function definition
/// into the tool object instead of nesting it.
#[derive(Debug, Clone, Serialize)]
pub struct WireTool {
    #[serde(rename = "type")]
    pub tool_type: String,
    pub name: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub description: String,
    pub parameters: Option<Box<serde_json::value::RawValue>>,
}

/// One input item. The struct covers every item shape the request side uses:
/// `message`, `function_call`, and `function_call_output`.
///
/// `output` is an [`Option`] rather than a plain `String` so a
/// `function_call_output` item emits `"output"` even when the tool produced no
/// text; the Responses API rejects the item otherwise. Other item types leave
/// it `None` and it is skipped.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct WireItem {
    #[serde(rename = "type")]
    pub item_type: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub role: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub content: Vec<WireContent>,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub call_id: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub name: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub arguments: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
}

/// One content part of a message item.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WireContent {
    #[serde(rename = "type")]
    pub content_type: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub text: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub image_url: String,
}

/// One decoded `data:` payload of the response stream. The payload repeats the
/// SSE `event:` name in its `type` field, so decoding switches on `type`
/// directly and the `event:` line is ignored.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct WireEvent {
    #[serde(rename = "type", default, deserialize_with = "null_as_default")]
    pub event_type: String,
    #[serde(default, deserialize_with = "null_as_default")]
    pub delta: String,
    #[serde(default, deserialize_with = "null_as_default")]
    pub item_id: String,
    #[serde(default)]
    pub item: Option<WireOutputItem>,
    #[serde(default)]
    pub response: Option<WireResult>,
}

/// One output item announced by `response.output_item.added` or completed by
/// `response.output_item.done`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct WireOutputItem {
    #[serde(rename = "type", default, deserialize_with = "null_as_default")]
    pub item_type: String,
    #[serde(default, deserialize_with = "null_as_default")]
    pub id: String,
    #[serde(default, deserialize_with = "null_as_default")]
    pub call_id: String,
    #[serde(default, deserialize_with = "null_as_default")]
    pub name: String,
    #[serde(default, deserialize_with = "null_as_default")]
    pub arguments: String,
}

/// The response object carried by the terminal event.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct WireResult {
    #[serde(default, deserialize_with = "null_as_default")]
    pub status: String,
    #[serde(default)]
    pub usage: Option<WireUsage>,
    #[serde(default)]
    pub incomplete_details: Option<WireIncomplete>,
}

/// Why a response stopped before it was complete.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct WireIncomplete {
    #[serde(default, deserialize_with = "null_as_default")]
    pub reason: String,
}

/// Token counts from the terminal event.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct WireUsage {
    #[serde(default, deserialize_with = "null_as_default")]
    pub input_tokens: i64,
    #[serde(default, deserialize_with = "null_as_default")]
    pub output_tokens: i64,
    #[serde(default)]
    pub input_tokens_details: Option<InputTokensDetails>,
}

impl WireUsage {
    /// The cached prompt tokens, or zero when the backend sent no breakdown.
    pub fn cached_tokens(&self) -> i64 {
        self.input_tokens_details
            .as_ref()
            .map_or(0, |details| details.cached_tokens)
    }
}

/// The breakdown of the input token count.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
pub struct InputTokensDetails {
    #[serde(default, deserialize_with = "null_as_default")]
    pub cached_tokens: i64,
}

/// Translates a neutral request into the Responses API body.
///
/// The system prompt becomes `instructions` rather than an input item. User
/// and context messages become `message` items with `input_text` content.
/// An assistant message contributes an `output_text` message item when its
/// text is non-empty, followed by one `function_call` item per tool-call
/// block. A tool message expands into one `function_call_output` item per
/// tool-result block; its other blocks are dropped.
pub fn build_request(request: &Request) -> WireRequest {
    let mut input: Vec<WireItem> = Vec::new();
    for message in &request.messages {
        match message.role {
            Role::User => input.push(user_message_item(message)),
            Role::Context => input.push(message_item("user", "input_text", message.text())),
            Role::Assistant => {
                let text = message.text();
                if !text.is_empty() {
                    input.push(message_item("assistant", "output_text", text));
                }
                for block in &message.blocks {
                    if block.block_type != BlockType::ToolCall {
                        continue;
                    }
                    input.push(WireItem {
                        item_type: "function_call".into(),
                        call_id: block.tool_call_id.clone(),
                        name: block.tool_name.clone(),
                        arguments: block
                            .arguments
                            .as_ref()
                            .map(|raw| raw.get().to_owned())
                            .unwrap_or_default(),
                        ..WireItem::default()
                    });
                }
            }
            Role::Tool => {
                for block in &message.blocks {
                    if block.block_type != BlockType::ToolResult {
                        continue;
                    }
                    input.push(WireItem {
                        item_type: "function_call_output".into(),
                        call_id: block.tool_call_id.clone(),
                        output: Some(block.text.clone()),
                        ..WireItem::default()
                    });
                }
            }
            Role::Other(_) => {}
        }
    }
    WireRequest {
        model: request.model.clone(),
        instructions: request.system_prompt.clone(),
        input,
        tools: request
            .tools
            .iter()
            .map(|tool| WireTool {
                tool_type: "function".into(),
                name: tool.name.clone(),
                description: tool.description.clone(),
                parameters: tool.parameters.clone(),
            })
            .collect(),
        reasoning: if request.thinking.is_empty() {
            None
        } else {
            Some(WireReasoning {
                effort: request.thinking.clone(),
            })
        },
        stream: true,
        store: false,
    }
}

/// Maps the terminal response state to the model enum.
///
/// A response that produced tool calls always finishes as
/// [`FinishReason::ToolCalls`]. Otherwise only the pair `status: "incomplete"`
/// with `incomplete_details.reason: "max_output_tokens"` means the output was
/// truncated; every other state is [`FinishReason::Stop`].
pub fn finish_reason(has_tool_calls: bool, status: &str, incomplete_reason: &str) -> FinishReason {
    if has_tool_calls {
        return FinishReason::ToolCalls;
    }
    if status == "incomplete" && incomplete_reason == "max_output_tokens" {
        return FinishReason::Length;
    }
    FinishReason::Stop
}

/// The byte length the request occupies on the wire.
pub fn serialized_request_size(request: &Request) -> Result<usize, serde_json::Error> {
    Ok(serde_json::to_vec(&build_request(request))?.len())
}

/// Reports whether the accumulated argument text is a complete JSON value,
/// the equivalent of Go's `json.Valid`.
pub fn valid_arguments(arguments: &str) -> bool {
    serde_json::from_str::<serde::de::IgnoredAny>(arguments).is_ok()
}

fn message_item(role: &str, content_type: &str, text: String) -> WireItem {
    WireItem {
        item_type: "message".into(),
        role: role.into(),
        content: vec![WireContent {
            content_type: content_type.into(),
            text,
            image_url: String::new(),
        }],
        ..WireItem::default()
    }
}

fn user_message_item(message: &Message) -> WireItem {
    let content = message
        .blocks
        .iter()
        .filter_map(|block| match block.block_type {
            BlockType::Text => Some(WireContent {
                content_type: "input_text".into(),
                text: block.text.clone(),
                image_url: String::new(),
            }),
            BlockType::Image => Some(WireContent {
                content_type: "input_image".into(),
                text: String::new(),
                image_url: format!("data:{};base64,{}", block.mime_type, block.data),
            }),
            _ => None,
        })
        .collect();
    WireItem {
        item_type: "message".into(),
        role: "user".into(),
        content,
        ..WireItem::default()
    }
}

/// Writes `null` for an empty input list, which is what Go's `encoding/json`
/// produces for the nil slice it builds in that case. The field has no
/// omit-empty tag, so it is always present.
fn serialize_input<S: Serializer>(input: &[WireItem], serializer: S) -> Result<S::Ok, S::Error> {
    if input.is_empty() {
        return serializer.serialize_none();
    }
    input.serialize(serializer)
}

/// Accepts JSON `null` for any field, the way Go leaves the zero value in
/// place.
fn null_as_default<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de> + Default,
{
    Ok(Option::<T>::deserialize(deserializer)?.unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Block, Message, Role, ToolDefinition};
    use serde_json::value::RawValue;

    fn raw(text: &str) -> Box<RawValue> {
        RawValue::from_string(text.to_owned()).expect("valid JSON")
    }

    fn encode(request: &Request) -> String {
        serde_json::to_string(&build_request(request)).expect("request serializes")
    }

    /// Port of `TestTranslateRequestEmptyToolResultKeepsOutput`. The Responses
    /// API requires every `function_call_output` item to carry an `output`
    /// field, even when the tool produced no text; dropping it yields HTTP 400
    /// "Missing required parameter: 'input[N].output'".
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn empty_tool_result_keeps_the_output_field() {
        let request = Request {
            model: "gpt-5".into(),
            messages: vec![Message {
                role: Role::Tool,
                blocks: vec![Block {
                    block_type: BlockType::ToolResult,
                    tool_call_id: "call_1".into(),
                    text: String::new(),
                    ..Block::default()
                }],
                ..Message::default()
            }],
            ..Request::default()
        };
        let item = serde_json::to_string(&build_request(&request).input[0]).expect("item encodes");
        assert_eq!(
            item,
            r#"{"type":"function_call_output","call_id":"call_1","output":""}"#
        );
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn empty_request_matches_go_encoding() {
        let request = Request {
            model: "m".into(),
            ..Request::default()
        };
        assert_eq!(
            encode(&request),
            r#"{"model":"m","input":null,"stream":true,"store":false}"#
        );
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn full_request_matches_go_encoding() {
        let request = Request {
            model: "test-model".into(),
            system_prompt: "sys".into(),
            thinking: "high".into(),
            messages: vec![
                Message {
                    role: Role::User,
                    blocks: vec![Block::text("hello")],
                    ..Message::default()
                },
                Message {
                    role: Role::Assistant,
                    blocks: vec![
                        Block::text("sure"),
                        Block {
                            block_type: BlockType::ToolCall,
                            tool_call_id: "call-1".into(),
                            tool_name: "read".into(),
                            arguments: Some(raw(r#"{"path":"README.md"}"#)),
                            ..Block::default()
                        },
                    ],
                    ..Message::default()
                },
                Message {
                    role: Role::Tool,
                    blocks: vec![Block {
                        block_type: BlockType::ToolResult,
                        text: "file body".into(),
                        tool_call_id: "call-1".into(),
                        tool_name: "read".into(),
                        ..Block::default()
                    }],
                    ..Message::default()
                },
            ],
            tools: vec![ToolDefinition {
                name: "read".into(),
                description: "read a file".into(),
                parameters: Some(raw(r#"{"alpha":{"a":3,"b":2},"zeta":1}"#)),
            }],
        };
        assert_eq!(
            encode(&request),
            concat!(
                r#"{"model":"test-model","instructions":"sys","input":["#,
                r#"{"type":"message","role":"user","content":[{"type":"input_text","text":"hello"}]},"#,
                r#"{"type":"message","role":"assistant","content":[{"type":"output_text","text":"sure"}]},"#,
                r#"{"type":"function_call","call_id":"call-1","name":"read","arguments":"{\"path\":\"README.md\"}"},"#,
                r#"{"type":"function_call_output","call_id":"call-1","output":"file body"}],"#,
                r#""tools":[{"type":"function","name":"read","description":"read a file","parameters":{"alpha":{"a":3,"b":2},"zeta":1}}],"#,
                r#""reasoning":{"effort":"high"},"stream":true,"store":false}"#
            )
        );
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn assistant_message_without_text_emits_only_its_tool_calls() {
        let request = Request {
            model: "m".into(),
            messages: vec![Message {
                role: Role::Assistant,
                blocks: vec![Block {
                    block_type: BlockType::ToolCall,
                    tool_call_id: "c1".into(),
                    tool_name: "read".into(),
                    arguments: Some(raw("{}")),
                    ..Block::default()
                }],
                ..Message::default()
            }],
            ..Request::default()
        };
        assert_eq!(
            encode(&request),
            concat!(
                r#"{"model":"m","input":[{"type":"function_call","call_id":"c1","name":"read","arguments":"{}"}],"#,
                r#""stream":true,"store":false}"#
            )
        );
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn context_messages_become_user_messages_and_tool_definitions_keep_null_parameters() {
        let request = Request {
            model: "m".into(),
            messages: vec![Message {
                role: Role::Context,
                context_type: "note".into(),
                blocks: vec![Block::text("recalled")],
                ..Message::default()
            }],
            tools: vec![ToolDefinition {
                name: "n".into(),
                description: "d".into(),
                parameters: None,
            }],
            ..Request::default()
        };
        assert_eq!(
            encode(&request),
            concat!(
                r#"{"model":"m","input":[{"type":"message","role":"user","content":[{"type":"input_text","text":"recalled"}]}],"#,
                r#""tools":[{"type":"function","name":"n","description":"d","parameters":null}],"#,
                r#""stream":true,"store":false}"#
            )
        );
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn user_images_become_response_content_parts() {
        let request = Request {
            model: "m".into(),
            messages: vec![Message {
                role: Role::User,
                blocks: vec![
                    Block::text("read it"),
                    Block::image("iVBORw0KGgo=", "image/png"),
                ],
                ..Message::default()
            }],
            ..Request::default()
        };
        assert_eq!(
            encode(&request),
            concat!(
                r#"{"model":"m","input":[{"type":"message","role":"user","content":["#,
                r#"{"type":"input_text","text":"read it"},"#,
                r#"{"type":"input_image","image_url":"data:image/png;base64,iVBORw0KGgo="}]}],"#,
                r#""stream":true,"store":false}"#
            )
        );
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn non_tool_result_blocks_of_a_tool_message_are_dropped() {
        let request = Request {
            model: "m".into(),
            messages: vec![Message {
                role: Role::Tool,
                blocks: vec![Block::text("ignored")],
                ..Message::default()
            }],
            ..Request::default()
        };
        assert_eq!(
            encode(&request),
            r#"{"model":"m","input":null,"stream":true,"store":false}"#
        );
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn finish_reason_covers_the_three_outcomes() {
        assert_eq!(
            finish_reason(true, "incomplete", "max_output_tokens"),
            FinishReason::ToolCalls
        );
        assert_eq!(
            finish_reason(false, "incomplete", "max_output_tokens"),
            FinishReason::Length
        );
        assert_eq!(
            finish_reason(false, "incomplete", "content_filter"),
            FinishReason::Stop
        );
        assert_eq!(finish_reason(false, "completed", ""), FinishReason::Stop);
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn serialized_request_size_is_the_encoded_byte_length() {
        let request = Request {
            model: "m".into(),
            ..Request::default()
        };
        assert_eq!(
            serialized_request_size(&request).expect("size"),
            encode(&request).len()
        );
        assert_eq!(serialized_request_size(&request).expect("size"), 54);
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn events_tolerate_null_fields_and_missing_objects() {
        let event: WireEvent = serde_json::from_str(
            r#"{"type":"response.output_text.delta","delta":null,"item_id":null,"item":null,"response":null}"#,
        )
        .expect("event decodes");
        assert_eq!(event.event_type, "response.output_text.delta");
        assert_eq!(event.delta, "");
        assert_eq!(event.item_id, "");
        assert_eq!(event.item, None);
        assert_eq!(event.response, None);
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn cached_tokens_default_to_zero_without_a_breakdown() {
        let usage: WireUsage =
            serde_json::from_str(r#"{"input_tokens":5,"output_tokens":7}"#).expect("usage decodes");
        assert_eq!(usage.cached_tokens(), 0);
        let detailed: WireUsage = serde_json::from_str(
            r#"{"input_tokens":5,"output_tokens":7,"input_tokens_details":{"cached_tokens":4}}"#,
        )
        .expect("usage decodes");
        assert_eq!(detailed.cached_tokens(), 4);
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn valid_arguments_matches_json_validity() {
        assert!(valid_arguments("{}"));
        assert!(valid_arguments(r#"{"a":1}"#));
        assert!(!valid_arguments("{"));
        assert!(!valid_arguments(""));
    }
}
