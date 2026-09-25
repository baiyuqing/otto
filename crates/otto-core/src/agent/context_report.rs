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
    /// The input tokens the provider reported for the last response.
    pub reported_input_tokens: Option<i64>,
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
}

/// Breaks `request` into sections, dropping empty ones.
///
/// `parts` are labeled pieces of the system prompt. They are used only when
/// they concatenate to it byte for byte; otherwise the prompt is one item.
/// `memory` is the previous turn's rendered recall.
pub fn sections(
    request: &Request,
    parts: &[(String, String)],
    memory: &str,
) -> Vec<ContextSection> {
    let prompt_items = if parts
        .iter()
        .map(|(_, text)| text.as_str())
        .collect::<String>()
        == request.system_prompt
    {
        parts
            .iter()
            .map(|(label, text)| item(label.clone(), estimate_string(text), text.clone()))
            .collect()
    } else {
        vec![item(
            "System prompt".into(),
            estimate_string(&request.system_prompt),
            request.system_prompt.clone(),
        )]
    };

    let (mut tools, mut mcp_tools) = (Vec::new(), Vec::new());
    for definition in &request.tools {
        let entry = item(
            definition.name.clone(),
            estimate_tool_definition(definition),
            serde_json::to_string_pretty(definition).unwrap_or_default(),
        );
        if definition.name.starts_with(MCP_TOOL_PREFIX) {
            mcp_tools.push(entry);
        } else {
            tools.push(entry);
        }
    }

    let (mut summaries, mut messages) = (Vec::new(), Vec::new());
    for (index, message) in request.messages.iter().enumerate() {
        let entry = item(
            message_label(index, message),
            estimate_message(message),
            message_text(message),
        );
        if message.role == Role::Context && message.context_type == COMPACTION_CONTEXT_TYPE {
            summaries.push(entry);
        } else {
            messages.push(entry);
        }
    }

    let memory_items = if memory.is_empty() {
        Vec::new()
    } else {
        vec![item(
            "Memory (last turn)".into(),
            estimate_string(memory),
            memory.to_owned(),
        )]
    };

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

fn item(label: String, tokens: i64, text: String) -> ContextItem {
    ContextItem {
        label,
        tokens,
        text,
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
