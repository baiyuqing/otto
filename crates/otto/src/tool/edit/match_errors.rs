//! Error text for absent and ambiguous edit matches, with bounded file listings.

use super::*;

pub(super) fn not_found(path: &str) -> String {
    format!("old_text was not found in {path}")
}

/// Lists the first matches of an ambiguous `old_text` as `(start, end)` byte
/// ranges of `content` with the lines around each; `count` is the total.
pub(super) fn ambiguous(
    content: &str,
    path: &str,
    spans: &[(usize, usize)],
    count: usize,
) -> String {
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
pub(super) fn not_found_near(content: &str, old_text: &str, path: &str) -> String {
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
pub(super) fn line_starts(content: &str) -> Vec<usize> {
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
pub(super) fn truncate_on_line(text: &str) -> Option<&str> {
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
pub(super) fn match_error(index: usize, total: usize, message: &str) -> String {
    if total == 1 {
        format!("edit failed: {message}")
    } else {
        format!("edit {} failed: {message}", index + 1)
    }
}
