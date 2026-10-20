//! Error and JSON response helpers and the listener serve loop.

use super::*;

// ---- error and JSON helpers ----

#[derive(Debug, Serialize)]
pub(super) struct ApiError {
    code: String,
    message: String,
}

#[derive(Debug, Serialize)]
pub(super) struct ErrorBody {
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
pub(super) fn internal_error(log: &Logger, error: &str) -> Response {
    log.error("internal_error", &[("error", error.to_string())]);
    error_response(
        StatusCode::INTERNAL_SERVER_ERROR,
        "internal",
        "internal error",
    )
}

pub(super) fn not_found(message: &str) -> Response {
    error_response(StatusCode::NOT_FOUND, "not_found", message)
}

pub(super) fn bad_request(message: &str) -> Response {
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

pub(super) fn turn_active(message: &str) -> Response {
    error_response(StatusCode::CONFLICT, "turn_active", message)
}
