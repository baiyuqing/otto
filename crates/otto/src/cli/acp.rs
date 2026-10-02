//! `otto acp`: composition and lifecycle around [`crate::acp::serve`].
//!
//! The composition root (`run.rs`) has already opened the workspace sandbox
//! and built the `Builder`; this module serves the connection, then closes
//! every session in the same order the REPL exit path uses: migrate (only
//! after a lease-holding SIGTERM), close MCP, close the controller, close the
//! sandbox.

use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::sync::Arc;

use otto_core::config::resolve::Runtime;
use tokio_util::sync::CancellationToken;

use crate::acp;
use crate::app::SandboxControl;

use super::runtime_builder::Builder;
use super::sandbox_switch::{SandboxReloader, SandboxSwitch};
use super::terminate::Terminate;

fn fail(stderr: &mut (dyn Write + Send), message: &str) -> i32 {
    let _ = writeln!(stderr, "otto: {message}");
    1
}

pub struct AcpOptions<'a> {
    pub builder: Arc<Builder>,
    pub runtime: Runtime,
    /// The canonical workspace.
    pub workspace: PathBuf,
    /// The process sandbox; [`run`] owns closing it.
    pub control: Arc<SandboxSwitch>,
    pub reloader: Option<Arc<SandboxReloader>>,
    /// Set to "serving" so a lease-less SIGTERM cancels instead of killing
    /// the process, and read afterwards to decide whether to migrate.
    pub terminate: &'a Terminate,
}

/// Serves ACP on `stdin`/`stdout` and returns the process exit code.
pub async fn run(
    options: AcpOptions<'_>,
    stdin: Box<dyn BufRead + Send + 'static>,
    stdout: &mut (dyn Write + Send),
    stderr: &mut (dyn Write + Send),
    cancel: &CancellationToken,
) -> i32 {
    let AcpOptions {
        builder,
        runtime,
        workspace,
        control,
        reloader,
        terminate,
    } = options;
    terminate.set_serve();
    let config = acp::Config {
        builder,
        runtime,
        workspace,
        sandbox: reloader.map(|reloader| reloader as Arc<dyn SandboxControl>),
    };
    let controllers = acp::serve(config, stdin, stdout, cancel).await;
    cancel.cancel();

    let mut close_error = None;
    for controller in controllers {
        if terminate.migrating() {
            for warning in controller.migrate().await {
                let _ = writeln!(stderr, "warning: {warning}");
            }
        }
        controller.close_mcp().await;
        let closing = Arc::clone(&controller);
        match tokio::task::spawn_blocking(move || closing.close()).await {
            Ok(Ok(())) => {}
            Ok(Err(message)) => close_error = Some(message),
            Err(join) => close_error = Some(join.to_string()),
        }
    }
    let sandbox_error = control.close().await.err();
    if let Some(message) = close_error {
        return fail(stderr, &format!("acp: {message}"));
    }
    if sandbox_error.is_some() {
        return fail(stderr, "close sandbox: sandbox runtime close failed");
    }
    0
}
