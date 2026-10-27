//! The evaluation log: session history flattened to one message per log id.

use serde::{Deserialize, Serialize};

use crate::model::{BlockType, Role};
use crate::session::HistoryMessage;

/// Characters kept at each end of a tool result.
pub const TOOL_RESULT_EDGE_CHARS: usize = 15_000;
/// Longest text of one log message other than a tool result, in characters.
pub const LOG_TEXT_MAXIMUM_CHARS: usize = 30_000;

/// What a log message holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LogKind {
    /// User text (an image is the text `[image]`).
    User,
    /// Assistant text, without reasoning.
    Assistant,
    /// One tool call: name and JSON arguments.
    ToolCall,
    /// One tool result.
    ToolResult,
}

impl LogKind {
    /// The snake_case name used in rendered lines and prompts.
    pub fn name(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Assistant => "assistant",
            Self::ToolCall => "tool_call",
            Self::ToolResult => "tool_result",
        }
    }
}

/// One entry of the log.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogMessage {
    /// Position in the log, from 0.
    pub id: u64,
    /// What the message holds.
    pub kind: LogKind,
    /// The message text.
    pub text: String,
    /// Id of the session entry the message came from.
    pub entry_id: String,
    /// RFC 3339 timestamp of that entry.
    pub timestamp: String,
}

/// Converts session history (oldest first) to log messages.
///
/// - user text and images: one `user` message; an image becomes `[image]`;
/// - assistant text: one `assistant` message; reasoning is dropped;
/// - each tool call: one `tool_call` message `name arguments`;
/// - each tool result: one `tool_result` message `tool_name[ (error)]` + newline + the
///   content, with the content clipped to its first and last 15,000 characters and a
///   marker that names the number of omitted characters;
/// - any other text over 30,000 characters is split into consecutive messages of at most
///   30,000 characters. Context-role messages are not logged.
pub fn log_from_history(history: &[HistoryMessage]) -> Vec<LogMessage> {
    let mut log = Vec::new();
    for item in history {
        let mut push = |kind: LogKind, text: String| {
            log.push(LogMessage {
                id: log.len() as u64,
                kind,
                text,
                entry_id: item.entry_id.clone(),
                timestamp: item.timestamp.clone(),
            });
        };
        match item.message.role {
            Role::User => {
                if let Some(text) = joined_text_parts(&item.message) {
                    for part in split_text(&text) {
                        push(LogKind::User, part);
                    }
                }
            }
            Role::Assistant => {
                if let Some(text) = joined_text_parts(&item.message) {
                    for part in split_text(&text) {
                        push(LogKind::Assistant, part);
                    }
                }
                for block in &item.message.blocks {
                    if block.block_type != BlockType::ToolCall {
                        continue;
                    }
                    let arguments = block.arguments.as_ref().map_or("{}", |raw| raw.get());
                    for part in split_text(&format!("{} {arguments}", block.tool_name)) {
                        push(LogKind::ToolCall, part);
                    }
                }
            }
            Role::Tool => {
                for block in &item.message.blocks {
                    if block.block_type != BlockType::ToolResult {
                        continue;
                    }
                    let error = if block.is_error { " (error)" } else { "" };
                    let text = format!(
                        "{}{error}\n{}",
                        block.tool_name,
                        clip_tool_result(&block.text)
                    );
                    push(LogKind::ToolResult, text);
                }
            }
            Role::Context | Role::Other(_) => {}
        }
    }
    log
}

fn joined_text_parts(message: &crate::model::Message) -> Option<String> {
    let parts: Vec<&str> = message
        .blocks
        .iter()
        .filter_map(|block| match block.block_type {
            BlockType::Text if !block.text.is_empty() => Some(block.text.as_str()),
            BlockType::Image => Some("[image]"),
            _ => None,
        })
        .collect();
    (!parts.is_empty()).then(|| parts.join("\n"))
}

fn clip_tool_result(text: &str) -> String {
    let total = text.chars().count();
    if total <= 2 * TOOL_RESULT_EDGE_CHARS {
        return text.to_owned();
    }
    let head_end = text
        .char_indices()
        .nth(TOOL_RESULT_EDGE_CHARS)
        .map_or(text.len(), |(at, _)| at);
    let tail_start = text
        .char_indices()
        .nth(total - TOOL_RESULT_EDGE_CHARS)
        .map_or(text.len(), |(at, _)| at);
    let omitted = total - 2 * TOOL_RESULT_EDGE_CHARS;
    format!(
        "{}\n[... {omitted} characters omitted ...]\n{}",
        &text[..head_end],
        &text[tail_start..]
    )
}

fn split_text(text: &str) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= LOG_TEXT_MAXIMUM_CHARS {
        return vec![text.to_owned()];
    }
    chars
        .chunks(LOG_TEXT_MAXIMUM_CHARS)
        .map(|chunk| chunk.iter().collect())
        .collect()
}
