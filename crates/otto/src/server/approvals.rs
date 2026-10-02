//! `POST /v1/sessions/{id}/approvals/{approval_id}`: the decision for the
//! elevated command the session's running turn waits on.

use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::Response;
use serde::{Deserialize, Serialize};

use super::{Server, bad_request, error_response, json_response, not_found};
use crate::app::ApprovalDecision;

#[derive(Deserialize)]
struct DecisionBody {
    #[serde(default)]
    decision: String,
}

#[derive(Serialize)]
struct DecisionResponse {
    decision: &'static str,
}

pub async fn decide(
    State(server): State<Arc<Server>>,
    Path((id, approval_id)): Path<(String, String)>,
    body: Bytes,
) -> Response {
    let Some(session) = server.lookup(&id) else {
        return not_found("session not found");
    };
    let (decision, label) = match serde_json::from_slice::<DecisionBody>(&body) {
        Ok(parsed) if parsed.decision == "allow" => (ApprovalDecision::Allow, "allow"),
        Ok(parsed) if parsed.decision == "deny" => (ApprovalDecision::Deny, "deny"),
        _ => return bad_request("decision must be \"allow\" or \"deny\""),
    };
    {
        let mut state = session.lock();
        if state
            .waiting
            .as_ref()
            .is_some_and(|waiting| waiting.id == approval_id)
        {
            let waiting = state.waiting.take().expect("checked above");
            state.remember_decided(&approval_id);
            // The receiver is gone only when the turn is ending; the decision
            // is moot then.
            let _ = waiting.decide.send(decision);
        } else if state.decided.iter().any(|known| *known == approval_id) {
            return error_response(
                StatusCode::CONFLICT,
                "approval_decided",
                "the approval was already decided",
            );
        } else {
            return error_response(
                StatusCode::CONFLICT,
                "approval_failed",
                "no such approval is waiting",
            );
        }
    }
    server.bump_status();
    json_response(StatusCode::OK, &DecisionResponse { decision: label })
}
