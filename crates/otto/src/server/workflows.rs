//! Durable workflow HTTP routes.

use std::sync::Arc;

use axum::Json;
use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::{StatusCode, header};
use axum::response::Response;
use serde::{Deserialize, Serialize};

use super::{Server, error_response, json_response};
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

fn controller(server: &Server) -> Option<&Arc<crate::workflow::Controller>> {
    server.workflows.as_ref()
}

fn unavailable() -> Response {
    error_response(
        StatusCode::NOT_IMPLEMENTED,
        "workflow_unavailable",
        "workflow runtime unavailable",
    )
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

pub async fn list(State(server): State<Arc<Server>>) -> Response {
    let controller = match controller(&server) {
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
    Json(request): Json<StartRequest>,
) -> Response {
    let controller = match controller(&server) {
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
    let controller = match controller(&server) {
        Some(controller) => controller,
        None => return unavailable(),
    };
    match view(controller, &id) {
        Ok(view) => json_response(StatusCode::OK, &view),
        Err(error) => map_error(error),
    }
}

pub async fn resume(
    State(server): State<Arc<Server>>,
    Path(id): Path<String>,
    Json(request): Json<ResumeRequest>,
) -> Response {
    let controller = match controller(&server) {
        Some(controller) => controller,
        None => return unavailable(),
    };
    match controller.resume(&id, request.retry.as_deref()).await {
        Ok(_) => match view(controller, &id) {
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
    let controller = match controller(&server) {
        Some(controller) => controller,
        None => return unavailable(),
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
    let controller = match controller(&server) {
        Some(controller) => controller,
        None => return unavailable(),
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
    respond(server, id, true)
}

pub async fn reject(State(server): State<Arc<Server>>, Path(id): Path<String>) -> Response {
    respond(server, id, false)
}

fn respond(server: Arc<Server>, id: String, approved: bool) -> Response {
    let controller = match controller(&server) {
        Some(controller) => controller,
        None => return unavailable(),
    };
    match controller.respond(&id, approved) {
        Ok(run) => match controller.requests(&run.id) {
            Ok(requests) => json_response(StatusCode::OK, &View { run, requests }),
            Err(error) => map_error(error),
        },
        Err(error) => map_error(error),
    }
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
    let controller = match controller(&server) {
        Some(controller) => controller,
        None => return unavailable(),
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
