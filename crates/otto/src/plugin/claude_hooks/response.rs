//! Translation of a finished Claude hook command (exit code, stdout, stderr) to an Otto hook
//! [`Response`]. Exit codes and JSON fields follow the Claude Code hook reference; fields an
//! Otto event does not accept are never set (see `plugin::hooks::binding`).
use super::tool_names::to_otto;
use super::{TEXT_LIMIT, truncate_bytes, truncate_chars};
use otto_core::agent::hooks::{Decision, Response};
use serde_json::{Map, Value};
use std::path::Path;

/// Bytes of stderr kept in the notice for an exit code other than 0 and 2.
const EXIT_NOTICE_STDERR_BYTES: usize = 1024;

/// What the runtime knows about the Otto event a Claude hook ran for.
pub struct ResponseContext<'a> {
    pub workspace: &'a Path,
    /// Otto tool name of a `before_tool`/`after_tool` event (e.g. "edit", "mcp_call_tool").
    pub otto_tool: Option<&'a str>,
    /// Otto `arguments` of that tool call before this hook ran.
    pub original_arguments: Option<&'a Value>,
    /// Otto `content` of an `after_tool` event before this hook ran.
    pub original_content: Option<&'a str>,
}

/// Events whose hook can deny: the Otto event is a `before_*` gate.
fn is_pre_event(claude_event: &str) -> bool {
    matches!(
        claude_event,
        "PreToolUse" | "UserPromptSubmit" | "PreCompact"
    )
}

fn limited(text: &str) -> String {
    truncate_chars(text, TEXT_LIMIT).to_string()
}

fn deny(reason: &str) -> Response {
    Response {
        decision: Some(Decision::Deny),
        reason: Some(limited(reason)),
        ..Response::default()
    }
}

fn notice_only(text: &str) -> Response {
    Response {
        notice: Some(limited(text)),
        ..Response::default()
    }
}

/// Text of a `[hook]` line appended to the tool result `base`.
fn append_hook_line(base: &mut String, text: &str) {
    base.push_str("\n[hook] ");
    base.push_str(&limited(text));
}

/// Fallback reason of a denial by a pre event.
fn default_deny_reason(claude_event: &str) -> &'static str {
    match claude_event {
        "UserPromptSubmit" => "Hook blocked this prompt",
        "PreCompact" => "Hook blocked compaction",
        _ => "Hook blocked this tool",
    }
}

/// Otto's hook response for a finished Claude hook command. `exit_code` is the process exit
/// status; the caller handles signals and timeouts before calling this.
pub fn translate(
    claude_event: &str,
    exit_code: i32,
    stdout: &str,
    stderr: &str,
    context: &ResponseContext,
) -> Result<Response, String> {
    if super::otto_event(claude_event).is_none() {
        return Err(format!("unknown Claude hook event {claude_event}"));
    }
    let stderr = stderr.trim();
    match exit_code {
        2 => Ok(translate_exit_two(claude_event, stderr, context)),
        0 => translate_success(claude_event, stdout.trim(), context),
        code => {
            if claude_event == "SessionEnd" {
                return Ok(Response::default());
            }
            let stderr = truncate_bytes(stderr, EXIT_NOTICE_STDERR_BYTES);
            let text = if stderr.is_empty() {
                format!("hook exited {code}")
            } else {
                format!("hook exited {code}: {stderr}")
            };
            Ok(notice_only(&text))
        }
    }
}

/// Exit code 2: the hook blocks; stdout is ignored and stderr carries the reason.
fn translate_exit_two(claude_event: &str, stderr: &str, context: &ResponseContext) -> Response {
    if is_pre_event(claude_event) {
        let reason = if stderr.is_empty() {
            default_deny_reason(claude_event)
        } else {
            stderr
        };
        return deny(reason);
    }
    match claude_event {
        "PostToolUse" | "PostToolUseFailure" => {
            if stderr.is_empty() {
                return Response::default();
            }
            let mut content = context.original_content.unwrap_or("").to_string();
            append_hook_line(&mut content, stderr);
            Response {
                content: Some(content),
                ..Response::default()
            }
        }
        "SessionStart" | "SubagentStart" | "Stop" | "SubagentStop" => {
            notice_only(if stderr.is_empty() {
                "hook exited 2"
            } else {
                stderr
            })
        }
        _ => Response::default(),
    }
}

/// Exit code 0: stdout is empty, plain text or a JSON object.
fn translate_success(
    claude_event: &str,
    stdout: &str,
    context: &ResponseContext,
) -> Result<Response, String> {
    if stdout.is_empty() {
        return Ok(Response::default());
    }
    match serde_json::from_str::<Value>(stdout) {
        Ok(Value::Object(output)) => translate_json(claude_event, &output, context),
        _ => {
            if matches!(claude_event, "UserPromptSubmit" | "SessionStart") {
                Ok(Response {
                    context: Some(limited(stdout)),
                    ..Response::default()
                })
            } else {
                Ok(Response::default())
            }
        }
    }
}

fn string_field<'a>(object: &'a Map<String, Value>, key: &str) -> Option<&'a str> {
    object.get(key).and_then(Value::as_str)
}

/// A non-empty `additionalContext` string of `hookSpecificOutput`.
fn additional_context(specific: Option<&Map<String, Value>>) -> Option<&str> {
    specific
        .and_then(|specific| string_field(specific, "additionalContext"))
        .filter(|text| !text.is_empty())
}

fn translate_json(
    claude_event: &str,
    output: &Map<String, Value>,
    context: &ResponseContext,
) -> Result<Response, String> {
    if claude_event == "SessionEnd" {
        return Ok(Response::default());
    }
    let mut notices: Vec<String> = Vec::new();
    let mut response = Response::default();
    if output.get("continue").and_then(Value::as_bool) == Some(false) {
        let stop_reason = string_field(output, "stopReason");
        if is_pre_event(claude_event) {
            response = deny(stop_reason.unwrap_or("Hook stopped the session"));
        } else {
            notices.push(limited(
                stop_reason.unwrap_or("hook requested stop; not supported"),
            ));
        }
    } else {
        let specific = output.get("hookSpecificOutput").and_then(Value::as_object);
        match claude_event {
            "PreToolUse" => {
                translate_pre_tool_use(output, specific, context, &mut response, &mut notices)?
            }
            "PostToolUse" => translate_post_tool_use(output, specific, context, &mut response),
            "PostToolUseFailure" => {
                if let Some(text) = additional_context(specific) {
                    let mut content = context.original_content.unwrap_or("").to_string();
                    append_hook_line(&mut content, text);
                    response.content = Some(content);
                }
            }
            "UserPromptSubmit" => {
                if string_field(output, "decision") == Some("block") {
                    let reason = string_field(output, "reason")
                        .unwrap_or_else(|| default_deny_reason(claude_event));
                    response = deny(reason);
                }
                response.context = additional_context(specific).map(limited);
            }
            "SessionStart" => response.context = additional_context(specific).map(limited),
            "PreCompact" => {
                if string_field(output, "decision") == Some("block") {
                    let reason = string_field(output, "reason")
                        .unwrap_or_else(|| default_deny_reason(claude_event));
                    response = deny(reason);
                }
            }
            "Stop" | "SubagentStop" if string_field(output, "decision") == Some("block") => {
                let mut text = "hook requested continuation; not supported".to_string();
                if let Some(reason) = string_field(output, "reason") {
                    text.push_str(": ");
                    text.push_str(reason);
                }
                notices.push(limited(&text));
            }
            _ => {}
        }
    }
    if let Some(message) = string_field(output, "systemMessage").filter(|text| !text.is_empty()) {
        notices.push(limited(message));
    }
    if !notices.is_empty() {
        response.notice = Some(notices.join("\n"));
    }
    Ok(response)
}

fn translate_pre_tool_use(
    output: &Map<String, Value>,
    specific: Option<&Map<String, Value>>,
    context: &ResponseContext,
    response: &mut Response,
    notices: &mut Vec<String>,
) -> Result<(), String> {
    let permission_reason =
        specific.and_then(|specific| string_field(specific, "permissionDecisionReason"));
    let permission = specific.and_then(|specific| string_field(specific, "permissionDecision"));
    match permission {
        Some("allow") => response.decision = Some(Decision::Allow),
        Some("deny") => {
            *response = deny(permission_reason.unwrap_or("Hook denied this tool"));
        }
        Some("ask" | "defer") => {
            let mut reason = "hook requested confirmation; not supported".to_string();
            if let Some(extra) = permission_reason {
                reason.push_str(": ");
                reason.push_str(extra);
            }
            *response = deny(&reason);
            let tool_name = context.otto_tool.unwrap_or("tool");
            notices.push(limited(&format!(
                "hook requested confirmation for {tool_name}; Otto denied the call"
            )));
        }
        _ => match string_field(output, "decision") {
            Some("approve") => response.decision = Some(Decision::Allow),
            Some("block") => {
                *response = deny(string_field(output, "reason").unwrap_or("Hook denied this tool"));
            }
            _ => {}
        },
    }
    if response.decision != Some(Decision::Deny)
        && let Some(updated_input) = specific
            .and_then(|specific| specific.get("updatedInput"))
            .filter(|value| !value.is_null())
    {
        let (Some(otto_tool), Some(original_arguments)) =
            (context.otto_tool, context.original_arguments)
        else {
            return Err("hook updatedInput without a tool call".into());
        };
        response.arguments = Some(to_otto(
            otto_tool,
            original_arguments,
            updated_input,
            context.workspace,
        )?);
    }
    Ok(())
}

fn translate_post_tool_use(
    output: &Map<String, Value>,
    specific: Option<&Map<String, Value>>,
    context: &ResponseContext,
    response: &mut Response,
) {
    let mut content = context.original_content.unwrap_or("").to_string();
    let mut changed = false;
    if let Some(updated) = specific.and_then(|specific| string_field(specific, "updatedToolOutput"))
    {
        content = updated.to_string();
        changed = true;
    }
    if string_field(output, "decision") == Some("block")
        && let Some(reason) = string_field(output, "reason")
    {
        append_hook_line(&mut content, reason);
        changed = true;
    }
    if let Some(text) = additional_context(specific) {
        append_hook_line(&mut content, text);
        changed = true;
    }
    if changed {
        response.content = Some(content);
    }
}
