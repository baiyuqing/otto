//! Cross-process sub-agent task routes: `GET /v1/tasks` and `GET
//! /v1/tasks/{parent_session}/{task_id}`. Unlike `tasks.rs`'s per-session
//! routes, which read one open [`otto::app::Controller`]'s in-memory
//! registry, these read `tasks.db` directly through
//! [`otto::subagent::record`] and answer for tasks started by any otto
//! process on the machine, not only ones this server has open.

use std::path::Path as FsPath;
use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::Response;
use otto_core::model::Message;
use serde::{Deserialize, Serialize};

use super::{Server, internal_error, json_response, not_found};
use otto::subagent::record::{self, TaskRow};

#[derive(Debug, Default, Deserialize)]
pub struct ListFilter {
    parent_session: Option<String>,
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

fn task_history(row: &TaskRow) -> Option<Vec<Message>> {
    if row.session_path.is_empty()
        || row.task_id.is_empty()
        || !row
            .task_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
    {
        return None;
    }

    let parent_path = FsPath::new(&row.parent_session_path);
    let child_dir = parent_path.with_extension("");
    let transcript_path = FsPath::new(&row.session_path);
    let file_name = transcript_path.file_name().and_then(|name| name.to_str())?;
    if transcript_path.parent() != Some(child_dir.as_path())
        || !file_name.starts_with(&format!("{}-", row.task_id))
        || transcript_path.extension().and_then(|ext| ext.to_str()) != Some("jsonl")
    {
        return None;
    }

    let Ok(directory_metadata) = std::fs::symlink_metadata(&child_dir) else {
        return None;
    };
    let Ok(file_metadata) = std::fs::symlink_metadata(transcript_path) else {
        return None;
    };
    if !directory_metadata.file_type().is_dir() || !file_metadata.file_type().is_file() {
        return None;
    }
    let Ok(canonical_dir) = std::fs::canonicalize(&child_dir) else {
        return None;
    };
    let Ok(canonical_file) = std::fs::canonicalize(transcript_path) else {
        return None;
    };
    if canonical_file.parent() != Some(canonical_dir.as_path()) {
        return None;
    }

    otto_session::Store::read_transcript(canonical_file).ok()
}

pub async fn list(State(server): State<Arc<Server>>, Query(filter): Query<ListFilter>) -> Response {
    let query = record::ListQuery {
        parent_session: filter.parent_session,
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
            if row.parent_session != parent_session || row.task_id != task_id {
                return not_found("task not found");
            }
            let cancelable = cancelable(&server, &row);
            let history = task_history(&row);
            json_response(
                StatusCode::OK,
                &TaskDetailResponse {
                    task: TaskRowResponse { row, cancelable },
                    transcript_missing: history.is_none(),
                    history: history.unwrap_or_default(),
                },
            )
        }
        Ok(None) => not_found("task not found"),
        Err(error) => internal_error(&server.log, &error),
    }
}
