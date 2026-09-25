//! Cross-process sub-agent task routes: `GET /v1/tasks` and `GET
//! /v1/tasks/{parent_session}/{task_id}`. Unlike `tasks.rs`'s per-session
//! routes, which read one open [`crate::app::Controller`]'s in-memory
//! registry, these read `tasks.db` directly through
//! [`crate::subagent::record`] and answer for tasks started by any otto
//! process on the machine, not only ones this server has open.

use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::Response;
use otto_core::model::Message;
use serde::{Deserialize, Serialize};

use super::{Server, internal_error, json_response, not_found};
use crate::subagent::record::{self, TaskRow};

#[derive(Debug, Default, Deserialize)]
pub struct ListFilter {
    status: Option<String>,
    workspace: Option<String>,
    limit: Option<u32>,
    before: Option<String>,
}

/// A [`TaskRow`] with the server-computed `cancelable` field the spec adds
/// to the wire shape; [`record::TaskRow`] itself cannot know which sessions
/// this process owns.
#[derive(Debug, Serialize)]
struct TaskRowResponse {
    #[serde(flatten)]
    row: TaskRow,
    cancelable: bool,
}

#[derive(Debug, Serialize)]
struct TaskListResponse {
    tasks: Vec<TaskRowResponse>,
    next_before: String,
}

#[derive(Debug, Serialize)]
struct TaskDetailResponse {
    task: TaskRowResponse,
    history: Vec<Message>,
    transcript_missing: bool,
}

/// A task can be cancelled only while it is queued or running.
fn is_final_status(status: &str) -> bool {
    !matches!(status, "queued" | "running")
}

fn cancelable(server: &Server, row: &TaskRow) -> bool {
    !is_final_status(&row.status) && server.lookup(&row.parent_session).is_some()
}

pub async fn list(State(server): State<Arc<Server>>, Query(filter): Query<ListFilter>) -> Response {
    let query = record::ListQuery {
        status: filter.status,
        workspace: filter.workspace,
        limit: filter.limit,
        before: filter.before,
    };
    match server.factory.tasks_list(&query) {
        Ok(result) => {
            let tasks = result
                .tasks
                .into_iter()
                .map(|row| {
                    let cancelable = cancelable(&server, &row);
                    TaskRowResponse { row, cancelable }
                })
                .collect();
            json_response(
                StatusCode::OK,
                &TaskListResponse {
                    tasks,
                    next_before: result.next_before,
                },
            )
        }
        Err(error) => internal_error(&server.log, &error),
    }
}

pub async fn get(
    State(server): State<Arc<Server>>,
    Path((parent_session, task_id)): Path<(String, String)>,
) -> Response {
    match server.factory.tasks_get(&parent_session, &task_id) {
        Ok(Some(row)) => {
            let cancelable = cancelable(&server, &row);
            let (history, transcript_missing) = if row.session_path.is_empty() {
                (Vec::new(), true)
            } else {
                match crate::session::Store::read_transcript(&row.session_path) {
                    Ok(messages) => (messages, false),
                    Err(_) => (Vec::new(), true),
                }
            };
            json_response(
                StatusCode::OK,
                &TaskDetailResponse {
                    task: TaskRowResponse { row, cancelable },
                    history,
                    transcript_missing,
                },
            )
        }
        Ok(None) => not_found("task not found"),
        Err(error) => internal_error(&server.log, &error),
    }
}
