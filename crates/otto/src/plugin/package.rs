//! Package discovery. All reads are confined to the supplied package root.
use crate::fsops::root::Root;
use nix::{fcntl::OFlag, sys::stat::Mode};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::BTreeSet,
    io::Read,
    path::{Path, PathBuf},
};

pub const PLUGIN_SCHEMA: &str = "https://agent-plugins.org/schemas/1.0.0/plugin.schema.json";
pub const MCP_SCHEMA: &str = "https://agent-plugins.org/schemas/1.0.0/mcp.schema.json";
const MAX_JSON: u64 = 1 << 20;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Package {
    pub name: String,
    pub version: Option<String>,
    pub format: String,
    pub skills: Vec<PathBuf>,
    pub servers: Vec<(String, Server)>,
    pub diagnostics: Vec<String>,
    #[serde(
        default,
        skip_serializing_if = "otto_core::config::plugins::Components::is_default"
    )]
    pub otto: otto_core::config::plugins::Components,
    /// `command` hooks of a `claude` package in the Claude Code hook format; see
    /// `super::claude_hooks`. Absent from registries written before this field existed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub claude_hooks: Vec<super::claude_hooks::ClaudeHook>,
}
impl Package {
    /// Whether trusting the package authorizes commands: `dev.otto` hooks, tools or memory,
    /// or Claude-format hooks.
    pub fn has_executables(&self) -> bool {
        self.otto.has_executables() || !self.claude_hooks.is_empty()
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Server {
    #[serde(rename = "type", default)]
    pub kind: String,
    pub command: Option<String>,
    pub args: Option<Vec<String>>,
    pub env: Option<std::collections::BTreeMap<String, String>>,
    pub cwd: Option<String>,
    pub url: Option<String>,
    pub headers: Option<std::collections::BTreeMap<String, String>>,
}

pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'.' || b == b'-')
        && name.as_bytes()[0].is_ascii_alphanumeric()
        && name.as_bytes()[name.len() - 1].is_ascii_alphanumeric()
        && !name.contains("--")
        && !name.contains("..")
}

pub(super) fn read(root: &Root, path: &Path) -> Result<Vec<u8>, String> {
    let mut file = root
        .open_file(path, OFlag::O_RDONLY | OFlag::O_NONBLOCK, Mode::empty())
        .map_err(|_| format!("cannot read package file {}", path.display()))?;
    if !file
        .metadata()
        .map_err(|_| "cannot inspect package file")?
        .is_file()
    {
        return Err("package file must be regular".into());
    }
    let mut package_file_bytes = Vec::new();
    file.by_ref()
        .take(MAX_JSON + 1)
        .read_to_end(&mut package_file_bytes)
        .map_err(|_| "cannot read package file")?;
    if package_file_bytes.len() as u64 > MAX_JSON {
        return Err("package JSON exceeds 1 MiB".into());
    }
    Ok(package_file_bytes)
}
fn json(root: &Root, path: &Path) -> Result<Value, String> {
    serde_json::from_slice(&read(root, path)?)
        .map_err(|e| format!("invalid JSON in {} at line {}", path.display(), e.line()))
}
fn paths(value: Option<&Value>, defaults: &[&str]) -> Result<Vec<PathBuf>, String> {
    match value {
        None => Ok(defaults.iter().map(PathBuf::from).collect()),
        Some(Value::String(s)) => Ok(vec![s.into()]),
        Some(Value::Array(a)) => a
            .iter()
            .map(|v| {
                v.as_str()
                    .map(PathBuf::from)
                    .ok_or_else(|| "component path must be a string".into())
            })
            .collect(),
        _ => Err("component path must be a string or string array".into()),
    }
}

pub fn discover(path: &Path) -> Result<Package, String> {
    let root = Root::open(path).map_err(|_| "cannot open plugin package")?;
    let portable = root.lstat(Path::new("plugin.json")).is_ok();
    let codex = root.lstat(Path::new(".codex-plugin/plugin.json")).is_ok();
    let claude = root.lstat(Path::new(".claude-plugin/plugin.json")).is_ok();
    let (manifest_path, format) = if portable {
        ("plugin.json", "agent-plugins-1.0")
    } else if codex && claude {
        return Err("ambiguous legacy plugin manifests".into());
    } else if codex {
        (".codex-plugin/plugin.json", "codex")
    } else if claude {
        (".claude-plugin/plugin.json", "claude")
    } else {
        return Err("missing plugin.json or supported legacy plugin manifest".into());
    };
    let manifest = json(&root, Path::new(manifest_path))?;
    let object = manifest
        .as_object()
        .ok_or("plugin manifest must be an object")?;
    if portable && manifest.get("$schema").and_then(Value::as_str) != Some(PLUGIN_SCHEMA) {
        return Err("unsupported or missing plugin schema version".into());
    }
    let name = manifest
        .get("name")
        .and_then(Value::as_str)
        .filter(|s| valid_name(s))
        .ok_or("invalid plugin name")?
        .to_owned();
    let version = manifest
        .get("version")
        .map(|v| {
            v.as_str()
                .map(str::to_owned)
                .ok_or("plugin version must be a string")
        })
        .transpose()?;
    let mut package = Package {
        name,
        version,
        format: format.into(),
        skills: Vec::new(),
        servers: Vec::new(),
        diagnostics: Vec::new(),
        otto: Default::default(),
        claude_hooks: Vec::new(),
    };
    if let Some(value) = manifest.pointer("/extensions/dev.otto") {
        package.otto =
            serde_json::from_value(value.clone()).map_err(|_| "invalid dev.otto extension")?;
        super::hooks::validate_components(&package.name, &package.otto)?;
        super::hooks::lua_handler::validate_sources(path, &package.otto)?;
    }
    if portable {
        for key in object.keys() {
            if ![
                "$schema",
                "name",
                "version",
                "description",
                "author",
                "homepage",
                "repository",
                "license",
                "keywords",
                "extensions",
            ]
            .contains(&key.as_str())
            {
                package
                    .diagnostics
                    .push(format!("ignored unknown manifest field {key}"));
            }
        }
        for field in ["description", "homepage", "repository", "license"] {
            if object.get(field).is_some_and(|v| !v.is_string()) {
                return Err(format!("{field} must be a string"));
            }
        }
        if let Some(author) = object.get("author") {
            let author = author.as_object().ok_or("author must be an object")?;
            if author
                .iter()
                .any(|(k, v)| !["name", "email", "url"].contains(&k.as_str()) || !v.is_string())
            {
                return Err("invalid author metadata".into());
            }
        }
        if object.get("keywords").is_some_and(|v| {
            v.as_array()
                .is_none_or(|a| a.iter().any(|v| !v.is_string()))
        }) {
            return Err("keywords must be a string array".into());
        }
        if let Some(extensions) = object.get("extensions").and_then(Value::as_object) {
            for (namespace, extension) in extensions {
                if matches!(namespace.as_str(), "com.openai" | "com.anthropic")
                    && extension.as_object().is_some_and(|o| {
                        o.keys().any(|k| {
                            ["hooks", "apps", "agents", "commands", "lspServers"]
                                .contains(&k.as_str())
                        })
                    })
                {
                    package.diagnostics.push(format!(
                        "unsupported host components in {namespace}; only portable Skills/MCP load"
                    ));
                }
            }
        }
    } else {
        // `claude` packages run their hooks natively; `codex` packages still report them.
        let native_hooks = format == "claude";
        if native_hooks {
            let discovered = super::claude_hooks::manifest::discover(&root, &manifest);
            package.claude_hooks = discovered.hooks;
            package.diagnostics.extend(discovered.diagnostics);
        }
        for key in ["commands", "agents", "hooks", "apps", "lspServers"] {
            if manifest.get(key).is_some() && !(native_hooks && key == "hooks") {
                package
                    .diagnostics
                    .push(format!("unsupported component {key}"));
            }
        }
        for (component, location) in [
            ("commands", "commands"),
            ("agents", "agents"),
            ("hooks", "hooks/hooks.json"),
            ("LSP", ".lsp.json"),
        ] {
            if native_hooks && component == "hooks" {
                continue;
            }
            if root.lstat(Path::new(location)).is_ok() {
                package
                    .diagnostics
                    .push(format!("unsupported {component} at {location}"));
            }
        }
    }
    let skill_paths = if portable {
        vec![PathBuf::from("skills")]
    } else {
        let mut configured = paths(manifest.get("skills"), &["skills"])?;
        if format == "claude" && !configured.contains(&PathBuf::from("skills")) {
            configured.insert(0, "skills".into());
        }
        configured
    };
    let mut seen = BTreeSet::new();
    for dir in skill_paths {
        if root.lstat(&dir).is_err() {
            continue;
        }
        if root.stat(&dir.join("SKILL.md")).is_ok() {
            if seen.insert(dir.clone()) {
                package.skills.push(dir);
            }
            continue;
        }
        match root.read_dir(&dir) {
            Ok(entries) => {
                for entry in entries {
                    let child = dir.join(entry.name);
                    if root.stat(&child.join("SKILL.md")).is_ok() && seen.insert(child.clone()) {
                        package.skills.push(child);
                    }
                }
            }
            Err(_) => package
                .diagnostics
                .push(format!("invalid skills path {}", dir.display())),
        }
    }
    let inline_mcp = !portable && manifest.get("mcpServers").is_some_and(Value::is_object);
    let mut mcp_paths = if inline_mcp {
        Vec::new()
    } else if portable {
        vec![PathBuf::from("mcp.json")]
    } else {
        paths(manifest.get("mcpServers"), &[".mcp.json"])?
    };
    if !portable && format == "claude" && !mcp_paths.contains(&PathBuf::from(".mcp.json")) {
        mcp_paths.insert(0, ".mcp.json".into());
    }
    if inline_mcp {
        mcp_paths.push(PathBuf::from("<manifest-mcp>"));
    }
    let mut server_names = BTreeSet::new();
    for config_path in mcp_paths {
        if config_path != Path::new("<manifest-mcp>") && root.lstat(&config_path).is_err() {
            continue;
        }
        let result = (|| {
            let value = if config_path == Path::new("<manifest-mcp>") {
                let inline = manifest
                    .get("mcpServers")
                    .expect("inline MCP exists")
                    .clone();
                if inline.get("mcpServers").is_some() {
                    inline
                } else {
                    serde_json::json!({"mcpServers":inline})
                }
            } else {
                json(&root, &config_path)?
            };
            if portable && value.get("$schema").and_then(Value::as_str) != Some(MCP_SCHEMA) {
                return Err("unsupported or missing MCP schema version".into());
            }
            let map = value
                .get("mcpServers")
                .and_then(Value::as_object)
                .ok_or("mcpServers must be an object")?;
            for (name, value) in map {
                let mut value = value.clone();
                if !portable && let Some(obj) = value.as_object_mut() {
                    if !obj.contains_key("type") {
                        obj.insert(
                            "type".into(),
                            if obj.contains_key("url") {
                                "streamable-http"
                            } else {
                                "stdio"
                            }
                            .into(),
                        );
                    }
                    if obj.get("type").and_then(Value::as_str) == Some("http") {
                        obj.insert("type".into(), "streamable-http".into());
                    }
                }
                match serde_json::from_value::<Server>(value) {
                    Ok(server)
                        if !name.is_empty()
                            && name.len() <= 128
                            && !name.chars().any(char::is_control)
                            && server_names.insert(name.clone()) =>
                    {
                        for value in server
                            .env
                            .iter()
                            .chain(server.headers.iter())
                            .flat_map(|m| m.values())
                        {
                            if otto_core::config::mcp::validate_credential_template(value).is_err()
                            {
                                return Err("literal credential in MCP configuration; use user-owned environment bindings".into());
                            }
                        }
                        package.servers.push((name.clone(), server))
                    }
                    _ => package
                        .diagnostics
                        .push(format!("invalid or duplicate MCP server {name:?}")),
                }
            }
            Ok::<_, String>(())
        })();
        if let Err(error) = result {
            if error.starts_with("literal credential") {
                return Err(error);
            }
            package
                .diagnostics
                .push(format!("{}: {error}", config_path.display()));
        }
    }
    package.skills.sort();
    Ok(package)
}
