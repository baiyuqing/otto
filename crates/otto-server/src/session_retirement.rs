//! Retiring a session from this server: explicit, irreversible deletion and
//! archiving. Both close an idle session first, then change its files only
//! through the native API. The plain `DELETE /v1/sessions/{id}` remains a close.
use super::*;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DeleteBody {
    confirm: String,
}

/// What happens to the closed session's files.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Retirement {
    /// Remove history, sub-agent transcripts, sidecars and lease/queue state.
    Delete,
    /// Move the session file to the workspace archive, as TUI `/archive` does.
    Archive,
}

pub(super) async fn delete_handle(
    State(server): State<Arc<Server>>,
    Path(id): Path<String>,
    body: Bytes,
) -> Response {
    let parsed: DeleteBody = match serde_json::from_slice(&body) {
        Ok(parsed) => parsed,
        Err(_) => return bad_request("JSON body must contain a string confirm field"),
    };
    if parsed.confirm != id {
        return bad_request("confirm must exactly match the session id");
    }
    spawn_retirement(server, id, Retirement::Delete).await
}

pub(super) async fn archive_handle(
    State(server): State<Arc<Server>>,
    Path(id): Path<String>,
) -> Response {
    spawn_retirement(server, id, Retirement::Archive).await
}

async fn spawn_retirement(server: Arc<Server>, id: String, retirement: Retirement) -> Response {
    // Keep admission and cleanup alive even if the HTTP request is dropped.
    let task_server = Arc::clone(&server);
    match tokio::spawn(async move { retire(&task_server, &id, retirement).await }).await {
        Ok(response) => response,
        Err(error) => internal_error(&server.log, &error.to_string()),
    }
}

fn retirement_error(server: &Server, error: &str) -> Response {
    if error == SESSION_NOT_FOUND {
        not_found("session not found")
    } else if error == app::PROMPT_ACTIVE
        || error.contains("lease is held")
        || error.contains("session is held by host")
        || error.contains("lease held")
        || error.contains("session is busy")
        || error.contains("file is already locked by another process")
    {
        error_response(StatusCode::CONFLICT, "session_busy", error)
    } else {
        internal_error(&server.log, error)
    }
}

async fn retire(server: &Arc<Server>, id: &str, retirement: Retirement) -> Response {
    let _gate = server.open_gate.lock().await;
    if server.permanently_unavailable(id) {
        return not_found("session not found");
    }
    let (session, registered) = match server.lookup(id) {
        Some(session) => (session, true),
        None => match server.factory.open(id, None).await {
            Ok(ctrl) => (OpenSession::new(ctrl), false),
            Err(error) => return retirement_error(server, &error),
        },
    };
    let info = session.ctrl.info();
    if info.session_id != id || info.session_path.is_empty() {
        return internal_error(&server.log, "session has no persistent identity");
    }
    // A native admission closes the gap left by an Arc<Controller> previously
    // handed to an attached frontend. Registry state alone cannot guard it.
    let admission = match session.ctrl.begin_operation() {
        Ok(admission) => admission,
        Err(error) => return retirement_error(server, &error),
    };
    {
        let mut state = session.lock();
        if !server.idle_session(&session, &state)
            || session.ctrl.resumes_interrupted_turn()
            || session.ctrl.turn_queue().is_some_and(|queue| {
                queue
                    .pending()
                    .iter()
                    .any(|prompt| !session.ctrl.has_input(&prompt.input_id))
            })
            || session
                .ctrl
                .notification_mailbox()
                .is_some_and(|mailbox| !mailbox.is_empty())
        {
            return error_response(
                StatusCode::CONFLICT,
                "session_busy",
                "session has active work",
            );
        }
        if registered
            && !server
                .sessions
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .get(id)
                .is_some_and(|current| Arc::ptr_eq(current, &session))
        {
            return error_response(
                StatusCode::CONFLICT,
                "session_busy",
                "session is being closed",
            );
        }
        state.begin_close();
        // Neither a resume nor an idempotent create retry may bring this id
        // back in this process while its files change, or after they did.
        server
            .permanent_deletions
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(id.to_owned());
        // No actual work is in flight: only our admission is held. Seal native
        // admission before releasing it, rather than cancelling a busy turn.
        session.ctrl.request_close();
        session.closed.cancel();
    }
    if registered && server.remove_if_same(id, &session) {
        server.metrics.sessions_open(-1);
    }
    server.forget_evicted(id);
    session.ctrl.close_resources().await;
    drop(admission);
    let ctrl = Arc::clone(&session.ctrl);
    let closed = tokio::task::spawn_blocking(move || ctrl.close()).await;
    let response = match closed {
        Ok(Ok(())) => change_files(server, &info, retirement).await,
        Ok(Err(error)) => retirement_error(server, &error),
        Err(error) => internal_error(&server.log, &error.to_string()),
    };
    // An archive is one file move, the last and only committed change, so a
    // failed archive leaves the session intact and it may be resumed again.
    // A failed delete may be partial, so its id stays unavailable.
    if retirement == Retirement::Archive && !response.status().is_success() {
        server
            .permanent_deletions
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(id);
    }
    response
}

/// The target's writer and lease are released. Never synthesize a filesystem
/// path from an HTTP id, and never change files outside the native API.
async fn change_files(server: &Arc<Server>, info: &app::Info, retirement: Retirement) -> Response {
    let operator = match server.factory.create(Some(&info.workspace)).await {
        Ok(ctrl) => ctrl,
        Err(error) => return retirement_error(server, &error),
    };
    let result = match retirement {
        Retirement::Delete => operator
            .delete_session(&info.session_path)
            .await
            .map(|()| None),
        Retirement::Archive => operator
            .archive_session(&info.session_path)
            .await
            .map(|archived| Some(archived.path)),
    };
    operator.close_resources().await;
    let close_result = tokio::task::spawn_blocking(move || operator.close()).await;
    let archived_path = match result {
        Ok(archived_path) => archived_path,
        Err(error) => return retirement_error(server, &error),
    };
    match close_result {
        Ok(Ok(())) => match archived_path {
            Some(path) => json_response(
                StatusCode::OK,
                &serde_json::json!({ "archived_path": path }),
            ),
            None => Response::builder()
                .status(StatusCode::NO_CONTENT)
                .body(Body::empty())
                .expect("static response"),
        },
        Ok(Err(error)) => internal_error(&server.log, &error),
        Err(error) => internal_error(&server.log, &error.to_string()),
    }
}
