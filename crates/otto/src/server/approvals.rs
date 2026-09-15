//! One-shot elevated Bash approval.

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::Response;
use serde::Serialize;

use super::{Server, error_response, json_response, not_found};

#[derive(Serialize)]
struct ApprovalResponse {
    prompt: String,
}

pub async fn approve(
    State(server): State<Arc<Server>>,
    Path((id, approval_id)): Path<(String, String)>,
) -> Response {
    let Some(session) = server.lookup(&id) else {
        return not_found("session not found");
    };
    match session.ctrl.approve_bash(&approval_id) {
        Ok(prompt) => json_response(StatusCode::OK, &ApprovalResponse { prompt }),
        Err(message) => error_response(StatusCode::CONFLICT, "approval_failed", &message),
    }
}
