//! ACP JSON-RPC over caller-owned streams.
//!
//! Process creation, confinement and lifecycle belong to the caller. Incoming
//! requests are rejected until an explicit permission bridge is installed.

use serde_json::{Value, json};
use std::sync::Arc;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio_util::sync::CancellationToken;

const MAX_MESSAGE_BYTES: usize = 8 * 1024 * 1024;

#[async_trait::async_trait]
pub(crate) trait PermissionHandler: Send + Sync {
    async fn decide(&self, params: Value) -> Value;
}

/// A sequential ACP connection; only one request may be in flight.
pub struct Connection<R, W> {
    reader: R,
    writer: W,
    next_id: u64,
    permission_handler: Option<Arc<dyn PermissionHandler>>,
    permission_session_id: String,
}

impl<R: AsyncBufRead + Unpin, W: AsyncWrite + Unpin> Connection<R, W> {
    pub fn new(reader: R, writer: W) -> Self {
        Self {
            reader,
            writer,
            next_id: 1,
            permission_handler: None,
            permission_session_id: String::new(),
        }
    }

    pub(crate) fn set_permission_handler(
        &mut self,
        session_id: String,
        handler: Arc<dyn PermissionHandler>,
    ) {
        self.permission_session_id = session_id;
        self.permission_handler = Some(handler);
    }

    /// Send a request, delivering session updates while awaiting its response.
    pub async fn request(
        &mut self,
        method: &str,
        params: Value,
        update: impl FnMut(Value),
    ) -> Result<Value, String> {
        self.request_with_cancel(method, params, None, update).await
    }

    /// Cancel an active session cooperatively and continue reading until it settles.
    pub(crate) async fn request_with_cancel(
        &mut self,
        method: &str,
        params: Value,
        cancellation: Option<(&CancellationToken, &str)>,
        mut update: impl FnMut(Value),
    ) -> Result<Value, String> {
        let mut cancellation_sent = false;
        let id = self.next_id;
        self.next_id = id.checked_add(1).ok_or("ACP request id exhausted")?;
        self.write(&json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}))
            .await?;
        let mut bytes = Vec::new();
        loop {
            let read = if let Some((cancel, session_id)) =
                cancellation.filter(|_| !cancellation_sent)
            {
                let result = {
                    let mut limited =
                        (&mut self.reader).take((MAX_MESSAGE_BYTES + 1 - bytes.len()) as u64);
                    tokio::select! {
                        result = limited.read_until(b'\n', &mut bytes) => Some(result),
                        () = cancel.cancelled() => None,
                    }
                };
                match result {
                    Some(result) => result.map_err(|e| e.to_string())?,
                    None => {
                        self.write(&json!({"jsonrpc":"2.0", "method":"session/cancel", "params":{"sessionId":session_id}})).await?;
                        cancellation_sent = true;
                        continue;
                    }
                }
            } else {
                (&mut self.reader)
                    .take((MAX_MESSAGE_BYTES + 1 - bytes.len()) as u64)
                    .read_until(b'\n', &mut bytes)
                    .await
                    .map_err(|e| e.to_string())?
            };
            if read == 0 {
                return Err("ACP agent closed the connection".into());
            }
            if bytes.len() > MAX_MESSAGE_BYTES || bytes.last() != Some(&b'\n') {
                return Err("ACP message exceeds limit or is incomplete".into());
            }
            let message: Value =
                serde_json::from_slice(&bytes).map_err(|e| format!("invalid ACP message: {e}"))?;
            bytes.clear();
            if message.get("id") == Some(&json!(id)) && message.get("method").is_none() {
                if let Some(error) = message.get("error") {
                    return Err(format!("ACP request failed: {error}"));
                }
                return message
                    .get("result")
                    .cloned()
                    .ok_or_else(|| "ACP response has no result".into());
            }
            if let Some(method) = message.get("method").and_then(Value::as_str) {
                if let Some(request_id) = message.get("id") {
                    let error = if method == "session/request_permission" {
                        let params = message.get("params").cloned().unwrap_or(Value::Null);
                        let result = match &self.permission_handler {
                            Some(handler)
                                if params.get("sessionId").and_then(Value::as_str)
                                    == Some(self.permission_session_id.as_str()) =>
                            {
                                handler.decide(params).await
                            }
                            _ => json!({"outcome":{"outcome":"cancelled"}}),
                        };
                        json!({"jsonrpc":"2.0","id":request_id,"result":result})
                    } else {
                        json!({"jsonrpc":"2.0","id":request_id,"error":{"code":-32601,"message":"Method not supported"}})
                    };
                    self.write(&error).await?;
                } else if method == "session/update" {
                    update(message.get("params").cloned().unwrap_or(Value::Null));
                }
            }
        }
    }

    async fn write(&mut self, message: &Value) -> Result<(), String> {
        let mut bytes = serde_json::to_vec(message).map_err(|e| e.to_string())?;
        bytes.push(b'\n');
        self.writer
            .write_all(&bytes)
            .await
            .map_err(|e| e.to_string())?;
        self.writer.flush().await.map_err(|e| e.to_string())
    }
}

#[cfg(test)]
#[path = "acp_transport_tests.rs"]
mod tests;
