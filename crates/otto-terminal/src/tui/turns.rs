//! Turn execution and restored prompts.

use super::*;

/// A non-fatal turn failure (already shown in the transcript by
/// [`run_turn`]/[`run_compact`]/[`run_wake`]) is swallowed so the session
/// continues; everything else (a fatal persistence failure, or the outer
/// `cancel` itself firing) ends [`run`].
pub(super) fn propagate_turn_error(error: ReplError) -> Result<(), ReplError> {
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
pub(super) fn drive_turn<B: Backend, T, E>(
    app: &mut App,
    terminal: &mut Terminal<B>,
    keys: &mut mpsc::Receiver<TuiEvent>,
    events: &mut mpsc::UnboundedReceiver<WireEvent>,
    turn: &CancellationToken,
    backend: &app::Backend<'_>,
    future: impl Future<Output = Result<T, E>>,
    mut apply: impl FnMut(&mut App, &WireEvent),
) -> impl Future<Output = Result<T, E>> {
    // Allocate before constructing the driver future: otherwise the controller's
    // large future is embedded in every enclosing TUI future, including restored
    // prompts and the main loop, and can overflow a default test thread's stack.
    let mut future = Box::pin(future);
    async move {
        let mut frames = render::spinner_interval();
        let mut refresh = tokio::time::interval(std::time::Duration::from_secs(2));
        let mut status = tokio::time::interval(std::time::Duration::from_millis(500));
        status.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut frame_gate = frame_gate::FrameGate::new(std::time::Instant::now());
        loop {
            let redraw = tokio::select! {
                result=&mut future=>{
                    while let Ok(event)=events.try_recv(){apply(app,&event);}
                    let _ = terminal.draw(|frame| render::draw(frame, app));
                    return result;
                }
                Some(event)=events.recv()=>{
                    let redraw=frame_gate::wire_redraw(&event);
                    apply(app,&event);
                    redraw
                }
                Some(event)=keys.recv()=>{
                    apply_turn_key(app,terminal,event,turn,backend);
                    frame_gate::Redraw::Immediate
                }
                completion=app.next_completion()=>{
                    if app.apply_completion(completion) {
                        frame_gate::Redraw::Immediate
                    } else {
                        frame_gate::Redraw::None
                    }
                }
                _=refresh.tick()=>{
                    app.tick_features();
                    app.request_approvals();
                    app.poll_steering();
                    frame_gate::Redraw::None
                }
                _=status.tick()=>{
                    app.poll_local_status();
                    frame_gate::Redraw::None
                }
                _=frames.tick()=>{
                    if render::needs_animation(app) {
                        frame_gate::Redraw::Immediate
                    } else {
                        frame_gate::Redraw::None
                    }
                }
                _=frame_gate::wait_for_frame(frame_gate.deadline())=>frame_gate::Redraw::Immediate,
            };
            app.poll_observer();
            let now = std::time::Instant::now();
            let draw = match redraw {
                frame_gate::Redraw::None => false,
                frame_gate::Redraw::Paced => frame_gate.request(now),
                frame_gate::Redraw::Immediate => true,
            };
            if draw {
                let _ = terminal.draw(|frame| render::draw(frame, app));
                frame_gate.drew(now);
            }
        }
    }
}

pub(super) fn apply_turn_key<B: Backend>(
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
pub(super) async fn run_restored_prompts<B: Backend>(
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
        controller.set_next_input_provenance(prompt.input_provenance);
        let result = run_turn(app, terminal, keys, controller, cancel, prompt.text, None).await;
        controller.set_next_input_id(None);
        if let Some(journal) = &journal {
            journal.forget(&prompt.input_id);
        }
        result?;
    }
    Ok(())
}

pub(super) async fn run_turn<B: Backend>(
    app: &mut App,
    terminal: &mut Terminal<B>,
    keys: &mut mpsc::Receiver<TuiEvent>,
    controller: &Controller,
    cancel: &CancellationToken,
    line: String,
    input_id: Option<&str>,
) -> Result<(), ReplError> {
    let provenance = input_id
        .and_then(|id| {
            controller
                .input_submissions()
                .ok()?
                .into_iter()
                .find(|record| record.id == id)
                .map(|record| record.provenance)
        })
        .unwrap_or_else(|| {
            otto_core::model::InputProvenance::local(otto_core::model::InputSource::Tui)
        });
    match controller
        .approval_message_with_provenance(&line, None, input_id, Some(provenance), cancel)
        .await
    {
        Ok(Some(reply)) => {
            app.push_system(reply.text);
            if reply.queued {
                app.queued_input = Some(line);
                app.queued_input_id = input_id.map(str::to_owned);
            }
            return Ok(());
        }
        Ok(None) => {}
        Err(message) => {
            app.push_system(message);
            return Ok(());
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
            controller.prompt(&line, &mut sink, &turn),
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
    let completion = if parent_cancelled || turn_cancelled {
        otto_platform::program_status::Completion::Canceled
    } else {
        otto_platform::program_status::Completion::from_result(&result)
    };
    app.finish_turn(completion);
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
pub(super) fn wire_frame(event: &Event) -> WireEvent {
    to_wire(event)
}
