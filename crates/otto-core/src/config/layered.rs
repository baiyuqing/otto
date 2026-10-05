//! Presence-preserving configuration merge and source authority, without I/O.

use super::{ConfigError, File};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use toml::{Table, Value};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Layer {
    User,
    Shared,
    Local,
    Managed,
}

impl Layer {
    pub fn project(self) -> bool {
        matches!(self, Self::Shared | Self::Local)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Origin {
    pub layer: Layer,
    pub source: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Diagnostic {
    pub key: String,
    pub source: String,
    pub reason: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Merged {
    pub values: Table,
    pub origins: BTreeMap<String, Origin>,
    pub overridden: BTreeMap<String, Vec<Origin>>,
    pub diagnostics: Vec<Diagnostic>,
}

impl Merged {
    pub fn add(
        &mut self,
        text: &str,
        source: &str,
        layer: Layer,
        trusted: bool,
    ) -> Result<(), ConfigError> {
        // Validate unknown fields/types before filtering: ignored does not mean invalid TOML.
        toml::from_str::<File>(text).map_err(|error| {
            let at = error.span().map_or(0, |span| span.start.min(text.len()));
            let line = text[..at].bytes().filter(|b| *b == b'\n').count() + 1;
            ConfigError::new(format!("invalid configuration in {source} at line {line}"))
        })?;
        let mut table: Table = toml::from_str(text)
            .map_err(|_| ConfigError::new(format!("invalid TOML in {source}")))?;
        if layer.project() {
            filter(&mut table, &[], source, trusted, &mut self.diagnostics);
            // Never mix a lower-layer secret binding with a new project execution identity.
            if let Some(servers) = table
                .get_mut("mcp")
                .and_then(Value::as_table_mut)
                .and_then(|m| m.get_mut("servers"))
                .and_then(Value::as_table_mut)
            {
                for (name, entry) in servers {
                    let old = self
                        .values
                        .get("mcp")
                        .and_then(Value::as_table)
                        .and_then(|m| m.get("servers"))
                        .and_then(Value::as_table)
                        .and_then(|m| m.get(name))
                        .and_then(Value::as_table);
                    if let (Some(old), Some(new)) = (old, entry.as_table_mut()) {
                        let bound = ["env", "headers"].iter().any(|k| {
                            old.get(*k)
                                .and_then(Value::as_table)
                                .is_some_and(|t| !t.is_empty())
                        }) || old
                            .get("auth")
                            .and_then(Value::as_str)
                            .is_some_and(|auth| auth != "none")
                            || ["oauth_client_id", "oauth_scopes"]
                                .iter()
                                .any(|k| old.contains_key(*k));
                        if bound
                            && ["transport", "command", "args", "cwd", "url"]
                                .iter()
                                .any(|k| new.get(*k).is_some_and(|v| old.get(*k) != Some(v)))
                        {
                            for key in ["transport", "command", "args", "cwd", "url"] {
                                new.remove(key);
                            }
                            self.diagnostics.push(Diagnostic {
                                key: format!("mcp.servers.{name}"),
                                source: source.into(),
                                reason: "project cannot redirect a credential-bound MCP server"
                                    .into(),
                            });
                        }
                    }
                }
            }
            if let Some(servers) = table
                .get_mut("mcp")
                .and_then(Value::as_table_mut)
                .and_then(|m| m.get_mut("servers"))
                .and_then(Value::as_table_mut)
            {
                for name in servers.keys().cloned().collect::<Vec<_>>() {
                    let existing = self
                        .values
                        .get("mcp")
                        .and_then(Value::as_table)
                        .and_then(|m| m.get("servers"))
                        .and_then(Value::as_table)
                        .is_some_and(|m| m.contains_key(&name));
                    if !existing && !trusted {
                        servers.remove(&name);
                    }
                }
            }
        }
        // A transport switch replaces its incompatible fields rather than making
        // an invalid hybrid server. Credential-bearing project switches were removed above.
        if let Some(incoming) = table
            .get("mcp")
            .and_then(Value::as_table)
            .and_then(|m| m.get("servers"))
            .and_then(Value::as_table)
            && let Some(existing) = self
                .values
                .get_mut("mcp")
                .and_then(Value::as_table_mut)
                .and_then(|m| m.get_mut("servers"))
                .and_then(Value::as_table_mut)
        {
            for (name, value) in incoming {
                let next = value
                    .as_table()
                    .and_then(|t| t.get("transport"))
                    .and_then(Value::as_str);
                if let Some(old) = existing.get_mut(name).and_then(Value::as_table_mut)
                    && let Some(next) = next
                        .filter(|next| old.get("transport").and_then(Value::as_str) != Some(*next))
                {
                    let incompatible: &[&str] = if next == "http" {
                        &["command", "args", "cwd", "env"]
                    } else {
                        &["url", "headers", "auth", "oauth_client_id", "oauth_scopes"]
                    };
                    for key in incompatible {
                        old.remove(*key);
                        let prefix = format!("mcp.servers.{name}.{key}");
                        let removed: Vec<_> = self
                            .origins
                            .keys()
                            .filter(|k| *k == &prefix || k.starts_with(&format!("{prefix}.")))
                            .cloned()
                            .collect();
                        for full in removed {
                            if let Some(previous) = self.origins.remove(&full) {
                                self.overridden.entry(full).or_default().push(previous);
                            }
                        }
                    }
                }
            }
        }
        let origin = Origin {
            layer,
            source: source.into(),
        };
        merge(
            &mut self.values,
            table,
            &[],
            &origin,
            &mut self.origins,
            &mut self.overridden,
        );
        Ok(())
    }

    pub fn file(&self) -> Result<File, ConfigError> {
        Value::Table(self.values.clone())
            .try_into()
            .map_err(|_| ConfigError::new("invalid merged configuration"))
    }

    /// Explain never exposes env/header bindings, including their templates.
    pub fn explained(&self) -> Self {
        let mut out = self.clone();
        redact_table(&mut out.values, &[]);
        out
    }
}

fn redact_table(table: &mut Table, path: &[String]) {
    for (key, value) in table {
        let mut p = path.to_vec();
        p.push(key.clone());
        if matches!(
            key.as_str(),
            "env" | "headers" | "api_key_env" | "args" | "url" | "base_url" | "oauth_client_id"
        ) {
            *value = Value::String("<redacted>".into());
        } else if let Some(t) = value.as_table_mut() {
            redact_table(t, &p);
        }
    }
}

fn filter(
    table: &mut Table,
    path: &[String],
    source: &str,
    trusted: bool,
    diagnostics: &mut Vec<Diagnostic>,
) {
    for key in table.keys().cloned().collect::<Vec<_>>() {
        let value = table.get_mut(&key).expect("existing key");
        let mut p = path.to_vec();
        p.push(key.clone());
        if let Some(reason) = project_rule(&p, value.is_table(), trusted) {
            diagnostics.push(Diagnostic {
                key: p.join("."),
                source: source.into(),
                reason: reason.into(),
            });
            table.remove(&key);
            continue;
        }
        if ((p.len() == 4 && p[0] == "mcp" && p[1] == "servers" && p[3] == "enabled")
            || (p.len() == 2 && p[0] == "mcp" && p[1] == "enabled"))
            && value.as_bool() == Some(true)
            && !trusted
        {
            diagnostics.push(Diagnostic {
                key: p.join("."),
                source: source.into(),
                reason: "activating a server requires executable trust".into(),
            });
            table.remove(&key);
            continue;
        }
        if p.len() == 4
            && p[0] == "mcp"
            && p[1] == "servers"
            && matches!(p[3].as_str(), "command" | "args" | "cwd" | "url")
            && (value.as_str().is_some_and(|v| v.contains("${"))
                || value.as_array().is_some_and(|vs| {
                    vs.iter()
                        .any(|v| v.as_str().is_some_and(|v| v.contains("${")))
                }))
        {
            diagnostics.push(Diagnostic {
                key: p.join("."),
                source: source.into(),
                reason: "environment bindings are restricted to user or managed configuration"
                    .into(),
            });
            table.remove(&key);
            continue;
        }
        if let Some(t) = value.as_table_mut() {
            filter(t, &p, source, trusted, diagnostics);
        }
    }
}

fn project_rule(p: &[String], table: bool, trusted: bool) -> Option<&'static str> {
    let keys: Vec<&str> = p.iter().map(String::as_str).collect();
    let allowed = match keys.as_slice() {
        ["default_profile"] => true,
        ["ui", "mode"] => true,
        ["agent", ..] => true,
        ["profiles", _, field] => matches!(
            *field,
            "model" | "thinking" | "context_window" | "compaction_window"
        ),
        ["mcp", field] => matches!(
            *field,
            "enabled" | "call_timeout_secs" | "connect_timeout_secs"
        ),
        ["mcp", "servers", _, field] => {
            if matches!(*field, "transport" | "command" | "args" | "cwd" | "url") {
                return if trusted {
                    None
                } else {
                    Some("executable project configuration is not trusted")
                };
            }
            *field == "enabled"
        }
        ["web", "allow_hosts"] => true,
        _ => false,
    };
    let container = table
        && matches!(
            keys.as_slice(),
            ["ui"]
                | ["agent", ..]
                | ["profiles"]
                | ["profiles", _]
                | ["mcp"]
                | ["mcp", "servers"]
                | ["mcp", "servers", _]
                | ["web"]
        );
    if allowed || container {
        None
    } else {
        Some("field is restricted to user or managed configuration")
    }
}

fn merge(
    into: &mut Table,
    from: Table,
    path: &[String],
    origin: &Origin,
    origins: &mut BTreeMap<String, Origin>,
    overridden: &mut BTreeMap<String, Vec<Origin>>,
) {
    for (key, value) in from {
        let mut p = path.to_vec();
        p.push(key.clone());
        if let Value::Table(table) = value {
            let entry = into
                .entry(key)
                .or_insert_with(|| Value::Table(Table::new()));
            if !entry.is_table() {
                *entry = Value::Table(Table::new());
            }
            merge(
                entry.as_table_mut().expect("table"),
                table,
                &p,
                origin,
                origins,
                overridden,
            );
        } else {
            let full = p.join(".");
            if let Some(previous) = origins.insert(full.clone(), origin.clone()) {
                overridden.entry(full).or_default().push(previous);
            }
            into.insert(key, value);
        }
    }
}

/// Administrator-owned policy. Empty allowlists deny all; absent lists do not constrain.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ManagedPolicy {
    #[serde(default)]
    pub force: Table,
    pub require_sandbox: Option<bool>,
    pub allow_project_executables: Option<bool>,
    pub allowed_provider_hosts: Option<Vec<String>>,
    pub allowed_mcp_hosts: Option<Vec<String>>,
    pub allowed_mcp_commands: Option<Vec<String>>,
    pub allowed_credential_variables: Option<Vec<String>>,
    #[serde(default)]
    pub ceilings: BTreeMap<String, i64>,
}

impl ManagedPolicy {
    pub fn apply(&self, merged: &mut Merged, source: &str) -> Result<(), ConfigError> {
        if self.allowed_mcp_commands.as_ref().is_some_and(|commands| {
            commands
                .iter()
                .any(|c| !c.starts_with('/') || c.split('/').any(|p| p == "..") || c.contains("${"))
        }) {
            return Err(ConfigError::new(
                "managed MCP commands must be absolute administrator-controlled paths",
            ));
        }
        // Value's display is an inline table; validate by deserializing directly.
        let _: File = Value::Table(self.force.clone())
            .try_into()
            .map_err(|_| ConfigError::new("invalid managed force fields"))?;
        let text = toml::to_string(&self.force)
            .map_err(|_| ConfigError::new("invalid managed force fields"))?;
        merged.add(&text, source, Layer::Managed, true)?;
        for (key, ceiling) in &self.ceilings {
            if !matches!(
                key.as_str(),
                "agent.max_output_bytes"
                    | "agent.compaction.reserve_tokens"
                    | "agent.compaction.keep_recent_tokens"
            ) || *ceiling <= 0
            {
                return Err(ConfigError::new("invalid managed ceiling"));
            }
            let mut value = None;
            let mut current = &merged.values;
            let parts: Vec<_> = key.split('.').collect();
            for (i, part) in parts.iter().enumerate() {
                if i + 1 == parts.len() {
                    value = current.get(*part).and_then(Value::as_integer);
                } else if let Some(next) = current.get(*part).and_then(Value::as_table) {
                    current = next;
                } else {
                    break;
                }
            }
            if value.is_some_and(|v| v > *ceiling) {
                return Err(ConfigError::new(format!(
                    "managed policy ceiling exceeded: {key}"
                )));
            }
        }
        Ok(())
    }

    pub fn runtime(&self, runtime: &super::resolve::Runtime) -> Result<(), ConfigError> {
        if self
            .force
            .get("default_profile")
            .and_then(Value::as_str)
            .is_some_and(|p| p != runtime.profile)
        {
            return Err(ConfigError::new("profile is forced by managed policy"));
        }
        if let Some(forced) = self
            .force
            .get("profiles")
            .and_then(Value::as_table)
            .and_then(|p| p.get(&runtime.profile))
            .and_then(Value::as_table)
        {
            for (key, actual) in [
                ("provider", &runtime.provider),
                ("base_url", &runtime.base_url),
                ("model", &runtime.model),
                ("thinking", &runtime.thinking),
                ("api_key_env", &runtime.api_key_env),
            ] {
                if forced.get(key).and_then(Value::as_str).is_some_and(|v| {
                    if key == "base_url" {
                        super::resolve::normalize_base_url(v)
                            .is_ok_and(|normalized| normalized != *actual)
                    } else {
                        v != actual
                    }
                }) {
                    return Err(ConfigError::new(format!(
                        "profile field is forced by managed policy: {key}"
                    )));
                }
            }
        }
        if let Some(agent) = self.force.get("agent").and_then(Value::as_table) {
            if agent
                .get("max_output_bytes")
                .and_then(Value::as_integer)
                .is_some_and(|v| v != runtime.max_output_bytes)
            {
                return Err(ConfigError::new("output limit is forced by managed policy"));
            }
            if let Some(timeout) = agent.get("shell_timeout").and_then(Value::as_str) {
                let nanos =
                    super::duration::parse_go_duration(timeout).map_err(ConfigError::new)?;
                if nanos <= 0
                    || runtime.shell_timeout != std::time::Duration::from_nanos(nanos as u64)
                {
                    return Err(ConfigError::new(
                        "shell timeout is forced by managed policy",
                    ));
                }
            }
        }
        if let Some(hosts) = &self.allowed_provider_hosts {
            let host = if runtime.provider == super::PROVIDER_CHATGPT {
                "chatgpt.com".to_string()
            } else {
                url::Url::parse(&runtime.base_url)
                    .ok()
                    .and_then(|u| u.host_str().map(str::to_owned))
                    .unwrap_or_default()
            };
            if !hosts.contains(&host) {
                return Err(ConfigError::new(
                    "provider destination is prohibited by managed policy",
                ));
            }
        }
        if let Some(names) = &self.allowed_credential_variables {
            let name = if runtime.api_key_env.is_empty() {
                "OTTO_API_KEY"
            } else {
                &runtime.api_key_env
            };
            if runtime.provider != super::PROVIDER_CHATGPT && !names.iter().any(|n| n == name) {
                return Err(ConfigError::new(
                    "provider credential binding is prohibited by managed policy",
                ));
            }
        }
        if self
            .ceilings
            .get("agent.max_output_bytes")
            .is_some_and(|v| runtime.max_output_bytes > *v)
        {
            return Err(ConfigError::new(
                "managed policy ceiling exceeded: agent.max_output_bytes",
            ));
        }
        for (key, actual) in [
            (
                "agent.compaction.reserve_tokens",
                runtime.compaction.reserve_tokens,
            ),
            (
                "agent.compaction.keep_recent_tokens",
                runtime.compaction.keep_recent_tokens,
            ),
        ] {
            if self.ceilings.get(key).is_some_and(|v| actual > *v) {
                return Err(ConfigError::new(format!(
                    "managed policy ceiling exceeded: {key}"
                )));
            }
        }
        Ok(())
    }

    pub fn sandbox(&self, settings: &super::SandboxSettings) -> Result<(), ConfigError> {
        if self.require_sandbox == Some(true) && settings.driver == super::SandboxDriverMode::Off {
            return Err(ConfigError::new(
                "managed policy prohibits disabling the sandbox",
            ));
        }
        if self.require_sandbox == Some(true) && !settings.excluded_commands.is_empty() {
            return Err(ConfigError::new(
                "managed sandbox policy prohibits excluded commands",
            ));
        }
        if let Some(Value::Table(forced)) = self.force.get("sandbox") {
            let driver = match settings.driver {
                super::SandboxDriverMode::Auto => "auto",
                super::SandboxDriverMode::Seatbelt => "seatbelt",
                super::SandboxDriverMode::Off => "off",
            };
            if forced
                .get("driver")
                .and_then(Value::as_str)
                .is_some_and(|v| v != driver)
            {
                return Err(ConfigError::new(
                    "sandbox driver is forced by managed policy",
                ));
            }
            let network = match settings.network {
                super::SandboxNetworkMode::Allow => "allow",
                super::SandboxNetworkMode::Deny => "deny",
            };
            if forced
                .get("network")
                .and_then(Value::as_str)
                .is_some_and(|v| v != network)
            {
                return Err(ConfigError::new(
                    "sandbox network is forced by managed policy",
                ));
            }
            for (key, actual) in [
                ("read_paths", &settings.read_paths),
                ("allow_env", &settings.allow_env),
                ("excluded_commands", &settings.excluded_commands),
            ] {
                if let Some(allowed) = forced.get(key).and_then(Value::as_array)
                    && actual
                        .iter()
                        .any(|a| !allowed.iter().any(|v| v.as_str() == Some(a)))
                {
                    return Err(ConfigError::new(format!(
                        "sandbox field is constrained by managed policy: {key}"
                    )));
                }
            }
        }
        Ok(())
    }

    pub fn mcp(&self, file: &File) -> Result<(), ConfigError> {
        for server in file
            .mcp
            .servers
            .values()
            .filter(|s| s.enabled != Some(false))
        {
            if server.transport == "http"
                && let Some(allowed) = &self.allowed_mcp_hosts
            {
                let host = server
                    .url
                    .as_ref()
                    .and_then(|u| url::Url::parse(u).ok())
                    .and_then(|u| u.host_str().map(str::to_owned))
                    .unwrap_or_default();
                if !allowed.contains(&host) {
                    return Err(ConfigError::new(
                        "MCP destination is prohibited by managed policy",
                    ));
                }
            }
            if server.transport == "stdio"
                && let Some(allowed) = &self.allowed_mcp_commands
                && !server
                    .command
                    .as_ref()
                    .is_some_and(|cmd| allowed.contains(cmd))
            {
                return Err(ConfigError::new(
                    "MCP command is prohibited by managed policy",
                ));
            }
            if let Some(allowed) = &self.allowed_credential_variables {
                for value in server
                    .env
                    .iter()
                    .chain(server.headers.iter())
                    .flat_map(|map| map.values())
                    .chain(server.command.iter())
                    .chain(server.args.iter().flatten())
                    .chain(server.cwd.iter())
                    .chain(server.url.iter())
                {
                    for reference in value.split("${").skip(1) {
                        let name = reference.split(['}', ':']).next().unwrap_or_default();
                        if !allowed.iter().any(|n| n == name) {
                            return Err(ConfigError::new(
                                "MCP credential binding is prohibited by managed policy",
                            ));
                        }
                    }
                }
                if server.auth.as_deref() == Some("oauth") {
                    return Err(ConfigError::new(
                        "MCP OAuth is prohibited by credential-binding policy",
                    ));
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn preserves_presence_and_merges_named_fields() {
        let mut m = Merged::default();
        m.add("default_profile='a'\n[profiles.a]\nprovider='chatgpt'\nmodel='one'\n[agent.compaction]\nauto=true", "user", Layer::User, false).unwrap();
        m.add(
            "[profiles.a]\nmodel='two'\n[agent.compaction]\nauto=false\nreserve_tokens=0",
            "project",
            Layer::Shared,
            false,
        )
        .unwrap();
        let f = m.file().unwrap();
        assert_eq!(f.profiles["a"].provider, "chatgpt");
        assert_eq!(f.profiles["a"].model, "two");
        assert_eq!(f.agent.compaction.auto, Some(false));
        assert_eq!(f.agent.compaction.reserve_tokens, Some(0));
        assert_eq!(m.origins["profiles.a.model"].source, "project");
        assert_eq!(m.overridden["profiles.a.model"].len(), 1);
    }
    #[test]
    fn project_authority_cannot_redirect_secrets_or_enable_untrusted_processes() {
        let mut m = Merged::default();
        m.add("[profiles.a]\nprovider='openai-compatible'\nbase_url='https://good.test'\napi_key_env='KEY'\n[mcp.servers.a]\ncommand='safe'\nenv={TOKEN='${KEY}'}", "u", Layer::User, true).unwrap();
        m.add("[sandbox]\ndriver='off'\n[profiles.a]\nbase_url='https://evil.test'\n[mcp.servers.a]\ncommand='evil'\n[mcp.servers.b]\ncommand='evil'", "p", Layer::Local, false).unwrap();
        assert_eq!(
            m.file().unwrap().profiles["a"].base_url,
            "https://good.test"
        );
        assert!(m.diagnostics.len() >= 4);
        m.add(
            "[mcp.servers.a]\ncommand='evil'",
            "trusted",
            Layer::Shared,
            true,
        )
        .unwrap();
        assert_eq!(
            m.file().unwrap().mcp.servers["a"].command.as_deref(),
            Some("safe")
        );
        assert!(
            !serde_json::to_string(&m.explained())
                .unwrap()
                .contains("${KEY}")
        );
    }
    #[test]
    fn arrays_replace_including_empty_and_named_entries_can_disable() {
        let mut m = Merged::default();
        m.add(
            "[mcp.servers.a]\ncommand='cmd'\nargs=['old']\n[mcp.servers.b]\ncommand='b'",
            "u",
            Layer::User,
            true,
        )
        .unwrap();
        m.add(
            "[mcp.servers.a]\nargs=[]\nenabled=false",
            "p",
            Layer::Shared,
            true,
        )
        .unwrap();
        assert_eq!(m.file().unwrap().mcp.servers["a"].args, Some(vec![]));
        assert_eq!(m.file().unwrap().mcp.servers["a"].enabled, Some(false));
        assert!(m.file().unwrap().mcp.servers.contains_key("b"));
    }
    #[test]
    fn transport_changes_remove_incompatible_fields() {
        let mut m = Merged::default();
        m.add(
            "[mcp.servers.a]\ntransport='stdio'\ncommand='/usr/bin/tool'\nargs=['one']",
            "user",
            Layer::User,
            true,
        )
        .unwrap();
        m.add(
            "[mcp.servers.a]\ntransport='http'\nurl='https://tools.test'",
            "local",
            Layer::Local,
            true,
        )
        .unwrap();
        let f = m.file().unwrap();
        assert!(f.mcp.servers["a"].command.is_none());
        assert!(f.mcp.servers["a"].args.is_none());
        assert!(!m.origins.contains_key("mcp.servers.a.command"));
        assert!(super::super::resolve_mcp(&f, &Default::default(), "/work").is_ok());
    }
    #[test]
    fn project_cannot_interpolate_credentials_or_reactivate_all_servers() {
        let mut m = Merged::default();
        m.add(
            "[mcp]\nenabled=false\n[mcp.servers.a]\ntransport='stdio'\ncommand='safe'",
            "user",
            Layer::User,
            true,
        )
        .unwrap();
        m.add(
            "[mcp]\nenabled=true\n[mcp.servers.a]\nargs=['${OTTO_API_KEY}']",
            "project",
            Layer::Shared,
            false,
        )
        .unwrap();
        assert_eq!(m.file().unwrap().mcp.enabled, Some(false));
        m.add(
            "[mcp.servers.a]\nargs=['${OTTO_API_KEY}']",
            "trusted-project",
            Layer::Shared,
            true,
        )
        .unwrap();
        assert!(m.file().unwrap().mcp.servers["a"].args.is_none());
        assert!(
            m.diagnostics
                .iter()
                .any(|d| d.reason.contains("environment bindings"))
        );
    }
    #[test]
    fn managed_force_survives_cli_and_session_overrides() {
        let policy: ManagedPolicy = toml::from_str("[force]\ndefault_profile='a'\n[force.profiles.a]\nprovider='chatgpt'\nmodel='gpt-5'\n[force.agent]\nmax_output_bytes=1000").unwrap();
        let mut m = Merged::default();
        policy.apply(&mut m, "managed").unwrap();
        let mut file = m.file().unwrap();
        file.policy = Some(policy);
        let env = std::collections::HashMap::new();
        assert!(
            super::super::resolve::resolve_without_credentials(
                &file,
                &env,
                &Default::default(),
                &Default::default()
            )
            .is_ok()
        );
        let overrides = super::super::Overrides {
            model: "other".into(),
            ..Default::default()
        };
        assert!(
            super::super::resolve::resolve_without_credentials(
                &file,
                &env,
                &Default::default(),
                &overrides
            )
            .is_err()
        );
        let overrides = super::super::Overrides {
            max_output_bytes: 2000,
            ..Default::default()
        };
        assert!(
            super::super::resolve::resolve_without_credentials(
                &file,
                &env,
                &Default::default(),
                &overrides
            )
            .is_err()
        );
        assert_eq!(m.origins["profiles.a.model"].layer, Layer::Managed);
    }
    #[test]
    fn managed_destinations_credentials_sandbox_and_ceilings_fail_closed() {
        let policy: ManagedPolicy = toml::from_str("require_sandbox=true\nallowed_provider_hosts=[]\nallowed_mcp_hosts=[]\nallowed_credential_variables=['SAFE']\n[ceilings]\n'agent.max_output_bytes'=100").unwrap();
        let runtime = super::super::Runtime {
            provider: "openai-compatible".into(),
            base_url: "https://evil.test/v1".into(),
            api_key_env: "OTHER".into(),
            max_output_bytes: 200,
            ..Default::default()
        };
        assert!(policy.runtime(&runtime).is_err());
        let settings = super::super::resolve_sandbox(&Default::default(), Some("off")).unwrap();
        assert!(policy.sandbox(&settings).is_err());
        let f = super::super::parse("[mcp.servers.a]\ntransport='http'\nurl='https://evil.test'")
            .unwrap();
        assert!(policy.mcp(&f).is_err());
        let mut m = Merged::default();
        m.add("[agent]\nmax_output_bytes=200", "user", Layer::User, true)
            .unwrap();
        assert!(policy.apply(&mut m, "managed").is_err());
        let policy: ManagedPolicy = toml::from_str("allowed_mcp_commands=['tool']").unwrap();
        assert!(policy.apply(&mut Merged::default(), "managed").is_err());
    }
    #[test]
    fn explanation_redacts_sensitive_url_arguments_and_bindings() {
        let mut m = Merged::default();
        m.add("[profiles.a]\nbase_url='https://private-marker.test'\napi_key_env='KEY'\n[mcp.servers.a]\nargs=['argument-marker']\nenv={TOKEN='binding-marker'}", "user", Layer::User, true).unwrap();
        let output = serde_json::to_string(&m.explained()).unwrap();
        for secret in ["private-marker", "argument-marker", "binding-marker"] {
            assert!(!output.contains(secret));
        }
        assert!(output.contains("redacted"));
    }
}
