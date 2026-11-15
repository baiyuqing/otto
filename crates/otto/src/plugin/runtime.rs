//! Adapt package components into existing native Skill/MCP contracts.
use super::{
    Installed, Service, digest,
    package::{self, Server},
};
use crate::hex::to_hex;
use crate::{
    mcp::{self, CallError, CallOutcome, ToolServer},
    skill::Catalog,
};
use otto_core::config::{File, McpAuth, McpServer, McpServerRuntime, McpTransport, resolve_mcp};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, HashMap},
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use tokio_util::sync::CancellationToken;

/// Resolves `[plugins.bindings.<package>]` (a key without `:`) into environment
/// entries for that package's hook and tool commands. Only `env` is allowed.
pub(crate) fn package_environment(
    file: &File,
    package: &str,
    environment: &HashMap<String, String>,
) -> Result<Vec<(String, String)>, String> {
    let Some(binding) = file.plugins.bindings.get(package) else {
        return Ok(Vec::new());
    };
    let invalid =
        |reason: &str| format!("invalid credential binding for package {package}: {reason}");
    if binding.headers.is_some()
        || binding.auth.is_some()
        || binding.oauth_client_id.is_some()
        || binding.oauth_scopes.is_some()
    {
        return Err(invalid("only env is allowed"));
    }
    let bound_env = binding.env.clone().unwrap_or_default();
    for name in bound_env.keys() {
        let mut chars = name.chars();
        let valid = chars
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
            && chars.all(|c| c.is_ascii_alphanumeric() || c == '_');
        if !valid {
            return Err(invalid("invalid environment name"));
        }
    }
    // A fixed server name: package names may contain dots or exceed the
    // 32-byte MCP server name limit.
    let mut config = File::default();
    config.mcp.servers.insert(
        "package".into(),
        McpServer {
            transport: "stdio".into(),
            command: Some("plugin".into()),
            env: Some(bound_env.clone()),
            ..McpServer::default()
        },
    );
    let resolved = resolve_mcp(&config, environment, "")
        .map_err(|_| invalid("check environment references and literal values"))?;
    let McpTransport::Stdio { env, .. } = &resolved.servers[0].transport else {
        return Err(invalid("unexpected transport"));
    };
    // resolve_mcp also inherits PATH/HOME; keep only the declared names.
    Ok(env
        .iter()
        .filter(|(key, _)| bound_env.contains_key(key))
        .cloned()
        .collect())
}

#[derive(Default)]
pub struct Snapshot {
    pub packages: Vec<Installed>,
    pub servers: Vec<McpServerRuntime>,
    pub guards: BTreeMap<String, Authorization>,
    pub diagnostics: Vec<String>,
    skill_paths: Vec<(String, PathBuf, Vec<PathBuf>)>,
}
#[derive(Clone)]
pub struct Authorization {
    service: Service,
    plugin: String,
    digest: String,
    pub logical_name: String,
    declaration: McpServer,
}
impl Authorization {
    pub fn valid(&self) -> bool {
        if !self.service.authorized(&self.plugin, &self.digest) {
            return false;
        }
        let policy = crate::config::layered::policy_at(crate::config::layered::managed_path());
        policy.is_ok_and(|policy| {
            policy.is_none_or(|policy| {
                let mut file = File::default();
                file.mcp
                    .servers
                    .insert("plugin".into(), self.declaration.clone());
                policy.mcp(&file).is_ok()
            })
        })
    }
    pub fn watch(&self, client: &Arc<mcp::client::Client>, servers: &Arc<mcp::Servers>) {
        let weak = Arc::downgrade(client);
        let statuses = Arc::downgrade(servers);
        let guard = self.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(1)).await;
                let Some(client) = weak.upgrade() else {
                    break;
                };
                let check = guard.clone();
                let valid = tokio::task::spawn_blocking(move || check.valid())
                    .await
                    .unwrap_or(false);
                if !valid {
                    client.close().await;
                    if let Some(servers) = statuses.upgrade() {
                        servers.mark_failed(
                            &guard.logical_name,
                            "plugin authorization revoked or package changed",
                        );
                    }
                    break;
                }
            }
        });
    }
}
/// The same wrapper is used by parent and child MCP router tools.
pub struct GuardedServer {
    client: Arc<mcp::client::Client>,
    authorization: Authorization,
}
impl GuardedServer {
    pub fn new(client: Arc<mcp::client::Client>, authorization: Authorization) -> Self {
        Self {
            client,
            authorization,
        }
    }
}
#[async_trait::async_trait]
impl ToolServer for GuardedServer {
    fn name(&self) -> &str {
        self.client.name()
    }
    async fn call(
        &self,
        tool: &str,
        arguments: serde_json::Value,
        cancel: &CancellationToken,
    ) -> Result<CallOutcome, CallError> {
        let guard = self.authorization.clone();
        if !tokio::task::spawn_blocking(move || guard.valid())
            .await
            .unwrap_or(false)
        {
            self.client.close().await;
            return Err(CallError::PluginUnauthorized);
        }
        self.client.call(tool, arguments, cancel).await
    }
}
impl Snapshot {
    pub fn load(home: &Path, file: &File, environment: &HashMap<String, String>) -> Self {
        let mut snapshot = Self::default();
        if file.plugins.enabled == Some(false) {
            return snapshot;
        }
        let service = Service::new(home);
        let installed = match service.list() {
            Ok(p) => p,
            Err(e) => {
                snapshot.diagnostics.push(e);
                return snapshot;
            }
        };
        for mut installed in installed {
            if !installed.enabled || file.plugins.disabled.contains(&installed.name) {
                continue;
            }
            let path = service.package_path(&installed.digest);
            if !digest(&path).is_ok_and(|d| d == installed.digest) {
                snapshot.diagnostics.push(format!(
                    "plugin {}: snapshot content changed or unavailable; reinstall",
                    installed.name
                ));
                continue;
            }
            let path = match std::fs::canonicalize(&path) {
                Ok(path) => path,
                Err(_) => {
                    snapshot.diagnostics.push(format!(
                        "plugin {}: cannot resolve snapshot root",
                        installed.name
                    ));
                    continue;
                }
            };
            let package = match package::discover(&path) {
                Ok(p) if p.name == installed.name => p,
                _ => {
                    snapshot.diagnostics.push(format!(
                        "plugin {}: invalid snapshot manifest",
                        installed.name
                    ));
                    continue;
                }
            };
            snapshot.diagnostics.extend(
                package
                    .diagnostics
                    .iter()
                    .map(|d| format!("plugin {}: {d}", installed.name)),
            );
            if !installed.trusted && package.has_executables() {
                snapshot.diagnostics.push(format!(
                    "plugin {}: hooks, custom tools and memory disabled; run otto plugin trust {}",
                    installed.name, installed.name
                ));
            }
            if file.skills.enabled != Some(false) {
                snapshot.skill_paths.push((
                    installed.name.clone(),
                    path.clone(),
                    package.skills.clone(),
                ));
            }
            if file.mcp.enabled != Some(false) {
                for (local_name, server) in &package.servers {
                    let logical = format!("{}:{local_name}", installed.name);
                    let name = internal_name(&logical);
                    if snapshot.guards.contains_key(&name) || file.mcp.servers.contains_key(&name) {
                        snapshot
                            .diagnostics
                            .push(format!("plugin {logical}: internal MCP identity collision"));
                        continue;
                    }
                    let result = (|| {
                        let package_data_dir = service.ensure_data(&installed.name)?;
                        let (mut runtime, declaration) = adapt_server(
                            server,
                            &name,
                            &logical,
                            &path,
                            &package_data_dir,
                            package.format != "agent-plugins-1.0",
                            file,
                            environment,
                        )?;
                        let authorization = Authorization {
                            service: service.clone(),
                            plugin: installed.name.clone(),
                            digest: installed.digest.clone(),
                            logical_name: logical.clone(),
                            declaration,
                        };
                        runtime.enabled = installed.trusted;
                        if !installed.trusted {
                            snapshot.diagnostics.push(format!(
                                "plugin {logical}: MCP disabled; run otto plugin trust {}",
                                installed.name
                            ));
                        }
                        snapshot.guards.insert(name.clone(), authorization);
                        snapshot.servers.push(runtime);
                        Ok::<_, String>(())
                    })();
                    if let Err(e) = result {
                        snapshot.diagnostics.push(format!("plugin {logical}: {e}"));
                    }
                }
            }
            installed.package = package;
            snapshot.packages.push(installed);
        }
        snapshot
    }
    pub fn extend_skills(&self, catalog: &mut Catalog) -> Vec<String> {
        let mut diagnostics = Vec::new();
        for (name, root, paths) in &self.skill_paths {
            diagnostics.extend(catalog.extend_plugin(name, root, paths));
        }
        diagnostics
    }
    pub fn provenance(&self) -> String {
        serde_json::to_string(
            &self
                .packages
                .iter()
                .map(|p| {
                    serde_json::json!({
                        "name":p.name,
                        "version":p.version,
                        "digest":p.digest,
                        "commit":p.commit
                    })
                })
                .collect::<Vec<_>>(),
        )
        .unwrap_or_else(|_| "[]".into())
    }
}
pub fn internal_name(logical: &str) -> String {
    let digest = to_hex(&Sha256::digest(logical.as_bytes()));
    format!("p_{}", &digest[..16])
}
pub(super) fn expand(raw: &str, root: &Path, data: &Path, legacy: bool) -> String {
    let value = raw
        .replace("${PLUGIN_ROOT}", &root.to_string_lossy())
        .replace("${PLUGIN_DATA}", &data.to_string_lossy());
    if legacy {
        value
            .replace("${CLAUDE_PLUGIN_ROOT}", &root.to_string_lossy())
            .replace("${CLAUDE_PLUGIN_DATA}", &data.to_string_lossy())
            .replace("${CODEX_PLUGIN_ROOT}", &root.to_string_lossy())
    } else {
        value
    }
}
fn confined_path(raw: &str, root: &Path, data: &Path, legacy: bool) -> Result<String, String> {
    let expanded = expand(raw, root, data, legacy);
    let in_data = raw == "${PLUGIN_DATA}"
        || raw.starts_with("${PLUGIN_DATA}/")
        || (legacy
            && (raw == "${CLAUDE_PLUGIN_DATA}" || raw.starts_with("${CLAUDE_PLUGIN_DATA}/")));
    let in_root = raw == "${PLUGIN_ROOT}"
        || raw.starts_with("${PLUGIN_ROOT}/")
        || raw.starts_with("./")
        || (legacy
            && (raw == "${CLAUDE_PLUGIN_ROOT}"
                || raw.starts_with("${CLAUDE_PLUGIN_ROOT}/")
                || raw == "${CODEX_PLUGIN_ROOT}"
                || raw.starts_with("${CODEX_PLUGIN_ROOT}/")));
    if !in_data && !in_root {
        return Err("cwd must be package-relative or rooted in PLUGIN_ROOT/PLUGIN_DATA".into());
    }
    let path = if raw.starts_with("./") {
        root.join(expanded)
    } else {
        PathBuf::from(expanded)
    };
    let path = std::fs::canonicalize(path).map_err(|_| "MCP working directory is unavailable")?;
    if !path.starts_with(if in_data { data } else { root }) || !path.is_dir() {
        return Err("MCP working directory escapes its declared root".into());
    }
    Ok(path.to_string_lossy().into_owned())
}
#[allow(clippy::too_many_arguments)]
fn adapt_server(
    server: &Server,
    name: &str,
    logical: &str,
    root: &Path,
    data: &Path,
    legacy: bool,
    file: &File,
    environment: &HashMap<String, String>,
) -> Result<(McpServerRuntime, McpServer), String> {
    let mut declaration = McpServer::default();
    let mut transport = match server.kind.as_str() {
        "stdio" => {
            if server.url.is_some() || server.headers.is_some() {
                return Err("HTTP fields are not valid for stdio".into());
            }
            let command = server
                .command
                .as_deref()
                .filter(|s| !s.is_empty())
                .ok_or("stdio requires command")?;
            let command = if command.starts_with("./") {
                let command = std::fs::canonicalize(root.join(command))
                    .map_err(|_| "package executable is unavailable")?;
                if !command.starts_with(root) || !command.is_file() {
                    return Err("package command escapes plugin root".into());
                }
                command.to_string_lossy().into_owned()
            } else if legacy
                && (command.starts_with("${CLAUDE_PLUGIN_ROOT}/")
                    || command.starts_with("${CODEX_PLUGIN_ROOT}/"))
            {
                let path = std::fs::canonicalize(expand(command, root, data, true))
                    .map_err(|_| "legacy package executable is unavailable")?;
                if !path.starts_with(root) || !path.is_file() {
                    return Err("legacy command escapes plugin root".into());
                }
                path.to_string_lossy().into_owned()
            } else {
                if command
                    .chars()
                    .any(|c| c.is_whitespace() || matches!(c, '/' | '\\' | '$'))
                {
                    return Err("command must be a bare executable token or ./package-path".into());
                }
                command.to_owned()
            };
            let mut env = BTreeMap::new();
            for (key, value) in server.env.iter().flatten() {
                if key.is_empty()
                    || key.contains(['=', '\0'])
                    || matches!(key.as_str(), "PLUGIN_ROOT" | "PLUGIN_DATA")
                {
                    return Err("invalid or reserved MCP environment key".into());
                }
                reject_secret(value)?;
                env.insert(key.clone(), expand(value, root, data, legacy));
            }
            for key in ["PATH", "HOME", "TMPDIR", "LANG", "TERM"] {
                if let Some(value) = environment.get(key) {
                    env.entry(key.into()).or_insert_with(|| value.clone());
                }
            }
            env.insert("PLUGIN_ROOT".into(), root.to_string_lossy().into_owned());
            env.insert("PLUGIN_DATA".into(), data.to_string_lossy().into_owned());
            let cwd = confined_path(
                server.cwd.as_deref().unwrap_or("${PLUGIN_ROOT}"),
                root,
                data,
                legacy,
            )?;
            declaration.transport = "stdio".into();
            declaration.command = Some(command.clone());
            declaration.args = Some(
                server
                    .args
                    .iter()
                    .flatten()
                    .map(|v| expand(v, root, data, legacy))
                    .collect(),
            );
            declaration.cwd = Some(cwd.clone());
            McpTransport::Stdio {
                command,
                args: server
                    .args
                    .iter()
                    .flatten()
                    .map(|v| expand(v, root, data, legacy))
                    .collect(),
                env: env.into_iter().collect(),
                cwd,
            }
        }
        "streamable-http" => {
            if server.command.is_some()
                || server.args.is_some()
                || server.env.is_some()
                || server.cwd.is_some()
            {
                return Err("stdio fields are not valid for HTTP".into());
            }
            let raw = server.url.as_deref().ok_or("HTTP requires url")?;
            let url = url::Url::parse(raw).map_err(|_| "invalid MCP URL")?;
            let loopback = url.host_str().is_some_and(|h| {
                h == "localhost"
                    || h.trim_matches(['[', ']'])
                        .parse::<std::net::IpAddr>()
                        .is_ok_and(|ip| ip.is_loopback())
            });
            if url.scheme() != "https" && !(url.scheme() == "http" && loopback) {
                return Err("MCP URL requires HTTPS except for loopback".into());
            }
            if !url.username().is_empty() || url.password().is_some() || url.fragment().is_some() {
                return Err("MCP URL must not contain credentials or fragments".into());
            }
            let mut headers = Vec::new();
            let mut seen = std::collections::BTreeSet::new();
            for (key, value) in server.headers.iter().flatten() {
                reject_secret(value)?;
                reqwest::header::HeaderName::from_bytes(key.as_bytes())
                    .map_err(|_| "invalid HTTP header name")?;
                reqwest::header::HeaderValue::from_str(value)
                    .map_err(|_| "invalid HTTP header value")?;
                if !seen.insert(key.to_ascii_lowercase()) {
                    return Err("duplicate HTTP header name".into());
                }
                headers.push((key.clone(), value.clone()));
            }
            declaration.transport = "http".into();
            declaration.url = Some(url.to_string());
            McpTransport::Http {
                url: url.to_string(),
                headers,
                auth: McpAuth::None,
                oauth_client_id: None,
                oauth_scopes: Vec::new(),
            }
        }
        _ => return Err("unsupported MCP transport (supported: stdio, streamable-http)".into()),
    };
    let mut secrets = Vec::new();
    if let Some(binding) = file.plugins.bindings.get(logical) {
        let mut config = File::default();
        let mut bound = McpServer {
            transport: declaration.transport.clone(),
            env: binding.env.clone(),
            headers: binding.headers.clone(),
            auth: binding.auth.clone(),
            oauth_client_id: binding.oauth_client_id.clone(),
            oauth_scopes: binding.oauth_scopes.clone(),
            ..McpServer::default()
        };
        if declaration.transport == "stdio" {
            bound.command = Some("plugin".into());
        } else {
            bound.url = declaration.url.clone();
        }
        // Policy validation sees the real command/destination, never the dummy
        // resolver command, and the original environment reference templates.
        declaration.env = bound.env.clone();
        declaration.headers = bound.headers.clone();
        config.mcp.servers.insert(name.into(), bound);
        let resolved = resolve_mcp(&config, environment, &root.to_string_lossy()).map_err(
            |_| "invalid plugin credential binding (check environment references and transport)",
        )?;
        let resolved = &resolved.servers[0];
        secrets = resolved.secrets.clone();
        match (&mut transport, &resolved.transport) {
            (McpTransport::Stdio { env, .. }, McpTransport::Stdio { env: overrides, .. }) => {
                let mut values: BTreeMap<_, _> = env.iter().cloned().collect();
                if binding
                    .env
                    .as_ref()
                    .is_some_and(|e| e.contains_key("PLUGIN_ROOT") || e.contains_key("PLUGIN_DATA"))
                {
                    return Err("bindings cannot override reserved plugin environment".into());
                }
                for (key, value) in overrides {
                    if binding.env.as_ref().is_some_and(|e| e.contains_key(key)) {
                        values.insert(key.clone(), value.clone());
                    }
                }
                *env = values.into_iter().collect();
            }
            (
                McpTransport::Http {
                    headers,
                    auth,
                    oauth_client_id,
                    oauth_scopes,
                    ..
                },
                McpTransport::Http {
                    headers: overrides,
                    auth: bound_auth,
                    oauth_client_id: client,
                    oauth_scopes: scopes,
                    ..
                },
            ) => {
                for (key, value) in overrides {
                    headers.retain(|(k, _)| !k.eq_ignore_ascii_case(key));
                    headers.push((key.clone(), value.clone()));
                }
                *auth = *bound_auth;
                *oauth_client_id = client.clone();
                *oauth_scopes = scopes.clone();
                if *auth == McpAuth::OAuth
                    && headers
                        .iter()
                        .any(|(k, _)| k.eq_ignore_ascii_case("authorization"))
                {
                    return Err("OAuth cannot be combined with an Authorization header".into());
                }
            }
            _ => return Err("invalid binding transport".into()),
        }
    }
    if file
        .policy
        .as_ref()
        .is_some_and(|p| p.allowed_mcp_commands.is_some())
        && let McpTransport::Stdio {
            command, env, cwd, ..
        } = &mut transport
        && !Path::new(command).is_absolute()
    {
        let path = env
            .iter()
            .find(|(key, _)| key == "PATH")
            .map(|(_, value)| value.as_str())
            .unwrap_or("/usr/bin:/bin");
        let executable = std::env::split_paths(path)
            .find_map(|directory| {
                crate::fsops::canonical_executable_file(
                    &Path::new(cwd).join(directory).join(&*command),
                )
                .ok()
            })
            .ok_or("managed MCP executable is unavailable on PATH")?;
        *command = executable.to_string_lossy().into_owned();
        declaration.command = Some(command.clone());
    }
    if let Some(policy) = &file.policy {
        let mut config = File::default();
        config.mcp.servers.insert(name.into(), declaration.clone());
        policy.mcp(&config).map_err(|e| e.to_string())?;
    }
    Ok((
        McpServerRuntime {
            name: name.into(),
            enabled: true,
            transport,
            secrets,
        },
        declaration,
    ))
}
fn reject_secret(value: &str) -> Result<(), String> {
    otto_core::config::mcp::validate_credential_template(value)
        .map_err(|_| "literal credential in package; use user-owned plugin bindings".into())
}
