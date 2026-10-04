//! HTTP adapters for transient active-task guidance and follow-up routing.

use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::Response;
use serde::{Deserialize, Serialize};

use super::{Server, bad_request, error_response, json_response, not_found};
use crate::app::Channel;

const MAX_TEXT_BYTES: usize = 32_000;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TextInput {
    text: String,
}

#[derive(Serialize)]
struct Accepted {
    accepted: bool,
}

/// `POST /v1/sessions/{id}/tasks/steer`.
pub async fn steer(
    State(server): State<Arc<Server>>,
    Path(id): Path<String>,
    body: Bytes,
) -> Response {
    let input: TextInput = match serde_json::from_slice(&body) {
        Ok(input) => input,
        Err(_) => return bad_request("expected text"),
    };
    if let Some(message) = invalid_text(&input.text) {
        return bad_request(message);
    }
    let Some(session) = server.lookup(&id) else {
        return not_found("session not found");
    };
    match session.ctrl.steer(&input.text) {
        Ok(()) => json_response(StatusCode::OK, &Accepted { accepted: true }),
        Err(message) => error_response(StatusCode::CONFLICT, "task_steer_failed", &message),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ControlInput {
    text: String,
    #[serde(default)]
    channel: Option<Channel>,
}

/// `POST /v1/sessions/{id}/tasks/control`.
pub async fn control(
    State(server): State<Arc<Server>>,
    Path(id): Path<String>,
    body: Bytes,
) -> Response {
    let input: ControlInput = match serde_json::from_slice(&body) {
        Ok(input) => input,
        Err(_) => return bad_request("expected text and an optional channel"),
    };
    if let Some(message) = invalid_text(&input.text) {
        return bad_request(message);
    }
    let Some(session) = server.lookup(&id) else {
        return not_found("session not found");
    };
    let cancel = session
        .lock()
        .turn
        .as_ref()
        .map_or_else(|| session.closed.clone(), |turn| turn.cancel_token());
    match session
        .ctrl
        .control_message_with_channel(&input.text, input.channel, &cancel)
        .await
    {
        Ok(reply) => json_response(StatusCode::OK, &reply),
        Err(message) => error_response(StatusCode::CONFLICT, "task_control_failed", &message),
    }
}

fn invalid_text(text: &str) -> Option<&'static str> {
    if text.trim().is_empty() {
        Some("text must contain non-whitespace characters")
    } else if text.len() > MAX_TEXT_BYTES {
        Some("text must be at most 32000 UTF-8 bytes")
    } else {
        None
    }
}
