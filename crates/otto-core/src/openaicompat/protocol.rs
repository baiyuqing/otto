//! Chat Completions wire types, request translation, and response decoding.
//!
//! Port of `internal/provider/openaicompat/protocol.go`. Field names, field
//! order, and the omit-empty rules reproduce Go's `encoding/json` output for
//! the same structs, so a request serialized here is byte-identical to the Go
//! request for the same [`Request`].
//!
//! Two differences are unavoidable and deliberate:
//!
//! - Go sorts the keys of `parameters` because it holds them in a `map`. Here
//!   the tool schema is raw JSON passed through verbatim, so its keys keep the
//!   order the caller supplied.
//! - Go writes `"messages":null` for a nil slice. [`serialize_messages`]
//!   reproduces that for an empty vector, because Go only ever produces a nil
//!   or a non-empty slice at that field.
//!
//! Ownership: [`build_request`] borrows the request and returns an owned wire
//! value. Decoding types own their data.
//!
//! Errors: only [`serialized_request_size`] can fail, and only if the request
//! does not serialize.

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::model::{BlockType, FinishReason, Message, Role};
use crate::provider::Request;

/// One `POST /chat/completions` body. Otto always streams, and always asks for
/// usage in the final chunk.
#[derive(Debug, Clone, Serialize)]
pub struct WireRequest {
    pub model: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub reasoning_effort: String,
    #[serde(serialize_with = "serialize_messages")]
    pub messages: Vec<WireMessage>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<WireTool>,
    pub stream: bool,
    pub stream_options: StreamOptions,
}

/// Streaming options. Otto sets `include_usage` so the last chunk carries the
/// token counts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct StreamOptions {
    pub include_usage: bool,
}

/// One request message. Text-only content stays a string; a user image turns
/// it into content parts with fixed high detail.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WireMessage {
    pub role: String,
    pub content: WireMessageContent,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<WireToolCall>,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub tool_call_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(untagged)]
pub enum WireMessageContent {
    Text(String),
    Parts(Vec<WireContentPart>),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WireContentPart {
    #[serde(rename = "type")]
    pub content_type: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image_url: Option<WireImageUrl>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WireImageUrl {
    pub url: String,
    pub detail: String,
}

/// One advertised tool.
#[derive(Debug, Clone, Serialize)]
pub struct WireTool {
    #[serde(rename = "type")]
    pub tool_type: String,
    pub function: WireFunction,
}

/// The function half of a tool definition. `parameters` is the caller's JSON
/// Schema, written verbatim, or `null` when absent.
#[derive(Debug, Clone, Serialize)]
pub struct WireFunction {
    pub name: String,
    pub description: String,
    pub parameters: Option<Box<serde_json::value::RawValue>>,
}

/// One tool call, on the request side complete and on the response side a
/// fragment identified by `index`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WireToolCall {
    #[serde(
        default,
        deserialize_with = "null_as_default",
        skip_serializing_if = "is_zero"
    )]
    pub index: i64,
    #[serde(
        default,
        deserialize_with = "null_as_default",
        skip_serializing_if = "String::is_empty"
    )]
    pub id: String,
    #[serde(
        rename = "type",
        default,
        deserialize_with = "null_as_default",
        skip_serializing_if = "String::is_empty"
    )]
    pub tool_type: String,
    #[serde(default)]
    pub function: WireToolCallFunction,
}

/// The name and the raw argument text of a tool call. `arguments` is a JSON
/// string whose content is itself JSON; it arrives in fragments.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WireToolCallFunction {
    #[serde(
        default,
        deserialize_with = "null_as_default",
        skip_serializing_if = "String::is_empty"
    )]
    pub name: String,
    #[serde(
        default,
        deserialize_with = "null_as_default",
        skip_serializing_if = "String::is_empty"
    )]
    pub arguments: String,
}

/// One decoded `data:` payload of the response stream.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct WireChunk {
    #[serde(default, deserialize_with = "null_as_default")]
    pub choices: Vec<WireChoice>,
    #[serde(default)]
    pub usage: Option<WireUsage>,
}

/// One choice of a chunk. Otto reads only the first stream of a response, but
/// iterates every choice the way Go does.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct WireChoice {
    #[serde(default)]
    pub delta: WireDelta,
    #[serde(default, deserialize_with = "null_as_default")]
    pub finish_reason: String,
}

/// The incremental content of one choice.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct WireDelta {
    #[serde(default, deserialize_with = "null_as_default")]
    pub content: String,
    #[serde(default, deserialize_with = "null_as_default")]
    pub tool_calls: Vec<WireToolCall>,
}

/// Token counts from the final chunk. Both the OpenAI and the DeepSeek spelling
/// of the cached-prompt count are accepted.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct WireUsage {
    #[serde(default, deserialize_with = "null_as_default")]
    pub prompt_tokens: i64,
    #[serde(default, deserialize_with = "null_as_default")]
    pub completion_tokens: i64,
    #[serde(default)]
    pub prompt_tokens_details: Option<PromptTokensDetails>,
    #[serde(default, deserialize_with = "null_as_default")]
    pub prompt_cache_hit_tokens: i64,
}

impl WireUsage {
    /// The cached prompt tokens: the OpenAI detail field when it is positive,
    /// otherwise the DeepSeek top-level field.
    pub fn cached_tokens(&self) -> i64 {
        match &self.prompt_tokens_details {
            Some(details) if details.cached_tokens > 0 => details.cached_tokens,
            _ => self.prompt_cache_hit_tokens,
        }
    }
}

/// The OpenAI breakdown of the prompt token count.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
pub struct PromptTokensDetails {
    #[serde(default, deserialize_with = "null_as_default")]
    pub cached_tokens: i64,
}

/// Translates a neutral request into the Chat Completions body.
///
/// The system prompt becomes the first `system` message when it is non-empty.
/// User and context messages become `user` messages carrying the concatenated
/// text of their text blocks. An assistant message carries its text plus one
/// wire tool call per tool-call block. A tool message expands into one `tool`
/// message per tool-result block; its other blocks are dropped.
pub fn build_request(request: &Request) -> WireRequest {
    let mut messages: Vec<WireMessage> = Vec::new();
    if !request.system_prompt.is_empty() {
        messages.push(wire_message("system", request.system_prompt.clone()));
    }
    for message in &request.messages {
        match message.role {
            Role::User => messages.push(user_wire_message(message)),
            Role::Context => messages.push(wire_message("user", message.text())),
            Role::Assistant => {
                let mut wire = wire_message("assistant", message.text());
                for block in &message.blocks {
                    if block.block_type != BlockType::ToolCall {
                        continue;
                    }
                    wire.tool_calls.push(WireToolCall {
                        index: 0,
                        id: block.tool_call_id.clone(),
                        tool_type: "function".into(),
                        function: WireToolCallFunction {
                            name: block.tool_name.clone(),
                            arguments: block
                                .arguments
                                .as_ref()
                                .map(|raw| raw.get().to_owned())
                                .unwrap_or_default(),
                        },
                    });
                }
                messages.push(wire);
            }
            Role::Tool => {
                for block in &message.blocks {
                    if block.block_type != BlockType::ToolResult {
                        continue;
                    }
                    messages.push(WireMessage {
                        role: "tool".into(),
                        content: WireMessageContent::Text(block.text.clone()),
                        tool_calls: Vec::new(),
                        tool_call_id: block.tool_call_id.clone(),
                    });
                }
            }
            Role::Other(_) => {}
        }
    }
    WireRequest {
        model: request.model.clone(),
        reasoning_effort: request.thinking.clone(),
        messages,
        tools: request
            .tools
            .iter()
            .map(|tool| WireTool {
                tool_type: "function".into(),
                function: WireFunction {
                    name: tool.name.clone(),
                    description: tool.description.clone(),
                    parameters: tool.parameters.clone(),
                },
            })
            .collect(),
        stream: true,
        stream_options: StreamOptions {
            include_usage: true,
        },
    }
}

/// Maps a wire finish reason to the model enum. Anything unrecognized, the
/// empty string included, becomes [`FinishReason::Unknown`].
pub fn finish_reason(wire: &str) -> FinishReason {
    match wire {
        "stop" => FinishReason::Stop,
        "tool_calls" => FinishReason::ToolCalls,
        "length" => FinishReason::Length,
        _ => FinishReason::Unknown,
    }
}

/// The byte length the request occupies on the wire. Phase 4 implements
/// `RequestSizer` with it.
pub fn serialized_request_size(request: &Request) -> Result<usize, serde_json::Error> {
    Ok(serde_json::to_vec(&build_request(request))?.len())
}

/// Reports whether the accumulated argument text is a complete JSON value,
/// the equivalent of Go's `json.Valid`.
pub fn valid_arguments(arguments: &str) -> bool {
    serde_json::from_str::<serde::de::IgnoredAny>(arguments).is_ok()
}

fn wire_message(role: &str, content: String) -> WireMessage {
    WireMessage {
        role: role.into(),
        content: WireMessageContent::Text(content),
        tool_calls: Vec::new(),
        tool_call_id: String::new(),
    }
}

fn user_wire_message(message: &Message) -> WireMessage {
    if !message
        .blocks
        .iter()
        .any(|block| block.block_type == BlockType::Image)
    {
        return wire_message("user", message.text());
    }
    let content = message
        .blocks
        .iter()
        .filter_map(|block| match block.block_type {
            BlockType::Text => Some(WireContentPart {
                content_type: "text".into(),
                text: block.text.clone(),
                image_url: None,
            }),
            BlockType::Image => Some(WireContentPart {
                content_type: "image_url".into(),
                text: String::new(),
                image_url: Some(WireImageUrl {
                    url: format!("data:{};base64,{}", block.mime_type, block.data),
                    detail: "high".into(),
                }),
            }),
            _ => None,
        })
        .collect();
    WireMessage {
        role: "user".into(),
        content: WireMessageContent::Parts(content),
        tool_calls: Vec::new(),
        tool_call_id: String::new(),
    }
}

/// Writes `null` for an empty message list, which is what Go's `encoding/json`
/// produces for the nil slice it builds in that case.
fn serialize_messages<S: Serializer>(
    messages: &[WireMessage],
    serializer: S,
) -> Result<S::Ok, S::Error> {
    if messages.is_empty() {
        return serializer.serialize_none();
    }
    messages.serialize(serializer)
}

/// Accepts JSON `null` for any field, the way Go leaves the zero value in
/// place. Providers send `"content":null` and `"finish_reason":null` on most
/// chunks.
fn null_as_default<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de> + Default,
{
    Ok(Option::<T>::deserialize(deserializer)?.unwrap_or_default())
}

fn is_zero(value: &i64) -> bool {
    *value == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Block, BlockType, Message, Role, ToolDefinition};
    use serde_json::value::RawValue;

    fn raw(text: &str) -> Box<RawValue> {
        RawValue::from_string(text.to_owned()).expect("valid JSON")
    }

    fn encode(request: &Request) -> String {
        serde_json::to_string(&build_request(request)).expect("request serializes")
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
            r#"{"model":"m","messages":null,"stream":true,"stream_options":{"include_usage":true}}"#
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
                r#"{"model":"test-model","reasoning_effort":"high","messages":["#,
                r#"{"role":"system","content":"sys"},"#,
                r#"{"role":"user","content":"hello"},"#,
                r#"{"role":"assistant","content":"sure","tool_calls":[{"id":"call-1","type":"function","function":{"name":"read","arguments":"{\"path\":\"README.md\"}"}}]},"#,
                r#"{"role":"tool","content":"file body","tool_call_id":"call-1"}],"#,
                r#""tools":[{"type":"function","function":{"name":"read","description":"read a file","parameters":{"alpha":{"a":3,"b":2},"zeta":1}}}],"#,
                r#""stream":true,"stream_options":{"include_usage":true}}"#
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
                r#"{"model":"m","messages":[{"role":"user","content":"recalled"}],"#,
                r#""tools":[{"type":"function","function":{"name":"n","description":"d","parameters":null}}],"#,
                r#""stream":true,"stream_options":{"include_usage":true}}"#
            )
        );
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn user_images_become_chat_content_parts() {
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
                r#"{"model":"m","messages":[{"role":"user","content":["#,
                r#"{"type":"text","text":"read it"},"#,
                r#"{"type":"image_url","image_url":{"url":"data:image/png;base64,iVBORw0KGgo=","detail":"high"}}]}],"#,
                r#""stream":true,"stream_options":{"include_usage":true}}"#
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
            r#"{"model":"m","messages":null,"stream":true,"stream_options":{"include_usage":true}}"#
        );
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn finish_reason_maps_the_four_wire_values() {
        assert_eq!(finish_reason("stop"), FinishReason::Stop);
        assert_eq!(finish_reason("tool_calls"), FinishReason::ToolCalls);
        assert_eq!(finish_reason("length"), FinishReason::Length);
        assert_eq!(finish_reason("content_filter"), FinishReason::Unknown);
        assert_eq!(finish_reason(""), FinishReason::Unknown);
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
        assert_eq!(serialized_request_size(&request).expect("size"), 83);
    }
}
