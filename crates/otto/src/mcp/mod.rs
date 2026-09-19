//! Model Context Protocol (MCP) client: tools exposed by external servers.
//!
//! Design: `docs/specs/2026-09-19-mcp-design.md`. This module owns the
//! JSON-RPC codec ([`jsonrpc`]), the two transports ([`stdio`], [`http`]),
//! the OAuth 2.1 flow for HTTP servers ([`oauth`]), and the per-server
//! [`client::Client`] that negotiates the protocol era and serves
//! `tools/list` and `tools/call`. The `Tool` adapter that presents one MCP
//! tool to the model lives in `crate::tool::mcp`.
//!
//! Ownership: every server's client is created at runner build, shared by
//! `Arc` between the tools that use it, and shut down by [`Servers::close`]
//! from `Runner::close`. Concurrency: [`ToolServer::call`] takes `&self` and
//! may run concurrently; each transport serializes its own writes.
//! Cancellation: a cancelled token aborts the in-flight call with
//! [`CallError::Cancelled`]; the stdio transport also sends
//! `notifications/cancelled`. Errors: every failure is reported in band to
//! the model as an error `ToolResult`; nothing here returns a `Result` to
//! the agent loop. Security: tool names, descriptions, schemas and results
//! are untrusted server data and are capped and redacted by the adapter.

pub mod client;
pub mod http;
pub mod jsonrpc;
pub mod oauth;
pub mod sse;
pub mod stdio;

use std::sync::{Arc, Mutex};

use serde::Deserialize;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

/// The protocol era a server negotiated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Era {
    /// `initialize` handshake; the value is the version the server answered.
    Legacy(String),
    /// 2026-07-28 stateless framing with per-request `_meta`.
    Modern,
}

/// The modern protocol version this client sends.
pub const MODERN_VERSION: &str = "2026-07-28";
/// The legacy protocol version this client offers in `initialize`.
pub const LEGACY_VERSION: &str = "2025-11-25";

/// One tool as advertised by `tools/list`. Untrusted server data.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct ToolInfo {
    pub name: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default, rename = "inputSchema")]
    pub input_schema: Option<Value>,
}

/// One `content` block of a `tools/call` result. Binary payloads keep only
/// their length: `ToolResult` is text-only and the base64 is never stored.
#[derive(Debug, Clone, PartialEq)]
pub enum ContentBlock {
    Text {
        text: String,
    },
    Image {
        mime_type: String,
        data_len: usize,
    },
    Audio {
        mime_type: String,
        data_len: usize,
    },
    ResourceLink {
        uri: String,
        name: String,
        title: Option<String>,
    },
    Resource {
        uri: String,
        mime_type: Option<String>,
        text: Option<String>,
        blob_len: usize,
    },
}

/// A decoded `tools/call` result.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CallOutcome {
    pub content: Vec<ContentBlock>,
    pub structured_content: Option<Value>,
    pub is_error: bool,
}

/// Why a call did not produce a [`CallOutcome`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CallError {
    /// A JSON-RPC error response from the server.
    #[error("{code} {message}")]
    Rpc { code: i64, message: String },
    /// The transport failed: child exited, connection refused, HTTP 5xx,
    /// malformed frame. The text is safe to show and never contains a secret.
    #[error("{0}")]
    Transport(String),
    #[error("timed out")]
    Timeout,
    #[error("context canceled")]
    Cancelled,
    /// An HTTP server rejected or lacks a bearer token; the user must run
    /// `otto mcp login <server>`.
    #[error("authorization required; run 'otto mcp login <server>'")]
    NeedsLogin,
}

/// The seam between a tool adapter and one server. Implemented by
/// [`client::Client`]; tests substitute a fake.
#[async_trait::async_trait]
pub trait ToolServer: Send + Sync {
    /// The configured server name (the `<server>` in `mcp__<server>__<tool>`).
    fn name(&self) -> &str;
    /// Calls `tool` with `arguments` (a JSON object).
    async fn call(
        &self,
        tool: &str,
        arguments: Value,
        cancel: &CancellationToken,
    ) -> Result<CallOutcome, CallError>;
}

/// Where an HTTP transport gets its bearer token. Implemented by the OAuth
/// token store in [`oauth`]; a server with `auth = "none"` has no source.
#[async_trait::async_trait]
pub trait BearerSource: Send + Sync {
    /// The current access token, refreshed first when it is expired or
    /// expiring within 60 s. `Err(NeedsLogin)` when there is no usable token.
    async fn bearer(&self, cancel: &CancellationToken) -> Result<String, CallError>;
    /// Called after a 401 with a token that `bearer` returned: refresh once
    /// and return the new token, or `Err(NeedsLogin)`.
    async fn refresh(&self, cancel: &CancellationToken) -> Result<String, CallError>;
    /// Secrets to redact from results and logs: the current access and
    /// refresh tokens.
    fn secrets(&self) -> Vec<String>;
}

/// One transport-level request or notification, era already decided.
#[derive(Debug, Clone, PartialEq)]
pub struct Outbound {
    pub method: String,
    pub params: Value,
    /// `None` while the era is still being probed.
    pub era: Option<Era>,
}

/// One JSON-RPC transport. Two implementations: [`stdio::StdioTransport`]
/// and [`http::HttpTransport`].
#[async_trait::async_trait]
pub trait Transport: Send + Sync {
    /// Sends a request and waits for its response. `Ok(Err(..))` is a JSON-RPC
    /// error from the server; `Err(..)` is a transport failure.
    async fn request(
        &self,
        outbound: Outbound,
        cancel: &CancellationToken,
    ) -> Result<Result<Value, jsonrpc::RpcError>, CallError>;
    /// Sends a notification; no response is expected.
    async fn notify(&self, outbound: Outbound) -> Result<(), CallError>;
    /// Releases the transport: closes the child or drops the connection.
    async fn close(&self);
}

/// The state of one configured server, as `/mcp` reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServerState {
    Connected { tools: usize },
    Disabled,
    NeedsLogin,
    Failed(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerStatus {
    pub name: String,
    /// `"stdio"` or `"http"`.
    pub transport: &'static str,
    pub era: Option<Era>,
    pub state: ServerState,
}

/// The connected servers of one runner: their status rows for `/mcp` and the
/// handles `Runner::close` shuts down.
#[derive(Default)]
pub struct Servers {
    status: Mutex<Vec<ServerStatus>>,
    clients: Mutex<Vec<Arc<client::Client>>>,
}

impl Servers {
    pub fn push(&self, status: ServerStatus, client: Option<Arc<client::Client>>) {
        self.status.lock().expect("mcp status lock").push(status);
        if let Some(client) = client {
            self.clients.lock().expect("mcp client lock").push(client);
        }
    }

    /// Status rows in configuration order.
    pub fn status(&self) -> Vec<ServerStatus> {
        self.status.lock().expect("mcp status lock").clone()
    }

    /// Shuts every client down. Idempotent.
    pub async fn close(&self) {
        let clients = std::mem::take(&mut *self.clients.lock().expect("mcp client lock"));
        for client in clients {
            client.close().await;
        }
    }
}
