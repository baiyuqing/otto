//! The standalone `otto mcp login|logout <server>` CLI.
//!
//! Dispatched before the main flag set is parsed, since its argument grammar
//! is its own (`memory_command.rs` and `login.rs` follow the same pattern).
//! It resolves `[mcp]` on its own and never builds a runner or controller;
//! restarting Otto is what picks up a token this command wrote or removed.

use std::collections::HashMap;
use std::io::Write;
use std::path::Path;
use std::sync::Mutex;

use otto_core::config::{McpAuth, McpTransport, resolve_mcp};
use tokio_util::sync::CancellationToken;

use super::login::{SharedWriter, browser_opener};
use super::run::fail;

const USAGE: &str = "usage: otto mcp login <server> | otto mcp logout <server>";

/// Runs `otto mcp ...` and returns its exit code.
pub async fn run(
    args: &[String],
    stdout: &mut (dyn Write + Send),
    stderr: &mut (dyn Write + Send),
    lookup: &HashMap<String, String>,
    cancel: &CancellationToken,
) -> i32 {
    let (subcommand, name) = match args {
        [subcommand, name] if subcommand == "login" || subcommand == "logout" => {
            (subcommand.as_str(), name.as_str())
        }
        _ => return fail(stderr, USAGE),
    };

    let home = match super::run::resolve_home_for(lookup) {
        Ok(home) => home,
        Err(message) => return fail(stderr, &message),
    };
    let Ok((_, config_file)) = super::run::load_config_for("", false, &home) else {
        return fail(
            stderr,
            "load config: configuration is invalid or unavailable",
        );
    };
    let environment = super::run::config_environment_for(&config_file, lookup);
    let Ok(workspace) = super::sandbox_runtime::canonical_directory(Path::new(".")) else {
        return fail(stderr, "working directory is invalid or unavailable");
    };
    let Ok(mcp_config) = resolve_mcp(&config_file, &environment, &workspace.to_string_lossy())
    else {
        return fail(stderr, "mcp configuration is invalid or unavailable");
    };
    let Some(server) = mcp_config.servers.iter().find(|server| server.name == name) else {
        return fail(stderr, &format!("unknown MCP server: {name}"));
    };
    let McpTransport::Http {
        url,
        auth,
        oauth_client_id,
        oauth_scopes,
        ..
    } = &server.transport
    else {
        return fail(
            stderr,
            &format!("{name} is a stdio server; MCP login only applies to HTTP servers with OAuth"),
        );
    };
    if *auth != McpAuth::OAuth {
        return fail(
            stderr,
            &format!("{name} does not use OAuth; no login is required"),
        );
    }
    let token_path = crate::mcp::oauth::token_path(Path::new(&home), name);

    if subcommand == "logout" {
        return match crate::mcp::oauth::logout(&token_path) {
            Ok(true) => {
                let _ = writeln!(stdout, "logged out of {name}");
                0
            }
            Ok(false) => {
                let _ = writeln!(stdout, "no token stored for {name}");
                0
            }
            Err(error) => fail(stderr, &error.to_string()),
        };
    }

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
            let _ = writeln!(stdout, "logged in to {name}");
            0
        }
        Err(error) => fail(stderr, &error.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Writes `[mcp.servers.<name>]` config at the default path under `home`
    /// and returns `home`'s environment lookup for [`run`].
    fn config_with_server(home: &Path, name: &str, body: &str) -> HashMap<String, String> {
        let directory = home.join(".config").join("otto");
        std::fs::create_dir_all(&directory).expect("config dir");
        std::fs::write(
            directory.join("config.toml"),
            format!("[mcp.servers.{name}]\n{body}\n"),
        )
        .expect("write config");
        HashMap::from([("HOME".to_string(), home.to_string_lossy().into_owned())])
    }

    /// Runs `otto mcp ...` and returns its exit code, stdout and stderr.
    async fn mcp(args: &[&str], lookup: &HashMap<String, String>) -> (i32, String, String) {
        let arguments: Vec<String> = args.iter().map(|value| value.to_string()).collect();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let code = run(
            &arguments,
            &mut stdout,
            &mut stderr,
            lookup,
            &CancellationToken::new(),
        )
        .await;
        (
            code,
            String::from_utf8_lossy(&stdout).into_owned(),
            String::from_utf8_lossy(&stderr).into_owned(),
        )
    }

    #[tokio::test]
    async fn wrong_argument_counts_and_subcommands_report_usage() {
        let home = tempfile::tempdir().expect("home");
        let lookup = HashMap::from([(
            "HOME".to_string(),
            home.path().to_string_lossy().into_owned(),
        )]);
        for args in [
            vec![],
            vec!["login"],
            vec!["bogus", "docs"],
            vec!["login", "a", "b"],
        ] {
            let (code, _, stderr) = mcp(&args, &lookup).await;
            assert_ne!(code, 0, "{args:?}");
            assert!(stderr.contains(USAGE), "{args:?} -> {stderr}");
        }
    }

    #[tokio::test]
    async fn logging_in_to_an_unknown_server_reports_it() {
        let home = tempfile::tempdir().expect("home");
        let lookup = config_with_server(
            home.path(),
            "docs",
            "transport = \"stdio\"\ncommand = \"true\"\n",
        );

        let (code, _, stderr) = mcp(&["login", "ghost"], &lookup).await;
        assert_ne!(code, 0);
        assert!(stderr.contains("unknown MCP server: ghost"), "{stderr}");
    }

    #[tokio::test]
    async fn logging_in_to_a_stdio_server_is_rejected() {
        let home = tempfile::tempdir().expect("home");
        let lookup = config_with_server(
            home.path(),
            "shell",
            "transport = \"stdio\"\ncommand = \"true\"\n",
        );

        let (code, _, stderr) = mcp(&["login", "shell"], &lookup).await;
        assert_ne!(code, 0);
        assert!(
            stderr.contains(
                "shell is a stdio server; MCP login only applies to HTTP servers with OAuth"
            ),
            "{stderr}"
        );
    }

    #[tokio::test]
    async fn logging_in_to_a_non_oauth_http_server_is_rejected() {
        let home = tempfile::tempdir().expect("home");
        let lookup = config_with_server(
            home.path(),
            "docs",
            "transport = \"http\"\nurl = \"https://mcp.example.com\"\n",
        );

        let (code, _, stderr) = mcp(&["login", "docs"], &lookup).await;
        assert_ne!(code, 0);
        assert!(
            stderr.contains("docs does not use OAuth; no login is required"),
            "{stderr}"
        );
    }

    #[tokio::test]
    async fn logging_out_removes_the_stored_token() {
        let home = tempfile::tempdir().expect("home");
        let lookup = config_with_server(
            home.path(),
            "docs",
            "transport = \"http\"\nurl = \"https://mcp.example.com\"\nauth = \"oauth\"\n",
        );
        let token_path = crate::mcp::oauth::token_path(home.path(), "docs");
        std::fs::create_dir_all(token_path.parent().expect("parent")).expect("token dir");
        std::fs::write(&token_path, b"{}").expect("write token");

        let (code, stdout, stderr) = mcp(&["logout", "docs"], &lookup).await;
        assert_eq!(code, 0, "stderr = {stderr}");
        assert!(stdout.contains("logged out of docs"), "{stdout}");
        assert!(!token_path.exists());
    }

    #[tokio::test]
    async fn logging_out_without_a_stored_token_says_so() {
        let home = tempfile::tempdir().expect("home");
        let lookup = config_with_server(
            home.path(),
            "docs",
            "transport = \"http\"\nurl = \"https://mcp.example.com\"\nauth = \"oauth\"\n",
        );

        let (code, stdout, stderr) = mcp(&["logout", "docs"], &lookup).await;
        assert_eq!(code, 0, "stderr = {stderr}");
        assert!(stdout.contains("no token stored for docs"), "{stdout}");
    }
}
