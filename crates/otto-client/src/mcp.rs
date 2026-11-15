//! Session MCP server status call.

use super::*;
use otto_core::wire::mcp::McpServer;

impl Client {
    /// `GET /v1/sessions/{id}/mcp`: the configured MCP servers and states.
    pub async fn mcp_servers(&self, session_id: &str) -> Result<Vec<McpServer>, Error> {
        let response = self
            .send(
                self.http
                    .get(format!("{BASE}/v1/sessions/{session_id}/mcp")),
            )
            .await?;
        #[derive(Deserialize)]
        struct Listed {
            servers: Vec<McpServer>,
        }
        Ok(decode::<Listed>(response).await?.servers)
    }
}
