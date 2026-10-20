//! The user messages, tool-set notices and file lists a compaction checkpoint
//! carries forward.
//!
//! A summary is model output and can omit what the user said, which tool-set
//! changes the session announced, and which files the discarded tool calls
//! read and wrote. The text of the summarized user messages and
//! `capability_update` messages and those paths are therefore appended to the
//! summary verbatim as four tagged blocks. The blocks are stripped off an existing summary before
//! it is summarized again and rebuilt from the stored values, so the
//! summarizer never rewrites them and a second compaction does not duplicate
//! them.
//!
//! Ownership: every function takes borrowed input and returns owned data.
//!
//! Errors: only [`append_compaction_blocks`] can fail, with a message the
//! caller wraps in [`crate::agent::AgentError::InvalidCompactionSummary`].
//!
//! `normalize_detail_path` only rejects empty and control-bearing paths; a Rust
//! `&str` cannot hold invalid UTF-8, so there is no encoding check.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::value::RawValue;

use crate::model::{BlockType, Message, Role};
use crate::session::{CAPABILITY_UPDATE_CONTEXT_TYPE, CompactionDetails};

use super::redactor::encode_json_string;
use super::summary::SUMMARY_MAXIMUM_BYTES;
use super::summary_validate::normalize_summary_line_endings;

/// The largest number of paths the two file blocks may name together.
pub const FILE_DETAILS_MAXIMUM_PATHS: usize = 1_024;
/// The largest number of path bytes the two file blocks may hold together.
pub const FILE_DETAILS_MAXIMUM_BYTES: usize = 64 * 1024;
/// The largest number of encoded line bytes the `user-messages` block may hold.
pub const USER_MESSAGES_MAXIMUM_BYTES: usize = 16 * 1024;
/// How many characters of one user message the `user-messages` block keeps.
pub const USER_MESSAGE_MAXIMUM_RUNES: usize = 2_000;
/// Appended to a user message the `user-messages` block had to cut short.
pub const USER_MESSAGE_TRUNCATION_MARKER: &str = "[user message truncated for compaction]";

/// The largest number of encoded line bytes the `capability-updates` block may
/// hold.
pub const CAPABILITY_UPDATES_MAXIMUM_BYTES: usize = 16 * 1024;
/// How many characters of one notice the `capability-updates` block keeps.
pub const CAPABILITY_UPDATE_MAXIMUM_RUNES: usize = 8_000;
/// Appended to a notice the `capability-updates` block had to cut short.
pub const CAPABILITY_UPDATE_TRUNCATION_MARKER: &str =
    "[capability update truncated for compaction]";

/// Replaces the trailing blocks of `summary` with a `user-messages` block for
/// `user_messages`, a `capability-updates` block for `capability_updates`, a
/// `plan` block for the session plan in force, and the file blocks `details`
/// describes, and checks the result is nonempty and within bounds.
///
/// The `plan` block is a checklist ([`crate::session::plan_checklist`]) built
/// from the current plan each time, never carried from a previous summary:
/// the session's `otto.plan` records stay the source of truth.
pub fn append_compaction_blocks(
    summary: &str,
    user_messages: &[String],
    capability_updates: &[String],
    plan: &[crate::session::PlanStep],
    details: &CompactionDetails,
) -> Result<String, String> {
    let mut complete = strip_compaction_blocks(summary).to_owned();
    if complete.trim().is_empty() {
        return Err("complete compaction summary must be nonempty UTF-8".into());
    }
    for (tag, lines) in [
        ("user-messages", user_messages),
        ("capability-updates", capability_updates),
    ] {
        if lines.is_empty() {
            continue;
        }
        complete.push_str(&format!("\n\n<{tag}>\n"));
        for line in lines {
            complete.push_str(&encode_block_line(line));
            complete.push('\n');
        }
        complete.push_str(&format!("</{tag}>"));
    }
    if !plan.is_empty() {
        complete.push_str("\n\n<plan>\n");
        complete.push_str(&crate::session::plan_checklist(plan));
        complete.push_str("\n</plan>");
    }
    let suffix = compaction_file_blocks(details);
    if !suffix.is_empty() {
        complete.push_str("\n\n");
        complete.push_str(&suffix);
    }
    if complete.len() > SUMMARY_MAXIMUM_BYTES {
        return Err(format!(
            "complete compaction summary exceeds {SUMMARY_MAXIMUM_BYTES} bytes"
        ));
    }
    Ok(complete)
}

/// Renders the `read-files` and `modified-files` blocks, or an empty string
/// when neither list has paths.
pub fn compaction_file_blocks(details: &CompactionDetails) -> String {
    let mut suffix = String::new();
    for (tag, paths) in [
        ("read-files", &details.read_files),
        ("modified-files", &details.modified_files),
    ] {
        if paths.is_empty() {
            continue;
        }
        if !suffix.is_empty() {
            suffix.push_str("\n\n");
        }
        suffix.push('<');
        suffix.push_str(tag);
        suffix.push_str(">\n");
        for path in paths {
            suffix.push_str(path);
            suffix.push('\n');
        }
        suffix.push_str("</");
        suffix.push_str(tag);
        suffix.push('>');
    }
    suffix
}

/// Removes the blocks a previous compaction appended, so they are not
/// summarized as prose or duplicated. Blocks inside a fenced code block are
/// left alone.
pub fn strip_compaction_blocks(summary: &str) -> &str {
    let mut end = summary.len();
    while let Some(start) = trailing_compaction_block_start(&summary[..end]) {
        end = start;
    }
    if end == summary.len() || summary_position_inside_fence(summary, end + 2) {
        return summary;
    }
    &summary[..end]
}

/// Reads the user messages a previous compaction appended in its
/// `user-messages` block. A line that is not a JSON string is skipped.
pub fn compaction_user_messages(summary: &str) -> Vec<String> {
    compaction_block_lines(summary, "user-messages")
}

/// Reads the notices a previous compaction appended in its
/// `capability-updates` block.
pub fn compaction_capability_updates(summary: &str) -> Vec<String> {
    compaction_block_lines(summary, "capability-updates")
}

fn compaction_block_lines(summary: &str, tag: &str) -> Vec<String> {
    let opener = format!("\n\n<{tag}>\n");
    let blocks = &summary[strip_compaction_blocks(summary).len()..];
    let Some(start) = blocks.find(&opener) else {
        return Vec::new();
    };
    let content = &blocks[start + opener.len()..];
    let end = content
        .find(&format!("\n</{tag}>"))
        .unwrap_or(content.len());
    content[..end]
        .split('\n')
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}

/// Appends the text of the user messages in `summarized` to the ones a
/// previous compaction carried, and keeps the newest run whose encoded lines
/// fit in [`USER_MESSAGES_MAXIMUM_BYTES`]. A new message longer than
/// [`USER_MESSAGE_MAXIMUM_RUNES`] characters is cut and marked; a carried one
/// was cut when it was first carried.
pub fn carry_user_messages<'a>(
    previous: &[String],
    summarized: impl IntoIterator<Item = &'a Message>,
) -> Vec<String> {
    carry_block_lines(
        previous,
        summarized
            .into_iter()
            .filter(|message| message.role == Role::User),
        USER_MESSAGE_MAXIMUM_RUNES,
        USER_MESSAGE_TRUNCATION_MARKER,
        USER_MESSAGES_MAXIMUM_BYTES,
    )
}

/// The same for the `capability_update` context messages in `summarized`, with
/// [`CAPABILITY_UPDATE_MAXIMUM_RUNES`] and [`CAPABILITY_UPDATES_MAXIMUM_BYTES`].
pub fn carry_capability_updates<'a>(
    previous: &[String],
    summarized: impl IntoIterator<Item = &'a Message>,
) -> Vec<String> {
    carry_block_lines(
        previous,
        summarized.into_iter().filter(|message| {
            message.role == Role::Context && message.context_type == CAPABILITY_UPDATE_CONTEXT_TYPE
        }),
        CAPABILITY_UPDATE_MAXIMUM_RUNES,
        CAPABILITY_UPDATE_TRUNCATION_MARKER,
        CAPABILITY_UPDATES_MAXIMUM_BYTES,
    )
}

fn carry_block_lines<'a>(
    previous: &[String],
    summarized: impl Iterator<Item = &'a Message>,
    maximum_runes: usize,
    marker: &str,
    maximum_bytes: usize,
) -> Vec<String> {
    let mut messages = previous.to_vec();
    for message in summarized {
        let text = message.text();
        if text.trim().is_empty() {
            continue;
        }
        messages.push(match text.char_indices().nth(maximum_runes) {
            None => text,
            Some((end, _)) => format!("{}\n{marker}", &text[..end]),
        });
    }
    let mut remaining = maximum_bytes;
    let mut first_kept = messages.len();
    while first_kept > 0 {
        let line_bytes = encode_block_line(&messages[first_kept - 1]).len() + 1;
        if line_bytes > remaining {
            break;
        }
        remaining -= line_bytes;
        first_kept -= 1;
    }
    messages.split_off(first_kept)
}

/// Encodes one message as a single-line JSON string. `encode_json_string`
/// leaves DEL and the C1 controls as they are; they are escaped here too, so
/// the line passes [`valid_compaction_block_content`] and the block is
/// stripped again on the next compaction.
fn encode_block_line(text: &str) -> String {
    let mut line = String::new();
    for character in encode_json_string(text).chars() {
        if character.is_control() {
            line.push_str(&format!("\\u{:04x}", u32::from(character)));
        } else {
            line.push(character);
        }
    }
    line
}

fn trailing_compaction_block_start(summary: &str) -> Option<usize> {
    for tag in [
        "user-messages",
        "capability-updates",
        "plan",
        "read-files",
        "modified-files",
    ] {
        let closing = format!("\n</{tag}>");
        if !summary.ends_with(&closing) {
            continue;
        }
        let content_end = summary.len() - closing.len();
        let opener = format!("\n\n<{tag}>\n");
        let Some(start) = summary[..content_end].rfind(&opener) else {
            continue;
        };
        let content = &summary[start + opener.len()..content_end];
        if !valid_compaction_block_content(content) {
            continue;
        }
        return Some(start);
    }
    None
}

fn valid_compaction_block_content(content: &str) -> bool {
    if content.is_empty() {
        return false;
    }
    content
        .split('\n')
        .all(|path| !path.is_empty() && !path.chars().any(char::is_control))
}

/// Whether byte offset `position` sits inside an unclosed fenced code block.
/// `position` may point past the end, which reads the whole summary.
fn summary_position_inside_fence(summary: &str, position: usize) -> bool {
    let head = &summary[..position.min(summary.len())];
    let mut fence = super::summary_validate::FenceScanner::default();
    for line in normalize_summary_line_endings(head).split('\n') {
        fence.consume(line);
    }
    fence.is_open()
}

/// One recorded tool call awaiting its result.
struct FileToolCall {
    name: String,
    arguments: Option<Box<RawValue>>,
}

/// Extracts the file paths the discarded transcript read and modified.
///
/// A path counts only when a successful result is paired with the call that
/// asked for it, in the immediately following tool message. A path that was
/// written is not also reported as read.
pub fn derive_compaction_file_details(
    messages: &[Message],
    previous: &CompactionDetails,
) -> CompactionDetails {
    let mut reads: BTreeSet<String> = BTreeSet::new();
    let mut modified: BTreeSet<String> = BTreeSet::new();
    add_previous_detail_paths(&mut reads, &previous.read_files);
    add_previous_detail_paths(&mut modified, &previous.modified_files);

    let mut pending: BTreeMap<String, FileToolCall> = BTreeMap::new();
    let mut invalid_ids: BTreeSet<String> = BTreeSet::new();
    for message in messages {
        match message.role {
            Role::Assistant => {
                pending.clear();
                for block in &message.blocks {
                    if block.block_type != BlockType::ToolCall || block.tool_call_id.is_empty() {
                        continue;
                    }
                    if pending.remove(&block.tool_call_id).is_some() {
                        invalid_ids.insert(block.tool_call_id.clone());
                        continue;
                    }
                    if invalid_ids.contains(&block.tool_call_id) {
                        continue;
                    }
                    pending.insert(
                        block.tool_call_id.clone(),
                        FileToolCall {
                            name: block.tool_name.clone(),
                            arguments: block.arguments.clone(),
                        },
                    );
                }
            }
            Role::Tool => {
                for block in &message.blocks {
                    if block.block_type != BlockType::ToolResult {
                        continue;
                    }
                    let call = pending.remove(&block.tool_call_id);
                    let Some(call) = call else { continue };
                    if block.is_error || call.name != block.tool_name {
                        continue;
                    }
                    let arguments = call.arguments.as_ref().map_or("null", |raw| raw.get());
                    let Some(path) = file_path_from_tool_arguments(arguments) else {
                        continue;
                    };
                    match call.name.as_str() {
                        "read" => {
                            reads.insert(path);
                        }
                        "write" | "edit" => {
                            modified.insert(path);
                        }
                        _ => {}
                    }
                }
            }
            _ => pending.clear(),
        }
    }

    for path in &modified {
        reads.remove(path);
    }
    bound_compaction_file_details(&reads, &modified, previous)
}

fn add_previous_detail_paths(destination: &mut BTreeSet<String>, paths: &[String]) {
    for path in paths {
        if let Some(normalized) = normalize_detail_path(path) {
            destination.insert(normalized);
        }
    }
}

/// Reads the `path` member of a tool call's arguments. A missing, repeated, or
/// non-string member yields no path, so an ambiguous call is never recorded.
fn file_path_from_tool_arguments(arguments: &str) -> Option<String> {
    let members = decode_pair_preserving_object(arguments)?;
    let mut path = None;
    for (key, value) in members {
        if key != "path" {
            continue;
        }
        if path.is_some() {
            return None;
        }
        match value {
            serde_json::Value::String(value) => path = Some(value),
            _ => return None,
        }
    }
    normalize_detail_path(&path?)
}

/// Decodes a JSON object into its members in source order, keeping duplicate
/// keys, which `serde_json::Map` would collapse. Anything but an object, or
/// input with trailing content, yields `None`.
fn decode_pair_preserving_object(raw: &str) -> Option<Vec<(String, serde_json::Value)>> {
    use serde::Deserialize;
    use serde::de::{MapAccess, Visitor};

    struct Members(Vec<(String, serde_json::Value)>);

    impl<'de> Deserialize<'de> for Members {
        fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
            struct MembersVisitor;
            impl<'de> Visitor<'de> for MembersVisitor {
                type Value = Members;

                fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                    formatter.write_str("a JSON object")
                }

                fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Members, A::Error> {
                    let mut members = Vec::new();
                    while let Some(entry) = map.next_entry::<String, serde_json::Value>()? {
                        members.push(entry);
                    }
                    Ok(Members(members))
                }
            }
            deserializer.deserialize_map(MembersVisitor)
        }
    }

    serde_json::from_str::<Members>(raw).ok().map(|it| it.0)
}

/// Cleans a path the way Go's `filepath.Clean` does and rejects an empty or
/// control-bearing one.
pub fn normalize_detail_path(path: &str) -> Option<String> {
    if path.is_empty() || path.chars().any(char::is_control) {
        return None;
    }
    let cleaned = clean_path(path);
    if cleaned.is_empty() {
        return None;
    }
    Some(cleaned)
}

/// Go's `path/filepath.Clean` semantics for slash-separated paths, which is
/// the only separator Otto's file tools accept.
fn clean_path(path: &str) -> String {
    let rooted = path.starts_with('/');
    let mut segments: Vec<&str> = Vec::new();
    for segment in path.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                if segments.last().is_some_and(|last| *last != "..") {
                    segments.pop();
                } else if !rooted {
                    segments.push("..");
                }
            }
            other => segments.push(other),
        }
    }
    let joined = segments.join("/");
    match (rooted, joined.is_empty()) {
        (true, _) => format!("/{joined}"),
        (false, true) => ".".into(),
        (false, false) => joined,
    }
}

/// Applies the path-count and byte bounds, counting what did not fit.
/// Modified files are placed first, because they matter more to a resumed
/// turn than files that were only read.
fn bound_compaction_file_details(
    read_set: &BTreeSet<String>,
    modified_set: &BTreeSet<String>,
    previous: &CompactionDetails,
) -> CompactionDetails {
    let mut result = CompactionDetails {
        omitted_read_files: previous.omitted_read_files.max(0),
        omitted_modified_files: previous.omitted_modified_files.max(0),
        ..CompactionDetails::default()
    };
    let mut remaining_paths = FILE_DETAILS_MAXIMUM_PATHS;
    let mut remaining_bytes = FILE_DETAILS_MAXIMUM_BYTES;
    for path in modified_set {
        if remaining_paths == 0 || path.len() > remaining_bytes {
            result.omitted_modified_files = result.omitted_modified_files.saturating_add(1);
            continue;
        }
        remaining_paths -= 1;
        remaining_bytes -= path.len();
        result.modified_files.push(path.clone());
    }
    for path in read_set {
        if remaining_paths == 0 || path.len() > remaining_bytes {
            result.omitted_read_files = result.omitted_read_files.saturating_add(1);
            continue;
        }
        remaining_paths -= 1;
        remaining_bytes -= path.len();
        result.read_files.push(path.clone());
    }
    result
}

#[cfg(test)]
mod tests;
