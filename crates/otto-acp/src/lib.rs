//! The Agent Client Protocol (v1) agent server behind `otto acp`.
//!
//! Transport: newline-delimited JSON-RPC 2.0, frames read from a stdin line
//! reader and written to stdout by one writer that owns the handle. Every
//! outgoing frame (responses, `session/update` notifications and
//! `session/request_permission` requests) goes through one unbounded channel,
//! so frames from concurrent prompts never interleave inside a line and keep
//! the order in which they were queued.
//!
//! Ownership: [`serve`] owns the connection until stdin ends or the process
//! token is cancelled. It then cancels every running prompt, waits for the
//! request tasks, and returns the session controllers; the caller closes them
//! (see `cli/acp.rs`). Local ACP stays in one workspace. Attached ACP forwards
//! each request's workspace to serve for canonicalization and admission.
//!
//! Concurrency: the read loop handles `initialize` and `session/cancel`
//! inline and runs every other method as a task, so a long prompt never
//! blocks reads. Ordinary prompts accept one at a time. `/steer` bypasses the
//! prompt slot, and explicit `/queue` uses a separate bounded FIFO; cancellation
//! clears pending queue entries and cancels active work.

mod approval;
pub mod attach;
mod command;
mod dispatcher;
mod local_session;
mod memory;
mod params;
mod prompt_slot;
mod prompt_turns;
mod schedule;
mod session_lifecycle;
mod session_notices;
mod shared_approval;
pub mod update;

use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};

use agent_client_protocol_schema::ProtocolVersion;
use agent_client_protocol_schema::v1::{
    AgentCapabilities, ContentBlock, ContentChunk, Error, Implementation, InitializeResponse,
    ListSessionsResponse, McpCapabilities, NewSessionResponse, PromptCapabilities, PromptResponse,
    SessionCapabilities, SessionInfo, SessionListCapabilities, SessionNotification, SessionUpdate,
    StopReason, TextContent,
};
use otto_core::config::resolve::Runtime;
use otto_core::model::Message;
use otto_core::wire::events::to_wire_compaction;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::{Notify, mpsc, oneshot};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use otto::app::{
    ActiveInputAdmission, ApprovalDecision, CLOSED, Channel, Controller, PROMPT_ACTIVE, Step, Stop,
};
use otto::runtime::builder::Builder;
use otto::sandbox::control::SandboxControl;
use otto_session::origin::Origin;
use otto_session::{MAX_LIST_SESSIONS, is_session_id, session_directory};

use approval::request_permission;
use local_session::*;
use params::*;

const PARSE_ERROR: i32 = -32700;
const INVALID_REQUEST: i32 = -32600;
const METHOD_NOT_FOUND: i32 = -32601;
const INVALID_PARAMS: i32 = -32602;
const INTERNAL_ERROR: i32 = -32603;
const RESOURCE_NOT_FOUND: i32 = -32002;
const TASK_CONTROL_UNSUPPORTED: &str = "task control is unavailable on the attached otto serve; upgrade and restart the server to enable it";
/// `_otto/memory/*`: memory is disabled or not usable in the session.
const MEMORY_UNAVAILABLE: i32 = -32010;
/// `_otto/memory/review`: the candidate was already decided or changed.
const MEMORY_CONFLICT: i32 = -32011;
const TITLE_CHARS: usize = 80;

/// What [`serve`] needs from the composition root.
pub struct Config {
    pub builder: Arc<Builder>,
    pub runtime: Runtime,
    /// The canonical workspace every request's `cwd` must match.
    pub workspace: PathBuf,
    /// The process sandbox, attached to every controller so it reports live
    /// sandbox state.
    pub sandbox: Option<Arc<dyn SandboxControl>>,
}

type Reply = Result<Value, Error>;

fn error(code: i32, message: impl Into<String>) -> Error {
    Error::new(code, message)
}

fn invalid_params(message: impl Into<String>) -> Error {
    error(INVALID_PARAMS, message)
}

fn unknown_session() -> Error {
    error(RESOURCE_NOT_FOUND, "unknown sessionId")
}

fn task_control_error(failure: otto_client::Error) -> Error {
    match &failure {
        otto_client::Error::Http {
            status: 404, code, ..
        } if code.is_empty() => error(METHOD_NOT_FOUND, TASK_CONTROL_UNSUPPORTED),
        _ => error(INTERNAL_ERROR, failure.to_string()),
    }
}

fn result<T: serde::Serialize>(value: T) -> Reply {
    Ok(serde_json::to_value(value).expect("ACP result serializes"))
}

fn queued_outcome_notice(reply: &Reply) -> Option<String> {
    match reply {
        Ok(value) if value["stopReason"] == "cancelled" => Some(
            "Queued follow-up canceled. Any effects completed before cancellation remain."
                .to_string(),
        ),
        Err(failure) => {
            let message = serde_json::to_value(failure)
                .ok()
                .and_then(|value| value["message"].as_str().map(str::to_string))
                .unwrap_or_else(|| "unknown error".to_string());
            Some(format!("Queued follow-up failed: {message}"))
        }
        _ => None,
    }
}

/// Session-local prompt admission, explicit queue, and attached-turn handles.
const ACP_QUEUE_LIMIT: usize = 16;

struct QueuedPrompt {
    text: String,
    channel: Option<Channel>,
    origin: Option<Origin>,
    /// The input id of its saved copy in the session's turn queue; its user
    /// message carries it.
    input_id: Option<String>,
    input_provenance: Option<otto_core::model::InputProvenance>,
}

#[derive(Default)]
enum PromptActivity {
    #[default]
    Idle,
    Prompt(CancellationToken),
    Wake(CancellationToken),
}

#[derive(Default)]
struct PromptState {
    activity: PromptActivity,
    queue: std::collections::VecDeque<QueuedPrompt>,
    draining: bool,
    /// An ordinary prompt waiting for a wake turn to end.
    waiting: Option<CancellationToken>,
    followups: HashMap<u64, CancellationToken>,
    next_followup: u64,
    next_followup_admission: u64,
}

#[derive(Default)]
struct PromptSlot {
    state: Mutex<PromptState>,
    changed: Notify,
    followup_changed: Notify,
    /// Counts releases of the slot, for the wake loop. Separate from
    /// `changed`, whose permits belong to the `/queue` worker.
    released: tokio::sync::watch::Sender<u64>,
}

/// One loaded session: its controller and ACP task/queue state.
struct Session {
    controller: Arc<Controller>,
    prompt: Arc<PromptSlot>,
}

/// The sessions this process runs itself.
struct Local {
    config: Config,
    sessions: Mutex<HashMap<String, Arc<Session>>>,
}

/// Where a connection's session operations go: this process (`otto acp`) or
/// an `otto serve` (`otto acp --attach`).
enum Backend {
    Local(Box<Local>),
    Attach(attach::Relay),
}

/// State shared by the read loop and its request tasks.
struct NoticeWatch {
    controller: Weak<Controller>,
    stop: CancellationToken,
}

struct Connection {
    client_capabilities: Mutex<Value>,
    /// Local requests must match this startup workspace; attached lists use
    /// it only when their request omits `cwd`.
    workspace: PathBuf,
    backend: Backend,
    out: mpsc::UnboundedSender<Value>,
    /// Requests sent to the client, by id, waiting for their response frame.
    pending: Mutex<HashMap<u64, oneshot::Sender<Value>>>,
    next_request_id: AtomicU64,
    task_stop: CancellationToken,
    notice_stop: CancellationToken,
    watching_notices: Mutex<HashMap<String, NoticeWatch>>,
    notice_ready: Mutex<HashMap<String, tokio::sync::watch::Sender<bool>>>,
    /// One wake loop per local session; awaited before `serve` returns.
    wake_loops: Mutex<HashMap<String, tokio::task::JoinHandle<()>>>,
}

fn acp_input_provenance(capabilities: &Value) -> otto_core::model::InputProvenance {
    let claim = capabilities
        .pointer("/clientCapabilities/_meta/otto/inputSource")
        .and_then(|value| {
            serde_json::from_value::<otto_core::model::InputSource>(value.clone()).ok()
        });
    otto_core::model::InputProvenance::remote(otto_core::model::InputTransport::Acp, claim)
}

impl Connection {
    fn input_provenance(&self) -> otto_core::model::InputProvenance {
        let capabilities = self.client_capabilities.lock().expect("ACP capabilities");
        acp_input_provenance(&capabilities)
    }

    fn queued_input_provenance(&self) -> otto_core::model::InputProvenance {
        let mut origin = self.input_provenance();
        origin.delivery = otto_core::model::InputDelivery::Queued;
        origin
    }

    fn input_client(&self, client: &otto_client::Client) -> otto_client::Client {
        client.clone().with_input_source(
            self.input_provenance()
                .client_source
                .unwrap_or(otto_core::model::InputSource::Acp),
        )
    }

    fn send(&self, frame: Value) {
        // The writer ends only after every sender is dropped, so a failed
        // send cannot happen while the connection is alive.
        let _ = self.out.send(frame);
    }

    fn reply(&self, id: Value, reply: Reply) {
        self.send(match reply {
            Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
            Err(error) => json!({"jsonrpc": "2.0", "id": id, "error": error}),
        });
    }

    fn update(&self, session_id: &str, update: SessionUpdate) {
        let notification = SessionNotification::new(session_id.to_string(), update);
        self.send(json!({"jsonrpc": "2.0", "method": "session/update", "params": notification}));
    }

    /// Sends a request to the client; the response frame arrives on the
    /// returned receiver.
    fn send_request(&self, method: &str, params: Value) -> (u64, oneshot::Receiver<Value>) {
        let id = self.next_request_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().expect("pending map").insert(id, tx);
        self.send(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        (id, rx)
    }

    fn forget_request(&self, id: u64) {
        self.pending.lock().expect("pending map").remove(&id);
    }

    /// Routes a response frame from the client; unknown ids are dropped.
    fn client_response(&self, frame: Value) {
        let Some(id) = frame.get("id").and_then(Value::as_u64) else {
            return;
        };
        if let Some(waiter) = self.pending.lock().expect("pending map").remove(&id) {
            let _ = waiter.send(frame);
        }
    }
}

/// The parameters `session/new` and `session/load` share, parsed leniently so
/// that the checks below see what the client actually sent.
#[derive(Deserialize)]
struct SessionParams {
    cwd: Option<String>,
    #[serde(rename = "sessionId")]
    session_id: Option<String>,
    #[serde(rename = "mcpServers", default)]
    mcp_servers: Vec<Value>,
}

impl SessionParams {
    fn parse(params: Value) -> Result<Self, Error> {
        serde_json::from_value(params).map_err(|failure| invalid_params(failure.to_string()))
    }

    fn check_mcp_servers(&self) -> Result<(), Error> {
        if self.mcp_servers.is_empty() {
            Ok(())
        } else {
            Err(invalid_params(
                "mcpServers must be empty; configure MCP servers in otto's config file",
            ))
        }
    }
}

/// The read loop's state: the shared connection, the request tasks, and the
/// token every prompt token descends from.
struct Dispatcher {
    connection: Arc<Connection>,
    /// Set once the schedule worker of this connection has been started.
    scheduling: bool,
    tasks: JoinSet<()>,
    stop: CancellationToken,
}

/// Writes one frame per line and flushes after each. A write error is
/// remembered and later frames are dropped, but the channel keeps draining so
/// senders never block.
async fn write_frames(stdout: &mut (dyn Write + Send), mut frames: mpsc::UnboundedReceiver<Value>) {
    let mut healthy = true;
    while let Some(frame) = frames.recv().await {
        if healthy {
            healthy = writeln!(stdout, "{frame}")
                .and_then(|()| stdout.flush())
                .is_ok();
        }
    }
}

/// Reads stdin lines on a dedicated thread. Invalid UTF-8 is replaced rather
/// than ending the connection. The thread ends at EOF or a read error, which
/// closes the channel.
fn read_lines(mut stdin: Box<dyn BufRead + Send>) -> mpsc::UnboundedReceiver<String> {
    let (tx, rx) = mpsc::unbounded_channel();
    std::thread::spawn(move || {
        let mut buffer = Vec::new();
        loop {
            buffer.clear();
            match stdin.read_until(b'\n', &mut buffer) {
                Ok(0) | Err(_) => return,
                Ok(_) => {
                    if tx
                        .send(String::from_utf8_lossy(&buffer).into_owned())
                        .is_err()
                    {
                        return;
                    }
                }
            }
        }
    });
    rx
}

/// Serves ACP until stdin ends or `cancel` fires, then returns the session
/// controllers for the caller to close. Every running prompt is answered with
/// `stopReason: cancelled` before the writer is released.
pub async fn serve(
    config: Config,
    stdin: Box<dyn BufRead + Send>,
    stdout: &mut (dyn Write + Send),
    cancel: &CancellationToken,
) -> Vec<Arc<Controller>> {
    let workspace = config.workspace.clone();
    let workflow_builder = Arc::clone(&config.builder);
    let local = Local {
        config,
        sessions: Mutex::new(HashMap::new()),
    };
    let controllers = drive(
        workspace,
        Backend::Local(Box::new(local)),
        stdin,
        stdout,
        &CancellationToken::new(),
        cancel,
        |connection| match &connection.backend {
            Backend::Local(local) => local
                .sessions
                .lock()
                .expect("session map")
                .values()
                .map(|session| Arc::clone(&session.controller))
                .collect(),
            Backend::Attach(_) => Vec::new(),
        },
    )
    .await;
    otto::runtime::workflow::close(&workflow_builder).await;
    controllers
}

/// Runs one connection until stdin ends, `cancel` fires or `lost` fires, and
/// returns what `collect` takes from the connection after every request task
/// has finished. `lost` is the attach relay's signal that serve is gone; the
/// request tasks answer their requests with an error before the writer is
/// released.
async fn drive<T>(
    workspace: PathBuf,
    backend: Backend,
    stdin: Box<dyn BufRead + Send>,
    stdout: &mut (dyn Write + Send),
    lost: &CancellationToken,
    cancel: &CancellationToken,
    collect: impl FnOnce(&Connection) -> T,
) -> T {
    let (out, frames) = mpsc::unbounded_channel();
    let notice_stop = CancellationToken::new();
    let task_stop = CancellationToken::new();
    let connection = Arc::new(Connection {
        workspace,
        backend,
        out,
        pending: Mutex::new(HashMap::new()),
        next_request_id: AtomicU64::new(1),
        client_capabilities: Mutex::new(Value::Null),
        task_stop: task_stop.clone(),
        notice_ready: Mutex::new(HashMap::new()),
        notice_stop: notice_stop.clone(),
        watching_notices: Mutex::new(HashMap::new()),
        wake_loops: Mutex::new(HashMap::new()),
    });
    let mut lines = read_lines(stdin);
    let read_loop = async {
        let mut dispatcher = Dispatcher {
            connection: Arc::clone(&connection),
            scheduling: false,
            tasks: JoinSet::new(),
            stop: task_stop,
        };
        loop {
            tokio::select! {
                line = lines.recv() => match line {
                    Some(line) => dispatcher.handle_line(&line),
                    None => break,
                },
                () = cancel.cancelled() => break,
                () = lost.cancelled() => break,
                Some(finished) = dispatcher.tasks.join_next() => {
                    if let Err(failure) = finished {
                        eprintln!("acp: request task failed: {failure}");
                    }
                }
            }
        }
        dispatcher.stop.cancel();
        connection.notice_stop.cancel();
        while dispatcher.tasks.join_next().await.is_some() {}
        let wake_loops: Vec<_> = connection
            .wake_loops
            .lock()
            .expect("wake loops")
            .drain()
            .map(|(_, handle)| handle)
            .collect();
        for handle in wake_loops {
            let _ = handle.await;
        }
        let collected = collect(&connection);
        // Dropping the dispatcher and the connection drops the last sender,
        // which ends the writer.
        drop(dispatcher);
        drop(connection);
        collected
    };
    let (collected, ()) = tokio::join!(read_loop, write_frames(stdout, frames));
    collected
}

#[cfg(test)]
mod channel_tests;
#[cfg(test)]
mod prompt_slot_tests;
