//! Terminal mode lifecycle and the key reader task.

use super::*;
use std::sync::atomic::{AtomicBool, Ordering};

/// True while a TUI session has mouse reporting or bracketed paste enabled on
/// the real stdout. Read by the panic hook, which cannot reach the
/// `TerminalInput` value.
static TERMINAL_INPUT_MODES_ENABLED: AtomicBool = AtomicBool::new(false);

/// Sequences that disable bracketed paste and mouse reporting.
const TERMINAL_INPUT_MODES_RESET: &[u8] = b"\x1b[?2004l\x1b[?1006l\x1b[?1002l\x1b[?1000l";

#[derive(Default)]
pub(super) struct TerminalInput {
    pub(super) mouse_active: bool,
    pub(super) paste_active: bool,
}

impl TerminalInput {
    pub(super) fn new() -> Self {
        Self::default()
    }

    pub(super) fn enable(&mut self, mut writer: impl io::Write) -> io::Result<()> {
        self.enable_mouse(&mut writer)?;
        self.enable_paste(&mut writer)
    }

    /// [`Self::enable`] on the real stdout, recording the result in
    /// `TERMINAL_INPUT_MODES_ENABLED`.
    pub(super) fn enable_terminal(&mut self) -> io::Result<()> {
        self.enable_recorded(std::io::stdout(), &TERMINAL_INPUT_MODES_ENABLED)
    }

    /// [`Self::restore`] on the real stdout, recording the result in
    /// `TERMINAL_INPUT_MODES_ENABLED`.
    pub(super) fn restore_terminal(&mut self) -> io::Result<()> {
        self.restore_recorded(std::io::stdout(), &TERMINAL_INPUT_MODES_ENABLED)
    }

    pub(super) fn enable_recorded(
        &mut self,
        writer: impl io::Write,
        record: &AtomicBool,
    ) -> io::Result<()> {
        let result = self.enable(writer);
        record.store(self.mouse_active || self.paste_active, Ordering::SeqCst);
        result
    }

    pub(super) fn restore_recorded(
        &mut self,
        writer: impl io::Write,
        record: &AtomicBool,
    ) -> io::Result<()> {
        let result = self.restore(writer);
        record.store(self.mouse_active || self.paste_active, Ordering::SeqCst);
        result
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
        let _ = self.restore_terminal();
    }
}

/// Disables mouse reporting and bracketed paste on stdout when a TUI session
/// enabled them. Called from the panic hook, where `Drop` does not run.
/// Writes nothing when no session enabled them, because stdout may be a pipe.
pub fn reset_terminal_input_modes_after_panic() {
    terminal_title::restore_after_panic();
    reset_input_modes(&TERMINAL_INPUT_MODES_ENABLED, std::io::stdout());
}

pub(super) fn reset_input_modes(record: &AtomicBool, mut writer: impl io::Write) {
    if !record.swap(false, Ordering::SeqCst) {
        return;
    }
    let _ = writer.write_all(TERMINAL_INPUT_MODES_RESET);
    let _ = writer.flush();
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
        steps: u16,
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
    /// Dropping this acknowledgement releases the reader after terminal handoff.
    #[cfg(unix)]
    Suspend(std::sync::mpsc::Sender<()>),
}

pub(super) fn map_terminal_event(event: TermEvent) -> Option<TuiEvent> {
    match event {
        TermEvent::Key(key) if key.kind == KeyEventKind::Press => Some(TuiEvent::Key(key)),
        TermEvent::Paste(text) => Some(TuiEvent::Paste(text)),
        TermEvent::Mouse(mouse) => {
            let phase = match mouse.kind {
                MouseEventKind::ScrollUp => return Some(TuiEvent::Wheel { up: true, steps: 1 }),
                MouseEventKind::ScrollDown => {
                    return Some(TuiEvent::Wheel {
                        up: false,
                        steps: 1,
                    });
                }
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

#[cfg(unix)]
pub(super) fn is_suspend_key(key: KeyEvent) -> bool {
    key.code == crossterm::event::KeyCode::Char('z')
        && key.modifiers == crossterm::event::KeyModifiers::CONTROL
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
        let mut pending = None;
        loop {
            // ponytail: focus/paste/key-release and right/middle-button
            // events have no behavior; drop them instead of redrawing.
            let forwarded = match pending.take() {
                Some(event) => Some(event),
                None => match crossterm::event::read() {
                    Ok(event) => map_terminal_event(event),
                    Err(_) => break,
                },
            };
            let Some(forwarded) = forwarded else {
                continue;
            };
            let (forwarded, next) = coalesce_terminal_event(forwarded, || {
                if !crossterm::event::poll(std::time::Duration::ZERO).ok()? {
                    return None;
                }
                map_terminal_event(crossterm::event::read().ok()?)
            });
            pending = next;
            if !forward_event(&sender, forwarded) {
                break;
            }
        }
    });
    receiver
}

/// Coalesces only ready terminal events whose intermediate states are not
/// observable. The first different event is retained so input order is exact.
pub(super) fn coalesce_terminal_event(
    event: TuiEvent,
    mut next: impl FnMut() -> Option<TuiEvent>,
) -> (TuiEvent, Option<TuiEvent>) {
    match event {
        TuiEvent::Wheel { up, mut steps } => {
            while steps < 64 {
                match next() {
                    Some(TuiEvent::Wheel {
                        up: direction,
                        steps: count,
                    }) if direction == up && count <= 64 - steps => steps += count,
                    pending => return (TuiEvent::Wheel { up, steps }, pending),
                }
            }
            (TuiEvent::Wheel { up, steps }, None)
        }
        TuiEvent::Redraw => {
            let mut count = 1;
            while count < 64 {
                match next() {
                    Some(TuiEvent::Redraw) => count += 1,
                    pending => return (TuiEvent::Redraw, pending),
                }
            }
            (TuiEvent::Redraw, None)
        }
        TuiEvent::Select {
            phase: DragPhase::Extend,
            mut col,
            mut row,
        } => {
            let mut count = 1;
            while count < 64 {
                match next() {
                    Some(TuiEvent::Select {
                        phase: DragPhase::Extend,
                        col: next_col,
                        row: next_row,
                    }) => {
                        col = next_col;
                        row = next_row;
                        count += 1;
                    }
                    pending => {
                        return (
                            TuiEvent::Select {
                                phase: DragPhase::Extend,
                                col,
                                row,
                            },
                            pending,
                        );
                    }
                }
            }
            (
                TuiEvent::Select {
                    phase: DragPhase::Extend,
                    col,
                    row,
                },
                None,
            )
        }
        event => (event, None),
    }
}

pub(super) fn forward_event(sender: &mpsc::Sender<TuiEvent>, event: TuiEvent) -> bool {
    #[cfg(unix)]
    if matches!(&event, TuiEvent::Key(key) if is_suspend_key(*key)) {
        let (resume, parked) = std::sync::mpsc::channel();
        if sender.blocking_send(TuiEvent::Suspend(resume)).is_err() {
            return false;
        }
        // No further terminal reads until the UI finishes the handoff. Drop
        // on errors or session exit also releases the waiting reader.
        let _ = parked.recv();
        return true;
    }
    sender.blocking_send(event).is_ok()
}

#[cfg(all(test, unix))]
#[path = "terminal_input_tests.rs"]
mod tests;
