//! `/mcp` report and login tests.

use super::*;

/// A controller whose builder carries the given MCP servers, each
/// reachable through `Controller::builder().mcp.servers` for `/mcp
/// login`'s validation without a real connection ever being attempted
/// (servers are `enabled: false`, so `connect_mcp` reports them as
/// `Disabled` instead of dialing out).
async fn controller_with_mcp_servers(
    workspace: &Path,
    sessions: &Path,
    servers: Vec<otto_core::config::McpServerRuntime>,
    dynamic_content: bool,
) -> Controller {
    let mut builder = fixtures::builder(workspace, sessions);
    builder.mcp = otto_core::config::McpRuntime {
        enabled: true,
        call_timeout_secs: 5,
        connect_timeout_secs: 1,
        servers,
    };
    let runtime = fixtures::initial_runtime(&builder);
    let session = builder.create_session(&runtime).expect("session");
    let runner = builder
        .build_runner(&session, &runtime)
        .await
        .expect("runner");
    let info = builder.runtime_info(&runtime);
    Controller::new(builder, dynamic_content, session, runner, info)
}

fn stdio_server(name: &str) -> otto_core::config::McpServerRuntime {
    otto_core::config::McpServerRuntime {
        name: name.to_string(),
        enabled: false,
        transport: McpTransport::Stdio {
            command: "otto-mcp-test-nonexistent-command".to_string(),
            args: Vec::new(),
            env: Vec::new(),
            cwd: ".".to_string(),
        },
        secrets: Vec::new(),
    }
}

fn http_server(name: &str, auth: McpAuth) -> otto_core::config::McpServerRuntime {
    otto_core::config::McpServerRuntime {
        name: name.to_string(),
        enabled: false,
        transport: McpTransport::Http {
            url: "https://mcp.example.com".to_string(),
            headers: Vec::new(),
            auth,
            oauth_client_id: None,
            oauth_scopes: Vec::new(),
        },
        secrets: Vec::new(),
    }
}

#[tokio::test]
async fn mcp_with_no_servers_reports_none_configured() {
    let workspace = tempfile::tempdir().expect("workspace");
    let sessions = tempfile::tempdir().expect("sessions");
    let controller = fixtures::controller(workspace.path(), sessions.path()).await;

    let (stdout, stderr) = session("/mcp\n/exit\n", &controller).await;
    assert!(stderr.is_empty(), "{stderr}");
    assert!(stdout.contains("No MCP servers configured."), "{stdout}");
}

#[tokio::test]
async fn mcp_with_malformed_arguments_prints_usage_on_stderr() {
    let workspace = tempfile::tempdir().expect("workspace");
    let sessions = tempfile::tempdir().expect("sessions");
    let controller = fixtures::controller(workspace.path(), sessions.path()).await;

    for input in ["/mcp bogus\n/exit\n", "/mcp login\n/exit\n"] {
        let (_, stderr) = session(input, &controller).await;
        assert!(stderr.contains(MCP_USAGE), "{input:?} -> {stderr}");
    }
}

#[tokio::test]
async fn mcp_login_for_an_unknown_server_reports_it_on_stderr() {
    let workspace = tempfile::tempdir().expect("workspace");
    let sessions = tempfile::tempdir().expect("sessions");
    let controller = controller_with_mcp_servers(
        workspace.path(),
        sessions.path(),
        vec![stdio_server("shell")],
        true,
    )
    .await;

    let (_, stderr) = session("/mcp login ghost\n/exit\n", &controller).await;
    assert!(stderr.contains("unknown MCP server: ghost"), "{stderr}");
}

#[tokio::test]
async fn mcp_login_for_a_stdio_server_reports_it_on_stderr() {
    let workspace = tempfile::tempdir().expect("workspace");
    let sessions = tempfile::tempdir().expect("sessions");
    let controller = controller_with_mcp_servers(
        workspace.path(),
        sessions.path(),
        vec![stdio_server("shell")],
        true,
    )
    .await;

    let (_, stderr) = session("/mcp login shell\n/exit\n", &controller).await;
    assert!(
        stderr
            .contains("shell is a stdio server; MCP login only applies to HTTP servers with OAuth"),
        "{stderr}"
    );
}

#[tokio::test]
async fn mcp_login_for_a_non_oauth_http_server_reports_no_login_required() {
    let workspace = tempfile::tempdir().expect("workspace");
    let sessions = tempfile::tempdir().expect("sessions");
    let controller = controller_with_mcp_servers(
        workspace.path(),
        sessions.path(),
        vec![http_server("docs", McpAuth::None)],
        true,
    )
    .await;

    let (_, stderr) = session("/mcp login docs\n/exit\n", &controller).await;
    assert!(
        stderr.contains("docs does not use OAuth; no login is required"),
        "{stderr}"
    );
}

#[tokio::test]
async fn mcp_login_is_unavailable_without_dynamic_content() {
    let workspace = tempfile::tempdir().expect("workspace");
    let sessions = tempfile::tempdir().expect("sessions");
    let controller = controller_with_mcp_servers(
        workspace.path(),
        sessions.path(),
        vec![http_server("docs", McpAuth::OAuth)],
        false,
    )
    .await;

    let (stdout, _) = session("/mcp login docs\n/exit\n", &controller).await;
    assert!(
        stdout.contains("MCP sign-in is unavailable in this session"),
        "{stdout}"
    );
}
