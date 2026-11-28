//! What the next provider request contains, part by part.
//!
//! [`super::Agent::context_report`] builds the request with the same function
//! the run loop uses, so the report and the provider call cannot diverge. The
//! token numbers are [`super::context_estimate`] estimates, not tokenizer
//! counts.

use serde::{Deserialize, Serialize};

use crate::model::{BlockType, Message, Role};
use crate::provider::Request;
use crate::session::COMPACTION_CONTEXT_TYPE;

use super::context_estimate::{estimate_message, estimate_string, estimate_tool_definition};

/// The prefix the native crate gives every MCP tool name.
const MCP_TOOL_PREFIX: &str = "mcp__";

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextReport {
    pub model: String,
    /// The provider's hard input ceiling; 0 when not configured.
    pub context_window: i64,
    /// The estimate at which automatic compaction runs; 0 when it is off.
    pub compaction_threshold: i64,
    /// The estimate compaction compares against the threshold. It is anchored
    /// on the last provider-reported usage, so it can differ from the sum of
    /// the sections.
    pub estimated_total: i64,
    /// False when an opaque provider prefix has no post-checkpoint usage anchor.
    #[serde(default = "default_true")]
    pub estimate_complete: bool,
    /// The input tokens the provider reported for the last response.
    pub reported_input_tokens: Option<i64>,
    /// The projected next request, as [`super::Agent::context_report`] builds
    /// it now. Items carry a `position` except the previous turn's recall.
    pub sections: Vec<ContextSection>,
    /// The last ordinary request this agent handed to the provider, after
    /// provider hooks and the redaction guard; `None` before the first one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_sent: Option<SentRequest>,
}

/// An ordinary provider request as it was handed to the provider.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SentRequest {
    /// When the request was handed to the provider, in Unix milliseconds.
    pub sent_at: i64,
    /// Its parts. The request-local memory message is in the
    /// [`SectionKind::Memory`] section, not in [`SectionKind::Messages`].
    pub sections: Vec<ContextSection>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SectionKind {
    SystemPrompt,
    Tools,
    McpTools,
    CompactionSummary,
    /// The previous turn's memory recall. The next turn recalls again.
    Memory,
    Messages,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextSection {
    pub kind: SectionKind,
    /// The sum of the items' tokens.
    pub tokens: i64,
    pub items: Vec<ContextItem>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextItem {
    pub label: String,
    pub tokens: i64,
    /// The text sent to the provider for this item.
    pub text: String,
    /// The item's index in request order: system prompt parts, then tool
    /// definitions in `request.tools` order, then messages in
    /// `request.messages` order. `None` for an item that is not part of the
    /// request (the previous turn's recall in the projected sections).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub position: Option<u32>,
}

/// Where the memory item of a report comes from.
enum MemorySource<'a> {
    /// The previous turn's rendered recall, which the next request does not
    /// contain. Its item has no position.
    Projected(&'a str),
    /// The text of the request-local memory message that the request contains,
    /// as the run loop inserted it. The message that carries this text moves
    /// from the messages to the memory section and keeps its position. Nothing
    /// moves when no message carries it, for example when a hook or the
    /// redaction guard changed its text.
    InRequest(&'a str),
}

/// Breaks `request` into sections, dropping empty ones.
///
/// `parts` are labeled pieces of the static system prompt. If they match its
/// prefix, any request-local suffix is reported separately.
/// `memory` is the previous turn's rendered recall.
pub fn sections(
    request: &Request,
    parts: &[(String, String)],
    memory: &str,
) -> Vec<ContextSection> {
    build_sections(request, parts, &MemorySource::Projected(memory))
}

/// Breaks a request that was sent into sections. `memory_text` is the text of
/// the request-local memory message, empty when the step had none.
pub(super) fn sent_sections(
    request: &Request,
    parts: &[(String, String)],
    memory_text: &str,
) -> Vec<ContextSection> {
    build_sections(request, parts, &MemorySource::InRequest(memory_text))
}

fn build_sections(
    request: &Request,
    parts: &[(String, String)],
    memory: &MemorySource<'_>,
) -> Vec<ContextSection> {
    let prefix: String = parts.iter().map(|(_, text)| text.as_str()).collect();
    let suffix = (!parts.is_empty())
        .then(|| request.system_prompt.strip_prefix(&prefix))
        .flatten();
    let prompt_items = if let Some(suffix) = suffix {
        let mut items: Vec<_> = parts
            .iter()
            .enumerate()
            .map(|(index, (label, text))| {
                item(label.clone(), estimate_string(text), text.clone(), index)
            })
            .collect();
        if !suffix.is_empty() {
            items.push(item(
                "Request context".into(),
                estimate_string(suffix),
                suffix.into(),
                items.len(),
            ));
        }
        items
    } else {
        vec![item(
            "System prompt".into(),
            estimate_string(&request.system_prompt),
            request.system_prompt.clone(),
            0,
        )]
    };
    let tools_start = prompt_items.len();
    let messages_start = tools_start + request.tools.len();

    let (mut tools, mut mcp_tools) = (Vec::new(), Vec::new());
    for (index, definition) in request.tools.iter().enumerate() {
        let entry = item(
            definition.name.clone(),
            estimate_tool_definition(definition),
            serde_json::to_string_pretty(definition).unwrap_or_default(),
            tools_start + index,
        );
        if definition.name.starts_with(MCP_TOOL_PREFIX) {
            mcp_tools.push(entry);
        } else {
            tools.push(entry);
        }
    }

    let memory_index = match memory {
        MemorySource::InRequest(text) if !text.is_empty() => request
            .messages
            .iter()
            // The memory message is inserted before the current user message,
            // so it is the last message that carries the text.
            .rposition(|message| message.role == Role::User && message.text() == *text),
        _ => None,
    };
    let mut memory_items = match memory {
        MemorySource::Projected(text) if !text.is_empty() => vec![ContextItem {
            label: "Memory (last turn)".into(),
            tokens: estimate_string(text),
            text: (*text).to_owned(),
            position: None,
        }],
        _ => Vec::new(),
    };
    let (mut summaries, mut messages) = (Vec::new(), Vec::new());
    for (index, message) in request.messages.iter().enumerate() {
        let position = messages_start + index;
        if memory_index == Some(index) {
            memory_items.push(item(
                "Memory recall".into(),
                estimate_message(message),
                message_text(message),
                position,
            ));
            continue;
        }
        let entry = item(
            message_label(index, message),
            estimate_message(message),
            message_text(message),
            position,
        );
        if message.role == Role::Context && message.context_type == COMPACTION_CONTEXT_TYPE {
            summaries.push(entry);
        } else {
            messages.push(entry);
        }
    }

    [
        (SectionKind::SystemPrompt, prompt_items),
        (SectionKind::Tools, tools),
        (SectionKind::McpTools, mcp_tools),
        (SectionKind::CompactionSummary, summaries),
        (SectionKind::Memory, memory_items),
        (SectionKind::Messages, messages),
    ]
    .into_iter()
    .filter(|(_, items)| !items.is_empty())
    .map(|(kind, items)| ContextSection {
        kind,
        tokens: items
            .iter()
            .fold(0_i64, |total, item| total.saturating_add(item.tokens)),
        items,
    })
    .collect()
}

fn item(label: String, tokens: i64, text: String, position: usize) -> ContextItem {
    ContextItem {
        label,
        tokens,
        text,
        position: u32::try_from(position).ok(),
    }
}

/// `#<1-based position> <role>`, plus the tool name for a tool call or result.
fn message_label(index: usize, message: &Message) -> String {
    let role: String = message.role.clone().into();
    let mut label = format!("#{} {role}", index + 1);
    if let Some(block) = message.blocks.iter().find(|block| {
        matches!(
            block.block_type,
            BlockType::ToolCall | BlockType::ToolResult
        ) && !block.tool_name.is_empty()
    }) {
        label.push(' ');
        label.push_str(&block.tool_name);
    }
    label
}

/// Every block as text: tool calls as `name(arguments)`, images as a marker.
fn message_text(message: &Message) -> String {
    message
        .blocks
        .iter()
        .map(|block| match block.block_type {
            BlockType::ToolCall => format!(
                "{}({})",
                block.tool_name,
                block.arguments.as_ref().map_or("", |raw| raw.get())
            ),
            BlockType::Image => format!("[image {}]", block.mime_type),
            _ => block.text.clone(),
        })
        .collect::<Vec<_>>()
        .join("\n")
}
