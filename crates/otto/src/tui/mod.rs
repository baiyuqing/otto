//! The terminal frontend (ratatui + crossterm).
//!
//! [`run`] is the entry point `cli::run` dispatches to for `--ui tui` (or `--ui
//! auto` on a terminal). It owns the alternate screen / raw mode lifecycle,
//! reads keys on a background task the way `cli::repl`'s `spawn_reader` reads
//! lines, and drives [`app::App`] against the same [`Controller`] the REPL
//! uses.
//!
//! Sub-agent wake turns: the idle loop also selects on the task registry's
//! update signal and runs an empty-text turn whenever a notification is
//! pending, matching [`crate::cli::repl::Repl`]. There is no second scheduler.

mod app;
pub(crate) mod attach;
mod backend;
mod commands;
mod entries;
mod features;
pub(crate) mod gutter;
mod input;
mod interaction;
mod layout;
mod markdown;
mod render;
mod selection;
mod transcript;
mod widgets;

use std::future::Future;
use std::io;
use std::sync::Arc;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use crossterm::event::{
    DisableBracketedPaste, EnableBracketedPaste, Event as TermEvent, KeyEvent, KeyEventKind,
    MouseButton, MouseEventKind,
};
use otto_core::agent::{AgentError, Event};
use otto_core::model::{Block, MAX_IMAGE_BYTES};
use otto_core::wire::events::{WireEvent, to_wire, to_wire_compaction};
use ratatui::Terminal;
use ratatui::backend::Backend;
use tokio::sync::{mpsc, watch};
use tokio_util::sync::CancellationToken;

use crate::app::Controller;
use crate::cli::login;
use crate::cli::repl::{Error as ReplError, is_fatal_persistence};
use crate::cli::repl_commands;

use app::{Action, App};
use selection::{DragPhase, Selection};

/// Runs the terminal frontend to completion.
///
/// Enters the alternate screen, raw mode, and mouse capture, all restored on
/// return and panic.
pub(crate) async fn run(
    controller: &Arc<Controller>,
    cancel: &CancellationToken,
) -> Result<(), ReplError> {
    let mut terminal = ratatui::try_init().map_err(io_error)?;
    let mut input = TerminalInput::new();
    if let Err(error) = input.enable(std::io::stdout()) {
        let _ = ratatui::try_restore();
        return Err(io_error(error));
    }
    let mut keys = spawn_key_reader();
    let result = run_app(&mut terminal, controller, cancel, &mut keys).await;
    let _ = input.restore(std::io::stdout());
    let _ = ratatui::try_restore();
    result
}

#[derive(Default)]
struct TerminalInput {
    mouse_active: bool,
    paste_active: bool,
}

impl TerminalInput {
    fn new() -> Self {
        Self::default()
    }

    fn enable(&mut self, mut writer: impl io::Write) -> io::Result<()> {
        self.enable_mouse(&mut writer)?;
        self.enable_paste(&mut writer)
    }

    fn restore(&mut self, mut writer: impl io::Write) -> io::Result<()> {
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
    fn enable_mouse(&mut self, mut writer: impl io::Write) -> io::Result<()> {
        if self.mouse_active {
            return Ok(());
        }
        writer.write_all(b"\x1b[?1000h\x1b[?1002h\x1b[?1006h")?;
        writer.flush()?;
        self.mouse_active = true;
        Ok(())
    }

    fn restore_mouse(&mut self, mut writer: impl io::Write) -> io::Result<()> {
        if !self.mouse_active {
            return Ok(());
        }
        writer.write_all(b"\x1b[?1006l\x1b[?1002l\x1b[?1000l")?;
        writer.flush()?;
        self.mouse_active = false;
        Ok(())
    }
    fn enable_paste(&mut self, mut writer: impl io::Write) -> io::Result<()> {
        if self.paste_active {
            return Ok(());
        }
        crossterm::execute!(writer, EnableBracketedPaste)?;
        self.paste_active = true;
        Ok(())
    }

    fn restore_paste(&mut self, mut writer: impl io::Write) -> io::Result<()> {
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

fn map_terminal_event(event: TermEvent) -> Option<TuiEvent> {
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
fn spawn_key_reader() -> mpsc::Receiver<TuiEvent> {
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

/// Applies one end of a left-button drag, redraws, and returns the text to
/// put on the clipboard when the drag ended on something.
///
/// The text is read inside the draw closure, from the buffer the frame is
/// being rendered into. It cannot be read afterwards from
/// `Terminal::current_buffer_mut`: `Terminal::draw` finishes by calling
/// `swap_buffers`, which resets the other buffer and then makes it current,
/// so after a draw that accessor returns a blank screen.
///
/// A press without movement is a plain click, which clears the previous
/// selection and copies nothing.
fn apply_selection<B: Backend>(
    app: &mut App,
    terminal: &mut Terminal<B>,
    phase: DragPhase,
    col: u16,
    row: u16,
) -> Result<Option<String>, ReplError> {
    match phase {
        DragPhase::Start => app.selection = Some(Selection::new(col, row)),
        // A drag that started before Otto was listening has no anchor.
        DragPhase::Extend | DragPhase::End => match app.selection.as_mut() {
            Some(selection) => selection.extend(col, row),
            None => return Ok(None),
        },
    }
    // Only a release decides a selection was a plain click: a press always
    // starts out empty, and so does a drag that has not left its anchor yet.
    if phase == DragPhase::End && app.selection.is_some_and(|it| it.is_empty()) {
        app.selection = None;
    }
    let copying = phase == DragPhase::End && app.selection.is_some();

    let mut text = None;
    terminal
        .draw(|frame| {
            render::draw(frame, app);
            if copying && let Some(selection) = &app.selection {
                text = Some(selection.text(frame.buffer_mut()));
            }
        })
        .map_err(draw_error)?;
    Ok(text)
}

fn draw_error<E: std::fmt::Display>(error: E) -> ReplError {
    io_error(std::io::Error::other(error.to_string()))
}

/// What the idle loop woke up for. Keys stay one branch and the task-registry
/// watch is the other, so a pending notification can start a turn without a
/// keypress.
enum IdleEvent {
    Input(Option<TuiEvent>),
    Registry(bool),
    Tick,
    FeaturesTick,
    Completion(Box<backend::Completion>),
    Notices,
    MemoryReviewAvailable,
}

/// Refreshes the sub-agent panel snapshot and redraws. The sole place
/// `run_app` calls `terminal.draw`, so the panel is never stale by more than
/// one frame.
fn redraw<B: Backend>(
    terminal: &mut Terminal<B>,
    app: &mut App,
    controller: &Controller,
) -> Result<(), ReplError> {
    app.info.approval_mode = controller.info().approval_mode;
    app.info.approval_settings = controller.info().approval_settings;
    app.refresh_tasks(&app::Backend::Local(controller));
    terminal
        .draw(|frame| render::draw(frame, app))
        .map(|_| ())
        .map_err(draw_error)
}

async fn run_app<B: Backend>(
    terminal: &mut Terminal<B>,
    controller: &Arc<Controller>,
    cancel: &CancellationToken,
    keys: &mut mpsc::Receiver<TuiEvent>,
) -> Result<(), ReplError> {
    let backend = app::Backend::Local(controller);
    let mut app = App::new(&backend);
    app.enable_program_status();
    app.driver = Some(backend::Driver::new(backend::Connection::Local(
        Arc::clone(controller),
    )));
    let mut updates: Option<(Arc<dyn otto_core::mailbox::Mailbox>, watch::Receiver<u64>)> = None;
    let mut notices = controller.notices_changed();
    let mut memory_review = controller.memory_review_available_changed();
    let mut tick = tokio::time::interval(std::time::Duration::from_millis(500));
    let mut feature_tick = tokio::time::interval(std::time::Duration::from_secs(2));
    feature_tick.reset();
    let mut actions = std::collections::VecDeque::new();
    // The session whose saved turn queue was restored; a `/resume` or
    // `/new` changes the session and restores again.
    let mut restored_session = String::new();
    redraw(terminal, &mut app, controller)?;
    loop {
        for text in controller.take_notices() {
            app.push_system(text);
        }
        let session_id = controller.info().session_id;
        if session_id != restored_session {
            restored_session = session_id;
            if let Err(error) =
                run_restored_prompts(&mut app, terminal, keys, controller, cancel).await
            {
                propagate_turn_error(error)?;
            }
        }
        if controller.take_bell() {
            use std::io::Write as _;
            let mut stdout = io::stdout();
            let _ = stdout.write_all(b"\x07").and_then(|()| stdout.flush());
        }
        match controller.notification_mailbox() {
            Some(tasks)
                if updates
                    .as_ref()
                    .is_none_or(|(held, _)| !Arc::ptr_eq(held, &tasks)) =>
            {
                let receiver = crate::runtime::workflow_notifications::updates(tasks.as_ref());
                updates = Some((Arc::clone(&tasks), receiver));
            }
            None => updates = None,
            _ => {}
        }
        let event = {
            let registry = async {
                match updates.as_mut() {
                    Some((_, receiver)) => receiver.changed().await.is_ok(),
                    None => std::future::pending().await,
                }
            };
            tokio::select! {
                _=cancel.cancelled()=>return Err(ReplError::Cancelled),
                input=keys.recv()=>IdleEvent::Input(input),
                result=app.next_completion()=>IdleEvent::Completion(Box::new(result)),
                open=registry=>IdleEvent::Registry(open),
                _=tick.tick()=>IdleEvent::Tick,
                _=feature_tick.tick()=>IdleEvent::FeaturesTick,
                _=notices.changed()=>IdleEvent::Notices,
                _=memory_review.changed()=>IdleEvent::MemoryReviewAvailable,
            }
        };
        match event {
            IdleEvent::Input(None) => {
                drain_writes(&mut app).await;
                return Ok(());
            }
            IdleEvent::Input(Some(event)) => {
                if let Some(action) = apply_input(&mut app, terminal, event, cancel, &backend)? {
                    actions.push_back(action);
                }
            }
            IdleEvent::Completion(completion) => app.apply_completion(*completion),
            IdleEvent::Tick => app.request_approvals(),
            IdleEvent::FeaturesTick => app.tick_features(),
            IdleEvent::MemoryReviewAvailable => app.memory_review_available(),
            IdleEvent::Notices => {}
            IdleEvent::Registry(false) => updates = None,
            IdleEvent::Registry(true) => {
                if let Err(error) = run_wake(&mut app, terminal, keys, controller, cancel).await {
                    propagate_turn_error(error)?;
                }
            }
        }
        if !app.busy()
            && !app.has_writes()
            && let Some(queued) = app.submit_queued_input(&backend, cancel)
        {
            actions.push_back(queued);
        }
        actions.extend(app.drain_actions());
        while let Some(action) = actions.pop_front() {
            match action {
                Action::Exit => {
                    drain_writes(&mut app).await;
                    return Ok(());
                }
                Action::CancelTurn => {}
                Action::Prompt(line) => {
                    let image = app.pending_image.take();
                    let result =
                        run_turn(&mut app, terminal, keys, controller, cancel, line, image).await;
                    if let Err(error) = result {
                        propagate_turn_error(error)?;
                    }
                    if let Some(action) = app.submit_queued_input(&backend, cancel) {
                        actions.push_back(action);
                    }
                }
                Action::Compact(focus) => {
                    if let Err(error) =
                        run_compact(&mut app, terminal, keys, controller, cancel, focus).await
                    {
                        propagate_turn_error(error)?;
                    }
                }
                Action::Reflect(focus) => {
                    if let Err(error) =
                        run_reflect(&mut app, terminal, keys, controller, cancel, focus).await
                    {
                        propagate_turn_error(error)?;
                    }
                }
                Action::Login(args) => {
                    login_dispatch(&mut app, terminal, keys, controller, &args, cancel).await
                }
                Action::McpLogin(name) => {
                    mcp_login_dispatch(&mut app, terminal, keys, controller, &name, cancel).await
                }
            }
            actions.extend(app.drain_actions());
            if !app.busy()
                && let Some(queued) = app.submit_queued_input(&backend, cancel)
            {
                actions.push_back(queued);
            }
        }
        app.refresh_info(&backend);
        redraw(terminal, &mut app, controller)?;
    }
}

async fn drain_writes(app: &mut App) {
    let completions = match &mut app.driver {
        Some(driver) => driver.drain_pending_writes().await,
        None => Vec::new(),
    };
    for completion in completions {
        app.apply_completion(completion);
    }
}

fn apply_input<B: Backend>(
    app: &mut App,
    terminal: &mut Terminal<B>,
    event: TuiEvent,
    cancel: &CancellationToken,
    backend: &app::Backend<'_>,
) -> Result<Option<Action>, ReplError> {
    match event {
        TuiEvent::Key(key) => {
            app.selection = None;
            Ok(app.handle_key(key, backend, cancel))
        }
        TuiEvent::Paste(text) => {
            app.insert_text(&text);
            Ok(None)
        }
        TuiEvent::Wheel { up } => {
            app.scroll_wheel(up);
            Ok(None)
        }
        TuiEvent::Select { phase, col, row } => {
            if app.has_surface() {
                if phase == DragPhase::Start {
                    focus_surface(app, col, row);
                }
            } else if let Some(text) = apply_selection(app, terminal, phase, col, row)?
                && let Err(error) = selection::copy(&text)
            {
                app.push_system(format!("copy: {error}"));
            }
            Ok(None)
        }
        TuiEvent::Redraw => Ok(None),
    }
}

fn focus_surface(app: &mut App, col: u16, row: u16) {
    let point = ratatui::layout::Position::new(col, row);
    let geometry = app.geometry.borrow();
    let Some(surface) = app.surfaces.top_mut() else {
        return;
    };
    focus_surface_state(&mut surface.state, &geometry, point);
}

fn focus_surface_state(
    state: &mut interaction::State,
    geometry: &widgets::Geometry,
    point: ratatui::layout::Position,
) {
    use interaction::Focus;
    if geometry.rows.is_some_and(|rect| rect.contains(point)) {
        state.focus = Focus::Rows;
    } else if let Some((id, _)) = geometry
        .fields
        .iter()
        .find(|(_, rect)| rect.contains(point))
    {
        state.focus = Focus::Field(id.clone());
    } else if let Some((id, _)) = geometry
        .actions
        .iter()
        .find(|(_, rect)| rect.contains(point))
    {
        state.focus = Focus::Actions;
        state.selected_action = Some(id.clone());
    }
}

/// A non-fatal turn failure (already shown in the transcript by
/// [`run_turn`]/[`run_compact`]/[`run_wake`]) is swallowed so the session
/// continues; everything else (a fatal persistence failure, or the outer
/// `cancel` itself firing) ends [`run`].
fn propagate_turn_error(error: ReplError) -> Result<(), ReplError> {
    match error {
        ReplError::Turn { fatal: false, .. } => Ok(()),
        other => Err(other),
    }
}

/// Drives one `Controller` call to completion, owning the screen for its whole
/// duration. Needed for [`Action::Prompt`]/[`Action::Compact`] and for a wake
/// turn: every other `Action` is a one-shot `.await`, and raw mode leaves no
/// real SIGINT to interrupt it with anyway.
///
/// The call's sink cannot draw for itself, because it would have to hold `app`
/// and `terminal` borrowed for the whole turn, leaving nothing here to redraw
/// with. So the sink only forwards each [`Event`] down `events`, and this loop
/// applies it, letting the same loop also redraw on a key, a resize, and every
/// [`render::SPINNER_FRAME`] so the thinking indicator animates while nothing
/// is streaming. An open `/agents` overlay is also re-queried on
/// the same cadence as idle feature refreshes.
#[allow(clippy::too_many_arguments)]
async fn drive_turn<B: Backend, T, E>(
    app: &mut App,
    terminal: &mut Terminal<B>,
    keys: &mut mpsc::Receiver<TuiEvent>,
    events: &mut mpsc::UnboundedReceiver<WireEvent>,
    turn: &CancellationToken,
    backend: &app::Backend<'_>,
    future: impl Future<Output = Result<T, E>>,
    mut apply: impl FnMut(&mut App, &WireEvent),
) -> Result<T, E> {
    tokio::pin!(future);
    let mut frames = tokio::time::interval(render::SPINNER_FRAME);
    let mut refresh = tokio::time::interval(std::time::Duration::from_secs(2));
    loop {
        tokio::select! {
            result=&mut future=>{
                while let Ok(event)=events.try_recv(){apply(app,&event);}
                return result;
            }
            Some(event)=events.recv()=>apply(app,&event),
            Some(event)=keys.recv()=>apply_turn_key(app,terminal,event,turn,backend),
            completion=app.next_completion()=>app.apply_completion(completion),
            _=refresh.tick()=>{app.tick_features();app.request_approvals();}
            _=frames.tick()=>{}
        }
        app.refresh_tasks(backend);
        let _ = terminal.draw(|frame| render::draw(frame, app));
    }
}

fn apply_turn_key<B: Backend>(
    app: &mut App,
    terminal: &mut Terminal<B>,
    event: TuiEvent,
    turn: &CancellationToken,
    backend: &app::Backend<'_>,
) {
    match apply_input(app, terminal, event, turn, backend) {
        Ok(Some(Action::CancelTurn)) => turn.cancel(),
        Ok(Some(action)) => {
            if matches!(action, Action::Exit) {
                turn.cancel();
            }
            app.outgoing.push(action);
        }
        Ok(None) => {}
        Err(error) => app.push_system(error.to_string()),
    }
}

/// Runs one prompt turn: build a sink over the live view, await the call, and
/// turn a non-cancelled `Err` into [`Error::Turn`] using the same
/// [`is_fatal_persistence`] check the line frontend uses.
/// Runs the prompts saved in the session's turn queue before Otto stopped,
/// in order, after the wake turn that resumes an interrupted turn. Each runs
/// as a normal turn carrying its saved input id, and its entry is removed
/// once it ran; the first delivers the restore notice.
async fn run_restored_prompts<B: Backend>(
    app: &mut App,
    terminal: &mut Terminal<B>,
    keys: &mut mpsc::Receiver<TuiEvent>,
    controller: &Arc<Controller>,
    cancel: &CancellationToken,
) -> Result<(), ReplError> {
    let prompts = controller.restorable_prompts();
    if prompts.is_empty() {
        return Ok(());
    }
    if controller
        .notification_mailbox()
        .is_some_and(|mailbox| !mailbox.is_empty())
    {
        run_wake(app, terminal, keys, controller, cancel).await?;
    }
    controller.announce_restored_prompts(&prompts);
    let journal = controller.turn_queue();
    for prompt in prompts {
        controller.set_next_input_id(Some(prompt.input_id.clone()));
        let result = run_turn(app, terminal, keys, controller, cancel, prompt.text, None).await;
        controller.set_next_input_id(None);
        if let Some(journal) = &journal {
            journal.forget(&prompt.input_id);
        }
        result?;
    }
    Ok(())
}

async fn run_turn<B: Backend>(
    app: &mut App,
    terminal: &mut Terminal<B>,
    keys: &mut mpsc::Receiver<TuiEvent>,
    controller: &Controller,
    cancel: &CancellationToken,
    line: String,
    image: Option<Block>,
) -> Result<(), ReplError> {
    if image.is_none() {
        match controller.approval_message(&line, cancel).await {
            Ok(Some(reply)) => {
                app.push_system(reply.text);
                if reply.queued {
                    app.queued_input = Some(line);
                }
                return Ok(());
            }
            Ok(None) => {}
            Err(message) => {
                app.push_system(message);
                return Ok(());
            }
        }
    }
    app.start_turn();
    let turn = cancel.child_token();
    let mut error_rendered = false;
    let result = {
        let (events, mut received) = mpsc::unbounded_channel();
        let mut sink = |event: Event| {
            let _ = events.send(wire_frame(&event));
        };
        drive_turn(
            app,
            terminal,
            keys,
            &mut received,
            &turn,
            &app::Backend::Local(controller),
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
    let parent_cancelled = cancel.is_cancelled();
    let turn_cancelled = result.as_ref().err().is_some_and(AgentError::is_cancelled);
    app.finish_turn(result.is_ok(), parent_cancelled || turn_cancelled);
    if !error_rendered && let Err(error) = &result {
        app.push_system(error.to_string());
    }
    if parent_cancelled {
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

/// The wire form of one local event, so a local turn and an attached turn
/// reach [`App::apply_event`] as the same frames, including delivered guidance.
fn wire_frame(event: &Event) -> WireEvent {
    to_wire(event)
}

fn image_block_from_path(path: &str) -> Result<Block, String> {
    use std::io::Read as _;
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.custom_flags(libc::O_NONBLOCK);
    }
    let file = options.open(path).map_err(|error| error.to_string())?;
    let metadata = file.metadata().map_err(|error| error.to_string())?;
    if !metadata.is_file() {
        return Err("image must be a regular file".into());
    }
    if metadata.len() > MAX_IMAGE_BYTES as u64 {
        return Err(format!("image exceeds {MAX_IMAGE_BYTES} bytes"));
    }
    let mut bytes = Vec::new();
    file.take(MAX_IMAGE_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| error.to_string())?;
    if bytes.len() > MAX_IMAGE_BYTES {
        return Err(format!("image exceeds {MAX_IMAGE_BYTES} bytes"));
    }
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

/// One empty-text turn delivering pending sub-agent notifications. The TUI has
/// no marker to skip, so the only extra work is bracketing the claim
/// with [`App::start_turn`] so Esc still cancels and the thinking line still
/// animates.
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
            let _ = events.send(wire_frame(&event));
        };
        drive_turn(
            app,
            terminal,
            keys,
            &mut received,
            &turn,
            &app::Backend::Local(controller),
            wake.run(&mut sink, &turn),
            |app, event| {
                if app.apply_event(event) {
                    error_rendered = true;
                }
            },
        )
        .await
    };
    let parent_cancelled = cancel.is_cancelled();
    let turn_cancelled = result.as_ref().err().is_some_and(AgentError::is_cancelled);
    app.finish_turn(result.is_ok(), parent_cancelled || turn_cancelled);
    if !error_rendered && let Err(error) = &result {
        app.push_system(error.to_string());
    }
    if parent_cancelled {
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

/// Runs one `/compact`, including the checkpoint/no-op de-duplication between
/// a streamed [`Event::CompactionCompleted`] and the call's final
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
            let _ = events.send(wire_frame(&event));
        };
        drive_turn(
            app,
            terminal,
            keys,
            &mut received,
            &turn,
            &app::Backend::Local(controller),
            controller.compact(&focus, &mut sink, &turn),
            |app, event| {
                if let Some(compaction) = event
                    .compaction
                    .as_ref()
                    .filter(|_| event.event_type == "compaction_completed")
                {
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
    let parent_cancelled = cancel.is_cancelled();
    let turn_cancelled = result.as_ref().err().is_some_and(AgentError::is_cancelled);
    app.finish_turn(result.is_ok(), parent_cancelled || turn_cancelled);
    if parent_cancelled {
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
                app.push_system(app::compaction_line(&to_wire_compaction(&result)));
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

/// Runs one `/reflect`: one model call with no tool events, then a one-line
/// report as system text.
async fn run_reflect<B: Backend>(
    app: &mut App,
    terminal: &mut Terminal<B>,
    keys: &mut mpsc::Receiver<TuiEvent>,
    controller: &Controller,
    cancel: &CancellationToken,
    focus: String,
) -> Result<(), ReplError> {
    app.start_turn();
    let turn = cancel.child_token();
    let result = {
        let (_events, mut received) = mpsc::unbounded_channel::<WireEvent>();
        drive_turn(
            app,
            terminal,
            keys,
            &mut received,
            &turn,
            &app::Backend::Local(controller),
            controller.reflect(&focus, &turn),
            |_app, _event| {},
        )
        .await
    };
    let cancelled =
        cancel.is_cancelled() || matches!(&result, Err(crate::reflection::Error::Cancelled));
    app.finish_turn(result.is_ok(), cancelled);
    if cancel.is_cancelled() {
        return Err(ReplError::Cancelled);
    }
    match result {
        Ok(report) => app.push_system(report.summary()),
        Err(crate::reflection::Error::Cancelled) => {}
        Err(error) => app.push_system(format!("/reflect: {error}")),
    }
    Ok(())
}

/// Streams ordinary command output through the same event/render loop as turns.
struct ProgressWriter {
    tx: mpsc::UnboundedSender<WireEvent>,
}
impl io::Write for ProgressWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let _ = self.tx.send(WireEvent {
            event_type: "frontend_notice".into(),
            text: String::from_utf8_lossy(bytes).into_owned(),
            ..Default::default()
        });
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn append_progress(app: &mut App, id: &str, event: &WireEvent) {
    if event.event_type != "frontend_notice" {
        return;
    }
    if let Some(entry) = app.entries.iter_mut().find(|e| e.id == id) {
        entry.raw.push_str(&event.text);
    } else {
        app.entries.push(entries::Entry {
            id: id.into(),
            kind: Some(entries::EntryKind::System),
            raw: event.text.clone(),
            ..Default::default()
        });
    }
}

async fn login_dispatch<B: Backend>(
    app: &mut App,
    terminal: &mut Terminal<B>,
    keys: &mut mpsc::Receiver<TuiEvent>,
    controller: &Controller,
    args: &str,
    cancel: &CancellationToken,
) {
    let (tx, mut events) = mpsc::unbounded_channel();
    let mut stdout = ProgressWriter { tx: tx.clone() };
    let mut stderr = ProgressWriter { tx };
    let operation = cancel.child_token();
    let id = format!("login-output-{}", app.entries.len());
    app.start_operation("authentication");
    let future = login::repl_login(controller, &mut stdout, &mut stderr, args, &operation);
    let result = drive_turn(
        app,
        terminal,
        keys,
        &mut events,
        &operation,
        &app::Backend::Local(controller),
        future,
        |app, event| append_progress(app, &id, event),
    )
    .await;
    app.end_turn();
    if let Err(error) = result {
        app.push_system(format!("/login: {error}"));
    }
}

async fn mcp_login_dispatch<B: Backend>(
    app: &mut App,
    terminal: &mut Terminal<B>,
    keys: &mut mpsc::Receiver<TuiEvent>,
    controller: &Controller,
    name: &str,
    cancel: &CancellationToken,
) {
    let (tx, mut events) = mpsc::unbounded_channel();
    let mut stdout = ProgressWriter { tx: tx.clone() };
    let mut stderr = ProgressWriter { tx };
    let operation = cancel.child_token();
    let args = format!("login {name}");
    let id = format!("mcp-login-output-{}", app.entries.len());
    app.start_operation("authentication");
    let future =
        repl_commands::repl_mcp_command(controller, &args, &mut stdout, &mut stderr, &operation);
    let result = drive_turn(
        app,
        terminal,
        keys,
        &mut events,
        &operation,
        &app::Backend::Local(controller),
        future,
        |app, event| append_progress(app, &id, event),
    )
    .await;
    app.end_turn();
    if let Err(error) = result {
        app.push_system(format!("/mcp login: {error}"));
    }
}

#[cfg(test)]
mod tests;
