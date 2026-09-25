//! The `grep` tool.
//!
//! Searches file contents with a regular expression. Binary files, files with a
//! line over 1 MiB, symbolic links, and `.git` subtrees are skipped, so the
//! model never receives object-store contents or a multi-megabyte line. Both
//! the match count and the byte count are bounded, and the marker says which
//! limit stopped the search.

use std::io::{self, Read};

use otto_core::model::ToolDefinition;
use otto_core::tool::ToolResult;
use regex::bytes::{Regex, RegexBuilder};
use serde::Deserialize;
use serde_json::json;
use serde_json::value::RawValue;
use tokio_util::sync::CancellationToken;

use super::gitignore::GitignoreStack;
use super::result::{CappedByteCollector, capped_collector_result, decode_strict_json};
use super::search::{
    WalkAction, match_glob_segments, resolve_search_limit, search_relative_path,
    search_root_inside_git, validated_glob_segments, walk_dir_ignoring,
};
use super::workspace::Workspace;
use super::{CONTEXT_CANCELED, Tool, definition, error_result};

const DEFAULT_GREP_LIMIT: usize = 100;
const MAXIMUM_GREP_LIMIT: usize = 1000;
/// A file containing a longer line is treated as non-text and skipped.
const MAXIMUM_GREP_LINE_BYTES: usize = 1 << 20;
/// The read size for one scan chunk.
const READ_CHUNK_BYTES: usize = 64 << 10;

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct GrepArgs {
    #[serde(default)]
    pattern: String,
    #[serde(default)]
    path: String,
    #[serde(default)]
    glob: String,
    #[serde(default)]
    ignore_case: bool,
    #[serde(default)]
    limit: Option<i64>,
    #[serde(default)]
    no_ignore: bool,
}

/// Searches workspace file contents.
pub struct GrepTool<'a> {
    workspace: &'a Workspace,
    max_output_bytes: usize,
}

impl<'a> GrepTool<'a> {
    pub fn new(workspace: &'a Workspace, max_output_bytes: usize) -> Self {
        Self {
            workspace,
            max_output_bytes,
        }
    }
}

/// The schema advertised for `grep`.
pub fn grep_definition() -> ToolDefinition {
    definition(
        "grep",
        "Search workspace file contents with a regular expression (read-only). Files the workspace's .gitignore excludes are skipped unless no_ignore is set",
        json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "pattern": {
                    "type": "string",
                    "description": "RE2 regular expression"
                },
                "path": {
                    "type": "string",
                    "description": "Workspace-relative directory or file to search; defaults to ."
                },
                "glob": {
                    "type": "string",
                    "description": "Optional relative file glob; supports recursive ** segments"
                },
                "ignore_case": {
                    "type": "boolean",
                    "description": "Match without case sensitivity; defaults to false"
                },
                "no_ignore": {
                    "type": "boolean",
                    "description": "Search files the repository's .gitignore excludes; defaults to false"
                },
                "limit": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": MAXIMUM_GREP_LIMIT,
                    "description": "Maximum matching lines; defaults to 100"
                }
            },
            "required": ["pattern"]
        }),
    )
}

#[async_trait::async_trait]
impl Tool for GrepTool<'_> {
    fn definition(&self) -> ToolDefinition {
        grep_definition()
    }

    async fn execute(&self, arguments: &RawValue, cancel: &CancellationToken) -> ToolResult {
        let args: GrepArgs = match decode_strict_json(arguments.get(), &["pattern"]) {
            Ok(args) => args,
            Err(message) => return error_result(message),
        };
        if args.pattern.is_empty() {
            return error_result("pattern must not be empty");
        }
        let expression = match RegexBuilder::new(&args.pattern)
            .case_insensitive(args.ignore_case)
            .build()
        {
            Ok(expression) => expression,
            Err(error) => return error_result(format!("invalid regular expression: {error}")),
        };
        let glob_segments = if args.glob.is_empty() {
            None
        } else {
            match validated_glob_segments(&args.glob) {
                Ok(segments) => Some(segments),
                Err(message) => return error_result(message),
            }
        };
        let limit = match resolve_search_limit(args.limit, DEFAULT_GREP_LIMIT, MAXIMUM_GREP_LIMIT) {
            Ok(limit) => limit,
            Err(message) => return error_result(message),
        };
        if cancel.is_cancelled() {
            return error_result(CONTEXT_CANCELED);
        }
        let search_path = if args.path.is_empty() {
            "."
        } else {
            &args.path
        };
        let root = match self
            .workspace
            .existing_relative(std::path::Path::new(search_path))
        {
            Ok(root) => root.to_string_lossy().into_owned(),
            Err(error) => return error_result(error),
        };
        match search_root_inside_git(self.workspace, search_path, &root) {
            Ok(true) => return ToolResult::default(),
            Ok(false) => {}
            Err(error) => return error_result(error),
        }

        let mut collector = CappedByteCollector::new(self.max_output_bytes);
        let mut match_count = 0usize;
        let mut truncation_marker = "";
        let mut ignore =
            (!args.no_ignore).then(|| GitignoreStack::for_root(self.workspace.root_fs(), &root));
        let walk = walk_dir_ignoring(
            self.workspace.root_fs(),
            &root,
            ignore.as_mut(),
            &mut |entry| {
                if cancel.is_cancelled() {
                    return Err(io::Error::other(CONTEXT_CANCELED));
                }
                if entry.path != root && entry.name == ".git" {
                    return Ok(if entry.is_dir {
                        WalkAction::SkipDir
                    } else {
                        WalkAction::Continue
                    });
                }
                if entry.is_dir || entry.is_symlink || !entry.is_regular {
                    return Ok(WalkAction::Continue);
                }
                let candidate = search_relative_path(&root, &entry.path)?;
                if let Some(segments) = &glob_segments
                    && !match_glob_segments(segments, &candidate)
                {
                    return Ok(WalkAction::Continue);
                }
                let file = self
                    .workspace
                    .open_relative(std::path::Path::new(&entry.path))?;
                let remaining_bytes = self
                    .max_output_bytes
                    .saturating_sub(collector.bytes().len());
                let scan = scan_grep_reader(
                    file,
                    &expression,
                    limit.saturating_sub(match_count),
                    remaining_bytes,
                    cancel,
                )?;
                if !scan.text_file {
                    return Ok(WalkAction::Continue);
                }
                for line in &scan.matches {
                    collector.write(
                        format!("{}:{}:{}\n", entry.path, line.number, line.text).as_bytes(),
                    );
                    match_count += 1;
                }
                if scan.match_overflow {
                    truncation_marker = "[truncated: result limit reached]";
                    return Ok(WalkAction::Stop);
                }
                if scan.byte_overflow || collector.discarded() > 0 {
                    truncation_marker = "[truncated: output limit reached]";
                    return Ok(WalkAction::Stop);
                }
                Ok(WalkAction::Continue)
            },
        );
        if let Err(error) = walk {
            return error_result(error);
        }
        capped_collector_result(&collector, truncation_marker)
    }
}

/// One matching line.
#[derive(Debug)]
struct GrepLine {
    number: usize,
    text: String,
}

/// What one file scan produced. `text_file` is false for a file that must be
/// skipped entirely.
#[derive(Debug, Default)]
struct GrepScanResult {
    matches: Vec<GrepLine>,
    text_file: bool,
    match_overflow: bool,
    byte_overflow: bool,
}

/// Scans one file for matching lines.
///
/// Cancellation is checked around every bounded read, so a large file stops
/// promptly. A NUL byte, invalid UTF-8, or a line over
/// [`MAXIMUM_GREP_LINE_BYTES`] makes the whole file non-text.
fn scan_grep_reader(
    mut source: impl Read,
    expression: &Regex,
    max_matches: usize,
    max_bytes: usize,
    cancel: &CancellationToken,
) -> io::Result<GrepScanResult> {
    let mut result = GrepScanResult {
        text_file: true,
        ..GrepScanResult::default()
    };
    let mut buffer = vec![0u8; READ_CHUNK_BYTES];
    let mut line: Vec<u8> = Vec::new();
    let mut line_number = 0usize;
    let mut collected_bytes = 0usize;
    loop {
        if cancel.is_cancelled() {
            return Err(io::Error::other(CONTEXT_CANCELED));
        }
        let read = source.read(&mut buffer)?;
        if cancel.is_cancelled() {
            return Err(io::Error::other(CONTEXT_CANCELED));
        }
        if read == 0 {
            if !line.is_empty()
                && !consume_line(
                    &line,
                    expression,
                    max_matches,
                    max_bytes,
                    &mut line_number,
                    &mut collected_bytes,
                    &mut result,
                )
            {
                return Ok(GrepScanResult::default());
            }
            return Ok(result);
        }
        let mut chunk = &buffer[..read];
        while let Some(index) = chunk.iter().position(|byte| *byte == b'\n') {
            if line.len() + index > MAXIMUM_GREP_LINE_BYTES {
                return Ok(GrepScanResult::default());
            }
            line.extend_from_slice(&chunk[..index]);
            chunk = &chunk[index + 1..];
            let text = strip_carriage_return(&line);
            if !consume_line(
                text,
                expression,
                max_matches,
                max_bytes,
                &mut line_number,
                &mut collected_bytes,
                &mut result,
            ) {
                return Ok(GrepScanResult::default());
            }
            line.clear();
        }
        if line.len() + chunk.len() > MAXIMUM_GREP_LINE_BYTES {
            return Ok(GrepScanResult::default());
        }
        line.extend_from_slice(chunk);
    }
}

/// Drops the `\r` of a CRLF terminator, matching `bufio.Reader.ReadLine`.
fn strip_carriage_return(line: &[u8]) -> &[u8] {
    match line.split_last() {
        Some((b'\r', rest)) => rest,
        _ => line,
    }
}

/// Records one line. Returns false when the file must be skipped entirely.
fn consume_line(
    line: &[u8],
    expression: &Regex,
    max_matches: usize,
    max_bytes: usize,
    line_number: &mut usize,
    collected_bytes: &mut usize,
    result: &mut GrepScanResult,
) -> bool {
    *line_number += 1;
    if line.contains(&0) || std::str::from_utf8(line).is_err() {
        return false;
    }
    if !expression.is_match(line) {
        return true;
    }
    if result.matches.len() >= max_matches {
        result.match_overflow = true;
    } else if *collected_bytes + line.len() > max_bytes {
        result.byte_overflow = true;
    } else {
        result.matches.push(GrepLine {
            number: *line_number,
            text: String::from_utf8_lossy(line).into_owned(),
        });
        *collected_bytes += line.len();
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::testutil::{
        MAX_OUTPUT_BYTES, run, run_cancelled, workspace, write_search_file,
    };

    #[tokio::test]
    async fn gitignored_files_are_skipped_unless_no_ignore_is_set() {
        let root = tempfile::tempdir().unwrap();
        write_search_file(root.path(), ".gitignore", "target/\n*.tmp\n");
        for name in ["main.go", "drop.tmp", "target/build.go"] {
            write_search_file(root.path(), name, "needle here\n");
        }
        let workspace = workspace(root.path());
        let tool = GrepTool::new(&workspace, MAX_OUTPUT_BYTES);

        let ignored = run(&tool, r#"{"pattern":"needle"}"#).await;
        assert!(!ignored.is_error, "{ignored:?}");
        assert_eq!(ignored.content, "main.go:1:needle here\n");

        let everything = run(&tool, r#"{"pattern":"needle","no_ignore":true}"#).await;
        assert!(!everything.is_error, "{everything:?}");
        assert_eq!(
            everything.content,
            "drop.tmp:1:needle here\nmain.go:1:needle here\ntarget/build.go:1:needle here\n"
        );
    }

    #[tokio::test]
    async fn regex_search_honours_globs_and_skips_git_binary_and_symlinks() {
        let root = tempfile::tempdir().unwrap();
        write_search_file(root.path(), "main.go", "TODO first\nordinary\ntodo lower\n");
        write_search_file(root.path(), "src/a.go", "FIXME TODO nested\n");
        write_search_file(root.path(), "src/skip.txt", "TODO text\n");
        write_search_file(root.path(), ".hidden.go", "TODO hidden\n");
        write_search_file(root.path(), ".git/secret.go", "TODO secret\n");
        std::fs::write(root.path().join("binary.go"), b"TODO\0x").unwrap();
        std::fs::write(root.path().join("invalid.go"), [0xff, b'\n']).unwrap();
        let outside = tempfile::tempdir().unwrap();
        write_search_file(outside.path(), "outside.go", "TODO outside\n");
        std::os::unix::fs::symlink(
            outside.path().join("outside.go"),
            root.path().join("linked.go"),
        )
        .unwrap();

        let workspace = workspace(root.path());
        let tool = GrepTool::new(&workspace, MAX_OUTPUT_BYTES);

        let result = run(&tool, r#"{"pattern":"TODO|FIXME","glob":"**/*.go"}"#).await;
        assert!(!result.is_error, "{result:?}");
        assert_eq!(
            result.content,
            ".hidden.go:1:TODO hidden\nmain.go:1:TODO first\nsrc/a.go:1:FIXME TODO nested\n"
        );

        let insensitive = run(
            &tool,
            r#"{"pattern":"todo","path":"main.go","glob":"*.go","ignore_case":true}"#,
        )
        .await;
        assert!(!insensitive.is_error, "{insensitive:?}");
        assert_eq!(
            insensitive.content,
            "main.go:1:TODO first\nmain.go:3:todo lower\n"
        );
    }

    #[tokio::test]
    async fn arguments_limits_cancellation_and_the_workspace_boundary_are_enforced() {
        let root = tempfile::tempdir().unwrap();
        write_search_file(root.path(), "a.txt", "match one\nmatch two\n");
        let workspace = workspace(root.path());
        let tool = GrepTool::new(&workspace, MAX_OUTPUT_BYTES);

        let limited = run(&tool, r#"{"pattern":"match","limit":1}"#).await;
        assert!(!limited.is_error, "{limited:?}");
        assert!(
            limited.content.starts_with("a.txt:1:match one\n"),
            "{limited:?}"
        );
        assert!(
            limited.content.contains("result limit reached"),
            "{limited:?}"
        );
        assert!(!limited.content.contains("match two"), "{limited:?}");

        for arguments in [
            "{}",
            r#"{"pattern":"("}"#,
            r#"{"pattern":"match","glob":"[broken"}"#,
            r#"{"pattern":"match","limit":0}"#,
            r#"{"pattern":"match","limit":-1}"#,
            r#"{"pattern":"match","limit":1001}"#,
            r#"{"pattern":"match","extra":true}"#,
            r#"{"pattern":"match","path":".."}"#,
        ] {
            let result = run(&tool, arguments).await;
            assert!(
                result.is_error,
                "grep({arguments}) = {result:?}, want an error"
            );
        }

        let cancelled = run_cancelled(&tool, r#"{"pattern":"match"}"#).await;
        assert!(
            cancelled.is_error && cancelled.content.contains(CONTEXT_CANCELED),
            "{cancelled:?}"
        );
    }

    #[tokio::test]
    async fn a_root_inside_the_git_directory_returns_nothing() {
        let root = tempfile::tempdir().unwrap();
        write_search_file(root.path(), ".git/config", "match secret\n");
        let workspace = workspace(root.path());
        let tool = GrepTool::new(&workspace, MAX_OUTPUT_BYTES);
        for search_path in [".git", ".git/config"] {
            let result = run(
                &tool,
                &format!(r#"{{"pattern":"match","path":"{search_path}"}}"#),
            )
            .await;
            assert!(
                !result.is_error && result.content.is_empty(),
                "{search_path}: {result:?}"
            );
        }
    }

    #[tokio::test]
    async fn output_is_capped_with_a_valid_truncation_marker() {
        let root = tempfile::tempdir().unwrap();
        write_search_file(root.path(), "file.txt", "matching long output line\n");
        let workspace = workspace(root.path());
        let result = run(&GrepTool::new(&workspace, 8), r#"{"pattern":"matching"}"#).await;
        assert!(!result.is_error, "{result:?}");
        assert!(result.content.contains("truncated"), "{result:?}");
        assert!(result.content.len() <= 80, "{result:?}");
    }

    /// Cancels the token during its first read, so the scan must stop before
    /// asking for more.
    struct CancellingReader {
        cancel: CancellationToken,
        content: Vec<u8>,
        reads: usize,
    }

    impl Read for CancellingReader {
        fn read(&mut self, destination: &mut [u8]) -> io::Result<usize> {
            self.reads += 1;
            if self.reads > 1 {
                return Err(io::Error::other(CONTEXT_CANCELED));
            }
            let length = self.content.len().min(destination.len());
            destination[..length].copy_from_slice(&self.content[..length]);
            self.cancel.cancel();
            Ok(length)
        }
    }

    #[test]
    fn the_scan_cancels_between_bounded_reads() {
        let cancel = CancellationToken::new();
        let mut reader = CancellingReader {
            cancel: cancel.clone(),
            content: b"match first\nmatch second\n".to_vec(),
            reads: 0,
        };
        let expression = Regex::new("match").unwrap();
        let error = scan_grep_reader(&mut reader, &expression, 100, 51200, &cancel)
            .expect_err("the scan should report cancellation");
        assert!(error.to_string().contains(CONTEXT_CANCELED), "{error}");
        assert_eq!(reader.reads, 1, "the scan read past cancellation");
    }

    #[test]
    fn a_file_with_an_oversized_line_is_skipped() {
        let content = format!("{}\nmatch later\n", "x".repeat(MAXIMUM_GREP_LINE_BYTES + 1));
        let expression = Regex::new("match").unwrap();
        let scan = scan_grep_reader(
            content.as_bytes(),
            &expression,
            100,
            51200,
            &CancellationToken::new(),
        )
        .expect("scanning succeeds");
        assert!(!scan.text_file && scan.matches.is_empty(), "{scan:?}");
        assert!(!scan.match_overflow && !scan.byte_overflow, "{scan:?}");
    }
}
