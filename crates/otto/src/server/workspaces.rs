//! `GET/POST /v1/workspaces`.

use std::sync::Arc;

use axum::Json;
use axum::body::Body;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::Response;
use serde::{Deserialize, Serialize};

use super::{
    Server, error_response, json_response, workspace_load_error_response,
    workspace_remove_error_response,
};

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

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RemoveQuery {
    path: String,
}

pub async fn remove(
    State(server): State<Arc<Server>>,
    Query(query): Query<RemoveQuery>,
) -> Response {
    // Sessions record the canonical workspace path; a path that no longer
    // resolves (a deleted directory's persisted entry) is used as given.
    let path = std::fs::canonicalize(&query.path)
        .map(|path| path.to_string_lossy().into_owned())
        .unwrap_or(query.path);
    let open = open_sessions(&server, &path);
    if open > 0 {
        return error_response(
            StatusCode::CONFLICT,
            "WORKSPACE_IN_USE",
            &format!("{open} open session(s) in this workspace"),
        );
    }
    match server.factory.remove_workspace(&path).await {
        Ok(()) => Response::builder()
            .status(StatusCode::NO_CONTENT)
            .body(Body::empty())
            .expect("static response"),
        Err(error) => workspace_remove_error_response(error),
    }
}
