//! The standalone `otto mcp login|logout <server>` CLI.
//!
//! Dispatched before the main flag set is parsed, since its argument grammar
//! is its own (`memory_command.rs` and `login.rs` follow the same pattern).
//! It resolves `[mcp]` on its own and never builds a runner or controller;
//! restarting Otto is what picks up a token this command wrote or removed.

use std::collections::{BTreeMap, HashMap};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use otto_core::config::{McpAuth, McpServer, McpTransport, resolve_mcp};
use tokio_util::sync::CancellationToken;

use super::login::{SharedWriter, browser_opener};
use super::run::fail;

const USAGE: &str = "usage: otto mcp login <server> | otto mcp logout <server> | otto mcp list | otto mcp add <server> --transport stdio --command CMD [--arg ARG...] [--env KEY=ENVVAR...] [--cwd DIR] | otto mcp add <server> --transport http --url URL [--header KEY=VALUE...] [--auth none|oauth] [--oauth-client-id ID] [--scope SCOPE...] | otto mcp remove <server> | otto mcp enable <server> | otto mcp disable <server>";

/// Runs `otto mcp ...` and returns its exit code.
pub async fn run(
    args: &[String],
    stdout: &mut (dyn Write + Send),
    stderr: &mut (dyn Write + Send),
    lookup: &HashMap<String, String>,
    cancel: &CancellationToken,
) -> i32 {
    let Some(subcommand) = args.first().map(String::as_str) else {
        return fail(stderr, USAGE);
    };
    match subcommand {
        "login" | "logout" => run_login_logout(args, stdout, stderr, lookup, cancel).await,
        "list" => run_list(args, stdout, stderr, lookup),
        "add" => run_add(args, stdout, stderr, lookup),
        "remove" => run_remove(args, stdout, stderr, lookup),
        "enable" => run_enabled(args, stdout, stderr, lookup, true),
        "disable" => run_enabled(args, stdout, stderr, lookup, false),
        _ => fail(stderr, USAGE),
    }
}

async fn run_login_logout(
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

fn config_path_for(lookup: &HashMap<String, String>) -> Result<PathBuf, String> {
    let home = super::run::resolve_home_for(lookup)?;
    Ok([home.as_str(), ".config", "otto", "config.toml"]
        .iter()
        .collect())
}

/// The config file's path, its parsed contents, and the exact bytes they were
/// parsed from.
///
/// [`save_editable_config`] passes those bytes back, so an edit another Otto
/// process wrote in between is reported instead of overwritten.
fn load_editable_config(
    lookup: &HashMap<String, String>,
) -> Result<(PathBuf, otto_core::config::File, Vec<u8>), String> {
    let path = config_path_for(lookup)?;
    match std::fs::read_to_string(&path) {
        Ok(text) => match otto_core::config::parse(&text) {
            Ok(file) => Ok((path, file, text.into_bytes())),
            Err(_) => Err("load config: configuration is invalid or unavailable".to_string()),
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Ok((path, otto_core::config::File::default(), Vec::new()))
        }
        Err(_) => Err("load config: configuration is invalid or unavailable".to_string()),
    }
}

fn save_editable_config(
    path: &Path,
    file: &otto_core::config::File,
    replacing: &[u8],
) -> Result<(), String> {
    crate::config::save(path, file, replacing).map_err(|error| format!("write config: {error}"))
}

fn valid_server_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 32
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
}

fn run_list(
    args: &[String],
    stdout: &mut (dyn Write + Send),
    stderr: &mut (dyn Write + Send),
    lookup: &HashMap<String, String>,
) -> i32 {
    if args.len() != 1 {
        return fail(stderr, USAGE);
    }
    let Ok((_, file, _)) = load_editable_config(lookup) else {
        return fail(
            stderr,
            "load config: configuration is invalid or unavailable",
        );
    };
    if file.mcp.servers.is_empty() {
        let _ = writeln!(stdout, "no MCP servers configured");
        return 0;
    }
    for (name, server) in &file.mcp.servers {
        let enabled = if server.enabled.unwrap_or(true) {
            "enabled"
        } else {
            "disabled"
        };
        let auth = if server.auth.as_deref() == Some("oauth") {
            "oauth"
        } else {
            "none"
        };
        let _ = writeln!(stdout, "{name}\t{}\t{enabled}\t{auth}", server.transport);
    }
    0
}

#[derive(Default)]
struct AddArgs {
    name: String,
    transport: String,
    command: Option<String>,
    args: Vec<String>,
    env: BTreeMap<String, String>,
    cwd: Option<String>,
    url: Option<String>,
    headers: BTreeMap<String, String>,
    auth: Option<String>,
    oauth_client_id: Option<String>,
    oauth_scopes: Vec<String>,
}

fn parse_add(args: &[String]) -> Result<AddArgs, String> {
    let [_, name, rest @ ..] = args else {
        return Err(USAGE.to_string());
    };
    if !valid_server_name(name) {
        return Err("invalid MCP server name: allowed [A-Za-z0-9_-], 1..32 bytes".to_string());
    }
    let mut parsed = AddArgs {
        name: name.clone(),
        ..AddArgs::default()
    };
    let mut index = 0;
    while index < rest.len() {
        let flag = rest[index].as_str();
        let Some(value) = rest.get(index + 1) else {
            return Err(USAGE.to_string());
        };
        match flag {
            "--transport" => parsed.transport = value.clone(),
            "--command" => parsed.command = Some(value.clone()),
            "--arg" => parsed.args.push(value.clone()),
            "--env" => {
                let Some((key, var)) = value.split_once('=') else {
                    return Err("--env must be KEY=ENVVAR".to_string());
                };
                parsed.env.insert(key.to_string(), format!("${{{var}}}"));
            }
            "--cwd" => parsed.cwd = Some(value.clone()),
            "--url" => parsed.url = Some(value.clone()),
            "--header" => {
                let Some((key, header_value)) = value.split_once('=') else {
                    return Err("--header must be KEY=VALUE".to_string());
                };
                parsed
                    .headers
                    .insert(key.to_string(), header_value.to_string());
            }
            "--auth" => parsed.auth = Some(value.clone()),
            "--oauth-client-id" => parsed.oauth_client_id = Some(value.clone()),
            "--scope" => parsed.oauth_scopes.push(value.clone()),
            _ => return Err(USAGE.to_string()),
        }
        index += 2;
    }
    match parsed.transport.as_str() {
        "stdio" if parsed.command.is_some() => Ok(parsed),
        "stdio" => Err("--command is required for stdio MCP servers".to_string()),
        "http" if parsed.url.is_some() => Ok(parsed),
        "http" => Err("--url is required for HTTP MCP servers".to_string()),
        _ => Err("--transport must be stdio or http".to_string()),
    }
}

fn server_from_add(parsed: AddArgs) -> McpServer {
    match parsed.transport.as_str() {
        "stdio" => McpServer {
            transport: "stdio".to_string(),
            command: parsed.command,
            args: (!parsed.args.is_empty()).then_some(parsed.args),
            env: (!parsed.env.is_empty()).then_some(parsed.env),
            cwd: parsed.cwd,
            ..McpServer::default()
        },
        "http" => McpServer {
            transport: "http".to_string(),
            url: parsed.url,
            headers: (!parsed.headers.is_empty()).then_some(parsed.headers),
            auth: parsed.auth,
            oauth_client_id: parsed.oauth_client_id,
            oauth_scopes: (!parsed.oauth_scopes.is_empty()).then_some(parsed.oauth_scopes),
            ..McpServer::default()
        },
        _ => McpServer::default(),
    }
}

fn run_add(
    args: &[String],
    stdout: &mut (dyn Write + Send),
    stderr: &mut (dyn Write + Send),
    lookup: &HashMap<String, String>,
) -> i32 {
    let parsed = match parse_add(args) {
        Ok(parsed) => parsed,
        Err(message) => return fail(stderr, &message),
    };
    let Ok((path, mut file, original)) = load_editable_config(lookup) else {
        return fail(
            stderr,
            "load config: configuration is invalid or unavailable",
        );
    };
    if file.mcp.servers.contains_key(&parsed.name) {
        return fail(
            stderr,
            &format!("MCP server already exists: {}", parsed.name),
        );
    }
    let name = parsed.name.clone();
    let uses_oauth = parsed.transport == "http" && parsed.auth.as_deref() == Some("oauth");
    file.mcp
        .servers
        .insert(name.clone(), server_from_add(parsed));
    if let Err(message) = save_editable_config(&path, &file, &original) {
        return fail(stderr, &message);
    }
    let _ = writeln!(
        stdout,
        "added MCP server {name}; restart Otto to connect it"
    );
    if uses_oauth {
        let _ = writeln!(
            stdout,
            "run 'otto mcp login {name}' before restarting if this server needs OAuth"
        );
    }
    0
}

fn run_remove(
    args: &[String],
    stdout: &mut (dyn Write + Send),
    stderr: &mut (dyn Write + Send),
    lookup: &HashMap<String, String>,
) -> i32 {
    let [_, name] = args else {
        return fail(stderr, USAGE);
    };
    let Ok((path, mut file, original)) = load_editable_config(lookup) else {
        return fail(
            stderr,
            "load config: configuration is invalid or unavailable",
        );
    };
    if file.mcp.servers.remove(name).is_none() {
        return fail(stderr, &format!("unknown MCP server: {name}"));
    }
    if let Err(message) = save_editable_config(&path, &file, &original) {
        return fail(stderr, &message);
    }
    let _ = writeln!(
        stdout,
        "removed MCP server {name}; restart Otto to apply changes"
    );
    0
}

fn run_enabled(
    args: &[String],
    stdout: &mut (dyn Write + Send),
    stderr: &mut (dyn Write + Send),
    lookup: &HashMap<String, String>,
    enabled: bool,
) -> i32 {
    let [_, name] = args else {
        return fail(stderr, USAGE);
    };
    let Ok((path, mut file, original)) = load_editable_config(lookup) else {
        return fail(
            stderr,
            "load config: configuration is invalid or unavailable",
        );
    };
    let Some(server) = file.mcp.servers.get_mut(name) else {
        return fail(stderr, &format!("unknown MCP server: {name}"));
    };
    server.enabled = Some(enabled);
    if let Err(message) = save_editable_config(&path, &file, &original) {
        return fail(stderr, &message);
    }
    let verb = if enabled { "enabled" } else { "disabled" };
    let _ = writeln!(
        stdout,
        "{verb} MCP server {name}; restart Otto to apply changes"
    );
    0
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

    #[tokio::test]
    async fn add_stdio_server_creates_config_without_literal_secret() {
        let home = tempfile::tempdir().expect("home");
        let lookup = HashMap::from([(
            "HOME".to_string(),
            home.path().to_string_lossy().into_owned(),
        )]);

        let (code, stdout, stderr) = mcp(
            &[
                "add",
                "github",
                "--transport",
                "stdio",
                "--command",
                "npx",
                "--arg",
                "-y",
                "--arg",
                "@modelcontextprotocol/server-github",
                "--env",
                "GITHUB_TOKEN=GITHUB_TOKEN",
            ],
            &lookup,
        )
        .await;
        assert_eq!(code, 0, "stderr = {stderr}");
        assert!(stdout.contains("added MCP server github"), "{stdout}");
        assert!(stdout.contains("restart Otto"), "{stdout}");

        let path = home.path().join(".config/otto/config.toml");
        let text = std::fs::read_to_string(path).expect("config");
        assert!(text.contains("[mcp.servers.github]"), "{text}");
        assert!(text.contains("transport = \"stdio\""), "{text}");
        assert!(text.contains("command = \"npx\""), "{text}");
        assert!(
            text.contains("args = [\"-y\", \"@modelcontextprotocol/server-github\"]"),
            "{text}"
        );
        assert!(
            text.contains("GITHUB_TOKEN = \"${GITHUB_TOKEN}\""),
            "{text}"
        );
    }

    #[tokio::test]
    async fn add_http_oauth_server_and_list_it() {
        let home = tempfile::tempdir().expect("home");
        let lookup = HashMap::from([(
            "HOME".to_string(),
            home.path().to_string_lossy().into_owned(),
        )]);

        let (code, stdout, stderr) = mcp(
            &[
                "add",
                "docs",
                "--transport",
                "http",
                "--url",
                "https://mcp.example.com/mcp",
                "--auth",
                "oauth",
                "--scope",
                "mcp:tools",
            ],
            &lookup,
        )
        .await;
        assert_eq!(code, 0, "stderr = {stderr}");
        assert!(stdout.contains("run 'otto mcp login docs'"), "{stdout}");

        let (code, stdout, stderr) = mcp(&["list"], &lookup).await;
        assert_eq!(code, 0, "stderr = {stderr}");
        assert!(stdout.contains("docs\thttp\tenabled\toauth"), "{stdout}");

        let text =
            std::fs::read_to_string(home.path().join(".config/otto/config.toml")).expect("config");
        assert!(text.contains("auth = \"oauth\""), "{text}");
        assert!(text.contains("oauth_scopes = [\"mcp:tools\"]"), "{text}");
    }

    #[test]
    fn writing_refuses_a_config_that_changed_since_it_was_read() {
        let home = tempfile::tempdir().expect("home");
        let lookup = config_with_server(
            home.path(),
            "docs",
            "transport = \"http\"\nurl = \"https://mcp.example.com\"\n",
        );
        let (path, mut file, original) = load_editable_config(&lookup).expect("load");
        file.mcp.servers.remove("docs");

        let concurrent = "default_profile = \"written by another otto\"\n";
        std::fs::write(&path, concurrent).expect("concurrent write");

        let error = save_editable_config(&path, &file, &original).expect_err("stale write");

        assert!(error.contains("changed on disk"), "{error}");
        assert_eq!(std::fs::read_to_string(&path).expect("read"), concurrent);
    }

    #[tokio::test]
    async fn remove_enable_and_disable_update_existing_server() {
        let home = tempfile::tempdir().expect("home");
        let lookup = config_with_server(
            home.path(),
            "docs",
            "transport = \"http\"\nurl = \"https://mcp.example.com\"\n",
        );

        let (code, stdout, stderr) = mcp(&["disable", "docs"], &lookup).await;
        assert_eq!(code, 0, "stderr = {stderr}");
        assert!(stdout.contains("disabled MCP server docs"), "{stdout}");
        let text =
            std::fs::read_to_string(home.path().join(".config/otto/config.toml")).expect("config");
        assert!(text.contains("enabled = false"), "{text}");

        let (code, stdout, stderr) = mcp(&["enable", "docs"], &lookup).await;
        assert_eq!(code, 0, "stderr = {stderr}");
        assert!(stdout.contains("enabled MCP server docs"), "{stdout}");
        let text =
            std::fs::read_to_string(home.path().join(".config/otto/config.toml")).expect("config");
        assert!(text.contains("enabled = true"), "{text}");

        let (code, stdout, stderr) = mcp(&["remove", "docs"], &lookup).await;
        assert_eq!(code, 0, "stderr = {stderr}");
        assert!(stdout.contains("removed MCP server docs"), "{stdout}");
        let text =
            std::fs::read_to_string(home.path().join(".config/otto/config.toml")).expect("config");
        assert!(!text.contains("mcp.servers.docs"), "{text}");
    }

    #[tokio::test]
    async fn add_rejects_existing_server_and_bad_names() {
        let home = tempfile::tempdir().expect("home");
        let lookup = config_with_server(
            home.path(),
            "docs",
            "transport = \"http\"\nurl = \"https://mcp.example.com\"\n",
        );

        let (code, _, stderr) = mcp(
            &[
                "add",
                "docs",
                "--transport",
                "http",
                "--url",
                "https://other.example.com/mcp",
            ],
            &lookup,
        )
        .await;
        assert_ne!(code, 0);
        assert!(
            stderr.contains("MCP server already exists: docs"),
            "{stderr}"
        );

        let (code, _, stderr) = mcp(
            &[
                "add",
                "bad.name",
                "--transport",
                "stdio",
                "--command",
                "true",
            ],
            &lookup,
        )
        .await;
        assert_ne!(code, 0);
        assert!(stderr.contains("invalid MCP server name"), "{stderr}");
    }
}
