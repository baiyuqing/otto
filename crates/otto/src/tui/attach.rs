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
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use crossterm::event::KeyEvent;
use otto_core::agent::context_report::ContextReport;
use otto_core::model::{Block, Message};
use otto_core::wire::events::{TURN_END, WireCompaction, WireEvent};
use ratatui::Terminal;
use ratatui::backend::Backend as TerminalBackend;
use tokio::runtime::Handle;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use super::app::{Action, App, Backend, PickerRow, compaction_line};
use super::{TerminalInput, TuiEvent, agents_view, render, selection, spawn_key_reader};
use crate::app::Info;
use crate::cli::repl::Error as ReplError;
use crate::client::{Client, Error, StatusRow, StatusStream, TurnStream};
use crate::subagent::record::{ListQuery, ListResult, TaskRow};

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
/// handlers from cached state or one bounded blocking request.
pub(crate) struct Remote {
    client: Client,
    handle: Handle,
    workspace: String,
    state: Mutex<State>,
}

impl Remote {
    fn new(client: Client, workspace: &str) -> Self {
        Self {
            client,
            handle: Handle::current(),
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

    fn session(&self) -> String {
        self.state().session.clone()
    }

    /// Runs one request to completion from synchronous code. The request runs
    /// on the runtime from a helper thread, because the caller is inside the
    /// runtime's `block_on` and cannot start another.
    fn block<T: Send>(
        &self,
        call: impl Future<Output = Result<T, Error>> + Send,
    ) -> Result<T, String> {
        std::thread::scope(|scope| {
            scope
                .spawn(|| self.handle.block_on(within(call)))
                .join()
                .unwrap_or_else(|_| Err(Error::Unreachable("request panicked".to_string())))
        })
        .map_err(|error| error.to_string())
    }

    pub(crate) fn tasks_list(&self, query: &ListQuery) -> Result<ListResult, String> {
        self.block(self.client.tasks(query))
    }

    pub(crate) fn tasks_get(&self, parent: &str, id: &str) -> Result<Option<TaskRow>, String> {
        self.block(self.client.task(parent, id))
    }

    pub(crate) fn context_report(&self) -> Result<ContextReport, String> {
        self.block(self.client.context(&self.session()))
    }

    pub(crate) fn rename_session(&self, name: &str) -> Result<(), String> {
        let session = self.session();
        self.block(self.client.rename_session(&session, name))?;
        let info = self.block(self.client.session(&session))?;
        self.state().info = info;
        Ok(())
    }

    /// Cancels the newest turn this process queued that has not started
    /// running. `false` when there is none.
    pub(crate) fn withdraw_queued_turn(&self) -> bool {
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
        let Some(turn) = turn else { return false };
        if self
            .block(self.client.cancel_turn(&session, &turn))
            .is_err()
        {
            return false;
        }
        self.state().own.retain(|id| *id != turn);
        true
    }

    /// The `/resume` rows of this workspace; the value is the session id.
    pub(crate) fn session_rows(&self, limit: usize) -> Result<Vec<PickerRow>, String> {
        let rows = self.block(self.client.list_sessions(&self.workspace))?;
        let current = self.session();
        Ok(rows
            .into_iter()
            .take(limit)
            .map(|row| {
                let label = if row.name.is_empty() {
                    &row.id
                } else {
                    &row.name
                };
                let marker = if row.id == current { " (current)" } else { "" };
                PickerRow {
                    label: format!("{label}{marker}"),
                    value: row.id,
                }
            })
            .collect())
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
    async fn open(&self, workspace: Option<&str>, resume: Option<&str>) -> Result<(), Error> {
        let id = within(self.client.open_session(workspace, resume)).await?;
        self.load(&id, None).await?;
        let mut state = self.state();
        state.own.clear();
        state.running = None;
        Ok(())
    }

    async fn refresh_info(&self) -> Result<(), Error> {
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
    let result = run_loop(&mut terminal, &remote, status, cancel, &mut keys).await;
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

type CompactCall<'a> =
    Pin<Box<dyn Future<Output = Result<Option<WireCompaction>, Error>> + Send + 'a>>;

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
    pending_image: Option<Block>,
    actions: VecDeque<Action>,
}

// ponytail: one short-lived value per loop turn; boxing the frame is not worth it.
#[allow(clippy::large_enum_variant)]
enum Ev {
    Input(Option<TuiEvent>),
    Frame(Result<Option<WireEvent>, Error>),
    Status(Result<Option<Vec<StatusRow>>, Error>),
    Compact(Result<Option<WireCompaction>, Error>),
    Retry,
    AgentsTick,
    Tick,
}

async fn run_loop<B: TerminalBackend>(
    terminal: &mut Terminal<B>,
    remote: &Remote,
    status: StatusStream,
    cancel: &CancellationToken,
    keys: &mut mpsc::Receiver<TuiEvent>,
) -> Result<(), ReplError> {
    let app = App::new(&Backend::Attach(remote));
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
        pending_image: None,
        actions: VecDeque::new(),
    };
    let mut retry = tokio::time::interval(RETRY_INTERVAL);
    let mut agents = tokio::time::interval(agents_view::REFRESH_INTERVAL);
    let mut frames = tokio::time::interval(render::SPINNER_FRAME);
    draw(terminal, &mut ui.app)?;
    loop {
        let event = {
            let followed = ui.followed.as_mut();
            let status = ui.status.as_mut();
            let compacting = ui.compacting.as_mut();
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
            let reconnect = async {
                if connected {
                    std::future::pending::<()>().await;
                }
                retry.tick().await;
            };
            tokio::select! {
                _ = cancel.cancelled() => return Err(ReplError::Cancelled),
                event = keys.recv() => Ev::Input(event),
                result = frame => Ev::Frame(result),
                result = snapshot => Ev::Status(result),
                result = compaction => Ev::Compact(result),
                () = reconnect => Ev::Retry,
                _ = agents.tick() => Ev::AgentsTick,
                _ = frames.tick() => Ev::Tick,
            }
        };
        match event {
            Ev::Input(None) => return Ok(()),
            Ev::Input(Some(event)) => ui.on_input(terminal, event).await?,
            Ev::Frame(result) => ui.on_frame(result).await,
            Ev::Status(result) => ui.on_status(result).await,
            Ev::Compact(result) => ui.on_compact(result).await,
            Ev::Retry => ui.reconnect().await,
            Ev::AgentsTick => {
                if let Some(view) = &mut ui.app.agents {
                    view.tick(&Backend::Attach(remote));
                }
            }
            Ev::Tick => {
                // Idle frames need no redraw unless something on screen moves.
                if !ui.app.busy()
                    && ui.app.agents.is_none()
                    && !render::needs_task_clock(&ui.app.tasks)
                {
                    continue;
                }
            }
        }
        if ui.run_actions().await {
            return Ok(());
        }
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
        self.followed = None;
        self.status = None;
        self.compacting = None;
        self.rows.clear();
        self.last_row = None;
        self.app.end_turn();
        self.app.approval = None;
        self.app.push_system(DISCONNECTED);
    }

    /// Resumes the same session on serve, reloads it and follows what runs.
    async fn reconnect(&mut self) {
        let Ok(status) = within(self.remote.client.status()).await else {
            return;
        };
        let session = self.remote.session();
        if self.remote.open(None, Some(&session)).await.is_err() {
            return;
        }
        self.app.refresh(&Backend::Attach(self.remote));
        self.app.push_system("reconnected to otto serve");
        self.status = Some(status);
        self.connected = true;
        self.last_done = None;
        self.fresh = true;
        self.refresh_tasks().await;
    }

    async fn on_input<B: TerminalBackend>(
        &mut self,
        terminal: &mut Terminal<B>,
        event: TuiEvent,
    ) -> Result<(), ReplError> {
        match event {
            TuiEvent::Key(key) => self.on_key(key).await,
            TuiEvent::Paste(text) => self.app.insert_text(&text),
            TuiEvent::Wheel { up } => self.app.scroll_wheel(up),
            TuiEvent::Select { phase, col, row } => {
                if let Some(text) =
                    super::apply_selection(&mut self.app, terminal, phase, col, row)?
                    && let Err(error) = selection::copy(&text)
                {
                    self.app.push_system(format!("copy: {error}"));
                }
            }
            TuiEvent::Redraw => {}
        }
        Ok(())
    }

    async fn on_key(&mut self, key: KeyEvent) {
        let backend = Backend::Attach(self.remote);
        self.app.selection = None;
        let dialog = self.app.approval.is_some()
            || self.app.picker.is_some()
            || self.app.context.is_some()
            || self.app.show_help;
        if self.app.busy() && !dialog {
            if self.app.handle_turn_key(key, &backend) {
                self.cancel_followed().await;
            }
        } else if let Some(action) = self.app.handle_key(key, &backend, self.cancel) {
            self.actions.push_back(action);
        }
        self.actions.extend(self.app.outgoing.drain(..));
    }

    /// Esc: cancels the followed turn. A `/compact` has no turn to cancel.
    async fn cancel_followed(&mut self) {
        let Some(turn) = self.followed.as_ref().map(|f| f.stream.turn_id.clone()) else {
            return;
        };
        let session = self.remote.session();
        if let Err(error) = within(self.remote.client.cancel_turn(&session, &turn)).await {
            self.report("cancel", error);
        }
    }

    async fn on_frame(&mut self, result: Result<Option<WireEvent>, Error>) {
        match result {
            Ok(Some(event)) if event.event_type == TURN_END => self.end_turn(&event).await,
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

    async fn end_turn(&mut self, event: &WireEvent) {
        let Some(followed) = self.followed.take() else {
            return;
        };
        let id = followed.stream.turn_id;
        self.remote.finish(&id);
        self.last_done = Some(id);
        self.app.end_turn();
        match event.status.as_str() {
            "error" if !followed.error_rendered => {
                let message = if event.error.is_empty() {
                    "turn failed"
                } else {
                    &event.error
                };
                self.app.push_system(message);
            }
            "canceled" => self.app.push_system("turn canceled"),
            _ => {}
        }
        self.refresh_info().await;
        if self.app.queued_input.is_some() {
            let backend = Backend::Attach(self.remote);
            if let Some(action) = self.app.submit_queued_input(&backend, self.cancel) {
                self.actions.push_back(action);
            }
        }
        self.sync().await;
    }

    async fn refresh_info(&mut self) {
        match self.remote.refresh_info().await {
            Ok(()) => self.app.refresh_info(&Backend::Attach(self.remote)),
            Err(error) => self.report("session", error),
        }
    }

    async fn refresh_tasks(&mut self) {
        let session = self.remote.session();
        match within(self.remote.client.session_tasks(&session)).await {
            Ok(tasks) => self.app.tasks = tasks,
            Err(error @ Error::Unreachable(_)) => self.report("tasks", error),
            Err(_) => {}
        }
    }

    async fn on_status(&mut self, result: Result<Option<Vec<StatusRow>>, Error>) {
        match result {
            Ok(Some(rows)) => {
                self.rows = rows;
                self.sync().await;
            }
            Ok(None) | Err(_) => self.disconnect(),
        }
    }

    /// Applies the current status row of this session: refreshes the
    /// sub-agent panel when the row changed and follows a newly running turn.
    async fn sync(&mut self) {
        let session = self.remote.session();
        let row = self.rows.iter().find(|row| row.id == session).cloned();
        let Some(row) = row else { return };
        if self.last_row.as_ref() != Some(&row) {
            self.last_row = Some(row.clone());
            self.refresh_tasks().await;
        }
        let followed = self.followed.as_ref().map(|f| f.stream.turn_id.as_str());
        match follow_decision(
            &row,
            followed,
            &self.remote.own(),
            self.last_done.as_deref(),
        ) {
            Some(Follow::Own(id)) => self.follow(&id, false).await,
            Some(Follow::Foreign(id)) => self.follow(&id, true).await,
            Some(Follow::Reload(id)) => {
                self.last_done = Some(id.clone());
                self.remote.finish(&id);
                if !self.fresh {
                    match self.remote.load(&session, None).await {
                        Ok(()) => self.app.refresh(&Backend::Attach(self.remote)),
                        Err(error) => self.report("history", error),
                    }
                }
            }
            None => {}
        }
        self.fresh = false;
    }

    /// Starts following `turn`. A turn another client started is preceded by
    /// the history that existed when it began, so its prompt is not shown
    /// twice.
    async fn follow(&mut self, turn: &str, foreign: bool) {
        let session = self.remote.session();
        if foreign {
            if let Err(error) = self.remote.load(&session, Some(turn)).await {
                self.last_done = Some(turn.to_string());
                return self.report("history", error);
            }
            self.app.refresh(&Backend::Attach(self.remote));
        }
        match within(self.remote.client.turn_events(&session, turn, None)).await {
            Ok(stream) => {
                self.remote.state().running = Some(turn.to_string());
                self.followed = Some(Followed {
                    stream,
                    error_rendered: false,
                });
                self.app.start_turn();
            }
            Err(error) => {
                self.last_done = Some(turn.to_string());
                self.report("turn", error);
            }
        }
    }

    async fn on_compact(&mut self, result: Result<Option<WireCompaction>, Error>) {
        self.compacting = None;
        self.app.end_turn();
        match result {
            Ok(Some(compaction)) => self.app.push_system(compaction_line(&compaction)),
            Ok(None) => self.app.push_system("/compact: canceled"),
            Err(error) => return self.report("/compact", error),
        }
        self.refresh_info().await;
    }

    /// Runs the queued actions; `true` when one asked to exit.
    async fn run_actions(&mut self) -> bool {
        while let Some(action) = self.actions.pop_front() {
            if matches!(action, Action::Exit) {
                return true;
            }
            if !self.connected {
                self.app.push_system(DISCONNECTED);
                continue;
            }
            self.act(action).await;
        }
        false
    }

    async fn act(&mut self, action: Action) {
        let remote = self.remote;
        let session = remote.session();
        match action {
            Action::Prompt(line) => {
                let image = self.pending_image.take();
                match within(remote.client.start_turn(&session, &line, image.as_ref())).await {
                    Ok(stream) => {
                        // Dropping the response does not cancel the turn; its
                        // events are read with `turn_events`.
                        let turn = stream.turn_id.clone();
                        drop(stream);
                        remote.state().own.push(turn.clone());
                        if self.followed.is_none() {
                            self.follow(&turn, false).await;
                        }
                    }
                    Err(error) => self.report("prompt", error),
                }
            }
            Action::Image(path) => match super::image_block_from_path(&path) {
                Ok(image) => {
                    self.pending_image = Some(image);
                    self.app.push_system(format!("Attached image: {path}"));
                }
                Err(message) => self.app.push_system(format!("/image: {message}")),
            },
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
            Action::NewSession => {
                self.switch(Some(&remote.workspace), None, "/new").await;
            }
            Action::Resume(id) => self.switch(None, Some(&id), "/resume").await,
            Action::SandboxReload => match within(remote.client.reload_sandbox()).await {
                Ok(summary) => {
                    self.app.push_system(format!("Sandbox: {summary}"));
                    self.refresh_info().await;
                }
                Err(error) => self.report("/sandbox reload", error),
            },
            Action::Approve(id) => self.decide(&session, &id, true).await,
            Action::Deny(id) => self.decide(&session, &id, false).await,
            // The other actions come from commands that print
            // `not available with --attach` instead of raising them.
            _ => {}
        }
    }

    async fn decide(&mut self, session: &str, id: &str, allow: bool) {
        if let Err(error) = within(self.remote.client.decide_approval(session, id, allow)).await {
            self.report("/approve", error);
        }
    }

    async fn switch(&mut self, workspace: Option<&str>, resume: Option<&str>, what: &str) {
        if let Err(error) = self.remote.open(workspace, resume).await {
            return self.report(what, error);
        }
        self.pending_image = None;
        self.followed = None;
        self.last_done = None;
        self.fresh = true;
        self.last_row = None;
        self.app.end_turn();
        self.app.refresh(&Backend::Attach(self.remote));
        self.app
            .push_system(format!("Session: {}", self.remote.session()));
        self.sync().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert!(!remote.withdraw_queued_turn());
        // The running turn is not withdrawable; only a queued one is.
        {
            let mut state = remote.state();
            state.own.push("t1".into());
            state.running = Some("t1".into());
        }
        assert!(!remote.withdraw_queued_turn());
    }
}
