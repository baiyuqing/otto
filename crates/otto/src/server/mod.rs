//! `otto serve`: the HTTP+JSON+SSE frontend.
//!
//! One [`app::Controller`] per session, a turn event buffer decoupled from the
//! agent's synchronous emit callback ([`turn::Turn`]), Prometheus metrics
//! ([`crate::metrics::Metrics`]), and structured request logging. See
//! `docs/specs/2026-09-03-agent-server-design.md`.
//!
//! Concurrency: `sessions` is a plain `Mutex<HashMap>` held only for map
//! operations. Each [`OpenSession`] has its own `Mutex` for the turn,
//! compaction, and reflection slots.

pub mod agents;
pub mod approvals;
pub mod auth;
pub mod compact;
pub mod config;
pub mod delivery;
pub mod diff;
pub mod fs;
pub mod listen;
pub mod mcp;
pub mod reflection;
pub mod sandbox;
pub mod schedule;
pub mod skills;
pub mod task_control;
pub mod tasks;
pub mod timers;
pub mod turn;
pub mod workflows;
pub mod workspaces;

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{DefaultBodyLimit, MatchedPath, Path, Query, State};
use axum::http::{HeaderMap, Method, StatusCode, header};
use axum::middleware::Next;
use axum::response::Response;
use axum::routing::{delete, get, post};
use otto_core::agent::Event;
use otto_core::model::{Block, MAX_IMAGE_BYTES, Message, Usage};
use otto_core::session::ListResult;
use otto_core::wire::sse::format_frame;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use crate::app::task_control::DUPLICATE_INPUT;
use crate::app::{self, ApprovalDecision, ApprovalRequest, Channel, Controller, Step, Stop};
pub use crate::log::Logger;
pub(crate) use crate::log::request_id;
use crate::metrics::{Metrics, SessionContext};
use crate::runtime::info::SandboxInfo;
#[cfg(test)]
use crate::runtime::info::{SandboxMode, SandboxNetwork, SandboxReason};
use otto_core::wire::events::WireEvent;
use turn::{TRIGGER_RECOVERY, TRIGGER_TASK, TRIGGER_USER, Turn};

/// A [`Factory::open`] that answers with exactly this text produces 404
/// `not_found` instead of 500.
pub const SESSION_NOT_FOUND: &str = "session not found";
const IMAGE_TURN_BODY_MAX_BYTES: usize = MAX_IMAGE_BYTES * 4 / 3 + 4096;
const REMEMBERED_REFLECTION_CANCELS: usize = 16;

/// An open session with nothing running is closed after this much idle time,
/// unless `[server].session_idle_timeout` says otherwise; a server that stays
/// up for weeks then keeps only the sessions in play. `[server]` also caps how
/// many stay open at once. See [`Server::sweep_idle_sessions`].
const SESSION_SWEEP_INTERVAL: Duration = Duration::from_secs(60);
/// How many evicted session ids a later request still re-opens. A client that
/// touches an evicted session finds it open again; ids evicted before the last
/// [`REMEMBERED_EVICTIONS`] are treated as never opened, which is what a
/// request for a closed session already answers.
const REMEMBERED_EVICTIONS: usize = 1024;

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
    fn approval_service(&self) -> Option<Arc<crate::approval::Service>> {
        None
    }
    /// A brand new session. `workspace` is the canonical path of an already
    /// admitted-and-loaded workspace; `None` means the startup workspace.
    async fn create(&self, workspace: Option<&str>) -> Result<Controller, String>;
    /// Create a session with a durable command receipt before acknowledging it.
    async fn create_with_receipt(
        &self,
        _workspace: Option<&str>,
        _id: String,
        _request_id: &str,
    ) -> Result<Controller, String> {
        Err("idempotent session creation is unavailable".into())
    }
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
    /// A delivery status scoped to its source session. The status is a narrow
    /// read-only projection and deliberately does not reuse ordinary tasks.
    fn delivery_status(
        &self,
        _source_session: &str,
        _approval_id: &str,
    ) -> Result<Option<delivery::Status>, String> {
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
    /// Records `path` as trusted (the same config write as `otto trust`), so
    /// a following [`Factory::load_workspace`] admits it. Only `POST
    /// /v1/workspaces` with `trust: true` calls this, after a load answered
    /// [`WorkspaceLoadError::NotAdmitted`]. The default refuses, for a
    /// `Factory` with no config file to write.
    async fn trust_workspace(&self, path: &str) -> Result<(), WorkspaceLoadError> {
        Err(WorkspaceLoadError::Failed(format!(
            "{path}: trusting a directory is not available"
        )))
    }
    /// The canonical directories `GET /v1/fs/dirs` may list, with their
    /// descendants: the user's home directory, then the configured workspace
    /// roots. The first entry is where browsing starts. Empty (the default)
    /// makes the route answer 404.
    fn browse_roots(&self) -> Vec<std::path::PathBuf> {
        Vec::new()
    }
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
#[derive(Debug)]
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
    /// The resolved Otto configuration file; config-management routes are the
    /// only server routes that read or write it.
    pub config_path: std::path::PathBuf,
    /// When non-empty, required as `Authorization: Bearer <token>` on every
    /// `/v1/` route. Empty means no check, which is only safe behind a Unix
    /// socket with private file modes.
    pub token: String,
    pub logger: Option<Arc<Logger>>,
    pub workflows: Option<Arc<crate::workflow::Controller>>,
    /// The scheduled-delivery store shared by every workspace; `None` when
    /// `~/.otto/schedule.db` could not be opened.
    pub schedule: Option<Arc<dyn crate::schedule::ScheduleStore>>,
    /// `[server].session_idle_timeout`: how long a session with nothing running
    /// stays open. `None` never closes one for being idle.
    pub session_idle_timeout: Option<Duration>,
    /// `[server].max_open_sessions`: how many sessions stay open at once.
    /// `None` keeps every open session, whatever the count.
    pub max_open_sessions: Option<usize>,
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
    /// Milliseconds after [`activity_now`]'s origin of the last route lookup,
    /// turn start and turn end. [`Server::sweep_idle_sessions`] reads it.
    last_active: AtomicU64,
    /// How many live frontend connections hold this session open. A pinned
    /// session is never evicted; see [`Server::pin_session`].
    pins: AtomicUsize,
}

/// Milliseconds since this process's first session activity stamp. Monotonic
/// like [`Instant`], and cheap enough to read and write per request.
fn activity_now() -> u64 {
    static ORIGIN: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    ORIGIN.get_or_init(Instant::now).elapsed().as_millis() as u64
}

/// A session queues at most this many turns behind the running one.
const MAX_QUEUED_TURNS: usize = 16;
/// A waiting approval with no decision after this long is decided `timeout`.
const APPROVAL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(600);
/// How many decided approval ids a session remembers, so a late decision is
/// answered `approval_decided` rather than `approval_failed`.
const REMEMBERED_DECISIONS: usize = 32;

/// A user turn accepted with `"queue": true` behind another turn.
struct QueuedTurn {
    turn: Arc<Turn>,
    text: String,
    image: Option<Block>,
    channel: Option<Channel>,
    origin: Option<crate::schedule::Origin>,
    input_id: Option<String>,
}

/// How many recent `input_id`s each session remembers for answering a client
/// retry with the turn it already started.
const MAX_REMEMBERED_INPUTS: usize = 256;

/// The elevated command the running turn waits on. At most one per session,
/// because the turn waits.
struct WaitingApproval {
    id: String,
    shared_revision: Option<u64>,
    decide: tokio::sync::oneshot::Sender<ApprovalDecision>,
}

struct ActiveReflection {
    request_id: Option<String>,
    cancel: CancellationToken,
}

#[derive(Default)]
struct SessionState {
    /// The running turn, or the last one that finished. Never a queued turn.
    turn: Option<Arc<Turn>>,
    /// Non-`None` while `POST .../compact` runs. `start_turn` and
    /// `handle_compact` both check it under the same lock, so a turn and a
    /// compaction are never admitted together.
    compacting: Option<CancellationToken>,
    /// Non-`None` while `POST .../reflect` runs.
    reflecting: Option<ActiveReflection>,
    /// Reflection request ids cancelled before their matching reflect request
    /// registered. Bounded to the most recent 16 ids; request ids are expected
    /// to be unique per client operation.
    canceled_reflections: VecDeque<String>,
    /// Turns waiting for the running turn or compaction, oldest first. Every
    /// reader of this field holds the session lock, so the queue, `turn` and
    /// `compacting` change together.
    queue: VecDeque<QueuedTurn>,
    /// Turns restored from the session's saved turn queue on open, oldest
    /// first. They join the front of `queue` once the recovery wake turn,
    /// if any, has run (see [`Server::release_restored`]).
    restored: VecDeque<QueuedTurn>,
    waiting: Option<WaitingApproval>,
    decided: VecDeque<String>,
    /// The turns started for recent client `input_id`s, newest last.
    inputs: VecDeque<(String, Arc<Turn>)>,
}

impl SessionState {
    fn turn_running(&self) -> bool {
        self.turn.as_ref().is_some_and(|turn| !turn.is_done())
    }

    /// Whether a new turn or compaction must wait. A non-empty queue counts:
    /// the next queued turn starts when the current one ends, and nothing may
    /// start in between.
    pub(crate) fn busy(&self) -> bool {
        self.busy_for_wake() || !self.restored.is_empty()
    }

    /// [`Self::busy`] without the restored turns: they wait for the wake turn
    /// that resumes an interrupted turn, so they must not hold it back.
    fn busy_for_wake(&self) -> bool {
        self.turn_running()
            || !self.queue.is_empty()
            || self.compacting.is_some()
            || self.reflecting.is_some()
    }

    fn reflection_cancelled(&self, request_id: &str) -> bool {
        self.canceled_reflections
            .iter()
            .any(|known| known == request_id)
    }

    fn remember_reflection_cancel(&mut self, request_id: &str) {
        if self.reflection_cancelled(request_id) {
            return;
        }
        if self.canceled_reflections.len() == REMEMBERED_REFLECTION_CANCELS {
            self.canceled_reflections.pop_front();
        }
        self.canceled_reflections.push_back(request_id.to_string());
    }

    fn remember_decided(&mut self, id: &str) {
        if self.decided.iter().any(|known| known == id) {
            return;
        }
        if self.decided.len() == REMEMBERED_DECISIONS {
            self.decided.pop_front();
        }
        self.decided.push_back(id.to_string());
    }

    /// Ends every queued turn `canceled`; none of them ran. Their saved
    /// copies stay in the turn queue journal, so a later open of the
    /// session restores them; a caller that ends them for good also clears
    /// the journal.
    fn cancel_queue(&mut self) {
        for queued in self.queue.drain(..).chain(self.restored.drain(..)) {
            queued.turn.finish(Some(String::new()), true);
        }
    }
}

impl OpenSession {
    fn new(ctrl: Controller) -> Arc<Self> {
        Arc::new(Self {
            ctrl: Arc::new(ctrl),
            state: Mutex::new(SessionState::default()),
            turn_finished: tokio::sync::Notify::new(),
            closed: CancellationToken::new(),
            last_active: AtomicU64::new(activity_now()),
            pins: AtomicUsize::new(0),
        })
    }

    /// Records that the session was just used, so the sweeper leaves it alone.
    fn touch(&self) {
        self.last_active.store(activity_now(), Ordering::Relaxed);
    }

    /// How long ago [`Self::touch`] last ran.
    fn idle_millis(&self, now: u64) -> u64 {
        now.saturating_sub(self.last_active.load(Ordering::Relaxed))
    }

    fn pinned(&self) -> bool {
        self.pins.load(Ordering::SeqCst) > 0
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, SessionState> {
        self.state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    }

    fn current_turn(&self) -> Option<Arc<Turn>> {
        self.lock().turn.clone()
    }

    /// The running, last finished or queued turn named `id`.
    fn find_turn(&self, id: &str) -> Option<Arc<Turn>> {
        let state = self.lock();
        state
            .turn
            .iter()
            .chain(state.queue.iter().map(|queued| &queued.turn))
            .chain(state.restored.iter().map(|queued| &queued.turn))
            .find(|turn| turn.id == id)
            .cloned()
    }

    /// Cancels the running turn or compaction so a following close does not
    /// wait on a provider call, ends the queued turns, and stops the wake
    /// loop.
    fn cancel_work(&self) {
        self.closed.cancel();
        let mut state = self.lock();
        state.cancel_queue();
        if let Some(turn) = state.turn.as_ref() {
            turn.cancel();
        }
        if let Some(cancel) = state.compacting.as_ref() {
            cancel.cancel();
        }
        if let Some(active) = state.reflecting.as_ref() {
            active.cancel.cancel();
        }
    }
}

/// `otto serve`'s router plus the session registry and turn lifecycle behind
/// it. Construct with [`Server::new`].
pub struct Server {
    factory: Arc<dyn Factory>,
    info: Info,
    config_path: std::path::PathBuf,
    /// One-time, in-memory profile-change previews keyed by their capability
    /// token. Tokens expire in the config route before a write can occur.
    approvals: Option<Arc<crate::approval::Service>>,
    token: String,
    log: Arc<Logger>,
    metrics: Arc<Metrics>,
    sessions: Mutex<HashMap<String, Arc<OpenSession>>>,
    /// Serializes every `Factory::open`, so two concurrent resumes of one id
    /// cannot build two controllers for the same session file at once.
    ///
    /// ponytail: one gate for every id instead of a per-id gate. Resuming is an
    /// admin-rate path; make it per-id if it ever contends.
    open_gate: tokio::sync::Mutex<()>,
    /// One handle per running wake loop, awaited by [`Server::close`].
    wake_loops: Mutex<Vec<tokio::task::JoinHandle<()>>>,
    /// Ids the sweeper evicted, oldest first. A request that names one
    /// re-opens it from disk ([`Server::session_for_route`]), which is what
    /// makes eviction invisible to a client; an id the client closed itself
    /// is not here, so a close still answers 404 as it always did.
    evicted: Mutex<VecDeque<String>>,
    /// How long a session with nothing running stays open
    /// (`[server].session_idle_timeout`). `None` never closes one for being
    /// idle.
    session_idle_timeout: Option<Duration>,
    /// How many sessions stay open at once (`[server].max_open_sessions`).
    /// `None` keeps every open session, whatever the count.
    max_open_sessions: Option<usize>,
    cancel: CancellationToken,
    workflows: Option<Arc<crate::workflow::Controller>>,
    schedule: Option<Arc<dyn crate::schedule::ScheduleStore>>,
    /// Bumped whenever a `GET /v1/status` snapshot could change: a session
    /// opened or closed, a turn started or finished, a task registry update,
    /// or a granted Bash approval. See [`Server::status_snapshot`].
    status_changed: watch::Sender<u64>,
}

impl Server {
    pub fn new(options: Options) -> Arc<Self> {
        let approvals = options.factory.approval_service();
        if let Some(service) = &approvals {
            service.start();
            service
                .register_domain(
                    &options.info.workspace,
                    "config",
                    Arc::new(crate::approval::config::Adapter {
                        path: options.config_path.clone(),
                    }),
                )
                .expect("approval domain registry");
        }
        Arc::new(Self {
            factory: options.factory,
            info: options.info,
            config_path: options.config_path,
            approvals,
            token: options.token,
            log: options.logger.unwrap_or_else(|| Arc::new(Logger::stderr())),
            metrics: Arc::new(Metrics::new()),
            sessions: Mutex::new(HashMap::new()),
            open_gate: tokio::sync::Mutex::new(()),
            wake_loops: Mutex::new(Vec::new()),
            evicted: Mutex::new(VecDeque::new()),
            session_idle_timeout: options.session_idle_timeout,
            max_open_sessions: options.max_open_sessions,
            cancel: CancellationToken::new(),
            workflows: options.workflows,
            schedule: options.schedule,
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

    /// The scheduled-delivery store, for the ACP TCP listener's delivery
    /// workers.
    pub fn schedule_store(&self) -> Option<Arc<dyn crate::schedule::ScheduleStore>> {
        self.schedule.clone()
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

    /// Runs a SIGTERM migration across every open session: cancels each
    /// session's in-flight turn or compaction the same way [`Self::close`]
    /// does, then runs [`Controller::migrate`] on each. Called by
    /// `cli::serve::run` after [`Self::cancel_token`] was cancelled and
    /// before [`Self::close`]; unlike `close`, this neither removes a
    /// session from the registry nor closes a controller, so the caller's
    /// own `close` still runs the normal shutdown right after this returns.
    pub async fn migrate(&self) -> Vec<String> {
        let sessions = self.all_sessions();
        for session in &sessions {
            session.cancel_work();
        }
        let mut warnings = Vec::new();
        for session in &sessions {
            warnings.extend(session.ctrl.migrate().await);
        }
        warnings
    }

    // ---- routing ----

    /// Every route in `openapi.yaml`, plus the two token-free UI routes.
    /// The router for a Unix socket listener. Reaching the socket needs
    /// file-system permission on its path, so requests on it skip the bearer
    /// token that [`Server::router`] requires on TCP.
    pub fn socket_router(self: &Arc<Self>) -> Router {
        self.router().layer(axum::middleware::from_fn(
            |mut request: axum::extract::Request, next: Next| async move {
                request.extensions_mut().insert(SocketRequest);
                next.run(request).await
            },
        ))
    }

    pub fn router(self: &Arc<Self>) -> Router {
        let state = Arc::clone(self);
        Router::new()
            .route("/v1/approvals", get(approvals::list_shared))
            .route("/v1/approvals/changes", get(approvals::changes_shared))
            .route("/v1/approvals/events", get(approvals::events_shared))
            .route("/v1/approvals/{id}", get(approvals::get_shared))
            .route(
                "/v1/approvals/{id}/decision",
                post(approvals::decide_shared),
            )
            .route(
                "/v1/approvals/{id}/withdraw",
                post(approvals::withdraw_shared),
            )
            .route("/v1/config/profiles", get(config::profiles))
            .route("/v1/config/profiles/{name}", get(config::get_profile))
            .route("/v1/config/models", get(config::models))
            .route("/v1/config/changes", post(config::preview))
            .route(
                "/v1/config/changes/{id}",
                post(config::confirm).delete(config::cancel),
            )
            .route("/v1/sessions", post(create_session).get(list_sessions))
            .route(
                "/v1/sessions/{id}",
                get(get_session)
                    .patch(rename_session)
                    .delete(delete_session),
            )
            .route("/v1/sessions/{id}/fork", post(fork_session))
            .route("/v1/sessions/{id}/profile", post(set_session_profile))
            .route("/v1/sessions/{id}/fork-points", get(fork_points))
            .route("/v1/sessions/{id}/history", get(history))
            .route("/v1/sessions/{id}/context", get(context))
            .route("/v1/sessions/{id}/trace", get(trace))
            .route("/v1/sessions/{id}/trace/{span_id}", get(trace_span))
            .route(
                "/v1/sessions/{id}/approvals/message",
                post(approvals::message),
            )
            .route("/v1/sessions/{id}/inputs", post(task_control::admit_input))
            .route(
                "/v1/sessions/{id}/tasks/steer",
                get(task_control::steering_messages).post(task_control::steer),
            )
            .route(
                "/v1/sessions/{id}/tasks/steer/{message_id}",
                delete(task_control::withdraw_steering),
            )
            .route(
                "/v1/sessions/{id}/tasks/control",
                post(task_control::control),
            )
            .route(
                "/v1/sessions/{id}/approvals/{approval_id}",
                post(approvals::decide),
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
            .route("/v1/sessions/{id}/reflect", post(reflection::reflect))
            .route("/v1/sessions/{id}/reflect/cancel", post(reflection::cancel))
            .route("/v1/sessions/{id}/notices", get(reflection::notices))
            .route(
                "/v1/sessions/{id}/wake-replies",
                get(reflection::wake_replies),
            )
            .route(
                "/v1/sessions/{id}/reflection/runs",
                get(reflection::list_runs),
            )
            .route(
                "/v1/sessions/{id}/reflection/runs/{run}",
                get(reflection::show_run),
            )
            .route(
                "/v1/sessions/{id}/reflection/skills",
                get(reflection::list_skills),
            )
            .route(
                "/v1/sessions/{id}/reflection/skills/{name}/revert",
                post(reflection::revert_skill),
            )
            .route("/v1/sessions/{id}/tasks", get(tasks::list))
            .route(
                "/v1/sessions/{id}/deliveries/{approval_id}",
                get(delivery::get),
            )
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
            .route("/v1/sessions/{id}/skill-manager", get(skills::manager))
            .route("/v1/sessions/{id}/skill-manager/{name}", get(skills::info))
            .route("/v1/sessions/{id}/skills", get(skills::list))
            .route("/v1/sessions/{id}/skills/{name}", get(skills::get))
            .route(
                "/v1/sessions/{id}/skills/{name}/enable",
                post(skills::enable),
            )
            .route(
                "/v1/sessions/{id}/skills/{name}/disable",
                post(skills::disable),
            )
            .route("/v1/sandbox/reload", post(sandbox::reload))
            .route(
                "/v1/sessions/{id}/approval-events",
                get(approvals::session_events),
            )
            .route(
                "/v1/sessions/{id}/approval-mode",
                get(approvals::mode).put(approvals::set_mode),
            )
            .route(
                "/v1/sessions/{id}/approval-policy",
                get(approvals::policy).put(approvals::set_policy),
            )
            .route("/v1/schedule/claim", post(schedule::claim))
            .route("/v1/schedule/{id}/delivered", post(schedule::delivered))
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
            .route("/v1/fs/dirs", get(fs::dirs))
            .route("/v1/info", get(info))
            .route("/v1/status", get(status))
            .route("/v1/usage", get(usage))
            .route("/v1/usage/daily", get(daily_usage))
            .route("/v1/openapi.yaml", get(openapi))
            .route("/healthz", get(healthz))
            .route("/metrics", get(prometheus))
            .layer(DefaultBodyLimit::max(1 << 20))
            .layer(axum::middleware::from_fn_with_state(
                Arc::clone(&state),
                reopen_evicted_session,
            ))
            .layer(axum::middleware::from_fn_with_state(
                Arc::clone(&state),
                instrument,
            ))
            .with_state(state)
    }

    // ---- registry ----

    /// Shared controller for an already-open session; used by ACP memory review.
    pub(crate) fn controller(&self, id: &str) -> Option<Arc<Controller>> {
        self.lookup(id).map(|session| Arc::clone(&session.ctrl))
    }

    /// Shared controller for a route that names a session in a query or a body
    /// rather than in its path, re-opening one the sweeper evicted. `None` when
    /// no such session exists, which the caller answers 404 for.
    pub(crate) async fn controller_for_route(
        self: &Arc<Self>,
        id: &str,
    ) -> Option<Arc<Controller>> {
        self.session_for_route(id)
            .await
            .map(|session| Arc::clone(&session.ctrl))
    }

    /// The session a route names: the open one, else one this server evicted and
    /// can re-open from disk. `None` when it is neither, so a client that closed
    /// a session still gets the 404 it always did.
    ///
    /// A re-open goes through [`Self::resume_or_create`], which serializes on
    /// `open_gate`, so concurrent requests for one evicted id open it once.
    async fn session_for_route(self: &Arc<Self>, id: &str) -> Option<Arc<OpenSession>> {
        if let Some(open) = self.lookup(id) {
            return Some(open);
        }
        if !self.was_evicted(id) {
            return None;
        }
        match self.resume_or_create(id, None).await {
            Ok((session, _created)) => Some(session),
            // The id stays evicted only until the next request; this one gets
            // the same 404 a request for a closed session gets.
            Err(message) if message == SESSION_NOT_FOUND => None,
            Err(message) => {
                self.log.warn(
                    "session_reopen_failed",
                    &[("session_id", id.to_string()), ("error", message)],
                );
                None
            }
        }
    }

    fn was_evicted(&self, id: &str) -> bool {
        self.evicted
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .contains(&id.to_string())
    }

    /// Remembers that `id` was evicted, so a later request re-opens it. Drops
    /// the oldest id once [`REMEMBERED_EVICTIONS`] are remembered.
    fn remember_evicted(&self, id: &str) {
        let mut evicted = self
            .evicted
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if evicted.iter().any(|known| known == id) {
            return;
        }
        if evicted.len() == REMEMBERED_EVICTIONS {
            evicted.pop_front();
        }
        evicted.push_back(id.to_string());
    }

    /// Forgets an eviction: the session is open again.
    fn forget_evicted(&self, id: &str) {
        let mut evicted = self
            .evicted
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if let Some(position) = evicted.iter().position(|known| known == id) {
            evicted.remove(position);
        }
    }

    /// Pins `id` open for a live frontend connection, so the sweeper leaves it
    /// alone. `false` when no such session is open, which a caller that has just
    /// opened one never sees.
    pub fn pin_session(&self, id: &str) -> bool {
        let Some(session) = self.lookup(id) else {
            return false;
        };
        session.pins.fetch_add(1, Ordering::SeqCst);
        true
    }

    /// Releases one [`Self::pin_session`] hold.
    pub fn unpin_session(&self, id: &str) {
        let Some(session) = self.lookup(id) else {
            return;
        };
        let _ = session
            .pins
            .try_update(Ordering::SeqCst, Ordering::SeqCst, |count| {
                count.checked_sub(1)
            });
    }

    fn lookup(&self, id: &str) -> Option<Arc<OpenSession>> {
        let session = self
            .sessions
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .get(id)
            .cloned();
        if let Some(session) = &session {
            session.touch();
        }
        session
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
            .insert(id.clone(), Arc::clone(&session));
        self.forget_evicted(&id);
        self.metrics.sessions_open(1);
        self.status_changed.send_modify(|version| *version += 1);
        self.restore_queued_turns(&session);
        self.start_wake_loop(&session);
        session
    }

    /// Removes `id` when the registry still holds exactly `session`, so a sweep
    /// that lost a race never evicts a session that was deleted, or re-opened as
    /// a new one, while it was deciding.
    fn remove_if_same(&self, id: &str, session: &Arc<OpenSession>) -> bool {
        let mut sessions = self
            .sessions
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        match sessions.get(id) {
            Some(current) if Arc::ptr_eq(current, session) => {
                sessions.remove(id);
                self.status_changed.send_modify(|version| *version += 1);
                true
            }
            _ => false,
        }
    }

    /// Cancels everything running on `session` and closes its controller. The
    /// session's files, its saved queued turns and its eviction marker stay as
    /// they are, so a later open restores it; [`delete_session`] clears the
    /// marker and the journal itself.
    async fn close_session(&self, session: &Arc<OpenSession>) -> Result<(), String> {
        session.cancel_work();
        // Awaited before the blocking close, exactly as `Server::close` does:
        // `Controller::close` only spawns the MCP shutdown.
        session.ctrl.close_mcp().await;
        let ctrl = Arc::clone(&session.ctrl);
        match tokio::task::spawn_blocking(move || ctrl.close()).await {
            Ok(result) => result,
            Err(error) => Err(error.to_string()),
        }
    }

    /// Closes every open session that has been idle for
    /// [`Self::session_idle_timeout`] and has nothing running, then the
    /// least-recently-active idle sessions while the registry holds more than
    /// [`Self::max_open_sessions`]. Returns the ids it closed; `None` in either
    /// setting turns that rule off, and both off close nothing.
    ///
    /// A session closed here keeps its files and its saved queued turns, and a
    /// request that names it re-opens it ([`Self::session_for_route`]), so a
    /// client never notices. Busy work -- a running or queued turn, a
    /// compaction, a reflection, a waiting approval, an unfinished sub-agent
    /// task or an undelivered notification -- keeps a session whatever its age,
    /// so an idle sweeper never truncates real work. Pinned sessions are
    /// skipped: a live frontend connection holds them.
    pub(crate) async fn sweep_idle_sessions(&self) -> Vec<String> {
        let (idle_for, max_open) = (self.session_idle_timeout, self.max_open_sessions);
        if idle_for.is_none() && max_open.is_none() {
            return Vec::new();
        }
        let now = activity_now();
        // With no idle window nothing is ever aged; the cap alone decides.
        let threshold = idle_for.map_or(u64::MAX, |idle_for| idle_for.as_millis() as u64);
        // Snapshot the registry, then judge every candidate outside its lock.
        let mut candidates: Vec<(u64, String, Arc<OpenSession>)> = self
            .all_sessions()
            .into_iter()
            .filter(|session| !session.pinned())
            .map(|session| {
                let idle = session.idle_millis(now);
                (idle, session.ctrl.info().session_id, session)
            })
            .collect();
        // Most idle first, which is also the cap's eviction order.
        candidates.sort_by_key(|candidate| std::cmp::Reverse(candidate.0));

        let mut remaining = candidates.len();
        let mut victims: Vec<(u64, String, Arc<OpenSession>)> = Vec::new();
        for (idle, id, session) in candidates {
            let aged = idle >= threshold;
            let over_cap = max_open.is_some_and(|max_open| remaining > max_open);
            if !aged && !over_cap {
                break;
            }
            if !self.idle_session(&session) {
                continue;
            }
            victims.push((idle, id, session));
            remaining -= 1;
        }

        let mut evicted = Vec::with_capacity(victims.len());
        for (idle, id, session) in victims {
            // A delete, or a request that re-opened the session while this
            // sweep decided, already answered for this id.
            if !self.remove_if_same(&id, &session) {
                continue;
            }
            self.metrics.sessions_open(-1);
            self.metrics.session_evicted();
            self.remember_evicted(&id);
            let fields = [("session_id", id.clone()), ("idle_ms", idle.to_string())];
            self.log.info("session_evicted", &fields);
            if let Err(error) = self.close_session(&session).await {
                self.log.error(
                    "session_close_error",
                    &[("session_id", id.clone()), ("error", error)],
                );
            }
            evicted.push(id);
        }
        evicted
    }

    /// Whether `session` has nothing to lose: no turn, compaction, reflection,
    /// waiting approval, unfinished sub-agent task or undelivered
    /// notification.
    fn idle_session(&self, session: &OpenSession) -> bool {
        {
            let state = session.lock();
            if state.busy() || state.waiting.is_some() {
                return false;
            }
        }
        match session.ctrl.tasks() {
            Some(tasks) => {
                tasks.pending() == 0 && tasks.list().iter().all(|task| task.status.final_status())
            }
            None => true,
        }
    }

    /// Runs [`Self::sweep_idle_sessions`] every [`SESSION_SWEEP_INTERVAL`] with
    /// this server's configured idle window and cap, until `cancel` is
    /// cancelled. `otto serve` starts one per process.
    pub fn start_session_sweeper(self: &Arc<Self>, cancel: CancellationToken) {
        let server = Arc::clone(self);
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(SESSION_SWEEP_INTERVAL);
            // The first tick of a fresh interval is immediate; a server that
            // just started has nothing to sweep.
            tick.tick().await;
            loop {
                tokio::select! {
                    () = cancel.cancelled() => return,
                    _ = tick.tick() => {
                        server.sweep_idle_sessions().await;
                    }
                }
            }
        });
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
            if workspace.is_some_and(|path| existing.ctrl.info().workspace != path) {
                return Err(SESSION_NOT_FOUND.to_string());
            }
            return Ok((existing, false));
        }
        let _gate = self.open_gate.lock().await;
        // Re-check: another resume of this id may have finished while we
        // waited for the gate.
        if let Some(existing) = self.lookup(id) {
            if workspace.is_some_and(|path| existing.ctrl.info().workspace != path) {
                return Err(SESSION_NOT_FOUND.to_string());
            }
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
            auto_mode: info.approval_mode.is_automatic(),
            approval_mode: info.approval_mode.as_str(),
            approval_settings: info.approval_settings,
            turn,
            forked_from: info.forked_from,
            plan: info.plan,
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
                let (turn, turn_id, queued, approvals) = {
                    let state = session.lock();
                    let summary = state.turn.as_ref().map(|turn| turn.summary());
                    // The running turn; else the newest, which is a queued
                    // one while a compaction holds the session.
                    let turn_id = match &summary {
                        Some(summary) if summary.status == turn::TURN_RUNNING => {
                            Some(summary.id.clone())
                        }
                        _ => state
                            .queue
                            .back()
                            .map(|queued| queued.turn.id.clone())
                            .or_else(|| summary.as_ref().map(|summary| summary.id.clone())),
                    };
                    (
                        summary.map(|summary| summary.status),
                        turn_id,
                        state.queue.len(),
                        usize::from(state.waiting.is_some()),
                    )
                };
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
                let scope = session.ctrl.approval_scope();
                let counts = session
                    .ctrl
                    .approval_service()
                    .activity_snapshot(&scope.workspace, Some(&scope.session_id))
                    .map(|activity| activity.counts)
                    .unwrap_or_default();
                StatusSessionWire {
                    id: info.session_id,
                    workspace: info.workspace,
                    turn,
                    turn_id,
                    queued,
                    approvals: approvals.max(counts.human_pending),
                    human_pending: counts.human_pending,
                    reviewing: counts.reviewing,
                    application_failed: counts.application_failed,
                    tasks,
                    auto_mode: info.approval_mode.is_automatic(),
                    approval_mode: info.approval_mode.as_str(),
                    approval_settings: info.approval_settings,
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
        let Some(mailbox) = session.ctrl.notification_mailbox() else {
            return;
        };
        let mut updates = crate::runtime::workflow_notifications::updates(mailbox.as_ref());
        let tasks = session.ctrl.subagent_tasks();
        let server = Arc::clone(self);
        let session = Arc::clone(session);
        let handle = tokio::spawn(async move {
            let mut seen = std::collections::BTreeMap::new();
            let mut task_updates = tasks.as_ref().map(|tasks| tasks.updates());
            // Recheck pre-existing queued inputs on open.
            server.wake_turn(&session).await;
            loop {
                tokio::select! {
                    _ = async { match task_updates.as_mut() { Some(receiver) => { if receiver.changed().await.is_err() { std::future::pending::<()>().await; } }, None => std::future::pending::<()>().await } } => {
                        if let Some(view) = session.ctrl.tasks() { server.metrics.diff_tasks(&mut seen, &view.list()); }
                        server.status_changed.send_modify(|version| *version += 1);
                    }
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

    fn bump_status(&self) {
        self.status_changed.send_modify(|version| *version += 1);
    }

    /// Runs one task-triggered turn on `session` when a notification is pending
    /// and nothing else holds the session.
    ///
    /// The wake is prepared before the turn is published, so a no-op or busy
    /// admission cannot leave a phantom turn visible on `GET /v1/sessions`. A
    /// busy session, including one with queued turns, is left to the turn that
    /// holds it.
    async fn wake_turn(self: &Arc<Self>, session: &Arc<OpenSession>) {
        let prepared = {
            let mut state = session.lock();
            if state.busy_for_wake() {
                None
            } else {
                let trigger = if session.ctrl.resumes_interrupted_turn() {
                    TRIGGER_RECOVERY
                } else {
                    TRIGGER_TASK
                };
                match session.ctrl.prepare_wake() {
                    Ok(None) => None,
                    Ok(Some(wake)) => match new_id() {
                        Ok(id) => {
                            let turn = Arc::new(
                                Turn::new(id, trigger, self.cancel.child_token()).with_log(
                                    Arc::clone(&self.log),
                                    &session.ctrl.info().session_id,
                                    "",
                                ),
                            );
                            turn.start_running(session.ctrl.history().len());
                            state.turn = Some(Arc::clone(&turn));
                            Some(Ok((turn, wake, trigger)))
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
        let (turn, wake, trigger) = match prepared {
            // No wake turn to wait for: restored turns may run now.
            None => {
                self.release_restored(session);
                return;
            }
            Some(Err(message)) => {
                self.log.error("wake_turn_error", &[("error", message)]);
                return;
            }
            Some(Ok(prepared)) => prepared,
        };
        self.bump_status();

        self.metrics.turn_started();
        let mut fields = turn.log_ids();
        fields.push(("trigger", trigger.to_string()));
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
        self.run_turn(session, &turn, Step::Wake(wake), trigger, None, None)
            .await;
        // A wake turn that ran for longer than the idle timeout restarts the
        // clock, so a reader can still replay it from the buffer.
        session.touch();
        // Restored turns, then queued turns that arrived during the wake
        // turn, start now.
        self.release_restored(session);
        self.start_next(session);
    }

    /// Rebuilds the turns saved in the session's turn queue before Otto
    /// stopped, keeping their turn ids. They wait in `restored` for the
    /// recovery wake turn the session's pending notice starts; with nothing
    /// pending they are released at once.
    fn restore_queued_turns(self: &Arc<Self>, session: &Arc<OpenSession>) {
        let prompts = session.ctrl.restorable_prompts();
        if prompts.is_empty() {
            return;
        }
        let session_id = session.ctrl.info().session_id;
        {
            let mut state = session.lock();
            for prompt in prompts {
                let id = if prompt.turn_id.is_empty() {
                    match new_id() {
                        Ok(id) => id,
                        Err(_) => continue,
                    }
                } else {
                    prompt.turn_id.clone()
                };
                let turn = Arc::new(
                    Turn::queued(id, TRIGGER_USER, self.cancel.child_token()).with_log(
                        Arc::clone(&self.log),
                        &session_id,
                        "",
                    ),
                );
                if state.inputs.len() >= MAX_REMEMBERED_INPUTS {
                    state.inputs.pop_front();
                }
                state
                    .inputs
                    .push_back((prompt.input_id.clone(), Arc::clone(&turn)));
                state.restored.push_back(QueuedTurn {
                    turn,
                    text: prompt.text,
                    image: None,
                    channel: prompt.channel,
                    origin: prompt.origin,
                    input_id: Some(prompt.input_id),
                });
            }
        }
        self.bump_status();
        let pending_wake = session
            .ctrl
            .notification_mailbox()
            .is_some_and(|mailbox| !mailbox.is_empty());
        if !pending_wake {
            self.release_restored(session);
        }
    }

    /// Moves the restored turns to the front of the queue, queues the notice
    /// the first of them delivers, and starts it when the session is free.
    fn release_restored(self: &Arc<Self>, session: &Arc<OpenSession>) {
        let released = {
            let mut state = session.lock();
            if state.restored.is_empty() {
                return;
            }
            let restored: Vec<QueuedTurn> = state.restored.drain(..).collect();
            let prompts: Vec<app::turn_queue::QueuedPrompt> = restored
                .iter()
                .map(|queued| app::turn_queue::QueuedPrompt {
                    input_id: queued.input_id.clone().unwrap_or_default(),
                    turn_id: queued.turn.id.clone(),
                    text: queued.text.clone(),
                    channel: queued.channel,
                    origin: queued.origin.clone(),
                    image_dropped: false,
                })
                .collect();
            for queued in restored.into_iter().rev() {
                state.queue.push_front(queued);
            }
            prompts
        };
        session.ctrl.announce_restored_prompts(&released);
        self.start_next(session);
    }

    /// Runs `first` and its approval retries as `turn`, then finishes it. The
    /// error text is redacted before it reaches the turn frame, the summary
    /// or the log.
    fn run_turn<'a>(
        self: &'a Arc<Self>,
        session: &'a Arc<OpenSession>,
        turn: &'a Arc<Turn>,
        first: Step<'a>,
        trigger: &'static str,
        channel: Option<Channel>,
        origin: Option<crate::schedule::Origin>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'a>> {
        // Keep the full agent/review future out of the idle wake loop and
        // request-admission futures that only decide whether a turn can run.
        Box::pin(async move {
            let cancel = turn.cancel_token();
            let result = {
                let mut buffer = turn.emitter(&self.metrics);
                // The agent_error frame carries the same provider error that
                // turn_end carries redacted.
                let mut emit = |event: Event| match event {
                    Event::AgentError { message } => buffer(Event::AgentError {
                        message: session.ctrl.redact_error(&message),
                    }),
                    event @ Event::ApprovalChanged { .. } => {
                        buffer(event);
                        self.bump_status();
                    }
                    event => buffer(event),
                };
                let (server, open, running, token) = (&**self, &**session, &**turn, &cancel);
                session
                    .ctrl
                    .run_with_channel(first, channel, origin, &mut emit, &cancel, |request| {
                        server.wait_for_approval(open, running, token, request)
                    })
                    .await
            };
            let (error, canceled) = match result {
                Ok(Stop::EndTurn) => (None, false),
                Ok(Stop::Cancelled) => (Some(String::new()), true),
                Err(error) if error.is_cancelled() => (Some(String::new()), true),
                Err(error) => (Some(session.ctrl.redact_error(&error.to_string())), false),
            };
            turn.finish(error.clone(), canceled);
            self.bump_status();
            let summary = turn.summary();
            self.metrics.turn_finished(&summary.status, turn.elapsed());
            if let Some(message) = error.filter(|_| !canceled) {
                let mut fields = turn.log_ids();
                fields.extend([("trigger", trigger.to_string()), ("error", message)]);
                self.log.error("turn_error", &fields);
            }
            let mut fields = turn.log_ids();
            fields.extend([
                ("trigger", trigger.to_string()),
                ("status", summary.status.clone()),
                ("duration_ms", turn.elapsed().as_millis().to_string()),
            ]);
            fields.extend(turn.log_totals());
            self.log.info("turn_finished", &fields);
        })
    }

    /// Publishes the waiting approval, then waits for the decision route, the
    /// turn's cancellation, or [`APPROVAL_TIMEOUT`]. The turn stays `running`.
    /// Every outcome but cancellation is recorded as an `approval_decided`
    /// frame; a timeout is a deny.
    async fn wait_for_approval(
        &self,
        session: &OpenSession,
        turn: &Turn,
        cancel: &CancellationToken,
        request: ApprovalRequest,
    ) -> ApprovalDecision {
        loop {
            let scope = session.ctrl.approval_scope();
            let mut actionable = None;
            if let Ok(shared) = session.ctrl.approval_service().get(
                &request.approval_id,
                &scope.workspace,
                Some(&scope.session_id),
            ) {
                let shared = match session
                    .ctrl
                    .approval_service()
                    .wait_actionable(&shared.id, &shared.scope, cancel)
                    .await
                {
                    Ok(shared) => shared,
                    Err(_) => return ApprovalDecision::Deny,
                };
                if !shared.is_human_pending() {
                    return match shared.state {
                        otto_core::approval::State::Approved => ApprovalDecision::Allow,
                        _ if cancel.is_cancelled() => ApprovalDecision::Cancelled,
                        _ => ApprovalDecision::Deny,
                    };
                }
                actionable = Some(shared);
            }
            let (decide, decided) = tokio::sync::oneshot::channel();
            let mut event = WireEvent::approval_requested(
                &request.approval_id,
                &request.tool_call_id,
                &request.command,
                &request.justification,
            );
            event.read_path = request.read_path.clone();
            event.approval = actionable.clone();
            session.lock().waiting = Some(WaitingApproval {
                id: request.approval_id.clone(),
                shared_revision: event.approval.as_ref().map(|request| request.revision),
                decide,
            });
            turn.push_event(event);
            self.bump_status();

            let (decision, label, refresh) = tokio::select! {
                biased;
                received = decided => match received {
                    Ok(ApprovalDecision::Allow) => (ApprovalDecision::Allow, "allow", false),
                    _ => (ApprovalDecision::Deny, "deny", false),
                },
                () = cancel.cancelled() => (ApprovalDecision::Cancelled, "", false),
                shared = async {
                    let scope = session.ctrl.approval_scope();
                    if let Ok(shared) = session.ctrl.approval_service().get(&request.approval_id,&scope.workspace,Some(&scope.session_id)) {
                        session.ctrl.approval_service().wait(&shared.id,&shared.scope,cancel).await.ok()
                    } else { std::future::pending().await }
                } => match shared {
                    Some(r) if r.state == otto_core::approval::State::Approved => (ApprovalDecision::Allow,"allow", false),
                    _ => (ApprovalDecision::Deny,"deny", false),
                },
                () = async {
                    let Some(binding) = &actionable else { return std::future::pending().await; };
                    let service = session.ctrl.approval_service();
                    let mut changes = service.changes();
                    let mut poll = tokio::time::interval(Duration::from_millis(200));
                    loop {
                        if let Ok(current) = service.get(&binding.id, &binding.scope.workspace, Some(&binding.scope.session_id))
                            && current.state == otto_core::approval::State::Pending
                            && (current.revision != binding.revision || current.subject != binding.subject) {
                            return;
                        }
                        tokio::select! { _ = changes.changed() => {}, _ = poll.tick() => {} }
                    }
                } => (ApprovalDecision::Deny, "superseded", true),
                () = session.ctrl.approval_withdrawn(&request.approval_id) => (ApprovalDecision::Deny, "deny", false),
                () = tokio::time::sleep(APPROVAL_TIMEOUT) => (ApprovalDecision::Deny, "timeout", false),
            };
            {
                let mut state = session.lock();
                if state
                    .waiting
                    .as_ref()
                    .is_some_and(|waiting| waiting.id == request.approval_id)
                {
                    state.waiting = None;
                }
                if !refresh {
                    state.remember_decided(&request.approval_id);
                }
            }
            if decision != ApprovalDecision::Cancelled {
                turn.push_event(WireEvent::approval_decided(&request.approval_id, label));
            }
            self.bump_status();
            if !refresh {
                return decision;
            }
        }
    }

    /// Starts a user turn on `session`, or queues it when `queue` is set and
    /// the session is busy. The `trigger == triggerTask` branch lives in
    /// [`Server::wake_turn`], because a wake turn needs no HTTP reply and is
    /// awaited by the wake loop that admitted it.
    /// Starts or queues a user turn. A turn whose `input_id` this session
    /// started recently is returned again instead of starting a second one;
    /// one whose `input_id` the transcript already holds fails with
    /// [`DUPLICATE_INPUT`].
    #[allow(clippy::too_many_arguments)]
    fn start_turn(
        self: &Arc<Self>,
        session: &Arc<OpenSession>,
        text: String,
        image: Option<Block>,
        (channel, origin): (Option<Channel>, Option<crate::schedule::Origin>),
        queue: bool,
        request_id: &str,
        input_id: Option<String>,
    ) -> Result<Arc<Turn>, String> {
        let (turn, running) = {
            let mut state = session.lock();
            if let Some(input_id) = &input_id {
                if let Some((_, turn)) = state.inputs.iter().find(|(id, _)| id == input_id) {
                    return Ok(Arc::clone(turn));
                }
                if session.ctrl.has_input(input_id) {
                    return Err(DUPLICATE_INPUT.to_string());
                }
            }
            let busy = state.busy();
            if busy && !queue {
                return Err(TURN_ACTIVE.to_string());
            }
            if busy && state.queue.len() >= MAX_QUEUED_TURNS {
                return Err(QUEUE_FULL.to_string());
            }
            let id = new_id()?;
            let cancel = self.cancel.child_token();
            let session_id = session.ctrl.info().session_id;
            let log = Arc::clone(&self.log);
            let (turn, running) = if busy {
                // The saved copy needs an input id so a restore can tell
                // whether the turn already started.
                let queued_input_id = input_id.clone().unwrap_or_else(|| format!("queued:{id}"));
                if let Some(journal) = session.ctrl.turn_queue() {
                    journal
                        .save(&app::turn_queue::QueuedPrompt {
                            input_id: queued_input_id.clone(),
                            turn_id: id.clone(),
                            text: text.clone(),
                            channel,
                            origin: origin.clone(),
                            image_dropped: image.is_some(),
                        })
                        .map_err(|_| PROMPT_NOT_PERSISTED.to_string())?;
                }
                let turn = Arc::new(Turn::queued(id, TRIGGER_USER, cancel).with_log(
                    log,
                    &session_id,
                    request_id,
                ));
                state.queue.push_back(QueuedTurn {
                    turn: Arc::clone(&turn),
                    text,
                    image,
                    channel,
                    origin,
                    input_id: Some(queued_input_id),
                });
                (turn, None)
            } else {
                let turn = Arc::new(Turn::new(id, TRIGGER_USER, cancel).with_log(
                    log,
                    &session_id,
                    request_id,
                ));
                turn.start_running(session.ctrl.history().len());
                state.turn = Some(Arc::clone(&turn));
                (turn, Some((text, image, channel, origin, input_id.clone())))
            };
            if let Some(input_id) = input_id {
                if state.inputs.len() >= MAX_REMEMBERED_INPUTS {
                    state.inputs.pop_front();
                }
                state.inputs.push_back((input_id, Arc::clone(&turn)));
            }
            (turn, running)
        };
        session.touch();
        self.bump_status();
        if let Some((text, image, channel, origin, input_id)) = running {
            self.spawn_user_turn(
                session,
                Arc::clone(&turn),
                text,
                image,
                (channel, origin),
                input_id,
            );
        }
        Ok(turn)
    }

    /// Starts the oldest queued turn when nothing holds the session. While the
    /// server or the session is closing, ends every queued turn `canceled`
    /// instead. Called whenever a turn or a compaction ends.
    fn start_next(self: &Arc<Self>, session: &Arc<OpenSession>) {
        let next = {
            let mut state = session.lock();
            if state.turn_running() || state.compacting.is_some() || state.reflecting.is_some() {
                return;
            }
            if self.cancel.is_cancelled() || session.closed.is_cancelled() {
                state.cancel_queue();
                None
            } else if let Some(next) = state.queue.pop_front() {
                next.turn.start_running(session.ctrl.history().len());
                state.turn = Some(Arc::clone(&next.turn));
                Some(next)
            } else {
                None
            }
        };
        self.bump_status();
        if let Some(next) = next {
            session.touch();
            self.spawn_user_turn(
                session,
                next.turn,
                next.text,
                next.image,
                (next.channel, next.origin),
                next.input_id,
            );
        }
    }

    /// Spawns the task that runs `turn`, which is already `state.turn`.
    fn spawn_user_turn(
        self: &Arc<Self>,
        session: &Arc<OpenSession>,
        turn: Arc<Turn>,
        text: String,
        image: Option<Block>,
        (channel, origin): (Option<Channel>, Option<crate::schedule::Origin>),
        input_id: Option<String>,
    ) {
        turn.push_event(WireEvent::user_message(&text, image.is_some()));
        self.metrics.turn_started();
        let mut fields = turn.log_ids();
        fields.push(("trigger", TRIGGER_USER.to_string()));
        self.log.info("turn_started", &fields);
        let server = Arc::clone(self);
        let session = Arc::clone(session);
        tokio::spawn(async move {
            let first = match image {
                Some(image) => Step::Image(&text, image),
                None => Step::Text(&text),
            };
            session.ctrl.set_next_input_id(input_id.clone());
            server
                .run_turn(&session, &turn, first, TRIGGER_USER, channel, origin)
                .await;
            session.ctrl.set_next_input_id(None);
            // The turn ran: a saved copy of it is no longer waiting.
            if let (Some(input_id), Some(journal)) = (&input_id, session.ctrl.turn_queue()) {
                journal.forget(input_id);
            }
            // A turn that ran for longer than the idle timeout restarts the
            // clock, so a reader can still replay it from the buffer.
            session.touch();
            server.start_next(&session);
            // A notification that landed too late for this turn's own drain is
            // caught by the wake loop's end-of-turn check.
            session.turn_finished.notify_one();
        });
    }
}

const TURN_ACTIVE: &str = "turn already active";
const PROMPT_NOT_PERSISTED: &str = app::turn_queue::PROMPT_NOT_PERSISTED;
const QUEUE_FULL: &str = "turn queue full";

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

/// Request extension set by [`Server::socket_router`].
#[derive(Clone, Copy)]
struct SocketRequest;

/// Re-opens a session the idle sweeper evicted before the route runs, so a
/// client that holds a session id keeps working across an eviction. Runs inside
/// [`instrument`], so a request still has to pass the token gate first.
///
/// A failure is not answered here: the route resolves the session itself and
/// answers its own 404 (or its own error) exactly as it does for a session that
/// was never open.
async fn reopen_evicted_session(
    State(server): State<Arc<Server>>,
    request: axum::extract::Request,
    next: Next,
) -> Response {
    if let Some(id) = session_id_in_path(request.uri().path()) {
        let _ = server.session_for_route(id).await;
    }
    next.run(request).await
}

/// The session id in `/v1/sessions/{id}[...]`, when the request's path names
/// one. Anything else -- `/v1/sessions` itself, a route that is not
/// session-scoped, an id that is not a session id -- is left to the route.
fn session_id_in_path(path: &str) -> Option<&str> {
    let id = path.strip_prefix("/v1/sessions/")?.split('/').next()?;
    crate::session::is_session_id(id).then_some(id)
}

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
        && request.extensions().get::<SocketRequest>().is_none()
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
/// only the path, so the method is prepended.
fn route_label(method: &Method, matched: Option<&str>) -> String {
    match matched {
        None => "unmatched".to_string(),
        Some(path) => format!("{method} {path}"),
    }
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
        listen::Listener::Unix(bound, socket_file) => {
            let result = axum::serve(bound, router)
                .with_graceful_shutdown(graceful)
                .await;
            drop(socket_file);
            result
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
    /// True when the session decides requests automatically, in AUTO or YOLO;
    /// read `approval_mode` for the exact policy.
    auto_mode: bool,
    approval_mode: &'static str,
    approval_settings: otto_core::approval::ApprovalSettings,
    turn: Option<SessionTurnWire>,
    #[serde(skip_serializing_if = "Option::is_none")]
    forked_from: Option<app::ForkedFrom>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    plan: Vec<otto_core::session::PlanStep>,
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
    /// The last user message, cut to [`LAST_USER_TEXT_CHARS`] characters.
    #[serde(skip_serializing_if = "String::is_empty")]
    last_user_text: String,
    /// RFC 3339 time of the last write to the session file.
    #[serde(skip_serializing_if = "String::is_empty")]
    modified: String,
}

const LAST_USER_TEXT_CHARS: usize = 80;

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
    /// True when the session decides requests automatically, in AUTO or YOLO;
    /// read `approval_mode` for the exact policy.
    auto_mode: bool,
    approval_mode: &'static str,
    approval_settings: otto_core::approval::ApprovalSettings,
    id: String,
    workspace: String,
    turn: Option<String>,
    /// The running turn, else the newest one; `null` until a turn exists.
    turn_id: Option<String>,
    /// Turns waiting behind the running one.
    queued: usize,
    approvals: usize,
    human_pending: usize,
    reviewing: usize,
    application_failed: usize,
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
    /// Stable identity for retries of a new-session command.
    #[serde(default)]
    request_id: Option<String>,
}

fn command_session_id(kind: &str, workspace: &str, request_id: &str) -> String {
    let mut hash = Sha256::new();
    for part in ["otto.session-command.v1", kind, workspace, request_id] {
        hash.update((part.len() as u64).to_be_bytes());
        hash.update(part.as_bytes());
    }
    hash.finalize()[..16]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn valid_command_id(id: &str) -> bool {
    otto_core::model::is_input_id(id)
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

    if let Some(request_id) = parsed.request_id.as_deref() {
        if !valid_command_id(request_id) {
            return bad_request(
                "request_id must be 1 to 128 ASCII letters, digits, '.', '_', ':' or '-'",
            );
        }
        if !parsed.resume.is_empty() {
            return bad_request("request_id is only valid when creating a session");
        }
        let path = workspace.as_deref().unwrap_or(&server.info.workspace);
        let id = command_session_id("create", path, request_id);
        let _gate = server.open_gate.lock().await;
        let existing = match server.lookup(&id) {
            Some(session) => Some(session),
            None => match server.factory.open(&id, Some(path)).await {
                Ok(ctrl) => Some(server.register(ctrl)),
                Err(error) if error == SESSION_NOT_FOUND => None,
                Err(error) => return internal_error(&server.log, &error),
            },
        };
        if let Some(session) = existing {
            let receipt = session.ctrl.session_custom_data("otto.session_command");
            if receipt.is_none()
                && session.ctrl.history().is_empty()
                && session
                    .ctrl
                    .session_custom_data(otto_core::session::FORK_CUSTOM_TYPE)
                    .is_none()
            {
                // A crash after the header write but before the receipt can
                // leave an otherwise empty deterministic session file.
                return match session.ctrl.write_session_command_receipt(request_id).await {
                    Ok(()) => json_response(StatusCode::OK, &server.session_wire(&session)),
                    Err(error) => internal_error(&server.log, &error),
                };
            }
            let matches = receipt
                .as_deref()
                .and_then(|raw| serde_json::from_str::<serde_json::Value>(raw).ok())
                .is_some_and(|value| {
                    value["command"] == "create"
                        && value["requestId"] == request_id
                        && session.ctrl.workspace() == path
                });
            return if matches {
                json_response(StatusCode::OK, &server.session_wire(&session))
            } else {
                error_response(
                    StatusCode::CONFLICT,
                    "request_id_conflict",
                    "request_id belongs to a different session command",
                )
            };
        }
        return match server
            .factory
            .create_with_receipt(Some(path), id, request_id)
            .await
        {
            Ok(ctrl) => json_response(
                StatusCode::CREATED,
                &server.session_wire(&server.register(ctrl)),
            ),
            Err(error) => internal_error(&server.log, &error),
        };
    }

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
            last_user_text: info
                .last_user_text
                .chars()
                .take(LAST_USER_TEXT_CHARS)
                .collect(),
            modified: info.modified.to_rfc3339(),
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
            last_user_text: String::new(),
            modified: String::new(),
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
struct ProfileBody {
    /// The configured profile this session should run on.
    #[serde(default)]
    profile: String,
}

/// `POST /v1/sessions/{id}/profile`. Rebuilds this session's runner on another
/// configured profile and answers with the session wire. It never writes the
/// configuration file, so other sessions and the next `otto serve` start keep
/// the profile they had.
async fn set_session_profile(
    State(server): State<Arc<Server>>,
    Path(id): Path<String>,
    body: Bytes,
) -> Response {
    let Some(session) = server.lookup(&id) else {
        return not_found("session not found");
    };
    let parsed: ProfileBody = match serde_json::from_slice(&body) {
        Ok(parsed) => parsed,
        Err(_) => return bad_request("invalid JSON body"),
    };
    if parsed.profile.is_empty() {
        return bad_request("profile must not be empty");
    }
    match session.ctrl.switch_profile_result(&parsed.profile).await {
        Ok(_) => {
            server.logger().info(
                "profile_switched",
                &[("session_id", id), ("profile", parsed.profile)],
            );
            json_response(StatusCode::OK, &server.session_wire(&session))
        }
        Err(message) if message == app::PROMPT_ACTIVE => {
            turn_active("a turn is already active for this session")
        }
        // A session without dynamic content, or under a policy that forbids a
        // dynamic profile, cannot switch; retrying the same request cannot help.
        Err(message)
            if message == app::PROFILE_SWITCH_UNAVAILABLE
                || message == app::SESSION_OPERATION_UNAVAILABLE =>
        {
            error_response(StatusCode::CONFLICT, "profile_switch_unavailable", &message)
        }
        // Unknown profile or invalid profile content; the builder redacts it.
        Err(message) => bad_request(&message),
    }
}

#[derive(Debug, Default, Deserialize)]
struct ForkBody {
    /// The turn-opening user message to stop before; absent forks the whole
    /// history up to the last turn boundary.
    #[serde(default)]
    before: Option<String>,
    #[serde(default)]
    request_id: Option<String>,
}

#[derive(Debug, Serialize)]
struct ForkedWire {
    #[serde(flatten)]
    session: SessionWire,
    /// The text of the message the fork stops before, for the editor.
    #[serde(skip_serializing_if = "Option::is_none")]
    prompt: Option<String>,
}

/// `POST /v1/sessions/{id}/fork`: writes a new session from this one's
/// history and opens it. The source session is unchanged.
async fn fork_session(
    State(server): State<Arc<Server>>,
    Path(id): Path<String>,
    body: Bytes,
) -> Response {
    let parsed: ForkBody = if body.iter().all(u8::is_ascii_whitespace) {
        ForkBody::default()
    } else {
        match serde_json::from_slice(&body) {
            Ok(parsed) => parsed,
            Err(_) => return bad_request("invalid JSON body"),
        }
    };
    let session = if parsed.request_id.is_some() {
        match server.resume_or_create(&id, None).await {
            Ok((session, _)) => session,
            Err(error) if error == SESSION_NOT_FOUND => return not_found("session not found"),
            Err(error) => return internal_error(&server.log, &error),
        }
    } else {
        let Some(session) = server.lookup(&id) else {
            return not_found("session not found");
        };
        session
    };
    if let Some(request_id) = parsed.request_id.as_deref() {
        if !valid_command_id(request_id) {
            return bad_request(
                "request_id must be 1 to 128 ASCII letters, digits, '.', '_', ':' or '-'",
            );
        }
        let workspace = session.ctrl.info().workspace;
        let fork_id = command_session_id("fork", &workspace, request_id);
        let _gate = server.open_gate.lock().await;
        let existing = match server.lookup(&fork_id) {
            Some(open) => Some(open),
            None => match server.factory.open(&fork_id, Some(&workspace)).await {
                Ok(ctrl) => Some(server.register(ctrl)),
                Err(error) if error == SESSION_NOT_FOUND => None,
                Err(error) => return internal_error(&server.log, &error),
            },
        };
        if let Some(open) = existing {
            let record = open
                .ctrl
                .session_custom_data(otto_core::session::FORK_CUSTOM_TYPE)
                .and_then(|raw| otto_core::session::decode_fork_record(&raw));
            return match record {
                Some(record)
                    if record.request_id.as_deref() == Some(request_id)
                        && record.source_session_id == id
                        && record.source_entry_id.as_deref() == parsed.before.as_deref() =>
                {
                    json_response(
                        StatusCode::OK,
                        &ForkedWire {
                            session: server.session_wire(&open),
                            prompt: record.prompt,
                        },
                    )
                }
                _ => error_response(
                    StatusCode::CONFLICT,
                    "request_id_conflict",
                    "request_id belongs to a different fork command",
                ),
            };
        }
        let forked = match session.ctrl.fork_file_with_id(
            parsed.before.as_deref(),
            Some(&fork_id),
            Some(request_id),
        ) {
            Ok(forked) => forked,
            Err(app::ForkFailure::InvalidPoint(message)) => {
                return error_response(StatusCode::CONFLICT, "invalid_fork_point", &message);
            }
            Err(app::ForkFailure::Failed(message)) if message == app::NOTHING_TO_FORK => {
                return error_response(StatusCode::CONFLICT, "invalid_fork_point", &message);
            }
            Err(app::ForkFailure::Failed(message)) => return internal_error(&server.log, &message),
        };
        return match server.factory.open(&forked.id, Some(&workspace)).await {
            Ok(ctrl) => json_response(
                StatusCode::CREATED,
                &ForkedWire {
                    session: server.session_wire(&server.register(ctrl)),
                    prompt: forked.point.map(|point| point.prompt),
                },
            ),
            Err(error) => internal_error(&server.log, &error),
        };
    }
    let forked = match session.ctrl.fork_file(parsed.before.as_deref()) {
        Ok(forked) => forked,
        Err(app::ForkFailure::InvalidPoint(message)) => {
            return error_response(StatusCode::CONFLICT, "invalid_fork_point", &message);
        }
        Err(app::ForkFailure::Failed(message)) if message == app::NOTHING_TO_FORK => {
            return error_response(StatusCode::CONFLICT, "invalid_fork_point", &message);
        }
        Err(app::ForkFailure::Failed(message)) => return internal_error(&server.log, &message),
    };
    let workspace = session.ctrl.info().workspace;
    match server.resume_or_create(&forked.id, Some(&workspace)).await {
        Ok((opened, _)) => json_response(
            StatusCode::CREATED,
            &ForkedWire {
                session: server.session_wire(&opened),
                prompt: forked.point.map(|point| point.prompt),
            },
        ),
        Err(error) => internal_error(&server.log, &error),
    }
}

#[derive(Debug, Serialize)]
struct ForkPointWire {
    entry_id: String,
    prompt: String,
}

#[derive(Debug, Serialize)]
struct ForkPointsResponse {
    fork_points: Vec<ForkPointWire>,
}

/// `GET /v1/sessions/{id}/fork-points`: the user prompts a fork can stop
/// before, oldest first.
async fn fork_points(State(server): State<Arc<Server>>, Path(id): Path<String>) -> Response {
    let Some(session) = server.lookup(&id) else {
        return not_found("session not found");
    };
    match session.ctrl.fork_points() {
        Ok(points) => json_response(
            StatusCode::OK,
            &ForkPointsResponse {
                fork_points: points
                    .into_iter()
                    .map(|point| ForkPointWire {
                        entry_id: point.entry_id,
                        prompt: point.prompt,
                    })
                    .collect(),
            },
        ),
        Err(error) => internal_error(&server.log, &error),
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
    // Deleting ends the session's queued turns for good.
    if let Some(journal) = session.ctrl.turn_queue() {
        journal.clear();
    }
    // A close, not an eviction: the client asked for this session to be closed,
    // so a later request for it must not re-open it.
    server.forget_evicted(&id);
    if let Err(error) = server.close_session(&session).await {
        server.log.error(
            "session_close_error",
            &[("session_id", id.clone()), ("error", error)],
        );
    }
    server.metrics.sessions_open(-1);
    Response::builder()
        .status(StatusCode::NO_CONTENT)
        .body(Body::empty())
        .expect("static response")
}

#[derive(Debug, Default, Deserialize)]
struct HistoryQuery {
    before_turn: Option<String>,
}

/// The session's messages; with `before_turn`, only those that existed when
/// that turn started, so a reader replaying the turn's events from 0 does not
/// see its prompt and finished steps twice.
async fn history(
    State(server): State<Arc<Server>>,
    Path(id): Path<String>,
    Query(query): Query<HistoryQuery>,
) -> Response {
    let Some(session) = server.lookup(&id) else {
        return not_found("session not found");
    };
    let mut messages = session.ctrl.history();
    if let Some(turn_id) = query.before_turn.as_deref() {
        let Some(start) = session
            .find_turn(turn_id)
            .and_then(|turn| turn.history_start())
        else {
            return not_found("turn not found");
        };
        messages.truncate(start);
    }
    // An empty Vec serializes as "[]".
    json_response::<Vec<Message>>(StatusCode::OK, &messages)
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

async fn trace(State(server): State<Arc<Server>>, Path(id): Path<String>) -> Response {
    let Some(session) = server.lookup(&id) else {
        return not_found("session not found");
    };
    let ctrl = session.ctrl.clone();
    match tokio::task::spawn_blocking(move || ctrl.trace()).await {
        Ok(Ok(trace)) => json_response(StatusCode::OK, &trace),
        Ok(Err(message)) => error_response(StatusCode::CONFLICT, "trace_unavailable", &message),
        Err(_) => error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal",
            "the trace could not be read",
        ),
    }
}

async fn trace_span(
    State(server): State<Arc<Server>>,
    Path((id, span_id)): Path<(String, String)>,
) -> Response {
    let Some(session) = server.lookup(&id) else {
        return not_found("session not found");
    };
    let ctrl = session.ctrl.clone();
    match tokio::task::spawn_blocking(move || ctrl.trace_span(&span_id)).await {
        Ok(Ok(Some(content))) => json_response(StatusCode::OK, &content),
        Ok(Ok(None)) => not_found("span not found"),
        Ok(Err(message)) => error_response(StatusCode::CONFLICT, "trace_unavailable", &message),
        Err(_) => error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal",
            "the trace could not be read",
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
    /// Queue behind a running turn instead of answering 409 `turn_active`.
    #[serde(default)]
    queue: bool,
    /// Per-turn chat channel context.
    #[serde(default)]
    channel: Option<Channel>,
    /// Where the turn came from: `terminal`, `web` or
    /// `chat:<platform>:<chat id>`. Absent means `web`.
    #[serde(default)]
    origin: Option<String>,
    /// The client's stable id for this input, so a retry is not run twice.
    #[serde(default)]
    input_id: Option<String>,
}

/// The `400` message for a malformed `input_id`.
pub(crate) const INVALID_INPUT_ID: &str =
    "input_id must be 1 to 128 ASCII letters, digits, '.', '_', ':' or '-'";

#[derive(Debug, Deserialize)]
struct StartTurnImage {
    #[serde(default)]
    data: String,
    #[serde(default)]
    mime_type: String,
}

/// The client-supplied `X-Request-Id` header, sanitized; empty when absent
/// (the middleware's generated fallback is not read here).
fn client_request_id(headers: &HeaderMap) -> String {
    headers
        .get("x-request-id")
        .map(|value| request_id(value.as_bytes()))
        .unwrap_or_default()
}

async fn start_turn(
    State(server): State<Arc<Server>>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let Some(session) = server.lookup(&id) else {
        return not_found("session not found");
    };
    let request_id = client_request_id(&headers);
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
    let origin = match parsed.origin.as_deref().map(crate::schedule::Origin::parse) {
        Some(Ok(origin)) => origin,
        Some(Err(message)) => return bad_request(&message),
        None => crate::schedule::Origin::web(),
    };
    if let (Some(channel), Some(platform)) = (parsed.channel, origin.chat_platform()) {
        let expected = match channel {
            Channel::Telegram => "telegram",
            Channel::Feishu => "feishu",
        };
        if platform != expected {
            return bad_request("origin platform does not match channel");
        }
    }
    let stream = parsed.stream.unwrap_or(true);

    if parsed
        .input_id
        .as_deref()
        .is_some_and(|id| !otto_core::model::is_input_id(id))
    {
        return bad_request(INVALID_INPUT_ID);
    }
    let turn = match server.start_turn(
        &session,
        parsed.text,
        image,
        (parsed.channel, Some(origin)),
        parsed.queue,
        &request_id,
        parsed.input_id,
    ) {
        Ok(turn) => turn,
        Err(error) if error == DUPLICATE_INPUT => {
            return error_response(StatusCode::CONFLICT, "duplicate_input", DUPLICATE_INPUT);
        }
        Err(error) if error == TURN_ACTIVE => {
            return turn_active("a turn is already active for this session");
        }
        Err(error) if error == QUEUE_FULL => {
            return error_response(
                StatusCode::CONFLICT,
                "queue_full",
                "the turn queue for this session is full",
            );
        }
        Err(error) if error == PROMPT_NOT_PERSISTED => {
            return error_response(
                StatusCode::SERVICE_UNAVAILABLE,
                "input_not_persisted",
                PROMPT_NOT_PERSISTED,
            );
        }
        Err(error) => return internal_error(&server.log, &error),
    };

    let mut response = if stream {
        stream_sse(&server.metrics, Arc::clone(&turn), 0)
    } else {
        wait_done(&turn).await;
        json_response(StatusCode::OK, &turn.summary())
    };
    if let Ok(value) = axum::http::HeaderValue::from_str(&turn.id) {
        response.headers_mut().insert(TURN_ID_HEADER, value);
    }
    response
}

/// Names the turn a `POST .../turns` response belongs to.
const TURN_ID_HEADER: &str = "otto-turn-id";

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
    session.find_turn(turn_id).ok_or("turn not found")
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
            // A queued turn never ran: remove it and end it `canceled`. The
            // queue and the end happen under one lock so `start_next` cannot
            // pick it up in between.
            let removed = server.lookup(&id).is_some_and(|session| {
                let mut guard = session.lock();
                let state = &mut *guard;
                let mut cancelled = Vec::new();
                for queue in [&mut state.queue, &mut state.restored] {
                    if let Some(position) =
                        queue.iter().position(|queued| queued.turn.id == turn.id)
                        && let Some(queued) = queue.remove(position)
                    {
                        cancelled.push(queued);
                    }
                }
                for queued in &cancelled {
                    turn.finish(Some(String::new()), true);
                    // Cancelled for good: a restart must not run it.
                    if let (Some(input_id), Some(journal)) =
                        (&queued.input_id, session.ctrl.turn_queue())
                    {
                        journal.forget(input_id);
                    }
                }
                !cancelled.is_empty()
            });
            if removed {
                server.bump_status();
            } else {
                turn.cancel();
            }
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
mod tests;
