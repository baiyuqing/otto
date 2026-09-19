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
//!
//! Sub-agent wake turns: the idle loop also selects on the task registry's
//! update signal and runs an empty-text turn whenever a notification is
//! pending, matching [`crate::cli::repl::Repl`]. There is no second
//! scheduler.

mod app;
mod commands;
mod entries;
mod layout;
mod markdown;
mod render;

use std::future::Future;
use std::sync::Arc;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use crossterm::event::{
    DisableMouseCapture, EnableMouseCapture, Event as TermEvent, KeyEvent, KeyEventKind,
    MouseEventKind,
};
use otto_core::agent::Event;
use otto_core::model::{Block, MAX_IMAGE_BYTES};
use ratatui::Terminal;
use ratatui::backend::Backend;
use tokio::sync::{mpsc, watch};
use tokio_util::sync::CancellationToken;

use crate::app::Controller;
use crate::cli::login;
use crate::cli::repl::{Error as ReplError, is_fatal_persistence};
use crate::cli::repl_commands;
use crate::subagent::tasks::Tasks;

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
    let mut keys = spawn_key_reader();
    let result = run_app(&mut terminal, controller, cancel, &mut keys).await;
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

/// One event [`spawn_key_reader`] forwards to the main loop: a keypress, one
/// mouse-wheel notch, or a resize that only needs a redraw.
///
/// The wheel is its own variant rather than a synthesized `Up`/`Down` key,
/// because the composer's own Up/Down recall prompt history
/// ([`app::History`]); a wheel notch always scrolls the transcript.
enum TuiEvent {
    Key(KeyEvent),
    Wheel { up: bool },
    Redraw,
}

fn map_terminal_event(event: TermEvent) -> Option<TuiEvent> {
    match event {
        TermEvent::Key(key) if key.kind == KeyEventKind::Press => Some(TuiEvent::Key(key)),
        TermEvent::Mouse(mouse) => Some(TuiEvent::Wheel {
            up: match mouse.kind {
                MouseEventKind::ScrollUp => true,
                MouseEventKind::ScrollDown => false,
                _ => return None,
            },
        }),
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

fn draw_error<E: std::fmt::Display>(error: E) -> ReplError {
    io_error(std::io::Error::other(error.to_string()))
}

/// What the idle loop woke up for. Port of the REPL's `select` over stdin
/// versus the task-registry watch: keys stay one branch, and a registry
/// signal is the other, so a pending notification can start a turn without
/// a keypress.
enum IdleEvent {
    Input(Option<TuiEvent>),
    Registry(bool),
}

async fn run_app<B: Backend>(
    terminal: &mut Terminal<B>,
    controller: &Controller,
    cancel: &CancellationToken,
    keys: &mut mpsc::Receiver<TuiEvent>,
) -> Result<(), ReplError> {
    let mut app = App::new(controller);
    let mut pending_image = None;
    terminal
        .draw(|frame| render::draw(frame, &app))
        .map_err(draw_error)?;

    let mut updates: Option<(Arc<Tasks>, watch::Receiver<u64>)> = None;
    loop {
        match controller.subagent_tasks() {
            Some(tasks) => {
                if updates
                    .as_ref()
                    .is_none_or(|(held, _)| !Arc::ptr_eq(held, &tasks))
                {
                    let receiver = tasks.updates();
                    updates = Some((tasks, receiver));
                }
            }
            None => updates = None,
        }
        let event = {
            let signal = async {
                match updates.as_mut() {
                    Some((_, receiver)) => receiver.changed().await.is_ok(),
                    None => std::future::pending().await,
                }
            };
            tokio::select! {
                _ = cancel.cancelled() => return Err(ReplError::Cancelled),
                event = keys.recv() => IdleEvent::Input(event),
                open = signal => IdleEvent::Registry(open),
            }
        };
        let event = match event {
            IdleEvent::Input(event) => event,
            IdleEvent::Registry(false) => {
                updates = None;
                continue;
            }
            IdleEvent::Registry(true) => {
                if let Err(error) = run_wake(&mut app, terminal, keys, controller, cancel).await {
                    propagate_turn_error(error)?;
                }
                app.refresh_info(controller);
                terminal
                    .draw(|frame| render::draw(frame, &app))
                    .map_err(draw_error)?;
                continue;
            }
        };
        let Some(event) = event else { return Ok(()) };
        let key = match event {
            TuiEvent::Key(key) => key,
            TuiEvent::Wheel { up } => {
                app.scroll_wheel(up);
                terminal
                    .draw(|frame| render::draw(frame, &app))
                    .map_err(draw_error)?;
                continue;
            }
            TuiEvent::Redraw => {
                terminal
                    .draw(|frame| render::draw(frame, &app))
                    .map_err(draw_error)?;
                continue;
            }
        };

        match app.handle_key(key, controller, cancel) {
            None => {}
            Some(Action::Exit) => return Ok(()),
            Some(Action::Prompt(line)) => {
                if let Err(error) = run_turn(
                    &mut app,
                    terminal,
                    keys,
                    controller,
                    cancel,
                    line,
                    pending_image.take(),
                )
                .await
                {
                    propagate_turn_error(error)?;
                }
            }
            Some(Action::Image(path)) => match image_block_from_path(&path) {
                Ok(image) => {
                    pending_image = Some(image);
                    app.push_system(format!("Attached image: {path}"));
                }
                Err(message) => app.push_system(format!("/image: {message}")),
            },
            Some(Action::Compact(focus)) => {
                if let Err(error) =
                    run_compact(&mut app, terminal, keys, controller, cancel, focus).await
                {
                    propagate_turn_error(error)?;
                }
            }
            Some(Action::NewSession) => {
                pending_image = None;
                match controller.new_session().await {
                    Ok(()) => {
                        app.refresh(controller);
                        push_session_id(&mut app, controller);
                    }
                    Err(message) => app.push_system(format!("/new: {message}")),
                }
            }
            Some(Action::SwitchProfile(profile)) => {
                pending_image = None;
                switch_profile(&mut app, controller, &profile).await;
            }
            Some(Action::Resume(path)) => {
                pending_image = None;
                match controller.resume_session(&path).await {
                    Ok(result) => {
                        app.refresh(controller);
                        app.push_system(format!("Resumed: {}", result.session_path));
                        for warning in &result.warnings {
                            app.push_system(warning.clone());
                        }
                        push_session_id(&mut app, controller);
                    }
                    Err(message) => app.push_system(format!("/resume: {message}")),
                }
            }
            Some(Action::Archive(path)) => {
                pending_image = None;
                match controller.archive_session(&path).await {
                    Ok(result) => {
                        app.refresh(controller);
                        app.push_system(format!("Archived: {}", result.path));
                        push_session_id(&mut app, controller);
                    }
                    Err(message) => app.push_system(format!("/archive: {message}")),
                }
            }
            Some(Action::SandboxReload) => match controller.reload_sandbox().await {
                Ok(info) => app.push_system(format!("Sandbox: {}", info.summary())),
                Err(message) => app.push_system(format!("/sandbox reload: {message}")),
            },
            Some(Action::Approve(id)) => match controller.approve_bash(&id) {
                Ok(prompt) => {
                    app.push_system(format!("Approved {id} for one command."));
                    if let Err(error) =
                        run_turn(&mut app, terminal, keys, controller, cancel, prompt, None).await
                    {
                        propagate_turn_error(error)?;
                    }
                }
                Err(message) => app.push_system(format!("/approve: {message}")),
            },
            Some(Action::Login(args)) => {
                login_dispatch(&mut app, controller, &args, cancel).await;
            }
            Some(Action::McpLogin(name)) => {
                mcp_login_dispatch(&mut app, controller, &name, cancel).await;
            }
        }

        app.refresh_info(controller);
        terminal
            .draw(|frame| render::draw(frame, &app))
            .map_err(draw_error)?;
    }
}

/// Port of `internal/repl::Repl::run`'s handling of its own `prompt()`'s
/// result: a non-fatal turn failure (already shown in the transcript by
/// [`run_turn`]/[`run_compact`]/[`run_wake`]) is swallowed so the session continues;
/// everything else (a fatal persistence failure, or the outer `cancel`
/// itself firing) ends [`run`].
fn propagate_turn_error(error: ReplError) -> Result<(), ReplError> {
    match error {
        ReplError::Turn { fatal: false, .. } => Ok(()),
        other => Err(other),
    }
}

/// Drives one `Controller` call to completion, owning the screen for its
/// whole duration. Needed for [`Action::Prompt`]/[`Action::Compact`] and for
/// a wake turn: every other `Action` is a one-shot `.await` with no Go
/// precedent for interrupting it, and raw mode leaves no real SIGINT to
/// interrupt it with anyway.
///
/// The call's sink cannot draw for itself, because it would have to hold
/// `app` and `terminal` borrowed for the whole turn, leaving nothing here to
/// redraw with. So the sink only forwards each [`Event`] down `events`, and
/// this loop applies it, letting the same loop also redraw on a key, a
/// resize, and every [`render::SPINNER_FRAME`] so the thinking indicator
/// animates while nothing is streaming.
async fn drive_turn<B: Backend, T, E>(
    app: &mut App,
    terminal: &mut Terminal<B>,
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
/// Scrolling has to work here and not only between turns: a streaming turn
/// is when there is most output to read back through.
fn apply_turn_key(app: &mut App, event: TuiEvent, turn: &CancellationToken) {
    let key = match event {
        TuiEvent::Key(key) => key,
        TuiEvent::Wheel { up } => {
            app.scroll_wheel(up);
            return;
        }
        TuiEvent::Redraw => return,
    };
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
async fn run_turn<B: Backend>(
    app: &mut App,
    terminal: &mut Terminal<B>,
    keys: &mut mpsc::Receiver<TuiEvent>,
    controller: &Controller,
    cancel: &CancellationToken,
    line: String,
    image: Option<Block>,
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
            async {
                match image {
                    Some(image) => {
                        controller
                            .prompt_with_image(&line, image, &mut sink, &turn)
                            .await
                    }
                    None => controller.prompt(&line, &mut sink, &turn).await,
                }
            },
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

fn image_block_from_path(path: &str) -> Result<Block, String> {
    let metadata = std::fs::metadata(path).map_err(|error| error.to_string())?;
    if metadata.len() > MAX_IMAGE_BYTES as u64 {
        return Err(format!("image exceeds {MAX_IMAGE_BYTES} bytes"));
    }
    let bytes = std::fs::read(path).map_err(|error| error.to_string())?;
    let mime_type = if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        "image/png"
    } else if bytes.starts_with(b"\xff\xd8\xff") {
        "image/jpeg"
    } else if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" {
        "image/webp"
    } else {
        return Err("unsupported image; use PNG, JPEG, or WebP".into());
    };
    let image = Block::image(BASE64.encode(bytes), mime_type);
    image.validate().map_err(|error| error.to_string())?;
    Ok(image)
}

/// One empty-text turn delivering pending sub-agent notifications.
/// Port of `internal/repl`'s `wake`: the TUI has no `"> "` marker to skip,
/// so the only extra work is bracketing the claim with [`App::start_turn`]
/// so Esc still cancels and the thinking line still animates.
async fn run_wake<B: Backend>(
    app: &mut App,
    terminal: &mut Terminal<B>,
    keys: &mut mpsc::Receiver<TuiEvent>,
    controller: &Controller,
    cancel: &CancellationToken,
) -> Result<(), ReplError> {
    let wake = match controller.prepare_wake() {
        Ok(Some(wake)) => wake,
        Ok(None) => return Ok(()),
        // A prompt or close raced the idle loop; both are the server wake
        // loop's ignored `errTurnActive` / `ErrClosed`.
        Err(message) if message == crate::app::PROMPT_ACTIVE || message == crate::app::CLOSED => {
            return Ok(());
        }
        Err(message) => {
            return Err(ReplError::Turn {
                fatal: false,
                message,
            });
        }
    };
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
            wake.run(&mut sink, &turn),
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
async fn run_compact<B: Backend>(
    app: &mut App,
    terminal: &mut Terminal<B>,
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

/// Port of `/mcp login <server>` dispatch: reuses [`repl_commands::repl_mcp_command`]
/// against captured buffers, matching [`login_dispatch`] above.
async fn mcp_login_dispatch(
    app: &mut App,
    controller: &Controller,
    name: &str,
    cancel: &CancellationToken,
) {
    let mut out = Vec::new();
    let mut err = Vec::new();
    let result = repl_commands::repl_mcp_command(
        controller,
        &format!("login {name}"),
        &mut out,
        &mut err,
        cancel,
    )
    .await;
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
        Err(error) => app.push_system(format!("/mcp login: {error}")),
    }
}

#[cfg(test)]
mod tests {
    use crossterm::event::{KeyCode, KeyModifiers, MouseEvent, MouseEventKind};

    use super::*;

    #[test]
    fn image_path_becomes_a_valid_image_block() {
        use std::io::Write as _;

        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("shot.png");
        std::fs::File::create(&path)
            .expect("create")
            .write_all(b"\x89PNG\r\n\x1a\n")
            .expect("write");

        let image = image_block_from_path(path.to_str().expect("utf8 path")).expect("image");
        assert_eq!(image.mime_type, "image/png");
        assert_eq!(image.data, "iVBORw0KGgo=");
    }

    /// The wheel must not reach the composer's keys: Up/Down there recall
    /// prompt history, so a wheel notch mapped onto them would scroll the
    /// history instead of the transcript.
    #[test]
    fn mouse_wheel_scrolls_without_becoming_a_composer_key() {
        let wheel = |kind| {
            let event = TermEvent::Mouse(MouseEvent {
                kind,
                column: 0,
                row: 0,
                modifiers: KeyModifiers::NONE,
            });
            match map_terminal_event(event) {
                Some(TuiEvent::Wheel { up }) => Some(up),
                _ => None,
            }
        };

        assert_eq!(wheel(MouseEventKind::ScrollUp), Some(true));
        assert_eq!(wheel(MouseEventKind::ScrollDown), Some(false));
        assert!(wheel(MouseEventKind::Moved).is_none());
    }

    /// The reported bad experience: while a turn streamed, every key but the
    /// interrupt keys was dropped, so the wheel did nothing at exactly the
    /// moment there was output to scroll back through.
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

    /// Port of `TestREPLWakesOnlyWhenNotificationIsPending`, against the TUI
    /// idle loop rather than stdin. A registry signal with nothing pending
    /// must not start a turn; a later pending notification must.
    #[tokio::test]
    async fn the_idle_loop_wakes_only_when_a_notification_is_pending() {
        use std::sync::{Arc, Mutex};
        use std::time::Duration;

        use otto_core::agent::inbox::Notification;
        use otto_core::model::{Block, BlockType, FinishReason, Message, Role};
        use otto_core::provider::{
            Provider, ProviderError, Request as ProviderRequest, Response as ProviderResponse,
            StreamEvent, StreamSink,
        };
        use ratatui::backend::TestBackend;

        use crate::cli::runtime_builder::Runner;
        use crate::subagent::tasks::Tasks;

        struct ScriptedProvider {
            reply: Box<dyn Fn(usize) -> Result<String, String> + Send + Sync>,
            roles: Mutex<Vec<Role>>,
            calls: tokio::sync::watch::Sender<usize>,
        }

        impl ScriptedProvider {
            fn new(
                reply: impl Fn(usize) -> Result<String, String> + Send + Sync + 'static,
            ) -> Arc<Self> {
                Arc::new(Self {
                    reply: Box::new(reply),
                    roles: Mutex::new(Vec::new()),
                    calls: tokio::sync::watch::channel(0).0,
                })
            }

            fn roles(&self) -> Vec<Role> {
                self.roles.lock().expect("roles").clone()
            }

            fn calls(&self) -> usize {
                *self.calls.borrow()
            }

            async fn wait_calls(&self, count: usize) {
                let mut receiver = self.calls.subscribe();
                receiver
                    .wait_for(|seen| *seen >= count)
                    .await
                    .expect("sender");
            }
        }

        #[async_trait::async_trait]
        impl Provider for ScriptedProvider {
            async fn complete(
                &self,
                request: &ProviderRequest,
                emit: StreamSink<'_>,
                _cancel: &CancellationToken,
            ) -> Result<ProviderResponse, ProviderError> {
                let call = {
                    let mut roles = self.roles.lock().expect("roles");
                    roles.push(
                        request
                            .messages
                            .last()
                            .map(|message| message.role.clone())
                            .unwrap_or(Role::User),
                    );
                    roles.len()
                };
                self.calls.send_modify(|seen| *seen = call);
                let text = (self.reply)(call).map_err(ProviderError::Other)?;
                emit(StreamEvent::TextDelta { text: text.clone() });
                Ok(ProviderResponse {
                    message: Message {
                        role: Role::Assistant,
                        finish_reason: Some(FinishReason::Stop),
                        blocks: vec![Block {
                            block_type: BlockType::Text,
                            text,
                            ..Block::default()
                        }],
                        ..Message::default()
                    },
                })
            }
        }

        let workspace = tempfile::tempdir().expect("workspace");
        let sessions = tempfile::tempdir().expect("sessions");
        let tasks = Arc::new(Tasks::new());
        let provider = ScriptedProvider::new(|_| Ok("woke up".to_string()));
        let builder = crate::cli::testutil::builder(workspace.path(), sessions.path());
        let runtime = crate::cli::testutil::initial_runtime(&builder);
        let session = builder.create_session(&runtime).expect("session");
        let info = builder.runtime_info(&runtime);
        let runner = Runner::scripted(
            session.clone(),
            Arc::clone(&provider) as Arc<dyn Provider + Send + Sync>,
            Arc::clone(&tasks),
        );
        let controller = Controller::new(builder, true, session, runner, info);

        let mut terminal = Terminal::new(TestBackend::new(80, 24)).expect("terminal");
        let (_keys_tx, mut keys) = mpsc::channel(1);
        let cancel = CancellationToken::new();

        let driver = async {
            tokio::time::sleep(Duration::from_millis(20)).await;
            tasks
                .add(crate::subagent::tasks::Task::default(), None, None)
                .expect("add");
            tokio::time::sleep(Duration::from_millis(50)).await;
            assert_eq!(
                provider.calls(),
                0,
                "a wake turn ran before any notification was pending"
            );
            tasks.notifications().push(Notification {
                task_id: "t1".to_string(),
                text: "[task-notification] task t1 succeeded".to_string(),
                ..Notification::default()
            });
            tokio::time::timeout(Duration::from_secs(2), provider.wait_calls(1))
                .await
                .expect("the pending notification did not trigger a wake turn");
            cancel.cancel();
        };
        let (result, ()) = tokio::join!(
            run_app(&mut terminal, &controller, &cancel, &mut keys),
            driver
        );

        match result {
            Err(ReplError::Cancelled) => {}
            other => panic!("idle loop should end on cancel, got {other:?}"),
        }
        assert_eq!(
            provider.roles(),
            vec![Role::Context],
            "want exactly one wake turn, whose last request message is the notification"
        );
    }
}
