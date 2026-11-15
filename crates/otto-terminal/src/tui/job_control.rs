//! Unix job control at the shared terminal ownership boundary.

use super::*;

#[cfg(unix)]
pub(super) fn suspend<B: Backend>(terminal: &mut Terminal<B>) -> Result<(), ReplError> {
    use crossterm::{cursor, execute, terminal as modes};

    if !owns_terminal().map_err(io_error)? {
        return Err(io_error(io::Error::other(
            "Otto does not own the foreground terminal",
        )));
    }
    // The input reader is parked on the acknowledgement carried by Suspend.
    // No renderer or reader can race the shell while this thread is stopped.
    let mut input = TerminalInput {
        mouse_active: true,
        paste_active: true,
    };
    let restored_input = input.restore_terminal();
    let restored_terminal = ratatui::try_restore();
    restored_input.and(restored_terminal).map_err(io_error)?;

    stop_job().map_err(io_error)?;
    // `bg` is not a terminal handoff. Stay stopped until the shell gives this
    // process group the controlling terminal (normally via `fg`).
    while !owns_terminal().map_err(io_error)? {
        stop_job().map_err(io_error)?;
    }

    // Do not create a second Terminal: its diff buffers and the App belong to
    // this session. Clear them after re-entering, so even unchanged cells paint.
    modes::enable_raw_mode().map_err(io_error)?;
    execute!(io::stdout(), modes::EnterAlternateScreen, cursor::Hide).map_err(io_error)?;
    input.enable_terminal().map_err(io_error)?;
    terminal.clear().map_err(draw_error)?;
    // The outer session's TerminalInput remains responsible for final cleanup.
    input.mouse_active = false;
    input.paste_active = false;
    Ok(())
}

#[cfg(unix)]
#[allow(unsafe_code)]
fn stop_job() -> io::Result<()> {
    // SAFETY: kill with pid 0 addresses our own process group and takes no
    // pointers. SIGSTOP also works with inherited ignored SIGTSTP and orphaned
    // groups; unlike a handler it cannot run terminal I/O in signal context.
    if unsafe { libc::kill(0, libc::SIGSTOP) } == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(unix)]
#[allow(unsafe_code)]
fn owns_terminal() -> io::Result<bool> {
    use std::os::fd::AsRawFd;
    let controlling_terminal = std::fs::File::open("/dev/tty")?;
    // SAFETY: the file owns a valid controlling-terminal descriptor.
    let foreground = unsafe { libc::tcgetpgrp(controlling_terminal.as_raw_fd()) };
    if foreground == -1 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: getpgrp has no arguments or memory preconditions.
    Ok(foreground == unsafe { libc::getpgrp() })
}
