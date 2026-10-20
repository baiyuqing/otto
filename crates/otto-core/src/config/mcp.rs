//! The `[mcp]` table and its resolution: parsing, `${VAR}` expansion, and
//! validation for MCP server declarations. See
//! `docs/specs/2026-09-19-mcp-design.md` ("Configuration") for the format.
//!
//! This module is pure: no filesystem, no process environment access, no
//! network. The caller supplies the environment as `env: &HashMap<String,
//! String>`.

use std::collections::{BTreeMap, HashMap};
use std::sync::LazyLock;

use regex::Regex;
use serde::{Deserialize, Serialize};
use url::Url;

use super::{ConfigError, skills::resolve_roots};

const DEFAULT_CALL_TIMEOUT_SECS: u64 = 60;
const DEFAULT_CONNECT_TIMEOUT_SECS: u64 = 20;

/// Bytes copied from Otto's own environment into a stdio child's environment
/// when the `[mcp.servers.*].env` table does not already set them.
const INHERITED_STDIO_ENV_KEYS: [&str; 5] = ["PATH", "HOME", "TMPDIR", "LANG", "TERM"];

/// Substrings that mark a config value as a literal credential rather than a
/// `${VAR}` reference, checked case-sensitively (the prefixes are fixed
/// vendor formats).
const LITERAL_CREDENTIAL_PREFIXES: [&str; 4] = ["ghp_", "sk-", "xoxb-", "xoxp-"];

static BEARER_TOKEN_PATTERN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)bearer\s+\S").expect("static pattern compiles"));

/// The `[mcp]` table.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Mcp {
    pub enabled: Option<bool>,
    pub call_timeout_secs: Option<u64>,
    pub connect_timeout_secs: Option<u64>,
    #[serde(default)]
    pub servers: BTreeMap<String, McpServer>,
}

/// One `[mcp.servers.<name>]` table.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpServer {
    pub enabled: Option<bool>,
    #[serde(default)]
    pub transport: String,
    pub command: Option<String>,
    pub args: Option<Vec<String>>,
    pub env: Option<BTreeMap<String, String>>,
    pub cwd: Option<String>,
    pub url: Option<String>,
    pub headers: Option<BTreeMap<String, String>>,
    pub auth: Option<String>,
    pub oauth_client_id: Option<String>,
    pub oauth_scopes: Option<Vec<String>>,
}

/// The resolved `[mcp]` configuration for one runner build.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpRuntime {
    pub enabled: bool,
    pub call_timeout_secs: u64,
    pub connect_timeout_secs: u64,
    /// In `[mcp.servers]` table order (`BTreeMap`, so sorted by name).
    pub servers: Vec<McpServerRuntime>,
}

/// One resolved server, valid regardless of `enabled` (a disabled server is
/// still validated so a later `/mcp` enable does not surface a stale error).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpServerRuntime {
    pub name: String,
    pub enabled: bool,
    pub transport: McpTransport,
    /// Every non-empty value substituted from `${VAR}` in this server's `env`
    /// or `headers` table, deduplicated. `command`, `args`, `url`, and `cwd`
    /// are not collected here, so a plain path or URL substitution is never
    /// redacted from a tool result. Used to redact tool results and logs.
    pub secrets: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum McpTransport {
    Stdio {
        command: String,
        args: Vec<String>,
        /// Sorted by key: the expanded `env` table plus inherited keys.
        env: Vec<(String, String)>,
        cwd: String,
    },
    Http {
        /// Trailing slash removed, fragment stripped.
        url: String,
        headers: Vec<(String, String)>,
        auth: McpAuth,
        oauth_client_id: Option<String>,
        oauth_scopes: Vec<String>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpAuth {
    None,
    OAuth,
}

/// Resolves `[mcp]` into a runtime ready for `connect_all`. `enabled = false`
/// short-circuits before any server is validated.
pub fn resolve_mcp(
    file: &super::File,
    env: &HashMap<String, String>,
    workspace_path: &str,
) -> Result<McpRuntime, ConfigError> {
    if let Some(policy) = &file.policy {
        policy.mcp(file)?;
    }
    let enabled = file.mcp.enabled.unwrap_or(true);
    if !enabled {
        return Ok(McpRuntime {
            enabled: false,
            call_timeout_secs: DEFAULT_CALL_TIMEOUT_SECS,
            connect_timeout_secs: DEFAULT_CONNECT_TIMEOUT_SECS,
            servers: Vec::new(),
        });
    }

    let call_timeout_secs = resolve_timeout(
        file.mcp.call_timeout_secs,
        DEFAULT_CALL_TIMEOUT_SECS,
        "call_timeout_secs",
    )?;
    let connect_timeout_secs = resolve_timeout(
        file.mcp.connect_timeout_secs,
        DEFAULT_CONNECT_TIMEOUT_SECS,
        "connect_timeout_secs",
    )?;

    let mut servers = Vec::with_capacity(file.mcp.servers.len());
    for (name, server) in &file.mcp.servers {
        servers.push(resolve_server(name, server, env, workspace_path)?);
    }

    Ok(McpRuntime {
        enabled: true,
        call_timeout_secs,
        connect_timeout_secs,
        servers,
    })
}

fn resolve_timeout(value: Option<u64>, default: u64, field: &str) -> Result<u64, ConfigError> {
    match value {
        Some(0) => Err(ConfigError::new(format!(
            "invalid mcp {field}: must be at least 1"
        ))),
        Some(value) => Ok(value),
        None => Ok(default),
    }
}

fn resolve_server(
    name: &str,
    server: &McpServer,
    env: &HashMap<String, String>,
    workspace_path: &str,
) -> Result<McpServerRuntime, ConfigError> {
    validate_name(name)?;
    let enabled = server.enabled.unwrap_or(true);
    let mut secrets: Vec<String> = Vec::new();

    let transport = match server.transport.as_str() {
        "stdio" => resolve_stdio(name, server, env, workspace_path, &mut secrets)?,
        "http" => resolve_http(name, server, env, &mut secrets)?,
        other => {
            return Err(ConfigError::new(format!(
                "mcp server {name}: transport must be \"stdio\" or \"http\" (got \"{other}\")"
            )));
        }
    };

    Ok(McpServerRuntime {
        name: name.to_string(),
        enabled,
        transport,
        secrets,
    })
}

fn validate_name(name: &str) -> Result<(), ConfigError> {
    let valid = !name.is_empty()
        && name.len() <= 32
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-');
    if valid {
        Ok(())
    } else {
        Err(ConfigError::new(format!(
            "mcp server {name}: invalid name (allowed: [A-Za-z0-9_-], 1..32 bytes)"
        )))
    }
}

fn resolve_stdio(
    name: &str,
    server: &McpServer,
    env: &HashMap<String, String>,
    workspace_path: &str,
    secrets: &mut Vec<String>,
) -> Result<McpTransport, ConfigError> {
    if server.url.is_some() {
        return Err(ConfigError::new(format!(
            "mcp server {name}: url is not allowed for stdio transport"
        )));
    }
    if server.headers.is_some() {
        return Err(ConfigError::new(format!(
            "mcp server {name}: headers is not allowed for stdio transport"
        )));
    }
    if server.auth.is_some() {
        return Err(ConfigError::new(format!(
            "mcp server {name}: auth is not allowed for stdio transport"
        )));
    }
    if server.oauth_client_id.is_some() || server.oauth_scopes.is_some() {
        return Err(ConfigError::new(format!(
            "mcp server {name}: oauth_client_id and oauth_scopes are not allowed for stdio transport"
        )));
    }

    let raw_command = server.command.as_deref().ok_or_else(|| {
        ConfigError::new(format!(
            "mcp server {name}: command is required for stdio transport"
        ))
    })?;
    let command = expand_value(raw_command, env, name, "command", None)?;

    let mut args = Vec::new();
    if let Some(raw_args) = &server.args {
        for (index, raw_arg) in raw_args.iter().enumerate() {
            let key = format!("args[{index}]");
            args.push(expand_value(raw_arg, env, name, &key, None)?);
        }
    }

    let mut env_table = BTreeMap::new();
    if let Some(raw_env) = &server.env {
        for (key, raw_value) in raw_env {
            check_literal_credential(name, key, raw_value)?;
            let value = expand_value(raw_value, env, name, key, Some(&mut *secrets))?;
            env_table.insert(key.clone(), value);
        }
    }
    for key in INHERITED_STDIO_ENV_KEYS {
        if !env_table.contains_key(key)
            && let Some(value) = env.get(key)
        {
            env_table.insert(key.to_string(), value.clone());
        }
    }

    let cwd = match &server.cwd {
        None => workspace_path.to_string(),
        Some(raw_cwd) => {
            let expanded = expand_value(raw_cwd, env, name, "cwd", None)?;
            resolve_roots(std::slice::from_ref(&expanded), env, workspace_path)
                .into_iter()
                .next()
                .ok_or_else(|| {
                    ConfigError::new(format!("mcp server {name}: cwd cannot be resolved"))
                })?
        }
    };

    Ok(McpTransport::Stdio {
        command,
        args,
        env: env_table.into_iter().collect(),
        cwd,
    })
}

fn resolve_http(
    name: &str,
    server: &McpServer,
    env: &HashMap<String, String>,
    secrets: &mut Vec<String>,
) -> Result<McpTransport, ConfigError> {
    if server.command.is_some() {
        return Err(ConfigError::new(format!(
            "mcp server {name}: command is not allowed for http transport"
        )));
    }
    if server.args.is_some() {
        return Err(ConfigError::new(format!(
            "mcp server {name}: args is not allowed for http transport"
        )));
    }
    if server.env.is_some() {
        return Err(ConfigError::new(format!(
            "mcp server {name}: env is not allowed for http transport"
        )));
    }
    if server.cwd.is_some() {
        return Err(ConfigError::new(format!(
            "mcp server {name}: cwd is not allowed for http transport"
        )));
    }

    let raw_url = server.url.as_deref().ok_or_else(|| {
        ConfigError::new(format!(
            "mcp server {name}: url is required for http transport"
        ))
    })?;
    let expanded_url = expand_value(raw_url, env, name, "url", None)?;
    let url = normalize_url(name, &expanded_url)?;

    let auth = match server.auth.as_deref().unwrap_or("none") {
        "none" => McpAuth::None,
        "oauth" => McpAuth::OAuth,
        other => {
            return Err(ConfigError::new(format!(
                "mcp server {name}: auth must be \"none\" or \"oauth\" (got \"{other}\")"
            )));
        }
    };

    if server.oauth_client_id.is_some() && auth != McpAuth::OAuth {
        return Err(ConfigError::new(format!(
            "mcp server {name}: oauth_client_id requires auth = \"oauth\""
        )));
    }
    if server.oauth_scopes.is_some() && auth != McpAuth::OAuth {
        return Err(ConfigError::new(format!(
            "mcp server {name}: oauth_scopes requires auth = \"oauth\""
        )));
    }

    let mut headers = Vec::new();
    if let Some(raw_headers) = &server.headers {
        for (key, raw_value) in raw_headers {
            check_literal_credential(name, key, raw_value)?;
            if auth == McpAuth::OAuth && key.eq_ignore_ascii_case("authorization") {
                return Err(ConfigError::new(format!(
                    "mcp server {name}: headers cannot set {key} when auth = \"oauth\""
                )));
            }
            let value = expand_value(raw_value, env, name, key, Some(&mut *secrets))?;
            headers.push((key.clone(), value));
        }
    }

    Ok(McpTransport::Http {
        url,
        headers,
        auth,
        oauth_client_id: server.oauth_client_id.clone(),
        oauth_scopes: server.oauth_scopes.clone().unwrap_or_default(),
    })
}

/// Parses `raw` as an absolute URL, requires an `http`/`https` scheme, strips
/// the fragment, and removes one trailing slash.
fn normalize_url(name: &str, raw: &str) -> Result<String, ConfigError> {
    let mut parsed = Url::parse(raw)
        .map_err(|err| ConfigError::new(format!("mcp server {name}: invalid url: {err}")))?;
    if parsed.scheme() != "http" && parsed.scheme() != "https" {
        return Err(ConfigError::new(format!(
            "mcp server {name}: url must use http or https"
        )));
    }
    parsed.set_fragment(None);
    let mut result = parsed.to_string();
    if result.ends_with('/') {
        result.pop();
    }
    Ok(result)
}

/// Expands every `${VAR}` and `${VAR:-default}` reference in `raw` against
/// `env`. `$$` and a `$` not followed by `{` are literal. Every non-empty
/// value substituted from `env` (not a default) is pushed to `secrets` if not
/// already present, when the caller passes one: only `env` and `headers`
/// values are redaction-worthy (the design's "Result mapping" section);
/// `command`, `args`, `url`, and `cwd` pass `None` so a value such as
/// `cwd = "${HOME}/x"` does not redact the home path from every tool result.
fn expand_value(
    raw: &str,
    env: &HashMap<String, String>,
    server_name: &str,
    key: &str,
    mut secrets: Option<&mut Vec<String>>,
) -> Result<String, ConfigError> {
    let mut out = String::with_capacity(raw.len());
    let mut rest = raw;
    while let Some(dollar) = rest.find('$') {
        out.push_str(&rest[..dollar]);
        let after = &rest[dollar + 1..];
        if let Some(inner_and_rest) = after.strip_prefix('{')
            && let Some(end) = inner_and_rest.find('}')
        {
            let inner = &inner_and_rest[..end];
            let (var_name, default) = match inner.find(":-") {
                Some(split) => (&inner[..split], Some(&inner[split + 2..])),
                None => (inner, None),
            };
            match env.get(var_name) {
                Some(value) => {
                    out.push_str(value);
                    if let Some(secrets) = secrets.as_deref_mut()
                        && !value.is_empty()
                        && !secrets.iter().any(|s| s == value)
                    {
                        secrets.push(value.clone());
                    }
                }
                None => match default {
                    Some(default_value) => out.push_str(default_value),
                    None => {
                        return Err(ConfigError::new(format!(
                            "mcp server {server_name}: {key} references unset environment variable {var_name}"
                        )));
                    }
                },
            }
            rest = &inner_and_rest[end + 1..];
            continue;
        }
        out.push('$');
        rest = after;
    }
    out.push_str(rest);
    Ok(out)
}

/// Removes every `${...}` reference from `raw` without expanding it, for the
/// literal-credential check below (which must run before expansion so a
/// credential typed directly into the config, rather than referenced via
/// `${VAR}`, is caught).
fn strip_var_refs(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut rest = raw;
    while let Some(dollar) = rest.find('$') {
        out.push_str(&rest[..dollar]);
        let after = &rest[dollar + 1..];
        if let Some(inner_and_rest) = after.strip_prefix('{')
            && let Some(end) = inner_and_rest.find('}')
        {
            rest = &inner_and_rest[end + 1..];
            continue;
        }
        out.push('$');
        rest = after;
    }
    out.push_str(rest);
    out
}

/// Validates a credential template without resolving or revealing its value.
/// Native package readers reuse this boundary before persisting configuration.
pub fn validate_credential_template(raw: &str) -> Result<(), ConfigError> {
    check_literal_credential("plugin", "credential", raw)
}

fn check_literal_credential(name: &str, key: &str, raw: &str) -> Result<(), ConfigError> {
    let stripped = strip_var_refs(raw);
    let looks_like_credential = BEARER_TOKEN_PATTERN.is_match(&stripped)
        || LITERAL_CREDENTIAL_PREFIXES
            .iter()
            .any(|prefix| stripped.contains(prefix));
    if looks_like_credential {
        return Err(ConfigError::new(format!(
            "mcp server {name}: {key} looks like a literal credential; use ${{VAR}}"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests;
