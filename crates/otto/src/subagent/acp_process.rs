//! ACP adapter execution through Otto's process-group manager.

use std::path::Path;
use std::time::Duration;

use serde_json::Value;
use tokio::io::BufReader;
use tokio_util::sync::CancellationToken;

use super::acp_session::Session;
use super::acp_transport::PermissionHandler;
use crate::sandbox::process::{Manager, Spec};
use std::sync::Arc;

/// Run one external ACP turn. The caller owns executable authorization and
/// agent-specific permission configuration; the manager owns the process group.
pub(crate) async fn run(
    executable: &Path,
    workspace: &Path,
    environment: Vec<String>,
    prompt: &str,
    cancel: &CancellationToken,
    permission_handler: Option<Arc<dyn PermissionHandler>>,
    mut update: impl FnMut(Value),
) -> Result<Value, String> {
    let path = executable.to_str().ok_or("adapter path is not UTF-8")?;
    let cwd = workspace.to_str().ok_or("workspace path is not UTF-8")?;
    let spec = Spec {
        path: path.to_owned(),
        args: Vec::new(),
        directory: workspace.to_path_buf(),
        environment,
        stdin: None,
    };
    let manager = Manager::default();
    manager.run_interactive(spec, cancel, Duration::from_secs(2), |stdout, stdin| async {
        let session = tokio::select! {
            session = Session::open(BufReader::new(stdout), stdin, cwd) => session,
            () = cancel.cancelled() => Err("ACP task cancelled during session setup".to_owned()),
        };
        let result = match session {
            Err(error) => Err(error),
            Ok(mut session) => {
                if let Some(handler) = permission_handler { session.set_permission_handler(handler); }
                let operation = session.prompt_cancellable(prompt, cancel, &mut update);
                tokio::pin!(operation);
                tokio::select! {
                    result = &mut operation => result,
                    () = cancel.cancelled() => tokio::time::timeout(Duration::from_secs(5), &mut operation)
                        .await.unwrap_or_else(|_| Err("ACP cancellation timed out".to_owned())),
                }
            }
        };
        Ok(result)
    }).await.map_err(|error| error.to_string())?
}

#[cfg(test)]
#[path = "acp_process_tests.rs"]
mod tests;
