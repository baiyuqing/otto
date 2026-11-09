//! The attached-session event loop and its drawing.

use super::*;

pub(super) async fn run_loop<B: TerminalBackend>(
    terminal: &mut Terminal<B>,
    remote: &Arc<Remote>,
    status: StatusStream,
    cancel: &CancellationToken,
    keys: &mut mpsc::Receiver<TuiEvent>,
    deliveries: &mut mpsc::Receiver<String>,
    bindings: super::super::observer::bindings::Bindings,
) -> Result<(), ReplError> {
    let mut app = App::new(&Backend::Attach(remote));
    app.observer.bindings = bindings;
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
        connection: ConnectionState::Connected(status),
        rows: Vec::new(),
        last_row: None,
        last_done: None,
        fresh: true,
        compacting: None,
        transport: TransportState::Idle,
        epoch: 0,
        transcript: 0,
        needs_sync: false,
        actions: VecDeque::new(),
    };
    let mut retry = tokio::time::interval(RETRY_INTERVAL);
    let mut agents = tokio::time::interval(Duration::from_secs(2));
    let mut approvals = tokio::time::interval(Duration::from_millis(500));
    let mut frames = render::spinner_interval();
    draw(terminal, &mut ui.app)?;
    let mut frame_gate = frame_gate::FrameGate::new(std::time::Instant::now());
    loop {
        let event = {
            let followed = ui.followed.as_mut();
            let connected = ui.connection.is_connected();
            let status = ui.connection.status_mut();
            let compacting = ui.compacting.as_mut();
            let pending = ui.transport.pending_mut();
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
                _ = frame_gate::wait_for_frame(frame_gate.deadline()) => Ev::Draw,
            }
        };
        let mut redraw = match event {
            Ev::Input(None) => {
                ui.drain_transport().await;
                super::super::drain_writes(&mut ui.app).await;
                return Ok(());
            }
            Ev::Input(Some(event)) => {
                ui.on_input(terminal, event)?;
                frame_gate::Redraw::Immediate
            }
            Ev::Frame(result) => {
                let redraw = match &result {
                    Ok(Some(event)) => frame_gate::wire_redraw(event),
                    Ok(None) | Err(_) => frame_gate::Redraw::Immediate,
                };
                ui.on_frame(result);
                redraw
            }
            Ev::Status(result) => {
                let disconnected = !matches!(&result, Ok(Some(_)));
                ui.on_status(result);
                if disconnected {
                    frame_gate::Redraw::Immediate
                } else {
                    frame_gate::Redraw::None
                }
            }
            Ev::Compact(result) => {
                ui.on_compact(result);
                frame_gate::Redraw::Immediate
            }
            Ev::Transport(result) => {
                ui.on_transport(result);
                frame_gate::Redraw::Immediate
            }
            Ev::Scheduled(text) => {
                let visible = text.is_some();
                if let Some(text) = text {
                    ui.transcript = ui.transcript.wrapping_add(1);
                    ui.app.push_system(text);
                    // The terminal backend writes to stdout.
                    let mut stdout = std::io::stdout();
                    let _ = stdout.write_all(b"\x07").and_then(|()| stdout.flush());
                }
                if visible {
                    frame_gate::Redraw::Immediate
                } else {
                    frame_gate::Redraw::None
                }
            }
            Ev::Retry => {
                ui.reconnect();
                frame_gate::Redraw::None
            }
            Ev::Completion(completion) => {
                let previous = ui.app.info.session_id.clone();
                let entries = ui.app.entries.len();
                let mutation = completion.mutation;
                let changed = ui.app.apply_completion(completion);
                if mutation && ui.transport.discard_read() {
                    ui.needs_sync = true;
                }
                if ui.app.entries.len() != entries {
                    ui.transcript = ui.transcript.wrapping_add(1);
                }
                if ui.app.info.session_id != previous {
                    ui.epoch = ui.epoch.wrapping_add(1);
                    ui.transport.discard_read();
                    ui.needs_sync = true;
                    ui.followed = None;
                    ui.last_done = None;
                    ui.last_row = None;
                    ui.fresh = true;
                    ui.app
                        .finish_turn(crate::program_status::Completion::Canceled);
                }
                if changed {
                    frame_gate::Redraw::Immediate
                } else {
                    frame_gate::Redraw::None
                }
            }
            Ev::FeaturesTick => {
                ui.app.tick_features();
                frame_gate::Redraw::None
            }
            Ev::ApprovalsTick => {
                ui.app.request_approvals();
                ui.app.poll_steering();
                frame_gate::Redraw::None
            }
            Ev::Tick => {
                ui.app.poll_observer();
                if render::needs_animation(&ui.app) {
                    frame_gate::Redraw::Immediate
                } else {
                    frame_gate::Redraw::None
                }
            }
            Ev::Draw => frame_gate::Redraw::Immediate,
        };
        if !ui.app.busy()
            && !ui.app.has_writes()
            && let Some(action) = ui.app.submit_queued_input(&Backend::Attach(remote), cancel)
        {
            ui.actions.push_back(action);
        }
        ui.actions.extend(ui.app.drain_actions());
        let has_actions = !ui.actions.is_empty();
        if ui.run_actions() {
            ui.drain_transport().await;
            super::super::drain_writes(&mut ui.app).await;
            return Ok(());
        }
        if has_actions {
            redraw = frame_gate::Redraw::Immediate;
        }
        ui.sync();
        let now = std::time::Instant::now();
        let should_draw = match redraw {
            frame_gate::Redraw::None => false,
            frame_gate::Redraw::Paced => frame_gate.request(now),
            frame_gate::Redraw::Immediate => true,
        };
        if should_draw {
            draw(terminal, &mut ui.app)?;
            frame_gate.drew(now);
        }
    }
}

fn draw<B: TerminalBackend>(terminal: &mut Terminal<B>, app: &mut App) -> Result<(), ReplError> {
    terminal
        .draw(|frame| render::draw(frame, app))
        .map(|_| ())
        .map_err(super::super::draw_error)
}
