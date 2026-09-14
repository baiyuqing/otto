//! The terminal frontend (ratatui + crossterm). Port of `internal/tui`.
//!
//! This module is built up incrementally; each submodule is a direct port
//! of the matching Go file(s) and says so in its own doc comment.
//!
//! [`run`] is the entry point `cli::run` dispatches to for `--ui tui` (or
//! `--ui auto` on a terminal). It owns the alternate screen / raw mode
//! lifecycle, reads keys on a background task the way `cli::repl`'s
//! `spawn_reader` reads lines, and drives [`app::App`] against the same
//! [`Controller`] the REPL uses.

mod app;
mod commands;
mod entries;
mod layout;
mod markdown;
mod render;

use std::future::Future;

use crossterm::event::{
    DisableMouseCapture, EnableMouseCapture, Event as TermEvent, KeyCode, KeyEvent, KeyEventKind,
    MouseEventKind,
};
use otto_core::agent::Event;
use ratatui::DefaultTerminal;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::app::Controller;
use crate::cli::login;
use crate::cli::repl::{Error as ReplError, is_fatal_persistence};

use app::{Action, App};

/// Runs the terminal frontend to completion. Port of `internal/tui.Run`.
///
/// Enters the alternate screen, raw mode, and mouse capture, all restored on
/// return and panic.
pub(crate) async fn run(
    controller: &Controller,
    cancel: &CancellationToken,
) -> Result<(), ReplError> {
    let mut terminal = ratatui::try_init().map_err(io_error)?;
    let mouse_capture = match MouseCapture::enable() {
        Ok(capture) => capture,
        Err(error) => {
            let _ = ratatui::try_restore();
            return Err(io_error(error));
        }
    };
    let result = run_app(&mut terminal, controller, cancel).await;
    drop(mouse_capture);
    let _ = ratatui::try_restore();
    result
}

struct MouseCapture;

impl MouseCapture {
    fn enable() -> std::io::Result<Self> {
        crossterm::execute!(std::io::stdout(), EnableMouseCapture)?;
        Ok(Self)
    }
}

impl Drop for MouseCapture {
    fn drop(&mut self) {
        let _ = crossterm::execute!(std::io::stdout(), DisableMouseCapture);
    }
}

fn io_error(error: std::io::Error) -> ReplError {
    ReplError::Input(error.to_string())
}

/// One event [`spawn_key_reader`] forwards to the main loop: either a keypress
/// (including a mouse wheel event mapped to the existing scroll keys), or a
/// resize that only needs a redraw.
enum TuiEvent {
    Key(KeyEvent),
    Redraw,
}

fn map_terminal_event(event: TermEvent) -> Option<TuiEvent> {
    match event {
        TermEvent::Key(key) if key.kind == KeyEventKind::Press => Some(TuiEvent::Key(key)),
        TermEvent::Mouse(mouse) => Some(TuiEvent::Key(
            match mouse.kind {
                MouseEventKind::ScrollUp => KeyCode::Up,
                MouseEventKind::ScrollDown => KeyCode::Down,
                _ => return None,
            }
            .into(),
        )),
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
fn spawn_key_reader() -> mpsc::Receiver<TuiEvent> {
    let (sender, receiver) = mpsc::channel(1);
    std::thread::spawn(move || {
        while let Ok(event) = crossterm::event::read() {
            // ponytail: focus/paste/key-release and non-wheel mouse events
            // have no behavior; drop them instead of redrawing.
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

async fn run_app(
    terminal: &mut DefaultTerminal,
    controller: &Controller,
    cancel: &CancellationToken,
) -> Result<(), ReplError> {
    let mut app = App::new(controller);
    let mut keys = spawn_key_reader();
    terminal
        .draw(|frame| render::draw(frame, &app))
        .map_err(io_error)?;

    loop {
        let event = tokio::select! {
            _ = cancel.cancelled() => return Err(ReplError::Cancelled),
            event = keys.recv() => event,
        };
        let Some(event) = event else { return Ok(()) };
        let key = match event {
            TuiEvent::Key(key) => key,
            TuiEvent::Redraw => {
                terminal
                    .draw(|frame| render::draw(frame, &app))
                    .map_err(io_error)?;
                continue;
            }
        };

        match app.handle_key(key, controller, cancel) {
            None => {}
            Some(Action::Exit) => return Ok(()),
            Some(Action::Prompt(line)) => {
                if let Err(error) =
                    run_turn(&mut app, terminal, &mut keys, controller, cancel, line).await
                {
                    propagate_turn_error(error)?;
                }
            }
            Some(Action::Compact(focus)) => {
                if let Err(error) =
                    run_compact(&mut app, terminal, &mut keys, controller, cancel, focus).await
                {
                    propagate_turn_error(error)?;
                }
            }
            Some(Action::NewSession) => match controller.new_session().await {
                Ok(()) => {
                    app.refresh(controller);
                    push_session_id(&mut app, controller);
                }
                Err(message) => app.push_system(format!("/new: {message}")),
            },
            Some(Action::SwitchProfile(profile)) => {
                switch_profile(&mut app, controller, &profile).await;
            }
            Some(Action::Resume(path)) => match controller.resume_session(&path).await {
                Ok(result) => {
                    app.refresh(controller);
                    app.push_system(format!("Resumed: {}", result.session_path));
                    for warning in &result.warnings {
                        app.push_system(warning.clone());
                    }
                    push_session_id(&mut app, controller);
                }
                Err(message) => app.push_system(format!("/resume: {message}")),
            },
            Some(Action::Archive(path)) => match controller.archive_session(&path).await {
                Ok(result) => {
                    app.refresh(controller);
                    app.push_system(format!("Archived: {}", result.path));
                    push_session_id(&mut app, controller);
                }
                Err(message) => app.push_system(format!("/archive: {message}")),
            },
            Some(Action::SandboxReload) => match controller.reload_sandbox().await {
                Ok(info) => app.push_system(format!("Sandbox: {}", info.summary())),
                Err(message) => app.push_system(format!("/sandbox reload: {message}")),
            },
            Some(Action::Login(args)) => {
                login_dispatch(&mut app, controller, &args, cancel).await;
            }
        }

        app.refresh_info(controller);
        terminal
            .draw(|frame| render::draw(frame, &app))
            .map_err(io_error)?;
    }
}

/// Port of `internal/repl::Repl::run`'s handling of its own `prompt()`'s
/// result: a non-fatal turn failure (already shown in the transcript by
/// [`run_turn`]/[`run_compact`]) is swallowed so the session continues;
/// everything else (a fatal persistence failure, or the outer `cancel`
/// itself firing) ends [`run`].
fn propagate_turn_error(error: ReplError) -> Result<(), ReplError> {
    match error {
        ReplError::Turn { fatal: false, .. } => Ok(()),
        other => Err(other),
    }
}

/// Drives one `Controller` call to completion, owning the screen for its
/// whole duration. Needed only for [`Action::Prompt`]/[`Action::Compact`]:
/// every other `Action` is a one-shot `.await` with no Go precedent for
/// interrupting it, and raw mode leaves no real SIGINT to interrupt it with
/// anyway.
///
/// The call's sink cannot draw for itself, because it would have to hold
/// `app` and `terminal` borrowed for the whole turn, leaving nothing here to
/// redraw with. So the sink only forwards each [`Event`] down `events`, and
/// this loop applies it, letting the same loop also redraw on a key, a
/// resize, and every [`render::SPINNER_FRAME`] so the thinking indicator
/// animates while nothing is streaming.
async fn drive_turn<T, E>(
    app: &mut App,
    terminal: &mut DefaultTerminal,
    keys: &mut mpsc::Receiver<TuiEvent>,
    events: &mut mpsc::UnboundedReceiver<Event>,
    turn: &CancellationToken,
    future: impl Future<Output = Result<T, E>>,
    mut apply: impl FnMut(&mut App, Event),
) -> Result<T, E> {
    tokio::pin!(future);
    let mut frames = tokio::time::interval(render::SPINNER_FRAME);
    loop {
        tokio::select! {
            result = &mut future => {
                // `select!` picks at random between ready branches, so the
                // sink's last events can still be queued behind the call's
                // own completion.
                while let Ok(event) = events.try_recv() {
                    apply(app, event);
                }
                return result;
            }
            Some(event) = events.recv() => apply(app, event),
            Some(event) = keys.recv() => apply_turn_key(app, event, turn),
            _ = frames.tick() => {}
        }
        let _ = terminal.draw(|frame| render::draw(frame, app));
    }
}

/// Handles one key delivered while a turn is running: the interrupt keys
/// cancel it, the scroll keys move the transcript, and everything else is
/// dropped the way [`App::handle_key`]'s `busy()` branch drops it.
///
/// Scrolling has to work here and not only between turns: the wheel arrives
/// as [`KeyCode::Up`]/[`KeyCode::Down`] (see [`map_terminal_event`]), and a
/// streaming turn is when there is most output to read back through.
fn apply_turn_key(app: &mut App, event: TuiEvent, turn: &CancellationToken) {
    let TuiEvent::Key(key) = event else { return };
    if App::is_interrupt_key(&key) {
        turn.cancel();
    } else {
        app.handle_scroll_key(&key);
    }
}

/// Runs one prompt turn. Structurally a port of `internal/repl`'s own
/// `prompt()`: build a sink over the live view, await the call, and turn a
/// non-cancelled `Err` into [`Error::Turn`] using the same
/// [`is_fatal_persistence`] check.
async fn run_turn(
    app: &mut App,
    terminal: &mut DefaultTerminal,
    keys: &mut mpsc::Receiver<TuiEvent>,
    controller: &Controller,
    cancel: &CancellationToken,
    line: String,
) -> Result<(), ReplError> {
    app.start_turn();
    let turn = cancel.child_token();
    let mut error_rendered = false;
    let result = {
        let (events, mut received) = mpsc::unbounded_channel();
        let mut sink = |event: Event| {
            let _ = events.send(event);
        };
        drive_turn(
            app,
            terminal,
            keys,
            &mut received,
            &turn,
            controller.prompt(&line, &mut sink, &turn),
            |app, event| {
                if app.apply_event(event) {
                    error_rendered = true;
                }
            },
        )
        .await
    };
    app.end_turn();
    if !error_rendered && let Err(error) = &result {
        app.push_system(error.to_string());
    }
    if cancel.is_cancelled() {
        return Err(ReplError::Cancelled);
    }
    match result {
        Ok(()) => Ok(()),
        Err(error) => Err(ReplError::Turn {
            fatal: is_fatal_persistence(&error),
            message: error.to_string(),
        }),
    }
}

/// Runs one `/compact`. Structurally a port of `internal/repl`'s own
/// `compact()`, including its checkpoint/no-op de-duplication between a
/// streamed [`Event::CompactionCompleted`] and the call's final
/// [`otto_core::agent::CompactionResult`] (both can describe the same
/// compaction).
async fn run_compact(
    app: &mut App,
    terminal: &mut DefaultTerminal,
    keys: &mut mpsc::Receiver<TuiEvent>,
    controller: &Controller,
    cancel: &CancellationToken,
    focus: String,
) -> Result<(), ReplError> {
    app.start_turn();
    let turn = cancel.child_token();
    let mut rendered_ids: Vec<String> = Vec::new();
    let mut rendered_noop_empty = false;
    let mut error_rendered = false;
    let result = {
        let (events, mut received) = mpsc::unbounded_channel();
        let mut sink = |event: Event| {
            let _ = events.send(event);
        };
        drive_turn(
            app,
            terminal,
            keys,
            &mut received,
            &turn,
            controller.compact(&focus, &mut sink, &turn),
            |app, event| {
                if let Event::CompactionCompleted { compaction } = &event {
                    let already = if !compaction.checkpoint_id.is_empty() {
                        rendered_ids.contains(&compaction.checkpoint_id)
                    } else {
                        compaction.noop && rendered_noop_empty
                    };
                    if already {
                        return;
                    }
                    if compaction.checkpoint_id.is_empty() {
                        rendered_noop_empty = compaction.noop;
                    } else {
                        rendered_ids.push(compaction.checkpoint_id.clone());
                    }
                }
                if app.apply_event(event) {
                    error_rendered = true;
                }
            },
        )
        .await
    };
    app.end_turn();
    if cancel.is_cancelled() {
        return Err(ReplError::Cancelled);
    }
    match result {
        Ok(result) => {
            let already = if !result.checkpoint_id.is_empty() {
                rendered_ids.contains(&result.checkpoint_id)
            } else {
                result.noop && rendered_noop_empty
            };
            if !already {
                app.push_system(app::compaction_line(&result));
            }
            Ok(())
        }
        Err(error) => {
            if error.is_cancelled() && !is_fatal_persistence(&error) {
                return Ok(());
            }
            if !error_rendered {
                app.push_system(error.to_string());
            }
            Err(ReplError::Turn {
                fatal: is_fatal_persistence(&error),
                message: error.to_string(),
            })
        }
    }
}

fn push_session_id(app: &mut App, controller: &Controller) {
    let id = controller.info().session_id;
    if !id.is_empty() {
        app.push_system(format!("Session: {id}"));
    }
}

/// Port of `internal/repl`'s `model()` with-argument branch: switch, try to
/// persist as the default profile, and report either way.
async fn switch_profile(app: &mut App, controller: &Controller, profile: &str) {
    if let Err(message) = controller.switch_profile(profile).await {
        app.push_system(format!("/model: {message}"));
        return;
    }
    app.refresh(controller);
    let saved = controller.set_default_profile(profile);
    let info = controller.info();
    match saved {
        Ok(()) => app.push_system(format!(
            "Switched to profile {} (provider {}, model {}). Set as default profile.",
            info.profile, info.provider, info.model
        )),
        Err(message) => app.push_system(format!(
            "Switched to profile {} (provider {}, model {}), but the default profile was not saved: {message}",
            info.profile, info.provider, info.model
        )),
    }
    push_session_id(app, controller);
}

/// Port of `internal/repl`'s `/login` dispatch: `repl_login` writes its
/// report to `stdout`/`stderr` buffers rather than the transcript directly,
/// so capture both and push whatever they produced as one system entry,
/// matching the `/logout` handling already in `App::dispatch_line`.
async fn login_dispatch(
    app: &mut App,
    controller: &Controller,
    args: &str,
    cancel: &CancellationToken,
) {
    let mut out = Vec::new();
    let mut err = Vec::new();
    let result = login::repl_login(controller, &mut out, &mut err, args, cancel).await;
    let mut text = String::from_utf8_lossy(&out).trim_end().to_string();
    let err_text = String::from_utf8_lossy(&err);
    let err_text = err_text.trim_end();
    if !err_text.is_empty() {
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str(err_text);
    }
    match result {
        Ok(()) => {
            if !text.is_empty() {
                app.push_system(text);
            }
        }
        Err(error) => app.push_system(format!("/login: {error}")),
    }
}

#[cfg(test)]
mod tests {
    use crossterm::event::{KeyCode, KeyModifiers, MouseEvent, MouseEventKind};

    use super::*;

    #[test]
    fn mouse_wheel_uses_the_existing_scroll_keys() {
        let key_code = |kind| {
            let event = TermEvent::Mouse(MouseEvent {
                kind,
                column: 0,
                row: 0,
                modifiers: KeyModifiers::NONE,
            });
            match map_terminal_event(event) {
                Some(TuiEvent::Key(key)) => Some(key.code),
                _ => None,
            }
        };

        assert_eq!(key_code(MouseEventKind::ScrollUp), Some(KeyCode::Up));
        assert_eq!(key_code(MouseEventKind::ScrollDown), Some(KeyCode::Down));
    }

    /// The reported bad experience: while a turn streamed, every key but the
    /// interrupt keys was dropped, so the wheel (mapped to `Up`/`Down` by
    /// [`map_terminal_event`]) did nothing at exactly the moment there was
    /// output to scroll back through.
    #[tokio::test]
    async fn a_turn_scrolls_on_the_wheel_and_still_cancels_on_esc() {
        let workspace = tempfile::tempdir().expect("workspace");
        let sessions = tempfile::tempdir().expect("sessions");
        let controller = crate::cli::testutil::controller(workspace.path(), sessions.path()).await;
        let mut app = App::new(&controller);
        app.max_scroll.set(10);
        app.start_turn();
        let turn = CancellationToken::new();

        let wheel_up = map_terminal_event(TermEvent::Mouse(MouseEvent {
            kind: MouseEventKind::ScrollUp,
            column: 0,
            row: 0,
            modifiers: KeyModifiers::NONE,
        }))
        .expect("wheel event");
        apply_turn_key(&mut app, wheel_up, &turn);

        assert_eq!(app.scroll, Some(9));
        assert!(!turn.is_cancelled());

        apply_turn_key(&mut app, TuiEvent::Key(KeyCode::Esc.into()), &turn);
        assert!(turn.is_cancelled());
    }
}
