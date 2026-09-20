//! The session timer routes.
//!
//! The wire record is [`crate::tool::remind::StoredReminder`], the same shape
//! the session's `.reminders.json` sidecar stores. A runner without a timer
//! registry (`--no-session` child runners, and any runner built without the
//! timer tools) lists nothing and answers 404 on cancel, the way the task
//! routes treat a runner that tracks no tasks.

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::Response;
use serde::Serialize;

use super::{Server, json_response, not_found};
use crate::tool::remind::StoredReminder;

#[derive(Debug, Serialize)]
struct TimerListResponse {
    timers: Vec<StoredReminder>,
}

pub async fn list(State(server): State<Arc<Server>>, Path(id): Path<String>) -> Response {
    let Some(session) = server.lookup(&id) else {
        return not_found("session not found");
    };
    let timers = match session.ctrl.reminders() {
        Some(reminders) => reminders.list(),
        None => Vec::new(),
    };
    json_response(StatusCode::OK, &TimerListResponse { timers })
}

pub async fn cancel(
    State(server): State<Arc<Server>>,
    Path((id, timer_id)): Path<(String, String)>,
) -> Response {
    let Some(session) = server.lookup(&id) else {
        return not_found("session not found");
    };
    let Some(reminders) = session.ctrl.reminders() else {
        return not_found("timer not found");
    };
    match reminders.cancel(&timer_id) {
        Ok(timer) => json_response(StatusCode::OK, &timer),
        Err(_) => not_found("timer not found"),
    }
}
