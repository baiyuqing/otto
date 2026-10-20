//! The transcript reducer: turns session history into renderable entries.
//!
//! It operates on the native `otto_core::model::{Message, Block}` types rather
//! than the JSON-wire transcript in `otto_core::wire`, because that is what
//! `Controller::history()` returns and what a live turn's `Event` stream
//! describes incrementally.
//!
//! ponytail: an entry keeps no render cache: ratatui redraws the whole frame
//! every tick, so a cached rendering would have no reader. Upgrade path:
//! reintroduce it if profiling shows markdown rendering is a hot path at large
//! scrollback sizes.

use std::time::{Duration, Instant};

use otto_core::model::{
    Block, BlockType, EffectCertainty, Message, OperationDisposition, OperationStopReason, Role,
    Usage,
};
use otto_core::session::COMPACTION_CONTEXT_TYPE;

/// What one transcript entry renders as.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    User,
    Assistant,
    /// Model reasoning, drawn dimmed above the reply it precedes.
    Reasoning,
    Tool,
    Compaction,
    Error,
    System,
}

/// The prefix the session layer puts on a compaction summary's display text;
/// entries strip it before showing the summary body.
const COMPACTION_SUMMARY_DISPLAY_PREFIX: &str = "[Compaction summary]\n";

const TASK_NOTIFICATION_CONTEXT_TYPE: &str = "task_notification";

/// Number of report lines kept after a task notification's header line
/// before the rest is truncated with a "/task <id>" hint.
const NOTIFICATION_BODY_LINE_LIMIT: usize = 20;

/// One transcript entry. There is no render cache (see the module doc).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Entry {
    pub id: String,
    pub kind: Option<EntryKind>,
    pub raw: String,
    pub checkpoint_id: String,
    pub tokens_before: i64,
    pub tool_call_id: String,
    /// Highest live execution attempt observed for this logical invocation.
    pub tool_attempt: u32,
    /// Local monotonic observations only; never reconstructed from history.
    pub tool_started: Option<Instant>,
    pub tool_elapsed: Option<Duration>,
    pub tool_name: String,
    pub tool_args: String,
    pub tool_output: String,
    pub tool_error: bool,
    pub tool_done: bool,
    pub operation_id: Option<String>,
    pub disposition: Option<OperationDisposition>,
    pub effect_certainty: Option<EffectCertainty>,
    pub stop_reason: Option<OperationStopReason>,
}

impl Entry {
    pub fn execution_elapsed(&self, now: Instant) -> Option<Duration> {
        self.tool_elapsed.or_else(|| {
            self.tool_started
                .map(|start| now.saturating_duration_since(start))
        })
    }

    fn new(id: String, kind: EntryKind) -> Self {
        Self {
            id,
            kind: Some(kind),
            ..Self::default()
        }
    }
}

/// Fold one tool frame by Otto operation identity, not provider ID or arguments.
/// Legacy frames can only match pending provider calls; completed calls remain
/// distinct because legacy streams do not carry a replay-safe invocation key.
#[cfg(test)]
pub fn reduce_tool_event(entries: &mut Vec<Entry>, event: &otto_core::wire::events::WireEvent) {
    reduce_tool_event_at(entries, event, Some(Instant::now()));
}

/// Attach streams replay from frame zero without execution timestamps, so their
/// delivery intervals cannot establish tool execution duration.
pub fn reduce_tool_event_at(
    entries: &mut Vec<Entry>,
    event: &otto_core::wire::events::WireEvent,
    now: Option<Instant>,
) {
    let operation = (!event.operation_id.is_empty()).then_some(event.operation_id.as_str());
    let index = entries.iter().rposition(|e| {
        e.kind == Some(EntryKind::Tool)
            && match operation {
                Some(op) => e.operation_id.as_deref() == Some(op),
                None => !e.tool_done && e.tool_call_id == event.tool_call_id,
            }
    });
    let index = index.unwrap_or_else(|| {
        entries.push(Entry {
            id: operation
                .map(|op| format!("operation:{op}"))
                .unwrap_or_else(|| format!("streaming-tool-{}", entries.len())),
            kind: Some(EntryKind::Tool),
            tool_call_id: event.tool_call_id.clone(),
            tool_name: event.tool_name.clone(),
            operation_id: operation.map(str::to_owned),
            ..Entry::default()
        });
        entries.len() - 1
    });
    let entry = &mut entries[index];
    let attempt = event.attempt.unwrap_or(1);
    if attempt < entry.tool_attempt {
        return;
    }
    if event.event_type == "tool_call_started" {
        // Replayed starts cannot turn a completed attempt back into running.
        if entry.tool_done && (attempt == entry.tool_attempt || entry.tool_attempt == 0) {
            return;
        }
        if attempt > entry.tool_attempt {
            entry.tool_done = false;
            entry.tool_started = now;
            entry.tool_elapsed = None;
            entry.tool_error = false;
            entry.disposition = None;
            entry.effect_certainty = None;
            entry.stop_reason = None;
        }
        entry.tool_attempt = attempt;
        entry.tool_args = event
            .tool_args
            .as_deref()
            .map(|v| v.get().to_owned())
            .unwrap_or_default();
        entry.tool_name = event.tool_name.clone();
        entry.tool_call_id = event.tool_call_id.clone();
    } else if let Some(result) = &event.result {
        if entry.tool_done && entry.tool_attempt == 0 {
            return;
        }
        if !entry.tool_done || attempt > entry.tool_attempt {
            entry.tool_elapsed = entry
                .tool_started
                .zip(now)
                .filter(|_| attempt == entry.tool_attempt)
                .map(|(start, end)| end.saturating_duration_since(start));
            entry.tool_started = None;
        }
        entry.tool_attempt = attempt;
        entry.tool_output = result.content.clone();
        entry.tool_error = result.is_error;
        entry.tool_done = true;
        entry.disposition = result.disposition;
        entry.effect_certainty = result.effect_certainty;
        entry.stop_reason = result.stop_reason;
    }
}

/// Reduces a session's whole history into transcript entries plus the total
/// usage across every message.
pub fn entries_from_history(history: &[Message]) -> (Vec<Entry>, Usage) {
    let mut entries: Vec<Entry> = Vec::with_capacity(history.len());
    // Tool-call entry indexes awaiting their result, keyed by tool_call_id,
    // oldest first (a duplicated call id resolves in call order).
    let mut pending: std::collections::HashMap<String, Vec<usize>> =
        std::collections::HashMap::new();
    let mut usage = Usage::default();

    for (message_index, message) in history.iter().enumerate() {
        usage = add_usage_totals(usage, message.usage.as_ref());
        if message.role == Role::Context && !message.display {
            continue;
        }
        if let Some(entry) = compaction_entry_from_message(message, message_index) {
            entries.push(entry);
            continue;
        }
        if let Some(entry) = notification_entry_from_message(message, message_index) {
            entries.push(entry);
            continue;
        }

        let base_id = message_entry_base_id(message, message_index);
        let mut text_ordinal = 0usize;
        let mut text = String::new();
        let flush_text =
            |entries: &mut Vec<Entry>, text: &mut String, ordinal: &mut usize, force: bool| {
                if !force && text.is_empty() {
                    return;
                }
                entries.push(Entry::new(
                    format!("{base_id}-text-{ordinal}"),
                    entry_kind_for_role(&message.role),
                ));
                entries.last_mut().expect("just pushed").raw = std::mem::take(text);
                *ordinal += 1;
            };

        if message.blocks.is_empty() {
            flush_text(&mut entries, &mut text, &mut text_ordinal, true);
            continue;
        }

        for (block_index, block) in message.blocks.iter().enumerate() {
            match block.block_type {
                BlockType::Text => {
                    if message.role == Role::Tool {
                        flush_text(&mut entries, &mut text, &mut text_ordinal, false);
                        entries.push(orphan_tool_entry(&base_id, block_index, block, true));
                        continue;
                    }
                    text.push_str(&block.text);
                }
                BlockType::Reasoning => {
                    flush_text(&mut entries, &mut text, &mut text_ordinal, false);
                    entries.push(Entry {
                        raw: block.text.clone(),
                        ..Entry::new(
                            format!("{base_id}-reasoning-{block_index}"),
                            EntryKind::Reasoning,
                        )
                    });
                }
                BlockType::Image => {
                    flush_text(&mut entries, &mut text, &mut text_ordinal, false);
                    entries.push(Entry {
                        raw: "[image]".into(),
                        ..Entry::new(format!("{base_id}-image-{block_index}"), EntryKind::User)
                    });
                }
                BlockType::ToolCall => {
                    flush_text(&mut entries, &mut text, &mut text_ordinal, false);
                    let entry = Entry {
                        tool_call_id: block.tool_call_id.clone(),
                        tool_name: block.tool_name.clone(),
                        tool_args: raw_arguments(block),
                        operation_id: (!block.invocation_id.is_empty())
                            .then(|| block.invocation_id.clone()),
                        tool_attempt: block.invocation_attempt.unwrap_or(0),
                        ..Entry::new(format!("{base_id}-tool-{block_index}"), EntryKind::Tool)
                    };
                    entries.push(entry);
                    if !block.tool_call_id.is_empty() {
                        pending
                            .entry(block.tool_call_id.clone())
                            .or_default()
                            .push(entries.len() - 1);
                    }
                }
                BlockType::ToolResult => {
                    flush_text(&mut entries, &mut text, &mut text_ordinal, false);
                    if !pair_tool_result(&mut entries, &mut pending, block) {
                        entries.push(orphan_tool_entry(&base_id, block_index, block, true));
                    }
                }
                BlockType::Other(_) => {
                    if preserve_unknown_tool_block(&message.role, block) {
                        flush_text(&mut entries, &mut text, &mut text_ordinal, false);
                        entries.push(orphan_tool_entry(
                            &base_id,
                            block_index,
                            block,
                            !block.text.is_empty() || block.is_error,
                        ));
                        continue;
                    }
                    text.push_str(&block.text);
                }
            }
        }

        flush_text(&mut entries, &mut text, &mut text_ordinal, false);
    }

    // Result metadata is the persisted operation identity. Coalesce retries and
    // replayed results, never calls merely sharing provider IDs or arguments.
    let mut unique: Vec<Entry> = Vec::with_capacity(entries.len());
    for mut entry in entries {
        if let Some(op) = entry.operation_id.as_deref().filter(|op| !op.is_empty()) {
            entry.id = format!("operation:{op}");
            if let Some(prior) = unique
                .iter_mut()
                .find(|e| e.kind == Some(EntryKind::Tool) && e.operation_id.as_deref() == Some(op))
            {
                *prior = entry;
                continue;
            }
        }
        unique.push(entry);
    }
    (unique, usage)
}

fn raw_arguments(block: &Block) -> String {
    block
        .arguments
        .as_ref()
        .map(|raw| raw.get().to_string())
        .unwrap_or_default()
}

fn compaction_entry_from_message(message: &Message, index: usize) -> Option<Entry> {
    if message.role != Role::Context
        || message.context_type != COMPACTION_CONTEXT_TYPE
        || !message.display
    {
        return None;
    }
    let id = if message.id.is_empty() {
        message_entry_base_id(message, index)
    } else {
        message.id.clone()
    };
    Some(Entry {
        raw: message
            .text()
            .strip_prefix(COMPACTION_SUMMARY_DISPLAY_PREFIX)
            .map(str::to_string)
            .unwrap_or_else(|| message.text()),
        checkpoint_id: message.id.clone(),
        tokens_before: message.context_tokens_before.max(0),
        ..Entry::new(id, EntryKind::Compaction)
    })
}

/// Truncates a task notification's body (everything after the header line)
/// to [`NOTIFICATION_BODY_LINE_LIMIT`] lines, keeping the header intact.
fn notification_entry_text(task_id: &str, text: &str) -> String {
    let mut lines = text.split('\n');
    let Some(header) = lines.next() else {
        return text.to_string();
    };
    let body: Vec<&str> = lines.collect();
    if body.len() <= NOTIFICATION_BODY_LINE_LIMIT {
        return text.to_string();
    }
    let omitted = body.len() - NOTIFICATION_BODY_LINE_LIMIT;
    let mut kept: Vec<String> = Vec::with_capacity(NOTIFICATION_BODY_LINE_LIMIT + 2);
    kept.push(header.to_string());
    kept.extend(
        body[..NOTIFICATION_BODY_LINE_LIMIT]
            .iter()
            .map(|line| line.to_string()),
    );
    kept.push(format!("… ({omitted} more lines; /task {task_id})"));
    kept.join("\n")
}

/// Recovers the task id from a legacy persisted notification header
/// (`"[task-notification] task <id> ..."`). New messages carry the id in
/// `context_metadata` and do not depend on notification wording.
fn notification_task_id(text: &str) -> String {
    const PREFIX: &str = "[task-notification] task ";
    let header = text.split('\n').next().unwrap_or("");
    let Some(rest) = header.strip_prefix(PREFIX) else {
        return String::new();
    };
    rest.split(' ').next().unwrap_or("").to_string()
}

fn notification_entry_from_message(message: &Message, index: usize) -> Option<Entry> {
    if message.role != Role::Context
        || message.context_type != TASK_NOTIFICATION_CONTEXT_TYPE
        || !message.display
    {
        return None;
    }
    let id = if message.id.is_empty() {
        message_entry_base_id(message, index)
    } else {
        message.id.clone()
    };
    let text = message.text();
    let task_id = message
        .context_metadata
        .as_ref()
        .map(|metadata| metadata.task_id.clone())
        .filter(|id| !id.is_empty())
        .unwrap_or_else(|| notification_task_id(&text));
    Some(Entry {
        raw: notification_entry_text(&task_id, &text),
        ..Entry::new(id, EntryKind::System)
    })
}

fn entry_kind_for_role(role: &Role) -> EntryKind {
    match role {
        Role::User => EntryKind::User,
        Role::Assistant => EntryKind::Assistant,
        Role::Tool => EntryKind::Tool,
        Role::Other(name) if name == "error" => EntryKind::Error,
        _ => EntryKind::System,
    }
}

/// The wire label of an [`EntryKind`], used to synthesize ids such as
/// `message-2-system`.
fn entry_kind_label(kind: EntryKind) -> &'static str {
    match kind {
        EntryKind::User => "user",
        EntryKind::Assistant => "assistant",
        EntryKind::Reasoning => "reasoning",
        EntryKind::Tool => "tool",
        EntryKind::Compaction => "compaction",
        EntryKind::Error => "error",
        EntryKind::System => "system",
    }
}

/// Note the fallback (no message id) formats with the message's *entry kind*
/// label, not its role: a context message without an id falls back to
/// `message-N-system`, not `message-N-context`, because the kind-for-role
/// mapping has no `Context` arm.
fn message_entry_base_id(message: &Message, index: usize) -> String {
    if !message.id.is_empty() {
        return format!("message-{index}-{}", message.id);
    }
    format!(
        "message-{index}-{}",
        entry_kind_label(entry_kind_for_role(&message.role))
    )
}

fn preserve_unknown_tool_block(role: &Role, block: &Block) -> bool {
    *role == Role::Tool
        || !block.tool_call_id.is_empty()
        || !block.tool_name.is_empty()
        || block.arguments.is_some()
        || block.is_error
}

fn copy_tool_result_metadata(entry: &mut Entry, block: &Block) {
    entry.tool_attempt = block.invocation_attempt.unwrap_or(0);
    let Some(metadata) = block.operation_metadata.as_ref() else {
        return;
    };
    entry.operation_id = metadata.operation_id.as_ref().map(ToString::to_string);
    entry.disposition = Some(metadata.disposition);
    entry.effect_certainty = Some(metadata.effect_certainty);
    entry.stop_reason = metadata.stop_reason;
}

fn orphan_tool_entry(base_id: &str, block_index: usize, block: &Block, done: bool) -> Entry {
    let mut entry = Entry {
        tool_call_id: block.tool_call_id.clone(),
        tool_name: block.tool_name.clone(),
        tool_args: raw_arguments(block),
        tool_output: block.text.clone(),
        tool_error: block.is_error,
        tool_done: done,
        ..Entry::new(format!("{base_id}-tool-{block_index}"), EntryKind::Tool)
    };
    copy_tool_result_metadata(&mut entry, block);
    entry
}

/// Pairs a tool-result block with its earliest unmatched call, in call order.
fn pair_tool_result(
    entries: &mut [Entry],
    pending: &mut std::collections::HashMap<String, Vec<usize>>,
    block: &Block,
) -> bool {
    if block.tool_call_id.is_empty() {
        return false;
    }
    let Some(indexes) = pending.get_mut(&block.tool_call_id) else {
        return false;
    };
    if indexes.is_empty() {
        return false;
    }
    let entry_index = indexes.remove(0);
    if indexes.is_empty() {
        pending.remove(&block.tool_call_id);
    }
    let entry = &mut entries[entry_index];
    entry.tool_output = block.text.clone();
    entry.tool_error = block.is_error;
    entry.tool_done = true;
    copy_tool_result_metadata(entry, block);
    if entry.tool_name.is_empty() {
        entry.tool_name = block.tool_name.clone();
    }
    true
}

/// Saturating, negative-clamped accumulation. Saturating, non-negative addition
/// of usage totals, in the `i64` `otto_core::model::Usage` uses.
fn add_usage_totals(total: Usage, usage: Option<&Usage>) -> Usage {
    let Some(usage) = usage else { return total };
    Usage {
        input_tokens: saturating_add_non_negative(total.input_tokens, usage.input_tokens),
        output_tokens: saturating_add_non_negative(total.output_tokens, usage.output_tokens),
        cached_input_tokens: saturating_add_non_negative(
            total.cached_input_tokens,
            usage.cached_input_tokens,
        ),
    }
}

fn saturating_add_non_negative(total: i64, delta: i64) -> i64 {
    let total = total.max(0);
    if delta <= 0 {
        return total;
    }
    total.saturating_add(delta)
}

#[cfg(test)]
mod tests;
