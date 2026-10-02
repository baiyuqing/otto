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
    SessionCapabilities, SessionListCapabilities, SessionNotification, SessionUpdate, StopReason,
};
use otto_core::agent::Event;
use otto_core::config::resolve::Runtime;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use crate::app::{Controller, SandboxControl};
use crate::cli::runtime_builder::Builder;
use crate::session::{MAX_LIST_SESSIONS, session_directory};

use approval::{Decision, request_permission};

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

/// One loaded session: its controller and the token of the running prompt.
struct Session {
    controller: Arc<Controller>,
    prompt: Mutex<Option<CancellationToken>>,
}

impl Session {
    /// Installs the token of a new prompt; `None` while one is running.
    fn begin_prompt(&self, parent: &CancellationToken) -> Option<CancellationToken> {
        let mut slot = self.prompt.lock().expect("prompt slot");
        if slot.is_some() {
            return None;
        }
        let token = parent.child_token();
        *slot = Some(token.clone());
        Some(token)
    }

    fn end_prompt(&self) {
        *self.prompt.lock().expect("prompt slot") = None;
    }

    fn cancel(&self) {
        if let Some(token) = self.prompt.lock().expect("prompt slot").as_ref() {
            token.cancel();
        }
    }
}

/// State shared by the read loop and its request tasks.
struct Connection {
    config: Config,
    out: mpsc::UnboundedSender<Value>,
    sessions: Mutex<HashMap<String, Arc<Session>>>,
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

    fn session(&self, id: &str) -> Option<Arc<Session>> {
        self.sessions.lock().expect("session map").get(id).cloned()
    }

    fn redact(&self, message: &str) -> String {
        self.config
            .builder
            .redact_error(message, Some(&self.config.runtime))
    }

    /// Accepts `cwd` only when it canonicalizes to the process workspace.
    fn check_cwd(&self, cwd: Option<&str>) -> Result<(), Error> {
        let cwd = cwd.ok_or_else(|| invalid_params("cwd is required"))?;
        if !Path::new(cwd).is_absolute() {
            return Err(invalid_params("cwd must be an absolute path"));
        }
        match crate::cli::sandbox_runtime::canonical_directory(Path::new(cwd)) {
            Ok(canonical) if canonical == self.config.workspace => Ok(()),
            _ => Err(invalid_params(format!(
                "cwd must be the workspace {}",
                self.config.workspace.display()
            ))),
        }
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
            prompt: Mutex::new(None),
        });
        self.sessions
            .lock()
            .expect("session map")
            .insert(id.clone(), Arc::clone(&session));
        (id, session)
    }

    async fn new_session(&self, params: Value) -> Reply {
        let params = SessionParams::parse(params)?;
        self.check_cwd(params.cwd.as_deref())?;
        params.check_mcp_servers()?;
        let controller = Controller::create(Arc::clone(&self.config.builder), &self.config.runtime)
            .await
            .map_err(|message| error(INTERNAL_ERROR, self.redact(&message)))?;
        let (id, _) = self.register(controller);
        result(NewSessionResponse::new(id))
    }

    async fn load_session(&self, params: Value) -> Reply {
        let params = SessionParams::parse(params)?;
        self.check_cwd(params.cwd.as_deref())?;
        params.check_mcp_servers()?;
        let id = params.session_id.unwrap_or_default();
        if id.len() != 32 || !id.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
            return Err(invalid_params(
                "sessionId must be 32 lowercase hexadecimal characters",
            ));
        }
        let session = match self.session(&id) {
            Some(session) => session,
            None => {
                let path = session_directory(
                    &self.config.builder.session_root,
                    &self.config.workspace.to_string_lossy(),
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
        for update in update::history_updates(&session.controller.history()) {
            self.update(&id, update);
        }
        Ok(json!({ "sessionId": id }))
    }

    async fn list_sessions(&self, params: Value) -> Reply {
        self.check_cwd(params.get("cwd").and_then(Value::as_str))?;
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
        let sessions = listed
            .into_iter()
            .map(|info| {
                let title = if info.name.is_empty() {
                    &info.last_user_text
                } else {
                    &info.name
                };
                agent_client_protocol_schema::v1::SessionInfo::new(
                    info.id.clone(),
                    PathBuf::from(&info.cwd),
                )
                .title(title.chars().take(TITLE_CHARS).collect::<String>())
                .updated_at(info.modified.to_rfc3339())
            })
            .collect();
        result(ListSessionsResponse::new(sessions))
    }

    /// Runs one prompt, including the approval retries, to its stop reason.
    async fn prompt(
        &self,
        session: &Session,
        session_id: &str,
        mut text: String,
        cancel: &CancellationToken,
    ) -> Reply {
        let stopped = |reason| result(PromptResponse::new(reason));
        loop {
            let mut request = None;
            let outcome = session
                .controller
                .prompt(
                    &text,
                    &mut |event| {
                        if let Event::ToolCallFinished {
                            tool_name,
                            tool_call_id,
                            result,
                            ..
                        } = &event
                            && let Some(found) =
                                crate::tool::bash::parse_approval_request(tool_name, result)
                        {
                            request = Some((tool_call_id.clone(), found));
                        }
                        if let Some(update) = update::event_update(&event) {
                            self.update(session_id, update);
                        }
                    },
                    cancel,
                )
                .await;
            if cancel.is_cancelled() {
                return stopped(StopReason::Cancelled);
            }
            if let Err(failure) = outcome {
                return Err(error(INTERNAL_ERROR, self.redact(&failure.to_string())));
            }
            let Some((tool_call_id, request)) = request else {
                return stopped(StopReason::EndTurn);
            };
            let pending = self
                .config
                .builder
                .bash_approvals
                .as_ref()
                .and_then(|approvals| approvals.pending_command(session_id, &request.id));
            let Some(command) = pending else {
                return stopped(StopReason::EndTurn);
            };
            match request_permission(self, session_id, &tool_call_id, &command, cancel).await {
                Decision::Cancelled => return stopped(StopReason::Cancelled),
                Decision::Deny => return stopped(StopReason::EndTurn),
                Decision::Allow => {
                    text = session
                        .controller
                        .approve_bash(&request.id)
                        .map_err(|message| error(INTERNAL_ERROR, self.redact(&message)))?;
                }
            }
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

fn initialize_result() -> Reply {
    let capabilities = AgentCapabilities::new()
        .load_session(true)
        .prompt_capabilities(PromptCapabilities::new())
        .mcp_capabilities(McpCapabilities::new())
        .session_capabilities(SessionCapabilities::new().list(SessionListCapabilities::new()));
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
                if let Some(session) = session_id.and_then(|id| self.connection.session(id)) {
                    session.cancel();
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
            "initialize" => connection.reply(id, initialize_result()),
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
        let Some(session) = connection.session(&session_id) else {
            return connection.reply(id, Err(unknown_session()));
        };
        let Some(token) = session.begin_prompt(&self.stop) else {
            return connection.reply(
                id,
                Err(error(INTERNAL_ERROR, "a prompt is already running")),
            );
        };
        self.tasks.spawn(async move {
            let reply = connection.prompt(&session, &session_id, text, &token).await;
            session.end_prompt();
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
    let (out, frames) = mpsc::unbounded_channel();
    let connection = Arc::new(Connection {
        config,
        out,
        sessions: Mutex::new(HashMap::new()),
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
                Some(finished) = dispatcher.tasks.join_next() => {
                    if let Err(failure) = finished {
                        eprintln!("acp: request task failed: {failure}");
                    }
                }
            }
        }
        dispatcher.stop.cancel();
        while dispatcher.tasks.join_next().await.is_some() {}
        let controllers = connection
            .sessions
            .lock()
            .expect("session map")
            .values()
            .map(|session| Arc::clone(&session.controller))
            .collect();
        // Dropping the dispatcher and the connection drops the last sender,
        // which ends the writer.
        drop(dispatcher);
        drop(connection);
        controllers
    };
    let (controllers, ()) = tokio::join!(read_loop, write_frames(stdout, frames));
    controllers
}
