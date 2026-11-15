//! `otto serve`: the HTTP+JSON+SSE frontend.
//!
//! One [`app::Controller`] per session, a turn event buffer decoupled from the
//! agent's synchronous emit callback ([`turn::Turn`]), Prometheus metrics
//! ([`otto::metrics::Metrics`]), and structured request logging.
//!
//! Concurrency: `sessions` is a plain `Mutex<HashMap>` held only for map
//! operations. Each [`OpenSession`] has its own `Mutex` for admission,
//! exclusive activity and queued work.

pub mod agents;
pub mod approvals;
pub mod auth;
pub mod authorization;
pub mod compact;
pub mod config_routes;
pub mod delivery;
pub mod diff;
pub mod fs;
pub mod listen;
mod local_commands;
pub mod mcp;
mod memory_dto;
pub mod memory_routes;
mod middleware;
mod registry;
mod responses;
pub mod sandbox;
mod sandbox_changes;
pub mod schedule;
mod session_activity;
mod session_retirement;
mod session_routes;
pub mod skills;
mod status;
pub mod task_control;
pub mod tasks;
pub mod timers;
pub mod tls;
pub mod turn;
mod turn_routes;
mod turn_runner;
mod wake_replies;
pub mod workflows;
mod workspace_factory;
pub mod workspaces;

use authorization::{OperationId, OperationRoutes};
use middleware::*;
pub use responses::*;
use session_activity::SessionState;
pub use session_routes::*;
use status::*;
pub use turn_routes::INVALID_INPUT_ID;
pub(crate) use turn_routes::*;
pub use workspace_factory::*;

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
use axum::routing::post;
use otto_core::agent::Event;
use otto_core::model::{Block, MAX_IMAGE_BYTES, Message, Usage};
use otto_core::session::ListResult;
use otto_core::wire::sse::format_frame;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use otto::app::{self, ApprovalDecision, ApprovalRequest, Channel, Controller, Step, Stop};
use otto::metrics::{Metrics, SessionContext};
use otto::runtime::task_control::DUPLICATE_INPUT;
use otto::sandbox::info::SandboxInfo;
#[cfg(test)]
use otto::sandbox::info::{SandboxMode, SandboxNetwork, SandboxReason};
use otto_core::wire::events::WireEvent;
pub use otto_platform::log::Logger;
pub(crate) use otto_platform::log::request_id;
use turn::{TRIGGER_RECOVERY, TRIGGER_TASK, TRIGGER_USER, Turn};

/// A [`Factory::open`] that answers with exactly this text produces 404
/// `not_found` instead of 500.
pub const SESSION_NOT_FOUND: &str = "session not found";
const IMAGE_TURN_BODY_MAX_BYTES: usize = MAX_IMAGE_BYTES * 4 / 3 + 4096;

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

/// Configures a [`Server`].
pub struct Options {
    pub factory: Arc<dyn Factory>,
    pub info: Info,
    /// The resolved Otto configuration file; config-management routes are the
    /// only server routes that read or write it.
    pub config_path: std::path::PathBuf,
    /// When non-empty, required as `Authorization: Bearer <token>` on every
    /// `/v1/` route, and on every route except `GET /healthz` when
    /// `remote_listener` is set. Empty means no check, which is only safe
    /// behind a Unix socket with private file modes. Requests that arrive on
    /// the Unix socket router never need the token.
    pub token: String,
    /// The TCP listener is bound to a non-loopback address. Every route except
    /// `GET /healthz` then needs the token, because `/metrics` carries session
    /// ids and model names.
    pub remote_listener: bool,
    pub logger: Option<Arc<Logger>>,
    pub workflows: Option<Arc<otto::workflow::Controller>>,
    /// The scheduled-delivery store shared by every workspace; `None` when
    /// `~/.otto/schedule.db` could not be opened.
    pub schedule: Option<Arc<dyn otto::schedule::ScheduleStore>>,
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
    origin: Option<otto_session::origin::Origin>,
    input_id: Option<String>,
    input_provenance: Option<otto_core::model::InputProvenance>,
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
        self.lock().last_turn().cloned()
    }

    /// The running, last finished or queued turn named `id`.
    fn find_turn(&self, id: &str) -> Option<Arc<Turn>> {
        let state = self.lock();
        state
            .last_turn()
            .into_iter()
            .chain(state.queue.iter().map(|queued| &queued.turn))
            .chain(state.restored.iter().map(|queued| &queued.turn))
            .find(|turn| turn.id == id)
            .cloned()
    }

    /// Cancels the running turn or compaction so a following close does not
    /// wait on a provider call, ends the queued turns, and stops the wake
    /// loop.
    fn cancel_work(&self) {
        let mut state = self.lock();
        state.begin_close();
        self.closed.cancel();
        state.cancel_queue();
    }
}

/// `otto serve`'s router plus the session registry and turn lifecycle behind
/// it. Construct with [`Server::new`].
pub struct Server {
    factory: Arc<dyn Factory>,
    info: Info,
    config_path: std::path::PathBuf,
    /// One-time, in-memory model-change previews keyed by their capability
    /// token. Tokens expire in the config route before a write can occur.
    approvals: Option<Arc<otto::approval::Service>>,
    token: String,
    /// See [`Options::remote_listener`].
    remote_listener: bool,
    log: Arc<Logger>,
    metrics: Arc<Metrics>,
    sessions: Mutex<HashMap<String, Arc<OpenSession>>>,
    /// Serializes every `Factory::open`, so two concurrent resumes of one id
    /// cannot build two controllers for the same session file at once.
    ///
    /// ponytail: one gate for every id instead of a per-id gate. Resuming is an
    /// admin-rate path; make it per-id if it ever contends.
    open_gate: tokio::sync::Mutex<()>,
    /// IDs being permanently deleted, or already deleted in this process.
    permanent_deletions: Mutex<std::collections::HashSet<String>>,
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
    workflows: Option<Arc<otto::workflow::Controller>>,
    schedule: Option<Arc<dyn otto::schedule::ScheduleStore>>,
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
                    Arc::new(otto::approval::model_change::Adapter {
                        path: options.config_path.clone(),
                    }),
                )
                .expect("approval domain registry");
        }
        let logger = options.logger.unwrap_or_else(|| Arc::new(Logger::stderr()));
        Arc::new(Self {
            factory: options.factory,
            info: options.info,
            config_path: options.config_path,
            approvals,
            token: options.token,
            remote_listener: options.remote_listener,
            metrics: Arc::new(Metrics::with_logger(Arc::clone(&logger))),
            log: logger,
            sessions: Mutex::new(HashMap::new()),
            open_gate: tokio::sync::Mutex::new(()),
            permanent_deletions: Mutex::new(std::collections::HashSet::new()),
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
            session.ctrl.close_resources().await;
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
    /// `otto_cli::serve::run` after [`Self::cancel_token`] was cancelled and
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

    /// The router for a transport whose callers are already authenticated as
    /// `principal`: the Unix socket and the in-process ACP connections. Its
    /// requests skip the bearer token that [`Server::router`] requires on TCP
    /// and are authorized as `principal`.
    pub fn router_for(self: &Arc<Self>, principal: authorization::Principal) -> Router {
        self.router().layer(axum::middleware::from_fn(
            move |mut request: axum::extract::Request, next: Next| {
                let principal = principal.clone();
                async move {
                    request.extensions_mut().insert(principal);
                    next.run(request).await
                }
            },
        ))
    }

    /// The router for a Unix socket listener. Reaching the socket needs
    /// file-system permission on its path, so its callers are the owner.
    pub fn socket_router(self: &Arc<Self>) -> Router {
        self.router_for(authorization::Principal::Owner)
    }

    /// Every API operation, plus the token-free `/healthz` and `/metrics`.
    pub fn router(self: &Arc<Self>) -> Router {
        let state = Arc::clone(self);
        api_routes()
            .into_router()
            .layer(DefaultBodyLimit::max(1 << 20))
            .layer(axum::middleware::from_fn_with_state(
                Arc::clone(&state),
                reopen_evicted_session,
            ))
            .layer(axum::middleware::from_fn_with_state(
                Arc::clone(&state),
                authorization::authorize_operation,
            ))
            .layer(axum::middleware::from_fn_with_state(
                Arc::clone(&state),
                instrument,
            ))
            .with_state(state)
    }
}

/// The handler of every [`OperationId`].
pub(crate) fn api_routes() -> OperationRoutes {
    use OperationId as Op;
    OperationRoutes::new()
        .add(Op::ApprovalList, approvals::list_shared)
        .add(Op::ApprovalChanges, approvals::changes_shared)
        .add(Op::ApprovalEvents, approvals::events_shared)
        .add(Op::ApprovalGet, approvals::get_shared)
        .add(Op::ApprovalDecide, approvals::decide_shared)
        .add(Op::ApprovalWithdraw, approvals::withdraw_shared)
        .add(Op::ConfigProviderList, config_routes::providers)
        .add(Op::ConfigProviderGet, config_routes::get_provider)
        .add(Op::ConfigModelList, config_routes::models)
        .add(Op::ConfigChangePreview, config_routes::preview)
        .add(Op::ConfigChangeConfirm, config_routes::confirm)
        .add(Op::ConfigChangeCancel, config_routes::cancel)
        .add(Op::SessionCreate, create_session)
        .add(Op::SessionList, list_sessions)
        .add(Op::SessionGet, get_session)
        .add(Op::SessionRename, rename_session)
        .add(Op::SessionDelete, delete_session)
        .add(
            Op::SessionDeletePermanent,
            session_retirement::delete_handle,
        )
        .add(Op::SessionArchive, session_retirement::archive_handle)
        .add(Op::SessionFork, fork_session)
        .add(Op::SessionModelSet, set_session_model)
        .add(Op::SessionThinkingSet, set_session_thinking)
        .add(Op::SessionForkPoints, fork_points)
        .add(Op::SessionHistory, history)
        .add(Op::SessionLocalCommand, local_commands::execute)
        .add(Op::SessionContext, context)
        .add(Op::SessionTrace, trace)
        .add(Op::SessionTraceSpan, trace_span)
        .add(Op::SessionApprovalMessage, approvals::message)
        .add(Op::SessionInputAdmit, task_control::admit_input)
        .add(Op::SessionSteerList, task_control::steering_messages)
        .add(Op::SessionSteer, task_control::steer)
        .add(Op::SessionSteerInterrupt, task_control::interrupt_steering)
        .add(Op::SessionSteerWithdraw, task_control::withdraw_steering)
        .add(Op::SessionTaskControl, task_control::control)
        .add(Op::SessionApprovalDecide, approvals::decide)
        .add_method_router(
            Op::SessionTurnStart,
            post(start_turn).layer(DefaultBodyLimit::max(IMAGE_TURN_BODY_MAX_BYTES)),
        )
        .add(Op::SessionTurnGet, get_turn)
        .add(Op::SessionTurnEvents, turn_events)
        .add(Op::SessionTurnCancel, cancel_turn)
        .add(Op::SessionCompact, compact::handle)
        .add(Op::SessionWakeReplies, wake_replies::list)
        .add(Op::SessionTaskList, tasks::list)
        .add(Op::SessionTaskObserver, tasks::observer)
        .add(Op::SessionDeliveryGet, delivery::get)
        .add(Op::SessionTaskGet, tasks::get)
        .add(Op::SessionTaskCancel, tasks::cancel)
        .add(Op::SessionTimerList, timers::list)
        .add(Op::SessionTimerCancel, timers::cancel)
        .add(Op::SessionMcpList, mcp::list)
        .add(Op::MemoryRecordList, memory_routes::list)
        .add(Op::MemoryRecordCreate, memory_routes::create)
        .add(Op::MemoryRecordGet, memory_routes::show)
        .add(Op::MemoryRecordForget, memory_routes::forget)
        .add(Op::MemoryRecordStateSet, memory_routes::set_state)
        .add(Op::MemoryRecordVerify, memory_routes::verify)
        .add(Op::MemoryRecordHistory, memory_routes::history)
        .add(Op::MemoryRecordDiff, memory_routes::diff)
        .add(Op::MemoryRecordRestore, memory_routes::restore)
        .add(Op::MemoryRecordPropose, memory_routes::propose)
        .add(Op::MemorySearch, memory_routes::search)
        .add(Op::MemoryCandidateList, memory_routes::candidates)
        .add(Op::MemoryCandidateReview, memory_routes::review)
        .add(Op::SkillManagerList, skills::manager)
        .add(Op::SkillManagerInfo, skills::info)
        .add(Op::SkillList, skills::list)
        .add(Op::SkillGet, skills::get)
        .add(Op::SkillEnable, skills::enable)
        .add(Op::SkillDisable, skills::disable)
        .add(Op::SandboxReload, sandbox::reload)
        .add(Op::SessionSandboxChangeApply, sandbox_changes::apply)
        .add(Op::SessionApprovalEvents, approvals::session_events)
        .add(Op::SessionApprovalModeGet, approvals::mode)
        .add(Op::SessionApprovalModeSet, approvals::set_mode)
        .add(Op::SessionApprovalPolicyGet, approvals::policy)
        .add(Op::SessionApprovalPolicySet, approvals::set_policy)
        .add(Op::ScheduleClaim, schedule::claim)
        .add(Op::ScheduleDelivered, schedule::delivered)
        .add(Op::TaskList, agents::list)
        .add(Op::TaskGet, agents::get)
        .add(Op::WorkflowList, workflows::list)
        .add(Op::WorkflowStart, workflows::start)
        .add(Op::WorkflowGet, workflows::get)
        .add(Op::WorkflowEvents, workflows::events)
        .add(Op::WorkflowResume, workflows::resume)
        .add(Op::WorkflowFork, workflows::fork)
        .add(Op::WorkflowCancel, workflows::cancel)
        .add(Op::WorkflowRequestApprove, workflows::approve)
        .add(Op::WorkflowRequestReject, workflows::reject)
        .add(Op::WorkspaceList, workspaces::list)
        .add(Op::WorkspaceRegister, workspaces::register)
        .add(Op::WorkspaceRemove, workspaces::remove)
        .add(Op::WorkspaceDiff, diff::get)
        .add(Op::FsDirList, fs::dirs)
        .add(Op::Info, info)
        .add(Op::Status, status)
        .add(Op::Usage, usage)
        .add(Op::UsageDaily, daily_usage)
        .add(Op::OpenApi, openapi)
        .add(Op::Healthz, healthz)
        .add(Op::Metrics, prometheus)
}

#[cfg(test)]
mod tests;
