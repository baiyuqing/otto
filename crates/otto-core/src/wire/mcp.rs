//! The `GET /v1/sessions/{id}/mcp` record: one configured MCP server and its
//! state. `otto-server` serializes it and the attached TUI deserializes it.

use serde::{Deserialize, Serialize};

/// One MCP server row. `state` is `connected`, `connecting`, `disabled`,
/// `needs_login` or `failed`; `tools` is non-zero only when connected and
/// `error` is set only when failed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpServer {
    pub name: String,
    pub transport: String,
    pub protocol_version: Option<String>,
    pub state: String,
    pub tools: u64,
    pub error: Option<String>,
}
