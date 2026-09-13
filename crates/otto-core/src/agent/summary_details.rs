//! The file lists a compaction checkpoint carries forward.
//!
//! Port of `internal/agent/summary_details.go`. A summary loses the tool calls
//! that read and wrote files, so the paths are extracted from the discarded
//! transcript and appended to the summary as two tagged blocks. The blocks are
//! also parsed back off an existing summary, so a second compaction does not
//! duplicate them.
//!
//! Ownership: every function takes borrowed input and returns owned data.
//!
//! Errors: only [`append_compaction_file_blocks`] can fail, with the Go error
//! text, which the caller wraps in
//! [`crate::agent::AgentError::InvalidCompactionSummary`].
//!
//! Deviations from Go, both because a Rust `&str` cannot hold invalid UTF-8:
//! the `utf8.ValidString` checks are gone, and `normalize_detail_path` only
//! rejects empty and control-bearing paths.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::value::RawValue;

use crate::model::{BlockType, Message, Role};
use crate::session::CompactionDetails;

use super::summary::SUMMARY_MAXIMUM_BYTES;
use super::summary_validate::normalize_summary_line_endings;

/// The largest number of paths the two blocks may name together.
pub const FILE_DETAILS_MAXIMUM_PATHS: usize = 1_024;
/// The largest number of path bytes the two blocks may hold together.
pub const FILE_DETAILS_MAXIMUM_BYTES: usize = 64 * 1024;

/// Replaces the trailing file blocks of `summary` with the ones `details`
/// describes, and checks the result is nonempty and within bounds.
pub fn append_compaction_file_blocks(
    summary: &str,
    details: &CompactionDetails,
) -> Result<String, String> {
    let mut complete = strip_compaction_file_blocks(summary).to_owned();
    if complete.trim().is_empty() {
        return Err("complete compaction summary must be nonempty UTF-8".into());
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

/// Removes the file blocks a previous compaction appended, so they are not
/// summarized as prose or duplicated. Blocks inside a fenced code block are
/// left alone.
pub fn strip_compaction_file_blocks(summary: &str) -> &str {
    let mut end = summary.len();
    while let Some(start) = trailing_compaction_file_block_start(&summary[..end]) {
        end = start;
    }
    if end == summary.len() || summary_position_inside_fence(summary, end + 2) {
        return summary;
    }
    &summary[..end]
}

fn trailing_compaction_file_block_start(summary: &str) -> Option<usize> {
    for tag in ["read-files", "modified-files"] {
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
        if !valid_compaction_file_block_content(content) {
            continue;
        }
        return Some(start);
    }
    None
}

fn valid_compaction_file_block_content(content: &str) -> bool {
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

/// Port of Go's `filepath.Clean` for slash-separated paths, which is the only
/// separator Otto's file tools accept.
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
mod tests {
    use crate::model::Block;

    use super::*;

    fn raw(json: &str) -> Box<RawValue> {
        RawValue::from_string(json.to_owned()).expect("valid JSON")
    }

    fn call(id: &str, name: &str, arguments: &str) -> Message {
        Message {
            role: Role::Assistant,
            blocks: vec![Block {
                block_type: BlockType::ToolCall,
                tool_name: name.into(),
                tool_call_id: id.into(),
                arguments: Some(raw(arguments)),
                ..Block::default()
            }],
            ..Message::default()
        }
    }

    fn result(id: &str, name: &str, is_error: bool) -> Message {
        Message {
            role: Role::Tool,
            blocks: vec![Block {
                block_type: BlockType::ToolResult,
                text: "ok".into(),
                tool_name: name.into(),
                tool_call_id: id.into(),
                is_error,
                ..Block::default()
            }],
            ..Message::default()
        }
    }

    #[test]
    fn paired_successful_calls_produce_the_two_path_lists() {
        let details = derive_compaction_file_details(
            &[
                call("c1", "read", r#"{"path":"./a.txt"}"#),
                result("c1", "read", false),
                call("c2", "write", r#"{"path":"b/../b.txt"}"#),
                result("c2", "write", false),
            ],
            &CompactionDetails::default(),
        );
        assert_eq!(details.read_files, ["a.txt"]);
        assert_eq!(details.modified_files, ["b.txt"]);
    }

    #[test]
    fn a_modified_path_is_not_also_reported_as_read() {
        let details = derive_compaction_file_details(
            &[
                call("c1", "read", r#"{"path":"a.txt"}"#),
                result("c1", "read", false),
                call("c2", "edit", r#"{"path":"a.txt"}"#),
                result("c2", "edit", false),
            ],
            &CompactionDetails::default(),
        );
        assert!(details.read_files.is_empty());
        assert_eq!(details.modified_files, ["a.txt"]);
    }

    #[test]
    fn unpaired_failed_and_mismatched_results_record_nothing() {
        let details = derive_compaction_file_details(
            &[
                call("c1", "read", r#"{"path":"failed.txt"}"#),
                result("c1", "read", true),
                call("c2", "read", r#"{"path":"mismatch.txt"}"#),
                result("c2", "write", false),
                call("c3", "read", r#"{"path":"orphan.txt"}"#),
                result("c9", "read", false),
            ],
            &CompactionDetails::default(),
        );
        assert!(details.read_files.is_empty(), "{:?}", details.read_files);
        assert!(details.modified_files.is_empty());
    }

    #[test]
    fn a_user_message_between_a_call_and_its_result_breaks_the_pair() {
        let details = derive_compaction_file_details(
            &[
                call("c1", "read", r#"{"path":"a.txt"}"#),
                Message {
                    role: Role::User,
                    blocks: vec![Block::text("wait")],
                    ..Message::default()
                },
                result("c1", "read", false),
            ],
            &CompactionDetails::default(),
        );
        assert!(details.read_files.is_empty());
    }

    #[test]
    fn ambiguous_arguments_record_nothing() {
        for arguments in [
            r#"{"path":"a.txt","path":"b.txt"}"#,
            r#"{"path":12}"#,
            r#"{"other":"a.txt"}"#,
            r#"["a.txt"]"#,
            "null",
            r#""a.txt""#,
            r#"{"path":""}"#,
        ] {
            let details = derive_compaction_file_details(
                &[call("c1", "read", arguments), result("c1", "read", false)],
                &CompactionDetails::default(),
            );
            assert!(
                details.read_files.is_empty(),
                "{arguments} should record nothing"
            );
        }
    }

    #[test]
    fn previous_paths_are_carried_forward_and_normalized() {
        let details = derive_compaction_file_details(
            &[],
            &CompactionDetails {
                read_files: vec!["./old.txt".into(), "bad\u{1}".into()],
                modified_files: vec!["dir/../kept.txt".into()],
                omitted_read_files: 2,
                omitted_modified_files: -5,
            },
        );
        assert_eq!(details.read_files, ["old.txt"]);
        assert_eq!(details.modified_files, ["kept.txt"]);
        assert_eq!(details.omitted_read_files, 2);
        assert_eq!(details.omitted_modified_files, 0);
    }

    #[test]
    fn the_path_count_bound_counts_what_it_dropped() {
        let mut messages = Vec::new();
        for index in 0..FILE_DETAILS_MAXIMUM_PATHS + 3 {
            let id = format!("c{index}");
            messages.push(call(&id, "read", &format!(r#"{{"path":"f{index:05}"}}"#)));
            messages.push(result(&id, "read", false));
        }
        let details = derive_compaction_file_details(&messages, &CompactionDetails::default());
        assert_eq!(details.read_files.len(), FILE_DETAILS_MAXIMUM_PATHS);
        assert_eq!(details.omitted_read_files, 3);
    }

    #[test]
    fn file_blocks_render_and_strip_round_trip() {
        let details = CompactionDetails {
            read_files: vec!["a.txt".into()],
            modified_files: vec!["b.txt".into()],
            ..CompactionDetails::default()
        };
        let complete = append_compaction_file_blocks("summary body", &details).expect("valid");
        assert_eq!(
            complete,
            "summary body\n\n<read-files>\na.txt\n</read-files>\n\n<modified-files>\nb.txt\n</modified-files>"
        );
        assert_eq!(strip_compaction_file_blocks(&complete), "summary body");

        let replaced =
            append_compaction_file_blocks(&complete, &CompactionDetails::default()).expect("valid");
        assert_eq!(replaced, "summary body");
    }

    #[test]
    fn an_empty_summary_is_rejected() {
        assert_eq!(
            append_compaction_file_blocks("  \n ", &CompactionDetails::default()).unwrap_err(),
            "complete compaction summary must be nonempty UTF-8"
        );
        let only_blocks = "\n\n<read-files>\na.txt\n</read-files>";
        assert_eq!(
            append_compaction_file_blocks(only_blocks, &CompactionDetails::default()).unwrap_err(),
            "complete compaction summary must be nonempty UTF-8"
        );
    }

    #[test]
    fn file_blocks_inside_an_open_fence_are_not_stripped() {
        let fenced = "body\n```\n\n<read-files>\na.txt\n</read-files>";
        assert_eq!(strip_compaction_file_blocks(fenced), fenced);
    }

    #[test]
    fn a_block_with_an_empty_or_control_path_is_not_stripped() {
        let broken = "body\n\n<read-files>\n\n</read-files>";
        assert_eq!(strip_compaction_file_blocks(broken), broken);
        let control = "body\n\n<read-files>\na\u{1}b\n</read-files>";
        assert_eq!(strip_compaction_file_blocks(control), control);
    }

    #[test]
    fn an_oversized_result_is_rejected() {
        let body = "x".repeat(SUMMARY_MAXIMUM_BYTES - 8);
        let details = CompactionDetails {
            read_files: vec!["a.txt".into()],
            ..CompactionDetails::default()
        };
        assert_eq!(
            append_compaction_file_blocks(&body, &details).unwrap_err(),
            format!("complete compaction summary exceeds {SUMMARY_MAXIMUM_BYTES} bytes")
        );
    }

    #[test]
    fn path_cleaning_matches_go() {
        for (input, expected) in [
            ("a/./b", "a/b"),
            ("a//b", "a/b"),
            ("a/b/../c", "a/c"),
            ("../a", "../a"),
            ("/../a", "/a"),
            ("./", "."),
            ("/", "/"),
        ] {
            assert_eq!(clean_path(input), expected, "clean {input}");
        }
        assert_eq!(normalize_detail_path("./"), Some(".".into()));
        assert_eq!(normalize_detail_path(""), None);
        assert_eq!(normalize_detail_path("a\nb"), None);
    }
}
