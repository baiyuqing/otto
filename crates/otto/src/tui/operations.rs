//! Wake, compact, reflect and login turns driven from the terminal UI.

use super::*;

/// One empty-text turn delivering pending sub-agent notifications. The TUI has
/// no marker to skip, so the only extra work is bracketing the claim
/// with [`App::start_turn`] so Esc still cancels and the thinking line still
/// animates.
pub(super) async fn run_wake<B: Backend>(
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
pub(super) async fn run_compact<B: Backend>(
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
pub(super) async fn run_reflect<B: Backend>(
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
pub(super) struct ProgressWriter {
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

pub(super) fn append_progress(app: &mut App, id: &str, event: &WireEvent) {
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

pub(super) async fn login_dispatch<B: Backend>(
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

pub(super) async fn mcp_login_dispatch<B: Backend>(
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
