//! The live region: the rows at the bottom of the terminal Otto still
//! redraws. Everything above them belongs to the terminal.
//!
//! Otto used to own the whole screen (alternate screen + a scroll offset in
//! [`super::app::App`]), which made the wheel and native text selection
//! mutually exclusive: the wheel reaches an alternate-screen application only
//! while mouse reporting is on, and mouse reporting is what stops terminals
//! doing drag-selection. Writing finished entries into the terminal's own
//! scrollback instead gives both back, because both are then the terminal's
//! own behavior. See
//! [the design](../../../../docs/specs/2026-09-20-tui-inline-viewport.md).
//!
//! The region is a [`Viewport::Fixed`] rect. Fixed is the one viewport kind
//! ratatui never re-measures behind Otto's back:
//! [`Terminal::with_options`] takes the rect as given, `autoresize` skips it,
//! and [`Terminal::resize`] assigns and clears it. Every other path
//! ([`Viewport::Inline`], [`Terminal::clear`]) issues a cursor-position query,
//! whose reply arrives on stdin and would race [`super::spawn_key_reader`]'s
//! thread for it.

use ratatui::backend::{Backend, ClearType};
use ratatui::buffer::{Buffer, Cell};
use ratatui::layout::{Position, Rect, Size};
use ratatui::{Terminal, TerminalOptions, Viewport};

use super::app::App;
use super::render;

/// The bottom rows of the screen, and what Otto knows about where they are.
pub(super) struct LiveRegion<B: Backend> {
    terminal: Terminal<B>,
    /// The whole terminal, re-read every frame: a fixed viewport is never
    /// autoresized, so nothing else notices a window resize.
    screen: Size,
    /// The first screen row the region occupies. Rows above it are the
    /// terminal's scrollback and Otto never writes to them again.
    top: u16,
    /// Rows the region occupies, from [`render::live_height`].
    height: u16,
}

impl<B: Backend> LiveRegion<B> {
    /// Places the region below whatever is already on the screen, scrolling
    /// the screen only as far as it takes to make room.
    ///
    /// Reads the cursor position once, so this has to run before the key
    /// reader thread starts: the terminal's reply arrives on stdin and
    /// whichever reader gets there first consumes it.
    pub(super) fn new(mut backend: B, height: u16) -> Result<Self, B::Error> {
        let screen = backend.size()?;
        let height = height.clamp(1, screen.height.max(1));
        // A terminal that never answers the query (crossterm gives up after
        // two seconds) must not keep the TUI from starting. Assuming the
        // cursor is past the last row scrolls the region into place instead
        // of writing over whatever is already on screen.
        let mut top = match backend.get_cursor_position() {
            Ok(position) => position.y.min(screen.height),
            Err(_) => screen.height,
        };
        let overflow = top.saturating_add(height).saturating_sub(screen.height);
        if overflow > 0 {
            backend.set_cursor_position(Position::new(0, screen.height.saturating_sub(1)))?;
            backend.append_lines(overflow)?;
            top -= overflow;
        }
        backend.set_cursor_position(Position::new(0, top))?;
        backend.clear_region(ClearType::AfterCursor)?;
        let terminal = Terminal::with_options(
            backend,
            TerminalOptions {
                viewport: Viewport::Fixed(Rect::new(0, top, screen.width, height)),
            },
        )?;
        Ok(Self {
            terminal,
            screen,
            top,
            height,
        })
    }

    /// Writes out everything that can no longer change, resizes the region to
    /// what is left, and redraws it.
    pub(super) fn present(&mut self, app: &mut App) -> Result<(), B::Error> {
        let screen = self.terminal.backend_mut().size()?;
        if screen != self.screen {
            self.rescreen(screen)?;
        }
        let details = app.show_details;
        let committed = render::committed_buffer(app.take_committable(), details, screen.width);
        if let Some(rows) = committed {
            self.commit(rows)?;
        }
        self.resize(render::live_height(app, screen))?;
        self.terminal
            .draw(|frame| render::draw(frame, app, screen))?;
        Ok(())
    }

    /// Writes the transcript that never made it out and leaves the cursor
    /// directly below it, so the shell prompt continues where Otto stopped.
    pub(super) fn finish(&mut self, app: &App) -> Result<(), B::Error> {
        let rows =
            render::committed_buffer(app.live_entries(), app.show_details, self.screen.width);
        if let Some(rows) = rows {
            self.commit(rows)?;
        }
        self.clear_from(self.top)?;
        self.terminal.show_cursor()?;
        let backend = self.terminal.backend_mut();
        backend.set_cursor_position(Position::new(0, self.top))?;
        backend.flush()
    }

    /// Writes `rows` into the terminal's scrollback above the region, pushing
    /// the region towards the bottom of the screen and then scrolling the
    /// screen once it is there.
    ///
    /// Port of ratatui's `Terminal::insert_before`, which refuses to run on
    /// anything but an inline viewport. Same loop: draw at most a screenful
    /// per pass, scrolling only as much as that pass needs, so the region does
    /// not end up stranded in the middle of the screen.
    pub(super) fn commit(&mut self, rows: Buffer) -> Result<(), B::Error> {
        if self.screen.width == 0 || self.screen.height == 0 {
            return Ok(());
        }
        let mut cells = rows.content.as_slice();
        let mut drawn = i32::from(self.top);
        let mut pending = i32::from(rows.area.height);
        let region = i32::from(self.height);
        let screen = i32::from(self.screen.height);
        while pending + region > screen {
            let chunk = pending.min(screen);
            let scroll = (drawn + chunk - screen).max(0);
            self.scroll_up(scroll as u16)?;
            cells = self.draw_rows((drawn - scroll) as u16, chunk as u16, cells)?;
            drawn += chunk - scroll;
            pending -= chunk;
        }
        let scroll = (drawn + pending + region - screen).max(0);
        self.scroll_up(scroll as u16)?;
        self.draw_rows((drawn - scroll) as u16, pending as u16, cells)?;
        self.top = (drawn + pending - scroll) as u16;
        self.place()
    }

    /// Follows a terminal resize. The rows already written are the terminal's
    /// to reflow, so the region simply re-anchors to the bottom of the new
    /// screen.
    fn rescreen(&mut self, screen: Size) -> Result<(), B::Error> {
        let narrower = screen.width < self.screen.width;
        self.screen = screen;
        self.height = self.height.clamp(1, screen.height.max(1));
        self.top = screen.height.saturating_sub(self.height);
        if narrower {
            // ratatui clears the whole screen when a viewport narrows, and
            // re-anchors it to row 0 while doing so; let it, then put the
            // region back where it belongs.
            self.terminal
                .resize(Rect::new(0, 0, screen.width, self.height))?;
        }
        self.clear_from(self.top)?;
        self.place()
    }

    /// Grows or shrinks the region in place.
    fn resize(&mut self, height: u16) -> Result<(), B::Error> {
        let height = height.clamp(1, self.screen.height.max(1));
        if height == self.height {
            return Ok(());
        }
        let previous_top = self.top;
        // Growing downwards would run off the bottom of the screen, so grow
        // upwards instead and scroll the rows above out of the way.
        let overflow = self
            .top
            .saturating_add(height)
            .saturating_sub(self.screen.height);
        self.scroll_up(overflow)?;
        self.top -= overflow;
        self.height = height;
        // Shrinking leaves the rows the region gave up holding its last frame.
        self.clear_from(previous_top.min(self.top))?;
        self.place()
    }

    fn place(&mut self) -> Result<(), B::Error> {
        self.terminal
            .resize(Rect::new(0, self.top, self.screen.width, self.height))
    }

    fn clear_from(&mut self, y: u16) -> Result<(), B::Error> {
        let backend = self.terminal.backend_mut();
        backend.set_cursor_position(Position::new(0, y))?;
        backend.clear_region(ClearType::AfterCursor)
    }

    fn scroll_up(&mut self, lines: u16) -> Result<(), B::Error> {
        if lines == 0 {
            return Ok(());
        }
        let bottom = self.screen.height.saturating_sub(1);
        let backend = self.terminal.backend_mut();
        backend.set_cursor_position(Position::new(0, bottom))?;
        backend.append_lines(lines)
    }

    fn draw_rows<'cells>(
        &mut self,
        y: u16,
        rows: u16,
        cells: &'cells [Cell],
    ) -> Result<&'cells [Cell], B::Error> {
        let width = usize::from(self.screen.width);
        let (drawn, rest) = cells.split_at((width * usize::from(rows)).min(cells.len()));
        if rows > 0 {
            let backend = self.terminal.backend_mut();
            backend.draw(
                drawn.iter().enumerate().map(|(index, cell)| {
                    ((index % width) as u16, y + (index / width) as u16, cell)
                }),
            )?;
            backend.flush()?;
        }
        Ok(rest)
    }
}

#[cfg(test)]
mod tests {
    use ratatui::backend::TestBackend;
    use ratatui::style::Style;

    use super::*;

    fn screen(width: u16, height: u16, cursor: u16) -> TestBackend {
        let mut backend = TestBackend::new(width, height);
        backend
            .set_cursor_position(Position::new(0, cursor))
            .expect("cursor");
        backend
    }

    fn rows(lines: &[&str], width: u16) -> Buffer {
        let mut buffer = Buffer::empty(Rect::new(0, 0, width, lines.len() as u16));
        for (index, line) in lines.iter().enumerate() {
            buffer.set_string(0, index as u16, line, Style::default());
        }
        buffer
    }

    /// Starting Otto must not scroll away what is already on the screen when
    /// there is room below it.
    #[test]
    fn the_region_starts_below_what_is_already_on_screen() {
        let region = LiveRegion::new(screen(10, 6, 2), 3).expect("region");

        assert_eq!(region.top, 2);
        region.terminal.backend().assert_scrollback_empty();
    }

    #[test]
    fn a_region_that_does_not_fit_scrolls_the_screen_to_make_room() {
        let region = LiveRegion::new(screen(10, 6, 5), 3).expect("region");

        assert_eq!(region.top, 3, "5 + 3 rows needs 2 rows of scrolling");
        assert_eq!(region.terminal.backend().scrollback().area.height, 2);
    }

    /// Committed rows go above the region, which moves down to stay below
    /// them; nothing is scrolled while the screen still has room.
    #[test]
    fn committed_rows_land_above_the_region_and_push_it_down() {
        let mut region = LiveRegion::new(screen(10, 6, 0), 3).expect("region");

        region
            .commit(rows(&["first", "second"], 10))
            .expect("commit");

        assert_eq!(region.top, 2);
        region.terminal.backend().assert_scrollback_empty();
        region.terminal.backend().assert_buffer_lines([
            "first     ",
            "second    ",
            "          ",
            "          ",
            "          ",
            "          ",
        ]);
    }

    /// Once the region is at the bottom, committing scrolls the screen, which
    /// is what puts the rows into the terminal's own scrollback.
    #[test]
    fn committing_at_the_bottom_scrolls_rows_into_scrollback() {
        let mut region = LiveRegion::new(screen(10, 6, 3), 3).expect("region");
        assert_eq!(region.top, 3);

        region
            .commit(rows(&["first", "second"], 10))
            .expect("commit");

        assert_eq!(region.top, 3, "the region stays pinned to the bottom");
        region
            .terminal
            .backend()
            .assert_scrollback_lines(["          ", "          "]);
        region.terminal.backend().assert_buffer_lines([
            "          ",
            "first     ",
            "second    ",
            "          ",
            "          ",
            "          ",
        ]);
    }

    /// More rows than the screen holds still all reach scrollback, in order.
    #[test]
    fn a_commit_taller_than_the_screen_writes_every_row() {
        let mut region = LiveRegion::new(screen(10, 6, 0), 2).expect("region");

        let lines: Vec<String> = (0..10).map(|row| format!("row {row}")).collect();
        let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
        region.commit(rows(&refs, 10)).expect("commit");

        assert_eq!(region.top, 4, "the region ends up at the bottom");
        let backend = region.terminal.backend();
        let written: Vec<String> = backend
            .scrollback()
            .content
            .chunks(10)
            .chain(backend.buffer().content.chunks(10))
            .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
            .filter(|row| !row.trim().is_empty())
            .collect();
        assert_eq!(
            written,
            (0..10)
                .map(|row| format!("{:<10}", format!("row {row}")))
                .collect::<Vec<_>>()
        );
    }

    /// The region grows upwards, because there is nothing below it to grow
    /// into once it is at the bottom of the screen.
    #[test]
    fn growing_at_the_bottom_scrolls_instead_of_running_off_the_screen() {
        let mut region = LiveRegion::new(screen(10, 6, 3), 3).expect("region");

        region.resize(5).expect("resize");

        assert_eq!(region.top, 1);
        assert_eq!(region.height, 5);
        assert_eq!(region.terminal.backend().scrollback().area.height, 2);
    }

    #[test]
    fn a_region_taller_than_the_screen_is_capped_at_it() {
        let mut region = LiveRegion::new(screen(10, 6, 0), 3).expect("region");

        region.resize(99).expect("resize");

        assert_eq!(region.height, 6);
        assert_eq!(region.top, 0);
    }
}
