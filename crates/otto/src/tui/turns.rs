//! Turn execution, restored prompts and image attachment loading.

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
pub(super) async fn drive_turn<B: Backend, T, E>(
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
pub(super) fn wire_frame(event: &Event) -> WireEvent {
    to_wire(event)
}

pub(super) fn image_block_from_path(path: &str) -> Result<Block, String> {
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
