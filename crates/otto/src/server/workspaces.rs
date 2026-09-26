//! `GET/POST /v1/workspaces`.

use std::sync::Arc;

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::Response;
use serde::{Deserialize, Serialize};

use super::{Server, json_response, workspace_load_error_response};

#[derive(Serialize)]
struct Entry {
    path: String,
    open_sessions: usize,
    workflows: bool,
}

#[derive(Serialize)]
struct ListResponse {
    startup: String,
    roots: Vec<String>,
    workspaces: Vec<Entry>,
}

/// Sessions open on `path`, counted from the server's own registry: the
/// [`Factory`](super::Factory) has no view of open sessions.
fn open_sessions(server: &Server, path: &str) -> usize {
    server
        .all_sessions()
        .iter()
        .filter(|session| session.ctrl.workspace() == path)
        .count()
}

pub async fn list(State(server): State<Arc<Server>>) -> Response {
    let registry = server.factory.workspaces().await;
    let workspaces = registry
        .loaded
        .into_iter()
        .map(|workspace| Entry {
            open_sessions: open_sessions(&server, &workspace.path),
            path: workspace.path,
            workflows: workspace.workflows,
        })
        .collect();
    json_response(
        StatusCode::OK,
        &ListResponse {
            startup: registry.startup,
            roots: registry.roots,
            workspaces,
        },
    )
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RegisterRequest {
    path: String,
}

pub async fn register(
    State(server): State<Arc<Server>>,
    Json(request): Json<RegisterRequest>,
) -> Response {
    match server.factory.load_workspace(&request.path).await {
        Ok((info, newly_loaded)) => {
            let status = if newly_loaded {
                StatusCode::CREATED
            } else {
                StatusCode::OK
            };
            let entry = Entry {
                open_sessions: open_sessions(&server, &info.path),
                path: info.path,
                workflows: info.workflows,
            };
            json_response(status, &entry)
        }
        Err(error) => workspace_load_error_response(error),
    }
}
