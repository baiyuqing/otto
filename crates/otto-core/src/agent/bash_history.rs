//! Projects Bash previews from older Otto transcripts into the model history.

use crate::model::{BlockType, Message, Role};

/// Keep Bash call/result pairing and the command's status. The old formatter
/// appended status after both streams, so text earlier in the result cannot
/// supply a forged status. Never change the stored transcript.
pub(super) fn summarize(messages: &mut [Message]) -> bool {
    let mut projected = false;
    for message in messages
        .iter_mut()
        .filter(|message| message.role == Role::Tool)
    {
        for block in &mut message.blocks {
            if block.block_type != BlockType::ToolResult
                || block.tool_name != "bash"
                || !(block.text.starts_with("stdout:\n") || block.text.contains("\nstdout:\n"))
                || !block.text.contains("\nstderr:\n")
            {
                continue;
            }
            let status = block.text.lines().last().unwrap_or_default();
            let status = status
                .split_once("; signal: ")
                .map_or(status, |(base, _)| base);
            let summary = if let Some(code) = status
                .strip_prefix("exit_code: ")
                .and_then(|code| code.parse::<i32>().ok())
            {
                format!("exit_code: {code}")
            } else if status == "status: cancelled" {
                "status: cancelled".to_owned()
            } else if status.starts_with("status: timed out after ") {
                "status: timed out".to_owned()
            } else {
                "status: unavailable".to_owned()
            };
            block.text = format!("{summary}\nHistorical Bash output omitted.");
            projected = true;
        }
    }
    projected
}
