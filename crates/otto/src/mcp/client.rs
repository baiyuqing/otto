//! The per-server client: era negotiation, `tools/list`, `tools/call`.
//!
//! Owned by the stdio/client step; see `docs/specs/2026-09-19-mcp-design.md`.
//! Era negotiation ([`Client::connect`]) probes `server/discover`: a modern
//! server answers it, a legacy server rejects it with a recognizable error,
//! and only then is the `initialize`/`notifications/initialized` handshake
//! attempted. Every later request is stamped with the negotiated era so the
//! transport can frame it correctly (modern requests carry `_meta`).

use std::time::Duration;

use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use super::jsonrpc::{self, METHOD_NOT_FOUND, UNSUPPORTED_PROTOCOL_VERSION};
use super::{CallError, CallOutcome, Era, Outbound, ToolInfo, ToolServer, Transport};

/// How long era negotiation waits for `server/discover` before assuming the
/// server does not understand it and falling back to the legacy handshake.
const DISCOVER_TIMEOUT: Duration = Duration::from_secs(5);

/// One connected server: its transport, negotiated era, and advertised tools.
pub struct Client {
    name: String,
    transport: Box<dyn Transport>,
    era: Era,
    tools: Vec<ToolInfo>,
    call_timeout: Duration,
}

impl Client {
    /// Connects to a server: negotiates the protocol era, then fetches its
    /// full tool list (paging through `nextCursor`).
    pub async fn connect(
        name: String,
        transport: Box<dyn Transport>,
        connect_timeout: Duration,
        call_timeout: Duration,
        cancel: &CancellationToken,
    ) -> Result<Client, CallError> {
        let era = negotiate_era(transport.as_ref(), connect_timeout, cancel).await?;
        let tools = fetch_all_tools(transport.as_ref(), &era, cancel).await?;
        Ok(Client {
            name,
            transport,
            era,
            tools,
            call_timeout,
        })
    }

    pub fn era(&self) -> &Era {
        &self.era
    }

    pub fn tools(&self) -> &[ToolInfo] {
        &self.tools
    }

    /// Shuts the server down. Idempotent.
    pub async fn close(&self) {
        self.transport.close().await;
    }
}

#[async_trait::async_trait]
impl ToolServer for Client {
    fn name(&self) -> &str {
        &self.name
    }

    async fn call(
        &self,
        tool: &str,
        arguments: Value,
        cancel: &CancellationToken,
    ) -> Result<CallOutcome, CallError> {
        let params = era_params(&self.era, json!({"name": tool, "arguments": arguments}));
        let outbound = Outbound {
            method: "tools/call".to_string(),
            params,
            era: Some(self.era.clone()),
        };
        let result =
            tokio::time::timeout(self.call_timeout, self.transport.request(outbound, cancel))
                .await
                .map_err(|_| CallError::Timeout)??;
        let result = result.map_err(|error| CallError::Rpc {
            code: error.code,
            message: error.message,
        })?;
        jsonrpc::decode_call_result(&result, &self.era).map_err(CallError::Transport)
    }
}

/// Wraps `params` with `_meta` on the modern era; leaves legacy params as is.
fn era_params(era: &Era, params: Value) -> Value {
    match era {
        Era::Modern => jsonrpc::modern_meta(params),
        Era::Legacy(_) => params,
    }
}

/// Probes `server/discover`; on a recognized rejection (or a timeout), falls
/// back to the legacy `initialize` handshake. Any other outcome is a
/// transport failure: the server exists but does not speak a supported era.
async fn negotiate_era(
    transport: &dyn Transport,
    connect_timeout: Duration,
    cancel: &CancellationToken,
) -> Result<Era, CallError> {
    let discover = Outbound {
        method: "server/discover".to_string(),
        params: jsonrpc::modern_meta(json!({})),
        era: None,
    };
    let outcome = tokio::time::timeout(DISCOVER_TIMEOUT, transport.request(discover, cancel)).await;

    let is_legacy_rejection = match &outcome {
        Ok(Ok(Ok(_))) => return Ok(Era::Modern),
        Ok(Ok(Err(error))) => {
            error.code == METHOD_NOT_FOUND
                || error.code == jsonrpc::INVALID_PARAMS
                || (error.code == UNSUPPORTED_PROTOCOL_VERSION && !jsonrpc::supports_modern(error))
        }
        Ok(Err(CallError::Timeout)) | Err(_) => true,
        Ok(Err(_)) => false,
    };

    if !is_legacy_rejection {
        return Err(CallError::Transport(format!(
            "mcp server rejected server/discover: {outcome:?}"
        )));
    }

    legacy_handshake(transport, connect_timeout, cancel).await
}

async fn legacy_handshake(
    transport: &dyn Transport,
    connect_timeout: Duration,
    cancel: &CancellationToken,
) -> Result<Era, CallError> {
    let initialize = Outbound {
        method: "initialize".to_string(),
        params: jsonrpc::initialize_params(),
        era: None,
    };
    let result = tokio::time::timeout(connect_timeout, transport.request(initialize, cancel))
        .await
        .map_err(|_| CallError::Timeout)??
        .map_err(|error| CallError::Rpc {
            code: error.code,
            message: error.message,
        })?;
    let version = jsonrpc::decode_initialize(&result).map_err(CallError::Transport)?;

    transport
        .notify(Outbound {
            method: "notifications/initialized".to_string(),
            params: Value::Null,
            era: Some(Era::Legacy(version.clone())),
        })
        .await?;

    Ok(Era::Legacy(version))
}

/// Fetches every page of `tools/list`, following `nextCursor` until it is
/// `None`.
async fn fetch_all_tools(
    transport: &dyn Transport,
    era: &Era,
    cancel: &CancellationToken,
) -> Result<Vec<ToolInfo>, CallError> {
    let mut tools = Vec::new();
    let mut cursor: Option<String> = None;
    loop {
        let mut params = json!({});
        if let Some(cursor) = &cursor {
            params["cursor"] = json!(cursor);
        }
        let outbound = Outbound {
            method: "tools/list".to_string(),
            params: era_params(era, params),
            era: Some(era.clone()),
        };
        let result = transport
            .request(outbound, cancel)
            .await?
            .map_err(|error| CallError::Rpc {
                code: error.code,
                message: error.message,
            })?;
        let (mut page, next_cursor) =
            jsonrpc::decode_tools_list(&result).map_err(CallError::Transport)?;
        tools.append(&mut page);
        match next_cursor {
            Some(next) => cursor = Some(next),
            None => break,
        }
    }
    Ok(tools)
}
