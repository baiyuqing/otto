//! Durable workflow HTTP routes.

use std::sync::Arc;

use axum::Json;
use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::{StatusCode, header};
use axum::response::Response;
use serde::{Deserialize, Serialize};

use super::{Server, error_response, json_response, workspace_load_error_response};
use otto::workflow::{ApprovalRequest, Error, Run};

#[derive(Serialize)]
struct ListResponse {
    runs: Vec<Run>,
    /// Cursor for the next page; empty on the last page.
    next_before: String,
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
    /// Page size for `GET`; defaults to [`otto::workflow::DEFAULT_PAGE_LIMIT`].
    limit: Option<usize>,
    /// Return runs created before this cursor (a run's `created_at`).
    before: Option<String>,
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

fn view(controller: &otto::workflow::Controller, run_id: &str) -> Result<View, Error> {
    let run = controller.get(run_id)?;
    let requests = controller.requests(run_id)?;
    Ok(View { run, requests })
}

/// Renders a workflow failure: the kind decides the status, and only internal
/// failures log their cause instead of returning it.
fn map_error(server: &Server, error: Error) -> Response {
    match error {
        Error::NotFound(message) => {
            error_response(StatusCode::NOT_FOUND, "workflow_not_found", &message)
        }
        Error::Invalid(message) => {
            error_response(StatusCode::BAD_REQUEST, "workflow_invalid", &message)
        }
        Error::Conflict(message) => {
            error_response(StatusCode::CONFLICT, "workflow_conflict", &message)
        }
        Error::Unavailable(message) => error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "workflow_unavailable",
            &message,
        ),
        Error::Internal(message) => super::internal_error(&server.log, &message),
    }
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
) -> Result<Arc<otto::workflow::Controller>, Response> {
    let controllers = server.factory.workflow_controllers().await;
    if controllers.is_empty() {
        return Err(unavailable());
    }
    let mut last_error = Error::not_found("workflow run not found");
    for controller in controllers {
        match controller.get(run_id) {
            Ok(_) => return Ok(controller),
            Err(error) if error.is_not_found() => last_error = error,
            Err(error) => return Err(map_error(server, error)),
        }
    }
    Err(map_error(server, last_error))
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
    let limit = match query.limit {
        Some(0) => {
            return error_response(
                StatusCode::BAD_REQUEST,
                "invalid_workflow_limit",
                "limit must be at least 1",
            );
        }
        Some(limit) => limit.min(otto::workflow::MAX_PAGE_LIMIT),
        None => otto::workflow::DEFAULT_PAGE_LIMIT,
    };
    match controller.page(limit, query.before.clone()) {
        Ok(page) => json_response(
            StatusCode::OK,
            &ListResponse {
                runs: page.runs,
                next_before: page.next_before,
            },
        ),
        Err(error) => map_error(&server, error),
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
            Err(error) => map_error(&server, error),
        },
        Err(error) => map_error(&server, error),
    }
}

pub async fn get(State(server): State<Arc<Server>>, Path(id): Path<String>) -> Response {
    let controller = match find_by_run(&server, &id).await {
        Ok(controller) => controller,
        Err(response) => return response,
    };
    match view(&controller, &id) {
        Ok(view) => json_response(StatusCode::OK, &view),
        Err(error) => map_error(&server, error),
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
            Err(error) => map_error(&server, error),
        },
        Err(error) => map_error(&server, error),
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
            Err(error) => map_error(&server, error),
        },
        Err(error) => map_error(&server, error),
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
            Err(error) => map_error(&server, error),
        },
        Err(error) => map_error(&server, error),
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
    let mut last_error = Error::not_found("approval request not found");
    for controller in controllers {
        match controller.respond(&id, approved) {
            Ok(run) => {
                return match controller.requests(&run.id) {
                    Ok(requests) => json_response(StatusCode::OK, &View { run, requests }),
                    Err(error) => map_error(&server, error),
                };
            }
            Err(error) if error.is_not_found() => last_error = error,
            Err(error) => return map_error(&server, error),
        }
    }
    map_error(&server, last_error)
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
        Err(error) => return map_error(&server, error),
    };
    let mut body = String::new();
    for event in events {
        let event_json = match serde_json::to_string(&event) {
            Ok(event_json) => event_json,
            Err(_) => {
                return super::internal_error(&server.log, "workflow event serialization failed");
            }
        };
        body.push_str(&format!(
            "id: {}\nevent: workflow\ndata: {event_json}\n\n",
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
