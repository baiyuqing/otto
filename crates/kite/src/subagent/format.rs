//! Task rendering shared by the `agent_status` tool and the `/tasks` REPL
//! command.
//!
//! Every function here is pure. The column widths and the exact strings are
//! pinned by the tests, because this output goes straight to a terminal.

use chrono::{DateTime, TimeDelta, Utc};
use kite_core::model::{BlockType, Message, Role};

use super::tasks::{Task, TaskStatus};

/// One task in the layout shared with `agent_status`: id, name, status,
/// elapsed time, tool count, last tool (or the final token total) and a
/// description or prompt label. Trailing padding is trimmed.
pub fn task_line(task: &Task, now: DateTime<Utc>) -> String {
    let name = if task.agent.is_empty() {
        "(default)"
    } else {
        &task.agent
    };
    let line = format!(
        "{:<4} {:<10} {:<9} {:>6} {:>9}  {:<24} {}",
        task.id,
        name,
        task.status.as_str(),
        task_elapsed(task, now),
        task_tools_column(task),
        task_detail(task),
        task_label(task)
    );
    line.trim_end_matches(' ').to_string()
}

/// A task's elapsed time: empty while queued, the running duration to `now`,
/// or the finished duration. A task canceled while still queued never got a
/// start time; its elapsed time reads `"0s"` rather than measuring from the
/// zero time.
pub fn task_elapsed(task: &Task, now: DateTime<Utc>) -> String {
    match task.status {
        TaskStatus::Queued => String::new(),
        TaskStatus::Running => {
            let started = task.started_at.unwrap_or_default();
            round_to_seconds(now.signed_duration_since(started))
        }
        _ => {
            let Some(started) = task.started_at else {
                return "0s".to_string();
            };
            let finished = task.finished_at.unwrap_or_default();
            round_to_seconds(finished.signed_duration_since(started))
        }
    }
}

/// A task's tool-call count, `"1 tool"` or `"4 tools"`; empty while queued.
pub fn task_tools_column(task: &Task) -> String {
    match task.status {
        TaskStatus::Queued => String::new(),
        _ if task.tool_calls == 1 => "1 tool".to_string(),
        _ => format!("{} tools", task.tool_calls),
    }
}

/// A task's current activity, meaning its last tool, or once finished its
/// total token count; empty while queued.
pub fn task_detail(task: &Task) -> String {
    if task.status == TaskStatus::Queued {
        return String::new();
    }
    if task.is_final() {
        return format!(
            "{} tokens",
            comma_int(task.usage.input_tokens + task.usage.output_tokens)
        );
    }
    task.last_tool.clone()
}

/// A task's description, or the first 60 characters of its prompt collapsed
/// to one line when no description was given, prefixed by the task's name and
/// `": "` when it has one.
pub fn task_label(task: &Task) -> String {
    let mut label = if task.description.is_empty() {
        first_runes(&one_line(&task.prompt), 60)
    } else {
        task.description.clone()
    };
    if !task.name.is_empty() {
        label = format!("{}: {label}", task.name);
    }
    label
}

/// Collapses `value` to a single line, joining fields with one space. Splits on
/// Unicode whitespace, which `char::is_whitespace` matches.
pub fn one_line(value: &str) -> String {
    value.split_whitespace().collect::<Vec<&str>>().join(" ")
}

/// The first `count` characters of `value`, unchanged when it is no longer.
/// Counted in scalar values, not bytes.
pub fn first_runes(value: &str, count: usize) -> String {
    value.chars().take(count).collect()
}

/// Formats `value` with thousands separators, e.g. 12310 becomes `"12,310"`.
pub fn comma_int(value: i64) -> String {
    let digits = value.unsigned_abs().to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3 + 1);
    if value < 0 {
        out.push('-');
    }
    for (index, digit) in digits.char_indices() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            out.push(',');
        }
        out.push(digit);
    }
    out
}

/// A child's transcript as tool calls and assistant text, in order. Tool
/// results and the delegated prompt, which is a user message, are not shown,
/// matching `agent_status`'s step listing.
pub fn task_steps(history: &[Message]) -> String {
    let mut out = String::new();
    for message in history {
        for block in &message.blocks {
            if block.block_type == BlockType::ToolCall {
                let arguments = block.arguments.as_ref().map_or("", |raw| raw.get());
                out.push_str(&format!(
                    "  → {} {}\n",
                    block.tool_name,
                    first_runes(&one_line(arguments), 80)
                ));
            } else if block.block_type == BlockType::Text
                && message.role == Role::Assistant
                && !block.text.is_empty()
            {
                out.push_str(&format!("  assistant: {}\n", block.text));
            }
        }
    }
    out
}

/// The Go `time.Duration` spelling, rounded to the second, for the whole-second
/// durations this module produces: `"0s"`, `"42s"`, `"2m0s"`, `"1h0m0s"`.
/// Rounding is half away from zero.
pub(crate) fn round_to_seconds(delta: TimeDelta) -> String {
    let milliseconds = delta.num_milliseconds();
    let negative = milliseconds < 0;
    let total = (milliseconds.abs() + 500) / 1000;
    let (hours, minutes, seconds) = (total / 3600, (total / 60) % 60, total % 60);
    let sign = if negative && total != 0 { "-" } else { "" };
    if hours != 0 {
        format!("{sign}{hours}h{minutes}m{seconds}s")
    } else if minutes != 0 {
        format!("{sign}{minutes}m{seconds}s")
    } else {
        format!("{sign}{seconds}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kite_core::model::Block;
    use serde_json::value::RawValue;

    fn task(id: &str) -> Task {
        Task {
            id: id.to_string(),
            ..Task::default()
        }
    }

    #[test]
    fn a_running_task_fills_every_column() {
        let now = Utc::now();
        let mut running = task("t1");
        running.agent = "explorer".into();
        running.status = TaskStatus::Running;
        running.started_at = Some(now - TimeDelta::seconds(42));
        running.tool_calls = 4;
        running.last_tool = "grep \"session\"".into();
        running.prompt = "find where sessions are written".into();

        assert_eq!(
            task_line(&running, now),
            "t1   explorer   running      42s   4 tools  grep \"session\"           \
             find where sessions are written"
        );
    }

    #[test]
    fn a_queued_task_shows_the_default_agent_and_its_label() {
        let now = Utc::now();
        let mut queued = task("t2");
        queued.description = "review the diff".into();

        let line = task_line(&queued, now);

        assert!(line.contains("(default)"), "{line:?}");
        assert!(line.ends_with("review the diff"), "{line:?}");

        let mut named = queued.clone();
        named.name = "lint-check".into();
        assert_eq!(task_label(&named), "lint-check: review the diff");
        assert!(
            task_line(&named, now).ends_with("lint-check: review the diff"),
            "{line:?}"
        );
    }

    #[test]
    fn steps_render_tool_calls_and_assistant_text_only() {
        let history = [
            Message {
                role: Role::User,
                blocks: vec![Block {
                    block_type: BlockType::Text,
                    text: "review please".into(),
                    ..Block::default()
                }],
                ..Message::default()
            },
            Message {
                role: Role::Assistant,
                blocks: vec![Block {
                    block_type: BlockType::ToolCall,
                    tool_name: "read".into(),
                    arguments: Some(
                        RawValue::from_string(r#"{"path":"main.go"}"#.into()).expect("valid JSON"),
                    ),
                    ..Block::default()
                }],
                ..Message::default()
            },
            Message {
                role: Role::Tool,
                blocks: vec![Block {
                    block_type: BlockType::ToolResult,
                    text: "package main".into(),
                    ..Block::default()
                }],
                ..Message::default()
            },
            Message {
                role: Role::Assistant,
                blocks: vec![Block {
                    block_type: BlockType::Text,
                    text: "looks fine".into(),
                    ..Block::default()
                }],
                ..Message::default()
            },
        ];

        assert_eq!(
            task_steps(&history),
            "  → read {\"path\":\"main.go\"}\n  assistant: looks fine\n"
        );
    }

    /// The pieces the format tests exercise only indirectly: the thousands
    /// separator, the elapsed-time fallback for a task canceled while queued,
    /// and the duration spelling above one minute.
    #[test]
    fn the_column_helpers_match_the_go_duration_spellings() {
        assert_eq!(comma_int(0), "0");
        assert_eq!(comma_int(999), "999");
        assert_eq!(comma_int(12310), "12,310");
        assert_eq!(comma_int(-12310), "-12,310");
        assert_eq!(comma_int(1_000_000), "1,000,000");

        assert_eq!(first_runes("héllo", 3), "hél");
        assert_eq!(one_line("  a \n b  c "), "a b c");

        let mut canceled = task("t1");
        canceled.status = TaskStatus::Canceled;
        assert_eq!(task_elapsed(&canceled, Utc::now()), "0s");
        assert_eq!(task_detail(&canceled), "0 tokens");
        assert_eq!(task_tools_column(&canceled), "0 tools");

        let start = DateTime::from_timestamp(0, 0).expect("a valid instant");
        canceled.started_at = Some(start);
        canceled.finished_at = Some(start + TimeDelta::seconds(120));
        assert_eq!(task_elapsed(&canceled, Utc::now()), "2m0s");
        canceled.finished_at = Some(start + TimeDelta::milliseconds(3_661_500));
        assert_eq!(task_elapsed(&canceled, Utc::now()), "1h1m2s");
    }
}
