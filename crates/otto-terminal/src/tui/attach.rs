//! `otto --attach`: the terminal UI as a client of a running `otto serve`.
//!
//! The process opens no session, runs no model or tool and reads no provider
//! credentials. Every session operation is an HTTP request on serve's Unix
//! socket (see [`otto_client`]); the screen is the shared `App` with
//! `Backend::Attach`.
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
use super::{TerminalInput, TuiEvent, frame_gate, render, spawn_key_reader};
use crate::cli::repl::Error as ReplError;
use otto::app::Info;
use otto::schedule::Scope;
use otto::schedule::worker::{DeliverRole, Sink, Worker};
use otto_client::{Client, Error, StatusRow, StatusStream, TurnStream};
use otto_session::origin::Origin;

mod event_loop;
mod frames;
mod transport;
mod transport_state;
use event_loop::*;
use transport_state::*;

/// Bound on one request. A long call (`/compact`) runs as a future in the
/// loop instead.
const CALL_TIMEOUT: Duration = Duration::from_secs(5);
const RETRY_INTERVAL: Duration = Duration::from_secs(1);
const DISCONNECTED: &str = "disconnected from otto serve";

/// Which session `otto --attach` opens at startup.
pub enum Start {
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
            client: client.with_input_source(otto_core::model::InputSource::Tui),
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

    pub fn history(&self) -> Vec<Message> {
        self.state().history.clone()
    }

    pub(crate) fn workspace(&self) -> &str {
        &self.workspace
    }

    pub(crate) fn session(&self) -> String {
        self.state().session.clone()
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
    pub(crate) fn for_client(client: Client, session: &str) -> Self {
        let remote = Self::new(client, "/work");
        {
            let mut state = remote.state();
            state.session = session.to_string();
            state.info.session_id = session.to_string();
            state.info.workspace = "/work".to_string();
        }
        remote
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
pub async fn run(
    socket: &Path,
    workspace: &str,
    start: Start,
    schedule: Option<Arc<dyn otto::schedule::ScheduleStore>>,
    cancel: &CancellationToken,
    stderr: &mut (dyn Write + Send),
) -> i32 {
    run_with_observer_bindings(
        socket,
        workspace,
        start,
        schedule,
        cancel,
        stderr,
        &Default::default(),
    )
    .await
}

/// Runs attach with client-local observer keys, validated before connecting/raw mode.
pub async fn run_with_observer_bindings(
    socket: &Path,
    workspace: &str,
    start: Start,
    schedule: Option<Arc<dyn otto::schedule::ScheduleStore>>,
    cancel: &CancellationToken,
    stderr: &mut (dyn Write + Send),
    bindings: &otto_core::config::ui::ObserverBindings,
) -> i32 {
    let bindings = match super::observer::bindings::Bindings::parse(bindings) {
        Ok(bindings) => bindings,
        Err(error) => {
            let _ = writeln!(
                stderr,
                "otto: {error}; edit [ui.observer] in your config and restart"
            );
            return 1;
        }
    };
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
    if let Err(error) = input.enable_terminal() {
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
                logger: Arc::new(otto_platform::log::Logger::discard()),
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
        bindings,
    )
    .await;
    // Sends after the loop has exited fail, so those items stay undelivered.
    drop(deliveries);
    worker_cancel.cancel();
    if let Some(worker) = worker {
        let _ = worker.await;
    }
    drop(deliver_tx);
    let _ = input.restore_terminal();
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
    call: TransportCall<'a>,
}

struct Snapshot {
    info: Option<Info>,
    tasks: Option<Vec<otto::subagent::tasks::Task>>,
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
/// Connection and request ownership are explicit; turn following and compaction
/// remain independent because their events are polled concurrently.
struct Ui<'a> {
    remote: &'a Remote,
    app: App,
    cancel: &'a CancellationToken,
    followed: Option<Followed>,
    connection: ConnectionState,
    rows: Vec<StatusRow>,
    /// The row of this session at the last sync, to refresh the sub-agent
    /// panel only when it changes.
    last_row: Option<StatusRow>,
    last_done: Option<String>,
    /// The history was loaded after the last status snapshot, so a final turn
    /// in the next one is already in it.
    fresh: bool,
    compacting: Option<CompactCall<'a>>,
    transport: TransportState<'a>,
    epoch: u64,
    transcript: u64,
    needs_sync: bool,
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
    Draw,
}

#[cfg(test)]
mod tests;
