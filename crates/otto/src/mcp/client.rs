//! The per-server client: era negotiation, `tools/list`, `tools/call`.
//!
//! Owned by the stdio/client step; see `docs/specs/2026-09-19-mcp-design.md`.
//! Era negotiation ([`Client::connect`]) probes `server/discover`: a modern
//! server answers it, a legacy server rejects it with a recognizable error,
//! and only then is the `initialize`/`notifications/initialized` handshake
//! attempted. Every later request is stamped with the negotiated era so the
//! transport can frame it correctly (modern requests carry `_meta`).

use std::sync::atomic::{AtomicU8, Ordering};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::sync::OnceCell;
use tokio_util::sync::CancellationToken;

use super::jsonrpc::{self, METHOD_NOT_FOUND, UNSUPPORTED_PROTOCOL_VERSION};
use super::{CallError, CallOutcome, Era, Outbound, ToolInfo, ToolServer, Transport};

/// How long era negotiation waits for `server/discover` before assuming the
/// server does not understand it and falling back to the legacy handshake.
const DISCOVER_TIMEOUT: Duration = Duration::from_secs(5);
/// Connect failure cleanup is itself bounded, so a broken transport cannot
/// turn a connect deadline into an unbounded wait.
const CONNECT_CLOSE_TIMEOUT: Duration = Duration::from_secs(10);

const ACTIVE: u8 = 0;
const STOPPING: u8 = 1;

/// One connected server: its transport, negotiated era, and advertised tools.
pub struct Client {
    name: String,
    transport: Box<dyn Transport>,
    era: Era,
    tools: Vec<ToolInfo>,
    call_timeout: Duration,
    cancellation_grace: Duration,
    lifecycle: AtomicU8,
    close_done: OnceCell<()>,
}

impl Client {
    /// Connects to a server: negotiates the protocol era, then fetches its
    /// full tool list (paging through `nextCursor`). `connect_timeout` bounds
    /// the whole connect (probe, handshake, and tool listing together), not
    /// just the initial handshake. On any failure, including a timeout, the
    /// transport is closed before the error is returned so a stdio child
    /// never outlives a failed connect.
    pub async fn connect(
        name: String,
        transport: Box<dyn Transport>,
        connect_timeout: Duration,
        call_timeout: Duration,
        cancel: &CancellationToken,
    ) -> Result<Client, CallError> {
        Self::connect_with_grace(
            name,
            transport,
            connect_timeout,
            call_timeout,
            super::DEFAULT_CANCELLATION_GRACE,
            cancel,
        )
        .await
    }

    /// Connects with a caller-selected cooperative cancellation grace.
    pub async fn connect_with_grace(
        name: String,
        transport: Box<dyn Transport>,
        connect_timeout: Duration,
        call_timeout: Duration,
        cancellation_grace: Duration,
        cancel: &CancellationToken,
    ) -> Result<Client, CallError> {
        let outcome = tokio::time::timeout(connect_timeout, async {
            let era = negotiate_era(transport.as_ref(), connect_timeout, cancel).await?;
            let tools = fetch_all_tools(transport.as_ref(), &era, cancel).await?;
            Ok((era, tools))
        })
        .await;

        match outcome {
            Ok(Ok((era, tools))) => Ok(Client {
                name,
                transport,
                era,
                tools,
                call_timeout,
                cancellation_grace,
                lifecycle: AtomicU8::new(ACTIVE),
                close_done: OnceCell::new(),
            }),
            Ok(Err(error)) => {
                close_failed_connect(transport.as_ref()).await;
                Err(error)
            }
            Err(_) => {
                close_failed_connect(transport.as_ref()).await;
                Err(CallError::Timeout)
            }
        }
    }

    /// Sends `outbound`, cooperatively cancels it at `call_timeout`, then
    /// affords cancellation a bounded grace period. HTTP may abandon just
    /// this request after grace; stdio must stop the shared stream.
    async fn request_within_timeout(
        &self,
        outbound: Outbound,
        cancel: &CancellationToken,
    ) -> Result<Result<Value, jsonrpc::RpcError>, CallError> {
        if self.lifecycle.load(Ordering::Acquire) != ACTIVE {
            return Err(CallError::Stopping);
        }

        let child = cancel.child_token();
        let request = self.transport.request(outbound, &child);
        let mut request = std::pin::pin!(request);
        let deadline = tokio::time::sleep(self.call_timeout);
        let mut deadline = std::pin::pin!(deadline);
        let mut request_released = false;
        let mut caller_cancelled = tokio::select! {
            // A successful/RPC response that is already available is
            // authoritative. For stdio, however, transport-level Cancelled
            // racing caller cancellation only releases the local waiter; it
            // must still enter the shared-server grace path below.
            biased;
            outcome = &mut request => {
                if matches!(&outcome, Err(CallError::Cancelled))
                    && cancel.is_cancelled()
                    && self.transport.timeout_stops_server()
                {
                    request_released = true;
                    true
                } else {
                    return outcome;
                }
            },
            () = cancel.cancelled() => true,
            () = &mut deadline => false,
        };

        child.cancel();
        let grace = tokio::time::sleep(self.cancellation_grace);
        let mut grace = std::pin::pin!(grace);
        tokio::select! {
            // Verified completion wins if completion and grace are both ready.
            biased;
            outcome = &mut request, if !request_released => {
                match outcome {
                    // Stdio releasing its local waiter does not prove the
                    // shared server stopped the remote effect. Keep the grace
                    // and then stop the stream. HTTP cancellation is scoped to
                    // this request and can complete immediately.
                    Err(CallError::Cancelled) if self.transport.timeout_stops_server() => {
                        if caller_cancelled {
                            (&mut grace).await;
                        } else {
                            tokio::select! {
                                biased;
                                () = cancel.cancelled() => caller_cancelled = true,
                                () = &mut grace => {}
                            }
                        }
                    }
                    Err(CallError::Cancelled) => {
                        return if caller_cancelled || cancel.is_cancelled() {
                            Err(CallError::Cancelled)
                        } else {
                            Err(CallError::Timeout)
                        };
                    }
                    outcome => return outcome,
                }
            }
            () = cancel.cancelled(), if !caller_cancelled => {
                caller_cancelled = true;
                (&mut grace).await;
            }
            () = &mut grace => {}
        }

        if self.transport.timeout_stops_server() {
            let _ = self.lifecycle.compare_exchange(
                ACTIVE,
                STOPPING,
                Ordering::AcqRel,
                Ordering::Acquire,
            );
            self.close_transport().await;
        }
        if caller_cancelled {
            Err(CallError::Cancelled)
        } else {
            Err(CallError::Timeout)
        }
    }

    async fn close_transport(&self) {
        self.close_done
            .get_or_init(|| async { self.transport.close().await })
            .await;
    }

    pub fn era(&self) -> &Era {
        &self.era
    }

    pub fn tools(&self) -> &[ToolInfo] {
        &self.tools
    }

    /// Shuts the server down. Idempotent.
    pub async fn close(&self) {
        self.lifecycle.store(STOPPING, Ordering::Release);
        self.close_transport().await;
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
        let result = self.request_within_timeout(outbound, cancel).await?;
        let result = result.map_err(|error| CallError::Rpc {
            code: error.code,
            message: error.message,
        })?;
        jsonrpc::decode_call_result(&result, &self.era).map_err(CallError::Transport)
    }
}

async fn close_failed_connect(transport: &dyn Transport) {
    let _ = tokio::time::timeout(CONNECT_CLOSE_TIMEOUT, transport.close()).await;
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
        let message = match outcome {
            // A recognized RPC error, reported with just code and message:
            // `data` (which can carry the server's whole error payload) is
            // never included, and the message is bounded so a hostile or
            // buggy server cannot grow the status/HTTP-error text without
            // limit.
            Ok(Ok(Err(error))) => format!(
                "server/discover failed: {} {}",
                error.code,
                truncated(&error.message, MAX_DISCOVER_ERROR_BYTES)
            ),
            // `Ok(Ok(Ok(_)))` returned above and `Ok(Err(Timeout)) | Err(_)`
            // are legacy rejections handled by `is_legacy_rejection`, so the
            // only other case reaching here is a transport-level error.
            Ok(Err(other)) => format!(
                "server/discover failed: {}",
                truncated(&other.to_string(), MAX_DISCOVER_ERROR_BYTES)
            ),
            _ => unreachable!("legacy rejection cases are handled above"),
        };
        return Err(CallError::Transport(message));
    }

    legacy_handshake(transport, connect_timeout, cancel).await
}

/// How much of an RPC error message or transport error text is kept when
/// reporting a `server/discover` failure. Bytes, not chars, but the cut
/// always lands on a char boundary.
const MAX_DISCOVER_ERROR_BYTES: usize = 200;

/// `text` truncated to at most `max` bytes, cut on a char boundary.
fn truncated(text: &str, max: usize) -> &str {
    if text.len() <= max {
        return text;
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
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

/// The most `tools/list` pages a single connect will follow before giving up.
/// A well-behaved server pages a tool catalog in single digits to low
/// hundreds; this only guards against a server that never terminates paging.
const MAX_TOOLS_LIST_PAGES: usize = 1000;

/// Fetches every page of `tools/list`, following `nextCursor` until it is
/// `None`. Guards against a server that never terminates paging: a page
/// count over [`MAX_TOOLS_LIST_PAGES`] or a `nextCursor` repeating the cursor
/// just sent are both reported as transport failures instead of looping
/// forever.
async fn fetch_all_tools(
    transport: &dyn Transport,
    era: &Era,
    cancel: &CancellationToken,
) -> Result<Vec<ToolInfo>, CallError> {
    let mut tools = Vec::new();
    let mut cursor: Option<String> = None;
    for _ in 0..MAX_TOOLS_LIST_PAGES {
        let sent_cursor = cursor.clone();
        let mut params = json!({});
        if let Some(cursor) = &sent_cursor {
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
            Some(next) if Some(&next) == sent_cursor.as_ref() => {
                return Err(CallError::Transport(
                    "tools/list returned the same cursor twice".to_string(),
                ));
            }
            Some(next) => cursor = Some(next),
            None => return Ok(tools),
        }
    }
    Err(CallError::Transport(format!(
        "tools/list exceeded {MAX_TOOLS_LIST_PAGES} pages"
    )))
}

#[cfg(test)]
mod tests;
