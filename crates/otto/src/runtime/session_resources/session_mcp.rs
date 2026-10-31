//! The MCP servers of one session, connected once.

use std::io::Write;
use std::sync::Arc;
use std::time::Duration;

use super::SessionResources;
use crate::runtime::builder::Builder;
use crate::runtime::wiring::ConnectedServer;

/// The servers one session connected. Every runner of the session reads
/// `/mcp` status from `servers` and builds its own tool adapters (with its
/// own output cap) over `connected`.
pub(crate) struct SessionMcp {
    pub(crate) servers: Arc<crate::mcp::Servers>,
    pub(crate) connected: Vec<ConnectedServer>,
}

impl Builder {
    /// Returns the session's connected MCP servers, connecting them on the
    /// first call. A call made while another is connecting waits for it.
    pub(crate) async fn session_mcp<'a>(
        &self,
        resources: &'a SessionResources,
        max_output: usize,
        cancellation_grace: Duration,
        warnings: &mut (dyn Write + Send),
        plugins: &crate::plugin::Snapshot,
    ) -> &'a SessionMcp {
        let mcp = resources
            .mcp
            .get_or_init(|| async {
                let (_tools, connected, servers) = self
                    .connect_mcp_with_plugins(max_output, cancellation_grace, warnings, plugins)
                    .await;
                if let Some(guard) = crate::tool::project_guard::Guard::for_config(
                    &self.config,
                    &self.home,
                    &self.workspace_path,
                ) && guard.check_trust
                {
                    guard.watch_mcp(&servers);
                }
                SessionMcp { servers, connected }
            })
            .await;
        // The session closed while the servers were connecting: nothing else
        // will close them.
        if resources.is_closed()
            && let Ok(runtime) = tokio::runtime::Handle::try_current()
        {
            let servers = Arc::clone(&mcp.servers);
            runtime.spawn(async move { servers.close().await });
        }
        mcp
    }
}
