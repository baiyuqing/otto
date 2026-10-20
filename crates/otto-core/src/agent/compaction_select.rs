//! Choosing what a compaction summarizes and what it keeps verbatim.
//!
//! The transcript is grouped into turns, the most recent turns are kept, and
//! everything before them becomes the summary source. A retained tail must
//! start on a protocol-safe boundary, so an assistant tool call is never
//! separated from its results.
//!
//! Ownership: the selector borrows the transcript and returns owned clones, so
//! the caller may redact them without touching the session.
//!
//! Errors: [`crate::agent::AgentError::NothingToCompact`] when no safe prefix
//! exists, [`crate::agent::AgentError::CurrentTurnTooLarge`] when what must be
//! kept already exceeds the budget, and [`crate::agent::AgentError::Other`] for
//! a malformed transcript.

use std::collections::HashMap;

use crate::model::{BlockType, Message, Role};
use crate::session::CompactionMetadata;

use super::AgentError;
use super::context_estimate::{estimate_message, saturating_add};
use super::summary_details::{
    compaction_capability_updates, compaction_user_messages, strip_compaction_blocks,
};

/// The prefix a compaction summary message carries in the transcript, so a
/// frontend can label it.
pub const COMPACTION_SUMMARY_DISPLAY_PREFIX: &str = "[Compaction summary]\n";

/// What one compaction will summarize and keep.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CompactionSelection {
    /// The summary already in force, with its trailing blocks removed.
    pub previous_summary: String,
    /// The user messages the summary in force carries verbatim, oldest first.
    pub previous_user_messages: Vec<String>,
    /// The notices the summary in force carries verbatim, oldest first.
    pub previous_capability_updates: Vec<String>,
    /// Messages before the current turn, summarized in structured mode.
    pub historical_source: Vec<Message>,
    /// The start of the current turn, summarized in turn-prefix mode.
    pub turn_prefix_source: Vec<Message>,
    /// Messages that stay in the transcript verbatim.
    pub retained: Vec<Message>,
    /// The id of `retained[0]`, which anchors the checkpoint.
    pub first_kept_id: String,
    /// Whether the current turn itself was split.
    pub split_turn: bool,
}

/// One turn: an optional run of context messages, its user or assistant
/// message, and any tool results that answer it.
#[derive(Debug, Clone, Copy)]
struct MessageGroup {
    start: usize,
    end: usize,
    primary: usize,
    tokens: i64,
}

/// Selects the compaction boundary.
///
/// `keep_recent_tokens` is how much recent transcript to try to keep;
/// `hard_input_budget` is the ceiling the retained part must not cross, or 0
/// for no ceiling.
pub fn select_compaction(
    messages: &[Message],
    latest: Option<&CompactionMetadata>,
    keep_recent_tokens: i64,
    hard_input_budget: i64,
) -> Result<CompactionSelection, AgentError> {
    let (transcript, previous_summary) = compaction_transcript(messages, latest);
    let selection = CompactionSelection {
        previous_summary: strip_compaction_blocks(&previous_summary).to_owned(),
        previous_user_messages: compaction_user_messages(&previous_summary),
        previous_capability_updates: compaction_capability_updates(&previous_summary),
        ..CompactionSelection::default()
    };

    validate_retained_tool_pairs(&transcript)
        .map_err(|error| AgentError::Other(format!("select compaction: {error}")))?;
    if transcript.is_empty() {
        return Err(AgentError::NothingToCompact);
    }

    if let Some(latest) = latest
        && latest.retained_tail_only
    {
        return select_after_retained_tail(
            &transcript,
            selection,
            latest,
            keep_recent_tokens,
            hard_input_budget,
        );
    }

    let groups = group_compaction_messages(&transcript)?;
    select_recent_compaction(
        &transcript,
        selection,
        &groups,
        keep_recent_tokens,
        hard_input_budget,
    )
}

/// The path taken when the active checkpoint already carries a synthetic
/// retained tail: the boundary must move forward from the recorded
/// post-checkpoint anchor rather than being rediscovered.
fn select_after_retained_tail(
    transcript: &[Message],
    selection: CompactionSelection,
    latest: &CompactionMetadata,
    keep_recent_tokens: i64,
    hard_input_budget: i64,
) -> Result<CompactionSelection, AgentError> {
    if latest.first_post_checkpoint_message_id.is_empty() {
        return Err(AgentError::NothingToCompact);
    }
    let Some(anchor) = transcript
        .iter()
        .position(|message| message.id == latest.first_post_checkpoint_message_id)
    else {
        return Err(AgentError::NothingToCompact);
    };
    let groups = group_compaction_messages(transcript)?;
    let mut retained_group = None;
    for (index, group) in groups.iter().enumerate() {
        if group.start == anchor {
            retained_group = Some(index);
        } else if group.start < anchor && anchor < group.end {
            retained_group = Some(index + 1);
        } else if group.start > anchor {
            retained_group = Some(index);
        }
        if retained_group.is_some() {
            break;
        }
    }
    let Some(retained_group) = retained_group.filter(|index| *index < groups.len()) else {
        return Err(AgentError::NothingToCompact);
    };
    if groups[retained_group].start == 0 {
        return select_recent_compaction(
            transcript,
            selection,
            &groups,
            keep_recent_tokens,
            hard_input_budget,
        );
    }

    let retained_start = groups[retained_group].start;
    if hard_input_budget > 0
        && select_message_estimate(&transcript[retained_start..]) > hard_input_budget
    {
        return Err(AgentError::CurrentTurnTooLarge);
    }
    let mut selection = selection;
    selection.retained = transcript[retained_start..].to_vec();
    selection.first_kept_id = selection.retained[0].id.clone();
    let selection = partition_compaction_source(
        transcript,
        &groups,
        retained_group,
        retained_start,
        selection,
    );
    validate_compaction_selection(&selection)?;
    Ok(selection)
}

/// The ordinary path: keep the most recent user turn, then walk backwards
/// while the retained estimate is under `keep_recent_tokens` and stays under
/// the hard budget.
fn select_recent_compaction(
    transcript: &[Message],
    selection: CompactionSelection,
    groups: &[MessageGroup],
    keep_recent_tokens: i64,
    hard_input_budget: i64,
) -> Result<CompactionSelection, AgentError> {
    if groups.is_empty() {
        return Err(AgentError::NothingToCompact);
    }
    let mut start = latest_user_group(transcript, groups).unwrap_or(groups.len() - 1);
    let mut retained_tokens = group_token_sum(&groups[start..]);
    let trailing_start = groups[groups.len() - 1].end;
    if trailing_start < transcript.len() {
        retained_tokens = saturating_add(
            retained_tokens,
            select_message_estimate(&transcript[trailing_start..]),
        );
    }
    if hard_input_budget > 0 && retained_tokens > hard_input_budget {
        return Err(AgentError::CurrentTurnTooLarge);
    }
    while start > 0 && retained_tokens < keep_recent_tokens {
        let candidate = saturating_add(retained_tokens, groups[start - 1].tokens);
        if hard_input_budget > 0 && candidate > hard_input_budget {
            break;
        }
        start -= 1;
        retained_tokens = candidate;
    }
    if start == 0 {
        return Err(AgentError::NothingToCompact);
    }

    let retained_start = groups[start].start;
    let mut selection = selection;
    selection.retained = transcript[retained_start..].to_vec();
    selection.first_kept_id = selection.retained[0].id.clone();
    let selection =
        partition_compaction_source(transcript, groups, start, retained_start, selection);
    if selection.historical_source.is_empty() && selection.turn_prefix_source.is_empty() {
        return Err(AgentError::NothingToCompact);
    }
    validate_compaction_selection(&selection)?;
    Ok(selection)
}

/// Splits the summarized prefix in two when the retained part starts on an
/// assistant message, so the user message that opened the same turn is
/// summarized separately and its context is not lost.
fn partition_compaction_source(
    transcript: &[Message],
    groups: &[MessageGroup],
    start_group: usize,
    retained_start: usize,
    mut selection: CompactionSelection,
) -> CompactionSelection {
    if transcript[groups[start_group].primary].role == Role::Assistant
        && let Some(turn_start) = preceding_user_group(transcript, groups, start_group)
    {
        let turn_start_message = groups[turn_start].start;
        selection.historical_source = transcript[..turn_start_message].to_vec();
        selection.turn_prefix_source = transcript[turn_start_message..retained_start].to_vec();
        selection.split_turn = !selection.turn_prefix_source.is_empty();
        return selection;
    }
    selection.historical_source = transcript[..retained_start].to_vec();
    selection
}

/// Removes compaction context messages from the transcript and recovers the
/// summary currently in force, trailing blocks included.
fn compaction_transcript(
    messages: &[Message],
    latest: Option<&CompactionMetadata>,
) -> (Vec<Message>, String) {
    let mut transcript = Vec::with_capacity(messages.len());
    let mut previous_summary = String::new();
    let mut found_latest = false;
    for message in messages {
        if message.role != Role::Context || message.context_type != "compaction" {
            transcript.push(message.clone());
            continue;
        }
        let is_latest = match latest {
            None => true,
            Some(latest) => latest.id.is_empty() || message.id == latest.id,
        };
        if is_latest {
            let text = message.text();
            let stripped = text
                .strip_prefix(COMPACTION_SUMMARY_DISPLAY_PREFIX)
                .unwrap_or(&text);
            previous_summary = stripped.to_owned();
            found_latest = true;
        }
    }
    if let Some(latest) = latest
        && !found_latest
    {
        previous_summary = latest.summary.clone();
    }
    (transcript, previous_summary)
}

/// Groups the transcript into turns. A leading run of context messages joins
/// the turn that follows it, and tool results join the assistant message that
/// called them.
fn group_compaction_messages(messages: &[Message]) -> Result<Vec<MessageGroup>, AgentError> {
    let mut groups = Vec::with_capacity(messages.len());
    let mut pending_context: Option<usize> = None;
    let mut index = 0;
    while index < messages.len() {
        let message = &messages[index];
        if message.role == Role::Context {
            pending_context.get_or_insert(index);
            index += 1;
            continue;
        }
        if message.role != Role::User && message.role != Role::Assistant {
            return Err(AgentError::Other(
                "select compaction: tool result is not attached to an assistant call".into(),
            ));
        }
        let start = pending_context.take().unwrap_or(index);
        let mut end = index + 1;
        if message.role == Role::Assistant && message.has_tool_call() {
            while end < messages.len() && messages[end].role == Role::Tool {
                end += 1;
            }
        }
        groups.push(MessageGroup {
            start,
            end,
            primary: index,
            tokens: select_message_estimate(&messages[start..end]),
        });
        index = end;
    }
    Ok(groups)
}

fn latest_user_group(messages: &[Message], groups: &[MessageGroup]) -> Option<usize> {
    (0..groups.len())
        .rev()
        .find(|index| messages[groups[*index].primary].role == Role::User)
}

fn preceding_user_group(
    messages: &[Message],
    groups: &[MessageGroup],
    before: usize,
) -> Option<usize> {
    (0..before)
        .rev()
        .find(|index| messages[groups[*index].primary].role == Role::User)
}

fn group_token_sum(groups: &[MessageGroup]) -> i64 {
    groups
        .iter()
        .fold(0, |total, group| saturating_add(total, group.tokens))
}

/// Sums the message estimates of a slice, saturating.
pub fn select_message_estimate(messages: &[Message]) -> i64 {
    messages.iter().fold(0, |total, message| {
        saturating_add(total, estimate_message(message))
    })
}

fn validate_compaction_selection(selection: &CompactionSelection) -> Result<(), AgentError> {
    for (name, messages) in [
        ("historical source", &selection.historical_source),
        ("turn-prefix source", &selection.turn_prefix_source),
        ("retained context", &selection.retained),
    ] {
        validate_retained_tool_pairs(messages).map_err(|error| {
            AgentError::Other(format!("select compaction: invalid {name}: {error}"))
        })?;
    }
    if selection.retained.is_empty() || selection.retained[0].role == Role::Tool {
        return Err(AgentError::Other(
            "select compaction: retained context has no protocol-safe start".into(),
        ));
    }
    if selection.first_kept_id.is_empty() {
        return Err(AgentError::NothingToCompact);
    }
    Ok(())
}

/// Checks that every tool call is answered by the tool message that directly
/// follows it, and that no other message type carries tool blocks. This is
/// what keeps a compacted transcript acceptable to the provider.
pub fn validate_retained_tool_pairs(messages: &[Message]) -> Result<(), String> {
    let mut pending: HashMap<&str, &str> = HashMap::new();
    let mut seen: HashMap<&str, ()> = HashMap::new();
    for message in messages {
        match &message.role {
            Role::Assistant => {
                if !pending.is_empty() {
                    return Err(
                        "unresolved tool calls must be followed immediately by results".into(),
                    );
                }
                for block in &message.blocks {
                    if block.block_type == BlockType::ToolResult {
                        return Err("assistant message contains a tool result".into());
                    }
                    if block.block_type != BlockType::ToolCall {
                        continue;
                    }
                    if block.tool_call_id.trim().is_empty() || block.tool_name.trim().is_empty() {
                        return Err("assistant tool call requires an id and name".into());
                    }
                    if seen.insert(&block.tool_call_id, ()).is_some() {
                        return Err(format!(
                            "duplicate tool-call id {}",
                            quote_go(&block.tool_call_id)
                        ));
                    }
                    pending.insert(&block.tool_call_id, &block.tool_name);
                }
            }
            Role::Tool => {
                if message.blocks.is_empty() {
                    return Err("tool message must contain a result".into());
                }
                for block in &message.blocks {
                    if block.block_type != BlockType::ToolResult {
                        return Err("tool message contains a non-result block".into());
                    }
                    let Some(name) = pending.remove(block.tool_call_id.as_str()) else {
                        return Err(format!(
                            "tool result {} has no pending call",
                            quote_go(&block.tool_call_id)
                        ));
                    };
                    if name != block.tool_name {
                        return Err(format!(
                            "tool result {} name does not match its call",
                            quote_go(&block.tool_call_id)
                        ));
                    }
                }
            }
            Role::User | Role::Context => {
                if !pending.is_empty() {
                    return Err(
                        "unresolved tool calls must be followed immediately by results".into(),
                    );
                }
                for block in &message.blocks {
                    if matches!(
                        block.block_type,
                        BlockType::ToolCall | BlockType::ToolResult
                    ) {
                        return Err(format!(
                            "tool blocks are incompatible with {} messages",
                            String::from(message.role.clone())
                        ));
                    }
                }
            }
            Role::Other(role) => {
                return Err(format!("unsupported message role {}", quote_go(role)));
            }
        }
    }
    if !pending.is_empty() {
        return Err("unresolved tool calls at end of context".into());
    }
    Ok(())
}

fn quote_go(value: &str) -> String {
    serde_json::to_string(value).expect("a string always encodes")
}

#[cfg(test)]
mod tests;
