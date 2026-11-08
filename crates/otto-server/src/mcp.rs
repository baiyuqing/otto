//! The MCP server status route.
//!
//! Modeled on `tasks::list`: one read-only endpoint over
//! `otto::app::Controller::mcp`, no per-server detail route.

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::Response;
use serde::Serialize;

use super::{Server, json_response, not_found};
use otto_core::wire::mcp::McpServer;

#[derive(Debug, Serialize)]
struct McpServerList {
    servers: Vec<McpServer>,
}

pub async fn list(State(server): State<Arc<Server>>, Path(id): Path<String>) -> Response {
    let Some(session) = server.lookup(&id) else {
        return not_found("session not found");
    };
    let servers = session
        .ctrl
        .mcp()
        .into_iter()
        .map(McpServer::from)
        .collect();
    json_response(StatusCode::OK, &McpServerList { servers })
}
