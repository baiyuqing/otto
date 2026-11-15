//! ACP session operations over an already established, caller-owned transport.

use serde_json::{Value, json};
use tokio::io::{AsyncBufRead, AsyncWrite};
use tokio_util::sync::CancellationToken;

use super::acp_transport::{Connection, PermissionHandler};
use std::sync::Arc;

/// One ACP session. Process ownership and cancellation remain with the caller.
pub struct Session<R, W> {
    connection: Connection<R, W>,
    session_id: String,
}

impl<R: AsyncBufRead + Unpin, W: AsyncWrite + Unpin> Session<R, W> {
    pub async fn open(reader: R, writer: W, workspace: &str) -> Result<Self, String> {
        let mut connection = Connection::new(reader, writer);
        let initialization = connection
            .request(
                "initialize",
                json!({
                    "protocolVersion": 1,
                    "clientCapabilities": {},
                    "clientInfo": {"name": "otto", "version": env!("CARGO_PKG_VERSION")}
                }),
                |_| {},
            )
            .await?;
        if initialization
            .get("protocolVersion")
            .and_then(Value::as_u64)
            != Some(1)
        {
            return Err("ACP agent did not negotiate protocol version 1".into());
        }
        let session = connection
            .request(
                "session/new",
                json!({
                    "cwd": workspace,
                    "mcpServers": []
                }),
                |_| {},
            )
            .await?;
        let session_id = session
            .get("sessionId")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
            .ok_or("ACP agent returned no session id")?
            .to_owned();
        Ok(Self {
            connection,
            session_id,
        })
    }

    pub(crate) fn set_permission_handler(&mut self, handler: Arc<dyn PermissionHandler>) {
        self.connection
            .set_permission_handler(self.session_id.clone(), handler);
    }

    /// Deliver a prompt and stream updates from this session to the caller.
    #[cfg(test)]
    pub async fn prompt(
        &mut self,
        prompt: &str,
        update: impl FnMut(Value),
    ) -> Result<Value, String> {
        self.prompt_inner(prompt, None, update).await
    }

    pub(crate) async fn prompt_cancellable(
        &mut self,
        prompt: &str,
        cancel: &CancellationToken,
        update: impl FnMut(Value),
    ) -> Result<Value, String> {
        self.prompt_inner(prompt, Some(cancel), update).await
    }

    async fn prompt_inner(
        &mut self,
        prompt: &str,
        cancel: Option<&CancellationToken>,
        mut update: impl FnMut(Value),
    ) -> Result<Value, String> {
        let session_id = self.session_id.clone();
        self.connection
            .request_with_cancel(
                "session/prompt",
                json!({
                    "sessionId": session_id,
                    "prompt": [{"type": "text", "text": prompt}]
                }),
                cancel.map(|token| (token, session_id.as_str())),
                |message| {
                    if message.get("sessionId").and_then(Value::as_str) == Some(session_id.as_str())
                    {
                        update(message);
                    }
                },
            )
            .await
    }
}

#[cfg(test)]
#[path = "acp_session_tests.rs"]
mod tests;
