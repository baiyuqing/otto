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
//!
//! Errors: see the table in `docs/specs/2026-10-02-acp-agent-server.md`.

mod approval;
pub mod attach;
mod command;
mod memory;
mod schedule;
mod shared_approval;
pub mod tcp;
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

use crate::app::{
    ActiveInputAdmission, ApprovalDecision, CLOSED, Channel, Controller, PROMPT_ACTIVE,
    SandboxControl, Step, Stop,
};
use crate::runtime::builder::Builder;
use crate::schedule::Origin;
use crate::session::{MAX_LIST_SESSIONS, is_session_id, session_directory};

use approval::request_permission;

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

fn task_control_error(failure: crate::client::Error) -> Error {
    match &failure {
        crate::client::Error::Http {
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
}

#[derive(Default)]
struct PromptState {
    active: Option<CancellationToken>,
    queue: std::collections::VecDeque<QueuedPrompt>,
    draining: bool,
    /// True while `active` belongs to a wake turn rather than a prompt.
    wake: bool,
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

impl PromptSlot {
    fn signal_released(&self) {
        self.released.send_modify(|count| *count += 1);
    }

    /// Claims the slot for a wake turn when nothing else holds or awaits it.
    fn begin_wake(&self, parent: &CancellationToken) -> Option<CancellationToken> {
        let mut state = self.state.lock().expect("prompt slot");
        if state.active.is_some()
            || state.draining
            || !state.followups.is_empty()
            || state.waiting.is_some()
        {
            return None;
        }
        let token = parent.child_token();
        state.active = Some(token.clone());
        state.wake = true;
        Some(token)
    }

    /// Registers an ordinary prompt to run after the wake turn that holds the
    /// slot. The caller calls this after `begin` failed under a separate lock,
    /// so a slot that is free again (the wake turn ended in between) is also
    /// accepted. `None` when the slot is held by anything else or already
    /// awaited.
    fn wait_for_wake(&self, parent: &CancellationToken) -> Option<CancellationToken> {
        let mut state = self.state.lock().expect("prompt slot");
        let free = state.active.is_none() && !state.draining && state.followups.is_empty();
        if !(state.wake || free) || state.waiting.is_some() {
            return None;
        }
        let token = parent.child_token();
        state.waiting = Some(token.clone());
        Some(token)
    }

    /// Waits until the slot is free and installs the waiting prompt's token as
    /// active. `false` when the wait was cancelled.
    async fn claim_after_wake(&self, token: &CancellationToken) -> bool {
        let mut released = self.released.subscribe();
        loop {
            {
                let mut state = self.state.lock().expect("prompt slot");
                if token.is_cancelled() {
                    state.waiting = None;
                    return false;
                }
                if state.active.is_none() && !state.draining && state.followups.is_empty() {
                    state.waiting = None;
                    state.active = Some(token.clone());
                    return true;
                }
            }
            tokio::select! {
                biased;
                () = token.cancelled() => {}
                _ = released.changed() => {}
            }
        }
    }

    /// Installs a normal prompt only when no prompt or explicit queue is active.
    fn begin(&self, parent: &CancellationToken) -> Option<CancellationToken> {
        let mut state = self.state.lock().expect("prompt slot");
        if state.active.is_some() || state.draining || !state.followups.is_empty() {
            return None;
        }
        let token = parent.child_token();
        state.active = Some(token.clone());
        Some(token)
    }

    fn end(&self) {
        {
            let mut state = self.state.lock().expect("prompt slot");
            state.active = None;
            state.wake = false;
        }
        self.changed.notify_one();
        self.signal_released();
    }

    fn cancel(&self) -> usize {
        let mut state = self.state.lock().expect("prompt slot");
        let dropped = state.queue.len();
        state.queue.clear();
        if let Some(token) = state.active.as_ref() {
            token.cancel();
        }
        if let Some(token) = state.waiting.as_ref() {
            token.cancel();
        }
        for token in state.followups.values() {
            token.cancel();
        }
        self.changed.notify_one();
        dropped
    }

    fn active_cancel(&self) -> CancellationToken {
        self.state
            .lock()
            .expect("prompt slot")
            .active
            .clone()
            .unwrap_or_default()
    }

    fn enqueue(
        &self,
        text: String,
        (channel, origin): (Option<Channel>, Option<Origin>),
        input_id: Option<String>,
    ) -> Result<bool, &'static str> {
        let mut state = self.state.lock().expect("prompt slot");
        if state.queue.len() >= ACP_QUEUE_LIMIT {
            return Err("queue is full (16 pending prompts)");
        }
        state.queue.push_back(QueuedPrompt {
            text,
            channel,
            origin,
            input_id,
        });
        let start_worker = !state.draining;
        state.draining = true;
        drop(state);
        self.changed.notify_one();
        Ok(start_worker)
    }

    async fn drain_queue(self: Arc<Self>, connection: Arc<Connection>, session_id: String) {
        let stop = connection.task_stop.child_token();
        loop {
            let changed = self.changed.notified();
            let next = {
                let mut state = self.state.lock().expect("prompt slot");
                if stop.is_cancelled() {
                    state.queue.clear();
                    state.draining = false;
                    drop(state);
                    self.signal_released();
                    return;
                }
                if state.active.is_some() {
                    None
                } else if let Some(queued) = state.queue.pop_front() {
                    let token = stop.child_token();
                    state.active = Some(token.clone());
                    Some((queued, token))
                } else {
                    state.draining = false;
                    drop(state);
                    self.signal_released();
                    return;
                }
            };
            let Some((queued, token)) = next else {
                tokio::select! {
                    biased;
                    () = stop.cancelled() => continue,
                    () = changed => continue,
                }
            };
            let controller = connection.local_controller(&session_id);
            if let Some(controller) = &controller {
                controller.set_next_input_id(queued.input_id.clone());
            }
            let reply = connection
                .queued_prompt(
                    &session_id,
                    queued.text,
                    queued.channel,
                    queued.origin,
                    None,
                    &token,
                )
                .await;
            if let Some(controller) = &controller {
                controller.set_next_input_id(None);
                // The prompt ran: its saved copy is no longer waiting.
                if let (Some(input_id), Some(journal)) = (&queued.input_id, controller.turn_queue())
                {
                    journal.forget(input_id);
                }
            }
            if let Some(notice) = queued_outcome_notice(&reply) {
                connection.say(&session_id, &notice);
            }
            self.end();
        }
    }

    fn begin_followup(
        &self,
        parent: &CancellationToken,
    ) -> Result<(u64, CancellationToken), &'static str> {
        let mut state = self.state.lock().expect("prompt slot");
        let limit = ACP_QUEUE_LIMIT + if state.active.is_none() { 1 } else { 0 };
        if state.followups.len() >= limit {
            return Err("queue is full (16 pending prompts)");
        }
        state.next_followup = state.next_followup.wrapping_add(1);
        if state.next_followup == 1 {
            state.next_followup_admission = 1;
        }
        let id = state.next_followup;
        let token = parent.child_token();
        state.followups.insert(id, token.clone());
        Ok((id, token))
    }

    async fn wait_followup_admission(&self, id: u64) {
        loop {
            let mut changed = Box::pin(self.followup_changed.notified());
            changed.as_mut().enable();
            if self
                .state
                .lock()
                .expect("prompt slot")
                .next_followup_admission
                == id
            {
                return;
            }
            changed.await;
        }
    }

    fn finish_followup_admission(&self, id: u64) {
        let mut state = self.state.lock().expect("prompt slot");
        if state.next_followup_admission == id {
            state.next_followup_admission = state.next_followup_admission.wrapping_add(1);
        }
        drop(state);
        self.followup_changed.notify_waiters();
    }

    fn end_followup(&self, id: u64) {
        self.state
            .lock()
            .expect("prompt slot")
            .followups
            .remove(&id);
        self.signal_released();
    }
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
    /// The controller of a session this process runs itself.
    fn local_controller(&self, id: &str) -> Option<Arc<Controller>> {
        match &self.backend {
            Backend::Local(local) => local
                .session(id)
                .map(|session| Arc::clone(&session.controller)),
            Backend::Attach(_) => None,
        }
    }

    /// Restores the `/queue` prompts saved before Otto stopped, after the
    /// session's pending wake turn (the interrupted turn's recovery) has run:
    /// they wait until the slot is free and either no notification is
    /// pending or a wake turn has ended since. The first of them delivers
    /// the restore notice.
    fn restore_queued_prompts(self: &Arc<Self>, session_id: &str) {
        let (Some(controller), Some(slot)) =
            (self.local_controller(session_id), self.slot(session_id))
        else {
            return;
        };
        let prompts = controller.restorable_prompts();
        if prompts.is_empty() {
            return;
        }
        let connection = Arc::clone(self);
        let session_id = session_id.to_string();
        let stop = self.task_stop.clone();
        tokio::spawn(async move {
            let mut released = slot.released.subscribe();
            let start = *released.borrow_and_update();
            loop {
                let idle = {
                    let state = slot.state.lock().expect("prompt slot");
                    state.active.is_none() && !state.draining && state.followups.is_empty()
                };
                let pending = controller
                    .subagent_tasks()
                    .is_some_and(|tasks| !tasks.notifications().is_empty());
                if idle && (!pending || *released.borrow() != start) {
                    break;
                }
                tokio::select! {
                    () = stop.cancelled() => return,
                    changed = released.changed() => if changed.is_err() { return },
                    () = tokio::time::sleep(std::time::Duration::from_millis(50)) => {}
                }
            }
            let mut start_worker = false;
            for prompt in &prompts {
                if let Ok(start) = slot.enqueue(
                    prompt.text.clone(),
                    (prompt.channel, prompt.origin.clone()),
                    Some(prompt.input_id.clone()),
                ) {
                    start_worker |= start;
                }
            }
            controller.announce_restored_prompts(&prompts);
            if start_worker {
                slot.drain_queue(connection, session_id).await;
            }
        });
    }

    fn slot(&self, id: &str) -> Option<Arc<PromptSlot>> {
        match &self.backend {
            Backend::Local(local) => local.session(id).map(|session| Arc::clone(&session.prompt)),
            Backend::Attach(relay) => relay.slot(id),
        }
    }

    /// Local ACP stays pinned to its workspace; serve owns attach admission.
    fn request_workspace(&self, cwd: Option<&str>) -> Result<PathBuf, Error> {
        let cwd = cwd.ok_or_else(|| invalid_params("cwd is required"))?;
        if !Path::new(cwd).is_absolute() {
            return Err(invalid_params("cwd must be an absolute path"));
        }
        if matches!(&self.backend, Backend::Attach(_)) {
            return Ok(PathBuf::from(cwd));
        }
        match crate::runtime::sandbox_runtime::canonical_directory(Path::new(cwd)) {
            Ok(canonical) if canonical == self.workspace => Ok(canonical),
            _ => Err(invalid_params(format!(
                "cwd must be the workspace {}",
                self.workspace.display()
            ))),
        }
    }

    async fn new_session(self: &Arc<Self>, params: Value) -> Reply {
        let params = SessionParams::parse(params)?;
        let workspace = self.request_workspace(params.cwd.as_deref())?;
        params.check_mcp_servers()?;
        let reply = match &self.backend {
            Backend::Local(local) => local.new_session().await,
            Backend::Attach(relay) => relay.new_session(&workspace).await,
        }?;
        let id = reply
            .get("sessionId")
            .and_then(Value::as_str)
            .unwrap_or_default();
        self.start_wake_loop(id);
        self.start_notice_watcher(id).await;
        Ok(reply)
    }

    async fn load_session(self: &Arc<Self>, params: Value) -> Reply {
        let params = SessionParams::parse(params)?;
        let workspace = self.request_workspace(params.cwd.as_deref())?;
        params.check_mcp_servers()?;
        let id = params.session_id.unwrap_or_default();
        if !is_session_id(&id) {
            return Err(invalid_params(
                "sessionId must be 32 lowercase hexadecimal characters",
            ));
        }
        let (history, plan) = match &self.backend {
            Backend::Local(local) => local.load_session(&id, &workspace).await?,
            Backend::Attach(relay) => relay.load_session(&id, &workspace).await?,
        };
        for update in update::history_updates(&history) {
            self.update(&id, update);
        }
        self.restore_queued_prompts(&id);
        if !plan.is_empty() {
            self.update(&id, update::plan_update(&plan));
        }
        self.start_wake_loop(&id);
        self.start_notice_watcher(&id).await;
        Ok(json!({ "sessionId": id }))
    }

    /// Runs the session's wake turns until the connection ends: a due
    /// sub-agent or timer notification starts a turn whenever nothing else
    /// holds the session. The turn streams nothing; its reply text is queued
    /// on the controller and sent by the wake-reply watcher.
    fn start_wake_loop(&self, session_id: &str) {
        let Backend::Local(local) = &self.backend else {
            return;
        };
        let Some(session) = local.session(session_id) else {
            return;
        };
        let Some(tasks) = session.controller.subagent_tasks() else {
            return;
        };
        let mut loops = self.wake_loops.lock().expect("wake loops");
        if loops.contains_key(session_id) {
            return;
        }
        let stop = self.task_stop.clone();
        loops.insert(
            session_id.to_string(),
            tokio::spawn(async move {
                let mut updates = tasks.updates();
                let mut released = session.prompt.released.subscribe();
                loop {
                    if wake_turn(&session, &stop).await {
                        // A failed turn leaves its notification queued; wait
                        // before retrying so a persistent error does not spin.
                        tokio::select! {
                            () = stop.cancelled() => return,
                            () = tokio::time::sleep(std::time::Duration::from_secs(5)) => {}
                        }
                    }
                    tokio::select! {
                        biased;
                        () = stop.cancelled() => return,
                        changed = updates.changed() => if changed.is_err() { return },
                        changed = released.changed() => if changed.is_err() { return },
                    }
                }
            }),
        );
    }

    /// Follows the session's notices until the connection ends. A local
    /// session starts at the beginning, which includes the notices queued when
    /// its controller was built. An attached session starts at the current
    /// cursor, so a reconnect does not replay old outcomes.
    async fn start_notice_watcher(self: &Arc<Self>, session_id: &str) {
        let id = session_id.to_string();
        let controller = match &self.backend {
            Backend::Local(local) => local
                .session(&id)
                .map(|session| Arc::clone(&session.controller)),
            Backend::Attach(relay) => relay
                .server
                .as_ref()
                .and_then(|server| server.controller(&id)),
        };
        let connection = Arc::clone(self);
        let stop = self.notice_stop.child_token();
        {
            let mut watchers = self.watching_notices.lock().expect("notice watchers");
            if let Some(current) = &controller
                && watchers
                    .get(&id)
                    .and_then(|watch| watch.controller.upgrade())
                    .is_some_and(|watched| Arc::ptr_eq(&watched, current))
            {
                return;
            }
            // A different client may have closed and reopened this session id.
            if let Some(previous) = watchers.insert(
                id.clone(),
                NoticeWatch {
                    controller: controller.as_ref().map(Arc::downgrade).unwrap_or_default(),
                    stop: stop.clone(),
                },
            ) {
                previous.stop.cancel();
            }
        }
        let (ready, start) = tokio::sync::watch::channel(false);
        self.notice_ready
            .lock()
            .expect("notice ready map")
            .insert(id.clone(), ready);
        match &self.backend {
            Backend::Local(_) => {
                let Some(controller) = controller else {
                    return;
                };
                let changed = controller.notices_changed();
                // This session owns the controller, so every notice it holds,
                // including one queued when it was built, is new to this client.
                let replies_changed = controller.wake_replies_changed();
                let (wake_connection, wake_controller) =
                    (Arc::clone(&connection), Arc::clone(&controller));
                let (wake_id, wake_stop, wake_start) = (id.clone(), stop.clone(), start.clone());
                tokio::spawn(async move {
                    if !notice_watcher_ready(wake_start, &wake_stop).await {
                        return;
                    }
                    watch_controller_notices(
                        wake_connection,
                        wake_controller,
                        wake_id,
                        wake_stop,
                        replies_changed,
                        0,
                        true,
                    )
                    .await;
                });
                tokio::spawn(async move {
                    if !notice_watcher_ready(start, &stop).await {
                        return;
                    }
                    watch_controller_notices(connection, controller, id, stop, changed, 0, false)
                        .await;
                });
            }
            Backend::Attach(relay) => {
                if relay.server.is_some() {
                    let Some(controller) = controller else {
                        return;
                    };
                    let changed = controller.notices_changed();
                    let (_, after) = controller.notices_since(0);
                    let replies_changed = controller.wake_replies_changed();
                    let (_, replies_after) = controller.wake_replies_since(0);
                    let (wake_connection, wake_controller) =
                        (Arc::clone(&connection), Arc::clone(&controller));
                    let (wake_id, wake_stop, wake_start) =
                        (id.clone(), stop.clone(), start.clone());
                    tokio::spawn(async move {
                        if !notice_watcher_ready(wake_start, &wake_stop).await {
                            return;
                        }
                        watch_controller_notices(
                            wake_connection,
                            wake_controller,
                            wake_id,
                            wake_stop,
                            replies_changed,
                            replies_after,
                            true,
                        )
                        .await;
                    });
                    tokio::spawn(async move {
                        if !notice_watcher_ready(start, &stop).await {
                            return;
                        }
                        watch_controller_notices(
                            connection, controller, id, stop, changed, after, false,
                        )
                        .await;
                    });
                    return;
                }
                let initial = tokio::select! {
                    biased;
                    () = stop.cancelled() => return,
                    result = relay.client.reflection_notices(&id, 0) => result,
                };
                let initial = match initial {
                    Ok(initial) => initial,
                    Err(failure) => {
                        relay.fail(failure);
                        return;
                    }
                };
                // An older serve has no wake-replies route; only this polling
                // stops then.
                let mut replies_after = tokio::select! {
                    biased;
                    () = stop.cancelled() => return,
                    result = relay.client.wake_replies(&id, 0) => match result {
                        Ok(initial) => Some(initial.last),
                        Err(crate::client::Error::Http { status: 404, .. }) => None,
                        Err(failure) => {
                            relay.fail(failure);
                            return;
                        }
                    },
                };
                let connection = Arc::clone(&connection);
                tokio::spawn(async move {
                    if !notice_watcher_ready(start, &stop).await {
                        return;
                    }
                    let mut after = initial.last;
                    loop {
                        tokio::select! {
                            biased;
                            () = stop.cancelled() => return,
                            () = tokio::time::sleep(std::time::Duration::from_secs(1)) => {}
                        }
                        let result = match &connection.backend {
                            Backend::Attach(relay) => tokio::select! {
                                biased;
                                () = stop.cancelled() => return,
                                result = relay.client.reflection_notices(&id, after) => result,
                            },
                            Backend::Local(_) => return,
                        };
                        if let Some(replies_cursor) = replies_after {
                            let polled = match &connection.backend {
                                Backend::Attach(relay) => tokio::select! {
                                    biased;
                                    () = stop.cancelled() => return,
                                    result = relay.client.wake_replies(&id, replies_cursor) => result,
                                },
                                Backend::Local(_) => return,
                            };
                            match polled {
                                Ok(page) => {
                                    emit_notices(
                                        &connection,
                                        &id,
                                        page.notices.into_iter().map(|n| (n.id, n.text)).collect(),
                                        true,
                                    );
                                    replies_after = Some(page.last);
                                }
                                Err(crate::client::Error::Http { status: 404, .. }) => {
                                    replies_after = None;
                                }
                                Err(failure) => {
                                    if let Backend::Attach(relay) = &connection.backend {
                                        relay.fail(failure);
                                    }
                                    return;
                                }
                            }
                        }
                        match result {
                            Ok(page) => {
                                for notice in page.notices {
                                    connection.reflection_notice(
                                        &id,
                                        notice.id,
                                        &notice.text,
                                        &notice.presentation_origin,
                                    );
                                }
                                after = page.last;
                            }
                            Err(crate::client::Error::Http { status: 404, .. }) => return,
                            Err(failure) => {
                                if let Backend::Attach(relay) = &connection.backend {
                                    relay.fail(failure);
                                }
                                return;
                            }
                        }
                    }
                });
            }
        }
    }

    /// Memory review uses a controller owned by this process, either a local
    /// ACP session or a serve session opened by this TCP connection.
    fn memory_request(&self, method: &str, params: Value) -> Reply {
        if matches!(&self.backend, Backend::Attach(relay) if relay.server.is_none()) {
            return Err(error(METHOD_NOT_FOUND, format!("unknown method {method}")));
        }
        let session_of = |id: &str| {
            match &self.backend {
                Backend::Local(local) => local
                    .session(id)
                    .map(|session| Arc::clone(&session.controller)),
                Backend::Attach(relay) => relay
                    .server
                    .as_ref()
                    .filter(|_| relay.slot(id).is_some())
                    .and_then(|server| server.controller(id)),
            }
            .ok_or_else(unknown_session)
        };
        if method == memory::PENDING_METHOD {
            let params: memory::PendingParams = serde_json::from_value(params)
                .map_err(|failure| invalid_params(failure.to_string()))?;
            memory::pending(session_of(&params.session_id)?.as_ref(), &params)
        } else {
            let params: memory::ReviewParams = serde_json::from_value(params)
                .map_err(|failure| invalid_params(failure.to_string()))?;
            memory::review(session_of(&params.session_id)?.as_ref(), &params)
        }
    }

    async fn list_sessions(&self, params: Value) -> Reply {
        let workspace = match params.get("cwd") {
            None | Some(Value::Null) => self.workspace.clone(),
            Some(value) => self.request_workspace(value.as_str())?,
        };
        let sessions = match &self.backend {
            Backend::Local(local) => local.list_sessions().await?,
            Backend::Attach(relay) => relay.list_sessions(&workspace).await?,
        };
        result(ListSessionsResponse::new(sessions))
    }

    /// Sends the command list after a successful `session/new` or
    /// `session/load` reply, so the client already knows the session id.
    fn advertise_commands(&self, reply: &Reply) {
        if let Some(id) = reply
            .as_ref()
            .ok()
            .and_then(|value| value.get("sessionId"))
            .and_then(Value::as_str)
        {
            self.update(id, command::available_commands());
        }
    }

    fn notices_ready(&self, reply: &Reply) {
        if let Some(id) = reply
            .as_ref()
            .ok()
            .and_then(|value| value.get("sessionId"))
            .and_then(Value::as_str)
            && let Some(ready) = self
                .notice_ready
                .lock()
                .expect("notice ready map")
                .remove(id)
        {
            let _ = ready.send(true);
        }
    }

    fn say(&self, session_id: &str, text: &str) {
        self.update(
            session_id,
            SessionUpdate::AgentMessageChunk(ContentChunk::new(ContentBlock::Text(
                TextContent::new(text),
            ))),
        );
    }

    fn reflection_notice(
        &self,
        session_id: &str,
        notice_id: u64,
        text: &str,
        presentation_origin: &str,
    ) {
        let meta = serde_json::Map::from_iter([(
            "otto".to_string(),
            json!({
                "reflectionNotice": true,
                "noticeId": notice_id,
                "presentationOrigin": presentation_origin,
            }),
        )]);
        self.update(
            session_id,
            SessionUpdate::AgentMessageChunk(
                ContentChunk::new(ContentBlock::Text(TextContent::new(text))).meta(meta),
            ),
        );
    }

    fn wake_reply(&self, session_id: &str, reply_id: u64, text: &str) {
        let meta = serde_json::Map::from_iter([(
            "otto".to_string(),
            json!({"wakeReply": true, "replyId": reply_id}),
        )]);
        self.update(
            session_id,
            SessionUpdate::AgentMessageChunk(
                ContentChunk::new(ContentBlock::Text(TextContent::new(text))).meta(meta),
            ),
        );
    }

    /// Runs one normal prompt to its stop reason. Local slash commands run
    /// here and never reach the task model.
    async fn prompt(
        &self,
        session_id: &str,
        text: String,
        channel: Option<Channel>,
        origin: Option<Origin>,
        (request_id, input_id): (Option<&str>, Option<&str>),
        cancel: &CancellationToken,
    ) -> Reply {
        if let Some(command) = command::parse(&text) {
            return self.run_command(session_id, command, cancel).await;
        }
        // Keep the local agent turn and attach transport state machines on the
        // heap. Combining both large futures inline can overflow the default
        // ACP task stack as either backend evolves.
        match &self.backend {
            Backend::Local(local) => {
                let session = local.session(session_id).ok_or_else(unknown_session)?;
                if let Some(input_id) = input_id {
                    if session.controller.has_input(input_id) {
                        return Err(invalid_params(crate::app::task_control::DUPLICATE_INPUT));
                    }
                    session
                        .controller
                        .set_next_input_id(Some(input_id.to_string()));
                }
                let reply = Box::pin(self.local_prompt(
                    local,
                    &session,
                    session_id,
                    text,
                    (channel, origin),
                    cancel,
                ))
                .await;
                session.controller.set_next_input_id(None);
                reply
            }
            Backend::Attach(relay) => {
                Box::pin(relay.prompt(
                    self,
                    session_id,
                    &text,
                    (channel, origin.as_ref()),
                    (request_id, input_id),
                    cancel,
                ))
                .await
            }
        }
    }

    /// Runs a queued prompt as normal user text; slash commands inside its
    /// payload are not recursively interpreted as ACP control commands.
    async fn queued_prompt(
        &self,
        session_id: &str,
        text: String,
        channel: Option<Channel>,
        origin: Option<Origin>,
        request_id: Option<&str>,
        cancel: &CancellationToken,
    ) -> Reply {
        // Match `prompt`: queued turns use the same backend boundary and must
        // not inline either large state machine into this dispatcher.
        match &self.backend {
            Backend::Local(local) => {
                let session = local.session(session_id).ok_or_else(unknown_session)?;
                Box::pin(self.local_prompt(
                    local,
                    &session,
                    session_id,
                    text,
                    (channel, origin),
                    cancel,
                ))
                .await
            }
            Backend::Attach(relay) => {
                Box::pin(relay.prompt(
                    self,
                    session_id,
                    &text,
                    (channel, origin.as_ref()),
                    (request_id, None),
                    cancel,
                ))
                .await
            }
        }
    }

    /// Deterministically admits an ordinary prompt to the exact active run.
    /// It intentionally does not use the model-backed control dialogue.
    async fn admit_prompt(
        &self,
        session_id: &str,
        text: &str,
        input_id: Option<&str>,
    ) -> Result<ActiveInputAdmission, agent_client_protocol_schema::v1::Error> {
        match &self.backend {
            Backend::Local(local) => local
                .session(session_id)
                .ok_or_else(unknown_session)?
                .controller
                .admit_active_input_with(text, input_id)
                .map_err(|message| error(INTERNAL_ERROR, message)),
            Backend::Attach(relay) => relay
                .client
                .admit_input(session_id, text, input_id)
                .await
                .map_err(|failure| relay.task_control_error(failure)),
        }
    }

    async fn steer_prompt(&self, session_id: &str, guidance: &str) -> Reply {
        let accepted = match &self.backend {
            Backend::Local(local) => match local.session(session_id) {
                Some(session) => session
                    .controller
                    .steer(guidance)
                    .map_err(|message| error(INTERNAL_ERROR, message)),
                None => return Err(unknown_session()),
            },
            Backend::Attach(relay) => relay
                .client
                .steer(session_id, guidance)
                .await
                .map_err(|failure| relay.task_control_error(failure))
                .and_then(|message| {
                    (message.status == otto_core::agent::steering::Status::Queued)
                        .then_some(message)
                        .ok_or_else(|| {
                            error(INTERNAL_ERROR, "the active task did not accept guidance")
                        })
                }),
        };
        match accepted {
            Ok(message) => {
                self.say(
                    session_id,
                    &format!(
                        "Guidance queued (#{}): {}\nIt will be delivered at the next safe boundary.",
                        message.id, message.text
                    ),
                );
                result(PromptResponse::new(StopReason::EndTurn))
            }
            Err(failure) => Err(failure),
        }
    }

    async fn control_message(
        &self,
        session_id: &str,
        text: &str,
        channel: Option<Channel>,
    ) -> Reply {
        let reply = match &self.backend {
            Backend::Local(local) => match local.session(session_id) {
                Some(session) => {
                    let cancel = session.prompt.active_cancel();
                    session
                        .controller
                        .control_message_with_channel(text, channel, &cancel)
                        .await
                        .map_err(|message| error(INTERNAL_ERROR, message))?
                }
                None => return Err(unknown_session()),
            },
            Backend::Attach(relay) => relay
                .client
                .control_message_with_channel(session_id, text, channel)
                .await
                .map_err(|failure| relay.task_control_error(failure))?,
        };
        result(reply)
    }

    async fn run_command(
        &self,
        session_id: &str,
        command: command::Command<'_>,
        cancel: &CancellationToken,
    ) -> Reply {
        let end_turn = || result(PromptResponse::new(StopReason::EndTurn));
        match (&self.backend, command) {
            (Backend::Local(local), command) => {
                let session = local.session(session_id).ok_or_else(unknown_session)?;
                match command {
                    command::Command::Permissions { args } => {
                        let text = session
                            .controller
                            .permissions_command(args)
                            .map_err(|e| invalid_params(&e))?;
                        self.say(session_id, &text);
                        end_turn()
                    }
                    command::Command::Auto { args } => {
                        let text = session
                            .controller
                            .auto_mode_command(args)
                            .map_err(|e| invalid_params(&e))?;
                        self.say(session_id, &text);
                        end_turn()
                    }
                    command::Command::Model { profile } => {
                        if profile.is_empty() {
                            let info = session.controller.info();
                            let available = session.controller.profiles();
                            self.say(
                                session_id,
                                &command::model_text(
                                    &info.profile,
                                    &info.provider,
                                    &info.model,
                                    &info.thinking,
                                    &available,
                                ),
                            );
                            return end_turn();
                        }
                        match session.controller.switch_profile_result(profile).await {
                            Ok(switched) => {
                                let info = session.controller.info();
                                self.say(
                                    session_id,
                                    &command::model_switched_text(
                                        &info.profile,
                                        &info.provider,
                                        &info.model,
                                        &info.thinking,
                                    ),
                                );
                                for warning in switched.warnings {
                                    self.say(session_id, &local.redact(&warning));
                                }
                                end_turn()
                            }
                            Err(message) => Err(invalid_params(local.redact(&message))),
                        }
                    }
                    command::Command::Context => {
                        let report = session.controller.context_report();
                        self.say(
                            session_id,
                            &report.map_or(command::CONTEXT_UNAVAILABLE.to_string(), |report| {
                                command::context_text(&report)
                            }),
                        );
                        end_turn()
                    }
                    command::Command::Compact { focus } => {
                        let outcome = session
                            .controller
                            .compact(
                                focus,
                                &mut |event| {
                                    if let Some(update) =
                                        update::event_update(&update::local_wire(&event))
                                    {
                                        self.update(session_id, update);
                                    }
                                },
                                cancel,
                            )
                            .await;
                        match outcome {
                            Ok(done) => {
                                self.say(
                                    session_id,
                                    &command::compaction_text(&to_wire_compaction(&done)),
                                );
                                end_turn()
                            }
                            Err(_) if cancel.is_cancelled() => {
                                result(PromptResponse::new(StopReason::Cancelled))
                            }
                            Err(failure) => {
                                Err(error(INTERNAL_ERROR, local.redact(&failure.to_string())))
                            }
                        }
                    }
                    command::Command::Reflect { focus } => {
                        if let Some(text) = crate::cli::repl_commands::reflect_runs_report(
                            &session.controller,
                            focus,
                        ) {
                            self.say(session_id, &local.redact(&text));
                            return end_turn();
                        }
                        match session.controller.reflect(focus, cancel).await {
                            Ok(report) => {
                                self.say(session_id, &report.summary());
                                end_turn()
                            }
                            Err(_) if cancel.is_cancelled() => {
                                result(PromptResponse::new(StopReason::Cancelled))
                            }
                            Err(failure) => {
                                Err(error(INTERNAL_ERROR, local.redact(&failure.to_string())))
                            }
                        }
                    }
                    command::Command::Skill { args } => {
                        let words: Vec<_> = args.split_whitespace().collect();
                        let text = match words.as_slice() {
                            ["generated"] => crate::cli::repl_commands::skill_report(
                                &session.controller,
                                "generated",
                            ),
                            ["revert", name] => crate::cli::repl_commands::skill_report(
                                &session.controller,
                                &format!("revert {name}"),
                            ),
                            _ => "Usage: /skill generated | /skill revert <name>".to_string(),
                        };
                        self.say(session_id, &text);
                        end_turn()
                    }
                    command::Command::Steer { guidance } => {
                        match session.controller.steer(guidance) {
                            Ok(message) => {
                                self.say(
                                    session_id,
                                    &format!(
                                        "Guidance queued (#{}): {}\nIt will be delivered at the next safe boundary.",
                                        message.id, message.text
                                    ),
                                );
                                end_turn()
                            }
                            Err(message) => Err(error(INTERNAL_ERROR, message)),
                        }
                    }
                    command::Command::Queue { .. } => Err(invalid_params(
                        "nested /queue is not supported; submit a top-level /queue prompt",
                    )),
                }
            }
            (Backend::Attach(relay), command) => {
                let text = relay.run_command(command, session_id, cancel).await?;
                match text {
                    Some(text) => {
                        self.say(session_id, &text);
                        end_turn()
                    }
                    None => result(PromptResponse::new(StopReason::Cancelled)),
                }
            }
        }
    }

    /// Runs one local prompt, including the approval retries.
    async fn local_prompt(
        &self,
        local: &Local,
        session: &Session,
        session_id: &str,
        text: String,
        (channel, origin): (Option<Channel>, Option<Origin>),
        cancel: &CancellationToken,
    ) -> Reply {
        let outcome = session
            .controller
            .run_with_channel(
                Step::Text(&text),
                channel,
                origin,
                &mut |event| {
                    if let Some(update) = update::event_update(&update::local_wire(&event)) {
                        self.update(session_id, update);
                    }
                },
                cancel,
                |request| async move {
                    let scope = session.controller.approval_scope();
                    if let Ok(shared) = session.controller.approval_service().get(
                        &request.approval_id,
                        &scope.workspace,
                        Some(&scope.session_id),
                    ) {
                        let shared = match session
                            .controller
                            .approval_service()
                            .wait_actionable(&shared.id, &shared.scope, cancel)
                            .await
                        {
                            Ok(shared) => shared,
                            Err(_) => return ApprovalDecision::Deny,
                        };
                        if !shared.is_human_pending() {
                            return match shared.state {
                                crate::approval::State::Approved => ApprovalDecision::Allow,
                                _ if cancel.is_cancelled() => ApprovalDecision::Cancelled,
                                _ => ApprovalDecision::Deny,
                            };
                        }
                        if shared.kind == crate::approval::Kind::Confirmation
                            || self
                                .client_capabilities
                                .lock()
                                .expect("ACP capabilities")
                                .pointer("/_meta/otto/approvals/version")
                                .and_then(serde_json::Value::as_u64)
                                == Some(1)
                        {
                            return self
                                .confirmation(&session.controller, &shared, cancel)
                                .await;
                        }
                        return approval::shared_request_permission(
                            self,
                            &session.controller,
                            &shared,
                            &request,
                            cancel,
                        )
                        .await;
                    }
                    request_permission(
                        self,
                        session_id,
                        &request.tool_call_id,
                        &request.command,
                        &request.read_path,
                        &request.justification,
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

/// Runs one wake turn when a notification is pending and nothing else holds
/// the session. Returns true when the turn failed.
async fn wake_turn(session: &Session, stop: &CancellationToken) -> bool {
    // An empty check must not release the slot: that release would wake this
    // same loop again even though no notification arrived.
    if session
        .controller
        .subagent_tasks()
        .is_none_or(|tasks| tasks.pending() == 0)
    {
        return false;
    }
    let Some(token) = session.prompt.begin_wake(stop) else {
        return false;
    };
    let controller = &session.controller;
    let failure = match controller.prepare_wake() {
        Ok(None) => None,
        Ok(Some(wake)) => controller
            .run_with_channel(
                Step::Wake(wake),
                None,
                None,
                &mut |_| {},
                &token,
                |_| async { ApprovalDecision::Deny },
            )
            .await
            .err()
            .map(|failure| failure.to_string()),
        Err(message) if message == PROMPT_ACTIVE || message == CLOSED => None,
        Err(message) => Some(message),
    };
    session.prompt.end();
    if let Some(message) = &failure {
        eprintln!(
            "acp: wake turn failed: {}",
            controller.redact_error(message)
        );
    }
    failure.is_some()
}

async fn watch_controller_notices(
    connection: Arc<Connection>,
    controller: Arc<Controller>,
    session_id: String,
    stop: CancellationToken,
    mut changed: tokio::sync::watch::Receiver<u64>,
    mut after: u64,
    wake_replies: bool,
) {
    // A receiver subscribed before a line was queued does not fire for it
    // again, so read what is already queued before the first wait.
    loop {
        if stop.is_cancelled() {
            return;
        }
        let last = if wake_replies {
            let (notices, last) = controller.wake_replies_since(after);
            for (reply_id, text) in notices {
                connection.wake_reply(&session_id, reply_id, &text);
            }
            last
        } else {
            let (notices, last) = controller.routed_notices_since(after);
            for notice in notices {
                connection.reflection_notice(
                    &session_id,
                    notice.id,
                    &notice.text,
                    &notice.presentation_origin,
                );
            }
            last
        };
        after = last;
        tokio::select! {
            biased;
            () = stop.cancelled() => return,
            result = changed.changed() => if result.is_err() { return },
        }
    }
}

async fn notice_watcher_ready(
    mut start: tokio::sync::watch::Receiver<bool>,
    stop: &CancellationToken,
) -> bool {
    tokio::select! {
        biased;
        () = stop.cancelled() => false,
        result = start.wait_for(|ready| *ready) => result.is_ok(),
    }
}

fn emit_notices(
    connection: &Connection,
    session_id: &str,
    notices: Vec<(u64, String)>,
    wake_replies: bool,
) {
    for (notice_id, text) in notices {
        if wake_replies {
            connection.wake_reply(session_id, notice_id, &text);
        } else {
            connection.reflection_notice(session_id, notice_id, &text, "");
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
    /// Opens the session and returns its history and plan for replay.
    async fn load_session(
        &self,
        id: &str,
        workspace: &Path,
    ) -> Result<(Vec<Message>, Vec<otto_core::session::PlanStep>), Error> {
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
        Ok((session.controller.history(), session.controller.info().plan))
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
fn parse_prompt(params: &Value) -> Result<(String, String, Option<Channel>), Error> {
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
    let channel_value = params
        .get("_meta")
        .and_then(|meta| meta.get("otto"))
        .and_then(|otto| otto.get("channel"));
    let channel = match channel_value {
        None => None,
        Some(Value::String(value)) => Some(
            serde_json::from_value::<Channel>(Value::String(value.clone()))
                .map_err(|_| invalid_params("_meta.otto.channel must be telegram or feishu"))?,
        ),
        Some(_) => return Err(invalid_params("_meta.otto.channel must be a string")),
    };
    Ok((session_id, text, channel))
}

/// The chat origin of a `session/prompt`, from `_meta.otto.chat`
/// (`<platform>:<chat id>`). A missing or invalid value, or a platform that
/// differs from the channel's, gives no origin; stdout is not touched.
fn parse_origin(params: &Value, channel: Option<Channel>) -> Option<Origin> {
    let value = params
        .get("_meta")
        .and_then(|meta| meta.get("otto"))
        .and_then(|otto| otto.get("chat"))?;
    let origin = value
        .as_str()
        .ok_or_else(|| "_meta.otto.chat must be a string".to_string())
        .and_then(|chat| {
            let (platform, id) = chat
                .split_once(':')
                .ok_or_else(|| "_meta.otto.chat must be <platform>:<chat id>".to_string())?;
            Origin::chat(platform, id)
        });
    let origin = origin.and_then(|origin| match (channel, origin.chat_platform()) {
        (Some(channel), Some(platform)) if channel_name(channel) != platform => {
            Err("_meta.otto.chat platform differs from _meta.otto.channel".to_string())
        }
        _ => Ok(origin),
    });
    match origin {
        Ok(origin) => Some(origin),
        Err(message) => {
            eprintln!("acp: ignoring origin: {message}");
            None
        }
    }
}

/// The correlation ID of a `session/prompt`, from `_meta.otto.requestId`,
/// reduced by the same rule as the `X-Request-Id` header. A missing,
/// non-string, or empty-after-sanitizing value is absent; it never rejects
/// the prompt.
fn parse_request_id(params: &Value) -> Option<String> {
    let raw = params
        .get("_meta")?
        .get("otto")?
        .get("requestId")?
        .as_str()?;
    Some(crate::log::request_id(raw.as_bytes())).filter(|id| !id.is_empty())
}

/// The idempotency key of a `session/prompt`, from `_meta.otto.inputId`: a
/// retry with the same id in the same session runs once. Unlike
/// `requestId`, a malformed value rejects the prompt.
fn parse_input_id(
    params: &Value,
) -> Result<Option<String>, agent_client_protocol_schema::v1::Error> {
    let Some(raw) = params
        .get("_meta")
        .and_then(|meta| meta.get("otto"))
        .and_then(|otto| otto.get("inputId"))
    else {
        return Ok(None);
    };
    match raw.as_str() {
        Some(id) if otto_core::model::is_input_id(id) => Ok(Some(id.to_string())),
        _ => Err(invalid_params(crate::server::INVALID_INPUT_ID)),
    }
}

fn channel_name(channel: Channel) -> &'static str {
    match channel {
        Channel::Telegram => "telegram",
        Channel::Feishu => "feishu",
    }
}

/// Advertise memory review only when the controller lives in this process.
fn initialize_result(local: bool) -> Reply {
    let capabilities = AgentCapabilities::new()
        .load_session(true)
        .prompt_capabilities(PromptCapabilities::new())
        .mcp_capabilities(McpCapabilities::new())
        .session_capabilities(SessionCapabilities::new().list(SessionListCapabilities::new()));
    let capabilities = match local {
        true => capabilities.meta(serde_json::Map::from_iter([(
            "otto".to_string(),
            json!({"memoryReview": true, "approvalDialogue":true, "channelContext": true, "taskControl": true, "scheduleDelivery": true, "scheduleDeliveryChats": true, "approvalMode":{"version":1},"approvalPolicy":{"version":1},"approvals":{"version":1}}),
        )])),
        false => capabilities.meta(serde_json::Map::from_iter([(
            "otto".to_string(),
            json!({"approvalDialogue":true, "channelContext": true, "taskControl": true, "scheduleDelivery": true, "scheduleDeliveryChats": true, "approvalMode":{"version":1},"approvalPolicy":{"version":1},"approvals":{"version":1}}),
        )])),
    };
    result(
        InitializeResponse::new(ProtocolVersion::V1)
            .agent_capabilities(capabilities)
            .auth_methods(vec![])
            .agent_info(Implementation::new("otto", crate::VERSION)),
    )
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
                    let dropped = slot.cancel();
                    // Cancelled for good: a restart must not run them.
                    if let Some(journal) = session_id
                        .and_then(|id| self.connection.local_controller(id))
                        .and_then(|controller| controller.turn_queue())
                    {
                        journal.clear();
                    }
                    if dropped > 0 {
                        self.connection.say(
                            session_id.expect("slot requires session id"),
                            &format!("Canceled {dropped} queued follow-up prompt(s)."),
                        );
                    }
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
                let local = match &connection.backend {
                    Backend::Local(_) => true,
                    Backend::Attach(relay) => relay.server.is_some(),
                };
                let platforms = schedule::delivery_platforms(&params);
                if let Err(message) = schedule::delivery_chats(&params, &platforms) {
                    return connection.reply(id, Err(invalid_params(message)));
                }
                *connection
                    .client_capabilities
                    .lock()
                    .expect("ACP capabilities") = params.clone();
                if !self.scheduling
                    && let Some(worker) = schedule::worker(&connection, platforms)
                {
                    self.scheduling = true;
                    let stop = self.stop.clone();
                    self.tasks.spawn(worker.run(stop));
                }
                connection.reply(id, initialize_result(local));
            }
            "session/new" => {
                self.tasks.spawn(async move {
                    let reply = connection.new_session(params).await;
                    connection.reply(id, reply.clone());
                    connection.advertise_commands(&reply);
                    connection.notices_ready(&reply);
                });
            }
            "session/load" => {
                self.tasks.spawn(async move {
                    let reply = connection.load_session(params).await;
                    connection.reply(id, reply.clone());
                    connection.advertise_commands(&reply);
                    connection.notices_ready(&reply);
                });
            }
            "session/list" => {
                self.tasks.spawn(async move {
                    let reply = connection.list_sessions(params).await;
                    connection.reply(id, reply);
                });
            }
            "session/prompt" => self.start_prompt(id, &params),
            "_otto/tasks/steer" => {
                self.tasks.spawn(async move {
                    #[derive(Deserialize)]
                    #[serde(rename_all = "camelCase", deny_unknown_fields)]
                    struct Input {
                        session_id: String,
                        text: String,
                    }
                    let reply = match serde_json::from_value::<Input>(params) {
                        Err(failure) => Err(invalid_params(failure.to_string())),
                        Ok(input) if input.text.trim().is_empty() => Err(invalid_params(
                            "text must contain non-whitespace characters",
                        )),
                        Ok(input) if input.text.len() > 32_000 => {
                            Err(invalid_params("text must be at most 32000 UTF-8 bytes"))
                        }
                        Ok(input) => match &connection.backend {
                            Backend::Local(local) => match local.session(&input.session_id) {
                                None => Err(unknown_session()),
                                Some(session) => session
                                    .controller
                                    .steer(&input.text)
                                    .map(|message| {
                                        serde_json::to_value(message)
                                            .expect("steering message serializes")
                                    })
                                    .map_err(|failure| error(INTERNAL_ERROR, failure)),
                            },
                            Backend::Attach(relay) => relay
                                .client
                                .steer(&input.session_id, &input.text)
                                .await
                                .map(|message| {
                                    serde_json::to_value(message)
                                        .expect("steering message serializes")
                                })
                                .map_err(|failure| relay.task_control_error(failure)),
                        },
                    };
                    connection.reply(id, reply);
                });
            }
            "_otto/tasks/control" => {
                self.tasks.spawn(async move {
                    #[derive(Deserialize)]
                    #[serde(rename_all = "camelCase", deny_unknown_fields)]
                    struct Input {
                        session_id: String,
                        text: String,
                        #[serde(default)]
                        channel: Option<Channel>,
                    }
                    let reply = match serde_json::from_value::<Input>(params) {
                        Err(failure) => Err(invalid_params(failure.to_string())),
                        Ok(input) if input.text.trim().is_empty() => Err(invalid_params(
                            "text must contain non-whitespace characters",
                        )),
                        Ok(input) if input.text.len() > 32_000 => {
                            Err(invalid_params("text must be at most 32000 UTF-8 bytes"))
                        }
                        Ok(input) => {
                            connection
                                .control_message(&input.session_id, &input.text, input.channel)
                                .await
                        }
                    };
                    connection.reply(id, reply);
                });
            }
            "_otto/approvals/message" => {
                self.tasks.spawn(async move {
                    #[derive(Deserialize)]
                    #[serde(rename_all = "camelCase", deny_unknown_fields)]
                    struct Input {
                        session_id: String,
                        text: String,
                        #[serde(default)]
                        channel: Option<Channel>,
                    }
                    let reply = match serde_json::from_value::<Input>(params) {
                        Err(failure) => Err(invalid_params(failure.to_string())),
                        Ok(input) => match &connection.backend {
                            Backend::Local(local) => match local.session(&input.session_id) {
                                None => Err(unknown_session()),
                                Some(session) => {
                                    let cancel = session.prompt.active_cancel();
                                    match session
                                        .controller
                                        .approval_message_with_channel(
                                            &input.text,
                                            input.channel,
                                            &cancel,
                                        )
                                        .await
                                    {
                                        Ok(reply) => result(reply),
                                        Err(failure) => Err(error(INTERNAL_ERROR, failure)),
                                    }
                                }
                            },
                            Backend::Attach(relay) => match relay
                                .client
                                .approval_message_with_channel(
                                    &input.session_id,
                                    &input.text,
                                    input.channel,
                                )
                                .await
                            {
                                Ok(reply) => result(reply),
                                Err(failure) => Err(error(INTERNAL_ERROR, failure.to_string())),
                            },
                        },
                    };
                    connection.reply(id, reply);
                });
            }
            "_otto/approval_policy/get" | "_otto/approval_policy/set" => {
                self.tasks.spawn(async move {
                    let reply = connection.approval_policy(&method, params).await;
                    connection.reply(id, reply);
                });
            }
            "_otto/approval_mode/get" | "_otto/approval_mode/set" => {
                self.tasks.spawn(async move {
                    let reply = connection.approval_mode(&method, params).await;
                    connection.reply(id, reply);
                });
            }
            "_otto/approvals/list"
            | "_otto/approvals/watch"
            | "_otto/approvals/get"
            | "_otto/approvals/decide"
            | "_otto/approvals/withdraw" => {
                self.tasks.spawn(async move {
                    let reply = connection.shared_approval(&method, params).await;
                    connection.reply(id, reply);
                });
            }
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
        let (session_id, text, channel) = match parse_prompt(params) {
            Ok(parsed) => parsed,
            Err(failure) => return connection.reply(id, Err(failure)),
        };
        let origin = parse_origin(params, channel);
        let request_id = parse_request_id(params);
        let input_id = match parse_input_id(params) {
            Ok(input_id) => input_id,
            Err(failure) => return connection.reply(id, Err(failure)),
        };
        let Some(slot) = connection.slot(&session_id) else {
            return connection.reply(id, Err(unknown_session()));
        };
        match command::parse(&text) {
            Some(command::Command::Steer { guidance }) => {
                let guidance = guidance.to_string();
                self.tasks.spawn(async move {
                    let reply = connection.steer_prompt(&session_id, &guidance).await;
                    connection.reply(id, reply);
                });
                return;
            }
            Some(command::Command::Queue { prompt }) => {
                if prompt.trim().is_empty() {
                    return connection.reply(
                        id,
                        Err(invalid_params("/queue requires a non-empty prompt")),
                    );
                }
                if prompt.len() > 32_000 {
                    return connection.reply(
                        id,
                        Err(invalid_params(
                            "/queue prompt must be at most 32000 UTF-8 bytes",
                        )),
                    );
                }
                let prompt = prompt.to_string();
                match &connection.backend {
                    Backend::Local(_) => {
                        // Saved before it is accepted, so a stop before it runs
                        // does not lose it.
                        let saved_id = input_id.clone().unwrap_or_else(|| {
                            format!("queued:{}", crate::runtime::sidecar_mailbox::new_input_id())
                        });
                        let journal = connection
                            .local_controller(&session_id)
                            .and_then(|controller| controller.turn_queue());
                        if let Some(journal) = &journal
                            && let Err(message) =
                                journal.save(&crate::app::turn_queue::QueuedPrompt {
                                    input_id: saved_id.clone(),
                                    turn_id: String::new(),
                                    text: prompt.clone(),
                                    channel,
                                    origin: origin.clone(),
                                    image_dropped: false,
                                })
                        {
                            return connection.reply(id, Err(error(INTERNAL_ERROR, message)));
                        }
                        match slot.enqueue(prompt, (channel, origin), Some(saved_id.clone())) {
                            Ok(start_worker) => {
                                connection.say(
                                &session_id,
                                "Follow-up accepted; its answer will arrive as a later session update.",
                            );
                                connection
                                    .reply(id, result(PromptResponse::new(StopReason::EndTurn)));
                                if start_worker {
                                    let worker_slot = Arc::clone(&slot);
                                    let worker_connection = Arc::clone(&connection);
                                    let worker_session = session_id.clone();
                                    self.tasks.spawn(async move {
                                        worker_slot
                                            .drain_queue(worker_connection, worker_session)
                                            .await;
                                    });
                                }
                            }
                            Err(message) => {
                                if let Some(journal) = &journal {
                                    journal.forget(&saved_id);
                                }
                                connection.reply(id, Err(error(INTERNAL_ERROR, message)))
                            }
                        }
                    }
                    Backend::Attach(_) => {
                        let Ok((followup_id, token)) = slot.begin_followup(&self.stop) else {
                            return connection.reply(
                                id,
                                Err(error(INTERNAL_ERROR, "queue is full (16 pending prompts)")),
                            );
                        };
                        self.tasks.spawn(async move {
                            let Backend::Attach(relay) = &connection.backend else {
                                unreachable!("backend remains attach for this request")
                            };
                            slot.wait_followup_admission(followup_id).await;
                            if token.is_cancelled() {
                                slot.finish_followup_admission(followup_id);
                                connection.say(&session_id, "Queued follow-up canceled before it started.");
                                connection.reply(
                                    id,
                                    result(PromptResponse::new(StopReason::EndTurn)),
                                );
                                slot.end_followup(followup_id);
                                return;
                            }
                            let started = relay
                                .start_queued_prompt(
                                    &session_id,
                                    &prompt,
                                    channel,
                                    origin.as_ref(),
                                    request_id.as_deref(),
                                )
                                .await;
                            match started {
                                Ok(stream) => {
                                    connection.say(
                                        &session_id,
                                        "Follow-up accepted; its answer will arrive as a later session update.",
                                    );
                                    connection.reply(
                                        id,
                                        result(PromptResponse::new(StopReason::EndTurn)),
                                    );
                                    slot.finish_followup_admission(followup_id);
                                    let reply = relay
                                        .stream_turn(&connection, &session_id, stream, &token)
                                        .await;
                                    if let Some(notice) = queued_outcome_notice(&reply) {
                                        connection.say(&session_id, &notice);
                                    }
                                }
                                Err(failure) => {
                                    connection.reply(id, Err(failure));
                                    slot.finish_followup_admission(followup_id);
                                }
                            }
                            slot.end_followup(followup_id);
                        });
                    }
                }
                return;
            }
            _ => {}
        }
        let stop = self.stop.clone();
        self.tasks.spawn(async move {
            match connection
                .admit_prompt(&session_id, &text, input_id.as_deref())
                .await
            {
                Ok(ActiveInputAdmission::Steered) => {
                    connection.say(
                        &session_id,
                        "Guidance accepted for the active task; it will be delivered at the next safe boundary.",
                    );
                    connection.reply(id, result(PromptResponse::new(StopReason::EndTurn)));
                    return;
                }
                Ok(ActiveInputAdmission::ApprovalPending) => {
                    connection.say(
                        &session_id,
                        "The active task is waiting for approval; resolve that approval before sending task guidance.",
                    );
                    connection.reply(id, result(PromptResponse::new(StopReason::EndTurn)));
                    return;
                }
                Ok(ActiveInputAdmission::Idle) => {}
                Err(failure) => {
                    connection.reply(id, Err(failure));
                    return;
                }
            }

            let Some(token) = slot.begin(&stop) else {
                // A wake turn is not a prompt: wait for it, then run.
                if let Some(token) = slot.wait_for_wake(&stop) {
                    if !slot.claim_after_wake(&token).await {
                        return connection
                            .reply(id, result(PromptResponse::new(StopReason::Cancelled)));
                    }
                    let reply = connection
                        .prompt(
                            &session_id,
                            text,
                            channel,
                            origin,
                            (request_id.as_deref(), input_id.as_deref()),
                            &token,
                        )
                        .await;
                    slot.end();
                    connection.reply(id, reply);
                    return;
                }
                return connection.reply(
                    id,
                    Err(error(INTERNAL_ERROR, "a prompt is already running")),
                );
            };
            let reply = connection
                .prompt(
                    &session_id,
                    text,
                    channel,
                    origin,
                    (request_id.as_deref(), input_id.as_deref()),
                    &token,
                )
                .await;
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
mod channel_tests {
    use super::*;

    #[test]
    fn task_control_errors_distinguish_old_serve_from_missing_session() {
        let unsupported = task_control_error(crate::client::Error::Http {
            status: 404,
            code: String::new(),
            message: String::new(),
        });
        let unsupported = serde_json::to_value(unsupported).unwrap();
        assert_eq!(unsupported["code"], METHOD_NOT_FOUND);
        assert!(
            unsupported["message"]
                .as_str()
                .unwrap()
                .contains("upgrade and restart")
        );

        let missing = task_control_error(crate::client::Error::Http {
            status: 404,
            code: "not_found".into(),
            message: "session not found".into(),
        });
        let missing = serde_json::to_value(missing).unwrap();
        assert_eq!(missing["code"], INTERNAL_ERROR);
        assert_eq!(missing["message"], "HTTP 404 not_found: session not found");
    }

    fn prompt(meta: Value) -> Value {
        json!({
            "sessionId": "s1",
            "prompt": [{"type":"text", "text":"plain user text"}],
            "_meta": meta,
        })
    }

    #[test]
    fn channel_metadata_is_structured_and_never_changes_prompt_text() {
        let (session, text, channel) = parse_prompt(&prompt(json!({
            "otto": {"channel": "telegram"}
        })))
        .unwrap();
        assert_eq!(session, "s1");
        assert_eq!(text, "plain user text");
        assert_eq!(channel, Some(Channel::Telegram));
    }

    #[tokio::test]
    async fn reloading_a_session_rebinds_notices_to_the_current_controller() {
        let workspace = tempfile::tempdir().unwrap();
        let sessions = tempfile::tempdir().unwrap();
        let first =
            Arc::new(crate::cli::testutil::controller(workspace.path(), sessions.path()).await);
        let second =
            Arc::new(crate::cli::testutil::controller(workspace.path(), sessions.path()).await);
        let id = first.info().session_id;
        let builder = Arc::clone(first.builder());
        let (out, mut frames) = mpsc::unbounded_channel();
        let connection = Arc::new(Connection {
            workspace: workspace.path().to_path_buf(),
            backend: Backend::Local(Box::new(Local {
                config: Config {
                    runtime: crate::cli::testutil::initial_runtime(&builder),
                    builder,
                    workspace: workspace.path().to_path_buf(),
                    sandbox: None,
                },
                sessions: Mutex::new(HashMap::from([(
                    id.clone(),
                    Arc::new(Session {
                        controller: Arc::clone(&first),
                        prompt: Arc::new(PromptSlot::default()),
                    }),
                )])),
            })),
            out,
            pending: Mutex::new(HashMap::new()),
            next_request_id: AtomicU64::new(1),
            client_capabilities: Mutex::new(Value::Null),
            task_stop: CancellationToken::new(),
            notice_stop: CancellationToken::new(),
            watching_notices: Mutex::new(HashMap::new()),
            notice_ready: Mutex::new(HashMap::new()),
            wake_loops: Mutex::new(HashMap::new()),
        });
        connection.start_notice_watcher(&id).await;
        connection.notices_ready(&Ok(json!({"sessionId": id})));
        first.push_notice("before close");
        let frame = tokio::time::timeout(std::time::Duration::from_secs(2), frames.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            frame
                .pointer("/params/update/content/text")
                .and_then(Value::as_str),
            Some("before close")
        );
        let previous = connection
            .watching_notices
            .lock()
            .unwrap()
            .get(&id)
            .unwrap()
            .stop
            .clone();
        connection.start_notice_watcher(&id).await;
        assert!(
            !previous.is_cancelled(),
            "same controller keeps its subscription"
        );
        let Backend::Local(local) = &connection.backend else {
            unreachable!()
        };
        local.sessions.lock().unwrap().insert(
            id.clone(),
            Arc::new(Session {
                controller: Arc::clone(&second),
                prompt: Arc::new(PromptSlot::default()),
            }),
        );
        connection.start_notice_watcher(&id).await;
        connection.notices_ready(&Ok(json!({"sessionId": id})));
        assert!(
            previous.is_cancelled(),
            "retired controller subscription must stop"
        );
        first.push_notice("old controller");
        second.push_notice("new controller");
        let frame = tokio::time::timeout(std::time::Duration::from_secs(2), frames.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            frame
                .pointer("/params/update/content/text")
                .and_then(Value::as_str),
            Some("new controller")
        );
        connection.notice_stop.cancel();
        first.close().unwrap();
        second.close().unwrap();
    }

    #[tokio::test]
    async fn idle_wake_checks_do_not_signal_slot_release() {
        let workspace = tempfile::tempdir().unwrap();
        let sessions = tempfile::tempdir().unwrap();
        let controller = crate::cli::testutil::controller(workspace.path(), sessions.path()).await;
        let session = Session {
            controller: Arc::new(controller),
            prompt: Arc::new(PromptSlot::default()),
        };
        let tasks = session.controller.subagent_tasks().expect("task registry");
        assert_eq!(tasks.pending(), 0);
        let mut released = session.prompt.released.subscribe();
        let stop = CancellationToken::new();
        for _ in 0..3 {
            assert!(!wake_turn(&session, &stop).await);
            assert!(
                !released.has_changed().unwrap(),
                "idle checks must stay asleep"
            );
            assert!(session.prompt.begin(&stop).is_some());
            session.prompt.end();
            // A real prompt completion still wakes the loop.
            assert!(released.has_changed().unwrap());
            released.borrow_and_update();
        }
    }

    #[tokio::test]
    async fn a_prompt_is_admitted_when_the_wake_turn_ends_between_begin_and_wait() {
        let slot = PromptSlot::default();
        let parent = CancellationToken::new();
        let wake = slot.begin_wake(&parent).expect("wake turn claims the slot");
        assert!(
            slot.begin(&parent).is_none(),
            "the wake turn holds the slot"
        );
        // The wake turn ends before the prompt registers its wait.
        slot.end();
        drop(wake);
        let token = slot
            .wait_for_wake(&parent)
            .expect("a slot freed after begin failed admits the prompt");
        assert!(slot.claim_after_wake(&token).await);
        assert!(
            slot.begin(&parent).is_none(),
            "the admitted prompt holds the slot"
        );
    }

    #[tokio::test]
    async fn attached_followup_admission_is_fifo_and_cancellation_keeps_its_turn() {
        let slot = Arc::new(PromptSlot::default());
        let parent = CancellationToken::new();
        let (first, first_token) = slot.begin_followup(&parent).unwrap();
        let (second, canceled) = slot.begin_followup(&parent).unwrap();
        let waiting_slot = Arc::clone(&slot);
        let waiting = tokio::spawn(async move {
            waiting_slot.wait_followup_admission(second).await;
            second
        });
        tokio::task::yield_now().await;
        assert!(
            !waiting.is_finished(),
            "second admission must wait for first"
        );

        assert_eq!(slot.cancel(), 0);
        assert!(first_token.is_cancelled());
        assert!(canceled.is_cancelled());
        slot.finish_followup_admission(first);
        assert_eq!(waiting.await.unwrap(), second);
        assert!(canceled.is_cancelled());
        slot.finish_followup_admission(second);
    }

    #[test]
    fn input_id_metadata_is_optional_but_must_be_well_formed() {
        assert_eq!(parse_input_id(&json!({})).unwrap(), None);
        assert_eq!(
            parse_input_id(&json!({"_meta": {"otto": {"inputId": "chat:telegram:c1:7"}}})).unwrap(),
            Some("chat:telegram:c1:7".to_string())
        );
        assert!(parse_input_id(&json!({"_meta": {"otto": {"inputId": "bad id"}}})).is_err());
        assert!(parse_input_id(&json!({"_meta": {"otto": {"inputId": 7}}})).is_err());
    }

    #[test]
    fn unknown_metadata_is_ignored_but_invalid_channel_is_rejected() {
        let (_, text, channel) =
            parse_prompt(&prompt(json!({ "other": {"channel": "slack"} }))).unwrap();
        assert_eq!(text, "plain user text");
        assert_eq!(channel, None);
        let error = parse_prompt(&prompt(json!({
            "otto": {"channel": "slack"}
        })))
        .unwrap_err();
        let code: i32 = error.code.into();
        assert_eq!(code, INVALID_PARAMS);
    }

    #[test]
    fn chat_metadata_becomes_an_origin_only_when_valid_and_consistent() {
        let chat = |value: Value, channel| {
            parse_origin(&prompt(json!({"otto": {"chat": value}})), channel)
        };
        let telegram = Some(Channel::Telegram);
        assert_eq!(
            chat(json!("telegram:123"), None).unwrap().as_str(),
            "chat:telegram:123"
        );
        assert_eq!(
            chat(json!("feishu:oc:1"), None).unwrap().as_str(),
            "chat:feishu:oc:1"
        );
        assert!(chat(json!("telegram:123"), telegram).is_some());
        assert_eq!(chat(json!("feishu:oc_1"), telegram), None);
        assert_eq!(chat(json!("telegram123"), None), None);
        assert_eq!(chat(json!("slack:1"), None), None);
        assert_eq!(chat(json!("telegram:"), None), None);
        assert_eq!(chat(json!(5), None), None);
        assert_eq!(parse_origin(&prompt(json!({})), None), None);
    }

    #[test]
    fn channel_context_capability_is_advertised() {
        for local in [false, true] {
            let reply = initialize_result(local).unwrap();
            assert_eq!(
                reply["agentCapabilities"]["_meta"]["otto"]["channelContext"],
                true
            );
            assert_eq!(
                reply["agentCapabilities"]["_meta"]["otto"]["scheduleDelivery"],
                true
            );
        }
    }
}
