//! The transcript gutter: one marker column per entry kind, kept on every row.
//!
//! The transcript used to hand whole entries to `Paragraph::wrap` with the
//! marker baked into the first line's text, so a prompt wider than the
//! terminal lost its marker on every row after the first and a reply carried
//! no marker at all. At a glance a turn boundary was invisible. This module
//! wraps styled lines by display column here instead, so the renderer can put
//! a marker on the first row and an aligned indent on the rest; the transcript
//! `Paragraph` then draws pre-wrapped lines with wrapping turned off.
//!
//! Markers are all East Asian width-neutral (`>` is ASCII; `⏺`, `⎿` and `✻`
//! are Neutral, not Ambiguous), so a terminal configured for double-width
//! ambiguous characters still aligns them at one column.
//!
//! [`strip_gutters`] is the inverse for clipboard copy: a drag selects
//! rendered rows, and pasting the markers back is never what the reader meant.

use ratatui::style::Style;
use ratatui::text::{Line, Span};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// The user prompt marker. ASCII, so it is one column in every terminal.
pub(crate) const USER_MARK: &str = "> ";
/// Assistant text, and the header line of a tool call.
pub(crate) const BULLET_MARK: &str = "⏺ ";
/// Compaction checkpoints, notifications, and other system entries.
pub(crate) const SYSTEM_MARK: &str = "✻ ";
/// A tool call's result, indented under its header.
pub(crate) const RESULT_MARK: &str = "  ⎿ ";

/// Every marker a rendered row can start with, longest first so
/// [`strip_gutters`] removes `"  ⎿ "` before it would match its own indent.
const MARKS: [&str; 4] = [RESULT_MARK, USER_MARK, BULLET_MARK, SYSTEM_MARK];

/// The indent that continues `mark` on the rows after the first: as many
/// spaces as the marker takes columns, so wrapped text stays in one block.
pub(crate) fn continuation(mark: &str) -> String {
    " ".repeat(mark.width())
}

/// Splits `spans` into rows at most `width` display columns wide.
///
/// Breaking is by display column at the point where the next character would
/// not fit, the same rule [`super::render::composer_lines`] uses for the
/// composer, so a CJK character counts as two and a returned row never needs
/// re-wrapping. Styles are preserved across a break: a span cut in half
/// becomes two spans with the same style. An empty input is one empty row, so
/// a blank markdown line keeps its blank row.
pub(crate) fn wrap_spans(spans: &[Span<'static>], width: usize) -> Vec<Vec<Span<'static>>> {
    let width = width.max(1);
    let mut rows: Vec<Vec<Span<'static>>> = Vec::new();
    let mut row: Vec<Span<'static>> = Vec::new();
    let mut text = String::new();
    let mut column = 0usize;

    for span in spans {
        for ch in span.content.chars() {
            let ch_width = ch.width().unwrap_or(0);
            if column + ch_width > width && (column > 0 || !text.is_empty()) {
                if !text.is_empty() {
                    row.push(Span::styled(std::mem::take(&mut text), span.style));
                }
                rows.push(std::mem::take(&mut row));
                column = 0;
            }
            text.push(ch);
            column += ch_width;
        }
        if !text.is_empty() {
            row.push(Span::styled(std::mem::take(&mut text), span.style));
        }
    }
    rows.push(row);
    rows
}

/// Wraps a whole block of lines to `width` columns under one gutter: `mark`
/// on the block's first row, its [`continuation`] indent on every row after
/// it, all in `style`.
///
/// One marker per block, not per line, is what makes a prompt or a reply read
/// as a single unit: a wrap or a paragraph break inside it keeps the aligned
/// indent instead of starting what looks like a new turn.
///
/// `width` is the whole column budget including the marker, so the text is
/// wrapped to what is left over. A width narrower than the marker leaves at
/// least one column for text rather than returning nothing.
pub(crate) fn block(
    lines: &[Line<'static>],
    mark: &str,
    style: Style,
    width: usize,
) -> Vec<Line<'static>> {
    let indent = continuation(mark);
    let text_width = width.saturating_sub(mark.width()).max(1);
    let mut out: Vec<Line<'static>> = Vec::new();
    for line in lines {
        for mut spans in wrap_spans(&line.spans, text_width) {
            let prefix = if out.is_empty() {
                mark
            } else {
                indent.as_str()
            };
            let mut row = vec![Span::styled(prefix.to_string(), style)];
            row.append(&mut spans);
            out.push(Line::from(row));
        }
    }
    out
}

/// Removes the rendered gutter from copied text.
///
/// A row that starts with a marker loses it and nothing else: the marker was
/// the whole gutter on that row. The rows without one are continuations, so
/// they lose the indent they all share. Dedenting by the shared minimum
/// rather than by a fixed width is what keeps a fenced code block's own
/// indentation intact: only the columns the gutter added come off, never a
/// level of indentation the text itself carried.
pub(crate) fn strip_gutters(text: &str) -> String {
    let rows: Vec<(bool, &str)> = text
        .lines()
        .map(|line| {
            MARKS
                .iter()
                .find_map(|mark| line.strip_prefix(mark).map(|rest| (true, rest)))
                .unwrap_or((false, line))
        })
        .collect();
    let dedent = rows
        .iter()
        .filter(|(marked, line)| !marked && !line.trim().is_empty())
        .map(|(_, line)| line.len() - line.trim_start_matches(' ').len())
        .min()
        .unwrap_or(0);
    rows.iter()
        .map(|(marked, line)| {
            if line.trim().is_empty() {
                ""
            } else if *marked {
                line
            } else {
                &line[dedent.min(line.len())..]
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::{Color, Modifier};

    fn text(row: &[Span<'static>]) -> String {
        row.iter().map(|span| span.content.as_ref()).collect()
    }

    fn line_text(line: &Line<'static>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

    fn plain(value: &str) -> Line<'static> {
        Line::from(vec![Span::raw(value.to_string())])
    }

    #[test]
    fn every_marker_is_one_column_per_character() {
        for mark in MARKS {
            assert_eq!(
                mark.width(),
                mark.chars().count(),
                "{mark:?} must not be a wide or ambiguous glyph"
            );
        }
    }

    #[test]
    fn wrapping_breaks_at_the_column_budget() {
        let rows = wrap_spans(&[Span::raw("abcdefgh".to_string())], 3);
        let rows: Vec<String> = rows.iter().map(|row| text(row)).collect();
        assert_eq!(rows, ["abc", "def", "gh"]);
    }

    #[test]
    fn wrapping_counts_a_wide_character_as_two_columns() {
        let rows = wrap_spans(&[Span::raw("集群数量".to_string())], 5);
        let rows: Vec<String> = rows.iter().map(|row| text(row)).collect();
        assert_eq!(rows, ["集群", "数量"]);
    }

    #[test]
    fn wrapping_preserves_the_style_of_a_span_it_cuts() {
        let style = Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD);
        let rows = wrap_spans(&[Span::styled("abcd".to_string(), style)], 2);
        assert_eq!(rows.len(), 2);
        for row in &rows {
            assert_eq!(row[0].style, style);
        }
    }

    #[test]
    fn an_empty_line_stays_one_empty_row() {
        assert_eq!(wrap_spans(&[], 10).len(), 1);
        assert_eq!(wrap_spans(&[Span::raw(String::new())], 10).len(), 1);
    }

    #[test]
    fn the_marker_is_on_the_first_row_and_an_indent_on_the_rest() {
        let lines = block(&[plain("abcdefgh")], BULLET_MARK, Style::default(), 5);
        let rendered: Vec<String> = lines.iter().map(line_text).collect();
        assert_eq!(rendered, ["⏺ abc", "  def", "  gh"]);
    }

    #[test]
    fn the_marker_carries_the_entry_style() {
        let style = Style::default().fg(Color::Green);
        let lines = block(&[plain("hi")], USER_MARK, style, 20);
        assert_eq!(lines[0].spans[0].style, style);
    }

    #[test]
    fn a_width_narrower_than_the_marker_still_renders_a_column_of_text() {
        let lines = block(&[plain("ab")], RESULT_MARK, Style::default(), 2);
        let rendered: Vec<String> = lines.iter().map(line_text).collect();
        assert_eq!(rendered, ["  ⎿ a", "    b"]);
    }

    #[test]
    fn copying_drops_markers_and_the_gutter_indent() {
        let copied = strip_gutters("> what does this do\n⏺ it guards the query\n  from a timeout");
        assert_eq!(
            copied,
            "what does this do\nit guards the query\nfrom a timeout"
        );
    }

    #[test]
    fn copying_keeps_indentation_the_text_itself_carried() {
        let copied = strip_gutters("⏺ fn main() {\n      let x = 1;\n  }");
        assert_eq!(copied, "fn main() {\n    let x = 1;\n}");
    }

    #[test]
    fn copying_drops_a_result_marker_before_its_own_indent() {
        let copied = strip_gutters("  ⎿ ok\n    +2 lines");
        assert_eq!(copied, "ok\n+2 lines");
    }

    #[test]
    fn copying_leaves_a_blank_row_blank() {
        assert_eq!(strip_gutters("⏺ one\n\n⏺ two"), "one\n\ntwo");
    }
}
