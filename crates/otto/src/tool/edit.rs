//! The `edit` tool.
//!
//! Replaces one uniquely matching fragment per edit in a workspace file. A
//! match is tried exactly first and then over a whitespace- and
//! punctuation-normalized view of both sides, so text copied through a terminal
//! or a chat client still applies. An inexact match rewrites only the span of
//! `old_text` that `new_text` changes, so the file keeps the bytes the
//! normalization folded away. An ambiguous or missing match is an error rather
//! than a guess.
//!
//! Ownership: the tool borrows its workspace. Concurrency: mutations of one
//! path are serialized through the workspace's per-path lock, which `write`
//! shares, so two concurrent mutations cannot interleave read and write.
//! Errors: every failure is returned in band as an error [`ToolResult`].

use std::collections::HashMap;
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
use super::{Tool, definition, empty_as_none, error_result, preflight_error, text_result};
use crate::fsops::{PosixPublish, PublishOps};

/// Lines of unchanged context kept on either side of a hunk.
const DIFF_CONTEXT_LINES: usize = 3;
/// The largest diff returned to the model before it is cut on a line boundary.
const MAX_DIFF_BYTES: usize = 4096;
/// The most matches an ambiguity error locates.
const MAX_LISTED_MATCHES: usize = 5;
/// The longest file line an error shows before it is cut.
const MAX_SHOWN_LINE_BYTES: usize = 200;
/// The most lines a not-found error shows from the closest region.
const MAX_CLOSEST_LINES: usize = 20;
/// The most file lines one trimmed old_text line may match and still vote.
const MAX_VOTING_LINES: usize = 100;

/// The wire shape of edit arguments. Optional fields distinguish an absent key
/// from an empty string so that `"new_text": ""` stays a valid deletion.
/// Exactly one of `old_text`/`new_text` or `edits` must be present; an empty
/// `edits` array counts as absent, and so does a blank `old_text` and
/// `new_text` pair.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct EditRequest {
    #[serde(default)]
    path: String,
    #[serde(default)]
    old_text: Option<String>,
    #[serde(default)]
    new_text: Option<String>,
    #[serde(default, deserialize_with = "empty_as_none")]
    edits: Option<Vec<EditRequestItem>>,
}

/// One entry of the `edits` array.
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

/// A replacement located in the LF-normalized file content. `text` uses LF line
/// endings; the file's own newline style is restored on write.
#[derive(Debug)]
struct ResolvedEdit {
    start: usize,
    end: usize,
    text: String,
}

impl EditRequest {
    fn replacements(&self) -> Result<Vec<EditReplacement>, String> {
        // A blank string is the mirror of the empty `edits` array: models fill
        // the argument position they are not using beside the one they are.
        // Only a non-empty value asks for a single replacement, so
        // `"new_text": ""` beside a non-empty `old_text` stays a deletion.
        let single = [&self.old_text, &self.new_text]
            .into_iter()
            .any(|text| text.as_deref().is_some_and(|text| !text.is_empty()));
        if self.edits.is_some() && single {
            return Err(
                concat!(
                    "invalid arguments: edits cannot be combined with non-empty top-level old_text/new_text; ",
                    "move every replacement into edits and omit top-level old_text/new_text"
                )
                .to_owned(),
            );
        }
        // `empty_as_none` has already folded an empty list into `None`, so a
        // present `edits` holds at least one entry.
        let Some(edits) = &self.edits else {
            return Ok(vec![replacement(
                self.old_text.as_deref(),
                self.new_text.as_deref(),
            )?]);
        };
        edits
            .iter()
            .map(|item| replacement(item.old_text.as_deref(), item.new_text.as_deref()))
            .collect()
    }
}

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
    publish: &'a dyn PublishOps,
}

impl<'a> EditTool<'a> {
    pub fn new(workspace: &'a Workspace) -> Self {
        Self {
            workspace,
            publish: &PosixPublish,
        }
    }

    #[cfg(test)]
    fn with_publish_ops(workspace: &'a Workspace, publish: &'a dyn PublishOps) -> Self {
        Self { workspace, publish }
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

        let (replaced, diff, notes) = match apply_text_edits(&text, rel_path, edits) {
            Ok(applied) => applied,
            Err(message) => return error_result(message),
        };

        let relative = match self.workspace.write_relative(path) {
            Ok(relative) => relative,
            Err(error) => return error_result(error),
        };
        if let Err(error) =
            write_file_atomic(self.workspace, &relative, replaced.as_bytes(), self.publish)
        {
            return error.into_tool_result();
        }
        let mut output = format!("edited {rel_path}\n{diff}");
        for note in notes {
            if !output.ends_with('\n') {
                output.push('\n');
            }
            output.push_str(&note);
        }
        text_result(output)
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
            "Replace unique text fragments in a workspace file. Choose exactly one argument form: ",
            "single replacement: {\"path\":\"file.txt\",\"old_text\":\"before\",\"new_text\":\"after\"}; ",
            "batch: {\"path\":\"file.txt\",\"edits\":[{\"old_text\":\"before\",\"new_text\":\"after\"}]}. ",
            "For a single replacement, omit edits. For a batch, put EVERY replacement in edits and omit ",
            "top-level old_text/new_text; never split replacements between the two forms. ",
            "An empty new_text deletes the matched text. Batch edits apply together against the original file. When old_text has ",
            "no exact match, a match that ignores trailing whitespace and treats curly quotes, dashes, and ",
            "non-breaking spaces as ASCII is used, and only the part of old_text that new_text changes is rewritten. ",
            "The result shows a diff and notes when new_text drops the line break that ends old_text, joining the next line. ",
            "A match error writes nothing: when old_text matches more than once, it lists the line numbers of up to 5 matches ",
            "with the line before and after each; when old_text is not found, it shows the current file lines that best align with old_text.",
        ),
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
                    "description": "Single replacement only: existing text that must occur exactly once. Omit when using edits."
                },
                "new_text": {
                    "type": "string",
                    "description": "Single replacement only: replacement text; an empty string deletes the match. Omit when using edits."
                },
                "edits": {
                    "type": "array",
                    "description": "Batch replacements: put every replacement here and omit top-level old_text/new_text. Omit for a single top-level replacement. Entries must not overlap and each matches against the original file.",
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
            Err(message) => return preflight_error(message),
        };
        if request.path.is_empty() {
            return preflight_error("missing required argument: path");
        }
        let edits = match request.replacements() {
            Ok(edits) => edits,
            Err(message) => return preflight_error(message),
        };
        let key = match self.workspace.write_relative(Path::new(&request.path)) {
            Ok(key) => key,
            Err(error) => return preflight_error(error),
        };
        let _guard = self.workspace.lock_path(&key).await;
        self.execute_locked(&request.path, &edits)
    }
}

/// Applies every replacement to `text` and renders the diff and any notes
/// about likely mistakes. Every `old_text` is matched against the original
/// content, a leading byte-order mark and the file's newline style are
/// preserved, and overlapping edits are refused.
fn apply_text_edits(
    text: &str,
    path: &str,
    edits: &[EditReplacement],
) -> Result<(String, String, Vec<String>), String> {
    let body = text.strip_prefix('\u{feff}').unwrap_or(text);
    let has_bom = body.len() != text.len();
    let (content, content_to_body) = normalize_line_endings_with_map(body);
    let newline = detect_newline(body);

    let mut matcher = EditMatcher::new(&content);
    let mut resolved = Vec::with_capacity(edits.len());
    let mut notes = Vec::new();
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
        let edit = matcher.resolve(&old_text, &new_text, path, index, edits.len())?;
        if joins_next_line(&content, &edit, &old_text, &new_text) {
            notes.push(joined_line_note(index, edits.len()));
        }
        resolved.push(edit);
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
        notes,
    ))
}

/// Whether the edit drops the line break ending `old_text` while keeping text
/// in its place, so the line after the match continues the replacement. Models
/// often copy a whole line with its break into `old_text` but leave it off
/// `new_text`; deleting the match outright or editing the file's last line
/// joins nothing.
fn joins_next_line(content: &str, edit: &ResolvedEdit, old_text: &str, new_text: &str) -> bool {
    old_text.ends_with('\n')
        && !new_text.is_empty()
        && !new_text.ends_with('\n')
        && edit.end < content.len()
}

fn joined_line_note(index: usize, total: usize) -> String {
    let message = concat!(
        "old_text ends with a line break but new_text does not, so the next line now ",
        "continues the replaced text; if unintended, edit again to restore the line break"
    );
    if total == 1 {
        format!("note: {message}\n")
    } else {
        format!("note: edit {}: {message}\n", index + 1)
    }
}

/// Locates `old_text` in LF-normalized content. The fuzzy view of the content
/// is built on the first inexact lookup and reused for later edits.
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

    fn resolve(
        &mut self,
        old_text: &str,
        new_text: &str,
        path: &str,
        index: usize,
        total: usize,
    ) -> Result<ResolvedEdit, String> {
        let content = self.content;
        if old_text.is_empty() {
            // Only a stripped byte-order mark reaches here empty; an empty
            // needle would match everywhere.
            return Err(match_error(index, total, &not_found(path)));
        }
        let (offsets, count) = occurrences(content, old_text, |_| true);
        if count == 1 {
            let start = offsets[0];
            return Ok(ResolvedEdit {
                start,
                end: start + old_text.len(),
                text: new_text.to_owned(),
            });
        }
        if count > 1 {
            let spans: Vec<_> = offsets
                .iter()
                .map(|&at| (at, at + old_text.len()))
                .collect();
            let message = ambiguous(content, path, &spans, count);
            return Err(match_error(index, total, &message));
        }

        let (fuzzy, fuzzy_offsets) = self
            .fuzzy
            .get_or_insert_with(|| normalize_for_fuzzy_match_with_map(content));
        let (fuzzy_old, mut old_offsets) = normalize_for_fuzzy_match_with_map(old_text);
        if fuzzy_old.trim().is_empty() {
            return Err(match_error(index, total, &not_found(path)));
        }
        // The fuzzy view drops whitespace only where a line ends. When the last
        // line of old_text is text followed by whitespace, a match must also
        // end where a file line does, or `a\t` would match `ab` and the
        // whitespace new_text keeps from it would be lost. A last line of only
        // whitespace is the next line's indentation and keeps matching loosely.
        let tail_start = old_text.rfind('\n').map_or(0, |index| index + 1);
        let tail_end = trim_trailing_fuzzy_whitespace(old_text, tail_start, old_text.len());
        let ends_line = tail_start < tail_end && tail_end < old_text.len();
        let (offsets, count) = occurrences(fuzzy, &fuzzy_old, |at| {
            !ends_line
                || matches!(
                    fuzzy.as_bytes().get(at + fuzzy_old.len()),
                    None | Some(b'\n')
                )
        });
        if count == 0 {
            let message = not_found_near(content, old_text, path);
            return Err(match_error(index, total, &message));
        }
        if count > 1 {
            let spans: Vec<_> = offsets
                .iter()
                .map(|&at| {
                    let last = fuzzy_offsets[at + fuzzy_old.len() - 1];
                    (fuzzy_offsets[at], last + rune_len(content, last))
                })
                .collect();
            let message = ambiguous(content, path, &spans, count);
            return Err(match_error(index, total, &message));
        }
        let start = offsets[0];

        // The file's bytes differ from old_text inside the match (quotes,
        // dashes, trailing whitespace). Keep them wherever new_text leaves
        // old_text unchanged and rewrite only the span between the common
        // prefix and suffix.
        let (prefix, suffix) = common_affixes(old_text, new_text);
        let prefix = keep_interior_whitespace(old_text, new_text, prefix);
        old_offsets.truncate(fuzzy_old.len());
        let first = start + search_ints(&old_offsets, prefix);
        let last = start + search_ints(&old_offsets, old_text.len() - suffix);
        // Anchor the span right after the last matched rune rather than at the
        // next one, so whitespace the fuzzy view dropped from the file stays
        // after the inserted text instead of before it.
        let content_start = if first > start {
            let previous = fuzzy_offsets[first - 1];
            previous + rune_len(content, previous)
        } else {
            fuzzy_offsets[first]
        };
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

/// The first [`MAX_LISTED_MATCHES`] occurrences of the non-empty `needle` in
/// `haystack` that `accept` admits by its offset, and the number of admitted
/// occurrences, counting overlapping ones: `aa` occurs twice in `aaa`, so it is
/// ambiguous there rather than unique.
fn occurrences(
    haystack: &str,
    needle: &str,
    accept: impl Fn(usize) -> bool,
) -> (Vec<usize>, usize) {
    debug_assert!(!needle.is_empty(), "an empty needle matches everywhere");
    let mut first = Vec::new();
    let mut count = 0;
    let mut from = 0;
    while let Some(index) = haystack[from..].find(needle) {
        let at = from + index;
        if accept(at) {
            if first.len() < MAX_LISTED_MATCHES {
                first.push(at);
            }
            count += 1;
        }
        from = at + rune_len(haystack, at);
    }
    (first, count)
}

/// Shortens a common `prefix` that ends inside trailing whitespace of an
/// `old_text` line when `new_text` continues that line with more text. The
/// fuzzy view dropped that whitespace, so the file may not contain it; left in
/// the prefix, it would be treated as unchanged file bytes and lost, although
/// `new_text` needs it between the kept text and the inserted text.
fn keep_interior_whitespace(old_text: &str, new_text: &str, prefix: usize) -> usize {
    if matches!(new_text.as_bytes().get(prefix), None | Some(b'\n')) {
        return prefix;
    }
    let line_start = old_text[..prefix].rfind('\n').map_or(0, |index| index + 1);
    let line_end = old_text[prefix..]
        .find('\n')
        .map_or(old_text.len(), |index| prefix + index);
    prefix.min(trim_trailing_fuzzy_whitespace(
        old_text, line_start, line_end,
    ))
}

/// The byte length of the rune starting at `offset`. Mirrors Go's
/// `utf8.DecodeRuneInString`, which reports one byte for an invalid sequence.
fn rune_len(text: &str, offset: usize) -> usize {
    text[offset..].chars().next().map_or(1, char::len_utf8)
}

/// The smallest index whose value is at least `target`.
fn search_ints(values: &[usize], target: usize) -> usize {
    values.partition_point(|value| *value < target)
}

/// The byte lengths of the longest common prefix and suffix of `a` and `b`, cut
/// at rune boundaries and never overlapping.
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

/// Lists the first matches of an ambiguous `old_text` as `(start, end)` byte
/// ranges of `content` with the lines around each; `count` is the total.
fn ambiguous(content: &str, path: &str, spans: &[(usize, usize)], count: usize) -> String {
    let lines = file_lines(content);
    let starts = line_starts(content);
    let line_of = |offset: usize| search_ints(&starts, offset + 1) - 1;
    // Per match: the line before, the first and last matched lines with an
    // elision between them when not adjacent, and the line after.
    let shown: Vec<Vec<Option<usize>>> = spans
        .iter()
        .map(|&(start, end)| {
            let (first, last) = (line_of(start), line_of(end - 1));
            let mut rows = Vec::new();
            if first > 0 {
                rows.push(Some(first - 1));
            }
            rows.push(Some(first));
            if last > first {
                if last > first + 1 {
                    rows.push(None);
                }
                rows.push(Some(last));
            }
            if last + 1 < lines.len() {
                rows.push(Some(last + 1));
            }
            rows
        })
        .collect();
    let width = number_width(shown.iter().flatten().flatten().max());
    let mut listing = Vec::new();
    for (index, (&(start, end), rows)) in spans.iter().zip(&shown).enumerate() {
        let (first, last) = (line_of(start) + 1, line_of(end - 1) + 1);
        listing.push(if first == last {
            format!("match {}, line {first}:", index + 1)
        } else {
            format!("match {}, lines {first}-{last}:", index + 1)
        });
        listing.extend(rows.iter().map(|row| match row {
            Some(line) => shown_line(&lines, *line, width),
            None => "...".to_owned(),
        }));
    }
    if count > spans.len() {
        listing.push(format!("({} more matches not shown)", count - spans.len()));
    }
    format!(
        "old_text matched {count} locations in {path}; include more surrounding context to make it unique. Lines around each match:\n{}",
        cap_listing(listing.join("\n"))
    )
}

/// Describes the region of `content` whose lines best align with the lines of
/// `old_text`, which has no match. Every non-blank `old_text` line votes for
/// the file position that would put it on an equal line, ignoring leading and
/// trailing whitespace; the position with the most votes wins, the earliest on
/// a tie. The message holds file lines only, never `old_text`.
fn not_found_near(content: &str, old_text: &str, path: &str) -> String {
    let old_lines = file_lines(old_text);
    let nonblank = old_lines
        .iter()
        .filter(|line| !line.trim().is_empty())
        .count();
    if nonblank == 0 {
        return not_found(path);
    }
    let lines = file_lines(content);
    let mut by_text: HashMap<&str, Vec<usize>> = HashMap::new();
    for (index, line) in lines.iter().enumerate() {
        by_text.entry(line.trim()).or_default().push(index);
    }
    // votes[j + old_lines.len() - i] counts old_text lines i that equal file
    // line j when old_text starts at line j - i; the offset keeps the index
    // non-negative.
    let mut votes = vec![0u32; lines.len() + old_lines.len()];
    for (i, line) in old_lines.iter().enumerate() {
        let text = line.trim();
        if text.is_empty() {
            continue;
        }
        // ponytail: a line found on more than MAX_VOTING_LINES file lines, such
        // as `}`, does not vote, which bounds the votes at MAX_VOTING_LINES per
        // old_text line; an old_text made only of such lines gets the no-match
        // message.
        let Some(found) = by_text
            .get(text)
            .filter(|found| found.len() <= MAX_VOTING_LINES)
        else {
            continue;
        };
        for j in found {
            votes[j + old_lines.len() - i] += 1;
        }
    }
    let (best, top) =
        votes.iter().enumerate().fold(
            (0, 0),
            |best, (at, &count)| if count > best.1 { (at, count) } else { best },
        );
    if top == 0 {
        return format!(
            "{}; no line of old_text matches a line of the file, ignoring leading and trailing whitespace",
            not_found(path)
        );
    }
    let start = best as isize - old_lines.len() as isize;
    let first = start.max(0) as usize;
    let end = ((start + old_lines.len() as isize) as usize).min(lines.len());
    let shown_end = end.min(first + MAX_CLOSEST_LINES);
    let width = number_width(Some(&(shown_end - 1)));
    let mut listing: Vec<String> = (first..shown_end)
        .map(|line| shown_line(&lines, line, width))
        .collect();
    if end > shown_end {
        listing.push(format!("({} more lines)", end - shown_end));
    }
    format!(
        "{}. Closest lines in the current file ({top} of {nonblank} old_text lines match, ignoring leading and trailing whitespace):\n{}",
        not_found(path),
        cap_listing(listing.join("\n"))
    )
}

/// The lines of LF-normalized `content`; a final line break ends the last line
/// rather than starting an empty one.
fn file_lines(content: &str) -> Vec<&str> {
    let mut lines: Vec<&str> = content.split('\n').collect();
    if content.ends_with('\n') {
        lines.pop();
    }
    lines
}

/// The byte offset at which each line of `content` starts.
fn line_starts(content: &str) -> Vec<usize> {
    let mut starts = vec![0usize];
    starts.extend(
        content
            .bytes()
            .enumerate()
            .filter(|(_, byte)| *byte == b'\n')
            .map(|(index, _)| index + 1),
    );
    starts
}

/// The digits of the 1-based number of the largest 0-based line index shown.
fn number_width(largest: Option<&usize>) -> usize {
    largest.map_or(1, |line| (line + 1).to_string().len())
}

/// One numbered file line, cut to [`MAX_SHOWN_LINE_BYTES`].
fn shown_line(lines: &[&str], index: usize, width: usize) -> String {
    let text = lines[index];
    let text = if text.len() > MAX_SHOWN_LINE_BYTES {
        let cut = (0..=MAX_SHOWN_LINE_BYTES)
            .rev()
            .find(|cut| text.is_char_boundary(*cut))
            .expect("zero is a boundary");
        format!("{}...", &text[..cut])
    } else {
        text.to_owned()
    };
    format!("{:>width$} | {text}", index + 1)
}

/// Bounds a listing to [`MAX_DIFF_BYTES`].
fn cap_listing(listing: String) -> String {
    match truncate_on_line(&listing) {
        Some(cut) => format!("{cut}\n... (truncated)"),
        None => listing,
    }
}

/// The part of `text` to keep when it exceeds [`MAX_DIFF_BYTES`]: up to the last
/// line break within the limit, or up to the last rune boundary when the limit
/// lands inside the first line. `None` when `text` fits.
fn truncate_on_line(text: &str) -> Option<&str> {
    if text.len() <= MAX_DIFF_BYTES {
        return None;
    }
    // Cutting on a rune boundary keeps the fallback valid UTF-8 when the budget
    // lands mid-rune and no newline precedes it.
    let cut = text.as_bytes()[..MAX_DIFF_BYTES]
        .iter()
        .rposition(|byte| *byte == b'\n')
        .unwrap_or_else(|| {
            (0..=MAX_DIFF_BYTES)
                .rev()
                .find(|index| text.is_char_boundary(*index))
                .expect("zero is a boundary")
        });
    Some(&text[..cut])
}

/// Prefixes `message` with the edit position.
fn match_error(index: usize, total: usize, message: &str) -> String {
    if total == 1 {
        format!("edit failed: {message}")
    } else {
        format!("edit {} failed: {message}", index + 1)
    }
}

/// Applies sorted, non-overlapping edits in one pass. Edit offsets are in
/// normalized content; `offsets` maps them to positions in `text` and is `None`
/// when the two coincide.
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
/// offset it came from. The map is `None` when `text` needs no change, meaning
/// offsets are identical.
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

/// The file's line terminator. A lone CR only counts when the file has no LF at
/// all, so a stray CR inside a line does not change it.
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
/// byte with one extra entry for the input length.
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
/// character is already canonical.
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

/// One changed line range: `old_lines[old_start..old_end]` becomes `new_lines`.
struct DiffHunk {
    old_start: usize,
    old_end: usize,
    new_lines: Vec<String>,
}

/// Renders unified-style hunks for sorted, non-overlapping edits in
/// LF-normalized content. Edits touching the same lines form one hunk, and
/// hunks whose context lines meet are printed together.
fn edit_diff(content: &str, edits: &[ResolvedEdit]) -> String {
    let old_lines: Vec<&str> = content.split('\n').collect();
    let line_starts = line_starts(content);
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
    match truncate_on_line(diff) {
        Some(cut) => format!("{cut}\n... (diff truncated)"),
        None => diff.to_owned(),
    }
}

fn write_diff_lines<S: AsRef<str>>(out: &mut String, marker: char, lines: &[S]) {
    for line in lines {
        out.push(marker);
        out.push_str(line.as_ref());
        out.push('\n');
    }
}

/// The counts of equal leading and trailing lines, never overlapping.
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
    use crate::faults::publish::FaultyPublishOps;
    use crate::faults::{Fault, FaultError, FaultPlan, Rule, Trigger};
    use crate::fsops::PublishStep;
    use crate::tool::testutil::{run, workspace};
    use otto_core::model::{EffectCertainty, OperationDisposition};

    fn publish_fault(step: PublishStep) -> std::sync::Arc<FaultPlan<PublishStep>> {
        FaultPlan::new(vec![Rule::new(
            step,
            Trigger::Always,
            Fault::Before(FaultError::Os(libc::EIO)),
        )])
    }

    fn sample(contents: &str) -> (tempfile::TempDir, std::path::PathBuf) {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("sample.txt");
        std::fs::write(&path, contents).unwrap();
        (root, path)
    }

    #[tokio::test]
    async fn git_config_is_refused_through_a_symlink() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join(".git")).unwrap();
        std::fs::write(root.path().join(".git/config"), "old\n").unwrap();
        std::os::unix::fs::symlink(".git/config", root.path().join("link")).unwrap();
        let workspace = workspace(root.path());
        let tool = EditTool::new(&workspace);

        for path in [".git/config", ".GIT/config", "link"] {
            let result = run(
                &tool,
                &format!(r#"{{"path":"{path}","old_text":"old","new_text":"new"}}"#),
            )
            .await;
            assert!(
                result.is_error && result.content.contains("read-only"),
                "{path}: {result:?}"
            );
        }
        assert_eq!(
            std::fs::read_to_string(root.path().join(".git/config")).unwrap(),
            "old\n"
        );
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
        assert_eq!(
            missing_old.outcome_override.unwrap().effect_certainty,
            EffectCertainty::NotStarted
        );
    }

    #[tokio::test]
    async fn directory_sync_failure_after_edit_rename_is_unknown() {
        let (root, path) = sample("before\n");
        let workspace = workspace(root.path());
        let plan = publish_fault(PublishStep::DirSync);
        let ops = FaultyPublishOps::new(plan.clone());
        let tool = EditTool::with_publish_ops(&workspace, &ops);

        let result = run(
            &tool,
            r#"{"path":"sample.txt","old_text":"before","new_text":"after"}"#,
        )
        .await;
        plan.assert_all_fired();

        assert!(result.is_error, "{result:?}");
        let outcome = result
            .outcome_override
            .expect("post-rename failure override");
        assert_eq!(outcome.disposition, OperationDisposition::Error);
        assert_eq!(outcome.effect_certainty, EffectCertainty::Unknown);
        assert_eq!(std::fs::read_to_string(path).unwrap(), "after\n");
    }

    #[tokio::test]
    async fn rename_failure_keeps_the_file_and_leaves_no_temporary_file() {
        let (root, path) = sample("before\n");
        let workspace = workspace(root.path());
        let plan = publish_fault(PublishStep::Rename);
        let ops = FaultyPublishOps::new(plan.clone());
        let tool = EditTool::with_publish_ops(&workspace, &ops);

        let result = run(
            &tool,
            r#"{"path":"sample.txt","old_text":"before","new_text":"after"}"#,
        )
        .await;

        plan.assert_all_fired();
        assert!(result.is_error, "{result:?}");
        // A plain error result: only a post-rename failure overrides the
        // outcome to `Unknown`.
        assert!(result.outcome_override.is_none(), "{result:?}");
        assert_eq!(std::fs::read_to_string(path).unwrap(), "before\n");
        let names: Vec<_> = std::fs::read_dir(root.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(names, ["sample.txt"]);
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
            "edit failed: old_text matched 2 locations in sample.txt; include more surrounding context to make it unique. Lines around each match:\n\
             match 1, line 1:\n1 | same\n2 | same\n\
             match 2, line 2:\n1 | same\n2 | same"
        );

        let absent = run(
            &tool,
            r#"{"path":"sample.txt","old_text":"missing","new_text":"new"}"#,
        )
        .await;
        assert!(absent.is_error);
        assert_eq!(
            absent.content,
            "edit failed: old_text was not found in sample.txt; no line of old_text matches a line of the file, ignoring leading and trailing whitespace"
        );
    }

    #[tokio::test]
    async fn overlapping_occurrences_are_ambiguous() {
        for (name, file, old_text, line) in [
            ("exact", "aaa\n", "aa", "aaa"),
            (
                "fuzzy",
                "x\u{2019}x\u{2019}x\n",
                "x'x",
                "x\u{2019}x\u{2019}x",
            ),
        ] {
            let (root, path) = sample(file);
            let workspace = workspace(root.path());
            let arguments = serde_json::json!({
                "path": "sample.txt",
                "old_text": old_text,
                "new_text": "X",
            })
            .to_string();
            let result = run(&EditTool::new(&workspace), &arguments).await;
            assert!(result.is_error, "{name}: {result:?}");
            assert_eq!(
                result.content,
                format!(
                    "edit failed: old_text matched 2 locations in sample.txt; include more surrounding context to make it unique. Lines around each match:\nmatch 1, line 1:\n1 | {line}\nmatch 2, line 1:\n1 | {line}"
                ),
                "{name}"
            );
            assert_eq!(std::fs::read_to_string(&path).unwrap(), file, "{name}");
        }
    }

    #[tokio::test]
    async fn match_errors_show_only_bounded_file_lines() {
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

        let (root, _) = sample(&"dup\n".repeat(300));
        let big = crate::tool::testutil::workspace(root.path());
        let ambiguous = run(
            &EditTool::new(&big),
            r#"{"path":"sample.txt","old_text":"dup","new_text":"new"}"#,
        )
        .await;
        assert!(ambiguous.is_error, "{ambiguous:?}");
        assert!(
            ambiguous.content.contains("(295 more matches not shown)"),
            "{ambiguous:?}"
        );
        assert!(
            ambiguous.content.len() <= MAX_DIFF_BYTES + 300,
            "len = {}",
            ambiguous.content.len()
        );

        // Twenty 300-byte lines are cut to 203 bytes each, which exceeds the
        // listing cap.
        let lines: Vec<String> = (0..30)
            .map(|index| format!("{index:02}{}", "q".repeat(300)))
            .collect();
        let (root, _) = sample(&format!("{}\n", lines.join("\n")));
        let big = crate::tool::testutil::workspace(root.path());
        let arguments = serde_json::json!({
            "path": "sample.txt",
            "old_text": format!("{}\nmissing", lines.join("\n")),
            "new_text": "new",
        })
        .to_string();
        let capped = run(&EditTool::new(&big), &arguments).await;
        assert!(capped.is_error, "{capped:?}");
        let (_, listing) = capped.content.split_once(":\n").unwrap();
        assert!(listing.ends_with("\n... (truncated)"), "{capped:?}");
        assert!(
            listing.len() <= MAX_DIFF_BYTES + "\n... (truncated)".len(),
            "len = {}",
            listing.len()
        );
    }

    #[tokio::test]
    async fn an_ambiguous_match_lists_lines_around_each_match() {
        let file = "fn a() {\n    let x = 1;\n    one();\n    two();\n}\nfn b() {\n    let x = 1;\n    one();\n    two();\n}\n";
        let (root, path) = sample(file);
        let workspace = workspace(root.path());
        let arguments = serde_json::json!({
            "path": "sample.txt",
            "old_text": "let x = 1;\n    one();\n    two();",
            "new_text": "X",
        })
        .to_string();
        let result = run(&EditTool::new(&workspace), &arguments).await;
        assert!(result.is_error, "{result:?}");
        assert_eq!(
            result.content,
            "edit failed: old_text matched 2 locations in sample.txt; include more surrounding context to make it unique. Lines around each match:\n\
             match 1, lines 2-4:\n\
             \x201 | fn a() {\n\
             \x202 |     let x = 1;\n\
             ...\n\
             \x204 |     two();\n\
             \x205 | }\n\
             match 2, lines 7-9:\n\
             \x206 | fn b() {\n\
             \x207 |     let x = 1;\n\
             ...\n\
             \x209 |     two();\n\
             10 | }"
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), file);
    }

    #[tokio::test]
    async fn an_ambiguous_fuzzy_match_maps_to_file_lines() {
        let (root, _) = sample("a\nsay(\u{201c}hi\u{201d})\nb\nc\nsay(\u{201c}hi\u{201d})\nd\n");
        let workspace = workspace(root.path());
        let result = run(
            &EditTool::new(&workspace),
            r#"{"path":"sample.txt","old_text":"say(\"hi\")","new_text":"X"}"#,
        )
        .await;
        assert_eq!(
            result.content,
            "edit failed: old_text matched 2 locations in sample.txt; include more surrounding context to make it unique. Lines around each match:\n\
             match 1, line 2:\n1 | a\n2 | say(\u{201c}hi\u{201d})\n3 | b\n\
             match 2, line 5:\n4 | c\n5 | say(\u{201c}hi\u{201d})\n6 | d"
        );
    }

    #[tokio::test]
    async fn a_stale_old_text_shows_the_closest_current_lines() {
        let (root, path) = sample(
            "fn alpha() {}\nfn beta() {}\nassert_eq!(a, b);\nfn gamma() {}\nfn delta() {}\nfn omega() {}\n",
        );
        let workspace = workspace(root.path());
        let arguments = serde_json::json!({
            "path": "sample.txt",
            "old_text": "fn alpha() {}\nfn beta() {}\nassert_eq!(\n    a,\n    b\n);",
            "new_text": "X",
        })
        .to_string();
        let result = run(&EditTool::new(&workspace), &arguments).await;
        assert!(result.is_error, "{result:?}");
        assert_eq!(
            result.content,
            "edit failed: old_text was not found in sample.txt. Closest lines in the current file (2 of 6 old_text lines match, ignoring leading and trailing whitespace):\n\
             1 | fn alpha() {}\n\
             2 | fn beta() {}\n\
             3 | assert_eq!(a, b);\n\
             4 | fn gamma() {}\n\
             5 | fn delta() {}\n\
             6 | fn omega() {}"
        );
        assert!(!result.content.contains("assert_eq!(\n"), "{result:?}");
        assert!(
            std::fs::read_to_string(&path)
                .unwrap()
                .contains("assert_eq!(a, b);")
        );
    }

    #[tokio::test]
    async fn the_closest_lines_take_the_earliest_tie_and_stop_at_20() {
        let (root, _) = sample("one\nmid\none\n");
        let workspace = workspace(root.path());
        let result = run(
            &EditTool::new(&workspace),
            r#"{"path":"sample.txt","old_text":"one\nmissing","new_text":"X"}"#,
        )
        .await;
        assert!(
            result.content.ends_with(":\n1 | one\n2 | mid"),
            "{result:?}"
        );

        let file: String = (1..=30).map(|n| format!("l{n}\n")).collect();
        std::fs::write(root.path().join("sample.txt"), file).unwrap();
        let old_text: Vec<String> = std::iter::once("l1".to_owned())
            .chain((2..=25).map(|n| format!("gone{n}")))
            .collect();
        let arguments = serde_json::json!({
            "path": "sample.txt",
            "old_text": old_text.join("\n"),
            "new_text": "X",
        })
        .to_string();
        let result = run(&EditTool::new(&workspace), &arguments).await;
        let listing: Vec<String> = (1..=20).map(|n| format!("{n:>2} | l{n}")).collect();
        assert!(
            result.content.ends_with(&format!(
                "(1 of 25 old_text lines match, ignoring leading and trailing whitespace):\n{}\n(5 more lines)",
                listing.join("\n")
            )),
            "{result:?}"
        );
    }

    #[tokio::test]
    async fn a_not_found_without_any_matching_line_says_so() {
        let (root, _) = sample("one\ntwo\n");
        let workspace = workspace(root.path());
        let result = run(
            &EditTool::new(&workspace),
            r#"{"path":"sample.txt","old_text":"zzz\nyyy","new_text":"X"}"#,
        )
        .await;
        assert_eq!(
            result.content,
            "edit failed: old_text was not found in sample.txt; no line of old_text matches a line of the file, ignoring leading and trailing whitespace"
        );
    }

    #[tokio::test]
    async fn a_batch_match_error_keeps_its_edit_number_and_listing() {
        let (root, path) = sample("a\nb\nc\n");
        let workspace = workspace(root.path());
        let result = run(
            &EditTool::new(&workspace),
            r#"{"path":"sample.txt","edits":[{"old_text":"a","new_text":"A"},{"old_text":"x\nb","new_text":"B"}]}"#,
        )
        .await;
        assert_eq!(
            result.content,
            "edit 2 failed: old_text was not found in sample.txt. Closest lines in the current file (1 of 2 old_text lines match, ignoring leading and trailing whitespace):\n1 | a\n2 | b"
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "a\nb\nc\n");
    }

    #[tokio::test]
    async fn a_long_file_line_is_cut_in_an_error() {
        let line = format!("AB{}", "z".repeat(300));
        let (root, _) = sample(&format!("{line}\n{line}\n"));
        let workspace = workspace(root.path());
        let result = run(
            &EditTool::new(&workspace),
            r#"{"path":"sample.txt","old_text":"AB","new_text":"X"}"#,
        )
        .await;
        let cut = format!("AB{}...", "z".repeat(198));
        assert_eq!(
            result.content,
            format!(
                "edit failed: old_text matched 2 locations in sample.txt; include more surrounding context to make it unique. Lines around each match:\nmatch 1, line 1:\n1 | {cut}\n2 | {cut}\nmatch 2, line 2:\n1 | {cut}\n2 | {cut}"
            )
        );
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
    async fn dropping_the_line_break_ending_old_text_adds_a_note() {
        let (root, path) = sample("a\nb\nc\n");
        let workspace = workspace(root.path());
        let result = run(
            &EditTool::new(&workspace),
            r#"{"path":"sample.txt","old_text":"a\nb\n","new_text":"x\ny"}"#,
        )
        .await;
        assert!(!result.is_error, "{result:?}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "x\nyc\n");
        assert!(
            result
                .content
                .contains("note: old_text ends with a line break but new_text does not"),
            "{result:?}"
        );
    }

    #[tokio::test]
    async fn a_joining_fuzzy_match_in_a_batch_gets_a_numbered_note() {
        let (root, path) = sample("one \ntwo\nthree\n");
        let workspace = workspace(root.path());
        let result = run(
            &EditTool::new(&workspace),
            r#"{"path":"sample.txt","edits":[{"old_text":"three","new_text":"3"},{"old_text":"one\n","new_text":"1"}]}"#,
        )
        .await;
        assert!(!result.is_error, "{result:?}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "1two\n3\n");
        assert!(
            result.content.contains("note: edit 2: old_text ends"),
            "{result:?}"
        );
        assert!(!result.content.contains("edit 1:"), "{result:?}");
    }

    #[tokio::test]
    async fn kept_or_deleted_line_breaks_add_no_note() {
        for (old_text, new_text, expected) in [
            (r"a\nb\n", r"x\ny\n", "x\ny\nc\n"),
            (r"a\nb", r"x\ny", "x\ny\nc\n"),
            (r"a\n", "", "b\nc\n"),
            (r"c\n", "z", "a\nb\nz"),
        ] {
            let (root, path) = sample("a\nb\nc\n");
            let workspace = workspace(root.path());
            let result = run(
                &EditTool::new(&workspace),
                &format!(
                    r#"{{"path":"sample.txt","old_text":"{old_text}","new_text":"{new_text}"}}"#
                ),
            )
            .await;
            assert!(!result.is_error, "{result:?}");
            assert_eq!(std::fs::read_to_string(&path).unwrap(), expected);
            assert!(!result.content.contains("note:"), "{result:?}");
        }
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

    #[tokio::test]
    async fn mixed_argument_forms_explain_how_to_retry_without_writing() {
        let (root, path) = sample("a b c\n");
        let workspace = workspace(root.path());
        let tool = EditTool::new(&workspace);
        let result = run(
            &tool,
            r#"{"path":"sample.txt","old_text":"a","new_text":"A","edits":[{"old_text":"c","new_text":"C"}]}"#,
        )
        .await;
        assert!(result.is_error, "{result:?}");
        assert!(
            result
                .content
                .contains("move every replacement into edits and omit top-level old_text/new_text"),
            "{result:?}"
        );
        assert_eq!(
            result.outcome_override.unwrap().effect_certainty,
            EffectCertainty::NotStarted
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "a b c\n");

        let retry = run(
            &tool,
            r#"{"path":"sample.txt","edits":[{"old_text":"a","new_text":"A"},{"old_text":"c","new_text":"C"}]}"#,
        )
        .await;
        assert!(!retry.is_error, "{retry:?}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "A b C\n");
    }

    /// A null `edits` array with a single replacement, and the empty array
    /// models send in the same placeholder position.
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

    /// The mirror of the empty `edits` array: models emit `""` in the
    /// old_text/new_text position they are not using, beside the `edits` they
    /// are using.
    #[tokio::test]
    async fn blank_single_keys_allow_an_array_of_edits() {
        let (root, path) = sample("one\ntwo\nthree\n");
        let workspace = workspace(root.path());
        let result = run(
            &EditTool::new(&workspace),
            r#"{"path":"sample.txt","old_text":"","new_text":"","edits":[{"old_text":"one","new_text":"ONE"},{"old_text":"three","new_text":"THREE"}]}"#,
        )
        .await;
        assert!(!result.is_error, "{result:?}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "ONE\ntwo\nTHREE\n");
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
                "keeps old_text trailing whitespace that new_text makes interior",
                "say(\u{201c}hi\u{201d})\n",
                "say(\"hi\") ",
                "say(\"hi\") // x",
                "say(\u{201c}hi\u{201d}) // x\n",
            ),
            (
                "inserts before the file's trailing whitespace",
                "say(\u{201c}hi\u{201d})  \n",
                "say(\"hi\") ",
                "say(\"hi\") // x",
                "say(\u{201c}hi\u{201d}) // x  \n",
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

    /// Stripping the byte-order mark leaves an empty needle, which once
    /// panicked while counting occurrences.
    #[tokio::test]
    async fn a_bom_only_old_text_is_rejected() {
        let (root, path) = sample("\u{feff}hello\n");
        let workspace = workspace(root.path());
        let arguments = serde_json::json!({
            "path": "sample.txt",
            "old_text": "\u{feff}",
            "new_text": "X",
        })
        .to_string();
        let result = run(&EditTool::new(&workspace), &arguments).await;
        assert!(
            result.is_error && result.content.contains("old_text was not found"),
            "{result:?}"
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "\u{feff}hello\n");
    }

    /// Trailing whitespace of old_text is ignored only where a file line ends:
    /// mid-line it must not match other text and lose new_text's whitespace.
    #[tokio::test]
    async fn trailing_old_text_whitespace_matches_only_at_a_line_end() {
        for (name, file, old_text, new_text, want) in [
            (
                "mid-line tab is not ignored",
                "x = foo_bar \u{2014} 1\n",
                "foo\t",
                "baz\t",
                None,
            ),
            (
                "line-end whitespace still matches",
                "say \u{201c}hi\u{201d}\nnext\n",
                "say \"hi\"  ",
                "say \"bye\"  ",
                Some("say \u{201c}bye\u{201d}\nnext\n"),
            ),
            (
                "only line-end candidates count toward ambiguity",
                "a \u{2014} b\na \u{2014} bc\n",
                "a - b ",
                "a - B ",
                Some("a \u{2014} B\na \u{2014} bc\n"),
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
            match want {
                Some(want) => {
                    assert!(!result.is_error, "{name}: {result:?}");
                    assert_eq!(std::fs::read_to_string(&path).unwrap(), want, "{name}");
                }
                None => {
                    assert!(
                        result.is_error && result.content.contains("old_text was not found"),
                        "{name}: {result:?}"
                    );
                    assert_eq!(std::fs::read_to_string(&path).unwrap(), file, "{name}");
                }
            }
        }
    }

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
