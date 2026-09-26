//! Durable workflow HTTP routes.

use std::sync::Arc;

use axum::Json;
use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::{StatusCode, header};
use axum::response::Response;
use serde::{Deserialize, Serialize};

use super::{Server, error_response, json_response, workspace_load_error_response};
use crate::workflow::{ApprovalRequest, Run};

#[derive(Serialize)]
struct ListResponse {
    runs: Vec<Run>,
}

#[derive(Serialize)]
struct View {
    run: Run,
    requests: Vec<ApprovalRequest>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StartRequest {
    name: String,
    #[serde(default)]
    input: String,
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResumeRequest {
    retry: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ForkRequest {
    after_step: String,
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EventsQuery {
    #[serde(default)]
    after: i64,
}

/// Query for the workspace-scoped routes (`GET`/`POST /v1/workflows`).
/// Absent `workspace` means the startup workspace.
#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowsQuery {
    workspace: Option<String>,
}

fn unavailable() -> Response {
    error_response(
        StatusCode::NOT_IMPLEMENTED,
        "workflow_unavailable",
        "workflow runtime unavailable",
    )
}

/// Admits and loads `workspace` (a caller-named `?workspace=`), or leaves it
/// as the startup workspace when absent. Shared by `list` and `start`.
#[allow(clippy::result_large_err)]
async fn admit(server: &Server, workspace: &Option<String>) -> Result<Option<String>, Response> {
    match workspace {
        Some(path) => match server.factory.load_workspace(path).await {
            Ok((info, _newly_loaded)) => Ok(Some(info.path)),
            Err(error) => Err(workspace_load_error_response(error)),
        },
        None => Ok(None),
    }
}

fn view(controller: &crate::workflow::Controller, run_id: &str) -> Result<View, String> {
    let run = controller.get(run_id)?;
    let requests = controller.requests(run_id)?;
    Ok(View { run, requests })
}

fn map_error(message: String) -> Response {
    let status = if message.ends_with("not found") {
        StatusCode::NOT_FOUND
    } else {
        StatusCode::CONFLICT
    };
    error_response(status, "workflow_failed", &message)
}

/// The controller owning `run_id`, searched startup first then the other
/// loaded workspaces in path order. Only a "not found" error tries the next
/// controller; any other error is this run's own workspace reporting a real
/// failure and is returned immediately, so a broken workspace is never
/// masked as "not this one" (see the same principle applied to session
/// listing in `cli/serve.rs`).
#[allow(clippy::result_large_err)]
async fn find_by_run(
    server: &Server,
    run_id: &str,
) -> Result<Arc<crate::workflow::Controller>, Response> {
    let controllers = server.factory.workflow_controllers().await;
    if controllers.is_empty() {
        return Err(unavailable());
    }
    let mut last_error = "workflow run not found".to_string();
    for controller in controllers {
        match controller.get(run_id) {
            Ok(_) => return Ok(controller),
            Err(error) if error.ends_with("not found") => last_error = error,
            Err(error) => return Err(map_error(error)),
        }
    }
    Err(map_error(last_error))
}

pub async fn list(
    State(server): State<Arc<Server>>,
    Query(query): Query<WorkflowsQuery>,
) -> Response {
    let workspace = match admit(&server, &query.workspace).await {
        Ok(workspace) => workspace,
        Err(response) => return response,
    };
    let controller = match server
        .factory
        .workflow_controller(workspace.as_deref())
        .await
    {
        Some(controller) => controller,
        None => return unavailable(),
    };
    match controller.list() {
        Ok(runs) => json_response(StatusCode::OK, &ListResponse { runs }),
        Err(error) => map_error(error),
    }
}

pub async fn start(
    State(server): State<Arc<Server>>,
    Query(query): Query<WorkflowsQuery>,
    Json(request): Json<StartRequest>,
) -> Response {
    let workspace = match admit(&server, &query.workspace).await {
        Ok(workspace) => workspace,
        Err(response) => return response,
    };
    let controller = match server
        .factory
        .workflow_controller(workspace.as_deref())
        .await
    {
        Some(controller) => controller,
        None => return unavailable(),
    };
    match controller.start(&request.name, &request.input).await {
        Ok(run) => match controller.requests(&run.id) {
            Ok(requests) => json_response(StatusCode::CREATED, &View { run, requests }),
            Err(error) => map_error(error),
        },
        Err(error) => map_error(error),
    }
}

pub async fn get(State(server): State<Arc<Server>>, Path(id): Path<String>) -> Response {
    let controller = match find_by_run(&server, &id).await {
        Ok(controller) => controller,
        Err(response) => return response,
    };
    match view(&controller, &id) {
        Ok(view) => json_response(StatusCode::OK, &view),
        Err(error) => map_error(error),
    }
}

pub async fn resume(
    State(server): State<Arc<Server>>,
    Path(id): Path<String>,
    Json(request): Json<ResumeRequest>,
) -> Response {
    let controller = match find_by_run(&server, &id).await {
        Ok(controller) => controller,
        Err(response) => return response,
    };
    match controller.resume(&id, request.retry.as_deref()).await {
        Ok(_) => match view(&controller, &id) {
            Ok(view) => json_response(StatusCode::OK, &view),
            Err(error) => map_error(error),
        },
        Err(error) => map_error(error),
    }
}

pub async fn fork(
    State(server): State<Arc<Server>>,
    Path(id): Path<String>,
    Json(request): Json<ForkRequest>,
) -> Response {
    let controller = match find_by_run(&server, &id).await {
        Ok(controller) => controller,
        Err(response) => return response,
    };
    match controller.fork(&id, &request.after_step).await {
        Ok(run) => match controller.requests(&run.id) {
            Ok(requests) => json_response(StatusCode::CREATED, &View { run, requests }),
            Err(error) => map_error(error),
        },
        Err(error) => map_error(error),
    }
}

pub async fn cancel(State(server): State<Arc<Server>>, Path(id): Path<String>) -> Response {
    let controller = match find_by_run(&server, &id).await {
        Ok(controller) => controller,
        Err(response) => return response,
    };
    match controller.cancel(&id).await {
        Ok(run) => match controller.requests(&run.id) {
            Ok(requests) => json_response(StatusCode::OK, &View { run, requests }),
            Err(error) => map_error(error),
        },
        Err(error) => map_error(error),
    }
}

pub async fn approve(State(server): State<Arc<Server>>, Path(id): Path<String>) -> Response {
    respond(server, id, true).await
}

pub async fn reject(State(server): State<Arc<Server>>, Path(id): Path<String>) -> Response {
    respond(server, id, false).await
}

/// Searches every loaded workspace's controller for `request_id`, applying
/// the response. `Controller::respond` itself checks the request's run
/// against its own workspace before mutating anything, so trying it against
/// the wrong workspace's controller first is side-effect free.
async fn respond(server: Arc<Server>, id: String, approved: bool) -> Response {
    let controllers = server.factory.workflow_controllers().await;
    if controllers.is_empty() {
        return unavailable();
    }
    let mut last_error = "approval request not found".to_string();
    for controller in controllers {
        match controller.respond(&id, approved) {
            Ok(run) => {
                return match controller.requests(&run.id) {
                    Ok(requests) => json_response(StatusCode::OK, &View { run, requests }),
                    Err(error) => map_error(error),
                };
            }
            Err(error) if error.ends_with("not found") => last_error = error,
            Err(error) => return map_error(error),
        }
    }
    map_error(last_error)
}

pub async fn events(
    State(server): State<Arc<Server>>,
    Path(id): Path<String>,
    Query(query): Query<EventsQuery>,
) -> Response {
    if query.after < 0 {
        return error_response(
            StatusCode::BAD_REQUEST,
            "invalid_workflow_event_sequence",
            "after must be non-negative",
        );
    }
    let controller = match find_by_run(&server, &id).await {
        Ok(controller) => controller,
        Err(response) => return response,
    };
    let events = match controller.events(&id, query.after) {
        Ok(events) => events,
        Err(error) => return map_error(error),
    };
    let mut body = String::new();
    for event in events {
        let data = match serde_json::to_string(&event) {
            Ok(data) => data,
            Err(_) => return map_error("workflow event unavailable".to_string()),
        };
        body.push_str(&format!(
            "id: {}\nevent: workflow\ndata: {data}\n\n",
            event.seq
        ));
    }
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .body(Body::from(body))
        .unwrap_or_else(|_| Response::new(Body::empty()))
}
