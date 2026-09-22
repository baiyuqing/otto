//! Transcript entries -> gutter-marked, pre-wrapped `ratatui` lines.
//!
//! Split out of [`super::render`] so the rules that decide what a turn looks
//! like are a pure function of (entries, details, width): no `App`, no frame,
//! no terminal. See [`super::gutter`] for why wrapping happens here instead of
//! in `Paragraph::wrap`.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

use super::entries::{Entry, EntryKind};
use super::gutter::{self, BULLET_MARK, RESULT_MARK, SYSTEM_MARK, USER_MARK};
use super::layout::{escape_plain_text, escape_single_line_text};
use super::markdown;

/// How much of a tool call's arguments, or of one result line, a folded call
/// shows. The cut is by character count rather than terminal width because
/// folding exists to keep a call short: the gutter would otherwise wrap a
/// long line back into the block of text the fold removed.
const TOOL_PREVIEW_LIMIT: usize = 64;

/// How many result lines a folded tool call shows before the rest collapse
/// into a count. Three is enough for the shape of a test summary or a short
/// error, and still cannot push a reply off the screen.
const TOOL_RESULT_LINES: usize = 3;

/// The whole transcript, one blank line between entries so a prompt, a reply,
/// and a tool call read as separate blocks instead of one run of text.
pub(crate) fn lines(entries: &[Entry], details: bool, width: usize) -> Vec<Line<'static>> {
    let mut lines: Vec<Line<'static>> = Vec::new();
    for entry in entries {
        let rendered = entry_lines(entry, details, width);
        if rendered.is_empty() {
            continue;
        }
        if !lines.is_empty() {
            lines.push(Line::default());
        }
        lines.extend(rendered);
    }
    lines
}

/// Renders one entry under its gutter.
///
/// Every entry is one block under one marker: the marker starts it and an
/// aligned indent continues it, so a wrapped prompt and a multi-paragraph
/// reply each read as a single unit. A prompt is literal text the reader
/// typed and keeps its own line breaks; assistant and system text is
/// markdown; a tool call is a header plus its result.
fn entry_lines(entry: &Entry, details: bool, width: usize) -> Vec<Line<'static>> {
    if entry.kind == Some(EntryKind::Tool) {
        return tool_lines(entry, details, width);
    }
    if entry.raw.is_empty() {
        return Vec::new();
    }
    match entry.kind {
        Some(EntryKind::User) => {
            let style = Style::default().fg(Color::Green);
            let text: Vec<Line<'static>> = escape_plain_text(&entry.raw)
                .split('\n')
                .map(|line| Line::from(Span::styled(line.to_string(), style)))
                .collect();
            gutter::block(&text, USER_MARK, style, width)
        }
        kind => {
            let (mark, style) = match kind {
                Some(EntryKind::Error) => (BULLET_MARK, Style::default().fg(Color::Red)),
                Some(EntryKind::Compaction) | Some(EntryKind::System) => {
                    (SYSTEM_MARK, Style::default().add_modifier(Modifier::DIM))
                }
                _ => (BULLET_MARK, Style::default()),
            };
            gutter::block(&markdown::render(&entry.raw).lines, mark, style, width)
        }
    }
}

/// Renders one tool call: a bulleted header, then its result indented under
/// it. Folded (the default) the result is at most [`TOOL_RESULT_LINES`] cut
/// lines plus a count of the rest, so a long `bash` output cannot push the
/// reply that follows it off the screen; `Ctrl+O` ([`super::app::App`]'s
/// details flag) shows the call id, the arguments, and the output in full.
fn tool_lines(entry: &Entry, details: bool, width: usize) -> Vec<Line<'static>> {
    let dim = Style::default().add_modifier(Modifier::DIM);
    let marker_style = if entry.tool_error {
        Style::default().fg(Color::Red)
    } else {
        Style::default().fg(Color::Cyan)
    };
    let result_style = if entry.tool_error {
        Style::default().fg(Color::Red)
    } else {
        dim
    };

    let mut header = vec![Span::styled(
        entry.tool_name.clone(),
        Style::default().add_modifier(Modifier::BOLD),
    )];
    if details {
        header.push(Span::styled(format!(" ({})", entry.tool_call_id), dim));
    } else if !entry.tool_args.is_empty() {
        header.push(Span::styled(format!(" {}", preview(&entry.tool_args)), dim));
    }
    let mut lines = gutter::block(&[Line::from(header)], BULLET_MARK, marker_style, width);

    let indent = gutter::continuation(RESULT_MARK);
    let push_result = |lines: &mut Vec<Line<'static>>, text: String, first: bool| {
        let mark = if first { RESULT_MARK } else { indent.as_str() };
        lines.extend(gutter::block(
            &[Line::from(Span::styled(text, result_style))],
            mark,
            result_style,
            width,
        ));
    };

    if details {
        if !entry.tool_args.is_empty() {
            for line in escape_plain_text(&entry.tool_args).split('\n') {
                push_result(&mut lines, line.to_string(), false);
            }
        }
        if entry.tool_done {
            for (index, line) in escape_plain_text(&entry.tool_output)
                .split('\n')
                .enumerate()
            {
                push_result(&mut lines, line.to_string(), index == 0);
            }
        }
        return lines;
    }

    // A call still running has no result yet, and no row at all.
    if !entry.tool_done {
        return lines;
    }
    if entry.tool_output.trim().is_empty() {
        push_result(&mut lines, "(no output)".to_string(), true);
        return lines;
    }
    let output: Vec<&str> = entry.tool_output.lines().collect();
    for (index, line) in output.iter().take(TOOL_RESULT_LINES).enumerate() {
        push_result(&mut lines, preview(line), index == 0);
    }
    match output.len().saturating_sub(TOOL_RESULT_LINES) {
        0 => {}
        1 => push_result(&mut lines, "+1 line (ctrl+o)".to_string(), false),
        more => push_result(&mut lines, format!("+{more} lines (ctrl+o)"), false),
    }
    lines
}

/// The first line of `text`, control-escaped and cut to
/// [`TOOL_PREVIEW_LIMIT`] characters.
fn preview(text: &str) -> String {
    let line = escape_single_line_text(text.lines().next().unwrap_or_default());
    let mut cut: String = line.chars().take(TOOL_PREVIEW_LIMIT).collect();
    if line.chars().count() > TOOL_PREVIEW_LIMIT {
        cut.push('\u{2026}');
    }
    cut
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row_text(line: &Line<'_>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

    fn rows(lines: &[Line<'_>]) -> Vec<String> {
        lines.iter().map(row_text).collect()
    }

    fn entry(kind: EntryKind, raw: &str) -> Entry {
        Entry {
            id: "e1".to_string(),
            kind: Some(kind),
            raw: raw.to_string(),
            ..Entry::default()
        }
    }

    fn tool_entry(name: &str, args: &str, output: &str) -> Entry {
        Entry {
            id: "t1".to_string(),
            kind: Some(EntryKind::Tool),
            tool_call_id: "call-1".to_string(),
            tool_name: name.to_string(),
            tool_args: args.to_string(),
            tool_output: output.to_string(),
            tool_done: true,
            ..Entry::default()
        }
    }

    #[test]
    fn a_wrapped_prompt_is_marked_once_and_then_aligned() {
        let rendered = entry_lines(&entry(EntryKind::User, "alpha bravo charlie"), false, 12);
        let rows = rows(&rendered);
        assert!(rows.len() > 1, "expected a wrapped prompt");
        assert!(rows[0].starts_with(USER_MARK), "{:?}", rows[0]);
        for row in &rows[1..] {
            assert!(
                row.starts_with("  "),
                "{row:?} is not aligned under the marker"
            );
            assert!(!row.starts_with(USER_MARK), "{row:?} repeats the marker");
        }
    }

    #[test]
    fn a_prompt_is_green_including_its_marker() {
        let rendered = entry_lines(&entry(EntryKind::User, "hi"), false, 40);
        for span in &rendered[0].spans {
            assert_eq!(span.style.fg, Some(Color::Green));
        }
    }

    #[test]
    fn a_multi_line_prompt_keeps_its_own_line_breaks_under_one_marker() {
        let rendered = entry_lines(&entry(EntryKind::User, "one\ntwo"), false, 40);
        assert_eq!(rows(&rendered), ["> one", "  two"]);
    }

    #[test]
    fn a_reply_gets_one_bullet_and_an_aligned_indent() {
        let rendered = entry_lines(
            &entry(EntryKind::Assistant, "alpha bravo charlie"),
            false,
            12,
        );
        let rows = rows(&rendered);
        assert!(rows.len() > 1, "expected a wrapped reply");
        assert!(rows[0].starts_with(BULLET_MARK), "{:?}", rows[0]);
        for row in &rows[1..] {
            assert!(
                row.starts_with("  "),
                "{row:?} is not aligned under the bullet"
            );
            assert!(!row.starts_with(BULLET_MARK), "{row:?} repeats the bullet");
        }
    }

    #[test]
    fn a_reply_keeps_its_bullet_across_its_own_paragraphs() {
        let rendered = entry_lines(&entry(EntryKind::Assistant, "one\n\ntwo"), false, 40);
        let rows = rows(&rendered);
        assert!(rows[0].starts_with(BULLET_MARK), "{:?}", rows[0]);
        assert_eq!(
            rows.iter()
                .filter(|row| row.starts_with(BULLET_MARK))
                .count(),
            1,
            "only the first row of an entry is bulleted: {rows:?}"
        );
    }

    #[test]
    fn a_prompt_and_the_reply_after_it_are_separated_and_distinct() {
        let rendered = lines(
            &[
                entry(EntryKind::User, "what does this do"),
                entry(EntryKind::Assistant, "it guards the query"),
            ],
            false,
            40,
        );
        let rows = rows(&rendered);
        assert_eq!(rows[0], "> what does this do");
        assert_eq!(rows[1], "");
        assert_eq!(rows[2], "⏺ it guards the query");
    }

    #[test]
    fn an_empty_entry_renders_nothing() {
        assert!(entry_lines(&entry(EntryKind::Assistant, ""), false, 40).is_empty());
        assert!(lines(&[entry(EntryKind::Assistant, "")], false, 40).is_empty());
    }

    #[test]
    fn a_prompt_wraps_by_display_column_not_character_count() {
        let rendered = entry_lines(&entry(EntryKind::User, "集群数量极多"), false, 8);
        assert_eq!(rows(&rendered), ["> 集群数", "  量极多"]);
    }

    #[test]
    fn a_folded_tool_call_shows_its_name_and_a_result_under_it() {
        let rendered = entry_lines(
            &tool_entry("bash", "{\"command\":\"ls\"}", "one"),
            false,
            60,
        );
        let rows = rows(&rendered);
        assert_eq!(rows[0], "⏺ bash {\"command\":\"ls\"}");
        assert_eq!(rows[1], "  ⎿ one");
    }

    #[test]
    fn a_folded_tool_call_bullet_is_cyan_and_its_name_bold() {
        let rendered = entry_lines(&tool_entry("read", "", "ok"), false, 60);
        assert_eq!(rendered[0].spans[0].style.fg, Some(Color::Cyan));
        assert!(
            rendered[0].spans[1]
                .style
                .add_modifier
                .contains(Modifier::BOLD)
        );
    }

    #[test]
    fn a_folded_tool_call_shows_at_most_three_result_lines() {
        let output = "l1\nl2\nl3\nl4\nl5\nl6";
        let rendered = entry_lines(&tool_entry("bash", "", output), false, 60);
        let rows = rows(&rendered);
        assert_eq!(
            rows,
            [
                "⏺ bash",
                "  ⎿ l1",
                "    l2",
                "    l3",
                "    +3 lines (ctrl+o)",
            ]
        );
    }

    #[test]
    fn a_folded_tool_call_cuts_a_long_result_line() {
        let long = "x".repeat(200);
        let rendered = entry_lines(&tool_entry("bash", "", &long), false, 200);
        let rows = rows(&rendered);
        assert!(rows[1].chars().count() < 80, "{:?}", rows[1]);
        assert!(rows[1].ends_with('…'), "{:?}", rows[1]);
    }

    #[test]
    fn a_tool_call_with_no_output_says_so() {
        let rendered = entry_lines(&tool_entry("write", "", "  \n "), false, 60);
        assert_eq!(rows(&rendered)[1], "  ⎿ (no output)");
    }

    #[test]
    fn a_running_tool_call_has_no_result_row() {
        let mut call = tool_entry("bash", "", "");
        call.tool_done = false;
        let rendered = entry_lines(&call, false, 60);
        assert_eq!(rendered.len(), 1);
    }

    #[test]
    fn a_failed_tool_call_shows_its_result_in_red() {
        let mut call = tool_entry("bash", "", "boom");
        call.tool_error = true;
        let rendered = entry_lines(&call, false, 60);
        assert_eq!(rendered[1].spans[0].style.fg, Some(Color::Red));
    }

    #[test]
    fn expanded_details_show_the_call_id_and_every_output_line() {
        let rendered = entry_lines(&tool_entry("bash", "{}", "l1\nl2\nl3\nl4"), true, 60);
        let rows = rows(&rendered);
        assert_eq!(rows[0], "⏺ bash (call-1)");
        assert!(rows.iter().any(|row| row.ends_with("l4")), "{rows:?}");
        assert!(
            !rows.iter().any(|row| row.contains("ctrl+o")),
            "expanded output is not counted: {rows:?}"
        );
    }

    #[test]
    fn an_error_entry_is_red() {
        let rendered = entry_lines(&entry(EntryKind::Error, "it broke"), false, 40);
        assert_eq!(rendered[0].spans[0].style.fg, Some(Color::Red));
        assert!(rows(&rendered)[0].starts_with(BULLET_MARK));
    }

    #[test]
    fn a_compaction_entry_uses_the_system_marker() {
        let rendered = entry_lines(&entry(EntryKind::Compaction, "checkpoint"), false, 40);
        assert!(rows(&rendered)[0].starts_with(SYSTEM_MARK));
    }

    #[test]
    fn a_narrow_width_does_not_panic() {
        for width in [0usize, 1, 2, 3, 4] {
            let _ = lines(
                &[
                    entry(EntryKind::User, "alpha"),
                    entry(EntryKind::Assistant, "bravo"),
                    tool_entry("bash", "{\"command\":\"ls\"}", "one\ntwo"),
                ],
                false,
                width,
            );
        }
    }
}
