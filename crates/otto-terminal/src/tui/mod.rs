//! The terminal frontend (ratatui + crossterm).
//!
//! [`run`] is the entry point `otto_cli::run` dispatches to for `--ui tui` (or `--ui
//! auto` on a terminal). It owns the alternate screen / raw mode lifecycle,
//! reads keys on a background task the way `cli::repl`'s `spawn_reader` reads
//! lines, and drives `app::App` against the same [`Controller`] the REPL
//! uses.
//!
//! Sub-agent wake turns: the idle loop also selects on the task registry's
//! update signal and runs an empty-text turn whenever a notification is
//! pending, matching [`crate::cli::repl::Repl`]. There is no second scheduler.

mod app;
mod approval_display;
pub mod attach;
mod backend;
mod commands;
mod entries;
mod features;
mod frame_gate;
pub(crate) mod gutter;
mod input;
mod interaction;
#[cfg(unix)]
mod job_control;
mod layout;
mod markdown;
mod render;
#[cfg(test)]
mod scroll_tests;
mod selection;
mod transcript;
mod widgets;

use std::future::Future;
use std::io;
use std::sync::Arc;

use crossterm::event::{
    DisableBracketedPaste, EnableBracketedPaste, Event as TermEvent, KeyEvent, KeyEventKind,
    MouseButton, MouseEventKind,
};
use otto_core::agent::{AgentError, Event};
use otto_core::wire::events::{WireEvent, to_wire, to_wire_compaction};
use ratatui::Terminal;
use ratatui::backend::Backend;
use tokio::sync::{mpsc, watch};
use tokio_util::sync::CancellationToken;

use crate::cli::login;
use crate::cli::repl::{Error as ReplError, is_fatal_persistence};
use crate::cli::repl_commands;
use otto::app::Controller;

use app::{Action, App};
use selection::{DragPhase, Selection};

mod observer;
mod operations;
mod terminal_input;
mod turns;
use operations::*;
pub use terminal_input::reset_terminal_input_modes_after_panic;
use terminal_input::*;
use turns::*;

/// Runs the terminal frontend to completion.
///
/// Enters the alternate screen, raw mode, and mouse capture, all restored on
/// return and panic.
pub async fn run(
    controller: &Arc<Controller>,
    cancel: &CancellationToken,
) -> Result<(), ReplError> {
    run_with_observer_bindings(controller, cancel, &Default::default()).await
}

/// Runs the TUI with client-local observer keys, validated before raw mode.
pub async fn run_with_observer_bindings(
    controller: &Arc<Controller>,
    cancel: &CancellationToken,
    bindings: &otto_core::config::ui::ObserverBindings,
) -> Result<(), ReplError> {
    let bindings = observer::bindings::Bindings::parse(bindings)
        .map_err(|error| io_error(io::Error::new(io::ErrorKind::InvalidInput, error)))?;
    let mut terminal = ratatui::try_init().map_err(io_error)?;
    let mut input = TerminalInput::new();
    if let Err(error) = input.enable_terminal() {
        let _ = ratatui::try_restore();
        return Err(io_error(error));
    }
    let mut keys = spawn_key_reader();
    let result = run_app(&mut terminal, controller, cancel, &mut keys, bindings).await;
    let _ = input.restore_terminal();
    let _ = ratatui::try_restore();
    result
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
    Animation,
    FeaturesTick,
    Completion(Box<backend::Completion>),
    Notices,
    MemoryReviewAvailable,
}

/// Draws only the current UI snapshot. Backend reads run independently.
fn redraw<B: Backend>(terminal: &mut Terminal<B>, app: &mut App) -> Result<(), ReplError> {
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
    bindings: observer::bindings::Bindings,
) -> Result<(), ReplError> {
    let backend = app::Backend::Local(controller);
    let mut app = App::new(&backend);
    app.observer.bindings = bindings;
    app.enable_program_status();
    app.driver = Some(backend::Driver::new(backend::Connection::Local(
        Arc::clone(controller),
    )));
    app.refresh_steering();
    let mut updates: Option<(Arc<dyn otto_core::mailbox::Mailbox>, watch::Receiver<u64>)> = None;
    let mut notices = controller.notices_changed();
    let mut memory_review = controller.memory_review_available_changed();
    let mut tick = tokio::time::interval(std::time::Duration::from_millis(500));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut frames = render::spinner_interval();
    let mut feature_tick = tokio::time::interval(std::time::Duration::from_secs(2));
    feature_tick.reset();
    let mut actions = std::collections::VecDeque::new();
    // The session whose saved turn queue was restored; a `/resume` or
    // `/new` changes the session and restores again.
    let mut restored_session = String::new();
    redraw(terminal, &mut app)?;
    loop {
        for text in controller.take_notices() {
            app.push_system(text);
        }
        let session_id = app.info.session_id.clone();
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
                let receiver = otto::runtime::workflow_notifications::updates(tasks.as_ref());
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
                _=frames.tick()=>IdleEvent::Animation,
                _=feature_tick.tick()=>IdleEvent::FeaturesTick,
                _=notices.changed()=>IdleEvent::Notices,
                _=memory_review.changed()=>IdleEvent::MemoryReviewAvailable,
            }
        };
        let mut should_redraw = match event {
            IdleEvent::Input(None) => {
                drain_writes(&mut app).await;
                return Ok(());
            }
            IdleEvent::Input(Some(event)) => {
                if let Some(action) = apply_input(&mut app, terminal, event, cancel, &backend)? {
                    actions.push_back(action);
                }
                true
            }
            IdleEvent::Completion(completion) => app.apply_completion(*completion),
            IdleEvent::Tick => {
                app.poll_local_status();
                app.request_approvals();
                app.poll_steering();
                false
            }
            IdleEvent::Animation => {
                app.poll_observer();
                render::needs_animation(&app)
            }
            IdleEvent::FeaturesTick => {
                app.tick_features();
                false
            }
            IdleEvent::MemoryReviewAvailable => {
                app.memory_review_available();
                true
            }
            IdleEvent::Notices => true,
            IdleEvent::Registry(false) => {
                updates = None;
                false
            }
            IdleEvent::Registry(true) => {
                if let Err(error) = run_wake(&mut app, terminal, keys, controller, cancel).await {
                    propagate_turn_error(error)?;
                }
                true
            }
        };
        if !app.busy()
            && !app.has_writes()
            && let Some(queued) = app.submit_queued_input(&backend, cancel)
        {
            actions.push_back(queued);
        }
        actions.extend(app.drain_actions());
        while let Some(action) = actions.pop_front() {
            should_redraw = true;
            match action {
                Action::Exit => {
                    drain_writes(&mut app).await;
                    return Ok(());
                }
                Action::CancelTurn => {}
                action @ (Action::Prompt(_) | Action::LocalPrompt { .. }) => {
                    let (line, input_id, delivery) = match action {
                        Action::Prompt(line) => (
                            line,
                            None,
                            app.next_input_delivery.take().unwrap_or_default(),
                        ),
                        Action::LocalPrompt {
                            text,
                            input_id,
                            delivery,
                        } => (text, Some(input_id), delivery),
                        _ => unreachable!(),
                    };
                    let mut provenance = otto_core::model::InputProvenance::local(
                        otto_core::model::InputSource::Tui,
                    );
                    provenance.delivery = delivery;
                    controller.set_next_input_provenance(Some(provenance));
                    controller.set_next_input_id(input_id.clone());
                    if let Some(id) = &input_id {
                        match controller.update_input_submission(
                            id,
                            "accepted",
                            Some(provenance.delivery),
                        ) {
                            Ok(()) => app.push_system(format!(
                                "[{id}] Tui / {:?} / accepted",
                                provenance.delivery
                            )),
                            Err(error) => {
                                app.push_system(format!(
                                    "[{id}] NOT started: {error}; /recover {id}"
                                ));
                                controller.set_next_input_id(None);
                                controller.set_next_input_provenance(None);
                                continue;
                            }
                        }
                    }
                    if let Some(id) = &input_id {
                        app.entries.push(entries::Entry {
                            id: format!("input-{id}"),
                            input_id: id.clone(),
                            kind: Some(entries::EntryKind::User),
                            raw: line.clone(),
                            input_label: format!(
                                "[{id}] {} / accepted (not yet delivery evidence)",
                                provenance.label()
                            ),
                            ..Default::default()
                        });
                    }
                    let result = run_turn(
                        &mut app,
                        terminal,
                        keys,
                        controller,
                        cancel,
                        line,
                        input_id.as_deref(),
                    )
                    .await;
                    controller.set_next_input_id(None);
                    if let Some(id) = &input_id {
                        let status = if controller.has_input(id) {
                            "delivered"
                        } else {
                            "uncertain: turn ended without delivery evidence"
                        };
                        if let Err(error) = controller.update_input_submission(id, status, None) {
                            app.push_system(format!("[{id}] status save failed: {error}; inspect /inputs before retrying"));
                        }
                        if let Some(entry) = app
                            .entries
                            .iter_mut()
                            .find(|entry| entry.id == format!("input-{id}"))
                        {
                            entry.input_label = format!("[{id}] {} / {status}", provenance.label());
                        }
                        app.push_system(format!("[{id}] {status}; /inputs for recovery status"));
                    }
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
        app.poll_observer();
        if should_redraw {
            redraw(terminal, &mut app)?;
        }
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
        #[cfg(unix)]
        TuiEvent::Suspend(_reader_parked) => {
            job_control::suspend(terminal)?;
            Ok(None)
        }
        TuiEvent::Key(key) => {
            app.selection = None;
            Ok(app.handle_key(key, backend, cancel))
        }
        TuiEvent::Paste(text) => {
            app.insert_text(&text);
            Ok(None)
        }
        TuiEvent::Wheel { up, steps } => {
            for _ in 0..steps {
                app.scroll_wheel(up);
            }
            Ok(None)
        }
        TuiEvent::Select { phase, col, row } => {
            if app.has_surface() {
                if phase == DragPhase::Start {
                    focus_surface(app, col, row);
                }
            } else if app.observer.active {
                if phase == DragPhase::Start && !app.observer.toggle_at(col, row) {
                    app.observer.focus_at(col, row);
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

#[cfg(test)]
mod tests;

/// Built-in slash command names in catalog order, for tests of other frontends.
#[cfg(test)]
pub(crate) fn catalog_command_names() -> Vec<&'static str> {
    features::Catalog::default()
        .entries()
        .iter()
        .map(|entry| entry.name)
        .collect()
}
