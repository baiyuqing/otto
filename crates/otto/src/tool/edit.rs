//! The `edit` tool. Port of `internal/tool/edit.go`.
//!
//! Replaces one uniquely matching fragment per edit in a workspace file. A
//! match is tried exactly first and then over a whitespace- and
//! punctuation-normalized view of both sides, so text copied through a
//! terminal or a chat client still applies. An ambiguous or missing match is
//! an error rather than a guess.
//!
//! Ownership: the tool borrows its workspace. Concurrency: edits to one
//! resolved path are serialized through a process-wide queue keyed by the
//! canonical path, so two concurrent edits cannot interleave read and write.
//! Errors: every failure is returned in band as an error [`ToolResult`].

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use otto_core::model::ToolDefinition;
use otto_core::tool::ToolResult;
use serde_json::value::RawValue;
use serde_json::{Map, Value, json};
use tokio_util::sync::CancellationToken;

use super::read::read_validated_text_file;
use super::workspace::Workspace;
use super::write::write_file_atomic;
use super::{Tool, definition, error_result, text_result};

/// Lines of unchanged context kept on either side of a hunk.
const DIFF_CONTEXT_LINES: usize = 3;
/// The largest diff returned to the model before it is cut on a line boundary.
const MAX_DIFF_BYTES: usize = 4096;

#[derive(Debug, Default)]
struct EditArgs {
    path: String,
    edits: Vec<EditReplacement>,
}

#[derive(Debug, Clone, Default)]
struct EditReplacement {
    old_text: String,
    new_text: String,
}

#[derive(Debug)]
struct ResolvedEdit {
    start: usize,
    end: usize,
    text: String,
}

/// Replaces text in a workspace file.
pub struct EditTool<'a> {
    workspace: &'a Workspace,
}

impl<'a> EditTool<'a> {
    pub fn new(workspace: &'a Workspace) -> Self {
        Self { workspace }
    }

    fn execute_locked(&self, args: &EditArgs) -> ToolResult {
        let path = Path::new(&args.path);
        let file = match self.workspace.open(path) {
            Ok(file) => file,
            Err(error) => return error_result(error),
        };
        let text = match read_validated_text_file(file, &args.path) {
            Ok(text) => text,
            Err(message) => return error_result(message),
        };

        let replaced = match apply_text_edits(&text, &args.path, &args.edits) {
            Ok(replaced) => replaced,
            Err(message) => return error_result(message),
        };

        let relative = match self.workspace.write_relative(path) {
            Ok(relative) => relative,
            Err(error) => return error_result(error),
        };
        if let Err(message) = write_file_atomic(self.workspace, &relative, replaced.as_bytes()) {
            return error_result(message);
        }
        text_result(format!(
            "edited {}\n{}",
            args.path,
            edit_diff(&display_edit_text(&text), &display_edit_text(&replaced))
        ))
    }
}

/// The schema advertised for `edit`.
pub fn edit_definition() -> ToolDefinition {
    definition(
        "edit",
        "Replace exactly one matching text fragment in a workspace file",
        json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Workspace-relative file path to edit"
                },
                "old_text": {
                    "type": "string",
                    "description": "Exact existing text to replace"
                },
                "new_text": {
                    "type": "string",
                    "description": "Replacement text, kept for compatibility"
                },
                "oldText": {
                    "type": "string",
                    "description": "Exact existing text to replace, kept for compatibility"
                },
                "newText": {
                    "type": "string",
                    "description": "Replacement text, kept for compatibility"
                },
                "edits": {
                    "description": "One edit object, an array of edit objects, or a JSON string containing either shape"
                }
            },
            "required": ["path"]
        }),
    )
}

#[async_trait::async_trait]
impl Tool for EditTool<'_> {
    fn definition(&self) -> ToolDefinition {
        edit_definition()
    }

    async fn execute(&self, arguments: &RawValue, _cancel: &CancellationToken) -> ToolResult {
        let args = match prepare_edit_arguments(arguments.get()) {
            Ok(args) => args,
            Err(message) => return error_result(message),
        };
        let key = match self.workspace.resolve_existing(Path::new(&args.path)) {
            Ok(key) => key,
            Err(error) => return error_result(error),
        };
        let queue = file_mutation_queue(&key);
        let _guard = queue.lock().await;
        self.execute_locked(&args)
    }
}

/// Decodes the argument object. Port of `prepareEditArguments`: the schema
/// accepts a single replacement at the top level, one edit object, an array of
/// them, or a JSON string holding either shape.
fn prepare_edit_arguments(arguments: &str) -> Result<EditArgs, String> {
    let raw: Map<String, Value> =
        serde_json::from_str(arguments).map_err(|error| format!("invalid JSON: {error}"))?;

    const ALLOWED: [&str; 6] = [
        "path", "old_text", "new_text", "oldText", "newText", "edits",
    ];
    for key in raw.keys() {
        if !ALLOWED.contains(&key.as_str()) {
            return Err(format!("json: unknown field {key:?}"));
        }
    }

    let path = read_string_field(&raw, &["path"])?;
    let path = match path {
        Some(path) if !path.is_empty() => path,
        _ => return Err("missing required argument: path".to_owned()),
    };

    if let Some(edits) = raw.get("edits") {
        return Ok(EditArgs {
            path,
            edits: parse_edit_list(edits)?,
        });
    }
    Ok(EditArgs {
        path,
        edits: vec![parse_edit_object(&raw, true)?],
    })
}

/// Port of `parseEditList`.
fn parse_edit_list(raw: &Value) -> Result<Vec<EditReplacement>, String> {
    let decoded;
    let value = if let Value::String(encoded) = raw {
        decoded = serde_json::from_str::<Value>(encoded.trim())
            .map_err(|error| format!("invalid JSON: {error}"))?;
        &decoded
    } else {
        raw
    };

    match value {
        Value::Null => Err("missing required argument: edits".to_owned()),
        Value::Object(object) => Ok(vec![parse_edit_object(object, false)?]),
        Value::Array(items) => {
            if items.is_empty() {
                return Err("missing required argument: edits".to_owned());
            }
            items
                .iter()
                .map(|item| match item {
                    Value::Object(object) => parse_edit_object(object, false),
                    other => Err(format!(
                        "invalid JSON: json: cannot unmarshal {} into Go value of type map[string]json.RawMessage",
                        json_kind(other)
                    )),
                })
                .collect()
        }
        other => Err(format!(
            "invalid JSON: json: cannot unmarshal {} into Go value of type []map[string]json.RawMessage",
            json_kind(other)
        )),
    }
}

fn json_kind(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

/// Port of `parseEditObject`. `top_level` also allows `path`, which the
/// surrounding object carries.
fn parse_edit_object(raw: &Map<String, Value>, top_level: bool) -> Result<EditReplacement, String> {
    for key in raw.keys() {
        let allowed = matches!(
            key.as_str(),
            "old_text" | "new_text" | "oldText" | "newText"
        ) || (top_level && key == "path");
        if !allowed {
            return Err(format!("json: unknown field {key:?}"));
        }
    }

    let old_text = read_string_field(raw, &["old_text", "oldText"])?;
    let old_text = match old_text {
        Some(text) if !text.is_empty() => text,
        _ => return Err("missing required argument: old_text".to_owned()),
    };
    let Some(new_text) = read_string_field(raw, &["new_text", "newText"])? else {
        return Err("missing required argument: new_text".to_owned());
    };
    Ok(EditReplacement { old_text, new_text })
}

/// Returns the first present field among `names`. Port of `readStringField`.
fn read_string_field(raw: &Map<String, Value>, names: &[&str]) -> Result<Option<String>, String> {
    for name in names {
        let Some(value) = raw.get(*name) else {
            continue;
        };
        let Value::String(text) = value else {
            return Err(format!("invalid argument {name}: must be a string"));
        };
        return Ok(Some(text.clone()));
    }
    Ok(None)
}

/// Applies every replacement to `text`. Port of `applyTextEdits`: a leading
/// byte-order mark and the file's newline style are preserved, matching runs
/// over a `\n`-normalized view, and overlapping edits are refused.
fn apply_text_edits(text: &str, path: &str, edits: &[EditReplacement]) -> Result<String, String> {
    let body = text.strip_prefix('\u{feff}').unwrap_or(text);
    let has_bom = body.len() != text.len();
    let (content, content_to_body) = normalize_line_endings_with_map(body);
    let newline = detect_newline(body);

    let mut resolved = Vec::with_capacity(edits.len());
    for (index, edit) in edits.iter().enumerate() {
        let old_text = normalize_line_endings(&edit.old_text);
        let new_text = restore_line_endings(&normalize_line_endings(&edit.new_text), newline);
        let (start, end) = find_unique_edit_match(&content, &old_text, path, index, edits.len())?;
        resolved.push(ResolvedEdit {
            start: content_to_body[start],
            end: content_to_body[end],
            text: new_text,
        });
    }

    resolved.sort_by_key(|edit| edit.start);
    for pair in resolved.windows(2) {
        if pair[1].start < pair[0].end {
            return Err(format!(
                "edit failed: edits overlap in {path}; combine them or provide non-overlapping old_text values"
            ));
        }
    }

    let replaced = apply_resolved_edits(body, &resolved);
    Ok(if has_bom {
        format!("\u{feff}{replaced}")
    } else {
        replaced
    })
}

/// Locates the one place `old_text` occurs. Port of `findUniqueEditMatch`:
/// exact matching first, then a fuzzy view that folds typographic quotes,
/// dashes, and Unicode spaces and drops trailing whitespace on every line.
fn find_unique_edit_match(
    content: &str,
    old_text: &str,
    path: &str,
    index: usize,
    total: usize,
) -> Result<(usize, usize), String> {
    let count = content.matches(old_text).count();
    if count == 1 {
        let start = content.find(old_text).expect("one match exists");
        return Ok((start, start + old_text.len()));
    }
    if count > 1 {
        return Err(match_error(path, index, total, &ambiguous(count, path)));
    }

    let (fuzzy_content, fuzzy_to_content) = normalize_for_fuzzy_match_with_map(content);
    let (fuzzy_old_text, _) = normalize_for_fuzzy_match_with_map(old_text);
    if fuzzy_old_text.is_empty() {
        return Err(match_error(path, index, total, &not_found(path)));
    }
    let count = fuzzy_content.matches(&fuzzy_old_text).count();
    if count == 0 {
        return Err(match_error(path, index, total, &not_found(path)));
    }
    if count > 1 {
        return Err(match_error(path, index, total, &ambiguous(count, path)));
    }
    let start = fuzzy_content
        .find(&fuzzy_old_text)
        .expect("one match exists");
    Ok((
        fuzzy_to_content[start],
        fuzzy_to_content[start + fuzzy_old_text.len()],
    ))
}

fn not_found(path: &str) -> String {
    format!("old_text was not found in {path}")
}

fn ambiguous(count: usize, path: &str) -> String {
    format!(
        "old_text matched {count} locations in {path}; include more surrounding context to make it unique"
    )
}

/// Prefixes `message` with the edit position. Port of `editMatchError`.
fn match_error(_path: &str, index: usize, total: usize, message: &str) -> String {
    if total == 1 {
        format!("edit failed: {message}")
    } else {
        format!("edit {} failed: {message}", index + 1)
    }
}

/// Splices every replacement in from the end so earlier offsets stay valid.
fn apply_resolved_edits(text: &str, edits: &[ResolvedEdit]) -> String {
    let mut out = String::with_capacity(text.len());
    let mut cursor = 0;
    for edit in edits {
        out.push_str(&text[cursor..edit.start]);
        out.push_str(&edit.text);
        cursor = edit.end;
    }
    out.push_str(&text[cursor..]);
    out
}

fn normalize_line_endings(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\r', "\n")
}

/// Normalizes newlines and records, for every normalized byte, the offset it
/// came from. Port of `normalizeLineEndingsWithMap`.
fn normalize_line_endings_with_map(text: &str) -> (String, Vec<usize>) {
    let raw = text.as_bytes();
    let mut out = Vec::with_capacity(raw.len());
    let mut offsets = Vec::with_capacity(raw.len() + 1);
    let mut i = 0;
    while i < raw.len() {
        offsets.push(i);
        if raw[i] == b'\r' {
            out.push(b'\n');
            i += if i + 1 < raw.len() && raw[i + 1] == b'\n' {
                2
            } else {
                1
            };
            continue;
        }
        out.push(raw[i]);
        i += 1;
    }
    offsets.push(raw.len());
    (
        String::from_utf8(out).expect("replacing CR with LF keeps UTF-8 valid"),
        offsets,
    )
}

fn detect_newline(text: &str) -> &'static str {
    if text.contains("\r\n") {
        "\r\n"
    } else if text.contains('\r') {
        "\r"
    } else {
        "\n"
    }
}

fn restore_line_endings(text: &str, newline: &str) -> String {
    if newline == "\n" {
        text.to_owned()
    } else {
        text.replace('\n', newline)
    }
}

fn display_edit_text(text: &str) -> String {
    normalize_line_endings(text.strip_prefix('\u{feff}').unwrap_or(text))
}

/// Folds the characters a copy-paste round trip tends to change and drops
/// trailing whitespace per line, recording the source offset of every output
/// byte. Port of `normalizeForFuzzyMatchWithMap`.
fn normalize_for_fuzzy_match_with_map(text: &str) -> (String, Vec<usize>) {
    let mut out = String::with_capacity(text.len());
    let mut offsets = Vec::with_capacity(text.len() + 1);
    let mut pos = 0;
    loop {
        if pos >= text.len() {
            break;
        }
        let line_end = text[pos..].find('\n').map(|index| pos + index);
        let end = line_end.unwrap_or(text.len());
        let trimmed_end = trim_trailing_fuzzy_whitespace(text, pos, end);
        for (index, character) in text[pos..trimmed_end].char_indices() {
            match fuzzy_char(character) {
                Some(replacement) => {
                    for _ in 0..replacement.len() {
                        offsets.push(pos + index);
                    }
                    out.push_str(replacement);
                }
                None => {
                    for _ in 0..character.len_utf8() {
                        offsets.push(pos + index);
                    }
                    out.push(character);
                }
            }
        }
        let Some(line_end) = line_end else {
            break;
        };
        offsets.push(end);
        out.push('\n');
        pos = line_end + 1;
    }
    offsets.push(text.len());
    (out, offsets)
}

/// Returns the end offset with trailing spaces, but not line breaks, removed.
fn trim_trailing_fuzzy_whitespace(text: &str, start: usize, mut end: usize) -> usize {
    while end > start {
        let character = text[start..end]
            .chars()
            .next_back()
            .expect("the slice is not empty");
        if !character.is_whitespace() || character == '\n' || character == '\r' {
            break;
        }
        end -= character.len_utf8();
    }
    end
}

/// The canonical form of one character in the fuzzy view, or `None` when the
/// character is already canonical. Port of `fuzzyRune`.
fn fuzzy_char(character: char) -> Option<&'static str> {
    Some(match character {
        '\u{2018}' | '\u{2019}' | '\u{201a}' | '\u{201b}' => "'",
        '\u{201c}' | '\u{201d}' | '\u{201e}' | '\u{201f}' => "\"",
        '\u{2010}' | '\u{2011}' | '\u{2012}' | '\u{2013}' | '\u{2014}' | '\u{2015}'
        | '\u{2212}' => "-",
        '\u{00a0}' | '\u{2000}' | '\u{2001}' | '\u{2002}' | '\u{2003}' | '\u{2004}'
        | '\u{2005}' | '\u{2006}' | '\u{2007}' | '\u{2008}' | '\u{2009}' | '\u{200a}'
        | '\u{202f}' | '\u{205f}' | '\u{3000}' => " ",
        _ => return None,
    })
}

/// The per-path queue that serializes mutations. Port of
/// `withFileMutationQueue`.
// ponytail: locks live for the process lifetime; add ref-count cleanup if edit
// churn matters.
fn file_mutation_queue(path: &Path) -> Arc<tokio::sync::Mutex<()>> {
    static QUEUES: OnceLock<Mutex<HashMap<PathBuf, Arc<tokio::sync::Mutex<()>>>>> = OnceLock::new();
    let queues = QUEUES.get_or_init(|| Mutex::new(HashMap::new()));
    let mut queues = queues.lock().expect("the queue table is never poisoned");
    Arc::clone(
        queues
            .entry(path.to_path_buf())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(()))),
    )
}

/// Renders a unified-style hunk for the changed region. Port of `editDiff`:
/// the edits form one contiguous range, so trimming the common prefix and
/// suffix lines yields exactly the changed lines.
fn edit_diff(before: &str, after: &str) -> String {
    if before == after {
        return "(no textual changes)".to_owned();
    }
    let old_lines: Vec<&str> = before.split('\n').collect();
    let new_lines: Vec<&str> = after.split('\n').collect();

    let mut prefix = 0;
    while prefix < old_lines.len()
        && prefix < new_lines.len()
        && old_lines[prefix] == new_lines[prefix]
    {
        prefix += 1;
    }
    let mut suffix = 0;
    while suffix < old_lines.len() - prefix
        && suffix < new_lines.len() - prefix
        && old_lines[old_lines.len() - 1 - suffix] == new_lines[new_lines.len() - 1 - suffix]
    {
        suffix += 1;
    }

    let context_start = prefix.saturating_sub(DIFF_CONTEXT_LINES);
    let context_end = (old_lines.len() - suffix + DIFF_CONTEXT_LINES).min(old_lines.len());

    let old_count = context_end - context_start;
    let new_count =
        old_count + (new_lines.len() - suffix - prefix) - (old_lines.len() - suffix - prefix);

    let mut out = format!(
        "@@ -{},{} +{},{} @@\n",
        context_start + 1,
        old_count,
        context_start + 1,
        new_count
    );
    for line in &old_lines[context_start..prefix] {
        out.push_str(&format!(" {line}\n"));
    }
    for line in &old_lines[prefix..old_lines.len() - suffix] {
        out.push_str(&format!("-{line}\n"));
    }
    for line in &new_lines[prefix..new_lines.len() - suffix] {
        out.push_str(&format!("+{line}\n"));
    }
    for line in &old_lines[old_lines.len() - suffix..context_end] {
        out.push_str(&format!(" {line}\n"));
    }

    let diff = out.trim_end_matches('\n');
    if diff.len() <= MAX_DIFF_BYTES {
        return diff.to_owned();
    }
    let cut = diff[..MAX_DIFF_BYTES].rfind('\n').unwrap_or(MAX_DIFF_BYTES);
    format!("{}\n... (diff truncated)", &diff[..cut])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::testutil::{run, workspace};

    fn sample(contents: &str) -> (tempfile::TempDir, std::path::PathBuf) {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("sample.txt");
        std::fs::write(&path, contents).unwrap();
        (root, path)
    }

    #[tokio::test]
    async fn unknown_fields_and_missing_arguments_are_rejected() {
        let (root, _) = sample("same\n");
        let workspace = workspace(root.path());
        let tool = EditTool::new(&workspace);

        // Malformed JSON and trailing tokens are rejected by the stream
        // decoder before a `RawValue` exists; see `result::tests`.
        let unknown = run(
            &tool,
            r#"{"path":"sample.txt","old_text":"same","new_text":"new","extra":true}"#,
        )
        .await;
        assert!(
            unknown.is_error && unknown.content.contains("unknown field"),
            "{unknown:?}"
        );

        let missing_path = run(&tool, r#"{"old_text":"same","new_text":"new"}"#).await;
        assert!(
            missing_path.is_error && missing_path.content.contains("path"),
            "{missing_path:?}"
        );

        let missing_old = run(&tool, r#"{"path":"sample.txt","new_text":"new"}"#).await;
        assert!(
            missing_old.is_error && missing_old.content.contains("old_text"),
            "{missing_old:?}"
        );
    }

    #[tokio::test]
    async fn an_ambiguous_or_absent_match_is_an_error() {
        let (root, _) = sample("same\nsame\n");
        let workspace = workspace(root.path());
        let tool = EditTool::new(&workspace);

        let ambiguous = run(
            &tool,
            r#"{"path":"sample.txt","old_text":"same","new_text":"new"}"#,
        )
        .await;
        assert!(ambiguous.is_error);
        assert_eq!(
            ambiguous.content,
            "edit failed: old_text matched 2 locations in sample.txt; include more surrounding context to make it unique"
        );

        let absent = run(
            &tool,
            r#"{"path":"sample.txt","old_text":"missing","new_text":"new"}"#,
        )
        .await;
        assert!(absent.is_error);
        assert_eq!(
            absent.content,
            "edit failed: old_text was not found in sample.txt"
        );
    }

    #[tokio::test]
    async fn match_errors_do_not_echo_the_requested_text() {
        let (root, _) = sample("dup\ndup\n");
        let workspace = workspace(root.path());
        let tool = EditTool::new(&workspace);

        let arguments = serde_json::json!({
            "path": "sample.txt",
            "old_text": "ZQX marker ".repeat(2000),
            "new_text": "new",
        })
        .to_string();
        let not_found = run(&tool, &arguments).await;
        assert!(
            not_found.is_error && !not_found.content.contains("ZQX"),
            "{not_found:?}"
        );
        assert!(not_found.content.len() <= 200, "{not_found:?}");

        let ambiguous = run(
            &tool,
            r#"{"path":"sample.txt","old_text":"dup","new_text":"new"}"#,
        )
        .await;
        assert!(
            ambiguous.is_error && !ambiguous.content.contains("dup"),
            "{ambiguous:?}"
        );
        assert!(ambiguous.content.len() <= 200, "{ambiguous:?}");
    }

    #[tokio::test]
    async fn binary_and_oversized_files_are_rejected() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("binary"), [b'a', 0, b'b']).unwrap();
        let huge = root.path().join("huge.txt");
        std::fs::write(&huge, "x").unwrap();
        std::fs::File::options()
            .write(true)
            .open(&huge)
            .unwrap()
            .set_len(crate::tool::read::MAX_READ_FILE_BYTES + 1)
            .unwrap();
        let workspace = workspace(root.path());
        let tool = EditTool::new(&workspace);

        let binary = run(&tool, r#"{"path":"binary","old_text":"a","new_text":"c"}"#).await;
        assert!(
            binary.is_error && binary.content.contains("binary"),
            "{binary:?}"
        );

        let oversized = run(
            &tool,
            r#"{"path":"huge.txt","old_text":"x","new_text":"y"}"#,
        )
        .await;
        assert!(
            oversized.is_error && oversized.content.contains("too large"),
            "{oversized:?}"
        );
    }

    #[tokio::test]
    async fn an_exact_single_match_is_replaced_and_diffed() {
        let (root, path) = sample("hello world\n");
        let workspace = workspace(root.path());
        let result = run(
            &EditTool::new(&workspace),
            r#"{"path":"sample.txt","old_text":"world","new_text":"there"}"#,
        )
        .await;
        assert!(!result.is_error, "{result:?}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "hello there\n");
        assert!(result.content.contains("sample.txt"), "{result:?}");
        assert!(result.content.contains("-hello world"), "{result:?}");
        assert!(result.content.contains("+hello there"), "{result:?}");
    }

    #[tokio::test]
    async fn an_array_of_edits_is_applied() {
        let (root, path) = sample("one\ntwo\nthree\n");
        let workspace = workspace(root.path());
        let result = run(
            &EditTool::new(&workspace),
            r#"{"path":"sample.txt","edits":[{"oldText":"one","newText":"ONE"},{"oldText":"three","newText":"THREE"}]}"#,
        )
        .await;
        assert!(!result.is_error, "{result:?}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "ONE\ntwo\nTHREE\n");
    }

    #[tokio::test]
    async fn edits_may_be_a_json_string_or_a_single_object() {
        let (root, path) = sample("alpha\nbeta\n");
        let workspace = workspace(root.path());
        let tool = EditTool::new(&workspace);

        let encoded = run(
            &tool,
            r#"{"path":"sample.txt","edits":"{\"oldText\":\"alpha\",\"newText\":\"ALPHA\"}"}"#,
        )
        .await;
        assert!(!encoded.is_error, "{encoded:?}");

        let object = run(
            &tool,
            r#"{"path":"sample.txt","edits":{"old_text":"beta","new_text":"BETA"}}"#,
        )
        .await;
        assert!(!object.is_error, "{object:?}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "ALPHA\nBETA\n");
    }

    #[tokio::test]
    async fn whitespace_quotes_and_dashes_match_fuzzily() {
        let (root, path) =
            sample("const msg = \u{201c}hello\u{201d}  \nconst dash = \"a\u{2014}b\"\n");
        let workspace = workspace(root.path());
        let result = run(
            &EditTool::new(&workspace),
            r#"{"path":"sample.txt","oldText":"const msg = \"hello\"\nconst dash = \"a-b\"","newText":"const msg = \"hi\"\nconst dash = \"a-b\""}"#,
        )
        .await;
        assert!(!result.is_error, "{result:?}");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "const msg = \"hi\"\nconst dash = \"a-b\"\n"
        );
    }

    #[tokio::test]
    async fn a_byte_order_mark_and_crlf_endings_survive() {
        let (root, path) = sample("\u{feff}a\r\nb\r\n");
        let workspace = workspace(root.path());
        let result = run(
            &EditTool::new(&workspace),
            r#"{"path":"sample.txt","oldText":"a\nb\n","newText":"x\ny\n"}"#,
        )
        .await;
        assert!(!result.is_error, "{result:?}");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "\u{feff}x\r\ny\r\n"
        );
    }

    #[tokio::test]
    async fn overlapping_edits_are_refused_without_touching_the_file() {
        let (root, path) = sample("abcdef\n");
        let workspace = workspace(root.path());
        let result = run(
            &EditTool::new(&workspace),
            r#"{"path":"sample.txt","edits":[{"oldText":"abc","newText":"ABC"},{"oldText":"bcd","newText":"BCD"}]}"#,
        )
        .await;
        assert!(
            result.is_error && result.content.contains("overlap"),
            "{result:?}"
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "abcdef\n");
    }

    #[tokio::test]
    async fn every_edit_matches_against_the_original_content() {
        let (root, path) = sample("a\n");
        let workspace = workspace(root.path());
        let result = run(
            &EditTool::new(&workspace),
            r#"{"path":"sample.txt","edits":[{"oldText":"a","newText":"b"},{"oldText":"b","newText":"c"}]}"#,
        )
        .await;
        assert!(
            result.is_error && result.content.contains("old_text was not found"),
            "{result:?}"
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "a\n");
    }

    #[tokio::test]
    async fn the_diff_is_compact_and_trims_unchanged_lines() {
        let lines: Vec<String> = (1..=40).map(|index| format!("line {index}")).collect();
        let (root, _) = sample(&format!("{}\n", lines.join("\n")));
        let workspace = workspace(root.path());
        let result = run(
            &EditTool::new(&workspace),
            r#"{"path":"sample.txt","old_text":"line 20\n","new_text":"changed 20\n"}"#,
        )
        .await;
        assert!(!result.is_error, "{result:?}");
        for want in [
            "@@ -17,7 +17,7 @@",
            "-line 20",
            "+changed 20",
            " line 17",
            " line 23",
        ] {
            assert!(
                result.content.contains(want),
                "diff missing {want:?}: {result:?}"
            );
        }
        for absent in ["line 13", "line 27", "line 40"] {
            assert!(
                !result.content.contains(absent),
                "diff includes {absent:?}: {result:?}"
            );
        }

        let (trimmed_root, _) = sample("alpha\nbeta\ngamma\n");
        let trimmed_workspace = crate::tool::testutil::workspace(trimmed_root.path());
        let trimmed = run(
            &EditTool::new(&trimmed_workspace),
            r#"{"path":"sample.txt","old_text":"alpha\nbeta\ngamma\n","new_text":"alpha\nCHANGED\ngamma\n"}"#,
        )
        .await;
        assert!(!trimmed.is_error, "{trimmed:?}");
        assert!(
            trimmed.content.contains("-beta") && trimmed.content.contains("+CHANGED"),
            "{trimmed:?}"
        );
        assert!(
            !trimmed.content.contains("-alpha") && !trimmed.content.contains("-gamma"),
            "{trimmed:?}"
        );
    }

    #[tokio::test]
    async fn an_oversized_diff_is_truncated_but_the_file_is_written_whole() {
        let (root, path) = sample("target\n");
        let workspace = workspace(root.path());
        let replacement = "replacement line\n".repeat(2000);
        let arguments = serde_json::json!({
            "path": "sample.txt",
            "old_text": "target\n",
            "new_text": replacement,
        })
        .to_string();
        let result = run(&EditTool::new(&workspace), &arguments).await;
        assert!(!result.is_error, "{result:?}");
        assert!(result.content.contains("truncated"), "{result:?}");
        assert!(
            result.content.len() <= 8192,
            "len = {}",
            result.content.len()
        );
        assert_eq!(
            std::fs::metadata(&path).unwrap().len() as usize,
            replacement.len()
        );
    }

    #[tokio::test]
    async fn traversal_is_rejected() {
        let root = tempfile::tempdir().unwrap();
        let nested = root.path().join("inner");
        std::fs::create_dir(&nested).unwrap();
        std::fs::write(root.path().join("escape.txt"), "x").unwrap();
        let workspace = workspace(&nested);
        let result = run(
            &EditTool::new(&workspace),
            r#"{"path":"../escape.txt","old_text":"x","new_text":"y"}"#,
        )
        .await;
        assert!(
            result.is_error && result.content.contains("escapes workspace"),
            "{result:?}"
        );
    }

    #[tokio::test]
    async fn an_internal_final_symlink_is_edited_through() {
        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("target.txt");
        std::fs::write(&target, "write").unwrap();
        std::os::unix::fs::symlink("target.txt", root.path().join("relative-link")).unwrap();
        std::os::unix::fs::symlink(&target, root.path().join("absolute-link")).unwrap();
        let workspace = workspace(root.path());
        let tool = EditTool::new(&workspace);

        for (link, old, new) in [
            ("relative-link", "write", "edit"),
            ("absolute-link", "edit", "edited"),
        ] {
            let result = run(
                &tool,
                &format!(r#"{{"path":"{link}","old_text":"{old}","new_text":"{new}"}}"#),
            )
            .await;
            assert!(!result.is_error, "edit({link}) = {result:?}");
            assert!(
                std::fs::symlink_metadata(root.path().join(link))
                    .unwrap()
                    .file_type()
                    .is_symlink(),
                "link {link} was replaced"
            );
        }
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "edited");
    }
}
