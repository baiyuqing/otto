//! Claude Messages request mapping. Unsupported content fails rather than being
//! silently dropped. Persisted reasoning is intentionally not replayed.
//! Cache breakpoints sit on the system text, on the last content block of each
//! `Request::cache_boundaries` prefix and on the last content block (at most 4).
use super::output_budget::OutputBudget;
use crate::model::{BlockType, FinishReason, Role};
use crate::provider::{ProviderError, Request};
use serde::Serialize;
use serde_json::{json, value::RawValue};

/// Request encoding policy for an authorized Messages endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestDialect {
    Standard,
    OfficialClaude,
}

#[derive(Debug, Clone, Serialize)]
struct DisabledThinking {
    #[serde(rename = "type")]
    kind: &'static str,
}

#[derive(Debug, Clone, Serialize)]
pub struct WireRequest {
    pub model: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub system: Vec<SystemText>,
    pub messages: Vec<WireMessage>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<WireTool>,
    pub stream: bool,
    pub max_tokens: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    thinking: Option<DisabledThinking>,
}
#[derive(Debug, Clone, Copy, Serialize)]
pub struct CacheControl {
    #[serde(rename = "type")]
    kind: &'static str,
}
impl Default for CacheControl {
    fn default() -> Self {
        Self { kind: "ephemeral" }
    }
}
#[derive(Debug, Clone, Serialize)]
pub struct SystemText {
    #[serde(rename = "type")]
    kind: &'static str,
    text: String,
    cache_control: CacheControl,
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
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControl>,
    },
    ToolUse {
        id: String,
        name: String,
        input: Box<RawValue>,
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControl>,
    },
    ToolResult {
        tool_use_id: String,
        content: String,
        is_error: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControl>,
    },
}
#[derive(Debug, Clone, Serialize)]
pub struct WireTool {
    pub name: String,
    pub description: String,
    pub input_schema: Box<RawValue>,
}
impl WireBlock {
    fn set_cache_control(&mut self) {
        let (Self::Text { cache_control, .. }
        | Self::ToolUse { cache_control, .. }
        | Self::ToolResult { cache_control, .. }) = self;
        *cache_control = Some(CacheControl::default());
    }
}
/// Claude accepts at most 4 cache breakpoints per request.
const MAX_CACHE_BREAKPOINTS: usize = 4;
/// Appends the (message, block) position of the last emitted block unless it is
/// already recorded; positions stay in ascending order.
fn push_last_block_position(messages: &[WireMessage], positions: &mut Vec<(usize, usize)>) {
    let Some(last) = messages.last() else { return };
    let position = (messages.len() - 1, last.content.len() - 1);
    if positions.last() != Some(&position) {
        positions.push(position);
    }
}
fn invalid() -> ProviderError {
    ProviderError::Other("unsupported or malformed Claude request content".into())
}
fn object(raw: &RawValue) -> bool {
    serde_json::from_str::<std::collections::BTreeMap<String, &RawValue>>(raw.get()).is_ok()
}
/// Builds a streamed Messages body. Adjacent same-role messages are coalesced.
/// Cache breakpoints go on the system text, on the last emitted block of each
/// source-message prefix named in `Request::cache_boundaries` and on the last
/// block of the request; at most `MAX_CACHE_BREAKPOINTS` in total, dropping the
/// earliest boundaries first.
pub fn build_request(request: &Request) -> Result<WireRequest, ProviderError> {
    build_request_for(request, RequestDialect::Standard)
}

pub fn build_request_for(
    request: &Request,
    dialect: RequestDialect,
) -> Result<WireRequest, ProviderError> {
    if request.request_remote_compaction || request.remote_compaction.is_some() {
        return Err(invalid());
    }
    let mut messages: Vec<WireMessage> = Vec::new();
    let mut breakpoints: Vec<(usize, usize)> = Vec::new();
    for (source_index, message) in request.messages.iter().enumerate() {
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
                    cache_control: None,
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
                        cache_control: None,
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
                        cache_control: None,
                    }
                }
                _ => return Err(invalid()),
            });
        }
        if !content.is_empty() {
            if let Some(previous) = messages.last_mut().filter(|previous| previous.role == role) {
                previous.content.extend(content);
            } else {
                messages.push(WireMessage { role, content });
            }
        }
        if request.cache_boundaries.contains(&(source_index + 1)) {
            push_last_block_position(&messages, &mut breakpoints);
        }
    }
    push_last_block_position(&messages, &mut breakpoints);
    let system_breakpoints = usize::from(!request.system_prompt.is_empty());
    let keep = MAX_CACHE_BREAKPOINTS - system_breakpoints;
    for &(message_index, block_index) in &breakpoints[breakpoints.len().saturating_sub(keep)..] {
        messages[message_index].content[block_index].set_cache_control();
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
    let system = if request.system_prompt.is_empty() {
        Vec::new()
    } else {
        vec![SystemText {
            kind: "text",
            text: request.system_prompt.clone(),
            cache_control: CacheControl::default(),
        }]
    };
    Ok(WireRequest {
        model: request.model.clone(),
        system,
        messages,
        tools,
        stream: true,
        max_tokens: OutputBudget::for_model(&request.model).initial,
        // Haiku 5.5 defaults to thinking; use its supported disabled mode so
        // tool continuations do not require signed reasoning replay.
        thinking: match (dialect, request.model.as_str()) {
            (RequestDialect::OfficialClaude, "claude-haiku-5-5") => {
                Some(DisabledThinking { kind: "disabled" })
            }
            _ => None,
        },
    })
}
/// Exact serialized byte count, sharing validation with the request encoder.
pub fn serialized_request_size(request: &Request) -> Result<usize, ProviderError> {
    serialized_request_size_for(request, RequestDialect::Standard)
}

pub fn serialized_request_size_for(
    request: &Request,
    dialect: RequestDialect,
) -> Result<usize, ProviderError> {
    serde_json::to_vec(&build_request_for(request, dialect)?)
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
