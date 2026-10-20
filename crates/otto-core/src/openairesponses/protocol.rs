//! Responses API wire types and request translation.
//!
//! Field names, field order, and the omit-empty rules are fixed by what the
//! Responses endpoint accepts.
//!
//! A tool's schema is raw JSON passed through verbatim and keeps the caller's
//! key order rather than being re-sorted, the same decision as in
//! [`crate::openaicompat::protocol`].
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
    pub tools: Vec<WireToolEntry>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<WireReasoning>,
    pub stream: bool,
    pub store: bool,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub prompt_cache_key: String,
}

/// The reasoning request field, present only when the model asks for an
/// effort. `summary: "auto"` asks the backend to stream a reasoning summary,
/// which Otto shows and persists but never sends back.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WireReasoning {
    pub effort: String,
    pub summary: String,
}

/// One entry of the request `tools` array: a function Otto executes, or a
/// tool the backend hosts and runs itself.
#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
pub enum WireToolEntry {
    Function(WireTool),
    Hosted(WireHostedTool),
}

/// A backend-hosted tool, identified by its type alone. Otto never receives a
/// call for it: the backend runs it inside the response and the model's text
/// carries the outcome, so the stream reader ignores its output items.
#[derive(Debug, Clone, Serialize)]
pub struct WireHostedTool {
    #[serde(rename = "type")]
    pub tool_type: String,
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
/// `message`, `function_call`, `function_call_output`, and `compaction`.
///
/// A persisted compaction carries `id` and `encrypted_content`; a compaction
/// trigger carries only its type. Both opaque fields are omitted when empty.
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
    pub id: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub encrypted_content: String,
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
    #[serde(skip_serializing_if = "String::is_empty")]
    pub detail: String,
}

/// One decoded `data:` payload of the response stream. Most payloads repeat
/// the SSE `event:` name in their `type` field. The stream assembler fills an
/// empty `type` from the SSE field while preserving a non-empty JSON value.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct WireEvent {
    #[serde(rename = "type", default, deserialize_with = "null_as_default")]
    pub event_type: String,
    #[serde(default, deserialize_with = "null_as_default")]
    pub delta: String,
    #[serde(default, deserialize_with = "null_as_default")]
    pub item_id: String,
    /// The summary part a `response.reasoning_summary_text.delta` belongs to.
    #[serde(default, deserialize_with = "null_as_default")]
    pub summary_index: i64,
    #[serde(default)]
    pub item: Option<WireOutputItem>,
    #[serde(default)]
    pub response: Option<WireResult>,
    /// The error object of an `error` event, as the ChatGPT backend sends it.
    #[serde(default)]
    pub error: Option<WireError>,
    /// The code and message of an `error` event that carries them at the top
    /// level, as the public Responses API sends it.
    #[serde(default, deserialize_with = "null_as_default")]
    pub code: String,
    #[serde(default, deserialize_with = "null_as_default")]
    pub message: String,
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
    #[serde(default, deserialize_with = "null_as_default")]
    pub encrypted_content: String,
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
    /// Why a `response.failed` response failed.
    #[serde(default)]
    pub error: Option<WireError>,
}

/// The error object of a failed response.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct WireError {
    #[serde(default, deserialize_with = "null_as_default")]
    pub code: String,
    #[serde(rename = "type", default, deserialize_with = "null_as_default")]
    pub error_type: String,
    #[serde(default, deserialize_with = "null_as_default")]
    pub message: String,
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

    pub fn cached_tokens_present(&self) -> bool {
        self.input_tokens_details.is_some()
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
/// Context messages become `input_text`; user images become `input_image`
/// content with fixed high detail.
/// An assistant message contributes an `output_text` message item when its
/// text is non-empty, followed by one `function_call` item per tool-call
/// block. A tool message expands into one `function_call_output` item per
/// tool-result block; its other blocks are dropped.
pub fn build_request(request: &Request) -> WireRequest {
    build_request_with(request, false)
}

/// [`build_request`], additionally advertising the backend-hosted
/// `web_search` tool when `hosted_web_search` is set.
pub fn build_request_with(request: &Request, hosted_web_search: bool) -> WireRequest {
    let mut input: Vec<WireItem> = Vec::new();
    if let Some(compaction) = &request.remote_compaction {
        input.push(WireItem {
            item_type: "compaction".into(),
            id: compaction.id.clone(),
            encrypted_content: compaction.encrypted_content.clone(),
            ..WireItem::default()
        });
    }
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
    if request.request_remote_compaction {
        input.push(WireItem {
            item_type: "compaction".into(),
            ..WireItem::default()
        });
    }
    WireRequest {
        model: request.model.clone(),
        instructions: request.system_prompt.clone(),
        prompt_cache_key: request.cache_key.clone(),
        input,
        tools: request
            .tools
            .iter()
            .map(|tool| {
                WireToolEntry::Function(WireTool {
                    tool_type: "function".into(),
                    name: tool.name.clone(),
                    description: tool.description.clone(),
                    parameters: tool.parameters.clone(),
                })
            })
            .chain(hosted_web_search.then(|| {
                WireToolEntry::Hosted(WireHostedTool {
                    tool_type: "web_search".into(),
                })
            }))
            .collect(),
        reasoning: if request.thinking.is_empty() {
            None
        } else {
            Some(WireReasoning {
                effort: request.thinking.clone(),
                summary: "auto".into(),
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
    serialized_request_size_with(request, false)
}

/// [`serialized_request_size`] for a request built by [`build_request_with`].
pub fn serialized_request_size_with(
    request: &Request,
    hosted_web_search: bool,
) -> Result<usize, serde_json::Error> {
    Ok(serde_json::to_vec(&build_request_with(request, hosted_web_search))?.len())
}

/// Reports whether the accumulated argument text is a complete JSON value.
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
            detail: String::new(),
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
                detail: String::new(),
            }),
            BlockType::Image => Some(WireContent {
                content_type: "input_image".into(),
                text: String::new(),
                image_url: format!("data:{};base64,{}", block.mime_type, block.data),
                detail: "high".into(),
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

/// Writes `null` for an empty input list rather than `[]`. The field has no
/// omit-empty tag, so it is always present.
fn serialize_input<S: Serializer>(input: &[WireItem], serializer: S) -> Result<S::Ok, S::Error> {
    if input.is_empty() {
        return serializer.serialize_none();
    }
    input.serialize(serializer)
}

/// Accepts JSON `null` for any field, leaving the default value in place.
fn null_as_default<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de> + Default,
{
    Ok(Option::<T>::deserialize(deserializer)?.unwrap_or_default())
}

#[cfg(test)]
mod tests;
