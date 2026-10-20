//! The `--attach` clients: ACP and TUI against a running `otto serve` socket.

use super::*;

/// Attaches to the `otto serve` socket and returns the client's exit code.
#[allow(clippy::too_many_arguments)]
pub(super) async fn run_attach(
    options: &CliOptions,
    loaded: &startup::Loaded,
    configured: &startup::Configured,
    storage: &crate::runtime::storage::Storage,
    workspace_path: &str,
    terminal: bool,
    stdin: Box<dyn BufRead + Send + 'static>,
    stdout: &mut (dyn Write + Send),
    stderr: &mut (dyn Write + Send),
    cancel: &CancellationToken,
) -> i32 {
    // Neither client builds a provider runtime or opens a sandbox or
    // session: both use the `otto serve` on the socket, which is
    // `--socket`, `[server].socket` or the default, never a TCP address.
    let mut file = loaded.config.clone();
    file.server.listen.clear();
    let socket = resolve_server(&file, &configured.environment, &options.socket, "")
        .map_err(|error| error.to_string())
        .and_then(|server| std::path::absolute(&server.socket).map_err(|error| error.to_string()));
    let socket = match socket {
        Ok(socket) => socket,
        Err(message) => return fail(stderr, &loaded.boundary.redact(&message)),
    };
    let schedule = startup::open_schedule(storage, &loaded.home, &mut *stderr);
    if options.acp {
        return crate::acp::attach::run(
            &socket,
            PathBuf::from(workspace_path),
            schedule,
            Arc::new(crate::log::Logger::stderr()),
            stdin,
            stdout,
            stderr,
            cancel,
        )
        .await;
    }
    if !terminal {
        return fail(stderr, "--attach requires terminal stdin and stdout");
    }
    let start = if options.continue_last {
        crate::tui::attach::Start::Continue
    } else if !options.resume_path.is_empty() {
        crate::tui::attach::Start::Resume(options.resume_path.clone())
    } else {
        crate::tui::attach::Start::New
    };
    crate::tui::attach::run(&socket, workspace_path, start, schedule, cancel, stderr).await
}
