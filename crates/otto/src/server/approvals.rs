//! `POST /v1/sessions/{id}/approvals/{approval_id}`: the decision for the
//! elevated command the session's running turn waits on.

use std::collections::VecDeque;
use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::Response;
use serde::{Deserialize, Serialize};

use super::{Server, bad_request, error_response, json_response, not_found};
use crate::app::{ApprovalDecision, Channel};

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
    if session
        .ctrl
        .approval_service()
        .get(&approval_id, &session.ctrl.info().workspace, Some(&id))
        .is_ok()
    {
        return match session.ctrl.decide_shared_approval(
            &approval_id,
            decision == ApprovalDecision::Allow,
            "",
            "http",
        ) {
            Ok(request) => {
                server.bump_status();
                json_response(StatusCode::OK, &request)
            }
            Err(error) => {
                error_response(StatusCode::CONFLICT, "approval_failed", &error.to_string())
            }
        };
    }
    {
        let mut state = session.lock();
        if state
            .waiting
            .as_ref()
            .is_some_and(|waiting| waiting.id == approval_id)
        {
            if decision == ApprovalDecision::Allow
                && let Err(message) = session.ctrl.reserve_approval(&approval_id)
            {
                return error_response(StatusCode::CONFLICT, "approval_failed", &message);
            }
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

/// Separate from task-turn admission: only the controller's restricted tools
/// are available, and no original-session writer is opened.
pub async fn message(
    State(server): State<Arc<Server>>,
    Path(id): Path<String>,
    body: Bytes,
) -> Response {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Input {
        text: String,
        #[serde(default)]
        channel: Option<Channel>,
    }
    let input: Input = match serde_json::from_slice(&body) {
        Ok(input) => input,
        Err(_) => return bad_request("expected text"),
    };
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
        .approval_message_with_channel(&input.text, input.channel, &cancel)
        .await
    {
        Ok(reply) => json_response(StatusCode::OK, &reply),
        Err(message) => error_response(StatusCode::CONFLICT, "approval_failed", &message),
    }
}

#[derive(Deserialize)]
pub struct Query {
    pub session_id: String,
    #[serde(default)]
    pub after: i64,
}

pub async fn list_shared(
    State(server): State<Arc<Server>>,
    axum::extract::Query(query): axum::extract::Query<Query>,
) -> Response {
    let Some(ctrl) = server.controller_for_route(&query.session_id).await else {
        return not_found("session not found");
    };
    match ctrl.approval_snapshot() {
        Ok(snapshot) => json_response(StatusCode::OK, &snapshot),
        Err(error) => error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "approval_unavailable",
            &error.to_string(),
        ),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SharedDecision {
    pub session_id: String,
    pub revision: u64,
    pub option_id: String,
    #[serde(default)]
    pub feedback: String,
}

pub async fn decide_shared(
    State(server): State<Arc<Server>>,
    Path(id): Path<String>,
    axum::Json(input): axum::Json<SharedDecision>,
) -> Response {
    let Some(ctrl) = server.controller_for_route(&input.session_id).await else {
        return not_found("session not found");
    };
    let allow = match input.option_id.as_str() {
        "approve" => true,
        "reject" => false,
        _ => return bad_request("unknown approval option"),
    };
    let scope = ctrl.approval_scope();
    let result = ctrl
        .approval_service()
        .get(&id, &scope.workspace, Some(&scope.session_id))
        .and_then(|r| {
            ctrl.approval_service().decide(
                &id,
                &r.scope,
                input.revision,
                allow,
                &input.feedback,
                &crate::approval::Actor::local("http"),
            )
        });
    match result {
        Ok(request) => match ctrl.approval_service().finish_decision(request).await {
            Ok(request) => {
                server.bump_status();
                json_response(StatusCode::OK, &request)
            }
            Err(error) => error_response(
                StatusCode::SERVICE_UNAVAILABLE,
                "approval_unavailable",
                &error.to_string(),
            ),
        },
        Err(error) => error_response(
            StatusCode::CONFLICT,
            "approval_conflict",
            &error.to_string(),
        ),
    }
}

pub async fn get_shared(
    State(server): State<Arc<Server>>,
    Path(id): Path<String>,
    axum::extract::Query(query): axum::extract::Query<Query>,
) -> Response {
    let Some(ctrl) = server.controller_for_route(&query.session_id).await else {
        return not_found("session not found");
    };
    let scope = ctrl.approval_scope();
    match ctrl
        .approval_service()
        .get(&id, &scope.workspace, Some(&scope.session_id))
    {
        Ok(request) => json_response(StatusCode::OK, &request),
        Err(error) => error_response(
            StatusCode::NOT_FOUND,
            "approval_not_found",
            &error.to_string(),
        ),
    }
}

pub async fn withdraw_shared(
    State(server): State<Arc<Server>>,
    Path(id): Path<String>,
    axum::Json(input): axum::Json<SharedDecision>,
) -> Response {
    let Some(ctrl) = server.controller_for_route(&input.session_id).await else {
        return not_found("session not found");
    };
    let scope = ctrl.approval_scope();
    let result = ctrl
        .approval_service()
        .get(&id, &scope.workspace, Some(&scope.session_id))
        .and_then(|r| {
            ctrl.approval_service()
                .withdraw(&id, &r.scope, input.revision)
        });
    match result {
        Ok(request) => json_response(StatusCode::OK, &request),
        Err(error) => error_response(
            StatusCode::CONFLICT,
            "approval_conflict",
            &error.to_string(),
        ),
    }
}

pub async fn events_shared(
    State(server): State<Arc<Server>>,
    axum::extract::Query(query): axum::extract::Query<Query>,
) -> Response {
    use axum::response::{
        IntoResponse,
        sse::{Event, KeepAlive, Sse},
    };
    let Some(ctrl) = server.controller_for_route(&query.session_id).await else {
        return not_found("session not found");
    };
    if query.after < 0 {
        return bad_request("negative event cursor");
    }
    let scope = ctrl.approval_scope();
    let service = ctrl.approval_service().clone();
    let stream = futures_util::stream::unfold(
        (service, scope, query.after, VecDeque::new()),
        |(service, scope, mut cursor, mut buffer)| async move {
            loop {
                if let Some(change) = buffer.pop_front() {
                    let change: crate::approval::Change = change;
                    cursor = change.sequence;
                    let event = Event::default()
                        .event("approval_changed")
                        .id(cursor.to_string())
                        .json_data(&change)
                        .ok()?;
                    return Some((
                        Ok::<_, std::convert::Infallible>(event),
                        (service, scope, cursor, buffer),
                    ));
                }
                match service.events(&scope.workspace, Some(&scope.session_id), cursor) {
                    Ok(changes) => buffer.extend(changes),
                    Err(_) => return None,
                }
                if buffer.is_empty() {
                    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                }
            }
        },
    );
    Sse::new(stream)
        .keep_alive(KeepAlive::default())
        .into_response()
}

pub async fn changes_shared(
    State(server): State<Arc<Server>>,
    axum::extract::Query(query): axum::extract::Query<Query>,
) -> Response {
    let Some(ctrl) = server.controller_for_route(&query.session_id).await else {
        return not_found("session not found");
    };
    let scope = ctrl.approval_scope();
    match ctrl
        .approval_service()
        .events(&scope.workspace, Some(&scope.session_id), query.after)
    {
        Ok(changes) => json_response(StatusCode::OK, &changes),
        Err(error) => error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "approval_unavailable",
            &error.to_string(),
        ),
    }
}

/// `GET`/`PUT /v1/sessions/{id}/approval-mode`. A `PUT` accepts the older
/// `{"automatic":bool}` and the mode form `{"mode":"manual"|"auto"|"yolo"}`;
/// both responses carry `{"automatic":bool,"mode":...}`.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ModeBody {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    automatic: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    mode: Option<crate::approval::ApprovalMode>,
}

impl ModeBody {
    fn of(mode: crate::approval::ApprovalMode) -> Self {
        Self {
            automatic: Some(mode.is_automatic()),
            mode: Some(mode),
        }
    }

    /// The mode this body selects: the explicit mode, the boolean's meaning
    /// (MANUAL or AUTO), or nothing when a read leaves both out.
    fn selected(&self) -> Option<crate::approval::ApprovalMode> {
        match (self.mode, self.automatic) {
            (Some(mode), Some(automatic)) if mode.is_automatic() != automatic => None,
            (Some(mode), _) => Some(mode),
            (None, Some(true)) => Some(crate::approval::ApprovalMode::Auto),
            (None, Some(false)) => Some(crate::approval::ApprovalMode::Manual),
            (None, None) => None,
        }
    }

    /// Whether `automatic` and `mode` contradict each other.
    fn contradicts(&self) -> bool {
        matches!((self.mode, self.automatic), (Some(mode), Some(automatic))
            if mode.is_automatic() != automatic)
    }
}

pub async fn mode(State(server): State<Arc<Server>>, Path(id): Path<String>) -> Response {
    let Some(session) = server.lookup(&id) else {
        return not_found("session not found");
    };
    match session
        .ctrl
        .approval_service()
        .mode(&session.ctrl.approval_scope())
    {
        Ok(mode) => json_response(StatusCode::OK, &ModeBody::of(mode)),
        Err(e) => error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "approval_mode_failed",
            &e.to_string(),
        ),
    }
}

pub async fn set_mode(
    State(server): State<Arc<Server>>,
    Path(id): Path<String>,
    body: Bytes,
) -> Response {
    let Some(session) = server.lookup(&id) else {
        return not_found("session not found");
    };
    let Ok(input) = serde_json::from_slice::<ModeBody>(&body) else {
        return bad_request("expected mode or automatic");
    };
    if input.contradicts() {
        return bad_request("automatic contradicts mode");
    }
    let Some(mode) = input.selected() else {
        return bad_request("expected mode or automatic");
    };
    match session
        .ctrl
        .approval_service()
        .set_mode(&session.ctrl.approval_scope(), mode)
    {
        Ok(()) => {
            server.bump_status();
            json_response(StatusCode::OK, &ModeBody::of(mode))
        }
        Err(e) => error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "approval_mode_failed",
            &e.to_string(),
        ),
    }
}

#[derive(Deserialize)]
pub(super) struct EventQuery {
    #[serde(default)]
    after: i64,
}
pub(super) async fn session_events(
    State(server): State<Arc<Server>>,
    Path(id): Path<String>,
    axum::extract::Query(query): axum::extract::Query<EventQuery>,
) -> Response {
    if query.after < 0 {
        return bad_request("negative event cursor");
    }
    let Some(session) = server.lookup(&id) else {
        return not_found("session not found");
    };
    let scope = session.ctrl.approval_scope();
    match session.ctrl.approval_service().events(
        &scope.workspace,
        Some(&scope.session_id),
        query.after,
    ) {
        Ok(changes) => json_response(StatusCode::OK, &changes),
        Err(e) => error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "approval_events_failed",
            &e.to_string(),
        ),
    }
}
