//! The `edit` tool. Port of `internal/tool/edit.go`.
//!
//! Replaces one uniquely matching fragment per edit in a workspace file. A
//! match is tried exactly first and then over a whitespace- and
//! punctuation-normalized view of both sides, so text copied through a
//! terminal or a chat client still applies. An inexact match rewrites only the
//! span of `old_text` that `new_text` changes, so the file keeps the bytes the
//! normalization folded away. An ambiguous or missing match is an error rather
//! than a guess.
//!
//! Ownership: the tool borrows its workspace. Concurrency: mutations of one
//! path are serialized through the workspace's per-path lock, which `write`
//! shares, so two concurrent mutations cannot interleave read and write.
//! Errors: every failure is returned in band as an error [`ToolResult`].

use std::path::Path;

use otto_core::model::ToolDefinition;
use otto_core::tool::ToolResult;
use serde::Deserialize;
use serde_json::json;
use serde_json::value::RawValue;
use tokio_util::sync::CancellationToken;

use super::read::read_validated_text_file;
use super::result::decode_strict_json;
use super::workspace::Workspace;
use super::write::write_file_atomic;
use super::{Tool, definition, error_result, text_result};

/// Lines of unchanged context kept on either side of a hunk.
const DIFF_CONTEXT_LINES: usize = 3;
/// The largest diff returned to the model before it is cut on a line boundary.
const MAX_DIFF_BYTES: usize = 4096;

/// The wire shape of edit arguments. Port of `editRequest`: optional fields
/// distinguish an absent key from an empty string so that `"new_text": ""`
/// stays a valid deletion. Exactly one of `old_text`/`new_text` or `edits`
/// must be present; an empty `edits` array counts as absent.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct EditRequest {
    #[serde(default)]
    path: String,
    #[serde(default)]
    old_text: Option<String>,
    #[serde(default)]
    new_text: Option<String>,
    #[serde(default)]
    edits: Option<Vec<EditRequestItem>>,
}

/// One entry of the `edits` array. Port of `editRequestItem`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct EditRequestItem {
    #[serde(default)]
    old_text: Option<String>,
    #[serde(default)]
    new_text: Option<String>,
}

#[derive(Debug, Clone, Default)]
struct EditReplacement {
    old_text: String,
    new_text: String,
}

/// A replacement located in the LF-normalized file content. `text` uses LF
/// line endings; the file's own newline style is restored on write. Port of
/// `resolvedEdit`.
#[derive(Debug)]
struct ResolvedEdit {
    start: usize,
    end: usize,
    text: String,
}

impl EditRequest {
    /// Port of `editRequest.replacements`.
    fn replacements(&self) -> Result<Vec<EditReplacement>, String> {
        let single = self.old_text.is_some() || self.new_text.is_some();
        // An empty `edits` array reads as the absent key: models emit it as a
        // placeholder next to old_text/new_text.
        match self.edits.as_deref() {
            Some(edits) if !edits.is_empty() => {
                if single {
                    return Err(
                        "invalid arguments: pass either old_text and new_text or edits, not both"
                            .to_owned(),
                    );
                }
                edits
                    .iter()
                    .map(|item| replacement(item.old_text.as_deref(), item.new_text.as_deref()))
                    .collect()
            }
            Some(_) if !single => {
                Err("invalid argument edits: must contain at least one replacement".to_owned())
            }
            _ => Ok(vec![replacement(
                self.old_text.as_deref(),
                self.new_text.as_deref(),
            )?]),
        }
    }
}

/// Port of `editRequestItem.replacement`.
fn replacement(old_text: Option<&str>, new_text: Option<&str>) -> Result<EditReplacement, String> {
    let old_text = match old_text {
        Some(text) if !text.is_empty() => text.to_owned(),
        _ => return Err("missing required argument: old_text".to_owned()),
    };
    let Some(new_text) = new_text else {
        return Err("missing required argument: new_text".to_owned());
    };
    Ok(EditReplacement {
        old_text,
        new_text: new_text.to_owned(),
    })
}

/// Replaces text in a workspace file.
pub struct EditTool<'a> {
    workspace: &'a Workspace,
}

impl<'a> EditTool<'a> {
    pub fn new(workspace: &'a Workspace) -> Self {
        Self { workspace }
    }

    fn execute_locked(&self, rel_path: &str, edits: &[EditReplacement]) -> ToolResult {
        let path = Path::new(rel_path);
        let file = match self.workspace.open(path) {
            Ok(file) => file,
            Err(error) => return error_result(error),
        };
        let text = match read_validated_text_file(file, rel_path) {
            Ok(text) => text,
            Err(message) => return error_result(message),
        };

        let (replaced, diff) = match apply_text_edits(&text, rel_path, edits) {
            Ok(applied) => applied,
            Err(message) => return error_result(message),
        };

        let relative = match self.workspace.write_relative(path) {
            Ok(relative) => relative,
            Err(error) => return error_result(error),
        };
        if let Err(message) = write_file_atomic(self.workspace, &relative, replaced.as_bytes()) {
            return error_result(message);
        }
        text_result(format!("edited {rel_path}\n{diff}"))
    }
}

/// The schema advertised for `edit`.
pub fn edit_definition() -> ToolDefinition {
    let old_text = json!({
        "type": "string",
        "description": "Existing text to replace; it must occur exactly once in the file"
    });
    let new_text = json!({
        "type": "string",
        "description": "Replacement text"
    });
    definition(
        "edit",
        concat!(
            "Replace unique text fragments in a workspace file. Pass old_text and new_text for one ",
            "replacement, or edits for several applied together against the original file. When old_text has ",
            "no exact match, a match that ignores trailing whitespace and treats curly quotes, dashes, and ",
            "non-breaking spaces as ASCII is used, and only the part of old_text that new_text changes is rewritten.",
        ),
        json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Workspace-relative file path to edit"
                },
                "old_text": old_text,
                "new_text": new_text,
                "edits": {
                    "type": "array",
                    "description": "Non-overlapping replacements, each matched against the original file",
                    "items": {
                        "type": "object",
                        "additionalProperties": false,
                        "properties": {"old_text": old_text, "new_text": new_text},
                        "required": ["old_text", "new_text"]
                    }
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
        let request: EditRequest = match decode_strict_json(arguments.get(), &["path"]) {
            Ok(request) => request,
            Err(message) => return error_result(message),
        };
        if request.path.is_empty() {
            return error_result("missing required argument: path");
        }
        let edits = match request.replacements() {
            Ok(edits) => edits,
            Err(message) => return error_result(message),
        };
        let key = match self.workspace.write_relative(Path::new(&request.path)) {
            Ok(key) => key,
            Err(error) => return error_result(error),
        };
        let _guard = self.workspace.lock_path(&key).await;
        self.execute_locked(&request.path, &edits)
    }
}

/// Applies every replacement to `text` and renders the diff. Port of
/// `applyTextEdits`: every `old_text` is matched against the original content,
/// a leading byte-order mark and the file's newline style are preserved, and
/// overlapping edits are refused.
fn apply_text_edits(
    text: &str,
    path: &str,
    edits: &[EditReplacement],
) -> Result<(String, String), String> {
    let body = text.strip_prefix('\u{feff}').unwrap_or(text);
    let has_bom = body.len() != text.len();
    let (content, content_to_body) = normalize_line_endings_with_map(body);
    let newline = detect_newline(body);

    let mut matcher = EditMatcher::new(&content);
    let mut resolved = Vec::with_capacity(edits.len());
    for (index, edit) in edits.iter().enumerate() {
        let mut old_text = normalize_line_endings(&edit.old_text);
        let mut new_text = normalize_line_endings(&edit.new_text);
        if let Some(stripped) = old_text.strip_prefix('\u{feff}') {
            // read reports the BOM as part of line 1, so models copy it into old_text.
            old_text = stripped.to_owned();
            new_text = new_text
                .strip_prefix('\u{feff}')
                .unwrap_or(&new_text)
                .to_owned();
        }
        resolved.push(matcher.resolve(&old_text, &new_text, path, index, edits.len())?);
    }

    resolved.sort_by_key(|edit| edit.start);
    for pair in resolved.windows(2) {
        if pair[1].start < pair[0].end {
            return Err(format!(
                "edit failed: edits overlap in {path}; combine them or provide non-overlapping old_text values"
            ));
        }
    }

    let replaced = splice_edits(body, &resolved, content_to_body.as_deref(), newline);
    let diff = edit_diff(&content, &resolved);
    Ok((
        if has_bom {
            format!("\u{feff}{replaced}")
        } else {
            replaced
        },
        diff,
    ))
}

/// Locates `old_text` in LF-normalized content. Port of `editMatcher`: the
/// fuzzy view of the content is built on the first inexact lookup and reused
/// for later edits.
struct EditMatcher<'a> {
    content: &'a str,
    fuzzy: Option<(String, Vec<usize>)>,
}

impl<'a> EditMatcher<'a> {
    fn new(content: &'a str) -> Self {
        Self {
            content,
            fuzzy: None,
        }
    }

    /// Port of `editMatcher.resolve`.
    fn resolve(
        &mut self,
        old_text: &str,
        new_text: &str,
        path: &str,
        index: usize,
        total: usize,
    ) -> Result<ResolvedEdit, String> {
        let content = self.content;
        let count = content.matches(old_text).count();
        if count == 1 {
            let start = content.find(old_text).expect("one match exists");
            return Ok(ResolvedEdit {
                start,
                end: start + old_text.len(),
                text: new_text.to_owned(),
            });
        }
        if count > 1 {
            return Err(match_error(index, total, &ambiguous(count, path)));
        }

        let (fuzzy, fuzzy_offsets) = self
            .fuzzy
            .get_or_insert_with(|| normalize_for_fuzzy_match_with_map(content));
        let (fuzzy_old, mut old_offsets) = normalize_for_fuzzy_match_with_map(old_text);
        if fuzzy_old.trim().is_empty() {
            return Err(match_error(index, total, &not_found(path)));
        }
        let count = fuzzy.matches(&fuzzy_old).count();
        if count == 0 {
            return Err(match_error(index, total, &not_found(path)));
        }
        if count > 1 {
            return Err(match_error(index, total, &ambiguous(count, path)));
        }
        let start = fuzzy.find(&fuzzy_old).expect("one match exists");

        // The file's bytes differ from old_text inside the match (quotes,
        // dashes, trailing whitespace). Keep them wherever new_text leaves
        // old_text unchanged and rewrite only the span between the common
        // prefix and suffix.
        let (prefix, suffix) = common_affixes(old_text, new_text);
        old_offsets.truncate(fuzzy_old.len());
        let first = start + search_ints(&old_offsets, prefix);
        let last = start + search_ints(&old_offsets, old_text.len() - suffix);
        let content_start = fuzzy_offsets[first];
        let mut content_end = content_start;
        if last > first {
            let last_rune = fuzzy_offsets[last - 1];
            content_end = last_rune + rune_len(content, last_rune);
        }
        Ok(ResolvedEdit {
            start: content_start,
            end: content_end,
            text: new_text[prefix..new_text.len() - suffix].to_owned(),
        })
    }
}

/// The byte length of the rune starting at `offset`. Mirrors Go's
/// `utf8.DecodeRuneInString`, which reports one byte for an invalid sequence.
fn rune_len(text: &str, offset: usize) -> usize {
    text[offset..].chars().next().map_or(1, char::len_utf8)
}

/// The smallest index whose value is at least `target`. Port of
/// `sort.SearchInts` over a non-decreasing slice.
fn search_ints(values: &[usize], target: usize) -> usize {
    values.partition_point(|value| *value < target)
}

/// The byte lengths of the longest common prefix and suffix of `a` and `b`,
/// cut at rune boundaries and never overlapping. Port of `commonAffixes`.
fn common_affixes(a: &str, b: &str) -> (usize, usize) {
    let (left, right) = (a.as_bytes(), b.as_bytes());
    let limit = left.len().min(right.len());
    let mut prefix = 0;
    while prefix < limit && left[prefix] == right[prefix] {
        prefix += 1;
    }
    while prefix > 0 && !a.is_char_boundary(prefix) {
        prefix -= 1;
    }
    let limit = limit - prefix;
    let mut suffix = 0;
    while suffix < limit && left[left.len() - 1 - suffix] == right[right.len() - 1 - suffix] {
        suffix += 1;
    }
    while suffix > 0 && !a.is_char_boundary(left.len() - suffix) {
        suffix -= 1;
    }
    (prefix, suffix)
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
fn match_error(index: usize, total: usize, message: &str) -> String {
    if total == 1 {
        format!("edit failed: {message}")
    } else {
        format!("edit {} failed: {message}", index + 1)
    }
}

/// Applies sorted, non-overlapping edits in one pass. Port of `spliceEdits`:
/// edit offsets are in normalized content, `offsets` maps them to positions in
/// `text` and is `None` when the two coincide.
fn splice_edits(
    text: &str,
    edits: &[ResolvedEdit],
    offsets: Option<&[usize]>,
    newline: &str,
) -> String {
    let map = |index: usize| offsets.map_or(index, |offsets| offsets[index]);
    let mut out = String::with_capacity(text.len());
    let mut last = 0;
    for edit in edits {
        out.push_str(&text[last..map(edit.start)]);
        out.push_str(&restore_line_endings(&edit.text, newline));
        last = map(edit.end);
    }
    out.push_str(&text[last..]);
    out
}

fn normalize_line_endings(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\r', "\n")
}

/// Converts CRLF and CR to LF and records, for every normalized byte, the
/// offset it came from. Port of `normalizeLineEndingsWithMap`: the map is
/// `None` when `text` needs no change, meaning offsets are identical.
fn normalize_line_endings_with_map(text: &str) -> (String, Option<Vec<usize>>) {
    if !text.contains('\r') {
        return (text.to_owned(), None);
    }
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
        Some(offsets),
    )
}

/// The file's line terminator. Port of `detectNewline`: a lone CR only counts
/// when the file has no LF at all, so a stray CR inside a line does not change
/// it.
fn detect_newline(text: &str) -> &'static str {
    if text.contains("\r\n") {
        "\r\n"
    } else if text.contains('\n') {
        "\n"
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

/// Folds the characters a copy-paste round trip tends to change and drops
/// trailing whitespace per line, recording the source offset of every output
/// byte with one extra entry for the input length. Port of
/// `normalizeForFuzzyMatchWithMap`.
fn normalize_for_fuzzy_match_with_map(text: &str) -> (String, Vec<usize>) {
    let mut out = String::with_capacity(text.len());
    let mut offsets = Vec::with_capacity(text.len() + 1);
    let mut pos = 0;
    while pos < text.len() {
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

/// One changed line range: `old_lines[old_start..old_end]` becomes
/// `new_lines`. Port of `diffHunk`.
struct DiffHunk {
    old_start: usize,
    old_end: usize,
    new_lines: Vec<String>,
}

/// Renders unified-style hunks for sorted, non-overlapping edits in
/// LF-normalized content. Port of `editDiff`: edits touching the same lines
/// form one hunk, and hunks whose context lines meet are printed together.
fn edit_diff(content: &str, edits: &[ResolvedEdit]) -> String {
    let old_lines: Vec<&str> = content.split('\n').collect();
    let mut line_starts = vec![0usize];
    line_starts.extend(
        content
            .bytes()
            .enumerate()
            .filter(|(_, byte)| *byte == b'\n')
            .map(|(index, _)| index + 1),
    );
    let line_of = |offset: usize| search_ints(&line_starts, offset + 1) - 1;
    let last_line_of = |edit: &ResolvedEdit| {
        if edit.end == edit.start {
            return line_of(edit.start);
        }
        let line = line_of(edit.end - 1);
        if content.as_bytes()[edit.end - 1] == b'\n' {
            // Removing or keeping this newline decides whether the next line joins.
            line + 1
        } else {
            line
        }
    };

    let mut hunks: Vec<DiffHunk> = Vec::new();
    let mut i = 0;
    while i < edits.len() {
        let first = line_of(edits[i].start);
        let mut last = last_line_of(&edits[i]);
        let mut j = i + 1;
        while j < edits.len() && line_of(edits[j].start) <= last {
            last = last.max(last_line_of(&edits[j]));
            j += 1;
        }
        let region_start = line_starts[first];
        let region_end = if last + 1 < line_starts.len() {
            line_starts[last + 1] - 1
        } else {
            content.len()
        };
        let region: Vec<ResolvedEdit> = edits[i..j]
            .iter()
            .map(|edit| ResolvedEdit {
                start: edit.start - region_start,
                end: edit.end - region_start,
                text: edit.text.clone(),
            })
            .collect();
        let before = &old_lines[first..=last];
        let spliced = splice_edits(&content[region_start..region_end], &region, None, "\n");
        let after: Vec<&str> = spliced.split('\n').collect();
        let (prefix, suffix) = common_lines(before, &after);
        if prefix + suffix < before.len() || prefix + suffix < after.len() {
            hunks.push(DiffHunk {
                old_start: first + prefix,
                old_end: last + 1 - suffix,
                new_lines: after[prefix..after.len() - suffix]
                    .iter()
                    .map(|line| (*line).to_owned())
                    .collect(),
            });
        }
        i = j;
    }
    if hunks.is_empty() {
        return "(no textual changes)".to_owned();
    }

    let mut out = String::new();
    let mut delta: isize = 0;
    let mut g = 0;
    while g < hunks.len() {
        let context_start = hunks[g].old_start.saturating_sub(DIFF_CONTEXT_LINES);
        let mut context_end = (hunks[g].old_end + DIFF_CONTEXT_LINES).min(old_lines.len());
        let mut h = g + 1;
        while h < hunks.len()
            && hunks[h].old_start.saturating_sub(DIFF_CONTEXT_LINES) <= context_end
        {
            context_end = (hunks[h].old_end + DIFF_CONTEXT_LINES).min(old_lines.len());
            h += 1;
        }
        let old_count = context_end - context_start;
        let new_count = hunks[g..h].iter().fold(old_count as isize, |count, hunk| {
            count + hunk.new_lines.len() as isize - (hunk.old_end - hunk.old_start) as isize
        });
        out.push_str(&format!(
            "@@ -{},{} +{},{} @@\n",
            context_start + 1,
            old_count,
            context_start as isize + 1 + delta,
            new_count
        ));
        let mut cursor = context_start;
        for hunk in &hunks[g..h] {
            write_diff_lines(&mut out, ' ', &old_lines[cursor..hunk.old_start]);
            write_diff_lines(&mut out, '-', &old_lines[hunk.old_start..hunk.old_end]);
            write_diff_lines(&mut out, '+', &hunk.new_lines);
            cursor = hunk.old_end;
        }
        write_diff_lines(&mut out, ' ', &old_lines[cursor..context_end]);
        delta += new_count - old_count as isize;
        g = h;
    }

    let diff = out.trim_end_matches('\n');
    if diff.len() <= MAX_DIFF_BYTES {
        return diff.to_owned();
    }
    // Go slices raw bytes here; cutting on a rune boundary keeps the fallback
    // valid UTF-8 when the budget lands mid-rune and no newline precedes it.
    let cut = diff.as_bytes()[..MAX_DIFF_BYTES]
        .iter()
        .rposition(|byte| *byte == b'\n')
        .unwrap_or_else(|| {
            (0..=MAX_DIFF_BYTES)
                .rev()
                .find(|index| diff.is_char_boundary(*index))
                .expect("zero is a boundary")
        });
    format!("{}\n... (diff truncated)", &diff[..cut])
}

fn write_diff_lines<S: AsRef<str>>(out: &mut String, marker: char, lines: &[S]) {
    for line in lines {
        out.push(marker);
        out.push_str(line.as_ref());
        out.push('\n');
    }
}

/// The counts of equal leading and trailing lines, never overlapping. Port of
/// `commonLines`.
fn common_lines(a: &[&str], b: &[&str]) -> (usize, usize) {
    let limit = a.len().min(b.len());
    let mut prefix = 0;
    while prefix < limit && a[prefix] == b[prefix] {
        prefix += 1;
    }
    let limit = limit - prefix;
    let mut suffix = 0;
    while suffix < limit && a[a.len() - 1 - suffix] == b[b.len() - 1 - suffix] {
        suffix += 1;
    }
    (prefix, suffix)
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
            r#"{"path":"sample.txt","edits":[{"old_text":"one","new_text":"ONE"},{"old_text":"three","new_text":"THREE"}]}"#,
        )
        .await;
        assert!(!result.is_error, "{result:?}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "ONE\ntwo\nTHREE\n");
    }

    /// Port of `TestEditRejectsUntypedEditShapes`.
    #[tokio::test]
    async fn untyped_edit_shapes_are_rejected() {
        let (root, path) = sample("a b c\n");
        let workspace = workspace(root.path());
        let tool = EditTool::new(&workspace);

        for (name, arguments) in [
            (
                "camel case keys",
                r#"{"path":"sample.txt","oldText":"a","newText":"A"}"#,
            ),
            (
                "string encoded edits",
                r#"{"path":"sample.txt","edits":"[{\"old_text\":\"a\",\"new_text\":\"A\"}]"}"#,
            ),
            (
                "object edits",
                r#"{"path":"sample.txt","edits":{"old_text":"a","new_text":"A"}}"#,
            ),
            (
                "single and list edits",
                r#"{"path":"sample.txt","old_text":"a","new_text":"A","edits":[{"old_text":"c","new_text":"C"}]}"#,
            ),
            ("empty edits", r#"{"path":"sample.txt","edits":[]}"#),
            (
                "edit without new_text",
                r#"{"path":"sample.txt","edits":[{"old_text":"a"}]}"#,
            ),
        ] {
            let result = run(&tool, arguments).await;
            assert!(result.is_error, "{name}: expected an error, got {result:?}");
        }
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "a b c\n");
    }

    /// Port of `TestEditAllowsNullEditsWithSingleReplacement`, extended to the
    /// empty array models send in the same placeholder position.
    #[tokio::test]
    async fn an_empty_edits_key_allows_a_single_replacement() {
        for edits in ["null", "[]"] {
            let (root, path) = sample("a\n");
            let workspace = workspace(root.path());
            let result = run(
                &EditTool::new(&workspace),
                &format!(
                    r#"{{"path":"sample.txt","old_text":"a","new_text":"A","edits":{edits}}}"#
                ),
            )
            .await;
            assert!(!result.is_error, "{edits}: {result:?}");
            assert_eq!(std::fs::read_to_string(&path).unwrap(), "A\n");
        }
    }

    #[tokio::test]
    async fn whitespace_quotes_and_dashes_match_fuzzily() {
        let (root, path) =
            sample("const msg = \u{201c}hello\u{201d}  \nconst dash = \"a\u{2014}b\"\n");
        let workspace = workspace(root.path());
        let result = run(
            &EditTool::new(&workspace),
            r#"{"path":"sample.txt","old_text":"const msg = \"hello\"\nconst dash = \"a-b\"","new_text":"const msg = \"hi\"\nconst dash = \"a-b\""}"#,
        )
        .await;
        assert!(!result.is_error, "{result:?}");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "const msg = \u{201c}hi\u{201d}  \nconst dash = \"a\u{2014}b\"\n"
        );
    }

    /// Port of `TestEditFuzzyMatchOnlyRewritesChangedSpan`.
    #[tokio::test]
    async fn a_fuzzy_match_rewrites_only_the_changed_span() {
        for (name, file, old_text, new_text, want) in [
            (
                "keeps markdown hard break",
                "line one  \nline two\n",
                "line one\nline two",
                "line one\nline 2",
                "line one  \nline 2\n",
            ),
            (
                "keeps curly quotes and em dash",
                "x = \u{201c}a\u{201d} \u{2014} b\n",
                "x = \"a\" - b",
                "x = \"a\" - c",
                "x = \u{201c}a\u{201d} \u{2014} c\n",
            ),
            (
                "keeps trailing whitespace after changed word",
                "foo  \nbar\n",
                "foo\nbar",
                "baz\nbar",
                "baz  \nbar\n",
            ),
            (
                "keeps next line indentation",
                "foo  \n    bar\n",
                "foo\n    ",
                "FOO\n    ",
                "FOO  \n    bar\n",
            ),
            (
                "inserts between fuzzy lines",
                "a  \nb\n",
                "a\nb",
                "a\nX\nb",
                "a  \nX\nb\n",
            ),
        ] {
            let (root, path) = sample(file);
            let workspace = workspace(root.path());
            let arguments = serde_json::json!({
                "path": "sample.txt",
                "old_text": old_text,
                "new_text": new_text,
            })
            .to_string();
            let result = run(&EditTool::new(&workspace), &arguments).await;
            assert!(!result.is_error, "{name}: {result:?}");
            assert_eq!(std::fs::read_to_string(&path).unwrap(), want, "{name}");
        }
    }

    /// Port of `TestEditRejectsWhitespaceOnlyFuzzyOldText`.
    #[tokio::test]
    async fn whitespace_only_old_text_is_rejected() {
        let (root, path) = sample("abc  \n");
        let workspace = workspace(root.path());
        let result = run(
            &EditTool::new(&workspace),
            r#"{"path":"sample.txt","old_text":"\t\n","new_text":"X"}"#,
        )
        .await;
        assert!(
            result.is_error && result.content.contains("old_text was not found"),
            "{result:?}"
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "abc  \n");
    }

    /// Port of `TestEditMatchesBOMPrefixedOldText`.
    #[tokio::test]
    async fn a_bom_prefixed_old_text_matches() {
        for new_text in ["bye", "\u{feff}bye"] {
            let (root, path) = sample("\u{feff}hello\nworld\n");
            let workspace = workspace(root.path());
            let arguments = serde_json::json!({
                "path": "sample.txt",
                "old_text": "\u{feff}hello",
                "new_text": new_text,
            })
            .to_string();
            let result = run(&EditTool::new(&workspace), &arguments).await;
            assert!(!result.is_error, "new_text {new_text:?}: {result:?}");
            assert_eq!(
                std::fs::read_to_string(&path).unwrap(),
                "\u{feff}bye\nworld\n",
                "new_text {new_text:?}"
            );
        }
    }

    /// Port of `TestEditKeepsLFWhenFileContainsStrayCR`.
    #[tokio::test]
    async fn a_stray_cr_does_not_change_the_detected_newline() {
        let (root, path) = sample("a\rb\nc\n");
        let workspace = workspace(root.path());
        let result = run(
            &EditTool::new(&workspace),
            r#"{"path":"sample.txt","old_text":"c","new_text":"x\ny"}"#,
        )
        .await;
        assert!(!result.is_error, "{result:?}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "a\rb\nx\ny\n");
    }

    /// Port of `TestEditDiffRendersOneHunkPerEdit`.
    #[tokio::test]
    async fn the_diff_renders_one_hunk_per_edit() {
        let lines: Vec<String> = (1..=200).map(|index| format!("line {index}")).collect();
        let (root, _) = sample(&format!("{}\n", lines.join("\n")));
        let workspace = workspace(root.path());
        let result = run(
            &EditTool::new(&workspace),
            r#"{"path":"sample.txt","edits":[{"old_text":"line 5\n","new_text":"line 5\nextra\n"},{"old_text":"line 200\n","new_text":"LAST\n"}]}"#,
        )
        .await;
        assert!(!result.is_error, "{result:?}");
        for want in [
            "@@ -3,6 +3,7 @@",
            "+extra",
            "@@ -197,5 +198,5 @@",
            "-line 200",
            "+LAST",
        ] {
            assert!(
                result.content.contains(want),
                "diff missing {want:?}: {result:?}"
            );
        }
        for absent in ["line 100", "truncated", "-line 6"] {
            assert!(
                !result.content.contains(absent),
                "diff includes {absent:?}: {result:?}"
            );
        }
    }

    /// Port of `TestEditDiffMergesEditsOnOneLine`.
    #[tokio::test]
    async fn the_diff_merges_edits_on_one_line() {
        let (root, _) = sample("a b c\n");
        let workspace = workspace(root.path());
        let result = run(
            &EditTool::new(&workspace),
            r#"{"path":"sample.txt","edits":[{"old_text":"a","new_text":"A"},{"old_text":"c","new_text":"C"}]}"#,
        )
        .await;
        assert!(!result.is_error, "{result:?}");
        assert_eq!(result.content.matches("-a b c").count(), 1, "{result:?}");
        assert_eq!(result.content.matches("+A b C").count(), 1, "{result:?}");
        assert_eq!(result.content.matches("@@").count(), 2, "{result:?}");
    }

    /// Port of `TestWriteAndEditShareFileLock`.
    #[tokio::test]
    async fn write_and_edit_share_the_file_lock() {
        let (root, path) = sample("a\n");
        let workspace = workspace(root.path());
        let key = workspace.write_relative(Path::new("sample.txt")).unwrap();
        let edit = EditTool::new(&workspace);
        let write = crate::tool::write::WriteTool::new(&workspace);
        let calls: [(&str, &dyn Tool, &str); 2] = [
            ("write", &write, r#"{"path":"sample.txt","content":"b\n"}"#),
            (
                "edit",
                &edit,
                r#"{"path":"sample.txt","old_text":"a","new_text":"b"}"#,
            ),
        ];
        for (name, tool, arguments) in calls {
            let guard = workspace.lock_path(&key).await;
            let mut call = Box::pin(run(tool, arguments));
            let blocked =
                tokio::time::timeout(std::time::Duration::from_millis(50), &mut call).await;
            assert!(
                blocked.is_err(),
                "{name} completed while the file lock was held: {blocked:?}"
            );
            drop(guard);
            let result = call.await;
            assert!(!result.is_error, "{name}: {result:?}");
            std::fs::write(&path, "a\n").unwrap();
        }
    }

    #[tokio::test]
    async fn a_byte_order_mark_and_crlf_endings_survive() {
        let (root, path) = sample("\u{feff}a\r\nb\r\n");
        let workspace = workspace(root.path());
        let result = run(
            &EditTool::new(&workspace),
            r#"{"path":"sample.txt","old_text":"a\nb\n","new_text":"x\ny\n"}"#,
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
            r#"{"path":"sample.txt","edits":[{"old_text":"abc","new_text":"ABC"},{"old_text":"bcd","new_text":"BCD"}]}"#,
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
            r#"{"path":"sample.txt","edits":[{"old_text":"a","new_text":"b"},{"old_text":"b","new_text":"c"}]}"#,
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
