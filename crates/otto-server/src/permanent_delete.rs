//! Explicit, irreversible deletion; the existing DELETE route remains a close.
use super::*;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DeleteBody {
    confirm: String,
}

pub(super) async fn handle(
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
    // Keep admission and cleanup alive even if the HTTP request is dropped.
    let task_server = Arc::clone(&server);
    match tokio::spawn(async move { delete(&task_server, &id).await }).await {
        Ok(response) => response,
        Err(error) => internal_error(&server.log, &error.to_string()),
    }
}

fn delete_error(server: &Server, error: &str) -> Response {
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

async fn delete(server: &Arc<Server>, id: &str) -> Response {
    let _gate = server.open_gate.lock().await;
    if server.permanently_unavailable(id) {
        return not_found("session not found");
    }
    let (session, registered) = match server.lookup(id) {
        Some(session) => (session, true),
        None => match server.factory.open(id, None).await {
            Ok(ctrl) => (OpenSession::new(ctrl), false),
            Err(error) => return delete_error(server, &error),
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
        Err(error) => return delete_error(server, &error),
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
        state.deleting = true;
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
    match closed {
        Ok(Ok(())) => {}
        Ok(Err(error)) => return delete_error(server, &error),
        Err(error) => return internal_error(&server.log, &error.to_string()),
    }
    // The target's writer and lease are released. Never synthesize a filesystem
    // path from an HTTP id, and never remove files outside the native API.
    let deleter = match server.factory.create(Some(&info.workspace)).await {
        Ok(ctrl) => ctrl,
        Err(error) => return delete_error(server, &error),
    };
    let result = deleter.delete_session(&info.session_path).await;
    deleter.close_resources().await;
    let close_result = tokio::task::spawn_blocking(move || deleter.close()).await;
    if let Err(error) = result {
        return delete_error(server, &error);
    }
    match close_result {
        Ok(Ok(())) => Response::builder()
            .status(StatusCode::NO_CONTENT)
            .body(Body::empty())
            .expect("static response"),
        Ok(Err(error)) => internal_error(&server.log, &error),
        Err(error) => internal_error(&server.log, &error.to_string()),
    }
}
