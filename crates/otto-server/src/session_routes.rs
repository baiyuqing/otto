//! Session wire types and the session create/list/fork/rename/history routes.

use super::*;

// ---- wire types ----

#[derive(Debug, Clone, Serialize)]
pub struct SandboxWire {
    pub mode: String,
    pub network: String,
    pub bash_available: bool,
    pub summary: String,
    /// Configured `read_paths` entries below no denied path.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub inert_read_paths: Vec<String>,
    /// Built-in denylist entries that contain a writable root and are not applied.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub skipped_deny_paths: Vec<String>,
}

pub fn sandbox_wire(info: &SandboxInfo) -> SandboxWire {
    SandboxWire {
        mode: info.mode.as_str().to_string(),
        network: info.network.as_str().to_string(),
        bash_available: info.bash_available,
        summary: info.summary().to_string(),
        inert_read_paths: info.inert_read_paths.clone(),
        skipped_deny_paths: info.skipped_deny_paths.clone(),
    }
}

#[derive(Debug, Clone, Serialize)]
pub(super) struct SessionTurnWire {
    id: String,
    trigger: String,
    status: String,
}

#[derive(Debug, Clone, Serialize)]
pub(super) struct SessionWire {
    id: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    name: String,
    workspace: String,
    provider: String,
    provider_type: String,
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
pub(super) struct SessionListRow {
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

pub(super) const LAST_USER_TEXT_CHARS: usize = 80;

#[derive(Debug, Clone, Serialize)]
pub(super) struct SessionListResponse {
    sessions: Vec<SessionListRow>,
}

impl Server {
    pub(super) fn session_wire(&self, session: &OpenSession) -> SessionWire {
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
            provider_type: info.provider_type,
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
}

// ---- handlers ----

#[derive(Debug, Default, Deserialize)]
pub(super) struct CreateBody {
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

pub(super) fn command_session_id(kind: &str, workspace: &str, request_id: &str) -> String {
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

pub(super) fn valid_command_id(id: &str) -> bool {
    otto_core::model::is_input_id(id)
}

pub(super) async fn create_session(State(server): State<Arc<Server>>, body: Bytes) -> Response {
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
        if server.permanently_unavailable(&id) {
            return not_found("session was permanently deleted");
        }
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
            Err(error) => session_configuration_error(&server, &error),
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
        Err(error) => session_configuration_error(&server, &error),
    }
}

/// Only this known, value-free configuration failure is safe to expose.
fn session_configuration_error(server: &Server, error: &str) -> Response {
    if error.starts_with("missing model:") {
        return error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "configuration_required",
            "No model configured. Configure a provider and default model through /v1/config/changes or run otto setup, then restart otto serve.",
        );
    }
    internal_error(&server.log, error)
}

#[derive(Debug, Default, Deserialize)]
pub(super) struct ListSessionsQuery {
    /// Admitted and loaded before use; absent lists every loaded workspace.
    workspace: Option<String>,
}

pub(super) async fn list_sessions(
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

pub(super) async fn get_session(
    State(server): State<Arc<Server>>,
    Path(id): Path<String>,
) -> Response {
    match server.lookup(&id) {
        Some(session) => json_response(StatusCode::OK, &server.session_wire(&session)),
        None => not_found("session not found"),
    }
}

#[derive(Debug, Default, Deserialize)]
pub(super) struct ModelBody {
    /// The `<provider>/<model_id>` selector this session should run on.
    #[serde(default)]
    model: String,
}

/// `POST /v1/sessions/{id}/model`. Rebuilds this session's runner on another
/// model selector and answers with the session wire. It never writes the
/// configuration file, so other sessions and the next `otto serve` start keep
/// the model they had.
pub(super) async fn set_session_model(
    State(server): State<Arc<Server>>,
    Path(id): Path<String>,
    body: Bytes,
) -> Response {
    let Some(session) = server.lookup(&id) else {
        return not_found("session not found");
    };
    let parsed: ModelBody = match serde_json::from_slice(&body) {
        Ok(parsed) => parsed,
        Err(_) => return bad_request("invalid JSON body"),
    };
    if parsed.model.is_empty() {
        return bad_request("model must not be empty");
    }
    if let Err(error) = otto_core::config::ModelSelector::parse(&parsed.model) {
        return bad_request(&format!("model must be <provider>/<model_id>: {error}"));
    }
    match session.ctrl.switch_model_result(&parsed.model).await {
        Ok(_) => {
            server.logger().info(
                "model_switched",
                &[("session_id", id), ("model", parsed.model)],
            );
            json_response(StatusCode::OK, &server.session_wire(&session))
        }
        Err(message) if message == app::PROMPT_ACTIVE => {
            turn_active("a turn is already active for this session")
        }
        // A session without dynamic content, or under a policy that forbids a
        // dynamic model, cannot switch; retrying the same request cannot help.
        Err(message)
            if message == app::MODEL_SWITCH_UNAVAILABLE
                || message == app::SESSION_OPERATION_UNAVAILABLE =>
        {
            error_response(StatusCode::CONFLICT, "model_switch_unavailable", &message)
        }
        // Unknown provider or invalid model content; the builder redacts it.
        Err(message) => bad_request(&message),
    }
}

#[derive(Debug, Default, Deserialize)]
pub(super) struct ForkBody {
    /// The turn-opening user message to stop before; absent forks the whole
    /// history up to the last turn boundary.
    #[serde(default)]
    before: Option<String>,
    #[serde(default)]
    request_id: Option<String>,
}

#[derive(Debug, Serialize)]
pub(super) struct ForkedWire {
    #[serde(flatten)]
    session: SessionWire,
    /// The text of the message the fork stops before, for the editor.
    #[serde(skip_serializing_if = "Option::is_none")]
    prompt: Option<String>,
}

/// `POST /v1/sessions/{id}/fork`: writes a new session from this one's
/// history and opens it. The source session is unchanged.
pub(super) async fn fork_session(
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
        if server.permanently_unavailable(&fork_id) {
            return not_found("session was permanently deleted");
        }
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
pub(super) struct ForkPointWire {
    entry_id: String,
    prompt: String,
}

#[derive(Debug, Serialize)]
pub(super) struct ForkPointsResponse {
    fork_points: Vec<ForkPointWire>,
}

/// `GET /v1/sessions/{id}/fork-points`: the user prompts a fork can stop
/// before, oldest first.
pub(super) async fn fork_points(
    State(server): State<Arc<Server>>,
    Path(id): Path<String>,
) -> Response {
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
pub(super) struct RenameBody {
    #[serde(default)]
    name: String,
}

pub(super) async fn rename_session(
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

pub(super) async fn delete_session(
    State(server): State<Arc<Server>>,
    Path(id): Path<String>,
) -> Response {
    let _gate = server.open_gate.lock().await;
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
pub(super) struct HistoryQuery {
    before_turn: Option<String>,
}

/// The session's messages; with `before_turn`, only those that existed when
/// that turn started, so a reader replaying the turn's events from 0 does not
/// see its prompt and finished steps twice.
pub(super) async fn history(
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

pub(super) async fn context(State(server): State<Arc<Server>>, Path(id): Path<String>) -> Response {
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

pub(super) async fn trace(State(server): State<Arc<Server>>, Path(id): Path<String>) -> Response {
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

pub(super) async fn trace_span(
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
