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
use std::sync::Arc;

use otto_core::model::ToolDefinition;
use otto_core::tool::ToolResult;
use serde::Deserialize;
use serde_json::json;
use serde_json::value::RawValue;
use tokio_util::sync::CancellationToken;

use super::read::read_validated_text_file;
use super::result::decode_strict_json;
use super::write::write_file_atomic;
use super::{Tool, definition, empty_as_none, preflight_error, text_result};
use crate::fsops::{PosixPublish, PublishOps};
use crate::workspace::confined::Workspace;

mod diff;
mod line_endings;
mod match_errors;

use diff::*;
use line_endings::*;
use match_errors::*;

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
    workspace: Arc<Workspace>,
    publish: &'a dyn PublishOps,
}

impl<'a> EditTool<'a> {
    pub fn new(workspace: Arc<Workspace>) -> Self {
        Self {
            workspace,
            publish: &PosixPublish,
        }
    }

    #[cfg(test)]
    fn with_publish_ops(workspace: Arc<Workspace>, publish: &'a dyn PublishOps) -> Self {
        Self { workspace, publish }
    }

    fn execute_locked(
        &self,
        rel_path: &str,
        locked_key: &Path,
        edits: &[EditReplacement],
    ) -> ToolResult {
        let path = Path::new(rel_path);
        let file = match self.workspace.open(path) {
            Ok(file) => file,
            Err(error) => return preflight_error(error),
        };
        let text = match read_validated_text_file(file, rel_path) {
            Ok(text) => text,
            Err(message) => return preflight_error(message),
        };

        let (replaced, diff, notes) = match apply_text_edits(&text, rel_path, edits) {
            Ok(applied) => applied,
            Err(message) => return preflight_error(message),
        };

        let relative = match self.workspace.write_relative(path) {
            Ok(relative) => relative,
            Err(error) => return preflight_error(error),
        };
        // The lock was taken on the first resolution; a symbolic link replaced
        // since then must not move the write to a file this lock does not cover.
        if relative != locked_key {
            return preflight_error(format!("{rel_path} changed while it was being edited"));
        }
        if let Err(error) = write_file_atomic(
            &self.workspace,
            &relative,
            replaced.as_bytes(),
            self.publish,
        ) {
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
            "Replace unique text fragments in a workspace file. Combine all known changes to the same file into one edits batch. Choose exactly one argument form: ",
            "single replacement: {\"path\":\"file.txt\",\"old_text\":\"before\",\"new_text\":\"after\"}; ",
            "batch: {\"path\":\"file.txt\",\"edits\":[{\"old_text\":\"before\",\"new_text\":\"after\"}]}. ",
            "For a single replacement, omit edits. For a batch, put EVERY replacement in edits and omit ",
            "top-level old_text/new_text; never split replacements between the two forms. ",
            "An empty new_text deletes the matched text. Batch edits apply together against the original file. When old_text has ",
            "no exact match, a match that ignores trailing whitespace and treats curly quotes, dashes, and ",
            "non-breaking spaces as ASCII is used, and only the part of old_text that new_text changes is rewritten. ",
            "The result shows a diff and notes when new_text drops the line break \
                that ends old_text, joining the next line. After three successful \
                single replacements to one file in a turn, a one-time batching advisory \
                is appended; the edit still succeeds. ",
            "Validation and match errors write nothing. When old_text matches more than once, the error lists the line numbers of up to 5 matches ",
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
                    "description": "Batch replacements: combine all known changes to this file in one call; \
                        put every replacement here and omit top-level old_text/new_text. Omit \
                        for a single top-level replacement. Entries must not overlap and each \
                        matches against the original file.",
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

pub(crate) const EDIT_RETRY: (&str, otto_core::model::RetrySafety) =
    ("edit", otto_core::model::RetrySafety::NonIdempotent);

#[async_trait::async_trait]
impl Tool for EditTool<'_> {
    fn retry_safety(&self) -> otto_core::model::RetrySafety {
        EDIT_RETRY.1
    }

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
        self.execute_locked(&request.path, &key, &edits)
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

#[cfg(test)]
mod tests;
