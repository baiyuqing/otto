//! `POST /v1/sessions/{id}/compact`.
//!
//! Refused with 409 `turn_active` while a turn or another compaction runs on
//! the same session; [`super::Server::start_turn`] refuses turns for the same
//! window.

use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::Response;
use otto_core::agent::AgentError;
use otto_core::wire::events::to_wire_compaction;
use serde::Deserialize;

use super::{Server, bad_request, error_response, json_response, not_found, turn_active};
use crate::app;

#[derive(Debug, Default, Deserialize)]
struct CompactBody {
    #[serde(default)]
    focus: String,
}

pub async fn handle(
    State(server): State<Arc<Server>>,
    Path(id): Path<String>,
    body: Bytes,
) -> Response {
    let Some(session) = server.lookup(&id) else {
        return not_found("session not found");
    };
    let parsed: CompactBody = if body.iter().all(u8::is_ascii_whitespace) {
        CompactBody::default()
    } else {
        match serde_json::from_slice(&body) {
            Ok(parsed) => parsed,
            Err(_) => return bad_request("invalid JSON body"),
        }
    };

    let cancel = {
        let mut state = session.lock();
        let busy = state.turn.as_ref().is_some_and(|turn| !turn.is_done());
        if busy || state.compacting.is_some() {
            return turn_active("a turn is already active for this session");
        }
        let cancel = server.cancel_token().child_token();
        state.compacting = Some(cancel.clone());
        cancel
    };

    // ponytail: axum gives a handler no client-disconnect signal, so only
    // shutdown cancels. axum gives a handler no disconnect signal, so only
    // shutdown cancels. Add a disconnect watcher if a hung compaction after a
    // dropped client is ever observed.
    let result = {
        let mut emit = |_event| {};
        session
            .ctrl
            .compact(&parsed.focus, &mut emit, &cancel)
            .await
    };
    cancel.cancel();
    session.lock().compacting = None;

    match result {
        Ok(compaction) => {
            server.logger().info(
                "compaction_finished",
                &[
                    ("session_id", id),
                    ("noop", compaction.noop.to_string()),
                    ("tokens_before", compaction.tokens_before.to_string()),
                ],
            );
            json_response(StatusCode::OK, &to_wire_compaction(&compaction))
        }
        Err(AgentError::Other(message)) if message == app::PROMPT_ACTIVE => {
            turn_active("a turn is already active for this session")
        }
        // Client gone or server shutting down; nothing to write.
        Err(error) if error.is_cancelled() => Response::new(axum::body::Body::empty()),
        Err(error) => {
            let message = error.to_string();
            server.logger().error(
                "compaction_error",
                &[("session_id", id), ("error", message.clone())],
            );
            error_response(StatusCode::CONFLICT, "compaction_failed", &message)
        }
    }
}
