//! Completion text and small text helpers for task results.

use super::*;

/// The notification and wait text for a final task.
pub fn completion_text(task: &Task, max_output_bytes: usize) -> String {
    let name = agent_label(task);
    let duration = completion_duration(task);
    let calls = pluralize_tool_calls(task.tool_calls);
    let model_segment = if task.model.is_empty() {
        String::new()
    } else {
        format!(" · {}", task.model)
    };

    match task.status {
        TaskStatus::Failed => format!(
            "[task-notification] task {} {name} failed{model_segment} · {duration} · {calls}\n{}",
            task.id, task.error
        ),
        TaskStatus::Unavailable => format!(
            "[task-notification] task {} {name} unavailable; no execution is running; resume unavailable\n{}",
            task.id, task.error
        ),
        TaskStatus::Interrupted => format!(
            "[task-notification] task {} {name} interrupted{model_segment} · {duration} · {calls}\n{}",
            task.id, task.error
        ),
        // Every non-final status plus canceled renders this way; only a final
        // task ever reaches this function.
        TaskStatus::Canceled | TaskStatus::Queued | TaskStatus::Running => format!(
            "[task-notification] task {} {name} canceled{model_segment} · {duration} · {calls}",
            task.id
        ),
        TaskStatus::Succeeded => {
            let tokens = comma_int(task.usage.input_tokens + task.usage.output_tokens);
            format!(
                "[task-notification] task {} {name} succeeded{model_segment} · {duration} · {calls} · {tokens} tokens\n{}",
                task.id,
                capped_text_result(&task.result, max_output_bytes).content
            )
        }
    }
}

/// The label shown after a task id: the parenthesized definition name, such as
/// `(default)` or `(explorer)`, prefixed by the task's name and a space when
/// it has one, such as `lint-check (explorer)`.
pub fn agent_label(task: &Task) -> String {
    let label = definition_label(task);
    if task.name.is_empty() {
        label
    } else {
        format!("{} {label}", task.name)
    }
}

/// The parenthesized definition name alone. The status table uses it for its
/// definition column, so a task name appears only once per line.
pub fn definition_label(task: &Task) -> String {
    if task.agent.is_empty() {
        "(default)".to_string()
    } else {
        format!("({})", task.agent)
    }
}

/// The finished duration rounded to the second, or `"0s"` for a task canceled
/// while still queued, which never got a start time.
fn completion_duration(task: &Task) -> String {
    let Some(started) = task.started_at else {
        return "0s".to_string();
    };
    let finished = task.finished_at.unwrap_or(started);
    round_to_seconds(finished.signed_duration_since(started))
}

/// The trimmed text of the last assistant message, or `""` when there is none.
pub(super) fn last_assistant_text(messages: &[Message]) -> String {
    messages
        .iter()
        .rev()
        .find(|message| message.role == Role::Assistant)
        .map_or(String::new(), |message| message.text().trim().to_string())
}

pub(crate) fn pluralize_tool_calls(count: i64) -> String {
    if count == 1 {
        "1 tool call".to_string()
    } else {
        format!("{count} tool calls")
    }
}

pub(crate) fn pluralize_tools(count: i64) -> String {
    if count == 1 {
        "1 tool".to_string()
    } else {
        format!("{count} tools")
    }
}

/// `value` unchanged when it has at most `max_chars` characters, otherwise its
/// first `max_chars - 1` characters plus `…`, so the result has exactly
/// `max_chars` characters.
pub(super) fn truncate_with_ellipsis(value: &str, max_chars: usize) -> String {
    if value.chars().count() <= max_chars {
        return value.to_string();
    }
    if max_chars == 0 {
        return String::new();
    }
    format!("{}…", first_runes(value, max_chars - 1))
}

/// The last `max_bytes` bytes of `value`, advanced to the next character
/// boundary so the result never starts mid-character.
pub(crate) fn cap_last_bytes(value: &str, max_bytes: usize) -> &str {
    if value.len() <= max_bytes {
        return value;
    }
    let mut start = value.len() - max_bytes;
    while start < value.len() && !value.is_char_boundary(start) {
        start += 1;
    }
    &value[start..]
}

/// Raw JSON with its insignificant whitespace removed. Invalid JSON falls back
/// to collapsing the raw text onto one line.
pub(crate) fn compact_json(raw: &str) -> String {
    match serde_json::from_str::<serde_json::Value>(raw)
        .ok()
        .as_ref()
        .map(serde_json::to_string)
    {
        Some(Ok(text)) => text,
        _ => one_line(raw),
    }
}

/// Tool call arguments as compact JSON capped at `max_chars` characters.
pub(super) fn compact_args_preview(raw: &str, max_chars: usize) -> String {
    truncate_with_ellipsis(&compact_json(raw), max_chars)
}

pub(super) fn first_non_empty(candidates: &[&str]) -> String {
    candidates
        .iter()
        .find(|candidate| !candidate.is_empty())
        .unwrap_or(&"")
        .to_string()
}
