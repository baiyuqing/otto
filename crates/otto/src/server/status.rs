//! Status snapshot, health/info/usage/metrics routes and the status SSE streams.

use super::*;

impl Server {
    /// Whether any open session has a running turn.
    pub(super) fn any_turn_active(&self) -> bool {
        self.all_sessions().iter().any(|session| {
            session
                .current_turn()
                .is_some_and(|turn| turn.summary().status == turn::TURN_RUNNING)
        })
    }

    /// The full `GET /v1/status` payload: every open session in this
    /// process, sorted by workspace then id.
    pub(super) fn status_snapshot(&self) -> StatusSnapshotWire {
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
}

#[derive(Debug, Clone, Serialize)]
pub(super) struct HealthzWire {
    status: String,
    sessions_open: usize,
}

/// One row of `GET /v1/status`. `turn` is `null` until the session's first
/// turn starts.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub(super) struct StatusSessionWire {
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
pub(super) struct StatusSnapshotWire {
    sessions: Vec<StatusSessionWire>,
}

pub(super) async fn info(State(server): State<Arc<Server>>) -> Response {
    json_response(StatusCode::OK, &server.info)
}

pub(super) async fn status(State(server): State<Arc<Server>>) -> Response {
    stream_status(server)
}

#[derive(Debug, Default, Deserialize)]
pub(super) struct UsageFilter {
    session_id: Option<String>,
}

pub(super) async fn usage(
    State(server): State<Arc<Server>>,
    Query(filter): Query<UsageFilter>,
) -> Response {
    match server.factory.usage_summary(filter.session_id.as_deref()) {
        Ok(summary) => json_response(StatusCode::OK, &summary),
        Err(error) => internal_error(&server.log, &error),
    }
}

#[derive(Debug, Default, Deserialize)]
pub(super) struct DailyUsageFilter {
    days: Option<i64>,
    session_id: Option<String>,
}

pub(super) async fn daily_usage(
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

pub(super) async fn openapi() -> Response {
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/yaml")
        .body(Body::from(OPENAPI_YAML))
        .expect("static header values")
}

pub(super) async fn healthz(State(server): State<Arc<Server>>) -> Response {
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

pub(super) async fn prometheus(State(server): State<Arc<Server>>) -> Response {
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
pub(super) struct StreamClient(Arc<Metrics>);

impl Drop for StreamClient {
    fn drop(&mut self) {
        self.0.stream_clients(-1);
    }
}

/// Writes turn events from sequence `after` onward as SSE frames, then waits
/// for more until the turn finishes or the client disconnects. Disconnecting
/// never cancels the turn.
pub(super) fn stream_sse(metrics: &Arc<Metrics>, turn: Arc<Turn>, after: usize) -> Response {
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
                        let event_json = serde_json::to_string(event).unwrap_or_default();
                        chunk.push_str(&format_frame(
                            (after + offset) as i64,
                            &event.event_type,
                            &event_json,
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
pub(super) fn stream_status(server: Arc<Server>) -> Response {
    let changed = server.status_changed.subscribe();
    let stream = futures_util::stream::unfold(
        (server, changed, None::<StatusSnapshotWire>),
        |(server, mut changed, last)| async move {
            loop {
                let snapshot = server.status_snapshot();
                if last.as_ref() != Some(&snapshot) {
                    let snapshot_json = serde_json::to_string(&snapshot).unwrap_or_default();
                    let frame = format!("event: status\ndata: {snapshot_json}\n\n");
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
