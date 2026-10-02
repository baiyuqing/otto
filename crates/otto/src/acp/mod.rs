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
//! (see `cli/acp.rs`). One workspace per process: `cwd` in every request must
//! canonicalize to the configured workspace.
//!
//! Concurrency: the read loop handles `initialize` and `session/cancel`
//! inline and runs every other method as a task, so a long prompt never
//! blocks reads. A session accepts one prompt at a time; its cancellation
//! token is installed by the read loop before the prompt task starts, so a
//! `session/cancel` sent right after `session/prompt` always reaches it.
//!
//! Errors: see the table in `docs/specs/2026-10-02-acp-agent-server.md`.

mod approval;
pub mod attach;
mod memory;
pub mod update;

use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use agent_client_protocol_schema::ProtocolVersion;
use agent_client_protocol_schema::v1::{
    AgentCapabilities, ContentBlock, Error, Implementation, InitializeResponse,
    ListSessionsResponse, McpCapabilities, NewSessionResponse, PromptCapabilities, PromptResponse,
    SessionCapabilities, SessionInfo, SessionListCapabilities, SessionNotification, SessionUpdate,
    StopReason,
};
use otto_core::config::resolve::Runtime;
use otto_core::model::Message;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use crate::app::{Controller, SandboxControl, Step, Stop};
use crate::cli::runtime_builder::Builder;
use crate::session::{MAX_LIST_SESSIONS, is_session_id, session_directory};

use approval::request_permission;

const PARSE_ERROR: i32 = -32700;
const INVALID_REQUEST: i32 = -32600;
const METHOD_NOT_FOUND: i32 = -32601;
const INVALID_PARAMS: i32 = -32602;
const INTERNAL_ERROR: i32 = -32603;
const RESOURCE_NOT_FOUND: i32 = -32002;
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

fn result<T: serde::Serialize>(value: T) -> Reply {
    Ok(serde_json::to_value(value).expect("ACP result serializes"))
}

/// The token of a session's running prompt.
#[derive(Default)]
struct PromptSlot(Mutex<Option<CancellationToken>>);

impl PromptSlot {
    /// Installs the token of a new prompt; `None` while one is running.
    fn begin(&self, parent: &CancellationToken) -> Option<CancellationToken> {
        let mut slot = self.0.lock().expect("prompt slot");
        if slot.is_some() {
            return None;
        }
        let token = parent.child_token();
        *slot = Some(token.clone());
        Some(token)
    }

    fn end(&self) {
        *self.0.lock().expect("prompt slot") = None;
    }

    fn cancel(&self) {
        if let Some(token) = self.0.lock().expect("prompt slot").as_ref() {
            token.cancel();
        }
    }
}

/// One loaded session: its controller and the token of the running prompt.
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
struct Connection {
    /// The canonical workspace every request's `cwd` must match.
    workspace: PathBuf,
    backend: Backend,
    out: mpsc::UnboundedSender<Value>,
    /// Requests sent to the client, by id, waiting for their response frame.
    pending: Mutex<HashMap<u64, oneshot::Sender<Value>>>,
    next_request_id: AtomicU64,
}

impl Connection {
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

    /// The prompt slot of a session this connection has opened.
    fn slot(&self, id: &str) -> Option<Arc<PromptSlot>> {
        match &self.backend {
            Backend::Local(local) => local.session(id).map(|session| Arc::clone(&session.prompt)),
            Backend::Attach(relay) => relay.slot(id),
        }
    }

    /// Accepts `cwd` only when it canonicalizes to the process workspace.
    fn check_cwd(&self, cwd: Option<&str>) -> Result<(), Error> {
        let cwd = cwd.ok_or_else(|| invalid_params("cwd is required"))?;
        if !Path::new(cwd).is_absolute() {
            return Err(invalid_params("cwd must be an absolute path"));
        }
        match crate::cli::sandbox_runtime::canonical_directory(Path::new(cwd)) {
            Ok(canonical) if canonical == self.workspace => Ok(()),
            _ => Err(invalid_params(format!(
                "cwd must be the workspace {}",
                self.workspace.display()
            ))),
        }
    }

    async fn new_session(&self, params: Value) -> Reply {
        let params = SessionParams::parse(params)?;
        self.check_cwd(params.cwd.as_deref())?;
        params.check_mcp_servers()?;
        match &self.backend {
            Backend::Local(local) => local.new_session().await,
            Backend::Attach(relay) => relay.new_session(&self.workspace).await,
        }
    }

    async fn load_session(&self, params: Value) -> Reply {
        let params = SessionParams::parse(params)?;
        self.check_cwd(params.cwd.as_deref())?;
        params.check_mcp_servers()?;
        let id = params.session_id.unwrap_or_default();
        if !is_session_id(&id) {
            return Err(invalid_params(
                "sessionId must be 32 lowercase hexadecimal characters",
            ));
        }
        let history = match &self.backend {
            Backend::Local(local) => local.load_session(&id, &self.workspace).await?,
            Backend::Attach(relay) => relay.load_session(&id, &self.workspace).await?,
        };
        for update in update::history_updates(&history) {
            self.update(&id, update);
        }
        Ok(json!({ "sessionId": id }))
    }

    /// A `_otto/memory/*` request. Only the local backend serves them: the
    /// controller that owns the memory service lives in this process.
    fn memory_request(&self, method: &str, params: Value) -> Reply {
        let Backend::Local(local) = &self.backend else {
            return Err(error(METHOD_NOT_FOUND, format!("unknown method {method}")));
        };
        let session_of = |id: &str| local.session(id).ok_or_else(unknown_session);
        if method == memory::PENDING_METHOD {
            let params: memory::PendingParams = serde_json::from_value(params)
                .map_err(|failure| invalid_params(failure.to_string()))?;
            memory::pending(&session_of(&params.session_id)?.controller)
        } else {
            let params: memory::ReviewParams = serde_json::from_value(params)
                .map_err(|failure| invalid_params(failure.to_string()))?;
            memory::review(&session_of(&params.session_id)?.controller, &params)
        }
    }

    async fn list_sessions(&self, params: Value) -> Reply {
        self.check_cwd(params.get("cwd").and_then(Value::as_str))?;
        let sessions = match &self.backend {
            Backend::Local(local) => local.list_sessions().await?,
            Backend::Attach(relay) => relay.list_sessions(&self.workspace).await?,
        };
        result(ListSessionsResponse::new(sessions))
    }

    /// Runs one prompt to its stop reason.
    async fn prompt(&self, session_id: &str, text: String, cancel: &CancellationToken) -> Reply {
        match &self.backend {
            Backend::Local(local) => {
                let session = local.session(session_id).ok_or_else(unknown_session)?;
                self.local_prompt(local, &session, session_id, text, cancel)
                    .await
            }
            Backend::Attach(relay) => relay.prompt(self, session_id, &text, cancel).await,
        }
    }

    /// Runs one local prompt, including the approval retries.
    async fn local_prompt(
        &self,
        local: &Local,
        session: &Session,
        session_id: &str,
        text: String,
        cancel: &CancellationToken,
    ) -> Reply {
        let outcome = session
            .controller
            .run_with_approvals(
                Step::Text(&text),
                &mut |event| {
                    if let Some(update) = update::event_update(&update::local_wire(&event)) {
                        self.update(session_id, update);
                    }
                },
                cancel,
                |request| async move {
                    request_permission(
                        self,
                        session_id,
                        &request.tool_call_id,
                        &request.command,
                        cancel,
                    )
                    .await
                },
            )
            .await;
        match outcome {
            Ok(Stop::EndTurn) => result(PromptResponse::new(StopReason::EndTurn)),
            Ok(Stop::Cancelled) => result(PromptResponse::new(StopReason::Cancelled)),
            Err(failure) => Err(error(INTERNAL_ERROR, local.redact(&failure.to_string()))),
        }
    }
}

impl Local {
    fn session(&self, id: &str) -> Option<Arc<Session>> {
        self.sessions.lock().expect("session map").get(id).cloned()
    }

    fn redact(&self, message: &str) -> String {
        self.config
            .builder
            .redact_error(message, Some(&self.config.runtime))
    }

    fn wire(&self, controller: Controller) -> Controller {
        match &self.config.sandbox {
            Some(control) => controller.with_sandbox_control(Arc::clone(control)),
            None => controller,
        }
    }

    fn register(&self, controller: Controller) -> (String, Arc<Session>) {
        let controller = Arc::new(self.wire(controller));
        let id = controller.info().session_id;
        let session = Arc::new(Session {
            controller,
            prompt: Arc::default(),
        });
        self.sessions
            .lock()
            .expect("session map")
            .insert(id.clone(), Arc::clone(&session));
        (id, session)
    }

    async fn new_session(&self) -> Reply {
        let controller = Controller::create(Arc::clone(&self.config.builder), &self.config.runtime)
            .await
            .map_err(|message| error(INTERNAL_ERROR, self.redact(&message)))?;
        let (id, _) = self.register(controller);
        result(NewSessionResponse::new(id))
    }

    /// Loads the session if this process has not, and returns its history.
    async fn load_session(&self, id: &str, workspace: &Path) -> Result<Vec<Message>, Error> {
        let session = match self.session(id) {
            Some(session) => session,
            None => {
                let path = session_directory(
                    &self.config.builder.session_root,
                    &workspace.to_string_lossy(),
                )
                .map_err(|error_| error(INTERNAL_ERROR, self.redact(&error_.to_string())))?
                .join(format!("{id}.jsonl"));
                if !path.is_file() {
                    return Err(unknown_session());
                }
                let (controller, warnings) =
                    Controller::open(Arc::clone(&self.config.builder), &path)
                        .await
                        .map_err(|message| error(INTERNAL_ERROR, self.redact(&message)))?;
                for warning in warnings {
                    eprintln!("warning: {}", self.redact(&warning));
                }
                self.register(controller).1
            }
        };
        Ok(session.controller.history())
    }

    async fn list_sessions(&self) -> Result<Vec<SessionInfo>, Error> {
        let root = self.config.builder.session_root.clone();
        let workspace = self.config.workspace.to_string_lossy().into_owned();
        let listed = if root.exists() {
            tokio::task::spawn_blocking(move || {
                crate::session::list(&root, &workspace, "", MAX_LIST_SESSIONS)
            })
            .await
            .map_err(|join| error(INTERNAL_ERROR, join.to_string()))?
            .map_err(|failure| error(INTERNAL_ERROR, self.redact(&failure.to_string())))?
            .sessions
        } else {
            Vec::new()
        };
        Ok(listed
            .into_iter()
            .map(|info| {
                let title = if info.name.is_empty() {
                    &info.last_user_text
                } else {
                    &info.name
                };
                SessionInfo::new(info.id.clone(), PathBuf::from(&info.cwd))
                    .title(title.chars().take(TITLE_CHARS).collect::<String>())
                    .updated_at(info.modified.to_rfc3339())
            })
            .collect())
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

/// The prompt text of a `session/prompt` request: `text` blocks and
/// `resource_link` lines joined with `\n`.
fn parse_prompt(params: &Value) -> Result<(String, String), Error> {
    let session_id = params
        .get("sessionId")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid_params("sessionId is required"))?
        .to_string();
    let blocks = params
        .get("prompt")
        .and_then(Value::as_array)
        .ok_or_else(|| invalid_params("prompt must be an array of content blocks"))?;
    let mut lines = Vec::new();
    for block in blocks {
        match serde_json::from_value::<ContentBlock>(block.clone()) {
            Ok(ContentBlock::Text(text)) => lines.push(text.text),
            Ok(ContentBlock::ResourceLink(link)) => {
                lines.push(format!("{}: {}", link.name, link.uri))
            }
            Ok(_) => return Err(invalid_params("unsupported content block type")),
            Err(failure) => return Err(invalid_params(failure.to_string())),
        }
    }
    let text = lines.join("\n");
    if text.trim().is_empty() {
        return Err(invalid_params("prompt is empty"));
    }
    Ok((session_id, text))
}

/// `local` advertises the `_otto/memory/*` methods, which only the local
/// backend serves.
fn initialize_result(local: bool) -> Reply {
    let capabilities = AgentCapabilities::new()
        .load_session(true)
        .prompt_capabilities(PromptCapabilities::new())
        .mcp_capabilities(McpCapabilities::new())
        .session_capabilities(SessionCapabilities::new().list(SessionListCapabilities::new()));
    let capabilities = match local {
        true => capabilities.meta(serde_json::Map::from_iter([(
            "otto".to_string(),
            json!({"memoryReview": true}),
        )])),
        false => capabilities,
    };
    result(
        InitializeResponse::new(ProtocolVersion::V1)
            .agent_capabilities(capabilities)
            .auth_methods(vec![])
            .agent_info(Implementation::new("otto", env!("CARGO_PKG_VERSION"))),
    )
}

/// The read loop's state: the shared connection, the request tasks, and the
/// token every prompt token descends from.
struct Dispatcher {
    connection: Arc<Connection>,
    tasks: JoinSet<()>,
    stop: CancellationToken,
}

impl Dispatcher {
    fn handle_line(&mut self, line: &str) {
        let line = line.trim();
        if line.is_empty() {
            return;
        }
        let frame: Value = match serde_json::from_str(line) {
            Ok(frame) => frame,
            Err(failure) => {
                return self
                    .connection
                    .reply(Value::Null, Err(error(PARSE_ERROR, failure.to_string())));
            }
        };
        let Some(object) = frame.as_object() else {
            return self.connection.reply(
                Value::Null,
                Err(error(INVALID_REQUEST, "frame is not a JSON object")),
            );
        };
        let method = object.get("method").and_then(Value::as_str);
        let id = object.get("id").cloned();
        match (method, id) {
            (Some(method), Some(id)) => {
                let params = object.get("params").cloned().unwrap_or(Value::Null);
                self.request(method.to_string(), id, params);
            }
            (Some("session/cancel"), None) => {
                let session_id = object
                    .get("params")
                    .and_then(|params| params.get("sessionId"))
                    .and_then(Value::as_str);
                if let Some(slot) = session_id.and_then(|id| self.connection.slot(id)) {
                    slot.cancel();
                }
            }
            (Some(_), None) => {}
            (None, Some(_)) if object.contains_key("result") || object.contains_key("error") => {
                self.connection.client_response(frame);
            }
            _ => self.connection.reply(
                object.get("id").cloned().unwrap_or(Value::Null),
                Err(error(
                    INVALID_REQUEST,
                    "not a request, notification or response",
                )),
            ),
        }
    }

    fn request(&mut self, method: String, id: Value, params: Value) {
        let connection = Arc::clone(&self.connection);
        match method.as_str() {
            "initialize" => {
                let local = matches!(connection.backend, Backend::Local(_));
                connection.reply(id, initialize_result(local));
            }
            "session/new" => {
                self.tasks.spawn(async move {
                    let reply = connection.new_session(params).await;
                    connection.reply(id, reply);
                });
            }
            "session/load" => {
                self.tasks.spawn(async move {
                    let reply = connection.load_session(params).await;
                    connection.reply(id, reply);
                });
            }
            "session/list" => {
                self.tasks.spawn(async move {
                    let reply = connection.list_sessions(params).await;
                    connection.reply(id, reply);
                });
            }
            "session/prompt" => self.start_prompt(id, &params),
            memory::PENDING_METHOD | memory::REVIEW_METHOD => {
                self.tasks.spawn(async move {
                    let reply = connection.memory_request(&method, params);
                    connection.reply(id, reply);
                });
            }
            _ => connection.reply(
                id,
                Err(error(METHOD_NOT_FOUND, format!("unknown method {method}"))),
            ),
        }
    }

    /// Validates the request and installs the prompt token here, in the read
    /// loop, so a `session/cancel` read next already finds it.
    fn start_prompt(&mut self, id: Value, params: &Value) {
        let connection = Arc::clone(&self.connection);
        let (session_id, text) = match parse_prompt(params) {
            Ok(parsed) => parsed,
            Err(failure) => return connection.reply(id, Err(failure)),
        };
        let Some(slot) = connection.slot(&session_id) else {
            return connection.reply(id, Err(unknown_session()));
        };
        let Some(token) = slot.begin(&self.stop) else {
            return connection.reply(
                id,
                Err(error(INTERNAL_ERROR, "a prompt is already running")),
            );
        };
        self.tasks.spawn(async move {
            let reply = connection.prompt(&session_id, text, &token).await;
            slot.end();
            connection.reply(id, reply);
        });
    }
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
    let local = Local {
        config,
        sessions: Mutex::new(HashMap::new()),
    };
    drive(
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
    .await
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
    let connection = Arc::new(Connection {
        workspace,
        backend,
        out,
        pending: Mutex::new(HashMap::new()),
        next_request_id: AtomicU64::new(1),
    });
    let mut lines = read_lines(stdin);
    let read_loop = async {
        let mut dispatcher = Dispatcher {
            connection: Arc::clone(&connection),
            tasks: JoinSet::new(),
            stop: CancellationToken::new(),
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
        while dispatcher.tasks.join_next().await.is_some() {}
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
