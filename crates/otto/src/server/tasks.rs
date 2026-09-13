//! The sub-agent task routes.
//!
//! Port of `internal/server/tasks.go`. The wire record itself lives in
//! [`crate::app::tasks::Task`], whose field order matches Go's `taskWire`.
//!
//! Until phase 7 lands the registry, [`crate::app::tasks::task_view`] answers
//! `None` for every runner, so `GET .../tasks` returns an empty list and the
//! two per-task routes answer 404 — exactly what Go does for a runner that
//! tracks no tasks.

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::Response;
use otto_core::model::Message;
use serde::Serialize;

use super::{Server, error_response, json_response, not_found};
use crate::app::tasks::{TASK_FINISHED, Task};

#[derive(Debug, Serialize)]
struct TaskListResponse {
    tasks: Vec<Task>,
}

#[derive(Debug, Serialize)]
struct TaskDetail {
    #[serde(flatten)]
    task: Task,
    history: Vec<Message>,
}

pub async fn list(State(server): State<Arc<Server>>, Path(id): Path<String>) -> Response {
    let Some(session) = server.lookup(&id) else {
        return not_found("session not found");
    };
    let tasks = match session.ctrl.tasks() {
        Some(tasks) => tasks.list(),
        None => Vec::new(),
    };
    json_response(StatusCode::OK, &TaskListResponse { tasks })
}

pub async fn get(
    State(server): State<Arc<Server>>,
    Path((id, task_id)): Path<(String, String)>,
) -> Response {
    let Some(session) = server.lookup(&id) else {
        return not_found("session not found");
    };
    let Some(tasks) = session.ctrl.tasks() else {
        return not_found("task not found");
    };
    let Some(task) = tasks.get(&task_id) else {
        return not_found("task not found");
    };
    json_response(
        StatusCode::OK,
        &TaskDetail {
            task,
            // An empty Vec serializes as "[]", which is what Go's nil guard
            // achieves.
            history: tasks.history(&task_id).unwrap_or_default(),
        },
    )
}

pub async fn cancel(
    State(server): State<Arc<Server>>,
    Path((id, task_id)): Path<(String, String)>,
) -> Response {
    let Some(session) = server.lookup(&id) else {
        return not_found("session not found");
    };
    let Some(tasks) = session.ctrl.tasks() else {
        return not_found("task not found");
    };
    match tasks.cancel(&task_id) {
        Ok(()) => match tasks.get(&task_id) {
            Some(task) => json_response(StatusCode::OK, &task),
            None => not_found("task not found"),
        },
        Err(error) if error == TASK_FINISHED => {
            error_response(StatusCode::CONFLICT, "task_done", "task already finished")
        }
        Err(_) => not_found("task not found"),
    }
}
