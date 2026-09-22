//! The MCP server status route.
//!
//! Modeled on `tasks::list`: one read-only endpoint over
//! `crate::app::Controller::mcp`, no per-server detail route.

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::Response;
use serde::Serialize;

use super::{Server, json_response, not_found};
use crate::mcp::{Era, ServerState, ServerStatus};

#[derive(Debug, Serialize, PartialEq)]
pub struct McpServer {
    pub name: String,
    pub transport: String,
    pub protocol_version: Option<String>,
    pub state: String,
    pub tools: u64,
    pub error: Option<String>,
}

impl From<ServerStatus> for McpServer {
    fn from(status: ServerStatus) -> Self {
        let protocol_version = match status.era {
            Some(Era::Modern) => Some(crate::mcp::MODERN_VERSION.to_string()),
            Some(Era::Legacy(version)) => Some(version),
            None => None,
        };
        let (state, tools, error) = match status.state {
            ServerState::Connected { tools } => ("connected", tools as u64, None),
            ServerState::Connecting => ("connecting", 0, None),
            ServerState::Disabled => ("disabled", 0, None),
            ServerState::NeedsLogin => ("needs_login", 0, None),
            ServerState::Failed(message) => ("failed", 0, Some(message)),
        };
        McpServer {
            name: status.name,
            transport: status.transport.to_string(),
            protocol_version,
            state: state.to_string(),
            tools,
            error,
        }
    }
}

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_status_converts_to_the_wire_record_for_every_state_and_era() {
        assert_eq!(
            McpServer::from(ServerStatus {
                name: "docs".to_string(),
                transport: "http",
                era: Some(Era::Modern),
                state: ServerState::Connected { tools: 3 },
            }),
            McpServer {
                name: "docs".to_string(),
                transport: "http".to_string(),
                protocol_version: Some(crate::mcp::MODERN_VERSION.to_string()),
                state: "connected".to_string(),
                tools: 3,
                error: None,
            }
        );

        assert_eq!(
            McpServer::from(ServerStatus {
                name: "slow".to_string(),
                transport: "stdio",
                era: None,
                state: ServerState::Connecting,
            }),
            McpServer {
                name: "slow".to_string(),
                transport: "stdio".to_string(),
                protocol_version: None,
                state: "connecting".to_string(),
                tools: 0,
                error: None,
            }
        );

        assert_eq!(
            McpServer::from(ServerStatus {
                name: "legacy-tool".to_string(),
                transport: "http",
                era: Some(Era::Legacy("2025-11-25".to_string())),
                state: ServerState::NeedsLogin,
            }),
            McpServer {
                name: "legacy-tool".to_string(),
                transport: "http".to_string(),
                protocol_version: Some("2025-11-25".to_string()),
                state: "needs_login".to_string(),
                tools: 0,
                error: None,
            }
        );

        assert_eq!(
            McpServer::from(ServerStatus {
                name: "shell".to_string(),
                transport: "stdio",
                era: None,
                state: ServerState::Disabled,
            }),
            McpServer {
                name: "shell".to_string(),
                transport: "stdio".to_string(),
                protocol_version: None,
                state: "disabled".to_string(),
                tools: 0,
                error: None,
            }
        );

        assert_eq!(
            McpServer::from(ServerStatus {
                name: "broken".to_string(),
                transport: "stdio",
                era: None,
                state: ServerState::Failed("spawn failed: not found".to_string()),
            }),
            McpServer {
                name: "broken".to_string(),
                transport: "stdio".to_string(),
                protocol_version: None,
                state: "failed".to_string(),
                tools: 0,
                error: Some("spawn failed: not found".to_string()),
            }
        );
    }
}
