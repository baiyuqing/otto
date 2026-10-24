//! The attached-session event loop and its drawing.

use super::*;

pub(super) async fn run_loop<B: TerminalBackend>(
    terminal: &mut Terminal<B>,
    remote: &Arc<Remote>,
    status: StatusStream,
    cancel: &CancellationToken,
    keys: &mut mpsc::Receiver<TuiEvent>,
    deliveries: &mut mpsc::Receiver<String>,
) -> Result<(), ReplError> {
    let mut app = App::new(&Backend::Attach(remote));
    app.enable_program_status();
    app.driver = Some(super::super::backend::Driver::new(
        super::super::backend::Connection::Attach(Arc::clone(remote)),
    ));
    app.refresh_steering();
    let mut ui = Ui {
        remote,
        app,
        cancel,
        followed: None,
        status: Some(status),
        rows: Vec::new(),
        last_row: None,
        last_done: None,
        fresh: true,
        compacting: None,
        connected: true,
        pending: None,
        epoch: 0,
        transcript: 0,
        needs_sync: false,
        cancel_after_send: false,
        actions: VecDeque::new(),
    };
    let mut retry = tokio::time::interval(RETRY_INTERVAL);
    let mut agents = tokio::time::interval(Duration::from_secs(2));
    let mut approvals = tokio::time::interval(Duration::from_millis(500));
    let mut frames = render::spinner_interval();
    draw(terminal, &mut ui.app)?;
    loop {
        let event = {
            let followed = ui.followed.as_mut();
            let status = ui.status.as_mut();
            let compacting = ui.compacting.as_mut();
            let pending = ui.pending.as_mut();
            let connected = ui.connected;
            let frame = async {
                match followed {
                    Some(followed) => followed.stream.next().await,
                    None => std::future::pending().await,
                }
            };
            let snapshot = async {
                match status {
                    Some(status) => status.next().await,
                    None => std::future::pending().await,
                }
            };
            let compaction = async {
                match compacting {
                    Some(call) => call.await,
                    None => std::future::pending().await,
                }
            };
            let transport = async {
                match pending {
                    Some(pending) => pending.call.as_mut().await,
                    None => std::future::pending().await,
                }
            };
            let reconnect = async {
                if connected {
                    std::future::pending::<()>().await;
                }
                retry.tick().await;
            };
            tokio::select! {
                _ = cancel.cancelled() => return Err(ReplError::Cancelled),
                event = keys.recv() => Ev::Input(event),
                completion = ui.app.next_completion() => Ev::Completion(completion),
                result = frame => Ev::Frame(result),
                result = snapshot => Ev::Status(result),
                result = compaction => Ev::Compact(result),
                result = transport => Ev::Transport(result),
                text = deliveries.recv() => Ev::Scheduled(text),
                () = reconnect => Ev::Retry,
                _ = agents.tick() => Ev::FeaturesTick,
                _ = approvals.tick(), if connected => Ev::ApprovalsTick,
                _ = frames.tick() => Ev::Tick,
            }
        };
        match event {
            Ev::Input(None) => {
                ui.drain_transport().await;
                super::super::drain_writes(&mut ui.app).await;
                return Ok(());
            }
            Ev::Input(Some(event)) => ui.on_input(terminal, event)?,
            Ev::Frame(result) => ui.on_frame(result),
            Ev::Status(result) => ui.on_status(result),
            Ev::Compact(result) => ui.on_compact(result),
            Ev::Transport(result) => ui.on_transport(result),
            Ev::Scheduled(text) => {
                if let Some(text) = text {
                    ui.transcript = ui.transcript.wrapping_add(1);
                    ui.app.push_system(text);
                    // The terminal backend writes to stdout.
                    let mut stdout = std::io::stdout();
                    let _ = stdout.write_all(b"\x07").and_then(|()| stdout.flush());
                }
            }
            Ev::Retry => ui.reconnect(),
            Ev::Completion(completion) => {
                let previous = ui.app.info.session_id.clone();
                let entries = ui.app.entries.len();
                let mutation = completion.mutation;
                ui.app.apply_completion(completion);
                if mutation && ui.pending.as_ref().is_some_and(|pending| !pending.mutation) {
                    ui.pending = None;
                    ui.needs_sync = true;
                }
                if ui.app.entries.len() != entries {
                    ui.transcript = ui.transcript.wrapping_add(1);
                }
                if ui.app.info.session_id != previous {
                    ui.epoch = ui.epoch.wrapping_add(1);
                    if ui.pending.as_ref().is_some_and(|pending| !pending.mutation) {
                        ui.pending = None;
                    }
                    ui.needs_sync = true;
                    ui.followed = None;
                    ui.last_done = None;
                    ui.last_row = None;
                    ui.fresh = true;
                    ui.app.finish_turn(false, true);
                }
            }
            Ev::FeaturesTick => ui.app.tick_features(),
            Ev::ApprovalsTick => {
                ui.app.request_approvals();
                ui.app.poll_steering();
            }
            Ev::Tick => {
                // Idle frames need no redraw unless something on screen moves.
                if !render::needs_animation(&ui.app) {
                    continue;
                }
            }
        }
        if !ui.app.busy()
            && !ui.app.has_writes()
            && let Some(action) = ui.app.submit_queued_input(&Backend::Attach(remote), cancel)
        {
            ui.actions.push_back(action);
        }
        ui.actions.extend(ui.app.drain_actions());
        if ui.run_actions() {
            ui.drain_transport().await;
            super::super::drain_writes(&mut ui.app).await;
            return Ok(());
        }
        ui.sync();
        draw(terminal, &mut ui.app)?;
    }
}

fn draw<B: TerminalBackend>(terminal: &mut Terminal<B>, app: &mut App) -> Result<(), ReplError> {
    terminal
        .draw(|frame| render::draw(frame, app))
        .map(|_| ())
        .map_err(super::super::draw_error)
}
