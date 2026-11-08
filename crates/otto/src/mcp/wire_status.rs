//! Conversion between [`ServerStatus`] and the `/v1/sessions/{id}/mcp` wire
//! record [`McpServer`]. The server converts status to wire; the attached TUI
//! converts wire back to status.

use otto_core::wire::mcp::McpServer;

use super::{Era, MODERN_VERSION, ServerState, ServerStatus};

impl From<ServerStatus> for McpServer {
    fn from(status: ServerStatus) -> Self {
        let protocol_version = match status.era {
            Some(Era::Modern) => Some(MODERN_VERSION.to_string()),
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

impl TryFrom<McpServer> for ServerStatus {
    type Error = String;

    fn try_from(server: McpServer) -> Result<Self, String> {
        let transport = match server.transport.as_str() {
            "stdio" => "stdio",
            "http" => "http",
            other => {
                return Err(format!(
                    "unknown MCP transport {other:?} for server {}",
                    server.name
                ));
            }
        };
        let era = match server.protocol_version {
            Some(version) if version == MODERN_VERSION => Some(Era::Modern),
            Some(version) => Some(Era::Legacy(version)),
            None => None,
        };
        let state = match server.state.as_str() {
            "connected" => ServerState::Connected {
                tools: usize::try_from(server.tools).unwrap_or(usize::MAX),
            },
            "connecting" => ServerState::Connecting,
            "disabled" => ServerState::Disabled,
            "needs_login" => ServerState::NeedsLogin,
            "failed" => ServerState::Failed(server.error.unwrap_or_default()),
            other => {
                return Err(format!(
                    "unknown MCP state {other:?} for server {}",
                    server.name
                ));
            }
        };
        Ok(ServerStatus {
            name: server.name,
            transport,
            era,
            state,
        })
    }
}

#[cfg(test)]
mod tests;
