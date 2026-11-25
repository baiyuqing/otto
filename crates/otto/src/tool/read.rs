//! The `read` tool.
//!
//! Reads a UTF-8 text file from the workspace. A file that is not regular, is
//! larger than 64 MiB, contains a NUL byte, or is not valid UTF-8 is rejected
//! rather than truncated, so a device, a FIFO, or a binary never reaches the
//! model. Opening is non-blocking, so a FIFO fails instead of hanging.

use std::io::Read;
use std::path::Path;
use std::sync::Arc;

use otto_core::model::ToolDefinition;
use otto_core::tool::{ToolContentScope, ToolResult};
use serde::Deserialize;
use serde_json::json;
use serde_json::value::RawValue;
use tokio_util::sync::CancellationToken;

use super::result::{CappedByteCollector, RETRIEVAL_BYTES, decode_strict_json, valid_utf8_prefix};
use super::{Tool, definition, error_result, text_result};
use crate::workspace::confined::Workspace;

/// The largest file the read and edit tools will load, matching
/// `maxReadFileBytes`.
pub(crate) const MAX_READ_FILE_BYTES: u64 = 64 << 20;

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadArgs {
    #[serde(default)]
    path: String,
    #[serde(default)]
    offset: i64,
    #[serde(default)]
    limit: i64,
    #[serde(default)]
    character_offset: i64,
}

/// Reads a workspace file.
pub(crate) struct ReadTool {
    workspace: Arc<Workspace>,
    max_output_bytes: usize,
}

impl ReadTool {
    pub fn new(workspace: Arc<Workspace>, max_output_bytes: usize) -> Self {
        Self {
            workspace,
            max_output_bytes: max_output_bytes.min(RETRIEVAL_BYTES),
        }
    }
}

/// The schema advertised for `read`.
pub fn read_definition() -> ToolDefinition {
    definition(
        "read",
        "Read a UTF-8 workspace file, including persistent sub-agent reports and \
            indexes, or temporary Bash logs. For Bash logs, select only the needed lines \
            with offset and limit; the excerpt is available to the next model step. Returns at \
            most 4 KiB of selected text (or the smaller configured cap); truncation \
            gives character_offset to resume within the same line range. Returns an \
            error for unreadable, binary or oversized files.",
        json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Workspace-relative file path to read"
                },
                "offset": {
                    "type": "integer",
                    "description": "One-based starting line number"
                },
                "character_offset": {
                    "type": "integer",
                    "minimum": 0,
                    "description": "Zero-based Unicode character offset within the selected offset/limit \
                        line range; default 0. Use the returned value with the same path, \
                        offset and limit to continue a truncated range, including a long single \
                        line"
                },
                "limit": {
                    "type": "integer",
                    "description": "Maximum selected lines; 0 means all remaining lines. The 4 KiB body cap still applies"
                }
            },
            "required": ["path"]
        }),
    )
}

pub(crate) const READ_RETRY: (&str, otto_core::model::RetrySafety) =
    ("read", otto_core::model::RetrySafety::ReadOnly);

#[async_trait::async_trait]
impl Tool for ReadTool {
    fn retry_safety(&self) -> otto_core::model::RetrySafety {
        READ_RETRY.1
    }

    fn definition(&self) -> ToolDefinition {
        read_definition()
    }

    async fn execute(&self, arguments: &RawValue, _cancel: &CancellationToken) -> ToolResult {
        let args: ReadArgs = match decode_strict_json(arguments.get(), &["path"]) {
            Ok(args) => args,
            Err(message) => return error_result(message),
        };
        if args.path.is_empty() {
            return error_result("missing required argument: path");
        }
        if args.offset < 0 {
            return error_result("offset must be >= 0");
        }
        if args.limit < 0 {
            return error_result("limit must be >= 0");
        }

        if args.character_offset < 0 {
            return error_result("character_offset must be >= 0");
        }

        let path = Path::new(&args.path);
        let relative = match self.workspace.existing_relative(path) {
            Ok(relative) => relative,
            Err(error) => return error_result(error),
        };
        let file = match self.workspace.open_relative(&relative) {
            Ok(file) => file,
            Err(error) => return error_result(error),
        };
        let text = match read_validated_text_file(file, &args.path) {
            Ok(text) => text,
            Err(message) => return error_result(message),
        };

        let start_line = if args.offset == 0 { 1 } else { args.offset };
        let content: String = select_lines(&text, start_line, args.limit)
            .chars()
            .skip(args.character_offset as usize)
            .collect();
        let content = self.truncate(content, start_line, &args);
        if super::bash_output::is_output_log(&relative) {
            return ToolResult {
                content,
                persisted_content: Some(format!(
                    "Read Bash log excerpt (line offset {start_line}, limit {}, character offset {}). Excerpt omitted.",
                    args.limit, args.character_offset
                )),
                content_scope: ToolContentScope::NextStep,
                ..ToolResult::default()
            };
        }
        text_result(content)
    }
}

impl ReadTool {
    fn truncate(&self, content: String, start_line: i64, args: &ReadArgs) -> String {
        let mut collector = CappedByteCollector::new(self.max_output_bytes);
        collector.write(content.as_bytes());
        if collector.discarded() == 0 {
            return content;
        }
        let raw = collector.bytes();
        let safe = valid_utf8_prefix(raw);
        let omitted = collector.discarded() + raw.len() - safe.len();
        if safe.is_empty() {
            return format!(
                "[truncated: {omitted} bytes omitted; configured output cap is too small for the next UTF-8 character]"
            );
        }
        format!(
            "{}\n[truncated: {omitted} bytes omitted; continue with offset {start_line}, limit {}, character_offset {}]",
            String::from_utf8_lossy(safe),
            args.limit,
            args.character_offset as usize + String::from_utf8_lossy(safe).chars().count()
        )
    }
}

/// Reads `file` as text, rejecting anything the model must not be shown as a
/// string. The messages are the ones the model sees.
pub(crate) fn read_validated_text_file(file: std::fs::File, path: &str) -> Result<String, String> {
    let metadata = file.metadata().map_err(|error| error.to_string())?;
    if !metadata.is_file() {
        return Err(format!("not a regular file: {path}"));
    }
    if metadata.len() > MAX_READ_FILE_BYTES {
        return Err(too_large(metadata.len()));
    }
    let mut file_bytes = Vec::new();
    file.take(MAX_READ_FILE_BYTES + 1)
        .read_to_end(&mut file_bytes)
        .map_err(|error| error.to_string())?;
    if file_bytes.len() as u64 > MAX_READ_FILE_BYTES {
        return Err(too_large(file_bytes.len() as u64));
    }
    if file_bytes.contains(&0) {
        return Err(format!("binary file not supported: {path}"));
    }
    String::from_utf8(file_bytes).map_err(|_| format!("file is not valid UTF-8: {path}"))
}

fn too_large(size: u64) -> String {
    format!(
        "file is too large ({size} bytes); maximum readable size is {MAX_READ_FILE_BYTES} bytes"
    )
}

/// Returns `limit` lines starting at the one-based `offset`, keeping each
/// line's terminator.
fn select_lines(text: &str, offset: i64, limit: i64) -> String {
    let lines = split_lines_preserving_newlines(text);
    let offset = if offset <= 0 { 1 } else { offset };
    let start = (offset - 1) as usize;
    if start >= lines.len() {
        return String::new();
    }
    let mut end = lines.len();
    if limit > 0 && start.saturating_add(limit as usize) < end {
        end = start + limit as usize;
    }
    lines[start..end].concat()
}

fn split_lines_preserving_newlines(text: &str) -> Vec<&str> {
    if text.is_empty() {
        return Vec::new();
    }
    let mut lines = Vec::new();
    let mut rest = text;
    while let Some(index) = rest.find('\n') {
        lines.push(&rest[..=index]);
        rest = &rest[index + 1..];
    }
    if !rest.is_empty() {
        lines.push(rest);
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn select_lines_keeps_terminators_and_clamps_the_range() {
        let text = "one\ntwo\nthree\n";
        assert_eq!(select_lines(text, 1, 0), text);
        assert_eq!(select_lines(text, 2, 0), "two\nthree\n");
        assert_eq!(select_lines(text, 2, 1), "two\n");
        assert_eq!(select_lines(text, 9, 0), "");
        assert_eq!(select_lines("no newline", 1, 0), "no newline");
        assert_eq!(select_lines("", 1, 0), "");
    }
}

#[cfg(test)]
mod execute_tests {
    use super::*;
    use crate::tool::testutil::{MAX_OUTPUT_BYTES, run, workspace};

    #[tokio::test]
    async fn retrieval_cap_and_character_cursor_preserve_long_unicode_lines() {
        let dir = tempfile::tempdir().unwrap();
        let text = "你".repeat(2000);
        std::fs::write(dir.path().join("long.txt"), &text).unwrap();
        let workspace = workspace(dir.path());
        let tool = ReadTool::new(Arc::clone(&workspace), MAX_OUTPUT_BYTES);
        let first = run(&tool, r#"{"path":"long.txt"}"#).await;
        assert!(first.content.contains("character_offset 1365"));
        let head = first.content.split("\n[truncated:").next().unwrap();
        assert_eq!(head.len(), 4095);
        let second = run(&tool, r#"{"path":"long.txt","character_offset":1365}"#).await;
        assert_eq!(format!("{head}{}", second.content), text);
        assert!(
            run(&tool, r#"{"path":"long.txt","character_offset":-1}"#)
                .await
                .is_error
        );
    }

    #[tokio::test]
    async fn unknown_fields_and_a_missing_path_are_rejected() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("sample.txt"), "hello\n").unwrap();
        let workspace = workspace(root.path());
        let tool = ReadTool::new(Arc::clone(&workspace), MAX_OUTPUT_BYTES);

        // Malformed JSON and trailing tokens cannot reach a tool: the stream
        // decoder rejects them before a `RawValue` exists. Those cases are
        // covered by
        // `result::tests::strict_decoding_reports_the_go_error_text`.
        let unknown = run(&tool, r#"{"path":"sample.txt","extra":true}"#).await;
        assert!(
            unknown.is_error && unknown.content.contains("unknown field"),
            "{unknown:?}"
        );

        let missing = run(&tool, "{}").await;
        assert!(
            missing.is_error && missing.content.contains("path"),
            "{missing:?}"
        );
    }

    #[tokio::test]
    async fn binary_and_invalid_utf8_files_are_rejected() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("binary"), [b'a', 0, b'b']).unwrap();
        std::fs::write(root.path().join("invalid.txt"), [0xff, 0xfe]).unwrap();
        let workspace = workspace(root.path());
        let tool = ReadTool::new(Arc::clone(&workspace), MAX_OUTPUT_BYTES);

        let binary = run(&tool, r#"{"path":"binary"}"#).await;
        assert!(
            binary.is_error && binary.content.contains("binary"),
            "{binary:?}"
        );

        let invalid = run(&tool, r#"{"path":"invalid.txt"}"#).await;
        assert!(
            invalid.is_error && invalid.content.contains("UTF-8"),
            "{invalid:?}"
        );
    }

    #[tokio::test]
    async fn offset_and_limit_select_lines() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("sample.txt"), "one\ntwo\nthree\nfour\n").unwrap();
        let workspace = workspace(root.path());
        let tool = ReadTool::new(Arc::clone(&workspace), MAX_OUTPUT_BYTES);
        let result = run(&tool, r#"{"path":"sample.txt","offset":2,"limit":2}"#).await;
        assert!(!result.is_error, "{result:?}");
        assert_eq!(result.content, "two\nthree\n");

        let negative = run(&tool, r#"{"path":"sample.txt","offset":-1}"#).await;
        assert_eq!(negative.content, "offset must be >= 0");
        let negative_limit = run(&tool, r#"{"path":"sample.txt","limit":-1}"#).await;
        assert_eq!(negative_limit.content, "limit must be >= 0");
    }

    #[tokio::test]
    async fn truncation_reports_omitted_bytes_and_stays_valid_utf8() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("sample.txt"), "abcdefghi").unwrap();
        std::fs::write(root.path().join("accent.txt"), "é").unwrap();
        let workspace = workspace(root.path());

        let capped = ReadTool::new(Arc::clone(&workspace), 5);
        let result = run(&capped, r#"{"path":"sample.txt"}"#).await;
        assert!(!result.is_error, "{result:?}");
        assert!(result.content.starts_with("abcde"), "{result:?}");
        assert!(result.content.contains("truncated"), "{result:?}");

        let tiny = ReadTool::new(Arc::clone(&workspace), 1);
        let accented = run(&tiny, r#"{"path":"accent.txt"}"#).await;
        assert!(!accented.is_error, "{accented:?}");
        assert!(accented.content.contains("truncated"), "{accented:?}");
    }

    #[tokio::test]
    async fn traversal_is_rejected() {
        let root = tempfile::tempdir().unwrap();
        let nested = root.path().join("inner");
        std::fs::create_dir(&nested).unwrap();
        std::fs::write(root.path().join("escape.txt"), "nope").unwrap();
        let workspace = workspace(&nested);
        let tool = ReadTool::new(Arc::clone(&workspace), MAX_OUTPUT_BYTES);
        let result = run(&tool, r#"{"path":"../escape.txt"}"#).await;
        assert!(
            result.is_error && result.content.contains("escapes workspace"),
            "{result:?}"
        );
    }

    #[tokio::test]
    async fn a_file_exactly_at_the_limit_is_accepted() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(
            root.path().join("limit.txt"),
            "a".repeat(MAX_READ_FILE_BYTES as usize),
        )
        .unwrap();
        let workspace = workspace(root.path());
        let tool = ReadTool::new(Arc::clone(&workspace), MAX_OUTPUT_BYTES);
        let result = run(&tool, r#"{"path":"limit.txt"}"#).await;
        assert!(
            !result.is_error,
            "{}",
            &result.content[..result.content.len().min(200)]
        );
    }

    #[tokio::test]
    async fn the_original_workspace_is_kept_after_its_path_is_replaced() {
        let parent = tempfile::tempdir().unwrap();
        let root = parent.path().join("workspace");
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("note.txt"), "inside").unwrap();
        let workspace = workspace(&root);

        std::fs::rename(&root, parent.path().join("moved")).unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("note.txt"), "outside secret").unwrap();
        std::os::unix::fs::symlink(outside.path(), &root).unwrap();

        let tool = ReadTool::new(Arc::clone(&workspace), MAX_OUTPUT_BYTES);
        let result = run(&tool, r#"{"path":"note.txt"}"#).await;
        assert!(!result.is_error, "{result:?}");
        assert_eq!(result.content, "inside");
    }

    #[tokio::test]
    async fn an_oversized_file_is_rejected_and_a_fifo_does_not_block() {
        let root = tempfile::tempdir().unwrap();
        let huge = root.path().join("huge.txt");
        std::fs::write(&huge, "x").unwrap();
        std::fs::File::options()
            .write(true)
            .open(&huge)
            .unwrap()
            .set_len(MAX_READ_FILE_BYTES + 1)
            .unwrap();
        nix::unistd::mkfifo(
            &root.path().join("pipe"),
            nix::sys::stat::Mode::from_bits_truncate(0o600),
        )
        .unwrap();
        let workspace = workspace(root.path());
        let tool = ReadTool::new(Arc::clone(&workspace), MAX_OUTPUT_BYTES);

        let oversized = run(&tool, r#"{"path":"huge.txt"}"#).await;
        assert!(
            oversized.is_error && oversized.content.contains("too large"),
            "{oversized:?}"
        );

        let fifo = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            run(&tool, r#"{"path":"pipe"}"#),
        )
        .await
        .expect("opening a FIFO must not block");
        assert!(
            fifo.is_error && fifo.content.contains("not a regular file"),
            "{fifo:?}"
        );
    }
}
