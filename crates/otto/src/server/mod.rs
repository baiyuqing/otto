//! `otto serve`: the HTTP+JSON+SSE frontend.
//!
//! Port of `internal/server`. One [`app::Controller`] per session, a turn
//! event buffer decoupled from the agent's synchronous emit callback
//! ([`turn::Turn`]), Prometheus metrics ([`metrics::Metrics`]), and
//! structured request logging. See
//! `docs/specs/2026-09-03-agent-server-design.md`.
//!
//! Concurrency: `sessions` is a plain `Mutex<HashMap>` held only for map
//! operations. Each [`OpenSession`] has its own `Mutex` for the turn and
//! compaction slots, matching Go's `openSession.mu`.

pub mod approvals;
pub mod auth;
pub mod compact;
pub mod listen;
pub mod mcp;
pub mod metrics;
pub mod sandbox;
pub mod tasks;
pub mod turn;
pub mod ui;

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
use tokio_util::sync::CancellationToken;

use crate::app::{self, Controller};
use crate::cli::info::SandboxInfo;
use metrics::{Metrics, SessionContext};
use turn::{TRIGGER_TASK, TRIGGER_USER, Turn};

/// Go's `server.ErrSessionNotFound`. A [`Factory::open`] that answers with
/// exactly this text produces 404 `not_found` instead of 500.
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

/// Process-level static info, unrelated to any session. Port of
/// `server.Info`; field order matches, so the JSON bytes match.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Info {
    pub workspace: String,
    pub provider: String,
    pub profile: String,
    pub model: String,
    pub sandbox: String,
    pub profiles: Vec<String>,
}

/// What a [`Server`] needs from its composition root. Port of the four
/// function fields on `server.Options`.
///
/// `list` and `reload_sandbox` return `None` where Go leaves the field nil.
#[async_trait::async_trait]
pub trait Factory: Send + Sync {
    /// A brand new session.
    async fn create(&self) -> Result<Controller, String>;
    /// An existing session by id. [`SESSION_NOT_FOUND`] means 404.
    async fn open(&self, id: &str) -> Result<Controller, String>;
    /// The sessions on disk. `None` disables the disk half of `GET
    /// /v1/sessions`.
    fn list(&self) -> Option<Result<ListResult, String>> {
        None
    }
    /// Whether `POST /v1/sandbox/reload` is wired at all. Checked before
    /// the turn-active guard, matching Go's `reload == nil` first test.
    fn sandbox_reload_available(&self) -> bool {
        false
    }
    /// Re-reads the sandbox configuration. `None` disables `POST
    /// /v1/sandbox/reload`, which then answers 501.
    async fn reload_sandbox(&self) -> Option<Result<SandboxInfo, String>> {
        None
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
}

/// Configures a [`Server`]. Port of `server.Options`.
pub struct Options {
    pub factory: Arc<dyn Factory>,
    pub info: Info,
    /// When non-empty, required as `Authorization: Bearer <token>` on every
    /// `/v1/` route. Empty means no check, which is only safe behind a Unix
    /// socket with private file modes.
    pub token: String,
    pub logger: Option<Arc<Logger>>,
}

// ---- logging ----

/// Go `slog` `TextHandler` output, hand-rolled.
///
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

/// Go `slog`'s `needsQuoting` rule: quote an empty value, and any value
/// carrying a space, a quote, an equals sign, a backslash, or a control
/// character.
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
/// turn, if any. Port of `server.openSession`.
pub struct OpenSession {
    ctrl: Arc<Controller>,
    state: Mutex<SessionState>,
    /// Signaled after every turn on this session finishes. The wake loop is
    /// the only waiter. Port of `openSession.turnFinished`, a capacity-1
    /// channel: `Notify::notify_one` stores exactly one permit the same way,
    /// so an end-of-turn signal raised while the loop is busy is not lost.
    turn_finished: tokio::sync::Notify,
    /// Cancelled by [`OpenSession::cancel_work`], which every close path
    /// calls before closing the controller. Go's wake loop instead ends when
    /// `Tasks().Updates()` closes; the Rust registry's `watch` sender is
    /// owned by the registry and outlives the controller, so the loop needs
    /// its own stop signal.
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
    /// wait on a provider call, and stops the wake loop. Port of
    /// `openSession.cancelWork`.
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
    /// ponytail: one gate for every id instead of Go's per-id placeholder.
    /// Resuming is an admin-rate path; make it per-id if it ever contends.
    open_gate: tokio::sync::Mutex<()>,
    /// One handle per running wake loop, awaited by [`Server::close`]. Port
    /// of `Server.wakeWG`.
    wake_loops: Mutex<Vec<tokio::task::JoinHandle<()>>>,
    cancel: CancellationToken,
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

    /// Fans `notification` out to every currently open session. Sessions that
    /// have no task registry (sub-agents off) drop it. Returns how many
    /// sessions were open, including those that could not receive it.
    pub(crate) fn notify_open_sessions(&self, notification: Notification) -> usize {
        let sessions = self.all_sessions();
        for session in &sessions {
            session.ctrl.notify(notification.clone());
        }
        sessions.len()
    }

    /// Cancels every in-flight turn, then closes every open controller.
    /// Port of `Server.Close`.
    pub async fn close(&self) -> Result<(), String> {
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
        // Go's `s.wakeWG.Wait()`: every loop was told to stop by
        // `cancel_work` above, so this only waits out an in-flight wake turn.
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
            .route("/v1/sessions/{id}/mcp", get(mcp::list))
            .route("/v1/sandbox/reload", post(sandbox::reload))
            .route("/v1/info", get(info))
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
        self.sessions
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .remove(id)
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
        self.start_wake_loop(&session);
        session
    }

    /// Port of `Server.resumeOrCreate`. A hit in the registry returns the
    /// existing session without calling `open` again.
    async fn resume_or_create(
        self: &Arc<Self>,
        id: &str,
    ) -> Result<(Arc<OpenSession>, bool), String> {
        if id.is_empty() {
            let ctrl = self.factory.create().await?;
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
        let ctrl = self.factory.open(id).await?;
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

    /// Whether any open session has a running turn. Port of
    /// `Server.anyTurnActive`.
    fn any_turn_active(&self) -> bool {
        self.all_sessions().iter().any(|session| {
            session
                .current_turn()
                .is_some_and(|turn| turn.summary().status == turn::TURN_RUNNING)
        })
    }

    // ---- turns ----

    /// Drains `session`'s pending sub-agent notifications for as long as it
    /// is open. Port of `Server.startWakeLoop`.
    ///
    /// It is the sole caller of [`Server::wake_turn`]: both a registry update
    /// signal and the end of any turn on `session` route through this one
    /// task, so every "is a notification pending and no turn active" check
    /// happens one at a time and no two wake turns can start for the same
    /// notification. On every update signal it also diffs the task list into
    /// the task metrics. It does nothing when the runner tracks no tasks.
    ///
    /// Divergence from Go, which starts the wake turn in its own goroutine
    /// and re-checks through `turnFinished`: the turn is awaited here
    /// instead. A notification pushed while it runs bumps the registry's
    /// `watch` version, so the next `changed()` returns at once and the
    /// re-check happens anyway, with one fewer moving part.
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
        // A deleted session's loop has already returned; Go's WaitGroup
        // counter drops on its own, this list does not.
        loops.retain(|running| !running.is_finished());
        loops.push(handle);
    }

    /// Runs one task-triggered turn on `session` when a notification is
    /// pending and nothing else holds the session. Port of the `trigger ==
    /// triggerTask` branch of `Server.startTurn`.
    ///
    /// The wake is prepared before the turn is published, so a no-op or busy
    /// admission cannot leave a phantom turn visible on `GET /v1/sessions`.
    /// A busy session is Go's `errTurnActive`, which its wake loop ignores.
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
                    // A prompt or a close raced us; both are Go's ignored
                    // `errTurnActive` and `ErrClosed` shutdown path.
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

    /// Starts a user turn on `session`. Port of `Server.startTurn`; the
    /// `trigger == triggerTask` branch lives in [`Server::wake_turn`],
    /// because a wake turn needs no HTTP reply and is awaited by the wake
    /// loop that admitted it.
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
            // Go's `os.turnFinished <- struct{}{}`: a notification that
            // landed too late for this turn's own drain is caught by the
            // wake loop's end-of-turn check.
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

/// Go's `errTurnActive`.
const TURN_ACTIVE: &str = "turn already active";

/// 16 random bytes, hex encoded. Go duplicates `cmd/otto`'s `randomID` here
/// for the same reason: the package boundary forbids the import.
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
/// Port of `Server.instrument`, with `requireToken` folded in so a 401 is
/// still logged and measured under its real route, as Go's per-route wrap
/// achieves.
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

/// The metric and log label for one request. Go reads `r.Pattern`, which
/// already carries the method; axum's [`MatchedPath`] carries only the path,
/// and spells the two UI routes differently, so both are mapped back.
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

/// Port of `writeError`. `Content-Type: application/json`, no charset,
/// matching Go byte for byte.
/// Serves `router` on `listener` until `shutdown` fires. Port of
/// `server.Serve`: a cancelled shutdown is a clean exit, so only a listener
/// or protocol failure is an error.
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

/// Port of `writeJSON`.
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

/// Port of `internalError`: the fixed 500 body, with the real (already
/// redacted) error only logged.
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

// ---- handlers ----

#[derive(Debug, Default, Deserialize)]
struct CreateBody {
    #[serde(default)]
    resume: String,
}

async fn create_session(State(server): State<Arc<Server>>, body: Bytes) -> Response {
    // Go tolerates an empty body here (`ContentLength != 0` plus the io.EOF
    // exemption); only malformed JSON is a 400.
    let parsed: CreateBody = if body.iter().all(u8::is_ascii_whitespace) {
        CreateBody::default()
    } else {
        match serde_json::from_slice(&body) {
            Ok(parsed) => parsed,
            Err(_) => return bad_request("invalid JSON body"),
        }
    };

    match server.resume_or_create(&parsed.resume).await {
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

async fn list_sessions(State(server): State<Arc<Server>>) -> Response {
    let disk = match server.factory.list() {
        Some(Ok(result)) => result,
        Some(Err(error)) => return internal_error(&server.log, &error),
        None => ListResult::default(),
    };

    let open: HashMap<String, Arc<OpenSession>> = server
        .sessions
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .clone();

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
        // An empty Vec serializes as "[]", which is what Go's nil guard
        // achieves.
        Some(session) => json_response::<Vec<Message>>(StatusCode::OK, &session.ctrl.history()),
        None => not_found("session not found"),
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
    // Unlike create and rename, Go decodes unconditionally here, so an empty
    // body is a 400.
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
/// reader uses. Port of `waitDone`.
async fn wait_done(turn: &Arc<Turn>) {
    let mut changed = turn.subscribe();
    while !turn.is_done() {
        if changed.changed().await.is_err() {
            return;
        }
    }
}

/// The turn named by the path, or the 404 message to answer with. Port of
/// the repeated `t == nil || t.id != r.PathValue("turn_id")` guard.
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

/// Decrements the stream-client gauge when the response body is dropped,
/// which is what Go's `defer s.metrics.streamClients(-1)` does when the
/// handler returns.
struct StreamClient(Arc<Metrics>);

impl Drop for StreamClient {
    fn drop(&mut self) {
        self.0.stream_clients(-1);
    }
}

/// Writes turn events from sequence `after` onward as SSE frames, then waits
/// for more until the turn finishes or the client disconnects. Disconnecting
/// never cancels the turn. Port of `Server.streamSSE`.
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

#[cfg(test)]
mod tests {
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
    use otto_core::session::{CURRENT_VERSION, Header, ListResult};
    use otto_core::wire::sse::{Frame, parse_frames};
    use serde_json::Value;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;
    use tempfile::TempDir;
    use tokio::sync::watch;
    use tower::ServiceExt;

    // ---- the provider double ----

    /// What one turn does. Go injects a `runnerFunc` implementing
    /// `app.Runner`; `Runner` is a concrete struct here, so the script sits
    /// one layer down, at the provider.
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
        /// Runs at the top of every call with the 1-based call index, the way
        /// Go's fake runner body does. The wake tests push a notification
        /// from it.
        on_call: Option<Box<dyn Fn(usize) + Send + Sync>>,
    }

    struct ScriptedProvider {
        script: Script,
        started: watch::Sender<usize>,
        /// The role of each call's last request message, in call order. A
        /// prompt turn ends in [`Role::User`]; a wake turn ends in the
        /// delivered notification's [`Role::Context`]. Go asserts on the
        /// `Prompt` text instead, which a provider-level double never sees.
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
        /// Every controller shares this registry, the way Go's wake tests
        /// share one `agent.Tasks`. `None` gives each its own.
        tasks: Option<Arc<crate::subagent::tasks::Tasks>>,
        list: Option<ListResult>,
        create_calls: AtomicUsize,
        open_calls: AtomicUsize,
    }

    impl TestFactory {
        fn controller(&self, id: &str) -> Controller {
            let session = SharedSession::memory(Header {
                version: CURRENT_VERSION,
                id: id.to_string(),
                workspace: self.builder.workspace_path.clone(),
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
                    context_window: 128_000,
                    sandbox: self.builder.sandbox_info,
                },
            )
        }
    }

    #[async_trait::async_trait]
    impl Factory for TestFactory {
        async fn create(&self) -> Result<Controller, String> {
            self.create_calls.fetch_add(1, Ordering::SeqCst);
            if let Some(error) = &self.create_error {
                return Err(error.clone());
            }
            let id = self
                .fixed_id
                .clone()
                .unwrap_or_else(|| new_id().expect("id"));
            Ok(self.controller(&id))
        }

        async fn open(&self, id: &str) -> Result<Controller, String> {
            self.open_calls.fetch_add(1, Ordering::SeqCst);
            if let Some(gate) = &self.open_gate {
                gate.cancelled().await;
            }
            if let Some(error) = &self.open_error {
                return Err(error.clone());
            }
            Ok(self.controller(id))
        }

        fn list(&self) -> Option<Result<ListResult, String>> {
            self.list.clone().map(Ok)
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
        tasks: Option<Arc<crate::subagent::tasks::Tasks>>,
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
            let workspace = tempfile::tempdir().expect("workspace");
            let sessions = tempfile::tempdir().expect("sessions");
            let builder = Arc::new(testutil::builder(workspace.path(), sessions.path()));
            let provider = ScriptedProvider::new(options.script);
            let factory = Arc::new(TestFactory {
                builder,
                provider: Arc::clone(&provider),
                fixed_id: options.fixed_id,
                create_error: options.create_error,
                open_error: options.open_error,
                open_gate: options.open_gate,
                list: options.list,
                tasks: options.tasks,
                create_calls: AtomicUsize::new(0),
                open_calls: AtomicUsize::new(0),
            });
            let log = Arc::new(Mutex::new(Vec::new()));
            let server = Server::new(Options {
                factory: Arc::clone(&factory) as Arc<dyn Factory>,
                info: options.info,
                token: options.token,
                logger: Some(Arc::new(Logger::new(Box::new(SharedSink(Arc::clone(
                    &log,
                )))))),
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

        /// Polls the turn until it leaves `running`. Port of `waitTurnDone`.
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

    /// Every API path the router serves.
    ///
    /// ponytail: axum exposes no route table, so this list is written out
    /// once and checked against both the router and `openapi.yaml`. Go reads
    /// `http.ServeMux`'s patterns instead.
    const ROUTES: &[&str] = &[
        "/v1/sessions",
        "/v1/sessions/{id}",
        "/v1/sessions/{id}/history",
        "/v1/sessions/{id}/approvals/{approval_id}",
        "/v1/sessions/{id}/turns",
        "/v1/sessions/{id}/turns/{turn_id}",
        "/v1/sessions/{id}/turns/{turn_id}/events",
        "/v1/sessions/{id}/turns/{turn_id}/cancel",
        "/v1/sessions/{id}/compact",
        "/v1/sessions/{id}/tasks",
        "/v1/sessions/{id}/tasks/{task_id}",
        "/v1/sessions/{id}/tasks/{task_id}/cancel",
        "/v1/sessions/{id}/mcp",
        "/v1/sandbox/reload",
        "/v1/info",
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
                .replace("{approval_id}", "nope");
            // An unmatched path logs the route label "unmatched"; a matched
            // one logs its own pattern. That is the reachability check.
            let before = harness.logged().len();
            harness.send("GET", &probe, None).await;
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

    /// The routes read the runner's real registry. Port of
    /// `TestTaskRoutes`'s list and detail assertions in
    /// `internal/server/tasks_test.go`, driven through the registry rather
    /// than through a scripted `agent` tool call.
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

    // ---- wake turns ----

    /// A harness whose every controller shares `tasks`, plus the registry.
    /// Port of `newTaskTestController`'s shared `agent.Tasks`.
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

    /// Port of `TestWakeTurnStartsOnPendingNotificationWhenIdle`.
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

    /// Port of `TestWakeTurnSkippedWhileUserTurnActive`.
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

    /// Port of `TestWakeTurnFollowsUserTurnFinishingWithPendingNotification`.
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

    /// Port of `TestServerCloseEndsWakeLoop`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn closing_the_server_ends_the_wake_loop() {
        let (harness, _tasks) = wake_harness(Script::default());
        harness.create().await;

        tokio::time::timeout(Duration::from_secs(2), harness.server.close())
            .await
            .expect("close did not return; the wake loop leaked")
            .expect("close");
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
