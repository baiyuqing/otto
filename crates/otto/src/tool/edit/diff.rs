//! Compact diff rendering for applied edits.

use super::*;

/// One changed line range: `old_lines[old_start..old_end]` becomes `new_lines`.
struct DiffHunk {
    old_start: usize,
    old_end: usize,
    new_lines: Vec<String>,
}

/// Renders unified-style hunks for sorted, non-overlapping edits in
/// LF-normalized content. Edits touching the same lines form one hunk, and
/// hunks whose context lines meet are printed together.
pub(super) fn edit_diff(content: &str, edits: &[ResolvedEdit]) -> String {
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
