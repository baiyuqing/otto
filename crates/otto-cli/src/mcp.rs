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

use super::run::fail;
use otto::cli::login::{SharedWriter, browser_opener};

const USAGE: &str = "usage: otto mcp login <server> | otto mcp logout <server> | otto mcp \
    list | otto mcp add <server> --transport stdio --command CMD [--arg \
    ARG...] [--env KEY=ENVVAR...] [--cwd DIR] | otto mcp add <server> \
    --transport http --url URL [--header KEY=VALUE...] [--auth none|oauth] \
    [--oauth-client-id ID] [--scope SCOPE...] | otto mcp remove <server> | \
    otto mcp enable <server> | otto mcp disable <server>";

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
    let Ok(workspace) = otto::fsops::canonical_directory(Path::new(".")) else {
        return fail(stderr, "working directory is invalid or unavailable");
    };
    let Ok(mcp_config) = resolve_mcp(&config_file, &environment, &workspace.to_string_lossy())
    else {
        return fail(stderr, "mcp configuration is invalid or unavailable");
    };
    let plugins = otto::plugin::Snapshot::load(Path::new(&home), &config_file, &environment);
    let Some(server) = mcp_config
        .servers
        .iter()
        .chain(plugins.servers.iter())
        .find(|server| {
            server.name == name
                || plugins
                    .guards
                    .get(&server.name)
                    .is_some_and(|g| g.logical_name == name)
        })
    else {
        return fail(stderr, &format!("unknown MCP server: {name}"));
    };
    let name = server.name.as_str();
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
    let token_path = otto::mcp::oauth::token_path(Path::new(&home), name);

    if subcommand == "logout" {
        return match otto::mcp::oauth::logout(&token_path) {
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
    let request = otto::mcp::oauth::LoginRequest {
        server: name,
        url,
        client_id: oauth_client_id.as_deref(),
        scopes: oauth_scopes,
        ports: &otto::auth::oauth::LOOPBACK_PORTS,
        token_path: &token_path,
    };
    let result = otto::mcp::oauth::login(request, cancel, &opener).await;
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
/// [`write_edited_config`] passes those bytes back, so an edit another Otto
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

/// Applies `edit` to the text [`load_editable_config`] read as `original` and
/// writes the result in its place. `edit` changes one key or table in the
/// text (see `otto_core::config::edit`), so the rest of the file keeps its
/// bytes, comments included.
fn write_edited_config(
    path: &Path,
    original: &[u8],
    edit: impl FnOnce(&str) -> Result<String, otto_core::config::ConfigError>,
) -> Result<(), String> {
    let updated = edit(&String::from_utf8_lossy(original))
        .map_err(|error| format!("edit config: {error}"))?;
    otto::config::write_bytes(path, original, updated.as_bytes())
        .map_err(|error| format!("write config: {error}"))
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
    let home = match super::run::resolve_home_for(lookup) {
        Ok(home) => home,
        Err(e) => return fail(stderr, &e),
    };
    let environment = super::run::config_environment_for(&file, lookup);
    let plugins = otto::plugin::Snapshot::load(Path::new(&home), &file, &environment);
    if file.mcp.servers.is_empty() && plugins.servers.is_empty() {
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
    for server in &plugins.servers {
        let logical = &plugins.guards[&server.name].logical_name;
        let (transport, auth) = match &server.transport {
            McpTransport::Stdio { .. } => ("stdio", "none"),
            McpTransport::Http { auth, .. } => (
                "http",
                if *auth == McpAuth::OAuth {
                    "oauth"
                } else {
                    "none"
                },
            ),
        };
        let _ = writeln!(
            stdout,
            "{logical} ({})\t{transport}\t{}\t{auth}",
            server.name,
            if server.enabled {
                "enabled"
            } else {
                "disabled"
            }
        );
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
    let Ok((path, file, original)) = load_editable_config(lookup) else {
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
    let body = match toml::Table::try_from(server_from_add(parsed)) {
        Ok(body) => body,
        Err(error) => return fail(stderr, &format!("edit config: {error}")),
    };
    let written = write_edited_config(&path, &original, |text| {
        otto_core::config::edit::insert_table(text, &["mcp", "servers", &name], body)
    });
    if let Err(message) = written {
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
    let Ok((path, file, original)) = load_editable_config(lookup) else {
        return fail(
            stderr,
            "load config: configuration is invalid or unavailable",
        );
    };
    if !file.mcp.servers.contains_key(name) {
        return fail(stderr, &format!("unknown MCP server: {name}"));
    }
    let written = write_edited_config(&path, &original, |text| {
        otto_core::config::edit::remove_table(text, &["mcp", "servers", name])
    });
    if let Err(message) = written {
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
    let Ok((path, file, original)) = load_editable_config(lookup) else {
        return fail(
            stderr,
            "load config: configuration is invalid or unavailable",
        );
    };
    if !file.mcp.servers.contains_key(name) {
        return fail(stderr, &format!("unknown MCP server: {name}"));
    }
    let written = write_edited_config(&path, &original, |text| {
        otto_core::config::edit::set_value(
            text,
            &["mcp", "servers", name],
            "enabled",
            Some(&enabled.to_string()),
        )
    });
    if let Err(message) = written {
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
mod tests;
