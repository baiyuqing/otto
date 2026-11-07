//! HTTP adapters for transient active-task guidance and follow-up routing.

use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::Response;
use serde::Deserialize;

use super::{Server, bad_request, error_response, json_response, not_found};
use otto::app::Channel;

const MAX_TEXT_BYTES: usize = 32_000;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TextInput {
    #[serde(default)]
    client_source: Option<otto_core::model::InputSource>,
    text: String,
    /// The client's stable id for this input, so a retry is admitted once.
    #[serde(default)]
    input_id: Option<String>,
}

impl TextInput {
    /// The `400` answer when `input_id` is present and malformed.
    fn invalid_input_id(&self) -> Option<Response> {
        self.input_id
            .as_deref()
            .filter(|id| !otto_core::model::is_input_id(id))
            .map(|_| bad_request(super::INVALID_INPUT_ID))
    }
}

/// `POST /v1/sessions/{id}/inputs` deterministically admits ordinary human
/// input to the active run. It never invokes the model routing dialogue and
/// never creates a queued turn.
pub async fn admit_input(
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
    if let Some(response) = input.invalid_input_id() {
        return response;
    }
    let input_id = input.input_id.as_deref();
    match session.ctrl.admit_active_input_with_provenance(
        &input.text,
        input_id,
        Some(otto_core::model::InputProvenance::remote(
            otto_core::model::InputTransport::Http,
            input.client_source,
        )),
    ) {
        Ok(disposition) => json_response(StatusCode::OK, &disposition),
        Err(message) => input_error(&message, "input_admission_failed"),
    }
}

/// `GET /v1/sessions/{id}/tasks/steer`: current transient lifecycle items.
pub async fn steering_messages(
    State(server): State<Arc<Server>>,
    Path(id): Path<String>,
) -> Response {
    let Some(session) = server.lookup(&id) else {
        return not_found("session not found");
    };
    match session.ctrl.steering_messages() {
        Ok(messages) => json_response(StatusCode::OK, &messages),
        Err(message) => error_response(StatusCode::CONFLICT, "task_steer_failed", &message),
    }
}

/// `DELETE /v1/sessions/{id}/tasks/steer/{message_id}` withdraws one
/// undelivered steer. Delivered messages are append-only session history.
pub async fn withdraw_steering(
    State(server): State<Arc<Server>>,
    Path((id, message_id)): Path<(String, u64)>,
) -> Response {
    let Some(session) = server.lookup(&id) else {
        return not_found("session not found");
    };
    match session.ctrl.withdraw_steering(message_id) {
        Ok(message) => json_response(StatusCode::OK, &message),
        Err(message) => {
            error_response(StatusCode::CONFLICT, "task_steer_withdraw_failed", &message)
        }
    }
}

/// `POST /v1/sessions/{id}/tasks/steer/interrupt` stops the running step and
/// delivers the queued guidance inside the current turn. No body is read.
pub async fn interrupt_steering(
    State(server): State<Arc<Server>>,
    Path(id): Path<String>,
) -> Response {
    let Some(session) = server.lookup(&id) else {
        return not_found("session not found");
    };
    match session.ctrl.interrupt_step() {
        Ok(queued_guidance) => json_response(
            StatusCode::OK,
            &serde_json::json!({ "queued_guidance": queued_guidance as u32 }),
        ),
        Err(refusal) => error_response(StatusCode::CONFLICT, refusal.code(), &refusal.message()),
    }
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
    if let Some(response) = input.invalid_input_id() {
        return response;
    }
    let input_id = input.input_id.as_deref();
    match session.ctrl.steer_input_with_provenance(
        &input.text,
        input_id,
        Some(otto_core::model::InputProvenance::remote(
            otto_core::model::InputTransport::Http,
            input.client_source,
        )),
    ) {
        Ok(message) => json_response(StatusCode::OK, &message),
        Err(message) => input_error(&message, "task_steer_failed"),
    }
}

/// `503 input_not_persisted` when the input's durable copy could not be
/// saved, `409 duplicate_input` for a retry of a delivered input, otherwise
/// `409` with `code`.
fn input_error(message: &str, code: &str) -> Response {
    if message == otto::app::task_control::INPUT_NOT_PERSISTED {
        error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "input_not_persisted",
            message,
        )
    } else if message == otto::app::task_control::DUPLICATE_INPUT {
        error_response(StatusCode::CONFLICT, "duplicate_input", message)
    } else {
        error_response(StatusCode::CONFLICT, code, message)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unsaved_input_answers_503_and_other_failures_409() {
        let unsaved = input_error(
            otto::app::task_control::INPUT_NOT_PERSISTED,
            "input_admission_failed",
        );
        assert_eq!(unsaved.status(), StatusCode::SERVICE_UNAVAILABLE);
        let other = input_error("the active task changed", "input_admission_failed");
        assert_eq!(other.status(), StatusCode::CONFLICT);
    }
}
