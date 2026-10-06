//! `otto --attach`: the terminal UI as a client of a running `otto serve`.
//!
//! The process opens no session, runs no model or tool and reads no provider
//! credentials. Every session operation is an HTTP request on serve's Unix
//! socket (see [`crate::client`]); the screen is the shared [`App`] with
//! [`Backend::Attach`].
//!
//! Turns: a prompt posts a queued turn and the loop follows that turn's
//! events (`turn_events` from frame 0) until `turn_end`. The `user_message`
//! frame is the transcript's echo of the prompt, so nothing is shown locally
//! when it is sent. The status stream (`GET /v1/status`) names the session's
//! running turn; one this process did not start is followed too, after the
//! history before it is loaded.
//!
//! Loss: a request that cannot reach serve, a stream that ends, or a turn
//! stream that closes without `turn_end` marks the connection lost. The loop
//! retries `GET /v1/status` every second, then resumes the same session,
//! reloads its history and follows its running turn again.

use std::collections::VecDeque;
use std::future::Future;
use std::io::Write;
use std::path::Path;
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use otto_core::model::Message;
use otto_core::wire::events::{TURN_END, WireCompaction, WireEvent};
use ratatui::Terminal;
use ratatui::backend::Backend as TerminalBackend;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use super::app::{Action, App, Backend, compaction_line};
use super::{TerminalInput, TuiEvent, render, spawn_key_reader};
use crate::app::Info;
use crate::cli::repl::Error as ReplError;
use crate::client::{Client, Error, StatusRow, StatusStream, TurnStream};
use crate::schedule::worker::{DeliverRole, Sink, Worker};
use crate::schedule::{Origin, Scope};

/// Bound on one request. A long call (`/compact`) runs as a future in the
/// loop instead.
const CALL_TIMEOUT: Duration = Duration::from_secs(5);
const RETRY_INTERVAL: Duration = Duration::from_secs(1);
const DISCONNECTED: &str = "disconnected from otto serve";

/// Which session `otto --attach` opens at startup.
pub(crate) enum Start {
    /// A new session in the working directory's workspace.
    New,
    /// The newest session of that workspace, or a new one when it has none.
    Continue,
    /// The stored session of this id.
    Resume(String),
}

async fn within<T>(call: impl Future<Output = Result<T, Error>>) -> Result<T, Error> {
    tokio::time::timeout(CALL_TIMEOUT, call)
        .await
        .unwrap_or_else(|_| Err(Error::Unreachable("request timed out".to_string())))
}

#[derive(Default)]
struct State {
    session: String,
    info: Info,
    history: Vec<Message>,
    /// Turns this process started and has not seen end, oldest first.
    own: Vec<String>,
    /// The turn the loop is following.
    running: Option<String>,
}

/// The serve connection [`Backend::Attach`] reaches. The loop owns the
/// asynchronous calls; the synchronous methods below answer the [`App`]'s
/// handlers from cached state. Queries and operations use the async driver.
pub(crate) struct Remote {
    pub(crate) client: Client,
    workspace: String,
    state: Mutex<State>,
}

impl Remote {
    fn new(client: Client, workspace: &str) -> Self {
        Self {
            client,
            workspace: workspace.to_string(),
            state: Mutex::new(State::default()),
        }
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().expect("attach state")
    }

    pub(crate) fn info(&self) -> Info {
        self.state().info.clone()
    }

    pub(crate) fn history(&self) -> Vec<Message> {
        self.state().history.clone()
    }

    pub(crate) fn workspace(&self) -> &str {
        &self.workspace
    }

    pub(crate) fn session(&self) -> String {
        self.state().session.clone()
    }

    pub(crate) async fn withdraw_queued_turn_async(&self) -> Result<bool, String> {
        let (session, turn) = {
            let state = self.state();
            let turn = state
                .own
                .iter()
                .rev()
                .find(|id| state.running.as_deref() != Some(id.as_str()))
                .cloned();
            (state.session.clone(), turn)
        };
        let Some(turn) = turn else { return Ok(false) };
        within(self.client.cancel_turn(&session, &turn))
            .await
            .map_err(|e| e.to_string())?;
        let mut state = self.state();
        if state.session == session {
            state.own.retain(|id| *id != turn);
        }
        Ok(true)
    }

    /// Replaces the cached session, history and info.
    async fn load(&self, id: &str, before_turn: Option<&str>) -> Result<(), Error> {
        let history = within(self.client.history(id, before_turn)).await?;
        let info = within(self.client.session(id)).await?;
        let mut state = self.state();
        state.session = id.to_string();
        state.history = history;
        state.info = info;
        Ok(())
    }

    /// Opens a session on serve and makes it the current one.
    pub(crate) async fn open(
        &self,
        workspace: Option<&str>,
        resume: Option<&str>,
    ) -> Result<(), Error> {
        let id = within(self.client.open_session(workspace, resume)).await?;
        self.load(&id, None).await?;
        let mut state = self.state();
        state.own.clear();
        state.running = None;
        Ok(())
    }

    pub(crate) async fn refresh_info(&self) -> Result<(), Error> {
        let session = self.session();
        let info = within(self.client.session(&session)).await?;
        self.state().info = info;
        Ok(())
    }

    fn own(&self) -> Vec<String> {
        self.state().own.clone()
    }

    fn finish(&self, turn: &str) {
        let mut state = self.state();
        state.own.retain(|id| id != turn);
        if state.running.as_deref() == Some(turn) {
            state.running = None;
        }
    }

    #[cfg(test)]
    pub(crate) fn offline(history: Vec<Message>) -> Self {
        let client = Client::new(Path::new("/nonexistent/otto.sock")).expect("client");
        let remote = Self::new(client, "/work");
        remote.state().history = history;
        remote
    }
}

async fn connect(socket: &Path, workspace: &str, start: &Start) -> Result<Remote, Error> {
    let remote = Remote::new(Client::new(socket)?, workspace);
    within(remote.client.healthz()).await?;
    match start {
        Start::New => remote.open(Some(workspace), None).await?,
        Start::Resume(id) => remote.open(None, Some(id)).await?,
        Start::Continue => {
            let rows = within(remote.client.list_sessions(workspace)).await?;
            match rows.iter().max_by(|a, b| a.modified.cmp(&b.modified)) {
                Some(row) => remote.open(None, Some(&row.id)).await?,
                None => remote.open(Some(workspace), None).await?,
            }
        }
    }
    Ok(remote)
}

/// Runs the attached terminal UI to completion and returns the exit status.
pub(crate) async fn run(
    socket: &Path,
    workspace: &str,
    start: Start,
    schedule: Option<Arc<dyn crate::schedule::ScheduleStore>>,
    cancel: &CancellationToken,
    stderr: &mut (dyn Write + Send),
) -> i32 {
    let opened = async {
        let remote = connect(socket, workspace, &start).await?;
        let status = within(remote.client.status()).await?;
        Ok::<_, Error>((remote, status))
    }
    .await;
    let (remote, status) = match opened {
        Ok(opened) => opened,
        Err(error) => {
            let _ = writeln!(
                stderr,
                "otto serve is not reachable at {}: {error}",
                socket.display()
            );
            return 1;
        }
    };
    let remote = Arc::new(remote);
    let mut terminal = match ratatui::try_init() {
        Ok(terminal) => terminal,
        Err(error) => {
            let _ = writeln!(stderr, "otto: {error}");
            return 1;
        }
    };
    let mut input = TerminalInput::new();
    if let Err(error) = input.enable(std::io::stdout()) {
        let _ = ratatui::try_restore();
        let _ = writeln!(stderr, "otto: {error}");
        return 1;
    }
    let mut keys = spawn_key_reader();
    // `deliver_tx` stays alive in this function so a missing worker leaves the
    // receiver pending instead of closed.
    let (deliver_tx, mut deliveries) = mpsc::channel(8);
    let worker_cancel = cancel.child_token();
    let worker = schedule.map(|store| {
        tokio::spawn(
            Worker {
                store,
                run: None,
                deliver: Some(DeliverRole {
                    scope: Scope::Terminal,
                    sink: Arc::new(ChannelSink(deliver_tx.clone())),
                }),
                // The terminal screen is owned by the TUI.
                logger: Arc::new(crate::log::Logger::discard()),
                metrics: None,
            }
            .run(worker_cancel.clone()),
        )
    });
    let result = run_loop(
        &mut terminal,
        &remote,
        status,
        cancel,
        &mut keys,
        &mut deliveries,
    )
    .await;
    // Sends after the loop has exited fail, so those items stay undelivered.
    drop(deliveries);
    worker_cancel.cancel();
    if let Some(worker) = worker {
        let _ = worker.await;
    }
    drop(deliver_tx);
    let _ = input.restore(std::io::stdout());
    let _ = ratatui::try_restore();
    match result {
        Ok(()) => 0,
        Err(ReplError::Cancelled) => 130,
        Err(error) => {
            let _ = writeln!(stderr, "otto: {error}");
            1
        }
    }
}

/// What the loop does with one status row of its own session.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Follow {
    /// A turn this process queued has started running.
    Own(String),
    /// A turn another client started is running.
    Foreign(String),
    /// A turn started and ended between two status snapshots: reload the
    /// history, which already holds it.
    Reload(String),
}

/// Decides from the session's status row whether to start following a turn.
/// Nothing is followed while a turn already is (ponytail: one turn at a time,
/// the queued next turn is picked up when the followed one ends), and the
/// turn that just ended is skipped because the row can still name it.
pub(crate) fn follow_decision(
    row: &StatusRow,
    followed: Option<&str>,
    own: &[String],
    last_done: Option<&str>,
) -> Option<Follow> {
    let turn = row.turn.as_deref();
    if followed.is_some() || turn == Some("queued") {
        return None;
    }
    let id = row.turn_id.as_deref()?;
    if last_done == Some(id) {
        return None;
    }
    if turn != Some("running") {
        return turn.map(|_| Follow::Reload(id.to_string()));
    }
    Some(if own.iter().any(|own| own == id) {
        Follow::Own(id.to_string())
    } else {
        Follow::Foreign(id.to_string())
    })
}

struct Followed {
    stream: TurnStream,
    /// An `agent_error` frame already showed the failure.
    error_rendered: bool,
}

/// Hands scheduled deliveries to the attach loop. `Err` once the loop has
/// exited, so the item is retried by another process.
struct ChannelSink(mpsc::Sender<String>);

#[async_trait::async_trait]
impl Sink for ChannelSink {
    async fn deliver(&self, _origin: &Origin, text: &str) -> Result<(), String> {
        self.0
            .send(text.to_string())
            .await
            .map_err(|_| "the terminal UI has exited".to_string())
    }
}

type CompactCall<'a> =
    Pin<Box<dyn Future<Output = Result<Option<WireCompaction>, Error>> + Send + 'a>>;

type TransportCall<'a> = Pin<Box<dyn Future<Output = Result<TransportResult, Error>> + Send + 'a>>;

struct PendingTransport<'a> {
    session: String,
    epoch: u64,
    transcript: u64,
    mutation: bool,
    call: TransportCall<'a>,
}

struct Snapshot {
    info: Option<Info>,
    tasks: Option<Vec<crate::subagent::tasks::Task>>,
    history: Option<Vec<Message>>,
    stream: Option<TurnStream>,
    done: Option<String>,
    row: Option<StatusRow>,
}

// The HTTP results stay owned here until their original scope is checked.
#[allow(clippy::large_enum_variant)]
enum TransportResult {
    Sync(Snapshot),
    Reconnect {
        snapshot: Snapshot,
        status: StatusStream,
    },
    Prompt {
        reply: Option<String>,
        stream: Option<TurnStream>,
    },
    Cancel,
}

/// Network work is polled beside terminal input, stream frames and driver results.
/// The attach loop's states: connected or not (`connected`), idle or
/// following one turn (`followed`), and optionally a `/compact` in flight
/// (`compacting`). Keys, the followed turn's frames, status snapshots, the
/// compaction result and the reconnect timer are the events.
struct Ui<'a> {
    remote: &'a Remote,
    app: App,
    cancel: &'a CancellationToken,
    followed: Option<Followed>,
    status: Option<StatusStream>,
    rows: Vec<StatusRow>,
    /// The row of this session at the last sync, to refresh the sub-agent
    /// panel only when it changes.
    last_row: Option<StatusRow>,
    last_done: Option<String>,
    /// The history was loaded after the last status snapshot, so a final turn
    /// in the next one is already in it.
    fresh: bool,
    compacting: Option<CompactCall<'a>>,
    connected: bool,
    pending: Option<PendingTransport<'a>>,
    epoch: u64,
    transcript: u64,
    needs_sync: bool,
    cancel_after_send: bool,
    actions: VecDeque<Action>,
}

// ponytail: one short-lived value per loop turn; boxing the frame is not worth it.
#[allow(clippy::large_enum_variant)]
enum Ev {
    Input(Option<TuiEvent>),
    Frame(Result<Option<WireEvent>, Error>),
    Status(Result<Option<Vec<StatusRow>>, Error>),
    Compact(Result<Option<WireCompaction>, Error>),
    Transport(Result<TransportResult, Error>),
    Scheduled(Option<String>),
    Retry,
    Completion(super::backend::Completion),
    FeaturesTick,
    ApprovalsTick,
    Tick,
}

async fn run_loop<B: TerminalBackend>(
    terminal: &mut Terminal<B>,
    remote: &Arc<Remote>,
    status: StatusStream,
    cancel: &CancellationToken,
    keys: &mut mpsc::Receiver<TuiEvent>,
    deliveries: &mut mpsc::Receiver<String>,
) -> Result<(), ReplError> {
    let mut app = App::new(&Backend::Attach(remote));
    app.driver = Some(super::backend::Driver::new(
        super::backend::Connection::Attach(Arc::clone(remote)),
    ));
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
    let mut frames = tokio::time::interval(render::SPINNER_FRAME);
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
                super::drain_writes(&mut ui.app).await;
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
                    ui.app.pending_image = None;
                    ui.app.end_turn();
                }
            }
            Ev::FeaturesTick => ui.app.tick_features(),
            Ev::ApprovalsTick => ui.app.request_approvals(),
            Ev::Tick => {
                // Idle frames need no redraw unless something on screen moves.
                if !ui.app.busy()
                    && !ui.app.has_surface()
                    && !render::needs_task_clock(&ui.app.tasks)
                {
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
            super::drain_writes(&mut ui.app).await;
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
        .map_err(super::draw_error)
}

impl<'a> Ui<'a> {
    fn report(&mut self, what: &str, error: Error) {
        if matches!(error, Error::Unreachable(_)) {
            self.disconnect();
        } else {
            self.app.push_system(format!("{what}: {error}"));
        }
    }

    fn disconnect(&mut self) {
        if !self.connected {
            return;
        }
        self.connected = false;
        if self
            .pending
            .as_ref()
            .is_some_and(|pending| !pending.mutation)
        {
            self.pending = None;
        }
        self.followed = None;
        self.status = None;
        self.compacting = None;
        self.rows.clear();
        self.last_row = None;
        self.app.end_turn();
        self.app.connected = false;
        self.app.push_system(DISCONNECTED);
        self.transcript = self.transcript.wrapping_add(1);
    }

    fn pending_call(&mut self, mutation: bool, call: TransportCall<'a>) {
        self.pending = Some(PendingTransport {
            session: self.remote.session(),
            epoch: self.epoch,
            transcript: self.transcript,
            mutation,
            call,
        });
    }

    /// Reconnect commits nothing until the same session has been authoritatively reloaded.
    fn reconnect(&mut self) {
        if self.pending.is_some() {
            return;
        }
        let remote = self.remote;
        let session = remote.session();
        self.pending_call(
            false,
            Box::pin(async move {
                let status = within(remote.client.status()).await?;
                let id = within(remote.client.open_session(None, Some(&session))).await?;
                if id != session {
                    return Err(Error::Unreachable("reconnect changed session".into()));
                }
                let (info, history, tasks) = tokio::try_join!(
                    within(remote.client.session(&session)),
                    within(remote.client.history(&session, None)),
                    within(remote.client.session_tasks(&session)),
                )?;
                Ok(TransportResult::Reconnect {
                    snapshot: Snapshot {
                        info: Some(info),
                        tasks: Some(tasks),
                        history: Some(history),
                        stream: None,
                        done: None,
                        row: None,
                    },
                    status,
                })
            }),
        );
    }

    fn on_input<B: TerminalBackend>(
        &mut self,
        terminal: &mut Terminal<B>,
        event: TuiEvent,
    ) -> Result<(), ReplError> {
        let entries = self.app.entries.len();
        if let Some(action) = super::apply_input(
            &mut self.app,
            terminal,
            event,
            self.cancel,
            &Backend::Attach(self.remote),
        )? {
            match action {
                Action::CancelTurn => self.cancel_followed(),
                _ => self.actions.push_back(action),
            }
        }
        if self.app.entries.len() != entries {
            self.transcript = self.transcript.wrapping_add(1);
        }
        Ok(())
    }

    /// Esc: cancels the followed turn. A `/compact` has no turn to cancel.
    fn cancel_followed(&mut self) {
        if self
            .pending
            .as_ref()
            .is_some_and(|pending| pending.mutation)
        {
            self.cancel_after_send = true;
            return;
        }
        let Some(turn) = self.followed.as_ref().map(|f| f.stream.turn_id.clone()) else {
            return;
        };
        self.cancel_turn(turn);
    }

    fn cancel_turn(&mut self, turn: String) {
        self.pending = None;
        let remote = self.remote;
        let session = remote.session();
        self.pending_call(
            true,
            Box::pin(async move {
                within(remote.client.cancel_turn(&session, &turn)).await?;
                Ok(TransportResult::Cancel)
            }),
        );
    }

    fn on_frame(&mut self, result: Result<Option<WireEvent>, Error>) {
        self.transcript = self.transcript.wrapping_add(1);
        match result {
            Ok(Some(event)) if event.event_type == TURN_END => self.end_turn(&event),
            Ok(Some(event)) => {
                if self.app.apply_event(&event)
                    && let Some(followed) = &mut self.followed
                {
                    followed.error_rendered = true;
                }
            }
            Ok(None) | Err(_) => self.disconnect(),
        }
    }

    fn end_turn(&mut self, event: &WireEvent) {
        let Some(followed) = self.followed.take() else {
            return;
        };
        let id = followed.stream.turn_id;
        self.remote.finish(&id);
        self.last_done = Some(id);
        self.app.end_turn();
        match event.status.as_str() {
            "error" if !followed.error_rendered => {
                self.app.push_system(if event.error.is_empty() {
                    "turn failed"
                } else {
                    &event.error
                })
            }
            "canceled" => self.app.push_system("turn canceled"),
            _ => {}
        }
        self.needs_sync = true;
        if self.app.queued_input.is_some()
            && let Some(action) = self
                .app
                .submit_queued_input(&Backend::Attach(self.remote), self.cancel)
        {
            self.actions.push_back(action);
        }
    }

    fn on_status(&mut self, result: Result<Option<Vec<StatusRow>>, Error>) {
        match result {
            Ok(Some(rows)) => self.rows = rows,
            Ok(None) | Err(_) => self.disconnect(),
        }
    }

    /// Coalesces status changes while a snapshot or admitted operation is pending.
    fn sync(&mut self) {
        if !self.connected || self.pending.is_some() {
            return;
        }
        let session = self.remote.session();
        let row = self.rows.iter().find(|row| row.id == session).cloned();
        let decision = row.as_ref().and_then(|row| {
            follow_decision(
                row,
                self.followed.as_ref().map(|f| f.stream.turn_id.as_str()),
                &self.remote.own(),
                self.last_done.as_deref(),
            )
        });
        if !self.needs_sync && self.last_row == row && decision.is_none() {
            return;
        }
        let fresh = self.fresh;
        let remote = self.remote;
        self.needs_sync = false;
        self.pending_call(
            false,
            Box::pin(async move {
                let (before, done, follow) = match decision {
                    Some(Follow::Own(id)) => (None, None, Some(id)),
                    Some(Follow::Foreign(id)) => (Some(id.clone()), None, Some(id)),
                    Some(Follow::Reload(id)) => (None, Some(id), None),
                    None => (None, None, None),
                };
                let reload = before.is_some() || done.is_some() && !fresh;
                let history = async {
                    if reload {
                        within(remote.client.history(&session, before.as_deref()))
                            .await
                            .map(Some)
                    } else {
                        Ok(None)
                    }
                };
                let following = follow.is_some();
                let stream = async {
                    match follow {
                        Some(id) => within(remote.client.turn_events(&session, &id, None))
                            .await
                            .map(Some),
                        None => Ok(None),
                    }
                };
                let metadata = async {
                    if following {
                        return Ok((None, None));
                    }
                    let tasks = async {
                        match within(remote.client.session_tasks(&session)).await {
                            Ok(tasks) => Ok(Some(tasks)),
                            Err(Error::Http { .. }) => Ok(None),
                            Err(error) => Err(error),
                        }
                    };
                    let (info, tasks) =
                        tokio::try_join!(within(remote.client.session(&session)), tasks)?;
                    Ok::<_, Error>((Some(info), tasks))
                };
                let ((info, tasks), history, stream) = tokio::try_join!(metadata, history, stream)?;
                Ok(TransportResult::Sync(Snapshot {
                    info,
                    tasks,
                    history,
                    stream,
                    done,
                    row,
                }))
            }),
        );
    }

    fn on_transport(&mut self, result: Result<TransportResult, Error>) {
        let Some(pending) = self.pending.take() else {
            return;
        };
        if pending.session != self.remote.session() || pending.epoch != self.epoch {
            self.cancel_after_send = false;
            return;
        }
        let result = match result {
            Ok(result) => result,
            Err(error) => {
                self.cancel_after_send = false;
                if pending.mutation && matches!(error, Error::Unreachable(_)) {
                    self.app.push_system(format!(
                        "{error}; outcome unknown, reconcile before retrying"
                    ));
                }
                if !self.connected {
                    return;
                }
                if !pending.mutation && !matches!(error, Error::Unreachable(_)) {
                    self.last_row = self
                        .rows
                        .iter()
                        .find(|row| row.id == pending.session)
                        .cloned();
                    self.last_done = self.last_row.as_ref().and_then(|row| row.turn_id.clone());
                }
                if self.followed.is_none() && self.compacting.is_none() {
                    self.app.end_turn();
                }
                self.report("serve", error);
                return;
            }
        };
        match result {
            TransportResult::Sync(snapshot) => {
                if snapshot.history.is_some() && pending.transcript != self.transcript {
                    self.needs_sync = true;
                    return;
                }
                self.commit_snapshot(snapshot);
            }
            TransportResult::Reconnect { snapshot, status } => {
                if pending.transcript != self.transcript {
                    return;
                }
                self.commit_snapshot(snapshot);
                self.status = Some(status);
                self.connected = true;
                self.app.connected = true;
                self.app.push_system("reconnected to otto serve");
                self.transcript = self.transcript.wrapping_add(1);
                self.app.tick_features();
                self.app.request_approvals();
                self.last_done = None;
                self.fresh = true;
            }
            TransportResult::Prompt { reply, stream } => {
                let sent_turn = stream.as_ref().map(|stream| stream.turn_id.clone());
                if let Some(reply) = reply {
                    self.app.push_system(reply);
                    self.transcript = self.transcript.wrapping_add(1);
                }
                if let Some(stream) = stream {
                    let turn = stream.turn_id.clone();
                    self.remote.state().own.push(turn);
                    if self.connected && self.followed.is_none() {
                        self.adopt_stream(stream);
                    }
                } else if self.followed.is_none() && self.compacting.is_none() {
                    self.app.end_turn();
                }
                if self.cancel_after_send {
                    self.cancel_after_send = false;
                    if let Some(turn) = self
                        .followed
                        .as_ref()
                        .map(|followed| followed.stream.turn_id.clone())
                        .or(sent_turn)
                    {
                        self.cancel_turn(turn);
                    }
                }
            }
            TransportResult::Cancel => {
                self.cancel_after_send = false;
            }
        }
    }

    fn commit_snapshot(&mut self, snapshot: Snapshot) {
        let reload = snapshot.history.is_some();
        {
            let mut state = self.remote.state();
            if let Some(info) = snapshot.info {
                state.info = info;
            }
            if let Some(history) = snapshot.history {
                state.history = history;
            }
        }
        if reload {
            self.app.refresh(&Backend::Attach(self.remote));
            self.transcript = self.transcript.wrapping_add(1);
        } else {
            self.app.refresh_info(&Backend::Attach(self.remote));
        }
        if let Some(tasks) = snapshot.tasks {
            self.app.tasks = tasks;
        }
        self.last_row = snapshot.row;
        self.fresh = false;
        if let Some(id) = snapshot.done {
            self.remote.finish(&id);
            self.last_done = Some(id);
        }
        if let Some(stream) = snapshot.stream {
            self.adopt_stream(stream);
            self.needs_sync = true;
        }
    }

    fn adopt_stream(&mut self, stream: TurnStream) {
        self.remote.state().running = Some(stream.turn_id.clone());
        self.followed = Some(Followed {
            stream,
            error_rendered: false,
        });
        self.app.start_turn();
    }

    async fn drain_transport(&mut self) {
        if let Some(pending) = self.pending.as_mut()
            && pending.mutation
        {
            let result = pending.call.as_mut().await;
            self.on_transport(result);
        }
    }

    fn on_compact(&mut self, result: Result<Option<WireCompaction>, Error>) {
        self.compacting = None;
        self.app.end_turn();
        self.transcript = self.transcript.wrapping_add(1);
        match result {
            Ok(Some(compaction)) => self.app.push_system(compaction_line(&compaction)),
            Ok(None) => self.app.push_system("/compact: canceled"),
            Err(error) => return self.report("/compact", error),
        }
        self.needs_sync = true;
    }

    /// Runs the queued actions; `true` when one asked to exit.
    fn run_actions(&mut self) -> bool {
        while let Some(action) = self.actions.pop_front() {
            if matches!(action, Action::Exit) {
                return true;
            }
            if self.app.busy() && matches!(action, Action::Compact(_) | Action::Reflect(_)) {
                self.actions.push_front(action);
                break;
            }
            if !self.connected {
                self.app.push_system(DISCONNECTED);
                continue;
            }
            if self
                .pending
                .as_ref()
                .is_some_and(|pending| pending.mutation)
            {
                self.actions.push_front(action);
                break;
            }
            self.act(action);
        }
        false
    }

    fn act(&mut self, action: Action) {
        self.transcript = self.transcript.wrapping_add(1);
        let remote = self.remote;
        let session = remote.session();
        match action {
            Action::Prompt(line) => {
                let image = self.app.pending_image.take();
                self.pending = None;
                if self.followed.is_none() && self.compacting.is_none() {
                    self.app.start_turn();
                }
                self.pending_call(
                    true,
                    Box::pin(async move {
                        let mut reply = None;
                        if image.is_none()
                            && let Some(control) =
                                within(remote.client.approval_message(&session, &line)).await?
                        {
                            reply = Some(control.text);
                            if !control.queued {
                                return Ok(TransportResult::Prompt {
                                    reply,
                                    stream: None,
                                });
                            }
                        }
                        let stream =
                            within(remote.client.start_turn(&session, &line, image.as_ref()))
                                .await?;
                        Ok(TransportResult::Prompt {
                            reply,
                            stream: Some(stream),
                        })
                    }),
                );
            }
            Action::Compact(focus) => {
                if self.compacting.is_some() {
                    self.app.push_system("/compact: already running");
                    return;
                }
                self.app.start_turn();
                let client = &remote.client;
                self.compacting = Some(Box::pin(
                    async move { client.compact(&session, &focus).await },
                ));
            }
            Action::CancelTurn => self.cancel_followed(),
            Action::Exit => {}
            Action::Reflect(_) | Action::Login(_) | Action::McpLogin(_) => {
                self.app.push_system("not available with --attach")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn delayed_session_snapshot_keeps_input_and_stream_live_and_cannot_erase_deltas() {
        use axum::response::IntoResponse;
        let started = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let router = axum::Router::new().fallback({
            let started = started.clone();
            let release = release.clone();
            move |uri: axum::http::Uri| {
                let started = started.clone();
                let release = release.clone();
                async move {
                    match uri.path() {
                        "/v1/sessions/s1" => {
                            started.notify_one();
                            release.notified().await;
                            axum::Json(serde_json::json!({"id":"s1", "workspace":"/work"}))
                                .into_response()
                        }
                        "/v1/sessions/s1/tasks" => {
                            axum::Json(serde_json::json!({"tasks":[]})).into_response()
                        }
                        "/v1/sessions/s1/history" => {
                            axum::Json(serde_json::json!([])).into_response()
                        }
                        "/v1/sessions/s1/turns/t1/events" => {
                            let event = WireEvent {
                                event_type: "text_delta".into(),
                                text: "live ".into(),
                                ..WireEvent::default()
                            };
                            let data = serde_json::to_string(&event).unwrap();
                            axum::response::Response::builder()
                                .header("content-type", "text/event-stream")
                                .body(axum::body::Body::from(format!(
                                    "data: {data}\n\ndata: {data}\n\n"
                                )))
                                .unwrap()
                        }
                        _ => axum::http::StatusCode::NOT_FOUND.into_response(),
                    }
                }
            }
        });
        let remote = Remote::new(Client::in_process(router), "/work");
        {
            let mut state = remote.state();
            state.session = "s1".into();
            state.info = Info {
                session_id: "s1".into(),
                workspace: "/work".into(),
                ..Info::default()
            };
        }
        let cancel = CancellationToken::new();
        let mut ui = Ui {
            remote: &remote,
            app: App::new(&Backend::Attach(&remote)),
            cancel: &cancel,
            followed: None,
            status: None,
            rows: vec![row(Some("ok"), Some("old"))],
            last_row: None,
            last_done: None,
            fresh: false,
            compacting: None,
            connected: true,
            pending: None,
            epoch: 0,
            transcript: 0,
            needs_sync: false,
            cancel_after_send: false,
            actions: VecDeque::new(),
        };
        ui.sync();
        tokio::select! {
            result = ui.pending.as_mut().unwrap().call.as_mut() => panic!("snapshot completed before release: {}", result.is_ok()),
            _ = started.notified() => {}
        }
        let stream = remote.client.turn_events("s1", "t1", None).await.unwrap();
        ui.adopt_stream(stream);
        let (keys, mut received) = mpsc::channel(1);
        keys.send(TuiEvent::Paste("draft".into())).await.unwrap();
        let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(80, 24)).unwrap();
        let mut input = false;
        let mut frames = 0;
        while !input || frames < 2 {
            let event = tokio::time::timeout(Duration::from_secs(1), async {
                tokio::select! {
                    result = ui.pending.as_mut().unwrap().call.as_mut() => Ev::Transport(result),
                    event = received.recv(), if !input => Ev::Input(event),
                    frame = ui.followed.as_mut().unwrap().stream.next(), if frames < 2 => Ev::Frame(frame),
                }
            }).await.expect("input and frames must not await the session query");
            match event {
                Ev::Input(Some(event)) => {
                    ui.on_input(&mut terminal, event).unwrap();
                    input = true;
                }
                Ev::Frame(frame) => {
                    let entries = ui.app.entries.len();
                    ui.on_frame(frame);
                    frames += 1;
                    if frames == 2 {
                        assert_eq!(
                            ui.app.entries.len(),
                            entries,
                            "a delta updates an existing entry"
                        );
                    }
                }
                _ => panic!("blocked query completed unexpectedly"),
            }
        }
        assert_eq!(ui.app.composer.input.iter().collect::<String>(), "draft");
        assert!(ui.app.entries.iter().any(|entry| entry.raw == "live live "));
        release.notify_one();
        let result = ui.pending.as_mut().unwrap().call.as_mut().await;
        ui.on_transport(result);
        assert!(
            ui.app.entries.iter().any(|entry| entry.raw == "live live "),
            "stale history must not overwrite received deltas"
        );
        assert!(ui.needs_sync);
    }

    #[tokio::test]
    async fn the_channel_sink_forwards_text_and_fails_when_the_ui_is_gone() {
        let (tx, mut rx) = mpsc::channel(1);
        let sink = ChannelSink(tx);
        sink.deliver(&Origin::terminal(), "⏰ stretch")
            .await
            .unwrap();
        assert_eq!(rx.recv().await.as_deref(), Some("⏰ stretch"));
        drop(rx);
        assert!(sink.deliver(&Origin::terminal(), "x").await.is_err());
    }

    fn row(turn: Option<&str>, turn_id: Option<&str>) -> StatusRow {
        StatusRow {
            id: "s1".into(),
            turn: turn.map(str::to_string),
            turn_id: turn_id.map(str::to_string),
            ..StatusRow::default()
        }
    }

    #[test]
    fn follow_decision_follows_a_running_turn_once() {
        let own = vec!["t-own".to_string()];
        let cases = [
            // Another client's running turn is followed with its history.
            (
                row(Some("running"), Some("t-x")),
                None,
                None,
                Some(Follow::Foreign("t-x".into())),
            ),
            // This process's queued turn that has started running.
            (
                row(Some("running"), Some("t-own")),
                None,
                None,
                Some(Follow::Own("t-own".into())),
            ),
            // Already following a turn.
            (row(Some("running"), Some("t-x")), Some("t-y"), None, None),
            // The turn that just ended can still be named by the row.
            (row(Some("running"), Some("t-x")), None, Some("t-x"), None),
            // Not running.
            (row(Some("queued"), Some("t-own")), None, None, None),
            (row(None, None), None, None, None),
            // A turn that started and ended between two snapshots.
            (
                row(Some("error"), Some("t-new")),
                None,
                Some("t-old"),
                Some(Follow::Reload("t-new".into())),
            ),
            (row(Some("ok"), Some("t-x")), None, Some("t-x"), None),
            (row(Some("ok"), Some("t-x")), Some("t-y"), None, None),
        ];
        for (row, followed, last_done, want) in cases {
            assert_eq!(
                follow_decision(&row, followed, &own, last_done),
                want,
                "{row:?}"
            );
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn withdraw_without_a_queued_own_turn_is_false() {
        let remote = Remote::offline(Vec::new());
        assert!(!remote.withdraw_queued_turn_async().await.expect("withdraw"));
        // The running turn is not withdrawable; only a queued one is.
        {
            let mut state = remote.state();
            state.own.push("t1".into());
            state.running = Some("t1".into());
        }
        assert!(!remote.withdraw_queued_turn_async().await.expect("withdraw"));
    }
}
