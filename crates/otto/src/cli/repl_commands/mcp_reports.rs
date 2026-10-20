//! `/mcp` status report and OAuth login.

use super::*;

/// One line per configured server, in configuration order.
pub(crate) fn mcp_report(controller: &Controller) -> String {
    use crate::app::management::{Reply, Request};
    match controller.management(Request::parse("mcp", "").expect("valid MCP status request")) {
        Ok(Reply::McpStatus(rows)) => format_mcp_report(&rows),
        Err(error) => format!("/mcp: {error}"),
        _ => unreachable!("MCP status request returned a different management reply"),
    }
}

/// The formatting `mcp_report` applies to `Controller::mcp`'s status rows.
/// A free function so the per-state line format has a test that does not
/// need a runner with a real MCP server behind it.
pub(super) fn format_mcp_report(servers: &[crate::mcp::ServerStatus]) -> String {
    if servers.is_empty() {
        return "No MCP servers configured.".to_string();
    }
    let mut out = "MCP servers:".to_string();
    for server in servers {
        let era = match &server.era {
            Some(Era::Modern) => "modern".to_string(),
            Some(Era::Legacy(version)) => format!("legacy {version}"),
            None => "-".to_string(),
        };
        let state = match &server.state {
            ServerState::Connected { tools } => format!("connected ({tools} tools)"),
            ServerState::Connecting => "connecting".to_string(),
            ServerState::Disabled => "disabled".to_string(),
            ServerState::NeedsLogin => "needs login".to_string(),
            ServerState::Failed(message) => format!("failed: {message}"),
        };
        let _ = write!(
            out,
            "\n- {}: {} ({}, {})",
            server.name, state, server.transport, era
        );
    }
    out
}

/// Runs the OAuth flow for one configured HTTP server and persists its token.
async fn mcp_login(
    controller: &Controller,
    name: &str,
    stdout: &mut (dyn Write + Send),
    stderr: &mut (dyn Write + Send),
    cancel: &CancellationToken,
) -> Result<(), Error> {
    if !controller.dynamic_content() {
        let _ = writeln!(stdout, "MCP sign-in is unavailable in this session");
        return Ok(());
    }
    let builder = controller.builder();
    let Some(server) = builder
        .mcp
        .servers
        .iter()
        .find(|server| server.name == name)
    else {
        let _ = writeln!(stderr, "unknown MCP server: {name}");
        return Ok(());
    };
    let McpTransport::Http {
        url,
        auth,
        oauth_client_id,
        oauth_scopes,
        ..
    } = &server.transport
    else {
        let _ = writeln!(
            stderr,
            "{name} is a stdio server; MCP login only applies to HTTP servers with OAuth"
        );
        return Ok(());
    };
    if *auth != McpAuth::OAuth {
        let _ = writeln!(stderr, "{name} does not use OAuth; no login is required");
        return Ok(());
    }
    let token_path = crate::mcp::oauth::token_path(Path::new(&builder.home), name);
    let shared: SharedWriter<'_> = Mutex::new(stdout);
    let opener = browser_opener(&shared);
    let request = crate::mcp::oauth::LoginRequest {
        server: name,
        url,
        client_id: oauth_client_id.as_deref(),
        scopes: oauth_scopes,
        ports: &crate::auth::oauth::LOOPBACK_PORTS,
        token_path: &token_path,
    };
    let result = crate::mcp::oauth::login(request, cancel, &opener).await;
    drop(opener);
    let stdout = shared
        .into_inner()
        .unwrap_or_else(|poison| poison.into_inner());
    match result {
        Ok(()) => {
            let _ = writeln!(
                stdout,
                "Signed in to {name}. Restart Otto to use the new credentials."
            );
            Ok(())
        }
        Err(error) => Err(command_error("/mcp login", error)),
    }
}

/// A free function, rather than a `Repl` method, for the same reason as
/// [`repl_memory_command`].
pub(crate) async fn repl_mcp_command(
    controller: &Controller,
    args: &str,
    stdout: &mut (dyn Write + Send),
    stderr: &mut (dyn Write + Send),
    cancel: &CancellationToken,
) -> Result<(), Error> {
    let fields: Vec<&str> = args.split_whitespace().collect();
    match fields.as_slice() {
        [] => {
            let _ = writeln!(stdout, "{}", mcp_report(controller));
            Ok(())
        }
        ["login", name] => mcp_login(controller, name, stdout, stderr, cancel).await,
        _ => {
            let _ = writeln!(stderr, "{MCP_USAGE}");
            Ok(())
        }
    }
}
