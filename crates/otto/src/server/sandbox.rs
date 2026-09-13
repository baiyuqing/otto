//! `POST /v1/sandbox/reload`.
//!
//! Port of `internal/server/sandbox.go`. One sandbox serves every open
//! session, so the reload is refused while any session has a turn in flight:
//! replacing the executor while a `bash` command is using it would otherwise
//! block until that command ends.

use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::Response;

use super::{Server, error_response, json_response, sandbox_wire};

pub async fn reload(State(server): State<Arc<Server>>) -> Response {
    if !server.factory.sandbox_reload_available() {
        return error_response(
            StatusCode::NOT_IMPLEMENTED,
            "not_implemented",
            "sandbox reload is not available",
        );
    }
    if server.any_turn_active() {
        return error_response(
            StatusCode::CONFLICT,
            "turn_active",
            "a turn is active; sandbox reload would replace a running command's sandbox",
        );
    }
    match server.factory.reload_sandbox().await {
        None => error_response(
            StatusCode::NOT_IMPLEMENTED,
            "not_implemented",
            "sandbox reload is not available",
        ),
        Some(Ok(info)) => json_response(StatusCode::OK, &sandbox_wire(&info)),
        Some(Err(error)) => error_response(StatusCode::CONFLICT, "sandbox_reload_failed", &error),
    }
}
