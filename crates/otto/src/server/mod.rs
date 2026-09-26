//! `otto serve`: the HTTP+JSON+SSE frontend.
//!
//! One [`app::Controller`] per session, a turn event buffer decoupled from the
//! agent's synchronous emit callback ([`turn::Turn`]), Prometheus metrics
//! ([`metrics::Metrics`]), and structured request logging. See
//! `docs/specs/2026-09-03-agent-server-design.md`.
//!
//! Concurrency: `sessions` is a plain `Mutex<HashMap>` held only for map
//! operations. Each [`OpenSession`] has its own `Mutex` for the turn and
//! compaction slots.

pub mod agents;
pub mod approvals;
pub mod auth;
pub mod compact;
pub mod diff;
pub mod listen;
pub mod mcp;
pub mod metrics;
pub mod sandbox;
pub mod tasks;
pub mod timers;
pub mod turn;
pub mod ui;
pub mod workflows;
pub mod workspaces;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{DefaultBodyLimit, MatchedPath, Path, Query, State};
use axum::http::{HeaderMap, Method, StatusCode, header};
use axum::middleware::Next;
use axum::response::Response;
use axum::routing::{get, post};
use otto_core::agent::inbox::Notification;
use otto_core::model::{Block, MAX_IMAGE_BYTES, Message, Usage};
use otto_core::session::ListResult;
use otto_core::wire::sse::format_frame;
use serde::{Deserialize, Serialize};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use crate::app::{self, Controller};
use crate::cli::info::SandboxInfo;
#[cfg(test)]
use crate::cli::info::{SandboxMode, SandboxNetwork, SandboxReason};
use metrics::{Metrics, SessionContext};
use turn::{TRIGGER_TASK, TRIGGER_USER, Turn};

/// A [`Factory::open`] that answers with exactly this text produces 404
/// `not_found` instead of 500.
pub const SESSION_NOT_FOUND: &str = "session not found";
const IMAGE_TURN_BODY_MAX_BYTES: usize = MAX_IMAGE_BYTES * 4 / 3 + 4096;

/// The embedded API description. Served verbatim at `GET /v1/openapi.yaml`,
/// read from the shared fixture at the repository root so the server and the
/// web UI's wire types cannot drift.
pub const OPENAPI_YAML: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../testdata/server/openapi.yaml"
));

// ---- process-level info ----

/// Process-level static info, unrelated to any session.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Info {
    pub workspace: String,
    pub provider: String,
    pub profile: String,
    pub model: String,
    pub thinking: String,
    pub sandbox: String,
    pub profiles: Vec<String>,
}

/// What a [`Server`] needs from its composition root.
///
/// `list` and `reload_sandbox` return `None` when the composition root wires no
/// such capability.
#[async_trait::async_trait]
pub trait Factory: Send + Sync {
    /// A brand new session. `workspace` is the canonical path of an already
    /// admitted-and-loaded workspace; `None` means the startup workspace.
    async fn create(&self, workspace: Option<&str>) -> Result<Controller, String>;
    /// An existing session by id. [`SESSION_NOT_FOUND`] means 404.
    /// `workspace` searches just that workspace; `None` searches every
    /// loaded workspace.
    async fn open(&self, id: &str, workspace: Option<&str>) -> Result<Controller, String>;
    /// The sessions on disk for `workspace` (`None`: every loaded workspace,
    /// merged newest-first and capped at `MAX_LIST_SESSIONS`). `None` at the
    /// outer `Option` disables the disk half of `GET /v1/sessions`.
    async fn list(&self, _workspace: Option<&str>) -> Option<Result<ListResult, String>> {
        None
    }
    /// Whether `POST /v1/sandbox/reload` is wired at all. Checked before the
    /// turn-active guard.
    fn sandbox_reload_available(&self) -> bool {
        false
    }
    /// Re-reads the sandbox configuration. `None` disables `POST
    /// /v1/sandbox/reload`, which then answers 501.
    async fn reload_sandbox(&self) -> Option<Result<SandboxInfo, String>> {
        None
    }
    /// Reloads every other loaded workspace's own sandbox (the startup
    /// workspace's reload is [`Factory::reload_sandbox`]), by path. A
    /// workspace with no reloader of its own is left out.
    async fn reload_other_sandboxes(&self) -> Vec<(String, Result<SandboxInfo, String>)> {
        Vec::new()
    }
    /// Persisted provider token usage, optionally scoped to one session.
    fn usage_summary(&self, _session_id: Option<&str>) -> Result<crate::usage::Summary, String> {
        Ok(crate::usage::Summary::default())
    }
    fn usage_analysis(
        &self,
        days: u16,
        _session_id: Option<&str>,
    ) -> Result<crate::usage::Analysis, String> {
        crate::usage::Analysis::empty(days).map_err(|error| error.to_string())
    }
    /// Sub-agent task rows from `tasks.db`, across every otto process on the
    /// machine. An empty result when no recorder is wired.
    fn tasks_list(
        &self,
        _query: &crate::subagent::record::ListQuery,
    ) -> Result<crate::subagent::record::ListResult, String> {
        Ok(crate::subagent::record::ListResult::default())
    }
    /// One task row from `tasks.db` by its primary key. `Ok(None)` when no
    /// recorder is wired.
    fn tasks_get(
        &self,
        _parent_session: &str,
        _task_id: &str,
    ) -> Result<Option<crate::subagent::record::TaskRow>, String> {
        Ok(None)
    }
    /// Every loaded workspace, startup first then by path, for `GET
    /// /v1/workspaces`.
    async fn workspaces(&self) -> WorkspaceList;
    /// Admits and loads `path`, or returns the already-loaded workspace.
    /// `(_, true)` when this call built a new host, `(_, false)` when it was
    /// already loaded.
    async fn load_workspace(&self, path: &str)
    -> Result<(WorkspaceInfo, bool), WorkspaceLoadError>;
    /// Unloads `path` and removes it from the persisted workspace list.
    /// `path` is matched literally when it does not resolve (a deleted
    /// directory's persisted entry). The caller has already checked for open
    /// sessions, which `Factory` cannot see; this checks active workflow
    /// runs.
    async fn remove_workspace(&self, path: &str) -> Result<(), WorkspaceRemoveError>;
    /// The loaded workspace's command executor and sandbox environment, for
    /// `GET /v1/workspaces/diff`. `None` when the workspace has no usable
    /// sandbox (also the default for a `Factory` that never wires one, which
    /// answers `501 diff_unavailable`).
    async fn diff_runner(
        &self,
        _workspace: &str,
    ) -> Option<(Arc<dyn crate::sandbox::CommandExecutor>, Vec<String>)> {
        None
    }
    /// The workflow controller for `workspace` (`None`: the startup
    /// workspace). `None` when that workspace has no controller (workflows
    /// disabled there).
    async fn workflow_controller(
        &self,
        workspace: Option<&str>,
    ) -> Option<Arc<crate::workflow::Controller>>;
    /// Every loaded workspace's workflow controller, startup first then by
    /// path. A workspace with workflows disabled is left out.
    async fn workflow_controllers(&self) -> Vec<Arc<crate::workflow::Controller>>;
}

/// One workspace's registration state, independent of any open session.
#[derive(Debug, Clone, Serialize)]
pub struct WorkspaceInfo {
    pub path: String,
    pub workflows: bool,
}

/// [`Factory::workspaces`]'s result.
pub struct WorkspaceList {
    pub startup: String,
    pub roots: Vec<String>,
    /// Startup first, then by path.
    pub loaded: Vec<WorkspaceInfo>,
}

/// Why [`Factory::load_workspace`] refused or failed a path.
pub enum WorkspaceLoadError {
    /// Not an absolute, existing directory.
    Invalid(String),
    /// A real directory, but neither the startup workspace nor a descendant
    /// of a configured root.
    NotAdmitted(String),
    /// Admitted, but opening it failed (sandbox, MCP config, ...). Nothing
    /// was registered.
    Failed(String),
}

/// Why [`Factory::remove_workspace`] refused a path.
#[derive(Debug)]
pub enum WorkspaceRemoveError {
    /// Neither loaded nor in the persisted list.
    NotFound,
    /// The startup workspace, which is never removable.
    IsStartup,
    /// A session is open in that workspace or its workflow controller has an
    /// active run. The message names which.
    InUse(String),
}

/// Configures a [`Server`].
pub struct Options {
    pub factory: Arc<dyn Factory>,
    pub info: Info,
    /// When non-empty, required as `Authorization: Bearer <token>` on every
    /// `/v1/` route. Empty means no check, which is only safe behind a Unix
    /// socket with private file modes.
    pub token: String,
    pub logger: Option<Arc<Logger>>,
    pub workflows: Option<Arc<crate::workflow::Controller>>,
}

// ---- logging ----

/// ponytail: no logging crate is pinned and the server writes a handful of
/// fixed lines, so a small formatter beats adding `tracing` plus a subscriber.
/// Swap for `tracing` if any other crate needs structured logs.
pub struct Logger {
    sink: Mutex<Box<dyn std::io::Write + Send>>,
}

impl std::fmt::Debug for Logger {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("Logger")
    }
}

impl Logger {
    pub fn stderr() -> Self {
        Self::new(Box::new(std::io::stderr()))
    }

    pub fn new(sink: Box<dyn std::io::Write + Send>) -> Self {
        Self {
            sink: Mutex::new(sink),
        }
    }

    pub fn info(&self, message: &str, fields: &[(&str, String)]) {
        self.write("INFO", message, fields);
    }

    pub fn error(&self, message: &str, fields: &[(&str, String)]) {
        self.write("ERROR", message, fields);
    }

    fn write(&self, level: &str, message: &str, fields: &[(&str, String)]) {
        let mut line = format!(
            "time={} level={level} msg={}",
            chrono::Local::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, false),
            quote(message)
        );
        for (key, value) in fields {
            line.push(' ');
            line.push_str(key);
            line.push('=');
            line.push_str(&quote(value));
        }
        line.push('\n');
        let mut sink = self
            .sink
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let _ = sink.write_all(line.as_bytes());
        let _ = sink.flush();
    }
}

/// The log quoting rule: quote an empty value, and any value carrying a space,
/// a quote, an equals sign, a backslash, or a control character.
fn quote(value: &str) -> String {
    let unsafe_byte = value.is_empty()
        || value
            .chars()
            .any(|c| c <= ' ' || c == '"' || c == '=' || c == '\\' || c == '\u{7f}');
    if unsafe_byte {
        serde_json::to_string(value).unwrap_or_else(|_| format!("{value:?}"))
    } else {
        value.to_string()
    }
}

// ---- registry ----

/// One entry in the session registry: an open controller and its most recent
/// turn, if any.
pub struct OpenSession {
    ctrl: Arc<Controller>,
    state: Mutex<SessionState>,
    /// Signaled after every turn on this session finishes. The wake loop is the
    /// only waiter. `Notify::notify_one` stores exactly one permit, so an
    /// end-of-turn signal raised while the loop is busy is not lost.
    turn_finished: tokio::sync::Notify,
    /// Cancelled by [`OpenSession::cancel_work`], which every close path calls
    /// before closing the controller. The registry's `watch` sender is owned by
    /// the registry and outlives the controller, so the loop needs its own stop
    /// signal.
    closed: CancellationToken,
}

#[derive(Default)]
struct SessionState {
    turn: Option<Arc<Turn>>,
    /// Non-`None` while `POST .../compact` runs. `start_turn` and
    /// `handle_compact` both check it under the same lock, so a turn and a
    /// compaction are never admitted together.
    compacting: Option<CancellationToken>,
}

impl OpenSession {
    fn new(ctrl: Controller) -> Arc<Self> {
        Arc::new(Self {
            ctrl: Arc::new(ctrl),
            state: Mutex::new(SessionState::default()),
            turn_finished: tokio::sync::Notify::new(),
            closed: CancellationToken::new(),
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, SessionState> {
        self.state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    }

    fn current_turn(&self) -> Option<Arc<Turn>> {
        self.lock().turn.clone()
    }

    /// Cancels the running turn or compaction so a following close does not
    /// wait on a provider call, and stops the wake loop.
    fn cancel_work(&self) {
        self.closed.cancel();
        let state = self.lock();
        if let Some(turn) = state.turn.as_ref() {
            turn.cancel();
        }
        if let Some(cancel) = state.compacting.as_ref() {
            cancel.cancel();
        }
    }
}

/// `otto serve`'s router plus the session registry and turn lifecycle behind
/// it. Construct with [`Server::new`].
pub struct Server {
    factory: Arc<dyn Factory>,
    info: Info,
    token: String,
    log: Arc<Logger>,
    metrics: Arc<Metrics>,
    sessions: Mutex<HashMap<String, Arc<OpenSession>>>,
    /// Serializes every `Factory::open`, so two concurrent resumes of one id
    /// cannot open the same session file twice (the store takes no flock).
    ///
    /// ponytail: one gate for every id instead of a per-id gate. Resuming is an
    /// admin-rate path; make it per-id if it ever contends.
    open_gate: tokio::sync::Mutex<()>,
    /// One handle per running wake loop, awaited by [`Server::close`].
    wake_loops: Mutex<Vec<tokio::task::JoinHandle<()>>>,
    cancel: CancellationToken,
    workflows: Option<Arc<crate::workflow::Controller>>,
    /// Bumped whenever a `GET /v1/status` snapshot could change: a session
    /// opened or closed, a turn started or finished, a task registry update,
    /// or a granted Bash approval. See [`Server::status_snapshot`].
    status_changed: watch::Sender<u64>,
}

impl Server {
    pub fn new(options: Options) -> Arc<Self> {
        Arc::new(Self {
            factory: options.factory,
            info: options.info,
            token: options.token,
            log: options.logger.unwrap_or_else(|| Arc::new(Logger::stderr())),
            metrics: Arc::new(Metrics::new()),
            sessions: Mutex::new(HashMap::new()),
            open_gate: tokio::sync::Mutex::new(()),
            wake_loops: Mutex::new(Vec::new()),
            cancel: CancellationToken::new(),
            workflows: options.workflows,
            status_changed: watch::channel(0).0,
        })
    }

    /// The parent of every turn and compaction. Cancelling it is the first
    /// half of a graceful shutdown; [`Server::close`] is the second.
    pub fn cancel_token(&self) -> CancellationToken {
        self.cancel.clone()
    }

    pub fn metrics(&self) -> &Arc<Metrics> {
        &self.metrics
    }

    pub fn logger(&self) -> &Arc<Logger> {
        &self.log
    }

    /// Fans `notification` out to every currently open session in the startup
    /// workspace. Sessions in another workspace (opened via `?workspace=` or
    /// `{"workspace": ...}`) are not this host's concern: Feishu inbound is
    /// wired to one workspace only. Sessions that have no task registry
    /// (sub-agents off) drop it. Returns how many sessions received it.
    pub(crate) fn notify_open_sessions(&self, notification: Notification) -> usize {
        let sessions: Vec<Arc<OpenSession>> = self
            .all_sessions()
            .into_iter()
            .filter(|session| session.ctrl.workspace() == self.info.workspace)
            .collect();
        for session in &sessions {
            session.ctrl.notify(notification.clone());
        }
        sessions.len()
    }

    /// Cancels every in-flight turn, then closes every open controller.
    pub async fn close(&self) -> Result<(), String> {
        if let Some(workflows) = &self.workflows {
            workflows.close().await;
        }
        let sessions: Vec<Arc<OpenSession>> = {
            let mut map = self
                .sessions
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            let taken = map.values().cloned().collect();
            map.clear();
            taken
        };
        for session in &sessions {
            session.cancel_work();
        }
        let mut errors = Vec::new();
        for session in &sessions {
            // Awaited before the blocking close: `Controller::close` only
            // spawns the MCP shutdown, and a spawned task is dropped when the
            // process exits right after this returns.
            session.ctrl.close_mcp().await;
            let ctrl = Arc::clone(&session.ctrl);
            match tokio::task::spawn_blocking(move || ctrl.close()).await {
                Ok(Err(error)) => errors.push(error),
                Ok(Ok(())) => {}
                Err(error) => errors.push(error.to_string()),
            }
        }
        self.metrics.sessions_open(-(sessions.len() as i64));
        // Every loop was told to stop by `cancel_work` above, so this only
        // waits out an in-flight wake turn.
        let loops: Vec<_> = std::mem::take(
            &mut *self
                .wake_loops
                .lock()
                .unwrap_or_else(|poison| poison.into_inner()),
        );
        for handle in loops {
            let _ = handle.await;
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors.join("\n"))
        }
    }

    // ---- routing ----

    /// Every route in `openapi.yaml`, plus the two token-free UI routes.
    pub fn router(self: &Arc<Self>) -> Router {
        let state = Arc::clone(self);
        Router::new()
            .route("/v1/sessions", post(create_session).get(list_sessions))
            .route(
                "/v1/sessions/{id}",
                get(get_session)
                    .patch(rename_session)
                    .delete(delete_session),
            )
            .route("/v1/sessions/{id}/history", get(history))
            .route("/v1/sessions/{id}/context", get(context))
            .route(
                "/v1/sessions/{id}/approvals/{approval_id}",
                post(approvals::approve),
            )
            .route(
                "/v1/sessions/{id}/turns",
                post(start_turn).layer(DefaultBodyLimit::max(IMAGE_TURN_BODY_MAX_BYTES)),
            )
            .route("/v1/sessions/{id}/turns/{turn_id}", get(get_turn))
            .route("/v1/sessions/{id}/turns/{turn_id}/events", get(turn_events))
            .route(
                "/v1/sessions/{id}/turns/{turn_id}/cancel",
                post(cancel_turn),
            )
            .route("/v1/sessions/{id}/compact", post(compact::handle))
            .route("/v1/sessions/{id}/tasks", get(tasks::list))
            .route("/v1/sessions/{id}/tasks/{task_id}", get(tasks::get))
            .route(
                "/v1/sessions/{id}/tasks/{task_id}/cancel",
                post(tasks::cancel),
            )
            .route("/v1/sessions/{id}/timers", get(timers::list))
            .route(
                "/v1/sessions/{id}/timers/{timer_id}/cancel",
                post(timers::cancel),
            )
            .route("/v1/sessions/{id}/mcp", get(mcp::list))
            .route("/v1/sandbox/reload", post(sandbox::reload))
            .route("/v1/tasks", get(agents::list))
            .route("/v1/tasks/{parent_session}/{task_id}", get(agents::get))
            .route("/v1/workflows", get(workflows::list).post(workflows::start))
            .route("/v1/workflows/{id}", get(workflows::get))
            .route("/v1/workflows/{id}/events", get(workflows::events))
            .route("/v1/workflows/{id}/resume", post(workflows::resume))
            .route("/v1/workflows/{id}/fork", post(workflows::fork))
            .route("/v1/workflows/{id}/cancel", post(workflows::cancel))
            .route(
                "/v1/workflows/requests/{id}/approve",
                post(workflows::approve),
            )
            .route(
                "/v1/workflows/requests/{id}/reject",
                post(workflows::reject),
            )
            .route(
                "/v1/workspaces",
                get(workspaces::list)
                    .post(workspaces::register)
                    .delete(workspaces::remove),
            )
            .route("/v1/workspaces/diff", get(diff::get))
            .route("/v1/info", get(info))
            .route("/v1/status", get(status))
            .route("/v1/usage", get(usage))
            .route("/v1/usage/daily", get(daily_usage))
            .route("/v1/openapi.yaml", get(openapi))
            .route("/healthz", get(healthz))
            .route("/metrics", get(prometheus))
            // The embedded web UI is not part of the API: no token, not in
            // openapi.yaml. "/" is an exact match, so unknown paths still 404
            // instead of falling through to index.html.
            .route("/", get(index_page))
            .route("/assets/{*path}", get(asset))
            .layer(DefaultBodyLimit::max(1 << 20))
            .layer(axum::middleware::from_fn_with_state(
                Arc::clone(&state),
                instrument,
            ))
            .with_state(state)
    }

    // ---- registry ----

    fn lookup(&self, id: &str) -> Option<Arc<OpenSession>> {
        self.sessions
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .get(id)
            .cloned()
    }

    fn remove(&self, id: &str) -> Option<Arc<OpenSession>> {
        let removed = self
            .sessions
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .remove(id);
        if removed.is_some() {
            self.status_changed.send_modify(|version| *version += 1);
        }
        removed
    }

    fn all_sessions(&self) -> Vec<Arc<OpenSession>> {
        self.sessions
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .values()
            .cloned()
            .collect()
    }

    fn register(self: &Arc<Self>, ctrl: Controller) -> Arc<OpenSession> {
        let id = ctrl.info().session_id;
        let session = OpenSession::new(ctrl);
        self.sessions
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .insert(id, Arc::clone(&session));
        self.metrics.sessions_open(1);
        self.status_changed.send_modify(|version| *version += 1);
        self.start_wake_loop(&session);
        session
    }

    /// A hit in the registry returns the existing session without calling
    /// `open` again.
    async fn resume_or_create(
        self: &Arc<Self>,
        id: &str,
        workspace: Option<&str>,
    ) -> Result<(Arc<OpenSession>, bool), String> {
        if id.is_empty() {
            let ctrl = self.factory.create(workspace).await?;
            return Ok((self.register(ctrl), true));
        }
        if let Some(existing) = self.lookup(id) {
            return Ok((existing, false));
        }
        let _gate = self.open_gate.lock().await;
        // Re-check: another resume of this id may have finished while we
        // waited for the gate.
        if let Some(existing) = self.lookup(id) {
            return Ok((existing, false));
        }
        let ctrl = self.factory.open(id, workspace).await?;
        Ok((self.register(ctrl), false))
    }

    // ---- wire ----

    fn session_wire(&self, session: &OpenSession) -> SessionWire {
        let info = session.ctrl.info();
        let turn = session.current_turn().map(|turn| {
            let summary = turn.summary();
            SessionTurnWire {
                id: summary.id,
                trigger: summary.trigger,
                status: summary.status,
            }
        });
        SessionWire {
            id: info.session_id,
            name: info.session_name,
            workspace: info.workspace,
            provider: info.provider,
            profile: info.profile,
            model: info.model,
            thinking: info.thinking,
            context_window: info.context_window,
            usage: info.usage,
            context_input_tokens: info.context_input_tokens,
            sandbox: sandbox_wire(&info.sandbox),
            turn,
        }
    }

    fn session_context_metrics(&self) -> Vec<SessionContext> {
        self.all_sessions()
            .iter()
            .map(|session| {
                let info = session.ctrl.info();
                SessionContext {
                    session_id: info.session_id,
                    provider: info.provider,
                    model: info.model,
                    context_window: info.context_window,
                    context_input_tokens: info.context_input_tokens,
                    context_input_tokens_present: info.context_input_tokens_present,
                    context_input_tokens_pending: info.context_input_tokens_pending,
                }
            })
            .collect()
    }

    /// Whether any open session has a running turn.
    fn any_turn_active(&self) -> bool {
        self.all_sessions().iter().any(|session| {
            session
                .current_turn()
                .is_some_and(|turn| turn.summary().status == turn::TURN_RUNNING)
        })
    }

    /// The full `GET /v1/status` payload: every open session in this
    /// process, sorted by workspace then id.
    fn status_snapshot(&self) -> StatusSnapshotWire {
        let mut sessions: Vec<StatusSessionWire> = self
            .all_sessions()
            .iter()
            .map(|session| {
                let info = session.ctrl.info();
                let turn = session.current_turn().map(|turn| turn.summary().status);
                let approvals = session
                    .ctrl
                    .builder()
                    .bash_approvals
                    .as_ref()
                    .map_or(0, |approvals| approvals.pending_count(&info.session_id));
                let tasks = session
                    .ctrl
                    .tasks()
                    .map(|tasks| {
                        tasks
                            .list()
                            .iter()
                            .filter(|task| !task.status.final_status())
                            .count()
                    })
                    .unwrap_or(0);
                StatusSessionWire {
                    id: info.session_id,
                    workspace: info.workspace,
                    turn,
                    approvals,
                    tasks,
                }
            })
            .collect();
        sessions.sort_by(|a, b| (&a.workspace, &a.id).cmp(&(&b.workspace, &b.id)));
        StatusSnapshotWire { sessions }
    }

    // ---- turns ----

    /// Drains `session`'s pending sub-agent notifications for as long as it is
    /// open.
    ///
    /// It is the sole caller of [`Server::wake_turn`]: both a registry update
    /// signal and the end of any turn on `session` route through this one task,
    /// so every "is a notification pending and no turn active" check happens
    /// one at a time and no two wake turns can start for the same notification.
    /// On every update signal it also diffs the task list into the task
    /// metrics. It does nothing when the runner tracks no tasks.
    ///
    /// The turn is awaited here rather than started as its own task. A
    /// notification pushed while it runs bumps the registry's `watch` version,
    /// so the next `changed()` returns at once and the re-check happens anyway,
    /// with one fewer moving part.
    fn start_wake_loop(self: &Arc<Self>, session: &Arc<OpenSession>) {
        let Some(tasks) = session.ctrl.subagent_tasks() else {
            return;
        };
        let server = Arc::clone(self);
        let session = Arc::clone(session);
        let handle = tokio::spawn(async move {
            let mut seen = std::collections::BTreeMap::new();
            let mut updates = tasks.updates();
            loop {
                tokio::select! {
                    changed = updates.changed() => {
                        if changed.is_err() {
                            return;
                        }
                        if let Some(view) = session.ctrl.tasks() {
                            server.metrics.diff_tasks(&mut seen, &view.list());
                        }
                        server.status_changed.send_modify(|version| *version += 1);
                    }
                    () = session.turn_finished.notified() => {}
                    () = session.closed.cancelled() => return,
                    () = server.cancel.cancelled() => return,
                }
                server.wake_turn(&session).await;
            }
        });
        let mut loops = self
            .wake_loops
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        // A deleted session's loop has already returned; this list does not
        // shrink on its own.
        loops.retain(|running| !running.is_finished());
        loops.push(handle);
    }

    /// Runs one task-triggered turn on `session` when a notification is pending
    /// and nothing else holds the session.
    ///
    /// The wake is prepared before the turn is published, so a no-op or busy
    /// admission cannot leave a phantom turn visible on `GET /v1/sessions`. A
    /// busy session is left to the turn that holds it.
    async fn wake_turn(self: &Arc<Self>, session: &Arc<OpenSession>) {
        let prepared = {
            let mut state = session.lock();
            let busy = state.turn.as_ref().is_some_and(|turn| !turn.is_done());
            if busy || state.compacting.is_some() {
                None
            } else {
                match session.ctrl.prepare_wake() {
                    Ok(None) => None,
                    Ok(Some(wake)) => match new_id() {
                        Ok(id) => {
                            let turn =
                                Arc::new(Turn::new(id, TRIGGER_TASK, self.cancel.child_token()));
                            state.turn = Some(Arc::clone(&turn));
                            Some(Ok((turn, wake)))
                        }
                        Err(error) => Some(Err(error)),
                    },
                    // A prompt or a close raced us; both are expected and
                    // ignored here.
                    Err(message) if message == app::PROMPT_ACTIVE || message == app::CLOSED => None,
                    Err(message) => Some(Err(message)),
                }
            }
        };
        let (turn, wake) = match prepared {
            None => return,
            Some(Err(message)) => {
                self.log.error("wake_turn_error", &[("error", message)]);
                return;
            }
            Some(Ok(prepared)) => prepared,
        };
        self.status_changed.send_modify(|version| *version += 1);

        self.metrics.turn_started();
        let mut fields = vec![
            ("turn_id", turn.id.clone()),
            ("trigger", TRIGGER_TASK.to_string()),
        ];
        if let Some(kind) = session.ctrl.subagent_tasks().and_then(|tasks| {
            tasks
                .notifications()
                .snapshot()
                .into_iter()
                .find_map(|item| item.kind)
        }) {
            fields.push(("inbox_kind", kind.as_str().to_string()));
        }
        self.log.info("turn_started", &fields);
        let cancel = turn.cancel_token();
        let result = {
            let mut emit = turn.emitter(&self.metrics);
            wake.run(&mut emit, &cancel).await
        };
        let (error, canceled) = match &result {
            Ok(()) => (None, false),
            Err(error) => (Some(error.to_string()), error.is_cancelled()),
        };
        turn.finish(error.clone(), canceled);
        self.status_changed.send_modify(|version| *version += 1);
        let summary = turn.summary();
        self.metrics.turn_finished(&summary.status, turn.elapsed());
        if let Some(message) = error.filter(|_| !canceled) {
            self.log.error(
                "turn_error",
                &[
                    ("turn_id", turn.id.clone()),
                    ("trigger", TRIGGER_TASK.to_string()),
                    ("error", message),
                ],
            );
        }
        self.log.info(
            "turn_finished",
            &[
                ("turn_id", turn.id.clone()),
                ("trigger", TRIGGER_TASK.to_string()),
                ("status", summary.status.clone()),
                ("duration_ms", turn.elapsed().as_millis().to_string()),
            ],
        );
    }

    /// Starts a user turn on `session`. The `trigger == triggerTask` branch
    /// lives in [`Server::wake_turn`], because a wake turn needs no HTTP reply
    /// and is awaited by the wake loop that admitted it.
    fn start_turn(
        self: &Arc<Self>,
        session: &Arc<OpenSession>,
        text: String,
        image: Option<Block>,
    ) -> Result<Arc<Turn>, String> {
        let turn = {
            let mut state = session.lock();
            let busy = state.turn.as_ref().is_some_and(|turn| !turn.is_done());
            if busy || state.compacting.is_some() {
                return Err(TURN_ACTIVE.to_string());
            }
            let id = new_id()?;
            let cancel = self.cancel.child_token();
            let turn = Arc::new(Turn::new(id, TRIGGER_USER, cancel));
            state.turn = Some(Arc::clone(&turn));
            turn
        };
        self.status_changed.send_modify(|version| *version += 1);

        self.metrics.turn_started();
        let server = Arc::clone(self);
        let session = Arc::clone(session);
        let spawned = Arc::clone(&turn);
        tokio::spawn(async move {
            let cancel = spawned.cancel_token();
            let result = {
                let mut emit = spawned.emitter(&server.metrics);
                match image {
                    Some(image) => {
                        session
                            .ctrl
                            .prompt_with_image(&text, image, &mut emit, &cancel)
                            .await
                    }
                    None => session.ctrl.prompt(&text, &mut emit, &cancel).await,
                }
            };
            let (error, canceled) = match &result {
                Ok(()) => (None, false),
                Err(error) => (Some(error.to_string()), error.is_cancelled()),
            };
            spawned.finish(error.clone(), canceled);
            server.status_changed.send_modify(|version| *version += 1);
            let summary = spawned.summary();
            server
                .metrics
                .turn_finished(&summary.status, spawned.elapsed());
            if let Some(message) = error.filter(|_| !canceled) {
                server.log.error(
                    "turn_error",
                    &[
                        ("turn_id", spawned.id.clone()),
                        ("trigger", TRIGGER_USER.to_string()),
                        ("error", message),
                    ],
                );
            }
            server.log.info(
                "turn_finished",
                &[
                    ("turn_id", spawned.id.clone()),
                    ("trigger", TRIGGER_USER.to_string()),
                    ("status", summary.status.clone()),
                    ("duration_ms", spawned.elapsed().as_millis().to_string()),
                ],
            );
            // A notification that landed too late for this turn's own drain is
            // caught by the wake loop's end-of-turn check.
            session.turn_finished.notify_one();
        });

        self.log.info(
            "turn_started",
            &[
                ("turn_id", turn.id.clone()),
                ("trigger", TRIGGER_USER.to_string()),
            ],
        );
        Ok(turn)
    }
}

const TURN_ACTIVE: &str = "turn already active";

/// 16 random bytes, hex encoded. The same generator as `cmd`'s, duplicated
/// because the package boundary forbids the import.
fn new_id() -> Result<String, String> {
    use std::io::Read;
    let mut bytes = [0u8; 16];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut file| file.read_exact(&mut bytes))
        .map_err(|error| error.to_string())?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

// ---- middleware ----

/// Request-ID handling, token gating, structured logging and HTTP metrics.
/// Token gating is folded in so a 401 is still logged and measured under its
/// real route.
async fn instrument(
    State(server): State<Arc<Server>>,
    request: axum::extract::Request,
    next: Next,
) -> Response {
    let mut id = request_id(
        request
            .headers()
            .get("x-request-id")
            .map(header::HeaderValue::as_bytes)
            .unwrap_or_default(),
    );
    if id.is_empty() {
        id = new_id().unwrap_or_default();
    }

    let method = request.method().clone();
    let matched = request
        .extensions()
        .get::<MatchedPath>()
        .map(|path| path.as_str().to_string());
    let route = route_label(&method, matched.as_deref());

    let start = Instant::now();
    let mut response = if !server.token.is_empty()
        && route.contains(" /v1/")
        && !auth::authorized(&server.token, request.headers())
    {
        auth::unauthorized()
    } else {
        next.run(request).await
    };
    let elapsed = start.elapsed();

    if let Ok(value) = header::HeaderValue::from_str(&id) {
        response.headers_mut().insert("x-request-id", value);
    }

    let status = response.status().as_u16();
    server
        .metrics
        .http_request(&route, method.as_str(), status, elapsed);
    server.log.info(
        "http_request",
        &[
            ("method", method.to_string()),
            ("route", route),
            ("status", status.to_string()),
            ("duration_ms", elapsed.as_millis().to_string()),
            ("request_id", id),
        ],
    );
    response
}

/// The metric and log label for one request. axum's [`MatchedPath`] carries
/// only the path, and spells the two UI routes differently, so both are mapped
/// back and the method is prepended.
fn route_label(method: &Method, matched: Option<&str>) -> String {
    match matched {
        None => "unmatched".to_string(),
        Some("/") => "GET /{$}".to_string(),
        Some("/assets/{*path}") => "GET /assets/".to_string(),
        Some(path) => format!("{method} {path}"),
    }
}

/// Reduces a client-supplied `X-Request-ID` to at most 64 printable ASCII
/// bytes, per the design's trust-and-safety rule.
fn request_id(raw: &[u8]) -> String {
    raw.iter()
        .take(64)
        .filter(|byte| (0x20..0x7f).contains(*byte))
        .map(|&byte| byte as char)
        .collect()
}

// ---- error and JSON helpers ----

#[derive(Debug, Serialize)]
struct ApiError {
    code: String,
    message: String,
}

#[derive(Debug, Serialize)]
struct ErrorBody {
    error: ApiError,
}

/// `Content-Type: application/json`, no charset. Serves `router` on `listener`
/// until `shutdown` fires. A cancelled shutdown is a clean exit, so only a
/// listener or protocol failure is an error.
pub async fn serve(
    listener: listen::Listener,
    router: Router,
    shutdown: CancellationToken,
) -> Result<(), String> {
    let graceful = async move { shutdown.cancelled().await };
    match listener {
        listen::Listener::Tcp(bound) => {
            axum::serve(bound, router)
                .with_graceful_shutdown(graceful)
                .await
        }
        listen::Listener::Unix(bound) => {
            axum::serve(bound, router)
                .with_graceful_shutdown(graceful)
                .await
        }
    }
    .map_err(|error| error.to_string())
}

pub fn error_response(status: StatusCode, code: &str, message: &str) -> Response {
    let body = serde_json::to_vec(&ErrorBody {
        error: ApiError {
            code: code.to_string(),
            message: message.to_string(),
        },
    })
    .unwrap_or_default();
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body))
        .expect("static header values")
}

pub fn json_response<T: Serialize>(status: StatusCode, value: &T) -> Response {
    match serde_json::to_vec(value) {
        Ok(body) => Response::builder()
            .status(status)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body))
            .expect("static header values"),
        Err(_) => error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal",
            "internal error",
        ),
    }
}

/// The fixed 500 body, with the real (already redacted) error only logged.
fn internal_error(log: &Logger, error: &str) -> Response {
    log.error("internal_error", &[("error", error.to_string())]);
    error_response(
        StatusCode::INTERNAL_SERVER_ERROR,
        "internal",
        "internal error",
    )
}

fn not_found(message: &str) -> Response {
    error_response(StatusCode::NOT_FOUND, "not_found", message)
}

fn bad_request(message: &str) -> Response {
    error_response(StatusCode::BAD_REQUEST, "bad_request", message)
}

/// The response for a [`WorkspaceLoadError`], shared by every route that
/// admits a caller-named workspace before acting on it.
pub(crate) fn workspace_load_error_response(error: WorkspaceLoadError) -> Response {
    match error {
        WorkspaceLoadError::Invalid(message) => {
            error_response(StatusCode::BAD_REQUEST, "INVALID_WORKSPACE", &message)
        }
        WorkspaceLoadError::NotAdmitted(message) => {
            error_response(StatusCode::FORBIDDEN, "WORKSPACE_NOT_ADMITTED", &message)
        }
        WorkspaceLoadError::Failed(message) => {
            error_response(StatusCode::INTERNAL_SERVER_ERROR, "internal", &message)
        }
    }
}

/// The response for a [`WorkspaceRemoveError`], used by `DELETE
/// /v1/workspaces`.
pub(crate) fn workspace_remove_error_response(error: WorkspaceRemoveError) -> Response {
    match error {
        WorkspaceRemoveError::NotFound => error_response(
            StatusCode::NOT_FOUND,
            "WORKSPACE_NOT_FOUND",
            "workspace not found",
        ),
        WorkspaceRemoveError::IsStartup => error_response(
            StatusCode::CONFLICT,
            "WORKSPACE_IS_STARTUP",
            "the startup workspace cannot be removed",
        ),
        WorkspaceRemoveError::InUse(message) => {
            error_response(StatusCode::CONFLICT, "WORKSPACE_IN_USE", &message)
        }
    }
}

fn turn_active(message: &str) -> Response {
    error_response(StatusCode::CONFLICT, "turn_active", message)
}

// ---- wire types ----

#[derive(Debug, Clone, Serialize)]
pub struct SandboxWire {
    pub mode: String,
    pub network: String,
    pub bash_available: bool,
    pub summary: String,
}

pub fn sandbox_wire(info: &SandboxInfo) -> SandboxWire {
    SandboxWire {
        mode: info.mode.as_str().to_string(),
        network: info.network.as_str().to_string(),
        bash_available: info.bash_available,
        summary: info.summary().to_string(),
    }
}

#[derive(Debug, Clone, Serialize)]
struct SessionTurnWire {
    id: String,
    trigger: String,
    status: String,
}

#[derive(Debug, Clone, Serialize)]
struct SessionWire {
    id: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    name: String,
    workspace: String,
    provider: String,
    profile: String,
    model: String,
    thinking: String,
    context_window: i64,
    usage: Usage,
    context_input_tokens: i64,
    sandbox: SandboxWire,
    turn: Option<SessionTurnWire>,
}

#[derive(Debug, Clone, Serialize)]
struct SessionListRow {
    id: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    name: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    path: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    workspace: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    provider: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    model: String,
    open: bool,
}

#[derive(Debug, Clone, Serialize)]
struct SessionListResponse {
    sessions: Vec<SessionListRow>,
}

#[derive(Debug, Clone, Serialize)]
struct HealthzWire {
    status: String,
    sessions_open: usize,
}

/// One row of `GET /v1/status`. `turn` is `null` until the session's first
/// turn starts.
#[derive(Debug, Clone, PartialEq, Serialize)]
struct StatusSessionWire {
    id: String,
    workspace: String,
    turn: Option<String>,
    approvals: usize,
    tasks: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
struct StatusSnapshotWire {
    sessions: Vec<StatusSessionWire>,
}

// ---- handlers ----

#[derive(Debug, Default, Deserialize)]
struct CreateBody {
    #[serde(default)]
    resume: String,
    /// Admitted and loaded before use; absent means the startup workspace.
    /// With `resume`, the session is searched in this workspace, or in
    /// every loaded workspace when absent.
    #[serde(default)]
    workspace: Option<String>,
}

async fn create_session(State(server): State<Arc<Server>>, body: Bytes) -> Response {
    // An empty body is tolerated here; only malformed JSON is a 400.
    let parsed: CreateBody = if body.iter().all(u8::is_ascii_whitespace) {
        CreateBody::default()
    } else {
        match serde_json::from_slice(&body) {
            Ok(parsed) => parsed,
            Err(_) => return bad_request("invalid JSON body"),
        }
    };

    let workspace = match &parsed.workspace {
        Some(path) => match server.factory.load_workspace(path).await {
            Ok((info, _newly_loaded)) => Some(info.path),
            Err(error) => return workspace_load_error_response(error),
        },
        None => None,
    };

    match server
        .resume_or_create(&parsed.resume, workspace.as_deref())
        .await
    {
        Ok((session, created)) => {
            let status = if created {
                StatusCode::CREATED
            } else {
                StatusCode::OK
            };
            json_response(status, &server.session_wire(&session))
        }
        Err(error) if error == SESSION_NOT_FOUND => not_found("session not found"),
        Err(error) => internal_error(&server.log, &error),
    }
}

#[derive(Debug, Default, Deserialize)]
struct ListSessionsQuery {
    /// Admitted and loaded before use; absent lists every loaded workspace.
    workspace: Option<String>,
}

async fn list_sessions(
    State(server): State<Arc<Server>>,
    Query(query): Query<ListSessionsQuery>,
) -> Response {
    let workspace = match &query.workspace {
        Some(path) => match server.factory.load_workspace(path).await {
            Ok((info, _newly_loaded)) => Some(info.path),
            Err(error) => return workspace_load_error_response(error),
        },
        None => None,
    };

    let disk = match server.factory.list(workspace.as_deref()).await {
        Some(Ok(result)) => result,
        Some(Err(error)) => return internal_error(&server.log, &error),
        None => ListResult::default(),
    };

    let open: HashMap<String, Arc<OpenSession>> = server
        .sessions
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .iter()
        .filter(|(_, session)| match &workspace {
            Some(path) => session.ctrl.workspace() == path,
            None => true,
        })
        .map(|(id, session)| (id.clone(), Arc::clone(session)))
        .collect();

    let mut rows = Vec::with_capacity(disk.sessions.len() + open.len());
    let mut seen = std::collections::HashSet::new();
    for info in &disk.sessions {
        seen.insert(info.id.clone());
        rows.push(SessionListRow {
            id: info.id.clone(),
            name: info.name.clone(),
            path: info.path.clone(),
            workspace: info.cwd.clone(),
            provider: info.provider.clone(),
            model: info.model.clone(),
            open: open.contains_key(&info.id),
        });
    }
    for (id, session) in &open {
        if seen.contains(id) {
            continue;
        }
        let info = session.ctrl.info();
        rows.push(SessionListRow {
            id: id.clone(),
            name: info.session_name,
            path: String::new(),
            workspace: info.workspace,
            provider: info.provider,
            model: info.model,
            open: true,
        });
    }

    json_response(StatusCode::OK, &SessionListResponse { sessions: rows })
}

async fn get_session(State(server): State<Arc<Server>>, Path(id): Path<String>) -> Response {
    match server.lookup(&id) {
        Some(session) => json_response(StatusCode::OK, &server.session_wire(&session)),
        None => not_found("session not found"),
    }
}

#[derive(Debug, Default, Deserialize)]
struct RenameBody {
    #[serde(default)]
    name: String,
}

async fn rename_session(
    State(server): State<Arc<Server>>,
    Path(id): Path<String>,
    body: Bytes,
) -> Response {
    let Some(session) = server.lookup(&id) else {
        return not_found("session not found");
    };
    let parsed: RenameBody = if body.iter().all(u8::is_ascii_whitespace) {
        RenameBody::default()
    } else {
        match serde_json::from_slice(&body) {
            Ok(parsed) => parsed,
            Err(_) => return bad_request("invalid JSON body"),
        }
    };
    if parsed.name.trim().is_empty() {
        return bad_request("session name is required");
    }
    match session.ctrl.rename_session(&parsed.name) {
        Ok(()) => json_response(StatusCode::OK, &server.session_wire(&session)),
        Err(error) if error == app::PROMPT_ACTIVE => turn_active("a turn is active"),
        Err(error) if error.starts_with("session is invalid") => {
            bad_request("session name is invalid")
        }
        Err(error) => internal_error(&server.log, &error),
    }
}

async fn delete_session(State(server): State<Arc<Server>>, Path(id): Path<String>) -> Response {
    let Some(session) = server.remove(&id) else {
        return not_found("session not found");
    };
    session.cancel_work();
    let ctrl = Arc::clone(&session.ctrl);
    let closed = tokio::task::spawn_blocking(move || ctrl.close()).await;
    match closed {
        Ok(Err(error)) => server.log.error(
            "session_close_error",
            &[("session_id", id.clone()), ("error", error)],
        ),
        Err(error) => server.log.error(
            "session_close_error",
            &[("session_id", id.clone()), ("error", error.to_string())],
        ),
        Ok(Ok(())) => {}
    }
    server.metrics.sessions_open(-1);
    Response::builder()
        .status(StatusCode::NO_CONTENT)
        .body(Body::empty())
        .expect("static response")
}

async fn history(State(server): State<Arc<Server>>, Path(id): Path<String>) -> Response {
    match server.lookup(&id) {
        // An empty Vec serializes as "[]".
        Some(session) => json_response::<Vec<Message>>(StatusCode::OK, &session.ctrl.history()),
        None => not_found("session not found"),
    }
}

async fn context(State(server): State<Arc<Server>>, Path(id): Path<String>) -> Response {
    let Some(session) = server.lookup(&id) else {
        return not_found("session not found");
    };
    match session.ctrl.context_report() {
        Some(report) => json_response(StatusCode::OK, &report),
        None => error_response(
            StatusCode::CONFLICT,
            "context_unavailable",
            "the session context is not available",
        ),
    }
}

#[derive(Debug, Deserialize)]
struct StartTurnBody {
    #[serde(default)]
    text: String,
    #[serde(default)]
    stream: Option<bool>,
    #[serde(default)]
    image: Option<StartTurnImage>,
}

#[derive(Debug, Deserialize)]
struct StartTurnImage {
    #[serde(default)]
    data: String,
    #[serde(default)]
    mime_type: String,
}

async fn start_turn(
    State(server): State<Arc<Server>>,
    Path(id): Path<String>,
    body: Bytes,
) -> Response {
    let Some(session) = server.lookup(&id) else {
        return not_found("session not found");
    };
    // Unlike create and rename, the body is decoded unconditionally here, so an
    // empty body is a 400.
    let parsed: StartTurnBody = match serde_json::from_slice(&body) {
        Ok(parsed) => parsed,
        Err(_) => return bad_request("invalid JSON body"),
    };
    if parsed.text.trim().is_empty() && parsed.image.is_none() {
        return bad_request("text must not be empty");
    }
    let image = parsed
        .image
        .map(|image| Block::image(image.data, image.mime_type));
    if image
        .as_ref()
        .is_some_and(|image| image.validate().is_err())
    {
        return bad_request("image is invalid");
    }
    let stream = parsed.stream.unwrap_or(true);

    let turn = match server.start_turn(&session, parsed.text, image) {
        Ok(turn) => turn,
        Err(error) if error == TURN_ACTIVE => {
            return turn_active("a turn is already active for this session");
        }
        Err(error) => return internal_error(&server.log, &error),
    };

    if stream {
        return stream_sse(&server.metrics, turn, 0);
    }
    wait_done(&turn).await;
    json_response(StatusCode::OK, &turn.summary())
}

/// Blocks until the turn is done, riding the same version broadcast the SSE
/// reader uses.
async fn wait_done(turn: &Arc<Turn>) {
    let mut changed = turn.subscribe();
    while !turn.is_done() {
        if changed.changed().await.is_err() {
            return;
        }
    }
}

/// The turn named by the path, or the 404 message to answer with.
fn resolve_turn(server: &Server, id: &str, turn_id: &str) -> Result<Arc<Turn>, &'static str> {
    let Some(session) = server.lookup(id) else {
        return Err("session not found");
    };
    match session.current_turn() {
        Some(turn) if turn.id == turn_id => Ok(turn),
        _ => Err("turn not found"),
    }
}

async fn get_turn(
    State(server): State<Arc<Server>>,
    Path((id, turn_id)): Path<(String, String)>,
) -> Response {
    match resolve_turn(&server, &id, &turn_id) {
        Ok(turn) => json_response(StatusCode::OK, &turn.summary()),
        Err(message) => not_found(message),
    }
}

#[derive(Debug, Default, Deserialize)]
struct EventsQuery {
    after: Option<String>,
}

async fn turn_events(
    State(server): State<Arc<Server>>,
    Path((id, turn_id)): Path<(String, String)>,
    Query(query): Query<EventsQuery>,
    headers: HeaderMap,
) -> Response {
    let turn = match resolve_turn(&server, &id, &turn_id) {
        Ok(turn) => turn,
        Err(message) => return not_found(message),
    };

    let after = match query.after.as_deref().filter(|value| !value.is_empty()) {
        Some(value) => match value.parse::<usize>() {
            Ok(n) => n + 1,
            Err(_) => return bad_request("after must be a non-negative integer"),
        },
        None => headers
            .get("last-event-id")
            .and_then(|value| value.to_str().ok())
            .filter(|value| !value.is_empty())
            .and_then(|value| value.parse::<usize>().ok())
            .map(|n| n + 1)
            .unwrap_or(0),
    };

    stream_sse(&server.metrics, turn, after)
}

async fn cancel_turn(
    State(server): State<Arc<Server>>,
    Path((id, turn_id)): Path<(String, String)>,
) -> Response {
    match resolve_turn(&server, &id, &turn_id) {
        Ok(turn) => {
            turn.cancel();
            Response::builder()
                .status(StatusCode::ACCEPTED)
                .body(Body::empty())
                .expect("static response")
        }
        Err(message) => not_found(message),
    }
}

async fn info(State(server): State<Arc<Server>>) -> Response {
    json_response(StatusCode::OK, &server.info)
}

async fn status(State(server): State<Arc<Server>>) -> Response {
    stream_status(server)
}

#[derive(Debug, Default, Deserialize)]
struct UsageFilter {
    session_id: Option<String>,
}

async fn usage(State(server): State<Arc<Server>>, Query(filter): Query<UsageFilter>) -> Response {
    match server.factory.usage_summary(filter.session_id.as_deref()) {
        Ok(summary) => json_response(StatusCode::OK, &summary),
        Err(error) => internal_error(&server.log, &error),
    }
}

#[derive(Debug, Default, Deserialize)]
struct DailyUsageFilter {
    days: Option<i64>,
    session_id: Option<String>,
}

async fn daily_usage(
    State(server): State<Arc<Server>>,
    Query(filter): Query<DailyUsageFilter>,
) -> Response {
    let days = filter.days.unwrap_or(30);
    if !(1..=365).contains(&days) {
        return error_response(
            StatusCode::BAD_REQUEST,
            "invalid_usage_range",
            "days must be between 1 and 365",
        );
    }
    match server
        .factory
        .usage_analysis(days as u16, filter.session_id.as_deref())
    {
        Ok(analysis) => json_response(StatusCode::OK, &analysis),
        Err(error) => internal_error(&server.log, &error),
    }
}

async fn openapi() -> Response {
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/yaml")
        .body(Body::from(OPENAPI_YAML))
        .expect("static header values")
}

async fn healthz(State(server): State<Arc<Server>>) -> Response {
    let open = server
        .sessions
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .len();
    json_response(
        StatusCode::OK,
        &HealthzWire {
            status: "ok".to_string(),
            sessions_open: open,
        },
    )
}

async fn prometheus(State(server): State<Arc<Server>>) -> Response {
    server
        .metrics
        .replace_session_contexts(server.session_context_metrics());
    if let Some(workflows) = &server.workflows
        && let Ok(runs) = workflows.list()
    {
        server.metrics.replace_workflows(&runs);
    }
    Response::builder()
        .status(StatusCode::OK)
        .header(
            header::CONTENT_TYPE,
            "text/plain; version=0.0.4; charset=utf-8",
        )
        .body(Body::from(server.metrics.render()))
        .expect("static header values")
}

async fn index_page() -> Response {
    ui::index_response(ui::index_page())
}

async fn asset(Path(path): Path<String>) -> Response {
    let full = format!("assets/{path}");
    ui::asset_response(&full, ui::dist_file(&full))
}

// ---- SSE streaming ----

/// Decrements the stream-client gauge when the response body is dropped.
struct StreamClient(Arc<Metrics>);

impl Drop for StreamClient {
    fn drop(&mut self) {
        self.0.stream_clients(-1);
    }
}

/// Writes turn events from sequence `after` onward as SSE frames, then waits
/// for more until the turn finishes or the client disconnects. Disconnecting
/// never cancels the turn.
fn stream_sse(metrics: &Arc<Metrics>, turn: Arc<Turn>, after: usize) -> Response {
    metrics.stream_clients(1);
    let guard = StreamClient(Arc::clone(metrics));
    // Subscribe before the first snapshot: a bump that lands after the
    // snapshot still wakes the reader.
    let changed = turn.subscribe();

    let stream = futures_util::stream::unfold(
        (turn, changed, after, guard, false),
        |(turn, mut changed, mut after, guard, finished)| async move {
            if finished {
                return None;
            }
            loop {
                let (events, done) = turn.snapshot(after);
                if !events.is_empty() {
                    let mut chunk = String::new();
                    for (offset, event) in events.iter().enumerate() {
                        let data = serde_json::to_string(event).unwrap_or_default();
                        chunk.push_str(&format_frame(
                            (after + offset) as i64,
                            &event.event_type,
                            &data,
                        ));
                    }
                    after += events.len();
                    return Some((
                        Ok::<String, std::io::Error>(chunk),
                        (turn, changed, after, guard, done),
                    ));
                }
                if done {
                    return None;
                }
                if changed.changed().await.is_err() {
                    return None;
                }
            }
        },
    );

    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .header(header::CONNECTION, "keep-alive")
        .body(Body::from_stream(stream))
        .expect("static header values")
}

/// Writes a full `GET /v1/status` snapshot as an SSE frame whenever it
/// differs from the last one sent on this connection. No `id:` field and no
/// replay: a reconnecting client's first frame is the current snapshot,
/// which is all it needs.
fn stream_status(server: Arc<Server>) -> Response {
    let changed = server.status_changed.subscribe();
    let stream = futures_util::stream::unfold(
        (server, changed, None::<StatusSnapshotWire>),
        |(server, mut changed, last)| async move {
            loop {
                let snapshot = server.status_snapshot();
                if last.as_ref() != Some(&snapshot) {
                    let data = serde_json::to_string(&snapshot).unwrap_or_default();
                    let frame = format!("event: status\ndata: {data}\n\n");
                    return Some((
                        Ok::<String, std::io::Error>(frame),
                        (server, changed, Some(snapshot)),
                    ));
                }
                tokio::select! {
                    result = changed.changed() => {
                        if result.is_err() {
                            return None;
                        }
                    }
                    () = server.cancel.cancelled() => return None,
                }
            }
        },
    );

    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .header(header::CONNECTION, "keep-alive")
        .body(Body::from_stream(stream))
        .expect("static header values")
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    use super::*;
    use crate::cli::runtime_builder::{Builder, Runner, RuntimeInfo, SharedSession};
    use crate::cli::testutil;
    use axum::body::Body;
    use axum::http::Request;
    use base64::Engine as _;
    use base64::engine::general_purpose::STANDARD as BASE64;
    use futures_util::StreamExt;
    use otto_core::model::{Block, BlockType, Message, Role, Usage};
    use otto_core::provider::{
        Provider, ProviderError, Request as ProviderRequest, Response as ProviderResponse,
        StreamEvent, StreamSink,
    };
    use otto_core::session::{CURRENT_VERSION, Header, ListResult, SessionInfo};
    use otto_core::wire::sse::{Frame, parse_frames};
    use serde_json::Value;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;
    use tempfile::TempDir;
    use tokio::sync::watch;
    use tower::ServiceExt;

    // ---- the provider double ----

    /// What one turn does. `Runner` is a concrete struct here, so the script
    /// sits one layer down, at the provider.
    #[derive(Default)]
    struct Script {
        /// Streamed as text deltas and returned as the assistant text.
        deltas: Vec<String>,
        /// The assistant message's usage.
        usage: Option<Usage>,
        /// Fails the call with this message instead of answering.
        error: Option<String>,
        /// Streams back the last user message instead of `deltas`.
        echo: bool,
        /// Held after the deltas until cancelled, so the turn stays running.
        /// Cancelling the turn wins the race and yields `Cancelled`.
        gate: Option<CancellationToken>,
        /// Runs at the top of every call with the 1-based call index. The wake
        /// tests push a notification from it.
        on_call: Option<Box<dyn Fn(usize) + Send + Sync>>,
    }

    struct ScriptedProvider {
        script: Script,
        started: watch::Sender<usize>,
        /// The role of each call's last request message, in call order. A
        /// prompt turn ends in [`Role::User`]; a wake turn ends in the
        /// delivered notification's [`Role::Context`].
        roles: Mutex<Vec<Role>>,
    }

    impl ScriptedProvider {
        fn new(script: Script) -> Arc<Self> {
            Arc::new(Self {
                script,
                started: watch::channel(0).0,
                roles: Mutex::new(Vec::new()),
            })
        }

        fn roles(&self) -> Vec<Role> {
            self.roles
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .clone()
        }

        /// Resolves once `count` calls have reached the gate.
        async fn wait_started(&self, count: usize) {
            let mut rx = self.started.subscribe();
            rx.wait_for(|seen| *seen >= count).await.expect("sender");
        }
    }

    #[async_trait::async_trait]
    impl Provider for ScriptedProvider {
        async fn complete(
            &self,
            request: &ProviderRequest,
            emit: StreamSink<'_>,
            cancel: &CancellationToken,
        ) -> Result<ProviderResponse, ProviderError> {
            let call = {
                let mut roles = self
                    .roles
                    .lock()
                    .unwrap_or_else(|poison| poison.into_inner());
                roles.push(
                    request
                        .messages
                        .last()
                        .map_or(Role::User, |last| last.role.clone()),
                );
                roles.len()
            };
            if let Some(hook) = &self.script.on_call {
                hook(call);
            }
            let deltas: Vec<String> = if self.script.echo {
                vec![
                    request
                        .messages
                        .iter()
                        .rev()
                        .find(|message| message.role == Role::User)
                        .map(Message::text)
                        .unwrap_or_default(),
                ]
            } else {
                self.script.deltas.clone()
            };
            for delta in &deltas {
                emit(StreamEvent::TextDelta {
                    text: delta.clone(),
                });
            }
            self.started.send_modify(|seen| *seen += 1);

            if let Some(gate) = &self.script.gate {
                tokio::select! {
                    () = gate.cancelled() => {}
                    () = cancel.cancelled() => return Err(ProviderError::Cancelled),
                }
            }
            if let Some(message) = &self.script.error {
                return Err(ProviderError::Other(message.clone()));
            }
            Ok(ProviderResponse {
                message: Message {
                    role: Role::Assistant,
                    blocks: vec![Block {
                        block_type: BlockType::Text,
                        text: deltas.concat(),
                        ..Block::default()
                    }],
                    usage: self.script.usage,
                    ..Message::default()
                },
            })
        }
    }

    // ---- the workspace registry double ----

    /// A fake, in-memory workspace registry for `/v1/workspaces` tests. Real
    /// admission and the real load-once registry mutex live in
    /// `cli::serve::Workspaces` and its own tests; this only exercises the
    /// `Factory`/route wiring, so admission here is a plain allowlist rather
    /// than a filesystem check.
    struct FakeWorkspaces {
        startup: String,
        /// Paths besides `startup` this fake admits.
        admitted: Vec<String>,
        /// A path that fails to load, with the message to fail it with.
        /// Checked before the path is registered, so a repeat call for the
        /// same path after this fires once can still succeed.
        error: Option<(String, String)>,
        /// Held before a load registers its path, so a test can pile up
        /// concurrent loads of one path.
        gate: Option<CancellationToken>,
        loaded: tokio::sync::Mutex<BTreeMap<String, bool>>,
        /// A loaded path with a simulated active workflow run, so
        /// `remove_workspace` answers `WorkspaceRemoveError::InUse` for it.
        in_use: Option<String>,
    }

    impl Default for FakeWorkspaces {
        fn default() -> Self {
            Self {
                startup: String::new(),
                admitted: Vec::new(),
                error: None,
                gate: None,
                loaded: tokio::sync::Mutex::new(BTreeMap::new()),
                in_use: None,
            }
        }
    }

    // ---- the factory double ----

    struct TestFactory {
        builder: Arc<Builder>,
        provider: Arc<ScriptedProvider>,
        /// Gives every controller this id; otherwise each gets a random one.
        fixed_id: Option<String>,
        create_error: Option<String>,
        open_error: Option<String>,
        /// Held before `open` returns, so a test can pile up concurrent
        /// resumes of one id.
        open_gate: Option<CancellationToken>,
        /// Every controller shares this registry. `None` gives each its own.
        tasks: Option<Arc<crate::subagent::tasks::Tasks>>,
        list: Option<ListResult>,
        /// Backs `Factory::tasks_list`/`tasks_get`. `None` keeps the trait's
        /// empty defaults, matching a process with no recorder wired.
        task_recorder: Option<Arc<crate::subagent::record::Store>>,
        workspaces: FakeWorkspaces,
        /// Canned `Factory::list` answer for a named workspace (`?workspace=`).
        /// An absent key falls back to `list`.
        list_by_workspace: HashMap<String, ListResult>,
        /// Workflow controllers keyed by workspace path. The entry at the
        /// harness's own workspace path is the startup controller.
        workflows: HashMap<String, Arc<crate::workflow::Controller>>,
        /// Canned `Factory::reload_sandbox` answer. `None` matches the
        /// trait's default (`sandbox_reload_available` false, `POST
        /// /v1/sandbox/reload` answers 501).
        sandbox_reload: Option<Result<SandboxInfo, String>>,
        /// Canned `Factory::reload_other_sandboxes` answer, one entry per
        /// non-startup workspace with its own reloader.
        sandbox_reload_others: Vec<(String, Result<SandboxInfo, String>)>,
        /// Canned `Factory::diff_runner` answer for the startup workspace.
        /// `None` matches a workspace with no usable sandbox (`GET
        /// /v1/workspaces/diff` answers 501).
        diff_runner: Option<(Arc<dyn crate::sandbox::CommandExecutor>, Vec<String>)>,
        create_calls: AtomicUsize,
        open_calls: AtomicUsize,
    }

    impl TestFactory {
        /// `workspace` overrides the session header's workspace, matching a
        /// real `Factory::create`/`open` given a non-startup workspace; every
        /// other test-double detail (provider, tasks) stays on the one shared
        /// builder.
        fn controller_for(&self, id: &str, workspace: Option<&str>) -> Controller {
            let session = SharedSession::memory(Header {
                version: CURRENT_VERSION,
                id: id.to_string(),
                workspace: workspace
                    .unwrap_or(&self.builder.workspace_path)
                    .to_string(),
                provider: "openai-compatible".to_string(),
                profile: "alpha".to_string(),
                model: "test-model".to_string(),
                created_at: chrono::Utc::now(),
            });
            let runner = Runner::scripted(
                session.clone(),
                Arc::clone(&self.provider) as Arc<dyn Provider + Send + Sync>,
                self.tasks
                    .clone()
                    .unwrap_or_else(|| Arc::new(crate::subagent::tasks::Tasks::new())),
            );
            Controller::with_builder(
                Arc::clone(&self.builder),
                true,
                session,
                runner,
                RuntimeInfo {
                    provider: "openai-compatible".to_string(),
                    profile: "alpha".to_string(),
                    model: "test-model".to_string(),
                    thinking: "high".to_string(),
                    context_window: 128_000,
                    sandbox: self.builder.sandbox_info,
                },
            )
        }
    }

    #[async_trait::async_trait]
    impl Factory for TestFactory {
        async fn create(&self, workspace: Option<&str>) -> Result<Controller, String> {
            self.create_calls.fetch_add(1, Ordering::SeqCst);
            if let Some(error) = &self.create_error {
                return Err(error.clone());
            }
            let id = self
                .fixed_id
                .clone()
                .unwrap_or_else(|| new_id().expect("id"));
            Ok(self.controller_for(&id, workspace))
        }

        async fn open(&self, id: &str, workspace: Option<&str>) -> Result<Controller, String> {
            self.open_calls.fetch_add(1, Ordering::SeqCst);
            if let Some(gate) = &self.open_gate {
                gate.cancelled().await;
            }
            if let Some(error) = &self.open_error {
                return Err(error.clone());
            }
            Ok(self.controller_for(id, workspace))
        }

        async fn list(&self, workspace: Option<&str>) -> Option<Result<ListResult, String>> {
            match workspace {
                Some(path) => self
                    .list_by_workspace
                    .get(path)
                    .cloned()
                    .map(Ok)
                    .or_else(|| self.list.clone().map(Ok)),
                None => self.list.clone().map(Ok),
            }
        }

        fn tasks_list(
            &self,
            query: &crate::subagent::record::ListQuery,
        ) -> Result<crate::subagent::record::ListResult, String> {
            match &self.task_recorder {
                Some(store) => store.list(query).map_err(|error| error.to_string()),
                None => Ok(crate::subagent::record::ListResult::default()),
            }
        }

        fn tasks_get(
            &self,
            parent_session: &str,
            task_id: &str,
        ) -> Result<Option<crate::subagent::record::TaskRow>, String> {
            match &self.task_recorder {
                Some(store) => store
                    .get(parent_session, task_id)
                    .map_err(|error| error.to_string()),
                None => Ok(None),
            }
        }

        async fn workspaces(&self) -> WorkspaceList {
            let loaded = self.workspaces.loaded.lock().await;
            let mut rest: Vec<WorkspaceInfo> = loaded
                .iter()
                .filter(|(path, _)| **path != self.workspaces.startup)
                .map(|(path, workflows)| WorkspaceInfo {
                    path: path.clone(),
                    workflows: *workflows,
                })
                .collect();
            rest.sort_by(|a, b| a.path.cmp(&b.path));
            let mut all = vec![WorkspaceInfo {
                path: self.workspaces.startup.clone(),
                workflows: *loaded.get(&self.workspaces.startup).unwrap_or(&false),
            }];
            all.extend(rest);
            WorkspaceList {
                startup: self.workspaces.startup.clone(),
                roots: self.workspaces.admitted.clone(),
                loaded: all,
            }
        }

        async fn load_workspace(
            &self,
            path: &str,
        ) -> Result<(WorkspaceInfo, bool), WorkspaceLoadError> {
            if !path.starts_with('/') {
                return Err(WorkspaceLoadError::Invalid(format!(
                    "{path}: not an existing directory"
                )));
            }
            if path != self.workspaces.startup
                && !self.workspaces.admitted.contains(&path.to_string())
            {
                return Err(WorkspaceLoadError::NotAdmitted(format!(
                    "{path}: outside the startup workspace and configured roots"
                )));
            }
            let mut loaded = self.workspaces.loaded.lock().await;
            if let Some(workflows) = loaded.get(path) {
                return Ok((
                    WorkspaceInfo {
                        path: path.to_string(),
                        workflows: *workflows,
                    },
                    false,
                ));
            }
            if let Some((error_path, message)) = &self.workspaces.error
                && error_path == path
            {
                return Err(WorkspaceLoadError::Failed(message.clone()));
            }
            if let Some(gate) = &self.workspaces.gate {
                gate.cancelled().await;
            }
            loaded.insert(path.to_string(), true);
            Ok((
                WorkspaceInfo {
                    path: path.to_string(),
                    workflows: true,
                },
                true,
            ))
        }

        async fn remove_workspace(&self, path: &str) -> Result<(), WorkspaceRemoveError> {
            if path == self.workspaces.startup {
                return Err(WorkspaceRemoveError::IsStartup);
            }
            if self.workspaces.in_use.as_deref() == Some(path) {
                return Err(WorkspaceRemoveError::InUse(
                    "an active workflow run".to_string(),
                ));
            }
            let mut loaded = self.workspaces.loaded.lock().await;
            if loaded.remove(path).is_none() {
                return Err(WorkspaceRemoveError::NotFound);
            }
            Ok(())
        }

        async fn workflow_controller(
            &self,
            workspace: Option<&str>,
        ) -> Option<Arc<crate::workflow::Controller>> {
            let path = workspace.unwrap_or(&self.builder.workspace_path);
            self.workflows.get(path).cloned()
        }

        async fn workflow_controllers(&self) -> Vec<Arc<crate::workflow::Controller>> {
            let startup = &self.builder.workspace_path;
            let mut rest: Vec<(&String, &Arc<crate::workflow::Controller>)> = self
                .workflows
                .iter()
                .filter(|(path, _)| *path != startup)
                .collect();
            rest.sort_by(|a, b| a.0.cmp(b.0));
            let mut all = Vec::with_capacity(self.workflows.len());
            if let Some(controller) = self.workflows.get(startup) {
                all.push(Arc::clone(controller));
            }
            all.extend(
                rest.into_iter()
                    .map(|(_, controller)| Arc::clone(controller)),
            );
            all
        }

        fn sandbox_reload_available(&self) -> bool {
            self.sandbox_reload.is_some()
        }

        async fn reload_sandbox(&self) -> Option<Result<SandboxInfo, String>> {
            self.sandbox_reload.clone()
        }

        async fn reload_other_sandboxes(&self) -> Vec<(String, Result<SandboxInfo, String>)> {
            self.sandbox_reload_others.clone()
        }

        async fn diff_runner(
            &self,
            workspace: &str,
        ) -> Option<(Arc<dyn crate::sandbox::CommandExecutor>, Vec<String>)> {
            if workspace == self.workspaces.startup {
                self.diff_runner.clone()
            } else {
                None
            }
        }
    }

    // ---- the harness ----

    /// A `Vec<u8>` the test reads back after the logger wrote to it.
    struct SharedSink(Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for SharedSink {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[derive(Default)]
    struct HarnessOptions {
        script: Script,
        fixed_id: Option<String>,
        create_error: Option<String>,
        open_error: Option<String>,
        open_gate: Option<CancellationToken>,
        list: Option<ListResult>,
        list_by_workspace: HashMap<String, ListResult>,
        tasks: Option<Arc<crate::subagent::tasks::Tasks>>,
        task_recorder: Option<Arc<crate::subagent::record::Store>>,
        workflows: Option<Arc<crate::workflow::Controller>>,
        /// Extra workflow controllers keyed by workspace path, for a second
        /// (non-startup) workspace's workflow routes.
        workflow_workspaces: HashMap<String, Arc<crate::workflow::Controller>>,
        /// `startup` is always overwritten with the harness's own workspace;
        /// set `admitted`/`error`/`gate` for `/v1/workspaces` tests.
        workspaces: FakeWorkspaces,
        sandbox_reload: Option<Result<SandboxInfo, String>>,
        sandbox_reload_others: Vec<(String, Result<SandboxInfo, String>)>,
        /// Canned `Factory::diff_runner` answer for the startup workspace.
        diff_runner: Option<(Arc<dyn crate::sandbox::CommandExecutor>, Vec<String>)>,
        /// The startup workspace directory. Defaults to a fresh, empty temp
        /// directory; a test that runs real git commands (through a real
        /// `CommandExecutor`) sets this to the directory it initialized a
        /// repository in.
        workspace: Option<TempDir>,
        token: String,
        info: Info,
    }

    struct Harness {
        _workspace: TempDir,
        _sessions: TempDir,
        server: Arc<Server>,
        router: Router,
        factory: Arc<TestFactory>,
        provider: Arc<ScriptedProvider>,
        log: Arc<Mutex<Vec<u8>>>,
    }

    impl Harness {
        fn new() -> Self {
            Self::with(HarnessOptions::default())
        }

        fn with(options: HarnessOptions) -> Self {
            let workspace = options
                .workspace
                .unwrap_or_else(|| tempfile::tempdir().expect("workspace"));
            let sessions = tempfile::tempdir().expect("sessions");
            // Canonicalized so a real `sandbox::Executor` bound to the same
            // directory (a test's `diff_runner` against a real git
            // repository) accepts it: `Executor::valid_directory` requires
            // an already-canonical path, and macOS's `$TMPDIR` is a symlink.
            let canonical_workspace = std::fs::canonicalize(workspace.path())
                .unwrap_or_else(|_| workspace.path().to_path_buf());
            let builder = Arc::new(testutil::builder(&canonical_workspace, sessions.path()));
            let provider = ScriptedProvider::new(options.script);
            // Production always sets `Options.info.workspace` from the same
            // path as the startup workspace (`cli/serve.rs`); match that here
            // unless a test explicitly overrides `info` to check the field
            // itself (e.g. `the_info_endpoint_echoes_the_configured_info`).
            let mut info = options.info;
            if info.workspace.is_empty() {
                info.workspace = builder.workspace_path.clone();
            }
            let mut workspaces = options.workspaces;
            workspaces.startup = builder.workspace_path.clone();
            let mut workflows = options.workflow_workspaces;
            if let Some(controller) = &options.workflows {
                workflows.insert(builder.workspace_path.clone(), Arc::clone(controller));
            }
            let factory = Arc::new(TestFactory {
                builder,
                provider: Arc::clone(&provider),
                fixed_id: options.fixed_id,
                create_error: options.create_error,
                open_error: options.open_error,
                open_gate: options.open_gate,
                list: options.list,
                list_by_workspace: options.list_by_workspace,
                tasks: options.tasks,
                task_recorder: options.task_recorder,
                workspaces,
                workflows,
                sandbox_reload: options.sandbox_reload,
                sandbox_reload_others: options.sandbox_reload_others,
                diff_runner: options.diff_runner,
                create_calls: AtomicUsize::new(0),
                open_calls: AtomicUsize::new(0),
            });
            let log = Arc::new(Mutex::new(Vec::new()));
            let server = Server::new(Options {
                factory: Arc::clone(&factory) as Arc<dyn Factory>,
                info,
                token: options.token,
                logger: Some(Arc::new(Logger::new(Box::new(SharedSink(Arc::clone(
                    &log,
                )))))),
                workflows: options.workflows,
            });
            let router = server.router();
            Self {
                _workspace: workspace,
                _sessions: sessions,
                server,
                router,
                factory,
                provider,
                log,
            }
        }

        /// One request through the real router. `tower`'s `oneshot` needs no
        /// socket, so these tests run with no network access at all.
        async fn raw(
            &self,
            method: &str,
            path: &str,
            body: Option<&str>,
            headers: &[(&str, &[u8])],
        ) -> Response {
            let mut request = Request::builder().method(method).uri(path);
            for (name, value) in headers {
                request = request.header(*name, *value);
            }
            let request = request
                .body(body.map_or_else(Body::empty, |text| Body::from(text.to_string())))
                .expect("request");
            self.router
                .clone()
                .oneshot(request)
                .await
                .expect("infallible")
        }

        async fn send(&self, method: &str, path: &str, body: Option<&str>) -> Reply {
            self.send_with(method, path, body, &[]).await
        }

        async fn send_with(
            &self,
            method: &str,
            path: &str,
            body: Option<&str>,
            headers: &[(&str, &[u8])],
        ) -> Reply {
            let response = self.raw(method, path, body, headers).await;
            let status = response.status();
            let head = response.headers().clone();
            let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
                .await
                .expect("body");
            Reply {
                status,
                headers: head,
                body: String::from_utf8_lossy(&bytes).into_owned(),
            }
        }

        async fn create(&self) -> String {
            let reply = self.send("POST", "/v1/sessions", None).await;
            assert_eq!(reply.status, StatusCode::CREATED, "{}", reply.body);
            reply.json()["id"].as_str().expect("id").to_string()
        }

        fn logged(&self) -> String {
            String::from_utf8_lossy(&self.log.lock().unwrap_or_else(|poison| poison.into_inner()))
                .into_owned()
        }

        async fn metrics_body(&self) -> String {
            self.send("GET", "/metrics", None).await.body
        }

        /// Polls the turn until it leaves `running`.
        async fn wait_turn_done(&self, session: &str, turn_id: &str) -> Value {
            for _ in 0..600 {
                let body = self
                    .send(
                        "GET",
                        &format!("/v1/sessions/{session}/turns/{turn_id}"),
                        None,
                    )
                    .await
                    .json();
                if body["status"] != turn::TURN_RUNNING {
                    return body;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            panic!("turn {turn_id} did not finish in time");
        }

        /// Waits for the turn object itself, for the paths that have no HTTP
        /// route left (a deleted session, a closed server).
        async fn wait_done(turn: &Arc<Turn>) {
            for _ in 0..600 {
                if turn.is_done() {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            panic!("turn did not finish in time");
        }

        fn active_turn(&self, id: &str) -> Arc<Turn> {
            self.server
                .lookup(id)
                .and_then(|session| session.current_turn())
                .expect("an active turn")
        }
    }

    struct Reply {
        status: StatusCode,
        headers: HeaderMap,
        body: String,
    }

    impl Reply {
        fn json(&self) -> Value {
            serde_json::from_str(&self.body)
                .unwrap_or_else(|error| panic!("decode {:?}: {error}", self.body))
        }

        fn header(&self, name: &str) -> &str {
            self.headers
                .get(name)
                .and_then(|value| value.to_str().ok())
                .unwrap_or_default()
        }
    }

    // ---- the SSE reader ----

    /// Reads frames off a streaming body with the same splitter the browser
    /// client uses ([`parse_frames`]), so a framing change fails here too.
    struct SseReader {
        stream: axum::body::BodyDataStream,
        rest: String,
        pending: std::collections::VecDeque<Frame>,
    }

    impl SseReader {
        fn new(response: Response) -> Self {
            Self {
                stream: response.into_body().into_data_stream(),
                rest: String::new(),
                pending: std::collections::VecDeque::new(),
            }
        }

        async fn next(&mut self) -> Option<Frame> {
            loop {
                if let Some(frame) = self.pending.pop_front() {
                    return Some(frame);
                }
                let chunk = self.stream.next().await?.expect("chunk");
                self.rest.push_str(&String::from_utf8_lossy(&chunk));
                let parsed = parse_frames(&self.rest);
                self.rest = parsed.rest;
                self.pending.extend(parsed.frames);
            }
        }

        async fn all(mut self) -> Vec<Frame> {
            let mut frames = Vec::new();
            while let Some(frame) = self.next().await {
                frames.push(frame);
            }
            frames
        }
    }

    /// Starts a streaming turn and returns its reader plus the turn id, read
    /// out of the `agent_started` frame that always leads the stream.
    async fn start_stream(harness: &Harness, session: &str) -> (SseReader, String) {
        let response = harness
            .raw(
                "POST",
                &format!("/v1/sessions/{session}/turns"),
                Some(r#"{"text":"hi"}"#),
                &[],
            )
            .await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get(header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok()),
            Some("text/event-stream")
        );
        let mut reader = SseReader::new(response);
        let first = reader.next().await.expect("agent_started frame");
        assert_eq!(first.event, "agent_started");
        assert_eq!(first.id, Some(0));
        let data: Value = serde_json::from_str(&first.data).expect("frame data");
        let turn_id = data["turn_id"].as_str().expect("turn_id").to_string();
        assert!(!turn_id.is_empty());
        (reader, turn_id)
    }

    fn gated() -> (CancellationToken, HarnessOptions) {
        let gate = CancellationToken::new();
        (
            gate.clone(),
            HarnessOptions {
                script: Script {
                    // One delta so the buffer holds a sequence 1 while the
                    // gate is shut; several tests resume from it.
                    deltas: vec!["...".to_string()],
                    gate: Some(gate),
                    ..Script::default()
                },
                ..HarnessOptions::default()
            },
        )
    }

    // ---- sessions ----

    #[tokio::test]
    async fn renaming_a_session_updates_the_session_and_the_list() {
        let harness = Harness::with(HarnessOptions {
            list: Some(ListResult::default()),
            ..HarnessOptions::default()
        });
        let id = harness.create().await;

        let reply = harness
            .send(
                "PATCH",
                &format!("/v1/sessions/{id}"),
                Some(r#"{"name":"dev"}"#),
            )
            .await;
        assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
        assert_eq!(reply.json()["name"], "dev");

        let list = harness.send("GET", "/v1/sessions", None).await.json();
        assert_eq!(list["sessions"].as_array().expect("rows").len(), 1);
        assert_eq!(list["sessions"][0]["name"], "dev");
    }

    #[tokio::test]
    async fn renaming_rejects_an_unknown_session_and_a_blank_name() {
        let harness = Harness::new();
        let reply = harness
            .send("PATCH", "/v1/sessions/missing", Some(r#"{"name":"dev"}"#))
            .await;
        assert_eq!(reply.status, StatusCode::NOT_FOUND);

        let id = harness.create().await;
        let reply = harness
            .send(
                "PATCH",
                &format!("/v1/sessions/{id}"),
                Some(r#"{"name":" \t"}"#),
            )
            .await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST);
        assert_eq!(reply.json()["error"]["code"], "bad_request");
    }

    #[tokio::test]
    async fn creating_a_session_answers_201_and_counts_one_open_session() {
        let harness = Harness::new();
        let reply = harness.send("POST", "/v1/sessions", None).await;
        assert_eq!(reply.status, StatusCode::CREATED, "{}", reply.body);
        assert_eq!(reply.header("content-type"), "application/json");

        let body = reply.json();
        assert!(!body["id"].as_str().expect("id").is_empty());
        assert_eq!(body["provider"], "openai-compatible");
        assert_eq!(body["model"], "test-model");
        assert_eq!(body["context_window"], 128_000);
        assert!(body["sandbox"]["summary"].as_str().is_some());

        assert!(
            harness
                .metrics_body()
                .await
                .contains("otto_sessions_open 1")
        );
        let health = harness.send("GET", "/healthz", None).await.json();
        assert_eq!(health["status"], "ok");
        assert_eq!(health["sessions_open"], 1);
        assert_eq!(harness.factory.create_calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn metrics_expose_each_open_session_context() {
        let harness = Harness::with(HarnessOptions {
            fixed_id: Some("ctx-session".to_string()),
            ..HarnessOptions::default()
        });
        harness.create().await;

        let body = harness.metrics_body().await;
        let labels =
            r#"{session_id="ctx-session",provider="openai-compatible",model="test-model"}"#;
        for want in [
            format!("otto_session_context_window_tokens{labels} 128000"),
            format!("otto_session_context_input_tokens_pending{labels} 0"),
        ] {
            assert!(body.contains(&want), "missing {want}:\n{body}");
        }
        // A session that has never counted its context reports no input
        // gauge at all; the metric is documented "when available".
        assert!(!body.contains(&format!("otto_session_context_input_tokens{labels}")));
    }

    #[tokio::test]
    async fn resuming_an_unknown_session_is_404_not_found() {
        let harness = Harness::with(HarnessOptions {
            open_error: Some(SESSION_NOT_FOUND.to_string()),
            ..HarnessOptions::default()
        });
        let reply = harness
            .send("POST", "/v1/sessions", Some(r#"{"resume":"nope"}"#))
            .await;
        assert_eq!(reply.status, StatusCode::NOT_FOUND);
        assert_eq!(reply.json()["error"]["code"], "not_found");
    }

    #[tokio::test]
    async fn resuming_an_open_session_does_not_call_open_again() {
        let harness = Harness::new();
        let id = harness.create().await;

        let reply = harness
            .send(
                "POST",
                "/v1/sessions",
                Some(&format!(r#"{{"resume":"{id}"}}"#)),
            )
            .await;
        assert_eq!(reply.status, StatusCode::OK);
        assert_eq!(reply.json()["id"], id.as_str());
        assert_eq!(harness.factory.open_calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_resumes_of_one_id_call_open_once() {
        let gate = CancellationToken::new();
        let harness = Arc::new(Harness::with(HarnessOptions {
            fixed_id: Some("shared".to_string()),
            open_gate: Some(gate.clone()),
            ..HarnessOptions::default()
        }));

        let resumes: Vec<_> = (0..8)
            .map(|_| {
                let harness = Arc::clone(&harness);
                tokio::spawn(async move {
                    harness
                        .send("POST", "/v1/sessions", Some(r#"{"resume":"shared"}"#))
                        .await
                        .status
                })
            })
            .collect();

        // Let the first resume reach `open` before any of them may return.
        while harness.factory.open_calls.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
        gate.cancel();

        for resume in resumes {
            assert_eq!(resume.await.expect("resume"), StatusCode::OK);
        }
        assert_eq!(harness.factory.open_calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_session_is_gone_after_delete() {
        let harness = Harness::new();
        assert_eq!(
            harness
                .send("GET", "/v1/sessions/does-not-exist", None)
                .await
                .status,
            StatusCode::NOT_FOUND
        );

        let id = harness.create().await;
        assert_eq!(
            harness
                .send("GET", &format!("/v1/sessions/{id}"), None)
                .await
                .status,
            StatusCode::OK
        );
        let deleted = harness
            .send("DELETE", &format!("/v1/sessions/{id}"), None)
            .await;
        assert_eq!(deleted.status, StatusCode::NO_CONTENT);
        assert_eq!(deleted.body, "");
        assert_eq!(
            harness
                .send("GET", &format!("/v1/sessions/{id}"), None)
                .await
                .status,
            StatusCode::NOT_FOUND
        );
        assert!(
            harness
                .metrics_body()
                .await
                .contains("otto_sessions_open 0")
        );
    }

    #[tokio::test]
    async fn an_empty_history_is_an_empty_array() {
        let harness = Harness::new();
        let id = harness.create().await;
        let reply = harness
            .send("GET", &format!("/v1/sessions/{id}/history"), None)
            .await;
        assert_eq!(reply.status, StatusCode::OK);
        assert_eq!(reply.body, "[]");
        assert_eq!(
            harness
                .send("GET", "/v1/sessions/missing/history", None)
                .await
                .status,
            StatusCode::NOT_FOUND
        );
    }

    #[tokio::test]
    async fn the_context_report_is_served_per_open_session() {
        let harness = Harness::new();
        let id = harness.create().await;
        let reply = harness
            .send("GET", &format!("/v1/sessions/{id}/context"), None)
            .await;
        assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
        // The harness runner has no prompt and no tools; the section contents
        // are covered by the otto-core and runtime-builder tests.
        let report = reply.json();
        assert_eq!(report["model"], "test-model");
        assert!(report["sections"].is_array(), "{}", reply.body);
        assert!(report["reported_input_tokens"].is_null());
        assert_eq!(
            harness
                .send("GET", "/v1/sessions/missing/context", None)
                .await
                .status,
            StatusCode::NOT_FOUND
        );
    }

    #[tokio::test]
    async fn listing_merges_open_sessions_over_the_disk_list() {
        let harness = Harness::with(HarnessOptions {
            list: Some(ListResult::default()),
            ..HarnessOptions::default()
        });
        let empty = harness.send("GET", "/v1/sessions", None).await.json();
        assert_eq!(empty["sessions"].as_array().expect("rows").len(), 0);

        let id = harness.create().await;
        let body = harness.send("GET", "/v1/sessions", None).await.json();
        let rows = body["sessions"].as_array().expect("rows");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["id"], id.as_str());
        assert_eq!(rows[0]["open"], true);
        assert!(
            rows[0].get("path").is_none(),
            "an open-only row has no path"
        );
    }

    // ---- sessions across workspaces ----

    #[tokio::test]
    async fn creating_a_session_in_an_unadmitted_workspace_answers_403() {
        let harness = Harness::new();
        let reply = harness
            .send(
                "POST",
                "/v1/sessions",
                Some(r#"{"workspace":"/elsewhere"}"#),
            )
            .await;
        assert_eq!(reply.status, StatusCode::FORBIDDEN, "{}", reply.body);
        assert_eq!(reply.json()["error"]["code"], "WORKSPACE_NOT_ADMITTED");
    }

    #[tokio::test]
    async fn creating_a_session_in_an_invalid_workspace_answers_400() {
        let harness = Harness::new();
        let reply = harness
            .send("POST", "/v1/sessions", Some(r#"{"workspace":"relative"}"#))
            .await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{}", reply.body);
        assert_eq!(reply.json()["error"]["code"], "INVALID_WORKSPACE");
    }

    #[tokio::test]
    async fn creating_a_session_in_an_admitted_workspace_succeeds() {
        let harness = Harness::with(HarnessOptions {
            workspaces: FakeWorkspaces {
                admitted: vec!["/other".to_string()],
                ..FakeWorkspaces::default()
            },
            ..HarnessOptions::default()
        });
        let reply = harness
            .send("POST", "/v1/sessions", Some(r#"{"workspace":"/other"}"#))
            .await;
        assert_eq!(reply.status, StatusCode::CREATED, "{}", reply.body);
    }

    #[tokio::test]
    async fn resuming_by_id_finds_the_session_with_or_without_a_workspace_field() {
        let harness = Harness::with(HarnessOptions {
            workspaces: FakeWorkspaces {
                admitted: vec!["/other".to_string()],
                ..FakeWorkspaces::default()
            },
            ..HarnessOptions::default()
        });
        let id = harness.create().await;

        let without = harness
            .send(
                "POST",
                "/v1/sessions",
                Some(&format!(r#"{{"resume":"{id}"}}"#)),
            )
            .await;
        assert_eq!(without.status, StatusCode::OK, "{}", without.body);
        assert_eq!(without.json()["id"], id.as_str());

        // Already open, so the registry answers without consulting the
        // named workspace at all.
        let with = harness
            .send(
                "POST",
                "/v1/sessions",
                Some(&format!(r#"{{"resume":"{id}","workspace":"/other"}}"#)),
            )
            .await;
        assert_eq!(with.status, StatusCode::OK, "{}", with.body);
        assert_eq!(with.json()["id"], id.as_str());
    }

    #[tokio::test]
    async fn listing_sessions_returns_every_workspace_tagged_by_default() {
        let harness = Harness::with(HarnessOptions {
            list: Some(ListResult {
                sessions: vec![
                    SessionInfo {
                        id: "a1".to_string(),
                        cwd: "/workspace-a".to_string(),
                        ..SessionInfo::default()
                    },
                    SessionInfo {
                        id: "b1".to_string(),
                        cwd: "/workspace-b".to_string(),
                        ..SessionInfo::default()
                    },
                ],
                skipped: 0,
            }),
            ..HarnessOptions::default()
        });
        let body = harness.send("GET", "/v1/sessions", None).await.json();
        let rows = body["sessions"].as_array().expect("rows");
        assert_eq!(rows.len(), 2);
        let row = |id: &str| rows.iter().find(|row| row["id"] == id).expect("row");
        assert_eq!(row("a1")["workspace"], "/workspace-a");
        assert_eq!(row("b1")["workspace"], "/workspace-b");
    }

    #[tokio::test]
    async fn listing_sessions_with_a_workspace_query_filters_to_that_workspace() {
        let mut list_by_workspace = HashMap::new();
        list_by_workspace.insert(
            "/other".to_string(),
            ListResult {
                sessions: vec![SessionInfo {
                    id: "b1".to_string(),
                    cwd: "/other".to_string(),
                    ..SessionInfo::default()
                }],
                skipped: 0,
            },
        );
        let harness = Harness::with(HarnessOptions {
            list: Some(ListResult {
                sessions: vec![SessionInfo {
                    id: "a1".to_string(),
                    cwd: "/workspace-a".to_string(),
                    ..SessionInfo::default()
                }],
                skipped: 0,
            }),
            list_by_workspace,
            workspaces: FakeWorkspaces {
                admitted: vec!["/other".to_string()],
                ..FakeWorkspaces::default()
            },
            ..HarnessOptions::default()
        });

        let body = harness
            .send("GET", "/v1/sessions?workspace=/other", None)
            .await
            .json();
        let rows = body["sessions"].as_array().expect("rows");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["id"], "b1");
        assert_eq!(rows[0]["workspace"], "/other");
    }

    #[tokio::test]
    async fn listing_sessions_with_an_unadmitted_workspace_answers_403() {
        let harness = Harness::new();
        let reply = harness
            .send("GET", "/v1/sessions?workspace=/elsewhere", None)
            .await;
        assert_eq!(reply.status, StatusCode::FORBIDDEN, "{}", reply.body);
        assert_eq!(reply.json()["error"]["code"], "WORKSPACE_NOT_ADMITTED");
    }

    // ---- turns ----

    #[tokio::test]
    async fn posting_a_turn_streams_server_sent_events() {
        let harness = Harness::with(HarnessOptions {
            script: Script {
                deltas: vec!["hel".to_string(), "lo".to_string()],
                usage: Some(Usage {
                    input_tokens: 5,
                    output_tokens: 7,
                    cached_input_tokens: 0,
                }),
                ..Script::default()
            },
            ..HarnessOptions::default()
        });
        let id = harness.create().await;
        let (reader, turn_id) = start_stream(&harness, &id).await;

        let frames = reader.all().await;
        let events: Vec<&str> = frames.iter().map(|frame| frame.event.as_str()).collect();
        assert!(events.contains(&"text_delta"), "frames = {events:?}");
        assert_eq!(
            events.last(),
            Some(&"agent_finished"),
            "frames = {events:?}"
        );

        let summary = harness.wait_turn_done(&id, &turn_id).await;
        assert_eq!(summary["text"], "hello");
        assert_eq!(summary["status"], turn::TURN_OK);
        assert_eq!(summary["usage"]["input_tokens"], 5);
        assert_eq!(summary["usage"]["output_tokens"], 7);
        assert_eq!(summary["usage_present"], true);

        let body = harness.metrics_body().await;
        for want in [
            r#"otto_turns_total{status="ok"} 1"#,
            r#"otto_provider_tokens_total{kind="input"} 5"#,
            r#"otto_provider_tokens_total{kind="output"} 7"#,
        ] {
            assert!(body.contains(want), "missing {want}:\n{body}");
        }
    }

    #[tokio::test]
    async fn posting_a_turn_with_stream_false_answers_the_summary() {
        let harness = Harness::with(HarnessOptions {
            script: Script {
                deltas: vec!["ok".to_string()],
                ..Script::default()
            },
            ..HarnessOptions::default()
        });
        let id = harness.create().await;
        let reply = harness
            .send(
                "POST",
                &format!("/v1/sessions/{id}/turns"),
                Some(r#"{"text":"hi","stream":false}"#),
            )
            .await;
        assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
        assert_eq!(reply.header("content-type"), "application/json");
        let summary = reply.json();
        assert_eq!(summary["status"], turn::TURN_OK);
        assert_eq!(summary["text"], "ok");
        assert!(summary["finished_at"].is_string());
    }

    #[tokio::test]
    async fn posting_an_image_persists_it_with_the_user_prompt() {
        let harness = Harness::new();
        let id = harness.create().await;
        let reply = harness
            .send(
                "POST",
                &format!("/v1/sessions/{id}/turns"),
                Some(
                    r#"{"text":"read it","image":{"data":"iVBORw0KGgo=","mime_type":"image/png"},"stream":false}"#,
                ),
            )
            .await;
        assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);

        let history = harness
            .send("GET", &format!("/v1/sessions/{id}/history"), None)
            .await
            .json();
        assert_eq!(history[0]["blocks"][1]["type"], "image");
        assert_eq!(history[0]["blocks"][1]["mime_type"], "image/png");
        assert_eq!(history[0]["blocks"][1]["data"], "iVBORw0KGgo=");
    }

    #[tokio::test]
    async fn posting_an_image_without_text_starts_a_turn() {
        let harness = Harness::new();
        let id = harness.create().await;
        let reply = harness
            .send(
                "POST",
                &format!("/v1/sessions/{id}/turns"),
                Some(
                    r#"{"text":"","image":{"data":"iVBORw0KGgo=","mime_type":"image/png"},"stream":false}"#,
                ),
            )
            .await;
        assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);

        let history = harness
            .send("GET", &format!("/v1/sessions/{id}/history"), None)
            .await
            .json();
        assert_eq!(history[0]["blocks"].as_array().unwrap().len(), 1);
        assert_eq!(history[0]["blocks"][0]["type"], "image");
    }

    #[tokio::test]
    async fn image_turns_override_the_small_default_body_limit() {
        let harness = Harness::new();
        let id = harness.create().await;
        let mut bytes = vec![0; 1 << 20];
        bytes[..8].copy_from_slice(b"\x89PNG\r\n\x1a\n");
        let body = serde_json::json!({
            "text": "read it",
            "image": { "data": BASE64.encode(bytes), "mime_type": "image/png" },
            "stream": false
        })
        .to_string();

        let reply = harness
            .send("POST", &format!("/v1/sessions/{id}/turns"), Some(&body))
            .await;
        assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    }

    #[tokio::test]
    async fn posting_a_malformed_image_is_a_bad_request() {
        let harness = Harness::new();
        let id = harness.create().await;
        let reply = harness
            .send(
                "POST",
                &format!("/v1/sessions/{id}/turns"),
                Some(
                    r#"{"text":"read it","image":{"data":"/9j/","mime_type":"image/png"},"stream":false}"#,
                ),
            )
            .await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST);
        assert_eq!(reply.json()["error"]["message"], "image is invalid");
    }

    #[tokio::test]
    async fn a_started_turn_carries_the_user_trigger_everywhere() {
        let harness = Harness::new();
        let id = harness.create().await;
        let summary = harness
            .send(
                "POST",
                &format!("/v1/sessions/{id}/turns"),
                Some(r#"{"text":"hi","stream":false}"#),
            )
            .await
            .json();
        assert_eq!(summary["trigger"], turn::TRIGGER_USER);
        let turn_id = summary["id"].as_str().expect("turn id");

        let got = harness
            .send("GET", &format!("/v1/sessions/{id}/turns/{turn_id}"), None)
            .await
            .json();
        assert_eq!(got["trigger"], turn::TRIGGER_USER);

        let session = harness
            .send("GET", &format!("/v1/sessions/{id}"), None)
            .await
            .json();
        assert_eq!(session["turn"]["trigger"], turn::TRIGGER_USER);
        assert_eq!(session["turn"]["id"], turn_id);
    }

    #[tokio::test]
    async fn an_unknown_turn_is_404_on_every_turn_route() {
        let harness = Harness::new();
        let id = harness.create().await;
        for (method, path) in [
            ("GET", format!("/v1/sessions/{id}/turns/nope")),
            ("GET", format!("/v1/sessions/{id}/turns/nope/events")),
            ("POST", format!("/v1/sessions/{id}/turns/nope/cancel")),
            ("GET", "/v1/sessions/missing/turns/nope".to_string()),
        ] {
            let reply = harness.send(method, &path, None).await;
            assert_eq!(reply.status, StatusCode::NOT_FOUND, "{method} {path}");
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn turn_events_resume_from_after_and_from_last_event_id() {
        let (gate, options) = gated();
        let harness = Harness::with(options);
        let id = harness.create().await;
        let (_primary, turn_id) = start_stream(&harness, &id).await;
        harness.provider.wait_started(1).await;

        // after=0 replays from sequence 1, skipping agent_started.
        let response = harness
            .raw(
                "GET",
                &format!("/v1/sessions/{id}/turns/{turn_id}/events?after=0"),
                None,
                &[],
            )
            .await;
        assert_eq!(response.status(), StatusCode::OK);
        let mut resumed = SseReader::new(response);
        assert_eq!(resumed.next().await.expect("a frame").id, Some(1));

        // Last-Event-ID: 0 means the same thing.
        let response = harness
            .raw(
                "GET",
                &format!("/v1/sessions/{id}/turns/{turn_id}/events"),
                None,
                &[("last-event-id", b"0")],
            )
            .await;
        let mut replayed = SseReader::new(response);
        assert_eq!(replayed.next().await.expect("a frame").id, Some(1));

        gate.cancel();
        let tail = resumed.all().await;
        assert_eq!(
            tail.last().map(|frame| frame.event.as_str()),
            Some("agent_finished")
        );

        let reply = harness
            .send(
                "GET",
                &format!("/v1/sessions/{id}/turns/{turn_id}/events?after=x"),
                None,
            )
            .await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST);
        assert_eq!(reply.json()["error"]["code"], "bad_request");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn starting_a_turn_validates_the_body_and_refuses_a_second_turn() {
        let (gate, options) = gated();
        let harness = Harness::with(options);
        let id = harness.create().await;

        for body in [
            None,
            Some("{}"),
            Some(r#"{"text":""}"#),
            Some(r#"{"text":"   "}"#),
        ] {
            let reply = harness
                .send("POST", &format!("/v1/sessions/{id}/turns"), body)
                .await;
            assert_eq!(reply.status, StatusCode::BAD_REQUEST, "body {body:?}");
            assert_eq!(reply.json()["error"]["code"], "bad_request");
        }
        assert_eq!(
            harness
                .send(
                    "POST",
                    "/v1/sessions/missing/turns",
                    Some(r#"{"text":"hi"}"#)
                )
                .await
                .status,
            StatusCode::NOT_FOUND
        );

        let (_primary, _turn_id) = start_stream(&harness, &id).await;
        harness.provider.wait_started(1).await;

        let reply = harness
            .send(
                "POST",
                &format!("/v1/sessions/{id}/turns"),
                Some(r#"{"text":"again"}"#),
            )
            .await;
        assert_eq!(reply.status, StatusCode::CONFLICT);
        assert_eq!(reply.json()["error"]["code"], "turn_active");
        gate.cancel();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn cancelling_a_turn_finishes_it_as_canceled() {
        let (_gate, options) = gated();
        let harness = Harness::with(options);
        let id = harness.create().await;
        let (_stream, turn_id) = start_stream(&harness, &id).await;
        harness.provider.wait_started(1).await;

        let reply = harness
            .send(
                "POST",
                &format!("/v1/sessions/{id}/turns/{turn_id}/cancel"),
                None,
            )
            .await;
        assert_eq!(reply.status, StatusCode::ACCEPTED);
        assert_eq!(reply.body, "");

        let summary = harness.wait_turn_done(&id, &turn_id).await;
        assert_eq!(summary["status"], turn::TURN_CANCELED);
        assert!(
            harness
                .metrics_body()
                .await
                .contains(r#"otto_turns_total{status="canceled"} 1"#)
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn deleting_a_session_cancels_its_active_turn() {
        let (_gate, options) = gated();
        let harness = Harness::with(options);
        let id = harness.create().await;
        let (_stream, _turn_id) = start_stream(&harness, &id).await;
        harness.provider.wait_started(1).await;
        let active = harness.active_turn(&id);

        let started = Instant::now();
        let reply = harness
            .send("DELETE", &format!("/v1/sessions/{id}"), None)
            .await;
        assert_eq!(reply.status, StatusCode::NO_CONTENT);
        assert!(started.elapsed() < Duration::from_secs(5), "delete blocked");

        Harness::wait_done(&active).await;
        assert_eq!(active.summary().status, turn::TURN_CANCELED);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn two_sessions_run_their_turns_at_the_same_time() {
        let (gate, options) = gated();
        let harness = Harness::with(options);
        let first = harness.create().await;
        let second = harness.create().await;

        let (_a, _) = start_stream(&harness, &first).await;
        harness.provider.wait_started(1).await;
        let (_b, _) = start_stream(&harness, &second).await;
        // The second turn reaching the provider while the first still holds
        // the gate is the assertion; a per-process lock would hang here.
        harness.provider.wait_started(2).await;
        gate.cancel();
    }

    #[tokio::test]
    async fn a_failing_turn_finishes_with_the_error_status() {
        let harness = Harness::with(HarnessOptions {
            script: Script {
                error: Some("provider exploded".to_string()),
                ..Script::default()
            },
            ..HarnessOptions::default()
        });
        let id = harness.create().await;
        let summary = harness
            .send(
                "POST",
                &format!("/v1/sessions/{id}/turns"),
                Some(r#"{"text":"hi","stream":false}"#),
            )
            .await
            .json();
        assert_eq!(summary["status"], turn::TURN_ERROR);
        assert!(!summary["error"].as_str().expect("error").is_empty());
        assert!(
            harness
                .metrics_body()
                .await
                .contains(r#"otto_turns_total{status="error"} 1"#)
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn dropping_the_event_stream_does_not_cancel_the_turn() {
        let (gate, options) = gated();
        let harness = Harness::with(options);
        let id = harness.create().await;
        let (stream, turn_id) = start_stream(&harness, &id).await;
        harness.provider.wait_started(1).await;

        drop(stream); // the browser disconnects mid-turn
        gate.cancel();

        let summary = harness.wait_turn_done(&id, &turn_id).await;
        assert_eq!(summary["status"], turn::TURN_OK);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn closing_the_server_cancels_the_active_turn() {
        let (_gate, options) = gated();
        let harness = Harness::with(options);
        let id = harness.create().await;
        let (_stream, _turn_id) = start_stream(&harness, &id).await;
        harness.provider.wait_started(1).await;
        let active = harness.active_turn(&id);

        let started = Instant::now();
        harness.server.close().await.expect("close");
        assert!(started.elapsed() < Duration::from_secs(5), "close blocked");

        Harness::wait_done(&active).await;
        assert_eq!(active.summary().status, turn::TURN_CANCELED);
        assert!(
            harness
                .metrics_body()
                .await
                .contains("otto_sessions_open 0")
        );
    }

    // ---- errors, logging, and the fixed endpoints ----

    #[tokio::test]
    async fn a_create_failure_answers_a_fixed_internal_error_and_logs_the_real_one() {
        let harness = Harness::with(HarnessOptions {
            create_error: Some("disk full".to_string()),
            ..HarnessOptions::default()
        });
        let reply = harness.send("POST", "/v1/sessions", None).await;
        assert_eq!(reply.status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(
            reply.body,
            r#"{"error":{"code":"internal","message":"internal error"}}"#
        );
        assert!(
            harness.logged().contains("disk full"),
            "the real error is logged:\n{}",
            harness.logged()
        );
    }

    #[tokio::test]
    async fn neither_the_log_nor_the_metrics_carry_prompt_text() {
        const SECRET: &str = "quick-brown-fox-prompt-marker";
        let harness = Harness::with(HarnessOptions {
            script: Script {
                echo: true,
                ..Script::default()
            },
            ..HarnessOptions::default()
        });
        let id = harness.create().await;
        let summary = harness
            .send(
                "POST",
                &format!("/v1/sessions/{id}/turns"),
                Some(&format!(r#"{{"text":"{SECRET}","stream":false}}"#)),
            )
            .await
            .json();
        assert_eq!(summary["text"], SECRET, "the turn must actually have run");

        assert!(
            !harness.logged().contains(SECRET),
            "log: {}",
            harness.logged()
        );
        assert!(!harness.metrics_body().await.contains(SECRET));
    }

    #[tokio::test]
    async fn the_request_id_is_truncated_to_64_bytes_and_filtered_to_ascii() {
        let harness = Harness::new();
        // 0x85 is valid obs-text but not printable ASCII. It sits inside the
        // first 64 bytes, so truncation keeps it and the filter drops it.
        let mut raw = vec![b'a'; 30];
        raw.push(0x85);
        raw.extend(std::iter::repeat_n(b'a', 40));
        let reply = harness
            .send_with("GET", "/healthz", None, &[("x-request-id", &raw)])
            .await;
        assert_eq!(reply.header("x-request-id"), "a".repeat(63));

        let generated = harness.send("GET", "/healthz", None).await;
        assert_eq!(
            generated.header("x-request-id").len(),
            32,
            "an absent header gets a generated id"
        );
    }

    #[tokio::test]
    async fn the_info_endpoint_echoes_the_configured_info() {
        let info = Info {
            workspace: "/ws".to_string(),
            provider: "openai-compatible".to_string(),
            profile: "alpha".to_string(),
            model: "test-model".to_string(),
            thinking: "high".to_string(),
            sandbox: "off".to_string(),
            profiles: vec!["alpha".to_string(), "beta".to_string()],
        };
        let harness = Harness::with(HarnessOptions {
            info: info.clone(),
            ..HarnessOptions::default()
        });
        let reply = harness.send("GET", "/v1/info", None).await;
        assert_eq!(reply.status, StatusCode::OK);
        assert_eq!(reply.json(), serde_json::to_value(&info).expect("info"));
    }

    #[tokio::test]
    async fn the_usage_endpoint_returns_persisted_aggregates() {
        let harness = Harness::new();
        let reply = harness.send("GET", "/v1/usage", None).await;
        assert_eq!(reply.status, StatusCode::OK);
        assert_eq!(
            reply.json(),
            serde_json::json!({
                "requests": 0,
                "reported_requests": 0,
                "input_tokens": 0,
                "output_tokens": 0,
                "cached_input_tokens": 0,
                "cache_hit_rate": 0.0
            })
        );
    }

    #[tokio::test]
    async fn the_daily_usage_endpoint_validates_its_range() {
        let harness = Harness::new();
        let reply = harness.send("GET", "/v1/usage/daily?days=0", None).await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST);
        assert_eq!(reply.json()["error"]["code"], "invalid_usage_range");

        let reply = harness.send("GET", "/v1/usage/daily?days=7", None).await;
        assert_eq!(reply.status, StatusCode::OK);
        assert_eq!(reply.json()["summary"]["requests"], 0);
        assert_eq!(reply.json()["daily"].as_array().expect("daily").len(), 7);
    }

    #[tokio::test]
    async fn the_openapi_endpoint_serves_the_embedded_document() {
        let harness = Harness::new();
        let reply = harness.send("GET", "/v1/openapi.yaml", None).await;
        assert_eq!(reply.status, StatusCode::OK);
        assert_eq!(reply.header("content-type"), "application/yaml");
        assert!(reply.body.starts_with("openapi:"), "{}", &reply.body[..40]);
    }

    struct TestWorkflowExecutor;

    #[async_trait::async_trait]
    impl crate::workflow::Executor for TestWorkflowExecutor {
        async fn execute(
            &self,
            attempt: crate::workflow::Attempt,
            _cancel: &CancellationToken,
        ) -> Result<String, String> {
            Ok(format!("{} done", attempt.step_id))
        }
    }

    /// A real, workspace-scoped workflow controller backed by an in-memory
    /// store, for exercising the HTTP routes end to end.
    fn workflow_test_controller(workspace: &str) -> Arc<crate::workflow::Controller> {
        let store = Arc::new(crate::workflow::Store::open_in_memory());
        let transcripts = tempfile::tempdir().expect("transcripts").keep();
        crate::workflow::Controller::new(
            store,
            crate::workflow::Catalog::from_definitions(vec![crate::workflow::Definition {
                name: "review".to_string(),
                description: String::new(),
                hash: "0".repeat(64),
                agents: vec![crate::workflow::AgentSnapshot {
                    name: "worker".to_string(),
                    ..crate::workflow::AgentSnapshot::default()
                }],
                steps: vec![crate::workflow::Step {
                    id: "work".to_string(),
                    kind: crate::workflow::StepKind::Agent,
                    agent: "worker".to_string(),
                    prompt: "work".to_string(),
                    needs: Vec::new(),
                }],
            }]),
            Arc::new(TestWorkflowExecutor),
            workspace.to_string(),
            transcripts,
            crate::workflow::RuntimeIdentity::default(),
            1,
        )
    }

    #[tokio::test]
    async fn workflow_routes_start_and_report_a_durable_run() {
        let controller = workflow_test_controller("/workspace");
        let harness = Harness::with(HarnessOptions {
            workflows: Some(controller),
            ..HarnessOptions::default()
        });

        let reply = harness
            .send_with(
                "POST",
                "/v1/workflows",
                Some(r#"{"name":"review","input":"request"}"#),
                &[("content-type", b"application/json")],
            )
            .await;
        assert_eq!(reply.status, StatusCode::CREATED, "{}", reply.body);
        let id = reply.json()["run"]["id"]
            .as_str()
            .expect("run id")
            .to_string();
        let mut status = String::new();
        for _ in 0..20 {
            let reply = harness
                .send("GET", &format!("/v1/workflows/{id}"), None)
                .await;
            assert_eq!(reply.status, StatusCode::OK);
            status = reply.json()["run"]["status"]
                .as_str()
                .unwrap_or_default()
                .to_string();
            if status == "succeeded" {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert_eq!(status, "succeeded");
        let events = harness
            .send("GET", &format!("/v1/workflows/{id}/events?after=0"), None)
            .await;
        assert_eq!(events.status, StatusCode::OK);
        assert_eq!(events.header("content-type"), "text/event-stream");
        assert!(events.body.contains("step_succeeded"), "{}", events.body);

        let fork = harness
            .send_with(
                "POST",
                &format!("/v1/workflows/{id}/fork"),
                Some(r#"{"after_step":"work"}"#),
                &[("content-type", b"application/json")],
            )
            .await;
        assert_eq!(fork.status, StatusCode::CREATED, "{}", fork.body);
        assert_eq!(
            fork.json()["run"]["forked_from_run_id"].as_str(),
            Some(id.as_str())
        );
        assert_eq!(
            fork.json()["run"]["forked_from_step_id"].as_str(),
            Some("work")
        );
    }

    #[tokio::test]
    async fn workflow_routes_scope_runs_and_lists_by_workspace_query() {
        let harness = Harness::with(HarnessOptions {
            workflows: Some(workflow_test_controller("/a")),
            workflow_workspaces: HashMap::from([(
                "/other".to_string(),
                workflow_test_controller("/other"),
            )]),
            workspaces: FakeWorkspaces {
                admitted: vec!["/other".to_string()],
                ..FakeWorkspaces::default()
            },
            ..HarnessOptions::default()
        });

        let start = harness
            .send_with(
                "POST",
                "/v1/workflows?workspace=/other",
                Some(r#"{"name":"review","input":"request"}"#),
                &[("content-type", b"application/json")],
            )
            .await;
        assert_eq!(start.status, StatusCode::CREATED, "{}", start.body);
        let id = start.json()["run"]["id"]
            .as_str()
            .expect("run id")
            .to_string();

        let list_startup = harness.send("GET", "/v1/workflows", None).await;
        assert_eq!(list_startup.status, StatusCode::OK, "{}", list_startup.body);
        assert!(
            list_startup.json()["runs"]
                .as_array()
                .expect("runs")
                .is_empty()
        );

        let list_other = harness
            .send("GET", "/v1/workflows?workspace=/other", None)
            .await;
        assert_eq!(list_other.status, StatusCode::OK, "{}", list_other.body);
        let runs = list_other.json()["runs"].as_array().expect("runs").clone();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0]["id"].as_str(), Some(id.as_str()));

        // Run-scoped routes find the run without a `?workspace=` hint.
        let mut status = String::new();
        for _ in 0..20 {
            let reply = harness
                .send("GET", &format!("/v1/workflows/{id}"), None)
                .await;
            assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
            status = reply.json()["run"]["status"]
                .as_str()
                .unwrap_or_default()
                .to_string();
            if status == "succeeded" {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert_eq!(status, "succeeded");

        let cancel = harness
            .send("POST", &format!("/v1/workflows/{id}/cancel"), None)
            .await;
        assert_eq!(cancel.status, StatusCode::CONFLICT, "{}", cancel.body);
        assert_eq!(
            cancel.json()["error"]["message"],
            "workflow run already finished"
        );
    }

    #[tokio::test]
    async fn workflow_routes_report_the_disabled_error_for_a_workspace_with_no_controller() {
        let harness = Harness::with(HarnessOptions {
            workflows: Some(workflow_test_controller("/a")),
            workspaces: FakeWorkspaces {
                admitted: vec!["/other".to_string()],
                ..FakeWorkspaces::default()
            },
            ..HarnessOptions::default()
        });

        let list = harness
            .send("GET", "/v1/workflows?workspace=/other", None)
            .await;
        assert_eq!(list.status, StatusCode::NOT_IMPLEMENTED, "{}", list.body);
        assert_eq!(list.json()["error"]["code"], "workflow_unavailable");

        let start = harness
            .send_with(
                "POST",
                "/v1/workflows?workspace=/other",
                Some(r#"{"name":"review","input":"request"}"#),
                &[("content-type", b"application/json")],
            )
            .await;
        assert_eq!(start.status, StatusCode::NOT_IMPLEMENTED, "{}", start.body);

        // The startup workspace, with its own controller, is unaffected.
        let default_list = harness.send("GET", "/v1/workflows", None).await;
        assert_eq!(default_list.status, StatusCode::OK, "{}", default_list.body);
    }

    /// Every API path the router serves.
    ///
    /// ponytail: axum exposes no route table, so this list is written out once
    /// and checked against both the router and `openapi.yaml`.
    const ROUTES: &[&str] = &[
        "/v1/sessions",
        "/v1/sessions/{id}",
        "/v1/sessions/{id}/history",
        "/v1/sessions/{id}/context",
        "/v1/sessions/{id}/approvals/{approval_id}",
        "/v1/sessions/{id}/turns",
        "/v1/sessions/{id}/turns/{turn_id}",
        "/v1/sessions/{id}/turns/{turn_id}/events",
        "/v1/sessions/{id}/turns/{turn_id}/cancel",
        "/v1/sessions/{id}/compact",
        "/v1/sessions/{id}/tasks",
        "/v1/sessions/{id}/tasks/{task_id}",
        "/v1/sessions/{id}/tasks/{task_id}/cancel",
        "/v1/sessions/{id}/timers",
        "/v1/sessions/{id}/timers/{timer_id}/cancel",
        "/v1/sessions/{id}/mcp",
        "/v1/sandbox/reload",
        "/v1/tasks",
        "/v1/tasks/{parent_session}/{task_id}",
        "/v1/workflows",
        "/v1/workflows/{id}",
        "/v1/workflows/{id}/events",
        "/v1/workflows/{id}/resume",
        "/v1/workflows/{id}/fork",
        "/v1/workflows/{id}/cancel",
        "/v1/workflows/requests/{id}/approve",
        "/v1/workflows/requests/{id}/reject",
        "/v1/workspaces",
        "/v1/workspaces/diff",
        "/v1/info",
        "/v1/status",
        "/v1/usage",
        "/v1/usage/daily",
        "/v1/openapi.yaml",
        "/healthz",
        "/metrics",
    ];

    #[test]
    fn openapi_documents_every_route() {
        let document = String::from_utf8_lossy(OPENAPI_YAML);
        for path in ROUTES {
            let want = format!("\n  {path}:");
            assert!(document.contains(&want), "openapi.yaml is missing {path}");
        }
    }

    #[tokio::test]
    async fn every_documented_route_is_reachable_on_the_router() {
        let harness = Harness::new();
        let id = harness.create().await;
        for path in ROUTES {
            let probe = path
                .replace("{id}", &id)
                .replace("{turn_id}", "nope")
                .replace("{task_id}", "nope")
                .replace("{timer_id}", "nope")
                .replace("{approval_id}", "nope");
            // An unmatched path logs the route label "unmatched"; a matched
            // one logs its own pattern. That is the reachability check.
            let before = harness.logged().len();
            // `raw`, not `send`: `/v1/status`'s body is an infinite SSE
            // stream, and `send` awaits the full body.
            harness.raw("GET", &probe, None, &[]).await;
            assert!(
                !harness.logged()[before..].contains("route=unmatched"),
                "GET {probe} matched no route"
            );
        }
    }

    #[tokio::test]
    async fn a_token_gates_the_v1_routes_but_not_healthz_or_the_ui() {
        let harness = Harness::with(HarnessOptions {
            token: "secret".to_string(),
            ..HarnessOptions::default()
        });
        let reply = harness.send("GET", "/v1/info", None).await;
        assert_eq!(reply.status, StatusCode::UNAUTHORIZED);
        assert_eq!(reply.json()["error"]["code"], "unauthorized");

        for path in ["/healthz", "/metrics", "/"] {
            assert_eq!(
                harness.send("GET", path, None).await.status,
                StatusCode::OK,
                "{path} must not need a token"
            );
        }

        let reply = harness
            .send_with(
                "GET",
                "/v1/info",
                None,
                &[("authorization", b"Bearer secret")],
            )
            .await;
        assert_eq!(reply.status, StatusCode::OK);
    }

    // ---- compaction, sandbox reload, tasks ----

    #[tokio::test]
    async fn compacting_an_unknown_session_is_404() {
        let harness = Harness::new();
        let reply = harness
            .send("POST", "/v1/sessions/missing/compact", None)
            .await;
        assert_eq!(reply.status, StatusCode::NOT_FOUND);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn compacting_while_a_turn_runs_is_409_turn_active() {
        let (gate, options) = gated();
        let harness = Harness::with(options);
        let id = harness.create().await;
        let (_stream, _turn_id) = start_stream(&harness, &id).await;
        harness.provider.wait_started(1).await;

        let reply = harness
            .send("POST", &format!("/v1/sessions/{id}/compact"), None)
            .await;
        assert_eq!(reply.status, StatusCode::CONFLICT);
        assert_eq!(reply.json()["error"]["code"], "turn_active");
        gate.cancel();
    }

    #[tokio::test]
    async fn compacting_an_empty_session_is_a_noop() {
        let harness = Harness::new();
        let id = harness.create().await;
        let reply = harness
            .send("POST", &format!("/v1/sessions/{id}/compact"), None)
            .await;
        assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
        assert_eq!(reply.json()["noop"], true);
    }

    #[tokio::test]
    async fn sandbox_reload_is_501_when_the_factory_wires_none() {
        let harness = Harness::new();
        let reply = harness.send("POST", "/v1/sandbox/reload", None).await;
        assert_eq!(reply.status, StatusCode::NOT_IMPLEMENTED);
        assert_eq!(reply.json()["error"]["code"], "not_implemented");
    }

    #[tokio::test]
    async fn sandbox_reload_response_is_unchanged_for_a_single_workspace() {
        let harness = Harness::with(HarnessOptions {
            sandbox_reload: Some(Ok(SandboxInfo {
                mode: SandboxMode::Seatbelt,
                network: SandboxNetwork::Allowed,
                bash_available: true,
                reason: SandboxReason::None,
            })),
            ..Default::default()
        });
        let reply = harness.send("POST", "/v1/sandbox/reload", None).await;
        assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
        let body = reply.json();
        assert_eq!(body["mode"], "seatbelt");
        assert_eq!(body["network"], "allowed");
        assert_eq!(body["bash_available"], true);
        assert!(
            body.get("workspaces").is_none(),
            "single-workspace reload gained a workspaces field: {body}"
        );
    }

    #[tokio::test]
    async fn sandbox_reload_failure_is_unchanged_for_a_single_workspace() {
        let harness = Harness::with(HarnessOptions {
            sandbox_reload: Some(Err("boom".to_string())),
            ..Default::default()
        });
        let reply = harness.send("POST", "/v1/sandbox/reload", None).await;
        assert_eq!(reply.status, StatusCode::CONFLICT, "{}", reply.body);
        assert_eq!(reply.json()["error"]["code"], "sandbox_reload_failed");
        assert_eq!(reply.json()["error"]["message"], "boom");
    }

    #[tokio::test]
    async fn sandbox_reload_reloads_every_loaded_workspace() {
        let startup = "/startup".to_string();
        let harness = Harness::with(HarnessOptions {
            info: Info {
                workspace: startup.clone(),
                ..Default::default()
            },
            workspaces: FakeWorkspaces {
                admitted: vec!["/other".to_string()],
                ..Default::default()
            },
            sandbox_reload: Some(Ok(SandboxInfo {
                mode: SandboxMode::Seatbelt,
                network: SandboxNetwork::Allowed,
                bash_available: true,
                reason: SandboxReason::None,
            })),
            sandbox_reload_others: vec![(
                "/other".to_string(),
                Ok(SandboxInfo {
                    mode: SandboxMode::Off,
                    network: SandboxNetwork::Unconfined,
                    bash_available: true,
                    reason: SandboxReason::None,
                }),
            )],
            ..Default::default()
        });
        let reply = harness.send("POST", "/v1/sandbox/reload", None).await;
        assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
        let body = reply.json();
        assert_eq!(body["mode"], "seatbelt", "{body}");
        let workspaces = body["workspaces"].as_array().expect("workspaces array");
        assert_eq!(workspaces.len(), 2, "{body}");
        assert_eq!(workspaces[0]["workspace"], startup);
        assert_eq!(workspaces[0]["sandbox"]["mode"], "seatbelt");
        assert_eq!(workspaces[1]["workspace"], "/other");
        assert_eq!(workspaces[1]["sandbox"]["mode"], "off");
    }

    #[tokio::test]
    async fn sandbox_reload_names_the_workspace_that_failed() {
        let harness = Harness::with(HarnessOptions {
            workspaces: FakeWorkspaces {
                admitted: vec!["/other".to_string()],
                ..Default::default()
            },
            sandbox_reload: Some(Ok(SandboxInfo::default())),
            sandbox_reload_others: vec![("/other".to_string(), Err("boom".to_string()))],
            ..Default::default()
        });
        let reply = harness.send("POST", "/v1/sandbox/reload", None).await;
        assert_eq!(reply.status, StatusCode::CONFLICT, "{}", reply.body);
        assert_eq!(reply.json()["error"]["code"], "sandbox_reload_failed");
        let message = reply.json()["error"]["message"]
            .as_str()
            .expect("message")
            .to_string();
        assert!(message.contains("/other"), "{message}");
        assert!(message.contains("boom"), "{message}");
    }

    #[tokio::test]
    async fn approval_route_is_session_scoped_and_fails_closed() {
        let harness = Harness::new();
        let id = harness.create().await;
        let reply = harness
            .send(
                "POST",
                &format!("/v1/sessions/{id}/approvals/approval-1"),
                None,
            )
            .await;
        assert_eq!(reply.status, StatusCode::CONFLICT);
        assert_eq!(reply.json()["error"]["code"], "approval_failed");
        assert_eq!(
            harness
                .send("POST", "/v1/sessions/missing/approvals/approval-1", None,)
                .await
                .status,
            StatusCode::NOT_FOUND
        );
    }

    #[tokio::test]
    async fn the_task_routes_answer_empty_for_a_session_with_no_tasks() {
        let harness = Harness::new();
        let id = harness.create().await;
        let reply = harness
            .send("GET", &format!("/v1/sessions/{id}/tasks"), None)
            .await;
        assert_eq!(reply.status, StatusCode::OK);
        assert_eq!(reply.json()["tasks"].as_array().expect("tasks").len(), 0);

        for (method, path) in [
            ("GET", format!("/v1/sessions/{id}/tasks/t1")),
            ("POST", format!("/v1/sessions/{id}/tasks/t1/cancel")),
        ] {
            assert_eq!(
                harness.send(method, &path, None).await.status,
                StatusCode::NOT_FOUND,
                "{method} {path}"
            );
        }
    }

    #[tokio::test]
    async fn the_mcp_route_answers_empty_for_a_session_with_no_servers_configured() {
        let harness = Harness::new();
        let id = harness.create().await;
        let reply = harness
            .send("GET", &format!("/v1/sessions/{id}/mcp"), None)
            .await;
        assert_eq!(reply.status, StatusCode::OK);
        assert_eq!(
            reply.json()["servers"].as_array().expect("servers").len(),
            0
        );
    }

    #[tokio::test]
    async fn the_mcp_route_answers_404_for_an_unknown_session() {
        let harness = Harness::new();
        let reply = harness.send("GET", "/v1/sessions/missing/mcp", None).await;
        assert_eq!(reply.status, StatusCode::NOT_FOUND);
        assert_eq!(reply.json()["error"]["code"], "not_found");
    }

    /// The routes read the runner's real registry. The list and detail task
    /// routes, driven through the registry rather than through a scripted
    /// `agent` tool call.
    #[tokio::test]
    async fn the_task_routes_list_and_detail_a_real_task() {
        let harness = Harness::new();
        let id = harness.create().await;
        let registry = harness
            .server
            .lookup(&id)
            .expect("session")
            .ctrl
            .subagent_tasks()
            .expect("registry");
        let added = registry
            .add(
                crate::subagent::tasks::Task {
                    name: "lint".to_string(),
                    agent: "reviewer".to_string(),
                    description: "check the diff".to_string(),
                    model: "gpt-5".to_string(),
                    created_at: Some(chrono::Utc::now()),
                    ..crate::subagent::tasks::Task::default()
                },
                None,
                None,
            )
            .expect("add");

        let reply = harness
            .send("GET", &format!("/v1/sessions/{id}/tasks"), None)
            .await;
        assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
        let listed = reply.json();
        let listed = listed["tasks"].as_array().expect("tasks");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0]["id"], added.id);
        assert_eq!(listed[0]["name"], "lint");
        assert_eq!(listed[0]["agent"], "reviewer");
        assert_eq!(listed[0]["description"], "check the diff");
        assert_eq!(listed[0]["model"], "gpt-5");
        assert_eq!(listed[0]["status"], "queued");
        // The wire record never carries the prompt.
        assert!(listed[0].get("prompt").is_none());

        let detail = harness
            .send(
                "GET",
                &format!("/v1/sessions/{id}/tasks/{}", added.id),
                None,
            )
            .await;
        assert_eq!(detail.status, StatusCode::OK, "{}", detail.body);
        assert_eq!(detail.json()["id"], added.id);
        assert_eq!(
            detail.json()["history"].as_array().expect("history").len(),
            0
        );

        let canceled = harness
            .send(
                "POST",
                &format!("/v1/sessions/{id}/tasks/lint/cancel"),
                None,
            )
            .await;
        assert_eq!(canceled.status, StatusCode::OK, "{}", canceled.body);
        // Cancelling does not itself finish the task; once it is final the
        // route answers 409 `task_done`.
        assert_eq!(canceled.json()["status"], "queued");
        registry.finish(
            &added.id,
            crate::subagent::tasks::TaskStatus::Canceled,
            chrono::Utc::now(),
            "",
            "",
        );
        let repeat = harness
            .send(
                "POST",
                &format!("/v1/sessions/{id}/tasks/lint/cancel"),
                None,
            )
            .await;
        assert_eq!(repeat.status, StatusCode::CONFLICT);
        assert_eq!(repeat.json()["error"]["code"], "task_done");
    }

    // ---- workspaces ----

    #[tokio::test]
    async fn listing_workspaces_answers_the_startup_workspace_alone() {
        let harness = Harness::new();
        let reply = harness.send("GET", "/v1/workspaces", None).await;
        assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);

        let body = reply.json();
        let startup = harness.factory.workspaces.startup.clone();
        assert_eq!(body["startup"], startup);
        assert_eq!(body["roots"].as_array().expect("roots").len(), 0);
        let workspaces = body["workspaces"].as_array().expect("workspaces");
        assert_eq!(workspaces.len(), 1);
        assert_eq!(workspaces[0]["path"], startup);
        assert_eq!(workspaces[0]["open_sessions"], 0);
        assert_eq!(workspaces[0]["workflows"], false);
    }

    #[tokio::test]
    async fn registering_a_relative_path_answers_400() {
        let harness = Harness::new();
        let reply = harness
            .send_with(
                "POST",
                "/v1/workspaces",
                Some(r#"{"path":"relative"}"#),
                &[("content-type", b"application/json")],
            )
            .await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{}", reply.body);
        assert_eq!(reply.json()["error"]["code"], "INVALID_WORKSPACE");
    }

    #[tokio::test]
    async fn registering_an_unadmitted_path_answers_403() {
        let harness = Harness::new();
        let reply = harness
            .send_with(
                "POST",
                "/v1/workspaces",
                Some(r#"{"path":"/elsewhere"}"#),
                &[("content-type", b"application/json")],
            )
            .await;
        assert_eq!(reply.status, StatusCode::FORBIDDEN, "{}", reply.body);
        assert_eq!(reply.json()["error"]["code"], "WORKSPACE_NOT_ADMITTED");
    }

    #[tokio::test]
    async fn registering_an_admitted_path_answers_201_then_200_on_repeat() {
        let harness = Harness::with(HarnessOptions {
            workspaces: FakeWorkspaces {
                admitted: vec!["/other".to_string()],
                ..FakeWorkspaces::default()
            },
            ..HarnessOptions::default()
        });

        let first = harness
            .send_with(
                "POST",
                "/v1/workspaces",
                Some(r#"{"path":"/other"}"#),
                &[("content-type", b"application/json")],
            )
            .await;
        assert_eq!(first.status, StatusCode::CREATED, "{}", first.body);
        assert_eq!(first.json()["path"], "/other");
        assert_eq!(first.json()["open_sessions"], 0);
        assert_eq!(first.json()["workflows"], true);

        let repeat = harness
            .send_with(
                "POST",
                "/v1/workspaces",
                Some(r#"{"path":"/other"}"#),
                &[("content-type", b"application/json")],
            )
            .await;
        assert_eq!(repeat.status, StatusCode::OK, "{}", repeat.body);
        assert_eq!(repeat.json()["path"], "/other");

        let list = harness.send("GET", "/v1/workspaces", None).await.json();
        let workspaces = list["workspaces"].as_array().expect("workspaces");
        assert_eq!(workspaces.len(), 2);
        assert_eq!(workspaces[0]["path"], harness.factory.workspaces.startup);
        assert_eq!(workspaces[1]["path"], "/other");
    }

    #[tokio::test]
    async fn a_failed_load_answers_500_with_the_real_message() {
        let harness = Harness::with(HarnessOptions {
            workspaces: FakeWorkspaces {
                admitted: vec!["/broken".to_string()],
                error: Some(("/broken".to_string(), "sandbox open failed".to_string())),
                ..FakeWorkspaces::default()
            },
            ..HarnessOptions::default()
        });

        let reply = harness
            .send_with(
                "POST",
                "/v1/workspaces",
                Some(r#"{"path":"/broken"}"#),
                &[("content-type", b"application/json")],
            )
            .await;
        assert_eq!(
            reply.status,
            StatusCode::INTERNAL_SERVER_ERROR,
            "{}",
            reply.body
        );
        assert_eq!(reply.json()["error"]["code"], "internal");
        assert_eq!(reply.json()["error"]["message"], "sandbox open failed");
    }

    #[tokio::test]
    async fn deleting_a_loaded_workspace_unloads_it() {
        let harness = Harness::with(HarnessOptions {
            workspaces: FakeWorkspaces {
                admitted: vec!["/other".to_string()],
                loaded: tokio::sync::Mutex::new(BTreeMap::from([("/other".to_string(), true)])),
                ..FakeWorkspaces::default()
            },
            ..HarnessOptions::default()
        });

        let reply = harness
            .send("DELETE", "/v1/workspaces?path=/other", None)
            .await;
        assert_eq!(reply.status, StatusCode::NO_CONTENT, "{}", reply.body);

        let list = harness.send("GET", "/v1/workspaces", None).await.json();
        let workspaces = list["workspaces"].as_array().expect("workspaces");
        assert_eq!(workspaces.len(), 1);
        assert_eq!(workspaces[0]["path"], harness.factory.workspaces.startup);
    }

    #[tokio::test]
    async fn deleting_an_unknown_path_answers_404() {
        let harness = Harness::new();
        let reply = harness
            .send("DELETE", "/v1/workspaces?path=/nope", None)
            .await;
        assert_eq!(reply.status, StatusCode::NOT_FOUND, "{}", reply.body);
        assert_eq!(reply.json()["error"]["code"], "WORKSPACE_NOT_FOUND");
    }

    #[tokio::test]
    async fn deleting_the_startup_workspace_answers_409() {
        let harness = Harness::new();
        let startup = harness.factory.workspaces.startup.clone();
        let reply = harness
            .send("DELETE", &format!("/v1/workspaces?path={startup}"), None)
            .await;
        assert_eq!(reply.status, StatusCode::CONFLICT, "{}", reply.body);
        assert_eq!(reply.json()["error"]["code"], "WORKSPACE_IS_STARTUP");
    }

    #[tokio::test]
    async fn deleting_a_workspace_with_an_open_session_answers_409() {
        // A session registered directly against a second workspace's own
        // builder, bypassing `TestFactory` (which always builds against the
        // harness's one builder), matching
        // `notify_open_sessions_skips_sessions_outside_the_startup_workspace`.
        let other_workspace = tempfile::tempdir().expect("other workspace");
        let other_sessions = tempfile::tempdir().expect("other sessions");
        // Canonical, as a real session's workspace is (`/var` → `/private/var`).
        let other_root = std::fs::canonicalize(other_workspace.path()).expect("canonical");
        let other_builder = Arc::new(testutil::builder(&other_root, other_sessions.path()));
        let other_path = other_builder.workspace_path.clone();

        let harness = Harness::with(HarnessOptions {
            workspaces: FakeWorkspaces {
                admitted: vec![other_path.clone()],
                loaded: tokio::sync::Mutex::new(BTreeMap::from([(other_path.clone(), true)])),
                ..FakeWorkspaces::default()
            },
            ..HarnessOptions::default()
        });

        let session = SharedSession::memory(Header {
            version: CURRENT_VERSION,
            id: new_id().expect("id"),
            workspace: other_path.clone(),
            provider: "openai-compatible".to_string(),
            profile: "alpha".to_string(),
            model: "test-model".to_string(),
            created_at: chrono::Utc::now(),
        });
        let runner = Runner::scripted(
            session.clone(),
            Arc::clone(&harness.provider) as Arc<dyn Provider + Send + Sync>,
            Arc::new(crate::subagent::tasks::Tasks::new()),
        );
        let other_ctrl = Controller::with_builder(
            other_builder,
            true,
            session,
            runner,
            RuntimeInfo {
                provider: "openai-compatible".to_string(),
                profile: "alpha".to_string(),
                model: "test-model".to_string(),
                thinking: "high".to_string(),
                context_window: 128_000,
                sandbox: SandboxInfo::default(),
            },
        );
        harness.server.register(other_ctrl);

        let reply = harness
            .send("DELETE", &format!("/v1/workspaces?path={other_path}"), None)
            .await;
        assert_eq!(reply.status, StatusCode::CONFLICT, "{}", reply.body);
        assert_eq!(reply.json()["error"]["code"], "WORKSPACE_IN_USE");

        // A non-canonical spelling of the same directory is checked too.
        let reply = harness
            .send(
                "DELETE",
                &format!("/v1/workspaces?path={other_path}/."),
                None,
            )
            .await;
        assert_eq!(reply.status, StatusCode::CONFLICT, "{}", reply.body);

        let list = harness.send("GET", "/v1/workspaces", None).await.json();
        assert_eq!(list["workspaces"].as_array().expect("workspaces").len(), 2);
    }

    #[tokio::test]
    async fn deleting_a_workspace_with_an_active_run_answers_409() {
        let harness = Harness::with(HarnessOptions {
            workspaces: FakeWorkspaces {
                admitted: vec!["/other".to_string()],
                loaded: tokio::sync::Mutex::new(BTreeMap::from([("/other".to_string(), true)])),
                in_use: Some("/other".to_string()),
                ..FakeWorkspaces::default()
            },
            ..HarnessOptions::default()
        });

        let reply = harness
            .send("DELETE", "/v1/workspaces?path=/other", None)
            .await;
        assert_eq!(reply.status, StatusCode::CONFLICT, "{}", reply.body);
        assert_eq!(reply.json()["error"]["code"], "WORKSPACE_IN_USE");

        let list = harness.send("GET", "/v1/workspaces", None).await.json();
        assert_eq!(list["workspaces"].as_array().expect("workspaces").len(), 2);
    }

    #[tokio::test]
    async fn deleting_a_workspace_without_a_token_answers_401() {
        let harness = Harness::with(HarnessOptions {
            token: "secret".to_string(),
            ..HarnessOptions::default()
        });
        let reply = harness
            .send("DELETE", "/v1/workspaces?path=/other", None)
            .await;
        assert_eq!(reply.status, StatusCode::UNAUTHORIZED, "{}", reply.body);
    }

    // ---- workspace diff ----

    /// A `CommandExecutor` double that records every request and answers
    /// each call with the next canned `(exit code, stdout)` pair, or
    /// `(0, "")` once the list runs out.
    #[derive(Default)]
    struct RecordingExecutor {
        requests: Mutex<Vec<crate::sandbox::Request>>,
        outputs: Mutex<Vec<(i32, String)>>,
    }

    impl RecordingExecutor {
        fn with(outputs: Vec<(i32, String)>) -> Arc<Self> {
            Arc::new(Self {
                requests: Mutex::new(Vec::new()),
                outputs: Mutex::new(outputs),
            })
        }
    }

    #[async_trait::async_trait]
    impl crate::sandbox::CommandExecutor for RecordingExecutor {
        async fn execute(
            &self,
            request: crate::sandbox::Request,
            streams: crate::sandbox::Streams<'_>,
            _cancel: &CancellationToken,
        ) -> (
            crate::sandbox::ExitStatus,
            Result<(), crate::sandbox::Error>,
        ) {
            self.requests
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .push(request);
            let (code, text) = {
                let mut outputs = self
                    .outputs
                    .lock()
                    .unwrap_or_else(|poison| poison.into_inner());
                if outputs.is_empty() {
                    (0, String::new())
                } else {
                    outputs.remove(0)
                }
            };
            let _ = streams.stdout.write_all(text.as_bytes());
            (
                crate::sandbox::ExitStatus {
                    code,
                    ..crate::sandbox::ExitStatus::default()
                },
                Ok(()),
            )
        }
    }

    fn diff_options(executor: &Arc<RecordingExecutor>, environment: Vec<String>) -> HarnessOptions {
        HarnessOptions {
            diff_runner: Some((
                Arc::clone(executor) as Arc<dyn crate::sandbox::CommandExecutor>,
                environment,
            )),
            ..HarnessOptions::default()
        }
    }

    #[tokio::test]
    async fn the_diff_route_runs_git_through_the_workspace_executor() {
        let environment = vec!["PATH=/usr/bin".to_string()];
        let executor = RecordingExecutor::with(vec![
            (0, "true\n".to_string()),
            (0, "main\n".to_string()),
            (0, String::new()),
            (0, String::new()),
        ]);
        let harness = Harness::with(diff_options(&executor, environment.clone()));
        let reply = harness.send("GET", "/v1/workspaces/diff", None).await;
        assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
        assert_eq!(reply.json()["repository"], true);
        assert_eq!(reply.json()["branch"], "main");
        assert_eq!(reply.json()["files"], serde_json::json!([]));
        assert_eq!(reply.json()["truncated"], false);

        let requests = executor.requests.lock().unwrap();
        assert_eq!(requests.len(), 4);
        assert_eq!(
            requests[0].argv,
            vec![
                "git",
                "-c",
                "core.fsmonitor=false",
                "-c",
                "core.quotepath=off",
                "rev-parse",
                "--is-inside-work-tree",
            ]
        );
        assert_eq!(
            requests[2].argv,
            vec![
                "git",
                "-c",
                "core.fsmonitor=false",
                "-c",
                "core.quotepath=off",
                "diff",
                "HEAD",
                "--no-color",
                "--no-ext-diff",
                "--no-textconv",
                "--relative",
                "-M",
                "--",
                ".",
            ]
        );
        assert_eq!(
            requests[3].argv,
            vec![
                "git",
                "-c",
                "core.fsmonitor=false",
                "-c",
                "core.quotepath=off",
                "ls-files",
                "--others",
                "--exclude-standard",
                "-z",
                "--",
                ".",
            ]
        );
        assert_eq!(
            requests[0].dir,
            PathBuf::from(&harness.factory.workspaces.startup)
        );
        let mut expected_env = environment.clone();
        expected_env.push("GIT_OPTIONAL_LOCKS=0".to_string());
        assert_eq!(requests[0].env, expected_env);
    }

    #[tokio::test]
    async fn a_directory_outside_any_work_tree_answers_repository_false() {
        let executor = RecordingExecutor::with(vec![(1, String::new())]);
        let harness = Harness::with(diff_options(&executor, Vec::new()));
        let reply = harness.send("GET", "/v1/workspaces/diff", None).await;
        assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
        assert_eq!(reply.json()["repository"], false);
        assert_eq!(reply.json()["branch"], Value::Null);
        assert_eq!(reply.json()["files"], serde_json::json!([]));
        assert_eq!(executor.requests.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn before_the_first_commit_diffs_against_the_empty_tree() {
        let executor = RecordingExecutor::with(vec![
            (0, "true\n".to_string()),
            (1, String::new()),
            (0, String::new()),
            (0, String::new()),
        ]);
        let harness = Harness::with(diff_options(&executor, Vec::new()));
        let reply = harness.send("GET", "/v1/workspaces/diff", None).await;
        assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
        assert_eq!(reply.json()["repository"], true);
        assert_eq!(reply.json()["branch"], Value::Null);

        let requests = executor.requests.lock().unwrap();
        assert_eq!(
            requests[2].argv[5..7],
            [
                "diff".to_string(),
                "4b825dc642cb6eb9a060e54bf8d69288fbee4904".to_string()
            ]
        );
    }

    #[tokio::test]
    async fn an_untracked_file_is_reported_and_its_diff_no_index_exit_1_is_accepted() {
        let patch = "diff --git a/dev/null b/new.txt\nnew file mode 100644\nindex 0000000..1111111\n--- /dev/null\n+++ b/new.txt\n@@ -0,0 +1 @@\n+hello\n";
        let executor = RecordingExecutor::with(vec![
            (0, "true\n".to_string()),
            (0, "main\n".to_string()),
            (0, String::new()),
            (0, "new.txt\0".to_string()),
            (1, patch.to_string()),
        ]);
        let harness = Harness::with(diff_options(&executor, Vec::new()));
        let reply = harness.send("GET", "/v1/workspaces/diff", None).await;
        assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
        let files = reply.json()["files"].as_array().expect("files").clone();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0]["path"], "new.txt");
        assert_eq!(files[0]["status"], "untracked");
        assert!(
            files[0]["patch"]
                .as_str()
                .expect("patch")
                .contains("+hello")
        );

        let requests = executor.requests.lock().unwrap();
        assert_eq!(requests.len(), 5);
        assert_eq!(
            requests[4].argv,
            vec![
                "git",
                "-c",
                "core.fsmonitor=false",
                "-c",
                "core.quotepath=off",
                "diff",
                "--no-index",
                "--no-color",
                "--no-ext-diff",
                "--no-textconv",
                "--",
                "/dev/null",
                "new.txt",
            ]
        );
    }

    #[tokio::test]
    async fn no_executor_answers_501_diff_unavailable() {
        let harness = Harness::new();
        let reply = harness.send("GET", "/v1/workspaces/diff", None).await;
        assert_eq!(reply.status, StatusCode::NOT_IMPLEMENTED, "{}", reply.body);
        assert_eq!(reply.json()["error"]["code"], "diff_unavailable");
    }

    #[tokio::test]
    async fn the_diff_route_requires_a_token_like_every_v1_route() {
        let executor = RecordingExecutor::with(vec![(1, String::new())]);
        let mut options = diff_options(&executor, Vec::new());
        options.token = "secret".to_string();
        let harness = Harness::with(options);
        let reply = harness.send("GET", "/v1/workspaces/diff", None).await;
        assert_eq!(reply.status, StatusCode::UNAUTHORIZED, "{}", reply.body);
    }

    #[tokio::test]
    async fn an_unadmitted_workspace_query_answers_403() {
        let executor = RecordingExecutor::with(vec![(1, String::new())]);
        let mut options = diff_options(&executor, Vec::new());
        options.workspaces = FakeWorkspaces::default();
        let harness = Harness::with(options);
        let reply = harness
            .send("GET", "/v1/workspaces/diff?workspace=/elsewhere", None)
            .await;
        assert_eq!(reply.status, StatusCode::FORBIDDEN, "{}", reply.body);
        assert_eq!(reply.json()["error"]["code"], "WORKSPACE_NOT_ADMITTED");
    }

    #[tokio::test]
    async fn a_failing_git_diff_answers_500_git_failed() {
        let executor = RecordingExecutor::with(vec![
            (0, "true\n".to_string()),
            (0, "main\n".to_string()),
            (128, String::new()),
        ]);
        let harness = Harness::with(diff_options(&executor, Vec::new()));
        let reply = harness.send("GET", "/v1/workspaces/diff", None).await;
        assert_eq!(
            reply.status,
            StatusCode::INTERNAL_SERVER_ERROR,
            "{}",
            reply.body
        );
        assert_eq!(reply.json()["error"]["code"], "git_failed");
        assert!(
            reply.json()["error"]["message"]
                .as_str()
                .expect("message")
                .contains("128")
        );
    }

    #[tokio::test]
    async fn a_real_git_repository_reports_modified_added_and_untracked_files() {
        if std::process::Command::new("git")
            .arg("--version")
            .output()
            .is_err()
        {
            eprintln!("skipping: git not on PATH");
            return;
        }
        // The repository lives in the harness's own startup workspace, so
        // the route's default (no `?workspace=`) resolves straight to it.
        let repo = tempfile::tempdir().expect("temp dir");
        // Canonicalized to match what `Harness::with` derives as the
        // workspace path (see its comment on `canonical_workspace`).
        let root = std::fs::canonicalize(repo.path()).expect("canonicalize");
        let run = |args: &[&str]| {
            let status = std::process::Command::new("git")
                .args(args)
                .current_dir(&root)
                .status()
                .expect("git runs");
            assert!(status.success(), "git {args:?}");
        };
        std::fs::write(root.join("tracked.txt"), "one\n").expect("write");
        run(&["init", "-q"]);
        run(&["config", "user.email", "test@example.com"]);
        run(&["config", "user.name", "test"]);
        run(&["add", "tracked.txt"]);
        run(&["commit", "-q", "-m", "initial"]);
        std::fs::write(root.join("tracked.txt"), "one\ntwo\n").expect("write");
        std::fs::write(root.join("added.txt"), "added\n").expect("write");
        run(&["add", "added.txt"]);
        std::fs::write(root.join("untracked.txt"), "untracked\n").expect("write");

        let executor: Arc<dyn crate::sandbox::CommandExecutor> = Arc::new(
            crate::sandbox::Executor::new(
                Arc::new(crate::sandbox::direct::DirectDriver::new()),
                crate::sandbox::Policy {
                    filesystem: crate::sandbox::FilesystemMode::Unconfined,
                    network: crate::sandbox::NetworkMode::Allow,
                },
                &root,
            )
            .expect("the executor opens"),
        );
        // The direct driver runs with only the given environment (no
        // inherited `PATH`), so `git` must be resolvable from it.
        let path = std::env::var("PATH").unwrap_or_default();
        let harness = Harness::with(HarnessOptions {
            workspace: Some(repo),
            diff_runner: Some((executor, vec![format!("PATH={path}")])),
            ..HarnessOptions::default()
        });
        let reply = harness.send("GET", "/v1/workspaces/diff", None).await;
        assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
        assert_eq!(reply.json()["repository"], true);
        let files = reply.json()["files"].as_array().expect("files").clone();
        let by_path = |path: &str| files.iter().find(|file| file["path"] == path);
        assert_eq!(
            by_path("tracked.txt").expect("tracked.txt")["status"],
            "modified"
        );
        assert_eq!(by_path("added.txt").expect("added.txt")["status"], "added");
        assert_eq!(
            by_path("untracked.txt").expect("untracked.txt")["status"],
            "untracked"
        );
    }

    // ---- cross-process task routes (tasks.db) ----

    /// A `TaskContext` for `parent_session`, workspace `/work`.
    fn agents_context(parent_session: &str) -> crate::subagent::record::TaskContext {
        crate::subagent::record::TaskContext {
            parent_session: parent_session.to_string(),
            parent_session_path: format!("/home/me/.otto/sessions/{parent_session}.jsonl"),
            workspace: "/work".to_string(),
            pid: 4_294_967_294, // a pid that cannot exist, so queued/running rows read as interrupted
            process_started_at: "2026-09-25T10:00:00Z".to_string(),
        }
    }

    /// A stored task row: `task_id`, `status`, and `created_at` (RFC 3339,
    /// determines list order and the `before` cursor) vary per call.
    fn agents_task(
        task_id: &str,
        status: crate::subagent::tasks::TaskStatus,
        created_at: &str,
    ) -> crate::subagent::tasks::Task {
        crate::subagent::tasks::Task {
            id: task_id.to_string(),
            name: "reviewer".to_string(),
            agent: "code-reviewer".to_string(),
            description: "review the diff".to_string(),
            model: "gpt-5.1".to_string(),
            status,
            created_at: chrono::DateTime::parse_from_rfc3339(created_at)
                .ok()
                .map(|dt| dt.with_timezone(&chrono::Utc)),
            session_path: format!("/home/me/.otto/sessions/parent/{task_id}-child.jsonl"),
            ..crate::subagent::tasks::Task::default()
        }
    }

    #[tokio::test]
    async fn the_v1_tasks_route_lists_newest_first_and_filters_by_status_and_workspace() {
        use crate::subagent::record::{Recorder, Store, TaskContext};
        use crate::subagent::tasks::TaskStatus;

        let store = Arc::new(Store::open_in_memory().expect("store"));
        store.upsert(
            &agents_context("s1"),
            &agents_task("t1", TaskStatus::Running, "2026-09-25T10:00:00Z"),
        );
        store.upsert(
            &agents_context("s1"),
            &agents_task("t2", TaskStatus::Succeeded, "2026-09-25T10:01:00Z"),
        );
        store.upsert(
            &TaskContext {
                workspace: "/other".to_string(),
                ..agents_context("s2")
            },
            &agents_task("t3", TaskStatus::Running, "2026-09-25T10:02:00Z"),
        );

        let harness = Harness::with(HarnessOptions {
            task_recorder: Some(Arc::clone(&store)),
            ..HarnessOptions::default()
        });

        let reply = harness.send("GET", "/v1/tasks", None).await;
        assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
        let body = reply.json();
        let tasks = body["tasks"].as_array().expect("tasks");
        // Newest first.
        assert_eq!(
            tasks
                .iter()
                .map(|t| t["task_id"].clone())
                .collect::<Vec<_>>(),
            vec!["t3", "t2", "t1"]
        );

        let by_status = harness
            .send("GET", "/v1/tasks?status=succeeded", None)
            .await
            .json();
        let by_status = by_status["tasks"].as_array().expect("tasks");
        assert_eq!(by_status.len(), 1);
        assert_eq!(by_status[0]["task_id"], "t2");

        let by_workspace = harness
            .send("GET", "/v1/tasks?workspace=/other", None)
            .await
            .json();
        let by_workspace = by_workspace["tasks"].as_array().expect("tasks");
        assert_eq!(by_workspace.len(), 1);
        assert_eq!(by_workspace[0]["task_id"], "t3");
    }

    #[tokio::test]
    async fn the_v1_tasks_route_bounds_limit_and_pages_with_before() {
        use crate::subagent::record::{Recorder, Store};
        use crate::subagent::tasks::TaskStatus;

        let store = Arc::new(Store::open_in_memory().expect("store"));
        for n in 0..3 {
            store.upsert(
                &agents_context("s1"),
                &agents_task(
                    &format!("t{n}"),
                    TaskStatus::Succeeded,
                    &format!("2026-09-25T10:0{n}:00Z"),
                ),
            );
        }
        let harness = Harness::with(HarnessOptions {
            task_recorder: Some(Arc::clone(&store)),
            ..HarnessOptions::default()
        });

        // limit=1 returns only the newest row; next_before is an exclusive
        // cursor on its own created_at, so paging by it skips past it.
        let first_page = harness.send("GET", "/v1/tasks?limit=1", None).await.json();
        let tasks = first_page["tasks"].as_array().expect("tasks");
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0]["task_id"], "t2");
        let next_before = first_page["next_before"].as_str().expect("next_before");
        assert_eq!(next_before, "2026-09-25T10:02:00.000000000Z");

        let second_page = harness
            .send(
                "GET",
                &format!("/v1/tasks?limit=1&before={next_before}"),
                None,
            )
            .await
            .json();
        let tasks = second_page["tasks"].as_array().expect("tasks");
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0]["task_id"], "t1");

        // limit clamps to 500 rather than erroring on an out-of-range value.
        let clamped = harness
            .send("GET", "/v1/tasks?limit=100000", None)
            .await
            .json();
        assert_eq!(clamped["tasks"].as_array().expect("tasks").len(), 3);
    }

    #[tokio::test]
    async fn the_v1_tasks_route_marks_cancelable_only_for_an_owned_non_final_task() {
        use crate::subagent::record::{Recorder, Store, TaskContext};
        use crate::subagent::tasks::TaskStatus;

        let store = Arc::new(Store::open_in_memory().expect("store"));
        let harness = Harness::with(HarnessOptions {
            task_recorder: Some(Arc::clone(&store)),
            ..HarnessOptions::default()
        });
        let owned_session = harness.create().await;
        // The current process's own pid and start time, so a `queued`/
        // `running` row reads back as verifiably alive instead of
        // `interrupted` (see `record::interrupted`).
        let (pid, process_started_at) = crate::subagent::record::current_process();
        let live_context = |parent_session: &str| TaskContext {
            parent_session: parent_session.to_string(),
            parent_session_path: format!("/home/me/.otto/sessions/{parent_session}.jsonl"),
            workspace: "/work".to_string(),
            pid,
            process_started_at: process_started_at.clone(),
        };
        store.upsert(
            &live_context(&owned_session),
            &agents_task("running-owned", TaskStatus::Running, "2026-09-25T10:00:00Z"),
        );
        store.upsert(
            &live_context(&owned_session),
            &agents_task(
                "finished-owned",
                TaskStatus::Succeeded,
                "2026-09-25T10:01:00Z",
            ),
        );
        store.upsert(
            &live_context("not-open-here"),
            &agents_task(
                "running-elsewhere",
                TaskStatus::Running,
                "2026-09-25T10:02:00Z",
            ),
        );

        let body = harness.send("GET", "/v1/tasks", None).await.json();
        let tasks = body["tasks"].as_array().expect("tasks");
        let cancelable = |task_id: &str| {
            tasks.iter().find(|t| t["task_id"] == task_id).expect("row")["cancelable"]
                .as_bool()
                .expect("cancelable")
        };
        assert!(cancelable("running-owned"));
        assert!(!cancelable("finished-owned"));
        assert!(!cancelable("running-elsewhere"));
    }

    #[tokio::test]
    async fn the_v1_tasks_detail_route_reads_the_child_transcript_and_reports_a_missing_one() {
        use crate::subagent::record::{Recorder, Store};
        use crate::subagent::tasks::TaskStatus;

        let sessions = tempfile::tempdir().expect("sessions");
        let store = Arc::new(Store::open_in_memory().expect("store"));
        let harness = Harness::with(HarnessOptions {
            task_recorder: Some(Arc::clone(&store)),
            ..HarnessOptions::default()
        });

        // A task whose child transcript file exists.
        let child_store = crate::session::Store::create(
            sessions.path(),
            Header {
                version: CURRENT_VERSION,
                id: "t1-child".to_string(),
                workspace: "/work".to_string(),
                provider: "openai-compatible".to_string(),
                profile: "alpha".to_string(),
                model: "test-model".to_string(),
                created_at: chrono::Utc::now(),
            },
        )
        .expect("create child session");
        child_store
            .append_message(&testutil::user("hello from the child"))
            .expect("append");
        let child_path = child_store.path();
        let mut context = agents_context("s1");
        context.parent_session_path = "/home/me/.otto/sessions/s1.jsonl".to_string();
        let mut task = agents_task(
            "with-transcript",
            TaskStatus::Succeeded,
            "2026-09-25T10:00:00Z",
        );
        task.session_path = child_path;
        store.upsert(&context, &task);

        // A task whose recorded session_path does not exist on disk.
        store.upsert(
            &agents_context("s1"),
            &agents_task(
                "no-transcript",
                TaskStatus::Succeeded,
                "2026-09-25T10:01:00Z",
            ),
        );

        let found = harness
            .send("GET", "/v1/tasks/s1/with-transcript", None)
            .await;
        assert_eq!(found.status, StatusCode::OK, "{}", found.body);
        let found = found.json();
        assert_eq!(found["task"]["task_id"], "with-transcript");
        assert_eq!(found["transcript_missing"], false);
        assert_eq!(
            found["history"].as_array().expect("history").len(),
            1,
            "{found:?}"
        );

        let missing = harness
            .send("GET", "/v1/tasks/s1/no-transcript", None)
            .await
            .json();
        assert_eq!(missing["transcript_missing"], true);
        assert_eq!(missing["history"].as_array().expect("history").len(), 0);

        let not_found = harness.send("GET", "/v1/tasks/s1/nope", None).await;
        assert_eq!(not_found.status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn the_timer_routes_list_and_cancel_a_real_timer() {
        let harness = Harness::new();
        let id = harness.create().await;
        let reminders = harness
            .server
            .lookup(&id)
            .expect("session")
            .ctrl
            .reminders()
            .expect("registry");
        let scheduled = reminders
            .schedule(Duration::from_secs(600), "check the build".to_string())
            .expect("schedule");

        let reply = harness
            .send("GET", &format!("/v1/sessions/{id}/timers"), None)
            .await;
        assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
        let listed = reply.json();
        let listed = listed["timers"].as_array().expect("timers");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0]["id"], scheduled.id);
        assert_eq!(listed[0]["message"], "check the build");
        assert!(listed[0]["fire_at"].is_string(), "{listed:?}");

        let path = format!("/v1/sessions/{id}/timers/{}/cancel", scheduled.id);
        let canceled = harness.send("POST", &path, None).await;
        assert_eq!(canceled.status, StatusCode::OK, "{}", canceled.body);
        assert_eq!(canceled.json()["id"], scheduled.id);
        assert!(reminders.list().is_empty());

        // A second cancel finds nothing left to cancel.
        let repeat = harness.send("POST", &path, None).await;
        assert_eq!(repeat.status, StatusCode::NOT_FOUND);
        assert_eq!(repeat.json()["error"]["code"], "not_found");
    }

    // ---- wake turns ----

    /// A harness whose every controller shares `tasks`, plus the registry.
    fn wake_harness(script: Script) -> (Harness, Arc<crate::subagent::tasks::Tasks>) {
        let tasks = Arc::new(crate::subagent::tasks::Tasks::new());
        let harness = Harness::with(HarnessOptions {
            script,
            tasks: Some(Arc::clone(&tasks)),
            ..HarnessOptions::default()
        });
        (harness, tasks)
    }

    fn notification() -> otto_core::agent::inbox::Notification {
        otto_core::agent::inbox::Notification {
            task_id: "t1".to_string(),
            kind: Some(otto_core::agent::inbox::NotificationKind::TaskFinished),
            text: "done".to_string(),
            usage: None,
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_pending_notification_starts_a_wake_turn_while_idle() {
        let (harness, tasks) = wake_harness(Script {
            deltas: vec!["ok".to_string()],
            ..Script::default()
        });
        let id = harness.create().await;

        tasks.notifications().push(notification());

        tokio::time::timeout(Duration::from_secs(2), harness.provider.wait_started(1))
            .await
            .expect("the pending notification did not start a wake turn");
        assert_eq!(harness.provider.roles(), vec![Role::Context]);

        let session = harness
            .send("GET", &format!("/v1/sessions/{id}"), None)
            .await;
        let turn = session.json()["turn"].clone();
        assert_eq!(turn["trigger"], turn::TRIGGER_TASK, "{}", session.body);
        let turn_id = turn["id"].as_str().expect("turn id").to_string();

        let done = harness.wait_turn_done(&id, &turn_id).await;
        assert_eq!(done["trigger"], turn::TRIGGER_TASK);
    }

    #[tokio::test]
    async fn notify_open_sessions_fans_out_to_every_open_controller() {
        let harness = Harness::new();
        let first = harness.create().await;
        let second = harness.create().await;
        let notification = otto_core::agent::inbox::Notification {
            kind: Some(otto_core::agent::inbox::NotificationKind::Message),
            text: "[feishu] hello".to_string(),
            ..otto_core::agent::inbox::Notification::default()
        };
        assert_eq!(harness.server.notify_open_sessions(notification), 2);
        for id in [first, second] {
            let pending = harness
                .server
                .lookup(&id)
                .expect("session")
                .ctrl
                .subagent_tasks()
                .expect("registry")
                .notifications()
                .snapshot();
            assert_eq!(pending.len(), 1, "{id}");
            assert_eq!(pending[0].text, "[feishu] hello");
        }
    }

    #[tokio::test]
    async fn notify_open_sessions_skips_sessions_outside_the_startup_workspace() {
        let harness = Harness::new();
        let startup_id = harness.create().await;

        // A session opened directly against a second workspace, bypassing
        // `TestFactory` (which always builds against the harness's one
        // builder): `Server::register` takes a `Controller` regardless of
        // which workspace built it.
        let other_workspace = tempfile::tempdir().expect("other workspace");
        let other_sessions = tempfile::tempdir().expect("other sessions");
        let other_builder = Arc::new(testutil::builder(
            other_workspace.path(),
            other_sessions.path(),
        ));
        let session = SharedSession::memory(Header {
            version: CURRENT_VERSION,
            id: new_id().expect("id"),
            workspace: other_builder.workspace_path.clone(),
            provider: "openai-compatible".to_string(),
            profile: "alpha".to_string(),
            model: "test-model".to_string(),
            created_at: chrono::Utc::now(),
        });
        let runner = Runner::scripted(
            session.clone(),
            Arc::clone(&harness.provider) as Arc<dyn Provider + Send + Sync>,
            Arc::new(crate::subagent::tasks::Tasks::new()),
        );
        let other_ctrl = Controller::with_builder(
            other_builder,
            true,
            session,
            runner,
            RuntimeInfo {
                provider: "openai-compatible".to_string(),
                profile: "alpha".to_string(),
                model: "test-model".to_string(),
                thinking: "high".to_string(),
                context_window: 128_000,
                sandbox: SandboxInfo::default(),
            },
        );
        let other_id = other_ctrl.info().session_id.clone();
        harness.server.register(other_ctrl);

        let notification = otto_core::agent::inbox::Notification {
            kind: Some(otto_core::agent::inbox::NotificationKind::Message),
            text: "[feishu] hello".to_string(),
            ..otto_core::agent::inbox::Notification::default()
        };
        assert_eq!(harness.server.notify_open_sessions(notification), 1);

        let startup_pending = harness
            .server
            .lookup(&startup_id)
            .expect("startup session")
            .ctrl
            .subagent_tasks()
            .expect("registry")
            .notifications()
            .snapshot();
        assert_eq!(startup_pending.len(), 1);
        assert_eq!(startup_pending[0].text, "[feishu] hello");

        let other_pending = harness
            .server
            .lookup(&other_id)
            .expect("other session")
            .ctrl
            .subagent_tasks()
            .expect("registry")
            .notifications()
            .snapshot();
        assert!(other_pending.is_empty(), "{other_pending:?}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_message_wake_logs_inbox_kind() {
        let (harness, tasks) = wake_harness(Script {
            deltas: vec!["ok".to_string()],
            ..Script::default()
        });
        harness.create().await;
        tasks
            .notifications()
            .push(otto_core::agent::inbox::Notification {
                kind: Some(otto_core::agent::inbox::NotificationKind::Message),
                text: "[feishu] hi".to_string(),
                ..otto_core::agent::inbox::Notification::default()
            });
        tokio::time::timeout(Duration::from_secs(2), harness.provider.wait_started(1))
            .await
            .expect("the message did not start a wake turn");
        let log = harness.logged();
        assert!(log.contains("msg=turn_started"), "{log}");
        assert!(log.contains("inbox_kind=message"), "{log}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_notification_during_a_user_turn_waits_for_it_to_finish() {
        let gate = CancellationToken::new();
        let (harness, tasks) = wake_harness(Script {
            deltas: vec!["ok".to_string()],
            gate: Some(gate.clone()),
            ..Script::default()
        });
        let id = harness.create().await;
        let (_stream, _turn_id) = start_stream(&harness, &id).await;
        harness.provider.wait_started(1).await;

        // A notification while the user turn is active must not start a
        // second turn: the active turn's own inbox drain handles it.
        tasks.notifications().push(notification());
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(harness.provider.roles(), vec![Role::User]);

        gate.cancel();

        // The user turn's end-of-turn check finds the notification still
        // pending and starts exactly one wake turn.
        tokio::time::timeout(Duration::from_secs(2), harness.provider.wait_started(2))
            .await
            .expect("the finished user turn did not start a wake turn");
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(harness.provider.roles(), vec![Role::User, Role::Context]);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_notification_landing_as_a_user_turn_ends_starts_one_wake_turn() {
        let tasks = Arc::new(crate::subagent::tasks::Tasks::new());
        let pusher = Arc::clone(&tasks);
        let harness = Harness::with(HarnessOptions {
            script: Script {
                deltas: vec!["ok".to_string()],
                // The notification lands during the parent's last provider
                // call, so the turn's own drain never sees it.
                on_call: Some(Box::new(move |call| {
                    if call == 1 {
                        pusher
                            .notifications()
                            .push(otto_core::agent::inbox::Notification {
                                task_id: "t1".to_string(),
                                kind: Some(otto_core::agent::inbox::NotificationKind::TaskFinished),
                                text: "done".to_string(),
                                usage: None,
                            });
                    }
                })),
                ..Script::default()
            },
            tasks: Some(Arc::clone(&tasks)),
            ..HarnessOptions::default()
        });
        let id = harness.create().await;

        let reply = harness
            .send(
                "POST",
                &format!("/v1/sessions/{id}/turns"),
                Some(r#"{"text":"hi","stream":false}"#),
            )
            .await;
        assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);

        tokio::time::timeout(Duration::from_secs(2), harness.provider.wait_started(2))
            .await
            .expect("the late notification did not start a wake turn");
        // Give an incorrect extra wake turn a chance to start.
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(harness.provider.roles(), vec![Role::User, Role::Context]);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn closing_the_server_ends_the_wake_loop() {
        let (harness, _tasks) = wake_harness(Script::default());
        harness.create().await;

        tokio::time::timeout(Duration::from_secs(2), harness.server.close())
            .await
            .expect("close did not return; the wake loop leaked")
            .expect("close");
    }

    // ---- status ----

    /// Reads the next frame off a `/v1/status` reader and decodes its data.
    async fn status_snapshot(reader: &mut SseReader) -> Value {
        let frame = reader.next().await.expect("status frame");
        assert_eq!(frame.event, "status");
        assert_eq!(frame.id, None, "the status stream sends no id");
        serde_json::from_str(&frame.data).expect("status json")
    }

    #[tokio::test]
    async fn status_stream_without_a_token_is_401() {
        let harness = Harness::with(HarnessOptions {
            token: "secret".to_string(),
            ..HarnessOptions::default()
        });
        assert_eq!(
            harness.send("GET", "/v1/status", None).await.status,
            StatusCode::UNAUTHORIZED
        );
    }

    #[tokio::test]
    async fn status_stream_with_no_open_sessions_is_an_empty_snapshot() {
        let harness = Harness::new();
        let response = harness.raw("GET", "/v1/status", None, &[]).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get(header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok()),
            Some("text/event-stream")
        );
        let mut reader = SseReader::new(response);
        let snapshot = status_snapshot(&mut reader).await;
        assert_eq!(snapshot, serde_json::json!({"sessions": []}));
    }

    #[tokio::test]
    async fn status_stream_ends_when_the_server_is_cancelled() {
        let harness = Harness::new();
        let response = harness.raw("GET", "/v1/status", None, &[]).await;
        let mut reader = SseReader::new(response);
        status_snapshot(&mut reader).await;

        harness.server.cancel_token().cancel();

        let frame = tokio::time::timeout(Duration::from_secs(2), reader.next())
            .await
            .expect("the status stream should end once the server is cancelled");
        assert!(frame.is_none(), "expected end of stream, got {frame:?}");
    }

    #[tokio::test]
    async fn creating_a_session_appears_in_the_status_stream() {
        let harness = Harness::new();
        let response = harness.raw("GET", "/v1/status", None, &[]).await;
        let mut reader = SseReader::new(response);
        let empty = status_snapshot(&mut reader).await;
        assert_eq!(empty["sessions"].as_array().expect("rows").len(), 0);

        let id = harness.create().await;
        let snapshot = status_snapshot(&mut reader).await;
        let rows = snapshot["sessions"].as_array().expect("rows");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["id"], id.as_str());
        assert!(rows[0]["turn"].is_null());
        assert_eq!(rows[0]["approvals"], 0);
        assert_eq!(rows[0]["tasks"], 0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn starting_a_turn_shows_running_then_finishing_shows_ok() {
        let (gate, options) = gated();
        let harness = Harness::with(options);
        let id = harness.create().await;

        let response = harness.raw("GET", "/v1/status", None, &[]).await;
        let mut reader = SseReader::new(response);
        let before = status_snapshot(&mut reader).await;
        assert!(before["sessions"][0]["turn"].is_null());

        let (_stream, turn_id) = start_stream(&harness, &id).await;
        harness.provider.wait_started(1).await;

        let running = status_snapshot(&mut reader).await;
        assert_eq!(running["sessions"][0]["turn"], turn::TURN_RUNNING);

        gate.cancel();
        harness.wait_turn_done(&id, &turn_id).await;

        let done = status_snapshot(&mut reader).await;
        assert_eq!(done["sessions"][0]["turn"], turn::TURN_OK);
    }

    #[tokio::test]
    async fn a_failing_turn_shows_error_in_the_status_stream() {
        let harness = Harness::with(HarnessOptions {
            script: Script {
                error: Some("boom".to_string()),
                ..Script::default()
            },
            ..HarnessOptions::default()
        });
        let id = harness.create().await;
        let response = harness.raw("GET", "/v1/status", None, &[]).await;
        let mut reader = SseReader::new(response);
        status_snapshot(&mut reader).await; // the session with no turn yet

        harness
            .send(
                "POST",
                &format!("/v1/sessions/{id}/turns"),
                Some(r#"{"text":"hi","stream":false}"#),
            )
            .await;

        let after = status_snapshot(&mut reader).await;
        assert_eq!(after["sessions"][0]["turn"], turn::TURN_ERROR);
    }

    #[tokio::test]
    async fn a_session_in_a_second_workspace_appears_with_its_workspace() {
        let harness = Harness::with(HarnessOptions {
            workspaces: FakeWorkspaces {
                admitted: vec!["/other".to_string()],
                ..FakeWorkspaces::default()
            },
            ..HarnessOptions::default()
        });
        let response = harness.raw("GET", "/v1/status", None, &[]).await;
        let mut reader = SseReader::new(response);
        status_snapshot(&mut reader).await;

        let reply = harness
            .send("POST", "/v1/sessions", Some(r#"{"workspace":"/other"}"#))
            .await;
        assert_eq!(reply.status, StatusCode::CREATED, "{}", reply.body);

        let snapshot = status_snapshot(&mut reader).await;
        let rows = snapshot["sessions"].as_array().expect("rows");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["workspace"], "/other");
    }

    #[tokio::test]
    async fn closing_a_session_removes_it_from_the_status_stream() {
        let harness = Harness::new();
        harness.create().await;
        let response = harness.raw("GET", "/v1/status", None, &[]).await;
        let mut reader = SseReader::new(response);
        let first = status_snapshot(&mut reader).await;
        assert_eq!(first["sessions"].as_array().expect("rows").len(), 1);

        let id = first["sessions"][0]["id"].as_str().expect("id").to_string();
        harness
            .send("DELETE", &format!("/v1/sessions/{id}"), None)
            .await;

        let after = status_snapshot(&mut reader).await;
        assert_eq!(after["sessions"].as_array().expect("rows").len(), 0);
    }

    #[tokio::test]
    async fn identical_snapshots_in_a_row_are_sent_once() {
        let harness = Harness::new();
        let response = harness.raw("GET", "/v1/status", None, &[]).await;
        let mut reader = SseReader::new(response);
        status_snapshot(&mut reader).await;

        // Two bumps that leave the snapshot unchanged must coalesce into no
        // further frame; the reader only sees a change.
        harness
            .server
            .status_changed
            .send_modify(|version| *version += 1);
        harness
            .server
            .status_changed
            .send_modify(|version| *version += 1);

        assert!(
            tokio::time::timeout(Duration::from_millis(100), reader.next())
                .await
                .is_err(),
            "an unchanged snapshot must not be resent"
        );
    }

    // ---- the web UI ----

    #[tokio::test]
    async fn the_root_page_is_served_without_a_token_and_never_cached() {
        let harness = Harness::with(HarnessOptions {
            token: "secret".to_string(),
            ..HarnessOptions::default()
        });
        let reply = harness.send("GET", "/", None).await;
        assert_eq!(reply.status, StatusCode::OK);
        assert_eq!(reply.header("cache-control"), "no-cache");
        // An unbuilt checkout serves the placeholder; a built one the bundle.
        assert!(
            reply.body == ui::PLACEHOLDER || reply.body.contains("<html"),
            "unexpected root body: {:?}",
            &reply.body[..reply.body.len().min(120)]
        );

        assert_eq!(
            harness.send("GET", "/assets/missing.js", None).await.status,
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            harness.send("GET", "/no-such-path", None).await.status,
            StatusCode::NOT_FOUND,
            "an unknown path must not fall through to index.html"
        );
    }
}
