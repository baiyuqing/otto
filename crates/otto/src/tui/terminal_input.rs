//! Terminal mode lifecycle and the key reader task.

use super::*;

#[derive(Default)]
pub(super) struct TerminalInput {
    mouse_active: bool,
    paste_active: bool,
}

impl TerminalInput {
    pub(super) fn new() -> Self {
        Self::default()
    }

    pub(super) fn enable(&mut self, mut writer: impl io::Write) -> io::Result<()> {
        self.enable_mouse(&mut writer)?;
        self.enable_paste(&mut writer)
    }

    pub(super) fn restore(&mut self, mut writer: impl io::Write) -> io::Result<()> {
        let paste = self.restore_paste(&mut writer);
        let mouse = self.restore_mouse(&mut writer);
        paste.and(mouse)
    }

    /// Asks the terminal to report button events for the whole session.
    ///
    /// This is what makes the wheel scroll the transcript: the TUI runs on
    /// the alternate screen, which has no scrollback, so a notch the
    /// terminal keeps to itself has nowhere to go.
    ///
    /// The terminal then hands Otto plain click-drag instead of selecting
    /// text with it, which is why [`selection`] exists.
    ///
    /// Deliberately not `crossterm::event::EnableMouseCapture`, which also
    /// sets `?1003` (report every motion, button or not). Otto only ever
    /// reads motion while the left button is held, and `?1003` would wake
    /// the event loop on every pointer move across the window. `?1000`
    /// (button press/release, which is how the wheel arrives), `?1002`
    /// (motion while a button is held) and `?1006` (SGR encoding, needed
    /// past column 223) are the modes it actually uses.
    pub(super) fn enable_mouse(&mut self, mut writer: impl io::Write) -> io::Result<()> {
        if self.mouse_active {
            return Ok(());
        }
        writer.write_all(b"\x1b[?1000h\x1b[?1002h\x1b[?1006h")?;
        writer.flush()?;
        self.mouse_active = true;
        Ok(())
    }

    pub(super) fn restore_mouse(&mut self, mut writer: impl io::Write) -> io::Result<()> {
        if !self.mouse_active {
            return Ok(());
        }
        writer.write_all(b"\x1b[?1006l\x1b[?1002l\x1b[?1000l")?;
        writer.flush()?;
        self.mouse_active = false;
        Ok(())
    }
    pub(super) fn enable_paste(&mut self, mut writer: impl io::Write) -> io::Result<()> {
        if self.paste_active {
            return Ok(());
        }
        crossterm::execute!(writer, EnableBracketedPaste)?;
        self.paste_active = true;
        Ok(())
    }

    pub(super) fn restore_paste(&mut self, mut writer: impl io::Write) -> io::Result<()> {
        if !self.paste_active {
            return Ok(());
        }
        crossterm::execute!(writer, DisableBracketedPaste)?;
        self.paste_active = false;
        Ok(())
    }
}

impl Drop for TerminalInput {
    fn drop(&mut self) {
        let _ = self.restore(std::io::stdout());
    }
}

pub(super) fn io_error(error: std::io::Error) -> ReplError {
    ReplError::Input(error.to_string())
}

/// One event [`spawn_key_reader`] forwards to the main loop: a keypress, one
/// mouse-wheel notch, or a resize that only needs a redraw.
///
/// The wheel is its own variant rather than a synthesized `Up`/`Down` key,
/// because the composer's own Up/Down recall prompt history
/// ([`app::History`]); a wheel notch always scrolls the transcript.
pub(super) enum TuiEvent {
    Key(KeyEvent),
    Paste(String),
    Wheel {
        up: bool,
    },
    /// One end of a left-button drag, in terminal cells. Otto draws its own
    /// selection because asking for mouse reporting takes the terminal's
    /// away; see [`selection`].
    Select {
        phase: DragPhase,
        col: u16,
        row: u16,
    },
    Redraw,
}

pub(super) fn map_terminal_event(event: TermEvent) -> Option<TuiEvent> {
    match event {
        TermEvent::Key(key) if key.kind == KeyEventKind::Press => Some(TuiEvent::Key(key)),
        TermEvent::Paste(text) => Some(TuiEvent::Paste(text)),
        TermEvent::Mouse(mouse) => {
            let phase = match mouse.kind {
                MouseEventKind::ScrollUp => return Some(TuiEvent::Wheel { up: true }),
                MouseEventKind::ScrollDown => return Some(TuiEvent::Wheel { up: false }),
                MouseEventKind::Down(MouseButton::Left) => DragPhase::Start,
                MouseEventKind::Drag(MouseButton::Left) => DragPhase::Extend,
                MouseEventKind::Up(MouseButton::Left) => DragPhase::End,
                _ => return None,
            };
            Some(TuiEvent::Select {
                phase,
                col: mouse.column,
                row: mouse.row,
            })
        }
        TermEvent::Resize(_, _) => Some(TuiEvent::Redraw),
        _ => None,
    }
}

/// Reads terminal events on a blocking OS thread. Unlike `cli::repl`'s
/// `spawn_reader`, this deliberately uses a plain [`std::thread::spawn`]
/// rather than `tokio::task::spawn_blocking`: `crossterm::event::read()` only
/// returns on the next terminal event, EOF, or an I/O error, so this loop is
/// still parked in the `read`/`kevent` syscall at the moment `run` returns
/// (there is no terminal event that means "the app exited"). A task spawned
/// via `spawn_blocking` is tracked by Tokio's blocking pool, and dropping the
/// `Runtime` in `main` waits for every tracked blocking task to finish before
/// the process can exit — so that combination hangs the whole process after
/// `/exit` until one more key is pressed. A raw OS thread carries no such
/// join: the process exits (`main` returning `ExitCode` calls
/// `std::process::exit`, which does not wait for other threads) the moment
/// `run` is done, regardless of whether this thread is still blocked in
/// `crossterm::event::read()`.
pub(super) fn spawn_key_reader() -> mpsc::Receiver<TuiEvent> {
    let (sender, receiver) = mpsc::channel(1);
    std::thread::spawn(move || {
        while let Ok(event) = crossterm::event::read() {
            // ponytail: focus/paste/key-release and right/middle-button
            // events have no behavior; drop them instead of redrawing.
            let Some(forwarded) = map_terminal_event(event) else {
                continue;
            };
            if sender.blocking_send(forwarded).is_err() {
                break;
            }
        }
    });
    receiver
}
