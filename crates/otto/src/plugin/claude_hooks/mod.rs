//! Claude Code hook format for `.claude-plugin` packages: configuration parsing and the
//! translation of Otto hook events, tool calls and command results to and from the shape
//! documented at <https://code.claude.com/docs/en/hooks.md>. Only `type: command` handlers
//! run. The runtime that executes them is `plugin::hooks`; nothing here performs process I/O.
pub mod manifest;
#[cfg(test)]
mod manifest_tests;
pub mod matcher;
#[cfg(test)]
mod matcher_tests;
pub mod payload;
#[cfg(test)]
mod payload_tests;
pub mod response;
#[cfg(test)]
mod response_tests;
pub mod tool_names;
#[cfg(test)]
mod tool_names_tests;

use serde::{Deserialize, Serialize};

/// One `command` handler of a Claude-format hook configuration, as stored in the plugin
/// registry. Stable field names: the registry file is read by older and newer binaries.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClaudeHook {
    /// Claude event name as configured, for example `PreToolUse`.
    pub event: String,
    /// Matcher text as configured; empty matches every value. See [`matcher::Matcher`].
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub matcher: String,
    /// Shell command line (no `args`) or executable path (with `args`), before substitution.
    pub command: String,
    /// Present only for the exec form: the process is `command` plus these arguments.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub args: Option<Vec<String>>,
    /// `sh` or `bash`; selects `/bin/sh -c` or `/bin/bash -c` for the shell form.
    #[serde(default = "default_shell", skip_serializing_if = "is_default_shell")]
    pub shell: String,
    /// Declared or defaulted timeout, already clamped to [`MAX_TIMEOUT_SECONDS`].
    pub timeout_seconds: u64,
}

pub(crate) fn default_shell() -> String {
    "sh".into()
}
fn is_default_shell(shell: &String) -> bool {
    shell == "sh"
}

/// Otto's cap on hook command timeouts; declared Claude timeouts above it are clamped.
pub(crate) const MAX_TIMEOUT_SECONDS: u64 = 300;
/// Cap on `additionalContext`, `systemMessage` and reasons taken from a hook (characters),
/// as in Claude Code.
pub(crate) const TEXT_LIMIT: usize = 10_000;
/// Cap on the large payload strings `tool_response.stdout`, `error` and
/// `last_assistant_message` (bytes) before the runtime's 1 MiB input limit applies.
pub(crate) const FIELD_LIMIT: usize = 256 << 10;

/// Claude events Otto fires, each with the Otto hook event that drives it. Every other
/// Claude event (`Notification`, `PermissionRequest`, ...) never fires; discovery reports it.
pub(crate) const EVENTS: [(&str, &str); 11] = [
    ("SessionStart", "session_start"),
    ("SubagentStart", "session_start"),
    ("UserPromptSubmit", "before_prompt"),
    ("PreToolUse", "before_tool"),
    ("PostToolUse", "after_tool"),
    ("PostToolUseFailure", "after_tool"),
    ("Stop", "turn_end"),
    ("SubagentStop", "turn_end"),
    ("PreCompact", "before_compaction"),
    ("PostCompact", "after_compaction"),
    ("SessionEnd", "session_end"),
];

/// The Otto hook event that drives `claude_event`, or `None` for an event Otto never fires.
pub(crate) fn otto_event(claude_event: &str) -> Option<&'static str> {
    EVENTS
        .iter()
        .find(|(claude, _)| *claude == claude_event)
        .map(|(_, otto)| *otto)
}

/// Default timeout when the handler declares none: 30 s on `UserPromptSubmit`, 10 s on
/// `SessionEnd`, 60 s otherwise.
pub(crate) fn default_timeout_seconds(claude_event: &str) -> u64 {
    match claude_event {
        "UserPromptSubmit" => 30,
        "SessionEnd" => 10,
        _ => 60,
    }
}

/// Truncates `text` to at most `limit` characters on a char boundary.
pub(crate) fn truncate_chars(text: &str, limit: usize) -> &str {
    match text.char_indices().nth(limit) {
        Some((end, _)) => &text[..end],
        None => text,
    }
}

/// Truncates `text` to at most `limit` bytes on a char boundary.
pub(crate) fn truncate_bytes(text: &str, limit: usize) -> &str {
    if text.len() <= limit {
        return text;
    }
    let mut end = limit;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}
