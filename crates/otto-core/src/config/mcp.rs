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
    /// Every non-empty value substituted from `${VAR}` in this server's
    /// config, deduplicated. Used to redact tool results and logs.
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
    let command = expand_value(raw_command, env, name, "command", secrets)?;

    let mut args = Vec::new();
    if let Some(raw_args) = &server.args {
        for (index, raw_arg) in raw_args.iter().enumerate() {
            let key = format!("args[{index}]");
            args.push(expand_value(raw_arg, env, name, &key, secrets)?);
        }
    }

    let mut env_table = BTreeMap::new();
    if let Some(raw_env) = &server.env {
        for (key, raw_value) in raw_env {
            check_literal_credential(name, key, raw_value)?;
            let value = expand_value(raw_value, env, name, key, secrets)?;
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
            let expanded = expand_value(raw_cwd, env, name, "cwd", secrets)?;
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
    let expanded_url = expand_value(raw_url, env, name, "url", secrets)?;
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
            let value = expand_value(raw_value, env, name, key, secrets)?;
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
/// already present.
fn expand_value(
    raw: &str,
    env: &HashMap<String, String>,
    server_name: &str,
    key: &str,
    secrets: &mut Vec<String>,
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
                    if !value.is_empty() && !secrets.iter().any(|s| s == value) {
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
mod tests {
    use super::*;
    use crate::config::File;

    type FieldRejectionCase = (fn(&mut McpServer), &'static str);

    fn env(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    fn stdio_server(command: &str) -> McpServer {
        McpServer {
            transport: "stdio".into(),
            command: Some(command.into()),
            ..Default::default()
        }
    }

    fn http_server(url: &str) -> McpServer {
        McpServer {
            transport: "http".into(),
            url: Some(url.into()),
            ..Default::default()
        }
    }

    fn file_with(servers: Vec<(&str, McpServer)>) -> File {
        let mut file = File::default();
        for (name, server) in servers {
            file.mcp.servers.insert(name.to_string(), server);
        }
        file
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn disabled_skips_server_validation() {
        let mut file = file_with(vec![("bad name", stdio_server(""))]);
        file.mcp.enabled = Some(false);
        let runtime = resolve_mcp(&file, &HashMap::new(), "/work").expect("resolve");
        assert!(!runtime.enabled);
        assert!(runtime.servers.is_empty());
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn defaults_enabled_with_no_servers() {
        let runtime = resolve_mcp(&File::default(), &HashMap::new(), "/work").expect("resolve");
        assert!(runtime.enabled);
        assert_eq!(runtime.call_timeout_secs, 60);
        assert_eq!(runtime.connect_timeout_secs, 20);
        assert!(runtime.servers.is_empty());
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn rejects_invalid_server_name() {
        let file = file_with(vec![("bad name!", stdio_server("./run"))]);
        let err = resolve_mcp(&file, &HashMap::new(), "/work").unwrap_err();
        assert!(err.to_string().contains("invalid name"), "{err}");
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn accepts_max_length_name() {
        let name = "a".repeat(32);
        let file = file_with(vec![(&name, stdio_server("./run"))]);
        let runtime = resolve_mcp(&file, &HashMap::new(), "/work").expect("resolve");
        assert_eq!(runtime.servers[0].name, name);
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn rejects_too_long_name() {
        let name = "a".repeat(33);
        let file = file_with(vec![(&name, stdio_server("./run"))]);
        let err = resolve_mcp(&file, &HashMap::new(), "/work").unwrap_err();
        assert!(err.to_string().contains("invalid name"), "{err}");
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn rejects_unknown_transport() {
        let mut server = stdio_server("./run");
        server.transport = "websocket".into();
        let file = file_with(vec![("ws", server)]);
        let err = resolve_mcp(&file, &HashMap::new(), "/work").unwrap_err();
        assert!(err.to_string().contains("transport must be"), "{err}");
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn stdio_requires_command() {
        let mut server = stdio_server("./run");
        server.command = None;
        let file = file_with(vec![("s", server)]);
        let err = resolve_mcp(&file, &HashMap::new(), "/work").unwrap_err();
        assert!(err.to_string().contains("command is required"), "{err}");
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn stdio_rejects_http_only_fields() {
        let cases: [FieldRejectionCase; 4] = [
            (
                (|s: &mut McpServer| s.url = Some("https://example.com".into()))
                    as fn(&mut McpServer),
                "url is not allowed",
            ),
            (
                (|s: &mut McpServer| s.headers = Some(BTreeMap::from([("X".into(), "y".into())])))
                    as fn(&mut McpServer),
                "headers is not allowed",
            ),
            (
                (|s: &mut McpServer| s.auth = Some("none".into())) as fn(&mut McpServer),
                "auth is not allowed",
            ),
            (
                (|s: &mut McpServer| s.oauth_client_id = Some("id".into())) as fn(&mut McpServer),
                "oauth_client_id and oauth_scopes are not allowed",
            ),
        ];
        for (mutate, expected) in cases {
            let mut server = stdio_server("./run");
            mutate(&mut server);
            let file = file_with(vec![("s", server)]);
            let err = resolve_mcp(&file, &HashMap::new(), "/work").unwrap_err();
            assert!(err.to_string().contains(expected), "{err}");
        }
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn http_requires_url() {
        let mut server = http_server("https://example.com/mcp");
        server.url = None;
        let file = file_with(vec![("s", server)]);
        let err = resolve_mcp(&file, &HashMap::new(), "/work").unwrap_err();
        assert!(err.to_string().contains("url is required"), "{err}");
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn http_rejects_stdio_only_fields() {
        let cases: [FieldRejectionCase; 4] = [
            (
                (|s: &mut McpServer| s.command = Some("./run".into())) as fn(&mut McpServer),
                "command is not allowed",
            ),
            (
                (|s: &mut McpServer| s.args = Some(vec!["-y".into()])) as fn(&mut McpServer),
                "args is not allowed",
            ),
            (
                (|s: &mut McpServer| s.env = Some(BTreeMap::from([("X".into(), "y".into())])))
                    as fn(&mut McpServer),
                "env is not allowed",
            ),
            (
                (|s: &mut McpServer| s.cwd = Some(".".into())) as fn(&mut McpServer),
                "cwd is not allowed",
            ),
        ];
        for (mutate, expected) in cases {
            let mut server = http_server("https://example.com/mcp");
            mutate(&mut server);
            let file = file_with(vec![("s", server)]);
            let err = resolve_mcp(&file, &HashMap::new(), "/work").unwrap_err();
            assert!(err.to_string().contains(expected), "{err}");
        }
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn auth_defaults_to_none_and_rejects_unknown_value() {
        let file = file_with(vec![("s", http_server("https://example.com/mcp"))]);
        let runtime = resolve_mcp(&file, &HashMap::new(), "/work").expect("resolve");
        match &runtime.servers[0].transport {
            McpTransport::Http { auth, .. } => assert_eq!(*auth, McpAuth::None),
            _ => panic!("expected http transport"),
        }

        let mut server = http_server("https://example.com/mcp");
        server.auth = Some("basic".into());
        let file = file_with(vec![("s", server)]);
        let err = resolve_mcp(&file, &HashMap::new(), "/work").unwrap_err();
        assert!(err.to_string().contains("auth must be"), "{err}");
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn oauth_fields_require_auth_oauth() {
        let mut server = http_server("https://example.com/mcp");
        server.oauth_client_id = Some("client".into());
        let file = file_with(vec![("s", server)]);
        let err = resolve_mcp(&file, &HashMap::new(), "/work").unwrap_err();
        assert!(
            err.to_string().contains("oauth_client_id requires"),
            "{err}"
        );

        let mut server = http_server("https://example.com/mcp");
        server.oauth_scopes = Some(vec!["mcp:tools".into()]);
        let file = file_with(vec![("s", server)]);
        let err = resolve_mcp(&file, &HashMap::new(), "/work").unwrap_err();
        assert!(err.to_string().contains("oauth_scopes requires"), "{err}");

        let mut server = http_server("https://example.com/mcp");
        server.auth = Some("oauth".into());
        server.oauth_client_id = Some("client".into());
        server.oauth_scopes = Some(vec!["mcp:tools".into()]);
        let file = file_with(vec![("s", server)]);
        let runtime = resolve_mcp(&file, &HashMap::new(), "/work").expect("resolve");
        match &runtime.servers[0].transport {
            McpTransport::Http {
                auth,
                oauth_client_id,
                oauth_scopes,
                ..
            } => {
                assert_eq!(*auth, McpAuth::OAuth);
                assert_eq!(oauth_client_id.as_deref(), Some("client"));
                assert_eq!(oauth_scopes, &vec!["mcp:tools".to_string()]);
            }
            _ => panic!("expected http transport"),
        }
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn rejects_authorization_header_with_oauth() {
        let mut server = http_server("https://example.com/mcp");
        server.auth = Some("oauth".into());
        server.headers = Some(BTreeMap::from([("Authorization".into(), "x".into())]));
        let file = file_with(vec![("s", server)]);
        let err = resolve_mcp(&file, &HashMap::new(), "/work").unwrap_err();
        assert!(
            err.to_string().contains("cannot set Authorization"),
            "{err}"
        );
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn normalizes_url_trailing_slash_and_fragment() {
        let file = file_with(vec![(
            "s",
            http_server("https://mcp.example.com/mcp/#frag"),
        )]);
        let runtime = resolve_mcp(&file, &HashMap::new(), "/work").expect("resolve");
        match &runtime.servers[0].transport {
            McpTransport::Http { url, .. } => assert_eq!(url, "https://mcp.example.com/mcp"),
            _ => panic!("expected http transport"),
        }
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn rejects_non_http_scheme() {
        let file = file_with(vec![("s", http_server("ftp://example.com/mcp"))]);
        let err = resolve_mcp(&file, &HashMap::new(), "/work").unwrap_err();
        assert!(err.to_string().contains("must use http or https"), "{err}");
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn rejects_invalid_url() {
        let file = file_with(vec![("s", http_server("not a url"))]);
        let err = resolve_mcp(&file, &HashMap::new(), "/work").unwrap_err();
        assert!(err.to_string().contains("invalid url"), "{err}");
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn expands_var_and_default_across_fields() {
        let mut server = stdio_server("${BIN}");
        server.args = Some(vec!["${MODE:-prod}".into()]);
        server.env = Some(BTreeMap::from([("TOKEN".into(), "${GITHUB_TOKEN}".into())]));
        server.cwd = Some("${WORKDIR}".into());
        let file = file_with(vec![("s", server)]);
        let env = env(&[
            ("BIN", "/usr/bin/run"),
            ("GITHUB_TOKEN", "secret-token"),
            ("WORKDIR", "sub"),
        ]);
        let runtime = resolve_mcp(&file, &env, "/work").expect("resolve");
        let entry = &runtime.servers[0];
        match &entry.transport {
            McpTransport::Stdio {
                command,
                args,
                env: env_table,
                cwd,
            } => {
                assert_eq!(command, "/usr/bin/run");
                assert_eq!(args, &vec!["prod".to_string()]);
                assert!(env_table.contains(&("TOKEN".to_string(), "secret-token".to_string())));
                assert_eq!(cwd, "/work/sub");
            }
            _ => panic!("expected stdio transport"),
        }
        assert!(entry.secrets.contains(&"secret-token".to_string()));
        // "prod" came from a default, not an env substitution: not a secret.
        assert!(!entry.secrets.contains(&"prod".to_string()));
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn dollar_dollar_and_bare_dollar_are_literal() {
        let mut server = stdio_server("echo $$5 and $HOME");
        server.env = None;
        let file = file_with(vec![("s", server)]);
        let runtime = resolve_mcp(&file, &HashMap::new(), "/work").expect("resolve");
        match &runtime.servers[0].transport {
            McpTransport::Stdio { command, .. } => {
                assert_eq!(command, "echo $$5 and $HOME");
            }
            _ => panic!("expected stdio transport"),
        }
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn errors_on_unset_variable_without_default() {
        let server = stdio_server("${MISSING_VAR}");
        let file = file_with(vec![("s", server)]);
        let err = resolve_mcp(&file, &HashMap::new(), "/work").unwrap_err();
        let message = err.to_string();
        assert!(message.contains("command"), "{message}");
        assert!(
            message.contains("unset environment variable MISSING_VAR"),
            "{message}"
        );
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn rejects_literal_bearer_token_in_header() {
        let mut server = http_server("https://example.com/mcp");
        server.headers = Some(BTreeMap::from([(
            "Authorization".into(),
            "Bearer abc123".into(),
        )]));
        let file = file_with(vec![("s", server)]);
        let err = resolve_mcp(&file, &HashMap::new(), "/work").unwrap_err();
        assert!(
            err.to_string().contains("looks like a literal credential"),
            "{err}"
        );
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn rejects_known_credential_prefixes_in_env() {
        for value in ["ghp_abcdef", "sk-abcdef", "xoxb-abcdef", "xoxp-abcdef"] {
            let mut server = stdio_server("./run");
            server.env = Some(BTreeMap::from([("TOKEN".into(), value.into())]));
            let file = file_with(vec![("s", server)]);
            let err = resolve_mcp(&file, &HashMap::new(), "/work").unwrap_err();
            assert!(
                err.to_string().contains("looks like a literal credential"),
                "{value}: {err}"
            );
        }
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn allows_credential_prefix_inside_var_reference() {
        let mut server = http_server("https://example.com/mcp");
        server.headers = Some(BTreeMap::from([(
            "Authorization".into(),
            "Bearer ${TOKEN}".into(),
        )]));
        let file = file_with(vec![("s", server)]);
        let env = env(&[("TOKEN", "sk-should-not-be-checked-again")]);
        let runtime = resolve_mcp(&file, &env, "/work").expect("resolve");
        match &runtime.servers[0].transport {
            McpTransport::Http { headers, .. } => {
                assert_eq!(
                    headers,
                    &vec![(
                        "Authorization".to_string(),
                        "Bearer sk-should-not-be-checked-again".to_string()
                    )]
                );
            }
            _ => panic!("expected http transport"),
        }
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn stdio_child_env_includes_inherited_keys_when_present_and_not_overridden() {
        let mut server = stdio_server("./run");
        server.env = Some(BTreeMap::from([("PATH".into(), "/custom/bin".into())]));
        let file = file_with(vec![("s", server)]);
        let env = env(&[
            ("PATH", "/usr/bin"),
            ("HOME", "/home/u"),
            ("TMPDIR", "/tmp"),
            ("LANG", "en_US.UTF-8"),
            ("TERM", "xterm"),
            ("OTHER", "not-copied"),
        ]);
        let runtime = resolve_mcp(&file, &env, "/work").expect("resolve");
        match &runtime.servers[0].transport {
            McpTransport::Stdio { env: env_table, .. } => {
                assert!(env_table.contains(&("PATH".to_string(), "/custom/bin".to_string())));
                assert!(env_table.contains(&("HOME".to_string(), "/home/u".to_string())));
                assert!(env_table.contains(&("TMPDIR".to_string(), "/tmp".to_string())));
                assert!(env_table.contains(&("LANG".to_string(), "en_US.UTF-8".to_string())));
                assert!(env_table.contains(&("TERM".to_string(), "xterm".to_string())));
                assert!(!env_table.iter().any(|(k, _)| k == "OTHER"));
            }
            _ => panic!("expected stdio transport"),
        }
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn cwd_defaults_to_workspace_path() {
        let file = file_with(vec![("s", stdio_server("./run"))]);
        let runtime = resolve_mcp(&file, &HashMap::new(), "/work").expect("resolve");
        match &runtime.servers[0].transport {
            McpTransport::Stdio { cwd, .. } => assert_eq!(cwd, "/work"),
            _ => panic!("expected stdio transport"),
        }
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn cwd_resolves_home_relative_and_rejects_unresolvable_home() {
        let mut server = stdio_server("./run");
        server.cwd = Some("~/x".into());
        let file = file_with(vec![("s", server)]);

        let home_env = env(&[("HOME", "/home/u")]);
        let runtime = resolve_mcp(&file, &home_env, "/work").expect("resolve");
        match &runtime.servers[0].transport {
            McpTransport::Stdio { cwd, .. } => assert_eq!(cwd, "/home/u/x"),
            _ => panic!("expected stdio transport"),
        }

        let err = resolve_mcp(&file, &HashMap::new(), "/work").unwrap_err();
        assert!(err.to_string().contains("cwd cannot be resolved"), "{err}");
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn disabled_server_stays_listed_but_still_validated() {
        let mut server = stdio_server("./run");
        server.enabled = Some(false);
        let file = file_with(vec![("s", server)]);
        let runtime = resolve_mcp(&file, &HashMap::new(), "/work").expect("resolve");
        assert_eq!(runtime.servers.len(), 1);
        assert!(!runtime.servers[0].enabled);

        let mut bad_server = stdio_server("./run");
        bad_server.enabled = Some(false);
        bad_server.command = None;
        let file = file_with(vec![("s", bad_server)]);
        let err = resolve_mcp(&file, &HashMap::new(), "/work").unwrap_err();
        assert!(err.to_string().contains("command is required"), "{err}");
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn rejects_zero_timeouts() {
        let mut file = File::default();
        file.mcp.call_timeout_secs = Some(0);
        let err = resolve_mcp(&file, &HashMap::new(), "/work").unwrap_err();
        assert!(err.to_string().contains("call_timeout_secs"), "{err}");

        let mut file = File::default();
        file.mcp.connect_timeout_secs = Some(0);
        let err = resolve_mcp(&file, &HashMap::new(), "/work").unwrap_err();
        assert!(err.to_string().contains("connect_timeout_secs"), "{err}");
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn accepts_custom_positive_timeouts() {
        let mut file = File::default();
        file.mcp.call_timeout_secs = Some(5);
        file.mcp.connect_timeout_secs = Some(3);
        let runtime = resolve_mcp(&file, &HashMap::new(), "/work").expect("resolve");
        assert_eq!(runtime.call_timeout_secs, 5);
        assert_eq!(runtime.connect_timeout_secs, 3);
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn rejects_unknown_field_in_mcp_table() {
        let err = crate::config::parse("[mcp]\nunknown = true\n").unwrap_err();
        assert!(err.to_string().contains("unknown"), "{err}");
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn rejects_unknown_field_in_server_table() {
        let err = crate::config::parse(
            "[mcp.servers.s]\ntransport = \"stdio\"\ncommand = \"./run\"\nunknown = true\n",
        )
        .unwrap_err();
        assert!(err.to_string().contains("unknown"), "{err}");
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn parses_toml_example_from_spec() {
        let file = crate::config::parse(
            r#"[mcp]
enabled = true
call_timeout_secs = 60
connect_timeout_secs = 20

[mcp.servers.github]
transport = "stdio"
command = "npx"
args = ["-y", "@modelcontextprotocol/server-github"]
env = { GITHUB_TOKEN = "${GITHUB_TOKEN}" }
cwd = "."

[mcp.servers.docs]
transport = "http"
url = "https://mcp.example.com/mcp"
headers = { Authorization = "Bearer ${DOCS_MCP_TOKEN}" }

[mcp.servers.remote]
transport = "http"
url = "https://remote.example.com/mcp"
auth = "oauth"
oauth_client_id = "otto"
oauth_scopes = ["mcp:tools"]

[mcp.servers.legacy]
transport = "stdio"
command = "./bin/legacy-server"
enabled = false
"#,
        )
        .expect("parse");
        let env = env(&[
            ("GITHUB_TOKEN", "gh-secret"),
            ("DOCS_MCP_TOKEN", "docs-secret"),
        ]);
        let runtime = resolve_mcp(&file, &env, "/work").expect("resolve");
        assert_eq!(runtime.servers.len(), 4);
    }
}
