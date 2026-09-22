//! Mouse text selection over the drawn screen.
//!
//! Otto asks the terminal to report mouse events so the wheel can scroll the
//! transcript (see [`super::TerminalInput::enable_mouse`]). Every emulator
//! that honours that request also stops selecting text on a plain drag, so
//! the selection has to be Otto's own: this module tracks the drag, paints
//! the highlight, and reads the selected text back out of the frame.
//!
//! The text comes from the rendered [`Buffer`], not from the transcript
//! model, because what the user dragged over is the glyphs on screen —
//! already wrapped, already truncated, already scrolled.

use std::io::{self, Write as _};
use std::process::{Command, Stdio};

use ratatui::buffer::Buffer;
use ratatui::style::Modifier;
use unicode_width::UnicodeWidthStr;

/// Which end of a drag an event is. Mapped from crossterm's left-button
/// `Down`/`Drag`/`Up` in [`super::map_terminal_event`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DragPhase {
    Start,
    Extend,
    End,
}

/// A selection in terminal cells, anchored where the drag started.
///
/// `anchor` may sit after `cursor` when the drag went up or left; every read
/// goes through [`Selection::bounds`], which orders them.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Selection {
    anchor: (u16, u16),
    cursor: (u16, u16),
}

impl Selection {
    pub(crate) fn new(col: u16, row: u16) -> Self {
        Self {
            anchor: (col, row),
            cursor: (col, row),
        }
    }

    pub(crate) fn extend(&mut self, col: u16, row: u16) {
        self.cursor = (col, row);
    }

    /// The selection as `(start, end)` in reading order, both inclusive.
    fn bounds(&self) -> ((u16, u16), (u16, u16)) {
        let (anchor, cursor) = (self.anchor, self.cursor);
        // Compare by row first: this is a text-flow selection, not a block.
        if (anchor.1, anchor.0) <= (cursor.1, cursor.0) {
            (anchor, cursor)
        } else {
            (cursor, anchor)
        }
    }

    /// Whether the cell at `(col, row)` is selected.
    fn contains(&self, col: u16, row: u16) -> bool {
        let (start, end) = self.bounds();
        (start.1, start.0) <= (row, col) && (row, col) <= (end.1, end.0)
    }

    /// Is this a selection of nothing? A plain click (press and release
    /// without moving) selects one cell, which is not worth copying.
    pub(crate) fn is_empty(&self) -> bool {
        self.anchor == self.cursor
    }

    /// Reverses the selected cells in place, after everything else is drawn.
    pub(crate) fn highlight(&self, buffer: &mut Buffer) {
        let area = buffer.area;
        for row in area.top()..area.bottom() {
            for col in area.left()..area.right() {
                if self.contains(col, row)
                    && let Some(cell) = buffer.cell_mut((col, row))
                {
                    cell.modifier |= Modifier::REVERSED;
                }
            }
        }
    }

    /// The selected text, one `\n`-joined line per screen row.
    ///
    /// Trailing blanks are dropped: a transcript row is padded to the full
    /// width, and pasting that padding back is never what the user meant.
    /// The transcript's gutter goes the same way, through
    /// [`super::gutter::strip_gutters`]: the markers are Otto's own framing,
    /// not part of the prompt, reply, or command the reader is copying.
    pub(crate) fn text(&self, buffer: &Buffer) -> String {
        let area = buffer.area;
        let (start, end) = self.bounds();
        let mut lines = Vec::new();
        for row in start.1.max(area.top())..=end.1.min(area.bottom().saturating_sub(1)) {
            let first = if row == start.1 {
                start.0.max(area.left())
            } else {
                area.left()
            };
            let last = if row == end.1 {
                end.0.min(area.right().saturating_sub(1))
            } else {
                area.right().saturating_sub(1)
            };
            let mut line = String::new();
            let mut col = first;
            while col <= last {
                let Some(cell) = buffer.cell((col, row)) else {
                    break;
                };
                let symbol = cell.symbol();
                line.push_str(symbol);
                // A wide glyph lives in the first of the cells it covers;
                // ratatui leaves the rest holding a blank, so stepping one
                // cell at a time would read that blank back as real text.
                col += symbol.width().max(1) as u16;
            }
            lines.push(line.trim_end().to_string());
        }
        super::gutter::strip_gutters(&lines.join("\n"))
    }
}

/// The clipboard helpers to try, in order, for this target.
///
/// A helper rather than an OSC 52 escape: `pbcopy` is always present on
/// macOS, and OSC 52 is off by default in Terminal.app and gated behind a
/// preference in iTerm2, where it would fail silently. Linux has no single
/// always-present helper, so the Wayland one is tried before the X11 ones
/// and a host with none of them reports that no clipboard helper was found.
///
/// ponytail: no SSH story. If Otto ever runs on a remote host, OSC 52 is
/// the fallback to add here.
#[cfg(target_os = "macos")]
const CLIPBOARD_HELPERS: &[(&str, &[&str])] = &[("pbcopy", &[])];
/// See the macOS list above.
#[cfg(not(target_os = "macos"))]
const CLIPBOARD_HELPERS: &[(&str, &[&str])] = &[
    ("wl-copy", &[]),
    ("xclip", &["-selection", "clipboard"]),
    ("xsel", &["--clipboard", "--input"]),
];

/// Puts `text` on the system clipboard.
pub(crate) fn copy(text: &str) -> io::Result<()> {
    let mut missing = Vec::new();
    for (helper, arguments) in CLIPBOARD_HELPERS {
        match copy_with(helper, arguments, text) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => missing.push(*helper),
            outcome => return outcome,
        }
    }
    Err(io::Error::new(
        io::ErrorKind::NotFound,
        format!("no clipboard helper found (tried {})", missing.join(", ")),
    ))
}

/// Pipes `text` into one helper. `NotFound` means it is not installed, which
/// [`copy`] treats as "try the next one".
fn copy_with(helper: &str, arguments: &[&str], text: &str) -> io::Result<()> {
    let mut child = Command::new(helper)
        .args(arguments)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    child
        .stdin
        .take()
        .ok_or_else(|| io::Error::other(format!("{helper} took no stdin")))?
        .write_all(text.as_bytes())?;
    match child.wait()?.success() {
        true => Ok(()),
        false => Err(io::Error::other(format!("{helper} failed"))),
    }
}

#[cfg(test)]
mod tests {
    use ratatui::layout::Rect;

    use super::*;

    fn screen(rows: &[&str]) -> Buffer {
        let width = rows
            .iter()
            .map(|row| row.chars().count())
            .max()
            .unwrap_or(0) as u16;
        let mut buffer = Buffer::empty(Rect::new(0, 0, width, rows.len() as u16));
        for (y, row) in rows.iter().enumerate() {
            for (x, symbol) in row.chars().enumerate() {
                buffer
                    .cell_mut((x as u16, y as u16))
                    .expect("cell in bounds")
                    .set_symbol(&symbol.to_string());
            }
        }
        buffer
    }

    #[test]
    fn a_drag_inside_one_row_takes_the_dragged_cells() {
        let buffer = screen(&["hello world"]);
        let mut selection = Selection::new(0, 0);
        selection.extend(4, 0);
        assert_eq!(selection.text(&buffer), "hello");
    }

    #[test]
    fn a_drag_over_the_transcript_copies_without_the_gutter() {
        let buffer = screen(&[
            "> what does this do",
            "\u{23fa} it guards it",
            "  from a timeout",
        ]);
        let mut selection = Selection::new(0, 0);
        selection.extend(30, 2);
        assert_eq!(
            selection.text(&buffer),
            "what does this do\nit guards it\nfrom a timeout"
        );
    }

    #[test]
    fn a_backwards_drag_selects_the_same_text() {
        let buffer = screen(&["hello world"]);
        let mut forwards = Selection::new(6, 0);
        forwards.extend(10, 0);
        let mut backwards = Selection::new(10, 0);
        backwards.extend(6, 0);
        assert_eq!(forwards.text(&buffer), "world");
        assert_eq!(backwards.text(&buffer), "world");
    }

    #[test]
    fn a_multi_row_drag_takes_whole_intermediate_rows() {
        let buffer = screen(&["first line ", "second     ", "third one  "]);
        let mut selection = Selection::new(6, 0);
        selection.extend(4, 2);
        assert_eq!(selection.text(&buffer), "line\nsecond\nthird");
    }

    #[test]
    fn trailing_blanks_are_not_part_of_the_selection() {
        let buffer = screen(&["padded     "]);
        let mut selection = Selection::new(0, 0);
        selection.extend(10, 0);
        assert_eq!(selection.text(&buffer), "padded");
    }

    /// A wide character covers two cells and ratatui leaves the second one
    /// blank, so joining symbols must not read that blank back as a space.
    #[test]
    fn wide_characters_survive_a_round_trip() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 4, 1));
        buffer.set_string(0, 0, "中文", ratatui::style::Style::default());
        let mut selection = Selection::new(0, 0);
        selection.extend(3, 0);
        assert_eq!(selection.text(&buffer), "中文");
    }

    #[test]
    fn a_drag_past_the_edge_is_clamped_to_the_screen() {
        let buffer = screen(&["short"]);
        let mut selection = Selection::new(0, 0);
        selection.extend(400, 9);
        assert_eq!(selection.text(&buffer), "short");
    }

    #[test]
    fn a_click_without_a_drag_selects_nothing_worth_copying() {
        let mut selection = Selection::new(3, 1);
        assert!(selection.is_empty());
        selection.extend(4, 1);
        assert!(!selection.is_empty());
    }

    #[test]
    fn highlighting_reverses_only_the_selected_cells() {
        let mut buffer = screen(&["ab", "cd"]);
        let mut selection = Selection::new(1, 0);
        selection.extend(0, 1);
        selection.highlight(&mut buffer);

        let reversed = |buffer: &Buffer, col, row| {
            buffer
                .cell((col, row))
                .expect("cell in bounds")
                .modifier
                .contains(Modifier::REVERSED)
        };
        assert!(!reversed(&buffer, 0, 0), "'a' is before the anchor");
        assert!(reversed(&buffer, 1, 0), "'b' is the anchor");
        assert!(reversed(&buffer, 0, 1), "'c' is the cursor");
        assert!(!reversed(&buffer, 1, 1), "'d' is past the cursor");
    }
}
