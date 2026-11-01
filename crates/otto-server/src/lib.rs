//! `otto serve`: the HTTP+JSON+SSE frontend.
//!
//! One [`app::Controller`] per session, a turn event buffer decoupled from the
//! agent's synchronous emit callback ([`turn::Turn`]), Prometheus metrics
//! ([`otto::metrics::Metrics`]), and structured request logging. See
//! `docs/specs/2026-09-03-agent-server-design.md`.
//!
//! Concurrency: `sessions` is a plain `Mutex<HashMap>` held only for map
//! operations. Each [`OpenSession`] has its own `Mutex` for the turn,
//! and compaction slots.

pub mod agents;
pub mod approvals;
pub mod auth;
pub mod compact;
pub mod config_routes;
pub mod delivery;
pub mod diff;
pub mod fs;
pub mod listen;
pub mod mcp;
mod middleware;
mod registry;
mod responses;
pub mod sandbox;
pub mod schedule;
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

use middleware::*;
pub use responses::*;
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
use axum::routing::{delete, get, post};
use otto_core::agent::Event;
use otto_core::model::{Block, MAX_IMAGE_BYTES, Message, Usage};
use otto_core::session::ListResult;
use otto_core::wire::sse::format_frame;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use otto::app::task_control::DUPLICATE_INPUT;
use otto::app::{self, ApprovalDecision, ApprovalRequest, Channel, Controller, Step, Stop};
pub use otto::log::Logger;
pub(crate) use otto::log::request_id;
use otto::metrics::{Metrics, SessionContext};
use otto::runtime::info::SandboxInfo;
#[cfg(test)]
use otto::runtime::info::{SandboxMode, SandboxNetwork, SandboxReason};
use otto_core::wire::events::WireEvent;
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
    origin: Option<otto::schedule::Origin>,
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

#[derive(Default)]
struct SessionState {
    /// The running turn, or the last one that finished. Never a queued turn.
    turn: Option<Arc<Turn>>,
    /// Non-`None` while `POST .../compact` runs. `start_turn` and
    /// `handle_compact` both check it under the same lock, so a turn and a
    /// compaction are never admitted together.
    compacting: Option<CancellationToken>,
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
        self.turn_running() || !self.queue.is_empty() || self.compacting.is_some()
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
    pub fn schedule_store(&self) -> Option<Arc<dyn otto::schedule::ScheduleStore>> {
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
            .route("/v1/config/providers", get(config_routes::providers))
            .route(
                "/v1/config/providers/{name}",
                get(config_routes::get_provider),
            )
            .route("/v1/config/models", get(config_routes::models))
            .route("/v1/config/changes", post(config_routes::preview))
            .route(
                "/v1/config/changes/{id}",
                post(config_routes::confirm).delete(config_routes::cancel),
            )
            .route("/v1/sessions", post(create_session).get(list_sessions))
            .route(
                "/v1/sessions/{id}",
                get(get_session)
                    .patch(rename_session)
                    .delete(delete_session),
            )
            .route("/v1/sessions/{id}/fork", post(fork_session))
            .route("/v1/sessions/{id}/model", post(set_session_model))
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
                "/v1/sessions/{id}/tasks/steer/interrupt",
                post(task_control::interrupt_steering),
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
            .route("/v1/sessions/{id}/wake-replies", get(wake_replies::list))
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
}

#[cfg(test)]
mod tests;
