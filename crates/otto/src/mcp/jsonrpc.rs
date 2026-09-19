//! JSON-RPC 2.0 message types and the modern `_meta` builder.
//!
//! Owned by the MCP codec step; see `docs/specs/2026-09-19-mcp-design.md`.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A JSON-RPC error object as the server sent it. Untrusted data.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RpcError {
    pub code: i64,
    #[serde(default)]
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}
