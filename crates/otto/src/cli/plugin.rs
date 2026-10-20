//! Offline plugin management over the native service; no provider is required.
use super::run::fail;
use crate::plugin::Service;
use std::{collections::HashMap, io::Write, path::Path};
const USAGE: &str = "usage: otto plugin install <directory>|<git-url> --rev <commit> | list | inspect|enable|disable|trust|revoke|remove <name>";

pub fn run(
    args: &[String],
    stdout: &mut (dyn Write + Send),
    stderr: &mut (dyn Write + Send),
    environment: &HashMap<String, String>,
) -> i32 {
    let Some(home) = environment.get("HOME").filter(|s| !s.is_empty()) else {
        return fail(stderr, "plugin management requires HOME");
    };
    let service = Service::new(Path::new(home));
    let result = (|| {
        let Some(action) = args.first().map(String::as_str) else {
            return Err(USAGE.into());
        };
        match (action, &args[1..]) {
            ("list", []) => {
                for p in service.list()? {
                    writeln!(
                        stdout,
                        "{}\t{}\t{}\t{}",
                        p.name,
                        p.version.as_deref().unwrap_or("unversioned"),
                        if p.enabled { "enabled" } else { "disabled" },
                        if service.authorized(&p.name, &p.digest) {
                            "trusted"
                        } else {
                            "untrusted"
                        }
                    )
                    .map_err(|_| "cannot write plugin list")?;
                }
            }
            ("install", [source]) => {
                let p = service.install(source, None)?;
                writeln!(
                    stdout,
                    "Installed {} ({}). Skills enabled; MCP, hooks and custom tools require \
                    explicit trust. Start a new session to activate.",
                    p.name, p.digest
                )
                .map_err(|_| "cannot write installation status")?;
            }
            ("install", [source, flag, rev]) if flag == "--rev" => {
                let p = service.install(source, Some(rev))?;
                writeln!(
                    stdout,
                    "Installed {} at {}. Skills enabled; MCP, hooks and custom tools require \
                    explicit trust. Start a new session to activate.",
                    p.name,
                    p.commit.as_deref().unwrap_or("")
                )
                .map_err(|_| "cannot write installation status")?;
            }
            ("inspect", [name]) => {
                let p = service.inspect(name)?;
                let report = serde_json::json!({"plugin":p,
                    "content_authorized":service.authorized(name,&p.digest),
                    "data_directory":service.data_path(name),
                    "mcp_identities":p.package.servers.iter().map(|(s,_)| {
                        let logical=format!("{name}:{s}");
                        serde_json::json!({"logical":logical,"internal":crate::plugin::runtime_name(&logical)})
                    }).collect::<Vec<_>>()});
                writeln!(
                    stdout,
                    "{}",
                    serde_json::to_string_pretty(&report)
                        .map_err(|_| "cannot encode plugin inspection")?
                )
                .map_err(|_| "cannot write plugin inspection")?;
            }
            ("trust", [name]) => {
                let p = service.inspect(name)?;
                writeln!(
                    stdout,
                    "Authorizing plugin {} digest {} from {}",
                    p.name, p.digest, p.source
                )
                .map_err(|_| "cannot write trust details")?;
                for (server, config) in &p.package.servers {
                    writeln!(
                        stdout,
                        "MCP {name}:{server}: {}",
                        serde_json::to_string(config)
                            .map_err(|_| "cannot encode execution details")?
                    )
                    .map_err(|_| "cannot write trust details")?;
                }
                if p.package.otto.has_executables() {
                    writeln!(
                        stdout,
                        "Otto hooks/tools/memory: {}",
                        serde_json::to_string(&p.package.otto)
                            .map_err(|_| "cannot encode execution details")?
                    )
                    .map_err(|_| "cannot write trust details")?;
                }
                service.change(name, action)?;
                writeln!(
                    stdout,
                    "Trusted. Start a new session to activate MCP, hooks and custom tools."
                )
                .map_err(|_| "cannot write trust status")?;
            }
            ("enable" | "disable" | "revoke" | "remove", [name]) => {
                service.change(name, action)?;
                writeln!(
                    stdout,
                    "Plugin {name}: {action}. Enable/disable changes apply to new sessions; \
                    revoke/remove stops active plugin commands and MCP servers. Persistent \
                    data is retained."
                )
                .map_err(|_| "cannot write plugin status")?;
            }
            _ => return Err(USAGE.into()),
        }
        Ok::<_, String>(())
    })();
    match result {
        Ok(()) => 0,
        Err(e) => fail(stderr, &e),
    }
}
