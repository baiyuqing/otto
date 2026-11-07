//! Claude Messages request mapping. Unsupported content fails rather than being
//! silently dropped. Persisted reasoning is intentionally not replayed.
use super::output_budget::OutputBudget;
use crate::model::{BlockType, FinishReason, Role};
use crate::provider::{ProviderError, Request};
use serde::Serialize;
use serde_json::{json, value::RawValue};

#[derive(Debug, Clone, Serialize)]
pub struct WireRequest {
    pub model: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub system: String,
    pub messages: Vec<WireMessage>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<WireTool>,
    pub stream: bool,
    pub max_tokens: u32,
}
#[derive(Debug, Clone, Serialize)]
pub struct WireMessage {
    pub role: &'static str,
    pub content: Vec<WireBlock>,
}
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WireBlock {
    Text {
        text: String,
    },
    ToolUse {
        id: String,
        name: String,
        input: Box<RawValue>,
    },
    ToolResult {
        tool_use_id: String,
        content: String,
        is_error: bool,
    },
}
#[derive(Debug, Clone, Serialize)]
pub struct WireTool {
    pub name: String,
    pub description: String,
    pub input_schema: Box<RawValue>,
}
fn invalid() -> ProviderError {
    ProviderError::Other("unsupported or malformed Claude request content".into())
}
fn object(raw: &RawValue) -> bool {
    serde_json::from_str::<std::collections::BTreeMap<String, &RawValue>>(raw.get()).is_ok()
}
/// Builds a streamed Messages body. No thinking/cache/image or remote compaction
/// fields are emitted. Adjacent messages of the same role are coalesced.
pub fn build_request(request: &Request) -> Result<WireRequest, ProviderError> {
    if request.request_remote_compaction || request.remote_compaction.is_some() {
        return Err(invalid());
    }
    let mut messages: Vec<WireMessage> = Vec::new();
    for message in &request.messages {
        let role = match message.role {
            Role::Assistant => "assistant",
            Role::User | Role::Context | Role::Tool => "user",
            _ => return Err(invalid()),
        };
        let mut content = Vec::new();
        for block in &message.blocks {
            content.push(match block.block_type {
                BlockType::Text => WireBlock::Text {
                    text: block.text.clone(),
                },
                BlockType::Reasoning => continue,
                BlockType::ToolCall if role == "assistant" => {
                    let input = block
                        .arguments
                        .as_ref()
                        .filter(|raw| object(raw))
                        .ok_or_else(invalid)?;
                    if block.tool_call_id.is_empty() || block.tool_name.is_empty() {
                        return Err(invalid());
                    }
                    WireBlock::ToolUse {
                        id: block.tool_call_id.clone(),
                        name: block.tool_name.clone(),
                        input: input.clone(),
                    }
                }
                BlockType::ToolResult if role == "user" => {
                    if block.tool_call_id.is_empty() {
                        return Err(invalid());
                    }
                    WireBlock::ToolResult {
                        tool_use_id: block.tool_call_id.clone(),
                        content: block.text.clone(),
                        is_error: block.is_error,
                    }
                }
                _ => return Err(invalid()),
            });
        }
        if content.is_empty() {
            continue;
        }
        if let Some(previous) = messages.last_mut().filter(|previous| previous.role == role) {
            previous.content.extend(content);
        } else {
            messages.push(WireMessage { role, content });
        }
    }
    let mut tools = Vec::new();
    for tool in &request.tools {
        let schema = tool.parameters.clone().unwrap_or_else(|| {
            RawValue::from_string(json!({"type":"object","properties":{}}).to_string())
                .expect("static schema")
        });
        if tool.name.is_empty() || !object(&schema) {
            return Err(invalid());
        }
        tools.push(WireTool {
            name: tool.name.clone(),
            description: tool.description.clone(),
            input_schema: schema,
        });
    }
    Ok(WireRequest {
        model: request.model.clone(),
        system: request.system_prompt.clone(),
        messages,
        tools,
        stream: true,
        max_tokens: OutputBudget::for_model(&request.model).initial,
    })
}
/// Exact serialized byte count, sharing validation with the request encoder.
pub fn serialized_request_size(request: &Request) -> Result<usize, ProviderError> {
    serde_json::to_vec(&build_request(request)?)
        .map(|bytes| bytes.len())
        .map_err(|_| invalid())
}
/// Claude stop reasons are not OpenAI finish reasons. Unknown future values
/// remain successful responses with an unknown neutral finish reason.
pub fn finish_reason(reason: &str) -> FinishReason {
    match reason {
        "end_turn" | "stop_sequence" => FinishReason::Stop,
        "tool_use" => FinishReason::ToolCalls,
        "max_tokens" => FinishReason::Length,
        _ => FinishReason::Unknown,
    }
}
