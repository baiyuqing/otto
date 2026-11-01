//! Translation of an Otto hook event and its data into the Claude Code hook payload that is
//! written to a hook command's stdin. One Otto event produces at most one Claude event.
use super::tool_names::{ClaudeTool, to_claude};
use super::{EVENTS, FIELD_LIMIT, truncate_bytes};
use serde_json::{Map, Value, json};
use std::path::Path;

/// `agent_type` reported for every subagent (child) session.
const SUBAGENT_TYPE: &str = "otto-subagent";

/// Session identity of the running Otto session, as the hook runtime binds it.
pub struct SessionFields<'a> {
    pub session_id: &'a str,
    /// Set for a subagent (child) session only.
    pub parent_session_id: Option<&'a str>,
    pub task_id: &'a str,
    /// "new" or "resume".
    pub mode: &'a str,
    /// Otto's session history file; may be empty.
    pub history_path: &'a str,
}

pub struct ClaudePayload {
    /// Claude event name, one of the first elements of `super::EVENTS`.
    pub event: &'static str,
    /// The value the configured matcher is tested against; empty for events without a matcher.
    pub matcher_value: String,
    /// The JSON object written to the hook's stdin.
    pub body: Value,
}

/// The Claude event name from `EVENTS` equal to `name`.
fn claude_event_name(name: &str) -> &'static str {
    EVENTS
        .iter()
        .map(|(claude, _)| *claude)
        .find(|claude| *claude == name)
        .unwrap_or_else(|| unreachable!("{name} is not listed in EVENTS"))
}

fn string_field<'a>(data: &'a Value, key: &str) -> &'a str {
    data.get(key).and_then(Value::as_str).unwrap_or("")
}

fn limited_string(text: &str) -> String {
    truncate_bytes(text, FIELD_LIMIT).to_owned()
}

fn trigger_of(data: &Value) -> &'static str {
    if string_field(data, "reason") == "Manual" {
        "manual"
    } else {
        "auto"
    }
}

/// Text of the last assistant message in `data.turn.messages`, text blocks joined by "\n".
fn last_assistant_text(data: &Value) -> String {
    let last_assistant =
        data.pointer("/turn/messages")
            .and_then(Value::as_array)
            .and_then(|messages| {
                messages.iter().rev().find(|message| {
                    message.get("role").and_then(Value::as_str) == Some("assistant")
                })
            });
    let Some(message) = last_assistant else {
        return String::new();
    };
    let texts: Vec<&str> = message
        .get("blocks")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|block| block.get("type").and_then(Value::as_str) == Some("text"))
        .filter_map(|block| block.get("text").and_then(Value::as_str))
        .collect();
    limited_string(&texts.join("\n"))
}

fn tool_of(data: &Value, workspace: &Path) -> ClaudeTool {
    let no_arguments = json!({});
    let arguments = data.get("arguments").unwrap_or(&no_arguments);
    to_claude(string_field(data, "tool"), arguments, workspace)
}

/// The Claude payload for an Otto hook event, or `None` when no Claude event corresponds.
pub fn translate(
    otto_event: &str,
    data: &Value,
    session: &SessionFields,
    workspace: &Path,
) -> Option<ClaudePayload> {
    let is_child = session.parent_session_id.is_some();
    let mut fields = Map::new();
    let (event, matcher_value) = match otto_event {
        "session_start" if is_child => ("SubagentStart", SUBAGENT_TYPE.to_owned()),
        "session_start" => {
            let source = if session.mode == "new" {
                "startup"
            } else {
                "resume"
            };
            fields.insert("source".into(), json!(source));
            ("SessionStart", source.to_owned())
        }
        "before_prompt" => {
            fields.insert("prompt".into(), json!(string_field(data, "prompt")));
            ("UserPromptSubmit", String::new())
        }
        "before_tool" => {
            let tool = tool_of(data, workspace);
            fields.insert("tool_name".into(), json!(tool.name));
            fields.insert("tool_input".into(), tool.input);
            fields.insert(
                "tool_use_id".into(),
                json!(string_field(data, "tool_call_id")),
            );
            ("PreToolUse", tool.name)
        }
        "after_tool" => {
            let tool = tool_of(data, workspace);
            let content = limited_string(string_field(data, "content"));
            let event = if data.get("is_error").and_then(Value::as_bool) == Some(true) {
                fields.insert("error".into(), json!(content));
                fields.insert("is_interrupt".into(), json!(false));
                "PostToolUseFailure"
            } else {
                let response = if tool.name == "Bash" {
                    json!({"stdout": content, "stderr": "", "interrupted": false})
                } else {
                    json!({"content": content})
                };
                fields.insert("tool_response".into(), response);
                "PostToolUse"
            };
            fields.insert("tool_name".into(), json!(tool.name));
            fields.insert("tool_input".into(), tool.input);
            fields.insert(
                "tool_use_id".into(),
                json!(string_field(data, "tool_call_id")),
            );
            (event, tool.name)
        }
        "turn_end" => {
            if string_field(data, "status") != "success" {
                return None;
            }
            fields.insert("stop_hook_active".into(), json!(false));
            fields.insert(
                "last_assistant_message".into(),
                json!(last_assistant_text(data)),
            );
            if is_child {
                ("SubagentStop", SUBAGENT_TYPE.to_owned())
            } else {
                ("Stop", String::new())
            }
        }
        "before_compaction" => {
            let trigger = trigger_of(data);
            fields.insert("trigger".into(), json!(trigger));
            fields.insert(
                "custom_instructions".into(),
                json!(string_field(data, "focus")),
            );
            ("PreCompact", trigger.to_owned())
        }
        "after_compaction" => {
            let trigger = trigger_of(data);
            fields.insert("trigger".into(), json!(trigger));
            ("PostCompact", trigger.to_owned())
        }
        "session_end" => {
            fields.insert("reason".into(), json!("other"));
            ("SessionEnd", "other".to_owned())
        }
        _ => return None,
    };
    let event = claude_event_name(event);

    let mut body = Map::new();
    body.insert("session_id".into(), json!(session.session_id));
    if let Some(turn_id) = data.get("turn_id").and_then(Value::as_str) {
        body.insert("prompt_id".into(), json!(turn_id));
    }
    body.insert("transcript_path".into(), json!(session.history_path));
    body.insert("cwd".into(), json!(workspace.to_string_lossy()));
    body.insert("hook_event_name".into(), json!(event));
    if is_child {
        body.insert("agent_id".into(), json!(session.task_id));
        body.insert("agent_type".into(), json!(SUBAGENT_TYPE));
    }
    body.extend(fields);
    Some(ClaudePayload {
        event,
        matcher_value,
        body: Value::Object(body),
    })
}
